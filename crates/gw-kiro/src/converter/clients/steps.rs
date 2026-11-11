//! 各客户端翻译器共用的整理步骤(纯函数)。翻译器按自己的需要挑着用,兜底用全套。
//!
//! 这里只改发往上游的那份消息,不碰身份哈希的输入(conversationId / 调度亲和都在翻译之前
//! 用原始 messages 算好了)。

use crate::anthropic_types::Message;

use super::super::normalize::{
    strip_rolling_fingerprints, CLAUDE_AGENT_SDK_IDENTITY_LINE, CLAUDE_CODE_CLI_IDENTITY_LINE,
};

// === messages 数组里 role=="system" 消息的三级分流 ===

/// 把 Anthropic message 的 content(string 或 text-block 数组)拼成纯文本。
/// 非文本块忽略(system 消息一般纯文本)。
fn message_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                if let Some(t) = item.get("text").and_then(serde_json::Value::as_str) {
                    parts.push(t.to_string());
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// 稳定 system 前缀:SessionStart hook 注入 或 含 CC/Agent-SDK 身份行。
/// 这类内容跨轮稳定,提升进 top-level system(折叠 history[0],利于 prefix cache)。
fn is_stable_system_prefix(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("SessionStart hook additional context:")
        || text.lines().map(str::trim_start).any(|l| {
            l == CLAUDE_CODE_CLI_IDENTITY_LINE || l == CLAUDE_AGENT_SDK_IDENTITY_LINE
        })
}

/// 已知动态噪声:每轮变化、对模型无信息增量,丢弃以稳住缓存前缀。
fn is_dynamic_system_noise(text: &str) -> bool {
    text.trim_start()
        .starts_with("The task tools haven't been used recently.")
}

/// interrupted-user 特例:`The user sent a new message while you were working:` 开头的
/// system 消息,实为用户中途插话被框成 system。提取正文(遇 `\n\nIMPORTANT:` 截断)转 user。
fn interrupted_user_payload(text: &str) -> Option<String> {
    let body = text
        .trim_start()
        .strip_prefix("The user sent a new message while you were working:")?;
    let payload = body
        .split_once("\n\nIMPORTANT:")
        .map_or(body, |(p, _)| p)
        .trim();
    (!payload.is_empty()).then(|| payload.to_string())
}

/// 按 system 处理的角色:system,以及 OpenAI 系客户端的 developer(OpenAI 入口
/// `gw_core::openai` 同样把 developer 当 system)。
fn is_system_like(role: &str) -> bool {
    role == "system" || role == "developer"
}

/// 三级分流的产物(仅当确实存在 role=system / developer 消息时返回 Some)。
pub(in crate::converter) struct RoutedMessages {
    /// 路由后的 user/assistant 消息序列(role=system 已被剔除或转 user)。
    pub messages: Vec<Message>,
    /// 提升到 top-level 的稳定 system 文本(追加到 req.system 之后再折叠 history[0])。
    pub promoted_system: Vec<String>,
}

/// 处理 messages 数组里的 `role=="system"` 消息(代理链中段注入),三级分流:
/// - **StablePrefix** → 提升进 `promoted_system`(后续折叠进 history[0] 稳定区)
/// - **DynamicNoise** → 丢弃
/// - **interrupted-user** → 提取正文转 `role="user"` 按序保留
/// - **其余未知** → 包 `<system_context>...</system_context>` 转 `role="user"` 按序保留(保位置语义)
/// - 空 system → 丢弃
///
/// `role=="developer"`(OpenAI 系客户端放系统提示的角色)与 system 同口径;另外**第一条
/// assistant 之前**的 developer 一律提升(它就是客户端的系统提示)。2026-10-29 前这里只认
/// system,下游 history 拼装又只认 user/assistant,developer 被静默丢弃 —— pi 等客户端
/// 的请求到上游时一个字的 agent 提示都没有。
///
/// user/assistant 消息原样保留。**无 system/developer 消息时返回 None**(常见情形零拷贝,
/// 调用方继续用原 borrowed slice)。
/// 🟢 借鉴 static_flow `normalize.rs:212-356`,但只做 system-role 这一层(我方 top-level system
/// 折叠逻辑不变,不引入 static_flow 的 developer-role / 整条 normalize 管线)。
pub(in crate::converter) fn route_system_role_messages(messages: &[Message]) -> Option<RoutedMessages> {
    if !messages.iter().any(|m| is_system_like(&m.role)) {
        return None;
    }

    let mut out = Vec::with_capacity(messages.len());
    let mut promoted = Vec::new();
    let mut seen_assistant = false;
    for msg in messages {
        if msg.role == "assistant" {
            seen_assistant = true;
        }
        if !is_system_like(&msg.role) {
            out.push(msg.clone());
            continue;
        }
        let text = strip_rolling_fingerprints(&message_text(&msg.content));
        if text.trim().is_empty() {
            continue; // 空 system 丢弃
        }
        // 第一条 assistant 之前的 developer = 客户端自己的系统提示(pi 等
        // OpenAI 系客户端不用 top-level system,首条 user 是 `...` 占位、提示放这里),
        // 跨轮稳定,提升进 history[0]。
        if is_stable_system_prefix(&text) || (msg.role == "developer" && !seen_assistant) {
            promoted.push(text);
        } else if let Some(payload) = interrupted_user_payload(&text) {
            out.push(Message {
                role: "user".to_string(),
                content: serde_json::Value::String(payload),
            });
        } else if is_dynamic_system_noise(&text) {
            // 丢弃
        } else {
            out.push(Message {
                role: "user".to_string(),
                content: serde_json::Value::String(format!(
                    "<system_context>\n{text}\n</system_context>"
                )),
            });
        }
    }

    Some(RoutedMessages {
        messages: out,
        promoted_system: promoted,
    })
}

/// 客户端写的「这里本该有内容」的占位正文(pi / Grok Build 用 `...`)。
fn is_placeholder_text(text: &str) -> bool {
    matches!(text.trim(), "..." | "…")
}

/// content 只有占位文本、没有任何别的块(图片 / tool_result 等)。
fn is_placeholder_only(content: &serde_json::Value) -> bool {
    let only_text = match content {
        serde_json::Value::String(_) => true,
        serde_json::Value::Array(items) => items
            .iter()
            .all(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("text")),
        _ => false,
    };
    only_text && is_placeholder_text(&message_text(content))
}

/// 去掉 developer 提升之后留在最前面的 `...` 占位 user。
///
/// pi 的首条 user 只是占位,为了让 developer 能排在「第一条 user 之后」。developer 提升走以后,
/// 占位与真正的首句成了相邻两条 user,下游会把它们合并成 "...\n首句"。只在后面紧跟另一条
/// user 时去,否则会把请求删空。
pub(super) fn drop_leading_placeholder_user(messages: &mut Vec<Message>) {
    if messages.len() > 1
        && messages[0].role == "user"
        && messages[1].role == "user"
        && is_placeholder_only(&messages[0].content)
    {
        messages.remove(0);
    }
}

/// assistant「`...` 文本 + tool_use」里的 `...` 文本块删掉,按纯工具回合发。
///
/// 原样上传的话,模型看着自己几百次说 `...`,回复也跟着以 `...` 开头(10-29 线上 pi 63/311)。
/// 删掉以后下游按纯工具回合兜底(EMPTY_CONTENT_PLACEHOLDER),与其它客户端同类回合字节一致。
/// 不带 tool_use 的 `...` 是那一回合唯一的内容,不动。返回是否改过。
pub(super) fn drop_assistant_placeholder_text(messages: &mut [Message]) -> bool {
    let mut changed = false;
    for msg in messages.iter_mut().filter(|m| m.role == "assistant") {
        let serde_json::Value::Array(blocks) = &mut msg.content else {
            continue;
        };
        let has_tool_use = blocks
            .iter()
            .any(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("tool_use"));
        if !has_tool_use {
            continue;
        }
        let before = blocks.len();
        blocks.retain(|b| {
            let is_text = b.get("type").and_then(serde_json::Value::as_str) == Some("text");
            let text = b.get("text").and_then(serde_json::Value::as_str).unwrap_or("");
            !(is_text && is_placeholder_text(text))
        });
        changed |= blocks.len() != before;
    }
    changed
}
