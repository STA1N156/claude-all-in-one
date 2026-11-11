//! 逐条筛查:不看历史、只看这一条请求本身就能判定的两类高危请求,直接拒绝。
//!
//! # 为什么要拦
//!
//! 2026-10-14 17:32~17:37,某下游用户连发测试:撑大上下文(19 万字符,`max_tokens=64`)、
//! 「每 8 秒一条 ok」(`max_tokens=8`)、上下文探测(`max_tokens=16`)。接到的 4 个号 4~24
//! 分钟内全被封,同窗没接到的 26 个号 0 封;他此前 4 次集中测试有 3 次 30 分钟内跟着封号。
//! 这类请求量不大,[`super::batch_guard`] 的模板计数(40 条 / 10 分钟)根本数不到,只能逐条拦。
//!
//! 本规则 10-14 上线、10-23 重建 worker0 时丢失(源码只在临时目录里,从未进主代码树),
//! 10-28 按当时的判据重写。
//!
//! # 判据
//!
//! - **探针**:单轮(`messages.len() == 1`)+ 不带 tools + 没有 system(剥掉 CC 的计费头行后
//!   为空)+ `max_tokens ≤ 64`([`PROBE_MAX_TOKENS`])。主流对话 agent 的首句同样是单轮、
//!   无 system,但 `max_tokens` 几千起,不受影响;CC 自动模式分类器 `max_tokens=64` 但带
//!   system,也不受影响。
//! - **超大输入**:估算输入 > 150 万 token([`OVERSIZE_TOKENS`])一律拒。估算覆盖 system、
//!   全部文本块、tool_result 正文、tool_use 参数、thinking 与工具定义;图片 / 文档的 base64
//!   不计(它们另有体积闸)。
//!
//!   10-14 的原规则是「估算 > 100 万」。10-28 用线上 request_logs 校准:上游实际输入最大
//!   99.4 万(上下文就封顶在这儿),而本估算对大请求偏高最多 1.17 倍 —— 按 100 万拦,9 分钟
//!   就误伤了 2 条实际 89 万、成功返回的长会话。150 万 ÷ 1.17 ≈ 128 万,超过它的请求上游
//!   本就装不下,拦掉它们只是不让注定失败的请求去碰号。
//!
//! # 模式
//!
//! 跟随 `batch_guard_mode`(off / observe / enforce),不另设开关;`batch_guard_allow_prefixes`
//! 命中的模板同样豁免。只筛带客户 key 的请求(由调用方判断)。
//!
//! # 已知漏网
//!
//! `max_tokens` 大的测试(「数到 400」`max_tokens=4000`、上下文探测 `max_tokens=3000`)不拦 ——
//! 再收紧就会误伤正常的单轮首句。

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

use gw_core::config::batch_guard::{mode_str, BatchGuardMode};
use gw_core::normalize::strip_leading_billing_lines;

use super::batch_guard;

/// 探针判据的 `max_tokens` 上限(含)。
pub const PROBE_MAX_TOKENS: u64 = 64;
/// 超大输入判据:估算输入严格大于它即拒。取值依据见模块文档「超大输入」。
pub const OVERSIZE_TOKENS: u64 = 1_500_000;

/// 请求日志 `error_kind` 列里的标记。
pub const LOG_KIND_PROBE: &str = "ProbeBlocked";
pub const LOG_KIND_OVERSIZE: &str = "OversizeBlocked";

/// 拒绝文案。与 [`batch_guard::REJECT_MESSAGE`] 同理,不得出现下游 new-api 的渠道禁用关键词
/// (`Permission denied` / `Operation not allowed` / `Your account is not authorized` …)。
pub const PROBE_MESSAGE: &str = "检测到测试探针请求(单轮、无系统提示词、max_tokens 过小),本渠道不支持,请改用其它渠道";
pub const OVERSIZE_MESSAGE: &str = "请求输入过大(超出上下文上限),本渠道不支持,请压缩上下文后重试";

/// 命中了哪一条。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Rule {
    Probe,
    Oversize,
}

impl Rule {
    pub fn log_kind(self) -> &'static str {
        match self {
            Rule::Probe => LOG_KIND_PROBE,
            Rule::Oversize => LOG_KIND_OVERSIZE,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Rule::Probe => PROBE_MESSAGE,
            Rule::Oversize => OVERSIZE_MESSAGE,
        }
    }
}

/// 对一条请求的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// 命中。`block` = 是否真的拦(enforce);observe 下为 false。`est_tokens` 只在超大输入时有值。
    Hit { rule: Rule, block: bool, est_tokens: u64 },
}

thread_local! {
    /// 当前线程的模式覆盖。**只给测试用**:全局模式改成 enforce 会误伤并行跑的其他测试,
    /// 而 `#[tokio::test]` 默认单线程运行时,整条请求都在测试线程上执行。生产代码从不设置它。
    static MODE_OVERRIDE: std::cell::Cell<Option<BatchGuardMode>> = const { std::cell::Cell::new(None) };
}

/// [`override_mode`] 的守卫:drop 时恢复。
#[cfg(test)]
pub struct ModeOverride(Option<BatchGuardMode>);

#[cfg(test)]
impl Drop for ModeOverride {
    fn drop(&mut self) {
        MODE_OVERRIDE.with(|m| m.set(self.0));
    }
}

/// 在当前线程上临时改筛查模式(测试用,见 `MODE_OVERRIDE`)。
#[cfg(test)]
pub fn override_mode(mode: BatchGuardMode) -> ModeOverride {
    ModeOverride(MODE_OVERRIDE.with(|m| m.replace(Some(mode))))
}

static PROBE_HITS: AtomicU64 = AtomicU64::new(0);
static OVERSIZE_HITS: AtomicU64 = AtomicU64::new(0);
static BLOCKED: AtomicU64 = AtomicU64::new(0);

/// `/health` 展示用的累计计数(进程内,重启清零)。
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub mode: &'static str,
    pub probe_hits: u64,
    pub oversize_hits: u64,
    pub blocked: u64,
}

pub fn snapshot() -> Snapshot {
    Snapshot {
        mode: mode_str(batch_guard::current().mode),
        probe_hits: PROBE_HITS.load(Ordering::Relaxed),
        oversize_hits: OVERSIZE_HITS.load(Ordering::Relaxed),
        blocked: BLOCKED.load(Ordering::Relaxed),
    }
}

/// 筛查一条请求(并记账)。`count` 是单段文本的 token 估算函数(由 provider 层提供,
/// 与计费估算同一把尺)。
pub fn check(body: &serde_json::Value, count: &dyn Fn(&str) -> u64) -> Verdict {
    let cfg = batch_guard::current();
    let mode = MODE_OVERRIDE.with(|m| m.get()).unwrap_or(cfg.mode);
    let verdict = check_with(mode, &cfg.allow_prefixes, body, count);
    if let Verdict::Hit { rule, block, .. } = verdict {
        match rule {
            Rule::Probe => PROBE_HITS.fetch_add(1, Ordering::Relaxed),
            Rule::Oversize => OVERSIZE_HITS.fetch_add(1, Ordering::Relaxed),
        };
        if block {
            BLOCKED.fetch_add(1, Ordering::Relaxed);
        }
    }
    verdict
}

fn check_with(
    mode: BatchGuardMode,
    allow_prefixes: &[String],
    body: &serde_json::Value,
    count: &dyn Fn(&str) -> u64,
) -> Verdict {
    if mode == BatchGuardMode::Off {
        return Verdict::Pass;
    }
    if let Some(head) = batch_guard::prompt_head(body) {
        if allow_prefixes.iter().any(|p| !p.is_empty() && head.starts_with(p.as_str())) {
            return Verdict::Pass;
        }
    }
    let block = mode == BatchGuardMode::Enforce;
    if is_probe(body) {
        return Verdict::Hit { rule: Rule::Probe, block, est_tokens: 0 };
    }
    let est = estimate_capped(body, count, OVERSIZE_TOKENS);
    if est > OVERSIZE_TOKENS {
        return Verdict::Hit { rule: Rule::Oversize, block, est_tokens: est };
    }
    Verdict::Pass
}

/// 探针:单轮 + 不带 tools + 没有 system + `max_tokens ≤ 64`。缺 `max_tokens` 不算(认不出就放行)。
fn is_probe(body: &serde_json::Value) -> bool {
    let single_turn = body.get("messages").and_then(|m| m.as_array()).is_some_and(|m| m.len() == 1);
    let has_tools = body.get("tools").and_then(|t| t.as_array()).is_some_and(|t| !t.is_empty());
    let small = body.get("max_tokens").and_then(|v| v.as_u64()).is_some_and(|n| n <= PROBE_MAX_TOKENS);
    single_turn && !has_tools && small && !has_system(body)
}

/// system 里剥掉计费头行后还有没有非空文本。
fn has_system(body: &serde_json::Value) -> bool {
    fn meaningful(text: &str) -> bool {
        !strip_leading_billing_lines(text).trim().is_empty()
    }
    match body.get("system") {
        Some(serde_json::Value::String(s)) => meaningful(s),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .any(meaningful),
        _ => false,
    }
}

/// 估算整条请求的输入 token:system、全部文本块、tool_result 正文、tool_use 参数、
/// thinking、文本来源的 document、工具定义。图片 / 文档的 base64 不计。
///
/// 口径要跟转换器(`gw-kiro` converter)实际转发的正文对齐,否则能绕过:对象 / 数字形态的
/// tool_result.content 转换器会 `to_string()` 当正文发,`source.type = "text"` 的文档会把
/// `data` 当正文发 —— 这两样都要计。
#[cfg(test)]
pub fn estimate_input_tokens(body: &serde_json::Value, count: &dyn Fn(&str) -> u64) -> u64 {
    estimate_capped(body, count, u64::MAX)
}

/// 同 [`estimate_input_tokens`],但累计一超过 `cap` 就停(筛查只要「超没超」,不必扫完几十 MB)。
/// 返回值 > cap 时只保证「超了」,不是精确估算。
fn estimate_capped(body: &serde_json::Value, count: &dyn Fn(&str) -> u64, cap: u64) -> u64 {
    let mut acc = Acc { total: 0, cap, count };
    let _ = acc.body(body);
    acc.total
}

/// 带上限的累加器。各方法返回 `Err(())` = 已超上限,调用方立刻逐层返回。
struct Acc<'a> {
    total: u64,
    cap: u64,
    count: &'a dyn Fn(&str) -> u64,
}

impl Acc<'_> {
    fn add(&mut self, text: &str) -> Result<(), ()> {
        self.total = self.total.saturating_add((self.count)(text));
        if self.total > self.cap { Err(()) } else { Ok(()) }
    }

    fn body(&mut self, body: &serde_json::Value) -> Result<(), ()> {
        match body.get("system") {
            Some(serde_json::Value::String(s)) => self.add(s)?,
            Some(serde_json::Value::Array(blocks)) => {
                for b in blocks {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        self.add(t)?;
                    }
                }
            }
            _ => {}
        }
        if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
            for m in messages {
                self.content(m.get("content"))?;
            }
        }
        if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
            for t in tools {
                self.add(&t.to_string())?;
            }
        }
        Ok(())
    }

    fn content(&mut self, content: Option<&serde_json::Value>) -> Result<(), ()> {
        match content {
            None | Some(serde_json::Value::Null) => Ok(()),
            Some(serde_json::Value::String(s)) => self.add(s),
            Some(serde_json::Value::Array(blocks)) => {
                for b in blocks {
                    self.block(b)?;
                }
                Ok(())
            }
            // 对象 / 数字 / 布尔:转换器按 to_string() 当正文转发,照样计。
            Some(other) => self.add(&other.to_string()),
        }
    }

    fn block(&mut self, block: &serde_json::Value) -> Result<(), ()> {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => block.get("text").and_then(|t| t.as_str()).map_or(Ok(()), |t| self.add(t)),
            Some("thinking") => block.get("thinking").and_then(|t| t.as_str()).map_or(Ok(()), |t| self.add(t)),
            Some("tool_use") => block.get("input").map_or(Ok(()), |v| self.add(&v.to_string())),
            Some("tool_result") => self.content(block.get("content")),
            Some("document") => {
                let src = block.get("source");
                match src.and_then(|s| s.get("type")).and_then(|t| t.as_str()) {
                    Some("text") => src.and_then(|s| s.get("data")).and_then(|d| d.as_str()).map_or(Ok(()), |d| self.add(d)),
                    Some("content") => self.content(src.and_then(|s| s.get("content"))),
                    // base64 / url:另有体积闸,不计。
                    _ => Ok(()),
                }
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 测试用尺子:一个字符一个 token,数字好算。
    fn chars(s: &str) -> u64 {
        s.chars().count() as u64
    }

    fn run(mode: BatchGuardMode, body: &serde_json::Value) -> Verdict {
        check_with(mode, &[], body, &chars)
    }

    fn probe_body(max_tokens: u64) -> serde_json::Value {
        json!({"model": "claude-sonnet-5", "max_tokens": max_tokens,
               "messages": [{"role": "user", "content": "ok"}]})
    }

    #[test]
    fn probe_is_blocked_in_enforce_and_only_counted_in_observe() {
        for mt in [1, 8, 16, 64] {
            assert_eq!(
                run(BatchGuardMode::Enforce, &probe_body(mt)),
                Verdict::Hit { rule: Rule::Probe, block: true, est_tokens: 0 },
                "max_tokens={mt}"
            );
        }
        assert_eq!(
            run(BatchGuardMode::Observe, &probe_body(8)),
            Verdict::Hit { rule: Rule::Probe, block: false, est_tokens: 0 }
        );
        assert_eq!(run(BatchGuardMode::Off, &probe_body(8)), Verdict::Pass);
    }

    /// 只差一个条件就不是探针:正常的单轮首句、CC 分类器、子代理、多轮都必须放行。
    #[test]
    fn near_misses_are_not_probes() {
        let e = BatchGuardMode::Enforce;
        // max_tokens 65 / 几千:主流对话 agent 的首句。
        assert_eq!(run(e, &probe_body(65)), Verdict::Pass);
        assert_eq!(run(e, &probe_body(32000)), Verdict::Pass);
        // 缺 max_tokens:认不出就放行。
        assert_eq!(run(e, &json!({"messages": [{"role": "user", "content": "ok"}]})), Verdict::Pass);
        // 带 system(CC 自动模式分类器就是 max_tokens=64 + system)。
        let mut b = probe_body(64);
        b["system"] = json!([{"type": "text", "text": "You are a security monitor"}]);
        assert_eq!(run(e, &b), Verdict::Pass);
        let mut b = probe_body(64);
        b["system"] = json!("x-anthropic-billing-header: cc_version=2.1.31;\nYou are Claude Code");
        assert_eq!(run(e, &b), Verdict::Pass);
        // 带 tools。
        let mut b = probe_body(8);
        b["tools"] = json!([{"name": "Read", "input_schema": {}}]);
        assert_eq!(run(e, &b), Verdict::Pass);
        // 多轮。
        let mut b = probe_body(8);
        b["messages"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "ok"}
        ]);
        assert_eq!(run(e, &b), Verdict::Pass);
    }

    /// system 里只有计费头(或空串)等于没有 system,照样是探针。
    #[test]
    fn billing_header_only_system_counts_as_no_system() {
        for sys in [
            json!("x-anthropic-billing-header: cc_version=2.1.31; cc_entrypoint=cli;"),
            json!([{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.31;"}, {"type": "text", "text": "  "}]),
            json!(""),
            json!([]),
        ] {
            let mut b = probe_body(16);
            b["system"] = sys.clone();
            assert!(matches!(run(BatchGuardMode::Enforce, &b), Verdict::Hit { rule: Rule::Probe, .. }), "{sys}");
        }
    }

    #[test]
    fn oversize_counts_tool_results_and_tool_inputs() {
        let e = BatchGuardMode::Enforce;
        let half = "a".repeat(750_000);
        // 正文只有一半,另一半藏在 tool_result 里 —— 只数 text 块会漏掉。
        let b = json!({
            "max_tokens": 32000,
            "system": "sys",
            "messages": [
                {"role": "user", "content": half},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"p": "x"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1",
                    "content": [{"type": "text", "text": half}]}]}
            ]
        });
        match run(e, &b) {
            Verdict::Hit { rule: Rule::Oversize, block: true, est_tokens } => assert!(est_tokens > OVERSIZE_TOKENS),
            v => panic!("应判超大输入,实际 {v:?}"),
        }
        // 边界:恰好 150 万不拦(严格大于才拒),多 1 个就拦。
        let at = |n: usize| json!({"max_tokens": 32000, "messages": [
            {"role": "user", "content": "a".repeat(n - 1)},
            {"role": "assistant", "content": "b"}
        ]});
        assert_eq!(estimate_input_tokens(&at(1_500_000), &chars), 1_500_000);
        assert_eq!(run(e, &at(1_500_000)), Verdict::Pass);
        assert!(matches!(run(e, &at(1_500_001)), Verdict::Hit { rule: Rule::Oversize, .. }));
        // 线上实际 89 万的长会话,估算 105 万:不得误伤(10-28 校准时 100 万阈值就误伤了它)。
        assert_eq!(run(e, &at(1_050_000)), Verdict::Pass);
    }

    /// 转换器会当正文转发的形态都要计,否则能绕过(10-28 评审 high)。
    #[test]
    fn oversize_counts_object_tool_results_and_text_documents() {
        let e = BatchGuardMode::Enforce;
        let big = "a".repeat(1_600_000);
        let obj = json!({"max_tokens": 1000, "messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": "x", "name": "R", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "x", "content": {"result": big}}]}
        ]});
        assert!(matches!(run(e, &obj), Verdict::Hit { rule: Rule::Oversize, .. }), "对象形态 tool_result");
        let doc = json!({"max_tokens": 1000, "messages": [{"role": "user", "content": [
            {"type": "text", "text": "read"},
            {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": big}}
        ]}]});
        assert!(matches!(run(e, &doc), Verdict::Hit { rule: Rule::Oversize, .. }), "文本来源 document");
        let nested = json!({"max_tokens": 1000, "messages": [{"role": "user", "content": [
            {"type": "document", "source": {"type": "content", "content": [{"type": "text", "text": big}]}}
        ]}]});
        assert!(matches!(run(e, &nested), Verdict::Hit { rule: Rule::Oversize, .. }), "content 来源 document");
    }

    /// 超过上限立刻停,不扫完后面的几十 MB。
    #[test]
    fn capped_estimate_stops_early() {
        let scanned = std::cell::Cell::new(0u64);
        let counting = |s: &str| {
            scanned.set(scanned.get() + s.len() as u64);
            s.len() as u64
        };
        let b = json!({"max_tokens": 1000, "messages": [{"role": "user", "content": [
            {"type": "text", "text": "a".repeat(1_600_000)},
            {"type": "text", "text": "b".repeat(10_000_000)}
        ]}]});
        assert!(estimate_capped(&b, &counting, OVERSIZE_TOKENS) > OVERSIZE_TOKENS);
        assert_eq!(scanned.get(), 1_600_000, "第二块不该再扫");
        // 不封顶的版本照样给精确值。
        assert_eq!(estimate_input_tokens(&b, &chars), 11_600_000);
    }

    #[test]
    fn mode_override_is_thread_local_and_restored() {
        let before = MODE_OVERRIDE.with(|m| m.get());
        {
            let _g = override_mode(BatchGuardMode::Enforce);
            assert_eq!(MODE_OVERRIDE.with(|m| m.get()), Some(BatchGuardMode::Enforce));
            std::thread::spawn(|| assert_eq!(MODE_OVERRIDE.with(|m| m.get()), None)).join().unwrap();
        }
        assert_eq!(MODE_OVERRIDE.with(|m| m.get()), before);
    }

    /// 图片 / 文档的 base64 不计入估算(它们另有体积闸)。
    #[test]
    fn media_payload_is_not_counted() {
        let b = json!({"max_tokens": 32000, "messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "A".repeat(2_000_000)}},
            {"type": "text", "text": "看图"}
        ]}]});
        assert_eq!(estimate_input_tokens(&b, &chars), 2);
        assert_eq!(run(BatchGuardMode::Enforce, &b), Verdict::Pass);
    }

    #[test]
    fn allow_prefixes_exempt_both_rules() {
        let allow = vec!["ok".to_string()];
        assert_eq!(check_with(BatchGuardMode::Enforce, &allow, &probe_body(8), &chars), Verdict::Pass);
        // 空串前缀不得变成「全部豁免」。
        let empty = vec![String::new()];
        assert!(matches!(
            check_with(BatchGuardMode::Enforce, &empty, &probe_body(8), &chars),
            Verdict::Hit { rule: Rule::Probe, .. }
        ));
    }

    /// 文案里不得出现下游 new-api 的渠道自动禁用关键词。
    #[test]
    fn reject_messages_avoid_channel_disable_keywords() {
        for msg in [PROBE_MESSAGE, OVERSIZE_MESSAGE] {
            let lower = msg.to_lowercase();
            for kw in ["permission denied", "operation not allowed", "your account is not authorized",
                "your credit balance is too low", "organization has been disabled", "exceeded your current quota"] {
                assert!(!lower.contains(kw), "{msg} 含禁用关键词 {kw}");
            }
        }
    }
}
