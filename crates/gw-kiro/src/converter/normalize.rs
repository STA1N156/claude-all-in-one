//! 块1a:system 净化升级 —— 精确 billing strip + model identity 规范化。
//!
//! 🟢 借鉴 static_flow `converter/{identity,system,normalize}.rs`,但**保留我方既有优势**:
//! - top-level `system` 字段仍折叠进 history[0](我方稳定 conversationId 依赖此口径,不动)。
//! - conversationId 锚点继续走 [`super::session::normalized_client_system`](pre-routing),
//!   本模块的路由/identity 规范化**不参与**身份哈希,保证 conversationId 跨轮稳定、零迁移。
//!
//! 本模块只做两件事:
//! 1. billing strip 收窄到真正的 CC 滚动指纹行(误伤更小)。
//! 2. model identity 行规范化。
//!
//! messages 数组里 `role=="system"` / `developer` 消息的分流已搬到按客户端翻译的
//! [`super::clients`](`clients/steps.rs`)。

pub(super) use gw_core::clients::{CLAUDE_AGENT_SDK_IDENTITY_LINE, CLAUDE_CODE_CLI_IDENTITY_LINE};
const MODEL_IDENTITY_PREFIX: &str = "You are powered by the model named ";
const MODEL_IDENTITY_DELIMITER: &str = ". The exact model ID is ";

/// 删除 system 文本里的 CC 滚动 billing 指纹行(保留其余内容与换行结构)。
///
/// **实现已上收到 [`gw_core::normalize`]**,本函数是薄委托:cursor 通道也需要同一道处理
/// (它此前没有,后果是会话键每请求都变 —— 见 `gw-cursor` 的 `extract_system`),
/// 两边各留一份必然漂移。行为逐字节不变,本模块的既有测试原样守住。
pub(super) fn strip_rolling_fingerprints(s: &str) -> String {
    gw_core::normalize::strip_rolling_fingerprints(s)
}

// === model identity 规范化 ===

/// 由客户端请求的 model 名给出 (短名, 回显 model_id)。未知模型返回 None(不改写)。
///
/// 走权威表 [`super::model_map::resolve_base`](统一归一 plain/Thinking 后缀/日期):
/// - `short` 取该基础行的 `identity_short`(防 Kiro 真实代号 `claude-quince` 泄漏);
/// - 回显的 model_id 取**去 Thinking 后缀后的原请求名**,从而**保留日期名**
///   (如 `claude-sonnet-4-5-20250929`),让身份行与甲方请求名一致。
///
/// 历史 bug:旧实现对去 thinking 后的名字做精确匹配,日期快照名(`...-20250929`)匹配不上
/// → 返回 None → 身份行不规范化 → claude-quince 泄漏。改走 resolve_base 后日期名也覆盖。
fn requested_model_identity(model: &str) -> Option<(&'static str, String)> {
    // 1. 权威表精确归一:回显去 Thinking 后缀的原请求名
    //    (保留日期段)。去后缀**大小写不敏感**——resolve_base 小写匹配,回显也须一致剥除,
    //    否则 `...-THINKING` 会把后缀漏在身份行里(审查 Skeptic#3/Minimalist#4)。
    if let Some(base) = super::model_map::resolve_base(model) {
        let echo = super::model_map::strip_thinking_suffix(model);
        return Some((base.identity_short, echo.to_string()));
    }
    // 2. 兜底:凡 `map_model` 能路由的名字(子串兜底的异名/未列名)身份保护面也必须覆盖,
    //    否则上游真实代号 `claude-quince` 仍会从身份行泄漏(审查 Skeptic#1/Minimalist#1 共识 high:
    //    身份保护面必须 ⊇ chat 路由面)。反查权威表取 identity_short,回显规范裸名。
    let kiro = super::model_map::map_model(model)?;
    let base = super::model_map::all_models().find(|m| m.kiro_model == kiro)?;
    Some((base.identity_short, base.advertised_id.to_string()))
}

/// 规范化 system 文本里的 model identity 行,统一成
/// `You are powered by the model named {short}. The exact model ID is {id}.`。
///
/// 🟢 static_flow `identity.rs:319-358`。两种情形:① 已有 identity 行 → 原位替换(保留缩进);
/// ② 无 identity 行但有 CC/Agent-SDK 身份行 → 在其后插入一行。未知模型或都没有 → 原样返回。
/// 防止 Kiro 上游的真实模型代号(claude-quince)泄漏给客户端,对齐声明的官方模型名。
pub(super) fn normalize_model_identity(content: String, model: &str) -> String {
    let Some((short, model_id)) = requested_model_identity(model) else {
        return content;
    };
    let replacement =
        format!("{MODEL_IDENTITY_PREFIX}{short}{MODEL_IDENTITY_DELIMITER}{model_id}.");
    let has_identity = content.lines().any(|l| {
        let t = l.trim_start();
        t.contains(MODEL_IDENTITY_PREFIX) && t.contains(MODEL_IDENTITY_DELIMITER)
    });
    let mut replaced = false;
    let mut inserted = false;
    content
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.contains(MODEL_IDENTITY_PREFIX) && trimmed.contains(MODEL_IDENTITY_DELIMITER)
            {
                replaced = true;
                let indent = &line[..line.len() - trimmed.len()];
                format!("{indent}{replacement}")
            } else if !has_identity
                && !replaced
                && !inserted
                && (trimmed == CLAUDE_CODE_CLI_IDENTITY_LINE
                    || trimmed == CLAUDE_AGENT_SDK_IDENTITY_LINE)
            {
                inserted = true;
                format!("{line}\n{replacement}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不变量:每个**公告**对外名(含两种 Thinking 后缀和日期变体)都必须能身份规范化,
    /// 否则 /v1/models 公告了一个会泄漏 Kiro 真实代号(claude-quince)的模型(审查 Minimalist#4)。
    #[test]
    fn every_advertised_model_normalizes_identity() {
        for am in super::super::model_map::advertised_models() {
            assert!(
                requested_model_identity(&am.id).is_some(),
                "公告模型 {} 无身份规范化 → 真实代号可能泄漏",
                am.id
            );
        }
    }

    /// 日期快照名必须规范化,且**回显日期名**(身份行与甲方请求名一致、不泄漏 claude-quince)。
    #[test]
    fn dated_snapshot_normalizes_and_echoes_dated_id() {
        let (short, echo) = requested_model_identity("claude-sonnet-4-5-20250929").unwrap();
        assert_eq!(short, "Sonnet 4.5");
        assert_eq!(echo, "claude-sonnet-4-5-20250929", "应回显日期名");
        // 日期 + thinking:去 thinking 后回显日期名。
        let (_, echo2) = requested_model_identity("claude-sonnet-4-5-20250929-thinking").unwrap();
        assert_eq!(echo2, "claude-sonnet-4-5-20250929");
        let (_, echo3) = requested_model_identity("claude-sonnet-4-5-20250929-reasoning").unwrap();
        assert_eq!(echo3, "claude-sonnet-4-5-20250929");
    }

    /// opus-4-5(老客户端)仍能规范化以防代号泄漏(审查 Architect#1 回归保护)。
    #[test]
    fn legacy_opus_4_5_normalizes_identity() {
        assert!(requested_model_identity("claude-opus-4-5").is_some());
    }

    /// **身份保护面 ⊇ 路由面**:凡 map_model 能路由的名字(含子串兜底异名/反序后缀/大小写),
    /// requested_model_identity 都必须 Some,否则 claude-quince 泄漏(审查 Skeptic#1/Minimalist#1 high)。
    #[test]
    fn substring_routed_models_also_normalize_identity() {
        for m in [
            "claude-opus-4.7-beta",
            "anthropic/claude-3.5-sonnet-4.6",
            "claude-sonnet-4-5-thinking-20250929", // 反序后缀:子串兜底仍路由
            "claude-haiku-4-20250514",
        ] {
            assert!(
                super::super::model_map::map_model(m).is_none()
                    || requested_model_identity(m).is_some(),
                "{m} 可被路由但身份未规范化 → claude-quince 泄漏"
            );
        }
    }

    /// 大小写:`...-THINKING` 回显须去掉后缀(与小写匹配一致)。
    #[test]
    fn thinking_suffix_echo_is_case_insensitive() {
        let (_, echo) = requested_model_identity("claude-opus-4-8-THINKING").unwrap();
        assert_eq!(echo, "claude-opus-4-8", "大写 -THINKING 也应从回显剥除");
        let (_, echo) = requested_model_identity("claude-opus-4-8-REASONING").unwrap();
        assert_eq!(echo, "claude-opus-4-8", "大写 -REASONING 也应从回显剥除");
    }

    // === billing 指纹剥离(放宽判据后的回归保护) ===

    /// 当前已知格式的 billing 指纹行必须被剥,正文保留(回归保护)。
    #[test]
    fn strips_current_billing_fingerprint_line() {
        let sys = "x-anthropic-billing-header: cc_version=2.1.63.a43; cc_entrypoint=cli; cch=ea527;\n\
                   You are Claude Code, Anthropic's official CLI for Claude.\n";
        let out = strip_rolling_fingerprints(sys);
        assert!(!out.contains("cch="), "当前格式 billing 行应被剥");
        assert!(out.contains("You are Claude Code"), "billing 行之后的正文应保留");
    }

    /// CC 版本漂移:billing 头改了字段名/结构(不含 cc_version/cc_entrypoint/cch)也必须被剥,
    /// 否则每请求变化的随机量重新泄进前缀、上游 prefix cache 再次归零。
    /// 旧收窄判据(要求含 cc_* 字段)在此 case 会漏 —— 本测试锁死放宽后的行为。
    #[test]
    fn strips_billing_header_with_drifted_fields() {
        let sys = "x-anthropic-billing-header: ccv=2.2.0; entry=cli; h=abcde;\nrest of system\n";
        let out = strip_rolling_fingerprints(sys);
        assert!(
            !out.contains("x-anthropic-billing-header:"),
            "字段漂移的 billing 行也应被剥(否则随机量泄漏)"
        );
        assert!(out.contains("rest of system"), "正文应保留");
    }

    /// 不误伤:仅碰巧含 `x-anthropic` 但非 `x-anthropic-billing-header:` 前缀的用户正文行保留。
    #[test]
    fn preserves_non_billing_anthropic_lines() {
        let sys = "see header x-anthropic-foo: bar\nx-anthropic-version: 1\n";
        let out = strip_rolling_fingerprints(sys);
        assert_eq!(out, sys, "非 billing-header 前缀行不应被动(零误伤)");
    }

    /// 判据对大小写与前导空白不敏感;普通正文行不命中。
    ///
    /// 实现已上收到 `gw_core::normalize`,这里改为断言**经委托后的公开行为** ——
    /// 委托若被换成别的实现(或漂移),这条仍然会响。
    #[test]
    fn billing_match_is_case_and_indent_insensitive() {
        assert_eq!(strip_rolling_fingerprints("  X-Anthropic-Billing-Header: cch=1;\n"), "");
        assert_eq!(strip_rolling_fingerprints("x-anthropic-billing-header:\n"), "");
        assert_eq!(strip_rolling_fingerprints("hello world\n"), "hello world\n");
    }
}
