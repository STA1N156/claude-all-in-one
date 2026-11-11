//! gw-cursor —— 把 Cursor 订阅(IDE 后端 ConnectRPC 协议)接成一个 Provider。
//!
//! 逆向自本机 Cursor **3.14.27**(2026-09-12 对本机 **3.18.25** 重抓复核,见 wire.rs):
//! 对 `agentn.global.api5.cursor.sh` 的
//! `agent.v1.AgentService/Run` 发 HTTP/2 + ConnectRPC(protobuf,逐帧 gzip),
//! 鉴权 = `Bearer <session JWT>` + `x-cursor-checksum`(zyg cipher + machineId)。
//! 完整规格见 `PROTOCOL-agent-run.md`。
//!
//! 内部 IR 是 Anthropic Messages(与 gw-kiro/gw-dario 一致):`chat()` 吃 Anthropic
//! 请求体,吐 Anthropic SSE。
//!
//! ## 两个域名,别搞混
//!
//! - `agentn.global.api5.cursor.sh` —— 推理(`AgentService/Run`),BiDi 流。
//!   (2026-09 起官方默认带 global 区域路由;旧的 `agentn.api5.cursor.sh` 仍兼容。)
//! - `api2.cursor.sh` —— unary 服务:`ServerConfigService/GetServerConfig`(取
//!   `config_version`)、OAuth `/oauth/token`(刷新)、以及
//!   `DashboardService/GetCurrentPeriodUsage`(官方账期用量,admin 配额列)。
//!
//! ## 防关联(PROTOCOL §7)
//!
//! 每号一套**冻结**的身份:token / machineId / macMachineId / session-id /
//! client-key / checksum / config-version,加**一个独立出口**。刷新与发包必须同出口 ——
//! 这条在本 crate 里由 [`CursorProvider::client_for`] 保证:`chat`、`GetServerConfig`、
//! `refresh_auth`、`account_quota` 四条路径都从它取 client。
//!
//! 已支持:多轮(历史折叠)、tool_use 往返(参数含数字/布尔/对象/数组)、
//! thinking 透传(含 Claude Code `adaptive`)、图像(内联)、PDF(我方抽文本层)、
//! 上游自报 usage、官方账期用量查询。
//!
//! 未做:FileSyncService blob 上传(L2 文件附件)、
//! Cursor 内建工具的代执行(**有意不做**:那等于跑模型选定的 shell 命令)。

pub mod auth;
pub mod cache_sim;
mod chat;
pub mod cli;
mod exec;
mod held;
mod interaction;
mod config;
pub mod login;
mod models;
mod pdf;
pub mod protobuf;
pub mod run;
mod thinking_echo;
mod blobhist;
/// grokbot 0.39 驱动(pool=bot 账号的推理面,见模块文档)。
pub mod usage;
pub mod wire;
/// wire v2 反应式帧序驱动(生产与探针共用,见模块文档)。
pub mod wirev2;

// 模型目录热追加(worker 30s 设置环回写;见 models::set_extra_models)。
/// 模型目录:帧0 的 `1.14` 用它(08-23 实物是 9 条带参条目,两种轮次都发)。
/// 导出是给外部 harness 用的 —— 探针必须发**与生产同一份**目录,
/// 否则测出来的形态不等于生产发出去的形态。
pub use models::catalog;
pub use models::set_extra_models;
// 纯 bot 池的对外目录裁剪(worker `/v1/models` 用;见 models::BOT_POOL_MODEL)。
pub use models::bot_pool_list;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use async_trait::async_trait;
use gw_core::account::{Account, FieldSpec, FieldType};
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::model::ModelInfo;
use gw_core::provider::{AccountQuota, CallCtx, ChatRequest, ChatStream, Provider};

mod tool_guard;
mod account_config;
mod conv_registry;
mod assets;
mod sections;
mod shadow;
mod notices;
mod cursor_provider;
mod provider;
pub use self::tool_guard::*;
pub use self::account_config::*;
pub(crate) use self::conv_registry::*;
pub(crate) use self::assets::*;
pub use self::sections::*;
pub(crate) use self::shadow::*;
pub use self::notices::*;
pub use self::provider::*;
pub(crate) use self::cursor_provider::{CachedClient, ConnHandle};

pub struct CursorProvider {
    cfg: CursorConfig,
    /// 见 [`RunTuning`]。默认 [`RunTuning::faithful`]。
    tuning: RunTuning,
    /// worker 注入的默认出口 client(账号没配 proxy 时用)。
    egress_client: reqwest::Client,
    /// (账号, proxy URL) → client 缓存:**每个号一个 client、一条连接**,出口是该号自己的代理。
    /// 条目数 = 配了代理的号数(十来个),`reqwest::Client` 内部是 Arc,clone 廉价。
    ///
    /// 2026-11-10 前按 proxy URL 缓存,同出口的号共用一条 HTTP/2 连接,一个号把连接堵住
    /// 就连累同出口所有号(见 `CursorProvider::client_for`)。
    ///
    /// 本仓库既有做法是每个 provider 自管出口解析,不共用(gw-kiro `EgressResolver`、
    /// gw-dario `proxy_clients`)。
    ///
    /// 每条带代际号、外层 `Arc`:流里出现连接卡死的停滞时,包装层(`provider::heal_stalled_connection`)
    /// 要在 `chat()` 返回之后回来把这个号的旧连接丢掉,见 [`ConnHandle`]。
    proxy_clients: Arc<Mutex<HashMap<String, CachedClient>>>,
    /// 缓存键 → 结果。见 [`config_cache_key`]:键含**身份指纹**,不只是 account_id。
    ///
    /// 后台把同一个 account_id 的 token / machine_id / proxy 换掉时,旧身份取回来的
    /// config_version 会继续被复用最长 30 分钟 —— 而那是一个服务端没见过的组合。
    /// 指纹进键之后,换凭据自动等于换缓存条目。
    config_cache: Mutex<HashMap<String, ConfigEntry>>,
    /// `GetServerConfig` 的 single-flight 闸门,**按缓存键分**。
    ///
    /// 一次取要 5–6 秒,而缓存冷启动时并发请求会各取一次,所以要单飞。
    /// 但**不能用一把全局锁**:那样 50 个各有独立代理的冷号会被串成 250–300 秒,
    /// 而它们本可以并行。更糟的是任一个号的代理卡住,整个 cursor 池一起排队。
    ///
    /// 用 async 锁而不是 std 锁:必须**跨 await 持有**才挡得住并发,而 std 锁跨 await
    /// 会把 future 变成 `!Send`(且长期持锁阻塞整个 runtime 线程)。
    config_gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// 服务端已建立的会话。见 [`ConvRegistry`]。
    conversations: Arc<ConvRegistry>,
    /// 服务端 write_args 推来的资产(带图请求的附件副本)。见 [`AssetStore`]。
    assets: Arc<AssetStore>,
    /// 「上一轮被内建工具截断」的待发纠偏标记(见 [`TruncationNotices`])。
    notices: Arc<TruncationNotices>,
    /// NoZDR 数据政策同意的进程内去重表(`{account_id}\0{model_id}`)。
    /// 见 [`usage::KNOWN_NOZDR_CONSENTS`];幂等,重启后每号至多重放一次。
    nozdr_consented: Mutex<std::collections::HashSet<String>>,
    /// 续轮描述符影子表(见 [`DescriptorShadow`])。
    shadow: Arc<DescriptorShadow>,
    /// 内容分节库(应答服务端 `4.2` 点名,见 [`ContentSections`])。
    sections: Arc<ContentSections>,
}

/// FNV-1a 64 内容指纹(与分叉 digest 同族常量):影子日志里做「距上一份是否变化」
/// 的前缀对比,不把整份字节打进日志。
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// 资产库:同会话写后可读、跨会话隔离、容量闸挡内存炸弹。
    #[test]
    fn 资产库存取与上限() {
        let s = AssetStore::default();
        assert!(s.put("c1", "/assets/a.png", b"123"));
        assert_eq!(s.get("c1", "/assets/a.png").as_deref(), Some(&b"123"[..]));
        // 跨会话隔离:同路径另一个会话取不到。
        assert!(s.get("c2", "/assets/a.png").is_none());
        // 覆盖同路径:总量按差值更新,不被旧值重复计数。
        assert!(s.put("c1", "/assets/a.png", b"12345"));
        assert_eq!(s.get("c1", "/assets/a.png").as_deref(), Some(&b"12345"[..]));
        // 单文件超限直接拒。
        assert!(!s.put("c1", "/assets/big.png", &vec![0u8; ASSET_FILE_CAP + 1]));
        assert!(s.get("c1", "/assets/big.png").is_none());
        // 单会话装满:淘汰本会话最早写入的文件,新文件存得进(生产:每轮重传附件换新路径)。
        assert!(s.put("c1", "/assets/f1.bin", &vec![0u8; ASSET_FILE_CAP]));
        assert!(s.put("c1", "/assets/f2.bin", &vec![0u8; ASSET_FILE_CAP]));
        assert!(s.get("c1", "/assets/f2.bin").is_some(), "新图必须存得进");
        assert!(s.get("c1", "/assets/a.png").is_none(), "最早的 a.png 被淘汰");
        assert!(s.get("c1", "/assets/f1.bin").is_some(), "f1 + f2 正好 24MB,f1 不必淘汰");
        // 先读一次 f1:它变成「最近用过」,再写入时被淘汰的应是 f2。
        assert!(s.get("c1", "/assets/f1.bin").is_some());
        assert!(s.put("c1", "/assets/f3.bin", &vec![0u8; 1]));
        assert!(s.get("c1", "/assets/f2.bin").is_none(), "f2 最久没碰,先被淘汰");
        assert!(s.get("c1", "/assets/f1.bin").is_some(), "刚读过的 f1 保留");
        // 没存过的路径永远取不到(调用方据此走收口)。
        assert!(s.get("c1", "/etc/passwd").is_none());
    }

    fn acct(pairs: &[(&str, &str)]) -> Account {
        let mut e = BTreeMap::new();
        for (k, v) in pairs {
            e.insert(k.to_string(), serde_json::json!(v));
        }
        Account {
            account_id: "c1".into(),
            provider: "cursor".into(),
            max_concurrency: 2,
            disabled: false,
            created_at: 0,
            extra: e,
        }
    }

    /// 从 worker 收回的热设置:护栏策略句非法 → 返回 Err(原因并进面板的同步错误),
    /// 且保留上一份有效值;合法 → Ok 并生效。
    #[test]
    fn apply_hot_settings_reports_rejected_guard_and_keeps_previous() {
        let _g = GUARD_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let p = CursorProvider::new(CursorConfig::default());
        p.apply_hot_settings(&serde_json::json!({"cursor_tool_guard": "护栏 v1"})).unwrap();
        let rev = tool_guard_rev();
        let too_long = "x".repeat(TOOL_GUARD_POLICY_MAX_CHARS + 1);
        let err = p
            .apply_hot_settings(&serde_json::json!({"cursor_tool_guard": too_long}))
            .unwrap_err();
        assert!(err.contains("cursor 护栏策略句未应用"), "{err}");
        assert_eq!(tool_guard_rev(), rev, "非法值不得顶掉上一份有效值");
        assert_eq!(p.settings_fingerprints()["cursor_guard_rev"], serde_json::json!(rev));
        set_tool_guard_policy("").unwrap();
    }

    #[test]
    fn family_is_cursor() {
        assert_eq!(
            CursorProvider::new(CursorConfig::default()).family(),
            "cursor"
        );
    }

    fn test_registry() -> ConvRegistry {
        ConvRegistry {
            inner: std::sync::Mutex::new(std::collections::HashMap::new()),
            stateful: true,
        }
    }

    /// 2026-09-13 生产实病的回归锁:分叉重铺后,登记落基础 id 且 `wire_id`
    /// 回指上游新会话,下一轮必须自愈为 `Continue(wire_id)` —— 修复前
    /// confirm 写到每轮一新的 digest id,基础 id 条目永远停在旧指纹,
    /// 会话一旦分叉就永久每轮全量重铺、模拟缓存永不命中。
    #[test]
    fn 分叉重铺后基础条目回指上游会话() {
        let reg = test_registry();
        let base = "base-conv";
        // 旧历史 2 轮,正常登记(wire == base)。
        reg.confirm_with_fps(base, "acc", vec![11, 22], base);
        // 调用方改写历史(/compact):前缀对不上 → 分叉。
        assert!(matches!(
            reg.cli_lookup(base, "acc", &[99, 22]),
            CliLookup::Diverged
        ));
        // 分叉轮换新 id 重铺成功:登记落基础 id、回指新 wire id。
        reg.confirm_with_fps(base, "acc", vec![99, 22, 33], "wire-new");
        // 下一轮历史再涨一条(减末轮口径由调用方做,这里直接给前缀):
        // 自愈为 Continue,且回指分叉轮的新会话 id。
        match reg.cli_lookup(base, "acc", &[99, 22, 33]) {
            CliLookup::Continue(w) => assert_eq!(w, "wire-new"),
            _ => panic!("分叉重铺后应自愈为 Continue"),
        }
        // 继续涨历史仍 Continue(指纹更新模拟下一轮 confirm)。
        reg.confirm_with_fps(base, "acc", vec![99, 22, 33, 44], "wire-new");
        match reg.cli_lookup(base, "acc", &[99, 22, 33, 44]) {
            CliLookup::Continue(w) => assert_eq!(w, "wire-new"),
            _ => panic!("续轮 confirm 后应继续 Continue"),
        }
        // 调用方再次改写历史 → 再次分叉(安全语义不变)。
        assert!(matches!(
            reg.cli_lookup(base, "acc", &[77]),
            CliLookup::Diverged
        ));
        // 换号 → Fresh(会话是 per-account 的)。
        assert!(matches!(
            reg.cli_lookup(base, "other", &[99, 22, 33]),
            CliLookup::Fresh
        ));
        // 未分叉的普通会话:wire_id == key,Continue 回指自身。
        reg.confirm_with_fps("plain", "acc", vec![1, 2], "plain");
        match reg.cli_lookup("plain", "acc", &[1, 2]) {
            CliLookup::Continue(w) => assert_eq!(w, "plain"),
            _ => panic!("普通会话应 Continue"),
        }
    }

    #[test]
    fn default_hosts_are_the_run_endpoint_and_api2() {
        let d = CursorConfig::default();
        // 2026-09-12 起默认 = 服务端下发的 global 区域路由域名(3.18.25 重抓)。
        assert_eq!(d.agent_host, "agentn.global.api5.cursor.sh");
        assert_eq!(d.api_host, "api2.cursor.sh");
        // 退役端点的域名不该再作为推理主机
        assert_ne!(d.agent_host, "api2.cursor.sh");
    }

    #[test]
    fn from_config_reads_both_hosts_and_defaults_each() {
        let cfg = serde_json::json!({"cursor":{"agent_host":"agentn.api5.cursor.sh"}});
        let c = CursorConfig::from_cfg(&cfg);
        assert_eq!(c.agent_host, "agentn.api5.cursor.sh", "显式配置旧域名也要尊重(覆盖默认)");
        assert_eq!(c.api_host, "api2.cursor.sh", "未配的那个走默认");

        let empty = CursorConfig::from_cfg(&serde_json::Value::Null);
        assert_eq!(empty.agent_host, "agentn.global.api5.cursor.sh");
        // 空串不算配置
        let blank = CursorConfig::from_cfg(&serde_json::json!({"cursor":{"agent_host":"  "}}));
        assert_eq!(blank.agent_host, "agentn.global.api5.cursor.sh");
    }

    #[test]
    fn schema_declares_credentials_and_anti_correlation_fields() {
        let p = CursorProvider::new(CursorConfig::default());
        let s = p.account_schema();
        let has = |n: &str| s.iter().any(|f| f.name == n);
        assert!(s.iter().any(|f| f.name == "access_token" && f.required));
        for f in [
            "refresh_token",
            "machine_id",
            "mac_machine_id",
            "timezone",
            "proxy",
        ] {
            assert!(has(f), "schema 缺字段 {f}");
        }
        // 凭据必须是 Password 类型,别在后台明文回显
        for f in ["access_token", "refresh_token"] {
            let spec = s.iter().find(|x| x.name == f).unwrap();
            assert!(
                matches!(spec.field_type, FieldType::Password),
                "{f} 应为 Password"
            );
        }
        // 模型白名单已定稿落地(2026-08-13):字段名 model_allowlist,
        // 缺失/null=不限、空表/类型错=全禁,匹配器在 gw-core。旧键名 `models`
        // 从未有账号配过,不留兼容 —— schema 里绝不能再出现旧键。
        assert!(
            has("model_allowlist"),
            "白名单字段该进 schema 了(语义已定稿)"
        );
        assert!(!has("models"), "旧键名不许复活");
    }

    #[test]
    fn token_required_else_bad_request() {
        assert!(CursorProvider::token_of(&acct(&[])).is_err());
        assert_eq!(
            CursorProvider::token_of(&acct(&[("access_token", "tok")])).unwrap(),
            "tok"
        );
    }

    #[test]
    fn machine_ids_explicit_or_derived_and_always_present() {
        let a = acct(&[("access_token", "t"), ("machine_id", "MID")]);
        assert_eq!(CursorProvider::machine_id_of(&a, "t"), "MID");

        // 都留空时两个 id 都要派生出来,且**互不相同**(真机上是两个独立值)
        let b = acct(&[("access_token", "t")]);
        let mid = CursorProvider::machine_id_of(&b, "t");
        let mac = CursorProvider::mac_machine_id_of(&b, "t");
        assert_eq!(mid.len(), 64);
        assert_eq!(mac.len(), 64);
        assert_ne!(mid, mac);
        // 派生必须确定性:同号每次请求身份要冻结
        assert_eq!(mac, CursorProvider::mac_machine_id_of(&b, "t"));
    }

    #[test]
    fn derived_ids_produce_137_char_checksum_like_real_client() {
        // 真 IDE 的 checksum 恒为 137 字符;身份留空也不能缩水成 72。
        let a = acct(&[("access_token", "t")]);
        let mid = CursorProvider::machine_id_of(&a, "t");
        let mac = CursorProvider::mac_machine_id_of(&a, "t");
        assert_eq!(wire::checksum(&mid, Some(&mac)).len(), 137);
    }

    #[test]
    fn timezone_defaults_but_is_overridable() {
        assert_eq!(CursorProvider::timezone_of(&acct(&[])), "Asia/Shanghai");
        assert_eq!(
            CursorProvider::timezone_of(&acct(&[("timezone", "America/Los_Angeles")])),
            "America/Los_Angeles"
        );
    }

    #[test]
    fn client_for_falls_back_to_egress_without_proxy() {
        let p = CursorProvider::new(CursorConfig::default());
        // 无 proxy → 默认出口(不 panic,拿得到 client)
        let _ = p.client_for(&acct(&[("access_token", "t")]));
        // 非法 proxy → 回退而不是崩
        let _ = p.client_for(&acct(&[("access_token", "t"), ("proxy", "not a url")]));
        // 合法 proxy → 建得出来,且第二次命中缓存
        let a = acct(&[("access_token", "t"), ("proxy", "http://127.0.0.1:1080")]);
        let _ = p.client_for(&a);
        let _ = p.client_for(&a);
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 1);
    }

    /// 同出口的两个号**各自一个 client**(各自一条连接),不再共用一条 HTTP/2 连接;
    /// 同一个号换了出口也建新 client,不复用旧出口的连接。
    #[test]
    fn proxy_client_is_per_account_and_per_proxy() {
        let p = CursorProvider::new(CursorConfig::default());
        let mk = |id: &str, proxy: &str| {
            let mut a = acct(&[("access_token", "t"), ("proxy", proxy)]);
            a.account_id = id.into();
            a
        };
        let _ = p.client_for(&mk("cursor-a", "http://127.0.0.1:1080")).unwrap();
        let _ = p.client_for(&mk("cursor-b", "http://127.0.0.1:1080")).unwrap();
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 2, "同出口两个号应各有一个 client");
        let _ = p.client_for(&mk("cursor-a", "http://127.0.0.1:1080")).unwrap();
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 2, "同号同出口命中缓存");
        let _ = p.client_for(&mk("cursor-a", "http://127.0.0.1:1081")).unwrap();
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 3, "换出口建新 client");
        let keys: Vec<String> = p.proxy_clients.lock().unwrap().keys().cloned().collect();
        // 键里带着各自的代理地址:出口不会串
        assert!(keys.contains(&provider::proxy_client_key("cursor-a", "http://127.0.0.1:1081")));
        assert!(keys.contains(&provider::proxy_client_key("cursor-b", "http://127.0.0.1:1080")));
    }

    /// 连接句柄只丢「本请求用的那一代」:同一条坏连接上好几个请求陆续超时,第一个丢掉后
    /// 重建的新连接不能被后面的再丢。
    #[test]
    fn conn_handle_evicts_only_its_own_generation() {
        let p = CursorProvider::new(CursorConfig::default());
        let mut a = acct(&[("access_token", "t"), ("proxy", "http://127.0.0.1:1080")]);
        a.account_id = "cursor-a".into();
        let (_, h1) = p.client_and_handle(&a).unwrap();
        let (_, h1b) = p.client_and_handle(&a).unwrap(); // 同一代(命中缓存)
        let h1 = h1.expect("配了代理的号有句柄");
        let h1b = h1b.unwrap();
        assert!(h1.evict(), "第一次卡住:丢掉旧连接");
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 0);
        let (_, h2) = p.client_and_handle(&a).unwrap(); // 下一个请求重建
        let h2 = h2.unwrap();
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 1);
        assert!(!h1b.evict(), "旧连接上后到的超时不能丢掉新连接");
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 1);
        assert!(h2.evict(), "新连接自己卡住时照样能丢");
        // 默认出口(没配代理)没有句柄
        let (_, none) = p.client_and_handle(&acct(&[("access_token", "t")])).unwrap();
        assert!(none.is_none());
    }

    /// 只有「连接一个字节都没收到」算连接卡死;只有心跳(连接活着)、服务端总结超时、
    /// 工具参数增量、别的错误都不算。
    #[test]
    fn connection_stall_classification() {
        use gw_core::error::UpstreamError;
        let mk = |k, m: &str| UpstreamError::new(k, m.to_string());
        let stall = format!("Cursor Run 150s 内无进展,{} 150s(连接疑似卡死)", chat::CONN_DEAD_MARK);
        assert!(chat::is_connection_stall(&mk(UpstreamErrorKind::ServerError, &stall)));
        let hb_only = format!("Cursor Run 90s {}、没有任何文本或用量帧", chat::STALL_HEARTBEAT_ONLY);
        assert!(!chat::is_connection_stall(&mk(UpstreamErrorKind::ServerError, &hb_only)));
        assert!(!chat::is_connection_stall(&mk(UpstreamErrorKind::ServerError, "Cursor Run 服务端总结压缩上下文 180s 仍未完成")));
        assert!(!chat::is_connection_stall(&mk(UpstreamErrorKind::ServerError, "Cursor Run 150s 内只有工具参数/计数增量、没有任何可见输出")));
        assert!(!chat::is_connection_stall(&mk(UpstreamErrorKind::BadRequest, &stall)));
    }

    /// 包装层:流里出现连接卡死 → 丢掉这个号的连接;别的错误、正常项不动。
    #[tokio::test]
    async fn heal_stalled_connection_evicts_on_stall_only() {
        use futures::StreamExt;
        use gw_core::error::UpstreamError;
        let p = CursorProvider::new(CursorConfig::default());
        let mut a = acct(&[("access_token", "t"), ("proxy", "http://127.0.0.1:1080")]);
        a.account_id = "cursor-a".into();

        let (_, h) = p.client_and_handle(&a).unwrap();
        let other: ChatStream = Box::pin(futures::stream::iter(vec![Err(UpstreamError::new(
            UpstreamErrorKind::ServerError,
            format!("Cursor Run 90s {}、没有任何文本或用量帧", chat::STALL_HEARTBEAT_ONLY),
        ))]));
        let _ = provider::heal_stalled_connection(other, h).collect::<Vec<_>>().await;
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 1, "非连接卡死不丢连接");

        let (_, h) = p.client_and_handle(&a).unwrap();
        let stalled: ChatStream = Box::pin(futures::stream::iter(vec![Err(UpstreamError::new(
            UpstreamErrorKind::ServerError,
            format!("Cursor Run 150s 内无进展,{} 150s(连接疑似卡死)", chat::CONN_DEAD_MARK),
        ))]));
        let items = provider::heal_stalled_connection(stalled, h).collect::<Vec<_>>().await;
        assert_eq!(items.len(), 1, "错误照常交给下游");
        assert_eq!(p.proxy_clients.lock().unwrap().len(), 0, "连接卡死:丢掉这个号的连接");
    }

    /// 一条坏连接只扣一次账号健康:第一个流照常上报,同一代连接上的其余流打连带回声标记;
    /// 重建后的新一代连接再卡死,照常计数(根因一直不好时号仍会被禁)。
    #[tokio::test]
    async fn heal_counts_one_failure_per_dead_connection() {
        use futures::StreamExt;
        use gw_core::error::UpstreamError;
        let p = CursorProvider::new(CursorConfig::default());
        let mut a = acct(&[("access_token", "t"), ("proxy", "http://127.0.0.1:1080")]);
        a.account_id = "cursor-a".into();
        let dead = || -> ChatStream {
            Box::pin(futures::stream::iter(vec![Err(UpstreamError::new(
                UpstreamErrorKind::ServerError,
                format!("Cursor Run 90s 内无进展,{} 90s(连接疑似卡死)", chat::CONN_DEAD_MARK),
            ))]))
        };
        let spared = |items: Vec<Result<gw_core::provider::StreamItem, UpstreamError>>| match items.as_slice() {
            [Err(e)] => e.collateral_failure,
            other => panic!("应恰好一个错误,实际 {} 项", other.len()),
        };

        // 同一条连接上的三个流(同一代)。
        let (_, h1) = p.client_and_handle(&a).unwrap();
        let (_, h2) = p.client_and_handle(&a).unwrap();
        let (_, h3) = p.client_and_handle(&a).unwrap();
        let first = provider::heal_stalled_connection(dead(), h1).collect::<Vec<_>>().await;
        assert!(!spared(first), "第一个发现者照常上报");
        let second = provider::heal_stalled_connection(dead(), h2).collect::<Vec<_>>().await;
        assert!(spared(second), "同一代连接的连带超时不再计数");

        // 重建出新一代连接后再卡死:新的根因,照常计数。
        let (_, h_new) = p.client_and_handle(&a).unwrap();
        let fresh = provider::heal_stalled_connection(dead(), h_new).collect::<Vec<_>>().await;
        assert!(!spared(fresh), "新一代连接卡死照常上报");

        // 旧一代剩下的流此时才超时:缓存里已是更新的一代,仍算连带回声,也不能丢掉新连接。
        let late = provider::heal_stalled_connection(dead(), h3).collect::<Vec<_>>().await;
        assert!(spared(late), "旧连接上迟到的超时不计数");

        // 非连接卡死的报错从不打标记;默认出口(没有句柄)也不打。
        let (_, h_other) = p.client_and_handle(&a).unwrap();
        let hb: ChatStream = Box::pin(futures::stream::iter(vec![Err(UpstreamError::new(
            UpstreamErrorKind::ServerError,
            format!("Cursor Run 90s {}、没有任何文本或用量帧", chat::STALL_HEARTBEAT_ONLY),
        ))]));
        let hb = provider::heal_stalled_connection(hb, h_other).collect::<Vec<_>>().await;
        assert!(!spared(hb), "心跳型卡住不打标记");
        let no_handle = provider::heal_stalled_connection(dead(), None).collect::<Vec<_>>().await;
        assert!(!spared(no_handle), "没有连接句柄时不打标记");
    }

    #[test]
    fn validate_account_checks_token() {
        let p = CursorProvider::new(CursorConfig::default());
        assert!(p.validate_account(&acct(&[])).is_err());
        assert!(p
            .validate_account(&acct(&[("access_token", "tok")]))
            .is_ok());
    }

    /// ⭐ **严格前缀不得匹配** —— 这条是本表比 clidrv 严的全部理由。
    ///
    /// clidrv 容忍落后条目(CLI 子进程自己持有真实历史,续上不丢轮);描述符没有
    /// 这条腿:条目链是请求历史的严格前缀 = 存在这份清单不覆盖的轮次,而续轮只发
    /// 增量消息 → 模型看不到中间轮却照样答,**没有任何错误信号**。
    #[test]
    fn 影子表_严格前缀不匹配() {
        let s = DescriptorShadow::default();
        // turn1:请求历史 [u1],输出 a1 → 链 [u1, a1]。
        s.commit("conv", "A", "m1", &[1], Some(11), false, b"desc-1", 4);
        // 正:下一轮的历史正是 [u1, a1]。
        assert_eq!(
            s.lookup("conv", "A", "m1", &[1, 11]).as_deref(),
            Some(&b"desc-1"[..])
        );
        // 负:链是历史的严格前缀(中间又过了一轮 u2/a2)→ 必须重铺。
        assert!(
            s.lookup("conv", "A", "m1", &[1, 11, 2, 22]).is_none(),
            "落后一轮的描述符绝不能回放"
        );
        // 负:历史比链短(调用方截了历史)→ 也不匹配。
        assert!(s.lookup("conv", "A", "m1", &[1]).is_none());
        // 负:同长但内容不同(分叉)。
        assert!(s.lookup("conv", "A", "m1", &[1, 99]).is_none());
    }

    /// 两轮推进:commit 找到精确前任就**替换**,不累积条目。
    #[test]
    fn 影子表_两轮推进替换前任() {
        let s = DescriptorShadow::default();
        s.commit("conv", "A", "m1", &[1], Some(11), false, b"desc-1", 4);
        // turn2:请求历史 [u1,a1,u2],前任 = [u1,a1] → 替换。
        let c = s.commit("conv", "A", "m1", &[1, 11, 2], Some(22), false, b"desc-2", 6);
        assert_eq!(c.same_account, Some(true), "找到精确前任");
        assert_eq!(c.changed, Some(true));
        // 旧链已不存在。
        assert!(
            s.lookup("conv", "A", "m1", &[1, 11]).is_none(),
            "旧链应被接替"
        );
        // 新链可用。
        assert_eq!(
            s.lookup("conv", "A", "m1", &[1, 11, 2, 22]).as_deref(),
            Some(&b"desc-2"[..])
        );
    }

    /// 并列精确前序(并行分支撞上同一段历史)→ 歧义不挑,重铺。
    #[test]
    fn 影子表_并列前序歧义不挑() {
        // 两次 commit 的前任都是 `[]`(request_fps 只有一条),所以第二次找不到前任 →
        // 追加。于是同 conv 下两条条目的链都是 [1, 11] = 真并列。
        let s2 = DescriptorShadow::default();
        s2.commit("conv", "A", "m1", &[1], Some(11), false, b"desc-x", 1);
        s2.commit("conv", "A", "m1", &[1], Some(11), false, b"desc-y", 1);
        // 第二次 commit 的前任 = [] ,与第一条链 [1,11] 不等 → 追加,于是两条链都是 [1,11]。
        assert!(
            s2.lookup("conv", "A", "m1", &[1, 11]).is_none(),
            "两条精确前序并列时绝不瞎挑"
        );
    }

    /// 跨号 / 换模型不回放(描述符是 per-account 的服务端状态;预算表随模型)。
    #[test]
    fn 影子表_跨号与换模型不回放() {
        let s = DescriptorShadow::default();
        s.commit("conv", "A", "m1", &[1], Some(11), false, b"desc-A", 4);
        assert!(
            s.lookup("conv", "B", "m1", &[1, 11]).is_none(),
            "跨号必须重铺"
        );
        assert!(
            s.lookup("conv", "A", "m2", &[1, 11]).is_none(),
            "换模型必须重铺"
        );
        assert!(s.lookup("other", "A", "m1", &[1, 11]).is_none(), "别的会话");
        // 同号同模型仍可用(证明上面三条不是被别的原因挡掉的)。
        assert!(s.lookup("conv", "A", "m1", &[1, 11]).is_some());
    }

    /// 缺 assistant_fp 的条目(失败轮 / 工具轮)**硬性不匹配**:它的链是残的。
    #[test]
    fn 影子表_缺assistant_fp硬性不匹配() {
        let s = DescriptorShadow::default();
        s.commit("conv", "A", "m1", &[1], None, false, b"desc-partial", 4);
        // 链只有 [1](没追 assistant),按链相等去查也不给。
        assert!(
            s.lookup("conv", "A", "m1", &[1]).is_none(),
            "链残的条目即使链相等也不得回放"
        );
    }

    /// 工具轮条目(tool_turn):assistant_fp 恒 None,但走「链 == 历史减两条」的
    /// 宽松匹配 —— 历史减末条(tool_result)后尾部那条 assistant tool_use 不参与
    /// 比对(结构校验在 chat::prev_assistant_has_tool_use 做过了)。
    #[test]
    fn 影子表_tool_turn宽松匹配() {
        let s = DescriptorShadow::default();
        // 工具轮:请求历史 [1, 2](含当时末条 user),无 assistant 指纹。
        s.commit("conv", "A", "m1", &[1, 2], None, true, b"desc-tool", 5);
        // 工具结果轮:历史减末条 = [1, 2, <assistant tool_use>] → 命中。
        assert_eq!(
            s.lookup("conv", "A", "m1", &[1, 2, 77]).as_deref(),
            Some(&b"desc-tool"[..]),
            "链 == 历史减两条 必须命中"
        );
        // 不多减一条:[1, 2] 不是工具结果轮的视角(它尾部没有 tool_use)→ 不中。
        assert!(s.lookup("conv", "A", "m1", &[1, 2]).is_none());
        // 多减了两条以上 / 前缀不同 → 不中。
        assert!(s.lookup("conv", "A", "m1", &[1, 2, 77, 88]).is_none());
        assert!(s.lookup("conv", "A", "m1", &[9, 2, 77]).is_none());
        // 换号 / 换模型照旧不中(与普通条目同规矩)。
        assert!(s.lookup("conv", "B", "m1", &[1, 2, 77]).is_none());
        assert!(s.lookup("conv", "A", "m2", &[1, 2, 77]).is_none());
        // 工具轮条目之后的正常轮:链推进回精确匹配。模拟结果轮成功提交:
        // 请求历史 [1, 2, 77, 3](含末条 user)+ assistant 指纹 33。
        s.commit("conv", "A", "m1", &[1, 2, 77, 3], Some(33), false, b"desc-next", 6);
        assert!(s
            .lookup("conv", "A", "m1", &[1, 2, 77, 3, 33])
            .is_some());
    }

    /// 作废只摘同号,别号血脉不受影响。
    #[test]
    fn 影子表_作废只摘同号() {
        let s = DescriptorShadow::default();
        s.commit("conv", "A", "m1", &[1], Some(11), false, b"desc-A", 1);
        s.commit("conv", "B", "m1", &[2], Some(22), false, b"desc-B", 1);
        assert_eq!(s.invalidate("conv", "A"), 1);
        assert!(
            s.lookup("conv", "A", "m1", &[1, 11]).is_none(),
            "A 的已作废"
        );
        assert!(
            s.lookup("conv", "B", "m1", &[2, 22]).is_some(),
            "B 的不受影响"
        );
        // 幂等。
        assert_eq!(s.invalidate("conv", "A"), 0);
        assert_eq!(s.invalidate("nope", "A"), 0);
    }
}
