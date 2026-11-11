//! 客户端识别(责任链):认出请求来自哪家客户端。Kiro 线、dario 线共用这一份判据。
//!
//! # 为什么放在公共层
//!
//! 各家客户端拼 Anthropic 报文的习惯不一样(pi 把系统提示放在 `role=developer`、Grok Build
//! 和 pi 把纯工具回合写成 `...`、Copilot 带 53 个 VS Code 工具……)。「这是谁」的知识原先只在
//! `gw-kiro` 里,dario 线完全用不上;两条线各认各的迟早对不上。这里只放**判据**,各条线拿到
//! [`ClientKind`] 后按自己上游的需要整理(Kiro 线的整理在 `gw-kiro::converter::clients`,
//! dario 线的在 `gw-dario::cc`)。
//!
//! # 输入由调用方准备
//!
//! 判据只看三样东西(见 [`ClientSignals`]):system 全文、工具名、第一条 assistant 之前有没有
//! developer。Kiro 线判 developer 用的是**截掉末尾 prefill 之后**的消息,和原始 body 有细微差别,
//! 所以由各条线自己准备输入,保证各自的判定结果和改造前逐条一致。
//!
//! # 链怎么走
//!
//! [`CHAIN`] 从头往后问,第一个认出来的算数;都认不出来是 [`ClientKind::Fallback`]。
//! 顺序有讲究:特征最明确的在前。

use serde_json::Value;

/// Claude Code CLI 的身份行(system 里单独成行)。
pub const CLAUDE_CODE_CLI_IDENTITY_LINE: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
/// Claude Agent SDK 的身份行。
pub const CLAUDE_AGENT_SDK_IDENTITY_LINE: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";

/// CC 每个请求 system 第一行带的计费头(只看开头这么多字节)。
const BILLING_HEADER: &str = "x-anthropic-billing-header:";
const BILLING_HEADER_SCAN_BYTES: usize = 512;
/// 看 system 开头时只取这么多字节(判据都在开头,长提示词不必整段扫)。
const SYSTEM_SCAN_BYTES: usize = 512;

/// Grok Build 主 agent 与子 agent 都带的工具(子 agent 没有 spawn_subagent),命中两个以上即认。
const GROK_SIGNATURE_TOOLS: [&str; 3] =
    ["spawn_subagent", "get_command_or_subagent_output", "kill_command_or_subagent"];
const GROK_SIGNATURE_TOOLS_MIN: usize = 2;

/// 10-29 线上见到的 OpenCode 开头(「You are OpenCode, …」两种 + 「running in OpenCode」一种)。
const OPENCODE_PREFIXES: [&str; 2] = ["You are OpenCode", "You are an AI agent running in OpenCode"];
const DEEPSEEK_HARNESS_PREFIX: &str = "You are an AI agent powered by DeepSeek Harness";
/// VS Code Copilot Chat(agent 模式),10-30 线上 11/11 条一致。
const COPILOT_PREFIX: &str = "You are an expert AI programming assistant, working with a user in the VS Code editor";

/// 认出来的客户端。日志 / 统计用它分口径。会继续加新客户端,外部别穷举匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ClientKind {
    /// Claude Code / Claude Agent SDK(带计费头或身份行)。
    ClaudeCode,
    /// 无 top-level system、系统提示放在 `role=developer` 的客户端(目前是 pi)。
    /// 报文形态(10-29 线上 312/312 条一致):首条 user 是 `...` 占位,紧跟一条 developer,
    /// 再是真正的首句;纯工具回合的 assistant 正文写成 `...`。
    DeveloperRole,
    /// Grok Build(xAI 的命令行编程 agent)。单块 system「You are Grok released by xAI…」或
    /// 「You are a Grok Build subagent…」;有用户把系统提示开头改掉(10-29 有 72 条
    /// 「You are Claude Opus 5.5 from DogAPI…」),所以以它独有的工具名为主、开头为辅。
    GrokBuild,
    /// OpenCode(含 oh-my-opencode 等以它为底的发行版)。
    OpenCode,
    /// DeepSeek Harness(带 top-level system 的那种)。
    DeepSeekHarness,
    /// VS Code Copilot Chat。
    Copilot,
    /// 认不出来。
    Fallback,
}

impl ClientKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ClientKind::ClaudeCode => "claude_code",
            ClientKind::DeveloperRole => "developer_role",
            ClientKind::GrokBuild => "grok_build",
            ClientKind::OpenCode => "opencode",
            ClientKind::DeepSeekHarness => "deepseek_harness",
            ClientKind::Copilot => "copilot",
            ClientKind::Fallback => "fallback",
        }
    }
}

/// 识别要看的全部输入。
#[derive(Debug, Clone, Default)]
pub struct ClientSignals<'a> {
    /// top-level system 的全部文本(多块用换行拼);没有 system 为空串。
    pub system_text: String,
    pub tool_names: Vec<&'a str>,
    /// 第一条 assistant 之前出现过 `role=developer`。
    pub developer_before_first_assistant: bool,
}

impl<'a> ClientSignals<'a> {
    /// 从原始 Anthropic 请求体取(dario 线用)。system 是字符串就当一块,是数组就取各块的
    /// `text` 用换行拼(与 `gw-kiro` 的 `deserialize_system` 同口径)。
    pub fn from_body(body: &'a Value) -> Self {
        let system_text = match body.get("system") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .map(|b| b.get("text").and_then(Value::as_str).unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let tool_names = body
            .get("tools")
            .and_then(Value::as_array)
            .map(|ts| ts.iter().filter_map(|t| t.get("name").and_then(Value::as_str)).collect())
            .unwrap_or_default();
        let developer_before_first_assistant = body
            .get("messages")
            .and_then(Value::as_array)
            .map(|ms| ms.iter().take_while(|m| role_of(m) != "assistant").any(|m| role_of(m) == "developer"))
            .unwrap_or(false);
        Self { system_text, tool_names, developer_before_first_assistant }
    }
}

fn role_of(m: &Value) -> &str {
    m.get("role").and_then(Value::as_str).unwrap_or("")
}

type Recognizer = fn(&ClientSignals) -> bool;

/// 顺序有讲究:特征最明确的在前。
static CHAIN: &[(ClientKind, Recognizer)] = &[
    (ClientKind::ClaudeCode, is_claude_code),
    (ClientKind::DeveloperRole, is_developer_role),
    (ClientKind::GrokBuild, is_grok_build),
    (ClientKind::OpenCode, is_opencode),
    (ClientKind::DeepSeekHarness, is_deepseek_harness),
    (ClientKind::Copilot, is_copilot),
];

pub fn recognize(s: &ClientSignals) -> ClientKind {
    CHAIN.iter().find(|(_, is)| is(s)).map(|(kind, _)| *kind).unwrap_or(ClientKind::Fallback)
}

fn is_claude_code(s: &ClientSignals) -> bool {
    safe_prefix(&s.system_text, BILLING_HEADER_SCAN_BYTES).contains(BILLING_HEADER)
        || s.system_text.lines().map(str::trim_start).any(|l| {
            l == CLAUDE_CODE_CLI_IDENTITY_LINE || l == CLAUDE_AGENT_SDK_IDENTITY_LINE
        })
}

/// 无 top-level system,且第一条 assistant 之前有 developer。
fn is_developer_role(s: &ClientSignals) -> bool {
    s.system_text.trim().is_empty() && s.developer_before_first_assistant
}

fn is_grok_build(s: &ClientSignals) -> bool {
    // 按不同的工具名计数:同一个工具重复定义两次不能算命中两个。
    let hits = GROK_SIGNATURE_TOOLS.iter().filter(|sig| s.tool_names.iter().any(|n| n == *sig)).count();
    if hits >= GROK_SIGNATURE_TOOLS_MIN {
        return true;
    }
    let head = system_head(s);
    starts_with_word(head, "You are Grok") || head.contains("Grok Build subagent")
}

fn is_opencode(s: &ClientSignals) -> bool {
    let head = s.system_text.trim_start();
    OPENCODE_PREFIXES.iter().any(|p| head.starts_with(p))
}

fn is_deepseek_harness(s: &ClientSignals) -> bool {
    s.system_text.trim_start().starts_with(DEEPSEEK_HARNESS_PREFIX)
}

fn is_copilot(s: &ClientSignals) -> bool {
    s.system_text.trim_start().starts_with(COPILOT_PREFIX)
}

fn system_head<'s>(s: &'s ClientSignals) -> &'s str {
    safe_prefix(s.system_text.trim_start(), SYSTEM_SCAN_BYTES)
}

/// `s` 以 `word` 开头,且后面不是字母数字(「You are Grokking…」不算)。
fn starts_with_word(s: &str, word: &str) -> bool {
    s.strip_prefix(word)
        .is_some_and(|rest| !rest.chars().next().is_some_and(char::is_alphanumeric))
}

/// 取前 `max_bytes` 字节,退到最近的字符边界(不在多字节字符中间切)。
pub fn safe_prefix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kind(body: Value) -> ClientKind {
        recognize(&ClientSignals::from_body(&body))
    }

    #[test]
    fn claude_code_by_billing_header_or_identity_line() {
        let billing = json!({"system": [{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1;"},
                                        {"type": "text", "text": "随便什么"}], "messages": []});
        assert_eq!(kind(billing), ClientKind::ClaudeCode);
        let sdk = json!({"system": format!("前言\n{CLAUDE_AGENT_SDK_IDENTITY_LINE}\n其余"), "messages": []});
        assert_eq!(kind(sdk), ClientKind::ClaudeCode);
        // 身份行必须单独成行:句中提到不算。
        let mention = json!({"system": format!("请模仿 {CLAUDE_CODE_CLI_IDENTITY_LINE} 的口吻"), "messages": []});
        assert_eq!(kind(mention), ClientKind::Fallback);
    }

    #[test]
    fn developer_role_needs_empty_system_and_developer_before_first_assistant() {
        let pi = json!({"messages": [{"role": "user", "content": "..."}, {"role": "developer", "content": "提示"},
                                      {"role": "user", "content": "首句"}]});
        assert_eq!(kind(pi), ClientKind::DeveloperRole);
        let late = json!({"messages": [{"role": "user", "content": "a"}, {"role": "assistant", "content": "b"},
                                        {"role": "developer", "content": "插话"}]});
        assert_eq!(kind(late), ClientKind::Fallback, "assistant 之后的 developer 不是系统提示");
        let with_sys = json!({"system": "有 system", "messages": [{"role": "developer", "content": "x"}]});
        assert_eq!(kind(with_sys), ClientKind::Fallback);
    }

    #[test]
    fn grok_build_by_tools_survives_rewritten_system() {
        let tools = json!([{"name": "spawn_subagent"}, {"name": "kill_command_or_subagent"}]);
        let body = json!({"system": "You are Claude Opus 5.5 from DogAPI", "tools": tools, "messages": []});
        assert_eq!(kind(body), ClientKind::GrokBuild);
        // 同一个特征工具定义两次只算一个。
        let dup = json!({"tools": [{"name": "spawn_subagent"}, {"name": "spawn_subagent"}], "messages": []});
        assert_eq!(kind(dup), ClientKind::Fallback);
        assert_eq!(kind(json!({"system": "You are Grok released by xAI", "messages": []})), ClientKind::GrokBuild);
        assert_eq!(kind(json!({"system": "You are Grokking the code", "messages": []})), ClientKind::Fallback);
    }

    #[test]
    fn opencode_deepseek_copilot_by_system_prefix() {
        assert_eq!(kind(json!({"system": "You are OpenCode, the best", "messages": []})), ClientKind::OpenCode);
        assert_eq!(
            kind(json!({"system": [{"type": "text", "text": "You are an AI agent running in OpenCode."}], "messages": []})),
            ClientKind::OpenCode
        );
        assert_eq!(
            kind(json!({"system": "You are an AI agent powered by DeepSeek Harness.", "messages": []})),
            ClientKind::DeepSeekHarness
        );
        let copilot = "You are an expert AI programming assistant, working with a user in the VS Code editor.\nWhen asked for your name, you must respond with \"GitHub Copilot\".";
        assert_eq!(kind(json!({"system": [{"type": "text", "text": copilot}], "messages": []})), ClientKind::Copilot);
        assert_eq!(kind(json!({"system": "You are a helpful assistant.", "messages": []})), ClientKind::Fallback);
    }

    #[test]
    fn claude_code_outranks_others() {
        // CC 身份行 + Grok 特征工具:CC 在链首,先认。
        let body = json!({"system": CLAUDE_CODE_CLI_IDENTITY_LINE,
                          "tools": [{"name": "spawn_subagent"}, {"name": "kill_command_or_subagent"}], "messages": []});
        assert_eq!(kind(body), ClientKind::ClaudeCode);
    }

    #[test]
    fn safe_prefix_never_splits_a_char() {
        let s = "中文字符串";
        assert_eq!(safe_prefix(s, 4), "中");
        assert_eq!(safe_prefix(s, 100), s);
    }
}
