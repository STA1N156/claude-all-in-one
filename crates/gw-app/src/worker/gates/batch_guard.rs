//! 批量任务拦截:同一提示词模板的单轮请求短时间内大量涌入 → 判定为脚本化批量任务并拒绝。
//!
//! # 为什么要拦
//!
//! Kiro 对「脚本化批量处理」极度敏感(用户多次因此成批封号)。2026-10-13 的实例:一个
//! VFX 审图任务(system 固定开头、单轮、非流式、每条 1~4 张图)48 分钟打了 770 条,分散到
//! 29 个号上;约半小时后 40 分钟内 13 个号被封、3 个号被降级。按单号用量比较区分不出
//! 被封号与幸存号 —— Kiro 像是把「跑同一个自动化任务的一组号」成批处理,所以**限单号
//! 速率没用,只能在入口把这类任务整体挡掉**。
//!
//! # 判据
//!
//! - **模板** = 提示词开头:system 里第一个非计费头的文本块(剥掉 CC 放在开头的
//!   `x-anthropic-billing-header:` 行);没有 system 时取第一条 user 消息的开头。先把
//!   ≥4 位且含数字的十六进制/数字串(随机标签、编号、日期)归一成 `#`,再取前
//!   `prefix_chars` 个字符 —— 有的脚本每条都带随机标签(`[session e09b26bff4be] …`),
//!   不归一就会被拆成无数个模板。
//! - 只给**脚本形态**的请求计数:单轮(`messages.len() == 1`)且**不带 tools**。
//!   通用客户端(CC / OpenCode / Agent SDK …)所有用户共用同一句开头,但主对话是多轮、
//!   子代理带 tools,都不计数 —— 一个用户一口气开几百个子代理也触发不了。
//! - 判定要同时满足两条:窗口内脚本形态请求达到 `threshold`;并且在这 `threshold` 条的
//!   时间跨度里,同模板的全部请求中脚本形态占 ≥ 90%([`SCRIPTED_SHARE_NUM`])。通用客户端
//!   偶尔发的无 tools 单轮辅助请求(起标题、摘要)会被它自己的大量对话流量稀释。
//!   生产回放(10-13 晚两段共约 2.5 小时):批量任务 273 条/10 分钟、占比 100%;编程客户端
//!   单轮占比 0~3%;其余任何模板脚本形态 ≤ 20 条/10 分钟。
//! - 判定后该模板进入「已判定」表 `block_secs` 秒,期间只有**脚本形态**的请求按模式处理,
//!   多轮和带 tools 的照常放行 —— 万一误判,也掐不断正在进行的对话。
//! - `block_prefixes`:命中即判定,**不看形态**(运营手动拉黑,意图明确);`allow_prefixes`:
//!   命中即放行。CC 自动模式安全分类器的 system 前缀**内置豁免** —— 它也是单轮 + 固定前缀
//!   + 量大,平时由 [`super::automode`] 本地直答,但那个开关切到 off/haiku 时会流到这里。
//!
//! # 模式
//!
//! - `observe`(代码默认):照常判定、记日志、进已判定表,但**不拦**。上线先看误判。
//! - `enforce`:拒绝(调用方回 400 —— 下游 new-api 对 400 既不重试也不自动禁用渠道;
//!   文案刻意避开它的禁用关键词,见 [`REJECT_MESSAGE`])。
//! - `off`:完全不看、不计数。
//!
//! 状态纯内存、单进程(一个 worker 服务一个池)。热切换配置**不清状态**:observe 期间判定的
//! 指纹切到 enforce 后立即生效,这正是「先观察、再拦」想要的。重启 = 清空。

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};

use gw_core::config::batch_guard::*;
use gw_core::normalize::strip_leading_billing_lines;

use super::automode::SYSTEM_PREFIX as CLASSIFIER_PREFIX;

/// 被拦请求在请求日志 `error_kind` 列里的标记。**改这个串要同步改前端**
/// `admin-ui/src/features/logs/errorKind.ts` 的 `BATCH_BLOCKED_KIND`。
pub const LOG_KIND_BLOCKED: &str = "BatchBlocked";

/// 拒绝文案。下游 new-api 按关键词自动禁用渠道(`Permission denied` / `Operation not allowed`
/// / `Your account is not authorized` …),这里一个都不能出现,否则一次拦截会把整条渠道下线。
pub const REJECT_MESSAGE: &str =
    "检测到批量任务(同一提示词模板短时间内大量单轮请求),本渠道不支持批量处理,请改用其它渠道";

/// 指纹表上限。超出时淘汰最久没出现的模板(内存上界 ≈ 4096 × 2 × threshold × 16B)。
const MAX_TRACKED: usize = 4096;
/// 已判定表上限。满了先清过期项,仍满则挤掉最早到期的那条(防大量不同模板撑爆内存)。
const MAX_FLAGGED: usize = 512;
/// `/health` 里最多列多少条已判定模板(总数另报)。
const SNAPSHOT_FLAGGED: usize = 50;
/// 展示用的模板开头长度(日志 / 面板)。
const DISPLAY_CHARS: usize = 40;
/// 判定所需的脚本形态占比 = NUM / DEN(90%)。写成整数比,判定时不碰浮点。
/// 对话流量的计数队列上限取 `threshold`,前提是占比 ≥ 50%(见 [`decide`])。
const SCRIPTED_SHARE_NUM: u64 = 9;
const SCRIPTED_SHARE_DEN: u64 = 10;
/// 模板归一:纯数字串不论长短都换成 `#`;字母数字混合的十六进制串至少这么长才换。
const MIN_ID_RUN: usize = 4;

static CONFIG: LazyLock<RwLock<BatchGuardConfig>> =
    LazyLock::new(|| RwLock::new(BatchGuardConfig::default()));
static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));

/// 热应用。非法 → Err 且保留上一份有效值。
///
/// 状态一般**保留**(observe → enforce 时已判定的模板立即生效);以下切换会作废一部分
/// (累计数不清,见 [`reset_scope`]):
/// - 切到 `off`:关掉就该是干净的,重新打开从零开始,不延续关闭前的半截计数;
/// - `prefix_chars` 变了:指纹口径变了,旧指纹对不上新请求,留着只会占内存、在面板上误导;
/// - `threshold` / `window_secs` 变了:只清计数表 —— 队列是按旧阈值截断、按旧窗口清理的,
///   拿它按新参数算占比会误判;已判定的模板保留。
pub fn set_config(cfg: &BatchGuardConfig) -> Result<(), String> {
    validate(cfg)?;
    let mut c = cfg.clone();
    for p in c.block_prefixes.iter_mut().chain(c.allow_prefixes.iter_mut()) {
        *p = p.trim().to_string();
    }
    let mut guard = CONFIG.write().unwrap_or_else(|p| p.into_inner());
    let reset = reset_scope(&guard, &c);
    if reset.tracked || reset.flagged {
        let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
        st.tracked.clear();
        if reset.flagged {
            st.flagged.clear();
        }
    }
    *guard = c;
    Ok(())
}

/// 换配置时要作废的状态(见 [`set_config`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reset {
    tracked: bool,
    flagged: bool,
}

fn reset_scope(old: &BatchGuardConfig, new: &BatchGuardConfig) -> Reset {
    let all = (new.mode == BatchGuardMode::Off && old.mode != BatchGuardMode::Off) || new.prefix_chars != old.prefix_chars;
    let counts = new.threshold != old.threshold || new.window_secs != old.window_secs;
    Reset { tracked: all || counts, flagged: all }
}

pub fn current() -> BatchGuardConfig {
    CONFIG.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// 为什么命中。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// 命中 `block_prefixes`。
    Manual,
    /// 窗口内脚本形态请求数达到阈值且占比达标(本次或之前)。
    Rate,
}

/// 对一条请求的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// 判定为批量任务。`block` = 是否真的拦(enforce);observe 下为 false。
    Batch {
        block: bool,
        reason: Reason,
        /// 模板开头(展示用,前 [`DISPLAY_CHARS`] 字符)。
        head: String,
        /// 判定时窗口内的脚本形态请求数(手动拉黑为 0)。
        count: u32,
        /// 是不是**这一条**触发的判定(调用方据此只打一次 warn)。
        newly_flagged: bool,
    },
}

#[derive(Default)]
struct State {
    tracked: HashMap<[u8; 16], Track>,
    flagged: HashMap<[u8; 16], Flag>,
    batch_total: u64,
    blocked_total: u64,
}

struct Track {
    /// 窗口内脚本形态请求的时间(最多保留 threshold 个,够判定即可)。
    hits: VecDeque<Instant>,
    /// 窗口内同模板非脚本形态(多轮 / 带 tools)请求的时间,用来算占比。只在模板已建表后
    /// 才记 —— 判定只看 `hits` 跨度内的,建表前的必然落在跨度外。最多保留 threshold 个。
    others: VecDeque<Instant>,
    last_seen: Instant,
}

struct Flag {
    until: Instant,
    head: String,
    reason: Reason,
    count: u32,
    /// 判定后又来了多少条(含被拦的)。
    since: u64,
}

/// 模板开头:system 第一个非计费头文本块,退回第一条 user 消息。都没有 → None(不计)。
pub(crate) fn prompt_head(body: &serde_json::Value) -> Option<&str> {
    let from_system = match body.get("system") {
        Some(serde_json::Value::String(s)) => Some(strip_leading_billing_lines(s)).filter(|s| !s.is_empty()),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .map(strip_leading_billing_lines)
            .find(|s| !s.is_empty()),
        _ => None,
    };
    if from_system.is_some() {
        return from_system;
    }
    let first = body.get("messages")?.as_array()?.first()?;
    if first.get("role").and_then(|r| r.as_str()) != Some("user") {
        return None;
    }
    let text = match first.get("content")? {
        serde_json::Value::String(s) => s.as_str(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .find(|s| !s.trim().is_empty())?,
        _ => return None,
    };
    Some(text.trim_start()).filter(|s| !s.is_empty())
}

fn take_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// 模板归一 + 截断:一段连续的十六进制字符,纯数字(编号、日期)不论长短、或含数字且
/// ≥ [`MIN_ID_RUN`] 位(随机标签、哈希),换成一个 `#`;不含数字的(`decade`、`deadbeef`
/// 这类单词)和短混合串(`a1`)原样保留。
/// 凑够 `max_chars` 个字符就停(system 可能几十 KB);只有正在扫的那段十六进制要整段数完
/// 才知道归不归一,但只缓存可能原样输出的那几位。
fn template_key(head: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut n = 0usize;
    let mut chars = head.chars().peekable();
    while n < max_chars {
        let Some(c) = chars.next() else { break };
        if !c.is_ascii_hexdigit() {
            out.push(c);
            n += 1;
            continue;
        }
        let room = max_chars - n;
        let mut kept = String::from(c);
        let (mut len, mut any_digit, mut all_digits) = (1usize, c.is_ascii_digit(), c.is_ascii_digit());
        while let Some(d) = chars.next_if(|d| d.is_ascii_hexdigit()) {
            len += 1;
            any_digit |= d.is_ascii_digit();
            all_digits &= d.is_ascii_digit();
            if kept.len() < room {
                kept.push(d);
            }
        }
        if all_digits || (any_digit && len >= MIN_ID_RUN) {
            out.push('#');
            n += 1;
        } else {
            out.push_str(&kept);
            n += kept.len();
        }
    }
    out
}

fn fingerprint(key: &str) -> [u8; 16] {
    let digest = Sha256::digest(key.as_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// 手动规则在已判定表里的键:与请求指纹分属不同键空间(前缀 NUL 隔开)。
fn manual_key(rule: &str) -> [u8; 16] {
    fingerprint(&format!("\0manual\0{rule}"))
}

/// 脚本形态:单轮且不带 tools。通用客户端的对话是多轮的、子代理带 tools。
fn is_scripted(body: &serde_json::Value) -> bool {
    let single_turn = body.get("messages").and_then(|m| m.as_array()).is_some_and(|m| m.len() == 1);
    let has_tools = body.get("tools").and_then(|t| t.as_array()).is_some_and(|t| !t.is_empty());
    single_turn && !has_tools
}

/// 对一条请求做判定(并记账)。`now` 由调用方注入,测试可控。
///
/// 解析请求体、算指纹都在**锁外**做;全局锁只包住计数表的增删(微秒级)。
pub fn check(body: &serde_json::Value, now: Instant) -> Verdict {
    let cfg = current();
    let Some(probe) = probe(&cfg, body) else {
        return Verdict::Pass;
    };
    decide(&cfg, &mut STATE.lock().unwrap_or_else(|p| p.into_inner()), probe, now)
}

/// 一条请求里判定要用的东西(锁外算好)。
struct Probe {
    fp: [u8; 16],
    display: String,
    /// 命中的手动规则(原文)。
    manual: Option<String>,
    scripted: bool,
}

/// 关闭 / 认不出模板 / 豁免 → None(直接放行,不计数)。
/// 手动名单按**原文**开头匹配(运营从日志里复制的就是原文),指纹按归一后的模板算。
fn probe(cfg: &BatchGuardConfig, body: &serde_json::Value) -> Option<Probe> {
    if cfg.mode == BatchGuardMode::Off {
        return None;
    }
    let head = prompt_head(body)?;
    if head.starts_with(CLASSIFIER_PREFIX) || cfg.allow_prefixes.iter().any(|p| head.starts_with(p.as_str())) {
        return None;
    }
    let key = template_key(head, cfg.prefix_chars);
    Some(Probe {
        fp: fingerprint(&key),
        display: take_chars(&key, DISPLAY_CHARS).to_string(),
        manual: cfg.block_prefixes.iter().find(|p| head.starts_with(p.as_str())).cloned(),
        scripted: is_scripted(body),
    })
}

#[cfg(test)]
fn check_with(cfg: &BatchGuardConfig, st: &mut State, body: &serde_json::Value, now: Instant) -> Verdict {
    match probe(cfg, body) {
        Some(p) => decide(cfg, st, p, now),
        None => Verdict::Pass,
    }
}

fn decide(cfg: &BatchGuardConfig, st: &mut State, probe: Probe, now: Instant) -> Verdict {
    let Probe { fp, display, manual, scripted } = probe;
    let block = cfg.mode == BatchGuardMode::Enforce;
    // 已判定表只在被访问到时才删过期项;低流量时攒着的过期项在这里顺手清一次。
    if st.flagged.len() > 64 {
        st.flagged.retain(|_, f| f.until > now);
    }

    if let Some(rule) = manual {
        // 已判定表里按**规则**记一条,不用请求的归一化指纹 —— 否则归一后撞指纹、原文却
        // 不匹配规则的请求会被这条记录连带拦掉。手动拉黑每次都按原文重新匹配;记录只给
        // 面板展示「哪条规则在命中、最近一次命中后还会挂多久」。
        let key = manual_key(&rule);
        let newly = !st.flagged.contains_key(&key);
        if newly {
            st.make_room(now);
        }
        let f = st.flagged.entry(key).or_insert_with(|| Flag {
            until: now,
            head: take_chars(&rule, DISPLAY_CHARS).to_string(),
            reason: Reason::Manual,
            count: 0,
            since: 0,
        });
        f.until = now + Duration::from_secs(cfg.block_secs);
        return st.hit(key, block, display, Reason::Manual, 0, newly);
    }

    if let Some(f) = st.flagged.get(&fp) {
        if f.until > now {
            // 自动判定只管脚本形态;多轮、带 tools 的放行(也不算「判定后又来」)。
            if !scripted {
                return Verdict::Pass;
            }
            let (reason, count) = (f.reason, f.count);
            return st.hit(fp, block, display, reason, count, false);
        }
        st.flagged.remove(&fp);
    }

    let window = Duration::from_secs(cfg.window_secs);
    let threshold = cfg.threshold.max(2) as usize;
    if !scripted {
        // 只给已建表的模板记对话流量(算占比用);没建表的不建 —— 通用客户端的海量对话
        // 不该占计数表。
        if let Some(t) = st.tracked.get_mut(&fp) {
            t.last_seen = now;
            push_bounded(&mut t.others, now, window, threshold);
        }
        return Verdict::Pass;
    }
    if !st.tracked.contains_key(&fp) && st.tracked.len() >= MAX_TRACKED {
        st.evict_oldest();
    }
    let t = st
        .tracked
        .entry(fp)
        .or_insert_with(|| Track { hits: VecDeque::new(), others: VecDeque::new(), last_seen: now });
    t.last_seen = now;
    push_bounded(&mut t.hits, now, window, threshold);
    if t.hits.len() < threshold {
        return Verdict::Pass;
    }
    // 占比只看最近 threshold 条脚本请求的时间跨度内。对话流量队列也只留 threshold 个:
    // 占比门槛 ≥ 50% 时,跨度内对话流量超过 threshold 个必然不达标,数满即可否定。
    let span_start = t.hits.front().copied().unwrap_or(now);
    while t.others.front().is_some_and(|&x| now.saturating_duration_since(x) >= window) {
        t.others.pop_front();
    }
    let others = t.others.iter().filter(|&&x| x >= span_start).count() as u64;
    let scripted_n = t.hits.len() as u64;
    if scripted_n * (SCRIPTED_SHARE_DEN - SCRIPTED_SHARE_NUM) < others * SCRIPTED_SHARE_NUM {
        return Verdict::Pass;
    }
    let count = t.hits.len() as u32;
    st.tracked.remove(&fp);
    st.make_room(now);
    st.flagged.insert(
        fp,
        Flag {
            until: now + Duration::from_secs(cfg.block_secs),
            head: display.clone(),
            reason: Reason::Rate,
            count,
            since: 0,
        },
    );
    st.hit(fp, block, display, Reason::Rate, count, true)
}

/// 记一个时间点:先丢掉滑出窗口的,再追加,最后只留最近 `cap` 个。
fn push_bounded(q: &mut VecDeque<Instant>, now: Instant, window: Duration, cap: usize) {
    while q.front().is_some_and(|&x| now.saturating_duration_since(x) >= window) {
        q.pop_front();
    }
    q.push_back(now);
    while q.len() > cap {
        q.pop_front();
    }
}

impl State {
    fn hit(&mut self, fp: [u8; 16], block: bool, head: String, reason: Reason, count: u32, newly: bool) -> Verdict {
        self.batch_total += 1;
        if block {
            self.blocked_total += 1;
        }
        if !newly {
            if let Some(f) = self.flagged.get_mut(&fp) {
                f.since += 1;
            }
        }
        Verdict::Batch { block, reason, head, count, newly_flagged: newly }
    }

    /// 已判定表满了:先清过期项,仍满则挤掉最早到期的一条。
    fn make_room(&mut self, now: Instant) {
        if self.flagged.len() < MAX_FLAGGED {
            return;
        }
        self.flagged.retain(|_, f| f.until > now);
        while self.flagged.len() >= MAX_FLAGGED {
            match self.flagged.iter().min_by_key(|(_, f)| f.until).map(|(k, _)| *k) {
                Some(k) => {
                    self.flagged.remove(&k);
                }
                None => break,
            }
        }
    }

    fn evict_oldest(&mut self) {
        if let Some(k) = self.tracked.iter().min_by_key(|(_, t)| t.last_seen).map(|(k, _)| *k) {
            self.tracked.remove(&k);
        }
    }
}

/// `/health` 里的拦截实况。
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub mode: &'static str,
    pub window_secs: u64,
    pub threshold: u32,
    /// 判定为批量的请求累计数(observe 下也算)。
    pub batch_total: u64,
    /// 真正拒绝的累计数(enforce)。
    pub blocked_total: u64,
    /// 正在计数的模板数。
    pub tracked: usize,
    /// 当前有效的已判定模板总数。
    pub flagged_total: usize,
    /// 当前有效的已判定模板,按剩余时间倒序,最多 [`SNAPSHOT_FLAGGED`] 条。
    pub flagged: Vec<FlaggedView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FlaggedView {
    pub head: String,
    pub reason: Reason,
    pub count: u32,
    pub since: u64,
    pub remaining_secs: u64,
}

pub fn snapshot(now: Instant) -> Snapshot {
    let cfg = current();
    let st = STATE.lock().unwrap_or_else(|p| p.into_inner());
    snapshot_with(&cfg, &st, now)
}

fn snapshot_with(cfg: &BatchGuardConfig, st: &State, now: Instant) -> Snapshot {
    let mut flagged: Vec<FlaggedView> = st
        .flagged
        .values()
        .filter(|f| f.until > now)
        .map(|f| FlaggedView {
            head: f.head.clone(),
            reason: f.reason,
            count: f.count,
            since: f.since,
            remaining_secs: f.until.saturating_duration_since(now).as_secs(),
        })
        .collect();
    flagged.sort_by(|a, b| b.remaining_secs.cmp(&a.remaining_secs));
    let flagged_total = flagged.len();
    flagged.truncate(SNAPSHOT_FLAGGED);
    Snapshot {
        mode: mode_str(cfg.mode),
        window_secs: cfg.window_secs,
        threshold: cfg.threshold,
        batch_total: st.batch_total,
        blocked_total: st.blocked_total,
        tracked: st.tracked.len(),
        flagged_total,
        flagged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const VFX: &str = "You are a strict, calibrated senior VFX art director reviewing generated frames.";

    fn single(system: &str) -> serde_json::Value {
        json!({"model": "claude-sonnet-5", "system": system, "messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "AAAA"}},
            {"type": "text", "text": "review this shot"}]}]})
    }

    fn cfg(mode: BatchGuardMode) -> BatchGuardConfig {
        BatchGuardConfig { mode, threshold: 5, window_secs: 600, ..BatchGuardConfig::default() }
    }

    fn run(c: &BatchGuardConfig, st: &mut State, body: &serde_json::Value, t: Instant, n: usize) -> Vec<Verdict> {
        (0..n).map(|i| check_with(c, st, body, t + Duration::from_secs(i as u64))).collect()
    }

    #[test]
    fn flags_exactly_at_threshold_and_blocks_in_enforce() {
        let c = cfg(BatchGuardMode::Enforce);
        let mut st = State::default();
        let t = Instant::now();
        let v = run(&c, &mut st, &single(VFX), t, 6);
        assert!(v[..4].iter().all(|x| *x == Verdict::Pass), "阈值 5:前 4 条放行");
        match &v[4] {
            Verdict::Batch { block, reason, count, newly_flagged, head } => {
                assert!(*block);
                assert_eq!(*reason, Reason::Rate);
                assert_eq!(*count, 5);
                assert!(*newly_flagged, "第 5 条触发判定");
                assert!(head.starts_with("You are a strict"));
            }
            other => panic!("第 5 条应判定,得到 {other:?}"),
        }
        assert!(matches!(&v[5], Verdict::Batch { block: true, newly_flagged: false, .. }), "之后持续拦");
        assert_eq!((st.batch_total, st.blocked_total), (2, 2));
    }

    #[test]
    fn observe_flags_but_never_blocks_then_enforce_takes_over() {
        let mut st = State::default();
        let t = Instant::now();
        let v = run(&cfg(BatchGuardMode::Observe), &mut st, &single(VFX), t, 5);
        assert!(matches!(&v[4], Verdict::Batch { block: false, newly_flagged: true, .. }));
        assert_eq!(st.blocked_total, 0, "observe 不拦");
        // 热切到 enforce:状态保留,下一条立刻被拦。
        let v2 = check_with(&cfg(BatchGuardMode::Enforce), &mut st, &single(VFX), t + Duration::from_secs(10));
        assert!(matches!(v2, Verdict::Batch { block: true, newly_flagged: false, .. }));
    }

    #[test]
    fn hits_outside_window_do_not_count() {
        let c = cfg(BatchGuardMode::Enforce);
        let mut st = State::default();
        let t = Instant::now();
        // 每 200 秒一条,窗口 600 秒内最多 3 条,永远到不了 5。
        for i in 0..20u64 {
            assert_eq!(check_with(&c, &mut st, &single(VFX), t + Duration::from_secs(i * 200)), Verdict::Pass);
        }
    }

    #[test]
    fn flag_expires_after_block_secs() {
        let c = BatchGuardConfig { block_secs: 3600, ..cfg(BatchGuardMode::Enforce) };
        let mut st = State::default();
        let t = Instant::now();
        run(&c, &mut st, &single(VFX), t, 5);
        let later = t + Duration::from_secs(3600 + 10);
        assert_eq!(check_with(&c, &mut st, &single(VFX), later), Verdict::Pass, "到期后重新从 0 计数");
        assert!(snapshot_with(&c, &st, later).flagged.is_empty());
    }

    fn multi(system: &str) -> serde_json::Value {
        json!({"system": system, "messages": [
            {"role": "user", "content": "a"}, {"role": "assistant", "content": "b"}, {"role": "user", "content": "c"}]})
    }

    /// 单轮但带 tools:CC 子代理的第一条请求就是这个形态。
    fn single_with_tools(system: &str) -> serde_json::Value {
        json!({"system": system, "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": "explore the repo"}]})
    }

    /// 测试用的「各不相同、且不含数字」的串(数字串会被模板归一抹平)。
    fn tag(i: usize) -> String {
        format!("{i:08}").bytes().map(|b| (b'g' + (b - b'0')) as char).collect()
    }

    #[test]
    fn multi_turn_and_tool_requests_never_count_and_pass_once_flagged() {
        let c = cfg(BatchGuardMode::Enforce);
        let mut st = State::default();
        let t = Instant::now();
        for i in 0..50u64 {
            let at = t + Duration::from_secs(i);
            assert_eq!(check_with(&c, &mut st, &multi(VFX), at), Verdict::Pass, "多轮不计数");
            assert_eq!(check_with(&c, &mut st, &single_with_tools(VFX), at), Verdict::Pass, "带 tools 不计数");
        }
        assert!(st.tracked.is_empty());
        // 纯脚本形态 → 判定;判定后多轮、带 tools 的照常放行,只拦脚本形态。
        let v = run(&c, &mut st, &single(VFX), t + Duration::from_secs(60), 5);
        assert!(matches!(v[4], Verdict::Batch { newly_flagged: true, .. }));
        let after = t + Duration::from_secs(70);
        assert_eq!(check_with(&c, &mut st, &multi(VFX), after), Verdict::Pass, "自动判定不拦多轮");
        assert_eq!(check_with(&c, &mut st, &single_with_tools(VFX), after), Verdict::Pass, "自动判定不拦带 tools");
        assert!(matches!(check_with(&c, &mut st, &single(VFX), after), Verdict::Batch { block: true, .. }));
        let snap = snapshot_with(&c, &st, after);
        assert_eq!(snap.flagged[0].since, 1, "放行的请求不算「判定后又来」");
    }

    #[test]
    fn manual_block_covers_every_shape() {
        let c = BatchGuardConfig { block_prefixes: vec!["You are a strict".into()], ..cfg(BatchGuardMode::Enforce) };
        let mut st = State::default();
        let t = Instant::now();
        for body in [single(VFX), multi(VFX), single_with_tools(VFX)] {
            assert!(
                matches!(check_with(&c, &mut st, &body, t), Verdict::Batch { block: true, reason: Reason::Manual, .. }),
                "手动拉黑是运营明确意图,不看形态"
            );
        }
    }

    #[test]
    fn manual_rule_does_not_spread_to_normalized_siblings() {
        // 手动规则按原文匹配;归一后与它同指纹、但原文不匹配的请求不能被连带拦掉。
        let c = BatchGuardConfig { block_prefixes: vec!["task 1234: forbidden".into()], ..cfg(BatchGuardMode::Enforce) };
        let mut st = State::default();
        let t = Instant::now();
        let req = |n: u32| json!({"messages": [{"role": "user", "content": format!("task {n}: forbidden work item")}]});
        assert!(matches!(check_with(&c, &mut st, &req(1234), t), Verdict::Batch { reason: Reason::Manual, .. }));
        assert_eq!(check_with(&c, &mut st, &req(5678), t + Duration::from_secs(1)), Verdict::Pass, "原文不匹配规则");
        let snap = snapshot_with(&c, &st, t + Duration::from_secs(1));
        assert_eq!(snap.flagged.len(), 1);
        assert_eq!(snap.flagged[0].head, "task 1234: forbidden", "面板按规则展示");
    }

    #[test]
    fn subagent_fanout_with_tools_never_flags() {
        // 一个用户一口气开 200 个子代理:全是单轮 + 同一个 CC 开头,但都带 tools。
        let c = BatchGuardConfig { threshold: 40, ..cfg(BatchGuardMode::Enforce) };
        let mut st = State::default();
        let t = Instant::now();
        let cc = "You are Claude Code, Anthropic's official CLI for Claude.";
        for i in 0..200u64 {
            assert_eq!(check_with(&c, &mut st, &single_with_tools(cc), t + Duration::from_millis(i * 100)), Verdict::Pass);
        }
        assert!(st.tracked.is_empty(), "带 tools 的请求连计数表都不进");
    }

    #[test]
    fn share_gate_counts_only_others_inside_the_span() {
        // 阈值 5、占比 90%:最近 5 条脚本请求跨度内,混进 1 条对话流量就不判(5 < 9×1)。
        let c = cfg(BatchGuardMode::Enforce);
        let mut st = State::default();
        let t = Instant::now();
        let s = |k: u64| t + Duration::from_secs(k);
        assert_eq!(check_with(&c, &mut st, &single(VFX), s(0)), Verdict::Pass);
        assert_eq!(check_with(&c, &mut st, &multi(VFX), s(1)), Verdict::Pass);
        for k in 2..6u64 {
            assert_eq!(check_with(&c, &mut st, &single(VFX), s(k)), Verdict::Pass, "第 {k} 秒:占比不够");
        }
        // 第 6 条进来,最早那条脚本请求滑出「最近 5 条」,跨度从第 2 秒起,那条对话流量不在跨度内 → 判定。
        assert!(matches!(check_with(&c, &mut st, &single(VFX), s(6)), Verdict::Batch { newly_flagged: true, count: 5, .. }));
    }

    #[test]
    fn share_gate_at_production_threshold() {
        let c = BatchGuardConfig { threshold: 40, ..cfg(BatchGuardMode::Enforce) };
        let feed = |others: usize| {
            let mut st = State::default();
            let t = Instant::now();
            let mut last = Verdict::Pass;
            for i in 0..40usize {
                let at = t + Duration::from_secs(i as u64 * 10);
                if (1..=others).contains(&i) {
                    check_with(&c, &mut st, &multi(VFX), at - Duration::from_secs(1));
                }
                last = check_with(&c, &mut st, &single(VFX), at);
            }
            last
        };
        assert!(matches!(feed(4), Verdict::Batch { newly_flagged: true, .. }), "40 脚本 + 4 对话 = 91% → 判定");
        assert_eq!(feed(5), Verdict::Pass, "40 脚本 + 5 对话 = 89% → 不判");
    }

    #[test]
    fn others_before_first_scripted_are_ignored() {
        let c = cfg(BatchGuardMode::Enforce);
        let mut st = State::default();
        let t = Instant::now();
        for i in 0..100u64 {
            check_with(&c, &mut st, &multi(VFX), t + Duration::from_millis(i));
        }
        assert!(st.tracked.is_empty(), "只有对话流量的模板不建计数表");
        let v = run(&c, &mut st, &single(VFX), t + Duration::from_secs(1), 5);
        assert!(matches!(v[4], Verdict::Batch { newly_flagged: true, .. }), "跨度之前的对话流量不稀释");
    }

    #[test]
    fn others_deque_is_bounded() {
        let c = cfg(BatchGuardMode::Observe);
        let mut st = State::default();
        let t = Instant::now();
        check_with(&c, &mut st, &single(VFX), t);
        for i in 0..1000u64 {
            check_with(&c, &mut st, &multi(VFX), t + Duration::from_millis(i));
        }
        let track = st.tracked.values().next().unwrap();
        assert!(track.others.len() <= c.threshold as usize, "对话流量只需数到能否定占比为止: {}", track.others.len());
    }

    #[test]
    fn random_hex_tags_collapse_into_one_template() {
        let c = cfg(BatchGuardMode::Enforce);
        let mut st = State::default();
        let t = Instant::now();
        let tags = ["e09b26bff4be", "9f0b5ad93d7e", "862f6ea2310b", "47bfd15d1f8b", "2dad768b7901"];
        let mut last = Verdict::Pass;
        for (i, tg) in tags.iter().enumerate() {
            let body = json!({"messages": [{"role": "user", "content":
                format!("[session {tg}] 以下是一段代码审阅材料，请先读完。 function step0_{tg}(x){{ return x; }}")}]});
            last = check_with(&c, &mut st, &body, t + Duration::from_secs(i as u64));
        }
        match last {
            Verdict::Batch { newly_flagged: true, head, .. } => assert!(head.starts_with("[session #] 以下是"), "{head}"),
            other => panic!("随机标签不该把同一模板打散: {other:?}"),
        }
    }

    /// 生产回放(手动跑,默认跳过):
    /// `BATCH_GUARD_REPLAY=a.jsonl,b.jsonl cargo test -p gw-core --lib replay -- --ignored --nocapture`。
    /// 每行 `{"t": unix 秒, "body": 精简请求体}`;按默认参数(窗口 600s、80 字符)逐条喂,
    /// 阈值取 40 / 20 / 10 看余量,打印判定出的模板。
    #[test]
    #[ignore]
    fn replay_production_sample() {
        let Ok(paths) = std::env::var("BATCH_GUARD_REPLAY") else { return };
        for threshold in [40u32, 20, 10] {
            let c = BatchGuardConfig { threshold, ..BatchGuardConfig::default() };
            for path in paths.split(',') {
                let mut st = State::default();
                let base = Instant::now();
                let mut t0 = None;
                // 模板开头 → (首次判定时距样本开头的秒数, 判定后又命中的条数)
                let mut flagged: HashMap<String, (u64, u64)> = HashMap::new();
                for line in std::fs::read_to_string(path).unwrap().lines() {
                    let row: serde_json::Value = serde_json::from_str(line).unwrap();
                    let t = row["t"].as_u64().unwrap();
                    let off = t - *t0.get_or_insert(t);
                    let at = base + Duration::from_secs(off);
                    if let Verdict::Batch { head, newly_flagged, .. } = check_with(&c, &mut st, &row["body"], at) {
                        let e = flagged.entry(head).or_insert((off, 0));
                        if !newly_flagged {
                            e.1 += 1;
                        }
                    }
                }
                println!("== 阈值 {threshold} | {path}: 判定 {} 个模板", flagged.len());
                for (head, (at, after)) in &flagged {
                    println!("   +{at}s 判定, 之后又命中 {after} 条 | {head}");
                }
            }
        }
    }

    #[test]
    fn template_key_normalizes_ids_but_keeps_words() {
        assert_eq!(template_key("[ref 2dad768b79] x", 80), "[ref #] x");
        assert_eq!(template_key("Today is 2026-10-14, build 1568", 80), "Today is #-#-#, build #");
        assert_eq!(template_key("Item 7 of job 42, step0_x", 80), "Item # of job #, step#_x", "纯数字不论长短");
        assert_eq!(template_key("decade facade deadbeef SHA256 a1", 80), "decade facade deadbeef SH# a1", "不含数字的单词、短混合串不动");
        assert_eq!(template_key("abc 0123456789abcdef tail", 6), "abc # ", "一串只占一个字符");
        assert_eq!(template_key("你好世界", 2), "你好");
        let long = "f".repeat(1_000_000);
        assert_eq!(template_key(&format!("{long}1 tail"), 3), "# t", "整段数完才知道归不归一");
        assert_eq!(template_key(&long, 3), "fff");
    }

    #[test]
    fn classifier_prefix_and_allow_list_are_exempt() {
        let c = BatchGuardConfig { allow_prefixes: vec!["You write short session titles".into()], ..cfg(BatchGuardMode::Enforce) };
        let mut st = State::default();
        let t = Instant::now();
        let classifier = json!({"system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.296.b99; cc_entrypoint=cli;"},
            {"type": "text", "text": format!("{CLASSIFIER_PREFIX}\n\n## Context")}],
            "messages": [{"role": "user", "content": "transcript"}]});
        let titles = single("You write short session titles. Reply with only the title.");
        for i in 0..100u64 {
            assert_eq!(check_with(&c, &mut st, &classifier, t + Duration::from_secs(i)), Verdict::Pass);
            assert_eq!(check_with(&c, &mut st, &titles, t + Duration::from_secs(i)), Verdict::Pass);
        }
        assert_eq!(st.tracked.len(), 0, "豁免的模板根本不进计数表");
    }

    #[test]
    fn manual_block_prefix_blocks_immediately() {
        let c = BatchGuardConfig {
            block_prefixes: vec!["You are a strict, calibrated senior VFX".into()],
            ..cfg(BatchGuardMode::Enforce)
        };
        let mut st = State::default();
        let v = check_with(&c, &mut st, &single(VFX), Instant::now());
        assert!(matches!(v, Verdict::Batch { block: true, reason: Reason::Manual, newly_flagged: true, count: 0, .. }));
    }

    #[test]
    fn billing_header_is_stripped_before_fingerprinting() {
        let c = cfg(BatchGuardMode::Enforce);
        let mut st = State::default();
        let t = Instant::now();
        // 每条请求的计费头都不同(cc_version 滚动),模板相同 → 必须算同一个指纹。
        for i in 0..5u64 {
            let body = json!({"system": [
                {"type": "text", "text": format!("x-anthropic-billing-header: cc_version=2.1.{i}.abc; cc_entrypoint=sdk-py;")},
                {"type": "text", "text": VFX}],
                "messages": [{"role": "user", "content": "x"}]});
            let v = check_with(&c, &mut st, &body, t + Duration::from_secs(i));
            if i == 4 {
                assert!(matches!(v, Verdict::Batch { newly_flagged: true, .. }), "计费头不同不该打散指纹");
            }
        }
        // 计费头写在同一块的第一行也认。
        let inline = json!({"system": format!("x-anthropic-billing-header: cc_version=9;\n{VFX}"),
            "messages": [{"role": "user", "content": "x"}]});
        assert!(matches!(check_with(&c, &mut st, &inline, t + Duration::from_secs(9)), Verdict::Batch { .. }));
    }

    #[test]
    fn no_system_falls_back_to_first_user_text() {
        let c = cfg(BatchGuardMode::Enforce);
        let mut st = State::default();
        let t = Instant::now();
        let body = |n: u64| json!({"messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "x"}},
            {"type": "text", "text": format!("Translate the following product description into French. Item #{n}")}]}]});
        // 编号归一后是同一个模板 → 第 5 条判定。
        let v: Vec<Verdict> = (0..5u64).map(|i| check_with(&c, &mut st, &body(i), t + Duration::from_secs(i))).collect();
        assert!(matches!(v[4], Verdict::Batch { newly_flagged: true, .. }), "没有 system 时按第一条 user 文本的开头分组");
        // 开头文字各不相同的单轮提问 → 各是各的模板,不该误判。
        let mut st2 = State::default();
        for i in 0..20usize {
            let q = json!({"messages": [{"role": "user", "content": format!("Question {}: what is the capital", tag(i))}]});
            assert_eq!(check_with(&c, &mut st2, &q, t + Duration::from_secs(i as u64)), Verdict::Pass);
        }
    }

    #[test]
    fn empty_or_unrecognized_bodies_pass() {
        let c = cfg(BatchGuardMode::Enforce);
        let mut st = State::default();
        let t = Instant::now();
        for body in [json!({}), json!({"messages": []}), json!({"messages": [{"role": "assistant", "content": "x"}]}),
            json!({"system": "", "messages": [{"role": "user", "content": "   "}]})] {
            for i in 0..10u64 {
                assert_eq!(check_with(&c, &mut st, &body, t + Duration::from_secs(i)), Verdict::Pass);
            }
        }
    }

    #[test]
    fn off_mode_does_nothing() {
        let c = cfg(BatchGuardMode::Off);
        let mut st = State::default();
        let v = run(&c, &mut st, &single(VFX), Instant::now(), 50);
        assert!(v.iter().all(|x| *x == Verdict::Pass));
        assert!(st.tracked.is_empty());
    }

    #[test]
    fn tracked_table_is_bounded() {
        let c = cfg(BatchGuardMode::Observe);
        let mut st = State::default();
        let t = Instant::now();
        for i in 0..(MAX_TRACKED + 100) {
            check_with(&c, &mut st, &single(&format!("template number {} with a unique opening", tag(i))), t + Duration::from_millis(i as u64));
        }
        assert!(st.tracked.len() <= MAX_TRACKED);
    }

    #[test]
    fn since_counts_only_requests_after_the_flag() {
        let c = cfg(BatchGuardMode::Enforce);
        let mut st = State::default();
        let t = Instant::now();
        run(&c, &mut st, &single(VFX), t, 5);
        let snap = snapshot_with(&c, &st, t + Duration::from_secs(5));
        assert_eq!(snap.flagged[0].since, 0, "触发判定的那一条不算「之后又来」");
        check_with(&c, &mut st, &single(VFX), t + Duration::from_secs(6));
        assert_eq!(snapshot_with(&c, &st, t + Duration::from_secs(6)).flagged[0].since, 1);
    }

    #[test]
    fn flagged_table_is_bounded_and_snapshot_is_capped() {
        let c = BatchGuardConfig { threshold: 2, ..cfg(BatchGuardMode::Observe) };
        let mut st = State::default();
        let t = Instant::now();
        for i in 0..(MAX_FLAGGED + 50) {
            let body = single(&format!("distinct batch template {} with its own opening", tag(i)));
            let at = t + Duration::from_millis(i as u64 * 2);
            check_with(&c, &mut st, &body, at);
            check_with(&c, &mut st, &body, at + Duration::from_millis(1));
        }
        assert!(st.flagged.len() <= MAX_FLAGGED, "已判定表不能无限长: {}", st.flagged.len());
        let snap = snapshot_with(&c, &st, t + Duration::from_secs(10));
        assert_eq!(snap.flagged.len(), SNAPSHOT_FLAGGED);
        assert_eq!(snap.flagged_total, st.flagged.len());
    }

    #[test]
    fn reset_scope_matches_what_each_change_invalidates() {
        let base = cfg(BatchGuardMode::Observe);
        let enforce = BatchGuardConfig { mode: BatchGuardMode::Enforce, ..base.clone() };
        let off = BatchGuardConfig { mode: BatchGuardMode::Off, ..base.clone() };
        let wider = BatchGuardConfig { prefix_chars: 120, ..base.clone() };
        let none = Reset { tracked: false, flagged: false };
        let counts = Reset { tracked: true, flagged: false };
        let all = Reset { tracked: true, flagged: true };
        assert_eq!(reset_scope(&base, &enforce), none, "observe → enforce 保留判定");
        assert_eq!(reset_scope(&base, &BatchGuardConfig { block_secs: 99, ..base.clone() }), none);
        assert_eq!(reset_scope(&base, &BatchGuardConfig { threshold: 99, ..base.clone() }), counts, "队列按旧阈值截断过,作废");
        assert_eq!(reset_scope(&base, &BatchGuardConfig { window_secs: 999, ..base.clone() }), counts);
        assert_eq!(reset_scope(&base, &off), all, "切到 off 清空");
        assert_eq!(reset_scope(&off, &off), none);
        assert_eq!(reset_scope(&base, &wider), all, "指纹口径变了清空");
    }

    #[test]
    fn utf8_prefix_cut_is_char_safe() {
        assert_eq!(take_chars("你好世界", 2), "你好");
        assert_eq!(take_chars("ab", 10), "ab");
        let _ = fingerprint(&template_key("中文模板开头", 3));
    }

    #[test]
    fn reject_message_avoids_new_api_disable_keywords() {
        let lower = REJECT_MESSAGE.to_lowercase();
        for kw in ["permission denied", "operation not allowed", "your account is not authorized",
            "your credit balance is too low", "organization has been disabled", "exceeded your current quota"] {
            assert!(!lower.contains(kw), "拒绝文案含下游禁用关键词 {kw:?}");
        }
    }
}
