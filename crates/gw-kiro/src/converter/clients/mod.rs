//! 按客户端翻译(责任链):先认出是哪家客户端,再按它的习惯把消息整理好,交给通用转换器。
//!
//! # 为什么要分客户端
//!
//! 各家客户端拼 Anthropic 报文的习惯不一样:pi 把系统提示放在 `role=developer`、首条 user
//! 写 `...` 占位;Grok Build 和 pi 都把纯工具回合的正文写成 `...`。以前这些都是在通用转换器
//! 里按内容特征零散加的判断,哪条规则属于谁只能靠注释猜 —— 10-29 发现 pi 的系统提示被静默
//! 丢了很久,就是因为代码里没有「这是哪个客户端」的概念。
//!
//! # 链怎么走
//!
//! 识别(责任链)在 `gw_core::clients`,两条线共用;这里按认出来的客户端挑 Kiro 的整理步骤。
//! 认不出来的是 `ClientKind::Fallback`。
//! **兜底 = 今天线上的整套处理,逐字节不变**:现有规则本身带判据(只认 CC 原句之类),对别家
//! 请求不生效,留着没有坏处;某个 CC 请求万一没被认出来,兜底照样处理对。只有被认出来、并且
//! 配了专用步骤的客户端,输出才会变。
//!
//! # 不动的东西
//!
//! conversationId / 调度亲和在翻译之前就用原始 messages 算好了,翻译只改发往上游的那份消息,
//! 会话不会因为翻译换 ID、换号。

use crate::anthropic_types::{Message, MessagesRequest};
use gw_core::clients::{recognize, ClientSignals};

pub(super) mod steps;

pub use gw_core::clients::ClientKind;

/// 翻译结果:整理后的消息 + 提升进 history[0] 的系统文本。
pub(super) struct Translation {
    pub kind: ClientKind,
    /// `None` = 原样用调用方的 messages(常见情形零拷贝)。
    pub messages: Option<Vec<Message>>,
    pub promoted_system: Vec<String>,
}

impl Translation {
    /// 原有整套整理(10-29 前线上的行为,兜底与无专用步骤的客户端都用它)。
    ///
    /// ⚠️ 这里是**冻结的旧路径**:往 `steps::route_system_role_messages` 里加任何「通用」清理,
    /// 都会同时改变兜底、Claude Code、OpenCode、DeepSeek Harness、Copilot 的上游字节。新的整理
    /// 一律做成独立步骤,只挂在认出来的客户端上。
    fn legacy(kind: ClientKind, messages: &[Message]) -> Self {
        match steps::route_system_role_messages(messages) {
            Some(routed) => Self {
                kind,
                messages: Some(routed.messages),
                promoted_system: routed.promoted_system,
            },
            None => Self { kind, messages: None, promoted_system: Vec::new() },
        }
    }

    /// 专用步骤要改消息时拿一份可改的副本(还没复制过才复制)。
    fn messages_mut(&mut self, original: &[Message]) -> &mut Vec<Message> {
        self.messages.get_or_insert_with(|| original.to_vec())
    }
}

/// 认出客户端(判据在 `gw_core::clients`,两条线共用),再按 Kiro 的需要整理。
///
/// 判 developer 用的是调用方传进来的 `messages`(已截掉末尾 prefill),与改造前逐条一致。
pub(super) fn translate(req: &MessagesRequest, messages: &[Message]) -> Translation {
    let signals = ClientSignals {
        system_text: system_text(req),
        tool_names: req.tools.iter().flatten().map(|t| t.name.as_str()).collect(),
        developer_before_first_assistant: messages
            .iter()
            .take_while(|m| m.role != "assistant")
            .any(|m| m.role == "developer"),
    };
    let kind = recognize(&signals);
    let mut t = Translation::legacy(kind, messages);
    match kind {
        // pi:兜底步骤已经把首个 assistant 之前的 developer 提升进 history[0];
        // 再去掉开头的 `...` 占位 user 和工具回合里的 `...` 文本。
        ClientKind::DeveloperRole => {
            let msgs = t.messages_mut(messages);
            steps::drop_leading_placeholder_user(msgs);
            steps::drop_assistant_placeholder_text(msgs);
        }
        // Grok Build:纯工具回合的 assistant 正文 84% 是 `...`。
        ClientKind::GrokBuild => {
            steps::drop_assistant_placeholder_text(t.messages_mut(messages));
        }
        // Claude Code / OpenCode / DeepSeek Harness / Copilot / 兜底:今天的整套处理就是照着
        // CC 调的,别家目前没有已知怪癖,只认出来分口径统计。
        _ => {}
    }
    t
}

/// top-level system 的全部文本(多块用换行拼)。没有 system 返回空串。
fn system_text(req: &MessagesRequest) -> String {
    req.system
        .as_ref()
        .map(|blocks| blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
