//! 上游错误分类(HTTP 状态 / end-stream trailer,含 Anthropic 用量政策拦截)。
//!
//! 2026-11-10 由 chat.rs 拆出(只搬不改行为)。

use super::*;

/// HTTP 状态码 → UpstreamError(仅用于**非** 2xx;流内错误走 trailer 分类)。
pub(crate) fn classify_http_error(status: u16, body: &str) -> UpstreamError {
    // 非 2xx 时 Connect 也可能把结构化错误放在 body 里,先试着解出来。
    if let Some(t) = run::parse_trailer(body) {
        return trailer_to_error(&t).with_status(status);
    }
    let kind = match status {
        // ⚠️ 401 才是「这个 token 不行」。**403 不一定**:出口 IP 被 Cloudflare/ALB
        // 拦下时回的也是 403,但那时坏的是 **IP 不是号** —— 判 TokenInvalid 会让
        // gw-app 同号刷新(刷新走同出口,照样 403)然后把这个健康号禁掉。
        // Cursor 这种按 IP 信誉风控的服务,这个形态不罕见。
        //
        // 分辨法:真的凭据问题会给**结构化 Connect 错误体**
        // (`unauthenticated`/`permission_denied`,已在函数开头的 `parse_trailer` 里
        // 走掉了)。走到这里的 403 说明 body 不是 Connect 结构 —— 大概率是一张
        // HTML 错误页。判 `Other`(保守换号一次,不动账号健康)。
        401 => UpstreamErrorKind::TokenInvalid,
        403 => {
            tracing::warn!(
                body_head = %body.chars().take(120).collect::<String>(),
                "cursor 403 且无 Connect 结构化错误体 —— 疑似出口 IP 被拦(不是号的问题),不动账号健康"
            );
            UpstreamErrorKind::Other
        }
        429 => UpstreamErrorKind::RateLimited,
        400 | 464 => UpstreamErrorKind::BadRequest,
        500..=599 => UpstreamErrorKind::ServerError,
        _ => UpstreamErrorKind::Other,
    };
    UpstreamError::new(
        kind,
        format!(
            "Cursor 上游 {status}: {}",
            body.chars().take(300).collect::<String>()
        ),
    )
    .with_status(status)
}

/// end-stream trailer 里的错误 → UpstreamError。
///
/// 关键分类:`ERROR_RATE_LIMITED_CHANGEABLE` 是**该号在该模型上**的计费额度耗尽
/// (服务端还会给 `autoSwitchToModel`),不是账号健康问题 —— Cursor 自家模型
/// (composer/default)和 grok 同时仍然可用。所以映射到 `ModelNotAvailable`:
/// 不惩罚账号、换到有额度的号重试、调度层记住 `(账号,模型)` 不可用。
///
/// 若错判成 `RateLimited`,整个号会被冷却,连不受限的模型一起废掉。
pub(super) fn trailer_to_error(e: &run::TrailerError) -> UpstreamError {
    let rate_limited_changeable =
        e.debug_error == "ERROR_RATE_LIMITED_CHANGEABLE" || !e.auto_switch_to_model.is_empty();
    if !rate_limited_changeable && is_usage_policy_block(e) {
        return policy_block_error(e);
    }
    let kind =
        if rate_limited_changeable {
            UpstreamErrorKind::ModelNotAvailable
        } else if e.debug_error.contains("BAD_MODEL_NAME") {
            UpstreamErrorKind::BadRequest
        } else if is_client_integrity_block(e) {
            // ⚠️ **这一条必须排在下面 `resource_exhausted` 之前。**
            //
            // 客户端完整性门(config_version 空/过期、客户端版本过旧)回的**也是**
            // `resource_exhausted`,文案是 "Update Required"(见 `config.rs` 模块文档)。
            // 落到下面那个 match 就是 `QuotaExhausted` —— 调度层对它的处置是
            // **持久禁用、不自愈**。于是一次控制面抖动会杀掉一个额度充足的健康号。
            //
            // 判成 `ServerError`:可换号重试(别的号有自己的 config_version),
            // 计一次可自愈的失败,**不进 quota 禁用**。
            UpstreamErrorKind::ServerError
        } else if let Some(verdict) = provider_verdict(e) {
            // 模型提供方的错误必须排在下面按 code 分类之前:它的 code 也是
            // `resource_exhausted`,落到 match 里就是 QuotaExhausted(持久禁用 + 换号)。
            // 排在模型名非法、完整性门之后:那两类的既有处置优先(与改动前一致)。
            match verdict {
                ProviderVerdict::Rejected(status) => return provider_rejected_error(e, status),
                ProviderVerdict::Transient => UpstreamErrorKind::Overloaded,
            }
        } else {
            match e.code.as_str() {
                "unauthenticated" | "permission_denied" => UpstreamErrorKind::TokenInvalid,
                "resource_exhausted" => UpstreamErrorKind::QuotaExhausted,
                "invalid_argument" => UpstreamErrorKind::BadRequest,
                "unavailable" => UpstreamErrorKind::Overloaded,
                "deadline_exceeded" => UpstreamErrorKind::Network,
                "internal" | "unknown" | "data_loss" => UpstreamErrorKind::ServerError,
                _ => UpstreamErrorKind::Other,
            }
        };
    UpstreamError::new(kind, format!("Cursor Run: {}", e.summary()))
}

/// 是不是 **Anthropic 用量政策拦截**(上游安全分类器对内容的确定性拒答)。
///
/// 实物(2026-11-09):`invalid_argument` + `ERROR_OPENAI`,title `Request blocked by Anthropic`,
/// detail `…blocked under Anthropic's Usage Policy.`,`actionRequired=cyber_policy_review`。
/// 它的 code 与「请求体非法」共用 `invalid_argument`,按 code 分会落成 BadRequest ——
/// 对外只剩「请求无效,请检查请求体后重试」,客户端当成报文问题原样自动重试
/// (线上一个改论文的会话 30 分钟被拒 63 次),用户也不知道自己被判了网络安全。
/// 判据:code 必须是 `invalid_argument`(封号/鉴权/额度走 `permission_denied`/
/// `unauthenticated`/`resource_exhausted`,文案里即使提到用量政策也不能误判成内容拒答,
/// 否则死号不罚、留在轮转里),再看 title/detail 带用量政策字样或 actionRequired 是
/// `*_policy_review`。
pub(super) fn is_usage_policy_block(e: &run::TrailerError) -> bool {
    if e.code != "invalid_argument" {
        return false;
    }
    let hay = format!("{} {}", e.title, e.detail).to_ascii_lowercase();
    hay.contains("usage policy") || hay.contains("blocked by anthropic") || is_policy_review(&e.action_required)
}

/// `actionRequired` 是不是政策审查类(`cyber_policy_review` 等;大小写、首尾空白不敏感)。
pub(super) fn is_policy_review(action_required: &str) -> bool {
    action_required.trim().to_ascii_lowercase().ends_with("_policy_review")
}

/// 模型提供方错误的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderVerdict {
    /// 提供方拒绝了**请求本身**(带它回的 HTTP 状态码)→ BadRequest。
    Rejected(u16),
    /// 提供方侧的临时/容量问题,或分不清 → Overloaded。
    Transient,
}

/// 提供方判「请求本身不合法」的状态码:400 请求无效、413 太大、422 内容不可处理。
/// 其余 4xx(401/403 是 Cursor 自己的提供方密钥、404 提供方没有该模型、408 超时、409 冲突)
/// 都与客户请求无关,不能让客户以为是自己报文的问题。
const PROVIDER_REJECTS_REQUEST: [u16; 3] = [400, 413, 422];

/// 模型提供方(Anthropic 等)的错误 → 怎么处置;不是提供方错误返回 `None`。
///
/// Cursor 把提供方的失败一律包成 `resource_exhausted`,标记是 `debug.error =
/// ERROR_PROVIDER_ERROR`(或 `ERROR_RESOURCE_EXHAUSTED` 且带 providerStatusCode、或文案是
/// 「reach / connecting to the model provider」)。它们都与**账号**无关,绝不能按 code 判成
/// QuotaExhausted:
/// - **2026-09-03**(grok):上游 422/RESOURCE_EXHAUSTED 被当限流,账号被批量冷却,单模型
///   故障放大成全通道故障。当时只修了旧 inference 路径,Run 路径的这里漏了。
/// - **2026-11-10 12:45**:一个消息结构不合规的请求(图片排在 tool_result 前),Anthropic 回
///   400,被判 QuotaExhausted,调度层持久禁用并换号,同一个请求 30 秒禁掉 4 个号。
///
/// 文案判据只认这两个具体短语,不认单个 provider 字样:真的额度耗尽文案里也可能提到
/// provider,宽了会把真额度耗尽放过(号留在轮转里白白退避)。
///
/// 分类:
/// - 状态码 ∈ [`PROVIDER_REJECTS_REQUEST`] 且上游没说可重试 → [`ProviderVerdict::Rejected`]
///   (原样回客户,不罚号、不换号;换到哪个号都一样错)。
/// - 其余(5xx、429、其它 4xx、标了可重试、没给状态码)→ [`ProviderVerdict::Transient`]
///   (Overloaded:同号退避重试,不罚号)。
fn provider_verdict(e: &run::TrailerError) -> Option<ProviderVerdict> {
    let text = format!("{} {}", e.title, e.detail).to_ascii_lowercase();
    let provider_text =
        text.contains("reach the model provider") || text.contains("connecting to the model provider");
    let is_provider = e.debug_error == "ERROR_PROVIDER_ERROR"
        || (e.debug_error == "ERROR_RESOURCE_EXHAUSTED"
            && (provider_text || e.provider_status.is_some()));
    if !is_provider {
        return None;
    }
    Some(match e.provider_status {
        Some(s) if PROVIDER_REJECTS_REQUEST.contains(&s) && e.is_retryable != Some(true) => {
            ProviderVerdict::Rejected(s)
        }
        _ => ProviderVerdict::Transient,
    })
}

/// 提供方拒绝了请求本身 → 对客户端可见的 BadRequest。
///
/// 对外文案我方完全掌控、不带上游原文(上游原文是误导性的「连不上提供方,稍后重试」,
/// 客户端照做只会原样重发同一个坏请求);内部 message 追加完整摘要便于排障。
fn provider_rejected_error(e: &run::TrailerError, status: u16) -> UpstreamError {
    let mut err = UpstreamError::bad_request_visible(format!(
        "模型提供方拒绝了这个请求(HTTP {status},重试同一个请求不会成功)。\
         常见原因是消息结构不合规,例如 tool_result 没有紧跟在对应的 tool_use 之后、\
         或同一轮里图片/文字排在 tool_result 前面。请检查请求的消息结构。"
    ));
    err.message = format!("{} | Cursor Run: {}", err.message, e.summary());
    err
}

/// 用量政策拦截 → `ContentFiltered`:不换号、不罚号、对外带类别与上游原话,
/// 并进入 worker 的拒答记忆(同一请求原样重发不再反复打上游)。
/// 类别:`actionRequired` 是政策审查类时取它转大写,否则(缺失、`upgrade` 等无关值)记 `USAGE_POLICY`。
pub(super) fn policy_block_error(e: &run::TrailerError) -> UpstreamError {
    let category = if is_policy_review(&e.action_required) {
        e.action_required.trim().to_ascii_uppercase()
    } else {
        "USAGE_POLICY".to_string()
    };
    let explanation = if e.detail.is_empty() { &e.title } else { &e.detail };
    let mut err = UpstreamError::content_filtered(&category, explanation);
    // 内部 message 追加完整原文(含 code/debug_error)便于排障;对外文案在 client_detail 里。
    err.message = format!("{} | Cursor Run: {}", err.message, e.summary());
    err
}

/// 这个错误是不是**客户端完整性门**(而不是真的额度问题)。
///
/// Cursor 对「config_version 空/过期」「客户端版本过旧」这类身份问题,回的是
/// `resource_exhausted` + "Update Required" 文案 —— 与真的额度耗尽**共用同一个 code**。
/// 分不开这两者的后果是不对称的:
/// - 把额度耗尽错判成完整性门 → 多试一次,浪费一点。
/// - 把完整性门错判成额度耗尽 → 账号被**持久禁用**,要人工 reset。
///
/// 所以判据宁松勿紧:文案里出现 update/upgrade/version 就当完整性门。
pub(super) fn is_client_integrity_block(e: &run::TrailerError) -> bool {
    let hay = format!("{} {} {}", e.debug_error, e.title, e.detail).to_ascii_lowercase();
    [
        "update required",
        "please update",
        "upgrade",
        "client version",
        "out of date",
    ]
    .iter()
    .any(|needle| hay.contains(needle))
}
