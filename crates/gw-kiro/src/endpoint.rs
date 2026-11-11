//! 主推理上游端点选择 —— 按账号决定「发往哪个域名、用哪种报文形态」。
//!
//! 背景(2026-11-06 实测):降级后的 FREE 号走 `runtime.{region}.kiro.dev` 的 REST 形态
//! 拿不到 opus(400 `INVALID_MODEL_ID`),换成老端点 `q.{region}.amazonaws.com` 的 **REST**
//! 形态同样拿不到;但老端点的 **RPC 形态**(`POST /` + `x-amz-target:
//! AmazonCodeWhispererStreamingService.GenerateAssistantResponse` +
//! `content-type: application/x-amz-json-1.0`)对 opus-5.5 返回 200。放行条件是
//! 「域名 + 报文形态」这一对,与 UA 版本、body 内容都无关(jennifer / ambra / Poon / g4n1h45j
//! 四个号 BuilderID 与 Google 两种登录都实测通过;body 用 caio 原样含顶层 `agentMode`)。
//! 响应仍是 eventstream,解析器与错误分类(按状态码 + 报文子串)都不依赖请求形态。
//!
//! 账号级字段 `extra.kiro_endpoint`(热改,下一次请求生效):
//!
//! | 取值 | 域名 | 报文形态 |
//! |---|---|---|
//! | 缺失 / 空串 / 认不出 | 跟全局 `q_endpoint` 开关(默认 runtime) | REST |
//! | `runtime` | runtime.{region}.kiro.dev | REST |
//! | `q-rest` | q.{region}.amazonaws.com | REST(等同全局 q_endpoint 开) |
//! | `q` | q.{region}.amazonaws.com | **RPC(老端点,本次给降级号用)** |
//! | `responses` | runtime.{region}.kiro.dev | **`POST /v1/responses`(Kiro IDE 1.4.121 的 CreateResponse)** |
//!
//! `responses`(2026-11-09):1.4.121 起官方客户端被服务端实验组分到 `create_response` 时,
//! 主对话改走 OpenAI Responses 形态的 `/v1/responses`(请求体/响应 SSE 都不同,见
//! [`crate::responses`] / [`crate::stream`])。真客户端是**按账号**被分组的,所以这里也按账号开,
//! 只模拟 IDE(`origin: AI_EDITOR`、UA 带 machineId),不模拟 CLI —— 我们的号都是 IDE 登录签发的
//! token,冒充 CLI 会让同一个号刷新像 IDE、对话像 CLI。该号的刷新/查额度也随之切到 1.4.121 形态
//! (见 [`crate::headers::kiro_version`])。
//!
//! 优先级:env 整串覆盖 > 账号 `kiro_endpoint` > 全局 `q_endpoint` > 默认 runtime。
//! **env 整串覆盖(`KIRO_RUNTIME_UPSTREAM_BASE_URL` / `KIRO_UPSTREAM_BASE_URL`)存在时,
//! 域名与报文形态都按改动前的旧行为走(REST),账号字段不再生效**:它是给本地 mock /
//! 自建代理留的「显式全 URL」后门,对端只会讲 REST,不能被账号字段悄悄改成 RPC。
//! 认不出的取值**不报错、不拒发**:按「缺失」处理,并对每个(账号, 取值)告警一次
//! (fail-safe,手滑写错不会把号打死,只是不生效;告警带账号 ID 便于对出是哪个号)。
//!
//! 区域:`q.{region}.amazonaws.com` 只在 **us-east-1** 实测过,其它区域的 q 域名未验证
//! (Kiro 账号现实中只有 us-east-1 / eu-central-1)。
//!
//! 请求头顺序:RPC 形态里 `content-type` 紧跟 `x-amz-target`,是 AWS SDK awsJson1_0
//! 序列化的原生顺序;REST 形态与改动前完全一致。

use gw_core::account::Account;

/// 账号级端点字段名。
pub const ENDPOINT_KEY: &str = "kiro_endpoint";

/// 账号字段可写的取值(小写规范形)。管理面写侧校验与本模块读侧解析共用这一份,
/// 新增取值只改这里和 [`parse_choice`],测试会检查两边对得上。
pub const ENDPOINT_CHOICES: [&str; 4] = ["runtime", "q-rest", "q", "responses"];

/// RPC 形态的 `x-amz-target`(老 CodeWhisperer 流式服务)。
const STREAMING_TARGET: &str = "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";

/// 报文形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wire {
    /// `POST /generateAssistantResponse` + `application/json`(runtime 现行形态)。
    Rest,
    /// `POST /` + `application/x-amz-json-1.0` + `x-amz-target`(老端点 RPC 形态)。
    AwsJson10,
    /// `POST /v1/responses` + `application/json`,OpenAI Responses 形态的请求与 SSE 响应
    /// (Kiro IDE 1.4.121 CreateResponse)。
    Responses,
}

/// 一次主推理请求的落点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    /// 形如 `https://runtime.us-east-1.kiro.dev`(无尾斜杠)。
    pub base_url: String,
    pub wire: Wire,
}

impl Endpoint {
    /// 请求 URL。
    pub(crate) fn url(&self) -> String {
        match self.wire {
            Wire::Rest => format!("{}/generateAssistantResponse", self.base_url),
            Wire::AwsJson10 => format!("{}/", self.base_url),
            Wire::Responses => format!("{}/v1/responses", self.base_url),
        }
    }

    pub(crate) fn content_type(&self) -> &'static str {
        match self.wire {
            Wire::Rest | Wire::Responses => "application/json",
            Wire::AwsJson10 => "application/x-amz-json-1.0",
        }
    }

    /// RPC 形态才带 `x-amz-target`。
    pub(crate) fn amz_target(&self) -> Option<&'static str> {
        match self.wire {
            Wire::Rest | Wire::Responses => None,
            Wire::AwsJson10 => Some(STREAMING_TARGET),
        }
    }

    /// REST 形态直指给定 base(测试与简单场景用)。
    #[cfg(test)]
    pub(crate) fn rest(base_url: &str) -> Self {
        Self {
            base_url: base_url.to_string(),
            wire: Wire::Rest,
        }
    }
}

/// 账号字段的解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    /// 缺失 / 空 / 认不出:跟全局开关。
    Inherit,
    Runtime,
    QRest,
    Q,
    Responses,
}

fn parse_choice(raw: Option<&str>) -> (Choice, bool) {
    let Some(v) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return (Choice::Inherit, true);
    };
    match v.to_ascii_lowercase().as_str() {
        "runtime" => (Choice::Runtime, true),
        "q-rest" => (Choice::QRest, true),
        "q" => (Choice::Q, true),
        "responses" => (Choice::Responses, true),
        _ => (Choice::Inherit, false),
    }
}

/// 按账号 + 全局开关 + env 覆盖决定本次落点(热路径,每请求一次)。
pub(crate) fn resolve(account: &Account, region: &str) -> Endpoint {
    resolve_with(
        account,
        region,
        crate::headers::env_base_override(),
        crate::converter::q_endpoint_enabled(),
    )
}

/// 与 [`resolve`] 相同,但把 env 覆盖与全局开关显式传入(测试不碰进程全局状态)。
pub(crate) fn resolve_with(
    account: &Account,
    region: &str,
    env_override: Option<String>,
    global_q: bool,
) -> Endpoint {
    let raw = account.extra_str(ENDPOINT_KEY);
    let (choice, recognised) = parse_choice(raw);
    let endpoint = build(choice, region, env_override, global_q);
    if !recognised {
        if let Some(v) = raw {
            warn_unknown_once(&account.account_id, v);
        }
    } else if choice == Choice::Q && endpoint.wire == Wire::AwsJson10 {
        note_legacy_once(&account.account_id);
    } else if choice == Choice::Responses && endpoint.wire == Wire::Responses {
        note_responses_once(&account.account_id);
    }
    endpoint
}

/// 纯逻辑:已解析的选择 + 三个外部输入 → 落点。
fn build(choice: Choice, region: &str, env_override: Option<String>, global_q: bool) -> Endpoint {
    // env 整串覆盖 = 旧行为的后门:域名取覆盖值,形态固定 REST,账号字段不参与。
    if let Some(base_url) = env_override {
        return Endpoint {
            base_url,
            wire: Wire::Rest,
        };
    }
    let use_q = match choice {
        Choice::Runtime | Choice::Responses => false,
        Choice::QRest | Choice::Q => true,
        Choice::Inherit => global_q,
    };
    let wire = match choice {
        Choice::Q => Wire::AwsJson10,
        Choice::Responses => Wire::Responses,
        _ => Wire::Rest,
    };
    let base_url = if use_q {
        format!("https://q.{region}.amazonaws.com")
    } else {
        format!("https://runtime.{region}.kiro.dev")
    };
    Endpoint { base_url, wire }
}

/// 每个账号第一次走老端点时记一条 info,便于上线后确认配置真的生效(不逐请求刷屏)。
fn note_legacy_once(account_id: &str) {
    if first_seen(&LEGACY_SEEN, account_id.to_string(), LEGACY_SEEN_CAP) {
        tracing::info!(account = %account_id, "账号按 extra.kiro_endpoint=q 走老端点(q 域名 + RPC 形态)");
    }
}

/// 同上,记录走 `/v1/responses` 的号。与老端点共用去重集合(键加前缀区分)。
fn note_responses_once(account_id: &str) {
    if first_seen(&LEGACY_SEEN, format!("responses\u{0}{account_id}"), LEGACY_SEEN_CAP) {
        tracing::info!(account = %account_id, "账号按 extra.kiro_endpoint=responses 走 /v1/responses(IDE 1.4.121 形态)");
    }
}

/// 账号是否走 `/v1/responses`。刷新 / 查额度这些不经 [`resolve`] 的路径用它决定是否切到
/// 1.4.121 形态,所以必须与 [`resolve`] 同口径:env 整串覆盖存在时 [`resolve`] 退回 REST,
/// 这里也返回 false —— 否则会出现「对话走 REST、UA 却自报 1.4.121」的混搭。
pub(crate) fn account_uses_responses(account: &Account) -> bool {
    account_uses_responses_with(account, crate::headers::env_base_override().is_some())
}

fn account_uses_responses_with(account: &Account, env_override: bool) -> bool {
    !env_override && parse_choice(account.extra_str(ENDPOINT_KEY)).0 == Choice::Responses
}

fn warn_unknown_once(account_id: &str, value: &str) {
    if first_seen(
        &UNKNOWN_SEEN,
        format!("{account_id}\u{0}{value}"),
        UNKNOWN_SEEN_CAP,
    ) {
        tracing::warn!(
            account = %account_id,
            value,
            "extra.kiro_endpoint 取值认不出(可用 runtime / q-rest / q / responses),已按缺失处理(跟全局 q_endpoint 开关,即该号并没有走老端点)"
        );
    }
}

/// 去重集合的容量上限:`extra` 由 admin / 导入写入,取值与账号数都不可信,不能让
/// 「只记一次」的备忘集合随垃圾输入无界增长。满了就不再记录(宁可少打一条日志)。
const LEGACY_SEEN_CAP: usize = 4096;
const UNKNOWN_SEEN_CAP: usize = 64;

type SeenSet = std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>>;
static LEGACY_SEEN: SeenSet = std::sync::OnceLock::new();
static UNKNOWN_SEEN: SeenSet = std::sync::OnceLock::new();

/// 集合里没有且未满则插入并返回 true。先 `contains` 再 `insert`:热路径上绝大多数调用
/// 是「已见过」,不该为此分配 String 的键。告警路径不该因为一次 poison 就哑掉。
fn first_seen(cell: &'static SeenSet, key: String, cap: usize) -> bool {
    let set = cell.get_or_init(Default::default);
    let mut g = match set.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if g.contains(&key) || g.len() >= cap {
        return false;
    }
    g.insert(key)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_writable_choice_is_recognised_by_reader() {
        for v in super::ENDPOINT_CHOICES {
            let (_, ok) = super::parse_choice(Some(v));
            assert!(ok, "{v} 写侧收下了,读侧却不认");
        }
    }

    use super::*;
    use serde_json::json;

    fn account_with(extra: serde_json::Value) -> Account {
        let mut acc = Account {
            account_id: "t@example.com".into(),
            provider: "kiro".into(),
            max_concurrency: 1,
            disabled: false,
            created_at: 0,
            extra: Default::default(),
        };
        if let serde_json::Value::Object(map) = extra {
            for (k, v) in map {
                acc.extra.insert(k, v);
            }
        }
        acc
    }

    /// 测试入口:只给原始取值串,env / 全局开关显式传入。
    fn resolve_from(
        raw: Option<&str>,
        region: &str,
        env_override: Option<String>,
        global_q: bool,
    ) -> Endpoint {
        build(parse_choice(raw).0, region, env_override, global_q)
    }

    const R: &str = "https://runtime.us-east-1.kiro.dev";
    const Q: &str = "https://q.us-east-1.amazonaws.com";

    #[test]
    fn missing_field_follows_global_switch() {
        // 缺失 = 现状:全局关 → runtime REST,与改动前逐字节一致。
        let e = resolve_from(None, "us-east-1", None, false);
        assert_eq!(e, Endpoint::rest(R));
        assert_eq!(e.url(), format!("{R}/generateAssistantResponse"));
        assert_eq!(e.content_type(), "application/json");
        assert_eq!(e.amz_target(), None);
        // 全局开 → q REST(原有 q_endpoint 行为不变)。
        assert_eq!(
            resolve_from(None, "us-east-1", None, true),
            Endpoint::rest(Q)
        );
        // 空串 / 纯空白 同缺失。
        assert_eq!(
            resolve_from(Some(""), "us-east-1", None, false),
            Endpoint::rest(R)
        );
        assert_eq!(
            resolve_from(Some("  "), "us-east-1", None, true),
            Endpoint::rest(Q)
        );
    }

    #[test]
    fn account_runtime_beats_global_q() {
        // 号显式钉 runtime:即便全局开了 q 也走 runtime。
        assert_eq!(
            resolve_from(Some("runtime"), "us-east-1", None, true),
            Endpoint::rest(R)
        );
    }

    #[test]
    fn q_rest_is_q_domain_with_rest_wire() {
        let e = resolve_from(Some("q-rest"), "us-east-1", None, false);
        assert_eq!(e, Endpoint::rest(Q));
        assert_eq!(e.url(), format!("{Q}/generateAssistantResponse"));
    }

    #[test]
    fn q_is_legacy_rpc_form() {
        let e = resolve_from(Some("q"), "us-east-1", None, false);
        assert_eq!(e.base_url, Q);
        assert_eq!(e.wire, Wire::AwsJson10);
        // RPC 形态:路径是 `/`,不是 /generateAssistantResponse。
        assert_eq!(e.url(), format!("{Q}/"));
        assert_eq!(e.content_type(), "application/x-amz-json-1.0");
        assert_eq!(
            e.amz_target(),
            Some("AmazonCodeWhispererStreamingService.GenerateAssistantResponse")
        );
        // 区域跟随账号,不写死 us-east-1。
        let eu = resolve_from(Some("q"), "eu-central-1", None, false);
        assert_eq!(eu.base_url, "https://q.eu-central-1.amazonaws.com");
        // 与全局开关无关:全局开着也一样。
        assert_eq!(resolve_from(Some("q"), "us-east-1", None, true), e);
    }

    #[test]
    fn responses_is_runtime_domain_with_responses_wire() {
        let e = resolve_from(Some("responses"), "us-east-1", None, false);
        assert_eq!(e.base_url, R);
        assert_eq!(e.wire, Wire::Responses);
        assert_eq!(e.url(), format!("{R}/v1/responses"));
        assert_eq!(e.content_type(), "application/json");
        assert_eq!(e.amz_target(), None);
        // 全局 q 开着也不改域名(1.4.121 的 /v1/responses 只在 runtime 域名上)。
        assert_eq!(resolve_from(Some("responses"), "us-east-1", None, true), e);
        let eu = resolve_from(Some(" Responses "), "eu-central-1", None, false);
        assert_eq!(eu.url(), "https://runtime.eu-central-1.kiro.dev/v1/responses");
        assert!(account_uses_responses(&account_with(json!({"kiro_endpoint": "responses"}))));
        assert!(!account_uses_responses(&account_with(json!({"kiro_endpoint": "q"}))));
        assert!(!account_uses_responses(&account_with(json!({}))));
        assert!(!account_uses_responses_with(
            &account_with(json!({"kiro_endpoint": "responses"})),
            true
        ), "env 覆盖时与 resolve 同口径,不算走新协议");
    }

    #[test]
    fn value_is_case_and_space_insensitive() {
        for v in ["Q", " q ", "q\n"] {
            assert_eq!(
                resolve_from(Some(v), "us-east-1", None, false).wire,
                Wire::AwsJson10,
                "{v:?} 应识别为 q"
            );
        }
        assert_eq!(
            resolve_from(Some("Q-REST"), "us-east-1", None, false),
            Endpoint::rest(Q)
        );
    }

    #[test]
    fn unknown_value_falls_back_to_inherit_not_to_legacy() {
        // 手滑写错(多了字母 / 写成别的名)不得意外变成老端点,也不得拒发。
        for v in ["qq", "legacy", "old", "true", "1"] {
            assert_eq!(parse_choice(Some(v)), (Choice::Inherit, false), "{v}");
            assert_eq!(
                resolve_from(Some(v), "us-east-1", None, false),
                Endpoint::rest(R)
            );
            assert_eq!(
                resolve_from(Some(v), "us-east-1", None, true),
                Endpoint::rest(Q)
            );
        }
    }

    #[test]
    fn env_override_restores_old_behaviour_for_every_account_value() {
        // env 整串覆盖 = 旧行为后门:域名取覆盖值、形态固定 REST,账号字段(含 q)不得把它改成 RPC。
        let custom = "https://custom.example.com";
        for v in [
            None,
            Some("runtime"),
            Some("q-rest"),
            Some("q"),
            Some("responses"),
            Some("junk"),
        ] {
            for global_q in [false, true] {
                let e = resolve_from(v, "us-east-1", Some(custom.to_string()), global_q);
                assert_eq!(e, Endpoint::rest(custom), "v={v:?} global_q={global_q}");
                assert_eq!(e.url(), format!("{custom}/generateAssistantResponse"));
            }
        }
    }

    #[test]
    fn resolve_with_reads_account_extra() {
        // 走真实账号读取路径,env / 全局开关显式给定(不依赖进程全局状态,并行测试不互相污染)。
        let on = resolve_with(
            &account_with(json!({"kiro_endpoint": "q"})),
            "us-east-1",
            None,
            false,
        );
        assert_eq!(on.wire, Wire::AwsJson10);
        assert_eq!(on.base_url, Q);
        let off = resolve_with(&account_with(json!({})), "us-east-1", None, false);
        assert_eq!(off, Endpoint::rest(R));
        // 非字符串类型(布尔 / 数字)读不到字符串 → 按缺失。
        let bad = resolve_with(
            &account_with(json!({"kiro_endpoint": true})),
            "us-east-1",
            None,
            false,
        );
        assert_eq!(bad, Endpoint::rest(R));
        // env 覆盖存在时,账号写了 q 也是 REST(见 env_override_restores_old_behaviour…)。
        let ovr = resolve_with(
            &account_with(json!({"kiro_endpoint": "q"})),
            "us-east-1",
            Some("https://mock.local".into()),
            false,
        );
        assert_eq!(ovr.wire, Wire::Rest);
    }

    #[test]
    fn seen_sets_are_bounded_and_deduplicated() {
        static CELL: SeenSet = std::sync::OnceLock::new();
        assert!(first_seen(&CELL, "a".into(), 2), "首次见到 → true");
        assert!(!first_seen(&CELL, "a".into(), 2), "重复 → false");
        assert!(first_seen(&CELL, "b".into(), 2));
        // 容量满了:新键不再记录(也不再触发日志),旧键仍视为已见过。
        assert!(!first_seen(&CELL, "c".into(), 2), "满额后不再收录");
        assert!(!first_seen(&CELL, "a".into(), 2));
    }
}
