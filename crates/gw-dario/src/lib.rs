//! gw-dario —— Claude OAuth 号经 dario 直连 api.anthropic.com 的 Provider。
//!
//! 每个号一个**原版** dario 进程(由 `sidecars/dario-supervisor` 管理),令牌交接给 dario 后
//! 由它独占刷新;caio 只负责交接、同步出口、按号路由。见 [`supervisor`]。
mod cc;
mod chat;
mod credentials;
mod datefmt;
pub mod oauth;
pub mod supervisor;
mod token;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use async_trait::async_trait;
use gw_core::account::{Account, FieldSpec, FieldType};
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::model::ModelInfo;
use gw_core::provider::{CallCtx, ChatRequest, ChatStream, Provider};

pub use credentials::parse_cc_credentials;

/// 离线回放(`examples/cc_translate_replay.rs`):给一条请求体和 dario 的 CC 模板原文,返回它在
/// `dario_cc_translate=on` 时会怎么发(透传 / 翻译后的报文 / 拒绝原因)。只算不发。
#[doc(hidden)]
pub fn cc_replay(body: &serde_json::Value, template_json: &str) -> Result<serde_json::Value, String> {
    let tpl = cc::template::CcTemplate::parse(template_json)?;
    Ok(cc::replay_summary(body, &tpl))
}
pub use datefmt::format_rfc3339_z;

const DARIO_ACCOUNT_SCHEMA: &[FieldSpec] = &[
    FieldSpec::new("account_id", "账号 ID", FieldType::String, true),
    FieldSpec::new("access_token", "Access Token", FieldType::Password, false)
        .with_help("OAuth access token;导入 .credentials.json 自动填充"),
    FieldSpec::new("refresh_token", "Refresh Token", FieldType::Password, true)
        .with_help("OAuth refresh token;首次使用时交接给该号的 dario 进程,之后由 dario 刷新,caio 侧清空"),
    FieldSpec::new("expires_at", "过期时间(RFC3339 Z)", FieldType::String, false),
    FieldSpec::new("device_id", "Device ID", FieldType::String, false)
        .with_help("缺失时导入生成稳定 UUID(保 five_hour 计费分类)"),
    FieldSpec::new("account_uuid", "Account UUID", FieldType::String, false),
    FieldSpec::new("proxy", "出口代理", FieldType::String, false),
];

/// 校验并**规范化** dario per-account 出口代理 URL。chat 与 refresh 共用此函数,
/// 保证两条路径对同一字符串得到**字节一致**的判定与缓存键(消除"chat 放行/refresh
/// 回退"的不对称——审查 Skeptic#4/Architect#2)。仅 http(s):dario sidecar 不支持
/// socks。⚠️ 该代理必须是**固定单出口 IP**(非 rotating/backconnect)——refresh(reqwest)
/// 与 chat(dario/Bun)是各自独立连接,只有静态出口才能保证刷新 IP==发包 IP(Skeptic#2)。
pub(crate) fn normalize_dario_proxy(raw: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|e| format!("invalid proxy URL: {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        s => return Err(format!("dario proxy must be http(s); got {s} (socks unsupported by sidecar)")),
    }
    if url.host_str().unwrap_or("").is_empty() {
        return Err("proxy URL missing host".to_string());
    }
    Ok(url.as_str().to_string())
}

/// dario-supervisor 默认地址(配置键沿用 `dario.sidecar_url`)。
pub const DEFAULT_SUPERVISOR_URL: &str = "http://127.0.0.1:8788";

#[derive(Debug, Clone, Default)]
pub struct DarioConfig {
    /// dario-supervisor 地址(历史键名;以前指单个注入式 sidecar)。
    pub sidecar_url: String,
    /// 管理器鉴权与各 dario 进程的 DARIO_API_KEY 共用。
    pub api_key: String,
    /// dario 的 CC 模板数据文件(翻译成 Claude Code 报文用,见 [`cc`])。空 = 不翻译。
    pub cc_template_path: String,
}

impl DarioConfig {
    fn from_cfg(cfg: &serde_json::Value) -> Self {
        let d = cfg.get("dario");
        DarioConfig {
            sidecar_url: d.and_then(|v| v.get("sidecar_url")).and_then(|v| v.as_str())
                .filter(|s| !s.is_empty()).unwrap_or(DEFAULT_SUPERVISOR_URL).to_string(),
            api_key: d.and_then(|v| v.get("api_key")).and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            // 顶层键(见 gw-core `SystemConfig::dario_cc_template_path`:dario 段带 deny_unknown_fields)。
            cc_template_path: cfg.get("dario_cc_template_path").and_then(|v| v.as_str())
                .unwrap_or_default().trim().to_string(),
        }
    }
}

pub struct DarioProvider {
    cfg: DarioConfig,
    /// 直连 loopback(**无代理 + connect 超时**):caio→dario 本机回环,出口代理由 dario 负责。
    sidecar_client: reqwest::Client,
    /// 注入的本组 egress client(worker 级默认出口):账号无 extra.proxy 时 OAuth 换码用它。
    egress_client: reqwest::Client,
    /// 按账号 extra.proxy → OAuth 换码 client 缓存。换码出口必须与该号 dario 进程的出口
    /// (`--upstream-proxy`,同一 proxy 串)一致:铸 token IP == 刷新 IP == 发包 IP。
    /// key = 规范化 proxy URL;reqwest::Client 内部 Arc,clone 廉价。
    proxy_clients: Mutex<HashMap<String, reqwest::Client>>,
    /// dario-supervisor 客户端:交接、出口同步、按号拿端口。
    sup: supervisor::Supervisor,
    /// 每账号 5h/7d 利用率快照:chat 从 sidecar 透传的 Anthropic 限额头捕获写入,
    /// `account_quota` 只读返回(OAuth/Max 无只读用量接口,只能从真实流量捕获)。
    ratelimit: chat::RateLimitStore,
}

impl DarioProvider {
    /// 按开关准备发给 dario 的报文;不支持的客户端转成本地 400(`BadRequest`:不换号、不扣号)。
    fn prepare_cc(&self, body: &serde_json::Value) -> Result<cc::Prepared, UpstreamError> {
        // 开关关着就连模板文件都不碰(每个请求一次 stat + 全局锁,关着时没必要)。
        if cc::mode() == gw_core::config::dario_cc::DarioCcMode::Off {
            return Ok(cc::Prepared::legacy());
        }
        let tpl = cc::template::load(&self.cfg.cc_template_path);
        // 文案由我方完全掌控(不含上游原文与账号信息),走「对客户端可见」的入口,
        // 否则客户端只会看到通用的「请求无效,请检查请求体后重试」而反复重试。
        cc::prepare(body, tpl.as_deref()).map_err(|r| {
            UpstreamError::bad_request_visible(format!(
                "This route does not support this client yet ({}): {}",
                r.client.as_str(),
                r.reason
            ))
        })
    }

    pub fn new(cfg: DarioConfig) -> Self {
        Self::with_clients(cfg, reqwest::Client::new(), reqwest::Client::new())
    }
    pub fn with_clients(cfg: DarioConfig, sidecar_client: reqwest::Client, egress_client: reqwest::Client) -> Self {
        let url = if cfg.sidecar_url.trim().is_empty() { DEFAULT_SUPERVISOR_URL } else { cfg.sidecar_url.as_str() };
        let sup = supervisor::Supervisor::new(url, &cfg.api_key, sidecar_client.clone()).unwrap_or_else(|e| {
            tracing::error!(error = %e, "dario.sidecar_url 非法,回退默认 {DEFAULT_SUPERVISOR_URL}");
            supervisor::Supervisor::new(DEFAULT_SUPERVISOR_URL, &cfg.api_key, sidecar_client.clone())
                .expect("默认管理器地址合法")
        });
        Self {
            cfg,
            sidecar_client,
            egress_client,
            proxy_clients: Mutex::new(HashMap::new()),
            sup,
            ratelimit: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// refresh_auth 的出口 client。**fail-closed**(审查 Skeptic#1/Architect#1 共识阻断点):
    /// - 账号**未设** proxy → worker 级默认 egress_client(与 chat 默认出口一致);
    /// - 设了**合法 http(s)** proxy → 走该代理(与 chat 的 x-dario-upstream-proxy **同一**
    ///   规范化 URL → 同出口);
    /// - 设了但**非法/socks/构造失败** → **返回 Err 拒绝刷新**,绝不静默回退默认 IP
    ///   (回退 = 刷新 IP≠发包 IP = 正中关联封号)。刷新失败 = 该号暂不可用(token 过期后
    ///   下线),是**安全**的失败,远好于换 IP 刷新。
    /// 锁内同步构造(reqwest builder 无 await),消除并发重复 build;poison 用 into_inner 恢复。
    /// 按 **proxy 字符串**选出口 client(fail-closed)。`egress_client_for(account)`(refresh)与
    /// `oauth_exchange`(铸 token)**共用**此函数,保证「同一 proxy 串 → 同一出口 client」——egress
    /// 选择只依赖 proxy 串这一个输入,消除对 Account 其它字段的隐藏耦合(审查 Architect#4)。
    /// - `None`/空 → worker 级默认 egress_client(与 chat 默认出口一致);
    /// - 合法 http(s) → 该代理(与 chat 的 x-dario-upstream-proxy 同一规范化 URL → 同出口);
    /// - 非法/socks/构造失败 → **Err 拒绝**,绝不静默回退默认 IP(回退=换 IP=关联封号)。
    fn client_for_proxy(&self, raw_proxy: Option<&str>) -> Result<reqwest::Client, UpstreamError> {
        let raw = match raw_proxy.map(str::trim).filter(|s| !s.is_empty()) {
            Some(p) => p,
            None => return Ok(self.egress_client.clone()),
        };
        let norm = normalize_dario_proxy(raw).map_err(|e| {
            UpstreamError::new(
                UpstreamErrorKind::BadRequest,
                format!("dario proxy 非法({e}),拒绝以防换 IP(刷新/铸 token 出口必须==发包出口)"),
            )
        })?;
        let mut cache = self.proxy_clients.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(c) = cache.get(&norm) {
            return Ok(c.clone());
        }
        let proxy = reqwest::Proxy::all(&norm).map_err(|e| {
            UpstreamError::new(UpstreamErrorKind::BadRequest, format!("dario proxy 解析失败: {e}"))
        })?;
        let client = reqwest::Client::builder()
            .proxy(proxy)
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| {
                UpstreamError::new(UpstreamErrorKind::Other, format!("build dario proxy client: {e}"))
            })?;
        cache.insert(norm, client.clone());
        Ok(client)
    }

    /// 该账号的出口 client:仅取 `extra.proxy`,委托 [`Self::client_for_proxy`]。
    #[cfg(test)]
    fn egress_client_for(&self, account: &Account) -> Result<reqwest::Client, UpstreamError> {
        self.client_for_proxy(account.extra_str("proxy"))
    }
    pub fn from_config(cfg: &serde_json::Value, egress_client: reqwest::Client) -> anyhow::Result<Arc<dyn Provider>> {
        let c = DarioConfig::from_cfg(cfg);
        if c.api_key.is_empty() {
            tracing::warn!("dario.api_key 为空:dario-supervisor 要求非空 key,所有请求都会 401");
        }
        // 地址非法直接报错(启动期),不在运行中静默回退。
        supervisor::Supervisor::new(&c.sidecar_url, &c.api_key, reqwest::Client::new())?;
        let sidecar_client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(5))
            // 回环连接不留空闲池:sidecar(bun)对 keep-alive 空闲连接关闭得很凶,
            // 池里复用到已死的连接会让 POST(ensure/对话)0ms 瞬间失败且不留痕迹
            // (2026-10-12 Bean 连续 8 次 Network 502 的根因)。回环建连亚毫秒,不值得池化。
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| anyhow::anyhow!("build dario sidecar client: {e}"))?;
        Ok(Arc::new(Self::with_clients(c, sidecar_client, egress_client)))
    }
}

#[async_trait]
impl Provider for DarioProvider {
    fn family(&self) -> &'static str { "claude-dario" }

    /// 令牌已交接给该号自己的 dario 进程独占刷新(见 `supervisor`)。caio 再刷新同一条
    /// refresh token 会被 Anthropic 判为复用、整族作废。
    fn refresh_exempt(&self, _account: &Account) -> bool {
        true
    }

    /// 挡掉导入时缺凭据(access_token + refresh_token)的号。
    fn validate_on_load(&self) -> bool {
        true
    }

    fn startup_warnings(&self, egress: &str) -> Vec<String> {
        vec![format!(
            "dario 组:本 worker 的 egress({egress})必须与所连 dario sidecar 的出口为同一 IP,且等于该账号登录授权的来源 IP(二者均 direct 或同一代理皆可;不一致=刷新 IP≠发包 IP,关联封号风险)"
        )]
    }
    fn account_schema(&self) -> &'static [FieldSpec] { DARIO_ACCOUNT_SCHEMA }

    fn validate_account(&self, account: &Account) -> Result<(), UpstreamError> {
        // 已交接给 dario 的号 caio 侧不再有 refresh_token,凭 dario_rev 认定有效。
        let present = |k: &str| account.extra_str(k).map(str::trim).is_some_and(|s| !s.is_empty());
        if !(present("refresh_token") || present("access_token") || present(supervisor::HANDOVER_REV_KEY)) {
            return Err(UpstreamError::bad_request("claude-dario account missing access_token & refresh_token"));
        }
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError> {
        let mk = |id: &str, d: &str| {
            let mut m = ModelInfo::new(id);
            m.display_name = Some(d.into()); m.context_length = Some(200_000);
            m.supports_thinking = true; m.supports_tools = true; m.supports_vision = true; m
        };
        Ok(vec![
            // 订阅直连是 passthrough(上游认什么就能跑什么),清单按当前订阅实际可用维护:
            // 5.5 全系 2026-10-12 实测可用(opus/sonnet/haiku,含 adaptive thinking)。
            mk("claude-opus-5-5", "Claude Opus 5.5 (dario)"),
            mk("claude-sonnet-5-5", "Claude Sonnet 5.5 (dario)"),
            mk("claude-haiku-5-5", "Claude Haiku 5.5 (dario)"),
            mk("claude-opus-4-8", "Claude Opus 4.8 (dario)"),
            mk("claude-sonnet-4-6", "Claude Sonnet 4.6 (dario)"),
            mk("claude-haiku-4-5", "Claude Haiku 4.5 (dario)"),
        ])
    }

    /// 会话亲和键:优先取 `metadata.user_id` 里的 Claude Code 原生 session_id(UUID),
    /// 回退首条用户消息文本哈希。多号下让同一会话钉到组内同账号,最大化 Anthropic
    /// prompt cache 命中(按账号隔离)。详见 [`chat::affinity_key_from_body`]。
    ///
    /// 注:session_id 在请求 **body** 内(`metadata.user_id`),不受 caio router 第一跳
    /// 丢客户端 header 影响,故无需依赖 `CallCtx::session_id` 下发。
    fn affinity_key(&self, req: &ChatRequest) -> Option<String> {
        chat::affinity_key_from_body(&req.body)
    }

    async fn chat(&self, mut req: ChatRequest, ctx: &CallCtx) -> Result<ChatStream, UpstreamError> {
        // 报文环境标准化(按当次号的人设;dario 不改客户端系统提示词里的环境段,只能在这里做)。
        // 纯改 body、不碰网络,放在最前面:翻译要基于标准化之后的报文。
        gw_core::envnorm::apply_for(self.family(), &mut req.body, &ctx.account);
        // 翻译成 Claude Code 报文(开关 off 时原样)。不支持的客户端在这里就本地拒绝,不交接、不发上游。
        let prepared = self.prepare_cc(&req.body)?;
        self.sup.provision(&ctx.account).await?;
        let port = self.sup.ensure(&ctx.account.account_id).await?;
        let url = self.sup.process_url(port, "/v1/messages");
        chat::chat_via_sidecar(&self.cfg, &self.sidecar_client, self.ratelimit.clone(), req, prepared, ctx, &url).await
    }

    /// 只读本线路自己的开关 `dario_cc_translate`。缓存计费、图像压缩等 provider 级设置 dario 仍然
    /// 不热应用,所以 [`Provider::hot_settings_supported`] 保持 `false`(那个标志在面板上的含义是
    /// 「缓存计费这半边也热生效」);本开关的实际取值经 [`Self::settings_fingerprints`] 回显。
    fn apply_hot_settings(&self, settings: &serde_json::Value) -> Result<(), String> {
        let key = gw_core::config::dario_cc::SETTING_KEY;
        let raw = settings.get(key).and_then(|v| v.as_str()).unwrap_or("");
        match gw_core::config::dario_cc::DarioCcMode::parse(raw) {
            Ok(m) => {
                if m != cc::mode() {
                    tracing::info!(from = cc::mode().as_str(), to = m.as_str(), "dario 翻译成 CC 报文的开关已切换");
                }
                cc::set_mode(m);
                Ok(())
            }
            Err(e) => {
                tracing::error!("{key} 未应用(沿用上一份有效值): {e}");
                Err(format!("{key} 未应用: {e}"))
            }
        }
    }

    /// 回显本线路翻译开关与模板版本:回答「线上实际在按哪个模式、哪版 CC 模板翻译」。
    fn settings_fingerprints(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert("dario_cc_translate".into(), serde_json::json!(cc::mode().as_str()));
        let version = cc::template::load(&self.cfg.cc_template_path).map(|t| t.version.clone());
        m.insert("dario_cc_template".into(), serde_json::json!(version));
        m
    }

    /// 配额是本地廉价读(从聊天流量捕获的内存快照,无上游往返)→ 告诉 gw-app 别拿"昂贵调用"
    /// 的 TTL 节流(尤其别让陈旧 None 挡住刚捕获的快照,见 Provider::quota_is_local)。
    fn quota_is_local(&self) -> bool {
        true
    }

    /// 只读返回该账号最近一次从聊天流量捕获的 5h/7d 利用率快照(无积分概念,见 [`chat::DarioRateLimit`])。
    /// 未发过请求的号返回 `None`(面板显示「—」),因 Anthropic OAuth/Max 无只读用量接口、
    /// 且红线禁止真号探测,只能从真实流量响应头捕获。worker 配额缓存按 TTL 调本方法,纯读不发包。
    ///
    /// worker 每 4-5 分钟对每个号扫一次额度:借这个机会做交接,号创建后几分钟内令牌就归 dario,
    /// 不必等第一个请求(Provider 没有「号新增」钩子)。交接失败只记日志,不影响读快照。
    async fn account_quota(&self, account: &Account) -> Result<Option<gw_core::provider::AccountQuota>, UpstreamError> {
        if !account.disabled {
            if let Err(e) = self.sup.provision(account).await {
                tracing::warn!(account = %account.account_id, error = %e, "dario 交接/出口同步失败(下次轮询重试)");
            }
        }
        let m = self.ratelimit.lock().unwrap_or_else(|p| p.into_inner());
        Ok(m.get(&account.account_id).and_then(|s| s.to_quota()))
    }

    /// 令牌由该号的 dario 进程独占刷新,caio 绝不刷新(两方刷新同一条 refresh token 会被
    /// Anthropic 判为复用、整族作废)。worker 已对 claude-dario 豁免刷新,这里再兜底成空操作。
    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
        Ok(account.clone())
    }

    /// 交接成功后待落库的 extra 增量:清空 refresh_token、记 dario_rev / 交接时间。
    fn poll_token_updates(&self) -> Vec<(String, std::collections::BTreeMap<String, serde_json::Value>)> {
        self.sup.drain_updates()
    }

    /// OAuth 上号:`authorization_code` → token set。
    /// **复用 `egress_client_for`**(同 fail-closed + 同缓存):换码出口由该号 `proxy` 决定,
    /// 与它将来 refresh/chat 字节一致的同一 egress → 铸 token IP == 刷新 IP == 发包 IP。
    /// proxy 非法(socks/缺 host)直接 Err,绝不静默走默认 IP 换码。
    async fn oauth_exchange(
        &self,
        proxy: Option<&str>,
        code: &str,
        verifier: &str,
        state: &str,
    ) -> Result<serde_json::Value, UpstreamError> {
        // 换码出口 = 该号 proxy 选出的 client = 它将来 refresh/chat 的同一出口(同一 proxy 串)。
        let client = self.client_for_proxy(proxy)?;
        let fields = oauth::exchange(&client, code, verifier, state).await?;
        Ok(serde_json::Value::Object(fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gw_core::provider::Provider;
    #[test] fn family_is_claude_dario() {
        assert_eq!(DarioProvider::new(DarioConfig::default()).family(), "claude-dario");
    }
    #[test] fn schema_has_required_refresh_token() {
        let p = DarioProvider::new(DarioConfig::default());
        let s = p.account_schema();
        assert!(s.iter().any(|f| f.name == "refresh_token" && f.required));
        assert!(s.iter().any(|f| f.name == "device_id"));
    }
    #[test] fn from_config_reads_sidecar() {
        let cfg = serde_json::json!({"dario":{"sidecar_url":"http://127.0.0.1:39100","api_key":"k"}});
        assert_eq!(DarioProvider::from_config(&cfg, reqwest::Client::new()).unwrap().family(), "claude-dario");
    }
    #[test] fn from_config_warns_empty_api_key_but_builds() {
        // 空 api_key 不阻断构造(loopback dario 可不设 DARIO_API_KEY),仅记 warn。
        assert!(DarioProvider::from_config(&serde_json::Value::Null, reqwest::Client::new()).is_ok());
    }

    #[test]
    fn egress_client_for_failclosed_and_caches() {
        use std::collections::BTreeMap;
        fn acct(proxy: Option<&str>) -> Account {
            let mut e = BTreeMap::new();
            if let Some(p) = proxy { e.insert("proxy".into(), serde_json::json!(p)); }
            Account { account_id: "d1".into(), provider: "claude-dario".into(), max_concurrency: 2, disabled: false, created_at: 0, extra: e }
        }
        let p = DarioProvider::new(DarioConfig::default());
        // 无 proxy → Ok(worker 默认出口),缓存不增长。
        assert!(p.egress_client_for(&acct(None)).is_ok());
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 0);
        // socks → 不支持 → Err(fail-closed:不回退默认 IP,不缓存)。
        assert!(p.egress_client_for(&acct(Some("socks5://1.2.3.4:1080"))).is_err());
        // 非法(缺 host)→ Err。
        assert!(p.egress_client_for(&acct(Some("http://"))).is_err());
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 0);
        // http(含内嵌 basic auth)→ Ok,缓存一份;同 URL 二次复用不新增。
        assert!(p.egress_client_for(&acct(Some("http://caio:pw@45.77.219.188:13128"))).is_ok());
        assert!(p.egress_client_for(&acct(Some("http://caio:pw@45.77.219.188:13128"))).is_ok());
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn oauth_exchange_failclosed_on_socks_proxy() {
        // socks proxy 非法 → egress_client_for 直接 Err(在任何换码网络请求之前),
        // 绝不静默走默认 IP 换码(铸 token IP≠发包 IP = 关联封号)。
        let p = DarioProvider::new(DarioConfig::default());
        let r = p
            .oauth_exchange(Some("socks5://1.2.3.4:1080"), "code", "verifier", "state")
            .await;
        assert!(r.is_err(), "socks proxy 必须 fail-closed");
    }

    /// 端到端:chat → 管理器(交接已完成的号)→ ensure 拿端口 → 发给该号的 dario 进程。
    /// 断言发出去的只有入站 key 与会话键,令牌 / 身份 / 代理头全都不再出现;refresh_auth 是空操作。
    #[tokio::test]
    async fn chat_routes_to_the_account_process_without_credential_headers() {
        use axum::routing::{get, post};
        use futures::StreamExt;
        use std::collections::BTreeMap;
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let body_seen: Arc<Mutex<serde_json::Value>> = Arc::new(Mutex::new(serde_json::Value::Null));
        // 模拟该号的 dario 进程。
        let (seen2, body2) = (seen.clone(), body_seen.clone());
        let dario = axum::Router::new().route("/v1/messages", post(move |h: axum::http::HeaderMap, b: axum::Json<serde_json::Value>| {
            let (seen, body) = (seen2.clone(), body2.clone());
            async move {
                *seen.lock().unwrap() = h.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string())).collect();
                *body.lock().unwrap() = b.0;
                let sse = concat!(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
                    "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":2}}\n\n",
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                );
                ([("content-type", "text/event-stream"), ("anthropic-ratelimit-unified-5h-utilization", "0.25")], sse)
            }
        }));
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dport = dl.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(dl, dario).await.unwrap() });
        // 模拟管理器:号已交接(rev 与 caio 侧 dario_rev 一致),ensure 返回上面进程的端口。
        let sup = axum::Router::new()
            .route("/accounts/{id}", get(|| async { axum::Json(serde_json::json!({"rev": "rt-x", "port": 1, "proxy": null, "needsRelogin": false})) }))
            .route("/accounts/{id}/ensure", post(move || async move { axum::Json(serde_json::json!({"port": dport})) }));
        let sl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let saddr = sl.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(sl, sup).await.unwrap() });

        let p = DarioProvider::new(DarioConfig { sidecar_url: format!("http://{saddr}"), api_key: "k-in".into(), ..Default::default() });
        let mut e = BTreeMap::new();
        e.insert("dario_rev".into(), serde_json::json!("rt-x"));
        e.insert("access_token".into(), serde_json::json!("stale-at"));
        let account = Arc::new(Account { account_id: "a@b.com".into(), provider: "claude-dario".into(), max_concurrency: 2, disabled: false, created_at: 0, extra: e });
        let ctx = CallCtx { account: account.clone(), session_id: "sess-1".into(), cache_key: String::new() };
        let req = ChatRequest::from_anthropic_body(serde_json::json!({"model": "claude-opus-4-8", "max_tokens": 8, "stream": false, "messages": [{"role": "user", "content": "hi"}]}));
        let items: Vec<_> = p.chat(req, &ctx).await.unwrap().collect().await;
        assert!(items.iter().all(|i| i.is_ok()), "{items:?}");

        let headers = seen.lock().unwrap().clone();
        let h = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(h("x-api-key").as_deref(), Some("k-in"));
        assert_eq!(h("x-session-id").as_deref(), Some("sess-1"));        for gone in ["x-dario-upstream-token", "x-dario-device-id", "x-dario-account-uuid", "x-dario-upstream-proxy", "authorization"] {
            assert!(h(gone).is_none(), "{gone} 不应再发给 dario");
        }
        assert_eq!(body_seen.lock().unwrap()["stream"], true, "仍强制流式");
        // 额度快照仍从响应头捕获。
        let q = p.account_quota(&account).await.unwrap();
        assert!(q.is_some());
        // 令牌归 dario:refresh_auth 原样返回,不发任何请求。
        let same = p.refresh_auth(&account).await.unwrap();
        assert_eq!(same.extra, account.extra);

        // 回归:会话键带 NUL(组命名空间亲和键「组\0键」)→ 进 x-session-id 头前折叠,
        // 不能再让 reqwest builder error(2026-10-12 dario 组经 router 全灭)。
        let ctx2 = CallCtx { account, session_id: "DGRP\0sess-2".into(), cache_key: String::new() };
        let req2 = ChatRequest::from_anthropic_body(serde_json::json!({"model": "claude-opus-4-8", "max_tokens": 8, "stream": false, "messages": [{"role": "user", "content": "hi"}]}));
        let items2: Vec<_> = p.chat(req2, &ctx2).await.unwrap().collect().await;
        assert!(items2.iter().all(|i| i.is_ok()), "带 NUL 的会话键必须能发出去: {items2:?}");
        let headers2 = seen.lock().unwrap().clone();
        let sent = headers2.iter().find(|(n, _)| n == "x-session-id").map(|(_, v)| v.clone());
        assert_eq!(sent.as_deref(), Some("DGRP~sess-2"), "NUL 折叠成 '~'");
        assert!(!sent.as_deref().unwrap_or_default().contains('\0'));
    }

    #[test]
    fn validate_accepts_handed_over_account_without_tokens() {
        use std::collections::BTreeMap;
        let p = DarioProvider::new(DarioConfig::default());
        let mut e = BTreeMap::new();
        e.insert("refresh_token".into(), serde_json::json!(""));
        e.insert("dario_rev".into(), serde_json::json!("rt-x"));
        let a = Account { account_id: "a".into(), provider: "claude-dario".into(), max_concurrency: 1, disabled: false, created_at: 0, extra: e };
        assert!(p.validate_account(&a).is_ok());
        let empty = Account { extra: BTreeMap::new(), ..a };
        assert!(p.validate_account(&empty).is_err());
    }

    #[test]
    fn normalize_dario_proxy_rules() {
        assert!(normalize_dario_proxy("http://h:8080").is_ok());
        assert!(normalize_dario_proxy("https://user:pw@h:8443").is_ok());
        assert!(normalize_dario_proxy("socks5://h:1080").is_err());
        assert!(normalize_dario_proxy("http://").is_err());
        assert!(normalize_dario_proxy("not a url").is_err());
    }
}
