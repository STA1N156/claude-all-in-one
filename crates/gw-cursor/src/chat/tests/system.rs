//! chat 测试:system 提取与分流。

use super::*;

/// 快路径:没有 `role:"system"` 消息时**一个字节都不改** ——
/// 绝大多数流量走这条,任何改写都是形状漂移。
#[test]
fn 分流器_无system角色时逐字节不变() {
    for b in [
        json!({"messages": [{"role": "user", "content": "hi"}]}),
        json!({"system": [{"type": "text", "text": "S"}],
                   "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]},
                                {"role": "assistant", "content": "yo"}]}),
        json!({}),
    ] {
        let mut got = b.clone();
        route_system_role_messages(&mut got);
        assert_eq!(got, b, "{b}");
    }
}

/// 动态噪声的判据必须精确到「整条就是这个」:带正文的消息不能被误丢。
#[test]
fn 分流器_动态噪声判据不放宽() {
    assert!(is_dynamic_system_noise(
        "<total_tokens>123 tokens left</total_tokens>"
    ));
    assert!(is_dynamic_system_noise(
        "\n  <total_tokens>123 tokens left</total_tokens>  \n"
    ));
    // 前后带正文 → 不是纯噪声,必须保内容。
    assert!(!is_dynamic_system_noise(
        "记住这条约定\n<total_tokens>123 tokens left</total_tokens>"
    ));
    assert!(!is_dynamic_system_noise(
        "<total_tokens>123 tokens left</total_tokens>\n顺便说下预算含义"
    ));
    let mut body = json!({"messages": [
        {"role": "system", "content": "记住这条约定\n<total_tokens>1 tokens left</total_tokens>"},
    ]});
    route_system_role_messages(&mut body);
    assert_eq!(body["messages"].as_array().unwrap().len(), 1, "不能丢");
}

#[test]
fn extract_text_handles_string_and_blocks() {
    assert_eq!(extract_text(&json!("plain")), "plain");
    assert_eq!(
        extract_text(
            &json!([{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}])
        ),
        "a\nb"
    );
}

/// Claude Code 发 `thinking.type=adaptive`;旧逻辑只认 `enabled`,导致:
/// 上游开着思考帧刷新 last_progress,收侧却不透传 → 客户端 5 分钟无首字被
/// gw-app `STREAM_IDLE_ABORT` 掐掉(生产 treenalepsch8375 / 马里奥像素游戏)。
#[test]
fn client_wants_thinking_认_adaptive_与_enabled() {
    assert!(client_wants_thinking(Some(&json!({"type": "enabled"}))));
    assert!(client_wants_thinking(Some(&json!({"type": "adaptive"}))));
    assert!(!client_wants_thinking(Some(&json!({"type": "disabled"}))));
    assert!(!client_wants_thinking(None));
    assert!(!client_wants_thinking(Some(&json!({"type": "weird"}))));
    assert!(!client_wants_thinking(Some(&json!({}))));
}
