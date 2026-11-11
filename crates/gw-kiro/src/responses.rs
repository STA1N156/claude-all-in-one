//! Kiro `conversationState` → `/v1/responses`(CreateResponse)请求体。
//!
//! 只在账号 `extra.kiro_endpoint=responses` 时使用(见 [`crate::endpoint`])。
//!
//! 形状逐条对齐 Kiro IDE 1.4.121 的转换函数(extension.js,2026-11-08 解包;
//! 第二份独立核对见 `_notes/kiro-1.4.121/review-k31/FINDINGS.md`):
//!
//! | 本文件 | 客户端 | 内容 |
//! |---|---|---|
//! | [`CreateResponseRequest::from`] | `PUs`@5709506 | 顶层键序 `model,input,(instructions),tools,stream,origin,profileArn,metadata,reasoning` |
//! | [`message_items`] | `PEr` / `xUs` | 历史 + 当前轮逐条铺平 |
//! | [`reasoning_item`] | `kUs` / `REr`@5706213 | `encrypted_content` 是 JSON 字符串信封 |
//! | `tools` | `RUs` | `{type:"function",name,description,parameters}`,无名工具跳过 |
//! | [`response_reasoning`] | `OUs` + `Smr/bmr/Emr` | `reasoning:{type,effort,summary}`,缺省字段省略 |
//!
//! 刻意**不**做的:
//! - 不写 `instructions`:只有服务端实验 `system_field_injection` 打开时客户端才把首条
//!   system 抽到这里;默认(抓包所见)是 system 作为 user + "I will follow these instructions."
//!   铺在 input 里 —— caio 的 converter 本来就是这个形状,原样保留。
//! - 工具结果不带错误标记:客户端 `PEr` 根本不读 `status`(Responses 里没有对应字段)。

use serde::Serialize;

use crate::kiro_types::conversation::{
    AssistantMessage, KiroDocument, KiroImage, Message, ReasoningContent, UserInputMessage,
    UserMessage,
};
use crate::kiro_types::request::{KiroRequest, DEFAULT_AGENT_MODE};
use crate::kiro_types::tool::{ToolResult, ToolUseEntry};

/// 1.4.121 的会话 ID 带 `sess_` 前缀(抓包 `kiro:conversation_id: sess_<uuid>`)。
const SESSION_PREFIX: &str = "sess_";
/// IDE 的 origin(CLI 是 `KIRO_CLI`;我们的号都是 IDE 登录签发的,只模拟 IDE)。
pub(crate) const IDE_ORIGIN: &str = "AI_EDITOR";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateResponseRequest {
    model: String,
    input: Vec<ResponseInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ResponseTool>,
    stream: bool,
    origin: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile_arn: Option<String>,
    metadata: ResponseMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponseReasoning>,
}

#[derive(Debug, Serialize)]
struct ResponseMetadata {
    #[serde(rename = "kiro:agent_mode")]
    agent_mode: String,
    #[serde(rename = "kiro:mode_identifier")]
    mode_identifier: String,
    #[serde(rename = "kiro:agent_task_type", skip_serializing_if = "Option::is_none")]
    agent_task_type: Option<String>,
    #[serde(rename = "kiro:conversation_id")]
    conversation_id: String,
    #[serde(rename = "kiro:root_conversation_id")]
    root_conversation_id: String,
    #[serde(rename = "kiro:agent_continuation_id", skip_serializing_if = "Option::is_none")]
    agent_continuation_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResponseReasoning {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    thinking_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResponseTool {
    #[serde(rename = "type")]
    tool_type: &'static str,
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ResponseInput {
    Message {
        role: &'static str,
        content: MessageContent,
    },
    Reasoning {
        #[serde(rename = "type")]
        item_type: &'static str,
        summary: [(); 0],
        encrypted_content: String,
    },
    FunctionCallOutput {
        #[serde(rename = "type")]
        item_type: &'static str,
        call_id: String,
        output: String,
    },
    FunctionCall {
        #[serde(rename = "type")]
        item_type: &'static str,
        call_id: String,
        name: String,
        arguments: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ContentPart {
    #[serde(rename = "input_text")]
    Text { text: String },
    #[serde(rename = "input_image")]
    Image { image_url: String },
    #[serde(rename = "input_file")]
    File { filename: String, file_data: String },
}

/// 序列化成 `/v1/responses` 请求体。
pub fn request_body(request: &KiroRequest) -> Result<String, serde_json::Error> {
    serde_json::to_string(&CreateResponseRequest::from(request))
}

/// 每个用户轮次一个新的 `agent_continuation_id`(客户端 `_I()` = randomUUID,处理每条新用户消息时
/// 重置);同一次请求内的重试复用(调用方在第一次序列化前调用一次即可,已有则不动)。
///
/// 注意:converter 在 `agent_continuation` 开关打开时会填一个**稳定**派生值,那是给旧协议的
/// A/B;走 /v1/responses 时以客户端行为为准,所以这里不看已有值是谁写的 —— 只有调用方
/// 在同一请求内重复调用时才保留。
pub fn fresh_continuation_id(request: &mut KiroRequest) {
    request.conversation_state.agent_continuation_id = Some(uuid::Uuid::new_v4().to_string());
}

/// `sess_` + 标准 v4 UUID(客户端是 `randomUUID()`)。
///
/// caio 派生的 conversationId 是「sha256 前 16 字节排成 UUID」,版本位/变体位是随机的
/// (如 `…-ee50-…`),真客户端不会发这种值。这里只改那 6 个位(`Builder::from_random_bytes`
/// 按 RFC 4122 写版本 4 + 变体),其余 122 位原样保留:同一会话仍恒等映射到同一个 ID,
/// 上游缓存不受影响。只作用于新协议(旧协议的号保持原值,避免在跑的会话换 ID 丢缓存)。
fn session_id(raw: &str) -> String {
    let bare = raw.strip_prefix(SESSION_PREFIX).unwrap_or(raw);
    match uuid::Uuid::parse_str(bare) {
        Ok(parsed) => {
            let v4 = uuid::Builder::from_random_bytes(*parsed.as_bytes()).into_uuid();
            format!("{SESSION_PREFIX}{}", v4.hyphenated())
        }
        // 不是 UUID 形态(理论上不会出现):不改内容,只保证前缀。
        Err(_) => format!("{SESSION_PREFIX}{bare}"),
    }
}

impl From<&KiroRequest> for CreateResponseRequest {
    fn from(request: &KiroRequest) -> Self {
        let state = &request.conversation_state;
        let current = &state.current_message.user_input_message;
        let mut input = Vec::new();
        for message in &state.history {
            match message {
                Message::User(user) => input.extend(user_items(&user.user_input_message)),
                Message::Assistant(assistant) => {
                    input.extend(assistant_items(&assistant.assistant_response_message))
                }
            }
        }
        input.extend(current_items(current));
        // 抓包:主对话 `kiro:agent_mode` 与 `kiro:mode_identifier` 都是 "vibe"。
        let agent_mode = request
            .agent_mode
            .clone()
            .unwrap_or_else(|| DEFAULT_AGENT_MODE.to_string());
        // 主执行是 self-rooted:root 缺省时等于 conversationId(客户端序列化兜底逻辑)。
        let conversation_id = session_id(&state.conversation_id);
        Self {
            model: current.model_id.clone(),
            input,
            tools: current
                .user_input_message_context
                .tools
                .iter()
                .filter(|tool| !tool.tool_specification.name.is_empty())
                .map(|tool| ResponseTool {
                    tool_type: "function",
                    name: tool.tool_specification.name.clone(),
                    description: tool.tool_specification.description.clone(),
                    parameters: tool.tool_specification.input_schema.json.clone(),
                })
                .collect(),
            stream: true,
            origin: IDE_ORIGIN,
            profile_arn: request.profile_arn.clone(),
            metadata: ResponseMetadata {
                mode_identifier: agent_mode.clone(),
                agent_mode,
                agent_task_type: state
                    .agent_task_type
                    .as_ref()
                    .filter(|kind| kind.as_str() == "spec")
                    .cloned(),
                root_conversation_id: conversation_id.clone(),
                conversation_id,
                agent_continuation_id: state.agent_continuation_id.clone(),
            },
            reasoning: response_reasoning(request.additional_model_request_fields.as_ref()),
        }
    }
}

/// `OUs`:从 additionalModelRequestFields 取 `thinking.type` / `output_config.effort`(或
/// `reasoning.effort`)/ `thinking.display`,三者全无则整个字段省略(不是 null)。
fn response_reasoning(fields: Option<&serde_json::Value>) -> Option<ResponseReasoning> {
    let fields = fields?;
    let non_empty = |v: Option<&serde_json::Value>| {
        v.and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let thinking = fields.get("thinking").filter(|v| v.is_object());
    let thinking_type = non_empty(thinking.and_then(|t| t.get("type")));
    let summary = non_empty(thinking.and_then(|t| t.get("display")));
    let effort = ["output_config", "reasoning"]
        .into_iter()
        .find_map(|key| non_empty(fields.get(key).and_then(|v| v.get("effort"))));
    if thinking_type.is_none() && effort.is_none() && summary.is_none() {
        return None;
    }
    Some(ResponseReasoning {
        thinking_type,
        effort,
        summary,
    })
}

fn current_items(message: &UserInputMessage) -> Vec<ResponseInput> {
    message_items(
        &message.content,
        &message.images,
        &message.documents,
        &message.user_input_message_context.tool_results,
    )
}

fn user_items(message: &UserMessage) -> Vec<ResponseInput> {
    message_items(
        &message.content,
        &message.images,
        &message.documents,
        &message.user_input_message_context.tool_results,
    )
}

/// `PEr` 的 user 分支:先每个工具结果一条 `function_call_output`(text 块用单个换行拼接),
/// 再是 user 消息 —— 有附件时 content 是数组(可选 input_text 在前、图片、文档),
/// 只有文本时 content 是字符串,空文本且无附件则不出这一条。
fn message_items(
    content: &str,
    images: &[KiroImage],
    documents: &[KiroDocument],
    tool_results: &[ToolResult],
) -> Vec<ResponseInput> {
    let mut out = Vec::new();
    for result in tool_results {
        let output = result
            .content
            .iter()
            .map(|part| part.get("text").and_then(serde_json::Value::as_str).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        out.push(ResponseInput::FunctionCallOutput {
            item_type: "function_call_output",
            call_id: result.tool_use_id.clone(),
            output,
        });
    }
    let mut attachments = Vec::new();
    for image in images.iter().filter(|image| !image.source.bytes.is_empty()) {
        attachments.push(ContentPart::Image {
            image_url: format!(
                "data:image/{};base64,{}",
                image.format.to_ascii_lowercase(),
                image.source.bytes
            ),
        });
    }
    for document in documents.iter().filter(|doc| !doc.source.bytes.is_empty()) {
        let format = document.format.to_ascii_lowercase();
        attachments.push(ContentPart::File {
            filename: format!("{}.{}", document.name, format),
            file_data: format!("data:{};base64,{}", document_mime(&format), document.source.bytes),
        });
    }
    if attachments.is_empty() {
        if !content.is_empty() {
            out.push(ResponseInput::Message {
                role: "user",
                content: MessageContent::Text(content.to_string()),
            });
        }
    } else {
        let mut parts = Vec::with_capacity(attachments.len() + 1);
        if !content.is_empty() {
            parts.push(ContentPart::Text {
                text: content.to_string(),
            });
        }
        parts.extend(attachments);
        out.push(ResponseInput::Message {
            role: "user",
            content: MessageContent::Parts(parts),
        });
    }
    out
}

/// 客户端 MIME 表(`z1p`);表外扩展名用 `application/octet-stream`,不丢弃。
fn document_mime(format: &str) -> &'static str {
    match format {
        "pdf" => "application/pdf",
        "csv" => "text/csv",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "html" => "text/html",
        "txt" => "text/plain",
        "md" => "text/markdown",
        _ => "application/octet-stream",
    }
}

/// `PEr` 的 assistant 分支:reasoning → 正文(非空才出)→ 每个 toolUse 一条 function_call。
fn assistant_items(message: &AssistantMessage) -> Vec<ResponseInput> {
    let mut out = Vec::new();
    if let Some(item) = message.reasoning_content.as_ref().and_then(reasoning_item) {
        out.push(item);
    }
    if !message.content.is_empty() {
        out.push(ResponseInput::Message {
            role: "assistant",
            content: MessageContent::Text(message.content.clone()),
        });
    }
    for tool in message.tool_uses.as_deref().unwrap_or(&[]) {
        out.push(function_call(tool));
    }
    out
}

/// `kUs` + `REr`:`encrypted_content` 是 JSON **字符串**:
/// - 签名思考 → `{"reasoningText":{"text":…,"signature":…}}`;客户端只回放 text 与 signature
///   都非空的思考(`K$s`),缺一个就整条不发;
/// - redacted → `{"redactedContent":"<base64>"}`(caio 存的就是 base64 原文)。
fn reasoning_item(reasoning: &ReasoningContent) -> Option<ResponseInput> {
    let encrypted_content = match reasoning {
        ReasoningContent::ReasoningText { reasoning_text } => {
            if reasoning_text.text.is_empty() || reasoning_text.signature.is_empty() {
                return None;
            }
            serde_json::json!({
                "reasoningText": {
                    "text": reasoning_text.text,
                    "signature": reasoning_text.signature,
                }
            })
            .to_string()
        }
        ReasoningContent::Redacted { redacted_content } => {
            if redacted_content.is_empty() {
                return None;
            }
            serde_json::json!({ "redactedContent": redacted_content }).to_string()
        }
    };
    Some(ResponseInput::Reasoning {
        item_type: "reasoning",
        summary: [],
        encrypted_content,
    })
}

/// `arguments`:字符串原样,其余 `JSON.stringify(input ?? {})`。
fn function_call(tool: &ToolUseEntry) -> ResponseInput {
    let arguments = match &tool.input {
        serde_json::Value::String(raw) => raw.clone(),
        serde_json::Value::Null => "{}".to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
    };
    ResponseInput::FunctionCall {
        item_type: "function_call",
        call_id: tool.tool_use_id.clone(),
        name: tool.name.clone(),
        arguments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro_types::conversation::{
        ConversationState, CurrentMessage, HistoryAssistantMessage, HistoryUserMessage,
        ReasoningText, UserInputMessageContext,
    };
    use crate::kiro_types::tool::{InputSchema, Tool, ToolSpecification};

    fn tool(name: &str) -> Tool {
        Tool {
            tool_specification: ToolSpecification {
                name: name.into(),
                description: "Read a file".into(),
                input_schema: InputSchema::from_json(serde_json::json!({"type":"object"})),
            },
        }
    }

    fn state() -> ConversationState {
        let mut history_user = UserMessage::new("old question", "claude-opus-5.5");
        let mut two_part = ToolResult::success("tu0", "line1");
        two_part
            .content
            .push(serde_json::json!({"text": "line2"}).as_object().unwrap().clone());
        history_user.user_input_message_context.tool_results = vec![two_part];
        history_user.images.push(KiroImage::from_base64("PNG", "aW1n"));
        history_user
            .documents
            .push(KiroDocument::from_base64("notes", "md", "ZG9j"));
        let assistant = AssistantMessage::new("old answer")
            .with_tool_uses(vec![ToolUseEntry::new("tu1", "read_file")
                .with_input(serde_json::json!({"path":"a.rs"}))])
            .with_reasoning_content(ReasoningContent::ReasoningText {
                reasoning_text: ReasoningText {
                    text: "thinking".into(),
                    signature: "sig".into(),
                },
            });
        let current = UserInputMessage::new("new question", "claude-opus-5.5")
            .with_context(UserInputMessageContext::new().with_tools(vec![tool("read_file"), tool("")]));
        ConversationState::new("2f1c0b8e-0000-4000-8000-000000000001")
            .with_current_message(CurrentMessage::new(current))
            .with_history(vec![
                Message::User(HistoryUserMessage {
                    user_input_message: history_user,
                }),
                Message::Assistant(HistoryAssistantMessage {
                    assistant_response_message: assistant,
                }),
            ])
    }

    fn request(state: ConversationState, amrf: Option<serde_json::Value>) -> KiroRequest {
        KiroRequest {
            conversation_state: state,
            profile_arn: Some("arn:test".into()),
            additional_model_request_fields: amrf,
            agent_mode: Some("vibe".into()),
        }
    }

    #[test]
    fn complete_request_matches_ide_1_4_121_shapes() {
        let mut req = request(
            state(),
            Some(serde_json::json!({"output_config": {"effort": "medium"}})),
        );
        fresh_continuation_id(&mut req);
        let body = request_body(&req).unwrap();
        // 顶层键序与 1.4.121 抓包一致。
        assert!(body.starts_with("{\"model\":\"claude-opus-5.5\",\"input\":"), "{body}");
        // serde_json::Value 会把键排序,键序只能在原始字符串上看(顶层键第一次出现的位置)。
        let positions: Vec<usize> = [
            "\"model\":", "\"input\":", "\"tools\":", "\"stream\":", "\"origin\":",
            "\"profileArn\":", "\"metadata\":", "\"reasoning\":",
        ]
        .iter()
        .map(|k| body.find(k).unwrap_or_else(|| panic!("缺 {k}")))
        .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "顶层键序不对: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["origin"], "AI_EDITOR");
        assert_eq!(v["stream"], true);
        assert!(v.get("instructions").is_none());
        // 抓包里 opus-5.5 的 reasoning 只有 effort。
        assert_eq!(v["reasoning"], serde_json::json!({"effort": "medium"}));
        let md = &v["metadata"];
        assert_eq!(md["kiro:agent_mode"], "vibe");
        assert_eq!(md["kiro:mode_identifier"], "vibe");
        assert_eq!(md["kiro:conversation_id"], "sess_2f1c0b8e-0000-4000-8000-000000000001");
        assert_eq!(md["kiro:root_conversation_id"], md["kiro:conversation_id"]);
        assert!(md["kiro:agent_continuation_id"].as_str().unwrap().len() == 36);
        assert!(md.get("kiro:agent_task_type").is_none());
        // 无名工具被跳过。
        assert_eq!(v["tools"].as_array().unwrap().len(), 1);
        assert_eq!(v["tools"][0]["type"], "function");
        let input = v["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["output"], "line1\nline2", "多个 text 块用单个换行拼接");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[1]["content"][0], serde_json::json!({"type":"input_text","text":"old question"}));
        assert_eq!(input[1]["content"][1]["image_url"], "data:image/png;base64,aW1n");
        assert_eq!(input[1]["content"][2]["filename"], "notes.md");
        assert_eq!(input[1]["content"][2]["file_data"], "data:text/markdown;base64,ZG9j");
        assert_eq!(input[2]["type"], "reasoning");
        assert_eq!(input[2]["summary"], serde_json::json!([]));
        let envelope: serde_json::Value =
            serde_json::from_str(input[2]["encrypted_content"].as_str().unwrap()).unwrap();
        assert_eq!(
            envelope,
            serde_json::json!({"reasoningText": {"text": "thinking", "signature": "sig"}})
        );
        assert_eq!(input[3], serde_json::json!({"role":"assistant","content":"old answer"}));
        assert_eq!(input[4]["type"], "function_call");
        assert_eq!(input[4]["arguments"], "{\"path\":\"a.rs\"}");
        assert_eq!(input[5], serde_json::json!({"role":"user","content":"new question"}));
        assert_eq!(input.len(), 6);
    }

    #[test]
    fn unsigned_or_empty_reasoning_is_not_replayed() {
        for (text, sig) in [("t", ""), ("", "s")] {
            let r = ReasoningContent::ReasoningText {
                reasoning_text: ReasoningText {
                    text: text.into(),
                    signature: sig.into(),
                },
            };
            assert!(reasoning_item(&r).is_none(), "text={text:?} sig={sig:?}");
        }
        let red = ReasoningContent::Redacted {
            redacted_content: "QUJD".into(),
        };
        let ResponseInput::Reasoning { encrypted_content, .. } = reasoning_item(&red).unwrap() else {
            panic!("redacted 应生成 reasoning item");
        };
        assert_eq!(encrypted_content, r#"{"redactedContent":"QUJD"}"#);
    }

    #[test]
    fn missing_reasoning_fields_are_omitted_and_full_set_maps_through() {
        let bare = || {
            ConversationState::new("c").with_current_message(CurrentMessage::new(
                UserInputMessage::new("hi", "claude-sonnet-5.5"),
            ))
        };
        let v: serde_json::Value = serde_json::from_str(
            &request_body(&KiroRequest {
                conversation_state: bare(),
                profile_arn: None,
                additional_model_request_fields: None,
                agent_mode: None,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(v.get("reasoning").is_none());
        assert!(v.get("profileArn").is_none());
        assert!(v.get("tools").is_none());
        assert_eq!(v["metadata"]["kiro:agent_mode"], "vibe");
        assert!(v["metadata"].get("kiro:agent_continuation_id").is_none());
        let full = response_reasoning(Some(&serde_json::json!({
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": "high"}
        })))
        .unwrap();
        assert_eq!(full.thinking_type.as_deref(), Some("adaptive"));
        assert_eq!(full.effort.as_deref(), Some("high"));
        assert_eq!(full.summary.as_deref(), Some("summarized"));
        // 空串当缺省(客户端 `n.length>0` 判定)。
        assert!(response_reasoning(Some(&serde_json::json!({"output_config": {"effort": ""}}))).is_none());
    }

    #[test]
    fn continuation_is_fresh_per_turn() {
        let mut a = request(state(), None);
        let mut b = request(state(), None);
        fresh_continuation_id(&mut a);
        fresh_continuation_id(&mut b);
        assert_ne!(
            a.conversation_state.agent_continuation_id,
            b.conversation_state.agent_continuation_id
        );
        // 已带 sess_ 前缀的不再重复加。
        assert_eq!(session_id("sess_x"), "sess_x");
    }

    #[test]
    fn session_id_is_stable_v4_uuid() {
        // caio 派生值的版本位/变体位是随机的(真实样例 `…-ee50-…`)。
        let raw = "03c28f0c-f525-ee50-1234-56789abcdef0";
        let id = session_id(raw);
        let parsed = uuid::Uuid::parse_str(id.strip_prefix("sess_").unwrap()).unwrap();
        assert_eq!(parsed.get_version_num(), 4);
        assert_eq!(parsed.get_variant(), uuid::Variant::RFC4122);
        // 只动版本/变体位:其余字节不变,同一输入恒等(缓存不丢)。
        assert_eq!(id, "sess_03c28f0c-f525-4e50-9234-56789abcdef0");
        assert_eq!(session_id(raw), id);
        assert_eq!(session_id(&id), id);
        // 不同会话仍不同。
        assert_ne!(session_id("03c28f0c-f525-ee50-1234-56789abcdef1"), id);
    }

    #[test]
    fn spec_task_type_is_the_only_one_forwarded() {
        let mut s = state();
        s.agent_task_type = Some("vibe".into());
        let v: serde_json::Value =
            serde_json::from_str(&request_body(&request(s, None)).unwrap()).unwrap();
        assert!(v["metadata"].get("kiro:agent_task_type").is_none());
        let mut s = state();
        s.agent_task_type = Some("spec".into());
        let v: serde_json::Value =
            serde_json::from_str(&request_body(&request(s, None)).unwrap()).unwrap();
        assert_eq!(v["metadata"]["kiro:agent_task_type"], "spec");
    }
}
