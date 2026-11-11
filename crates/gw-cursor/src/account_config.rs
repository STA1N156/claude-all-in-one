//! 账号字段说明、配置版本缓存与运行参数(CursorConfig / RunTuning)。
//!
//! 2026-11-10 由 lib.rs 拆出(只搬不改行为)。

use super::*;

/// config_version 缓存 TTL:真 IDE 按 poll interval(分钟级)刷新,取 2 分钟保守值,
/// 既避免每次 chat 都打一发 GetServerConfig,又能跟上服务端轮换。
/// `config_version` 缓存寿命。
///
/// 2026-08-07 上调 120s → 30min:实测 `GetServerConfig` 单次要 **5–6 秒**,而
/// 120s 的 TTL 在真实客户端(opencode 一次交互并发发好几个请求)下几乎每轮都过期,
/// 于是每个请求头上都白挂 5 秒。这个值是握手下发的配置版本,不是会过期的凭据,
/// 半小时一取足够。
pub(super) const CONFIG_VERSION_TTL: Duration = Duration::from_secs(1800);

/// 一条 config_version 缓存记录。
///
/// `value: None` = **上次取失败了**(负缓存)。有它才能挡住「api2 持续故障时,
/// 每个请求都排进单飞闸门、各付一次完整超时」—— 那会把 worker 吞吐塌成 1 req / 5-6s。
pub(super) struct ConfigEntry {
    pub(super) value: Option<String>,
    pub(super) at: Instant,
}

/// 取失败后多久不再重试。
///
/// 比 [`CONFIG_VERSION_TTL`] 短得多:失败是瞬态的,60 秒后该再试一次;
/// 但 60 秒内的请求不该各自再去撞一遍。
pub(super) const CONFIG_FAIL_TTL: Duration = Duration::from_secs(60);

/// 账号未配时区时的缺省值。
pub(super) const DEFAULT_TIMEZONE: &str = "Asia/Shanghai";

pub(super) const CURSOR_ACCOUNT_SCHEMA: &[FieldSpec] = &[
    FieldSpec::new("account_id", "账号 ID", FieldType::String, true),
    FieldSpec::new("access_token", "Access Token", FieldType::Password, true)
        .with_help("Cursor session JWT;取自 state.vscdb 的 cursorAuth/accessToken"),
    FieldSpec::new("refresh_token", "Refresh Token", FieldType::Password, false)
        .with_help("取自 state.vscdb 的 cursorAuth/refreshToken。留空则无法自动续期,token 过期后该号会被判失效下线"),
    FieldSpec::new("machine_id", "Machine ID", FieldType::String, false)
        .with_help("checksum 用的 machineId;应填真 IDE 的 telemetry.machineId(64-hex,取自 storage.json);留空则按 sha256hex(token) 派生"),
    FieldSpec::new("mac_machine_id", "Mac Machine ID", FieldType::String, false)
        .with_help("真 IDE 的 telemetry.macMachineId(64-hex);留空则派生一个 —— 真客户端的 checksum 恒为 137 字符,缺了它长度对不上"),
    FieldSpec::new("config_version", "Config Version", FieldType::String, false)
        .with_help("x-cursor-config-version;留空则每会话现调 GetServerConfig 取新鲜值(推荐留空)"),
    FieldSpec::new("timezone", "时区", FieldType::String, false)
        .with_help("x-cursor-timezone,如 Asia/Shanghai / America/Los_Angeles。应与该号出口 IP 的地理位置一致,否则是关联特征。留空按 Asia/Shanghai"),
    FieldSpec::new("proxy", "出口代理", FieldType::String, false)
        .with_help("该账号专属出口(http/https/socks5)。防关联硬要求:推理、取 config、刷新 token 全走它。留空走 worker 默认出口"),
    // 账号级模型白名单(2026-08-13 落地,此前休眠)。评审提的语义坑已按规格修:
    // 缺失/null = 不限,空表/类型错 = 全禁(fail-closed),字段改名 model_allowlist,
    // 规范存储 JSON 字符串数组(admin PATCH 写侧归一,CSV 只当输入形态)。
    FieldSpec::new("model_allowlist", "可用模型白名单", FieldType::String, false)
        .with_help("该号允许服务的模型,逗号分隔;条目是 Run 侧模型名或「前缀*」(星号仅限末尾),如 default,composer*,grok*。留空 = 不限"),
];

#[derive(Debug, Clone)]
pub struct CursorConfig {
    /// 推理主机(`AgentService/Run`)。
    pub agent_host: String,
    /// unary 服务主机(`GetServerConfig`)。
    pub api_host: String,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            // 2026-09-12 起默认跟服务端下发的 global 区域路由域名(3.18.25 实测,
            // 响应头 x-cursor-server-region 按出口地域分配);旧默认 agentn.api5.cursor.sh
            // 仍可用,但已非真客户端默认。
            agent_host: "agentn.global.api5.cursor.sh".to_string(),
            api_host: "api2.cursor.sh".to_string(),
        }
    }
}

impl CursorConfig {
    pub(super) fn from_cfg(cfg: &serde_json::Value) -> Self {
        let c = cfg.get("cursor");
        let pick = |key: &str, dflt: &str| {
            c.and_then(|v| v.get(key))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(dflt)
                .to_string()
        };
        let d = CursorConfig::default();
        CursorConfig {
            agent_host: pick("agent_host", &d.agent_host),
            api_host: pick("api_host", &d.api_host),
        }
    }
}

/// 请求形状调优。
///
/// **生产一律用 [`Default`](RunTuning::default)**(= 与真客户端一致)。这组开关存在的
/// 唯一理由是协议试错:`PROTOCOL-agent-run.md` §0 承认从没做过删字段/删帧实验,
/// 「哪些字段必需」是空白,只能对着真上游二分。放在 API 上而不是读环境变量 ——
/// 上一版的 `CURSOR_METHOD` 环境变量开关就是这么让生产悄悄打错端点的。
#[derive(Debug, Clone, Copy)]
pub struct RunTuning {
    pub shape: run::RunShape,
    /// 补发两个 `field 3` 上下文帧(真客户端初始 3 帧)。
    pub context_frames: bool,
    /// 发完初始帧后不 half-close 请求流(真客户端是 BiDi)。
    pub keep_stream_open: bool,
    /// 请求形态:IDE(3.14.27)还是 CLI(cursor-agent 2026.08.11)。见 [`cli::Profile`]。
    pub profile: cli::Profile,
    /// 续轮把上一轮捕到的描述符原样回放进 `1.1`(wire v2 主路,仅 CLI 形态有意义)。
    ///
    /// 真 CLI 就是这么干的,所以 [`RunTuning::faithful`] 里是 `true`。
    ///
    /// ⚠️ **关掉它不是「另一种热续方案」**(codex 审查 #5)。本地构造 `1.1` 那条支
    /// 已经删除(08-23 实物证明客户端从不自己构造续轮 `1.1`),所以关掉后
    /// `lookup` 恒为 `None` → 每轮都走 Opening 全量重铺。也就是说这个开关比的是
    /// **「描述符增量」vs「CLI 形态全量重铺」**,不是两种增量方案。
    ///
    /// 「描述符路线比内联热续省不省」那个问题的对照臂是 **clidrv**(子进程驱动,
    /// 热续 49–60%),不是本开关的任何一档。
    pub wire_descriptor_replay: bool,
}

impl Default for RunTuning {
    fn default() -> Self {
        Self::faithful()
    }
}

impl RunTuning {
    /// 与真客户端一致:全分节 + 3 帧 + 不关流。
    ///
    /// 形态默认 IDE(生产在跑的形态);`CURSOR_PROFILE=cli` 切 CLI 形态 ——
    /// 2026-08-16 抓包证实 CLI 形态服务端持史成立(见 `cli.rs` 模块文档),
    /// 但默认切换要等 e2e 实测全部通过之后。
    pub fn faithful() -> Self {
        Self {
            shape: run::RunShape::default(),
            context_frames: true,
            keep_stream_open: true,
            profile: cli::Profile::from_env(),
            wire_descriptor_replay: true,
        }
    }
}
