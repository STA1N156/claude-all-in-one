//! 缓存计费收尾:把上游计量信号 + prefix 模拟器结果,折算成发给客户端的 usage。
//!
//! ## 流水线位置
//!
//! ```text
//! chat_stream ──observe──▶ SimResult ─┐
//!                                      ├─▶ BillingCtx ──▶ 流任务帧循环填 UsageSignals
//! KiroProvider.cache_policy ──────────┘                        │
//!                                                              ▼
//!                                         finalize(..) ─▶ BillingOutcome ─▶ message_delta.usage
//!                                                                         └▶ StreamItem::Usage(落库)
//! ```
//!
//! 帧循环只**记录**信号(tokenUsage / contextUsage / metering),不做任何计费算术;
//! 所有算术集中在本模块的纯函数 [`finalize`],可脱离 eventstream 单测。
//!
//! ## 口径
//!
//! - 计费基准(总输入):tokenUsageEvent 真值 > contextUsageEvent 估算(pct×窗口)> 模拟器总量。
//!   contextUsage 只在流末发一次,给的是**生成之后**的上下文大小,含本轮输出。
//! - 输出 token:Kiro 对 Claude 不下发 tokenUsageEvent,只能按字符估算。
//! - 输入输出口径由 `cache_io_correction` 热切换(默认关 = 旧口径,见 [`OutputCounts`]):
//!   - 开:输出按模型分词器折算(见 [`crate::text_tokens::OutputTally`]),输入 = contextUsage
//!     减本轮输出(见 [`UsageSignals::billed_input`]);
//!   - 关:输出按旧公式逐帧估算(少算约 37%),输入 = contextUsage 原值(可见输出在输入、
//!     输出里各收一次)。
//!   积分反推恒用新口径(内置参数表按新口径标定),算出的命中比例再乘到当前口径的输入上。
//! - 零产出(没交付任何正文/工具调用/思考)不计缓存:用户没拿到东西不该为缓存付费。
//! - cache_creation 恒 0(统一模型不单列缓存写入)。
//! - cache_read 上报按 [`CacheStrategy`] 三选一(设置面板热切换):
//!   - `sim`:模拟器命中比例 × 基准 × 倍率,按 floor/cap 夹限(见 [`crate::usage::reported_cache_read`])。
//!   - `credit_correct` / `credit_fit`:用 meteringEvent 的真实积分反推命中比例 h(见
//!     [`infer_hit_ratio`]),前者只判冷热(h ≥ 0.5 → cap,否则 floor),后者按 h 夹限上报。
//!     拿不到积分、模型无标定参数、输出过长、企业号时退回 `sim`(见 [`Decision`])。
//!
//! ## 为什么要积分策略(2026-10-05 生产核对)
//!
//! 模拟器每会话只记上一条请求,而 Kiro 同号会同时保留多份前缀(5 分钟);会话键前几轮会变;
//! 模拟器把 currentMessage 也算可命中,Kiro 实际不缓存它。24h 约 2.6 万条可靠样本里模拟器判错
//! 20%(两个方向都有)。积分是上游真实扣费,按模型拟合后在留出集上判对 97%–99%。

use std::collections::BTreeMap;
use std::sync::Arc;

use gw_core::config::{CacheStrategy, CreditModelSpec};

use crate::cache_sim::SimResult;
use crate::usage::{reported_cache_read, DEFAULT_CACHE_CAP_RATIO, DEFAULT_CACHE_FLOOR_RATIO, DEFAULT_CACHE_READ_MULTIPLIER};

/// 总输入低于此值不做积分反推:积分里的常数项和隐藏系统提示词占比太大,反推不稳。
const CREDIT_MIN_INPUT_TOKENS: i32 = 1000;
/// `credit_correct` 判热的阈值(命中比例)。
const CREDIT_WARM_THRESHOLD: f64 = 0.5;

/// 缓存计费参数。worker 启动时从 `system.cache` 段注入,之后由设置面板热调(30s 生效)。
///
/// 两个入口共用 [`CachePolicy::overlay`]:YAML 基线里的键名不带前缀(`read_multiplier`),
/// 热调设置里的键名带 `cache_` 前缀(`cache_read_multiplier`)。
#[derive(Debug, Clone, PartialEq)]
pub struct CachePolicy {
    pub read_multiplier: f64,
    pub cap_ratio: f64,
    pub floor_ratio: f64,
    pub strategy: CacheStrategy,
    /// 积分反推参数表(键 = Kiro 上游模型 id)。`Arc`:每请求快照一份,不拷表。
    pub credit_models: Arc<BTreeMap<String, CreditModelSpec>>,
    pub credit_max_output: u32,
    pub credit_discount_only: bool,
    /// 企业计价号也用积分反推(见 `gw_core::config::CacheConfig::credit_enterprise`)。
    pub credit_enterprise: bool,
    /// 输入输出口径修正(见模块文档)。
    pub io_correction: bool,
    /// 保底只对总输入 ≥ 本值的请求生效,更小的请求保底为 0。0 = 全部保底(旧行为)。
    /// 见 [`gw_core::config::CacheConfig::floor_min_input`]。
    pub floor_min_input: u32,
}

impl Default for CachePolicy {
    /// 与 `gw_core::config::CacheConfig::default()` 同源(同一组常量/默认表)。
    fn default() -> Self {
        Self {
            read_multiplier: DEFAULT_CACHE_READ_MULTIPLIER,
            cap_ratio: DEFAULT_CACHE_CAP_RATIO,
            floor_ratio: DEFAULT_CACHE_FLOOR_RATIO,
            strategy: CacheStrategy::default(),
            credit_models: Arc::new(gw_core::config::default_credit_models()),
            credit_max_output: 1500,
            credit_discount_only: false,
            credit_enterprise: true,
            io_correction: false,
            floor_min_input: 0,
        }
    }
}

impl CachePolicy {
    /// 从 provider 工厂的 `cfg`(worker 序列化的 `system.cache` 段)构造。
    /// 字段缺失/类型不符 → 各自保留默认,**不 fail**(计费参数容错优先于严格)。
    pub fn from_cfg(cfg: &serde_json::Value) -> Self {
        let mut p = Self::default();
        p.overlay(cfg, "");
        p
    }

    /// 用 `v` 里出现的字段覆盖当前值(部分更新)。`prefix`:键名前缀(基线 `""`,热调 `"cache_"`)。
    /// 字段存在但不是数字 → 打 warn 并保留旧值(不静默吞掉,否则面板显示已改、实际没生效)。
    pub fn overlay(&mut self, v: &serde_json::Value, prefix: &str) {
        overlay_field(v, prefix, "read_multiplier", &mut self.read_multiplier, |x| x.as_f64());
        overlay_field(v, prefix, "cap_ratio", &mut self.cap_ratio, |x| x.as_f64());
        overlay_field(v, prefix, "floor_ratio", &mut self.floor_ratio, |x| x.as_f64());
        overlay_field(v, prefix, "strategy", &mut self.strategy, |x| {
            serde_json::from_value::<CacheStrategy>(x.clone())
                .ok()
                .filter(|s| *s != CacheStrategy::Unknown)
        });
        overlay_field(v, prefix, "credit_models", &mut self.credit_models, |x| {
            let m: BTreeMap<String, CreditModelSpec> = serde_json::from_value(x.clone()).ok()?;
            match gw_core::config::validate_credit_models(&m) {
                Ok(()) => Some(Arc::new(m)),
                Err(e) => {
                    tracing::warn!(error = %e, "积分反推参数表不合法");
                    None
                }
            }
        });
        overlay_field(v, prefix, "credit_max_output", &mut self.credit_max_output, |x| {
            x.as_u64().and_then(|n| u32::try_from(n).ok())
        });
        overlay_field(v, prefix, "credit_discount_only", &mut self.credit_discount_only, |x| x.as_bool());
        overlay_field(v, prefix, "credit_enterprise", &mut self.credit_enterprise, |x| x.as_bool());
        overlay_field(v, prefix, "io_correction", &mut self.io_correction, |x| x.as_bool());
        overlay_field(v, prefix, "floor_min_input", &mut self.floor_min_input, |x| {
            x.as_u64().and_then(|n| u32::try_from(n).ok())
        });
    }
}

/// 字段存在且能解析 → 覆盖;存在但解析失败 → warn 并保留旧值;缺席/null → 不动。
fn overlay_field<T: std::fmt::Debug>(
    v: &serde_json::Value,
    prefix: &str,
    name: &str,
    slot: &mut T,
    parse: impl FnOnce(&serde_json::Value) -> Option<T>,
) {
    let key = format!("{prefix}{name}");
    match v.get(&key) {
        None | Some(serde_json::Value::Null) => {}
        Some(x) => match parse(x) {
            Some(val) => *slot = val,
            None => tracing::warn!(key = %key, value = %x, current = ?slot, "缓存计费设置不合法,保留当前值"),
        },
    }
}

/// 上游计量信号,由流任务的帧循环填写。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageSignals {
    /// tokenUsageEvent 的总输入(uncached + cacheRead);出现即为权威值,后到的覆盖先到的。
    pub token_usage_total: Option<i32>,
    /// contextUsageEvent 推算的总输入(pct × 窗口);只取第一次,tokenUsage 缺席时才用。
    pub context_usage_est: Option<i32>,
    /// tokenUsageEvent.cacheReadInputTokens(上游真实命中,诊断用,不参与计费)。
    pub real_cache_read: i64,
    /// meteringEvent.usage(本次真实积分消耗);`None` = 没收到计费帧。
    pub metering_credit: Option<f64>,
}

impl UsageSignals {
    pub fn record_token_usage(&mut self, uncached: i64, cache_read: i64) {
        let total = uncached + cache_read;
        if total > 0 {
            self.token_usage_total = Some(total.min(i32::MAX as i64) as i32);
        }
        self.real_cache_read = cache_read.max(0);
    }

    pub fn record_context_usage(&mut self, est: i32) {
        if est > 0 && self.context_usage_est.is_none() {
            self.context_usage_est = Some(est);
        }
    }

    pub fn record_metering(&mut self, credit: f64) {
        self.metering_credit = Some(credit);
    }

    /// 旧口径输入:tokenUsage 真值 > contextUsage 原值(含本轮输出)> 模拟器总量。
    pub fn legacy_input(&self, sim_total: i32) -> i32 {
        self.token_usage_total
            .or(self.context_usage_est)
            .unwrap_or(sim_total)
            .max(0)
    }

    /// 新口径输入:tokenUsage 真值 > contextUsage 估算减本轮输出 > 模拟器总量。
    ///
    /// contextUsage 在流末才到,是生成之后的上下文大小(2026-10-06 实测:让模型逐字复述
    /// 1 万字,上下文比只回 OK 多出的正好是复述的 token 数),所以要减掉本轮输出。
    /// tokenUsage 本身就是输入口径,不减;模拟器总量只数请求里的消息,也不减。
    pub fn billed_input(&self, sim_total: i32, output_tokens: i32) -> i32 {
        match (self.token_usage_total, self.context_usage_est) {
            (Some(t), _) => t.max(0),
            (None, Some(c)) => c.saturating_sub(output_tokens.max(0)).max(0),
            (None, None) => sim_total.max(0),
        }
    }
}

/// 本轮可见输出(推理摘要 + 正文 + 工具参数)的两种估算,由流任务收尾时给出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCounts {
    /// 新口径:按模型分词器折算(`OutputTally::tokens`)。积分反推恒用它。
    pub tokenized: i64,
    /// 旧口径:逐帧旧公式累加。`io_correction` 关闭时上报它。
    pub legacy: i64,
}

/// 计费所需的请求级上下文(chat_stream 组装,随流任务带到收尾)。
#[derive(Debug, Clone)]
pub struct BillingCtx {
    /// 本轮 prefix 模拟器结果(observe 在发包前完成)。
    pub sim: SimResult,
    /// 本请求开始时的计费参数快照(热调只影响之后的请求)。
    pub policy: CachePolicy,
    /// Kiro 上游模型 id(如 `claude-opus-5.5`),查积分参数表用。
    pub kiro_model: String,
    /// 企业计价号。`policy.credit_enterprise` 关时退回模拟器(旧行为:积分参数最初只在
    /// 个人号上标定);开(默认)时与个人号一样用积分反推。
    pub enterprise: bool,
}

/// 本次 cache_read 是怎么算出来的(诊断日志用)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    /// 策略就是 `sim`。
    Sim,
    /// 零产出,不计缓存。
    ZeroOutput,
    /// 积分策略生效;`h` = 反推的命中比例。
    Credit { h: f64 },
    /// 积分策略生效但被「只往便宜改」拦回模拟器的值。
    CreditKeptSim { h: f64 },
    /// 积分策略不可用,退回模拟器。
    Fallback(FallbackReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    NoMetering,
    UnknownModel,
    LongOutput,
    SmallInput,
    Enterprise,
}

/// 收尾产物:上报给客户端的拆分 + 落库用的诊断信号。
#[derive(Debug, Clone, PartialEq)]
pub struct BillingOutcome {
    /// 计费输入(含 cache_read,不含本轮输出);落库口径。发给客户端时
    /// input_tokens = 计费输入 − 缓存部分。
    pub input_total: i32,
    pub output_tokens: i32,
    pub cache_read: i32,
    pub cache_creation: i32,
    pub decision: Decision,
}

impl BillingOutcome {
    /// Anthropic `usage` 对象(message_delta 下发)。
    pub fn usage_json(&self) -> serde_json::Value {
        crate::usage::build_usage_json(
            self.input_total,
            self.output_tokens,
            self.cache_read,
            self.cache_creation,
        )
    }

    /// 落库用的结构化 usage(input_tokens 取总量,与 SSE 的拆分口径不同)。
    pub fn chat_usage(&self, signals: &UsageSignals) -> gw_core::provider::ChatUsage {
        gw_core::provider::ChatUsage {
            input_tokens: self.input_total.max(0) as u64,
            output_tokens: self.output_tokens.max(0) as u64,
            cache_read_tokens: self.cache_read.max(0) as u64,
            cache_creation_tokens: self.cache_creation.max(0) as u64,
            real_cache_read_tokens: signals.real_cache_read.max(0) as u64,
            metering_credit: signals.metering_credit.unwrap_or(0.0),
        }
    }
}

/// 计费收尾(纯函数)。`output`:本轮可见产出的两种估算;`produced_content`:是否交付过
/// 任何实质内容。
pub fn finalize(
    signals: &UsageSignals,
    ctx: &BillingCtx,
    output: OutputCounts,
    produced_content: bool,
) -> BillingOutcome {
    let to_i32 = |n: i64| n.clamp(0, i32::MAX as i64) as i32;
    let sim_total = ctx.sim.total_tokens.min(i32::MAX as u32) as i32;
    let tokenized = to_i32(output.tokenized);
    // 积分反推的口径(与参数表标定口径一致),与是否修正上报无关。
    let credit_basis = CreditBasis {
        input: signals.billed_input(sim_total, tokenized),
        output: tokenized,
    };
    let (input_total, output_tokens) = if ctx.policy.io_correction {
        (credit_basis.input, credit_basis.output)
    } else {
        (signals.legacy_input(sim_total), to_i32(output.legacy))
    };
    let (cache_read, decision) = if produced_content {
        report_cache_read(signals, ctx, input_total, credit_basis)
    } else {
        (0, Decision::ZeroOutput)
    };
    BillingOutcome {
        input_total,
        output_tokens,
        cache_read,
        cache_creation: 0,
        decision,
    }
}

/// 积分反推用的输入/输出(新口径)。
#[derive(Debug, Clone, Copy)]
struct CreditBasis {
    input: i32,
    output: i32,
}

/// 按策略算上报 cache_read(调用方已排除零产出)。比例乘到上报口径的 `input_total` 上。
fn report_cache_read(
    signals: &UsageSignals,
    ctx: &BillingCtx,
    input_total: i32,
    basis: CreditBasis,
) -> (i32, Decision) {
    let p = &ctx.policy;
    let sim = sim_reported(input_total, &ctx.sim, p);
    let fit_only = match p.strategy.resolved() {
        CacheStrategy::Sim | CacheStrategy::Unknown => return (sim, Decision::Sim),
        CacheStrategy::CreditCorrect => false,
        CacheStrategy::CreditFit => true,
    };
    let h = match credit_hit_ratio(signals, ctx, basis.input, basis.output) {
        Ok(h) => h,
        Err(reason) => return (sim, Decision::Fallback(reason)),
    };
    let ratio = if fit_only {
        h
    } else if h >= CREDIT_WARM_THRESHOLD {
        p.cap_ratio
    } else {
        effective_floor(p, input_total)
    };
    let reported = clamp_ratio(input_total, ratio, p);
    if p.credit_discount_only && reported < sim {
        return (sim, Decision::CreditKeptSim { h });
    }
    (reported, Decision::Credit { h })
}

/// 积分反推命中比例;不满足前提时给出退回原因。
fn credit_hit_ratio(
    signals: &UsageSignals,
    ctx: &BillingCtx,
    input_total: i32,
    output_tokens: i32,
) -> Result<f64, FallbackReason> {
    if ctx.enterprise && !ctx.policy.credit_enterprise {
        return Err(FallbackReason::Enterprise);
    }
    let credit = signals.metering_credit.ok_or(FallbackReason::NoMetering)?;
    hit_ratio_from_credit(&ctx.policy, &ctx.kiro_model, input_total, output_tokens, credit)
}

/// 积分反推命中比例的前提检查 + 反推(不含企业号开关)。计费收尾与 admin 日志页的
/// 「命中(积分反推)」诊断共用,保证两处口径一致。`input_total`/`output_tokens` 须是
/// 新口径(计费输入 = 上下文 − 本轮输出;输出按分词器折算),与参数表标定口径相同。
pub fn hit_ratio_from_credit(
    policy: &CachePolicy,
    kiro_model: &str,
    input_total: i32,
    output_tokens: i32,
    credit: f64,
) -> Result<f64, FallbackReason> {
    if !(credit.is_finite() && credit > 0.0) {
        return Err(FallbackReason::NoMetering);
    }
    let spec = policy
        .credit_models
        .get(kiro_model)
        .ok_or(FallbackReason::UnknownModel)?;
    if output_tokens as i64 > policy.credit_max_output as i64 {
        return Err(FallbackReason::LongOutput);
    }
    if input_total < CREDIT_MIN_INPUT_TOKENS {
        return Err(FallbackReason::SmallInput);
    }
    Ok(infer_hit_ratio(spec, input_total, output_tokens, credit))
}

/// 由积分反推命中比例(纯函数,结果夹到 [0,1]):
/// `h = (miss·I + out·O + base − 积分) / ((miss − hit)·I)`,I = 输入/1e5,O = 输出/1e3。
pub fn infer_hit_ratio(spec: &CreditModelSpec, input_total: i32, output_tokens: i32, credit: f64) -> f64 {
    let i = input_total as f64 / 1e5;
    let o = output_tokens as f64 / 1e3;
    let denom = (spec.miss - spec.hit) * i;
    if !(denom > 0.0) {
        return 0.0;
    }
    let h = (spec.miss * i + spec.out * o + spec.base - credit) / denom;
    if h.is_finite() {
        h.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// 本请求实际适用的保底比例:总输入低于 `floor_min_input` 的小请求不保底(0)。
///
/// 背景(2026-10-14 用户定):保底是给「长上下文会话冷启动」的让利;单轮小请求(典型是
/// 脚本化批量任务)本来就没有可复用的前缀,保底等于白送折扣,还把这类流量吸引过来。
fn effective_floor(p: &CachePolicy, input_total: i32) -> f64 {
    // 与 gw_core 的 CacheConfig 同为 u32;负的 input_total(不会出现)按 0 算。
    if p.floor_min_input > 0 && (input_total.max(0) as u32) < p.floor_min_input {
        0.0
    } else {
        p.floor_ratio
    }
}

/// 按比例上报,并用 floor/cap 夹限(参数非法时的回退同模拟器口径)。
fn clamp_ratio(input_total: i32, ratio: f64, p: &CachePolicy) -> i32 {
    let cap = if p.cap_ratio.is_finite() { p.cap_ratio } else { DEFAULT_CACHE_CAP_RATIO }.clamp(0.0, 1.0);
    let floor = effective_floor(p, input_total);
    let floor = if floor.is_finite() { floor } else { DEFAULT_CACHE_FLOOR_RATIO }.clamp(0.0, cap);
    let r = ratio.clamp(floor, cap);
    ((input_total.max(0) as f64 * r).round() as i32).clamp(0, input_total.max(0))
}

/// 模拟器口径的上报 cache_read。
fn sim_reported(input_total: i32, sim: &SimResult, p: &CachePolicy) -> i32 {
    reported_cache_read(
        input_total,
        sim.cache_read_tokens.min(i32::MAX as u32) as i32,
        sim.total_tokens.min(i32::MAX as u32) as i32,
        p.read_multiplier,
        p.cap_ratio,
        effective_floor(p, input_total),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(hit: u32, total: u32) -> BillingCtx {
        BillingCtx {
            sim: SimResult {
                cache_read_tokens: hit,
                total_tokens: total,
            },
            policy: CachePolicy {
                read_multiplier: 1.0,
                cap_ratio: 0.95,
                floor_ratio: 0.5,
                io_correction: true,
                ..CachePolicy::default()
            },
            kiro_model: "claude-opus-5.5".into(),
            enterprise: false,
        }
    }

    /// 两种口径给同一个数(多数用例只关心新口径)。
    fn oc(n: i64) -> OutputCounts {
        OutputCounts { tokenized: n, legacy: n }
    }

    fn with_strategy(mut c: BillingCtx, s: CacheStrategy) -> BillingCtx {
        c.policy.strategy = s;
        c
    }

    /// 生产真实请求的 (contextUsage 上下文, 积分),opus-5.5 个人号。计费输入 = 上下文 − 输出。
    fn signals(input: i32, credit: f64) -> UsageSignals {
        let mut s = UsageSignals::default();
        s.record_context_usage(input);
        s.record_metering(credit);
        s
    }

    #[test]
    fn billed_input_prefers_token_usage_then_context_minus_output_then_sim() {
        let mut s = UsageSignals::default();
        assert_eq!(s.billed_input(123, 40), 123, "无上游信号 → 模拟器总量,不减输出");
        s.record_context_usage(5000);
        assert_eq!(s.billed_input(123, 40), 4960, "contextUsage 含本轮输出,要减掉");
        assert_eq!(s.billed_input(123, 9000), 0, "减不出负数");
        s.record_context_usage(9999);
        assert_eq!(s.billed_input(123, 0), 5000, "contextUsage 只取第一次");
        s.record_token_usage(100, 7000);
        assert_eq!(s.billed_input(123, 40), 7100, "tokenUsage 覆盖 contextUsage,且本身是输入口径不减");
        s.record_token_usage(0, 0);
        assert_eq!(s.billed_input(123, 40), 7100, "零值 tokenUsage 不覆盖");
        s.record_token_usage(10, 20);
        assert_eq!(s.billed_input(123, 40), 30, "后到的 tokenUsage 覆盖先到的");
        assert_eq!(s.real_cache_read, 20);
    }

    #[test]
    fn metering_none_until_recorded_and_last_wins() {
        let mut s = UsageSignals::default();
        assert_eq!(s.metering_credit, None);
        s.record_metering(0.3);
        s.record_metering(1.5);
        assert_eq!(s.metering_credit, Some(1.5));
    }

    #[test]
    fn finalize_uses_sim_ratio_clamped_by_floor_and_cap() {
        let mut s = UsageSignals::default();
        s.record_context_usage(100_000);
        // 上下文 10 万含本轮输出 200 → 计费输入 99_800;模拟器命中 99% → 封顶 0.95
        let o = finalize(&s, &ctx(990, 1000), oc(200), true);
        assert_eq!((o.input_total, o.output_tokens, o.cache_read, o.cache_creation), (99_800, 200, 94_810, 0));
        // 模拟器全 miss → 保底 0.5
        let o = finalize(&s, &ctx(0, 1000), oc(200), true);
        assert_eq!(o.cache_read, 49_900);
    }

    #[test]
    fn finalize_zero_output_reports_no_cache_but_full_input() {
        let mut s = UsageSignals::default();
        s.record_context_usage(100_000);
        let o = finalize(&s, &ctx(990, 1000), oc(0), false);
        assert_eq!((o.input_total, o.cache_read), (100_000, 0));
    }

    #[test]
    fn finalize_falls_back_to_sim_total_without_upstream_counts() {
        let o = finalize(&UsageSignals::default(), &ctx(0, 1234), oc(5), true);
        assert_eq!(o.input_total, 1234);
        assert_eq!(o.cache_read, 617, "保底 0.5 × 1234 四舍五入");
    }

    #[test]
    fn outcome_json_and_chat_usage_keep_their_two_conventions() {
        let mut s = UsageSignals::default();
        s.record_token_usage(1000, 9000);
        s.record_metering(0.42);
        let o = finalize(&s, &ctx(990, 1000), oc(50), true);
        let j = o.usage_json();
        assert_eq!(j["input_tokens"], 500, "SSE 口径:只算未命中");
        assert_eq!(j["cache_read_input_tokens"], 9500);
        let u = o.chat_usage(&s);
        assert_eq!(u.input_tokens, 10_000, "落库口径:总量");
        assert_eq!(u.cache_read_tokens, 9500);
        assert_eq!(u.real_cache_read_tokens, 9000);
        assert_eq!(u.metering_credit, 0.42);
        assert_eq!(UsageSignals::default().metering_credit.unwrap_or(0.0), 0.0);
    }

    #[test]
    fn sim_strategy_ignores_metering() {
        // 136k 上下文、41 输出、0.551 积分:积分说是热的,但 sim 策略只看模拟器(全 miss → 保底)。
        let o = finalize(&signals(136_373, 0.551), &ctx(0, 1000), oc(41), true);
        assert_eq!((o.input_total, o.cache_read, o.decision), (136_332, 68_166, Decision::Sim));
    }

    #[test]
    fn io_correction_off_reports_legacy_but_credit_uses_new_basis() {
        // 136k 上下文、0.551 积分;同一段输出新口径 56、旧口径 41。
        let out = OutputCounts { tokenized: 56, legacy: 41 };
        // 关:上报与改动前一致(上下文原值、旧输出);sim 策略逐字节同旧版。
        let mut s = ctx(0, 1000);
        s.policy.io_correction = false;
        let o = finalize(&signals(136_373, 0.551), &s, out, true);
        assert_eq!((o.input_total, o.output_tokens, o.cache_read), (136_373, 41, 68_187));
        // 关 + 积分纠正:判冷热仍按新口径(上下文 − 新输出、新输出),比例乘到旧口径输入上。
        let mut c = with_strategy(s.clone(), CacheStrategy::CreditCorrect);
        let o = finalize(&signals(136_373, 0.551), &c, out, true);
        assert_eq!((o.input_total, o.output_tokens), (136_373, 41));
        assert_eq!(o.cache_read, (136_373_f64 * 0.95).round() as i32);
        assert!(matches!(o.decision, Decision::Credit { h } if h > 0.9), "{:?}", o.decision);
        // 开:上报新口径。
        c.policy.io_correction = true;
        let o = finalize(&signals(136_373, 0.551), &c, out, true);
        assert_eq!((o.input_total, o.output_tokens), (136_317, 56));
        assert_eq!(o.cache_read, (136_317_f64 * 0.95).round() as i32);
    }

    #[test]
    fn io_correction_defaults_off_and_overlays() {
        assert!(!CachePolicy::default().io_correction, "默认旧口径,上线不改变账单");
        let mut p = CachePolicy::default();
        p.overlay(&serde_json::json!({"cache_io_correction": true}), "cache_");
        assert!(p.io_correction);
        p.overlay(&serde_json::json!({"cache_io_correction": "yes"}), "cache_");
        assert!(p.io_correction, "非法值保留旧值");
        assert!(CachePolicy::from_cfg(&serde_json::json!({"io_correction": true})).io_correction);
    }

    #[test]
    fn credit_correct_flips_production_misjudgments_both_ways() {
        let c = with_strategy(ctx(0, 1000), CacheStrategy::CreditCorrect);
        // 2026-10-05 客户投诉的那类:积分很低(实际命中),模拟器报冷 → 纠正为封顶。
        let o = finalize(&signals(136_373, 0.551), &c, oc(41), true);
        assert_eq!(o.cache_read, ((136_373 - 41) as f64 * 0.95).round() as i32);
        assert!(matches!(o.decision, Decision::Credit { h } if h > 0.9), "{:?}", o.decision);
        // 反方向:模拟器报热,积分说冷(134k 输入、137 输出、1.732 积分) → 保底。
        let hot = with_strategy(ctx(990, 1000), CacheStrategy::CreditCorrect);
        let o = finalize(&signals(134_489, 1.732), &hot, oc(137), true);
        assert_eq!(o.cache_read, ((134_489 - 137) as f64 * 0.5).round() as i32);
        assert!(matches!(o.decision, Decision::Credit { h } if h < 0.1), "{:?}", o.decision);
    }

    #[test]
    fn credit_fit_reports_h_within_floor_and_cap() {
        let spec = CachePolicy::default().credit_models["claude-opus-5.5"];
        // 构造 h = 0.8 的积分。
        let (input, out): (i32, i32) = (100_000, 200);
        let credit = spec.miss * 0.2 + spec.hit * 0.8 + spec.out * 0.2 + spec.base;
        assert!((infer_hit_ratio(&spec, input, out, credit) - 0.8).abs() < 1e-9);
        let c = with_strategy(ctx(0, 1000), CacheStrategy::CreditFit);
        // 上下文 = 计费输入 + 本轮输出。
        let o = finalize(&signals(input + out, credit), &c, oc(out as i64), true);
        assert_eq!((o.input_total, o.cache_read), (100_000, 80_000));
        // h 超出夹限范围时贴边。
        let o = finalize(&signals(input + out, 5.0), &c, oc(out as i64), true);
        assert_eq!(o.cache_read, 50_000, "h=0 → 保底");
        let o = finalize(&signals(input + out, 0.01), &c, oc(out as i64), true);
        assert_eq!(o.cache_read, 95_000, "h=1 → 封顶");
    }

    #[test]
    fn credit_strategies_fall_back_to_sim_with_reason() {
        let c = with_strategy(ctx(0, 1000), CacheStrategy::CreditCorrect);
        let sim_floor = |input: i32| (input as f64 * 0.5).round() as i32;
        let mut no_meter = UsageSignals::default();
        no_meter.record_context_usage(136_373);
        let o = finalize(&no_meter, &c, oc(41), true);
        assert_eq!((o.cache_read, o.decision), (sim_floor(136_373 - 41), Decision::Fallback(FallbackReason::NoMetering)));
        let zero_meter = signals(136_373, 0.0);
        assert_eq!(finalize(&zero_meter, &c, oc(41), true).decision, Decision::Fallback(FallbackReason::NoMetering));
        let mut other = c.clone();
        other.kiro_model = "claude-opus-4.8".into();
        assert_eq!(finalize(&signals(136_373, 0.551), &other, oc(41), true).decision, Decision::Fallback(FallbackReason::UnknownModel));
        assert_eq!(finalize(&signals(136_373, 5.0), &c, oc(1501), true).decision, Decision::Fallback(FallbackReason::LongOutput));
        assert_eq!(finalize(&signals(999, 0.01), &c, oc(10), true).decision, Decision::Fallback(FallbackReason::SmallInput));
        let mut ent = c.clone();
        ent.enterprise = true;
        ent.policy.credit_enterprise = false;
        assert_eq!(finalize(&signals(136_373, 0.551), &ent, oc(41), true).decision, Decision::Fallback(FallbackReason::Enterprise));
        // 零产出优先于一切策略。
        assert_eq!(finalize(&signals(136_373, 0.551), &c, oc(0), false).decision, Decision::ZeroOutput);
    }

    #[test]
    fn discount_only_never_reports_less_cache_than_sim() {
        let mut c = with_strategy(ctx(990, 1000), CacheStrategy::CreditCorrect);
        c.policy.credit_discount_only = true;
        // 积分说冷、模拟器说热 → 维持模拟器(客户不多付)。
        let o = finalize(&signals(134_489, 1.732), &c, oc(137), true);
        assert_eq!(o.cache_read, ((134_489 - 137) as f64 * 0.95).round() as i32);
        assert!(matches!(o.decision, Decision::CreditKeptSim { .. }));
        // 积分说热、模拟器说冷 → 照常纠正。
        let mut cold = with_strategy(ctx(0, 1000), CacheStrategy::CreditCorrect);
        cold.policy.credit_discount_only = true;
        let o = finalize(&signals(136_373, 0.551), &cold, oc(41), true);
        assert_eq!(o.cache_read, ((136_373 - 41) as f64 * 0.95).round() as i32);
    }

    #[test]
    fn infer_hit_ratio_degenerate_inputs_are_safe() {
        let bad = CreditModelSpec { miss: 0.3, hit: 0.3, out: 0.1, base: 0.0 };
        assert_eq!(infer_hit_ratio(&bad, 100_000, 10, 0.1), 0.0, "分母为 0 → 按冷");
        let spec = CachePolicy::default().credit_models["claude-opus-5.5"];
        assert_eq!(infer_hit_ratio(&spec, 0, 10, 0.1), 0.0);
        assert_eq!(infer_hit_ratio(&spec, 100_000, 10, f64::NAN), 0.0);
    }

    #[test]
    fn policy_overlay_parses_strategy_fields_and_rejects_bad_values() {
        let mut p = CachePolicy::default();
        p.overlay(
            &serde_json::json!({
                "cache_strategy": "credit_fit",
                "cache_credit_max_output": 900,
                "cache_credit_discount_only": true,
                "cache_credit_models": {"claude-opus-5.5": {"miss": 1.0, "hit": 0.3, "out": 0.4}},
            }),
            "cache_",
        );
        assert_eq!(p.strategy, CacheStrategy::CreditFit);
        assert_eq!(p.credit_max_output, 900);
        assert!(p.credit_discount_only);
        assert_eq!(p.credit_models.len(), 1);
        // 非法值全部保留旧值:未知策略、hit ≥ miss 的表、负数上限。
        p.overlay(
            &serde_json::json!({
                "cache_strategy": "future",
                "cache_credit_models": {"claude-opus-5.5": {"miss": 0.3, "hit": 0.3, "out": 0.4}},
                "cache_credit_max_output": -1,
            }),
            "cache_",
        );
        assert_eq!(p.strategy, CacheStrategy::CreditFit);
        assert_eq!(p.credit_models["claude-opus-5.5"].miss, 1.0);
        assert_eq!(p.credit_max_output, 900);
        // 基线 cfg(无前缀)同一套解析。
        let b = CachePolicy::from_cfg(&serde_json::json!({"strategy": "credit_correct"}));
        assert_eq!(b.strategy, CacheStrategy::CreditCorrect);
    }

    #[test]
    fn policy_overlay_uses_prefix_and_keeps_value_on_bad_type() {
        let mut p = CachePolicy::from_cfg(&serde_json::json!({"read_multiplier": 2.5, "cap_ratio": 0.8}));
        assert_eq!((p.read_multiplier, p.cap_ratio, p.floor_ratio), (2.5, 0.8, DEFAULT_CACHE_FLOOR_RATIO));
        p.overlay(&serde_json::json!({"cache_floor_ratio": 0.4, "floor_ratio": 0.0, "cache_cap_ratio": "x"}), "cache_");
        assert_eq!(p.floor_ratio, 0.4, "热调只认带前缀的键");
        assert_eq!(p.cap_ratio, 0.8, "非数字保留旧值");
        let d = CachePolicy::from_cfg(&serde_json::Value::Null);
        assert_eq!(d, CachePolicy::default());
    }

    /// 2026-10-14:保底只给总输入 ≥ floor_min_input 的请求;更小的请求冷启动按实价(0 保底)。
    #[test]
    fn floor_applies_only_at_or_above_min_input() {
        let with_min = |mut c: BillingCtx, min: u32| {
            c.policy.floor_min_input = min;
            c
        };
        // sim 策略,模拟器全 miss:小请求保底 0,大请求仍 0.5。
        let mut small = UsageSignals::default();
        small.record_context_usage(10_200); // 计费输入 10_000
        let o = finalize(&small, &with_min(ctx(0, 1000), 100_000), oc(200), true);
        assert_eq!((o.input_total, o.cache_read), (10_000, 0), "小请求不保底");
        let mut big = UsageSignals::default();
        big.record_context_usage(150_200); // 计费输入 150_000
        let o = finalize(&big, &with_min(ctx(0, 1000), 100_000), oc(200), true);
        assert_eq!(o.cache_read, 75_000, "≥ 阈值照旧保底 0.5");
        // 热的照旧封顶,不受阈值影响。
        let o = finalize(&small, &with_min(ctx(990, 1000), 100_000), oc(200), true);
        assert_eq!(o.cache_read, 9_500);
        // 阈值 0 = 旧行为:小请求也保底。
        let o = finalize(&small, &with_min(ctx(0, 1000), 0), oc(200), true);
        assert_eq!(o.cache_read, 5_000);
    }

    #[test]
    fn credit_correct_cold_small_request_gets_no_floor() {
        // 积分判冷(h < 0.5)→ 原本取 floor 0.5;阈值 100k 下的 2 万上下文请求取 0。
        let mut c = with_strategy(ctx(0, 1000), CacheStrategy::CreditCorrect);
        c.policy.floor_min_input = 100_000;
        // 2 万输入、50 输出、积分按全 miss 给高(反推 h≈0)。
        let o = finalize(&signals(20_050, 5.0), &c, oc(50), true);
        assert!(matches!(o.decision, Decision::Credit { .. }), "走积分反推: {:?}", o.decision);
        assert_eq!(o.cache_read, 0, "判冷的小请求不保底");
        // 同样判冷的大请求仍保底。
        let o = finalize(&signals(200_050, 50.0), &c, oc(50), true);
        assert!(matches!(o.decision, Decision::Credit { .. }));
        assert_eq!(o.cache_read, 100_000, "判冷的大请求保底 0.5");
    }

    #[test]
    fn floor_min_input_overlays_from_hot_settings() {
        let mut p = CachePolicy::default();
        assert_eq!(p.floor_min_input, 0, "默认 0 = 全部保底(旧行为)");
        p.overlay(&serde_json::json!({"cache_floor_min_input": 100000}), "cache_");
        assert_eq!(p.floor_min_input, 100_000);
        p.overlay(&serde_json::json!({"cache_floor_min_input": -1}), "cache_");
        assert_eq!(p.floor_min_input, 100_000, "非法值保留旧值");
        let b = CachePolicy::from_cfg(&serde_json::json!({"floor_min_input": 5000}));
        assert_eq!(b.floor_min_input, 5000, "yaml 基线无前缀");
    }

    #[test]
    fn default_policy_matches_core_cache_config_default() {
        let c = gw_core::config::CacheConfig::default();
        let p = CachePolicy::default();
        assert_eq!((p.read_multiplier, p.cap_ratio, p.floor_ratio), (c.read_multiplier, c.cap_ratio, c.floor_ratio));
        assert_eq!(p.credit_enterprise, c.credit_enterprise);
    }

    #[test]
    fn enterprise_uses_credit_inference_by_default() {
        // 2026-10-08 生产真实请求:IdC 企业号 opus-5.5,计费输入 961,466、输出 166、积分 3.2558。
        // 模拟器判冷、按下限报 50%;按积分反推 h≈1(全冷应扣约 11.8)→ 报封顶 95%。
        let mut ent = with_strategy(ctx(0, 1000), CacheStrategy::CreditCorrect);
        ent.enterprise = true;
        assert!(ent.policy.credit_enterprise, "默认开");
        let sig = signals(961_466 + 166, 3.255_764_121_393_035_3);
        let o = finalize(&sig, &ent, oc(166), true);
        assert!(matches!(o.decision, Decision::Credit { h } if h > 0.99), "{:?}", o.decision);
        assert_eq!(o.cache_read, (961_466f64 * 0.95).round() as i32);

        // 热调关掉 → 回到旧行为:企业号退回模拟器(此处模拟器判冷 → 下限 50%)。
        let mut off = ent.clone();
        off.policy.overlay(&serde_json::json!({"cache_credit_enterprise": false}), "cache_");
        let o = finalize(&sig, &off, oc(166), true);
        assert_eq!(o.decision, Decision::Fallback(FallbackReason::Enterprise));
        assert_eq!(o.cache_read, (961_466f64 * 0.5).round() as i32);
    }
}
