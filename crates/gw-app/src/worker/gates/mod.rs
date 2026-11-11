//! 请求入口策略(编排层):在**选号之前**对一条请求做处置 —— 本地直答、拦截、改写。
//!
//! 与上游无关的「要不要把这条请求放给号」都在这里;各上游自己的处理在各自的包里,
//! 调度在 `worker/scheduler.rs`。这些模块带进程级热应用状态(全局配置、计数表),
//! 所以放在 gw-app 而不是 gw-core(gw-core 只放无状态的通用件;配置类型在
//! `gw_core::config::{automode, batch_guard}`)。
//!
//! 入口顺序(见 `worker::handle_chat`):分组头校验 → [`automode`] 分类器直答 →
//! [`batch_guard`] 批量任务 → [`request_screen`] 逐条筛查 → [`refusal_guard`] 拒答记忆
//! (本地拒答 / 钉号)→ 选号。拒答记忆的记账在请求日志收口([`refusal_guard_record`])。

pub mod automode;
pub mod batch_guard;
pub mod refusal_guard;
pub mod request_screen;

// ───────── 闸门:命中即在选号前回 400(记请求日志,不落到任何号上) ─────────

use super::*;

/// 自算状态码的错误(入站校验失败、选号失败)→ wire-aware 响应体。
///
/// Anthropic 分支与手写 `json!({"type":"error", ...})` **完全等价**:
/// `error_type_for_status` 就是那些字面量的来源(400→invalid_request_error、
/// 500→api_error),换成它是为了让两种线缆共用同一张状态码→类型表,而不是各写一份。
/// 批量任务拦截闸门。放行返回 `None`;enforce 下命中返回已构造好的 400。
///
/// 为什么是 400:下游 new-api 默认对 401 自动禁用渠道、对 401–407/5xx 换渠道重试;400 两样
/// 都不触发 —— 既不会因为拦一个脚本把整条渠道下线,也不会被重试放大成多倍流量。
pub(super) fn batch_guard_gate(
    st: &Arc<WorkerState>,
    req: &ChatRequest,
    client_key: &str,
    wire: Wire,
    started_at: std::time::Instant,
) -> Option<axum::response::Response> {
    let batch_guard::Verdict::Batch { block, reason, head, count, newly_flagged } =
        batch_guard::check(&req.body, std::time::Instant::now())
    else {
        return None;
    };
    if newly_flagged {
        tracing::warn!(
            model = %req.model, reason = ?reason, count, enforce = block, head = %head,
            "批量任务拦截:判定一个提示词模板为批量任务"
        );
    }
    if !block {
        return None;
    }
    let message = batch_guard::REJECT_MESSAGE;
    let elapsed = started_at.elapsed().as_millis() as i64;
    spawn_request_log_blocking(
        st,
        req.clone(),
        // 没有账号:这条请求没有落到任何号上。
        None,
        client_key.to_string(),
        req.stream,
        false,
        Some(400),
        Some(batch_guard::LOG_KIND_BLOCKED.to_string()),
        Some(elapsed),
        None,
        None,
        ResponseLog::Error(serde_json::json!({"type":"error","error":{
            "type": error_type_for_status(StatusCode::BAD_REQUEST), "message": message}})),
    );
    Some(error_response(wire, StatusCode::BAD_REQUEST, message))
}

/// 号池熔断闸门。熔断中(enforce)回 503 + 说明。
///
/// 为什么是 503:下游 new-api 对 5xx 换渠道重试,客户流量会转去别的渠道;400 不会换渠道,
/// 客户就只能干等。503 也不在 new-api 自动禁用渠道的状态码里(那是 401),恢复后无需人工启用渠道。
///
/// **不写请求日志**,只在内存计数(`/health` 的 `pool_breaker.blocked_total`):请求日志只留
/// 最新 2000 行,熔断后全量 503 加上 new-api 的重试几分钟就能把它冲满,而查「是哪个负载在封号」
/// 靠的正是熔断**之前**那批日志(审查 11-09)。
pub(super) fn pool_breaker_gate(wire: Wire) -> Option<axum::response::Response> {
    let message = super::pool_breaker::blocking_message()?;
    Some(error_response(wire, StatusCode::SERVICE_UNAVAILABLE, &message))
}

/// 逐条筛查闸门(探针 / 超大输入)。只筛带客户 key 的请求;observe 只记日志不拦。
pub(super) fn request_screen_gate(
    st: &Arc<WorkerState>,
    req: &ChatRequest,
    client_key: &str,
    wire: Wire,
    started_at: std::time::Instant,
) -> Option<axum::response::Response> {
    if client_key.is_empty() {
        return None;
    }
    let request_screen::Verdict::Hit { rule, block, est_tokens } =
        request_screen::check(&req.body, &gw_core::text_tokens::count_tokens)
    else {
        return None;
    };
    tracing::warn!(
        model = %req.model, rule = ?rule, enforce = block, est_tokens,
        max_tokens = ?req.body.get("max_tokens"),
        "逐条筛查:命中{}", if block { ",已拒绝" } else { "(observe,放行)" }
    );
    if !block {
        return None;
    }
    let message = rule.message();
    let elapsed = started_at.elapsed().as_millis() as i64;
    spawn_request_log_blocking(
        st,
        req.clone(),
        // 没有账号:这条请求没有落到任何号上。
        None,
        client_key.to_string(),
        req.stream,
        false,
        Some(400),
        Some(rule.log_kind().to_string()),
        Some(elapsed),
        None,
        None,
        ResponseLog::Error(serde_json::json!({"type":"error","error":{
            "type": error_type_for_status(StatusCode::BAD_REQUEST), "message": message}})),
    );
    Some(error_response(wire, StatusCode::BAD_REQUEST, message))
}

// ───────── 拒答记忆:被上游拒答过的同一条请求,重发钉回原号、超次数本地回拒答 ─────────

/// 本次请求被拒答记忆钉在哪个号上(enforce 下才有)。
#[derive(Debug, Clone)]
pub(super) struct RefusalPin {
    pub account_id: String,
    pub strikes: u32,
    /// 上次的拒答原文:钉的号用不了时拿它本地回拒答。
    pub message: String,
}

/// 拒答记忆闸门(选号之前)。`Ok(None)` 照常选号;`Ok(Some(pin))` 只许用 `pin.account_id`;
/// `Err(resp)` 已构造好的本地拒答(装箱:响应体比放行结果大得多)。observe 只记日志,永远 `Ok(None)`。
pub(super) fn refusal_guard_gate(
    st: &Arc<WorkerState>,
    req: &ChatRequest,
    client_key: &str,
    wire: Wire,
    started_at: std::time::Instant,
) -> Result<Option<RefusalPin>, Box<axum::response::Response>> {
    match refusal_guard::check(&req.model, &req.body, std::time::Instant::now()) {
        refusal_guard::Verdict::Pass => Ok(None),
        refusal_guard::Verdict::Pin { account_id, strikes, message, enforce } => {
            tracing::warn!(
                model = %req.model, account = %account_id, strikes, enforce,
                "拒答记忆:同一条请求被拒答过,{}", if enforce { "钉回原号" } else { "本应钉回原号(observe,照常选号)" }
            );
            Ok(enforce.then_some(RefusalPin { account_id, strikes, message }))
        }
        refusal_guard::Verdict::Block { strikes, message, enforce } => {
            tracing::warn!(
                model = %req.model, strikes, enforce,
                "拒答记忆:同一条请求已被拒答 {strikes} 次,{}", if enforce { "本地回拒答" } else { "本应本地回拒答(observe,放行)" }
            );
            if !enforce {
                return Ok(None);
            }
            Err(Box::new(refusal_repeat_response(st, req, client_key, wire, started_at, &message, strikes)))
        }
    }
}

/// 拒答记忆的本地拒答:400 + 上游原文 + 网关说明,记请求日志(没有落到任何号上)。
///
/// 为什么是 400:与上游拒答本身同码(`UpstreamErrorKind::ContentFiltered` → 400),下游
/// new-api 对 400 既不换渠道重试、也不自动禁用渠道。
pub(super) fn refusal_repeat_response(
    st: &Arc<WorkerState>,
    req: &ChatRequest,
    client_key: &str,
    wire: Wire,
    started_at: std::time::Instant,
    upstream_message: &str,
    strikes: u32,
) -> axum::response::Response {
    let message = refusal_guard::reject_message(upstream_message, strikes);
    let elapsed = started_at.elapsed().as_millis() as i64;
    spawn_request_log_blocking(
        st,
        req.clone(),
        None,
        client_key.to_string(),
        req.stream,
        false,
        Some(400),
        Some(refusal_guard::LOG_KIND_REPEAT.to_string()),
        Some(elapsed),
        None,
        None,
        ResponseLog::Error(serde_json::json!({"type":"error","error":{
            "type": error_type_for_status(StatusCode::BAD_REQUEST), "message": message}})),
    );
    error_response(wire, StatusCode::BAD_REQUEST, &message)
}

/// 拒答记忆的记账。请求收尾有两个收口,**两处都调**,且都在「库降级就跳过落库」之前
/// (库挂了记账照常):
/// - 请求日志收口 [`spawn_request_log_blocking`] —— 流式收尾、首包前失败、选号失败、本地拦截;
/// - 非流式收尾(`response.rs` 折叠完直接落库,不经过上面那个函数)。
///
/// `client_message` = 客户收到的错误文案(拒答时就是上游类别 + 说明),本地拒答沿用它。
pub(super) fn refusal_guard_record(
    req: &ChatRequest,
    account: Option<&Account>,
    success: bool,
    error_kind: Option<&str>,
    client_message: Option<&str>,
) {
    let outcome = match (success, error_kind, account) {
        (true, _, _) => refusal_guard::Outcome::Answered,
        (false, Some(CONTENT_FILTERED_KIND), Some(acc)) => {
            refusal_guard::Outcome::Refused { account_id: &acc.account_id, message: client_message }
        }
        _ => refusal_guard::Outcome::Other,
    };
    let change = refusal_guard::on_outcome(&req.model, &req.body, outcome, std::time::Instant::now());
    match change {
        Some(refusal_guard::RecordChange::Struck { account_id, strikes }) => {
            tracing::warn!(model = %req.model, account = %account_id, strikes, "拒答记忆:记下一次拒答");
        }
        Some(refusal_guard::RecordChange::Cleared { account_id, strikes }) => {
            tracing::info!(model = %req.model, account = %account_id, strikes, "拒答记忆:重发拿到回复,清除记录");
        }
        None => {}
    }
}
