//! 上游错误 → 对外状态码 / 错误体 / SSE 错误帧的映射。

use super::*;

/// 上游流内 error 事件的 type → 内部 kind(窥探期首内容即 error 时用)。
/// 只映射重试语义明确的三个;其余归 `Other`(502,保守可重试)。payload 本身是
/// 上游原文,只进日志 —— 对外文案走 kind 的 client_message,脱敏纪律不变。
pub(super) fn sse_error_event_kind(ev: &SseEvent) -> UpstreamErrorKind {
    match ev.data.pointer("/error/type").and_then(|t| t.as_str()) {
        Some("rate_limit_error") => UpstreamErrorKind::RateLimited,
        Some("overloaded_error") => UpstreamErrorKind::Overloaded,
        Some("invalid_request_error") => UpstreamErrorKind::BadRequest,
        _ => UpstreamErrorKind::Other,
    }
}

/// 窥探缓冲里的最后一份 Usage(dario 在 EOF/出错前刻意先产 Usage,
/// gw-dario chat.rs:526;Fail/Empty 分支不重放缓冲,不单独取出就会漏计费)。
pub(super) fn buffered_usage(
    buffered: &[Result<StreamItem, gw_core::error::UpstreamError>],
) -> Option<ChatUsage> {
    buffered.iter().rev().find_map(|it| match it {
        Ok(StreamItem::Usage(u)) => Some(u.clone()),
        _ => None,
    })
}

pub(super) fn upstream_status(kind: UpstreamErrorKind) -> StatusCode {
    match kind {
        UpstreamErrorKind::BadRequest | UpstreamErrorKind::ModelNotAvailable => {
            StatusCode::BAD_REQUEST
        }
        // 确定性内容级空流(疑 guardrail / 不支持的输入):内部语义本就"不换号、
        // 重试同一内容无意义",502 只会诱发网关/客户端的自动重试风暴(放大 error
        // 招封号,见 error.rs EmptyResponse 注释)。400 止住重试、把文案直接亮给
        // 用户(2026-08-24 用户报障:opus-5 封控只收到裸断流)。
        // ⚠️ 与之正交的瞬态停滞(cursor Run 90s 只有心跳)已在源头改挂 ServerError
        // (gw-cursor chat.rs STALL_TIMEOUT 分支),不会冒到这条 arm 被误标成 400。
        UpstreamErrorKind::EmptyResponse => StatusCode::BAD_REQUEST,
        // 上游分类器对该内容确定性拒答:同上,400 止住重试,文案带类别与说明。
        UpstreamErrorKind::ContentFiltered => StatusCode::BAD_REQUEST,
        UpstreamErrorKind::Overloaded => overloaded_status(),
        _ => StatusCode::BAD_GATEWAY,
    }
}

/// `UpstreamErrorKind` → Anthropic 协议的 `error.type` 常量。
///
/// ## 为什么这个字段**必须**有(2026-08-07 实测事故)
///
/// Anthropic 的错误体形状是 `{"type":"error","error":{"type":..., "message":...}}` ——
/// `error.type` 是**必填**。早先除 `Overloaded` 外都只发 `message`,于是按 schema
/// 严格校验的客户端(opencode 用 zod)**连错误都解析不了**:
///
/// ```text
/// Type validation failed: {"error":{"message":"服务未返回内容,请重试"},"type":"error"}
/// Error message: [{ "expected":"string", "path":["error","type"],
///                   "message":"Invalid input: expected string, received undefined" }]
/// ```
///
/// 客户看到的是一句 zod 报错,真正的 message 被吞掉 —— 排查成本极高,而且**所有
/// provider 都受影响**(不只 cursor)。
///
/// 取值**跟着 [`upstream_status`] 的状态码走**,不另立一套:type 与 status 不一致
/// 会让客户端 SDK 的重试判断和状态码判断打架。
/// - 400 → `invalid_request_error`
/// - 529 → `overloaded_error`
/// - 502 → `api_error`(Anthropic taxonomy 里没有 bad_gateway;`api_error` 的语义
///   正是"服务端出问题,重试可能有用",与 502 一致,且**不会**让客户端以为是自己的
///   key 或请求有问题)
pub(super) fn anthropic_error_type(kind: UpstreamErrorKind) -> &'static str {
    error_type_for_status(upstream_status(kind))
}

/// 选号失败原因 → 对外状态码。
///
/// 独立成函数的理由:① 映射本身可单测(下次有人改回去编译器+测试一起拦);
/// ② 穷尽 match(**不要改回二分 if**):新增变体时编译器强制在这里做出显式决策,
/// 而不是默默落进 503 —— GroupEmpty 与 NoModelSupport 的差异正是靠这里区分的。
pub(super) fn acquire_error_status(e: &scheduler::AcquireError) -> StatusCode {
    match e {
        // 客户侧可解(换模型/升级订阅),重试无用 → 400。
        scheduler::AcquireError::NoModelSupport => StatusCode::BAD_REQUEST,
        // 容量类但**稍后自行恢复**:并发满 / RPM 窗口 → 429
        // (`rate_limit_error`,SDK/new-api 的标准退避重试信号;
        // 2026-08-24 用户报障:503 经网关转成 502,用户完全看不出是"忙",
        // 而 429 是所有客户端都认识的"稍后再来"。已验 new-api 侧:默认仅
        // 401 触发渠道禁用(status_code_ranges.go),429 与 503 一样走普通重试)。
        scheduler::AcquireError::AllBusy | scheduler::AcquireError::AllRpmLimited => {
            StatusCode::TOO_MANY_REQUESTS
        }
        // 池子/分组的配置或运行时状态(没号/全禁),与"忙"语义不同 → 503。
        scheduler::AcquireError::AllDisabled
        | scheduler::AcquireError::GroupEmpty
        | scheduler::AcquireError::Empty => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// 状态码 → Anthropic `error.type`。
///
/// 单独拆出来是因为有些错误路径(选号失败)自己算状态码、手里没有 `UpstreamErrorKind`。
/// **type 必须跟着 status 走**:两者打架会让客户端 SDK 的重试判断与状态码判断冲突。
pub(super) fn error_type_for_status(code: StatusCode) -> &'static str {
    match code.as_u16() {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        // 503(池子暂时没号)与 529(模型级过载)都是"容量,稍后重试"。
        // Anthropic taxonomy 里没有 service_unavailable,`overloaded_error` 是语义最近的,
        // 而且客户端 SDK 对它的处置正是退避重试。
        503 | 529 => "overloaded_error",
        // 502 及其他 5xx:`api_error` = 服务端出问题、可重试,且**不指认**客户的
        // key 或请求有问题(那会让 Claude Code 之类去要求重新登录)。
        _ => "api_error",
    }
}

/// 把 [`UpstreamError`] 映射为对外 HTTP 响应(状态码见 [`upstream_status`])。
///
/// **message 走 `client_message()` 而不是 `to_string()`**:后者带内部 kind 标签 +
/// 上游原始报文(接口名 `generateAssistantResponse`、reason 码 …),等于把渠道来源
/// 印在每条报错上。诊断原文在调用方的 `tracing` 里一字不少。
pub(super) fn upstream_error_response(e: &gw_core::error::UpstreamError) -> axum::response::Response {
    let code = upstream_status(e.kind);
    (code, Json(upstream_error_payload(e))).into_response()
}

/// 对外错误体。**永远带 `error.type`**,见 [`anthropic_error_type`]。
pub(super) fn upstream_error_payload(e: &gw_core::error::UpstreamError) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "error": {
            "type": anthropic_error_type(e.kind),
            "message": e.client_message(),
        },
    })
}

/// [`upstream_error_response`] 的 wire-aware 版本。
///
/// `Wire::Anthropic` **原样调用旧函数**,逐字节不变 —— 这是生产主链路,不容形状漂移。
/// OpenAI 系只换外壳:状态码、中性文案、脱敏纪律全部沿用同一套。
pub(super) fn upstream_error_response_wire(
    wire: Wire,
    e: &gw_core::error::UpstreamError,
) -> axum::response::Response {
    if !wire.is_openai() {
        return upstream_error_response(e);
    }
    let code = upstream_status(e.kind);
    (
        code,
        Json(gw_core::openai::openai_error_body(
            code.as_u16(),
            &e.client_message(),
            None,
            None,
        )),
    )
        .into_response()
}
pub(super) fn error_response(wire: Wire, code: StatusCode, message: &str) -> axum::response::Response {
    if wire.is_openai() {
        return (
            code,
            Json(gw_core::openai::openai_error_body(
                code.as_u16(),
                message,
                None,
                None,
            )),
        )
            .into_response();
    }
    (
        code,
        Json(serde_json::json!({"type":"error","error":{
            "type": error_type_for_status(code), "message": message}})),
    )
        .into_response()
}

/// 流中硬错误 → 对外 SSE `error` 事件。
///
/// 与 [`upstream_error_response`] 同口径:**只发中性文案**。首包已出后状态码改不了,
/// message 是唯一出口 —— 越是这种时候越不能让它带上游指纹。原文在调用处落 `tracing`。
pub(super) fn sse_error_event(e: &gw_core::error::UpstreamError) -> Event {
    Event::default().event("error").data(sse_error_payload(e).to_string())
}

/// [`sse_error_event`] 的载荷。单独拆出来只为可测:`Event` 没有取回 data 的公开接口。
pub(super) fn sse_error_payload(e: &gw_core::error::UpstreamError) -> serde_json::Value {
    // 与 HTTP 那条路同口径 —— 包括**必须带 `error.type`**(见 anthropic_error_type)。
    upstream_error_payload(e)
}

/// 把**上游自己发来的** Anthropic 形状 error 载荷改写成中性版本。
///
/// 与 [`UpstreamError`] 那条路不同:这条路上错误已经是"合法 SSE 事件"了,provider 没把它
/// 转成 `Err`(dario 直透 Anthropic 流即如此),`fold_sse_to_message` 也会把它原样当作
/// 非流式的错误体回给客户。两处都得过这个函数。
///
/// **保留 `error.type`,只换 `message`**:客户端 SDK(Claude Code / NewAPI)按 type 判是否
/// 重试、是否把渠道判死,动它就等于改重试语义 —— 本次明确不做。type 本身是 Anthropic
/// 协议里的公开常量,不含厂商线索。
pub(super) fn sanitize_upstream_error_payload(data: &serde_json::Value) -> serde_json::Value {
    use UpstreamErrorKind as K;
    let ty = data.pointer("/error/type").and_then(|v| v.as_str());
    // type → 语义最接近的 kind,借用同一套中性文案,避免两处口径漂移。
    let msg = match ty {
        Some("overloaded_error") => K::Overloaded,
        Some("rate_limit_error") => K::RateLimited,
        Some("invalid_request_error") | Some("not_found_error") => K::BadRequest,
        Some("authentication_error") | Some("permission_error") => K::TokenInvalid,
        _ => K::ServerError,
    }
    .client_message();
    match ty {
        Some(t) => serde_json::json!({"type":"error","error":{"type": t, "message": msg}}),
        // 上游没给 type 也**必须补一个**:少了它,严格按 schema 校验的客户端连错误都
        // 解析不了(见 anthropic_error_type 的事故记录)。补 `api_error` 是最保守的选择 ——
        // 不指认客户的 key/请求有问题,语义是"服务端出问题,可重试"。
        None => serde_json::json!({"type":"error","error":{"type":"api_error","message": msg}}),
    }
}

/// 把 [`UpstreamError`] 映射为**运维面**响应 —— 与 [`upstream_error_response`] 的唯一
/// 区别是 message 带全量诊断(上游原文/接口名/刷新失败原因)。
///
/// 只给 worker 的**内网**运维端点用(`/oauth/exchange`、`/accounts/{id}/refresh`、
/// `/accounts/{id}/quota`):它们由 admin 面板扇出调用,listen 在 127.0.0.1,客户到不了。
/// 导号时"这个号到底是 invalid_grant 还是网络抖"全靠这段原文,脱敏等于把运维眼睛蒙上。
///
/// ⚠️ 新增端点前先问一句:客户能打到吗?能 → 用 [`upstream_error_response`]。
pub(super) fn admin_error_response(e: &gw_core::error::UpstreamError) -> axum::response::Response {
    let code = upstream_status(e.kind);
    (
        code,
        Json(serde_json::json!({"type":"error","error":{
            "type": anthropic_error_type(e.kind), "message": e.to_string()}})),
    )
        .into_response()
}

/// HTTP 529 —— Anthropic 的过载状态码。`StatusCode` 无对应常量,且 `from_u16` 不是
/// `const fn`,故用函数而非常量。529 落在合法区间(100..1000),`from_u16` 不会失败;
/// 仍写 `unwrap_or` 兜底以免任何情况下 panic 在错误路径上。
pub(super) fn overloaded_status() -> StatusCode {
    StatusCode::from_u16(529).unwrap_or(StatusCode::BAD_GATEWAY)
}
