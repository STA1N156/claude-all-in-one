//! 文本 token 估算(通用件)—— 纯本地字符级估算,**零网络**。
//!
//! 2026-10-28 从 gw-kiro `text_tokens` 迁入 gw-core:入口策略(逐条筛查)、调度器的输入量
//! 粗估、分类器本地直答的 usage、router 的 `count_tokens` 都在用,不是 Kiro 专属。
//! 按模型分词器折算输出的部分(`Tokenizer` / `OutputTally`)仍在 gw-kiro。
//!
//! 计算规则(逐字节对齐旧代码,保证 cache_sim 跨项目同口径,**不可改动**):
//! - 非西文字符(中日韩等)按 4 字符单位、西文按 1;4 单位 = 1 token;
//! - 再按档位非线性放大(<100 ×1.5 … ≥800 ×1.0),贴合真实 tokenizer 短文本高开销。

/// 判断字符是否为非西文字符(中日韩/阿拉伯等);西文含 ASCII 与拉丁扩展区。
fn is_non_western_char(c: char) -> bool {
    !matches!(c,
        '\u{0000}'..='\u{007F}'
        | '\u{0080}'..='\u{00FF}'
        | '\u{0100}'..='\u{024F}'
        | '\u{1E00}'..='\u{1EFF}'
        | '\u{2C60}'..='\u{2C7F}'
        | '\u{A720}'..='\u{A7FF}'
        | '\u{AB30}'..='\u{AB6F}'
    )
}

/// 估算一段文本的 token 数(本地字符级,非线性档位放大)。
///
/// 🔵 与旧 `token::count_tokens` 逐字节一致:cache_sim 的指纹 token、命中量、total
/// 都依赖它,**不可改动**否则跨轮/跨项目命中比例失真。
pub fn count_tokens(text: &str) -> u64 {
    let char_units: f64 = text
        .chars()
        .map(|c| if is_non_western_char(c) { 4.0 } else { 1.0 })
        .sum();

    let tokens = char_units / 4.0;

    (if tokens < 100.0 {
        tokens * 1.5
    } else if tokens < 200.0 {
        tokens * 1.3
    } else if tokens < 300.0 {
        tokens * 1.25
    } else if tokens < 800.0 {
        tokens * 1.2
    } else {
        tokens * 1.0
    }) as u64
}

/// 估算一个完整 Anthropic Messages 请求体的 input token(`/v1/messages/count_tokens` 用)。
///
/// 🔵 口径对齐 kiro.rs `token::count_all_tokens_local`:system 文本 + 各消息的
/// 字符串/数组 text 块 + 工具 name/description/input_schema(序列化后计字)。
/// 纯本地估算,零网络;结果至少 1。计费不依赖它(计费走上游 usage),只为
/// NewAPI/客户端预估兼容。
pub fn count_request_tokens(body: &serde_json::Value) -> u64 {
    let mut total: u64 = 0;

    // system:字符串或 [{type:text, text}] 数组两种形态。
    match body.get("system") {
        Some(serde_json::Value::String(s)) => total += count_tokens(s),
        Some(serde_json::Value::Array(arr)) => {
            for item in arr {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    total += count_tokens(text);
                }
            }
        }
        _ => {}
    }

    // messages:content 为字符串,或内容块数组(只计 text 字段,对齐 kiro.rs)。
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            match msg.get("content") {
                Some(serde_json::Value::String(s)) => total += count_tokens(s),
                Some(serde_json::Value::Array(arr)) => {
                    for item in arr {
                        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                            total += count_tokens(text);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // tools:name + description + input_schema(JSON 字符串化计字)。
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        for tool in tools {
            if let Some(name) = tool.get("name").and_then(|v| v.as_str()) {
                total += count_tokens(name);
            }
            if let Some(desc) = tool.get("description").and_then(|v| v.as_str()) {
                total += count_tokens(desc);
            }
            if let Some(schema) = tool.get("input_schema") {
                let schema_json = serde_json::to_string(schema).unwrap_or_default();
                total += count_tokens(&schema_json);
            }
        }
    }

    total.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_counts_one_unit_per_char() {
        // 纯英文短串:char_units = len,tokens = len/4,<100 档 ×1.5
        let t = count_tokens("hello world"); // 11 chars → 2.75 → ×1.5 = 4
        assert_eq!(t, 4);
    }

    #[test]
    fn cjk_counts_four_units_per_char() {
        // 4 个中文 → 16 units → 4 tokens → <100 档 ×1.5 = 6
        assert_eq!(count_tokens("你好世界"), 6);
    }

    #[test]
    fn empty_is_zero() {
        assert_eq!(count_tokens(""), 0);
    }

    #[test]
    fn long_text_no_multiplier() {
        // ≥800 token 档 ×1.0:4000 ascii → 1000 units → 1000 tokens(无放大)
        let big = "a".repeat(4000);
        assert_eq!(count_tokens(&big), 1000);
    }

    #[test]
    fn request_tokens_sums_system_messages_tools() {
        let body = serde_json::json!({
            "model": "claude-sonnet-4-5",
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "hello world"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "hi there"},
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
                ]}
            ],
            "tools": [{"name": "f", "description": "does things", "input_schema": {"type": "object"}}]
        });
        let total = count_request_tokens(&body);
        // 各段独立估算后求和;精确值依赖估算公式,这里只锚定关键性质。
        let sys = count_tokens("You are helpful.");
        assert!(total > sys, "应包含 system 之外的消息与工具贡献: {total}");
    }

    #[test]
    fn request_tokens_system_array_form_and_min_one() {
        let body = serde_json::json!({
            "system": [{"type": "text", "text": "anchor"}],
            "messages": []
        });
        assert!(count_request_tokens(&body) >= 1);
        // 空请求也至少 1(对齐 kiro.rs .max(1))。
        assert_eq!(count_request_tokens(&serde_json::json!({})), 1);
    }
}
