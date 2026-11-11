//! chat 测试:上游错误分类。

use super::*;

#[test]
fn rate_limit_downgrade_maps_to_model_not_available() {
    // 这是全文件最重要的一条分类:该号在该模型上没额度 ≠ 该号坏了。
    let t = run::TrailerError {
        code: "resource_exhausted".into(),
        debug_error: "ERROR_RATE_LIMITED_CHANGEABLE".into(),
        title: "API usage limit reached".into(),
        detail: "Switched to grok-4.5 after reaching API limit.".into(),
        auto_switch_to_model: "grok-4.5".into(),
        ..Default::default()
    };
    let e = trailer_to_error(&t);
    assert_eq!(e.kind, UpstreamErrorKind::ModelNotAvailable);
    // 不惩罚账号健康,才不会因为点了 claude 就把整个号冷却掉
    assert!(e.kind.spares_account_health());
}

#[test]
fn trailer_codes_map_to_distinct_kinds() {
    let mk = |code: &str, dbg: &str| {
        trailer_to_error(&run::TrailerError {
            code: code.into(),
            debug_error: dbg.into(),
            ..Default::default()
        })
        .kind
    };
    assert_eq!(mk("unauthenticated", ""), UpstreamErrorKind::TokenInvalid);
    assert_eq!(mk("permission_denied", ""), UpstreamErrorKind::TokenInvalid);
    assert_eq!(
        mk("resource_exhausted", ""),
        UpstreamErrorKind::QuotaExhausted
    );
    assert_eq!(mk("invalid_argument", ""), UpstreamErrorKind::BadRequest);
    assert_eq!(mk("unavailable", ""), UpstreamErrorKind::Overloaded);
    assert_eq!(mk("internal", ""), UpstreamErrorKind::ServerError);
    assert_eq!(mk("weird_code", ""), UpstreamErrorKind::Other);
    // 模型名非法不换号(换号也一样错)
    assert_eq!(
        mk("invalid_argument", "ERROR_BAD_MODEL_NAME"),
        UpstreamErrorKind::BadRequest
    );
}

/// Anthropic 用量政策拦截(2026-11-09 实物)必须判成分类器拒答,而不是笼统的
/// BadRequest:后者对外只有「请求无效,请检查请求体后重试」,客户端以为是报文问题,
/// 原样自动重试(线上一个会话 30 分钟被拒 63 次),用户也不知道自己被判了网络安全。
#[test]
fn anthropic_policy_block_is_content_filtered_with_reason() {
    let t = run::parse_trailer(run::tests::ANTHROPIC_CYBER_BLOCK_TRAILER).unwrap();
    let e = trailer_to_error(&t);
    assert_eq!(e.kind, UpstreamErrorKind::ContentFiltered);
    // 内容级确定性拒答:不换号、不罚号
    assert!(!e.kind.worth_switching_account());
    assert!(e.kind.spares_account_health());
    let msg = e.client_message();
    assert!(
        msg.contains("CYBER_POLICY_REVIEW"),
        "对外文案要带拦截类别: {msg}"
    );
    assert!(
        msg.contains("网络安全"),
        "对外文案要说清是网络安全策略: {msg}"
    );
    assert!(msg.contains("Usage Policy"), "对外文案要带上游原话: {msg}");
    assert!(
        !msg.to_ascii_lowercase().contains("cursor"),
        "不许泄露上游通道: {msg}"
    );
    // 内部 message 留原文(排障)
    assert!(e.message.contains("ERROR_OPENAI"), "{}", e.message);
}

/// 没有 actionRequired 的政策拦截也要认出来(类别退回通用值),HTTP 非 2xx 带结构化体同理。
#[test]
fn policy_block_without_category_still_content_filtered() {
    let t = run::TrailerError {
            code: "invalid_argument".into(),
            debug_error: "ERROR_OPENAI".into(),
            title: "Request blocked by Anthropic".into(),
            detail: "We are unable to complete this request because it was blocked under Anthropic's Usage Policy.".into(),
            ..Default::default()
        };
    let e = trailer_to_error(&t);
    assert_eq!(e.kind, UpstreamErrorKind::ContentFiltered);
    assert!(
        e.client_message().contains("USAGE_POLICY"),
        "{}",
        e.client_message()
    );

    let e = classify_http_error(400, run::tests::ANTHROPIC_CYBER_BLOCK_TRAILER);
    assert_eq!(e.kind, UpstreamErrorKind::ContentFiltered);
    assert_eq!(e.status_code, Some(400));
}

/// 封号/鉴权/额度类错误文案里提到用量政策,也不能判成内容拒答(否则死号不罚、留在轮转里);
/// actionRequired 不是政策审查类(如 `upgrade`)时类别不外露。
#[test]
fn 政策字样只在请求被拒时算拒答() {
    for code in ["permission_denied", "unauthenticated", "resource_exhausted"] {
        let t = run::TrailerError {
            code: code.into(),
            title: "Account suspended".into(),
            detail: "Your account violated our Usage Policy.".into(),
            ..Default::default()
        };
        assert_ne!(
            trailer_to_error(&t).kind,
            UpstreamErrorKind::ContentFiltered,
            "{code}"
        );
    }
    let t = run::TrailerError {
        code: "invalid_argument".into(),
        detail: "blocked under the Usage Policy".into(),
        action_required: "upgrade".into(),
        ..Default::default()
    };
    let e = trailer_to_error(&t);
    assert_eq!(e.kind, UpstreamErrorKind::ContentFiltered);
    assert!(
        e.client_message().contains("USAGE_POLICY"),
        "{}",
        e.client_message()
    );
    assert!(!e.client_message().contains("UPGRADE"));
    // 大小写/空白不敏感
    let t = run::TrailerError {
        code: "invalid_argument".into(),
        action_required: " CYBER_POLICY_REVIEW ".into(),
        ..Default::default()
    };
    assert_eq!(
        trailer_to_error(&t).kind,
        UpstreamErrorKind::ContentFiltered
    );
}

/// 普通 invalid_argument(无政策拦截字样)仍是 BadRequest,别误判成拒答。
#[test]
fn plain_invalid_argument_stays_bad_request() {
    let t = run::TrailerError {
        code: "invalid_argument".into(),
        debug_error: "ERROR_OPENAI".into(),
        title: "Bad request".into(),
        detail: "prompt is too long".into(),
        ..Default::default()
    };
    assert_eq!(trailer_to_error(&t).kind, UpstreamErrorKind::BadRequest);
}

#[test]
fn http_error_prefers_structured_trailer_over_status() {
    // 非 2xx 时 body 里若有 Connect 错误体,应按它分类而不是只看状态码。
    let body = r#"{"error":{"code":"resource_exhausted","details":[{"debug":
            {"error":"ERROR_RATE_LIMITED_CHANGEABLE","details":{"additionalInfo":
            {"autoSwitchToModel":"grok-4.5"}}}}]}}"#;
    let e = classify_http_error(429, body);
    assert_eq!(e.kind, UpstreamErrorKind::ModelNotAvailable);
    // 没有结构化体时退回状态码分类
    assert_eq!(
        classify_http_error(429, "rate limited").kind,
        UpstreamErrorKind::RateLimited
    );
    assert_eq!(
        classify_http_error(401, "nope").kind,
        UpstreamErrorKind::TokenInvalid
    );
}

/// 2026-11-10 12:45 事故:模型提供方对**请求本身**回 400(Cursor 包成 `resource_exhausted`
/// + `ERROR_PROVIDER_ERROR`),旧分类按 code 判成 QuotaExhausted(持久禁用、不自愈)并换号,
/// 同一个坏请求 30 秒禁掉 4 个号。正确语义:请求的问题 → 原样回给客户,不罚号、不换号。
#[test]
fn provider_4xx_is_bad_request_not_quota() {
    let t = run::parse_trailer(run::tests::PROVIDER_400_TRAILER).unwrap();
    let e = trailer_to_error(&t);
    assert_eq!(e.kind, UpstreamErrorKind::BadRequest);
    assert!(e.kind.spares_account_health());
    assert!(!e.kind.worth_switching_account());
    // 客户端要看得到原因(否则只剩「请求无效」,不知道改哪里);文案我方掌控、不带上游原文
    let msg = e.client_message();
    assert!(msg.contains("400"), "{msg}");
    assert!(msg.contains("tool_result"), "{msg}");
    assert!(!msg.contains("trouble connecting"), "{msg}");

    // 其它 4xx(422/413)同理
    for status in ["422", "413"] {
        let raw = run::tests::PROVIDER_400_TRAILER.replace("\"400\"", &format!("\"{status}\""));
        let e = trailer_to_error(&run::parse_trailer(&raw).unwrap());
        assert_eq!(e.kind, UpstreamErrorKind::BadRequest, "{status}");
    }
}

/// 提供方 5xx / 429 / 没给状态码 = 模型级容量问题(与 2026-09-03 grok 事故同口径):
/// 不罚号,同号退避重试。
#[test]
fn provider_5xx_429_or_unknown_is_overloaded() {
    for status in ["500", "503", "529", "429"] {
        let raw = run::tests::PROVIDER_400_TRAILER.replace("\"400\"", &format!("\"{status}\""));
        let e = trailer_to_error(&run::parse_trailer(&raw).unwrap());
        assert_eq!(e.kind, UpstreamErrorKind::Overloaded, "{status}");
        assert!(e.kind.spares_account_health(), "{status}");
    }
    let t = run::TrailerError {
        code: "resource_exhausted".into(),
        debug_error: "ERROR_PROVIDER_ERROR".into(),
        title: "Provider Error".into(),
        ..Default::default()
    };
    assert_eq!(trailer_to_error(&t).kind, UpstreamErrorKind::Overloaded);
    // ERROR_RESOURCE_EXHAUSTED 只有带提供方标记才算(标题/详情提到 provider,或有状态码)
    let t = run::TrailerError {
        code: "resource_exhausted".into(),
        debug_error: "ERROR_RESOURCE_EXHAUSTED".into(),
        title: "Unable to reach the model provider".into(),
        ..Default::default()
    };
    assert_eq!(trailer_to_error(&t).kind, UpstreamErrorKind::Overloaded);
    let t = run::TrailerError {
        code: "resource_exhausted".into(),
        debug_error: "ERROR_RESOURCE_EXHAUSTED".into(),
        provider_status: Some(400),
        ..Default::default()
    };
    assert_eq!(trailer_to_error(&t).kind, UpstreamErrorKind::BadRequest);
}

/// 没有提供方标记的 `resource_exhausted` 仍是真的额度耗尽(不能为修误判把真额度耗尽也放过)。
#[test]
fn plain_resource_exhausted_is_still_quota() {
    let t = run::TrailerError {
        code: "resource_exhausted".into(),
        debug_error: "ERROR_RESOURCE_EXHAUSTED".into(),
        title: "You've hit your usage limit".into(),
        ..Default::default()
    };
    assert_eq!(trailer_to_error(&t).kind, UpstreamErrorKind::QuotaExhausted);
}

/// 只有 400/413/422 算「请求本身不合法」。其它 4xx 与客户请求无关(401/403 是 Cursor 自己的
/// 提供方密钥、404 提供方没有该模型、408 超时、409 冲突),不能让客户以为是报文问题;
/// 上游标了可重试的也不算(审查意见)。
#[test]
fn only_request_level_4xx_is_rejected() {
    for status in ["401", "403", "404", "408", "409"] {
        let raw = run::tests::PROVIDER_400_TRAILER.replace("\"400\"", &format!("\"{status}\""));
        let e = trailer_to_error(&run::parse_trailer(&raw).unwrap());
        assert_eq!(e.kind, UpstreamErrorKind::Overloaded, "{status}");
    }
    let raw = run::tests::PROVIDER_400_TRAILER.replace("\"isRetryable\":false", "\"isRetryable\":true");
    let e = trailer_to_error(&run::parse_trailer(&raw).unwrap());
    assert_eq!(e.kind, UpstreamErrorKind::Overloaded);
}

/// 真的额度耗尽文案里提到 provider,不能被当成提供方错误放过(审查意见)。
#[test]
fn usage_limit_mentioning_provider_is_still_quota() {
    let t = run::TrailerError {
        code: "resource_exhausted".into(),
        debug_error: "ERROR_RESOURCE_EXHAUSTED".into(),
        detail: "You've hit your usage limit for this model provider".into(),
        ..Default::default()
    };
    assert_eq!(trailer_to_error(&t).kind, UpstreamErrorKind::QuotaExhausted);
}

/// 完整性门(Update Required)、模型名非法的既有处置优先于提供方判定(与改动前一致)。
#[test]
fn integrity_and_bad_model_take_priority_over_provider() {
    let t = run::TrailerError {
        code: "resource_exhausted".into(),
        debug_error: "ERROR_PROVIDER_ERROR".into(),
        title: "Update Required".into(),
        provider_status: Some(400),
        ..Default::default()
    };
    assert_eq!(trailer_to_error(&t).kind, UpstreamErrorKind::ServerError);
    let t = run::TrailerError {
        code: "resource_exhausted".into(),
        debug_error: "ERROR_BAD_MODEL_NAME".into(),
        provider_status: Some(422),
        ..Default::default()
    };
    let e = trailer_to_error(&t);
    assert_eq!(e.kind, UpstreamErrorKind::BadRequest);
    // 走的是模型名非法的原分支,不是提供方拒绝的文案
    assert!(!e.client_message().contains("模型提供方拒绝"), "{}", e.client_message());
}
