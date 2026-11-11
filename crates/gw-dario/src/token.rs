//! Claude OAuth token 端点常量与错误分类(OAuth 换码用)。
//! 令牌刷新已交给每号的 dario 进程,caio 不再刷新。
use gw_core::error::UpstreamErrorKind;

pub(crate) const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub(crate) const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// 刷新端点非 2xx 的分类:只有 `invalid_grant`(或等价撤销)才永久禁号。
///
/// 旧实现把所有 4xx 均映射 `TokenInvalid` = 永久禁号。但 WAF 502/400、
/// `server_error`、`temporarily_unavailable` 等都是瞬时错误,永久禁号会
/// 白白丢掉健康账号。只有响应体 `error` 字段明确为 `invalid_grant`(或
/// `token_revoked`)才表示 token 已被真正撤销。
///
/// 调用方须已将响应体 parse 为 `serde_json::Value`(失败时传 `Null` 即可,
/// 视为"非 invalid_grant" → `Other`)。
pub(crate) fn classify_refresh(code: u16, body: &serde_json::Value) -> UpstreamErrorKind {
    match code {
        429 => UpstreamErrorKind::RateLimited,
        500..=599 => UpstreamErrorKind::ServerError,
        _ => {
            // Permanent revocation signals.  Anything else (server_error,
            // temporarily_unavailable, HTML WAF pages → Null body, etc.) is
            // treated as transient (Other) to avoid permanently banning
            // accounts that just hit a momentary upstream hiccup.
            let err = body.get("error").and_then(|v| v.as_str()).unwrap_or("");
            if err == "invalid_grant" || err == "token_revoked" {
                UpstreamErrorKind::TokenInvalid
            } else {
                UpstreamErrorKind::Other
            }
        }
    }
}

pub(crate) fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gw_core::error::UpstreamErrorKind;





    #[test]
    fn classify_transient_vs_permanent() {
        // 429 / 5xx are always transient regardless of body.
        assert_eq!(classify_refresh(429, &serde_json::Value::Null), UpstreamErrorKind::RateLimited);
        assert_eq!(classify_refresh(503, &serde_json::Value::Null), UpstreamErrorKind::ServerError);
        // 400 + invalid_grant → permanent (token truly revoked).
        assert_eq!(
            classify_refresh(400, &serde_json::json!({"error": "invalid_grant"})),
            UpstreamErrorKind::TokenInvalid
        );
        // 400/401 without invalid_grant → Other (transient; do NOT ban the account).
        assert_eq!(classify_refresh(400, &serde_json::Value::Null), UpstreamErrorKind::Other);
        assert_eq!(classify_refresh(401, &serde_json::Value::Null), UpstreamErrorKind::Other);
    }

    #[test]
    fn classify_refresh_only_invalid_grant_is_permanent() {
        // Explicit invalid_grant → TokenInvalid (permanent ban).
        assert_eq!(
            classify_refresh(400, &serde_json::json!({"error": "invalid_grant"})),
            UpstreamErrorKind::TokenInvalid
        );
        // token_revoked is another well-known permanent revocation signal.
        assert_eq!(
            classify_refresh(401, &serde_json::json!({"error": "token_revoked"})),
            UpstreamErrorKind::TokenInvalid
        );
        // server_error (transient upstream failure) → Other, NOT TokenInvalid.
        assert_eq!(
            classify_refresh(400, &serde_json::json!({"error": "server_error"})),
            UpstreamErrorKind::Other
        );
        // temporarily_unavailable (e.g. maintenance) → Other.
        assert_eq!(
            classify_refresh(400, &serde_json::json!({"error": "temporarily_unavailable"})),
            UpstreamErrorKind::Other
        );
        // Non-JSON body (WAF HTML → parsed as Null) → Other.
        assert_eq!(
            classify_refresh(401, &serde_json::Value::Null),
            UpstreamErrorKind::Other
        );
        // 429 → RateLimited regardless of body.
        assert_eq!(
            classify_refresh(429, &serde_json::json!({"error": "invalid_grant"})),
            UpstreamErrorKind::RateLimited
        );
        // 503 → ServerError regardless of body.
        assert_eq!(
            classify_refresh(503, &serde_json::Value::Null),
            UpstreamErrorKind::ServerError
        );
    }

}
