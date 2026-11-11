//! 拒答记忆:同一条请求被上游安全分类器拒答后,客户端原样重发不再满池扩散。
//!
//! # 为什么要做
//!
//! 上游分类器拒答(ContentFiltered)拒的是内容,worker 自己从不因它换号重试
//! (`UpstreamErrorKind::worth_switching_account`)。但**客户端会原样重发**:2026-10-29
//! 前后 32 小时共 863 次拒答,拒答后同一会话的下一条请求 816/822 是原样重发,重发约 79%
//! 再被拒、约 16% 碰运气通过;而每条重发对网关都是一条新请求,调度照常选号 —— 三分之一
//! 落到了别的号上。结果一个逆向 DLL 的会话被拒 79 次、打到 27 个号,其中 15 个随后被封。
//! 每一次拒答都在那个号上记一次违规,**扩散本身就是封号的放大器**。
//!
//! # 做法
//!
//! 按「同一条请求」的指纹记账(见 [`fingerprint`]):
//!
//! | 记录 | 本次请求(enforce) | 上游结果 |
//! |---|---|---|
//! | 无 | 照常选号 | 零产出拒答 → 建记录(strikes=1,记下这个号) |
//! | strikes ≤ retries | **钉号**:只许用记录的那个号 | 又拒 → strikes+1;通过 → 删记录 |
//! | strikes > retries | **本地回拒答**,不选号、不碰上游 | — |
//!
//! 钉的号用不了(被封 / 禁用 / 不在本组)时由调用方本地回拒答,**绝不换号**。
//! 记录从最后一次被拒起保留 `ttl_secs`,到期自动放行。
//!
//! # 指纹口径
//!
//! `model ‖ system ‖ messages`,规范化后取 sha256:键排序、丢弃 `cache_control`(断点位置
//! 每轮都挪)、system 各文本块剥掉开头的计费头行(CC 的计费头带每请求变化的指纹)。
//! 不含 tools / thinking / max_tokens / metadata。**含 model**:上游原话就是「换个模型」,
//! 换模型必须放行。只做整条精确匹配:拒答后接着往下聊的只有 5/822,不值得承担误拦。
//!
//! 绝大多数请求不做哈希:另存 `(model, messages 条数)` 的计数索引,索引里没有的直接放行。
//!
//! # 模式
//!
//! - `observe`(代码默认):照常记账,判定结果只打日志,不钉号、不拦。
//! - `enforce`:钉号 + 本地拒答。
//! - `off`:完全不看、不记账。
//!
//! 状态纯内存、单进程,重启清空。热切换配置不清状态(observe 期间记下的立即在 enforce 生效)。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

use gw_core::config::refusal_guard::*;
use gw_core::normalize::strip_leading_billing_lines;

/// 本地回拒答在请求日志 `error_kind` 列里的标记。**改这个串要同步改前端**
/// `admin-ui/src/features/logs/errorKind.ts` 的 `REFUSAL_REPEAT_KIND`。
pub const LOG_KIND_REPEAT: &str = "RefusalRepeat";

/// 记录上限。满了先清过期,仍满则挤掉最早到期的那条。单条约几百字节(主要是拒答原文)。
const MAX_ENTRIES: usize = 4096;
/// `/health` 里最多列多少条记录(总数另报)。
const SNAPSHOT_ENTRIES: usize = 30;
/// 拒答原文缺失时的兜底(正常路径总能从请求日志的错误体里取到)。
const FALLBACK_MESSAGE: &str = "请求被模型的内容安全分类器拒绝。";

static CONFIG: LazyLock<RwLock<RefusalGuardConfig>> =
    LazyLock::new(|| RwLock::new(RefusalGuardConfig::default()));
static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));

/// 热应用。非法 → Err 且保留上一份有效值。切到 `off` 清空状态:关掉就该是干净的。
pub fn set_config(cfg: &RefusalGuardConfig) -> Result<(), String> {
    validate(cfg)?;
    let mut guard = CONFIG.write().unwrap_or_else(|p| p.into_inner());
    if cfg.mode == RefusalGuardMode::Off && guard.mode != RefusalGuardMode::Off {
        let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
        st.entries.clear();
        st.index.clear();
    }
    *guard = cfg.clone();
    Ok(())
}

pub fn current() -> RefusalGuardConfig {
    CONFIG.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// 对一条请求的结论。`enforce` = 是否真的执行(observe 下为 false,调用方只记日志)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// 同一条请求被拒过 `strikes` 次、还没超次数:只许用 `account_id`。
    /// `message` = 上次的拒答原文(钉的号用不了时调用方拿它本地回拒答)。
    Pin { account_id: String, strikes: u32, message: String, enforce: bool },
    /// 超次数:本地回拒答。
    Block { strikes: u32, message: String, enforce: bool },
}

/// 一次请求的上游结果(由请求日志收口处映射)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome<'a> {
    /// 零产出拒答。`account_id` = 落在哪个号上;`message` = 客户收到的拒答原文。
    Refused { account_id: &'a str, message: Option<&'a str> },
    /// 拿到了回复(含答到一半的 stop_reason=refusal:客户已有内容,下一轮必然不同)。
    Answered,
    /// 其它失败(限流、断流、本地拦截……):不改变记录。
    Other,
}

#[derive(Debug, Clone)]
struct Entry {
    account_id: String,
    strikes: u32,
    message: String,
    model: String,
    n_msgs: usize,
    expires_at: Instant,
}

#[derive(Default)]
struct State {
    entries: HashMap<[u8; 32], Entry>,
    /// `(model, messages 条数)` → 记录条数。查不到就不必算指纹。
    index: HashMap<(String, usize), u32>,
    recorded_total: u64,
    cleared_total: u64,
    pinned_total: u64,
    blocked_total: u64,
}

impl State {
    fn has_candidate(&self, model: &str, n_msgs: usize) -> bool {
        // 每请求一次小分配,换来绝大多数请求不做哈希。
        self.index.contains_key(&(model.to_string(), n_msgs))
    }

    fn remove(&mut self, fp: &[u8; 32]) -> Option<Entry> {
        let e = self.entries.remove(fp)?;
        let key = (e.model.clone(), e.n_msgs);
        if let Some(c) = self.index.get_mut(&key) {
            *c -= 1;
            if *c == 0 {
                self.index.remove(&key);
            }
        }
        Some(e)
    }

    fn prune_expired(&mut self, now: Instant) {
        let dead: Vec<[u8; 32]> =
            self.entries.iter().filter(|(_, e)| e.expires_at <= now).map(|(k, _)| *k).collect();
        for k in dead {
            self.remove(&k);
        }
    }

    fn make_room(&mut self, now: Instant) {
        if self.entries.len() < MAX_ENTRIES {
            return;
        }
        self.prune_expired(now);
        while self.entries.len() >= MAX_ENTRIES {
            let Some(oldest) = self.entries.iter().min_by_key(|(_, e)| e.expires_at).map(|(k, _)| *k) else {
                return;
            };
            self.remove(&oldest);
        }
    }
}

fn messages_len(body: &Value) -> Option<usize> {
    body.get("messages").and_then(Value::as_array).map(Vec::len)
}

/// 入口判定(选号之前调用)。不改状态里的记账,只在命中时累计钉号/拦截计数。
pub fn check(model: &str, body: &Value, now: Instant) -> Verdict {
    let cfg = current();
    if cfg.mode == RefusalGuardMode::Off {
        return Verdict::Pass;
    }
    let Some(n) = messages_len(body) else {
        return Verdict::Pass;
    };
    {
        let st = STATE.lock().unwrap_or_else(|p| p.into_inner());
        if !st.has_candidate(model, n) {
            return Verdict::Pass;
        }
    }
    // 哈希放在锁外(大请求几 MB,别让别的请求等它)。
    let fp = fingerprint(model, body);
    let enforce = cfg.mode == RefusalGuardMode::Enforce;
    let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let Some(e) = st.entries.get(&fp) else {
        return Verdict::Pass;
    };
    if e.expires_at <= now {
        st.remove(&fp);
        return Verdict::Pass;
    }
    let (strikes, message, account_id) = (e.strikes, e.message.clone(), e.account_id.clone());
    if strikes > cfg.retries {
        if enforce {
            st.blocked_total += 1;
        }
        Verdict::Block { strikes, message, enforce }
    } else {
        if enforce {
            st.pinned_total += 1;
        }
        Verdict::Pin { account_id, strikes, message, enforce }
    }
}

/// 请求收尾时记账。返回本次是否改动了记录(便于调用方打日志)。
pub fn on_outcome(model: &str, body: &Value, outcome: Outcome<'_>, now: Instant) -> Option<RecordChange> {
    let cfg = current();
    if cfg.mode == RefusalGuardMode::Off {
        return None;
    }
    let n = messages_len(body)?;
    match outcome {
        Outcome::Other => None,
        Outcome::Answered => {
            if !STATE.lock().unwrap_or_else(|p| p.into_inner()).has_candidate(model, n) {
                return None;
            }
            let fp = fingerprint(model, body);
            let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
            let e = st.remove(&fp)?;
            st.cleared_total += 1;
            Some(RecordChange::Cleared { account_id: e.account_id, strikes: e.strikes })
        }
        Outcome::Refused { account_id, message } => {
            let fp = fingerprint(model, body);
            let ttl = Duration::from_secs(cfg.ttl_secs);
            let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
            st.recorded_total += 1;
            if let Some(e) = st.entries.get_mut(&fp) {
                e.strikes += 1;
                e.expires_at = now + ttl;
                if let Some(m) = message {
                    e.message = m.to_string();
                }
                return Some(RecordChange::Struck { account_id: e.account_id.clone(), strikes: e.strikes });
            }
            st.make_room(now);
            st.entries.insert(
                fp,
                Entry {
                    account_id: account_id.to_string(),
                    strikes: 1,
                    message: message.unwrap_or(FALLBACK_MESSAGE).to_string(),
                    model: model.to_string(),
                    n_msgs: n,
                    expires_at: now + ttl,
                },
            );
            *st.index.entry((model.to_string(), n)).or_insert(0) += 1;
            Some(RecordChange::Struck { account_id: account_id.to_string(), strikes: 1 })
        }
    }
}

/// [`on_outcome`] 对记录做了什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordChange {
    /// 记了一次拒答。`account_id` = 记录里钉的号(首次被拒的号)。
    Struck { account_id: String, strikes: u32 },
    /// 重发拿到了回复,记录删除。
    Cleared { account_id: String, strikes: u32 },
}

/// 客户收到的本地拒答文案:上游原文 + 网关说明。不能含下游 new-api 的渠道禁用关键词
/// (`Permission denied` / `Operation not allowed` / `Your account is not authorized` …),
/// 否则一次本地拒答会把整条渠道下线。
pub fn reject_message(upstream: &str, strikes: u32) -> String {
    format!(
        "{}(网关:这条消息已被拒答 {strikes} 次,不再重复发送;请回退到更早的消息、改写内容或换模型后再试)",
        upstream.trim_end()
    )
}

/// 面板 / `/health` 用的快照。
pub fn snapshot(now: Instant) -> Value {
    let cfg = current();
    let st = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let mut live: Vec<&Entry> = st.entries.values().filter(|e| e.expires_at > now).collect();
    live.sort_by_key(|e| std::cmp::Reverse(e.strikes));
    let recent: Vec<Value> = live
        .iter()
        .take(SNAPSHOT_ENTRIES)
        .map(|e| {
            serde_json::json!({
                "account": e.account_id,
                "strikes": e.strikes,
                "model": e.model,
                "messages": e.n_msgs,
                "expires_in_secs": e.expires_at.saturating_duration_since(now).as_secs(),
            })
        })
        .collect();
    serde_json::json!({
        "mode": gw_core::config::batch_guard::mode_str(cfg.mode),
        "retries": cfg.retries,
        "ttl_secs": cfg.ttl_secs,
        "entries": live.len(),
        "recorded_total": st.recorded_total,
        "cleared_total": st.cleared_total,
        "pinned_total": st.pinned_total,
        "blocked_total": st.blocked_total,
        "top": recent,
    })
}

// ───────── 指纹 ─────────

/// 同一条请求的指纹(口径见模块文档)。
pub fn fingerprint(model: &str, body: &Value) -> [u8; 32] {
    let mut h = Sha256::new();
    feed_str(&mut h, model);
    match body.get("system") {
        Some(Value::String(s)) => {
            h.update(b"S");
            feed_str(&mut h, strip_leading_billing_lines(s));
        }
        Some(Value::Array(blocks)) => {
            h.update(b"A");
            h.update((blocks.len() as u64).to_le_bytes());
            for b in blocks {
                match b.get("text").and_then(Value::as_str) {
                    Some(t) if b.get("type").and_then(Value::as_str) == Some("text") => {
                        h.update(b"T");
                        feed_str(&mut h, strip_leading_billing_lines(t));
                    }
                    _ => feed_value(&mut h, b),
                }
            }
        }
        Some(other) => feed_value(&mut h, other),
        None => h.update(b"-"),
    }
    match body.get("messages") {
        Some(m) => feed_value(&mut h, m),
        None => h.update(b"-"),
    }
    h.finalize().into()
}

fn feed_str(h: &mut Sha256, s: &str) {
    h.update((s.len() as u64).to_le_bytes());
    h.update(s.as_bytes());
}

/// 规范化喂入:长度前缀防拼接歧义,对象键排序、丢 `cache_control`。
fn feed_value(h: &mut Sha256, v: &Value) {
    match v {
        Value::Null => h.update(b"n"),
        Value::Bool(b) => h.update(if *b { b"t" } else { b"f" }),
        Value::Number(n) => {
            h.update(b"#");
            feed_str(h, &n.to_string());
        }
        Value::String(s) => {
            h.update(b"s");
            feed_str(h, s);
        }
        Value::Array(a) => {
            h.update(b"[");
            h.update((a.len() as u64).to_le_bytes());
            for x in a {
                feed_value(h, x);
            }
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().filter(|k| k.as_str() != "cache_control").collect();
            keys.sort();
            h.update(b"{");
            h.update((keys.len() as u64).to_le_bytes());
            for k in keys {
                feed_str(h, k);
                feed_value(h, &m[k.as_str()]);
            }
        }
    }
}

/// 全局状态是进程级的:碰本模块配置 / 状态的测试(本模块单测 + worker 入口级测试)串行跑。
#[cfg(test)]
pub(crate) static TEST_SERIAL: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn reset_for_test() {
    let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
    *st = State::default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup(mode: RefusalGuardMode, retries: u32) -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        set_config(&RefusalGuardConfig { mode, retries, ttl_secs: 3600 }).unwrap();
        reset_for_test();
        g
    }

    fn body(text: &str) -> Value {
        json!({
            "model": "claude-opus-5-5",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.1; cch=abc123;"},
                {"type": "text", "text": "You are Claude Code.", "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "task"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
                {"role": "user", "content": [{"type": "text", "text": text, "cache_control": {"type": "ephemeral"}}]}
            ]
        })
    }

    const M: &str = "claude-opus-5-5";

    fn refuse(b: &Value, acc: &str, now: Instant) -> Option<RecordChange> {
        on_outcome(M, b, Outcome::Refused { account_id: acc, message: Some("请求被拒绝(类别:CYBER)") }, now)
    }

    #[test]
    fn fingerprint_ignores_cache_control_key_order_and_billing_header() {
        let a = body("disasm BCCLog.dll");
        let mut b = a.clone();
        // 断点挪到别的消息、计费头指纹变了、键序变了:仍是同一条请求。
        b["messages"][2]["content"][0].as_object_mut().unwrap().remove("cache_control");
        b["messages"][0]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
        b["system"][0]["text"] = json!("x-anthropic-billing-header: cc_version=2.1.1; cch=ffff99;");
        let reordered: Value = serde_json::from_str(
            &serde_json::to_string(&json!({"messages": b["messages"], "system": b["system"], "model": M})).unwrap(),
        )
        .unwrap();
        assert_eq!(fingerprint(M, &a), fingerprint(M, &b));
        assert_eq!(fingerprint(M, &a), fingerprint(M, &reordered));
    }

    #[test]
    fn fingerprint_changes_with_content_or_model() {
        let a = body("disasm BCCLog.dll");
        assert_ne!(fingerprint(M, &a), fingerprint(M, &body("disasm BCCLog.dlL")));
        assert_ne!(fingerprint(M, &a), fingerprint("claude-sonnet-5-5", &a), "换模型是上游建议的出路,必须放行");
        let mut sys = a.clone();
        sys["system"][1]["text"] = json!("You are Grok Build.");
        assert_ne!(fingerprint(M, &a), fingerprint(M, &sys));
    }

    #[test]
    fn refused_once_pins_then_blocks_after_retries() {
        let _g = setup(RefusalGuardMode::Enforce, 1);
        let now = Instant::now();
        let b = body("x");
        assert_eq!(check(M, &b, now), Verdict::Pass, "没记录照常选号");
        assert_eq!(refuse(&b, "acc-a", now), Some(RecordChange::Struck { account_id: "acc-a".into(), strikes: 1 }));
        match check(M, &b, now) {
            Verdict::Pin { account_id, strikes, enforce, .. } => {
                assert_eq!((account_id.as_str(), strikes, enforce), ("acc-a", 1, true));
            }
            v => panic!("应钉回原号: {v:?}"),
        }
        // 钉号重发又被拒(落在哪个号上不重要,记录里的号始终是第一次那个)。
        assert_eq!(refuse(&b, "acc-a", now), Some(RecordChange::Struck { account_id: "acc-a".into(), strikes: 2 }));
        match check(M, &b, now) {
            Verdict::Block { strikes, message, enforce } => {
                assert_eq!((strikes, enforce), (2, true));
                assert!(message.contains("CYBER"), "沿用上游拒答原文");
            }
            v => panic!("超次数应本地拦: {v:?}"),
        }
        assert_eq!(check(M, &body("y"), now), Verdict::Pass, "别的请求不受影响");
    }

    #[test]
    fn zero_retries_blocks_right_after_first_refusal() {
        let _g = setup(RefusalGuardMode::Enforce, 0);
        let now = Instant::now();
        let b = body("x");
        refuse(&b, "acc-a", now);
        assert!(matches!(check(M, &b, now), Verdict::Block { strikes: 1, .. }));
    }

    #[test]
    fn answered_resend_clears_the_record() {
        let _g = setup(RefusalGuardMode::Enforce, 1);
        let now = Instant::now();
        let b = body("x");
        refuse(&b, "acc-a", now);
        assert_eq!(
            on_outcome(M, &b, Outcome::Answered, now),
            Some(RecordChange::Cleared { account_id: "acc-a".into(), strikes: 1 })
        );
        assert_eq!(check(M, &b, now), Verdict::Pass);
        assert_eq!(on_outcome(M, &b, Outcome::Answered, now), None, "没记录的成功请求什么都不做");
    }

    #[test]
    fn other_failures_leave_the_record_alone() {
        let _g = setup(RefusalGuardMode::Enforce, 1);
        let now = Instant::now();
        let b = body("x");
        refuse(&b, "acc-a", now);
        assert_eq!(on_outcome(M, &b, Outcome::Other, now), None);
        assert!(matches!(check(M, &b, now), Verdict::Pin { strikes: 1, .. }));
    }

    #[test]
    fn record_expires_after_ttl() {
        let _g = setup(RefusalGuardMode::Enforce, 0);
        let now = Instant::now();
        let b = body("x");
        refuse(&b, "acc-a", now);
        assert!(matches!(check(M, &b, now + Duration::from_secs(3599)), Verdict::Block { .. }));
        assert_eq!(check(M, &b, now + Duration::from_secs(3600)), Verdict::Pass, "到期放行");
        assert_eq!(snapshot(now + Duration::from_secs(3600))["entries"], 0, "过期记录已删");
    }

    #[test]
    fn observe_reports_but_never_enforces() {
        let _g = setup(RefusalGuardMode::Observe, 0);
        let now = Instant::now();
        let b = body("x");
        refuse(&b, "acc-a", now);
        assert!(matches!(check(M, &b, now), Verdict::Block { enforce: false, .. }));
        let snap = snapshot(now);
        assert_eq!(snap["blocked_total"], 0, "observe 不计执行次数");
        assert_eq!(snap["recorded_total"], 1, "observe 照常记账");
    }

    #[test]
    fn off_mode_ignores_everything_and_clears_state() {
        let _g = setup(RefusalGuardMode::Enforce, 0);
        let now = Instant::now();
        let b = body("x");
        refuse(&b, "acc-a", now);
        set_config(&RefusalGuardConfig { mode: RefusalGuardMode::Off, retries: 0, ttl_secs: 3600 }).unwrap();
        assert_eq!(check(M, &b, now), Verdict::Pass);
        assert_eq!(refuse(&b, "acc-a", now), None);
        set_config(&RefusalGuardConfig { mode: RefusalGuardMode::Enforce, retries: 0, ttl_secs: 3600 }).unwrap();
        assert_eq!(check(M, &b, now), Verdict::Pass, "关掉再打开从零开始");
    }

    #[test]
    fn capacity_evicts_the_soonest_to_expire() {
        let _g = setup(RefusalGuardMode::Enforce, 0);
        let now = Instant::now();
        for i in 0..MAX_ENTRIES {
            refuse(&body(&format!("m{i}")), "acc", now + Duration::from_millis(i as u64));
        }
        refuse(&body("newest"), "acc", now + Duration::from_secs(10));
        let later = now + Duration::from_secs(10);
        assert_eq!(check(M, &body("m0"), later), Verdict::Pass, "最早到期的被挤掉");
        assert!(matches!(check(M, &body("newest"), later), Verdict::Block { .. }));
        assert!(matches!(check(M, &body("m1"), later), Verdict::Block { .. }));
        assert_eq!(snapshot(later)["entries"], MAX_ENTRIES);
    }

    #[test]
    fn invalid_config_keeps_previous() {
        let _g = setup(RefusalGuardMode::Enforce, 1);
        assert!(set_config(&RefusalGuardConfig { mode: RefusalGuardMode::Enforce, retries: 9, ttl_secs: 3600 }).is_err());
        assert_eq!(current().retries, 1);
    }

    #[test]
    fn log_kind_literal_matches_frontend() {
        // 前端 admin-ui/src/features/logs/errorKind.ts 的 REFUSAL_REPEAT_KIND。
        assert_eq!(LOG_KIND_REPEAT, "RefusalRepeat");
    }

    #[test]
    fn reject_message_keeps_upstream_text_and_avoids_channel_disable_keywords() {
        let m = reject_message("请求被模型的内容安全分类器拒绝(类别:CYBER)。", 2);
        assert!(m.starts_with("请求被模型的内容安全分类器拒绝(类别:CYBER)。"));
        assert!(m.contains("2 次"));
        for kw in ["Permission denied", "Operation not allowed", "not authorized", "quota", "disabled"] {
            assert!(!m.contains(kw), "含渠道禁用关键词 {kw}");
        }
    }
}
