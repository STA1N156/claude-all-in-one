//! trailer 错误解析测试。

use super::*;

#[test]
fn parse_trailer_none_when_no_error() {
    assert!(parse_trailer("{}").is_none());
    assert!(parse_trailer(r#"{"metadata":{}}"#).is_none());
    assert!(parse_trailer("not json").is_none());
}

#[test]
fn parse_trailer_extracts_rate_limit_downgrade() {
    // §4 的实测错误体,逐字。
    let raw = r#"{"error":{"code":"resource_exhausted","details":[{"debug":{"error":"ERROR_RATE_LIMITED_CHANGEABLE",
 "details":{"title":"API usage limit reached",
 "detail":"Switched to grok-4.5 after reaching API limit.",
 "additionalInfo":{"autoSwitchToModel":"grok-4.5","spendLimits":"[50,100,200]"},
 "analyticsMetadata":{"actionRequired":"upgrade"}}}}]}}"#;
    let e = parse_trailer(raw).unwrap();
    assert_eq!(e.code, "resource_exhausted");
    assert_eq!(e.debug_error, "ERROR_RATE_LIMITED_CHANGEABLE");
    assert_eq!(e.title, "API usage limit reached");
    assert_eq!(e.auto_switch_to_model, "grok-4.5");
    assert!(e.summary().contains("ERROR_RATE_LIMITED_CHANGEABLE"));
}

#[test]
fn parse_trailer_survives_missing_layers() {
    // 只有 code,没有 details —— 不能 panic,也不能丢掉 code。
    let e = parse_trailer(r#"{"error":{"code":"unauthenticated"}}"#).unwrap();
    assert_eq!(e.code, "unauthenticated");
    assert!(e.debug_error.is_empty());
    assert_eq!(e.summary(), "unauthenticated");
}

#[test]
fn parse_trailer_extracts_policy_block_category() {
    let e = parse_trailer(ANTHROPIC_CYBER_BLOCK_TRAILER).unwrap();
    assert_eq!(e.code, "invalid_argument");
    assert_eq!(e.debug_error, "ERROR_OPENAI");
    assert_eq!(e.title, "Request blocked by Anthropic");
    assert!(e.detail.contains("Usage Policy"));
    assert_eq!(e.action_required, "cyber_policy_review");
}

#[test]
fn parse_trailer_extracts_provider_status() {
    let e = parse_trailer(PROVIDER_400_TRAILER).unwrap();
    assert_eq!(e.code, "resource_exhausted");
    assert_eq!(e.debug_error, "ERROR_PROVIDER_ERROR");
    assert_eq!(e.provider_status, Some(400));
    assert_eq!(e.is_retryable, Some(false));
    // 摘要里要能看出是提供方回的 400,排障时不用再翻原始 JSON
    assert!(e.summary().contains("provider=400"), "{}", e.summary());

    // 数字形态也认(fixture 是字符串,别的错误里见过数字)
    let num = r#"{"error":{"code":"resource_exhausted","details":[{"debug":{"error":"ERROR_PROVIDER_ERROR",
 "details":{"title":"Provider Error","additionalInfo":{"providerStatusCode":422}}}}]}}"#;
    let e = parse_trailer(num).unwrap();
    assert_eq!(e.provider_status, Some(422));
    assert_eq!(e.is_retryable, None);

    // 认不出的值不瞎猜
    let junk = r#"{"error":{"code":"resource_exhausted","details":[{"debug":{"error":"ERROR_PROVIDER_ERROR",
 "details":{"additionalInfo":{"providerStatusCode":"abc"}}}}]}}"#;
    assert_eq!(parse_trailer(junk).unwrap().provider_status, None);
}

#[test]
fn parse_trailer_keeps_raw_when_nothing_recognizable() {
    // error 存在但形状完全陌生:不能返回一个全空、summary 没信息量的东西。
    let e = parse_trailer(r#"{"error":{"weird":123}}"#).unwrap();
    assert!(e.detail.contains("weird"));
    assert!(!e.summary().contains("上游未给出错误详情"));
}
