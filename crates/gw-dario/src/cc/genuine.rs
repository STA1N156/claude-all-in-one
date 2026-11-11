//! 「真 Claude Code」判据,逐字照搬 dario `cc-template.ts::isGenuineCCClient`。
//!
//! 两边必须一致:caio 判为真 CC 的请求原样交给 dario,dario 也得判为真 CC 才会走透传;
//! caio 翻译出来的报文,也必须让 dario 判为真 CC(否则 dario 会再套一遍自己的模板)。

use serde_json::Value;

/// dario v6.18.2 的 `CC_ORIGIN_SYSTEM_OPENERS`。
const CC_ORIGIN_SYSTEM_OPENERS: [&str; 5] = [
    "You are Claude Code",
    "You are an agent for Claude Code",
    "You are a file search specialist for Claude Code",
    "You are a software architect and planning specialist for Claude Code",
    "You are a security monitor for autonomous AI coding agents",
];

/// system 是至少两块的数组,第一块含计费头,第二块以 CC 开场白开头或提到 Claude Agent SDK。
pub(crate) fn is_genuine_cc(body: &Value) -> bool {
    let Some(sys) = body.get("system").and_then(Value::as_array) else {
        return false;
    };
    if sys.len() < 2 {
        return false;
    }
    let text = |i: usize| sys[i].get("text").and_then(Value::as_str);
    let Some(first) = text(0) else {
        return false;
    };
    if !first.contains("x-anthropic-billing-header:") {
        return false;
    }
    let Some(second) = text(1) else {
        return false;
    };
    CC_ORIGIN_SYSTEM_OPENERS.iter().any(|o| second.starts_with(o)) || second.contains("Claude Agent SDK")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn needs_billing_block_then_cc_opener() {
        let ok = json!({"system": [{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1;"},
                                   {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}]});
        assert!(is_genuine_cc(&ok));
        let sdk = json!({"system": [{"text": "x-anthropic-billing-header: a"}, {"text": "You are a Claude agent, built on Anthropic's Claude Agent SDK."}]});
        assert!(is_genuine_cc(&sdk));
        // 只有身份行、没有计费块 = 不是真 CC(dario 会走模板路径)。
        let no_billing = json!({"system": [{"text": "You are Claude Code"}, {"text": "You are Claude Code"}]});
        assert!(!is_genuine_cc(&no_billing));
        assert!(!is_genuine_cc(&json!({"system": "x-anthropic-billing-header: a\nYou are Claude Code"})));
        let one_block = json!({"system": [{"text": "x-anthropic-billing-header: a"}]});
        assert!(!is_genuine_cc(&one_block));
    }
}
