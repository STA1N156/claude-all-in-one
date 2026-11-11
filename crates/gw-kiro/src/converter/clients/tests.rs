use super::*;
use crate::anthropic_types::{SystemMessage, Tool};
use serde_json::json;

fn msg(role: &str, content: serde_json::Value) -> Message {
    Message { role: role.to_string(), content }
}

fn tool(name: &str) -> Tool {
    Tool {
        tool_type: None,
        name: name.to_string(),
        description: "desc".to_string(),
        input_schema: std::collections::HashMap::new(),
        max_uses: None,
    }
}

fn req(system: Option<&str>, tools: &[&str], messages: Vec<Message>) -> MessagesRequest {
    MessagesRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 1024,
        messages,
        stream: false,
        system: system.map(|s| vec![SystemMessage { text: s.to_string() }]),
        tools: (!tools.is_empty()).then(|| tools.iter().map(|n| tool(n)).collect()),
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
        context_management: None,
    }
}

/// 「`...` + tool_use」的 assistant 工具回合及其结果。
fn tool_turn() -> Vec<Message> {
    vec![
        msg("user", json!("开始")),
        msg(
            "assistant",
            json!([{"type": "text", "text": "..."}, {"type": "tool_use", "id": "t1", "name": "bash", "input": {}}]),
        ),
        msg("user", json!([{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}])),
    ]
}

fn kind_of(r: &MessagesRequest) -> ClientKind {
    translate(r, &r.messages).kind
}

fn assistant_blocks(t: &Translation, original: &[Message]) -> serde_json::Value {
    let msgs = t.messages.as_deref().unwrap_or(original);
    msgs.iter().find(|m| m.role == "assistant").unwrap().content.clone()
}

#[test]
fn claude_code_recognized_by_billing_header_or_identity_line() {
    let billing = "x-anthropic-billing-header: cc_version=2.1; cch=abc;\n其余";
    assert_eq!(kind_of(&req(Some(billing), &[], tool_turn())), ClientKind::ClaudeCode);
    let identity = "You are Claude Code, Anthropic's official CLI for Claude.\n其余";
    assert_eq!(kind_of(&req(Some(identity), &[], tool_turn())), ClientKind::ClaudeCode);
}

#[test]
fn developer_role_needs_no_system_and_developer_before_first_assistant() {
    let mut m = vec![msg("user", json!("...")), msg("developer", json!("You are pi."))];
    m.extend(tool_turn());
    assert_eq!(kind_of(&req(None, &[], m.clone())), ClientKind::DeveloperRole);
    // 有 top-level system 的不算(那是别家客户端中段插了 developer)。
    assert_eq!(kind_of(&req(Some("You are X."), &[], m)), ClientKind::Fallback);
    // developer 只出现在第一条 assistant 之后的不算。
    let mut late = tool_turn();
    late.push(msg("developer", json!("中段提示")));
    late.push(msg("user", json!("继续")));
    assert_eq!(kind_of(&req(None, &[], late)), ClientKind::Fallback);
}

#[test]
fn grok_build_recognized_by_tools_even_with_rewritten_system() {
    let tools = ["read_file", "get_command_or_subagent_output", "kill_command_or_subagent"];
    let custom = "You are Claude Opus 5.5 from DogAPI, not a model released by xAI.";
    assert_eq!(kind_of(&req(Some(custom), &tools, tool_turn())), ClientKind::GrokBuild);
    // 只命中一个特征工具、系统提示又不是 Grok 的,不认。
    assert_eq!(kind_of(&req(Some(custom), &["spawn_subagent"], tool_turn())), ClientKind::Fallback);
    // 同一个特征工具重复定义两次,仍只算一个。
    assert_eq!(
        kind_of(&req(Some(custom), &["spawn_subagent", "spawn_subagent"], tool_turn())),
        ClientKind::Fallback
    );
    let sys = "You are Grok released by xAI. You are an interactive CLI tool.";
    assert_eq!(kind_of(&req(Some(sys), &[], tool_turn())), ClientKind::GrokBuild);
    // 「You are Grok」必须是完整的词。
    let lookalike = "You are Grokking a workflow assistant.";
    assert_eq!(kind_of(&req(Some(lookalike), &[], tool_turn())), ClientKind::Fallback);
    let sub = "You are a Grok Build subagent — a focused worker delegated a specific task.";
    assert_eq!(kind_of(&req(Some(sub), &[], tool_turn())), ClientKind::GrokBuild);
}

#[test]
fn opencode_and_deepseek_harness_recognized_by_system_prefix() {
    let oc = "You are OpenCode, the best coding agent on the planet.";
    assert_eq!(kind_of(&req(Some(oc), &[], tool_turn())), ClientKind::OpenCode);
    let oc2 = "You are an AI agent running in OpenCode, a coding agent harness.";
    assert_eq!(kind_of(&req(Some(oc2), &[], tool_turn())), ClientKind::OpenCode);
    let dsh = "You are an AI agent powered by DeepSeek Harness. You are a coding agent.";
    assert_eq!(kind_of(&req(Some(dsh), &[], tool_turn())), ClientKind::DeepSeekHarness);
}

#[test]
fn claude_code_wins_over_later_links() {
    // CC 的系统提示里提到 Grok / OpenCode 也不会被后面的环节抢走:链按顺序问,CC 在最前。
    let sys = "x-anthropic-billing-header: cch=1;\nYou are Grok released by xAI.";
    assert_eq!(kind_of(&req(Some(sys), &["spawn_subagent", "kill_command_or_subagent"], tool_turn())), ClientKind::ClaudeCode);
}

#[test]
fn unknown_client_falls_back_without_copying() {
    let r = req(Some("You are ZCode."), &[], tool_turn());
    let t = translate(&r, &r.messages);
    assert_eq!(t.kind, ClientKind::Fallback);
    assert!(t.messages.is_none(), "兜底且没有 system/developer 消息时零拷贝");
    assert!(t.promoted_system.is_empty());
}

#[test]
fn fallback_keeps_assistant_placeholder_text() {
    // 兜底逐字节保持今天的行为:`...` 不清理。
    let r = req(Some("You are ZCode."), &[], tool_turn());
    let t = translate(&r, &r.messages);
    assert_eq!(assistant_blocks(&t, &r.messages)[0]["text"], "...");
}

#[test]
fn grok_build_drops_assistant_placeholder_text_only_beside_tool_use() {
    let mut m = tool_turn();
    m.push(msg("assistant", json!([{"type": "text", "text": "..."}])));
    m.push(msg("user", json!("下一句")));
    let r = req(Some("You are Grok released by xAI."), &[], m);
    let t = translate(&r, &r.messages);
    let msgs = t.messages.as_deref().unwrap();
    assert_eq!(msgs[1].content, json!([{"type": "tool_use", "id": "t1", "name": "bash", "input": {}}]));
    assert_eq!(msgs[3].content, json!([{"type": "text", "text": "..."}]), "没有 tool_use 的 `...` 不动");
}

#[test]
fn developer_role_promotes_prompt_and_drops_placeholders() {
    let mut m = vec![msg("user", json!([{"type": "text", "text": "..."}])), msg("developer", json!("You are pi."))];
    m.extend(tool_turn());
    let r = req(None, &[], m);
    let t = translate(&r, &r.messages);
    assert_eq!(t.promoted_system, vec!["You are pi.".to_string()]);
    let msgs = t.messages.as_deref().unwrap();
    assert_eq!(msgs[0].content, json!("开始"), "开头的 `...` 占位去掉");
    assert_eq!(msgs[1].content[0]["type"], "tool_use", "工具回合的 `...` 去掉");
}
