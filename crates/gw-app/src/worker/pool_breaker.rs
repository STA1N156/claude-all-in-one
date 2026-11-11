//! 号池熔断:短时间内多个号「出事」(首次被封 / 掉 FREE)→ 整个 Kiro 号池暂停接单,只能手动恢复。
//!
//! ## 为什么要有
//!
//! 封号是成片来的:11-08 20:30 一分钟内封了 7 个,10-29 一天 27 个。元凶往往是某个新出现的
//! 批量负载(见记忆 kiro-batch-workload-ban-risk),它在号池里换着号打,每换一个号就多死一个。
//! 等人看到时已经死了一片。熔断的意思是:**第 3 个号出事时,先把所有请求停下来,让人看一眼**。
//!
//! ## 什么算「出事」(每个号在窗口内只计一次)
//!
//! - [`EventKind::Suspended`]:调度器收到上游 `TEMPORARILY_SUSPENDED`,且该号封禁连击 0→1。
//!   冷却期满复活后又被封(连击 ≥2)是同一个死号的余波,不计。
//! - [`EventKind::Downgraded`]:额度轮询发现档位从付费档掉成 FREE。用户口径「掉 FREE 也算封号」。
//!   **北京时间每月 1、2 日不计**:月初订阅到期会批量掉档,那不是惩罚(用户 11-09 定)。
//! - 令牌失效、额度用完、限流都不计:它们平时就成批出现(空闲一夜后的死号就是一批)。
//!
//! ## 状态与落库
//!
//! 熔断先在**内存**里生效(下一条请求就被挡),再由热设置同步循环([`Breaker::reconcile`])写进
//! `settings` 表。库是跨重启、跨进程的真值,分两个键:
//! - [`STATE_KEY`] 只由 worker 写:熔断时间 + 出事的号。worker 重启后从这里接过熔断 ——
//!   重启不能变成「自动恢复」。
//! - [`RESET_KEY`] 只由 router(管理员点「恢复接单」)写:`reset_at`。worker 下一轮同步看到
//!   `reset_at` 不早于熔断时间就解除,**并清空窗口**(否则旧事件会让它一恢复就再熔断)。
//!   分成两个键是为了让 worker 的读-改-写永远碰不到管理员刚写的恢复(审查 11-09)。
//! - 落库失败时内存保持熔断,宁可多停,下一轮再写。
//!
//! 已知限制:计数窗口在每个 worker 进程内存里。现在只有 worker0 一个 Kiro worker;以后拆成
//! 多个 Kiro worker 时,「全池 30 分钟内 3 个号」会变成「每个进程各自 3 个」,要把窗口挪进库。

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex, RwLock};

use gw_core::config::pool_breaker::{validate, PoolBreakerConfig, PoolBreakerMode};
use serde::{Deserialize, Serialize};

/// 熔断状态(只由 worker 写)。
pub const STATE_KEY: &str = "pool_breaker_state";
/// 手动恢复时间(只由 router 写),值为 `{"reset_at": 毫秒}`。
pub const RESET_KEY: &str = "pool_breaker_reset";

const BEIJING_OFFSET_MS: i64 = 8 * 3600 * 1000;
const DAY_MS: i64 = 86_400 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// 上游临时封禁(首次)。
    Suspended,
    /// 付费档掉成 FREE。
    Downgraded,
}

impl EventKind {
    fn label(self) -> &'static str {
        match self {
            EventKind::Suspended => "被封",
            EventKind::Downgraded => "掉 FREE",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub account: String,
    pub kind: EventKind,
    /// unix 毫秒。
    pub at: i64,
}

/// 库里两个键合起来的视图(见 [`load_stored`])。`tripped_at` 有值且晚于 `reset_at`
/// (或没有 `reset_at`)= 熔断中。恢复只写 [`RESET_KEY`],上次熔断的时间和事件留着供后台展示。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredState {
    #[serde(default)]
    pub tripped_at: Option<i64>,
    #[serde(default)]
    pub events: Vec<Event>,
    #[serde(default)]
    pub reset_at: Option<i64>,
}

impl StoredState {
    pub fn is_tripped(&self) -> bool {
        match (self.tripped_at, self.reset_at) {
            (Some(t), Some(r)) => r < t,
            (Some(_), None) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trip {
    pub at: i64,
    pub events: Vec<Event>,
}

/// [`Breaker::note`] 的结果(调用方据此打日志)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Noted {
    /// 模式 off / 月初掉 FREE / 窗口内已计过这个号。
    Ignored,
    /// 计入窗口,还没到阈值;带窗口内账号数。
    Counted(usize),
    /// 这一个让号池熔断了(enforce)。
    Tripped(Trip),
    /// 已在熔断中,补记一个出事的号。
    AddedToTrip(usize),
    /// observe:本应熔断,只记录。
    WouldTrip(usize),
}

/// [`Breaker::reconcile`] 要调用方做的 IO。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconcile {
    Nothing,
    /// 把这份文档写进库;`notify` = 这次熔断还没发过通知。
    Persist { doc: StoredState, notify: bool },
    /// 从库里接过了熔断(重启 / 别的进程触发)。
    Adopted(Trip),
    /// 管理员恢复了:已解除并清空窗口。
    Reset,
}

/// 纯状态机(无全局、无 IO,测试直接用);进程级实例见下方全局函数。
#[derive(Debug, Default)]
pub struct Breaker {
    window: VecDeque<Event>,
    tripped: Option<Trip>,
    /// 当前这次熔断已原样落库。
    persisted: bool,
    /// 当前这次熔断已发过通知。
    notified: bool,
    pub trips_total: u64,
    pub would_trip_total: u64,
    /// 熔断期间挡掉的请求数(不写请求日志,见 `gates::pool_breaker_gate`)。
    pub blocked_total: u64,
}

impl Breaker {
    pub fn note(&mut self, cfg: &PoolBreakerConfig, account: &str, kind: EventKind, now: i64) -> Noted {
        if cfg.mode == PoolBreakerMode::Off {
            return Noted::Ignored;
        }
        if kind == EventKind::Downgraded && is_month_start_beijing(now) {
            return Noted::Ignored;
        }
        let window_ms = cfg.window_secs as i64 * 1000;
        while self.window.front().is_some_and(|e| e.at <= now - window_ms) {
            self.window.pop_front();
        }
        if self.window.iter().any(|e| e.account == account) {
            return Noted::Ignored;
        }
        let event = Event { account: account.to_string(), kind, at: now };
        self.window.push_back(event.clone());
        let count = self.window.len();
        if let Some(trip) = self.tripped.as_mut() {
            trip.events.push(event);
            self.persisted = false;
            return Noted::AddedToTrip(trip.events.len());
        }
        if count < cfg.threshold as usize {
            return Noted::Counted(count);
        }
        if cfg.mode == PoolBreakerMode::Observe {
            self.would_trip_total += 1;
            return Noted::WouldTrip(count);
        }
        let trip = Trip { at: now, events: self.window.iter().cloned().collect() };
        self.tripped = Some(trip.clone());
        self.persisted = false;
        self.notified = false;
        self.trips_total += 1;
        Noted::Tripped(trip)
    }

    /// 入口闸门用:enforce 且熔断中才挡。模式切到 off/observe 等于临时放行(应急口)。
    pub fn blocking(&self, cfg: &PoolBreakerConfig) -> Option<&Trip> {
        if cfg.mode == PoolBreakerMode::Enforce {
            self.tripped.as_ref()
        } else {
            None
        }
    }

    /// 与库内状态对账(每轮热设置同步调用一次)。`db` 为库里读到的文档(没有 = 默认值)。
    pub fn reconcile(&mut self, db: &StoredState) -> Reconcile {
        let Some(trip) = self.tripped.clone() else {
            if db.is_tripped() {
                let trip = Trip { at: db.tripped_at.unwrap_or_default(), events: db.events.clone() };
                self.tripped = Some(trip.clone());
                self.persisted = true;
                // 库里有熔断说明触发方已经处理过通知,这里不重发。
                self.notified = true;
                return Reconcile::Adopted(trip);
            }
            return Reconcile::Nothing;
        };
        if db.reset_at.is_some_and(|r| r >= trip.at) {
            self.tripped = None;
            self.window.clear();
            self.persisted = false;
            self.notified = false;
            return Reconcile::Reset;
        }
        let mut trip = trip;
        if db.tripped_at == Some(trip.at) {
            // 同一次熔断(另一个 Kiro worker 接过去后又补记了号):取并集,不互相覆盖。
            let mut merged = false;
            for e in &db.events {
                if !trip.events.iter().any(|x| x.account == e.account) {
                    trip.events.push(e.clone());
                    merged = true;
                }
            }
            if merged {
                trip.events.sort_by_key(|e| e.at);
                self.tripped = Some(trip.clone());
            }
            let db_has_all = trip.events.iter().all(|e| db.events.iter().any(|x| x.account == e.account));
            if db_has_all {
                self.persisted = true;
                return Reconcile::Nothing;
            }
        }
        // 只写 STATE_KEY 的内容;reset_at 属于 RESET_KEY,不经这里回写。
        let doc = StoredState { tripped_at: Some(trip.at), events: trip.events, reset_at: None };
        Reconcile::Persist { doc, notify: !self.notified }
    }

    /// 调用方写库成功后调用。
    pub fn mark_persisted(&mut self, notified: bool) {
        self.persisted = true;
        if notified {
            self.notified = true;
        }
    }

    pub fn snapshot(&self, cfg: &PoolBreakerConfig) -> serde_json::Value {
        serde_json::json!({
            "mode": gw_core::config::batch_guard::mode_str(cfg.mode),
            "window_secs": cfg.window_secs,
            "threshold": cfg.threshold,
            "tripped": self.tripped.as_ref().map(|t| serde_json::json!({"at": t.at, "events": t.events})),
            "persisted": self.persisted,
            "window": self.window.iter().collect::<Vec<_>>(),
            "trips_total": self.trips_total,
            "would_trip_total": self.would_trip_total,
            "blocked_total": self.blocked_total,
        })
    }
}

/// 北京时间是否为当月 1 日或 2 日(月初订阅到期批量掉档,掉 FREE 不计)。
pub fn is_month_start_beijing(now_ms: i64) -> bool {
    let days = (now_ms + BEIJING_OFFSET_MS).div_euclid(DAY_MS);
    let (_, _, day) = civil_from_days(days);
    day <= 2
}

/// 1970-01-01 起的天数 → (年, 月, 日)。Howard Hinnant 的 civil_from_days。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// 档位是否算付费档(非空且不含 FREE)。
pub fn is_paid_title(title: &str) -> bool {
    let t = title.trim();
    !t.is_empty() && !t.to_ascii_uppercase().contains("FREE")
}

/// 档位是否为 FREE。
pub fn is_free_title(title: &str) -> bool {
    title.to_ascii_uppercase().contains("FREE")
}

/// 熔断时回给客户的说明。
pub fn reject_message(trip: &Trip, cfg: &PoolBreakerConfig) -> String {
    let at = trip.at.div_euclid(1000) + 8 * 3600;
    let (hh, mm) = ((at.rem_euclid(86_400)) / 3600, (at.rem_euclid(3600)) / 60);
    format!(
        "号池熔断:北京时间 {hh:02}:{mm:02} 起 {} 分钟内有 {} 个号被封或掉 FREE,已暂停接单,等待管理员恢复。Kiro pool circuit breaker is open; requests are paused until an operator resets it.",
        cfg.window_secs / 60,
        trip.events.len()
    )
}

/// 给人看的一句话(日志 / 通知)。
pub fn describe(trip: &Trip) -> String {
    let list: Vec<String> = trip.events.iter().map(|e| format!("{}({})", e.account, e.kind.label())).collect();
    format!("{} 个号出事:{}", trip.events.len(), list.join("、"))
}

// ───────── 进程级实例 ─────────

/// 只有跑 Kiro 号池、且有控制面库的 worker 才打开(启动时 [`enable`])。
///
/// 默认关:cursor / dario worker 用同一个二进制、同一张 settings 表,不打开的话它们也会从库里
/// 接过熔断、把自己的流量一起停掉。测试直接构造 WorkerState 不走启动流程,也就不会让调度器
/// 测试里的封禁事件跨用例累计成一次全局熔断。
static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn enable() {
    ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

static CONFIG: LazyLock<RwLock<PoolBreakerConfig>> = LazyLock::new(|| RwLock::new(PoolBreakerConfig::default()));
static STATE: LazyLock<Mutex<Breaker>> = LazyLock::new(|| Mutex::new(Breaker::default()));

pub fn set_config(cfg: &PoolBreakerConfig) -> Result<(), String> {
    validate(cfg)?;
    *CONFIG.write().unwrap_or_else(|p| p.into_inner()) = cfg.clone();
    Ok(())
}

pub fn current() -> PoolBreakerConfig {
    CONFIG.read().unwrap_or_else(|p| p.into_inner()).clone()
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 记一次出事(调度器 / 额度轮询调用;只更新内存,不做 IO)。
pub fn note(account: &str, kind: EventKind) {
    if !enabled() {
        return;
    }
    let cfg = current();
    let result = STATE.lock().unwrap_or_else(|p| p.into_inner()).note(&cfg, account, kind, now_ms());
    match result {
        Noted::Ignored => {}
        Noted::Counted(n) => tracing::warn!(account, kind = kind.label(), n, threshold = cfg.threshold,
            "号池熔断计数:{} 分钟内第 {n} 个号出事", cfg.window_secs / 60),
        Noted::AddedToTrip(n) => tracing::error!(account, kind = kind.label(), n, "号池熔断中:又一个号出事"),
        Noted::WouldTrip(n) => tracing::warn!(account, kind = kind.label(), n,
            "号池熔断(observe):本应熔断,只记录不暂停"),
        Noted::Tripped(trip) => tracing::error!("号池熔断:已暂停接单,等待人工恢复。{}", describe(&trip)),
    }
}

/// 入口闸门:熔断中(且 enforce)返回回给客户的说明。
pub fn blocking_message() -> Option<String> {
    if !enabled() {
        return None;
    }
    let cfg = current();
    let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let message = st.blocking(&cfg).map(|trip| reject_message(trip, &cfg))?;
    st.blocked_total += 1;
    // 每条都打日志会刷屏;第一条和之后每 200 条打一次。
    if st.blocked_total % 200 == 1 {
        tracing::warn!(blocked_total = st.blocked_total, "号池熔断中:已拒绝请求(503)");
    }
    Some(message)
}

pub fn snapshot() -> serde_json::Value {
    let cfg = current();
    let mut v = STATE.lock().unwrap_or_else(|p| p.into_inner()).snapshot(&cfg);
    v["enabled"] = serde_json::json!(enabled());
    v
}

/// 与库对账并执行 IO(热设置同步循环里调用)。返回要发的通知文本(调用方异步发送)。
pub fn sync_with_store(store: &gw_store::SqliteStore) -> Option<String> {
    sync_breaker_with_store(&STATE, store)
}

/// 读库里两个键并合成一份视图(worker 对账和 router 管理接口共用)。
/// 文档坏了按空处理并记日志:worker 侧会用内存状态把坏文档覆盖掉。
pub fn load_stored(store: &gw_store::SqliteStore) -> anyhow::Result<StoredState> {
    let mut doc = match store.get_kv(STATE_KEY)? {
        Some(raw) => serde_json::from_str::<StoredState>(&raw).unwrap_or_else(|e| {
            tracing::error!("号池熔断:库内状态解析失败(按空处理): {e}");
            StoredState::default()
        }),
        None => StoredState::default(),
    };
    doc.reset_at = match store.get_kv(RESET_KEY)? {
        Some(raw) => serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v.get("reset_at").and_then(serde_json::Value::as_i64)),
        None => None,
    };
    Ok(doc)
}

/// 管理员恢复:只写 [`RESET_KEY`](worker 从不写它,恢复不会被覆盖)。
pub fn write_reset(store: &gw_store::SqliteStore, at: i64) -> anyhow::Result<()> {
    store.upsert_kv(RESET_KEY, &serde_json::json!({ "reset_at": at }).to_string())
}

fn sync_breaker_with_store(state: &Mutex<Breaker>, store: &gw_store::SqliteStore) -> Option<String> {
    let db = match load_stored(store) {
        Ok(doc) => doc,
        Err(e) => {
            // 读不到库就什么都不改:内存熔断照常挡,也不会因此误恢复。
            tracing::warn!("号池熔断:读库失败,本轮跳过对账: {e}");
            return None;
        }
    };
    let action = state.lock().unwrap_or_else(|p| p.into_inner()).reconcile(&db);
    match action {
        Reconcile::Nothing => None,
        Reconcile::Adopted(trip) => {
            tracing::error!("号池熔断:从库里恢复熔断状态,继续暂停接单。{}", describe(&trip));
            None
        }
        Reconcile::Reset => {
            tracing::warn!("号池熔断:管理员已恢复,重新接单(窗口已清空)");
            None
        }
        Reconcile::Persist { doc, notify } => {
            let raw = match serde_json::to_string(&doc) {
                Ok(raw) => raw,
                Err(e) => {
                    tracing::error!("号池熔断:状态序列化失败: {e}");
                    return None;
                }
            };
            if let Err(e) = store.upsert_kv(STATE_KEY, &raw) {
                tracing::error!("号池熔断:写库失败(内存仍熔断,下一轮重试): {e}");
                return None;
            }
            state.lock().unwrap_or_else(|p| p.into_inner()).mark_persisted(notify);
            let trip = Trip { at: doc.tripped_at.unwrap_or_default(), events: doc.events };
            notify.then(|| format!("caio 号池熔断,已暂停接单,请到后台查看并手动恢复。{}", describe(&trip)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;
    /// 2026-11-09 10:00 北京时间。
    const T0: i64 = 1_794_189_600_000;

    fn cfg(mode: PoolBreakerMode) -> PoolBreakerConfig {
        PoolBreakerConfig { mode, ..PoolBreakerConfig::default() }
    }

    #[test]
    fn third_distinct_account_in_window_trips() {
        let c = cfg(PoolBreakerMode::Enforce);
        let mut b = Breaker::default();
        assert_eq!(b.note(&c, "a", EventKind::Suspended, T0), Noted::Counted(1));
        assert_eq!(b.note(&c, "a", EventKind::Downgraded, T0 + MIN), Noted::Ignored, "同一个号窗口内只计一次");
        assert_eq!(b.note(&c, "b", EventKind::Downgraded, T0 + 2 * MIN), Noted::Counted(2));
        assert!(b.blocking(&c).is_none());
        let Noted::Tripped(trip) = b.note(&c, "c", EventKind::Suspended, T0 + 29 * MIN) else { panic!() };
        assert_eq!(trip.events.len(), 3);
        assert_eq!(b.blocking(&c).unwrap().at, T0 + 29 * MIN);
        assert_eq!(b.note(&c, "d", EventKind::Suspended, T0 + 30 * MIN), Noted::AddedToTrip(4));
    }

    #[test]
    fn events_older_than_window_expire() {
        let c = cfg(PoolBreakerMode::Enforce);
        let mut b = Breaker::default();
        b.note(&c, "a", EventKind::Suspended, T0);
        b.note(&c, "b", EventKind::Suspended, T0 + MIN);
        // a 已过 30 分钟窗口:b、c 两个,不熔断。
        assert_eq!(b.note(&c, "c", EventKind::Suspended, T0 + 30 * MIN), Noted::Counted(2));
        assert!(b.blocking(&c).is_none());
    }

    #[test]
    fn observe_and_off_never_block() {
        let mut b = Breaker::default();
        let o = cfg(PoolBreakerMode::Observe);
        b.note(&o, "a", EventKind::Suspended, T0);
        b.note(&o, "b", EventKind::Suspended, T0);
        assert_eq!(b.note(&o, "c", EventKind::Suspended, T0), Noted::WouldTrip(3));
        assert!(b.blocking(&o).is_none());
        let off = cfg(PoolBreakerMode::Off);
        assert_eq!(b.note(&off, "d", EventKind::Suspended, T0), Noted::Ignored);
    }

    #[test]
    fn switching_mode_off_releases_the_gate_without_forgetting() {
        let e = cfg(PoolBreakerMode::Enforce);
        let mut b = Breaker::default();
        for a in ["a", "b", "c"] {
            b.note(&e, a, EventKind::Suspended, T0);
        }
        assert!(b.blocking(&e).is_some());
        assert!(b.blocking(&cfg(PoolBreakerMode::Off)).is_none(), "应急口:切 off 立即放行");
        assert!(b.blocking(&e).is_some(), "切回 enforce 仍在熔断,只能手动恢复");
    }

    #[test]
    fn month_start_downgrades_do_not_count_but_suspends_do() {
        // 2026-12-01 00:30 北京 = 2026-11-30 16:30 UTC。
        let dec1 = 1_796_056_200_000;
        assert!(is_month_start_beijing(dec1));
        assert!(is_month_start_beijing(dec1 + 47 * 60 * MIN), "12-02 23:30 仍算月初");
        assert!(!is_month_start_beijing(dec1 + 48 * 60 * MIN), "12-03 00:30 不算");
        assert!(!is_month_start_beijing(dec1 - 60 * MIN), "11-30 23:30 不算");
        let c = cfg(PoolBreakerMode::Enforce);
        let mut b = Breaker::default();
        assert_eq!(b.note(&c, "a", EventKind::Downgraded, dec1), Noted::Ignored);
        assert_eq!(b.note(&c, "b", EventKind::Suspended, dec1), Noted::Counted(1));
    }

    #[test]
    fn trip_persists_then_reset_clears_window() {
        let c = cfg(PoolBreakerMode::Enforce);
        let mut b = Breaker::default();
        for a in ["a", "b", "c"] {
            b.note(&c, a, EventKind::Suspended, T0);
        }
        // 第一轮:库里没有 → 要求写库并通知。
        let Reconcile::Persist { doc, notify } = b.reconcile(&StoredState::default()) else { panic!() };
        assert!(notify && doc.is_tripped() && doc.events.len() == 3);
        b.mark_persisted(true);
        assert_eq!(b.reconcile(&doc), Reconcile::Nothing, "已同步");
        // 熔断中又一个号出事:要求重写库,但不重复通知。
        b.note(&c, "d", EventKind::Suspended, T0 + MIN);
        let Reconcile::Persist { doc, notify } = b.reconcile(&doc) else { panic!() };
        assert!(!notify && doc.events.len() == 4);
        b.mark_persisted(false);
        // 管理员恢复:写 reset_at。
        let reset = StoredState { reset_at: Some(T0 + 5 * MIN), ..doc };
        assert!(!reset.is_tripped());
        assert_eq!(b.reconcile(&reset), Reconcile::Reset);
        assert!(b.blocking(&c).is_none());
        // 窗口已清空:再出事一个号不会立刻熔断。
        assert_eq!(b.note(&c, "e", EventKind::Suspended, T0 + 6 * MIN), Noted::Counted(1));
        assert_eq!(b.reconcile(&reset), Reconcile::Nothing);
    }

    #[test]
    fn restart_adopts_trip_from_store() {
        let doc = StoredState {
            tripped_at: Some(T0),
            events: vec![Event { account: "a".into(), kind: EventKind::Suspended, at: T0 }],
            reset_at: Some(T0 - MIN),
        };
        let mut b = Breaker::default();
        assert!(matches!(b.reconcile(&doc), Reconcile::Adopted(_)));
        assert!(b.blocking(&cfg(PoolBreakerMode::Enforce)).is_some(), "重启不等于恢复");
        assert_eq!(b.reconcile(&doc), Reconcile::Nothing, "接过来的不重写、不重发通知");
    }

    #[test]
    fn new_trip_after_old_reset_is_persisted() {
        let c = cfg(PoolBreakerMode::Enforce);
        let old = StoredState { tripped_at: Some(T0 - 60 * MIN), events: vec![], reset_at: Some(T0 - 30 * MIN) };
        let mut b = Breaker::default();
        assert_eq!(b.reconcile(&old), Reconcile::Nothing);
        for a in ["a", "b", "c"] {
            b.note(&c, a, EventKind::Suspended, T0);
        }
        let Reconcile::Persist { doc, notify } = b.reconcile(&old) else { panic!() };
        assert!(notify && doc.is_tripped(), "旧的 reset_at 早于新熔断,不能把新熔断当成已恢复");
    }

    #[test]
    fn store_roundtrip_trip_reset_and_restart() {
        let store = gw_store::SqliteStore::open_in_memory().unwrap();
        let c = cfg(PoolBreakerMode::Enforce);
        let live = Mutex::new(Breaker::default());
        for a in ["a", "b", "c"] {
            live.lock().unwrap().note(&c, a, EventKind::Suspended, T0);
        }
        let text = sync_breaker_with_store(&live, &store).expect("第一次落库要发通知");
        assert!(text.contains("3 个号出事"), "{text}");
        assert!(sync_breaker_with_store(&live, &store).is_none(), "已同步,不重发");
        let doc: StoredState = serde_json::from_str(&store.get_kv(STATE_KEY).unwrap().unwrap()).unwrap();
        assert!(doc.is_tripped());

        // 模拟重启:新实例从库里接过熔断。
        let restarted = Mutex::new(Breaker::default());
        assert!(sync_breaker_with_store(&restarted, &store).is_none());
        assert!(restarted.lock().unwrap().blocking(&c).is_some());

        // 管理员恢复(router 写 RESET_KEY,与 admin 接口同口径)。
        assert!(doc.reset_at.is_none(), "worker 不往 STATE_KEY 里写 reset_at");
        write_reset(&store, T0 + MIN).unwrap();
        // worker 之后再落库(比如又补记了号)也覆盖不掉这次恢复:恢复在另一个键里。
        store.upsert_kv(STATE_KEY, &serde_json::to_string(&doc).unwrap()).unwrap();
        sync_breaker_with_store(&restarted, &store);
        sync_breaker_with_store(&live, &store);
        assert!(restarted.lock().unwrap().blocking(&c).is_none());
        assert!(live.lock().unwrap().blocking(&c).is_none());
    }

    #[test]
    fn two_workers_merge_events_instead_of_ping_pong() {
        let c = cfg(PoolBreakerMode::Enforce);
        let mut w0 = Breaker::default();
        for a in ["a", "b", "c"] {
            w0.note(&c, a, EventKind::Suspended, T0);
        }
        let Reconcile::Persist { doc, .. } = w0.reconcile(&StoredState::default()) else { panic!() };
        w0.mark_persisted(true);
        // w1 接过熔断,又补记 d 并落库。
        let mut w1 = Breaker::default();
        assert!(matches!(w1.reconcile(&doc), Reconcile::Adopted(_)));
        w1.note(&c, "d", EventKind::Suspended, T0 + MIN);
        let Reconcile::Persist { doc: doc1, .. } = w1.reconcile(&doc) else { panic!() };
        w1.mark_persisted(false);
        assert_eq!(doc1.events.len(), 4);
        // w0 看到库里多了 d:并入自己的,不再回写成 3 个。
        assert_eq!(w0.reconcile(&doc1), Reconcile::Nothing);
        assert_eq!(w0.blocking(&c).unwrap().events.len(), 4);
        assert_eq!(w1.reconcile(&doc1), Reconcile::Nothing);
    }

    #[test]
    fn corrupt_store_doc_never_releases_a_live_trip() {
        let store = gw_store::SqliteStore::open_in_memory().unwrap();
        store.upsert_kv(STATE_KEY, "{not json").unwrap();
        let c = cfg(PoolBreakerMode::Enforce);
        let live = Mutex::new(Breaker::default());
        for a in ["a", "b", "c"] {
            live.lock().unwrap().note(&c, a, EventKind::Suspended, T0);
        }
        sync_breaker_with_store(&live, &store);
        assert!(live.lock().unwrap().blocking(&c).is_some());
        let doc: StoredState = serde_json::from_str(&store.get_kv(STATE_KEY).unwrap().unwrap()).unwrap();
        assert!(doc.is_tripped(), "坏文档被覆盖成当前熔断");
    }

    /// 用实现本身回放真实封号序列(手动跑):
    /// `POOL_BREAKER_REPLAY=<文件> cargo test -p gw-app replay_real_trace -- --ignored --nocapture`,
    /// 文件每行 `账号\tunix秒`(每个号首次被封的时间,按时间排序)。
    #[test]
    #[ignore]
    fn replay_real_trace() {
        let path = std::env::var("POOL_BREAKER_REPLAY").expect("POOL_BREAKER_REPLAY");
        let c = cfg(PoolBreakerMode::Enforce);
        let mut b = Breaker::default();
        let mut trips = Vec::new();
        for line in std::fs::read_to_string(path).unwrap().lines() {
            let (account, secs) = line.split_once('\t').unwrap();
            let now = secs.trim().parse::<i64>().unwrap() * 1000;
            if let Noted::Tripped(trip) = b.note(&c, account, EventKind::Suspended, now) {
                trips.push(trip.at);
                // 回放里模拟「人看过后恢复」:触发即恢复,看下一次独立的熔断。
                let doc = StoredState { tripped_at: Some(trip.at), events: trip.events, reset_at: Some(trip.at) };
                assert_eq!(b.reconcile(&doc), Reconcile::Reset);
            }
        }
        let fmt = |ms: i64| {
            let secs = ms / 1000 + 8 * 3600;
            let (_, m, d) = civil_from_days(secs.div_euclid(86_400));
            format!("{m:02}-{d:02} {:02}:{:02}", secs.rem_euclid(86_400) / 3600, secs.rem_euclid(3600) / 60)
        };
        println!("熔断 {} 次: {:?}", trips.len(), trips.iter().map(|t| fmt(*t)).collect::<Vec<_>>());
    }

    #[test]
    fn titles_and_message() {
        assert!(is_paid_title("KIRO PRO+") && is_paid_title("KIRO POWER"));
        assert!(!is_paid_title("KIRO FREE") && !is_paid_title(""));
        assert!(is_free_title("KIRO FREE"));
        let trip = Trip { at: T0, events: vec![Event { account: "a".into(), kind: EventKind::Downgraded, at: T0 }] };
        let msg = reject_message(&trip, &cfg(PoolBreakerMode::Enforce));
        assert!(msg.contains("10:00") && msg.contains("30 分钟") && msg.contains("1 个号"), "{msg}");
        assert!(describe(&trip).contains("a(掉 FREE)"));
    }
}
