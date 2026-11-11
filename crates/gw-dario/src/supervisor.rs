//! dario-supervisor 客户端 + 凭证交接(管理器见 `sidecars/dario-supervisor/`)。
//!
//! 每个 claude-dario 号对应一个原版 dario 进程,由 dario **独占刷新**令牌。caio 只做三件事:
//! 1. **交接**:号第一次被用到(或重新登录、refresh_token 变了)时,把凭证 PUT 给管理器,
//!    随即经 [`Supervisor::drain_updates`] → worker 落库,**清空 caio 侧的 refresh_token**、
//!    记下 `dario_rev`。两方刷新同一条 refresh token 会触发 Anthropic 复用检测、令牌一起作废,
//!    清空是「只有一方能刷新」的物理保证;
//! 2. **同步出口**:caio 里该号的 proxy 与管理器不一致 → PATCH(管理器停进程,下次以新出口启动);
//! 3. **路由**:每个请求先 `ensure` 拿到该号进程的端口(顺带刷新管理器的闲置计时),再把
//!    `/v1/messages` 发过去。
//!
//! rev = refresh_token 的 sha256 前缀:同一个 refresh_token 永远同一个 rev,管理器据此判断
//! 「重复交接(不动令牌)」与「重新登录(替换)」。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gw_core::account::Account;
use gw_core::error::{RequestDelivery, UpstreamError, UpstreamErrorKind};
use sha2::{Digest, Sha256};

/// caio 侧记录「已交接给 dario 的凭证版本」的 extra 键。
pub const HANDOVER_REV_KEY: &str = "dario_rev";
/// 交接时间(RFC3339 Z),仅供面板/排障查看。
pub const HANDOVER_AT_KEY: &str = "dario_handover_at";
/// 交接/出口同步的结果缓存时长:窗口内同一 (rev, proxy) 不再问管理器。
const PROVISION_TTL: Duration = Duration::from_secs(60);
/// 管理器单次调用上限。`ensure` 冷启动最多等 15s,停进程(改代理/重新登录)最多 95s。
const SUPERVISOR_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) fn rev_of(refresh_token: &str) -> String {
    let digest = Sha256::digest(refresh_token.trim().as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("rt-{hex}")
}

/// 管理器返回的单号视图(只取用得到的字段)。
#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
pub(crate) struct Remote {
    pub rev: String,
    pub port: u16,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default, rename = "needsRelogin")]
    pub needs_relogin: bool,
    /// 管理器实际存下、该号 dario 对 Anthropic 呈现的身份(权威值)。
    #[serde(default, rename = "deviceId")]
    pub device_id: Option<String>,
    #[serde(default, rename = "accountUuid")]
    pub account_uuid: Option<String>,
}

/// 管理器侧出错都发生在推理请求发出之前:标 `NotSubmitted`,worker 可放心换号。
fn sup_err(kind: UpstreamErrorKind, msg: impl Into<String>) -> UpstreamError {
    UpstreamError::new(kind, msg).with_delivery(RequestDelivery::NotSubmitted)
}

fn unreachable(e: reqwest::Error) -> UpstreamError {
    sup_err(UpstreamErrorKind::ServerError, format!("dario 管理器不可达: {e}"))
}

/// hub 给出的「子机不可用」标记 → 错误。
///
/// **三种标记一律用 `RateLimited`,绝不用 ServerError / Network。** 后两者在
/// `scheduler::report_failure` 里同属一个分支,会累加 `failure_count`,满
/// `max_failures`(默认 5)就把号打成 `TooManyFailures` **永久禁用**。子机按设计跑在
/// NAT 后的小机器上,WAN 抖动是常态:一台子机连抖 5 次,就会把它上面那些**完好的**号
/// 在主机侧禁掉,而且子机恢复后不会自愈 —— 远端基础设施的状态不该写进账号的病历。
/// `RateLimited` 的语义正好:这个号现在用不了(它的机器不在)、过一会儿自己恢复、
/// 不计入账号健康。
///
/// 区别只在**投递语义**:离线/禁用时请求一个字节都没发出去 → `NotSubmitted`,可以放心
/// 换号重发;`Lost` 是请求已经进了隧道才断的 → `Unknown`,不能重发(子机可能已经执行、
/// 已经烧了上游额度),这条请求就地失败。
pub(crate) fn node_mark_error(mark: gw_core::node::NodeMark, detail: &str) -> UpstreamError {
    let msg = format!("dario 子机不可用({}): {}", mark.as_str(), detail.chars().take(200).collect::<String>());
    let delivery = if mark.not_submitted() { RequestDelivery::NotSubmitted } else { RequestDelivery::Unknown };
    UpstreamError::new(UpstreamErrorKind::RateLimited, msg).with_delivery(delivery)
}

/// 响应头里的 hub 标记(没有 = 这是子机/管理器自己的回答)。
pub(crate) fn node_mark(headers: &reqwest::header::HeaderMap) -> Option<gw_core::node::NodeMark> {
    headers
        .get(gw_core::node::HDR_NODE)
        .and_then(|v| v.to_str().ok())
        .and_then(gw_core::node::NodeMark::parse)
}

/// 管理器 `ensure` 的非 200 回答 → 错误(本机直连与子机代为 ensure 共用)。
///
/// `on_node`:这次 ensure 是**子机上**的管理器做的。此时的失败(管理器崩了、端口段耗尽、
/// 磁盘写不下凭证……)是那台远端机器的状态,不是这个号的健康问题 —— 按 ServerError 记
/// 会累加 `failure_count` 把号禁掉(同 [`node_mark_error`] 的理由),所以改用
/// `RateLimited`:换号、冷一会儿、不进账号病历。409(令牌真的失效)仍然照实报。
pub(crate) fn ensure_failure(id: &str, code: u16, text: &str, on_node: bool) -> UpstreamError {
    let infra = |msg: String| {
        if on_node {
            UpstreamError::new(UpstreamErrorKind::RateLimited, msg)
                .with_delivery(RequestDelivery::NotSubmitted)
        } else {
            sup_err(UpstreamErrorKind::ServerError, msg)
        }
    };
    match code {
        409 => relogin(id, "dario 判定 refresh token 已失效"),
        404 => infra(format!("dario 管理器里没有号 {id}(稍后重新交接)")),
        _ => infra(format!("dario 管理器 ensure {code}: {text}")),
    }
}

fn relogin(id: &str, why: &str) -> UpstreamError {
    sup_err(
        UpstreamErrorKind::TokenInvalid,
        format!("dario 托管的号 {id} 需要重新登录({why})"),
    )
}

pub(crate) struct Supervisor {
    base: reqwest::Url,
    /// 子机上的管理器(经 hub 访问)时为子机 ID;本机管理器为 `None`。
    node: Option<String>,
    api_key: String,
    client: reqwest::Client,
    /// account_id → (「rev|proxy」, 时间)。命中且未过期则跳过交接检查。
    provisioned: Mutex<HashMap<String, (String, Instant)>>,
    /// 待 worker 落库的 extra 增量(清空 refresh_token、记 rev 等)。
    pending: Mutex<Vec<(String, BTreeMap<String, serde_json::Value>)>>,
    /// 按号串行 provision:chat 与额度轮询可能同时首次交接同一个号。
    provision_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Supervisor {
    pub fn new(base: &str, api_key: &str, client: reqwest::Client) -> anyhow::Result<Self> {
        let base = reqwest::Url::parse(base.trim())
            .map_err(|e| anyhow::anyhow!("dario.sidecar_url 不是合法 URL: {e}"))?;
        if base.host_str().unwrap_or("").is_empty() {
            anyhow::bail!("dario.sidecar_url 缺 host");
        }
        Ok(Self {
            base,
            node: None,
            api_key: api_key.to_string(),
            client,
            provisioned: Mutex::new(HashMap::new()),
            pending: Mutex::new(Vec::new()),
            provision_locks: Mutex::new(HashMap::new()),
        })
    }

    /// 子机上的管理器:经 hub 本地口 `<hub>/n/<节点>/sup` 访问。
    pub fn for_node(hub_url: &str, node_id: &str, api_key: &str, client: reqwest::Client) -> anyhow::Result<Self> {
        if !gw_core::node::valid_node_id(node_id) {
            anyhow::bail!("子机 ID 非法: {node_id:?}");
        }
        let base = format!("{}/n/{node_id}/sup", hub_url.trim().trim_end_matches('/'));
        let mut sup = Self::new(&base, api_key, client)?;
        sup.node = Some(node_id.to_string());
        Ok(sup)
    }

    #[cfg(test)]
    pub fn node(&self) -> Option<&str> {
        self.node.as_deref()
    }

    /// 子机号的对话地址:`<hub>/n/<节点>/a/<号>/<path>`,由子机在本机 ensure 后转给 dario。
    pub fn node_chat_url(&self, id: &str, path: &str) -> String {
        let mut url = self.base.clone();
        {
            let mut seg = url.path_segments_mut().expect("http(s) URL 可加路径段");
            seg.pop_if_empty().pop(); // 去掉末尾的 "sup"
            seg.push("a").push(id);
            seg.extend(path.trim_start_matches('/').split('/'));
        }
        url.to_string()
    }

    /// 管理器调用的通用失败:先认 hub 标记,再按状态码。
    fn http_error(&self, headers: &reqwest::header::HeaderMap, what: &str, code: u16, text: &str) -> UpstreamError {
        if let Some(mark) = node_mark(headers) {
            return node_mark_error(mark, text);
        }
        // 400 = 我们给的东西不合法(缺 deviceId、socks 代理……),换号也一样,不惩罚账号健康。
        if code == 400 {
            return sup_err(UpstreamErrorKind::BadRequest, format!("dario 管理器 {what} {code}: {text}"));
        }
        let msg = format!("dario 管理器 {what} {code}: {text}");
        // 子机上的管理器出错 = 远端基础设施,不计入账号健康(同 node_mark_error 的理由)。
        if self.node.is_some() {
            return UpstreamError::new(UpstreamErrorKind::RateLimited, msg)
                .with_delivery(RequestDelivery::NotSubmitted);
        }
        sup_err(UpstreamErrorKind::ServerError, msg)
    }

    /// 连不上管理器:本机是 ServerError;子机号连不上的是 hub(主机本地口),同样按「没发出去」换号。
    fn unreachable(&self, e: reqwest::Error) -> UpstreamError {
        match &self.node {
            None => unreachable(e),
            Some(n) => node_mark_error(gw_core::node::NodeMark::Offline, &format!("hub 不可达(子机 {n}): {e}")),
        }
    }

    fn account_url(&self, id: &str, ensure: bool) -> reqwest::Url {
        let mut url = self.base.clone();
        {
            let mut seg = url.path_segments_mut().expect("http(s) URL 可加路径段");
            seg.pop_if_empty().push("accounts").push(id);
            if ensure {
                seg.push("ensure");
            }
        }
        url
    }

    /// 该号 dario 进程上的某个路径。进程与管理器同机,host 取管理器的。
    pub fn process_url(&self, port: u16, path: &str) -> String {
        let host = self.base.host_str().unwrap_or("127.0.0.1");
        // url 的 host_str 对 IPv6 已带方括号,别再包一层。
        let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
        format!("http://{host}:{port}{path}")
    }

    pub(crate) fn forget(&self, id: &str) {
        self.provisioned.lock().unwrap_or_else(|p| p.into_inner()).remove(id);
    }

    async fn get(&self, id: &str) -> Result<Option<Remote>, UpstreamError> {
        let resp = self
            .client
            .get(self.account_url(id, false))
            .header("x-api-key", &self.api_key)
            .timeout(SUPERVISOR_TIMEOUT)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        let code = resp.status().as_u16();
        if node_mark(resp.headers()).is_none() {
            match code {
                404 => return Ok(None),
                200 => {
                    return resp.json::<Remote>().await.map(Some).map_err(|e| {
                        sup_err(UpstreamErrorKind::ServerError, format!("dario 管理器返回无法解析: {e}"))
                    })
                }
                _ => {}
            }
        }
        let headers = resp.headers().clone();
        let text = resp.text().await.unwrap_or_default();
        Err(self.http_error(&headers, "GET", code, &text))
    }

    async fn send_json(
        &self,
        method: reqwest::Method,
        id: &str,
        body: &serde_json::Value,
    ) -> Result<Remote, UpstreamError> {
        let resp = self
            .client
            .request(method.clone(), self.account_url(id, false))
            .header("x-api-key", &self.api_key)
            .timeout(SUPERVISOR_TIMEOUT)
            .json(body)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        let code = resp.status().as_u16();
        if code == 200 && node_mark(resp.headers()).is_none() {
            return resp.json::<Remote>().await.map_err(|e| {
                sup_err(UpstreamErrorKind::ServerError, format!("dario 管理器返回无法解析: {e}"))
            });
        }
        let headers = resp.headers().clone();
        let text = resp.text().await.unwrap_or_default();
        Err(self.http_error(&headers, method.as_str(), code, &text))
    }

    /// 确保该号进程在跑,返回端口。每个请求调用一次。
    pub async fn ensure(&self, id: &str) -> Result<u16, UpstreamError> {
        let resp = self
            .client
            .post(self.account_url(id, true))
            .header("x-api-key", &self.api_key)
            .timeout(SUPERVISOR_TIMEOUT)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        let code = resp.status().as_u16();
        let mark = node_mark(resp.headers());
        let text = resp.text().await.unwrap_or_default();
        if let Some(mark) = mark {
            return Err(node_mark_error(mark, &text));
        }
        match code {
            200 => serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("port").and_then(|p| p.as_u64()))
                .and_then(|p| u16::try_from(p).ok())
                .ok_or_else(|| sup_err(UpstreamErrorKind::ServerError, format!("dario 管理器 ensure 返回无端口: {text}"))),
            409 | 404 => {
                self.forget(id);
                Err(ensure_failure(id, code, &text, self.node.is_some()))
            }
            _ => Err(ensure_failure(id, code, &text, self.node.is_some())),
        }
    }

    /// 交接凭证(必要时)+ 同步出口代理。幂等;成功结果缓存 [`PROVISION_TTL`]。
    pub async fn provision(&self, account: &Account) -> Result<(), UpstreamError> {
        let id = account.account_id.as_str();
        let proxy = normalized_proxy(account)?;
        // 子机号用子机本机 IP 直连:再配代理就等于换了出口,拒绝而不是默默照做。
        if self.node.is_some() && proxy.is_some() {
            return Err(sup_err(
                UpstreamErrorKind::BadRequest,
                format!("号 {id} 在子机上,不能再设出口代理(请清空 proxy)"),
            ));
        }
        let refresh_token = account.extra_str("refresh_token").map(str::trim).filter(|s| !s.is_empty());
        let handed_rev = account.extra_str(HANDOVER_REV_KEY).map(str::trim).filter(|s| !s.is_empty());
        let local_rev = refresh_token.map(rev_of).or_else(|| handed_rev.map(str::to_string));
        let cache_key = format!("{}|{}", local_rev.as_deref().unwrap_or("-"), proxy.as_deref().unwrap_or("-"));
        let fresh = |s: &Self| {
            let cache = s.provisioned.lock().unwrap_or_else(|p| p.into_inner());
            cache.get(id).is_some_and(|(k, at)| *k == cache_key && at.elapsed() < PROVISION_TTL)
        };
        if fresh(self) {
            return Ok(());
        }
        // 同一个号串行:否则两路首次交接会各自生成一套身份,caio 可能记下与 dario 实际所用不同的那套。
        let lock = self
            .provision_locks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(id.to_string())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        if fresh(self) {
            return Ok(());
        }

        let mut remote = self.get(id).await?;
        match (refresh_token, &remote) {
            (Some(rt), r) => {
                let rev = rev_of(rt);
                if r.as_ref().map(|r| r.rev != rev).unwrap_or(true) {
                    let (body, _) = handover_body(account, rt, &rev, proxy.as_deref());
                    let put = self.send_json(reqwest::Method::PUT, id, &body).await?;
                    tracing::info!(account = %id, rev = %rev, port = put.port, "dario 凭证已交接给管理器");
                    self.queue_handed_over(id, &rev, identity_delta(account, &put));
                    remote = Some(put);
                } else if let Some(r) = r {
                    // 已交接过、但 caio 侧还没清掉 refresh_token(上次落库前进程重启等):再排一次。
                    self.queue_handed_over(id, &rev, identity_delta(account, r));
                }
            }
            (None, None) => {
                return Err(relogin(id, "管理器里没有它,caio 侧也已无 refresh token"));
            }
            (None, Some(_)) => {}
        }
        if let Some(r) = &remote {
            if r.needs_relogin {
                return Err(relogin(id, "dario 判定 refresh token 已失效"));
            }
            if r.proxy.as_deref() != proxy.as_deref() {
                let body = serde_json::json!({ "proxy": proxy });
                self.send_json(reqwest::Method::PATCH, id, &body).await?;
                tracing::info!(account = %id, "dario 出口代理已同步到管理器");
            }
        }
        self.provisioned
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_string(), (cache_key, Instant::now()));
        Ok(())
    }

    /// 交接成功:清空 caio 侧 refresh_token、记 rev 与时间;顺带把身份字段对齐到管理器的权威值。
    fn queue_handed_over(&self, id: &str, rev: &str, mut delta: BTreeMap<String, serde_json::Value>) {
        delta.insert("refresh_token".into(), serde_json::json!(""));
        delta.insert(HANDOVER_REV_KEY.into(), serde_json::json!(rev));
        delta.insert(HANDOVER_AT_KEY.into(), serde_json::json!(crate::format_rfc3339_z(now_unix())));
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).push((id.to_string(), delta));
    }

    pub fn drain_updates(&self) -> Vec<(String, BTreeMap<String, serde_json::Value>)> {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

/// caio 侧身份字段与管理器实际存下的不一致(或缺失)时,以管理器为准回写。
/// 管理器没回传(旧版本)则不动。
fn identity_delta(account: &Account, remote: &Remote) -> BTreeMap<String, serde_json::Value> {
    let mut d = BTreeMap::new();
    for (key, val) in [("device_id", &remote.device_id), ("account_uuid", &remote.account_uuid)] {
        if let Some(v) = val.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            if account.extra_str(key) != Some(v) {
                d.insert(key.to_string(), serde_json::json!(v));
            }
        }
    }
    d
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 该号的出口代理(规范化);非法/socks → BadRequest,绝不静默改用默认出口。
pub(crate) fn normalized_proxy(account: &Account) -> Result<Option<String>, UpstreamError> {
    match account.extra_str("proxy").map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => crate::normalize_dario_proxy(raw)
            .map(Some)
            .map_err(|e| sup_err(UpstreamErrorKind::BadRequest, format!("dario account extra.proxy 非法: {e}"))),
        None => Ok(None),
    }
}

/// 组交接请求体。缺的身份字段现场生成,随交接结果一并落库(返回值第二项)。
///
/// - access_token 缺失(只导入了 refresh token)→ 给占位串、expiresAt=1:dario 启动时发现
///   已过期,会先刷新再开端口;
/// - expires_at 解析不了 → 同样按已过期处理,交给 dario 刷新。
pub(crate) fn handover_body(
    account: &Account,
    refresh_token: &str,
    rev: &str,
    proxy: Option<&str>,
) -> (serde_json::Value, BTreeMap<String, serde_json::Value>) {
    let mut generated = BTreeMap::new();
    let mut id_field = |key: &str| -> String {
        match account.extra_str(key).map(str::trim).filter(|s| !s.is_empty()) {
            Some(v) => v.to_string(),
            None => {
                let v = uuid::Uuid::new_v4().to_string();
                generated.insert(key.to_string(), serde_json::json!(v));
                v
            }
        }
    };
    let device_id = id_field("device_id");
    let account_uuid = id_field("account_uuid");
    let access_token = account
        .extra_str("access_token")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("pending-refresh")
        .to_string();
    let expires_at_ms = account
        .extra_str("expires_at")
        .and_then(crate::datefmt::parse_rfc3339_z)
        .filter(|_| access_token != "pending-refresh")
        .map(|s| s.saturating_mul(1000))
        .filter(|ms| *ms > 0)
        .unwrap_or(1);
    let mut body = serde_json::json!({
        "rev": rev,
        "accessToken": access_token,
        "refreshToken": refresh_token,
        "expiresAt": expires_at_ms,
        "deviceId": device_id,
        "accountUuid": account_uuid,
        "proxy": proxy,
    });
    if let Some(granted) = account.extra_str("granted_at").and_then(crate::datefmt::parse_rfc3339_z) {
        body["grantedAt"] = serde_json::json!(granted.saturating_mul(1000));
    }
    (body, generated)
}

/// 删除管理器里的号(router admin 删号时调用)。管理器没有这个号也算成功。
pub async fn delete_account(base: &str, api_key: &str, id: &str) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(SUPERVISOR_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let sup = Supervisor::new(base, api_key, client.clone()).map_err(|e| e.to_string())?;
    let resp = client
        .delete(sup.account_url(id, false))
        .header("x-api-key", api_key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    match resp.status().as_u16() {
        200 | 404 => Ok(()),
        code => Err(format!("{code}: {}", resp.text().await.unwrap_or_default())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::sync::Arc;

    /// 模拟 dario-supervisor:记录收到的每个调用,行为由 `accounts` 与 `ensure_status` 决定。
    #[derive(Default)]
    struct Mock {
        accounts: Mutex<HashMap<String, serde_json::Value>>,
        calls: Mutex<Vec<String>>,
        bodies: Mutex<Vec<serde_json::Value>>,
        ensure_status: Mutex<u16>,
        keys: Mutex<Vec<String>>,
    }
    type S = Arc<Mock>;

    fn note(s: &S, h: &HeaderMap, call: String) {
        s.calls.lock().unwrap().push(call);
        s.keys.lock().unwrap().push(h.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or("").to_string());
    }

    async fn get_one(State(s): State<S>, h: HeaderMap, Path(id): Path<String>) -> axum::response::Response {
        note(&s, &h, format!("GET {id}"));
        match s.accounts.lock().unwrap().get(&id) {
            Some(v) => Json(v.clone()).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }
    async fn put_one(State(s): State<S>, h: HeaderMap, Path(id): Path<String>, Json(b): Json<serde_json::Value>) -> axum::response::Response {
        note(&s, &h, format!("PUT {id}"));
        s.bodies.lock().unwrap().push(b.clone());
        // 与真管理器一致:同 rev 重放不覆盖,回传首次存下的身份。
        let mut m = s.accounts.lock().unwrap();
        if let Some(prev) = m.get(&id) {
            if prev["rev"] == b["rev"] {
                return Json(prev.clone()).into_response();
            }
        }
        let v = serde_json::json!({"rev": b["rev"], "port": 18801, "proxy": b["proxy"], "needsRelogin": false,
            "deviceId": b["deviceId"], "accountUuid": b["accountUuid"]});
        m.insert(id, v.clone());
        Json(v).into_response()
    }
    async fn patch_one(State(s): State<S>, h: HeaderMap, Path(id): Path<String>, Json(b): Json<serde_json::Value>) -> axum::response::Response {
        note(&s, &h, format!("PATCH {id}"));
        s.bodies.lock().unwrap().push(b.clone());
        let mut m = s.accounts.lock().unwrap();
        let v = m.get_mut(&id).unwrap();
        v["proxy"] = b["proxy"].clone();
        Json(v.clone()).into_response()
    }
    async fn ensure(State(s): State<S>, h: HeaderMap, Path(id): Path<String>) -> axum::response::Response {
        note(&s, &h, format!("ENSURE {id}"));
        let code = *s.ensure_status.lock().unwrap();
        match code {
            200 => Json(serde_json::json!({"port": 18801})).into_response(),
            c => (StatusCode::from_u16(c).unwrap(), Json(serde_json::json!({"error": "x"}))).into_response(),
        }
    }

    async fn spawn_mock() -> (S, String) {
        let s: S = Arc::new(Mock::default());
        *s.ensure_status.lock().unwrap() = 200;
        let app = Router::new()
            .route("/accounts/{id}", get(get_one).put(put_one).patch(patch_one))
            .route("/accounts/{id}/ensure", post(ensure))
            .with_state(s.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (s, format!("http://{addr}"))
    }

    fn acct(id: &str, kv: &[(&str, &str)]) -> Account {
        let mut e = BTreeMap::new();
        for (k, v) in kv {
            e.insert((*k).to_string(), serde_json::json!(v));
        }
        Account { account_id: id.into(), provider: "claude-dario".into(), max_concurrency: 2, disabled: false, created_at: 0, extra: e }
    }

    fn sup(base: &str) -> Supervisor {
        Supervisor::new(base, "k-test", reqwest::Client::new()).unwrap()
    }

    #[test]
    fn rev_is_stable_and_token_specific() {
        assert_eq!(rev_of("rt-a"), rev_of(" rt-a "));
        assert_ne!(rev_of("rt-a"), rev_of("rt-b"));
        assert!(rev_of("rt-a").starts_with("rt-") && rev_of("rt-a").len() == 19);
    }

    #[test]
    fn handover_body_maps_fields_and_generates_missing_identity() {
        let a = acct("x@y.com", &[
            ("access_token", "at"), ("refresh_token", "rt"), ("expires_at", "2026-06-04T00:00:00Z"),
            ("device_id", "dev"), ("granted_at", "2026-06-01T00:00:00Z"),
        ]);
        let (b, generated) = handover_body(&a, "rt", "rt-1", Some("http://p:1/"));
        assert_eq!(b["accessToken"], "at");
        assert_eq!(b["refreshToken"], "rt");
        assert_eq!(b["expiresAt"], 1_780_531_200_000_i64);
        assert_eq!(b["deviceId"], "dev");
        assert_eq!(b["grantedAt"], 1_780_272_000_000_i64);
        assert_eq!(b["proxy"], "http://p:1/");
        // 缺 account_uuid → 现场生成,并随交接结果回写 caio。
        assert_eq!(generated.len(), 1);
        assert_eq!(b["accountUuid"], generated["account_uuid"]);
    }

    #[test]
    fn handover_body_without_access_token_lets_dario_refresh_first() {
        let a = acct("x", &[("refresh_token", "rt"), ("expires_at", "2030-01-01T00:00:00Z"), ("device_id", "d"), ("account_uuid", "u")]);
        let (b, _) = handover_body(&a, "rt", "rt-1", None);
        assert_eq!(b["accessToken"], "pending-refresh");
        assert_eq!(b["expiresAt"], 1, "按已过期交接,dario 启动时先刷新");
        assert!(b["proxy"].is_null());
        assert!(b.get("grantedAt").is_none());
    }

    #[test]
    fn process_url_uses_supervisor_host() {
        assert_eq!(sup("http://127.0.0.1:8788").process_url(18801, "/v1/messages"), "http://127.0.0.1:18801/v1/messages");
        assert_eq!(sup("http://[::1]:8788").process_url(1, "/x"), "http://[::1]:1/x");
    }

    #[tokio::test]
    async fn first_use_hands_over_then_clears_caio_refresh_token() {
        let (m, base) = spawn_mock().await;
        let s = sup(&base);
        let a = acct("a@b.com", &[("access_token", "at"), ("refresh_token", "rt-live"), ("device_id", "d"), ("account_uuid", "u"), ("proxy", "http://h:1")]);
        s.provision(&a).await.unwrap();
        assert_eq!(*m.calls.lock().unwrap(), vec!["GET a@b.com", "PUT a@b.com"]);
        let put = m.bodies.lock().unwrap()[0].clone();
        assert_eq!(put["rev"], rev_of("rt-live"));
        assert_eq!(put["proxy"], "http://h:1/", "代理规范化后交接");
        assert!(m.keys.lock().unwrap().iter().all(|k| k == "k-test"));
        let ups = s.drain_updates();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].0, "a@b.com");
        assert_eq!(ups[0].1["refresh_token"], "", "交接后 caio 侧清空 refresh_token");
        assert_eq!(ups[0].1[HANDOVER_REV_KEY], rev_of("rt-live"));
        assert!(ups[0].1.contains_key(HANDOVER_AT_KEY));
        assert!(s.drain_updates().is_empty(), "drain 语义:取走即清空");
        // 缓存窗口内同一 (rev, proxy) 不再问管理器。
        s.provision(&a).await.unwrap();
        assert_eq!(m.calls.lock().unwrap().len(), 2);
        // 落库后(refresh_token 已空、只剩 dario_rev)缓存键不变,仍命中。
        let handed = acct("a@b.com", &[("refresh_token", ""), (HANDOVER_REV_KEY, &rev_of("rt-live")), ("proxy", "http://h:1")]);
        s.provision(&handed).await.unwrap();
        assert_eq!(m.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn same_rev_never_puts_again_but_still_clears_local_token() {
        let (m, base) = spawn_mock().await;
        m.accounts.lock().unwrap().insert("a".into(), serde_json::json!({"rev": rev_of("rt-1"), "port": 18801, "proxy": null, "needsRelogin": false}));
        let s = sup(&base);
        s.provision(&acct("a", &[("refresh_token", "rt-1"), ("device_id", "d"), ("account_uuid", "u")])).await.unwrap();
        assert_eq!(*m.calls.lock().unwrap(), vec!["GET a"]);
        assert_eq!(s.drain_updates()[0].1["refresh_token"], "");
    }

    #[tokio::test]
    async fn relogin_with_new_refresh_token_replaces() {
        let (m, base) = spawn_mock().await;
        m.accounts.lock().unwrap().insert("a".into(), serde_json::json!({"rev": rev_of("rt-old"), "port": 18801, "proxy": null, "needsRelogin": true}));
        let s = sup(&base);
        s.provision(&acct("a", &[("refresh_token", "rt-new"), ("device_id", "d"), ("account_uuid", "u")])).await.unwrap();
        assert_eq!(*m.calls.lock().unwrap(), vec!["GET a", "PUT a"]);
        assert_eq!(m.bodies.lock().unwrap()[0]["rev"], rev_of("rt-new"));
    }

    #[tokio::test]
    async fn handed_over_but_missing_or_dead_means_relogin() {
        let (m, base) = spawn_mock().await;
        let s = sup(&base);
        let handed = acct("a", &[(HANDOVER_REV_KEY, "rt-x")]);
        let e = s.provision(&handed).await.unwrap_err();
        assert_eq!(e.kind, UpstreamErrorKind::TokenInvalid);
        assert!(!e.delivery.may_have_executed());
        m.accounts.lock().unwrap().insert("a".into(), serde_json::json!({"rev": "rt-x", "port": 18801, "proxy": null, "needsRelogin": true}));
        assert_eq!(s.provision(&handed).await.unwrap_err().kind, UpstreamErrorKind::TokenInvalid);
    }

    #[tokio::test]
    async fn proxy_drift_is_patched_and_socks_rejected_locally() {
        let (m, base) = spawn_mock().await;
        m.accounts.lock().unwrap().insert("a".into(), serde_json::json!({"rev": "rt-x", "port": 18801, "proxy": "http://old:1/", "needsRelogin": false}));
        let s = sup(&base);
        s.provision(&acct("a", &[(HANDOVER_REV_KEY, "rt-x"), ("proxy", "http://new:2")])).await.unwrap();
        assert_eq!(*m.calls.lock().unwrap(), vec!["GET a", "PATCH a"]);
        assert_eq!(m.bodies.lock().unwrap()[0]["proxy"], "http://new:2/");
        let e = s.provision(&acct("b", &[(HANDOVER_REV_KEY, "rt-x"), ("proxy", "socks5://h:1")])).await.unwrap_err();
        assert_eq!(e.kind, UpstreamErrorKind::BadRequest);
        assert_eq!(m.calls.lock().unwrap().len(), 2, "非法代理在本地就拒,不打扰管理器");
    }

    #[tokio::test]
    async fn ensure_maps_supervisor_answers() {
        let (m, base) = spawn_mock().await;
        let s = sup(&base);
        assert_eq!(s.ensure("a").await.unwrap(), 18801);
        *m.ensure_status.lock().unwrap() = 409;
        let e = s.ensure("a").await.unwrap_err();
        assert_eq!(e.kind, UpstreamErrorKind::TokenInvalid);
        assert!(!e.delivery.may_have_executed());
        *m.ensure_status.lock().unwrap() = 503;
        assert_eq!(s.ensure("a").await.unwrap_err().kind, UpstreamErrorKind::ServerError);
        *m.ensure_status.lock().unwrap() = 404;
        assert_eq!(s.ensure("a").await.unwrap_err().kind, UpstreamErrorKind::ServerError);
        // 管理器不可达:服务端错误,推理未发出,可以换号。
        let down = sup("http://127.0.0.1:9");
        let e = down.ensure("a").await.unwrap_err();
        assert_eq!(e.kind, UpstreamErrorKind::ServerError);
        assert!(!e.delivery.may_have_executed());
    }

    #[tokio::test]
    async fn concurrent_first_handover_puts_once_and_persists_supervisor_identity() {
        let (m, base) = spawn_mock().await;
        let s = std::sync::Arc::new(sup(&base));
        // 导入的号缺身份字段:每一路 handover_body 都会各自生成随机身份。
        let a = acct("new", &[("refresh_token", "rt-n")]);
        let (r1, r2, r3) = tokio::join!(s.provision(&a), s.provision(&a), s.provision(&a));
        assert!(r1.is_ok() && r2.is_ok() && r3.is_ok());
        let puts = m.calls.lock().unwrap().iter().filter(|c| c.starts_with("PUT")).count();
        assert_eq!(puts, 1, "按号串行:只交接一次");
        let stored = m.accounts.lock().unwrap()["new"].clone();
        let ups = s.drain_updates();
        assert!(!ups.is_empty());
        for (_, d) in &ups {
            assert_eq!(d["device_id"], stored["deviceId"], "回写的身份必须是管理器实际存下的那套");
            assert_eq!(d["account_uuid"], stored["accountUuid"]);
        }
    }

    #[tokio::test]
    async fn identity_drift_is_corrected_from_supervisor() {
        let (m, base) = spawn_mock().await;
        m.accounts.lock().unwrap().insert("a".into(), serde_json::json!({"rev": rev_of("rt-1"), "port": 18801, "proxy": null,
            "needsRelogin": false, "deviceId": "dev-A", "accountUuid": "uuid-A"}));
        let s = sup(&base);
        s.provision(&acct("a", &[("refresh_token", "rt-1"), ("device_id", "dev-X"), ("account_uuid", "uuid-A")])).await.unwrap();
        let ups = s.drain_updates();
        assert_eq!(ups[0].1["device_id"], "dev-A");
        assert!(!ups[0].1.contains_key("account_uuid"), "一致的字段不重复回写");
    }

    #[tokio::test]
    async fn account_ids_with_special_chars_are_path_encoded() {
        let (m, base) = spawn_mock().await;
        let s = sup(&base);
        s.provision(&acct("a b/c@d", &[("refresh_token", "rt"), ("device_id", "d"), ("account_uuid", "u")])).await.unwrap();
        assert_eq!(m.calls.lock().unwrap()[0], "GET a b/c@d");
    }
}
