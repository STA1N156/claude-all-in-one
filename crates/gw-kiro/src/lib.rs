//! gw-kiro —— Kiro provider 实现。
//!
//! Phase 0:仅 stub,证明 [`gw_core::Provider`] 可被实现、可装进 registry。
//! 真实能力(wire/converter/cache_sim/stream/token/machine_id)在 P1+ 按
//! `docs/IMPROVEMENTS.md` 逐块搬运/借鉴。
//!
//! ## 已搬运资产(🔵=原样搬旧 kiro.rs,不重写)
//! - [`parser`]:AWS Event Stream 帧/header/CRC/decoder 状态机
//!   (来自旧 `src/kiro/parser/*`,抗分片/半包/坏包)。
//! - [`anthropic_types`]:Anthropic Messages 输入类型(旧 `src/anthropic/types.rs`)。
//! - [`kiro_types`]:Kiro 请求输出类型(旧 `src/kiro/model/requests/*`)。

pub mod anthropic_types;
pub mod billing;
pub mod cache_sim;
pub mod chat;
pub mod converter;
mod endpoint;
pub use endpoint::{ENDPOINT_CHOICES, ENDPOINT_KEY};
pub mod error_map;
pub mod headers;
pub mod image;
pub mod inline_thinking;
pub mod kiro_types;
pub mod machine_id;
pub mod models_api;
pub mod parser;
pub mod redacted_probe;
pub mod poison_memo;
pub mod profiles;
pub mod resolver;
pub mod responses;
pub mod signature;
pub mod stream;
pub mod text_tokens;
pub mod thinking_policy;
pub mod tool_repair;
pub mod import;
pub mod token;
pub mod usage;
pub mod usage_limits;
pub mod wire_profile;

use std::sync::Arc;

use async_trait::async_trait;
use gw_core::account::{Account, FieldSpec, FieldType};
use gw_core::error::UpstreamError;
use gw_core::model::{MachineIdentity, ModelInfo};
use gw_core::provider::{AccountQuota, CallCtx, ChatRequest, ChatStream, Provider};

pub use billing::CachePolicy;

/// 账号字段 schema(驱动 admin 表单 + accounts.yaml 校验)。
///
/// 字段全集对齐 static_flow `KiroAuthRecord`,覆盖 Social / IdC / API Key 三种凭据。
/// **防封关键**:`machine_id` —— Social 号(KiroManager 导出)必须带原始真机指纹,
/// 否则按 `sha256("KotlinNativeAPI/"+refresh_token)` 派生,会随 rolling token 漂移触发风控。
const KIRO_ACCOUNT_SCHEMA: &[FieldSpec] = &[
    FieldSpec::new("account_id", "账号 ID", FieldType::String, true),
    FieldSpec::new("refresh_token", "Refresh Token", FieldType::Password, false)
        .with_help("social/IdC 凭据必填;若用官方 API Key(kiro_api_key)则留空"),
    FieldSpec::new("kiro_api_key", "Kiro API Key", FieldType::Password, false)
        .with_help("官方 Kiro API Key(ksk_ 开头,app.kiro.dev 生成)。填此则走 API_KEY 鉴权:无需 refresh_token / profileArn / 刷新,长期有效"),
    FieldSpec::new("access_token", "Access Token", FieldType::Password, false),
    FieldSpec::new("profile_arn", "Profile ARN", FieldType::String, false),
    FieldSpec::new("region", "区域", FieldType::String, false)
        .with_help("AWS 区域,默认 us-east-1;同时兜底 api/auth 区域"),
    FieldSpec::new("machine_id", "设备指纹 (machineId)", FieldType::String, false)
        .with_help("Social 号防封关键:填原始真机 machineId(64 hex 或 UUID);留空则按 refresh_token 派生,会随 token 刷新漂移"),
    FieldSpec::new("auth_method", "认证方式", FieldType::String, false)
        .with_help("social / idc / external_idp;留空按 client_id+secret 自动判定 social/idc —— external_idp(Azure AD 企业 SSO)必须显式填写,无法从字段存在与否自动推断"),
    FieldSpec::new("kiro_provider", "身份来源", FieldType::String, false)
        .with_help("github / google / builderid / enterprise;用于缺失 profileArn 时取固定兜底"),
    FieldSpec::new("client_id", "IdC / External IdP Client ID", FieldType::String, false)
        .with_help("IdC(企业 SSO)或 external_idp(Azure AD 租户)账号必填"),
    FieldSpec::new("client_secret", "IdC Client Secret", FieldType::Password, false)
        .with_help("IdC 账号必填;走 AWS OIDC 刷新,不易封。external_idp 账号留空(Azure AD 公开客户端刷新不用 secret)"),
    FieldSpec::new("token_endpoint", "External IdP Token Endpoint", FieldType::String, false)
        .with_help("auth_method=external_idp(Azure AD 企业 SSO)账号的租户刷新端点,如 https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token;从 Kiro-Go 等工具导出时会按 userId 自动派生,一般无需手填"),
    FieldSpec::new("scope", "External IdP Scope", FieldType::String, false)
        .with_help("external_idp 账号的 OAuth2 scope(含 offline_access 才能拿到 refresh_token);留空按 client_id 自动派生 codewhisperer 默认 scope"),
    FieldSpec::new("kiro_version", "客户端版本", FieldType::String, false)
        .with_help("⚠️ UA 里的 KiroIDE 版本,默认 0.12.155;改它而不同步 OS/Node 版本会造成现实不存在的指纹组合,非必要勿动"),
    FieldSpec::new("proxy", "出口代理 URL (可选)", FieldType::String, false)
        .with_help("该账号专用出口代理,如 socks5://user:pass@host:port 或 http://host:port;该号 refresh/配额/发包全程走它。留空则用全局默认代理,再留空则用 worker 绑定源 IP"),
];

/// Kiro provider。持有按账号选出口的 [`resolver::EgressResolver`](每账号代理);
/// 计费/图像参数用 `RwLock` 承接热调(admin 设置面板 30s 生效)。
pub struct KiroProvider {
    family: &'static str,
    /// 出口 client 解析器:按账号(专属代理 → 默认代理 → worker 源 IP)选 client。
    /// 同一账号 refresh/quota/profile/chat 全程用 `resolver.client_for(account)`,同出口。
    resolver: Arc<resolver::EgressResolver>,
    /// 缓存计费参数(system.yaml + 热调;见 [`CachePolicy`])。
    cache_policy: parking_lot::RwLock<CachePolicy>,
    /// 图像压缩参数(system.yaml `image` 段 + 热调;chat 前对 body 内 base64 图瘦身 + OOM 护栏)。
    image_cfg: parking_lot::RwLock<gw_core::config::ImageConfig>,
    /// 出站请求体体积上限(字节)。序列化后的 KiroRequest 超此值时,先从 history
    /// 剔最老媒体瘦身;仍超限则本地 BadRequest(不发上游)。🔵 搬运自 kiro.rs v63;
    /// 上限值 2026-09-29 重新定标,见 [`DEFAULT_MAX_BODY_BYTES`]。
    ///
    /// 目前固定为 [`DEFAULT_MAX_BODY_BYTES`]:不经 admin 热调(caio 的 SystemConfig 分节
    /// 结构暂无此项干净归属;接入 admin 面板时再一并加 SystemSettings 字段与归属节)。
    /// 测试用 [`with_max_body_bytes`](Self::with_max_body_bytes) 注入小值验证护栏。
    max_body_bytes: usize,
}

/// 出站请求体体积上限默认值(字节)。
///
/// 【重新定标 2026-09-29,claude-opus-5.5,绕开本闸直打上游】全图片请求体 31,454,526 字节
/// 成功、32,630,000 字节起确定性 400 `CONTENT_LENGTH_EXCEEDS_THRESHOLD` —— 上游真实总上限
/// 约 32MB。旧值 6,300,000(2026-06 kiro.rs 定标)已过时:当年 7.3MB 那批 400 的会话带着
/// PDF,更可能是撞了**单文档 4.5MB** 上限而非总量(见 converter::media_limits)。
///
/// 取 24,000,000 = 实测上限的 ~75%,**刻意不贴着上限**:上游阈值可能按内容长度另算
/// (非纯 body 字节),且随上游版本漂移;留 ~8MB 余量换稳定。
pub const DEFAULT_MAX_BODY_BYTES: usize = 24_000_000;

/// 账号级开关(`extra.allow_paid_models`,JSON 布尔 true 才生效):让这个号跳过
/// `account_supports_model` 第③条的 FREE 档过滤(opus / `pro_only` 模型)。
///
/// 为什么不能改标题:`subscription_title` 每轮配额刷新都会被上游 getUsageLimits 的值回写,
/// 手改撑不过一轮。为什么不复用白名单:线上 35 个 FREE 号设了 `["claude*","kimi-k3"]`,
/// 让白名单抬 FREE 会把它们全放进 opus。
/// 2026-11-04 起用于两个标题 FREE、带 500 赠送积分(limit=550)、实测能跑 opus-5.5 的新号;
/// 运维用 `PATCH /admin/api/accounts/{id}` + `extra_merge: true` 热写,无需重启。
pub const ALLOW_PAID_MODELS_KEY: &str = "allow_paid_models";

impl KiroProvider {
    /// 用 worker 基础 egress client 构造(无默认代理)。测试与简单注入用。
    pub fn new(egress_client: reqwest::Client) -> Self {
        Self {
            family: "kiro",
            resolver: resolver::EgressResolver::new(egress_client, None),
            cache_policy: parking_lot::RwLock::new(CachePolicy::default()),
            image_cfg: parking_lot::RwLock::new(gw_core::config::ImageConfig::default()),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }

    /// 显式指定缓存计费参数(测试 / 配置注入用)。
    pub fn with_cache_policy(self, policy: CachePolicy) -> Self {
        *self.cache_policy.write() = policy;
        self
    }

    /// 显式指定出站体积上限(测试用:注入小值即可在不构造 MB 级 body 下验证护栏)。
    pub fn with_max_body_bytes(mut self, max_body_bytes: usize) -> Self {
        self.max_body_bytes = max_body_bytes;
        self
    }

    /// 显式指定图像压缩参数(测试 / 配置注入用)。
    pub fn with_image_config(self, cfg: gw_core::config::ImageConfig) -> Self {
        *self.image_cfg.write() = cfg;
        self
    }

    /// registry 工厂:接收 worker 的 egress client(固定源 IP),注入 provider。
    /// `cfg` 携带 `system.cache` 段(read_multiplier/cap_ratio/floor_ratio)+
    /// 可选 `image` 子对象 + 可选 `default_proxy`(全局默认出口代理;缺失/解析失败
    /// 回退默认,容错优先)。
    pub fn from_config(
        cfg: &serde_json::Value,
        egress_client: reqwest::Client,
    ) -> anyhow::Result<Arc<dyn Provider>> {
        let image_cfg = cfg
            .get("image")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let default_proxy = cfg
            .get("default_proxy")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(Arc::new(Self {
            family: "kiro",
            resolver: resolver::EgressResolver::new(egress_client, default_proxy),
            cache_policy: parking_lot::RwLock::new(CachePolicy::from_cfg(cfg)),
            image_cfg: parking_lot::RwLock::new(image_cfg),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }))
    }
}

impl KiroProvider {
    /// 该账号的设备指纹(machineId/UA)—— **Kiro 专属**,不在通用 Provider trait 内
    /// (审查 H5b:subprocess/dario 无 machineId 概念)。发包指纹的单一事实来源。
    ///
    /// machineId 派生见 [`machine_id::generate_from_account`](crate::machine_id)
    /// (🔵 照搬旧公式:social/OAuth = sha256("KotlinNativeAPI/"+refreshToken))。
    /// UA 把 machineId 嵌在末尾:`...KiroIDE-{version}-{machineId}`(封号根因点)。
    pub fn machine_identity(&self, account: &Account) -> MachineIdentity {
        let machine_id = machine_id::generate_from_account(account);
        // 客户端版本:账号可显式覆盖,否则用默认。
        let client_version = headers::kiro_version(account);
        // UA 与实际发包(headers 模块)同源,避免版本号漂移(此前写死 1.0.0 是陷阱)。
        let (x_amz_user_agent, user_agent) =
            headers::streaming_user_agents(&client_version, &machine_id);
        MachineIdentity {
            user_agent,
            x_amz_user_agent,
            machine_id,
            client_version,
        }
    }
}

#[async_trait]
impl Provider for KiroProvider {
    fn family(&self) -> &'static str {
        self.family
    }

    /// suspend 生命周期只对 kiro 启用:0/35 不可恢复的实测数据只覆盖 Kiro 号。
    fn suspend_lifecycle(&self) -> bool {
        true
    }

    /// 代价指数的计费模型(冷热差价、429 规律)只在 Kiro 上标定过。
    fn cost_index_capable(&self) -> bool {
        true
    }

    fn enterprise_priced(&self, account: &Account) -> bool {
        headers::is_enterprise_priced(account)
    }

    /// ksk_ API Key:长期有效、没有刷新概念。
    fn refresh_exempt(&self, account: &Account) -> bool {
        machine_id::is_api_key_credential(account)
    }

    fn region_from_profile_arn(&self, arn: &str) -> Option<&'static str> {
        import::region_from_profile_arn(arn)
    }

    /// 请求日志落「转换后发给 Kiro 的报文」,排查转换问题用。
    fn render_upstream_payload(&self, req: &ChatRequest, account: &Account) -> Option<String> {
        Some(chat::render_kiro_payload(req, account))
    }

    fn account_schema(&self) -> &'static [FieldSpec] {
        KIRO_ACCOUNT_SCHEMA
    }

    fn validate_account(&self, account: &Account) -> Result<(), UpstreamError> {
        // 两类凭据二选一:
        // - **API Key**(kiro_api_key 非空):走 `TokenType: API_KEY`,无需 refresh_token /
        //   profileArn / 刷新(chat 与配额均用 ksk_ 作 bearer)。
        // - **social / IdC**:需 refresh_token(运行时换取 access_token)。
        // 两者皆缺 → 该账号无任何可用凭据,加载即拒(避免"加载通过、首请求才炸"的边界破裂)。
        // 空 account_id:accounts 表的主键,正常不该为空(SQLite 允许空串主键,
        // 所以遗留/导入异常的号真能带着它进来)。它会让 conversationId 的账号盐落空
        // (见 `chat::account_scope`),而那条路径的失败是**静默**的。加载期就拒掉。
        if account.account_id.is_empty() {
            return Err(UpstreamError::bad_request(
                "Kiro 账号的 account_id 为空 —— 空主键的号会让 conversationId 的账号隔离失效",
            ));
        }
        let has_api_key = machine_id::is_api_key_credential(account);
        let has_refresh = account.extra_str("refresh_token").is_some_and(|s| !s.is_empty());
        if !has_api_key && !has_refresh {
            return Err(UpstreamError::bad_request(format!(
                "Kiro 账号 '{}' 缺少凭据(需 refresh_token[social/IdC] 或 kiro_api_key[官方 API Key])",
                account.account_id
            )));
        }
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError> {
        // 目录由 converter 权威表 `KIRO_MODELS` **生成**(`advertised_models`):每个基础模型
        // 展开 plain / 两种 Thinking 后缀 / 日期快照变体,与 chat 实际可服务(map_model)+
        // 身份规范化(requested_model_identity)同源、不漂移。context_length 由
        // get_context_window_size 统一推导。真实 ListAvailableModels(逐账号上游查询)留 P2。
        let models = converter::advertised_models()
            .into_iter()
            .map(|am| {
                let mut m = ModelInfo::new(am.id.clone());
                m.display_name = Some(am.display_name);
                m.context_length = Some(converter::get_context_window_size(&am.id).max(0) as u32);
                m.supports_thinking = am.supports_thinking;
                m.supports_tools = true;
                m.supports_vision = true;
                m
            })
            .collect();
        Ok(models)
    }

    async fn chat(&self, mut req: ChatRequest, ctx: &CallCtx) -> Result<ChatStream, UpstreamError> {
        // 报文环境标准化(按当次号的人设;开关见 gw_core::envnorm)。必须早于转换:
        // conversationId、缓存模拟器、发出去的字节都要基于同一份报文。
        gw_core::envnorm::apply_for(self.family, &mut req.body, &ctx.account);
        // 图像压缩(转换前):base64 大图按四档瘦身,解压炸弹在解码前被护栏拦截。
        // 失败回退原图,绝不阻断请求。ImageConfig 是 Copy,读锁后即拷出。
        let image_cfg = *self.image_cfg.read();
        image::compress_body_images(&mut req.body, &image_cfg).await;
        // 设备指纹(machineId 嵌 UA);出口 client 按账号解析(专属代理→默认代理→源IP)。
        let machine_id = self.machine_identity(&ctx.account).machine_id;
        let client = self.resolver.client_for(&ctx.account);
        let cache_policy = self.cache_policy.read().clone();
        chat::chat_stream(client, ctx.account.clone(), machine_id, req, cache_policy, self.max_body_bytes).await
    }

    /// 会话亲和键 = 派生的 conversationId(与上游 prefix cache 的会话粒度同源)。
    /// worker 据此把同会话钉到组内同账号,最大化 Kiro 缓存命中(按账号隔离)。
    fn affinity_key(&self, req: &ChatRequest) -> Option<String> {
        converter::affinity_key_from_body(&req.body)
    }

    /// 调度钉号键(会话 + 线程),见 [`converter::thread_key_from_body`]。
    fn thread_key(&self, req: &ChatRequest) -> Option<String> {
        converter::thread_key_from_body(&req.body)
    }

    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
        // API Key 凭据:长期有效、无 OIDC 刷新概念,原样返回(ksk_ 就是 bearer)。
        // 常态下不会走到这里(has_fresh_token 对 apikey 恒真);仅当 chat 收 403 走同号
        // 刷新兜底时会调用——空操作让 retry 用同 key 再试一次,失败即由调用方换号/上报。
        if machine_id::is_api_key_credential(account) {
            return Ok(account.clone());
        }
        // 用该账号的出口 client 刷新(social/IdC 自动分流),把新 token 写回 Account.extra 返回。
        // 持久化由 gw-app 负责(契约 H4)。出口与发包同源(防封):同一账号 refresh/chat 同代理。
        let client = self.resolver.client_for(account);
        let refreshed = token::refresh_auth(&client, account).await?;
        let mut updated = account.clone();
        // **不冻结 machineId**(对齐 kiro.rs `generate_from_credentials` + 真实 Kiro 客户端):
        // 无显式 machine_id 的号,每次按**当前** refresh_token 派生 sha256("KotlinNativeAPI/"+rt)。
        // 真实客户端正是这么算的——rt 滚动时 machineId 随之滚动、始终与上游对得上。
        // 早先的"冻结"(钉死首个 rt 的派生值)反而会在 rt 滚动后发出**陈旧** machineId
        // (真实客户端不会发的值)→ 在风控看来像换了设备 → 封号(mrdev3258 即此:被我
        // 反复刷新滚了 rt,却仍发陈旧冻结值)。kiro.rs 从不冻结、长期不封,故对齐之。
        // 有**真机** machine_id 的号(import 带入):generate_from_account 仍优先用显式值,不受影响。
        updated.extra.insert(
            "access_token".into(),
            serde_json::Value::String(refreshed.access_token),
        );
        if let Some(rt) = refreshed.refresh_token {
            updated
                .extra
                .insert("refresh_token".into(), serde_json::Value::String(rt));
        }
        if let Some(arn) = refreshed.profile_arn {
            updated
                .extra
                .insert("profile_arn".into(), serde_json::Value::String(arn));
        }
        if let Some(exp) = refreshed.expires_at {
            updated
                .extra
                .insert("expires_at".into(), serde_json::Value::String(exp));
        }
        Ok(updated)
    }

    /// 查询账号配额(只读 getUsageLimits)。account 须已带有效 access_token
    /// (worker 调用前已 ensure_credentialed)。用同一冻结 machineId,设备指纹与发包一致。
    async fn account_quota(
        &self,
        account: &Account,
    ) -> Result<Option<AccountQuota>, UpstreamError> {
        let client = self.resolver.client_for(account);
        usage_limits::get_account_quota(&client, account)
            .await
            .map(Some)
    }

    /// 拉取模型目录(`ListAvailableModels`,只读控制面)。用于取 `rateMultiplier`
    /// 定价与逐模型的 thinking 档位表。见 [`models_api`]。
    async fn model_catalog(
        &self,
        account: &Account,
    ) -> Result<Option<serde_json::Value>, UpstreamError> {
        let client = self.resolver.client_for(account);
        let catalog = models_api::list_available_models(&client, account).await?;
        serde_json::to_value(&catalog)
            .map(Some)
            .map_err(|e| UpstreamError::network(format!("模型目录序列化失败: {e}")))
    }

    /// 发现 profileArn(`ListAvailableProfiles`):企业/IdC 号 chat 与配额都要求
    /// profileArn,但凭据常不带;此处运行时向后端查询。account 已有显式值或可固定
    /// 兜底(social/builderid)时返回 None(不发请求)。见 [`profiles`]。
    async fn discover_profile_arn(
        &self,
        account: &Account,
    ) -> Result<Option<String>, UpstreamError> {
        // API Key 凭据不需要 profileArn(TokenType: API_KEY 让服务端按 key 自身账号解析),
        // 跳过 ListAvailableProfiles 网络调用。
        if machine_id::is_api_key_credential(account) {
            return Ok(None);
        }
        let client = self.resolver.client_for(account);
        profiles::discover_profile_arn(&client, account).await
    }

    /// 强制发现 profileArn(绕过固定兜底短路):付费 builderid 号被免费层共享 ARN 短路时,
    /// gw-app 在配额 403 兜底里调本方法查真实 profileArn。见 [`profiles::force_discover_profile_arn`]。
    async fn force_discover_profile_arn(
        &self,
        account: &Account,
    ) -> Result<Option<String>, UpstreamError> {
        let client = self.resolver.client_for(account);
        profiles::force_discover_profile_arn(&client, account).await
    }

    /// 见 trait 文档:本 provider 确实覆盖了 [`Self::apply_hot_settings`],
    /// 所以 `/health` 回显的 provider 级设置对它是**可信的实然值**。
    fn hot_settings_supported(&self) -> bool {
        true
    }

    /// `/health` 回显**实际生效**的热追加模型(不是设置里写了什么,而是校验通过、已换上的那份)。
    fn settings_fingerprints(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert("kiro_extra_models_active".into(), serde_json::json!(converter::active_extra_ids()));
        m
    }

    /// 热应用设置(worker 30s 轮询):更新默认代理 + 缓存计费 + 图像压缩参数。
    /// 仅覆盖 JSON 中出现的字段(部分更新);无副作用、线程安全(内部 RwLock)。
    fn apply_hot_settings(&self, settings: &serde_json::Value) -> Result<(), String> {
        // 默认代理(空串/缺失 → None,resolver 内部再归一)。
        let default_proxy = settings
            .get("default_proxy")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        self.resolver.update_default_proxy(default_proxy);

        // 缓存计费(present 字段才覆盖;热调键带 `cache_` 前缀,非法值 warn 后保留旧值)。
        self.cache_policy.write().overlay(settings, "cache_");

        // 热追加模型(present 才处理;from_effective 总会带,空数组 = 清空)。整组校验,
        // 非法整组拒收并保留旧表;与上次收到的相同则不动(不重复泄漏、不刷屏)。
        //  拒收时本函数返回 Err,worker 把原因并进设置同步错误、面板上说出来。
        let mut extra_models_error: Option<String> = None;
        if let Some(v) = settings.get("kiro_extra_models") {
            match serde_json::from_value::<Vec<gw_core::config::KiroExtraModelSpec>>(v.clone()) {
                Ok(specs) => match converter::set_extra_models(&specs) {
                    converter::ApplyOutcome::Applied => {
                        tracing::info!(models = ?converter::active_extra_ids(), "kiro_extra_models 已生效")
                    }
                    converter::ApplyOutcome::Unchanged => {}
                    converter::ApplyOutcome::Rejected { reason, fresh } => {
                        if fresh {
                            tracing::warn!("kiro_extra_models 不合法,整组拒收、沿用上一份: {reason}");
                        }
                        extra_models_error = Some(format!("kiro_extra_models 未生效: {reason}"));
                    }
                },
                Err(e) => extra_models_error = Some(format!("kiro_extra_models 形态不对,未生效: {e}")),
            }
        }

        // 图像压缩(present 字段才覆盖)。
        {
            let mut ic = self.image_cfg.write();
            if let Some(v) = settings.get("image_enabled").and_then(|v| v.as_bool()) {
                ic.enabled = v;
            }
            if let Some(v) = settings.get("image_max_long_edge").and_then(|v| v.as_u64()) {
                ic.max_long_edge = v as u32;
            }
            if let Some(v) = settings.get("image_max_pixels_single").and_then(|v| v.as_u64()) {
                ic.max_pixels_single = v as u32;
            }
            if let Some(v) = settings.get("image_max_pixels_multi").and_then(|v| v.as_u64()) {
                ic.max_pixels_multi = v as u32;
            }
            if let Some(v) = settings.get("image_multi_threshold").and_then(|v| v.as_u64()) {
                ic.multi_threshold = v as usize;
            }
        }

        // 实验开关(tools_in_prefix / cache_point / agent_continuation)。from_effective 总会带这些
        // 字段;缺失时退回 false(不覆盖到危险开启)。改写进程级实验全局,converter 下轮即读到。
        {
            let tools_in_prefix = settings
                .get("tools_in_prefix")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let cache_point = settings
                .get("cache_point")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let agent_continuation = settings
                .get("agent_continuation")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // thinking_signature 缺省 **true**(保留现状:带签名);只有显式 false 才关掉。
            // 与上面三个开关相反——它们危险默认关,这个安全默认开。
            let thinking_signature = settings
                .get("thinking_signature")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            // q_endpoint 与前三个开关同款:危险默认关(缺失→false=现状 runtime.kiro.dev),
            // 显式 true 才切旧 q.amazonaws.com 端点。
            let q_endpoint = settings
                .get("q_endpoint")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            crate::converter::set_experimental_flags(
                tools_in_prefix,
                cache_point,
                agent_continuation,
                thinking_signature,
                q_endpoint,
            );
        }

        // 默认思考档位(客户端没指定 effort 时用哪档)。字段缺失 = 不动当前值,**不**回落编译期
        // 兜底:轮询响应偶发缺字段时不该把用户在面板上设的档位悄悄打回出厂值。
        // 非法值只告警不生效(手改 DB 可以绕过 admin 的校验,脏档位打到上游是 400)。
        if let Some(raw) = settings.get("default_thinking_effort").and_then(|v| v.as_str()) {
            match crate::anthropic_types::set_default_effort(raw) {
                Some(applied) => tracing::debug!(effort = %applied, "默认思考档位已热应用"),
                None => tracing::warn!(
                    raw = %raw,
                    valid = ?crate::anthropic_types::VALID_EFFORTS,
                    current = %crate::anthropic_types::default_effort(),
                    "settings 里的 default_thinking_effort 不是合法档位，已忽略"
                ),
            }
        }
        // 历史 thinking 保留轮数(0=全丢/默认;N>0=保留倒数最近 N 个 assistant 合并单元;
        // 负=全保留)。与 default_thinking_effort 同口径:字段缺失 = 不动当前值,
        // 非法类型(非整数)只告警不生效 —— 手改 DB 可以绕过 admin 的 PUT 校验。
        // ⚠️ 从 0 调到非 0(或反向)会改变历史字节:在途会话下一轮缓存全量 miss 一次。
        match settings.get("history_thinking_turns") {
            None => {}
            Some(v) => match v.as_i64() {
                Some(n) => {
                    crate::converter::set_history_thinking_turns(n);
                    tracing::debug!(turns = n, "历史 thinking 保留轮数已热应用");
                }
                None => tracing::warn!(
                    value = %v,
                    "settings 里的 history_thinking_turns 不是整数，已忽略"
                ),
            },
        }
    
        // Kiro-Go 风格的本地前缀缓存开关 / TTL / 每分片容量。
        // present 字段才覆盖(from_effective 总会带上)。
        if let Some(v) = settings.get("cache_enabled").and_then(|v| v.as_bool()) {
            cache_sim::set_enabled(v);
        }
        if let Some(v) = settings.get("cache_sim_ttl_secs").and_then(|v| v.as_u64()) {
            cache_sim::set_ttl_secs(v);
        }
        if let Some(v) = settings.get("cache_max_sessions").and_then(|v| v.as_u64()) {
            cache_sim::set_max_sessions(v as usize);
        }
        match extra_models_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// 订阅能力过滤(对齐 kiro.rs `supports_opus`,credentials.rs:256):
    /// opus 系仅非 FREE 订阅可用——不过滤的话 opus 请求落到 FREE 号,上游 403 会被
    /// 误判 TokenInvalid 而永久禁用健康号。`subscription_title` 缺失(未导入且未查过
    /// 配额)放行,首次配额查询回填该字段后收敛。非 opus 模型全部放行。
    fn account_supports_model(&self, account: &Account, model: &str) -> bool {
        // ① 账号级模型白名单(`extra.model_allowlist`,与 cursor 共用 gw_core 那个匹配器,
        //    不另写一套)。缺失/null = 不限,空表/类型错 = 全禁。
        //    2026-10-23 接入时全库 0 个 kiro 号设过这个键,所以这条对存量是零行为变化。
        if !allowlist_allows(account, model) {
            return false;
        }
        // ② 非 claude 模型必须**逐号报名**。
        //    ①对「没设白名单」返回 true(不限),而线上绝大多数号都没设 —— 只靠①等于全放行,
        //    于是 kimi-k3 会被派给 1400+ 个目录里根本没有它的号,每个号先吃一次 400
        //    INVALID_MODEL_ID 才被标记,而标记 TTL 只有 6h,到期整轮重来。
        //    所以这里把语义反过来:claude 是默认供应,非 claude 默认不供应。
        //    判据用上游 modelId 前缀而非权威表 —— 权威表里有没有这一行跟「这个号的上游目录里
        //    有没有」是两件事,用表判会让「表里加了模型」自动等于「所有号都能接」。
        if !is_default_available_model(model) && !model_explicitly_allowlisted(account, model) {
            return false;
        }
        // ③ FREE 档过滤。与①②是**与**的关系:白名单不能把 FREE 号抬进这些模型。
        //    - opus:既有子串规则(对齐 kiro.rs);
        //    - 模型表标了 `pro_only` 的(haiku-5.5 起,热追加行默认也是):FREE 号的上游目录里
        //      没有它们,派过去只会吃 400 INVALID_MODEL_ID 再换号。
        //    - 单号开关 `allow_paid_models: true` 跳过本条(见 [`ALLOW_PAID_MODELS_KEY`])。
        if !model.to_ascii_lowercase().contains("opus") && !crate::converter::denies_free(model) {
            return true;
        }
        if matches!(account.extra.get(ALLOW_PAID_MODELS_KEY), Some(serde_json::Value::Bool(true))) {
            return true;
        }
        match account.extra_str("subscription_title") {
            Some(title) => !title.to_uppercase().contains("FREE"),
            None => true,
        }
    }
}

/// 「默认供应」的模型集 = Anthropic 系。caio 的客户买的就是 claude,全池 100% 流量是
/// claude,所有档位的 Kiro 号(FREE 档的 opus 限制另算)目录里都有它们。
///
/// 判据落在**这次请求最终会向上游要哪个模型**(`map_model` 的结果)上,而不是请求名的
/// 字面前缀。差别不是学术的:`anthropic/claude-3.5-sonnet-4.6`、
/// `openrouter/claude-opus-5-preview` 这类带前缀的异名走的是 `map_model` 的子串兜底,
/// 今天能正常路由;按字面前缀判会把它们判成非 claude → 要求显式报名 → 全池无号支持
/// → NoModelSupport,把一类今天正常的请求打死。
///
/// 也**不是**「查权威表里有没有这一行」:那会让「表里加了模型」自动等于「所有号都能接」,
/// 正是 `account_supports_model` 第②条要防的事。路由不到任何上游模型的名字返回 false,
/// 它本来也发不出去(`map_model` 为 None 时请求构造不出来)。
fn is_default_available_model(model: &str) -> bool {
    // 零分配:本函数在调度热路径上,一次选号要对全组(kiro 组 1400+ 个号)求值,
    // 而 gw_core 的准入谓词契约是锁内零分配。见 converter::routes_to_anthropic。
    crate::converter::routes_to_anthropic(model)
}

/// 白名单匹配:对**请求名**和它归一后的**基础名**各试一次,任一命中即放行。
///
/// 为什么要归一:调度器传进来的是客户端请求名(可能带 Thinking 后缀或日期快照),
/// 而运维在面板里写白名单时写的是基础名(`kimi-k3`)。只按请求名匹配的话,
/// `["kimi-k3"]` 挡不住……准确说是挡住了 `kimi-k3-thinking`,而那本来就是同一个上游模型。
/// 两种形态都试,既不用逼运维写 `kimi-k3*`,也不改 gw_core 那个共用匹配器的语义。
fn allowlist_allows(account: &Account, model: &str) -> bool {
    if gw_core::account::model_allowlist_allows(account, model) {
        return true;
    }
    let Some(base) = crate::converter::resolve_base(model) else {
        return false;
    };
    // 三种写法都认,取第一个命中:
    //   请求名(上面已试)、对外基础名(`claude-opus-4-8`)、上游真名(`claude-opus-4.8`)。
    // 对外基础名:调度器传进来的是客户端请求名(可能带 Thinking 后缀/日期),而运维写的是
    //   基础名,只按请求名匹配会把 `["kimi-k3"]` 和 `kimi-k3-thinking` 判成不匹配。
    // 上游真名:gw_core 的契约是「调用方传上游名」(cursor 就是这么做的),而运维很可能
    //   直接从「拉上游目录」的输出里照抄带点号的名字。不认它是 fail-closed —— 不会越权,
    //   但会把号意外摘出轮转,排查起来很费劲。(对抗评审 M4)
    [base.advertised_id, base.kiro_model]
        .iter()
        .filter(|name| **name != model)
        .any(|name| gw_core::account::model_allowlist_allows(account, name))
}

/// 白名单是否**显式**列了该模型。与 [`allowlist_allows`] 的差别只有一处:
/// 后者对「不限」返回 true,这里返回 false(没报名)。
///
/// ⚠️ **判「有没有这个键」是不够的,必须排除 `null`。** 面板清空白名单时后端写的是
/// `{"model_allowlist": null}` 而**不是**删键(`admin/accounts.rs` 的定点合并是读-合-写,
/// 写 null 不删键,那里有测试钉死这个行为),而 `gw_core` 把 null 与缺失同义为「不限」。
/// 只判 `contains_key` 的话,**任何一个曾经设过白名单又清空的号**都会被当成「报了名」,
/// 于是非 claude 模型派给它 → 400 INVALID_MODEL_ID,正是第②条要防的那场风暴,
/// 规模等于被清空过的号数。(对抗评审 H1)
fn model_explicitly_allowlisted(account: &Account, model: &str) -> bool {
    matches!(
        account.extra.get(gw_core::account::MODEL_ALLOWLIST_KEY),
        Some(v) if !v.is_null()
    ) && allowlist_allows(account, model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    fn account_with(extra: &[(&str, &str)]) -> Arc<Account> {
        let mut map = BTreeMap::new();
        for (k, v) in extra {
            map.insert((*k).to_string(), serde_json::Value::String((*v).to_string()));
        }
        Arc::new(Account {
            account_id: "k-test".into(),
            provider: "kiro".into(),
            max_concurrency: 1,
            disabled: false,
            created_at: 0,
            extra: map,
        })
    }

    #[tokio::test]
    async fn list_models_returns_known_models() {
        let p = KiroProvider::new(client());
        let models = p.list_models().await.unwrap();
        // 目录不再是 3 个 stub:opus-4-6/4-7 与 sonnet-4-6 必须公告(漏报会让客户端误判不支持)。
        for id in [
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
        ] {
            let m = models
                .iter()
                .find(|m| m.id == id)
                .unwrap_or_else(|| panic!("目录缺少 {id}"));
            assert!(m.supports_tools && m.supports_vision, "{id} 工具/视觉能力缺失");
            assert!(m.context_length.unwrap_or(0) >= 200_000, "{id} 上下文窗口异常");
            assert!(m.display_name.is_some(), "{id} 缺展示名");
        }
        // opus-4.6+ / sonnet-4.6 是 1M 窗口,目录应如实反映。
        let opus48 = models.iter().find(|m| m.id == "claude-opus-4-8").unwrap();
        assert_eq!(opus48.context_length, Some(1_000_000));
        let sonnet45 = models.iter().find(|m| m.id == "claude-sonnet-4-5").unwrap();
        assert_eq!(sonnet45.context_length, Some(200_000));
        // 2026-10-03 上游新增 sonnet-5.5:公告、1M 窗口、thinking 变体齐全。
        for id in ["claude-sonnet-5-5", "claude-sonnet-5-5-thinking"] {
            let m = models
                .iter()
                .find(|m| m.id == id)
                .unwrap_or_else(|| panic!("目录缺少 {id}"));
            assert_eq!(m.context_length, Some(1_000_000), "{id}");
            assert!(m.supports_tools && m.supports_vision, "{id} 工具/视觉能力缺失");
        }
        // 新:thinking 变体与日期快照名也要公告(NewAPI 渠道按这些名拉取)。
        for id in [
            "claude-opus-4-8-thinking",
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-5-20250929-thinking",
            "claude-haiku-4-5-20251001",
        ] {
            let m = models
                .iter()
                .find(|m| m.id == id)
                .unwrap_or_else(|| panic!("目录缺少 {id}"));
            assert!(m.context_length.unwrap_or(0) >= 200_000, "{id} 窗口异常");
        }
        // 日期快照窗口对齐其基础模型(sonnet-4.5=200k)。
        let dated = models.iter().find(|m| m.id == "claude-sonnet-4-5-20250929").unwrap();
        assert_eq!(dated.context_length, Some(200_000));
        // -thinking 变体 supports_thinking 恒 true。
        let opus48t = models.iter().find(|m| m.id == "claude-opus-4-8-thinking").unwrap();
        assert!(opus48t.supports_thinking);
    }

    #[test]
    fn from_config_reads_default_proxy_and_apply_hot_settings_updates() {
        // from_config 读 default_proxy → resolver 据此给无账号代理的号建代理 client。
        let cfg = serde_json::json!({"default_proxy": "socks5://cfg:1080"});
        let p = KiroProvider::from_config(&cfg, client()).unwrap();
        // 通过 chat 之外的公开行为不易直接断言 resolver 内部;改测 apply_hot_settings 热更:
        // 把默认代理换成新值,再换空 → 行为通过不 panic 验证(resolver 单测已覆盖解析)。
        p.apply_hot_settings(&serde_json::json!({
            "default_proxy": "http://hot:8888",
            "cache_read_multiplier": 3.0,
            "image_enabled": false
        })).unwrap();
        p.apply_hot_settings(&serde_json::json!({"default_proxy": ""})).unwrap();
        // 不 panic 即通过;字段级更新的正确性由 resolver/config 单测保证。
    }

    #[test]
    fn account_schema_includes_proxy_field() {
        let p = KiroProvider::new(client());
        assert!(
            p.account_schema().iter().any(|f| f.name == "proxy"),
            "schema 应含 proxy 字段(前端表单据此渲染)"
        );
    }

    #[test]
    fn machine_identity_valid_from_refresh_token() {
        let p = KiroProvider::new(client());
        let id = p.machine_identity(&account_with(&[("refresh_token", "rt_xyz")]));
        assert!(id.is_valid_machine_id());
        assert!(id.user_agent.contains(&id.machine_id));
        assert!(id.user_agent.contains("KiroIDE-"));
    }

    #[test]
    fn cache_policy_from_cfg_parses_and_falls_back() {
        // 完整 cfg → 全部采用。
        let full = serde_json::json!({"read_multiplier": 2.5, "cap_ratio": 0.8, "floor_ratio": 0.2});
        let b = CachePolicy::from_cfg(&full);
        assert_eq!(b.read_multiplier, 2.5);
        assert_eq!(b.cap_ratio, 0.8);
        assert_eq!(b.floor_ratio, 0.2);
        // 部分缺失 → 缺的回退默认,有的采用。
        let partial = serde_json::json!({"cap_ratio": 0.7});
        let b = CachePolicy::from_cfg(&partial);
        assert_eq!(b.cap_ratio, 0.7);
        assert_eq!(b.read_multiplier, CachePolicy::default().read_multiplier);
        // Null / 非对象 → 全默认,不 panic。
        let b = CachePolicy::from_cfg(&serde_json::Value::Null);
        assert_eq!(b, CachePolicy::default());
    }

    #[test]
    fn apply_hot_settings_updates_cache_policy() {
        let kp = KiroProvider::new(client());
        kp.apply_hot_settings(&serde_json::json!({"cache_floor_ratio": 0.5, "cache_cap_ratio": 0.95})).unwrap();
        let got = kp.cache_policy.read().clone();
        assert_eq!((got.cap_ratio, got.floor_ratio), (0.95, 0.5));
        // 缺席的字段保持不变。
        kp.apply_hot_settings(&serde_json::json!({"default_proxy": ""})).unwrap();
        assert_eq!(kp.cache_policy.read().floor_ratio, 0.5);
    }

    // ───────── 非 claude 模型的准入:必须逐号报名(2026-10-23) ─────────

    /// 只加模型不加准入,1469 个 kiro 号全都会成为 kimi-k3 的候选 → 每个号先吃一次
    /// 400 INVALID_MODEL_ID 才被标记,而标记 TTL 只有 6h,到期后整轮重来。
    /// 所以非 claude 模型**默认不派给任何号**,只派给白名单里显式列了它的号。
    #[test]
    fn non_claude_models_require_explicit_allowlist_optin() {
        let p = KiroProvider::new(client());
        // 没设白名单(线上 1469 个号的现状):claude 照旧放行,kimi-k3 拒。
        let plain = account_with(&[("subscription_title", "KIRO PRO MAX")]);
        assert!(p.account_supports_model(&plain, "claude-opus-5-5"));
        assert!(
            !p.account_supports_model(&plain, "kimi-k3"),
            "缺白名单 ≠ 放行:非 claude 必须显式报名"
        );
        assert!(!p.account_supports_model(&plain, "kimi-k3-thinking"));
    }

    /// **回归**:带前缀的 claude 异名(老客户端 / openrouter 风格)走的是 `map_model` 的
    /// 子串兜底,它们今天能正常路由。「非 claude 要显式报名」这条规则若按**请求名**的
    /// 字面前缀判,这类名字会被判成非 claude → 全池无号支持 → NoModelSupport,
    /// 把一类今天正常的请求打死。判据必须落在「路由到的上游模型是不是 claude」上。
    #[test]
    fn prefixed_claude_aliases_are_still_default_available() {
        let p = KiroProvider::new(client());
        let plain = account_with(&[("subscription_title", "KIRO PRO MAX")]);
        for alias in [
            "anthropic/claude-3.5-sonnet-4.6",
            "claude-opus-4.7-beta",
            "openrouter/claude-opus-5-preview",
            "CLAUDE-SONNET-5",
        ] {
            assert!(
                p.account_supports_model(&plain, alias),
                "{alias} 应按 claude 处理(它会被子串兜底路由到 claude 上游模型)"
            );
        }
        // 反面:路由不到任何上游模型的名字照旧拒(它本来也发不出去)。
        assert!(!p.account_supports_model(&plain, "gpt-4o"));
    }

    /// 报了名的号:kimi-k3 放行;白名单本身的排他语义照旧(没列 claude 就不给 claude)。
    #[test]
    fn allowlisted_account_serves_kimi_and_is_excluded_from_claude() {
        let p = KiroProvider::new(client());
        let kimi_only = Arc::new(Account {
            account_id: "k-kimi".into(),
            provider: "kiro".into(),
            max_concurrency: 1,
            disabled: false,
            created_at: 0,
            extra: BTreeMap::from([
                ("subscription_title".to_string(), serde_json::json!("KIRO PRO MAX")),
                ("model_allowlist".to_string(), serde_json::json!(["kimi-k3"])),
            ]),
        });
        assert!(p.account_supports_model(&kimi_only, "kimi-k3"));
        assert!(p.account_supports_model(&kimi_only, "kimi-k3-thinking"));
        assert!(!p.account_supports_model(&kimi_only, "claude-opus-5-5"));
        assert!(!p.account_supports_model(&kimi_only, "claude-sonnet-5"));
    }

    /// **H1 回归**:`model_allowlist: null` 不是「报名」。面板清空白名单写的就是 null
    /// 且**不删键**(`admin/accounts.rs` 有测试钉死),只判 contains_key 会让每一个
    /// 「设过又清空」的号永久变成非 claude 模型的候选。
    #[test]
    fn null_allowlist_is_not_an_optin() {
        let p = KiroProvider::new(client());
        let cleared = Arc::new(Account {
            account_id: "k-cleared".into(),
            provider: "kiro".into(),
            max_concurrency: 1,
            disabled: false,
            created_at: 0,
            extra: BTreeMap::from([
                ("subscription_title".to_string(), serde_json::json!("KIRO POWER")),
                ("model_allowlist".to_string(), serde_json::Value::Null),
            ]),
        });
        // null = 不限 → claude 照旧放行(与缺键同义,这条不能变)。
        assert!(p.account_supports_model(&cleared, "claude-opus-5-5"));
        // 但 null ≠ 报名 → 非 claude 仍然拒。
        assert!(
            !p.account_supports_model(&cleared, "kimi-k3"),
            "清空过白名单的号不得被当成报了名"
        );
    }

    /// 白名单只列 claude 的号:claude 放、kimi 拒(第 2 条不能被第 1 条的通过绕开)。
    #[test]
    fn claude_only_allowlist_still_blocks_kimi() {
        let p = KiroProvider::new(client());
        let claude_only = Arc::new(Account {
            account_id: "k-claude".into(),
            provider: "kiro".into(),
            max_concurrency: 1,
            disabled: false,
            created_at: 0,
            extra: BTreeMap::from([(
                "model_allowlist".to_string(),
                serde_json::json!(["claude-*"]),
            )]),
        });
        assert!(p.account_supports_model(&claude_only, "claude-opus-5-5"));
        assert!(!p.account_supports_model(&claude_only, "kimi-k3"));
    }

    /// **对抗评审 M4**:白名单的三种写法都要认 —— 请求名、对外基础名、上游真名。
    /// 运维很可能直接从「拉上游目录」的输出里照抄带点号的 `claude-opus-4.8`。
    #[test]
    fn allowlist_accepts_advertised_and_upstream_spellings() {
        let p = KiroProvider::new(client());
        let mk = |entry: serde_json::Value| {
            Arc::new(Account {
                account_id: "k-spell".into(),
                provider: "kiro".into(),
                max_concurrency: 1,
                disabled: false,
                created_at: 0,
                extra: BTreeMap::from([("model_allowlist".to_string(), entry)]),
            })
        };
        // 上游真名(带点号)
        let dotted = mk(serde_json::json!(["claude-opus-4.8"]));
        assert!(p.account_supports_model(&dotted, "claude-opus-4-8"));
        assert!(p.account_supports_model(&dotted, "claude-opus-4-8-thinking"));
        // 对外基础名(带连字符)
        let hyphen = mk(serde_json::json!(["claude-opus-4-8"]));
        assert!(p.account_supports_model(&hyphen, "claude-opus-4-8-thinking"));
        // 都不该放行别的模型
        assert!(!p.account_supports_model(&dotted, "claude-opus-5-5"));
        assert!(!p.account_supports_model(&hyphen, "kimi-k3"));
    }

    /// 空白名单 = 全禁(fail-closed,与 `gw_core::account::model_allowlist_allows` 同语义),
    /// 类型错(数字/对象)同样全禁 —— 手改配置填错不该变成「全放」。
    #[test]
    fn empty_or_malformed_allowlist_blocks_everything() {
        let p = KiroProvider::new(client());
        for bad in [serde_json::json!([]), serde_json::json!(1), serde_json::json!({"a": 1})] {
            let a = Arc::new(Account {
                account_id: "k-bad".into(),
                provider: "kiro".into(),
                max_concurrency: 1,
                disabled: false,
                created_at: 0,
                extra: BTreeMap::from([("model_allowlist".to_string(), bad.clone())]),
            });
            assert!(!p.account_supports_model(&a, "claude-opus-5-5"), "{bad}");
            assert!(!p.account_supports_model(&a, "kimi-k3"), "{bad}");
        }
    }

    /// opus/FREE 那条既有规则不受影响,且与白名单是**与**的关系:
    /// FREE 号即便把 opus 写进白名单也拿不到 opus。
    #[test]
    fn free_tier_opus_rule_survives_and_composes_with_allowlist() {
        let p = KiroProvider::new(client());
        let free_with_opus = Arc::new(Account {
            account_id: "k-free".into(),
            provider: "kiro".into(),
            max_concurrency: 1,
            disabled: false,
            created_at: 0,
            extra: BTreeMap::from([
                ("subscription_title".to_string(), serde_json::json!("KIRO FREE")),
                ("model_allowlist".to_string(), serde_json::json!(["claude-opus-5-5"])),
            ]),
        });
        assert!(
            !p.account_supports_model(&free_with_opus, "claude-opus-5-5"),
            "白名单不能把 FREE 号抬进 opus"
        );
    }

    #[test]
    fn account_supports_model_filters_free_for_opus() {
        let p = KiroProvider::new(client());
        let free = account_with(&[("subscription_title", "KIRO FREE")]);
        let pro = account_with(&[("subscription_title", "KIRO PRO")]);
        let unknown = account_with(&[]);
        // opus:FREE 拒、PRO 放、未知放行(待配额回填)。
        assert!(!p.account_supports_model(&free, "claude-opus-4-8"));
        assert!(p.account_supports_model(&pro, "claude-opus-4-8"));
        assert!(p.account_supports_model(&unknown, "claude-opus-4-8"));
        // 非 opus:FREE 也放。
        assert!(p.account_supports_model(&free, "claude-sonnet-4-5"));
        // 大小写不敏感。
        assert!(!p.account_supports_model(&free, "Claude-OPUS-4-8"));
    }

    /// haiku-5.5 只给付费档:FREE 号的上游目录里没有它(2026-11-03 实拉)。
    /// FREE 照旧能用 haiku-4.5 / sonnet-4.5,付费各档与「标题未知」放行。
    #[test]
    fn account_supports_model_keeps_haiku_5_5_off_free() {
        let p = KiroProvider::new(client());
        let free = account_with(&[("subscription_title", "KIRO FREE")]);
        for name in ["claude-haiku-5-5", "claude-haiku-5-5-thinking", "openrouter/claude-haiku-5.5"] {
            assert!(!p.account_supports_model(&free, name), "FREE 不该接 {name}");
        }
        assert!(p.account_supports_model(&free, "claude-haiku-4-5"));
        assert!(p.account_supports_model(&free, "claude-haiku-4-5-20251001"));
        assert!(p.account_supports_model(&free, "claude-sonnet-4-5"));
        for tier in ["KIRO PRO", "KIRO PRO+", "KIRO PRO MAX", "KIRO POWER", "KIRO STUDENT"] {
            let acc = account_with(&[("subscription_title", tier)]);
            assert!(p.account_supports_model(&acc, "claude-haiku-5-5"), "{tier} 应能接 haiku-5.5");
        }
        assert!(p.account_supports_model(&account_with(&[]), "claude-haiku-5-5"), "标题未知放行(待配额回填)");
    }

    /// 单号开关 `allow_paid_models: true`:上游标题仍是 FREE、但实际能用付费模型的号
    /// (2026-11-04:两个新号带 500 赠送积分,limit=550,实测跑通 opus-5.5)。
    /// 只认 JSON 布尔 true;没设/false/字符串都按原 FREE 规则。白名单仍然先判。
    #[test]
    fn allow_paid_models_flag_lifts_free_gate_for_that_account_only() {
        let p = KiroProvider::new(client());
        let mut flagged = (*account_with(&[("subscription_title", "KIRO FREE")])).clone();
        flagged.extra.insert(ALLOW_PAID_MODELS_KEY.into(), serde_json::Value::Bool(true));
        for name in ["claude-opus-5-5", "claude-opus-4-8", "claude-haiku-5-5"] {
            assert!(p.account_supports_model(&flagged, name), "带开关的 FREE 号应能接 {name}");
        }
        // 别的 FREE 号不受影响。
        let plain = account_with(&[("subscription_title", "KIRO FREE")]);
        assert!(!p.account_supports_model(&plain, "claude-opus-5-5"));
        // 只认布尔 true。
        let mut off = (*plain).clone();
        off.extra.insert(ALLOW_PAID_MODELS_KEY.into(), serde_json::Value::Bool(false));
        assert!(!p.account_supports_model(&off, "claude-opus-5-5"));
        let stringy = account_with(&[("subscription_title", "KIRO FREE"), (ALLOW_PAID_MODELS_KEY, "true")]);
        assert!(!p.account_supports_model(&stringy, "claude-opus-5-5"), "字符串 \"true\" 不算");
        // 与 `["claude*","kimi-k3"]` 白名单并存(35 个 FREE 号的现状)时,没开关仍拒 opus。
        let mut wl = (*plain).clone();
        wl.extra.insert(
            gw_core::account::MODEL_ALLOWLIST_KEY.into(),
            serde_json::json!(["claude*", "kimi-k3"]),
        );
        assert!(!p.account_supports_model(&wl, "claude-opus-5-5"));
        // 开关不越过白名单:白名单只写 kimi 时,带开关也不接 opus。
        let mut wl_only_kimi = flagged.clone();
        wl_only_kimi.extra.insert(gw_core::account::MODEL_ALLOWLIST_KEY.into(), serde_json::json!(["kimi-k3"]));
        assert!(!p.account_supports_model(&wl_only_kimi, "claude-opus-5-5"));
    }

    #[test]
    fn validate_account_requires_credential() {
        let p = KiroProvider::new(client());
        // 无 refresh_token / kiro_api_key → 拒绝
        assert!(p.validate_account(&account_with(&[])).is_err());
        // 有 refresh_token → 通过
        assert!(p.validate_account(&account_with(&[("refresh_token", "x")])).is_ok());
        // 有官方 API Key → 通过(无需 refresh_token)
        assert!(p
            .validate_account(&account_with(&[("kiro_api_key", "ksk_abc")]))
            .is_ok());
        // kiro_api_key 空串不算凭据 → 拒绝
        assert!(p.validate_account(&account_with(&[("kiro_api_key", "")])).is_err());
    }

    #[tokio::test]
    async fn refresh_auth_is_noop_for_api_key() {
        let p = KiroProvider::new(client());
        // apikey 号 refresh_auth 原样返回,绝不打 OIDC(无网络)。
        let acc = account_with(&[("kiro_api_key", "ksk_abc"), ("access_token", "ksk_abc")]);
        let out = p.refresh_auth(&acc).await.expect("apikey refresh 应空操作成功");
        assert_eq!(out.extra_str("kiro_api_key"), Some("ksk_abc"));
        assert_eq!(out.extra_str("access_token"), Some("ksk_abc"));
    }

    #[tokio::test]
    async fn discover_profile_arn_skipped_for_api_key() {
        let p = KiroProvider::new(client());
        let acc = account_with(&[("kiro_api_key", "ksk_abc")]);
        // 直接 None,不发 ListAvailableProfiles(无网络)。
        assert_eq!(p.discover_profile_arn(&acc).await.unwrap(), None);
    }

    #[test]
    fn api_key_power_account_supports_opus() {
        let p = KiroProvider::new(client());
        // 实测 ksk_ 号订阅 KIRO POWER,不含 FREE → 放行 opus。
        let power = account_with(&[
            ("kiro_api_key", "ksk_abc"),
            ("subscription_title", "KIRO POWER"),
        ]);
        assert!(p.account_supports_model(&power, "claude-opus-4-8"));
    }

    #[tokio::test]
    async fn chat_without_access_token_errors_token_invalid() {
        let p = KiroProvider::new(client());
        let req = ChatRequest::from_anthropic_body(serde_json::json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}]
        }));
        let ctx = CallCtx {
            account: account_with(&[("refresh_token", "rt")]), // 无 access_token
            session_id: "s".into(),
            cache_key: "c".into(),
        };
        let err = p.chat(req, &ctx).await.err().expect("缺 access_token 应报错");
        assert_eq!(err.kind, gw_core::error::UpstreamErrorKind::TokenInvalid);
    }
}
