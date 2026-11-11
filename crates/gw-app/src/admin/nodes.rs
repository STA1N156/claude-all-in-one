//! 子机(dario 节点)管理:新建(生成加入令牌与安装命令)、改名/禁用(排空)、吊销、解除机器绑定;
//! 列表合并库里的记录与 hub 的实时状态。另有给账号上号/删号用的几个辅助函数。
//!
//! 号绑在子机上之后不可迁移(令牌只能有一个持有者),所以吊销还有号的子机要显式 `force`。

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use gw_node::join::{self, JoinToken};
use serde::Deserialize;

use super::{internal_error, AdminState};

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/nodes", get(list_nodes).post(create_node))
        .route("/nodes/{id}", axum::routing::patch(update_node).delete(delete_node))
        .route("/nodes/{id}/reset-binding", post(reset_binding))
        .route("/nodes/{id}/exec", post(exec_on_node))
}

fn api_error(status: StatusCode, msg: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({"type": "error", "error": {"message": msg}}))).into_response()
}

pub(crate) fn hub_url(st: &AdminState) -> String {
    st.yaml_config.effective_node_hub_url().trim_end_matches('/').to_string()
}

fn hub_key(st: &AdminState) -> &str {
    st.yaml_config.dario.api_key.as_str()
}

/// hub 写进库里的身份:证书指纹 + 子机连主机用的地址。
fn hub_identity(st: &AdminState) -> (Option<String>, Option<String>) {
    let v = st
        .store
        .get_kv(gw_store::KEY_NODE_HUB_IDENTITY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or_default();
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(str::to_string);
    (get("spki"), get("public_addr"))
}

/// 拉 hub 的实时状态;拉不到返回 `None`(hub 没部署或挂了)。
async fn hub_status(st: &AdminState) -> Option<serde_json::Value> {
    let resp = st
        .http
        .get(format!("{}/status", hub_url(st)))
        .header("x-api-key", hub_key(st))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

/// 主机库里挂在各子机上的号。对账用(见 [`reconcile`])。
fn accounts_by_node(st: &AdminState) -> std::collections::HashMap<String, Vec<String>> {
    let mut out: std::collections::HashMap<String, Vec<String>> = Default::default();
    if let Ok(rows) = st.store.list_accounts() {
        for row in rows {
            if let Some(node) = super::accounts::row_node(&row.extra) {
                out.entry(node).or_default().push(row.account_id);
            }
        }
    }
    out
}

/// 「一个号的令牌只能有一个持有者」是这整套设计赖以成立的不变量,但它一直是**不可观测**的。
/// 子机的 `/info` 已经把它本机管理器里的号报上来了,这里跟主机库做一次差集:
///
/// - `orphan`:子机上有、主机库里没有 → 残留凭证。它是将来「双持有」的火种(同一个号
///   在新机器重新登录后,旧机器上那份还在),也是删号时子机离线、补删又没执行的痕迹。
/// - `missing`:主机库里有、子机上没有 → 令牌已经不在那台机器上了。号其实已经死了,
///   却还留在轮转里,要等第一个请求打上去才报「需要重新登录」。
///
/// 只有拉到了管理器账号清单才做判断;拉不到(管理器不可达)返回 None,不拿空集当「全丢了」。
fn reconcile(info: Option<&serde_json::Value>, expected: &[String]) -> Option<(Vec<String>, Vec<String>)> {
    let sup = info?.get("supervisor")?;
    if sup.get("error").is_some_and(|e| !e.is_null()) {
        return None;
    }
    let on_node: std::collections::HashSet<&str> = sup
        .get("accounts")?
        .as_array()?
        .iter()
        .filter_map(|a| a.get("id").and_then(|v| v.as_str()))
        .collect();
    let want: std::collections::HashSet<&str> = expected.iter().map(String::as_str).collect();
    let mut orphan: Vec<String> = on_node.difference(&want).map(|s| s.to_string()).collect();
    let mut missing: Vec<String> = want.difference(&on_node).map(|s| s.to_string()).collect();
    orphan.sort();
    missing.sort();
    Some((orphan, missing))
}

async fn list_nodes(State(st): State<AdminState>) -> axum::response::Response {
    let rows = match st.store.list_nodes() {
        Ok(r) => r,
        Err(e) => return internal_error(e),
    };
    let counts = st.store.count_node_accounts().unwrap_or_default();
    let expected = accounts_by_node(&st);
    let live = hub_status(&st).await;
    let live_of = |id: &str| -> Option<serde_json::Value> {
        live.as_ref()?
            .get("nodes")?
            .as_array()?
            .iter()
            .find(|n| n.get("id").and_then(|x| x.as_str()) == Some(id))
            .cloned()
    };
    let nodes: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            let l = live_of(&r.id).unwrap_or_default();
            let stored_info = r.info_json.as_deref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
            let info = l.get("info").cloned().filter(|v| !v.is_null()).or(stored_info);
            let empty: Vec<String> = Vec::new();
            let (orphan, missing) = reconcile(info.as_ref(), expected.get(&r.id).unwrap_or(&empty))
                .map_or((None, None), |(o, m)| (Some(o), Some(m)));
            serde_json::json!({
                "id": r.id,
                "name": r.name,
                "disabled": r.disabled,
                "bound": r.install_id.as_deref().is_some_and(|s| !s.is_empty()),
                "created_at": r.created_at,
                "last_seen": r.last_seen,
                "last_ip": r.last_ip,
                "version": l.get("version").cloned().filter(|v| !v.is_null()).unwrap_or(serde_json::json!(r.version)),
                "accounts": counts.get(&r.id).copied().unwrap_or(0),
                "online": l.get("online").and_then(|x| x.as_bool()).unwrap_or(false),
                "peer_ip": l.get("peer_ip").cloned().unwrap_or_default(),
                "hostname": l.get("hostname").cloned().unwrap_or_default(),
                "rtt_ms": l.get("rtt_ms").cloned().unwrap_or_default(),
                "connected_at": l.get("connected_at").cloned().unwrap_or_default(),
                "info": info,
                // 对账:子机上有而主机没有的号(残留凭证)、主机有而子机没有的号(令牌已丢)。
                // null = 拉不到子机的管理器清单,这次没法判断(别当成「全丢了」)。
                "orphan_accounts": orphan,
                "missing_accounts": missing,
                // 子机离线时删掉的号,等它重连后补删;一直不为空 = 那台机器再没回来过。
                "pending_deletions": st.store.list_node_tombstones(&r.id).unwrap_or_default(),
            })
        })
        .collect();
    let (spki, public_addr) = hub_identity(&st);
    Json(serde_json::json!({
        "hub": {
            "reachable": live.is_some(),
            "spki": spki,
            "public_addr": public_addr,
            // 远端 bash 是否已配钥匙(没配时后台不显示「终端」按钮)。
            "exec_enabled": st.yaml_config.effective_node_exec_key().is_some(),
        },
        "nodes": nodes,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct CreateNodeBody {
    #[serde(default)]
    name: String,
}

async fn create_node(State(st): State<AdminState>, headers: HeaderMap, Json(body): Json<CreateNodeBody>) -> axum::response::Response {
    let name = body.name.trim();
    if name.chars().count() > 64 {
        return api_error(StatusCode::BAD_REQUEST, "名称最多 64 个字符");
    }
    let id = join::new_node_id();
    let secret = join::new_secret();
    match st.store.create_node(&id, name, &join::secret_hash(&secret)) {
        Ok(true) => {}
        Ok(false) => return api_error(StatusCode::CONFLICT, "节点 ID 冲突,请重试"),
        Err(e) => return internal_error(e),
    }
    // 安装票据:与加入令牌**分开**的另一把(更短的)随机数,库里同样只存摘要。
    // 它身兼两职:下载安装包(/node-bundle、/i/ 脚本,24h 有效);以及**安装当天的
    // 加入凭证** —— /i/ 脚本把它当 --token 传给子机,装好后它就是那台子机的长期密钥
    // (hub 侧见 node_modes)。URL 会在 shell 历史/访问日志里留痕,所以下载口 24h 后封死;
    // 防克隆不变量不受影响:抢绑会被 install_id 绑定检查拒绝。
    let ticket = match issue_bundle_ticket(&st.store, &id) {
        Ok(t) => t,
        Err(e) => return internal_error(e),
    };
    let (spki, public_addr) = hub_identity(&st);
    let token = JoinToken {
        node_id: id.clone(),
        secret,
        master: public_addr.clone(),
        spki: spki.as_deref().and_then(|s| join::parse_spki(s).ok()),
    };
    let full = token.full();
    let short = token.short();
    // 主机上放好了安装包、且知道本次请求对外的地址时,直接给「一串 bash」形态:
    // 下载(票据鉴权)+ sha256 校验 + 执行,一条完成。缺任何一样都退回
    // 「先手动传包再执行」的旧形态,不给出跑不通的命令。
    let bundle_sha = match tokio::task::spawn_blocking(bundle_sha256).await {
        Ok(v) => v,
        Err(e) => return internal_error(e),
    };
    let bundle_ready = bundle_sha.is_some();
    let base = request_base(&headers);
    // 主机上放好了安装包、hub 身份已知、且知道本次请求对外的地址时,给「一串 curl」形态:
    // 真正干活的是 /i/<票据> 返回的那段脚本(下载 + sha256 校验 + 执行),用户只要敲一行。
    // 缺任何一样都退回「先手动传包再执行」的旧形态,不给出跑不通的命令。
    // 两条:有 sudo(系统级服务)/ 无 sudo(用户态 ~/.local,脚本里自动转交)。
    // 注意用 -sS 而**不**带 -f:/i/ 的报错响应体本身也是可执行脚本(echo + exit 1),
    // 带 -f 时 curl 会把人话吞掉只剩一句「The requested URL returned error」。
    let (install_cmd, install_cmd_user) = match (&full, &base, &bundle_sha) {
        (Some(_), Some(b), Some(_)) => (
            format!("curl -sSL '{b}/i/{ticket}' | sudo bash"),
            Some(format!("curl -sSL '{b}/i/{ticket}' | bash")),
        ),
        (Some(f), _, _) => (format!("sudo ./caio-node.run --token '{f}'"), None),
        (None, _, _) => (format!("sudo ./caio-node.run --master <主机:端口> --fp <证书指纹> --token '{short}'"), None),
    };
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": id,
            "name": name,
            // 令牌只在这里出现一次;库里只存摘要。
            "token": full.clone().unwrap_or_else(|| short.clone()),
            "token_short": short,
            "install_cmd": install_cmd,
            // 无 sudo 形态;只有「一串 curl」形态才有(退回旧形态时为 null)。
            "install_cmd_user": install_cmd_user,
            "hub_ready": full.is_some(),
            "bundle_ready": bundle_ready,
            // 票据(=命令里的 URL)下载有效期;过期后只能吊销重建(令牌本就只显示一次)。
            "ticket_ttl_secs": BUNDLE_TICKET_TTL_SECS,
        })),
    )
        .into_response()
}

/// 子机安装票据在 settings 表里的键(值见 [`issue_bundle_ticket`] 的 JSON)。
pub(crate) fn bundle_ticket_key(node_id: &str) -> String {
    format!("node_bundle_ticket/{node_id}")
}

/// 票据下载口的有效期:/i/ 脚本与 /node-bundle 超过这个时间就 403。
/// 只封下载口 —— 装好后的子机拿票据密钥当长期凭证,hub 校验不看过期(见
/// [`bundle_ticket_matches`]),否则在线子机 24h 后重连会全灭。
pub(crate) const BUNDLE_TICKET_TTL_SECS: i64 = 24 * 3600;

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// 签发安装票据:`caiob1.<节点>.<16 字符密钥>`(比加入令牌短,要进手敲的一行命令)。
/// 库里只存 `{"h": sha256, "exp": 下载到期}`;吊销子机时随节点行一起删(见 delete_node)。
pub(crate) fn issue_bundle_ticket(store: &gw_store::SqliteStore, node_id: &str) -> anyhow::Result<String> {
    let ticket = format!("caiob1.{node_id}.{}", join::new_ticket_secret());
    let v = serde_json::json!({ "h": join::secret_hash(&ticket), "exp": now_unix() + BUNDLE_TICKET_TTL_SECS });
    store.upsert_kv(&bundle_ticket_key(node_id), &v.to_string())?;
    Ok(ticket)
}

/// 解析库里的票据值:新格式 JSON `{"h", "exp"}`;旧格式是裸 sha256 十六进制(升级前
/// 签发的票据,视为不过期)。注意裸摘要若碰巧全是数字会被 serde 解析成 JSON number,
/// 取不到 "h" 一样落回旧格式分支,无碍。
fn parse_ticket_kv(stored: &str) -> (String, Option<i64>) {
    match serde_json::from_str::<serde_json::Value>(stored) {
        Ok(v) => match (v.get("h").and_then(|x| x.as_str()), v.get("exp").and_then(|x| x.as_i64())) {
            (Some(h), e) => (h.to_string(), e),
            _ => (stored.to_string(), None),
        },
        Err(_) => (stored.to_string(), None),
    }
}

/// 票据密钥比对(常量时间):hub 的 join 校验用 —— **不看过期**(装好的子机靠它重连),
/// 防克隆靠随后的 install_id 绑定检查,不靠这里。
pub(crate) fn bundle_ticket_matches(stored: &str, presented: &str) -> bool {
    let (hash, _) = parse_ticket_kv(stored);
    ct_eq(join::secret_hash(presented).as_bytes(), hash.as_bytes())
}

/// 下载口校验(/node-bundle、/i/):摘要对得上**且未过期**。任何异常一律 false,
/// 调用方统一笼统 403。
pub(crate) fn bundle_ticket_valid(stored: &str, presented: &str) -> bool {
    let (hash, exp) = parse_ticket_kv(stored);
    if exp.is_some_and(|e| now_unix() > e) {
        return false;
    }
    ct_eq(join::secret_hash(presented).as_bytes(), hash.as_bytes())
}

/// 常量时间比较(票据摘要比对用;与 admin 鉴权同一口径)。
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 安装包的 sha256(没有安装包 → None)。同步读 + 哈希,调用方用 spawn_blocking。
/// (router 的 /i/ 脚本端点也用。)
pub(crate) fn bundle_sha256() -> Option<String> {
    let bytes = std::fs::read(gw_core::node::bundle_path()).ok()?;
    use sha2::Digest;
    Some(sha2::Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect())
}

/// 从请求头推导「子机访问主机用的对外地址」:优先 X-Forwarded-Host(反代场景),
/// proto 只认 https(其余一律 http)。Host 会原样拼进要以 root 执行的单引号命令里,
/// 所以字符白名单收口,防注入。(router 的 /i/ 脚本端点也用。)
pub(crate) fn request_base(headers: &HeaderMap) -> Option<String> {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    if !host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b)) {
        return None;
    }
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .map(str::to_ascii_lowercase);
    let scheme = if proto.as_deref() == Some("https") { "https" } else { "http" };
    Some(format!("{scheme}://{host}"))
}

#[derive(Debug, Deserialize)]
struct UpdateNodeBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    disabled: Option<bool>,
}

async fn update_node(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateNodeBody>,
) -> axum::response::Response {
    let name = body.name.as_deref().map(str::trim);
    if name.is_some_and(|n| n.chars().count() > 64) {
        return api_error(StatusCode::BAD_REQUEST, "名称最多 64 个字符");
    }
    match st.store.update_node(&id, name, body.disabled) {
        Ok(true) => match st.store.get_node(&id) {
            Ok(Some(r)) => Json(serde_json::json!({"id": r.id, "name": r.name, "disabled": r.disabled})).into_response(),
            Ok(None) => api_error(StatusCode::NOT_FOUND, "子机不存在"),
            Err(e) => internal_error(e),
        },
        Ok(false) => api_error(StatusCode::NOT_FOUND, "子机不存在"),
        Err(e) => internal_error(e),
    }
}

#[derive(Debug, Deserialize)]
struct DeleteNodeQuery {
    #[serde(default)]
    force: bool,
}

async fn delete_node(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Query(q): Query<DeleteNodeQuery>,
) -> axum::response::Response {
    let n = st.store.count_node_accounts().ok().and_then(|m| m.get(&id).copied()).unwrap_or(0);
    if n > 0 && !q.force {
        return api_error(
            StatusCode::CONFLICT,
            &format!("这台子机上还有 {n} 个号;吊销后它们将永久无法使用。请先删号,或确认后加 force=true"),
        );
    }
    // 吊销会连这台子机的待删清单一起删(它再也不会回来执行了),所以把「就此作废的号」
    // 原样交给调用方:它们的 `dario_node` 指向一个不存在的子机 → 永远选不中,而
    // `dario_node` 又不可改(防双持有)→ 只能删号。不说清楚的话,它们就成了后台里
    // 既不能用也看不出为什么的僵尸行;那台机器上的凭证也没人知道要去清。
    let orphans: Vec<String> = accounts_by_node(&st).remove(&id).unwrap_or_default();
    match st.store.delete_node(&id) {
        Ok(true) => {
            // 安装包下载票据跟着吊销一起死。
            let _ = st.store.delete_kv(&bundle_ticket_key(&id));
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": id,
                    "revoked": true,
                    "dead_accounts": orphans,
                })),
            )
                .into_response()
        }
        Ok(false) => api_error(StatusCode::NOT_FOUND, "子机不存在"),
        Err(e) => internal_error(e),
    }
}

async fn reset_binding(State(st): State<AdminState>, Path(id): Path<String>) -> axum::response::Response {
    match st.store.reset_node_install(&id) {
        Ok(true) => Json(serde_json::json!({"id": id, "bound": false})).into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "子机不存在"),
        Err(e) => internal_error(e),
    }
}

// ── 远端 bash ────────────────────────────────────────────────────────────────

/// 必须大于子机 exec 上限 + hub 的 EXEC_DEADLINE,否则超时结果回不来。
const EXEC_CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(140);

#[derive(Debug, Deserialize)]
struct ExecBody {
    command: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// 在子机上跑一条 bash(后台「子机」页的终端)。经 hub 转发,结果原样回传。
///
/// 两道钥匙:hub 本地口 key(dario.api_key)+ exec 独立钥匙(node_exec_key,
/// 没配就 fail-closed)。子机以 caio 用户执行 —— **它能读到那台机器上所有号的凭证**
/// (node.env 与 /var/lib/caio/dario 对 caio 可读),这是有意的运维通道,不是防泄边界。
async fn exec_on_node(State(st): State<AdminState>, Path(id): Path<String>, Json(body): Json<ExecBody>) -> axum::response::Response {
    match st.store.get_node(&id) {
        Ok(Some(_)) => {}
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "子机不存在"),
        Err(e) => return internal_error(e),
    }
    if body.command.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "command 为空");
    }
    if body.command.chars().count() > gw_core::node::EXEC_CMD_MAX_CHARS {
        return api_error(StatusCode::BAD_REQUEST, &format!("command 超过 {} 字符", gw_core::node::EXEC_CMD_MAX_CHARS));
    }
    let Some(exec_key) = st.yaml_config.effective_node_exec_key() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "远端 bash 未启用:在 system.yaml 顶层加 node_exec_key(或环境变量 CAIO_EXEC_KEY),router 与 node-hub 都要重启",
        );
    };
    let timeout = body.timeout_secs.unwrap_or(gw_core::node::EXEC_TIMEOUT_DEFAULT).clamp(1, gw_core::node::EXEC_TIMEOUT_MAX);
    // 审计:谁在什么子机上跑了什么(截 200 字符)。输出不进日志。
    let preview: String = body.command.chars().take(200).collect();
    tracing::info!(node = %id, command = %preview, timeout_secs = timeout, "远端 bash 下发");
    let client = match reqwest::Client::builder().no_proxy().timeout(EXEC_CLIENT_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return internal_error(e),
    };
    let resp = match client
        .post(format!("{}/n/{id}/exec", hub_url(&st)))
        .header("x-api-key", hub_key(&st))
        .header(gw_core::node::HDR_EXEC_KEY, &exec_key)
        .json(&serde_json::json!({"command": body.command, "timeout_secs": timeout}))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return api_error(StatusCode::BAD_GATEWAY, &format!("hub 不可达: {e}")),
    };
    let status = resp.status().as_u16();
    // hub 的标记:offline/disabled = 命令肯定没发出去;lost = 途中断了,命令可能已执行。
    if let Some(mark) = resp
        .headers()
        .get(gw_core::node::HDR_NODE)
        .and_then(|v| v.to_str().ok())
        .and_then(gw_core::node::NodeMark::parse)
    {
        let msg = match mark {
            gw_core::node::NodeMark::Offline => "子机当前离线",
            gw_core::node::NodeMark::Disabled => "子机已被禁用(排空中)",
            gw_core::node::NodeMark::Lost => "与子机的连接中断,命令可能已执行",
        };
        return api_error(StatusCode::SERVICE_UNAVAILABLE, msg);
    }
    if (200..300).contains(&status) {
        let body = resp.bytes().await.unwrap_or_default();
        let mut out = (StatusCode::from_u16(status).unwrap_or(StatusCode::OK), body).into_response();
        out.headers_mut().insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/json"));
        return out;
    }
    // 非 2xx 一律包成 502,**绝不原样透传状态码**:hub 的 401 会把前端踢回登录页
    // (axios 拦截器见 401 清 token),那就成了一个点「执行」就被登出的怪坑。
    let text = resp.text().await.unwrap_or_default();
    let msg = match status {
        401 => "hub 的本地口 key 与 dario.api_key 不一致".to_string(),
        403 => {
            if text.contains("node_exec_key") || text.contains("钥匙") {
                text.chars().take(200).collect()
            } else {
                "子机拒绝了执行:子机版本过旧(不支持 /exec),或它用 CAIO_EXEC=0 关掉了远端 bash".to_string()
            }
        }
        _ => format!("子机执行失败({status}): {}", text.chars().take(200).collect::<String>()),
    };
    api_error(StatusCode::BAD_GATEWAY, &msg)
}

// ── 给账号上号 / 删号用 ──────────────────────────────────────────────────────

/// 上号到子机前的检查:子机存在;目标组的 worker 是新版(认得 `dario_node`)。
///
/// 老 worker 不认这个键,会把号的凭证交给**本机**管理器,与子机形成双持有,
/// 两边刷新同一条令牌会让它整组作废 —— 所以老 worker 在线时一律拒绝。
pub(crate) async fn check_node_target(st: &AdminState, node: &str, group: &str) -> Result<(), axum::response::Response> {
    match st.store.get_node(node) {
        Ok(Some(_)) => {}
        Ok(None) => return Err(api_error(StatusCode::BAD_REQUEST, "子机不存在")),
        Err(e) => return Err(internal_error(e)),
    }
    let workers: Vec<_> = st.workers.iter().filter(|w| w.account_group == group).collect();
    if workers.is_empty() {
        return Err(api_error(StatusCode::BAD_REQUEST, "该组没有 worker"));
    }
    for w in workers {
        let caps = match st.http.get(format!("http://{}/health", w.listen)).send().await {
            Ok(r) => r.json::<serde_json::Value>().await.ok(),
            Err(_) => None,
        };
        let Some(h) = caps else {
            return Err(api_error(StatusCode::BAD_GATEWAY, "该组 worker 不可达,无法确认它支持子机,请稍后重试"));
        };
        let ok = h.get("provider").and_then(|v| v.as_str()) == Some("claude-dario")
            && h.get("capabilities")
                .and_then(|v| v.as_array())
                .is_some_and(|a| a.iter().any(|c| c.as_str() == Some("dario_nodes")));
        if !ok {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "该组 worker 不是支持子机的 dario worker(请先升级 worker-dario)",
            ));
        }
    }
    Ok(())
}

/// 子机换码的失败分两种:code 根本没送到子机(可重试,保留上号会话),或已送到但换码失败。
pub(crate) enum NodeExchangeError {
    NotDelivered(String),
    Failed(u16, String),
}

/// 经 hub 让子机用它自己的 IP 换码。
pub(crate) async fn node_oauth_exchange(
    st: &AdminState,
    node: &str,
    code: &str,
    verifier: &str,
    state: &str,
) -> Result<serde_json::Value, NodeExchangeError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(45))
        .build()
        .map_err(|e| NodeExchangeError::NotDelivered(e.to_string()))?;
    let resp = client
        .post(format!("{}/n/{node}/oauth/exchange", hub_url(st)))
        .header("x-api-key", hub_key(st))
        .json(&serde_json::json!({"code": code, "verifier": verifier, "state": state}))
        .send()
        .await
        .map_err(|e| NodeExchangeError::NotDelivered(format!("hub 不可达: {e}")))?;
    let status = resp.status().as_u16();
    let mark = resp
        .headers()
        .get(gw_core::node::HDR_NODE)
        .and_then(|v| v.to_str().ok())
        .and_then(gw_core::node::NodeMark::parse);
    let body = resp.json::<serde_json::Value>().await.unwrap_or_default();
    let msg = body
        .pointer("/error/message")
        .and_then(|m| m.as_str())
        .unwrap_or("子机换码失败")
        .to_string();
    match mark {
        Some(m) if m.not_submitted() => Err(NodeExchangeError::NotDelivered(msg)),
        Some(_) => Err(NodeExchangeError::Failed(502, "与子机的连接在换码途中中断,code 可能已被消费,请重新发起上号".into())),
        None if (200..300).contains(&status) => Ok(body),
        // 401 = hub 本地口 key 不对:请求没进子机。
        None if status == 401 => Err(NodeExchangeError::NotDelivered("hub 拒绝了主机的 key(dario.api_key 与 hub 不一致)".into())),
        None => Err(NodeExchangeError::Failed(status, msg)),
    }
}

/// 删号时让子机上的管理器停进程、删凭证。子机离线就记一笔,等它重连后由 hub 补删。
pub(crate) async fn delete_on_node(st: &AdminState, node: &str, account_id: &str) {
    let base = format!("{}/n/{node}/sup", hub_url(st));
    if let Err(e) = gw_dario::supervisor::delete_account(&base, hub_key(st), account_id).await {
        tracing::warn!(node = %node, account = %account_id, error = %e, "子机删号失败,记入待删清单(子机重连后补删)");
        if let Err(e) = st.store.add_node_tombstone(node, account_id) {
            tracing::error!(node = %node, account = %account_id, error = %e, "写待删清单失败(需手动清理子机上的凭证)");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::admin::tests_support::{app, req};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<&str>) -> (u16, serde_json::Value) {
        let resp = app.clone().oneshot(req(method, uri, body)).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn create_list_disable_revoke() {
        // install_cmd 的形态取决于主机上有没有安装包:锁住并摘掉环境变量,钉死「无包」分支。
        let _guard = crate::admin::tests_support::BUNDLE_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("CAIO_NODE_BUNDLE");
        let (app, store) = app();
        store
            .upsert_kv(gw_store::KEY_NODE_HUB_IDENTITY, &serde_json::json!({
                "spki": gw_node::join::spki_b64(&[3u8; 32]), "public_addr": "203.0.113.9:38993"
            }).to_string())
            .unwrap();
        let (s, v) = call(&app, "POST", "/nodes", Some(r#"{"name":"东京-1"}"#)).await;
        assert_eq!(s, 201);
        let id = v["id"].as_str().unwrap().to_string();
        let token = gw_node::join::JoinToken::parse(v["token"].as_str().unwrap()).unwrap();
        assert_eq!(token.node_id, id);
        assert_eq!(token.master.as_deref(), Some("203.0.113.9:38993"));
        assert!(v["install_cmd"].as_str().unwrap().contains("caio-node.run --token"));
        let row = store.get_node(&id).unwrap().unwrap();
        assert_eq!(row.token_hash, gw_node::join::secret_hash(&token.secret), "库里只有摘要");

        let (s, v) = call(&app, "GET", "/nodes", None).await;
        assert_eq!(s, 200);
        assert_eq!(v["hub"]["reachable"], false);
        assert_eq!(v["nodes"][0]["name"], "东京-1");
        assert_eq!(v["nodes"][0]["online"], false);

        let (s, v) = call(&app, "PATCH", &format!("/nodes/{id}"), Some(r#"{"disabled":true}"#)).await;
        assert_eq!((s, v["disabled"].as_bool()), (200, Some(true)));

        store.create_account("a", "", "claude-dario", 1, &format!(r#"{{"dario_node":"{id}"}}"#)).unwrap();
        let (s, _) = call(&app, "DELETE", &format!("/nodes/{id}"), None).await;
        assert_eq!(s, 409, "还有号时要 force");
        let (s, v) = call(&app, "DELETE", &format!("/nodes/{id}?force=true"), None).await;
        assert_eq!(s, 200);
        assert_eq!(v["dead_accounts"], serde_json::json!(["a"]), "就此作废的号要原样交回");
        assert!(store.get_node(&id).unwrap().is_none());
    }

    #[tokio::test]
    async fn token_falls_back_to_short_form_before_hub_ever_ran() {
        let (app, _) = app();
        let (s, v) = call(&app, "POST", "/nodes", Some(r#"{"name":""}"#)).await;
        assert_eq!(s, 201);
        assert_eq!(v["hub_ready"], false);
        assert_eq!(v["token"], v["token_short"]);
        assert!(v["install_cmd"].as_str().unwrap().contains("--master"));
    }

    /// 主机上放好了安装包 + 请求带 Host → 给「一串 curl」形态(/i/ 脚本下载;sudo 与无 sudo 两条)。
    #[tokio::test]
    async fn install_cmd_is_one_liner_when_bundle_and_host_known() {
        let _guard = crate::admin::tests_support::BUNDLE_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("caio-bundle-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let bundle = dir.join("caio-node.run");
        std::fs::write(&bundle, b"fake-run").unwrap();
        std::env::set_var("CAIO_NODE_BUNDLE", &bundle);

        let (app, store) = app();
        store
            .upsert_kv(gw_store::KEY_NODE_HUB_IDENTITY, &serde_json::json!({
                "spki": gw_node::join::spki_b64(&[3u8; 32]), "public_addr": "203.0.113.9:38993"
            }).to_string())
            .unwrap();
        let resp = app
            .oneshot(
                axum::http::Request::post("/nodes")
                    .header("x-api-key", crate::admin::tests_support::TOKEN)
                    .header("host", "gw.example:38991")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"name":"东京"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let bytes = http_body_util::BodyExt::collect(resp.into_body()).await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let cmd = v["install_cmd"].as_str().unwrap();
        assert_eq!(v["bundle_ready"], true);
        let id = v["id"].as_str().unwrap();
        assert!(cmd.starts_with(&format!("curl -sSL 'http://gw.example:38991/i/caiob1.{id}.")), "{cmd}");
        assert!(cmd.ends_with("' | sudo bash"), "{cmd}");
        // 无 sudo 形态:同一条 URL,只是不接 sudo(脚本里自动转用户态安装)。
        let user = v["install_cmd_user"].as_str().unwrap();
        assert!(user.ends_with("' | bash") && !user.contains("sudo"), "{user}");
        // 票据是新格式 JSON(摘要 + 下载到期),摘要与命令里的票据对应。
        let stored = store.get_kv(&super::bundle_ticket_key(id)).unwrap().unwrap();
        let pv: serde_json::Value = serde_json::from_str(&stored).unwrap();
        let ticket = cmd.trim_start_matches("curl -sSL 'http://gw.example:38991/i/").trim_end_matches("' | sudo bash");
        assert_eq!(pv["h"].as_str().unwrap(), gw_node::join::secret_hash(ticket));
        assert!(pv["exp"].as_i64().unwrap() > 0);
        assert_eq!(v["ticket_ttl_secs"], super::BUNDLE_TICKET_TTL_SECS);

        std::env::remove_var("CAIO_NODE_BUNDLE");
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// 上号 / 删号经子机的端到端:假 worker(/health 带能力位)+ 假 hub(换码、删号)。
#[cfg(test)]
mod account_flow_tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{delete, get, post};
    use axum::Json;
    use gw_store::SqliteStore;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::admin::tests_support::req;
    use crate::admin::{admin_api_router, AdminState};

    #[derive(Default)]
    struct Hub {
        calls: Mutex<Vec<String>>,
        delete_offline: Mutex<bool>,
        exec_offline: Mutex<bool>,
    }
    type H = Arc<Hub>;

    async fn exchange(State(h): State<H>, hd: HeaderMap, Path(node): Path<String>, Json(b): Json<serde_json::Value>) -> axum::response::Response {
        assert_eq!(hd.get("x-api-key").unwrap(), "dario-key-0123456789");
        h.calls.lock().unwrap().push(format!("EXCHANGE {node} {}", b["code"].as_str().unwrap()));
        Json(serde_json::json!({"access_token": "at", "refresh_token": "rt-node", "expires_at": "2030-01-01T00:00:00Z"})).into_response()
    }

    async fn del(State(h): State<H>, Path((node, id)): Path<(String, String)>) -> axum::response::Response {
        h.calls.lock().unwrap().push(format!("DELETE {node} {id}"));
        if *h.delete_offline.lock().unwrap() {
            return (StatusCode::SERVICE_UNAVAILABLE, [(gw_core::node::HDR_NODE, "offline")], "离线").into_response();
        }
        Json(serde_json::json!({"deleted": true})).into_response()
    }

    async fn exec(State(h): State<H>, hd: HeaderMap, Path(node): Path<String>, Json(b): Json<serde_json::Value>) -> axum::response::Response {
        assert_eq!(hd.get("x-api-key").unwrap(), "dario-key-0123456789");
        assert_eq!(hd.get(gw_core::node::HDR_EXEC_KEY).unwrap(), "exec-key-0123456789abc");
        h.calls.lock().unwrap().push(format!("EXEC {node} {}", b["command"].as_str().unwrap()));
        if *h.exec_offline.lock().unwrap() {
            return (StatusCode::SERVICE_UNAVAILABLE, [(gw_core::node::HDR_NODE, "offline")], "离线").into_response();
        }
        Json(serde_json::json!({
            "exit_code": 0, "timed_out": false,
            "stdout": "ok\n", "stderr": "",
            "stdout_truncated": false, "stderr_truncated": false, "duration_ms": 5,
        }))
        .into_response()
    }

    async fn spawn(router: axum::Router) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
        a
    }

    async fn setup(caps: serde_json::Value) -> (axum::Router, Arc<SqliteStore>, H) {
        let worker = axum::Router::new()
            .route("/health", get(move || {
                let caps = caps.clone();
                async move { Json(serde_json::json!({"provider": "claude-dario", "capabilities": caps})) }
            }))
            .route("/sync", post(|| async { "ok" }));
        let waddr = spawn(worker).await;
        let h: H = Arc::new(Hub::default());
        let hub = axum::Router::new()
            .route("/n/{node}/oauth/exchange", post(exchange))
            .route("/n/{node}/sup/accounts/{id}", delete(del))
            .route("/n/{node}/exec", post(exec))
            .with_state(h.clone());
        let haddr = spawn(hub).await;
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store.create_group("DARIO", "", "").unwrap();
        store.create_node("n-1", "东京", "x").unwrap();
        let mut cfg = gw_core::config::SystemConfig::default();
        cfg.node_hub_url = format!("http://{haddr}");
        cfg.dario.api_key = "dario-key-0123456789".into();
        cfg.node_exec_key = Some("exec-key-0123456789abc".into());
        let w = gw_core::config::WorkerConfig {
            instance: 1,
            listen: waddr,
            egress: gw_core::config::EgressConfig::Direct,
            account_group: "DARIO".into(),
        };
        let st = AdminState::new(Arc::new(crate::admin::tests_support::TOKEN.to_string()), store.clone(), vec![w], cfg);
        (admin_api_router(st), store, h)
    }

    async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<&str>) -> (u16, serde_json::Value) {
        let resp = app.clone().oneshot(req(method, uri, body)).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn oauth_on_node_exchanges_there_and_binds_account() {
        let (app, store, hub) = setup(serde_json::json!(["dario_nodes"])).await;
        let (s, v) = call(&app, "POST", "/accounts/oauth/start", Some(r#"{"account_id":"c-x.com","group":"DARIO","node":"n-1"}"#)).await;
        assert_eq!(s, 200, "{v}");
        let state = v["state"].as_str().unwrap().to_string();
        let body = serde_json::json!({"state": state, "code": "the-code"}).to_string();
        let (s, v) = call(&app, "POST", "/accounts/oauth/complete", Some(&body)).await;
        assert_eq!(s, 201, "{v}");
        assert_eq!(*hub.calls.lock().unwrap(), vec!["EXCHANGE n-1 the-code"], "换码交给子机");
        let row = store.get_account("c-x.com").unwrap().unwrap();
        let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
        assert_eq!(extra["dario_node"], "n-1");
        assert_eq!(extra["refresh_token"], "rt-node");
        assert!(extra.get("proxy").is_none(), "子机号不配出口");

        // 子机号不能设代理,也不能改子机。
        let (s, _) = call(&app, "PATCH", "/accounts/c-x.com", Some(r#"{"proxy_url":"http://p:1"}"#)).await;
        assert_eq!(s, 400);
        let (s, _) = call(&app, "PATCH", "/accounts/c-x.com", Some(r#"{"extra":{"dario_node":"n-2"},"extra_merge":true}"#)).await;
        assert_eq!(s, 400);

        // 子机离线时删号:号照删,待删清单记一笔。
        *hub.delete_offline.lock().unwrap() = true;
        let (s, _) = call(&app, "DELETE", "/accounts/c-x.com", None).await;
        assert_eq!(s, 204);
        assert!(store.get_account("c-x.com").unwrap().is_none());
        assert_eq!(store.list_node_tombstones("n-1").unwrap(), vec!["c-x.com"]);
    }

    #[tokio::test]
    async fn exec_forwards_through_hub_and_maps_marks() {
        let (app, _, hub) = setup(serde_json::json!(["dario_nodes"])).await;
        // 正常:命令与超时透传,结果原样回来。
        let (s, v) = call(&app, "POST", "/nodes/n-1/exec", Some(r#"{"command":"echo ok","timeout_secs":5}"#)).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["stdout"], "ok\n");
        assert_eq!(*hub.calls.lock().unwrap(), vec!["EXEC n-1 echo ok"], "经 hub 转发到子机");
        // 离线:hub 的标记映射成 503(命令肯定没发出去)。
        *hub.exec_offline.lock().unwrap() = true;
        let (s, v) = call(&app, "POST", "/nodes/n-1/exec", Some(r#"{"command":"echo ok"}"#)).await;
        assert_eq!(s, 503, "{v}");
        // 参数校验:空命令 400;不存在的子机 404。
        let (s, _) = call(&app, "POST", "/nodes/n-1/exec", Some(r#"{"command":"  "}"#)).await;
        assert_eq!(s, 400);
        let (s, _) = call(&app, "POST", "/nodes/n-9/exec", Some(r#"{"command":"true"}"#)).await;
        assert_eq!(s, 404);
    }

    #[tokio::test]
    async fn old_worker_without_capability_is_refused() {        let (app, _, hub) = setup(serde_json::json!([])).await;
        let (s, _) = call(&app, "POST", "/accounts/oauth/start", Some(r#"{"account_id":"c-x.com","group":"DARIO","node":"n-1"}"#)).await;
        assert_eq!(s, 400, "老 worker 会把凭证交给本机管理器,必须拒绝");
        let (s, _) = call(&app, "POST", "/accounts", Some(r#"{"account_id":"d-x.com","group":"DARIO","provider":"claude-dario","node":"n-1","extra":{"refresh_token":"rt"}}"#)).await;
        assert_eq!(s, 400);
        assert!(hub.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn create_with_node_rejects_proxy_and_unknown_node() {
        let (app, store, _) = setup(serde_json::json!(["dario_nodes"])).await;
        let (s, _) = call(&app, "POST", "/accounts", Some(r#"{"account_id":"d-x.com","group":"DARIO","provider":"claude-dario","node":"n-1","extra":{"refresh_token":"rt","proxy":"http://p:1"}}"#)).await;
        assert_eq!(s, 400);
        let (s, _) = call(&app, "POST", "/accounts", Some(r#"{"account_id":"d-x.com","group":"DARIO","provider":"claude-dario","node":"n-9","extra":{"refresh_token":"rt"}}"#)).await;
        assert_eq!(s, 400);
        let (s, v) = call(&app, "POST", "/accounts", Some(r#"{"account_id":"d-x.com","group":"DARIO","provider":"claude-dario","node":"n-1","extra":{"refresh_token":"rt"}}"#)).await;
        assert_eq!(s, 201, "{v}");
        let extra: serde_json::Value = serde_json::from_str(&store.get_account("d-x.com").unwrap().unwrap().extra).unwrap();
        assert_eq!(extra["dario_node"], "n-1");
    }
}
