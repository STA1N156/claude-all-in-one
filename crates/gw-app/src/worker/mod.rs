//! worker 角色 —— 实际反代。
//!
//! 绑定固定出口(egress)+ 管理一组账号(account_group)。
//! 暴露 `/v1/messages`(和对外一样,绑 localhost 高位端口)+ `/health`。
//! 选号走组内 v52 会话亲和调度(见 [`scheduler`]):同会话钉同账号,最大化 Kiro
//! prefix cache 命中(缓存按上游账号隔离)。

mod gates;
pub(crate) mod pool_breaker;
mod cost_index;
mod scheduler;
pub(crate) use scheduler::PREFERRED_LOAD_KEY;
mod chat;
mod control;
mod errors;
mod request_log;
mod response;
mod startup;

use chat::*;
use control::*;
use errors::*;
use request_log::*;
use response::*;
use gates::{batch_guard_gate, pool_breaker_gate, refusal_guard_gate, refusal_guard_record, refusal_repeat_response, request_screen_gate};

pub use startup::run;

use std::path::Path;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use gw_core::account::Account;
use gw_core::config::{AccountsConfig, InstancesConfig, SystemConfig};
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::openai::{Wire, WireFrame};
use gw_core::provider::{
    AccountQuota, CallCtx, ChatRequest, ChatUsage, Provider, SseEvent, StreamItem,
};
use gw_core::store::{RequestLog, UsageRecord, UsageSink};
use gw_store::SqliteStore;

use crate::egress;
use crate::registry::Registry;
use scheduler::AccountScheduler;

/// 设置热调链路的**可观测状态**:worker 此刻真正在用的值 + 最近一次同步的结果。
///
/// ## 为什么需要它
///
/// 热调链路有两处**静默**失败:overlay 解析失败会每 30s 跳过一轮并**永久**保持旧配置
/// (只留一行 error 日志);本版本不认识的字段会被无声忽略(镜像比写库的那个旧时发生)。
/// 两者在面板上与「保存成功」完全无法区分,于是「我改了没生效」变成一个查不下去的问题 ——
/// 只能靠翻业务数据反推(如观察计费命中率有没有跳变),而那要等几分钟且需要有流量。
///
/// 把 worker **实际生效的值**回显出来,「没生效」与「生效了但我看早了」才分得开。
#[derive(Clone, Default)]
struct SettingsSync {
    /// 最近一次**成功应用**的 unix 秒。0 = 启动后一次都没成功过。
    applied_at: i64,
    /// 最近一次同步的错误(空 = 正常)。非空时 [`Self::applied_at`] 停在上次成功的时刻,
    /// 「现在 − applied_at」就是配置已经僵住多久 —— 这正是静默失效唯一的外部特征。
    error: String,
    /// 本版本不认识、已被忽略的 overlay 字段。非空 = 本进程镜像比写库的那个旧,
    /// 表现为「别的设置都生效,就这一个不生效」。
    unknown: Vec<String>,
    /// worker **应用之后**在用的关键热调值(不是 DB 里存的那份)。
    effective: serde_json::Value,
    /// 本 worker 的 provider 是否真的热应用 **provider 级**设置(缓存计费/图像/实验开关)。
    /// `false` 时 [`Self::effective`] 里那半边是「算得出但没生效」——必须让面板知道,
    /// 否则它会对着一份从未应用的值报「一致」。scheduler 那半边不受影响,一直是热的。
    provider_hot: bool,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// worker 此刻在用的**全部**热调值,键名与 `GET /settings` 的应然值逐字对齐。
///
/// 走 `SystemSettings::from_effective` 而不是手写字段清单,有三个理由:
///
/// 1. **手写清单会制造新的盲区**。漏掉的字段在面板上会得出「一致」这个错误结论,
///    而漏掉的恰恰是没人想到的那个(对抗审查三个视角都点了这条)。
/// 2. **键名天然对齐**。应然值就是同一个类型序列化出来的,前端逐键比对不需要映射表。
/// 3. **`default_proxy` 传 `None`**:代理 URL 里可能带 `user:pass@`,而这份数据要经
///    admin 上到浏览器。应然值那边有 `redact_proxy_url` 掩码,这边最省事也最稳的做法
///    是**根本不回显它** —— 少一个字段,少一条泄漏路径。
fn effective_view(eff: &gw_core::config::SystemConfig) -> serde_json::Value {
    serde_json::to_value(gw_core::config::SystemSettings::from_effective(eff, None))
        .unwrap_or(serde_json::Value::Null)
}

/// 掉进面板的错误串要先脱敏:serde 的类型错误会把出错的**值**嵌进消息里,
/// 而那个值可能是 `socks5://user:pass@host`(对抗审查 Architect#6)。
fn redact_err(msg: &str) -> String {
    crate::admin::redact_proxy_url(msg)
}

struct WorkerState {
    instance: u32,
    egress_desc: String,
    group: String,
    provider: Arc<dyn Provider>,
    /// 设置热调的可观测状态,见 [`SettingsSync`]。`/health` 直接回显。
    settings_sync: parking_lot::RwLock<SettingsSync>,
    /// 组内账号 v52 会话亲和调度器(选号 + 并发 + 冷却/禁用生命周期 + 凭证真值)。
    scheduler: AccountScheduler,
    /// per-account 刷新单飞锁:同一账号同时只允许一个 in-flight refresh(契约 H4)。
    /// 避免两个首请求并发刷新、互相覆盖 rolling refresh_token 导致一方 invalid_grant。
    refresh_locks: parking_lot::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// 401/403 被拒刷新的冷却表:account_id → 上次**成功**刷新时刻。
    /// 2026-09-09 实态:bot7-3/grokbot1 在一小时内被「401 → 同号刷新 → 再 401 → 再刷」
    /// 风暴反复打 oauth/token,最终 shouldLogout 终态死亡(疑似 RT 轮换重用检测/高频
    /// 风控)。token 本来 60 天有效,高频刷新零收益高风险 → 同账号 10 分钟内只允许
    /// 一次被拒刷新,再 401 直接拿新 token 重试,仍失败就交上层判死,绝不连环刷。
    rejection_refresh_at: parking_lot::Mutex<
        std::collections::HashMap<String, std::time::Instant>,
    >,
    /// usage 落库汇(#130)。打开失败时为 None(降级:usage 仅记日志不入库)。
    usage_sink: Option<Arc<dyn UsageSink>>,
    /// 在途异步 usage 落库登记(停机排空时等它们收尾)。
    pending_writes: Arc<PendingWrites>,
    /// 控制面库(账号事实源):刷新后回写 rolling refresh_token、30s 周期 sync 账号集。
    /// None = 库打开失败(降级:账号只来自 yaml 启动快照,改动需重启)。
    store: Option<Arc<SqliteStore>>,
    /// 账号配额缓存(account_id → (配额或 None, 上次**尝试**时刻)),TTL [`QUOTA_TTL`]。
    /// 关键:成功与失败都写入时刻 → 失败也受 TTL 节流(否则每次 /health 都重打上游,
    /// 审查 Skeptic#5)。value 的 `Option` 为 None 表示"查过但失败/无配额"。
    quota_cache: parking_lot::Mutex<
        std::collections::HashMap<String, (Option<AccountQuota>, std::time::Instant)>,
    >,
    /// 配额刷新在途去重:同账号同时只一个 getUsageLimits 在跑。
    quota_inflight: parking_lot::Mutex<std::collections::HashSet<String>>,
    /// 配额刷新并发上限信号量:防 /health 一次性给上百个账号同时刷新造成 stampede
    /// (审查 Architect#4)。后台任务先抢 permit 再打上游。
    quota_sem: Arc<tokio::sync::Semaphore>,
    /// 账号集同步串行锁(审查 Skeptic#1/Architect#1):30s 周期循环与 `/sync` 端点
    /// 并发跑 `sync_accounts_from_db` 时,若不串行"读 DB 快照 → 应用 scheduler",
    /// 旧快照可能最后应用,把刚导入的账号从内存移掉(等下轮才回来)。
    sync_lock: tokio::sync::Mutex<()>,
    /// **分组成员视图快照**:组名 → 该组在本 worker 名下的成员集与组内优先级。
    /// 与账号集同一轮同步(同一把 `sync_lock`),admin 改完可用 `/sync` 立即推送。
    ///
    /// 查不到的组名一律当**空视图**处理(→ 503),**绝不回落成全量池** —— 那等于
    /// 把一个受限分组的请求以全量权限放行,是静默提权。
    group_views: parking_lot::RwLock<std::collections::HashMap<String, scheduler::GroupView>>,
    /// worker 的 egress client(provider 已持有同一个;此处保留供诊断)。
    _client: reqwest::Client,
}

/// 配额缓存 TTL:被查看时每账号 ≤1 次/分钟打上游(只读 getUsageLimits,安全)。
const QUOTA_TTL: std::time::Duration = std::time::Duration::from_secs(60);
/// 配额刷新最大并发(stampede 护栏):上百账号同时被查看也只 N 个在打上游。
const QUOTA_MAX_CONCURRENCY: usize = 3;

/// 后台配额轮询一轮间隔下/上限(秒)。🟢 对齐 static_flow(240–300s + 抖动)。
/// worker 自身每轮兜底刷新一遍账号配额,不依赖 /health 被打。
/// ⚠️ 与 [`QUOTA_TTL`](=60s)耦合:轮询用 `QUOTA_POLL_MIN_SECS` 作 stale floor,必须 **>
/// `QUOTA_TTL`**,否则会和 /health 路径重复打点(被 /health 在 floor 内刷过的账号本轮才会跳过)。
/// 调整任一常量时检查二者关系(对抗审查 Architect#7)。
const QUOTA_POLL_MIN_SECS: u64 = 240;
const QUOTA_POLL_MAX_SECS: u64 = 300;
/// 启动后首轮配额 sweep 的短延迟:让启动握手/warm 先跑,但远早于 240s,避免"重启后无人看
/// dashboard 时前几分钟配额全空"的冷启动窗口(对抗审查 Skeptic#5)。
const QUOTA_POLL_WARMUP_SECS: u64 = 20;

/// 在 `[min, max]` 内取一个秒数。用 `SystemTime` 亚秒纳秒做熵,免引入 `rand` 依赖——
/// 抖动只为打散轮询时刻、避免精确固定间隔成为可识别指纹(防封),不需密码学强度随机。
fn jittered_secs(min: u64, max: u64) -> u64 {
    if min >= max {
        return min;
    }
    let span = max - min + 1;
    let entropy = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    min + (entropy % span)
}

impl WorkerState {
    /// 取该账号的 per-account 刷新锁(单飞:同账号同时只一个刷新;
    /// flush_dirty_extras / 配额回填同表取锁,互斥持久化)。
    fn refresh_lock(&self, account_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        lock_for(&self.refresh_locks, account_id)
    }

    /// 确保账号持有**未过期**的 access_token。
    ///
    /// 流程(对齐 kiro.rs try_ensure_token 双检锁 + expires_at 检查):
    /// 1. 有非空 access_token 且未过期/未临近过期 → 直接用(快路径,无锁);
    /// 2. 否则取 per-account 单飞锁,锁内**二次检查**(其他请求可能刚刷新好,从 scheduler
    ///    读最新),仍需刷新才真正 refresh_auth;
    /// 3. 刷新成功 → 回写 scheduler(rolling refresh_token 进选号池),返回新账号;
    /// 4. 刷新失败 → 原样返回 [`UpstreamError`](保留 kind:invalid_grant=TokenInvalid 永久,
    ///    网络/5xx/429=对应 transient,由调用方据 kind 决定禁用 vs 重试)。
    async fn ensure_credentialed(
        &self,
        account: Arc<Account>,
    ) -> Result<Arc<Account>, gw_core::error::UpstreamError> {
        if has_fresh_token(self.provider.as_ref(), &account) {
            return Ok(account);
        }
        self.refresh_locked(account).await
    }

    /// 强制刷新该账号一次:**无条件**走单飞锁 + 上游 refresh_auth,忽略 token 新鲜度。
    ///
    /// 人工“刷新 token”按钮用——操作者要真正向上游换一次,以验证 refresh_token 可用 /
    /// 轮换 rt 后立即拿到新 access_token。锁内取 scheduler 最新副本作基底(拿当前 rolling
    /// refresh_token、不抹并发 merge 字段),不因 fresh 提前返回。
    async fn force_refresh(
        &self,
        account: Arc<Account>,
    ) -> Result<Arc<Account>, gw_core::error::UpstreamError> {
        let lock = self.refresh_lock(&account.account_id);
        let _guard = lock.lock().await;
        let base = self
            .scheduler
            .account(&account.account_id)
            .unwrap_or(account);
        self.do_refresh_and_persist(base).await
    }

    /// chat 收 403 TokenInvalid 后的同号刷新:**仅当** scheduler 现存 access_token 仍是
    /// 这枚被拒 token(并发请求尚未替我刷过)才真打上游;否则直接返回最新副本。
    ///
    /// 判据是 **token 是否被换掉**,而非 expires_at 新鲜度——被拒 token 常仍“看似新”
    /// (吊销/时钟漂移),旧 `refresh_locked` 的 fresh 早返回会把它当好 token 返回、retry
    /// 必再失败。改用 CAS 后:同账号 N 个并发 403,第一个刷新换掉 token,其余进锁后发现
    /// token 已变 → 直接用新的、不再各自重刷,避免放大 token 交换 / rolling refresh_token
    /// (审查 Skeptic#2 / Architect#3)。
    async fn refresh_after_rejection(
        &self,
        account: Arc<Account>,
        rejected_token: Option<&str>,
    ) -> Result<Arc<Account>, gw_core::error::UpstreamError> {
        let lock = self.refresh_lock(&account.account_id);
        let _guard = lock.lock().await;
        let base = match self.scheduler.account(&account.account_id) {
            Some(fresh) => {
                // 他人已把 token 换成别的(不再是被拒的那枚)→ 用新的,别再刷。
                if fresh.extra_str("access_token") != rejected_token {
                    return Ok(fresh);
                }
                fresh
            }
            None => account,
        };
        // 刷新风暴防护(2026-09-09,bot7-3/grokbot1 疑似被连环刷新踢成 shouldLogout):
        // 同账号 10 分钟内已经成功刷过一次 → 现在这枚 token 就是最新货,再 401 说明
        // 不是"token 旧了"而是真被拒,连环刷 RT 只会加速死亡。跳过上上游,直接把最新
        // 副本交给调用方重试;仍 401 由上层计失败/判死。
        const REJECTION_REFRESH_COOLDOWN: std::time::Duration =
            std::time::Duration::from_secs(600);
        {
            let cooldown = self.rejection_refresh_at.lock();
            if let Some(t) = cooldown.get(&base.account_id) {
                if t.elapsed() < REJECTION_REFRESH_COOLDOWN {
                    tracing::warn!(
                        account = %base.account_id,
                        "被拒刷新冷却中(10min 内已刷过),跳过上上游、用最新 token 重试"
                    );
                    return Ok(base);
                }
            }
        }
        let account_id = base.account_id.clone();
        let out = self.do_refresh_and_persist(base).await;
        if out.is_ok() {
            self.rejection_refresh_at
                .lock()
                .insert(account_id, std::time::Instant::now());
        }
        out
    }

    /// 单飞锁内刷新:锁内二次检查(他人可能刚刷好)→ 仍需则 refresh_auth → 回写 scheduler。
    async fn refresh_locked(
        &self,
        account: Arc<Account>,
    ) -> Result<Arc<Account>, gw_core::error::UpstreamError> {
        let lock = self.refresh_lock(&account.account_id);
        let _guard = lock.lock().await;
        // 锁内取**最新**副本作为刷新基底:他人可能刚刷好(直接用);即便仍需刷新,
        // 基底也必须是 scheduler 真值——用调用方旧快照会 (a) 拿已作废的 rolling
        // refresh_token 去刷新,(b) 整块回写时抹掉期间 merge_extra 进来的字段
        // (如配额回填的 subscription_title)(审查②R Architect#2/Minimalist#2)。
        let base = match self.scheduler.account(&account.account_id) {
            Some(fresh) => {
                if has_fresh_token(self.provider.as_ref(), &fresh) {
                    return Ok(fresh);
                }
                fresh
            }
            None => account,
        };
        self.do_refresh_and_persist(base).await
    }

    /// 刷新 + 回写 + 持久化的共享尾段(`refresh_locked` / `force_refresh` 复用)。
    ///
    /// 调用方必须**已持有**该账号的 per-account 刷新锁,并已选定 `base`(scheduler 最新副本)。
    async fn do_refresh_and_persist(
        &self,
        base: Arc<Account>,
    ) -> Result<Arc<Account>, gw_core::error::UpstreamError> {
        let refreshed = match self.provider.refresh_auth(&base).await {
            Ok(a) => Arc::new(a),
            Err(e) => {
                // 孪生号自愈(2026-09-07):复制号( extra.twin_of → 源号 )与源号共享
                // 上游账号、各自持一条 token 链。源号先轮换后,复制号手里的旧 RT 过
                // 宽限期失效(cursor 实测:旧 RT 只在短时 leeway 内可用)。此时别直接
                // 判死 —— 若源号当前 RT 与本地不同(源号已轮换),借源号的链头重刷
                // 一次;源号也救不回才按原错误上抛(真封禁不受此机制影响)。
                if e.kind != gw_core::error::UpstreamErrorKind::TokenInvalid {
                    return Err(e);
                }
                match self.twin_resync(&base) {
                    Some((src, tw)) => match self.provider.refresh_auth(&tw).await {
                        Ok(a) => {
                            tracing::warn!(
                                account = %base.account_id,
                                twin_of = %src,
                                "孪生号本地 RT 已失效,借源号链头刷新成功(自愈)"
                            );
                            Arc::new(a)
                        }
                        Err(e2) => {
                            tracing::warn!(account = %base.account_id,
                                "孪生自愈失败(源号链头也刷不动): {e2}");
                            return Err(e); // 维持原判:永久失效
                        }
                    },
                    None => return Err(e),
                }
            }
        };

        let delta = refreshed.extra.iter().filter(|(k, v)| base.extra.get(*k) != Some(*v))
            .map(|(k, v)| (k.clone(), v.clone())).collect();
        self.scheduler.queue_extra_for_credentials(&base, delta);
        persist_pending_extra(&self.scheduler, self.store.as_deref(), &base.account_id, "认证刷新");
        Ok(self.scheduler.account(&base.account_id).unwrap_or(refreshed))
    }

    /// 孪生自愈的取链动作:`extra.twin_of` 指向的源号当前 refresh_token 与本号不同
    /// (源号已轮换)时,返回(源号 ID, 换了源号 token 链的本号候选副本)。
    /// 源号不存在/无 RT/与本号同一代 → None(没有可借的新链)。
    fn twin_resync(&self, base: &Arc<Account>) -> Option<(String, Arc<Account>)> {
        let src_id = base.extra_str("twin_of")?.to_string();
        if src_id == base.account_id {
            return None; // 防自指
        }
        let store = self.store.as_ref()?;
        let src = match store.get_account(&src_id) {
            Ok(Some(r)) => r,
            _ => return None,
        };
        let src_extra: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&src.extra).unwrap_or_default();
        let rt_of = |e: &serde_json::Map<String, serde_json::Value>| {
            e.get("refresh_token")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let src_rt = rt_of(&src_extra)?;
        let my_rt = base.extra_str("refresh_token").unwrap_or("");
        if src_rt == my_rt {
            return None; // 同一代链,借了也白借
        }
        let mut cand = (**base).clone();
        for k in ["refresh_token", "access_token", "expires_at"] {
            if let Some(v) = src_extra.get(k) {
                cand.extra.insert(k.to_string(), v.clone());
            }
        }
        Some((src_id, Arc::new(cand)))
    }

    /// 读账号配额缓存;命中(<TTL,含失败记录)直接返回,陈旧/缺失则触发**后台**刷新
    /// (去重)并返回当前缓存值(可能旧/None)。**不阻塞 /health**:只读查询在后台跑。
    fn quota_cached_or_refresh(self: &Arc<Self>, account_id: &str) -> Option<AccountQuota> {
        let now = std::time::Instant::now();
        let cached = self.quota_cache.lock().get(account_id).cloned();
        // fresh 看"上次尝试时刻"(成功或失败都算),失败也受 TTL 节流。
        // 例外:provider 的配额是本地廉价读(quota_is_local,如 dario 从流量捕获的内存快照)
        // 时**永不视为 fresh** → 每次都后台刷新,使刚捕获的快照下一轮(~一个前端轮询)就上面板,
        // 不被陈旧 None/旧值挡满一个 TTL(对抗审查 #1)。本地刷新无昂贵上游往返,节流无意义。
        let fresh = !self.provider.quota_is_local()
            && cached
                .as_ref()
                .is_some_and(|(_, at)| now.duration_since(*at) < QUOTA_TTL);
        if !fresh {
            self.spawn_quota_refresh(account_id.to_string());
        }
        cached.and_then(|(q, _)| q)
    }

    /// 后台刷新某账号配额(去重:同账号同时只一个在途)。无 tokio 上下文则跳过。
    fn spawn_quota_refresh(self: &Arc<Self>, account_id: String) {
        if !self.quota_inflight.lock().insert(account_id.clone()) {
            return; // 已有刷新在途。
        }
        if tokio::runtime::Handle::try_current().is_err() {
            self.quota_inflight.lock().remove(&account_id);
            return;
        }
        let st = self.clone();
        tokio::spawn(async move {
            st.refresh_quota_once(&account_id).await;
            st.quota_inflight.lock().remove(&account_id);
        });
    }

    /// 实际查一次配额并写缓存:抢并发 permit → 确保 token 有效(可能刷新,安全)→
    /// provider.account_quota。**无论成功失败都写"尝试时刻"**(失败写 None),让 TTL 同样
    /// 节流失败重试。只读 getUsageLimits + 刷新,绝不发 chat(见 no-chat-test-on-real-accounts)。
    async fn refresh_quota_once(self: &Arc<Self>, account_id: &str) {
        let _permit = match self.quota_sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return, // 信号量被关闭(停机),放弃。
        };
        let result = self.try_fetch_quota(account_id).await;
        // 顺手回填订阅档位:getUsageLimits 的 subscriptionTitle 是模型能力过滤
        // (FREE 不给 opus)的数据源——只导了 rt 的账号缺 subscription_title,
        // 首次配额查询后在此收敛。
        if let Ok(Some(q)) = &result {
            // **优先 `plan_tier`,`currency` 只是兜底**。历史实现只看 currency,而那是个
            // 展示标签:kiro 恰好把档位名放在那里,cursor 写死 `"USD"` —— 于是每个 cursor
            // 号的 `subscription_title` 都被填成 `"USD"`,档位永远看不出来,而且因为
            // `warm_subscription_titles` 只补「缺这个字段」的号,填错一次就再也不复查
            // (详见 `AccountQuota::plan_tier` 的文档)。kiro 侧 plan_tier 为 None,
            // 走 currency 分支,行为逐字节不变。
            if let Some(title) = q.plan_tier.as_ref().or(q.currency.as_ref()) {
                // 号池熔断:付费档掉成 FREE 也算一次「出事」(用户口径:掉 FREE 也算封号)。
                // 只在跑 Kiro 号池的 worker 上生效(`pool_breaker::enabled`)。
                let previous = self
                    .scheduler
                    .account(account_id)
                    .and_then(|a| a.extra_str("subscription_title").map(str::to_string));
                if previous.as_deref().is_some_and(pool_breaker::is_paid_title)
                    && pool_breaker::is_free_title(title)
                    && self.scheduler.is_in_service(account_id)
                {
                    tracing::warn!(account = %account_id, from = ?previous, to = %title, "账号档位掉成 FREE");
                    pool_breaker::note(account_id, pool_breaker::EventKind::Downgraded);
                }
                self.backfill_subscription_title(account_id, title).await;
            }
        }
        let value = match result {
            Ok(q) => q, // Some(配额) 或 None(账号已不在/无配额)
            Err(e) => {
                tracing::debug!(account = %account_id, "配额查询失败(节流后重试): {e}");
                None
            }
        };
        // 写时刻(成功 Some / 失败或无 None),fresh 判定据此节流。
        self.quota_cache
            .lock()
            .insert(account_id.to_string(), (value, std::time::Instant::now()));
    }

    /// 配额发现字段与认证刷新共用账号锁和待写增量入口。
    ///
    /// **只写档位,不碰组内优先级**(2026-10-28)。此前这里会按档位自动归位成员边
    /// (POWER → 0,其余 → 100),把个人 POWER 号抬进高层后 40 分钟封了 2 个,运维手改
    /// 回 100 又被下一轮配额轮询改回 0。优先级是业务决定,一律由运维(人工或管理接口)设定。
    async fn backfill_subscription_title(&self, account_id: &str, title: &str) {
        self.persist_extra_field(
            account_id,
            "subscription_title",
            serde_json::Value::String(title.to_string()),
            "订阅档位(模型过滤数据源)",
        )
        .await;
    }

    /// 单字段发现结果进入待写增量；成功只确认本次写出的版本。
    async fn persist_extra_field(
        &self,
        account_id: &str,
        key: &str,
        value: serde_json::Value,
        what: &str,
    ) {
        let lock = self.refresh_lock(account_id);
        let _guard = lock.lock().await;
        let Some(base) = self.scheduler.account(account_id) else { return };
        self.scheduler.queue_extra(&base, [(key.to_string(), value)].into());
        persist_pending_extra(&self.scheduler, self.store.as_deref(), account_id, what);
    }

    /// 确保企业/IdC 账号带 profileArn:缺失则 `ListAvailableProfiles` 发现并持久化,
    /// 返回带 profileArn 的更新副本(chat / 配额本次即用)。发现失败不阻断——让后续
    /// 上游 400「profileArn is required」自然报错(BadRequest,不惩罚账号)。
    /// social/builderid(固定兜底)与已有显式值的账号直接短路返回,无网络调用。
    async fn ensure_profile_arn(&self, account: Arc<Account>) -> Arc<Account> {
        // API Key 凭据不需要 profileArn:由 provider 自己的 discover_profile_arn 短路
        // (KiroProvider 对 ksk_ 直接回 Ok(None),不发 ListAvailableProfiles)。
        if account
            .extra_str("profile_arn")
            .is_some_and(|s| !s.trim().is_empty())
        {
            return account; // 已有显式值(含上次发现后持久化的)。
        }
        match self.provider.discover_profile_arn(&account).await {
            Ok(Some(arn)) => {
                self.persist_discovered_profile(&account, &arn, "profileArn 发现").await;
                // 取回带新 profile_arn 的副本供本次请求使用。
                self.scheduler.account(&account.account_id).unwrap_or(account)
            }
            Ok(None) => account, // 固定兜底或无可用 profile,按原样(headers 兜底处理)。
            Err(e) => {
                tracing::debug!(account = %account.account_id,
                    "ListAvailableProfiles 发现失败(不阻断): {e}");
                account
            }
        }
    }

    /// 预热订阅档位:对缺 `subscription_title` 的账号后台拉一次配额(getUsageLimits
    /// 只读,安全)。它是模型能力过滤的数据源——不预热的话,只导了 rt 的 FREE 号在
    /// 冷启动期间会被「未知放行」接 opus 然后 403(审查②R Architect#3)。
    /// 受 quota_sem(并发 3)+ TTL 节流,账号多也不会打爆;完整导入(KiroManager)
    /// 的账号自带该字段,不产生任何调用。
    fn warm_subscription_titles(self: &Arc<Self>) {
        for snap in self.scheduler.status_snapshot() {
            let Some(acc) = self.scheduler.account(&snap.account_id) else { continue };
            if acc.extra_str("subscription_title").is_none() {
                self.spawn_quota_refresh(snap.account_id);
            }
        }
    }

    /// 后台配额轮询的单轮(在轮询任务内**顺序内联**执行,不再 per-account spawn——避免上万
    /// 账号周期性创建上万个仅挂在 semaphore 上的 task,对抗审查 Architect#1)。
    ///
    /// 对**未禁用**且**距上次尝试 ≥ `QUOTA_POLL_MIN_SECS`** 的账号顺序刷新一次 getUsageLimits
    /// (只读,安全):
    /// - **跳过 disabled 账号**(人工隔离 / invalid_refresh_token / quota_exhausted / 配置禁用):
    ///   禁用即"不再使用",不该对其周期性打上游/试刷 token(对抗审查 Skeptic#4 / Architect#2 /
    ///   Minimalist#2)。
    /// - 与 /health 路径经 `quota_inflight` 去重(同账号同时只一个在途);被 /health
    ///   (TTL=[`QUOTA_TTL`]=60s)在 floor 内刷过的账号本轮自然跳过 → 不双倍打点。
    /// - 顺序处理:后台 sweep 不赶时间;`quota_sem` 仍护 /health 侧并发。
    async fn sweep_stale_quotas(self: &Arc<Self>) {
        let floor = std::time::Duration::from_secs(QUOTA_POLL_MIN_SECS);
        let now = std::time::Instant::now();
        let due: Vec<String> = self
            .scheduler
            .status_snapshot()
            .into_iter()
            .filter(|s| !s.disabled)
            .map(|s| s.account_id)
            .filter(|id| match self.quota_cache.lock().get(id) {
                Some((_, at)) => now.duration_since(*at) >= floor,
                None => true, // 从未查过 → 该刷。
            })
            .collect();
        for id in due {
            // 与 /health 触发去重:已在途则跳过,否则同账号并发两次 getUsageLimits。
            if !self.quota_inflight.lock().insert(id.clone()) {
                continue;
            }
            self.refresh_quota_once(&id).await;
            self.quota_inflight.lock().remove(&id);
        }
    }

    /// 拉一次配额(可能触发 token 刷新)。Ok(None) = 账号已不在/上游无配额。
    /// 错误保留 [`UpstreamError`](而非 anyhow 包装):按需验活端点要透出可分类的
    /// 状态码/kind(invalid_grant vs 网络抖动),后台轮询调用方只 log 不受影响。
    async fn try_fetch_quota(
        self: &Arc<Self>,
        account_id: &str,
    ) -> Result<Option<AccountQuota>, UpstreamError> {
        let Some(account) = self.scheduler.account(account_id) else {
            return Ok(None);
        };
        let account = self.ensure_credentialed(account).await?;
        // 企业/IdC 号 getUsageLimits 同样要求 profileArn:缺则先发现+持久化。
        let account = self.ensure_profile_arn(account).await;
        match self.provider.account_quota(&account).await {
            // TokenInvalid(401/403 bearer invalid)兜底一次,治两类常见误判:
            //  (1) at 已过期——导入号无 expires_at,has_fresh_token 误当新鲜、没预刷;
            //  (2) profileArn 套错——付费 builderid 号被免费层固定共享 ARN 短路,拿不到自己的 profile。
            // 故:强制刷新 at + (若仍无自带 profile_arn)强制重发现真实 profileArn 并持久化,
            // 再重试一次;仍失败才透出(真死号/真被封)。对齐 chat 路径 refresh_after_rejection 精神。
            // API Key 号排除:ksk_ 无可刷新(force_refresh 空操作)、也不需要 profileArn,
            // 兜底刷新+重试同 key 纯属放大配额轮询流量,直接透出错误(审查 r2 Skeptic#1)。
            Err(e)
                if matches!(e.kind, UpstreamErrorKind::TokenInvalid)
                    && !self.provider.refresh_exempt(&account) =>
            {
                let account = self.force_refresh(account).await?;
                // 付费号 profileArn 套错自愈:强制发现真实 ARN 并持久化(免费号/已有值不动)。
                // 与 chat 路径 403 兜底共用 discover_paid_profile_arn,逻辑单一来源不分叉。
                let account = self
                    .discover_paid_profile_arn(&account, "profileArn(配额 403 兜底强制发现)")
                    .await
                    .unwrap_or(account);
                self.provider.account_quota(&account).await
            }
            other => other,
        }
    }

    /// 付费号 profileArn 套错**自愈**:强制发现真实 profileArn 并持久化,返回更新后账号。
    ///
    /// 仅当 [`needs_profile_discovery`] 成立(账号无自带 profile_arn 且订阅为付费非 FREE)
    /// 才发现——免费号维持免费层固定共享 ARN,绝不被后台/chat 的例行 403 带进 force_discover
    /// 污染其真实 chat(resolve_profile_arn 被 chat 复用)。返回:
    /// - `Some(账号)`:发现到真实 arn 且已持久化(调用方改用它重试);
    /// - `None`:非付费 / 已有 arn / 发现失败(调用方维持原账号)。
    ///
    /// 配额路径(`try_fetch_quota`)与 chat 路径 403 兜底共用本方法,避免逻辑分叉。
    async fn discover_paid_profile_arn(
        self: &Arc<Self>,
        account: &Arc<Account>,
        trigger: &str,
    ) -> Option<Arc<Account>> {
        if !needs_profile_discovery(account) {
            return None;
        }
        match self.provider.force_discover_profile_arn(account).await {
            Ok(Some(arn)) => {
                self.persist_discovered_profile(account, &arn, trigger).await;
                Some(
                    self.scheduler
                        .account(&account.account_id)
                        .unwrap_or_else(|| account.clone()),
                )
            }
            // 免费号发现不到(空/错)/ 发现调用失败 → None,维持固定兜底靠刷新后的新 at 过。
            _ => None,
        }
    }

    /// ARN 与派生服务区共享查询时的认证基准。管理员换号后，旧身份的发现结果整批拒绝。
    async fn persist_discovered_profile(&self, base: &Account, arn: &str, context: &str) {
        let lock = self.refresh_lock(&base.account_id);
        let _guard = lock.lock().await;
        let mut delta = std::collections::BTreeMap::from([("profile_arn".into(), serde_json::json!(arn))]);
        if let Some(region) = region_correction_from_arn(self.provider.as_ref(), arn, base.extra_str("region")) {
            delta.insert("region".into(), serde_json::json!(region));
            delta.insert("api_region".into(), serde_json::json!(region));
        }
        self.scheduler.queue_extra_for_credentials(base, delta);
        persist_pending_extra(&self.scheduler, self.store.as_deref(), &base.account_id, context);
    }

    /// 从 DB 重读组内账号集并同步进 scheduler —— 30s 周期循环与 `/sync` 立即同步
    /// **共用本实现**(勿另写一份,语义漂移=同步行为分叉)。先冲刷上轮回写失败的
    /// 脏 extra,再差量同步账号集。返回 (added, removed);无库/读库失败 → None。
    /// 全程持 `sync_lock`:保证"读快照→应用"原子,后读的快照必然后应用。
    async fn sync_accounts_from_db(self: &Arc<Self>) -> Option<(usize, usize)> {
        let store = self.store.clone()?;
        let _serialized = self.sync_lock.lock().await;
        // 先冲刷 suspend 生命周期落库队列(最新状态覆盖旧的;退役条目同事务置
        // disabled=1)。**条件写**:Ok(false) = epoch 竞态落败(人工恢复更新),
        // 不回队——紧随其后的对账会采用库内真相;Err 才回队下轮重试。
        for (id, (lc, set_disabled)) in self.scheduler.take_pending_lifecycles() {
            match store.persist_suspend_lifecycle(&id, &lc, set_disabled) {
                Ok(true) => {
                    if set_disabled {
                        tracing::warn!(account = %id, "suspend 自动退役已落库(disabled=1)");
                    }
                }
                Ok(false) => {
                    tracing::debug!(account = %id, "suspend 生命周期落库 epoch 落败,由对账收敛");
                }
                Err(e) => {
                    tracing::error!(account = %id, "suspend 生命周期落库失败,下轮 sync 重试: {e}");
                    self.scheduler.requeue_lifecycle(&id, (lc, set_disabled));
                }
            }
        }
        // 对账:库内 epoch 更新(=人工恢复)的号,采用库内真相重水合运行态——
        // 运行态冷却的号 DB disabled 恒 0,配置翻转路径感知不到人工恢复,靠这里送达。
        match store.load_suspend_lifecycles() {
            Ok(rows) => self.scheduler.adopt_db_lifecycles(&rows),
            Err(e) => tracing::warn!("suspend 生命周期对账读库失败,本轮跳过: {e}"),
        }
        // 先重试上轮回写失败的 extra(脏账号),失败下轮再试。
        flush_dirty_extras(&self.scheduler, &store, &self.refresh_locks, "sync 重试").await;
        // provider 侧捕获的外部 token 轮换(cursor CLI 自刷新)CAS 落库 —— 不做的话
        // 号库里是已作废的旧 rt,下次 OAuth 刷新 invalid_grant,号被误判死。
        adopt_provider_token_updates(&self.provider, &self.scheduler, &store, &self.refresh_locks)
            .await;
        // 成员边与账号集**先都读出来,两个都成功才发布**(对抗审查 Skeptic#3)。
        // 分别读、分别发布会留下无限期的撕裂态:membership 成功而账号读失败 → 新视图
        // 立即生效但账号快照停在上一轮;反过来 membership 读失败而账号成功 → **已被撤销
        // 的成员边继续授权**,这是提权方向,尤其危险。任一失败就整轮跳过。
        let revisions = self.scheduler.extra_revisions();
        let (memberships, accounts) = match (
            store.load_group_memberships(&self.group),
            store.load_owned_accounts(&self.group),
        ) {
            (Ok(m), Ok(a)) => (m, a),
            (m, a) => {
                let err = m.err().map(|e| e.to_string()).or_else(|| a.err().map(|e| e.to_string()));
                tracing::warn!("sync 读库失败,整轮跳过(不发布半份快照): {err:?}");
                return None;
            }
        };
        // 发布顺序:**先视图后账号**。这样被撤销的成员边立刻失效(安全方向),而新进账号
        // 至多短暂不可选(仅是暂时不可用,不是提权)。反过来发布会让撤销晚一步生效。
        *self.group_views.write() = memberships
            .into_iter()
            .map(|(g, rank)| (g.clone(), scheduler::GroupView::new(g, rank)))
            .collect();
        let accounts =
            filter_by_provider(accounts.into_iter().map(Arc::new).collect(), self.provider.family());
        let out = self.scheduler.sync_accounts_since(accounts, &revisions);
        if out.added + out.removed > 0 {
            tracing::info!(added = out.added, removed = out.removed, "账号集已按 DB 同步");
            // 新进账号若缺订阅档位,预热配额查询补齐(模型过滤数据源)。
            self.warm_subscription_titles();
        }
        Some((out.added, out.removed))
    }
}

/// 配额 → JSON(null = 尚无缓存/查询中或查询失败,前端显示 —)。
fn quota_to_json(q: Option<AccountQuota>) -> serde_json::Value {
    match q {
        Some(q) => serde_json::json!({
            "used": q.used,
            "limit": q.limit,
            "remaining": q.remaining,
            "percent_used": q.percent_used,
            "label": q.currency,
            // 账号档位("FREE"/"PAID"/provider 档位名;null = 判不出来)。
            // 面板要能一眼看出号是不是被上游降级了 —— FREE 号连额度字段都没有,
            // 光看 used/limit 是一片空白,分不清"降级"和"查询失败"。
            "plan_tier": q.plan_tier,
            // 多窗口利用率(dario 的 5h/7d);Kiro 为空数组,前端据此分流显示。
            "windows": q.windows.iter().map(|w| serde_json::json!({
                "label": w.label,
                "percent_used": w.percent_used,
                "reset_at": w.reset_at,
            })).collect::<Vec<_>>(),
            // 超额(on-demand)额度:null = 该 provider 无此概念 / 未查到,前端显示 —。
            "on_demand": q.on_demand.as_ref().map(|od| serde_json::json!({
                "enabled": od.enabled,
                "limit": od.limit,
                "used": od.used,
                "unlimited": od.unlimited,
            })),
        }),
        None => serde_json::Value::Null,
    }
}

/// 是否需要为该账号强制发现真实 profileArn:无自带 profile_arn(空/缺)**且**订阅为付费
/// (subscription_title 存在且不含 FREE)。付费 builderid 号有自己的 profile,被免费层固定
/// 共享 ARN 短路后 getUsageLimits/chat 都 403;免费号(或订阅档未回填)一律不发现,维持
/// 共享 ARN,绝不误污染健康免费号。付费闸门 = 免费/付费结构同构下唯一安全的区分维度。
fn needs_profile_discovery(account: &Account) -> bool {
    account
        .extra_str("profile_arn")
        .map_or(true, |s| s.trim().is_empty())
        && account
            .extra_str("subscription_title")
            .is_some_and(|t| !t.to_uppercase().contains("FREE"))
}

/// 若新发现的 profileArn 揭示了与当前配置**不同**的已知服务区,返回该区域(供调用方
/// 据此修正 region + api_region)。ARN 区域段不在 `Provider::region_from_profile_arn`
/// 认可的已知服务区清单内,或与当前一致 → `None`(不改动)。纯函数,不发网络请求。
///
/// 大小写不敏感比较:`current_region` 是 DB 里的原始字符串,admin `PATCH /accounts/{id}`
/// 的 `extra` 整块替换不做区域大小写归一,可能存进混合大小写值(如 "US-EAST-1")。若直接
/// `!=` 会把等价值误判成"不同"、触发多余的重写(无害但吵)——按 discovered 的归一口径比较。
fn region_correction_from_arn(
    provider: &dyn Provider,
    arn: &str,
    current_region: Option<&str>,
) -> Option<&'static str> {
    let discovered = provider.region_from_profile_arn(arn)?;
    let already_matches = current_region.is_some_and(|r| r.eq_ignore_ascii_case(discovered));
    (!already_matches).then_some(discovered)
}

/// 账号是否持有未过期(且非临近过期)的 access_token。
///
/// 无 access_token → false(需刷新)。有 token 但无 expires_at → 视为有效(无从判断,
/// 沿用旧行为;真过期会被上游 403 触发 force_refresh 兜底)。有 expires_at → 距现在
/// < 60s 视为临近过期需提前刷新(对齐 kiro.rs cred_expiring_soon)。
/// 本 worker 绝不能刷新的号:
/// - Kiro ksk_ API Key:长期有效、无刷新概念;
/// - claude-dario:令牌已交接给该号自己的 dario 进程独占刷新(见 gw_dario::supervisor)。
///   caio 再刷新同一条 refresh token 会被 Anthropic 判为复用、整族作废。
///
/// 两类号遇到 TokenInvalid 都直接按失效处理(不「刷新后重试」)。claude-dario 只有在
/// dario-supervisor 判定需要重新登录时才会返回 TokenInvalid。
fn has_fresh_token(provider: &dyn Provider, account: &Account) -> bool {
    // 不可刷新的号(API Key / dario 托管,见 Provider::refresh_exempt)永远视为"新鲜",
    // 绝不触发 refresh_auth。
    if provider.refresh_exempt(account) {
        return true;
    }
    let Some(tok) = account.extra_str("access_token") else {
        return false;
    };
    if tok.is_empty() {
        return false;
    }
    match account.extra_str("expires_at").and_then(parse_rfc3339_unix) {
        Some(exp) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            exp - now > 60 // 留 60s 余量提前刷新
        }
        None => true, // 无过期信息 → 当作有效,靠 403 兜底
    }
}

/// 解析 "YYYY-MM-DDTHH:MM:SSZ"(token.rs 写入的格式)为 Unix 秒。失败返回 None。
fn parse_rfc3339_unix(s: &str) -> Option<i64> {
    // 仅支持本项目 token.rs 产出的 UTC "Z" 形态(纯算术,不引 chrono)。
    let s = s.strip_suffix('Z').unwrap_or(s);
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hh: i64 = t.next()?.parse().ok()?;
    let mm: i64 = t.next()?.parse().ok()?;
    let ss: i64 = t.next().unwrap_or("0").parse().ok()?;
    // civil → days since epoch(Howard Hinnant 算法,与 token.rs format_unix_utc 互逆)。
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// 在途异步落库登记:SSE 收尾的 usage 落库是 Drop 里 detach 的 spawn 任务,
/// graceful shutdown 只等响应体、不等这些任务——停机时经 wait_idle 等到清零
/// (或超时),否则最后一批 usage/quota 增量会随 runtime 关闭静默丢失(审查 Skeptic#2)。
struct PendingWrites {
    count: std::sync::atomic::AtomicUsize,
    notify: tokio::sync::Notify,
}

/// RAII 登记凭据:创建即计数 +1,Drop -1 并唤醒等待者。
struct PendingWriteGuard(Arc<PendingWrites>);

impl PendingWrites {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            count: std::sync::atomic::AtomicUsize::new(0),
            notify: tokio::sync::Notify::new(),
        })
    }

    fn enter(self: &Arc<Self>) -> PendingWriteGuard {
        self.count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        PendingWriteGuard(self.clone())
    }

    /// 等到无在途落库或超时;返回最终是否清零。
    async fn wait_idle(&self, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.count.load(std::sync::atomic::Ordering::Acquire) == 0 {
                return true;
            }
            let notified = self.notify.notified();
            // 注册 waker 后复查,封住「最后一个 guard 在两步之间 drop」的丢唤醒窗口。
            if self.count.load(std::sync::atomic::Ordering::Acquire) == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.count.load(std::sync::atomic::Ordering::Acquire) == 0;
            }
        }
    }
}

impl Drop for PendingWriteGuard {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
            self.0.notify.notify_waiters();
        }
    }
}

/// per-account 刷新锁表的取锁(WorkerState::refresh_lock 与 flush 共用同一张表,
/// 保证 flush 与 token 刷新/配额回填互斥)。
type RefreshLocks = parking_lot::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

fn lock_for(locks: &RefreshLocks, account_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = locks.lock();
    map.entry(account_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// 持账号锁重读并重试待写字段；不使用入口时的完整账号快照。
/// 周期同步和停机共用，失败保留待写项供下次重试。
async fn flush_dirty_extras(
    scheduler: &AccountScheduler,
    store: &SqliteStore,
    refresh_locks: &RefreshLocks,
    context: &str,
) {
    let dirty_ids: Vec<String> = scheduler
        .dirty_accounts()
        .iter()
        .map(|a| a.account_id.clone())
        .collect();
    for id in dirty_ids {
        let lock = lock_for(refresh_locks, &id);
        let _guard = lock.lock().await;
        // 锁内重读:并发刷新可能已替换副本/自行落库清脏。
        if !scheduler.is_extra_dirty(&id) {
            continue;
        }
        persist_pending_extra(scheduler, Some(store), &id, context);
    }
}

/// 调用者持账号刷新锁。确认整批实际结果，失败保留待写项；删除不是写入成功。
fn persist_pending_extra(scheduler: &AccountScheduler, store: Option<&SqliteStore>, id: &str, context: &str) {
    let changes = scheduler.pending_extra(id);
    if changes.is_empty() { return; }
    let Some(store) = store else {
        if let Some(account) = scheduler.account(id) {
            scheduler.acknowledge_extra(id, &changes, &account.extra);
        }
        return;
    };
    match store.apply_account_extra_changes(id, &changes) {
        Ok(Some(actual)) => scheduler.acknowledge_extra(id, &changes, &actual),
        Ok(None) => tracing::warn!(account = %id, "{context}: 账号已删除，等待同步移除内存待写项"),
        Err(e) => tracing::warn!(account = %id, "{context}: 增量落库失败，保留待写项: {e}"),
    }
}

/// 接收 provider 的凭据与普通字段增量。凭据先按 access_token / expires_at 筛旧，
/// 普通字段独立保留，再经同一待写入口进行 SQLite 条件合并。
async fn adopt_provider_token_updates(
    provider: &Arc<dyn Provider>,
    scheduler: &AccountScheduler,
    store: &SqliteStore,
    refresh_locks: &RefreshLocks,
) {
    for (account_id, mut delta) in provider.poll_token_updates() {
        let lock = lock_for(refresh_locks, &account_id);
        let _guard = lock.lock().await;
        let Some(base) = scheduler.account(&account_id) else { continue };
        let stale_credentials = delta.get("access_token").and_then(|v| v.as_str()).is_some_and(|at| {
            base.extra_str("access_token") == Some(at) || match (
                delta.get("expires_at").and_then(|v| v.as_str()), base.extra_str("expires_at"),
            ) { (Some(inc), Some(cur)) => inc <= cur, _ => false }
        });
        if stale_credentials {
            // 凭据回声与普通字段分开：不能连带丢掉刚发现的 agent id。
            delta.retain(|key, _| !gw_core::account::CREDENTIAL_EXTRA_KEYS.contains(&key.as_str()));
        }
        scheduler.queue_extra(&base, delta);
        persist_pending_extra(scheduler, Some(store), &account_id, "provider 增量");
    }
}

/// 过滤掉与本 worker provider 家族不符的账号(单 provider 组模型;
/// provider 为空 = 跟随组家族,放行)。
fn filter_by_provider(accounts: Vec<Arc<Account>>, family: &str) -> Vec<Arc<Account>> {
    accounts
        .into_iter()
        .filter(|a| {
            let ok = a.provider.is_empty() || a.provider == family;
            if !ok {
                tracing::warn!(account = %a.account_id, provider = %a.provider, family,
                    "账号 provider 与本 worker 家族不符,跳过(不会被服务)");
            }
            ok
        })
        .collect()
}

/// listen 地址是否绑 loopback(127.0.0.1 / ::1 / localhost)。
fn is_loopback_listen(listen: &str) -> bool {
    if let Ok(addr) = listen.parse::<std::net::SocketAddr>() {
        return addr.ip().is_loopback();
    }
    match listen.rsplit_once(':') {
        Some((host, _)) => {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false)
        }
        None => false,
    }
}

/// 上游错误 → 对外 HTTP 状态码。**唯一映射点**:响应与请求日志都走它,杜绝两处漂移
/// (日志里记 502 而客户端实收 529 会让排查彻底跑偏)。
///
/// - `BadRequest` / `ModelNotAvailable` → 400(客户端可解:改请求 / 换模型 / 升级订阅)
/// - `Overloaded` → **529**(Anthropic 官方过载语义:Claude Code 与各家 SDK 据此自动重试,
///   且不会把渠道判死。语义上这也确实不是"坏网关",是上游没容量)
/// - 其余 → 502
#[cfg(test)]
mod error_shape_tests;

#[cfg(test)]
mod tests;
