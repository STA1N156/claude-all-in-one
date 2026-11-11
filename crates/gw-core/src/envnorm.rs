//! 报文环境信息标准化:把客户端塞进请求里的**非功能性**环境细节,换成按号固定的「标准人设」。
//!
//! 与 [`crate::normalize`](线缆卫生,剥每请求都变的指纹)并列,三条线共用同一份实现。
//!
//! ## 为什么
//! 客户端(Claude Code、opencode、Codex、Copilot 类插件)会把本机环境写进 system 或第一条
//! user 消息,caio 原样转给上游。2026-10-06 抽查 2000 条:
//! - `OS Version: Windows 11 Home China 10.0.22631`(地区版 Windows,几百条);
//! - `Timezone: Asia/Shanghai`、`Locale: zh-CN`(oh-my-opencode 的 `<omo-env>`、Codex 的 `<environment_context>`);
//! - `Respond in the following locale: zh-cn`(编辑器界面语言);
//! - `Git user: <真实姓名>`。
//!
//! 这些对模型干活没用,却直接暴露使用者所在地区与身份,且同一个号会先后服务多个客户,
//! 上游看到的环境来回跳。
//!
//! ## 不动什么
//! 平台大类、Shell、工作目录、日期是 agent 的**功能性**信息:模型据此决定发 PowerShell 还是
//! bash、用什么路径。改了客户端就执行失败。所以只换上面那几类,其余一律原样。
//!
//! ## 性质
//! - **按号固定**:同一个号永远呈现同一套人设(号的 `extra.env_persona` 优先,否则按号哈希);
//!   上游缓存本来就按号隔离,换号不会额外失效。
//! - **确定性 + 幂等**:同号同输入 → 同输出,处理两次 = 处理一次,保证多轮的缓存前缀稳定。
//! - **扫 system 与所有 user 消息的 text 块**(不只当前轮):只改当前轮的话,下一轮历史里的同一段
//!   字节对不上,前缀缓存断。不碰 tool_result、assistant、工具定义;不改块数与 `cache_control`。
//! - 认不出的格式一律不动。
//!
//! ## 开关
//! 进程级全局配置([`set_config`] / [`enabled_for`]),worker 启动与 30s 热调时同步,
//! 各线路(provider family)单独开关,默认全关。打开的瞬间,进行中的会话会有一次缓存失效。

use std::sync::{Arc, LazyLock, RwLock};

use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::account::Account;
use crate::config::EnvNormalizeConfig;

/// 可开关的线路(provider family)。
pub const KNOWN_FAMILIES: &[&str] = &["kiro", "claude-dario", "cursor"];

/// 一套标准人设:同一个号在各平台大类下呈现的系统版本,以及 git 用户名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Persona {
    pub name: &'static str,
    /// Windows 版本名(不含地区词),如 `Windows 11 Pro`。
    pub windows_edition: &'static str,
    /// Windows 内部版本号,如 `10.0.26100`。
    pub windows_build: &'static str,
    /// macOS 的 `OS Version` 写法,如 `Darwin 24.6.0`。
    pub macos_kernel: &'static str,
    /// Linux 的 `OS Version` 写法,如 `Linux 6.8.0-85-generic`。
    pub linux_kernel: &'static str,
    pub git_user: &'static str,
}

/// 内置人设。改动会让对应号的上游前缀缓存失效一次;只追加、不改已有项。
pub const PERSONAS: &[Persona] = &[
    Persona { name: "p1", windows_edition: "Windows 11 Pro", windows_build: "10.0.26100", macos_kernel: "Darwin 24.6.0", linux_kernel: "Linux 6.8.0-85-generic", git_user: "Alex Morgan" },
    Persona { name: "p2", windows_edition: "Windows 11 Home", windows_build: "10.0.26100", macos_kernel: "Darwin 25.0.0", linux_kernel: "Linux 6.11.0-29-generic", git_user: "Jordan Lee" },
    Persona { name: "p3", windows_edition: "Windows 11 Pro", windows_build: "10.0.22631", macos_kernel: "Darwin 24.5.0", linux_kernel: "Linux 6.14.0-33-generic", git_user: "Sam Carter" },
    Persona { name: "p4", windows_edition: "Windows 10 Pro", windows_build: "10.0.19045", macos_kernel: "Darwin 23.6.0", linux_kernel: "Linux 6.8.0-79-generic", git_user: "Taylor Brooks" },
    Persona { name: "p5", windows_edition: "Windows 11 Enterprise", windows_build: "10.0.26100", macos_kernel: "Darwin 24.6.0", linux_kernel: "Linux 6.1.0-38-amd64", git_user: "Casey Reed" },
    Persona { name: "p6", windows_edition: "Windows 11 Home", windows_build: "10.0.22631", macos_kernel: "Darwin 25.0.0", linux_kernel: "Linux 6.12.43+deb13-amd64", git_user: "Riley Quinn" },
];

/// 号的人设:`extra.env_persona`(人设名)优先,否则 sha256(account_id) 对人设数取模。
pub fn persona_for(account: &Account) -> &'static Persona {
    if let Some(name) = account.extra_str("env_persona").map(str::trim) {
        if let Some(p) = PERSONAS.iter().find(|p| p.name == name) {
            return p;
        }
    }
    let digest = Sha256::digest(account.account_id.as_bytes());
    let n = u64::from_be_bytes(digest[..8].try_into().expect("8 字节"));
    &PERSONAS[(n % PERSONAS.len() as u64) as usize]
}

/// 时区写法:`UTC`,或 IANA `Area/City[/Sub]`。
pub fn valid_timezone(tz: &str) -> bool {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(UTC|[A-Za-z]+(/[A-Za-z0-9_+\-]+){1,2})$").expect("re"));
    RE.is_match(tz)
}

/// locale 写法:`xx` 或 `xx-YY`。
pub fn valid_locale(l: &str) -> bool {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z]{2,3}(-[A-Z]{2})?$").expect("re"));
    RE.is_match(l)
}

/// 校验配置(写库前与热应用时共用)。
pub fn validate(cfg: &EnvNormalizeConfig) -> Result<(), String> {
    for f in &cfg.providers {
        if !KNOWN_FAMILIES.contains(&f.as_str()) {
            return Err(format!("未知线路 {f:?}(可选 {})", KNOWN_FAMILIES.join(" / ")));
        }
    }
    if !valid_timezone(&cfg.timezone) {
        return Err(format!("时区 {:?} 不合法(须为 UTC 或 Area/City)", cfg.timezone));
    }
    if !valid_locale(&cfg.locale) {
        return Err(format!("locale {:?} 不合法(须形如 en-US)", cfg.locale));
    }
    Ok(())
}

static CONFIG: LazyLock<RwLock<Arc<EnvNormalizeConfig>>> =
    LazyLock::new(|| RwLock::new(Arc::new(EnvNormalizeConfig::default())));

/// 热应用配置。非法 → Err 且保留上一份有效值(说出来,不静默)。
pub fn set_config(cfg: &EnvNormalizeConfig) -> Result<(), String> {
    validate(cfg)?;
    *CONFIG.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(cfg.clone());
    Ok(())
}

pub fn current() -> Arc<EnvNormalizeConfig> {
    CONFIG.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// 该线路是否开启标准化。
pub fn enabled_for(family: &str) -> bool {
    current().providers.iter().any(|f| f == family)
}

/// 本次替换的命中数(按规则)。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EnvNormStats {
    pub os_version: u32,
    pub timezone: u32,
    pub locale: u32,
    pub locale_directive: u32,
    pub git_user: u32,
}

impl EnvNormStats {
    pub fn total(&self) -> u32 {
        self.os_version + self.timezone + self.locale + self.locale_directive + self.git_user
    }
}

/// 该号的时区:`extra.timezone`(须合法;cursor 已用它表示「与出口 IP 一致的时区」)优先,否则全局。
fn timezone_for(account: &Account, cfg: &EnvNormalizeConfig) -> String {
    account
        .extra_str("timezone")
        .map(str::trim)
        .filter(|t| valid_timezone(t))
        .unwrap_or(&cfg.timezone)
        .to_string()
}

/// 按当前全局配置处理(各 provider 调用入口)。
pub fn normalize_body(body: &mut Value, account: &Account) -> EnvNormStats {
    normalize_body_with(body, account, &current())
}

/// 各 provider chat 入口的统一调用:该线路开启才处理,命中时记 debug 日志。
/// 必须在选号之后调用(按 `account` 选人设),并且早于该 provider 的报文转换与缓存模拟。
pub fn apply_for(family: &str, body: &mut Value, account: &Account) -> EnvNormStats {
    if !enabled_for(family) {
        return EnvNormStats::default();
    }
    let st = normalize_body(body, account);
    if st.total() > 0 {
        tracing::debug!(
            family,
            account = %account.account_id,
            persona = persona_for(account).name,
            os_version = st.os_version,
            timezone = st.timezone,
            locale = st.locale,
            locale_directive = st.locale_directive,
            git_user = st.git_user,
            "报文环境信息已标准化"
        );
    }
    st
}

/// 处理 `system` 与所有 user 消息的 text 块。
pub fn normalize_body_with(body: &mut Value, account: &Account, cfg: &EnvNormalizeConfig) -> EnvNormStats {
    let ctx = Ctx { persona: persona_for(account), timezone: timezone_for(account, cfg), locale: cfg.locale.clone() };
    let mut stats = EnvNormStats::default();
    if let Some(sys) = body.get_mut("system") {
        rewrite_content(sys, &ctx, &mut stats);
    }
    if let Some(Value::Array(msgs)) = body.get_mut("messages") {
        for m in msgs.iter_mut() {
            if m.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            if let Some(c) = m.get_mut("content") {
                rewrite_content(c, &ctx, &mut stats);
            }
        }
    }
    stats
}

struct Ctx {
    persona: &'static Persona,
    timezone: String,
    locale: String,
}

/// 字符串内容,或 `[{type:"text", text}]` 块数组(其它类型的块不动)。
fn rewrite_content(v: &mut Value, ctx: &Ctx, stats: &mut EnvNormStats) {
    match v {
        Value::String(s) => {
            if let Some(n) = rewrite_text(s, ctx, stats) {
                *s = n;
            }
        }
        Value::Array(blocks) => {
            for b in blocks.iter_mut() {
                if b.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                if let Some(Value::String(s)) = b.get_mut("text") {
                    if let Some(n) = rewrite_text(s, ctx, stats) {
                        *s = n;
                    }
                }
            }
        }
        _ => {}
    }
}

static RE_OS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^([ \t]*(?:-[ \t]*)?OS Version:[ \t]*)([^\r\n]+)").expect("re"));
static RE_TZ_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^([ \t]*(?:-[ \t]*)?Timezone:[ \t]*)([^\s<]+)").expect("re"));
static RE_TZ_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<timezone>[^<\r\n]*</timezone>").expect("re"));
static RE_LOCALE_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^([ \t]*(?:-[ \t]*)?Locale:[ \t]*)([^\s<]+)").expect("re"));
static RE_LOCALE_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Respond in the following locale:[ \t]*[A-Za-z_\-]+\.?").expect("re"));
static RE_GIT_USER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^(Git user:[ \t]*)([^\r\n]+)").expect("re"));

/// 跟随用户语言的中性指令(取代暴露编辑器语言的 locale 指令)。
pub const LANGUAGE_DIRECTIVE: &str = "Respond in the same language the user writes in.";

/// 有改动返回新串,否则 None(先用 contains 粗筛,大报文开销可忽略)。
fn rewrite_text(s: &str, ctx: &Ctx, stats: &mut EnvNormStats) -> Option<String> {
    let has = |m: &str| s.contains(m);
    if !(has("OS Version:") || has("Timezone:") || has("<timezone>") || has("Locale:") || has("Respond in the following locale:") || has("Git user:")) {
        return None;
    }
    let mut out = s.to_string();
    let mut changed = false;
    let mut apply = |re: &Regex, out: &mut String, f: &dyn Fn(&regex::Captures) -> Option<String>, counter: &mut u32| {
        let mut hits = 0u32;
        let replaced = re
            .replace_all(out.as_str(), |c: &regex::Captures| match f(c) {
                Some(r) => {
                    if r != c[0] {
                        hits += 1;
                    }
                    r
                }
                None => c[0].to_string(),
            })
            .into_owned();
        if hits > 0 {
            *out = replaced;
            *counter += hits;
            changed = true;
        }
    };
    let p = ctx.persona;
    apply(&RE_OS, &mut out, &|c| os_version(&c[2]).map(|fam| format!("{}{}", &c[1], render_os(fam, &c[2], p))), &mut stats.os_version);
    apply(&RE_TZ_LINE, &mut out, &|c| Some(format!("{}{}", &c[1], ctx.timezone)), &mut stats.timezone);
    apply(&RE_TZ_TAG, &mut out, &|_| Some(format!("<timezone>{}</timezone>", ctx.timezone)), &mut stats.timezone);
    apply(&RE_LOCALE_LINE, &mut out, &|c| Some(format!("{}{}", &c[1], ctx.locale)), &mut stats.locale);
    apply(&RE_LOCALE_DIRECTIVE, &mut out, &|_| Some(LANGUAGE_DIRECTIVE.to_string()), &mut stats.locale_directive);
    apply(&RE_GIT_USER, &mut out, &|c| Some(format!("{}{}", &c[1], p.git_user)), &mut stats.git_user);
    changed.then_some(out)
}

#[derive(Clone, Copy)]
enum OsFamily {
    /// `Windows 11 Home China 10.0.22631` 这类。
    WindowsNamed,
    /// `win32 10.0.26200 x64` 这类。
    Win32,
    Darwin,
    Linux,
}

/// 认得的系统版本写法;认不出返回 None(原样保留)。
fn os_version(v: &str) -> Option<OsFamily> {
    let v = v.trim();
    if v.starts_with("Windows") {
        Some(OsFamily::WindowsNamed)
    } else if v.starts_with("win32") {
        Some(OsFamily::Win32)
    } else if v.starts_with("Darwin") || v.starts_with("darwin") || v.starts_with("macOS") {
        Some(OsFamily::Darwin)
    } else if v.starts_with("Linux") || v.starts_with("linux") {
        Some(OsFamily::Linux)
    } else {
        None
    }
}

fn render_os(fam: OsFamily, original: &str, p: &Persona) -> String {
    match fam {
        OsFamily::WindowsNamed => format!("{} {}", p.windows_edition, p.windows_build),
        OsFamily::Win32 => {
            // 保留原架构词(x64 / arm64),只换版本号。
            let arch = original.split_whitespace().nth(2).unwrap_or("x64");
            format!("win32 {} {arch}", p.windows_build)
        }
        OsFamily::Darwin => p.macos_kernel.to_string(),
        OsFamily::Linux => p.linux_kernel.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn acct(id: &str, kv: &[(&str, &str)]) -> Account {
        let mut e = BTreeMap::new();
        for (k, v) in kv {
            e.insert((*k).to_string(), json!(v));
        }
        Account { account_id: id.into(), provider: "kiro".into(), max_concurrency: 1, disabled: false, created_at: 0, extra: e }
    }

    fn cfg() -> EnvNormalizeConfig {
        EnvNormalizeConfig { providers: vec!["claude-dario".into()], timezone: "Asia/Singapore".into(), locale: "en-US".into() }
    }

    /// 真实格式的打码样例(2026-10-06 报文目录)。
    const CC_ENV: &str = "# Environment\nYou have been invoked in the following environment: \n - Primary working directory: C:\\Users\\u\\proj\n - Is a git repository: true\n - Platform: win32\n - Shell: PowerShell (primary)\n - OS Version: Windows 11 Home China 10.0.22631\n - The current date is 2026-10-06";
    const OMO: &str = "<omo-env>\n  Timezone: Asia/Shanghai\n  Locale: zh-CN\n</omo-env>\nYou are powered by the model";
    const OPENCODE_ENV: &str = "<env>\n  Working directory: D:\\opencode\n  Is directory a git repo: yes\n  Platform: win32\n  Timezone: Asia/Shanghai\n  Today's date: Tue Oct 06 2026\n</env>";
    const CODEX: &str = "<environment_context>\n  <cwd>D:\\learning_project\\x</cwd>\n  <shell>powershell</shell>\n  <current_date>2026-10-06</current_date>\n  <timezone>Asia/Shanghai</timezone>\n</environment_context>";
    const GIT: &str = "gitStatus: This is the git status at the start of the conversation.\nCurrent branch: main\n\nGit user: 张三\n\nStatus:\nM README.md";
    const COPILOT: &str = "</outputFormatting>\nRespond in the following locale: zh-cn\n<memoryInstructions>";

    #[test]
    fn rewrites_each_known_structure_and_keeps_functional_fields() {
        let a = acct("acc-1", &[("env_persona", "p1")]);
        let mut body = json!({
            "system": [{"type": "text", "text": CC_ENV, "cache_control": {"type": "ephemeral"}}, {"type": "text", "text": format!("{OMO}\n{OPENCODE_ENV}\n{GIT}\n{COPILOT}")}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": CODEX}, {"type": "tool_result", "tool_use_id": "t", "content": "Timezone: Asia/Shanghai"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Timezone: Asia/Shanghai"}]},
                {"role": "user", "content": "OS Version: Windows 11 Home China 10.0.26200"},
            ],
        });
        let st = normalize_body_with(&mut body, &a, &cfg());
        let s0 = body["system"][0]["text"].as_str().unwrap();
        assert!(s0.contains(" - OS Version: Windows 11 Pro 10.0.26100"), "{s0}");
        assert!(s0.contains("Platform: win32") && s0.contains("Shell: PowerShell (primary)") && s0.contains("C:\\Users\\u\\proj"), "平台/Shell/目录保留");
        assert!(s0.contains("2026-10-06"), "日期保留");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral", "cache_control 不动");
        let s1 = body["system"][1]["text"].as_str().unwrap();
        assert!(s1.contains("  Timezone: Asia/Singapore\n  Locale: en-US"), "{s1}");
        assert!(s1.contains("Platform: win32\n  Timezone: Asia/Singapore\n  Today's date: Tue Oct 06 2026"), "{s1}");
        assert!(s1.contains("Git user: Alex Morgan"));
        assert!(s1.contains(&format!("</outputFormatting>\n{LANGUAGE_DIRECTIVE}\n<memoryInstructions>")), "{s1}");
        let m0 = body["messages"][0]["content"][0]["text"].as_str().unwrap();
        assert!(m0.contains("<timezone>Asia/Singapore</timezone>") && m0.contains("<shell>powershell</shell>"));
        assert_eq!(body["messages"][0]["content"][1]["content"], "Timezone: Asia/Shanghai", "tool_result 不动");
        assert_eq!(body["messages"][1]["content"][0]["text"], "Timezone: Asia/Shanghai", "assistant 不动");
        assert_eq!(body["messages"][2]["content"], "OS Version: Windows 11 Pro 10.0.26100", "历史里的 user 字符串内容同样处理");
        assert_eq!(st, EnvNormStats { os_version: 2, timezone: 3, locale: 1, locale_directive: 1, git_user: 1 });
        for needle in ["Asia/Shanghai", "zh-CN", "zh-cn", "Home China", "张三"] {
            let text = body["system"].to_string() + &body["messages"][0]["content"][0]["text"].to_string();
            assert!(!text.contains(needle), "{needle} 应已替换");
        }
    }

    #[test]
    fn idempotent_and_deterministic() {
        let a = acct("acc-2", &[]);
        let src = json!({"system": format!("{CC_ENV}\n{OMO}\n{GIT}\n{COPILOT}"), "messages": [{"role": "user", "content": CODEX}]});
        let mut once = src.clone();
        normalize_body_with(&mut once, &a, &cfg());
        let mut twice = once.clone();
        let st2 = normalize_body_with(&mut twice, &a, &cfg());
        assert_eq!(once, twice, "处理两次 = 处理一次");
        assert_eq!(st2.total(), 0);
        let mut again = src.clone();
        normalize_body_with(&mut again, &a, &cfg());
        assert_eq!(once, again, "同号同输入 → 同输出");
    }

    #[test]
    fn os_version_styles() {
        let a = acct("x", &[("env_persona", "p4")]);
        for (input, want) in [
            ("OS Version: win32 10.0.26200 x64", "OS Version: win32 10.0.19045 x64"),
            ("OS Version: win32 10.0.26200 arm64", "OS Version: win32 10.0.19045 arm64"),
            ("OS Version: Darwin 23.4.0", "OS Version: Darwin 23.6.0"),
            ("OS Version: Linux 6.8.0-134-generic", "OS Version: Linux 6.8.0-79-generic"),
            ("OS Version: FreeBSD 14.1", "OS Version: FreeBSD 14.1"),
        ] {
            let mut b = json!({"system": input});
            normalize_body_with(&mut b, &a, &cfg());
            assert_eq!(b["system"], want, "{input}");
        }
    }

    #[test]
    fn persona_is_stable_per_account_and_overridable() {
        let a = acct("same-account", &[]);
        assert_eq!(persona_for(&a), persona_for(&a));
        let names: std::collections::HashSet<_> = (0..40).map(|i| persona_for(&acct(&format!("acc-{i}"), &[])).name).collect();
        assert!(names.len() >= 4, "不同号分散到多套人设:{names:?}");
        assert_eq!(persona_for(&acct("same-account", &[("env_persona", "p5")])).name, "p5");
        assert_eq!(persona_for(&acct("same-account", &[("env_persona", "nope")])), persona_for(&a), "无效名回落哈希");
    }

    #[test]
    fn account_timezone_takes_precedence_when_valid() {
        let mut b = json!({"system": OMO});
        normalize_body_with(&mut b, &acct("x", &[("timezone", "America/New_York")]), &cfg());
        assert!(b["system"].as_str().unwrap().contains("Timezone: America/New_York"));
        let mut b = json!({"system": OMO});
        normalize_body_with(&mut b, &acct("x", &[("timezone", "not a tz")]), &cfg());
        assert!(b["system"].as_str().unwrap().contains("Timezone: Asia/Singapore"), "非法值回落全局");
    }

    #[test]
    fn untouched_when_nothing_matches() {
        let src = json!({"system": [{"type": "text", "text": "You are helpful."}], "messages": [{"role": "user", "content": [{"type": "image", "source": {}}, {"type": "text", "text": "hi"}]}]});
        let mut b = src.clone();
        assert_eq!(normalize_body_with(&mut b, &acct("x", &[]), &cfg()).total(), 0);
        assert_eq!(b, src);
    }

    #[test]
    fn config_validation_and_hot_apply() {
        assert!(validate(&cfg()).is_ok());
        assert!(validate(&EnvNormalizeConfig { providers: vec!["openai".into()], ..cfg() }).is_err());
        assert!(validate(&EnvNormalizeConfig { timezone: "Shanghai".into(), ..cfg() }).is_err());
        assert!(validate(&EnvNormalizeConfig { locale: "english".into(), ..cfg() }).is_err());
        assert!(valid_timezone("UTC") && valid_timezone("America/Argentina/Buenos_Aires") && valid_timezone("Etc/GMT+8"));
        assert!(valid_locale("en") && valid_locale("en-US") && !valid_locale("en_us"));
    }
}
