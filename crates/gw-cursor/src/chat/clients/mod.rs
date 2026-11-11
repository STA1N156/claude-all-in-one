//! 按客户端翻译(责任链):先认出是哪家客户端,再把它的报文整理成标准 Anthropic 形态,
//! 交给后面的 cursor 编码(`to_turns` / 自造历史 `blobhist`)。
//!
//! # 为什么 cursor 线也要有
//!
//! 各家客户端(以及它们前面的中转)拼报文的习惯不一样。Kiro 线 10-29 起就有这条链
//! (`gw-kiro::converter::clients`),cursor 线一直只借用了识别那一半(护栏文案取客户端名),
//! 消息本身原样编码。2026-11-10 12:45 一个 Claude Code 会话把并行读图的结果拆成多条、
//! 图片排在 tool_result 前,原样编码后上游提供方回 400,又被误判成额度耗尽,同一个请求
//! 30 秒禁掉 4 个号(分类那一半修在 `chat::errors::provider_error_kind`)。
//!
//! # 链怎么走
//!
//! ```text
//! 识别(gw_core::clients,各线共用)→ ① 兜底整理(冻结)→ ② 按客户端的步骤 → ③ 出口合规(所有客户端)
//! ```
//!
//! - **识别**在分流**之前**取信号(原始 top-level system、工具名),与 Kiro 用原始 system
//!   识别同口径。中段注入后被提升进 system 的文本不参与识别。
//! - **① 兜底** = 原有的 system 角色分流([`super::route_system_role_messages`]),原样不动。
//!   ⚠️ 别往里加「通用」清理 —— 那会同时改掉所有客户端的上游字节;新的整理做成独立步骤。
//! - **② 按客户端**:目前没有专用步骤。Kiro 给 pi / Grok Build 做的 `...` 占位清理**刻意没搬**:
//!   11-10 窗口 cursor 流量里 Grok 61 条有 57 条带 `...`、其它 109 条,但带文字的回复里以
//!   `...` 开头的是 0(Kiro 线当时是 19%/18%);搬过来会改几百条正常请求的字节、让进行中的
//!   会话重铺一轮,却没有要修的问题。有证据了再按 `kind` 挂步骤。
//! - **③ 出口合规**按**形态**修,不分客户端(谁发来的违规报文都会让上游 400):放在最后,
//!   因为 ① 可能把中段注入的 system 变成 user,插到 tool_use 与 tool_result 之间。
//!
//! # 不变量
//!
//! - 格式正确的请求**一个字节都不改**(各步骤都有快路径)。
//! - 调用点在 `provider.rs::chat()` 第一行(原来 system 分流的位置),早于会话 ID、
//!   `history_fps`、envnorm 与自造历史;调度亲和键(worker 用原始 body 算)不受影响。
//! - 改写只依赖「一段连续 user 消息」自身,一段后面跟了 assistant 就不再变 → 跨轮稳定,
//!   会话指纹不会因翻译分叉。

use gw_core::clients::{recognize, ClientSignals};
use serde_json::Value;

pub(crate) use gw_core::clients::ClientKind;

mod steps;
pub(crate) use steps::ReorderStats;

/// 翻译结果(请求体已就地改好)。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Translation {
    /// 认出来的客户端(日志按它分口径)。
    pub kind: ClientKind,
    /// 出口合规改写统计;`runs == 0` 表示这一步没动。
    pub reorder: ReorderStats,
}

/// 认出客户端,按链整理请求体(就地修改)。
pub(crate) fn translate(body: &mut Value) -> Translation {
    // 识别:分流之前取信号(见模块文档)。
    let kind = recognize(&ClientSignals::from_body(body));

    // ① 兜底:原有 system 角色分流,冻结。
    super::route_system_role_messages(body);

    // ② 按客户端:目前没有专用步骤(理由见模块文档)。要加时在这里 `match kind`。

    // ③ 出口合规:所有客户端。
    let reorder = match body.get_mut("messages").and_then(Value::as_array_mut) {
        Some(msgs) => steps::tool_results_first(msgs),
        None => ReorderStats::default(),
    };

    Translation { kind, reorder }
}

#[cfg(test)]
mod tests;
