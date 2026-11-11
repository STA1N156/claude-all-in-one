//! admin 请求日志端点(调试用):列表 + 详情(含完整报文)。
//!
//! - `GET /logs`      列表(按成功/失败、账号、模型、时间窗筛选;不含 payload)
//! - `GET /logs/{id}`  单条详情(含用户原始报文 + 发 Kiro 前报文 + 去重的媒体 blob)
//!
//! 数据由 worker 在每次 chat 收尾时环形落库(最新 N 条,见 worker::write_request_log)。
//! 时间筛选优先级:`from`/`to`(Unix 秒)> `all=true` > `days`(默认 7,钳 [0,3650])。
//!
//! 每行附带 `credit_hit_ratio`:按当前生效的积分参数从 Kiro 实扣积分反推的缓存命中比例
//! (与计费收尾同一函数 [`hit_ratio_from_credit`])。Kiro 已基本不回报真实命中
//! (`real_cache_read_tokens` 恒 0),日志页「真」列靠它判断这次到底热没热。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use gw_core::store::{RequestLogFilter, RequestLogRow};
use gw_kiro::billing::{hit_ratio_from_credit, CachePolicy};
use serde::{Deserialize, Serialize};

use super::{internal_error, AdminState};

/// 每页默认条数(前端未显式传 page_size/limit 时)。
const DEFAULT_PAGE_SIZE: i64 = 50;
/// 每页条数硬上限(钳制客户端传入,防一次请求放大查询/JSON;与环形容量同量级)。
const MAX_LIST_LIMIT: i64 = 2000;

#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    #[serde(default)]
    days: Option<i64>,
    #[serde(default)]
    all: Option<bool>,
    #[serde(default)]
    from: Option<i64>,
    #[serde(default)]
    to: Option<i64>,
    /// 按上游账号筛选(空串=不限)。
    #[serde(default)]
    account: Option<String>,
    /// 按模型筛选(空串=不限)。
    #[serde(default)]
    model: Option<String>,
    /// `true`=只看成功;`false`=只看失败;缺省=全部。
    #[serde(default)]
    success: Option<bool>,
    /// 页码(1 起;缺省 1)。
    #[serde(default)]
    page: Option<i64>,
    /// 每页条数(缺省 DEFAULT_PAGE_SIZE,钳 [1, MAX_LIST_LIMIT]);兼容旧 `limit` 参数。
    #[serde(default)]
    page_size: Option<i64>,
    /// 兼容旧客户端:等价于 page_size。
    #[serde(default)]
    limit: Option<i64>,
}

impl LogsQuery {
    /// 解析分页:返回 (page≥1, page_size∈[1,MAX])。page_size 优先 page_size > limit > 默认。
    fn paging(&self) -> (i64, i64) {
        let page_size = self
            .page_size
            .or(self.limit)
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_LIST_LIMIT);
        let page = self.page.unwrap_or(1).max(1);
        (page, page_size)
    }

    fn to_filter(&self) -> RequestLogFilter {
        let (since, until) = if self.from.is_some() || self.to.is_some() {
            (self.from, self.to)
        } else if self.all.unwrap_or(false) {
            (None, None)
        } else {
            let days = self.days.unwrap_or(7).clamp(0, 3650);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            (Some(now.saturating_sub(days.saturating_mul(86_400))), None)
        };
        let (page, page_size) = self.paging();
        RequestLogFilter {
            since_unix: since,
            until_unix: until,
            account_id: self.account.clone().filter(|s| !s.is_empty()),
            model: self.model.clone().filter(|s| !s.is_empty()),
            success: self.success,
            limit: page_size,
            offset: (page - 1).saturating_mul(page_size),
        }
    }
}

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/logs", get(list))
        .route("/logs/{id}", get(detail))
}

/// 日志行 + 积分反推命中比例(诊断字段,不落库;非 Kiro / 不满足反推前提 → null)。
#[derive(Serialize)]
struct WithCreditHit<T: Serialize> {
    #[serde(flatten)]
    row: T,
    credit_hit_ratio: Option<f64>,
}

/// 当前生效的缓存计费参数(YAML 基线 + DB 热调 overlay),与 worker 收尾时用的同一份口径。
/// 读 overlay 失败 → 只用基线(诊断字段,不值得让整页报错)。
fn current_cache_policy(st: &AdminState) -> CachePolicy {
    let mut cfg = (*st.yaml_config).clone();
    if let Ok(overlay) = super::settings::read_overlay(st) {
        overlay.apply_to(&mut cfg);
    }
    serde_json::to_value(&cfg.cache)
        .map(|v| CachePolicy::from_cfg(&v))
        .unwrap_or_default()
}

/// 按积分反推本条的缓存命中比例。日志里的输入/输出在 `cache_io_correction` 开启时
/// 就是反推口径;关闭时输入多含本轮输出,结果略偏冷(生产已开)。
fn credit_hit_ratio(policy: &CachePolicy, row: &RequestLogRow) -> Option<f64> {
    let kiro_model = gw_kiro::converter::map_model(&row.model)?;
    let to_i32 = |n: i64| n.clamp(0, i32::MAX as i64) as i32;
    hit_ratio_from_credit(
        policy,
        &kiro_model,
        to_i32(row.input_tokens),
        to_i32(row.output_tokens),
        row.metering_credit,
    )
    .ok()
}

async fn list(State(st): State<AdminState>, Query(q): Query<LogsQuery>) -> axum::response::Response {
    let filter = q.to_filter();
    let (page, page_size) = q.paging();
    let items = match st.store.list_request_logs(&filter, DEFAULT_PAGE_SIZE) {
        Ok(rows) => rows,
        Err(e) => return internal_error(e),
    };
    let policy = current_cache_policy(&st);
    let items: Vec<_> = items
        .into_iter()
        .map(|row| WithCreditHit {
            credit_hit_ratio: credit_hit_ratio(&policy, &row),
            row,
        })
        .collect();
    let total = match st.store.count_request_logs(&filter) {
        Ok(n) => n,
        Err(e) => return internal_error(e),
    };
    Json(serde_json::json!({
        "items": items,
        "total": total,
        "page": page,
        "page_size": page_size,
    }))
    .into_response()
}

async fn detail(State(st): State<AdminState>, Path(id): Path<i64>) -> axum::response::Response {
    match st.store.get_request_log(id) {
        Ok(Some(d)) => {
            let credit_hit_ratio = credit_hit_ratio(&current_cache_policy(&st), &d.row);
            Json(WithCreditHit { row: d, credit_hit_ratio }).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"type":"error","error":{"message":"请求日志不存在"}})),
        )
            .into_response(),
        Err(e) => internal_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(model: &str, input: i64, output: i64, credit: f64) -> RequestLogRow {
        RequestLogRow {
            id: 1,
            created_at: 0,
            client_key_id: String::new(),
            account_id: "a".into(),
            model: model.into(),
            stream: true,
            success: true,
            status_code: Some(200),
            error_kind: None,
            duration_ms: None,
            ttfb_ms: None,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            reported_tokens: 0,
            real_cache_read_tokens: 0,
            metering_credit: credit,
        }
    }

    #[test]
    fn credit_hit_ratio_matches_billing_inference() {
        let p = CachePolicy::default();
        // 2026-10-08 生产真实请求:96 万输入只扣 3.26 积分 → 全热(全冷约 11.8)。
        let hot = credit_hit_ratio(&p, &row("claude-opus-5-5", 961_466, 166, 3.255_764)).unwrap();
        assert!(hot > 0.99, "hot={hot}");
        // 同样输入按全冷扣费 → 冷。
        let cold = credit_hit_ratio(&p, &row("claude-opus-5-5", 961_466, 166, 11.8)).unwrap();
        assert!(cold < 0.05, "cold={cold}");
    }

    #[test]
    fn credit_hit_ratio_none_without_preconditions() {
        let p = CachePolicy::default();
        assert_eq!(credit_hit_ratio(&p, &row("claude-opus-5-5", 961_466, 166, 0.0)), None, "无积分(非 Kiro)");
        assert_eq!(credit_hit_ratio(&p, &row("composer-2.5", 961_466, 166, 3.0)), None, "映射不到 Kiro 模型");
        assert_eq!(credit_hit_ratio(&p, &row("claude-opus-5-5", 961_466, 5_000, 9.0)), None, "长输出");
        assert_eq!(credit_hit_ratio(&p, &row("claude-opus-5-5", 500, 10, 0.1)), None, "输入太小");
    }
}
