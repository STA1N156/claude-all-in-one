//! impl Provider for CursorProvider 及其辅助函数。
//!
//! 2026-11-10 由 lib.rs 拆出(只搬不改行为)。

use super::*;

/// 缓存查询结果。
pub(super) enum CacheLook {
    /// 有未过期的值。
    Fresh(String),
    /// 近期取失败过,负缓存未到期 —— 别再排队重试。
    RecentlyFailed,
    /// 没有可用信息,该去取。
    Miss,
}

/// config_version 的缓存键 = `account_id` + **身份指纹**。
///
/// 只用 account_id 的话:后台把同一个号的 token / machine_id / proxy 换掉之后,
/// 旧身份取回来的 config_version 会继续被复用最长 30 分钟 —— 而「新 token + 旧 config」
/// 是一个服务端没见过的组合,正好撞上完整性门。指纹进键 = 换凭据自动换条目。
pub(super) fn config_cache_key(account: &Account, token: &str, machine_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.update(b"\x00");
    h.update(machine_id.as_bytes());
    h.update(b"\x00");
    h.update(account.extra_str("proxy").unwrap_or_default().as_bytes());
    let d = h.finalize();
    // 前 8 字节够区分了,键不必长。
    let mut fp = String::with_capacity(16);
    for b in &d[..8] {
        fp.push_str(&format!("{b:02x}"));
    }
    format!("{}\u{0}{fp}", account.account_id)
}

/// 连接卡死自愈:流里出现「N 秒内只有心跳」的停滞时,丢弃这个号缓存的连接,下一个请求
/// 自动重建(出口不变,只影响这一个号)。
///
/// 2026-11-10 生产:某个号的连接进入「只发不收」(139 上看 lastrcv 133s、lastsnd 0.7s),
/// 这个号上所有请求一起卡 90~150s;号连续失败 3 次被禁用,冷却结束后**还用这条坏连接**,
/// 于是反复卡、反复禁,只有重启 worker(连接全部重建)才好。默认出口(没配代理)没有句柄,原样返回。
///
/// **一条坏连接只扣一次账号健康**:同一条连接上常有 3~9 个并发流,会在几秒内各自超时。
/// 第一个发现的流丢弃连接、照常上报;其余流发现这一代连接已被丢弃(`evict` 返回 false),
/// 报错打上连带回声标记,不再累加连续失败。11-10 上线自愈后实测:07:26:54 第一个流已重建
/// 连接,07:27:12、07:27:18 两个同连接的流仍凑够 3 次把号禁掉,只能人工重置。
/// 重建后的新连接如果又卡死,那是新的一代,照常计数,3 代都坏仍会禁号。
pub(super) fn heal_stalled_connection(stream: ChatStream, conn: Option<ConnHandle>) -> ChatStream {
    use futures::StreamExt;
    let Some(conn) = conn else {
        return stream;
    };
    Box::pin(stream.map(move |item| match item {
        Err(e) if chat::is_connection_stall(&e) => {
            if conn.evict() {
                tracing::warn!(
                    account = %conn.account_id,
                    error = %e.message,
                    "cursor:号的连接卡死,已丢弃缓存连接,下个请求重建(出口不变)"
                );
                Err(e)
            } else {
                tracing::info!(
                    account = %conn.account_id,
                    "cursor:同一条坏连接上的连带超时,连接已重建过,不再计入账号连续失败"
                );
                Err(e.as_collateral_failure())
            }
        }
        other => other,
    }))
}

/// 代理 client 缓存键:账号 + 代理 URL(见 `CursorProvider::client_for`)。
///
/// 带上代理 URL:账号换了出口(PATCH proxy)时自然建新 client,绝不复用旧出口的连接。
/// `\x1f` 分隔:account_id 与 URL 里都不会出现。
pub(super) fn proxy_client_key(account_id: &str, proxy_url: &str) -> String {
    format!("{account_id}\x1f{proxy_url}")
}

/// 按 proxy URL 构建 client。支持 http/https/socks5。
pub(super) fn build_proxy_client(url: &str) -> anyhow::Result<reqwest::Client> {
    let proxy = reqwest::Proxy::all(url)?;
    Ok(reqwest::Client::builder()
        .proxy(proxy)
        .connect_timeout(Duration::from_secs(20))
        // 不设整请求超时:Run 是流式的,读完整个流可能很久。
        .build()?)
}

/// 纯 bot 池裁剪:持有的账号**全部**钉 `extra.pool="bot"` 时只出 bot 能接的那一项。
///
/// grokbot 0.39(sandchat 面)没有模型字段,服务端永远是同一个 Grok;把全量目录给客户端
/// 只会诱导它点到 bot 号接不了的模型、然后撞「已钉 pool=bot」拒绝。无账号(还没 sync 到)
/// 或混有任何非 bot 号 → 维持全量目录:裁剪只是客户体验优化,拿不准时绝不裁。
pub fn models_for_pool(list: Vec<ModelInfo>, accounts: &[Arc<Account>]) -> Vec<ModelInfo> {
    let bot_only = !accounts.is_empty()
        && accounts
            .iter()
            .all(|a| a.extra.get("pool").and_then(|v| v.as_str()) == Some("bot"));
    if bot_only { bot_pool_list() } else { list }
}

/// 热追加的模型条目 → Run 侧模型(menu=false = 探测位:可被点名但不进 1.14 清单)。
pub(super) fn extra_models_from_specs(specs: &[gw_core::config::ExtraModelSpec]) -> Vec<run::Model> {
    specs
        .iter()
        .filter(|s| !s.name.trim().is_empty())
        .map(|s| {
            let params: Vec<(&str, &str)> = s.params.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let m = run::Model::with_params(s.name.trim(), &params);
            if s.menu { m } else { m.probe() }
        })
        .collect()
}

#[async_trait]
impl Provider for CursorProvider {
    fn family(&self) -> &'static str {
        "cursor"
    }

    /// 护栏策略句的短指纹:「线上跑的是哪一版、从什么时候起」,按版本分桶比对内建工具收口率用。
    /// 只回显指纹不回显全文 —— 全文是每个请求都发给上游的系统提示的一部分。
    fn settings_fingerprints(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert("cursor_guard_rev".into(), serde_json::json!(tool_guard_rev()));
        m.insert("cursor_loop_nudge_rev".into(), serde_json::json!(chat::tool_loop_nudge_rev()));
        m.insert("cursor_hold_tool_streams".into(), serde_json::json!(crate::held::enabled()));
        m.insert("cursor_blob_history".into(), serde_json::json!(crate::blobhist::enabled()));
        m.insert("cursor_held_runs".into(), serde_json::json!(crate::held::registry().len()));
        m
    }

    fn models_for_accounts(&self, list: Vec<ModelInfo>, accounts: &[Arc<Account>]) -> Vec<ModelInfo> {
        models_for_pool(list, accounts)
    }

    /// cursor 的客户端会说 OpenAI(Cursor IDE / OpenAI SDK)。
    fn openai_wire(&self) -> bool {
        true
    }

    /// 会话亲和键要不要**再按客户 key** 分一层命名空间。
    ///
    /// ## 为什么 cursor 必须分(不分就是跨客户串话)
    ///
    /// cursor 是**唯一有服务端会话续写**的上游:`ConvRegistry::phase_for` 在
    /// (conversation_id, account_id) 都命中时返回 `Phase::Continuation`,于是本轮只发增量、
    /// 由 Cursor 服务端接着上一轮往下说。而 `conversation_id` 来自
    /// `gw_cursor::chat::affinity_key_from_body` = `hash(system + 第一条 user)` —— **纯内容派生,
    /// 不含任何客户身份**。
    ///
    /// 于是:同组两个不同 API key,只要 system 相同、开场那句话相同(通用 agent 提示词 +
    /// "hello" 就够了),就会拿到同一个 conversation_id;亲和又把它们钉到同一个账号;
    /// 两个条件一齐命中 → 后来那位客户的请求被当作前一位客户会话的**续写**发上去。
    /// gw-cursor 自己的文档注释早就点了这个名:「第三条在 `CURSOR_STATEFUL=1` 下是跨用户串话」,
    /// 而 `stateful` 的默认值是**开**(`CURSOR_STATEFUL` 只有显式 `=0` 才关)。
    ///
    /// 这条路径在本次改动之前就存在(Anthropic 入口打 cursor worker 同样成立),但把入口开给
    /// **NewAPI 这种多租户中转**正好凑齐了碰撞前提:同一个组下面挂着许多互不相识的客户。
    /// 所以在这里补上客户维度 —— 不同客户永不共享 conversation_id,`Continuation` 也就永不跨客户。
    ///
    /// ## 为什么只给 cursor
    ///
    /// kiro / dario 没有服务端续写:每轮重传全量历史,共享的键只影响**前缀缓存命中**,
    /// 泄不出上下文(kiro 发给上游的 conversationId 还额外按账号加盐)。给它们也分层会
    /// 无谓打散主链路的缓存亲和,而主链路是生产主力 —— 不动。
    fn affinity_scoped_by_client(&self) -> bool {
        true
    }

    /// 启动时挡掉代理配错的号:provider 侧 fail-closed、绝不回退默认出口,进池只会每次失败。
    fn validate_on_load(&self) -> bool {
        true
    }

    /// 刷新成功但重试仍 401:cursor 的 401 与号无关(推理面 / 请求形态问题,健康号同样 401),
    /// 证明不了凭据死亡,降级为 Other。真死号由刷新环节(shouldLogout / invalid_grant)判定。
    fn reclassify_retry_error(&self, mut e: UpstreamError) -> UpstreamError {
        if e.kind == UpstreamErrorKind::TokenInvalid {
            e.message = format!(
                "刷新成功但重试仍 401:推理面/请求形态问题而非凭据死亡(原 TokenInvalid 已降级): {e}"
            );
            e.kind = UpstreamErrorKind::Other;
        }
        e
    }

    /// 热应用缓存计费三参数(worker 每 30s 轮询 settings 后调用)。
    ///
    /// ⚠️ **在此之前 cursor 通道完全不读这三个旋钮**(整个 provider 没覆盖本方法,
    /// 用的是 trait 默认 no-op)。后果:admin 上把 `cache_read_multiplier` /
    /// `cache_cap_ratio` / `cache_floor_ratio` 调成什么,cursor 侧都不生效,
    /// 而 kiro 侧照常生效 —— 面板显示"已保存",cursor 照旧。
    ///
    /// 2026-08-17 用户决定:**照搬 kiro 的数值**(读同一份 settings),
    /// 两条通道的客户账单口径由此对齐。线上现值 `cap=0.95 / floor=0.75`
    /// (`multiplier` 未设 → 默认 1.8);`floor=0.75` 是主动让利:冷启动零命中
    /// 也按 75% 走缓存价(0.1× 输入价)。这是运营决策,不是估算的一部分。
    ///
    /// 参数落进 [`crate::cache_sim`] 的进程级 cell(下一个请求即读到),不放 provider
    /// 字段:当年 CLI 驱动(clidrv,2026-11-10 已删除)在拿不到 `&self` 的地方自估用量。
    ///
    /// present 字段才覆盖(与 kiro 同语义):缺失 = 沿用当前值、**不回落默认** ——
    /// 否则本版本不认识的新字段会把已调好的值悄悄冲掉(记忆
    /// caio-cache-billing-and-hot-settings 记的两处静默失败之一)。
    fn apply_hot_settings(&self, settings: &serde_json::Value) -> Result<(), String> {
        let mut b = crate::cache_sim::billing();
        if let Some(v) = settings
            .get("cache_read_multiplier")
            .and_then(|v| v.as_f64())
        {
            b.read_multiplier = v;
        }
        if let Some(v) = settings.get("cache_cap_ratio").and_then(|v| v.as_f64()) {
            b.cap_ratio = v;
        }
        if let Some(v) = settings.get("cache_floor_ratio").and_then(|v| v.as_f64()) {
            b.floor_ratio = v;
        }
        crate::cache_sim::set_billing(b);

    
        // cache_sim:cursor 有自己独立的 store(与 kiro 不共用全局),漏掉它 = 恒用 300s 而线上
        // 配 1800s,短 6 倍 → 大量该命中的会话被判冷启动(2026-08-17 标定实测:上游真实命中
        // 8180 而模拟器 raw_hit=0)。
        if let Some(v) = settings.get("cache_sim_ttl_secs").and_then(|v| v.as_u64()) {
            crate::cache_sim::global().set_ttl_secs(v);
        }
        if let Some(v) = settings.get("cache_max_sessions").and_then(|v| v.as_u64()) {
            crate::cache_sim::global().set_max_sessions(v as usize);
        }
        // 追加模型目录(探测位 / 进清单)。
        if let Some(v) = settings.get("cursor_extra_models") {
            match serde_json::from_value::<Vec<gw_core::config::ExtraModelSpec>>(v.clone()) {
                Ok(specs) => set_extra_models(extra_models_from_specs(&specs)),
                Err(e) => tracing::warn!("settings 里的 cursor_extra_models 形态不对,已忽略: {e}"),
            }
        }
        // 自造会话状态(缺键 = 不动当前值)。
        if let Some(v) = settings.get("cursor_blob_history") {
            match v.as_bool() {
                Some(b) => crate::blobhist::set_enabled(b),
                None => tracing::warn!(value = %v, "settings 里的 cursor_blob_history 不是布尔,已忽略"),
            }
        }
        // 挂流续跑(缺键 = 不动当前值;from_effective 总会带上)。
        if let Some(v) = settings.get("cursor_hold_tool_streams") {
            match v.as_bool() {
                Some(b) => crate::held::set_enabled(b),
                None => tracing::warn!(value = %v, "settings 里的 cursor_hold_tool_streams 不是布尔,已忽略"),
            }
        }
        // 护栏策略句与 CLI 说明:校验失败保留上一份有效值,并把原因报给 worker
        // (并进本轮设置同步错误,面板上说出来)。
        let mut errs: Vec<String> = Vec::new();
        if let Some(text) = settings.get("cursor_tool_guard").and_then(|v| v.as_str()) {
            if let Err(e) = set_tool_guard_policy(text) {
                tracing::error!("cursor 护栏策略句未应用(沿用上一份有效值): {e}");
                errs.push(format!("cursor 护栏策略句未应用: {e}"));
            }
        }
        // 工具回路中间轮提醒(折叠重铺时附在工具结果后)。缺键 = 不动(保持上一份/内置默认)。
        if let Some(text) = settings.get("cursor_tool_loop_nudge").and_then(|v| v.as_str()) {
            if let Err(e) = chat::set_tool_loop_nudge(text) {
                tracing::error!("cursor 工具回路提醒未应用(沿用上一份有效值): {e}");
                errs.push(format!("cursor 工具回路提醒未应用: {e}"));
            }
        }
        if errs.is_empty() { Ok(()) } else { Err(errs.join("; ")) }
    }

    /// 与 [`Self::apply_hot_settings`] 同进退(trait 文档:只覆盖其中一个就是在撒谎)。
    ///
    /// 声明 true 之后 `/health` 才会回显 cursor 侧此刻真正在用的计费参数,
    /// 面板那张「worker 实际生效值」卡才不会对着一个 no-op 报绿 —— 那正是
    /// claude-dario 踩过的坑(apply_hot_settings 是空壳,面板却显示一致)。
    fn hot_settings_supported(&self) -> bool {
        true
    }

    fn account_schema(&self) -> &'static [FieldSpec] {
        CURSOR_ACCOUNT_SCHEMA
    }

    /// 加载期 fail-fast:token 必填,且**配了 proxy 就必须构造得出来**。
    ///
    /// 把代理校验放在这里而不是只等第一次 chat:worker 启动时就报出来,
    /// 比让一个配错代理的号静静躺在池里、直到真有客户请求打上去才失败要好
    /// (那时表现是"号在池里但每次都失败",而不是"这个号配置有问题")。
    fn validate_account(&self, account: &Account) -> Result<(), UpstreamError> {
        Self::token_of(account)?;
        self.client_for(account).map(|_| ())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError> {
        Ok(models::list())
    }

    /// 账号级模型权限过滤(`extra.models` 白名单,语义见
    /// [`models::account_supports`])。
    ///
    /// Cursor 账号的模型权限不齐:一部分号只有 `composer`/`default`/`grok`,
    /// claude / gpt 要另外的计费额度。不过滤时每个 claude 请求都可能落到没权限的号上,
    /// 换来一次 `ERROR_RATE_LIMITED_CHANGEABLE` —— 调度层会记 `(号,模型)` 6h 不可用
    /// 并换号,但那是先赔一次失败才学会,且每次 TTL 过期要重赔。
    ///
    /// 调度器在锁内对每个候选号调用,所以这里**必须无副作用且快**:
    /// 只读 `extra`,不查上游。
    fn account_supports_model(&self, account: &Account, model: &str) -> bool {
        models::account_supports(account, model)
    }

    /// 会话亲和键。**必须覆盖** —— 不覆盖的后果见
    /// [`chat::affinity_key_from_body`](crate::chat) 的文档:trait 默认的 `None` 会让
    /// worker 把 `CallCtx.session_id`/`cache_key` 装成空串,进而让上游 `1.5` 与
    /// 两把每会话密钥全部退化。
    ///
    /// 对 Cursor 而言这比对 kiro 更要紧:kiro 丢亲和只是丢前缀缓存命中,
    /// Cursor 的会话历史在**服务端且属于某一个账号**,换号即失忆。
    fn affinity_key(&self, req: &ChatRequest) -> Option<String> {
        chat::affinity_key_from_body(&req.body)
    }

    async fn chat(&self, mut req: ChatRequest, ctx: &CallCtx) -> Result<ChatStream, UpstreamError> {
        // 第一件事:按客户端翻译(责任链,见 `chat::clients`)。兜底一步就是原来这里的
        // system 角色分流(中转在 messages 中段注入的 `role:"system"`,不分流的后果见
        // [`chat::route_system_role_messages`]),再加出口合规(tool_result 排在 user 回合最前)。
        // 必须在 conversation_id 派生 / 任何 to_turns 之前 —— 下游全部函数(tool_result 接续、
        // 指纹、自造历史)都假定 messages 是合规的 user/assistant 交替。
        let translation = chat::clients::translate(&mut req.body);
        if translation.reorder.runs > 0 {
            tracing::info!(
                account = %ctx.account.account_id,
                client = translation.kind.as_str(),
                runs = translation.reorder.runs,
                tool_results = translation.reorder.tool_results,
                other_blocks = translation.reorder.other_blocks,
                dropped_keys = translation.reorder.dropped_keys,
                "cursor 翻译:tool_result 挪到 user 回合最前(出口合规)"
            );
        }
        // 报文环境标准化(按当次号的人设)。放在 system 分流之后:被挪进 system 的
        // 中转注入内容也一并处理;早于 to_turns / 指纹 / 缓存模拟。
        gw_core::envnorm::apply_for(self.family(), &mut req.body, &ctx.account);

        let token = Self::token_of(&ctx.account)?;

        // 2026-11-10 起只剩线协议(wire)一条路:CLI 驱动(clidrv,起 cursor-agent 子进程)、
        // 旧推理面(inference,2026-09-09 起上游一律 401)、Bot 周池面(sandchat)已删除
        // (线上 0 流量,用户决定)。账号 `extra.driver` / `extra.pool` 与环境变量
        // `CURSOR_DRIVER` 不再生效。路由日志保留(监控按 route="wire" 统计流量)。
        tracing::info!(
            account = %ctx.account.account_id,
            model = %req.model,
            route = "wire",
            client = translation.kind.as_str(),
            has_tools = !chat::to_tools(&req.body).is_empty(),
            "cursor 路由"
        );

        let machine_id = Self::machine_id_of(&ctx.account, &token);
        let mac_machine_id = Self::mac_machine_id_of(&ctx.account, &token);
        // 连接句柄:流里出现连接卡死时丢弃这个号的连接重建(见 heal_stalled_connection)。
        let (client, conn) = self.client_and_handle(&ctx.account)?;

        // CLI 形态不需要 config_version(头表里没有这条)—— 省掉
        // GetServerConfig 握手(单次 5–6s),也顺带绕开「取不到就失败」的冷启动面。
        let config_version = if self.tuning.profile.is_cli() {
            String::new()
        } else {
            self.resolve_config_version(&ctx.account, &client, &token, &machine_id, &mac_machine_id)
                .await?
        };

        // conversation_id:优先 router 下发的 session_id(会话稳定),否则 cache_key,
        // 两者都空时**从请求体自行派生** —— 绝不让空串上线。
        //
        // ⚠️ 为什么要有最后这层兜底:worker 是拿 `affinity_key().unwrap_or_default()` 装
        // `session_id`/`cache_key` 的。只要 `affinity_key` 返回 `None`,这两个值就都是空串,
        // 而空 `1.5` + 每账号恒定的 blob/fs key 是真客户端不会有的形态(见
        // `chat::affinity_key_from_body`)。本 provider 现在覆盖了 `affinity_key`,
        // 但兜底仍留着:调度层将来若改口径,这里不能又静默退回空串。
        //
        // 过一遍 `conversation_uuid`:既保证非空,也把调度用的分组前缀折进哈希 ——
        // 那是 caio 内部命名空间,不该原样进上游报文。
        let material = if !ctx.session_id.is_empty() {
            ctx.session_id.clone()
        } else if !ctx.cache_key.is_empty() {
            ctx.cache_key.clone()
        } else {
            chat::affinity_key_from_body(&req.body).unwrap_or_else(|| {
                // 连内容都派生不出来(空消息体)→ 当作全新会话。
                tracing::debug!("cursor 无法派生 conversation_id,按新会话处理");
                uuid::Uuid::new_v4().to_string()
            })
        };
        let mut conversation_id = chat::conversation_uuid(&material);


        // CLI 形态:服务端持史(2026-08-16 实测成立,见 cli.rs 模块文档),所以必须
        // 盯住「调用方历史分叉」—— 调用方改写历史(/compact、编辑重发)后继续只发
        // 增量,等于让模型看两份不一样的历史。逐轮指纹前缀校验,分叉换新会话重铺。
        let cli_fps: Vec<u64> = if self.tuning.profile.is_cli() {
            chat::history_fps(&req.body)
        } else {
            Vec::new()
        };
        // 基础 id:分叉轮换掉 conversation_id 之前的那个(调用方亲和键派生、
        // 跨轮稳定)。会话成功登记一律落它之下,靠条目里的 `wire_id` 回指上游
        // 真正持有的会话 —— 否则分叉一次之后就永久 Diverged:每轮 lookup
        // 基础 id 看到的都是旧指纹、confirm 又写进每轮一新的 digest id,
        // 结果是每轮全量重铺 + 模拟缓存按每轮新 key 永不命中
        // (2026-09-13 pi 长会话生产实病)。
        let base_conversation_id = conversation_id.clone();
        // 本轮是「分叉换新 id 重铺」时为真:失败清理只忘新 id、保留基础条目
        // (下轮会再次分叉换新 id 重试),与其余轮的「忘掉基础条目、下轮首轮
        // 重铺」语义分开。
        let mut diverged_retry = false;
        let phase = if self.tuning.profile.is_cli() {
            let history = &cli_fps[..cli_fps.len().saturating_sub(1)];
            match self
                .conversations
                .cli_lookup(&conversation_id, &ctx.account.account_id, history)
            {
                CliLookup::Continue(wire_id) => {
                    // 服务端持有的会话是条目回指的 wire_id(分叉重铺过的会话
                    // 与基础 id 不同;未分叉时两者相等),续轮必须发到它。
                    conversation_id = wire_id;
                    run::Phase::Continuation
                }
                CliLookup::Fresh => run::Phase::Opening,
                CliLookup::Diverged => {
                    // 分叉必须换 conversation_id:旧的还在服务端手里,直接重铺会把
                    // 两段历史粘在一起。新 id 吃进全部指纹,内容变即 id 变。
                    let digest = cli_fps.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, f| {
                        (h ^ f).wrapping_mul(0x0000_0100_0000_01b3)
                    });
                    conversation_id =
                        chat::conversation_uuid(&format!("{material}\x1f{digest:016x}"));
                    diverged_retry = true;
                    run::Phase::Opening
                }
            }
        } else {
            self.conversations
                .phase_for(&conversation_id, &ctx.account.account_id)
        };

        let stream = chat::chat_stream(
            client,
            chat::RunCtx {
                host: self.cfg.agent_host.clone(),
                token,
                machine_id,
                mac_machine_id: Some(mac_machine_id),
                config_version,
                timezone: Self::timezone_of(&ctx.account),
                phase,
                profile: self.tuning.profile,
                conversation_id: conversation_id.clone(),
                // 模拟缓存键用基础 id:分叉轮换掉的是发给上游的 conversation_id,
                // 上游缓存与它无关(见 RunCtx::sim_conversation_id)。
                sim_conversation_id: base_conversation_id.clone(),
                // 与 token/machine_id/phase 同源:都从这一个 ctx.account 取,
                // 调度换号时缓存键跟着换(服务端会话 per-account,换号=冷启动)。
                account_id: ctx.account.account_id.clone(),
                shape: self.tuning.shape,
                context_frames: self.tuning.context_frames,
                keep_stream_open: self.tuning.keep_stream_open,
                assets: self.assets.clone(),
                notices: self.notices.clone(),
                shadow: self.shadow.clone(),
                sections: self.sections.clone(),
                // 影子血脉判据:与上面 cli_lookup 同一份指纹、同一个减末轮口径。
                shadow_fps: cli_fps[..cli_fps.len().saturating_sub(1)].to_vec(),
                shadow_fps_full: cli_fps.clone(),
                wire_descriptor_replay: self.tuning.wire_descriptor_replay,
            },
            req,
            // 只在**成功收尾**后才登记会话已建立。失败时清掉,下次从首轮重铺 ——
            // 服务端很可能没落下这一轮,而错用 Continuation 是无声的上下文丢失。
            //
            // 登记落**基础 id** 之下并带上本轮实际发给上游的 `wire_id`(见
            // `ConvEntry::wire_id` / `confirm_with_fps`):分叉重铺轮换了新 id,
            // 但只有基础 id 是下轮 lookup 会查的键。失败清理分两种:分叉重铺轮
            // 只忘新 id(基础条目保留,下轮再次分叉换新 id 重试);其余轮忘基础
            // 条目(下轮从首轮重铺)。
            {
                let reg = self.conversations.clone();
                let account_id = ctx.account.account_id.clone();
                let base_id = base_conversation_id;
                let wire_id = conversation_id.clone();
                Some(Arc::new(move |ok: bool| {
                    if ok {
                        reg.confirm_with_fps(&base_id, &account_id, cli_fps.clone(), &wire_id);
                    } else if diverged_retry {
                        reg.forget(&wire_id);
                    } else {
                        reg.forget(&base_id);
                    }
                }))
            },
        )
        .await?;
        Ok(heal_stalled_connection(stream, conn))
    }


    /// 用 `refresh_token` 换一份新凭据(标准 OAuth2,见 [`auth`])。
    ///
    /// 走该账号**专属出口**,与推理同 IP。刷新成功后 access/refresh 都更新 ——
    /// Cursor 的新 access_token 兼任新的 refresh_token。
    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
        let refresh_token = Self::opt_str(account, "refresh_token")
            // 没单独配 refresh_token 时退回 access_token:两者在 Cursor 侧本就是同一个
            // JWT(见 auth 模块),旧号只录了 access_token 也能续上。
            .or_else(|| Self::opt_str(account, "access_token"))
            .ok_or_else(|| {
                UpstreamError::new(
                    UpstreamErrorKind::TokenInvalid,
                    "cursor 账号既无 refresh_token 也无 access_token,无法刷新",
                )
            })?;

        let client = self.client_for(account)?;
        let fresh = auth::refresh(&client, &refresh_token).await?;

        let mut updated = account.clone();
        updated.extra.insert(
            "access_token".to_string(),
            serde_json::Value::String(fresh.access_token.clone()),
        );
        updated.extra.insert(
            "refresh_token".to_string(),
            serde_json::Value::String(fresh.refresh_token),
        );

        // 写回 `expires_at`。**不写它的后果**:gw-app 的 `has_fresh_token` 对缺失该字段
        // 的号「视为永鲜」→ 从不主动刷新 → 每个过期号都要先吃一次 401/403 才被动刷,
        // 而 403 的分类本身就是雷区(出口 IP 被拦也是 403)。能不走到那步就别走。
        if let Some(exp) = auth::token_expires_at(&fresh.access_token) {
            updated.extra.insert(
                "expires_at".to_string(),
                serde_json::Value::String(auth::format_unix_utc(exp)),
            );
        } else {
            tracing::warn!(account = %account.account_id,
                "cursor 新 token 解不出 exp,不写 expires_at(gw-app 会当永鲜,靠 403 兜底)");
        }

        // token 变了 → client-key / session-id / 派生的 machineId 全跟着变。
        // 缓存的 config_version 是按旧身份取的,必须作废,否则新 token 配旧 config
        // 会是一个服务端没见过的组合。
        //
        // 缓存键已含身份指纹(见 `config_cache_key`),换 token 天然换条目;
        // 这里再把该号名下的旧条目清掉,免得它们在表里躺到过期。
        {
            let prefix = format!("{}\u{0}", account.account_id);
            self.config_cache
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|k, _| !k.starts_with(&prefix));
        }
        tracing::info!(account = %account.account_id, "cursor token 已刷新");
        Ok(updated)
    }

    /// 官方账期用量(只读 `GetCurrentPeriodUsage` + `GetHardLimit`)。走账号专属出口,
    /// 与推理同 IP。
    async fn account_quota(
        &self,
        account: &Account,
    ) -> Result<Option<AccountQuota>, UpstreamError> {
        let client = self.client_for(account)?;
        let mut q = usage::get_account_quota(&client, account, &self.cfg.api_host).await?;
        // 门控模型的数据保留政策同意(NoZDR consent):未同意的号请求 fable 等模型
        // 会被上游 ERROR_MODEL_BLOCKED 全灭(2026-09-12 生产事故:fable-5-1 自上架
        // 0 成功,真因被 CLI 吞成「未成功收尾」)。配额查询是每个号周期性必走之路
        //(新进预热 / TTL 刷新 / sweep),顺路把已知同意点掉,等价导入即自动 Accept。
        // 进程内去重;失败不拖垮主查询,下个配额周期重试。
        for (model_id, consent_version) in usage::KNOWN_NOZDR_CONSENTS {
            let key = format!("{}\u{0}{}", account.account_id, model_id);
            if self
                .nozdr_consented
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(&key)
            {
                continue;
            }
            match usage::set_user_no_zdr_model_consent(
                &client,
                account,
                &self.cfg.api_host,
                model_id,
                consent_version,
            )
            .await
            {
                // 上游 200 就当落定(含 consented=false 的未确认:也记去重,免得每个
                // 配额周期都重放一次写操作;进程重启后自然再试)。
                Ok(consented) => {
                    self.nozdr_consented
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(key);
                    if consented {
                        tracing::info!(account = %account.account_id, model = model_id,
                            "cursor 数据保留政策已同意(NoZDR consent)");
                    } else {
                        tracing::warn!(account = %account.account_id, model = model_id,
                            "cursor NoZDR consent 上游未确认(consented=false),本进程内不再重试");
                    }
                }
                Err(e) => tracing::debug!(account = %account.account_id, model = model_id,
                    "cursor NoZDR consent 失败(下周期重试,不影响配额): {e}"),
            }
        }
        // 超额开关/上限来自 GetHardLimit(用量接口只在**已开启**时给数字,推不出开关态)。
        //
        // 这一跳失败**不能**让整个配额查询失败:套餐额度是账号页的主信息,不该被一个
        // 附加字段拖垮(超额那栏退化成"—")。
        match usage::get_on_demand(&client, account, &self.cfg.api_host).await {
            Ok(od) => {
                // 已用金额只有用量接口有,GetHardLimit 不带 → 从上一步的结果里接回来,
                // 否则会把已用覆盖成 0。
                let used = q.on_demand.as_ref().map(|p| p.used).unwrap_or(0.0);
                q.on_demand = Some(gw_core::provider::OnDemandQuota { used, ..od });
            }
            Err(e) => tracing::debug!(account = %account.account_id,
                "cursor GetHardLimit 失败,超额开关未知(不影响套餐额度): {e}"),
        }
        // Bot(Grok Bot / Sand)周池:与套餐月池独立的第三池。失败同样不拖垮主查询。
        match usage::get_sand_usage(&client, account, &self.cfg.api_host).await {
            Ok(Some(w)) => q.windows.push(w),
            Ok(None) => {} // 无 Bot 池(免费号/未开通)或上游没给百分比:不出空条。
            Err(e) => tracing::debug!(account = %account.account_id,
                "cursor GetSandUsageStatus 失败,Bot 池未知(不影响套餐额度): {e}"),
        }
        Ok(Some(q))
    }

    fn on_demand_supported(&self) -> bool {
        true
    }

    /// 设超额额度(`SetHardLimit`)。**写操作**,只由 admin 显式调用。
    async fn set_on_demand_limit(
        &self,
        account: &Account,
        limit_usd: Option<u32>,
    ) -> Result<(), UpstreamError> {
        let client = self.client_for(account)?;
        usage::set_on_demand(&client, account, &self.cfg.api_host, limit_usd).await
    }
}
