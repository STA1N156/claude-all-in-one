//! generateAssistantResponse 的错误分类 —— 🟢 借鉴 static_flow `kiro_dispatch.rs:1258`。
//!
//! 把上游 HTTP 状态码 + 响应体映射为 [`UpstreamErrorKind`],调度层(gw-app)据此
//! 决定换号/冷却/直接返回(见 [`gw_core::error`])。
//!
//! static_flow 的映射(我方对齐):
//! - 402 + 月度额度文案 → QuotaExhausted(换号)
//! - 429 + 日额度文案 → RateLimited(冷却换号);其余 429 → RateLimited
//! - 400 + invalid model(瞬时)→ 视为可重试(我方归 ServerError 让其重试);其余 400 → BadRequest(不换号)
//! - 401/403 → TokenInvalid(先刷新同号,失败再换)
//! - 408 / 5xx → ServerError(瞬时重试或换号)
//! - 其他 → Other
//!
//! 我方在 static_flow 之外多出的一档:**5xx + `MODEL_TEMPORARILY_UNAVAILABLE`** → `Overloaded`
//! (模型级容量不足:不惩罚账号 + 不换号 + 同号退避重试),见 [`is_model_overloaded`]。

use gw_core::error::{UpstreamError, UpstreamErrorKind};

/// 月度额度耗尽的响应体特征(static_flow is_monthly_request_limit 同义)。
fn is_monthly_limit(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("monthly") && (b.contains("limit") || b.contains("quota"))
}

/// 日额度限制的响应体特征。
fn is_daily_limit(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("daily") && b.contains("limit")
}

/// 400 但属于"瞬时 invalid model"(上游偶发,换号/重试可恢复)而非请求本身非法。
fn is_transient_invalid_model(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("invalid") && b.contains("model")
}

/// 账号被上游封禁/暂停的 403 响应体特征(Kiro `TEMPORARILY_SUSPENDED` 等)。
///
/// 只命中明确的封禁词:纯 token 过期/未授权体(ExpiredToken/Unauthorized/invalid_grant)
/// **不**命中,保持「刷新同号」行为。命中则归 `TemporarilyBlocked`(冷却自愈 + 不换号),
/// 杜绝把"被封请求"换号扩散到健康号(2026-06 雪崩根因)。
///
/// ⚠️ 部署前用 139 `request_logs` 里真实被封号的 403 body 复核此标记串(见计划 §B)。
/// `pub(crate)`:token.rs 的刷新错误分类也复用它(403+suspend 刷新失败 → 临时冷却而非永久禁号)。
pub(crate) fn is_account_suspended(body: &str) -> bool {
    body.to_ascii_lowercase().contains("suspend")
}

/// 400 响应体是否为 Kiro 的 `INVALID_MODEL_ID`(该账号不支持所请求的模型:模型在其
/// 区域/订阅档未上线)。归 `ModelNotAvailable`——不惩罚账号 + 换号到有该模型的号。
fn is_invalid_model_id(body: &str) -> bool {
    body.to_ascii_lowercase().contains("invalid_model_id")
}

/// 400 响应体是否为**额度用完**。老端点(q 域名 + RPC 形态)把月度额度耗尽报成
/// `400 ServiceQuotaExceededException`(`reason: MONTHLY_REQUEST_COUNT`),而新端点报的是 402。
///
/// 2026-11-06 事故:4 个走老端点的号 13:20 额度用完,这条 400 被当成 BadRequest —— 号不禁用、
/// 不换号,直接把 400 回给客户(客户看到「请求体无效」),19 分钟失败 557 次。必须排在
/// 通用 `400 => BadRequest` 之前。
fn is_quota_exceeded_400(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("servicequotaexceededexception") || lower.contains("monthly_request_count")
}

/// 5xx 响应体是否为**模型级容量不足**。归 `Overloaded`——不惩罚账号 + 不换号 + 同号退避重试。
///
/// 判据取上游明确的 `reason`,而不是宽泛的"所有 5xx":后者里混着上游真内部错误,
/// 语义不同不应共用策略。2026-07-25 opus-5 实测报文:
/// ```text
/// {"message":"Encountered unexpectedly high load when processing the request, please try again.",
///  "reason":"MODEL_TEMPORARILY_UNAVAILABLE"}
/// ```
/// ⚠️ 同期还有 74% 的 5xx 是 `{"message":"Encountered an unexpected error ...","reason":null}`,
/// **不**命中本判据,仍按 `ServerError`(换号重试 + 记账号失败)处理。若后续证实那批也是容量
/// 抖动,放宽判据即可——但那是独立决策,别顺手改。
fn is_model_overloaded(body: &str) -> bool {
    let b = body.to_ascii_uppercase();
    b.contains("MODEL_TEMPORARILY_UNAVAILABLE")
}

/// 400 响应体的 `reason` 若是上游媒体硬限制,返回给客户的**我方**提示语(不含上游原文)。
///
/// 正常情况下 `converter::media_limits` 出站前已把这些限制预检掉,走到这里说明上游改了
/// 限制或预检有漏;至少让客户知道是附件的问题、该怎么办,而不是一句「请求无效」。
/// 2026-09-29 那例就是:151 张图撞 100 张上限,客户只看到「请求无效,请检查请求体后重试」。
fn media_limit_hint(body: &str) -> Option<&'static str> {
    let reason = serde_json::from_str::<serde_json::Value>(body).ok()?;
    Some(match reason.get("reason")?.as_str()? {
        "IMAGE_COUNT_EXCEEDED" => "对话中的图片/文档总数超出单次请求上限(100 个)。请新开会话,或回退到更早的位置后重试",
        "IMAGE_SIZE_EXCEEDED" => "有图片超出单张 5MB 的上限。请压缩或缩小图片后重试",
        "IMAGE_DIMENSION_EXCEEDED" => "有图片边长超出 8000 像素的上限。请缩小或裁剪图片后重试",
        "DOCUMENT_SIZE_EXCEEDED" => "有文档超出单个 4.5MB 的上限。请拆分文件或分页读取后重试",
        "DOCUMENT_COUNT_EXCEEDED" => "单条消息最多附带 5 个文档。请分成几条消息发送",
        _ => return None,
    })
}

/// 把 generateAssistantResponse 的失败响应分类。
///
/// `status` = HTTP 状态码,`body` = 响应体文本(可空)。
pub fn classify_chat_error(status: u16, body: &str) -> UpstreamError {
    let kind = match status {
        402 if is_monthly_limit(body) => UpstreamErrorKind::QuotaExhausted,
        402 => UpstreamErrorKind::QuotaExhausted, // 402 默认按额度耗尽处理
        429 if is_daily_limit(body) => UpstreamErrorKind::RateLimited,
        429 => UpstreamErrorKind::RateLimited,
        // 400 + INVALID_MODEL_ID:该号不支持所请求的模型(模型在其区域/订阅档未上线,
        //   如 eu-central-1 号点 claude-sonnet-5)→ ModelNotAvailable:**不惩罚账号**
        //   (否则计失败刷禁健康号)+ 换号到有该模型的号(见 gw_core::UpstreamErrorKind)。
        400 if is_invalid_model_id(body) => UpstreamErrorKind::ModelNotAvailable,
        // 400 + 额度用完(老端点的报法,新端点是 402):与 402 同等处理 —— 禁用该号、换号。
        400 if is_quota_exceeded_400(body) => UpstreamErrorKind::QuotaExhausted,
        // 400 + 已知媒体限制 reason:结构化判据优先于下面按正文关键词的模糊匹配(正文里
        // 恰好同时出现 invalid/model 也不能被当成瞬时错误去换号)。
        400 if media_limit_hint(body).is_some() => UpstreamErrorKind::BadRequest,
        // 其余"invalid model"文案(无 INVALID_MODEL_ID):当瞬时故障可重试(ServerError)。
        400 if is_transient_invalid_model(body) => UpstreamErrorKind::ServerError,
        400 => UpstreamErrorKind::BadRequest,
        401 => UpstreamErrorKind::TokenInvalid,
        // 403:区分「账号被封禁/暂停」与「access_token 失效」——这是 2026-06 雪崩的关键。
        // - 封禁(TEMPORARILY_SUSPENDED 等)→ TemporarilyBlocked:冷却自愈 + **不换号**
        //   (worth_switching_account=false),否则换号把同一请求扩散到健康号 → 封全池。
        // - 其余 403(token 过期/撤销)→ TokenInvalid:刷新同号,失败再换。
        403 if is_account_suspended(body) => UpstreamErrorKind::TemporarilyBlocked,
        403 => UpstreamErrorKind::TokenInvalid,
        408 => UpstreamErrorKind::ServerError,
        // 5xx + MODEL_TEMPORARILY_UNAVAILABLE:模型级容量不足 → Overloaded(不惩罚账号、
        // 不换号、同号退避重试)。必须排在下面的通用 5xx 之前。
        500..=599 if is_model_overloaded(body) => UpstreamErrorKind::Overloaded,
        500..=599 => UpstreamErrorKind::ServerError,
        _ => UpstreamErrorKind::Other,
    };
    let message = format!("kiro generateAssistantResponse 失败: {status} {body}");
    let err = match media_limit_hint(body) {
        Some(hint) if kind == UpstreamErrorKind::BadRequest => {
            UpstreamError::bad_request_with_hint(message, hint)
        }
        _ => UpstreamError::new(kind, message),
    };
    err.with_status(status)
}

/// `/v1/responses` 的错误(HTTP 非 2xx 的错误体,或流里的 `error` / `response.failed` 事件)
/// 归一到旧协议的错误策略。
///
/// 客户端 `UUs` / `$Us`(extension.js@5713464)先按 `error.type` / `error.code` 定一个状态码,
/// 这里照抄那张表,再把 code 翻成 caio 既有判定认得的 reason,交给 [`classify_chat_error`] ——
/// 封号(`suspend`)、月度额度、媒体上限提示、模型不可用这些按号处理的策略因此原样生效。
///
/// 与客户端的一处有意差别:`kiro:model_requires_user_opt_in` 客户端算 403(拒绝访问),
/// caio 若照搬会落进「403 = token 失效 → 刷新换号」;它实际是「这个号用不了这个模型」,
/// 所以按 `INVALID_MODEL_ID` 归为 ModelNotAvailable(该号该模型打标、换号)。
pub fn classify_responses_error(http_status: u16, body: &str) -> UpstreamError {
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let error = parsed
        .as_ref()
        .map(|v| v.get("error").filter(|e| e.is_object()).unwrap_or(v));
    let field = |k: &str| {
        error
            .and_then(|e| e.get(k))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    // 也可能是旧式 AWS 错误体(顶层 `reason` / `__type`,没有 `error.code`):用 reason 顶上,
    // 媒体上限提示与「模型不可用」判定都读它。
    let code = match field("code") {
        c if c.is_empty() => field("reason"),
        c => c,
    };
    classify_responses_error_parts(http_status, &field("type"), &code, &field("message"), body)
}

/// 流内 `error` / `response.failed` 事件(已取出 error 对象)。没有 HTTP 状态可参考,按 200 传入
/// —— `$Us` 认不出时兜底 500(ServerError),与客户端一致。
pub fn classify_responses_stream_error(error: &serde_json::Value) -> UpstreamError {
    let field = |k: &str| {
        error
            .get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    classify_responses_error_parts(200, &field("type"), &field("code"), &field("message"), &error.to_string())
}

fn classify_responses_error_parts(
    http_status: u16,
    error_type: &str,
    code: &str,
    message: &str,
    raw: &str,
) -> UpstreamError {
    let status = responses_error_status(http_status, error_type, code);
    let bare = code.trim_start_matches("kiro:").to_ascii_uppercase();
    let reason = match code {
        "model_not_found" | "kiro:model_requires_user_opt_in" => "INVALID_MODEL_ID".to_string(),
        "context_length_exceeded" => "CONTENT_LENGTH_EXCEEDS_THRESHOLD".to_string(),
        "server_is_overloaded" => "MODEL_TEMPORARILY_UNAVAILABLE".to_string(),
        _ if error_type.eq_ignore_ascii_case("not_found") => "INVALID_MODEL_ID".to_string(),
        _ => bare,
    };
    // 模型不可用统一走 400 + INVALID_MODEL_ID(旧协议同一判定);上下文超长同理按 400 坏请求。
    let status = match reason.as_str() {
        "INVALID_MODEL_ID" | "CONTENT_LENGTH_EXCEEDS_THRESHOLD" => 400,
        _ => status,
    };
    // reason 放在顶层,媒体上限提示(`media_limit_hint`)读的就是它;原文附在后面,
    // `suspend` / `monthly` 等子串判定照样能命中上游原文。
    let normalized = serde_json::json!({
        "reason": reason,
        "type": error_type,
        "code": code,
        "message": message,
        "upstream": raw,
    })
    .to_string();
    let mut err = classify_chat_error(status, &normalized);
    // 流内事件(HTTP 已是 200)报换算出的状态码,别把失败标成 200(下游有按它回 HTTP 状态的)。
    let reported = if (200..300).contains(&http_status) {
        err.message = format!("kiro /v1/responses 流内失败: {raw}");
        status
    } else {
        err.message = format!("kiro /v1/responses 失败: {http_status} {raw}");
        http_status
    };
    err.with_status(reported)
}

/// `$Us`:由 `error.type` / `error.code` 定状态码;二者都认不出时用 HTTP 状态(流内事件则 500)。
/// 客户端里另有三张 code 集合(`K1p` 403 / `J1p` 402 / `X1p` 429)本次没解出全集,
/// 只按名字里的语义兜底(`quota` / `throttl` / `suspend`);其余 `invalid_* / unsupported_* /
/// missing_*` 与客户端同样归 400。
fn responses_error_status(http_status: u16, error_type: &str, code: &str) -> u16 {
    let t = error_type.to_ascii_lowercase();
    let upper = code.trim_start_matches("kiro:").to_ascii_uppercase();
    if code == "context_length_exceeded" {
        return 413;
    }
    if code == "model_not_found" || t == "not_found" {
        return 404;
    }
    if t == "too_many_requests" || code == "rate_limit_exceeded" || upper.contains("THROTTL") {
        return 429;
    }
    if t == "insufficient_quota" || code == "credit_balance_exhausted" || upper.contains("QUOTA") {
        return 402;
    }
    if code == "access_denied" || code == "kiro:model_requires_user_opt_in" || upper.contains("SUSPEND") {
        return 403;
    }
    if t == "invalid_request" {
        return 400;
    }
    if code == "server_is_overloaded" {
        return 503;
    }
    if t == "model_error" || t == "server_error" {
        return 500;
    }
    // 前缀规则只在「本来就是 400」或流内事件时用:HTTP 401/403 带 `invalid_token` 之类的码
    // 是凭据问题,不能被改判成 BadRequest(那样不刷新也不换号)。
    let may_be_request_error = http_status == 400 || (200..300).contains(&http_status);
    if t.is_empty() && !code.is_empty() && may_be_request_error {
        let lower = code.trim_start_matches("kiro:").to_ascii_lowercase();
        if ["invalid_", "unsupported_", "missing_"].iter().any(|p| lower.starts_with(p))
            || REQUEST_LIMIT_CODES.contains(&upper.as_str())
        {
            return 400;
        }
    }
    // 认不出的错误:HTTP 响应沿用真实状态码(旧协议的判定口径);流内事件没有状态码,
    // 按客户端兜底 500。
    if (200..300).contains(&http_status) {
        500
    } else {
        http_status
    }
}

/// 请求本身超限的错误码(客户端 `Y1p` 集合没解出全集,这里列 caio 已知会出现的那些,
/// 也正是 [`media_limit_hint`] 能给客户提示的那些)。
const REQUEST_LIMIT_CODES: [&str; 6] = [
    "IMAGE_COUNT_EXCEEDED",
    "IMAGE_SIZE_EXCEEDED",
    "IMAGE_DIMENSION_EXCEEDED",
    "DOCUMENT_SIZE_EXCEEDED",
    "DOCUMENT_COUNT_EXCEEDED",
    "CONTENT_LENGTH_EXCEEDS_THRESHOLD",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monthly_402_is_quota_exhausted() {
        let e = classify_chat_error(402, r#"{"message":"Monthly request limit reached"}"#);
        assert_eq!(e.kind, UpstreamErrorKind::QuotaExhausted);
    }

    #[test]
    fn plain_429_is_rate_limited() {
        let e = classify_chat_error(429, "Too many requests");
        assert_eq!(e.kind, UpstreamErrorKind::RateLimited);
    }

    #[test]
    fn invalid_model_id_400_is_model_not_available_and_switches() {
        // INVALID_MODEL_ID = 该号不支持此模型 → ModelNotAvailable(不惩罚账号 + 换号)。
        let e = classify_chat_error(400, r#"{"reason":"INVALID_MODEL_ID"}"#);
        assert_eq!(e.kind, UpstreamErrorKind::ModelNotAvailable);
        // 换号到有该模型的号(非 BadRequest 直接返回)。
        assert!(e.kind.worth_switching_account());
    }

    #[test]
    fn transient_invalid_model_without_id_stays_server_error() {
        // 无 INVALID_MODEL_ID 的"invalid model"文案:仍当瞬时故障重试(ServerError)。
        let e = classify_chat_error(400, "the model is invalid right now");
        assert_eq!(e.kind, UpstreamErrorKind::ServerError);
    }

    #[test]
    fn plain_400_is_bad_request_no_switch() {
        let e = classify_chat_error(400, "Improperly formed request.");
        assert_eq!(e.kind, UpstreamErrorKind::BadRequest);
        assert!(!e.kind.worth_switching_account(), "请求非法换号无意义");
    }

    #[test]
    fn unauthorized_is_token_invalid() {
        assert_eq!(classify_chat_error(401, "").kind, UpstreamErrorKind::TokenInvalid);
        // 无封禁标记的 403 → TokenInvalid(刷新同号)。
        assert_eq!(classify_chat_error(403, "").kind, UpstreamErrorKind::TokenInvalid);
    }

    #[test]
    fn suspended_403_is_temporarily_blocked_not_token_invalid() {
        // 封号 403 → TemporarilyBlocked:冷却自愈 + **不换号**(杜绝雪崩扩散)。
        let e = classify_chat_error(403, r#"{"reason":"TEMPORARILY_SUSPENDED"}"#);
        assert_eq!(e.kind, UpstreamErrorKind::TemporarilyBlocked);
        assert!(!e.kind.worth_switching_account(), "封号请求绝不能换号扩散到健康号");
        // 纯 token 失效的 403 不命中封禁标记,仍走刷新同号。
        let t = classify_chat_error(403, "Unauthorized: access token expired");
        assert_eq!(t.kind, UpstreamErrorKind::TokenInvalid);
    }

    #[test]
    fn server_5xx_is_server_error() {
        assert_eq!(classify_chat_error(503, "unavailable").kind, UpstreamErrorKind::ServerError);
    }

    /// 2026-07-25 opus-5 事故的真实报文(逐字取自 139 `caio-worker0` 日志)。
    #[test]
    fn model_temporarily_unavailable_5xx_is_overloaded() {
        let e = classify_chat_error(
            500,
            r#"{"message":"Encountered unexpectedly high load when processing the request, please try again.","reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#,
        );
        assert_eq!(e.kind, UpstreamErrorKind::Overloaded);
        // 三条策略同时成立,少一条就回到事故现场。
        assert!(e.kind.spares_account_health(), "上游没容量不是账号的错,绝不能记进 failure_count");
        assert!(!e.kind.worth_switching_account(), "容量是模型级的,换号只是扩散 + 白烧配额");
        assert!(e.kind.worth_same_account_backoff(), "唯一有效手段是同号退避重试");
    }

    /// 同期占 74% 的另一种 5xx 报文:`reason` 为 null,**不**命中过载判据。
    #[test]
    fn generic_5xx_without_reason_stays_server_error() {
        let e = classify_chat_error(
            500,
            r#"{"message":"Encountered an unexpected error when processing the request, please try again.","reason":null}"#,
        );
        assert_eq!(e.kind, UpstreamErrorKind::ServerError);
        assert!(!e.kind.spares_account_health(), "通用 5xx 仍记账号健康(旧行为不变)");
        assert!(e.kind.worth_switching_account(), "通用 5xx 仍可换号(旧行为不变)");
        assert!(!e.kind.worth_same_account_backoff(), "同号退避只给 Overloaded");
    }

    #[test]
    fn overload_marker_matched_case_insensitively_across_5xx() {
        for status in [500u16, 502, 503, 529] {
            let e = classify_chat_error(status, r#"{"reason":"model_temporarily_unavailable"}"#);
            assert_eq!(e.kind, UpstreamErrorKind::Overloaded, "status={status}");
        }
        // 非 5xx 不因带该串就变过载(429 仍是限流:它要冷却该号,语义不同)。
        let rl = classify_chat_error(429, r#"{"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#);
        assert_eq!(rl.kind, UpstreamErrorKind::RateLimited);
    }

    #[test]
    fn status_preserved() {
        assert_eq!(classify_chat_error(429, "x").status_code, Some(429));
    }

    #[test]
    fn media_limit_400_gets_actionable_hint_without_upstream_text() {
        let body = r#"{"message":"too many inline media segments: 151 exceeds limit 100","reason":"IMAGE_COUNT_EXCEEDED"}"#;
        let e = classify_chat_error(400, body);
        assert_eq!(e.kind, UpstreamErrorKind::BadRequest, "仍是内容级错误:不换号、不惩罚账号");
        assert_eq!(e.status_code, Some(400));
        let out = e.client_message();
        assert!(out.contains("图片/文档总数") && out.contains("100"), "{out}");
        assert!(!out.contains("inline media") && !out.contains("IMAGE_COUNT_EXCEEDED"), "不外传上游原文: {out}");
        assert!(e.message.contains(body), "内部诊断留原文");
    }

    /// 提示语里写死的数字必须和预检常量一致,改了一边忘了另一边这里会红。
    #[test]
    fn media_limit_hints_match_precheck_constants() {
        use crate::converter::media_limits as ml;
        let hint = |reason: &str| media_limit_hint(&format!(r#"{{"reason":"{reason}"}}"#)).unwrap();
        assert!(hint("IMAGE_COUNT_EXCEEDED").contains(&format!("{} 个", ml::MAX_MEDIA_PER_REQUEST)));
        assert_eq!(ml::MAX_IMAGE_BASE64_BYTES, 5 * 1024 * 1024);
        assert!(hint("IMAGE_DIMENSION_EXCEEDED").contains(&format!("{} 像素", ml::MAX_IMAGE_SIDE_PX)));
        assert_eq!(ml::MAX_DOCUMENT_RAW_BYTES, 4_500_000);
        assert!(hint("DOCUMENT_COUNT_EXCEEDED").contains(&format!("{} 个文档", ml::MAX_DOCUMENTS_PER_MESSAGE)));
    }

    #[test]
    fn media_reason_beats_fuzzy_invalid_model_match() {
        let body = r#"{"reason":"IMAGE_COUNT_EXCEEDED","message":"Invalid request for model: too many inline media segments"}"#;
        let e = classify_chat_error(400, body);
        assert_eq!(e.kind, UpstreamErrorKind::BadRequest, "不能被当成瞬时 invalid model 去换号");
        assert!(e.client_message().contains("图片/文档总数"));
    }

    #[test]
    fn unknown_or_non_json_400_keeps_neutral_message() {
        for body in ["Improperly formed request.", r#"{"reason":"SOMETHING_NEW"}"#, r#"{"reason":null}"#] {
            let e = classify_chat_error(400, body);
            assert_eq!(e.client_message(), UpstreamErrorKind::BadRequest.client_message(), "{body}");
        }
    }

    /// 分类结果的**对外**文案绝不带上游身份 —— `message` 里那串
    /// `kiro generateAssistantResponse 失败: <原始报文>` 只进日志。
    ///
    /// 这里逐档遍历是有意的:将来有人给某一档加 `with_client_detail`(比如想把 402 的
    /// 额度文案透出去),这条测试会立刻按住他 —— 上游报文一律不外传。
    #[test]
    fn classified_errors_never_leak_upstream_identity_to_client() {
        let cases = [
            (402, r#"{"message":"Monthly request limit reached"}"#),
            (429, r#"{"reason":"USER_REQUEST_RATE_EXCEEDED"}"#),
            (400, r#"{"reason":"INVALID_MODEL_ID"}"#),
            (400, "Improperly formed request."),
            (401, "Unauthorized"),
            (403, r#"{"reason":"TEMPORARILY_SUSPENDED"}"#),
            (500, r#"{"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#),
            (503, "unavailable"),
        ];
        for (status, body) in cases {
            let e = classify_chat_error(status, body);
            let out = e.client_message();
            for fp in [
                "kiro",
                "Kiro",
                "generateAssistantResponse",
                "USER_REQUEST_RATE_EXCEEDED",
                "MODEL_TEMPORARILY_UNAVAILABLE",
                "TEMPORARILY_SUSPENDED",
                "INVALID_MODEL_ID",
            ] {
                assert!(!out.contains(fp), "{status} 的对外文案泄露 `{fp}`: {out}");
            }
            // 反向:内部诊断必须完好,否则 139 上的排查就没得看了。
            assert!(
                e.message.contains("generateAssistantResponse") && e.message.contains(body),
                "{status} 的内部诊断被削弱: {}",
                e.message
            );
        }
    }

    /// 老端点 RPC 形态(awsJson1_0)的报错体多一个 `__type`,`reason` 仍在。2026-11-06 在
    /// runtime 域名上用 RPC 形态实测抓到的原文,分类必须与 REST 形态一致。
    #[test]
    fn rpc_form_error_bodies_classify_like_rest() {
        let invalid_model = r#"{"__type":"com.amazon.kiro.runtimeservice#ValidationException","message":"Invalid model ID. Please select a different model to continue.","reason":"INVALID_MODEL_ID"}"#;
        assert_eq!(
            classify_chat_error(400, invalid_model).kind,
            UpstreamErrorKind::ModelNotAvailable
        );
        // 其余状态码只看状态码 + 子串,与报文外壳无关。
        let throttled = r#"{"__type":"com.amazon.aws.codewhisperer#ThrottlingException","message":"Too many requests, please wait before trying again."}"#;
        assert_eq!(classify_chat_error(429, throttled).kind, UpstreamErrorKind::RateLimited);
        let suspended = r#"{"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your User ID is temporarily suspended.","reason":"TEMPORARILY_SUSPENDED"}"#;
        assert_eq!(
            classify_chat_error(403, suspended).kind,
            UpstreamErrorKind::TemporarilyBlocked
        );
    }

    #[test]
    fn legacy_endpoint_quota_400_is_quota_exhausted_not_bad_request() {
        // 2026-11-06 线上原样报文(老端点 RPC 形态,额度用完)。当成 BadRequest 会把 400 直接
        // 回给客户、号也不禁用,高优先级下一直被选中。
        let body = r#"{"__type":"com.amazon.aws.codewhisperer#ServiceQuotaExceededException","message":"You have reached the limit.","reason":"MONTHLY_REQUEST_COUNT"}"#;
        assert_eq!(classify_chat_error(400, body).kind, UpstreamErrorKind::QuotaExhausted);
        // 普通 400 照旧是 BadRequest。
        let plain = r#"{"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"Improperly formed request.","reason":"REQUEST_BODY_INVALID"}"#;
        assert_eq!(classify_chat_error(400, plain).kind, UpstreamErrorKind::BadRequest);
    }

    #[test]
    fn responses_errors_keep_existing_account_policies() {
        let k = |status: u16, body: &str| classify_responses_error(status, body).kind;
        use UpstreamErrorKind as K;
        assert_eq!(k(404, r#"{"error":{"type":"not_found","code":"model_not_found","message":"x"}}"#), K::ModelNotAvailable);
        assert_eq!(k(403, r#"{"error":{"type":"access_denied","code":"kiro:model_requires_user_opt_in","message":"x"}}"#), K::ModelNotAvailable, "opt-in 不能当 token 失效");
        assert_eq!(k(503, r#"{"error":{"type":"server_error","code":"server_is_overloaded","message":"busy"}}"#), K::Overloaded);
        assert_eq!(k(500, r#"{"error":{"type":"server_error","code":"","message":"boom"}}"#), K::ServerError);
        assert_eq!(k(402, r#"{"error":{"type":"insufficient_quota","code":"credit_balance_exhausted","message":"empty"}}"#), K::QuotaExhausted);
        assert_eq!(k(429, r#"{"error":{"type":"too_many_requests","code":"rate_limit_exceeded","message":"slow"}}"#), K::RateLimited);
        assert_eq!(k(403, r#"{"error":{"type":"access_denied","code":"access_denied","message":"Your account is TEMPORARILY_SUSPENDED"}}"#), K::TemporarilyBlocked, "封号判定看原文");
        assert_eq!(k(403, r#"{"error":{"type":"access_denied","code":"access_denied","message":"bad token"}}"#), K::TokenInvalid);
        assert_eq!(k(400, r#"{"error":{"type":"invalid_request","code":"context_length_exceeded","message":"too long"}}"#), K::BadRequest);
        // 没有 JSON 错误体:按 HTTP 状态走旧判定。
        assert_eq!(k(429, "Too many requests"), K::RateLimited);
        assert_eq!(k(502, "<html>bad gateway</html>"), K::ServerError);
        let e = classify_responses_error(429, r#"{"error":{"type":"too_many_requests","code":"rate_limit_exceeded","message":"slow"}}"#);
        assert!(e.message.contains("/v1/responses"));
        assert_eq!(e.status_code, Some(429));
    }

    #[test]
    fn responses_media_limit_code_keeps_customer_hint() {
        let e = classify_responses_error(400, r#"{"error":{"code":"kiro:image_count_exceeded","message":"too many"}}"#);
        assert_eq!(e.kind, UpstreamErrorKind::BadRequest);
        assert!(e.to_string().contains("100") || format!("{e:?}").contains("100"), "{e:?}");
    }

    #[test]
    fn responses_aws_style_reason_and_auth_codes_keep_their_meaning() {
        // 旧式错误体:顶层 reason + 含 invalid/model 字样的 message,不能被模糊匹配成 ServerError。
        let e = classify_responses_error(
            400,
            r#"{"reason":"IMAGE_COUNT_EXCEEDED","message":"Invalid request for model: too many inline media segments"}"#,
        );
        assert_eq!(e.kind, UpstreamErrorKind::BadRequest);
        // 401/403 + invalid_* 码是凭据问题,走刷新换号,不是坏请求。
        assert_eq!(classify_responses_error(401, r#"{"error":{"code":"invalid_token"}}"#).kind, UpstreamErrorKind::TokenInvalid);
        assert_eq!(classify_responses_error(403, r#"{"error":{"code":"missing_authentication_token"}}"#).kind, UpstreamErrorKind::TokenInvalid);
        assert_eq!(classify_responses_error(400, r#"{"error":{"code":"invalid_value"}}"#).kind, UpstreamErrorKind::BadRequest);
        // 流内失败报换算出的状态码,不是 200。
        let e = classify_responses_stream_error(&serde_json::json!({"type":"insufficient_quota","code":"credit_balance_exhausted"}));
        assert_eq!(e.status_code, Some(402));
        assert!(e.message.contains("流内"));
    }

    #[test]
    fn responses_stream_error_defaults_to_server_error() {
        let e = classify_responses_stream_error(&serde_json::json!({"type":"server_error","message":"x"}));
        assert_eq!(e.kind, UpstreamErrorKind::ServerError);
        let e = classify_responses_stream_error(&serde_json::json!({"code":"credit_balance_exhausted"}));
        assert_eq!(e.kind, UpstreamErrorKind::QuotaExhausted);
        let e = classify_responses_stream_error(&serde_json::json!({}));
        assert_eq!(e.kind, UpstreamErrorKind::ServerError);
    }
}
