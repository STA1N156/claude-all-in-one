//! 把一条非 Claude Code 客户端的请求拼成「dario 眼里的真 CC」报文。
//!
//! 字段与取值尽量和 dario 模板路径(`cc-template.ts::buildCCRequest`)一致,只有一处故意不同:
//! **不注入 CC 自己的系统提示词**。dario 的研究(`docs/research/system-prompt-classifier-study.md`)
//! 实测系统提示词的内容、长度、块数都不影响计费分类;注入只会每轮多花 token,还会和客户端
//! 自己的指令打架。system 只保留 CC 的结构:计费头占位 + 身份行 + 客户端指令。

use std::sync::Arc;

use gw_core::clients::ClientKind;
use serde_json::{json, Map, Value};

use super::cleanup;
use super::order::to_ordered_bytes;
use super::scrub::scrub_system;
use super::template::CcTemplate;
use super::tools::{self, Mode, Stats, ToolMap};

/// CC 2.1.143 起的默认输出上限(dario `DEFAULT_MAX_TOKENS`)。
const DEFAULT_MAX_TOKENS: u64 = 64000;
/// OpenCode 的系统提示开头(走 Anthropic 时前面会多一块 CC 身份行,所以按行找)。
const OPENCODE_LINES: [&str; 2] = ["You are OpenCode", "You are an AI agent running in OpenCode"];
/// 客户端指令前的引导语:CC 的结构里第三块是「运营方给的任务指令」。
const CLIENT_PREFACE: &str =
    "The operator of this session has supplied the following task-specific instructions. Follow them exactly:\n\n";
const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

pub(crate) struct Translated {
    pub body: Vec<u8>,
    /// 有改过名的工具才有;回包按它改回客户端的名字与参数。
    pub map: Option<Arc<ToolMap>>,
    pub stats: Stats,
}

/// 支持的客户端翻译成 CC 报文;不支持返回拒绝原因(给客户端看的英文 + 日志用的客户端类型在调用方)。
pub(crate) fn translate(kind: ClientKind, body: &Value, tpl: &CcTemplate) -> Result<Translated, String> {
    let client_tools: Vec<Value> = body.get("tools").and_then(Value::as_array).cloned().unwrap_or_default();
    let built = build_tools(kind, &client_tools, &client_system_text(body), tpl)?;

    let messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let cleaned = cleanup::clean(&messages);
    let mut messages = cleaned.messages;
    rewrite_history(&mut messages, &built.map);
    if messages.is_empty() {
        return Err("request has no messages".into());
    }

    let mut system_text = client_system_text(body);
    for extra in &cleaned.promoted_system {
        if !system_text.is_empty() {
            system_text.push_str("\n\n");
        }
        system_text.push_str(extra);
    }
    let mut note = built.map.correspondence_note();
    for extra in built.notes {
        if !note.is_empty() {
            note.push(' ');
        }
        note.push_str(extra);
    }
    let system = build_system(tpl, &scrub_system(&system_text), &note);

    let model = body.get("model").cloned().unwrap_or(Value::Null);
    let model_name = model.as_str().unwrap_or("").to_ascii_lowercase();
    let mut out = Map::new();
    out.insert("model".into(), model);
    out.insert("messages".into(), Value::Array(messages));
    out.insert("system".into(), system);
    if !built.wire_tools.is_empty() {
        out.insert("tools".into(), Value::Array(built.wire_tools.clone()));
    }
    // 占位:dario 会写入它自己的身份。先占住位置,dario 改值不改位置(JS 对象覆写已有键)。
    out.insert("metadata".into(), json!({"user_id": ""}));
    out.insert("max_tokens".into(), json!(max_tokens_for(&model_name)));
    let tool_choice = tool_choice(body, &built.map, &out);
    // 强制调用工具(any / 指定工具)与扩展思考不能同时开,Anthropic 会 400。客户端要的是强制调用:
    // 支持自适应思考的模型显式关思考 —— 光不写 thinking 不够,Opus 5 不写也会思考(dario 实测);
    // 而 `disabled` 在 Opus 5 上 effort 高于 high 会 400,所以同时把 effort 压到 high 以内。
    let forced_tool = tool_choice
        .as_ref()
        .and_then(|tc| tc.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|t| t == "any" || t == "tool");
    let mut output_config = Map::new();
    if !model_name.contains("haiku") {
        let mut effort = effort(body);
        if supports_adaptive_thinking(&model_name) {
            if forced_tool {
                out.insert("thinking".into(), json!({"type": "disabled"}));
                if matches!(effort, "xhigh" | "max") {
                    effort = "high";
                }
            } else {
                out.insert("thinking".into(), json!({"type": "adaptive", "display": "omitted"}));
                out.insert("context_management".into(), json!({"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]}));
            }
        }
        output_config.insert("effort".into(), json!(effort));
    }
    // 客户端要的结构化输出(JSON schema)原样带上,丢了客户端的解析器会坏。
    if let Some(format) = body.pointer("/output_config/format") {
        output_config.insert("format".into(), format.clone());
    }
    if !output_config.is_empty() {
        out.insert("output_config".into(), Value::Object(output_config));
    }
    out.insert("stream".into(), Value::Bool(true));
    if let Some(tc) = tool_choice {
        out.insert("tool_choice".into(), tc);
    }

    let stats = built.stats;
    let map = built.map.has_renames().then(|| Arc::new(built.map));
    Ok(Translated { body: to_ordered_bytes(&out, &tpl.body_field_order), map, stats })
}

/// 选工具处理方式并建映射表。
///
/// - 不带工具的请求:只放行认出来的支持客户端(OpenCode、Copilot、Claude Code 变体),认不出的拒。
///   用户 10-30 定:不给它们补工具冒充 agent 请求 —— 真 CC 自己的辅助调用(起标题、压缩、安全检查)
///   本来就不带工具,补一整套 CC 工具(约一两万 token、还得 `tool_choice: none`)反而和真 CC 对不上,
///   所以宁可拒。代价:系统提示与主对话不同的辅助调用(如 OpenCode 的 title generator)会被认成
///   兜底而拒掉。
/// - Claude Code 变体:工具得是 CC 风格(模板原生 / 首字母大写 / `mcp__*`)。不是的话看 system 里
///   有没有 OpenCode 那一行 —— OpenCode 走 Anthropic 时会在前面加一块 CC 身份行,被认成 CC,
///   但工具是 OpenCode 的小写工具,要按 OpenCode 的表翻译,不能原样发。
fn build_tools(kind: ClientKind, client_tools: &[Value], system_text: &str, tpl: &CcTemplate) -> Result<tools::Built, String> {
    let unsupported = |e: String| format!("client '{}' tools not supported: {e}", kind.as_str());
    if client_tools.is_empty() {
        return match kind {
            ClientKind::OpenCode | ClientKind::Copilot | ClientKind::ClaudeCode => {
                tools::build(Mode::CcSurface { strict: true }, client_tools, tpl).map_err(unsupported)
            }
            other => Err(format!("client '{}' is not supported on this route", other.as_str())),
        };
    }
    match kind {
        ClientKind::OpenCode => tools::build(Mode::Table(&tools::OPENCODE), client_tools, tpl).map_err(unsupported),
        ClientKind::Copilot => tools::build(Mode::Table(&tools::COPILOT), client_tools, tpl).map_err(unsupported),
        ClientKind::ClaudeCode => match tools::build(Mode::CcSurface { strict: false }, client_tools, tpl) {
            Ok(b) => Ok(b),
            Err(_) if has_opencode_line(system_text) => {
                tools::build(Mode::Table(&tools::OPENCODE), client_tools, tpl).map_err(unsupported)
            }
            Err(e) => Err(unsupported(e)),
        },
        // 认不出来,但工具全是模板里的 CC 原生名或 MCP:按 CC 变体处理。
        ClientKind::Fallback => tools::build(Mode::CcSurface { strict: true }, client_tools, tpl).map_err(unsupported),
        other => Err(format!("client '{}' is not supported on this route", other.as_str())),
    }
}

fn has_opencode_line(system_text: &str) -> bool {
    system_text.lines().map(str::trim_start).any(|l| OPENCODE_LINES.iter().any(|p| l.starts_with(p)))
}

/// CC 对各模型的输出上限:老模型上限更低,发 64000 会被上游 400(dario 也没按模型限,这里补上)。
fn max_tokens_for(model: &str) -> u64 {
    if model.contains("opus-4-1") || model.contains("opus-4-2025") || model.ends_with("opus-4") || model.contains("opus-4-0") {
        32000
    } else if model.contains("3-5-") || model.contains("3-haiku") {
        8192
    } else {
        DEFAULT_MAX_TOKENS
    }
}

/// 客户端 top-level system 的文本(多块用空行隔开);去掉客户端自带的计费头行(那一行只该在第一块)。
fn client_system_text(body: &Value) -> String {
    let raw = match body.get("system") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .filter(|t| !t.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    };
    raw.lines()
        .filter(|l| !l.trim_start().starts_with(BILLING_HEADER_PREFIX))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// [计费头占位, 身份行, 引导语 + 客户端指令 + 工具名对照]。第一块 dario 会换成它自己的计费标签,
/// 这里只要能通过它的真 CC 判据;缓存断点由 dario 按真 CC 的位置重打,这里不放。
fn build_system(tpl: &CcTemplate, client_text: &str, note: &str) -> Value {
    let billing = format!("{BILLING_HEADER_PREFIX} cc_version={}; cc_entrypoint=sdk-cli;", tpl.version);
    let mut blocks = vec![json!({"type": "text", "text": billing}), json!({"type": "text", "text": tpl.agent_identity})];
    let mut third = String::new();
    if !client_text.is_empty() {
        third.push_str(CLIENT_PREFACE);
        third.push_str(client_text);
    }
    if !note.is_empty() {
        if !third.is_empty() {
            third.push_str("\n\n");
        }
        third.push_str(note);
    }
    if !third.is_empty() {
        blocks.push(json!({"type": "text", "text": third}));
    }
    Value::Array(blocks)
}

/// 历史里 assistant 的 tool_use 换成出站名/参数;客户端带的 cache_control 去掉 —— dario 透传
/// 路径会按真 CC 的位置重打。
///
/// 消息正文不擦客户端名字(dario 模板路径会擦「正文安全子集」):正文不是计费分类器的输入,
/// 擦了反而可能改坏用户的代码与工具结果(源码里的 "Roo Code" 字符串会被删)。只擦 system。
fn rewrite_history(messages: &mut [Value], map: &ToolMap) {
    for m in messages.iter_mut() {
        let is_assistant = m.get("role").and_then(Value::as_str) == Some("assistant");
        if let Some(Value::Array(blocks)) = m.get_mut("content") {
            for b in blocks.iter_mut() {
                rewrite_block(b, map, is_assistant);
            }
        }
    }
}

fn rewrite_block(b: &mut Value, map: &ToolMap, is_assistant: bool) {
    let Some(obj) = b.as_object_mut() else {
        return;
    };
    obj.remove("cache_control");
    // tool_result 只留 CC 会发的四个字段(同 dario 模板路径):翻译后 dario 走透传不再替我们清,
    // 客户端或中转多塞的字段到上游是「Extra inputs are not permitted」,整个会话之后每轮都 400。
    if obj.get("type").and_then(Value::as_str) == Some("tool_result") {
        obj.retain(|k, _| matches!(k.as_str(), "type" | "tool_use_id" | "content" | "is_error"));
    }
    if is_assistant && obj.get("type").and_then(Value::as_str) == Some("tool_use") {
        let name = obj.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        let input = obj.get("input").cloned().unwrap_or(json!({}));
        if let Some((wire, args)) = map.forward(&name, &input) {
            obj.insert("name".into(), Value::String(wire));
            obj.insert("input".into(), args);
        }
    }
}

/// 思考强度:客户端给了 `output_config.effort` 就用(规范到接口认的五档),否则 high(同 dario)。
fn effort(body: &Value) -> &'static str {
    let raw = body.pointer("/output_config/effort").and_then(Value::as_str).unwrap_or("");
    match raw.trim().to_ascii_lowercase().as_str() {
        "low" => "low",
        "medium" => "medium",
        "xhigh" | "ultracode" => "xhigh",
        "max" => "max",
        _ => "high",
    }
}

/// 客户端的 tool_choice 能对上出站工具才带(没有 tools 时带 tool_choice 上游 400);
/// 强制某个工具时换成出站名。
fn tool_choice(body: &Value, map: &ToolMap, out: &Map<String, Value>) -> Option<Value> {
    if out.get("tools").and_then(Value::as_array).is_none_or(|t| t.is_empty()) {
        return None;
    }
    let tc = body.get("tool_choice")?.as_object()?;
    match tc.get("type").and_then(Value::as_str)? {
        "auto" | "any" | "none" => Some(Value::Object(tc.clone())),
        "tool" => {
            let name = tc.get("name").and_then(Value::as_str)?;
            let wire = map.wire_name(name).unwrap_or(name);
            let offered = out
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|ts| ts.iter().any(|t| t.get("name").and_then(Value::as_str) == Some(wire)));
            offered.then(|| {
                let mut tc2 = tc.clone();
                tc2.insert("name".into(), Value::String(wire.into()));
                Value::Object(tc2)
            })
        }
        _ => None,
    }
}

/// 照搬 dario `supportsAdaptiveThinking`:opus/sonnet/fable 的 4.6 及以上、5 及以上用自适应思考。
/// 原版是两条正则(`fam-(\d{1,2})-(\d{1,2})\b`,不中再试 `fam-(\d{1,2})(?!\d|-)` 且只认 5+);
/// Rust 的 regex 没有前瞻,手写同样的匹配。`model` 已转小写。
fn supports_adaptive_thinking(model: &str) -> bool {
    const FAMILIES: [&str; 3] = ["opus-", "sonnet-", "fable-"];
    let starts: Vec<usize> = (0..model.len())
        .filter(|&i| model.is_char_boundary(i) && FAMILIES.iter().any(|f| model[i..].starts_with(f)))
        .collect();
    let after_family = |i: usize| -> &str {
        let f = FAMILIES.iter().find(|f| model[i..].starts_with(*f)).expect("上面筛过");
        &model[i + f.len()..]
    };
    // 第一条:major-minor,minor 后面是单词边界。
    for &i in &starts {
        if let Some((major, minor)) = major_minor(after_family(i)) {
            return major > 4 || (major == 4 && minor >= 6);
        }
    }
    // 第二条:只有 major,后面不是数字也不是 `-`。
    for &i in &starts {
        if let Some(major) = major_only(after_family(i)) {
            return major >= 5;
        }
    }
    false
}

/// 1~2 位数字(先试两位,不行再试一位,同正则的回溯)。返回 (数值, 剩余)。
fn digit_prefixes(s: &str) -> Vec<(u32, &str)> {
    let n = s.bytes().take(2).take_while(u8::is_ascii_digit).count();
    (1..=n).rev().map(|k| (s[..k].parse().expect("纯数字"), &s[k..])).collect()
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn major_minor(s: &str) -> Option<(u32, u32)> {
    for (major, rest) in digit_prefixes(s) {
        let Some(rest) = rest.strip_prefix('-') else { continue };
        for (minor, tail) in digit_prefixes(rest) {
            // `\b`:minor 的最后一位是单词字符,后面必须不是单词字符(或到头)。
            if tail.bytes().next().is_none_or(|b| !is_word_byte(b)) {
                return Some((major, minor));
            }
        }
    }
    None
}

fn major_only(s: &str) -> Option<u32> {
    digit_prefixes(s)
        .into_iter()
        .find(|(_, rest)| rest.bytes().next().is_none_or(|b| !b.is_ascii_digit() && b != b'-'))
        .map(|(major, _)| major)
}

#[cfg(test)]
mod tests {
    use super::super::genuine::is_genuine_cc;
    use super::super::template::test_template;
    use super::*;

    fn opencode_body() -> Value {
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 4096,
            "temperature": 0.3,
            "top_p": 0.9,
            "stream": true,
            "system": [{"type": "text", "text": "You are OpenCode, the best coding agent.", "cache_control": {"type": "ephemeral"}}],
            "tools": [
                {"name": "read", "description": "Read a file", "input_schema": {"type": "object", "properties": {"path": {}, "offset": {}, "limit": {}}}},
                {"name": "todowrite", "description": "todo", "input_schema": {"type": "object", "properties": {"todos": {}}}}
            ],
            "messages": [
                {"role": "user", "content": "看看 main.rs"},
                {"role": "assistant", "content": [{"type": "text", "text": "..."},
                    {"type": "tool_use", "id": "t1", "name": "read", "input": {"path": "/src/main.rs"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "fn main() {}"}]}
            ]
        })
    }

    fn parse(t: &Translated) -> Value {
        serde_json::from_slice(&t.body).unwrap()
    }

    #[test]
    fn opencode_request_becomes_genuine_cc_shape() {
        let tpl = test_template();
        let t = translate(ClientKind::OpenCode, &opencode_body(), &tpl).unwrap();
        let v = parse(&t);
        assert!(is_genuine_cc(&v), "dario 必须把翻译结果判为真 CC");
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        // serde_json 解析回来是字母序,字段顺序看原始字节。
        let s = String::from_utf8(t.body.clone()).unwrap();
        let pos = |k: &str| s.find(&format!("\"{k}\":")).unwrap();
        assert!(pos("model") < pos("messages") && pos("messages") < pos("system") && pos("system") < pos("tools"));
        assert!(pos("tools") < pos("metadata") && pos("metadata") < pos("max_tokens") && pos("max_tokens") < pos("thinking"));
        assert!(pos("output_config") < pos("stream"));
        assert!(!keys.contains(&"temperature") && !keys.contains(&"top_p"), "采样参数去掉");
        assert_eq!(v["max_tokens"], 64000);
        assert_eq!(v["thinking"]["type"], "adaptive");
        assert_eq!(v["output_config"]["effort"], "high");
        // 工具:read → Read(CC 原版定义),todowrite → MCP。
        let names: Vec<&str> = v["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Read", "mcp__workspace__todowrite"]);
        // 历史里的 tool_use 换成 CC 名与参数,`...` 已删。
        let asst = v["messages"][1]["content"].as_array().unwrap();
        assert_eq!(asst.len(), 1);
        assert_eq!(asst[0]["name"], "Read");
        assert_eq!(asst[0]["input"], json!({"file_path": "/src/main.rs"}));
        // system 第三块:客户端名已擦、带工具名对照,不带 CC 自己的提示词。
        let third = v["system"][2]["text"].as_str().unwrap();
        assert!(third.contains("You are , the best coding agent."));
        assert!(third.contains("read -> Read"));
        assert!(v["system"][2].get("cache_control").is_none());
        assert_eq!(t.stats, Stats { native: 1, renamed: 1, same: 0 });
        assert!(t.map.is_some());
    }

    #[test]
    fn translation_is_deterministic() {
        let tpl = test_template();
        let a = translate(ClientKind::OpenCode, &opencode_body(), &tpl).unwrap();
        let b = translate(ClientKind::OpenCode, &opencode_body(), &tpl).unwrap();
        assert_eq!(a.body, b.body, "同样的输入必须得到逐字节相同的输出(缓存前缀)");
    }

    #[test]
    fn unsupported_clients_are_rejected() {
        let tpl = test_template();
        let body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        // 不带工具:只放行认出来的支持客户端。
        assert!(translate(ClientKind::GrokBuild, &body, &tpl).is_err());
        assert!(translate(ClientKind::Fallback, &body, &tpl).is_err(), "认不出、也没有工具");
        assert!(translate(ClientKind::OpenCode, &body, &tpl).is_ok());
        assert!(translate(ClientKind::ClaudeCode, &body, &tpl).is_ok());
        let with_tools = json!({"model": "m", "tools": [{"name": "spawn_subagent"}], "messages": [{"role": "user", "content": "hi"}]});
        assert!(translate(ClientKind::GrokBuild, &with_tools, &tpl).is_err(), "带工具的不支持客户端照样拒");
        assert!(translate(ClientKind::DeveloperRole, &with_tools, &tpl).is_err());
        let foreign = json!({"model": "m", "tools": [{"name": "read_file"}], "messages": [{"role": "user", "content": "hi"}]});
        assert!(translate(ClientKind::Fallback, &foreign, &tpl).is_err(), "工具不是 CC 形态");
        let cc_like = json!({"model": "m", "tools": [{"name": "Read"}, {"name": "mcp__x__y"}], "messages": [{"role": "user", "content": "hi"}]});
        assert!(translate(ClientKind::Fallback, &cc_like, &tpl).is_ok(), "CC 工具面的认不出客户端照样支持");
    }

    #[test]
    fn haiku_gets_no_thinking_and_tool_choice_is_mapped() {
        let tpl = test_template();
        let mut body = opencode_body();
        body["model"] = json!("claude-haiku-4-5");
        body["tool_choice"] = json!({"type": "tool", "name": "read"});
        let v = parse(&translate(ClientKind::OpenCode, &body, &tpl).unwrap());
        assert!(v.get("thinking").is_none() && v.get("output_config").is_none());
        assert_eq!(v["tool_choice"], json!({"type": "tool", "name": "Read"}));
    }

    #[test]
    fn forced_tool_choice_turns_thinking_off_but_auto_keeps_it() {
        let tpl = test_template();
        let mut body = opencode_body();
        body["tool_choice"] = json!({"type": "any"});
        body["output_config"] = json!({"effort": "max"});
        let v = parse(&translate(ClientKind::OpenCode, &body, &tpl).unwrap());
        assert_eq!(v["thinking"], json!({"type": "disabled"}), "强制调用工具时显式关思考(Opus 5 不写也会思考)");
        assert!(v.get("context_management").is_none());
        assert_eq!(v["output_config"]["effort"], "high", "disabled 时 effort 压到 high 以内");
        assert_eq!(v["tool_choice"], json!({"type": "any"}));
        body["tool_choice"] = json!({"type": "auto"});
        let v = parse(&translate(ClientKind::OpenCode, &body, &tpl).unwrap());
        assert_eq!(v["thinking"]["type"], "adaptive");
    }

    #[test]
    fn history_thinking_is_stripped_and_content_is_left_alone() {
        let tpl = test_template();
        let mut body = opencode_body();
        body["messages"][1]["content"] = json!([
            {"type": "thinking", "thinking": "想", "signature": "kiro-synthetic"},
            {"type": "tool_use", "id": "t1", "name": "read", "input": {"path": "/a"}}
        ]);
        body["messages"][0]["content"] = json!("const product = \"Roo Code\";");
        let v = parse(&translate(ClientKind::OpenCode, &body, &tpl).unwrap());
        let asst = v["messages"][1]["content"].as_array().unwrap();
        assert!(asst.iter().all(|b| b["type"] != "thinking"), "历史 thinking 删掉(同 dario 模板路径)");
        assert_eq!(v["messages"][0]["content"], "const product = \"Roo Code\";", "正文不擦");
    }

    #[test]
    fn opencode_behind_cc_identity_line_still_gets_its_tools_mapped() {
        let tpl = test_template();
        let mut body = opencode_body();
        body["system"] = json!([{"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
                                {"type": "text", "text": "You are OpenCode, the best coding agent."}]);
        let v = parse(&translate(ClientKind::ClaudeCode, &body, &tpl).unwrap());
        let names: Vec<&str> = v["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Read", "mcp__workspace__todowrite"], "小写工具不能原样发");
        // 真 CC 变体:CC 风格的工具(含模板外的 ListMcpResourcesTool)原样。
        let cc = json!({"model": "m", "system": "You are Claude Code, Anthropic's official CLI for Claude.",
                        "tools": [{"name": "Read"}, {"name": "ListMcpResourcesTool"}], "messages": [{"role": "user", "content": "hi"}]});
        assert!(translate(ClientKind::ClaudeCode, &cc, &tpl).is_ok());
    }

    #[test]
    fn misc_request_fields() {
        let tpl = test_template();
        let mut body = opencode_body();
        body["tools"] = json!([]);
        body["tool_choice"] = json!({"type": "auto"});
        body["output_config"] = json!({"format": {"type": "json_schema", "schema": {"type": "object"}}});
        body["messages"][2]["content"][0]["x_extra"] = json!(1);
        let v = parse(&translate(ClientKind::OpenCode, &body, &tpl).unwrap());
        assert!(v.get("tools").is_none() && v.get("tool_choice").is_none(), "没有工具就不带 tool_choice");
        assert_eq!(v["output_config"]["format"]["type"], "json_schema", "结构化输出保留");
        assert!(v["messages"][2]["content"][0].get("x_extra").is_none(), "tool_result 多余字段去掉");
        assert_eq!(max_tokens_for("claude-opus-4-1-20250805"), 32000);
        assert_eq!(max_tokens_for("claude-3-5-haiku-20241022"), 8192);
        assert_eq!(max_tokens_for("claude-opus-5-5"), 64000);
    }

    #[test]
    fn adaptive_thinking_matches_dario_rule() {
        for (m, want) in [
            ("claude-opus-4-5-20251101", false),
            ("claude-opus-4-6", true),
            ("claude-sonnet-4-6[1m]", true),
            ("claude-sonnet-5-5", true),
            ("claude-opus-5", true),
            ("claude-fable-5-1", true),
            ("claude-opus-4-20250514", false),
            ("claude-haiku-4-5", false),
            ("claude-3-5-sonnet-20241022", false),
            ("claude-sonnet-4", false),
        ] {
            assert_eq!(supports_adaptive_thinking(m), want, "{m}");
        }
    }

    #[test]
    fn effort_normalized_to_wire_values() {
        assert_eq!(effort(&json!({"output_config": {"effort": "ULTRACODE"}})), "xhigh");
        assert_eq!(effort(&json!({"output_config": {"effort": "low"}})), "low");
        assert_eq!(effort(&json!({})), "high");
    }
}
