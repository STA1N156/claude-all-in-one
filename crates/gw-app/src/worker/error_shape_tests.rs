use super::*;
use gw_core::error::{UpstreamError, UpstreamErrorKind as K};

/// ⭐ 2026-08-07 事故:除 Overloaded 外都不发 `error.type`,按 schema 严格校验的
/// 客户端(opencode/zod)连错误都解析不了,真 message 被吞掉。
/// **每一种 kind 都必须给出非空的 `error.type`。**
#[test]
fn every_error_kind_emits_a_string_error_type() {
    for kind in [
        K::TokenInvalid, K::RateLimited, K::TemporarilyBlocked, K::QuotaExhausted,
        K::Network, K::ServerError, K::Overloaded, K::BadRequest,
        K::ModelNotAvailable, K::EmptyResponse, K::ContentFiltered, K::Other,
    ] {
        let e = UpstreamError::new(kind, "x");
        for (path, body) in [
            ("http", upstream_error_payload(&e)),
            ("sse", sse_error_payload(&e)),
        ] {
            let ty = body.pointer("/error/type").and_then(|v| v.as_str());
            assert!(
                ty.is_some_and(|t| !t.is_empty()),
                "{path} 路径 {kind:?} 缺 error.type:{body}"
            );
            assert!(body.pointer("/error/message").is_some_and(|m| m.is_string()));
            assert_eq!(body["type"], "error");
        }
    }
}

/// 面板按 error_kind 字面量标红:常量必须与 kind 的 Debug 串一致(首包前失败路径用的是后者)。
#[test]
fn content_filtered_kind_constant_matches_debug_name() {
    assert_eq!(CONTENT_FILTERED_KIND, format!("{:?}", K::ContentFiltered));
}

#[test]
fn refusal_stop_reason_is_detected_in_stream_and_folded_message() {
    let refusal = SseEvent::new(
        "message_delta",
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"refusal"}}),
    );
    let normal = SseEvent::new(
        "message_delta",
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
    );
    assert!(is_refusal_delta(&refusal));
    assert!(!is_refusal_delta(&normal));
    assert!(is_refusal_message(&serde_json::json!({"stop_reason":"refusal"})));
    assert!(!is_refusal_message(&serde_json::json!({"stop_reason":"end_turn"})));
}

/// 分类器拒答对外:400 + 客户端错误体里带上游类别与说明。
#[test]
fn content_filtered_error_body_carries_full_reason() {
    let e = UpstreamError::content_filtered("REASONING_EXTRACTION", "The selected model cannot continue this conversation.");
    assert_eq!(upstream_status(e.kind), StatusCode::BAD_REQUEST);
    let body = upstream_error_payload(&e);
    assert_eq!(body["error"]["type"], "invalid_request_error");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("REASONING_EXTRACTION") && msg.contains("cannot continue"), "{msg}");
}

/// type 必须与状态码自洽 —— 两者打架会让客户端 SDK 的重试判断和状态码判断冲突。
#[test]
fn error_type_agrees_with_status_code() {
    assert_eq!(anthropic_error_type(K::BadRequest), "invalid_request_error");
    assert_eq!(anthropic_error_type(K::ModelNotAvailable), "invalid_request_error");
    // 确定性空流 → 400 → invalid_request_error(止住重试,见 upstream_status 注释)。
    assert_eq!(anthropic_error_type(K::EmptyResponse), "invalid_request_error");
    assert_eq!(anthropic_error_type(K::ContentFiltered), "invalid_request_error");
    assert_eq!(anthropic_error_type(K::Overloaded), "overloaded_error");
    // 选号失败那条路自己算状态码,走 error_type_for_status
    assert_eq!(error_type_for_status(StatusCode::SERVICE_UNAVAILABLE), "overloaded_error");
    assert_eq!(error_type_for_status(StatusCode::TOO_MANY_REQUESTS), "rate_limit_error");
    assert_eq!(error_type_for_status(StatusCode::BAD_REQUEST), "invalid_request_error");
    assert_eq!(error_type_for_status(StatusCode::BAD_GATEWAY), "api_error");
    // 502 那一大类:api_error(而不是 authentication_error —— 那会让客户以为
    // 是自己的 key 有问题,而坏的是我们的上游账号)。
    for k in [K::TokenInvalid, K::Network, K::QuotaExhausted] {
        assert_eq!(anthropic_error_type(k), "api_error", "{k:?}");
    }
}

/// 选号失败映射(2026-08-24 用户报障驱动):忙 = 429,没号 = 503,模型不可解 = 400。
/// 改映射必须过这关 —— 429/503 语义不同(客户端退避 vs 等运维),不许再揉在一起。
#[test]
fn acquire_error_status_忙429_没号503_模型400() {
    use scheduler::AcquireError as A;
    assert_eq!(acquire_error_status(&A::AllBusy), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(acquire_error_status(&A::AllRpmLimited), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(acquire_error_status(&A::AllDisabled), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(acquire_error_status(&A::Empty), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(acquire_error_status(&A::GroupEmpty), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(acquire_error_status(&A::NoModelSupport), StatusCode::BAD_REQUEST);
}

/// 号都因所在子机离线/排空被排除:对外 503(稍后重试),不能是 400「换模型」。
#[test]
fn unroutable_only_turns_no_model_support_into_503() {
    use scheduler::AcquireError as A;
    assert_eq!(acquire_error_status(&remap_unroutable(A::NoModelSupport, true)), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(acquire_error_status(&remap_unroutable(A::NoModelSupport, false)), StatusCode::BAD_REQUEST);
    assert_eq!(remap_unroutable(A::AllBusy, true), A::AllBusy, "忙就是忙,不改");
}

/// 上游自己发来的 error 载荷若没带 type,也要补上。
#[test]
fn sanitizing_upstream_payload_fills_missing_type() {
    let out = sanitize_upstream_error_payload(&serde_json::json!({
        "type":"error","error":{"message":"上游原文含指纹"}
    }));
    assert_eq!(out.pointer("/error/type").unwrap(), "api_error");
    assert_ne!(out.pointer("/error/message").unwrap(), "上游原文含指纹");
    // 带 type 的保留原 type(客户端按它判重试语义,不能动)。
    let kept = sanitize_upstream_error_payload(&serde_json::json!({
        "type":"error","error":{"type":"rate_limit_error","message":"raw"}
    }));
    assert_eq!(kept.pointer("/error/type").unwrap(), "rate_limit_error");
}
