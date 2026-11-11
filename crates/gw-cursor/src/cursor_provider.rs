//! CursorProvider 的内部方法(令牌、出口客户端、配置版本等)。
//!
//! 2026-11-10 由 lib.rs 拆出(只搬不改行为)。

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

/// 缓存里一个号的 client(一条连接池),带代际号(见 [`ConnHandle::evict`])。
pub(crate) struct CachedClient {
    pub(crate) client: reqwest::Client,
    pub(crate) gen: u64,
}

/// 代际号发号器:每新建一个号的 client 取一个,全进程唯一递增。
static CLIENT_GEN: AtomicU64 = AtomicU64::new(1);

/// 一次请求用的是哪个号的哪一代连接。流里出现连接卡死时,据此把这个号的旧连接从缓存里
/// 丢掉,下一个请求重建(见 `provider::heal_stalled_connection`)。
pub(crate) struct ConnHandle {
    map: Arc<Mutex<HashMap<String, CachedClient>>>,
    key: String,
    gen: u64,
    pub(crate) account_id: String,
}

impl ConnHandle {
    /// 丢弃这个号缓存的连接(出口不变,下一个请求按同一个代理重建)。返回是否真的丢了。
    ///
    /// **只在缓存里仍是本请求用的那一代时才丢**:一条坏连接上往往有好几个请求同时卡住,
    /// 第一个丢掉后已经重建的新连接,不能被后面陆续超时的旧请求再丢一遍。
    pub(crate) fn evict(&self) -> bool {
        let mut m = self.map.lock().unwrap_or_else(|p| p.into_inner());
        match m.get(&self.key) {
            Some(c) if c.gen == self.gen => {
                m.remove(&self.key);
                true
            }
            _ => false,
        }
    }
}

impl CursorProvider {
    pub fn new(cfg: CursorConfig) -> Self {
        // 默认构造路径也禁止 box 鉴权头随跳转外泄。
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().last().and_then(|u| u.host_str())
                    .is_some_and(|h| h.ends_with(".cursorvm.com")) {
                    attempt.stop()
                } else {
                    reqwest::redirect::Policy::default().redirect(attempt)
                }
            }))
            .build()
            .expect("构造 Cursor 默认 HTTP client");
        Self::with_client(cfg, client)
    }

    /// 注入固定出口 client；调用方须禁止 *.cursorvm.com 的 HTTP 跳转，
    /// 防止 box 自定义网络鉴权头外泄。生产 egress::build_client 已满足此约束。
    pub fn with_client(cfg: CursorConfig, egress_client: reqwest::Client) -> Self {
        chat::warn_if_dump_enabled();
        Self {
            cfg,
            tuning: RunTuning::faithful(),
            egress_client,
            proxy_clients: Arc::new(Mutex::new(HashMap::new())),
            config_cache: Mutex::new(HashMap::new()),
            config_gates: Mutex::new(HashMap::new()),
            conversations: Arc::new(ConvRegistry::from_env()),
            assets: Arc::new(AssetStore::default()),
            notices: Arc::new(TruncationNotices::default()),
            nozdr_consented: Mutex::new(std::collections::HashSet::new()),
            shadow: Arc::new(DescriptorShadow::default()),
            sections: Arc::new(ContentSections::default()),
        }
    }

    /// 覆盖请求形状(协议试错用;生产别调)。
    pub fn with_tuning(mut self, tuning: RunTuning) -> Self {
        self.tuning = tuning;
        self
    }

    pub fn from_config(
        cfg: &serde_json::Value,
        egress_client: reqwest::Client,
    ) -> anyhow::Result<Arc<dyn Provider>> {
        Ok(Arc::new(Self::with_client(
            CursorConfig::from_cfg(cfg),
            egress_client,
        )))
    }

    /// 该账号应当使用的 HTTP client。**fail-closed。**
    ///
    /// **三条上游路径(chat / GetServerConfig / refresh)必须全部经由本方法取 client。**
    /// 刷新走了别的出口而发包走代理,等于把两个 IP 绑到同一个号上,是已知的关联维度。
    ///
    /// ## 为什么代理构造失败必须拒绝,而不是回退默认出口
    ///
    /// 这里曾经是「构造失败就 warn 一声回退 `egress_client`」,理由是"配置写错不该让号
    /// 彻底不可用"。那个权衡是反的:账号配了 `proxy` 就是在声明"我要独占这个出口",
    /// 回退等于**把本该互相隔离的多个号并到同一个 IP 上**,而这是已实测的封号维度
    /// (同出口关联封禁 59.5% vs 独立代理 0%,见记忆 caio-egress-silent-direct-bug)。
    /// 一个号暂时不可用是**安全的失败**;一批号被关联封掉不是。
    ///
    /// gw-dario 早先的对抗审查已经就同一问题定过案(`client_for_proxy` 明确 fail-closed),
    /// 这里对齐它 —— 同一个安全边界不该因为 provider 不同就换个结论。
    pub(super) fn client_for(&self, account: &Account) -> Result<reqwest::Client, UpstreamError> {
        self.client_and_handle(account).map(|(c, _)| c)
    }

    /// 同 [`Self::client_for`],另外交出这次用的连接句柄(配了代理的号才有;默认出口没有),
    /// 供 `chat()` 在连接卡死时把它丢掉重建。
    pub(super) fn client_and_handle(
        &self,
        account: &Account,
    ) -> Result<(reqwest::Client, Option<ConnHandle>), UpstreamError> {
        let proxy = account
            .extra_str("proxy")
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(url) = proxy else {
            return Ok((self.egress_client.clone(), None));
        };
        // 缓存键 = 账号 + 代理:**每个号一个 client(一条连接)**,出口仍是该号自己配的代理。
        // 2026-11-10 以前按代理 URL 缓存 → 同出口的几个号共用一条 HTTP/2 连接,一个大流量号
        // 把连接堵住,同出口所有号的流一起卡成「90s 只有心跳」(出口 8 三个号 30~40% 卡住、
        // 只有两个小号的出口 7 为 0,重启重建连接后立刻归零);多号令牌走同一条连接也比
        // 只共用 IP 多一个关联维度。
        let key = proxy_client_key(&account.account_id, url);
        let handle = |gen: u64| ConnHandle {
            map: self.proxy_clients.clone(),
            key: key.clone(),
            gen,
            account_id: account.account_id.clone(),
        };
        // poison 用 into_inner 恢复:这把锁只护一张纯缓存,毒化不代表数据不可用,
        // 而 unwrap 会让此后每个请求都 panic(对齐 gw-dario 的写法)。
        {
            let cache = self.proxy_clients.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(c) = cache.get(&key) {
                return Ok((c.client.clone(), Some(handle(c.gen))));
            }
        }
        match build_proxy_client(url) {
            Ok(c) => {
                let gen = CLIENT_GEN.fetch_add(1, Ordering::Relaxed);
                let mut cache = self.proxy_clients.lock().unwrap_or_else(|p| p.into_inner());
                // 并发下别的请求可能刚建好同一个号的:用先进缓存的那个,两边拿到同一条连接。
                let e = cache.entry(key.clone()).or_insert(CachedClient { client: c, gen });
                Ok((e.client.clone(), Some(handle(e.gen))))
            }
            Err(e) => {
                tracing::error!(%url, account = %account.account_id,
                    "cursor 账号的出口代理无法构造,拒绝使用该号(绝不回退默认出口): {e}");
                Err(UpstreamError::new(
                    UpstreamErrorKind::BadRequest,
                    format!(
                        "cursor 账号 {} 的出口代理配置非法,拒绝发包(回退默认出口=关联封号风险)",
                        account.account_id
                    ),
                ))
            }
        }
    }

    /// 取账号 token(必填)。
    pub(super) fn token_of(account: &Account) -> Result<String, UpstreamError> {
        account
            .extra_str("access_token")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| UpstreamError::bad_request("cursor 账号缺少 access_token"))
    }

    pub(super) fn opt_str(account: &Account, key: &str) -> Option<String> {
        account
            .extra_str(key)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    /// machineId:显式 > sha256hex(token) 派生。
    pub(super) fn machine_id_of(account: &Account, token: &str) -> String {
        Self::opt_str(account, "machine_id").unwrap_or_else(|| wire::default_machine_id(token))
    }

    /// macMachineId:显式 > 派生。**不返回 `None`** —— 真客户端的 checksum 恒含它。
    pub(super) fn mac_machine_id_of(account: &Account, token: &str) -> String {
        Self::opt_str(account, "mac_machine_id")
            .unwrap_or_else(|| wire::default_mac_machine_id(token))
    }

    pub(super) fn timezone_of(account: &Account) -> String {
        Self::opt_str(account, "timezone").unwrap_or_else(|| DEFAULT_TIMEZONE.to_string())
    }

    /// 解析当前请求应回显的 `config_version`:
    /// 显式配置 > 缓存内未过期值 > 现调 GetServerConfig。
    pub(super) async fn resolve_config_version(
        &self,
        account: &Account,
        client: &reqwest::Client,
        token: &str,
        machine_id: &str,
        mac_machine_id: &str,
    ) -> Result<String, UpstreamError> {
        if let Some(explicit) = Self::opt_str(account, "config_version") {
            return Ok(explicit);
        }
        let key = config_cache_key(account, token, machine_id);
        match self.cached_config_version(&key) {
            CacheLook::Fresh(v) => return Ok(v),
            CacheLook::RecentlyFailed => {
                // 负缓存命中:60 秒内刚失败过,别再排队各付一次 5–6 秒的超时。
                return Err(UpstreamError::network(
                    "cursor config_version 近期取失败(负缓存未过期),未发出推理请求".to_string(),
                ));
            }
            CacheLook::Miss => {}
        }

        // ── single-flight(按缓存键,不是全局)────────────────────────────────
        // 这一趟要 5–6 秒。没有闸门时,一次交互并发来的 N 个请求会各打一次
        // (实测 opencode 一轮发 4 个,四条 5 秒白等并行叠在一起)。
        // 但闸门**必须按号分**:一把全局锁会把 N 个各有独立代理的冷号串成 N×6 秒,
        // 而它们本可以并行;任一个号的代理卡住还会让整个池排队。
        // 拿到闸门后**必须重查缓存** —— 排在后面的那些,前面那位已经取回来了。
        let gate = {
            let mut gates = self.config_gates.lock().unwrap_or_else(|p| p.into_inner());
            // 表按「身份指纹」增长,换凭据会留下旧条目。数量级 = O(账号数×换凭据次数),
            // 每项是一把空 Mutex,泄漏量可忽略;真要清理应跟着账号生命周期走,不在这里猜。
            gates.entry(key.clone()).or_default().clone()
        };
        let _held = gate.lock().await;
        if let CacheLook::Fresh(v) = self.cached_config_version(&key) {
            return Ok(v);
        }

        tracing::debug!(account = %account.account_id, "cursor 现调 GetServerConfig 取 config_version");
        let fetched = config::fetch_config_version(
            client,
            &self.cfg.api_host,
            token,
            machine_id,
            Some(mac_machine_id),
            // 时区必须与推理请求一致。硬编码 Asia/Shanghai 会让同一个"客户端会话"的
            // unary 报上海、推理报账号配的时区 —— 一个**内部自相矛盾**的指纹,
            // 比配错更可疑(schema 里那句"应与出口 IP 地理位置一致"就白写了)。
            &Self::timezone_of(account),
        )
        .await;

        match fetched {
            Ok(fresh) => {
                tracing::debug!("cursor GetServerConfig 返回,长度 {}", fresh.len());
                self.store_config(&key, Some(fresh.clone()));
                Ok(fresh)
            }
            // ⚠️ **取不到时宁可让本次请求失败,也不能发空串。**
            //
            // 这里曾经是「退化到过期旧值,再退化到空串」,理由是"配置版本号不是凭据,
            // 一次 api2 抖动不该打挂聊天"。那个推理漏了下游:`config.rs` 自己的结论是
            // **回显空/过期的 config_version 会被完整性门以 `resource_exhausted` 软封**,
            // 而 `resource_exhausted` 在 `trailer_to_error` 里映射成 `QuotaExhausted`,
            // 到了调度层是**持久禁用、不自愈、要人工 reset**(scheduler `DisabledReason`)。
            //
            // 也就是说那个"不阻断请求"的善意退化,实际效果是把一次 api2 超时变成
            // **一个健康账号被永久禁用**;api2 连续抖动就是整个 cursor 池挨个阵亡。
            // 现在改成返回可重试错误:`Run` 还没发出去,gw-app 不算 committed,
            // 会换号重试(别的号有自己的 config_version 缓存)。
            //
            // 过期旧值也不再兜底 —— 它走的是同一条软封路径,只是概率低一点。
            // 未过期的缓存值在上面已经返回了,走到这里就是真的没有可用值。
            Err(e) => {
                self.store_config(&key, None); // 负缓存,见 CONFIG_FAIL_TTL
                tracing::warn!(
                    account = %account.account_id,
                    error = %e,
                    "cursor 取不到 config_version,本次请求按可重试失败返回(绝不发空串:会被软封成额度耗尽)"
                );
                Err(UpstreamError::network(format!(
                    "cursor 取 config_version 失败,未发出推理请求: {e}"
                )))
            }
        }
    }

    pub(super) fn store_config(&self, key: &str, value: Option<String>) {
        self.config_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                key.to_string(),
                ConfigEntry {
                    value,
                    at: Instant::now(),
                },
            );
    }

    pub(super) fn cached_config_version(&self, key: &str) -> CacheLook {
        let map = self.config_cache.lock().unwrap_or_else(|p| p.into_inner());
        let Some(e) = map.get(key) else {
            return CacheLook::Miss;
        };
        match &e.value {
            Some(v) if e.at.elapsed() < CONFIG_VERSION_TTL => CacheLook::Fresh(v.clone()),
            None if e.at.elapsed() < CONFIG_FAIL_TTL => CacheLook::RecentlyFailed,
            _ => CacheLook::Miss,
        }
    }
}
