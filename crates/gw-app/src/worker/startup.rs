//! worker 进程启动:读配置、构造 provider、装载账号与成员边、起设置轮询与后台任务、挂路由。

use super::*;

pub async fn run(
    instance: u32,
    instances_path: &Path,
    accounts_path: &Path,
    system_path: &Path,
    db_path: &Path,
) -> anyhow::Result<()> {
    let instances: InstancesConfig = load_yaml(instances_path)?;
    instances.validate()?; // 拓扑约束:同组多 worker 等违规直接拒绝启动。
    // accounts.yaml 自切片④起为可选:首启播种用,导入后 DB 是账号事实源。
    let accounts_cfg: Option<AccountsConfig> = match load_yaml::<AccountsConfig>(accounts_path) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::info!("accounts.yaml 不可用(账号将只来自 DB): {e}");
            None
        }
    };
    // system.yaml **缺失**是合法形态(全用默认值);但**存在却解析不了**必须当场拒绝启动。
    //
    // 原先一律 `unwrap_or_default()`:各配置段都带 `deny_unknown_fields`,所以一个拼错的字段名
    // 会让**整个** SystemConfig 静默换成默认值 —— 上游超时、调度参数、缓存计费、图像压缩、
    // 实验开关一起被重置,而线上只表现为"行为莫名其妙变了",日志里连一行指向配置的线索都没有。
    // 对抗审查 Skeptic#1 指出这条,新增 thinking 段又给它添了一个新触发点。
    let system: SystemConfig = if system_path.exists() {
        load_yaml(system_path).map_err(|e| {
            anyhow::anyhow!(
                "{} 解析失败,拒绝以默认配置启动(默认值会静默重置超时/调度/缓存/实验开关): {e}",
                system_path.display()
            )
        })?
    } else {
        tracing::info!("{} 不存在,SystemConfig 全用默认值", system_path.display());
        SystemConfig::default()
    };

    let wcfg = instances
        .worker(instance)
        .ok_or_else(|| anyhow::anyhow!("instances.yaml 中无 instance={instance}"))?
        .clone();

    // 控制面库:账号事实源 + usage 落库(与 router 共享同一 SQLite,WAL 多进程并发)。
    // 打不开则降级:账号退回 yaml 启动快照、usage 仅记日志不入库。
    let store: Option<Arc<SqliteStore>> = match SqliteStore::open(db_path) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            tracing::warn!("控制面库打开失败(账号回退 yaml,usage 不落库): {e}");
            None
        }
    };
    let prompt_cache_path = db_path
        .parent()
        .unwrap_or_else(|| Path::new("data"))
        .join("prompt-cache.bin");
    gw_kiro::cache_sim::configure_persistence(prompt_cache_path);

    // 首启播种:yaml → DB 幂等导入(已有行不覆盖,绝不回滚已 roll 的 token)。
    if let (Some(store), Some(cfg)) = (&store, &accounts_cfg) {
        match store.import_accounts(cfg) {
            Ok(0) => {}
            Ok(n) => tracing::info!(imported = n, "accounts.yaml 新账号已导入 DB"),
            Err(e) => tracing::warn!("accounts.yaml 导入失败: {e}"),
        }
    }

    // 账号集:DB 优先(admin 可管理),无库时退回 yaml。
    let accounts: Vec<Arc<Account>> = match &store {
        Some(store) => store
            .load_owned_accounts(&wcfg.account_group)?
            .into_iter()
            .map(Arc::new)
            .collect(),
        None => accounts_cfg
            .as_ref()
            .and_then(|c| c.group_accounts_with_provider(&wcfg.account_group))
            .unwrap_or_default()
            .into_iter()
            .map(Arc::new)
            .collect(),
    };
    if accounts.is_empty() {
        tracing::warn!(group = %wcfg.account_group,
            "组内暂无账号(可经 admin 添加,30s 内生效);期间请求将报『组内无账号』");
    }

    // provider 家族:yaml 组定义优先,否则取组内首个账号的 provider,缺省 kiro。
    let provider_family = accounts_cfg
        .as_ref()
        .and_then(|c| c.group(&wcfg.account_group))
        .map(|g| g.provider.clone())
        .or_else(|| {
            accounts
                .first()
                .map(|a| a.provider.clone())
                .filter(|p| !p.is_empty())
        })
        .unwrap_or_else(|| "kiro".to_string());

    // 单 provider 组模型:与本 worker provider 家族不符的账号一律跳过——
    // 否则会拿 Kiro 实现去刷新/调用别家凭据(审查 Architect#2)。
    let accounts = filter_by_provider(accounts, &provider_family);

    let registry = Registry::with_builtins();
    tracing::debug!(providers = ?registry.families(), "已注册 provider");
    // 先按本 worker 的固定出口构造 egress client,注入 provider——
    // 保证该 provider 所有上游请求走同一出口 IP(防关联封号)。
    let client = egress::build_client(&wcfg.egress, system.upstream_timeout_secs)?;
    let egress_desc = egress::describe(&wcfg.egress);
    // DB 设置 overlay:叠在 system.yaml 基线上得"有效配置"(热调首启即生效)。
    // default_proxy 单独取出注入 provider(出口选择,不属运行开关)。
    let mut effective_system = system.clone();
    // 启动期这次解析的结果要**带到 /health 上**,不能只进日志:退回 YAML 基线是会打出
    // 全量 503 的状态,而它原本的唯一痕迹是一行启动日志 —— 事后没人翻得到。
    let mut initial_settings_error = String::new();
    let mut initial_settings_unknown: Vec<String> = Vec::new();
    let initial_default_proxy: Option<String> = match store
        .as_ref()
        .and_then(|s| s.get_settings().ok().flatten())
    {
        Some(json) => match serde_json::from_str::<gw_core::config::SystemSettings>(&json) {
            Ok(s) => {
                if !s.unknown.is_empty() {
                    // 启动期没有"上一轮配置"可保持,只能忽略这几个字段继续起。
                    // 但必须喊出来:它意味着本镜像比写库的那个旧。
                    initial_settings_unknown = s.unknown_keys().into_iter().map(String::from).collect();
                    tracing::warn!(
                        keys = ?s.unknown_keys(),
                        "settings overlay 含本版本不认识的字段,已忽略这几个、其余照常生效\
                         (通常说明本进程镜像偏旧)"
                    );
                }
                s.apply_to(&mut effective_system);
                s.default_proxy.clone()
            }
            Err(e) => {
                // ⚠️ 这条是**启动期**的回落,刻意不 fail-closed:否则一次 admin 误操作
                // 就能让整套栈起不来。但级别从 warn 提到 error —— 此时全部调度参数
                // (含 429 冷却)都退回 YAML 基线,是会打出全量 503 的状态。
                initial_settings_error = format!("启动期 overlay 解析失败,已退回 YAML 基线: {e}");
                tracing::error!("settings overlay 解析失败,本次启动退回 YAML 基线(调度参数全部失去热值): {e}");
                None
            }
        },
        None => None,
    };

    let provider_cfg = provider_cfg(&effective_system, initial_default_proxy.as_deref());
    let provider = registry.build(&provider_family, &provider_cfg, client.clone())?;
    // 在 provider 被 move 进 WorkerState 之前取一次:这是「provider 级设置是否热生效」
    // 的唯一权威来源(见 Provider::hot_settings_supported)。
    let provider_hot_supported = provider.hot_settings_supported();

    // 启动即把"有效设置"热应用一次:实验开关等进程级全局仅由 env 初始化,DB overlay 原本要等
    // 30s 轮询(首跳被跳过)才生效。期间若 DB 已设 `thinking_signature=false`,这段窗口里 Kiro
    // 仍会发 Kiro 合成签名 → 重演跨通道 THINKING_SIGNATURE_INVALID(对抗审查 #1)。这里用与轮询
    // 同一 `from_effective` 口径先应用一次,关掉窗口。对 kiro 是幂等(cache/image/proxy 已经 from_config
    // 设过);dario 只在这里读它自己的翻译开关 `dario_cc_translate`(缓存计费等 provider 级设置
    // dario 仍不热应用,所以它的 hot_settings_supported 保持 false)。
    {
        let full = gw_core::config::SystemSettings::from_effective(
            &effective_system,
            initial_default_proxy.clone(),
        );
        // 各上游自己的设置(cache_sim、cursor 追加模型 / 护栏 / CLI 说明……)都在
        // apply_hot_settings 里读;启动即应用一次,不留 30s 内置默认的窗口。
        if let Ok(sv) = serde_json::to_value(&full) {
            if let Err(e) = provider.apply_hot_settings(&sv) {
                tracing::error!("provider 级设置启动初载有项未应用,该项沿用内置默认: {e}");
            }
        }
        // 报文环境标准化:各线路开关是进程级全局(dario 等不实现 apply_hot_settings),启动即载入。
        if let Err(e) = gw_core::envnorm::set_config(&effective_system.env_normalize) {
            tracing::error!("报文环境标准化配置启动初载失败,本进程保持关闭: {e}");
        }
        // 自动模式分类器开关:同样是进程级全局,启动即载入(不载的话头 30s 按默认 off 跑)。
        if let Err(e) = gates::automode::set_config(&effective_system.auto_mode_classifier) {
            tracing::error!("自动模式分类器配置启动初载失败,本进程保持关闭: {e}");
        }
        // 批量任务拦截:同理启动即载入(否则头 30s 用内置默认 observe,而 DB 里可能已是 enforce)。
        if let Err(e) = gates::batch_guard::set_config(&effective_system.batch_guard) {
            tracing::error!("批量任务拦截配置启动初载失败,本进程沿用内置默认(observe): {e}");
        }
        // 拒答记忆:同理启动即载入。
        if let Err(e) = gates::refusal_guard::set_config(&effective_system.refusal_guard) {
            tracing::error!("拒答记忆配置启动初载失败,本进程沿用内置默认(observe): {e}");
        }
        // 号池熔断:同理启动即载入。
        if let Err(e) = super::pool_breaker::set_config(&effective_system.pool_breaker) {
            tracing::error!("号池熔断配置启动初载失败,本进程沿用内置默认(enforce): {e}");
        }
    }

    // Fix6: For dario workers, filter out accounts that fail validate_account
    // (missing access_token + refresh_token) before they enter the scheduler.
    // Scope this to "claude-dario" only — kiro's validate_account requires
    // refresh_token + machine_id, and there may be edge-case kiro accounts
    // on-line that intentionally omit machine_id (e.g. legacy rows that rely
    // on runtime defaults).  Restricting to dario eliminates any kiro regression
    // risk while still catching dario accounts imported without credentials.
    // cursor 一并纳入(2026-08-07):它的 validate_account 除了 token 还会校验
    // **出口代理构造得出来**。代理配错的 cursor 号必须挡在池外 —— 让它进池的话,
    // 每个打到它的请求都会失败(provider 侧 fail-closed,绝不回退默认出口:回退
    // 等于把本该隔离的号并到同一 IP,已实测的封号维度)。挡在池外 + 启动告警,
    // 比"号在池里但每次都失败"好查得多。
    let accounts = if provider.validate_on_load() {
        let total = accounts.len();
        let valid: Vec<Arc<Account>> = accounts
            .into_iter()
            .filter(|a| match provider.validate_account(a.as_ref()) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(
                        account = %a.account_id,
                        reason = %e,
                        family = %provider_family,
                        "账号校验失败,跳过不进调度池"
                    );
                    false
                }
            })
            .collect();
        let skipped = total - valid.len();
        if skipped > 0 {
            tracing::info!(
                total,
                valid = valid.len(),
                skipped,
                family = %provider_family,
                "账号校验:部分账号未通过(缺凭据 / 出口代理非法)被跳过"
            );
        }
        valid
    } else {
        accounts
    };

    let usage_sink: Option<Arc<dyn UsageSink>> =
        store.clone().map(|s| s as Arc<dyn UsageSink>);

    tracing::info!(
        instance,
        listen = %wcfg.listen,
        egress = %egress_desc,
        group = %wcfg.account_group,
        accounts = accounts.len(),
        provider = provider.family(),
        usage_sink = usage_sink.is_some(),
        "worker 就绪"
    );
    for w in provider.startup_warnings(&egress_desc) {
        tracing::warn!("{w}");
    }

    // 成员边**必须在开始接流量之前同步装载**。周期同步的首跳是被跳过的(见下方
    // `tick.tick().await`,注释"启动时刚加载过"只对账号成立),若在这里留空,启动后
    // 头 30 秒内每个带组名的请求都会取到空视图 → GroupEmpty/503,等于每次发版打出一个
    // 30 秒全组不可用窗口(对抗审查 Skeptic#1)。读库失败按空视图起,由周期同步补齐。
    let initial_views: std::collections::HashMap<String, scheduler::GroupView> = store
        .as_ref()
        .map(|s| s.load_group_memberships(&wcfg.account_group))
        .transpose()
        .unwrap_or_else(|e| {
            tracing::error!("启动装载成员边失败,本轮以空视图起(30s 后由周期同步补齐): {e}");
            None
        })
        .unwrap_or_default()
        .into_iter()
        .map(|(g, rank)| (g.clone(), scheduler::GroupView::new(g, rank)))
        .collect();
    tracing::info!(groups = initial_views.len(), "成员边启动装载完成");

    // suspend 生命周期(退避/观察期/退役/落库)只对 kiro 启用:0/35 不可恢复的
    // 实测数据只覆盖 Kiro 号,dario 等 provider 的 TemporarilyBlocked 语义不同,
    // 保持逐字节旧行为(平冷却、到期满血复活)。
    let lifecycle_enabled = provider.suspend_lifecycle();
    // 号池熔断只在 Kiro 号池 worker 上开(与 suspend 生命周期同口径),且必须有控制面库:
    // 恢复靠库里的 reset_at,没有库就没法手动恢复。开了之后**先从库里接一次状态**,
    // 让重启前的熔断在接第一条请求前就生效 —— 重启不能变成「自动恢复」。
    if lifecycle_enabled {
        if let Some(st) = store.as_deref() {
            super::pool_breaker::enable();
            let _ = super::pool_breaker::sync_with_store(st);
            tracing::info!(cfg = ?super::pool_breaker::current(), "号池熔断已启用");
        }
    }
    // 落库的 suspend 生命周期在构造时水合:重启不丢退避进度、不重抽抖动,
    // 部署重启不会让"已退到 24h 档"的号提前复活。读库失败按空表起(=旧行为),
    // 由周期 sync 把内存里的新转换陆续落库补齐。
    let lifecycles = if lifecycle_enabled {
        store
            .as_ref()
            .map(|s| s.load_suspend_lifecycles())
            .transpose()
            .unwrap_or_else(|e| {
                tracing::error!("启动装载 suspend 生命周期失败,本轮以空表起(退避进度本轮不水合): {e}");
                None
            })
            .unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };
    if !lifecycles.is_empty() {
        tracing::info!(accounts = lifecycles.len(), "suspend 生命周期启动水合完成");
    }
    let scheduler =
        AccountScheduler::new_with_lifecycles(accounts, &effective_system.scheduler, &lifecycles, lifecycle_enabled);
    // 最优控制调度器只赋给 kiro worker:它的计费模型(冷热差价、429 规律)只在 Kiro 上标定过,
    // cursor/dario worker 即使设置为 cost_index 也恒走旧版。环境变量 `KIRO_COST_INDEX=0`
    // 可对单个 worker 急停(重启该 worker 生效),不改设置里的选择。
    let scheduler = if cost_index_allowed(provider.as_ref()) {
        let pricing = provider.clone();
        scheduler.with_cost_index(move |a| {
            if pricing.enterprise_priced(a) {
                cost_index::PriceClass::Enterprise
            } else {
                cost_index::PriceClass::Personal
            }
        })
    } else {
        scheduler
    };
    tracing::info!(
        cost_index_capable = scheduler.cost_index_capable(),
        algorithm = scheduler.active_algorithm(),
        "调度器就绪"
    );
    if lifecycle_enabled {
        if let Some(s) = &store {
            scheduler.set_lifecycle_store(s.clone());
        }
    }

    let state = Arc::new(WorkerState {
        group_views: parking_lot::RwLock::new(initial_views),
        instance,
        egress_desc,
        group: wcfg.account_group.clone(),
        provider,
        scheduler,
        refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
            rejection_refresh_at: parking_lot::Mutex::new(std::collections::HashMap::new()),
        usage_sink,
        pending_writes: PendingWrites::new(),
        store: store.clone(),
        quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
        quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
        quota_sem: Arc::new(tokio::sync::Semaphore::new(QUOTA_MAX_CONCURRENCY)),
        sync_lock: tokio::sync::Mutex::new(()),
        // 启动期的同步结果照实记:此时 `effective_system` 已经叠过 overlay(或在解析失败时
        // 退回了 YAML 基线),两种情况都要能在面板上看出来 —— 启动就退回基线是最危险的
        // 状态(调度参数全失去热值),而它原本只有一行日志。
        settings_sync: parking_lot::RwLock::new(SettingsSync {
            applied_at: if initial_settings_error.is_empty() { now_unix() } else { 0 },
            // 库打不开时 30s 轮询循环**根本不会 spawn**,热调从此永久失效。
            // 不写一句的话,面板 90 秒后只会永久标红「N 秒前同步」,没有任何线索,
            // 运维会把一个按设计降级运行的进程当成同步卡死去重启(对抗审查 Architect#9)。
            error: if store.is_none() && initial_settings_error.is_empty() {
                "控制面库不可用,本 worker 的设置热调不生效(改设置必须重启它)".to_string()
            } else {
                initial_settings_error
            },
            unknown: initial_settings_unknown,
            effective: effective_view(&effective_system),
            provider_hot: provider_hot_supported,
        }),
        _client: client,
    });

    // 账号配置 sync:30s 从 DB 重读组内账号集,admin 增删改无需重启 worker 即生效。
    // (翻转语义见 scheduler::sync_accounts;读库失败跳过本轮,不影响服务。)
    if let Some(store) = store {
        let st = state.clone();
        // YAML 基线快照:每轮把 DB overlay 叠在它之上算"有效配置"再热应用。
        let sys_base = system.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // 首跳立即触发,跳过(启动时刚加载过)。
            // 上一轮告警过的未知字段集。只在**变化时**告警,否则 30s 一条会淹掉日志。
            loop {
                tick.tick().await;
                // 账号集同步(含脏 extra 冲刷)—— 与 /sync 立即同步共用实现。
                st.sync_accounts_from_db().await;
                // 号池熔断与库对账(落库 / 接过熔断 / 管理员恢复)。放在设置解析之前、独立于它:
                // 设置 overlay 解析失败会 `continue` 跳过本轮,而熔断的恢复不能被它卡住。
                if super::pool_breaker::enabled() {
                    if let Some(text) = super::pool_breaker::sync_with_store(&store) {
                        let store = store.clone();
                        tokio::spawn(async move { super::pool_breaker::send_notify(&store, text).await });
                    }
                }
                // 热应用 DB 设置 overlay:代理/计费/图像(provider)+ 调度参数 + cache_sim。
                // 用**有效全量**(YAML 基线叠 overlay,再回灌 from_effective)喂给 provider,
                // 这样 overlay 删某字段时能正确恢复到 YAML 默认(而非停留在上次热值)。
                match store.get_settings() {
                    Ok(opt) => {
                        // ⚠️ 这里曾是 `.ok().unwrap_or_default()` —— 解析失败**静默**变成空
                        // overlay,于是全部调度参数回落 YAML 基线(冷却 300s),企业号几秒内
                        // 被全部打下线 → 全量 503,而日志一行都没有。现在:解析失败**跳过本轮**,
                        // 保持上一轮已生效的配置,并按 error 级别喊出来。
                        let overlay = match opt {
                            None => gw_core::config::SystemSettings::default(),
                            Some(j) => {
                                match serde_json::from_str::<gw_core::config::SystemSettings>(&j) {
                                    Ok(s) => s,
                                    Err(e) => {
                                        // 这一跳会**每 30s 重复发生且永不自愈**,配置就此僵在
                                        // 上一轮的值上。只记日志的话,面板上看到的是「保存成功」,
                                        // 实际再也不会生效 —— 必须让它在 /health 上冒头。
                                        st.settings_sync.write().error = format!(
                                            "overlay 解析失败,配置已僵在上一次成功的值: {}",
                                            redact_err(&e.to_string())
                                        );
                                        tracing::error!(
                                            "settings overlay 解析失败,**保持上一轮已生效配置**\
                                             (不回落 YAML 基线): {e}"
                                        );
                                        continue;
                                    }
                                }
                            }
                        };
                        // 未知字段不再作废整份 overlay(其余字段照常生效),但必须可见:
                        // 它的含义通常是「本进程的镜像比写库的那个旧」。
                        // BTreeMap → 顺序稳定,可以直接比。去重基准取自 settings_sync,
                        // 不再另存一份 warned_unknown(两份手工同步早晚会不一致)。
                        let unknown: Vec<String> =
                            overlay.unknown.keys().cloned().collect();
                        let warned_unknown = st.settings_sync.read().unknown.clone();
                        if unknown != warned_unknown {
                            if !unknown.is_empty() {
                                tracing::warn!(
                                    keys = ?unknown,
                                    "settings overlay 含本版本不认识的字段,已忽略这几个、其余照常生效\
                                     (通常说明本进程镜像偏旧)"
                                );
                            } else if !warned_unknown.is_empty() {
                                tracing::info!("settings overlay 的未知字段已消失,恢复完全识别");
                            }
                        }
                        let mut eff = sys_base.clone();
                        overlay.apply_to(&mut eff);
                        let full = gw_core::config::SystemSettings::from_effective(
                            &eff,
                            overlay.default_proxy.clone(),
                        );
                        // provider 侧应用失败必须记下来。原本这里是 `if let Ok(..)`
                        // 静默跳过:序列化一失败,provider 拿不到新值,而紧随其后的
                        // scheduler / cache_sim 照常更新,快照照记「成功」——
                        // 面板于是显示「一致」而 provider 级设置(缓存计费)根本没变。
                        // 这正是本功能要消灭的那种谎(对抗审查三视角一致指出)。
                        let apply_err = match serde_json::to_value(&full) {
                            Ok(sv) => match st.provider.apply_hot_settings(&sv) {
                                Ok(()) => String::new(),
                                // 原因已由 provider 打日志;这里并进本轮同步错误,面板上说出来。
                                Err(e) => e,
                            },
                            Err(e) => {
                                tracing::error!("settings 序列化失败,provider 级设置本轮未应用: {e}");
                                format!("provider 级设置未应用(序列化失败): {}", redact_err(&e.to_string()))
                            }
                        };
                        st.scheduler.update_tuning(&eff.scheduler);
                        // 报文环境标准化:同一条原则(失败保留上一份 + 说出来)。
                        let apply_err = match gw_core::envnorm::set_config(&eff.env_normalize) {
                            Ok(()) => apply_err,
                            Err(e) => {
                                tracing::error!("报文环境标准化配置未应用(沿用上一份有效值): {e}");
                                let msg = format!("报文环境标准化未应用: {e}");
                                if apply_err.is_empty() { msg } else { format!("{apply_err}; {msg}") }
                            }
                        };
                        // 自动模式分类器开关:同一条原则。热切 allow/haiku/off 即时生效。
                        let apply_err = match gates::automode::set_config(&eff.auto_mode_classifier) {
                            Ok(()) => apply_err,
                            Err(e) => {
                                tracing::error!("自动模式分类器配置未应用(沿用上一份有效值): {e}");
                                let msg = format!("自动模式分类器未应用: {e}");
                                if apply_err.is_empty() { msg } else { format!("{apply_err}; {msg}") }
                            }
                        };
                        // 批量任务拦截:同一条原则(失败保留上一份 + 说出来)。不清计数状态。
                        let apply_err = match gates::batch_guard::set_config(&eff.batch_guard) {
                            Ok(()) => apply_err,
                            Err(e) => {
                                tracing::error!("批量任务拦截配置未应用(沿用上一份有效值): {e}");
                                let msg = format!("批量任务拦截未应用: {e}");
                                if apply_err.is_empty() { msg } else { format!("{apply_err}; {msg}") }
                            }
                        };
                        // 号池熔断:同一条原则(失败保留上一份 + 说出来)。不清熔断状态。
                        let apply_err = match super::pool_breaker::set_config(&eff.pool_breaker) {
                            Ok(()) => apply_err,
                            Err(e) => {
                                tracing::error!("号池熔断配置未应用(沿用上一份有效值): {e}");
                                let msg = format!("号池熔断未应用: {e}");
                                if apply_err.is_empty() { msg } else { format!("{apply_err}; {msg}") }
                            }
                        };
                        // 拒答记忆:同一条原则(失败保留上一份 + 说出来)。不清记账状态。
                        let apply_err = match gates::refusal_guard::set_config(&eff.refusal_guard) {
                            Ok(()) => apply_err,
                            Err(e) => {
                                tracing::error!("拒答记忆配置未应用(沿用上一份有效值): {e}");
                                let msg = format!("拒答记忆未应用: {e}");
                                if apply_err.is_empty() { msg } else { format!("{apply_err}; {msg}") }
                            }
                        };
                        // 应用成功才刷新时间戳与快照:面板上「多久前同步的」才是可信的
                        // 新鲜度指标 —— 失败时它会停住不动,一眼看出配置僵了多久。
                        *st.settings_sync.write() = SettingsSync {
                            applied_at: now_unix(),
                            error: apply_err,
                            unknown: unknown.clone(),
                            effective: effective_view(&eff),
                            // 本 provider 压根不热应用 provider 级设置(dario 就是这样):
                            // scheduler 那半边确实生效了,但缓存计费这半边要重启才动。
                            // 不说出来的话,面板会对着一份「算得出但没生效」的值报「一致」。
                            provider_hot: st.provider.hot_settings_supported(),
                        };
                    }
                    Err(e) => {
                        st.settings_sync.write().error =
                            format!("读库失败,本轮跳过: {}", redact_err(&e.to_string()));
                        tracing::warn!("settings sync 读库失败,跳过本轮: {e}");
                    }
                }
            }
        });
    }

    // static_flow 式后台配额轮询:worker 自身每 240–300s(带抖动)兜底刷新一遍账号配额,
    // **不依赖 /health 被打**。目的:① 复刻真实 Kiro IDE 每 ~5min 一次 getUsageLimits 的
    // ambient 流量(防封——纯反代账号在两次聊天间对上游静默是最易被审计的指纹);② 让配额
    // 面板在无人看 dashboard 时也保持新鲜。被 /health(60s TTL)刚刷过的账号本轮自动跳过,
    // 不产生双倍流量;上游并发仍受 quota_sem(3)节流。
    //
    // 启停:**始终 spawn**,每轮读 `scheduler.quota_poll_enabled()` 热开关(经 SchedulerConfig
    // + 30s settings overlay 热生效——设置面板可即时启停,无需重启;对抗审查 Architect#5/
    // Minimalist#4 取代了原 env 开关)。冷启动:首轮只等 QUOTA_POLL_WARMUP_SECS 就 sweep 一遍
    // (对抗审查 Skeptic#5),之后进入抖动节律。
    // shutdown:本任务是 daemon(同既有 30s sync loop,无独立 shutdown 协调)。安全性依据:
    // refresh_locked 刷新 token 后**同步落库**(merge_account_extra),故停机中途被 drop 也不丢
    // rolling token;sweep 唯一外部副作用是只读 getUsageLimits,被 drop 无损(对抗审查 Architect#6)。
    {
        let st = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(QUOTA_POLL_WARMUP_SECS)).await;
            loop {
                if st.scheduler.quota_poll_enabled() {
                    st.sweep_stale_quotas().await;
                }
                let secs = jittered_secs(QUOTA_POLL_MIN_SECS, QUOTA_POLL_MAX_SECS);
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            }
        });
        tracing::info!(
            min = QUOTA_POLL_MIN_SECS,
            max = QUOTA_POLL_MAX_SECS,
            "后台配额轮询已就绪(static_flow 式 ambient getUsageLimits;启停经设置面板热控)"
        );
    }

    // 启动预热:缺订阅档位的账号后台拉一次配额(getUsageLimits 只读,安全),
    // 让模型能力过滤(FREE 不给 opus)冷启动即有数据源,而非等 /health 被动触发。
    state.warm_subscription_titles();

    let mut app = Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/models", get(models))
        .route("/health", get(health))
        // 只回显设置同步实况的**轻量**端点。
        //
        // 为什么不复用 /health:那个端点会跑全账号 status_snapshot,并对配额缓存
        // 陈旧的账号触发上游 getUsageLimits —— 也就是说「有人打开设置页」会变成
        // 「对付费账号打上游」。这个账号池对封禁很敏感,一个只读的可观测性接口
        // 不该和生产流量耦合(对抗审查三视角一致指出)。这里只读一把 RwLock。
        .route("/settings-sync", get(settings_sync));

    // OpenAI 线缆:cursor 上游本来就供 gpt / grok / gemini 这些非 Anthropic 家族的模型,
    // 却只能用 Anthropic 协议去要 —— 给它开 OpenAI 入口,顺带省掉下游 NewAPI 那道
    // 有损的 Claude→OpenAI 转换(它只认 5 种事件,见 `keepalive_frame` 的注释)。
    //
    // **按 family 条件挂载**是结构性闸门:kiro / dario / claude-subprocess 只挂拒绝入口
    // (404 + 未执行标记)，不进入协议转换或 provider。它们的主链路全程 Anthropic、
    // 零转换,那是刻意保住的资产(见 gw_core::provider 模块文档),不该被 OpenAI 入口稀释。
    if state.provider.openai_wire() {
        app = app
            .route("/v1/chat/completions", post(chat_completions))
            .route("/v1/responses", post(responses));
        tracing::info!(
            family = state.provider.family(),
            "已挂载 OpenAI 线缆入口: /v1/chat/completions + /v1/responses"
        );
    } else {
        // 明确拒绝且不进入 provider，供新版 router 安全识别未执行的协议入口。
        app = app
            .route("/v1/chat/completions", post(unsupported_openai_endpoint))
            .route("/v1/responses", post(unsupported_openai_endpoint));
    }

    // worker 不做对外鉴权、且信任 router 注入的 X-Gw-Client-Key;必须只绑 loopback,
    // 否则客户端可直连 worker 绕过 router 鉴权并伪造用量归属(审查 #2)。
    let loopback = is_loopback_listen(&wcfg.listen);
    if loopback {
        // 内网管理端点(无鉴权,信任同机 router):人工救号 + 人工强制刷新 token。
        // **仅 loopback 才挂载**——非 loopback 误配下暴露它等于把"清禁用保护/强制刷新"
        // 开给整个网络(审查②R 共识 high)。
        app = app
            .route("/accounts/{id}/reset", post(reset_account))
            .route("/accounts/{id}/refresh", post(refresh_account))
            .route("/accounts/{id}/quota", post(quota_account))
            .route("/accounts/{id}/on-demand", post(on_demand_account))
            .route("/accounts/{id}/models", post(models_account))
            .route("/accounts/{id}/models/local", get(models_local))
            .route("/accounts/{id}/probe", post(probe_account))
            .route("/oauth/exchange", post(oauth_exchange))
            .route("/sync", post(sync_now));
    } else {
        tracing::warn!(
            listen = %wcfg.listen,
            "⚠️ worker 绑定到非 loopback 地址:这会让客户端绕过 router 鉴权并伪造 client_key 归属;\
             reset 管理端点已禁用。请改绑 127.0.0.1,或为 router→worker 内网跳加共享密钥。"
        );
    }
    // 入站体积上限:messages 用 Json<Value> 全量缓冲,axum 默认 2MB 会在 handler 前 413。
    // 提到 effective_system.max_request_body_bytes(默认 16MB)。这是第二道入站咽喉——
    // router 放开后此处不放开仍会再 413(两处缺一不可)。管理端点无 body,层对其无害。
    // 启动日志打印有效值:与 router 日志对照即可发现两进程配置漂移(只重启一侧时原 bug
    // 会在 worker 侧复现——Architect 审查)。
    let max_body = effective_system.effective_max_request_body_bytes();
    tracing::info!(
        max_request_body_bytes = max_body,
        "worker 入站体积上限(应与 router 一致;不一致=配置漂移,大请求仍会在此 413)"
    );
    let app = app
        .layer(axum::extract::DefaultBodyLimit::max(max_body))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&wcfg.listen).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(crate::shutdown_signal("worker"))
        .await?;

    // 排空后先等 Drop 里 detach 的 usage/quota 落库任务收尾(graceful shutdown 只等
    // 响应体,不等这些 spawn 任务;5s 上限防极端卡死)。
    if !state
        .pending_writes
        .wait_idle(std::time::Duration::from_secs(5))
        .await
    {
        tracing::warn!("停机:5s 内仍有 usage 落库任务未完成,最后一批记录可能丢失");
    }
    // 最后机会落盘脏 extra:30s 重试循环随进程退出被 drop,「刷新成功但 DB 回写
    // 失败」的 rolling token 若不在此落盘,退出即丢(账号下次只能拿旧 token 刷新,
    // 可能已被上游作废)。
    if let Some(store) = &state.store {
        flush_dirty_extras(&state.scheduler, store, &state.refresh_locks, "停机排空").await;
    }
    Ok(())
}
/// provider 工厂 cfg:注入有效 cache(缓存计费)+ image(图像压缩,"image" 子对象)+
/// 可选 default_proxy(全局默认出口代理)+ dario 段 + 顶层的 `node_hub_url` / `dario_cc_template_path`。
/// 序列化失败退回缺省(provider 各自回退,不致命)。
///
/// provider 只看得到这里放进去的键:system.yaml 新增的顶层键必须在这里显式带上,否则
/// provider 读到的永远是空(`dario_cc_template_path` 就漏过一次,见本文件测试)。
fn provider_cfg(effective_system: &SystemConfig, default_proxy: Option<&str>) -> serde_json::Value {
    let mut cfg = serde_json::to_value(&effective_system.cache).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::Object(map) = &mut cfg {
        if let Ok(img) = serde_json::to_value(&effective_system.image) {
            map.insert("image".into(), img);
        }
        if let Some(dp) = default_proxy {
            map.insert("default_proxy".into(), serde_json::json!(dp));
        }
        if let Ok(dario) = serde_json::to_value(&effective_system.dario) {
            map.insert("dario".into(), dario);
        }
        map.insert(
            "node_hub_url".into(),
            serde_json::json!(effective_system.effective_node_hub_url()),
        );
        map.insert(
            "dario_cc_template_path".into(),
            serde_json::json!(effective_system.dario_cc_template_path),
        );
    }
    cfg
}

pub(super) fn load_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读取 {} 失败: {e}", path.display()))?;
    Ok(serde_yaml::from_str(&text)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-01 上线后发现:`dario_cc_template_path` 没进 provider 工厂 cfg,dario 拿到空路径,
    /// 开关切到 on 也不翻译(模板指纹恒为 null)。顶层键都得在这里显式带上。
    #[test]
    fn provider_cfg_带上顶层的子机与模板键() {
        let mut sys = SystemConfig::default();
        sys.dario_cc_template_path = "/app/dario/cc-template-data.json".into();
        sys.node_hub_url = "http://127.0.0.1:8790".into();
        let cfg = provider_cfg(&sys, Some("http://proxy:1"));
        assert_eq!(cfg["dario_cc_template_path"], "/app/dario/cc-template-data.json");
        assert_eq!(cfg["node_hub_url"], "http://127.0.0.1:8790");
        assert_eq!(cfg["default_proxy"], "http://proxy:1");
        assert!(cfg.get("dario").is_some() && cfg.get("image").is_some());
    }
}
