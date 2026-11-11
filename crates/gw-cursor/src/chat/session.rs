//! 会话键、上游 conversation_id 与请求头。
//!
//! 2026-11-10 由 chat.rs 拆出(只搬不改行为)。

use super::*;

/// 排查用的落盘开关如果开着,吵一声。
///
/// `CURSOR_DUMP_REQ` / `CURSOR_DUMP_TOOL_FRAMES` 会把**含用户对话全文**(system prompt、
/// 客户代码)的帧明文写进磁盘,每请求一个文件、永不清理。生产机上有人为排查设了忘了摘,
/// 就是隐私与磁盘双输。provider 构造时报一次,让它在启动日志里显眼。
pub(crate) fn warn_if_dump_enabled() {
    for var in ["CURSOR_DUMP_REQ", "CURSOR_DUMP_TOOL_FRAMES", "CURSOR_DUMP_KV"] {
        if let Ok(dir) = std::env::var(var) {
            tracing::warn!(
                %var, %dir,
                "⚠️ gw-cursor 落盘开关已开启:会把含用户对话全文的原始帧明文写进磁盘,\
                 且不清理。仅供排查,生产务必摘掉"
            );
        }
    }
}

/// 从请求体派生**会话亲和键**(`Provider::affinity_key` 用)。
///
/// ## 为什么必须有
///
/// 不覆盖 `affinity_key` 的代价不是"少一层优化",而是 `CallCtx.session_id` 与
/// `cache_key` 双双变成**空串**(worker 那边是 `affinity_key.unwrap_or_default()`),
/// 于是 `conversation_id` 恒为 `""`:上游 `1.5` 发零长度字符串(真客户端是 UUID)、
/// `x-blob-encryption-key`/`x-fs-client-key` 退化成每账号一个常量(真客户端每会话一把)、
/// [`crate::ConvRegistry`] 全部会话挤在同一个键上。前两条是稳定的可区分指纹,
/// 第三条在 `CURSOR_STATEFUL=1` 下是跨用户串话。
///
/// ## 为什么锚在**第一条**用户消息
///
/// 键必须在同一会话的多轮之间保持稳定,而 Anthropic 客户端每轮都重传全量历史 ——
/// 只有"第一条用户消息"这个锚点不随轮次变化。与 gw-kiro 的
/// `converter::affinity_key_from_body` 同一思路(那边也是首条锚定),
/// 但本 crate 不依赖 gw-kiro,故自带一份。
pub fn affinity_key_from_body(body: &Value) -> Option<String> {
    // 显式会话标识优先:客户端给了就用它,比内容哈希准。
    if let Some(uid) = body
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|u| u.as_str())
    {
        if let Some(sid) = gw_core::routing::extract_session_from_metadata(uid) {
            return Some(sid);
        }
    }
    let msgs = body.get("messages").and_then(|m| m.as_array())?;
    // ⚠️ `role=="system"` 必须跳过。这个函数由 worker 在 `Provider::chat` **之前**
    // 用原始 body 调用,那时 [`route_system_role_messages`] 还没跑过。中转注入的
    // system 消息若排在首位,锚点就会取到它 —— 而注入内容往往每轮都变
    // (预算计数器),亲和键跟着每轮都变,等于没有亲和。
    let first_user = msgs.iter().find(|m| {
        !matches!(
            m.get("role").and_then(|r| r.as_str()),
            Some("assistant") | Some("system")
        )
    })?;
    let anchor = extract_text(first_user.get("content")?);
    if anchor.trim().is_empty() {
        return None;
    }
    // system 一并入哈希:同一句"hi"在不同 agent(不同 system)下是不同会话。
    Some(gw_core::routing::short_hash(&[
        &extract_system(body),
        &anchor,
    ]))
}

/// 把任意亲和键材料变成一个**非空的 UUID** 形态 conversation_id。
///
/// 两件事一起办:
/// - 保证非空。空 `1.5` 是真客户端不会发的值。
/// - 保证是 UUID。worker 传下来的键可能带调度用的分组前缀(`"<组>\0<键>"`),
///   那是 caio 内部的命名空间,不该原样出现在上游报文里。UUIDv5 把它吃掉。
pub fn conversation_uuid(material: &str) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, material.as_bytes()).to_string()
}

/// 每会话密钥:`sha256hex(token || conversation_id || 用途)`。
///
/// 真客户端用 `crypto.getRandomValues(32)`,每 conversation 一把。我们改成按会话
/// 派生而不是每请求现随机,理由:
/// - 服务端**无法区分**一个随机 64-hex 与一个派生 64-hex,所以指纹上等价;
/// - 但反代是无状态的,现随机会让同一会话的多次请求带**不同**的 key,
///   那反而与真客户端「一会话一把」的行为可区分。
///
/// ⚠️ 仅在无文件附件(L1)时成立。真要实现 L2 blob 加密时,这把 key 会被用来
/// 实际加密上传内容,那时必须换成全熵随机并跟随会话状态存起来。

pub(crate) fn session_key(token: &str, conversation_id: &str, purpose: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.update(b"\x00");
    h.update(conversation_id.as_bytes());
    h.update(b"\x00");
    h.update(purpose.as_bytes());
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 按 §2 铺满 Run 的请求头(30 条)。
///
/// 顺序与分组照 §2 的四类来源排,方便和文档逐行对照。
pub(super) fn apply_headers(mut rb: reqwest::RequestBuilder, ctx: &RunCtx) -> reqwest::RequestBuilder {
    let request_id = uuid::Uuid::new_v4().to_string();

    // §2.1 静态常量
    rb = rb
        .header("connect-protocol-version", "1")
        .header("connect-accept-encoding", "gzip")
        .header("connect-content-encoding", "gzip")
        .header("content-type", "application/connect+proto")
        .header("user-agent", wire::USER_AGENT)
        .header("x-cursor-client-device-type", "desktop")
        .header("x-cursor-remote-type", "none")
        .header("x-cursor-retryinterceptor-enabled", "true")
        .header("x-cursor-streaming", "true")
        // ⚠️ 这两条旧代码发反了。真 IDE 是 ghost-mode=true / onboarding-completed=false。
        .header("x-ghost-mode", "true")
        .header("x-new-onboarding-completed", "false");

    // §2.2 客户端/平台标识
    // ⚠️ 真请求**不发** x-cursor-client-commit 与 x-cursor-client-os-version。
    rb = rb
        .header("x-cursor-client-type", wire::CLIENT_TYPE)
        .header("x-cursor-client-layout", wire::CLIENT_LAYOUT)
        .header("x-cursor-client-version", wire::CLIENT_VERSION)
        .header("x-cursor-client-os", wire::CLIENT_OS)
        .header("x-cursor-client-arch", wire::CLIENT_ARCH)
        .header("x-cursor-timezone", &ctx.timezone);

    // §2.3 由 token 派生
    rb = rb
        .header("authorization", format!("Bearer {}", ctx.token))
        .header("x-client-key", wire::client_key(&ctx.token))
        .header("x-session-id", wire::session_id(&ctx.token))
        .header(
            "x-cursor-checksum",
            wire::checksum(&ctx.machine_id, ctx.mac_machine_id.as_deref()),
        );

    // §2.4 握手下发
    rb = rb.header("x-cursor-config-version", &ctx.config_version);

    // §2.5 每会话
    rb = rb
        .header(
            "x-blob-encryption-key",
            session_key(&ctx.token, &ctx.conversation_id, "blob"),
        )
        .header(
            "x-fs-client-key",
            session_key(&ctx.token, &ctx.conversation_id, "fs"),
        );

    // §2.6 每请求 trace
    rb.header("x-request-id", &request_id)
        .header("x-original-request-id", &request_id)
        .header("x-amzn-trace-id", format!("Root={request_id}"))
        // ⚠️ 两条的 trace-flags 不同:抓包实物 traceparent 是 `-00`(未采样),
        // backend-traceparent 是 `-01`(已采样)。两条都写 -01 是可区分特征。
        .header("traceparent", wire::traceparent_unsampled())
        .header("backend-traceparent", wire::traceparent())
        // 真请求发一个**空** cookie 头。不发这个头本身就是差异。
        .header("cookie", "")
}

/// CLI 形态的请求头(2026-08-16 抓包实物,cursor-agent 2026.08.11)。
///
/// 与 IDE 形态的全部差异:
/// - **没有** checksum / machineId / session-id / client-key / config-version /
///   fs-client-key / timezone / amzn-trace-id / cookie —— 也不需要 GetServerConfig 握手;
/// - `x-cursor-client-type: cli`,`x-cursor-client-version: cli-<版本>`;
/// - `x-ghost-mode: false`(IDE 是 true);
/// - 两条 traceparent **同值**且 flags 都是 `-01`(IDE 是一 `-00` 一 `-01`);
/// - `accept-encoding: gzip,br`(IDE 只有 gzip);
/// - `x-original-request-id` == `x-request-id` == 帧0 的 `1.25`(首个逻辑尝试;
///   真 CLI 重试时 x-request-id 换新、另两个不变,我方每个网关请求都是新尝试)。
pub(super) fn apply_cli_headers(
    rb: reqwest::RequestBuilder,
    ctx: &RunCtx,
    request_id: &str,
) -> reqwest::RequestBuilder {
    // 头表的唯一来源是 `cli::cli_headers` —— 探针(examples/probe_wire_v2)也走它。
    // 若两处各自维护一份,探针测出来的形态就不等于生产发出去的形态,结论不可迁移。
    crate::cli::cli_headers(&ctx.token, &ctx.conversation_id, request_id)
        .into_iter()
        .fold(rb, |rb, (k, v)| rb.header(k, v))
}
