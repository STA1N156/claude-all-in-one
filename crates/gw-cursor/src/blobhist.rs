//! 自造会话状态:把客户端的完整历史编成 Cursor 服务端同款的「消息 blob」列表,
//! 放进 `AgentRunRequest.conversation_state.1`(root_prompt_messages_json)。
//!
//! ## 为什么
//!
//! 线协议原先在接不上服务端会话时(工具轮之后、续轮描述符缺失、客户端改了历史……)
//! 把整段历史折成**一条** user 文本重发(`chat::fold_history`)。Anthropic 的提示缓存
//! 只在消息块边界上命中,一条越长越新的大块每轮都要整段重写:2026-11-09 16~20 点
//! 5 个号实扣 $2761,按标准价拆 87% 是缓存写;90 万 token 级会话每次请求约 $10。
//!
//! Cursor 的会话状态其实托管在客户端:服务端把每条消息做成 JSON blob 经 KV `set_blob`
//! 推给客户端,会话状态 `.1` 只存这些 blob 的 sha256;下一轮按哈希来要。客户端
//! **自己构造** `.1`(OmniRoute 用它放系统提示)服务端照单全收。2026-11-09 实测:
//! 另开新会话、用同样的 blob 重建历史 → 直接读到上一请求写下的缓存;同会话追加两条 →
//! 服务端只来要新增的两个 blob,缓存只写 130 token。
//!
//! 所以「接不上就整段重写」不是代理的宿命:每次按客户端发来的完整历史重建同样的
//! blob 列表,前缀不变的部分自然命中。**前缀必须只由请求内容决定**(同样的历史 →
//! 逐字节同样的 blob),这是整个模块的不变量。
//!
//! ## 格式(照服务端推来的实物,AI SDK 消息形态)
//!
//! ```text
//! user      {"role":"user","content":[<图片…>,{"type":"text","text":"<user_query>\n…\n</user_query>"}]}
//!           图片:{"type":"image","image":{"__type":"Uint8Array","hex":…},"mimeType":…}
//! assistant {"role":"assistant","content":[{"type":"text","text":…},
//!            {"type":"tool-call","toolCallId":…,"toolName":"CallDynamicTool",
//!             "args":{"namespace":"claude-local","toolName":<客户端工具名>,"arguments":{…}}}]}
//! tool      {"role":"tool","content":[{"type":"tool-result","toolCallId":…,"toolName":"CallDynamicTool",
//!            "result":<文本>,"experimental_content":[{"type":"text","text":<文本>},
//!            {"type":"image","data":<base64>,"mimeType":…}…]}]}
//! ```
//!
//! 客户端的工具在 Cursor 那边一律经 `CallDynamicTool` 中转(CLI 形态 MCP 服务名
//! `claude-local`),所以历史里的工具调用也按这个形态写。系统提示不放:服务端会自己
//! 加在最前面(实测不放也读到同样的系统提示缓存,不重复)。服务端自带的 `<user_info>`
//! (OS / 工作目录 / 日期)**不放**:它不会自动补,而客户端自己的系统提示里本来就有环境
//! 信息;Cursor 那份写的是我方服务器的 `/` 目录,放了反而误导。用一份全进程共享、随推送
//! 变化的 blob 打头还会让前缀哈希来回变、缓存失效,并把一个会话的环境信息带进另一个
//! 会话(代码审查指出)。
//!
//! ## 边界
//!
//! - 只在 CLI 形态、拿不到续轮描述符时用;续轮描述符与挂流续跑本来就是增量。
//! - 图片(base64)编进 blob;本轮带的图片单独作为一条历史 user 消息(本轮消息只能是文字)。
//!   解不开的 base64 留占位文字并告警,不静默丢。文档(PDF 等)与非 base64 图片 → 不适用,照旧折叠。
//! - 历史里的 thinking 不回传;客户端回传的思考回声已由调用方(`thinking_echo`)置空。
//! - 末条若是 assistant(prefill)→ 不适用。
//!
//! 热开关 `cursor_blob_history`(默认关)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::protobuf::Writer;

/// `ConversationStateStructure.root_prompt_messages_json`(repeated bytes,blob 的 sha256)。
const CSS_ROOT_PROMPT: u32 = 1;

/// 客户端工具在 Cursor 侧的中转工具名(CLI 形态,实物)。
const DYNAMIC_TOOL: &str = "CallDynamicTool";

/// 工具结果为空时填的文本。
const EMPTY_TOOL_OUTPUT: &str = "(no output)";
/// 末条只有图片、没有文字时本轮用户消息的正文。
const IMAGE_ONLY_PROMPT: &str = "(见上方图片)";
/// 末条既没有文字也没有调用方提醒时的正文(本轮消息不能为空,否则上游静默挂起)。
const CONTINUE_PROMPT: &str = "(继续)";
/// 图片 base64 解不开时留在原位的占位文字。
const BAD_IMAGE_PLACEHOLDER: &str = "[图片无法解码,已省略]";

static ENABLED: AtomicBool = AtomicBool::new(false);

/// 热开关(`cursor_blob_history`)。
pub(crate) fn set_enabled(on: bool) {
    let was = ENABLED.swap(on, Ordering::Relaxed);
    if was != on {
        tracing::info!(on, "cursor:自造会话状态(消息 blob 历史)开关已切换");
    }
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 重建结果。
#[derive(Debug)]
pub(crate) struct Rebuilt {
    /// 会话状态(放进帧0 `1.1`):`.1` × N = 各 blob 的 sha256。
    pub css: Vec<u8>,
    /// 本请求的 blob(hash → 字节),服务端点名时由 WireDriver 从这里交。
    pub blobs: Arc<HashMap<[u8; 32], Vec<u8>>>,
    /// 本轮放进 action 的用户消息正文(非空)。
    pub current: String,
    /// 历史消息(blob)条数。
    pub messages: usize,
    /// blob 总字节。
    pub bytes: usize,
}

/// 末条消息(含其中的工具结果)有没有图片。
pub(crate) fn last_message_has_images(body: &Value) -> bool {
    fn img(v: &Value) -> bool {
        match v {
            Value::Array(a) => a.iter().any(img),
            Value::Object(o) => {
                o.get("type").and_then(Value::as_str) == Some("image") || o.get("content").is_some_and(img)
            }
            _ => false,
        }
    }
    body.get("messages")
        .and_then(Value::as_array)
        .and_then(|m| m.last())
        .and_then(|m| m.get("content"))
        .is_some_and(img)
}

/// 便宜的前置判断:这个请求能不能走自造会话状态(不做编码)。
///
/// **与 [`rebuild`] 是同一套条件**(代码审查:两者不一致时,被放进 CLI 形态的请求若重建
/// 失败,会退回只带文字的折叠,图片与工具结构都会丢):有历史、末条不是 assistant、
/// 没有不支持的媒体。满足它,`rebuild` 就一定返回 `Some`。
pub(crate) fn eligible(body: &Value) -> bool {
    let Some(msgs) = body.get("messages").and_then(Value::as_array) else {
        return false;
    };
    msgs.len() > 1
        && msgs.last().and_then(|m| m.get("role")).and_then(Value::as_str) != Some("assistant")
        && !has_unsupported_media(msgs)
}

/// 按请求体重建会话状态。`eligible(body)` 为真时一定返回 `Some`。
///
/// `current_fallback`:末条没有文字时(只有工具结果 / 只有图片)本轮用户消息的正文
/// (调用方给工具回路提醒 + 原始请求)。再没有就用固定占位,保证本轮消息非空。
pub(crate) fn rebuild(body: &Value, current_fallback: Option<String>) -> Option<Rebuilt> {
    if !eligible(body) {
        return None;
    }
    let msgs = body.get("messages")?.as_array()?;
    let (last, history) = msgs.split_last()?;
    let mut out: Vec<Vec<u8>> = Vec::new();
    for m in history {
        encode_message(m, &mut out);
    }
    // 末条:工具结果进历史(tool 消息),图片单独一条历史 user 消息,文字留作本轮用户消息。
    let last_part = split_last_user(last.get("content").unwrap_or(&Value::Null));
    for r in &last_part.results {
        out.push(tool_result_blob(r));
    }
    if !last_part.images.is_empty() {
        out.push(user_blob("", &last_part.images));
    }
    let current = if !last_part.text.trim().is_empty() {
        last_part.text
    } else if let Some(f) = current_fallback.filter(|f| !f.trim().is_empty()) {
        f
    } else if !last_part.images.is_empty() {
        IMAGE_ONLY_PROMPT.to_string()
    } else {
        CONTINUE_PROMPT.to_string()
    };
    let messages = out.len();
    let mut css = Writer::new();
    let mut blobs = HashMap::with_capacity(out.len());
    let mut bytes = 0usize;
    for b in out {
        let h: [u8; 32] = Sha256::digest(&b).into();
        css.bytes(CSS_ROOT_PROMPT, &h);
        bytes += b.len();
        blobs.insert(h, b);
    }
    Some(Rebuilt {
        css: css.into_bytes(),
        blobs: Arc::new(blobs),
        current,
        messages,
        bytes,
    })
}

/// 有没有**不支持**的媒体:文档(PDF 等,blob 格式未核实)、非 base64 来源的图片。
fn has_unsupported_media(msgs: &[Value]) -> bool {
    fn bad(v: &Value) -> bool {
        match v {
            Value::Array(a) => a.iter().any(bad),
            Value::Object(o) => {
                let kind = o.get("type").and_then(Value::as_str);
                kind == Some("document")
                    || (kind == Some("image") && base64_image(v).is_none())
                    || o.get("content").is_some_and(bad)
            }
            _ => false,
        }
    }
    msgs.iter().any(|m| m.get("content").is_some_and(bad))
}

/// Anthropic 图片块 → (media_type, base64 数据)。不是 base64 来源返回 `None`。
fn base64_image(b: &Value) -> Option<(&str, &str)> {
    let src = b.get("source")?;
    if src.get("type").and_then(Value::as_str) != Some("base64") {
        return None;
    }
    let mt = src.get("media_type").and_then(Value::as_str).unwrap_or("image/png");
    let data = src.get("data").and_then(Value::as_str)?;
    Some((mt, data))
}

/// 宽松解 base64:去掉空白,依次试标准 / URL 安全字母表(带不带填充)。
fn decode_base64_lenient(data: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    use base64::Engine as _;
    let compact: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    [STANDARD, URL_SAFE, STANDARD_NO_PAD, URL_SAFE_NO_PAD]
        .iter()
        .find_map(|e| e.decode(compact.as_bytes()).ok())
}

/// 查表转小写十六进制(大图每请求都要转,逐字节 `format!` 太慢)。
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize]);
        out.push(HEX[(b & 0x0f) as usize]);
    }
    String::from_utf8(out).expect("十六进制字符都是 ASCII")
}

/// 用户消息里的图片 → 图片 part(服务端实物:字节以十六进制放在 `{"__type":"Uint8Array","hex"}`)。
/// 解不开返回 `Err(占位文字)`,调用方把它放进正文,不静默丢图。
fn user_image_part(b: &Value) -> Result<Value, &'static str> {
    let Some((mt, data)) = base64_image(b) else {
        return Err(BAD_IMAGE_PLACEHOLDER);
    };
    match decode_base64_lenient(data) {
        Some(bytes) => Ok(json!({
            "type": "image",
            "image": {"__type": "Uint8Array", "hex": to_hex(&bytes)},
            "mimeType": mt
        })),
        None => {
            tracing::warn!(media_type = mt, len = data.len(), "cursor 自造会话状态:图片 base64 解不开,留占位文字");
            Err(BAD_IMAGE_PLACEHOLDER)
        }
    }
}

/// 工具结果里的图片 → experimental_content 的图片 part(base64 原样,只规整空白)。
fn tool_image_part(b: &Value) -> Result<Value, &'static str> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    let Some((mt, data)) = base64_image(b) else {
        return Err(BAD_IMAGE_PLACEHOLDER);
    };
    match decode_base64_lenient(data) {
        Some(bytes) => Ok(json!({"type": "image", "data": STANDARD.encode(bytes), "mimeType": mt})),
        None => {
            tracing::warn!(media_type = mt, len = data.len(), "cursor 自造会话状态:工具结果图片 base64 解不开,留占位文字");
            Err(BAD_IMAGE_PLACEHOLDER)
        }
    }
}

fn to_bytes(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).expect("json 值序列化不会失败")
}

fn user_blob(text: &str, images: &[Value]) -> Vec<u8> {
    let mut parts: Vec<Value> = images.to_vec();
    if !text.trim().is_empty() {
        parts.push(json!({"type": "text", "text": format!("<user_query>\n{text}\n</user_query>")}));
    }
    to_bytes(&json!({"role": "user", "content": parts}))
}

fn tool_result_blob(r: &ToolResult) -> Vec<u8> {
    let text = if r.text.trim().is_empty() { EMPTY_TOOL_OUTPUT } else { r.text.as_str() };
    let mut content = vec![json!({"type": "text", "text": text})];
    content.extend(r.images.iter().cloned());
    let mut part = json!({
        "type": "tool-result",
        "toolCallId": r.id,
        "toolName": DYNAMIC_TOOL,
        "result": text,
        "experimental_content": content
    });
    if r.is_error {
        part["isError"] = json!(true);
    }
    to_bytes(&json!({"role": "tool", "content": [part]}))
}

/// 追加一段文字(换行分隔)。
fn push_line(buf: &mut String, t: &str) {
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(t);
}

/// 一条客户端消息 → 0~N 个 blob,按原顺序。
fn encode_message(m: &Value, out: &mut Vec<Vec<u8>>) {
    let content = m.get("content").unwrap_or(&Value::Null);
    if m.get("role").and_then(Value::as_str) == Some("assistant") {
        if let Some(b) = assistant_blob(content) {
            out.push(b);
        }
        return;
    }
    // user(及路由后残留的其它角色):工具结果各成一条 tool 消息,文字与图片按出现顺序归并。
    let mut pending = String::new();
    let mut images: Vec<Value> = Vec::new();
    let flush = |pending: &mut String, images: &mut Vec<Value>, out: &mut Vec<Vec<u8>>| {
        if !pending.trim().is_empty() || !images.is_empty() {
            out.push(user_blob(pending.trim_end(), images));
        }
        pending.clear();
        images.clear();
    };
    match content {
        Value::String(s) => pending.push_str(s),
        Value::Array(blocks) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str).unwrap_or("") {
                    "tool_result" => {
                        flush(&mut pending, &mut images, out);
                        out.push(tool_result_blob(&ToolResult::from_block(b)));
                    }
                    "image" => match user_image_part(b) {
                        Ok(p) => images.push(p),
                        Err(placeholder) => push_line(&mut pending, placeholder),
                    },
                    _ => {
                        if let Some(t) = user_block_text(b) {
                            push_line(&mut pending, &t);
                        }
                    }
                }
            }
        }
        _ => {}
    }
    flush(&mut pending, &mut images, out);
}

fn user_block_text(b: &Value) -> Option<String> {
    match b.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => b.get("text").and_then(Value::as_str).map(str::to_string),
        "tool_reference" => Some(format!(
            "[引用工具 {}]",
            b.get("tool_name").and_then(Value::as_str).unwrap_or("?")
        )),
        _ => None,
    }
}

fn assistant_blob(content: &Value) -> Option<Vec<u8>> {
    let mut parts: Vec<Value> = Vec::new();
    match content {
        Value::String(s) if !s.trim().is_empty() => parts.push(json!({"type": "text", "text": s})),
        Value::Array(blocks) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" => {
                        // 思考回声已由调用方置空(thinking_echo::strip_from_history),空块跳过即可。
                        let t = b.get("text").and_then(Value::as_str).unwrap_or("");
                        if !t.trim().is_empty() {
                            parts.push(json!({"type": "text", "text": t}));
                        }
                    }
                    "tool_use" => {
                        let id = b.get("id").and_then(Value::as_str).unwrap_or("");
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
                        let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                        parts.push(json!({
                            "type": "tool-call",
                            "toolCallId": id,
                            "toolName": DYNAMIC_TOOL,
                            "args": {
                                "namespace": crate::cli::MCP_PROVIDER_CLAUDE_LOCAL,
                                "toolName": name,
                                "arguments": input
                            }
                        }));
                    }
                    // thinking / redacted_thinking:不回传。
                    _ => {}
                }
            }
        }
        _ => {}
    }
    (!parts.is_empty()).then(|| to_bytes(&json!({"role": "assistant", "content": parts})))
}

struct ToolResult {
    id: String,
    text: String,
    is_error: bool,
    /// 工具结果里的图片 part(服务端实物形态 `{"type":"image","data":<base64>,"mimeType"}`)。
    images: Vec<Value>,
}

impl ToolResult {
    fn from_block(b: &Value) -> Self {
        let content = b.get("content").unwrap_or(&Value::Null);
        let mut text = crate::chat::extract_text(content);
        let mut images = Vec::new();
        for n in content.as_array().into_iter().flatten() {
            if n.get("type").and_then(Value::as_str) == Some("image") {
                match tool_image_part(n) {
                    Ok(p) => images.push(p),
                    Err(placeholder) => push_line(&mut text, placeholder),
                }
            }
        }
        Self {
            id: b.get("tool_use_id").and_then(Value::as_str).unwrap_or("").to_string(),
            text,
            is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            images,
        }
    }
}

/// 末条 user 消息拆出来的三部分。
struct LastPart {
    results: Vec<ToolResult>,
    images: Vec<Value>,
    text: String,
}

fn split_last_user(content: &Value) -> LastPart {
    let mut p = LastPart {
        results: Vec::new(),
        images: Vec::new(),
        text: String::new(),
    };
    match content {
        Value::String(s) => p.text.push_str(s),
        Value::Array(blocks) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str).unwrap_or("") {
                    "tool_result" => p.results.push(ToolResult::from_block(b)),
                    "image" => match user_image_part(b) {
                        Ok(img) => p.images.push(img),
                        Err(placeholder) => push_line(&mut p.text, placeholder),
                    },
                    _ => {
                        if let Some(t) = user_block_text(b) {
                            push_line(&mut p.text, &t);
                        }
                    }
                }
            }
        }
        _ => {}
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protobuf::{Reader, Value as Pb};

    fn blobs_in_order(r: &Rebuilt) -> Vec<Value> {
        Reader::new(&r.css)
            .map(|(f, v)| {
                assert_eq!(f, CSS_ROOT_PROMPT);
                let Pb::Len(h) = v else { panic!("`.1` 应是 bytes") };
                let h: [u8; 32] = h.try_into().unwrap();
                serde_json::from_slice(&r.blobs[&h]).unwrap()
            })
            .collect()
    }

    fn body() -> Value {
        json!({"messages": [
            {"role": "user", "content": "读一下 a.txt"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "想一想", "signature": ""},
                {"type": "text", "text": "我先读文件。"},
                {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"file_path": "a.txt"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "苹果"}]}
            ]}
        ]})
    }

    #[test]
    fn rebuilds_history_in_server_blob_shapes() {
        let r = rebuild(&body(), Some("继续".into())).unwrap();
        assert_eq!(r.current, "继续", "末条只有工具结果时用调用方给的提醒");
        let b = blobs_in_order(&r);
        assert_eq!(b.len(), 3);
        assert_eq!(b[0], json!({"role":"user","content":[{"type":"text","text":"<user_query>\n读一下 a.txt\n</user_query>"}]}));
        // assistant:思考不回传,正文 + CallDynamicTool 工具调用。
        assert_eq!(b[1]["role"], "assistant");
        assert_eq!(b[1]["content"].as_array().unwrap().len(), 2);
        assert_eq!(b[1]["content"][0], json!({"type":"text","text":"我先读文件。"}));
        let call = &b[1]["content"][1];
        assert_eq!(call["type"], "tool-call");
        assert_eq!(call["toolCallId"], "toolu_1");
        assert_eq!(call["toolName"], "CallDynamicTool");
        assert_eq!(call["args"], json!({"namespace":"claude-local","toolName":"Read","arguments":{"file_path":"a.txt"}}));
        // tool:结果文本两处都放(实物如此)。
        assert_eq!(b[2]["role"], "tool");
        let res = &b[2]["content"][0];
        assert_eq!(res["toolCallId"], "toolu_1");
        assert_eq!(res["result"], "苹果");
        assert_eq!(res["experimental_content"], json!([{"type":"text","text":"苹果"}]));
        assert!(res.get("isError").is_none());
    }

    #[test]
    fn same_history_gives_same_hashes_and_extension_keeps_prefix() {
        let a = rebuild(&body(), Some("继续".into())).unwrap();
        let a2 = rebuild(&body(), Some("继续".into())).unwrap();
        assert_eq!(a.css, a2.css, "同样的历史必须逐字节相同,否则缓存命中不了");
        let mut longer = body();
        let ms = longer["messages"].as_array_mut().unwrap();
        ms.push(json!({"role":"assistant","content":[{"type":"text","text":"是苹果。"}]}));
        ms.push(json!({"role":"user","content":"再读 b.txt"}));
        let b = rebuild(&longer, None).unwrap();
        assert!(b.css.starts_with(&a.css), "追加轮次后前面的引用必须原样保持");
        assert_eq!(b.current, "再读 b.txt");
        assert_eq!(b.messages, 4);
    }

    #[test]
    fn last_message_text_and_results_split() {
        let mut b = body();
        b["messages"][2]["content"].as_array_mut().unwrap().push(json!({"type":"text","text":"顺便看看 b.txt"}));
        b["messages"][2]["content"][0]["is_error"] = json!(true);
        let r = rebuild(&b, Some("不该用".into())).unwrap();
        assert_eq!(r.current, "顺便看看 b.txt");
        let blobs = blobs_in_order(&r);
        assert_eq!(blobs[2]["content"][0]["isError"], json!(true));
    }

    #[test]
    fn refuses_unsupported_media_and_prefill() {
        let doc = json!({"messages":[
            {"role":"user","content":[{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"AA=="}}]},
            {"role":"assistant","content":"看到了"},
            {"role":"user","content":"继续"}]});
        assert!(!eligible(&doc) && rebuild(&doc, None).is_none(), "有文档照旧折叠");
        let url_img = json!({"messages":[
            {"role":"user","content":"q"},
            {"role":"assistant","content":[{"type":"tool_use","id":"t","name":"x","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":[{"type":"image","source":{"type":"url","url":"https://x"}}]}]}]});
        assert!(!eligible(&url_img), "工具结果里的 URL 图片不支持");
        let prefill = json!({"messages":[{"role":"user","content":"q"},{"role":"assistant","content":"答案是"}]});
        assert!(!eligible(&prefill) && rebuild(&prefill, None).is_none());
        let single = json!({"messages":[{"role":"user","content":"q"}]});
        assert!(!eligible(&single) && rebuild(&single, None).is_none(), "没有历史不必重建");
    }

    /// 代码审查:预判通过就必须重建成功(否则被放进 CLI 形态后退回折叠,图片会丢)。
    #[test]
    fn eligible_implies_rebuild_succeeds_even_without_text() {
        let img = json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAEC"}});
        // 末条只有截图、没有文字,也没有调用方提醒。
        let only_img = json!({"messages":[
            {"role":"user","content":"看看这个"},
            {"role":"assistant","content":"好"},
            {"role":"user","content":[img]}]});
        assert!(eligible(&only_img));
        let r = rebuild(&only_img, None).unwrap();
        assert_eq!(r.current, IMAGE_ONLY_PROMPT);
        assert_eq!(blobs_in_order(&r).last().unwrap()["content"][0]["type"], "image");
        // 末条只有工具结果、调用方也没给提醒。
        let r = rebuild(&body(), None).unwrap();
        assert_eq!(r.current, CONTINUE_PROMPT);
    }

    #[test]
    fn images_encoded_in_server_shapes() {
        // "AAEC" = 字节 00 01 02
        let img = json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAEC"}});
        let b = json!({"messages":[
            {"role":"user","content":[img.clone(), {"type":"text","text":"看图"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{"p":"x.png"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"截图"}, img.clone()]}]},
            {"role":"assistant","content":"好"},
            {"role":"user","content":[img.clone(), {"type":"text","text":"这张呢"}]}]});
        assert!(eligible(&b));
        let r = rebuild(&b, None).unwrap();
        assert_eq!(r.current, "这张呢");
        let blobs = blobs_in_order(&r);
        assert_eq!(blobs[0]["content"][0], json!({"type":"image","image":{"__type":"Uint8Array","hex":"000102"},"mimeType":"image/png"}));
        assert_eq!(blobs[0]["content"][1]["text"], "<user_query>\n看图\n</user_query>");
        assert_eq!(blobs[2]["content"][0]["experimental_content"][1], json!({"type":"image","data":"AAEC","mimeType":"image/png"}));
        assert!(last_message_has_images(&b));
        assert!(!last_message_has_images(&body()));
        // 本轮的图片单独成一条历史消息,只有图片。
        let last = blobs.last().unwrap();
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"].as_array().unwrap().len(), 1);
        assert_eq!(last["content"][0]["type"], "image");
    }

    /// 代码审查:base64 带换行 / URL 安全字母表要能解;实在解不开留占位,不静默丢。
    #[test]
    fn lenient_base64_and_placeholder_for_bad_images() {
        assert_eq!(decode_base64_lenient("AA\nEC"), Some(vec![0, 1, 2]));
        assert_eq!(decode_base64_lenient("_-8"), Some(vec![0xff, 0xef]));
        let bad = json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"blob:abc!"}});
        let b = json!({"messages":[
            {"role":"user","content":[bad.clone(), {"type":"text","text":"看图"}]},
            {"role":"assistant","content":"嗯"},
            {"role":"user","content":"然后呢"}]});
        let r = rebuild(&b, None).unwrap();
        let blobs = blobs_in_order(&r);
        assert_eq!(blobs[0]["content"].as_array().unwrap().len(), 1, "解不开的图不留 image part");
        assert!(blobs[0]["content"][0]["text"].as_str().unwrap().contains(BAD_IMAGE_PLACEHOLDER));
    }

    #[test]
    fn hex_table_matches_format() {
        let bytes: Vec<u8> = (0..=255).collect();
        let want: String = bytes.iter().map(|x| format!("{x:02x}")).collect();
        assert_eq!(to_hex(&bytes), want);
    }
}
