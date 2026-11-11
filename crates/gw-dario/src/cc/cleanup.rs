//! 把消息整理成 Anthropic 接口能收、又像 CC 的样子(翻译路径用;真 CC 透传不走这里)。
//!
//! 主要清的是中转 new-api 把 OpenAI 格式转成 Claude 格式时留下的痕迹(10-30 查明,
//! `new-api/service/relayconvert/internal/oai_chat/to_claude_messages_req.go`):
//! - `role=developer` / `system` 留在 messages 里(Anthropic 的 messages 只收 user/assistant,原样发会 400);
//! - 首条非 user 时补一条 `...` user 占位;
//! - assistant 工具回合的空正文补成 `...`。
//!
//! 语义与 Kiro 线 `gw-kiro::converter::clients::steps` 对齐(占位认 `...` 与 `…`,开头占位只在
//! 后面紧跟另一条 user 时才删)。

use serde_json::{json, Value};

pub(crate) struct Cleaned {
    pub messages: Vec<Value>,
    /// 第一条 assistant 之前的 developer / system 消息正文(客户端的系统提示),按出现顺序。
    pub promoted_system: Vec<String>,
}

pub(crate) fn clean(messages: &[Value]) -> Cleaned {
    let (routed, promoted_system) = route_system_like(messages);
    let mut msgs = routed;
    drop_leading_placeholder_user(&mut msgs);
    drop_assistant_placeholder_text(&mut msgs);
    strip_history_thinking(&mut msgs);
    drop_empty_text(&mut msgs);
    let messages = merge_same_role(msgs);
    Cleaned { messages, promoted_system }
}

fn role(m: &Value) -> &str {
    m.get("role").and_then(Value::as_str).unwrap_or("")
}

fn is_system_like(r: &str) -> bool {
    r == "system" || r == "developer"
}

/// content(字符串或块数组)里的全部文本,块之间用换行拼;非文本块忽略。
pub(crate) fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 第一条 assistant 之前的 developer/system 提成系统提示;之后的(代理链中途注入的)转成
/// user,包在 `<system_context>` 里保住位置语义(同 Kiro 线对「未知 system」的处理)。空的丢掉。
fn route_system_like(messages: &[Value]) -> (Vec<Value>, Vec<String>) {
    let mut out = Vec::with_capacity(messages.len());
    let mut promoted = Vec::new();
    let mut seen_assistant = false;
    for m in messages {
        let r = role(m);
        if r == "assistant" {
            seen_assistant = true;
        }
        if !is_system_like(r) {
            out.push(m.clone());
            continue;
        }
        let text = content_text(m.get("content").unwrap_or(&Value::Null));
        if text.trim().is_empty() {
            continue;
        }
        if seen_assistant {
            out.push(json!({"role": "user", "content": format!("<system_context>\n{text}\n</system_context>")}));
        } else {
            promoted.push(text);
        }
    }
    (out, promoted)
}

fn is_placeholder_text(text: &str) -> bool {
    matches!(text.trim(), "..." | "…")
}

fn block_type(b: &Value) -> &str {
    b.get("type").and_then(Value::as_str).unwrap_or("")
}

/// content 只有占位文本、没有任何别的块。
fn is_placeholder_only(content: &Value) -> bool {
    let only_text = match content {
        Value::String(_) => true,
        Value::Array(items) => items.iter().all(|b| block_type(b) == "text"),
        _ => false,
    };
    only_text && is_placeholder_text(&content_text(content))
}

/// 去掉最前面的 `...` 占位 user(只在后面紧跟另一条 user 时去,否则会把请求删空)。
fn drop_leading_placeholder_user(msgs: &mut Vec<Value>) {
    if msgs.len() > 1
        && role(&msgs[0]) == "user"
        && role(&msgs[1]) == "user"
        && is_placeholder_only(msgs[0].get("content").unwrap_or(&Value::Null))
    {
        msgs.remove(0);
    }
}

/// assistant「`...` 文本 + tool_use」里的 `...` 文本块删掉。不带 tool_use 的 `...` 是那一回合
/// 唯一的内容,不动。
fn drop_assistant_placeholder_text(msgs: &mut [Value]) {
    for m in msgs.iter_mut().filter(|m| role(m) == "assistant") {
        let Some(Value::Array(blocks)) = m.get_mut("content") else {
            continue;
        };
        if !blocks.iter().any(|b| block_type(b) == "tool_use") {
            continue;
        }
        blocks.retain(|b| {
            !(block_type(b) == "text" && is_placeholder_text(b.get("text").and_then(Value::as_str).unwrap_or("")))
        });
    }
}

/// 历史 assistant 里的 `thinking` 块删掉(照搬 dario 模板路径 `cc-template.ts` 的 Strip thinking)。
///
/// 翻译出来的报文会被 dario 判为真 CC 走透传,dario 不再替它清 —— 这件事得我们做。客户端的历史
/// thinking 可能来自别的上游(Kiro 合成签名等),原样发给 Anthropic 会验签失败,而且之后每轮都失败。
/// `redacted_thinking` 与 dario 一样保留。
fn strip_history_thinking(msgs: &mut [Value]) {
    for m in msgs.iter_mut().filter(|m| role(m) == "assistant") {
        if let Some(Value::Array(blocks)) = m.get_mut("content") {
            blocks.retain(|b| block_type(b) != "thinking");
        }
    }
}

/// 空文本块 Anthropic 直接 400(dario#1077);删掉后整条变空的消息也删掉。
fn drop_empty_text(msgs: &mut Vec<Value>) {
    for m in msgs.iter_mut() {
        if let Some(Value::Array(blocks)) = m.get_mut("content") {
            blocks.retain(|b| {
                !(block_type(b) == "text" && b.get("text").and_then(Value::as_str).unwrap_or("").trim().is_empty())
            });
        }
    }
    msgs.retain(|m| match m.get("content") {
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(blocks)) => !blocks.is_empty(),
        _ => false,
    });
}

fn into_blocks(content: Value) -> Vec<Value> {
    match content {
        Value::String(s) => vec![json!({"type": "text", "text": s})],
        Value::Array(blocks) => blocks,
        _ => Vec::new(),
    }
}

/// 相邻同角色消息合并成一条(真 CC 不发连续同角色)。合并后 tool_result 块排在最前 ——
/// Anthropic 要求工具结果紧跟在对应的 tool_use 之后,文字排在它前面会 400。
fn merge_same_role(msgs: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(msgs.len());
    for m in msgs {
        let same = out.last().is_some_and(|prev| role(prev) == role(&m));
        if !same {
            out.push(m);
            continue;
        }
        let prev = out.last_mut().expect("上面判过非空");
        let mut blocks = into_blocks(prev.get("content").cloned().unwrap_or(Value::Null));
        blocks.extend(into_blocks(m.get("content").cloned().unwrap_or(Value::Null)));
        let (mut results, rest): (Vec<Value>, Vec<Value>) =
            blocks.into_iter().partition(|b| block_type(b) == "tool_result");
        results.extend(rest);
        prev["content"] = Value::Array(results);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_shape_is_cleaned_to_a_valid_conversation() {
        // pi 经 new-api:首条 `...` 占位、developer 系统提示、工具回合正文 `...`。
        let msgs = vec![
            json!({"role": "user", "content": "..."}),
            json!({"role": "developer", "content": "你是 pi"}),
            json!({"role": "user", "content": "首句"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "..."},
                                                    {"type": "tool_use", "id": "t1", "name": "read", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}),
        ];
        let c = clean(&msgs);
        assert_eq!(c.promoted_system, vec!["你是 pi".to_string()]);
        assert_eq!(c.messages.len(), 3);
        assert_eq!(c.messages[0], json!({"role": "user", "content": "首句"}));
        assert_eq!(c.messages[1]["content"].as_array().unwrap().len(), 1, "`...` 文本已删,只剩 tool_use");
    }

    #[test]
    fn late_system_becomes_user_and_tool_results_stay_first_after_merge() {
        let msgs = vec![
            json!({"role": "user", "content": "q"}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "x", "input": {}}]}),
            json!({"role": "system", "content": "插话"}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}),
        ];
        let c = clean(&msgs);
        assert!(c.promoted_system.is_empty());
        assert_eq!(c.messages.len(), 3);
        let last = c.messages[2]["content"].as_array().unwrap();
        assert_eq!(last[0]["type"], "tool_result");
        assert!(last[1]["text"].as_str().unwrap().contains("<system_context>"));
    }

    #[test]
    fn empty_text_blocks_and_empty_messages_are_dropped() {
        let msgs = vec![
            json!({"role": "user", "content": "q"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "  "}]}),
            json!({"role": "user", "content": [{"type": "text", "text": ""}, {"type": "text", "text": "再问"}]}),
        ];
        let c = clean(&msgs);
        assert_eq!(c.messages.len(), 1, "空 assistant 删掉后两条 user 合并");
        assert_eq!(c.messages[0]["content"], json!([{"type": "text", "text": "q"}, {"type": "text", "text": "再问"}]));
    }

    #[test]
    fn lone_placeholder_is_kept_when_it_is_the_whole_turn() {
        let msgs = vec![json!({"role": "user", "content": "..."})];
        assert_eq!(clean(&msgs).messages, msgs);
        let msgs = vec![json!({"role": "user", "content": "q"}), json!({"role": "assistant", "content": [{"type": "text", "text": "..."}]})];
        assert_eq!(clean(&msgs).messages, msgs, "没有 tool_use 的 `...` 是唯一内容,不动");
    }
}
