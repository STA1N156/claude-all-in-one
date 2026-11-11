//! Kiro 上游请求头构造 —— 逐字节对齐 static_flow(最新生产客户端形态)。
//!
//! 金标准来源:`_refs/static_flow/crates/llm-access/src/kiro_headers.rs` +
//! `provider/kiro_protocol.rs` + `kiro_refresh.rs`(commit 9051d71)。封号根因点在
//! UA 里嵌的 machineId 与端点指纹,这里集中管理,改动需对照 static_flow。
//!
//! 主推理 `generateAssistantResponse`(落点按账号选,见 [`crate::endpoint`]):
//! - 默认:`POST https://runtime.{region}.kiro.dev/generateAssistantResponse`,
//!   `content-type: application/json`;
//! - 全局 `q_endpoint` 开 / 账号 `kiro_endpoint=q-rest`:同形态打 `q.{region}.amazonaws.com`
//!   (kiro.rs 端点、做服务端 prompt 缓存);
//! - 账号 `kiro_endpoint=q`:老端点 **RPC 形态** —— `POST https://q.{region}.amazonaws.com/` +
//!   `content-type: application/x-amz-json-1.0` + `x-amz-target:
//!   AmazonCodeWhispererStreamingService.GenerateAssistantResponse`(其余头不变);
//! - env `KIRO_RUNTIME_UPSTREAM_BASE_URL` / `KIRO_UPSTREAM_BASE_URL` 整串覆盖优先(恒 REST)。
//! - UA `aws-sdk-js/1.0.34 ua/2.1 os/darwin#24.6.0 lang/js md/nodejs#22.22.0
//!   api/codewhispererstreaming#1.0.34 KiroIDE-{ver}-{machine}`(**无 `m/E`**)
//! - 条件头:`TokenType: EXTERNAL_IDP`(auth_method=external_idp)、
//!   `redirect-for-internal: true`(provider=internal)。

use gw_core::account::Account;

/// AWS SDK 版本(主推理流)。对齐 static_flow `KIRO_PROVIDER_AWS_SDK_VERSION`。
pub(crate) const AWS_SDK_VERSION: &str = "1.0.34";
/// 默认 Kiro 客户端版本(可被 account.extra["kiro_version"] 覆盖)。
///
/// 2026-07-28 对齐真实客户端:deb `kiro 1.0.212-1784842874`,`product.json` version=1.0.212
/// (commit 8848ae36,build 2026-07-23)。UA 拼法未变,仍是
/// `KiroIDE-${kiroVersion}-${machineId}`(`extension.js:374150`)。
pub(crate) const CURRENT_KIRO_VERSION: &str = "1.0.212";
/// 2026-07-28 之前的旧默认版本。`KIRO_LEGACY_WIRE=1`(见 wire_profile)时 UA 回到它,
/// 与旧报文形态配套 —— 版本号和报文形态必须同属一个时代,混搭是最差形态。
pub(crate) const LEGACY_KIRO_VERSION: &str = "0.12.155";
/// UA 里的系统/Node 版本,逐字对齐 static_flow `DEFAULT_SYSTEM_VERSION`/`DEFAULT_NODE_VERSION`。
pub(crate) const DEFAULT_SYSTEM_VERSION: &str = "darwin#24.6.0";
pub(crate) const DEFAULT_NODE_VERSION: &str = "22.22.0";

/// `extra.kiro_endpoint=responses` 的号自报的 Kiro IDE 版本(2026-11-08 拆包/抓包的 1.4.121,
/// product commit c79656137d)。该号的刷新、查额度等全部 UA 都跟着它,避免「对话是 1.4.121、
/// 刷新还是旧版本」的同号混搭。
pub(crate) const RESPONSES_KIRO_VERSION: &str = "1.4.121";
/// 1.4.121 内置 agent(KAS)版本,出现在 `/v1/responses` 的 UA 里。
pub(crate) const RESPONSES_KAS_VERSION: &str = "0.66.89";
/// 1.4.121 自带的 Node 版本(Electron 内置,与操作系统无关;抓包 `md/nodejs#24.22.1`)。
/// 版本号是跟客户端版本绑死的:自报 1.4.121 却带 22.22.0 是现实里不存在的组合。
pub(crate) const RESPONSES_NODE_VERSION: &str = "24.22.1";

/// UA 里的 Node 版本跟自报的 Kiro 版本走:1.4.121 → 24.22.1,其余保持原值(逐字节不变)。
pub(crate) fn node_version_for(version: &str) -> &'static str {
    if version == RESPONSES_KIRO_VERSION {
        RESPONSES_NODE_VERSION
    } else {
        DEFAULT_NODE_VERSION
    }
}

/// IdC(AWS SSO OIDC)刷新用的 AWS SDK 版本。对齐 static_flow `KIRO_IDC_AWS_SDK_VERSION`。
pub(crate) const IDC_AWS_SDK_VERSION: &str = "3.980.0";

/// Social(GitHub/Google)免费层共享 profileArn。对齐 static_flow `KIRO_SOCIAL_SIGN_IN_PROFILE_ARN`。
pub(crate) const SOCIAL_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK";
/// BuilderId 免费层共享 profileArn。对齐 static_flow `KIRO_BUILDER_ID_PROFILE_ARN`。
pub(crate) const BUILDER_ID_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";

const RUNTIME_BASE_ENV: &str = "KIRO_RUNTIME_UPSTREAM_BASE_URL";
const UPSTREAM_BASE_ENV: &str = "KIRO_UPSTREAM_BASE_URL";

/// 主推理上游 base url 的 env 整串覆盖(`KIRO_RUNTIME_UPSTREAM_BASE_URL` /
/// `KIRO_UPSTREAM_BASE_URL`,含 region,对齐 static_flow `configured_upstream_base_url`)。
/// 给显式全 URL 场景留后门,优先级最高;域名/报文形态的完整选择逻辑见 [`crate::endpoint`]。
pub(crate) fn env_base_override() -> Option<String> {
    read_base_env(RUNTIME_BASE_ENV).or_else(|| read_base_env(UPSTREAM_BASE_ENV))
}

fn read_base_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
}

/// 从 base url 提 host 头(含端口)。对齐 static_flow `upstream_host_header`。
/// 解析失败时退回原串(调用方已保证是合法 URL)。
pub(crate) fn host_header(base_url: &str) -> String {
    match reqwest::Url::parse(base_url) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            (None, _) => base_url.to_string(),
        },
        Err(_) => base_url.to_string(),
    }
}

/// 主推理 UA 对(x-amz-user-agent, user-agent)。**主流 UA 无 `m/E`**(对齐 static_flow)。
pub(crate) fn streaming_user_agents(version: &str, machine_id: &str) -> (String, String) {
    let x_amz = format!("aws-sdk-js/{AWS_SDK_VERSION} KiroIDE-{version}-{machine_id}");
    let node = node_version_for(version);
    let ua = format!(
        "aws-sdk-js/{AWS_SDK_VERSION} ua/2.1 os/{DEFAULT_SYSTEM_VERSION} lang/js \
         md/nodejs#{node} api/codewhispererstreaming#{AWS_SDK_VERSION} \
         KiroIDE-{version}-{machine_id}"
    );
    (x_amz, ua)
}

/// IdC 刷新 UA 对(x-amz-user-agent, user-agent),逐字对齐 static_flow:
/// - x-amz 带版本 `KiroIDE-{ver}`;
/// - user-agent **无** `api/sso-oidc`、**无**尾部 `KiroIDE`,带 `m/E`。
pub(crate) fn idc_refresh_user_agents(version: &str) -> (String, String) {
    let x_amz = format!("aws-sdk-js/{IDC_AWS_SDK_VERSION} KiroIDE-{version}");
    let node = node_version_for(version);
    let ua = format!(
        "aws-sdk-js/{IDC_AWS_SDK_VERSION} ua/2.1 os/{DEFAULT_SYSTEM_VERSION} lang/js \
         md/nodejs#{node} m/E"
    );
    (x_amz, ua)
}

/// 账号的有效 Kiro 客户端版本(extra 覆盖 > 当前形态默认)。
///
/// ⚠️ **覆盖只改 UA 里自报的版本号,不会把报文形态一起改回去。** body 形态、
/// 配额端点等都跟着 `KIRO_LEGACY_WIRE` 总开关走(见 wire_profile)。把单个账号
/// 钉到与总开关不一致的版本,得到的是「自称 A 版本、行为却是 B 版本」的混搭 ——
/// 现实中不存在的组合,比不改更容易被识别。要回旧形态就用总开关整体回。
///
/// 保留这个字段是留一条将来支持多形态时的口子;在那之前,不等于生效默认版本就告警一次。
/// (2026-07-28 生产实测 222 个账号**无一**设置该字段,故此处只警示不改行为。)
pub(crate) fn kiro_version(account: &Account) -> String {
    // 走 /v1/responses 的号整号按 1.4.121 自报(报文形态本身就是 1.4.121 的,不算混搭)。
    let default = if crate::endpoint::account_uses_responses(account) {
        RESPONSES_KIRO_VERSION
    } else {
        default_kiro_version()
    };
    match account.extra_str("kiro_version").filter(|v| !v.is_empty()) {
        Some(v) if v != default => {
            warn_version_mismatch(v, default);
            v.to_string()
        }
        Some(v) => v.to_string(),
        None => default.to_string(),
    }
}

/// 当前生效的默认版本:legacy 总开关开 → 旧版本,否则现行版本。
pub(crate) fn default_kiro_version() -> &'static str {
    default_version_for(crate::wire_profile::legacy_wire())
}

/// 纯逻辑(开关注入便于测试;读 env 的测试会与并行用例互相污染)。
fn default_version_for(legacy: bool) -> &'static str {
    if legacy {
        LEGACY_KIRO_VERSION
    } else {
        CURRENT_KIRO_VERSION
    }
}

/// 每个偏离版本只吼一次,避免逐请求刷屏。
fn warn_version_mismatch(v: &str, wire_default: &'static str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let mut g = match seen.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(), // 告警路径不该因为一次 poison 就哑掉
    };
    if g.insert(v.to_string()) {
        tracing::warn!(
            account_kiro_version = v,
            wire_shape_version = wire_default,
            "账号钉了非默认 kiro_version：UA 自报该版本，但报文形态与端点仍是默认版本的，\
             这是现实中不存在的组合。除非确知在做什么，否则请清掉该字段。"
        );
    }
}

/// 把主推理金标准头加到请求上(顺序对齐 static_flow `add_kiro_headers`)。
/// `access_token` / `machine_id` / `version` 由调用方算好传入。
pub(crate) fn apply_streaming_headers(
    rb: reqwest::RequestBuilder,
    account: &Account,
    endpoint: &crate::endpoint::Endpoint,
    access_token: &str,
    machine_id: &str,
    version: &str,
) -> reqwest::RequestBuilder {
    let (x_amz_ua, ua) = streaming_user_agents(version, machine_id);
    // content-type 与 x-amz-target 随报文形态走(REST:application/json、无 target;
    // 老端点 RPC:application/x-amz-json-1.0 + target)。顺序与实测跑通的报文一致。
    let mut rb = rb.header("content-type", endpoint.content_type());
    if let Some(target) = endpoint.amz_target() {
        rb = rb.header("x-amz-target", target);
    }
    let mut rb = rb
        .header("accept", "application/vnd.amazon.eventstream")
        .header("x-amzn-kiro-agent-mode", "vibe")
        .header("x-amzn-codewhisperer-optout", "true");
    // 条件头(顺序对齐 static_flow:在 UA 之前)。
    // API Key 凭据:`TokenType: API_KEY` —— 官方 Kiro CLI headless 模式的真实客户端形态,
    // **服务端强制要求**(实测:缺此头则 ksk_ 被当普通 OAuth token,报 400「profileArn is
    // required」;带此头则免 profileArn、直接放行)。api_key 与 external_idp 互斥。
    if crate::machine_id::is_api_key_credential(account) {
        rb = rb.header("TokenType", "API_KEY");
    } else if is_external_idp(account) {
        rb = rb.header("TokenType", "EXTERNAL_IDP");
    }
    if is_internal_provider(account) {
        rb = rb.header("redirect-for-internal", "true");
    }
    rb.header("x-amz-user-agent", x_amz_ua)
        .header("user-agent", ua)
        .header("host", host_header(&endpoint.base_url))
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=3")
        .header("authorization", format!("Bearer {access_token}"))
}

/// `/v1/responses` 的 UA(Kiro IDE 1.4.121 CreateResponse 原生 HTTPS 请求,不走 AWS SDK,
/// 所以没有 `x-amz-user-agent`;版本与 machineId 之间是**空格**,与 SDK 的 `-` 拼法不同)。
pub(crate) fn responses_user_agent(machine_id: &str) -> String {
    format!(
        "KiroIDE {RESPONSES_KIRO_VERSION} {machine_id} KAS/{RESPONSES_KAS_VERSION} acp-client/kiro-ide"
    )
}

/// `/v1/responses` 请求头,集合与顺序对齐 1.4.121 的 `MUs`(extension.js@5710416)与抓包
/// `wire-capture-pro-claude-1.4.121.jsonl`:
/// `Content-Type, Authorization, x-amzn-kiro-origin, x-amzn-codewhisperer-optout,
/// [x-amzn-kiro-profile], [TokenType], User-Agent, x-amzn-kiro-attempt, x-amzn-kiro-fallback,
/// (Content-Length), Host, Connection`。
///
/// - `optout`:真客户端按「内容收集」设置取值(抓包那台开着 → `false`)。这里沿用本号在旧协议上
///   一直发的 `true` —— 同一个用户升级客户端不会顺手改隐私设置,同号前后一致比跟某一台抓包机一致重要。
/// - `TokenType`:与旧协议同一判定(API Key / 外部 IdP 才带;社交号不带,抓包一致)。
/// - 没有 `accept`、`x-amzn-kiro-agent-mode`、`amz-sdk-*`(都是 AWS SDK 路径才有的)。
pub(crate) fn apply_responses_headers(
    rb: reqwest::RequestBuilder,
    account: &Account,
    endpoint: &crate::endpoint::Endpoint,
    access_token: &str,
    machine_id: &str,
    profile_arn: Option<&str>,
) -> reqwest::RequestBuilder {
    let mut rb = rb
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {access_token}"))
        .header("x-amzn-kiro-origin", "AI_EDITOR")
        .header("x-amzn-codewhisperer-optout", "true");
    if let Some(arn) = profile_arn {
        rb = rb.header("x-amzn-kiro-profile", arn);
    }
    if crate::machine_id::is_api_key_credential(account) {
        rb = rb.header("TokenType", "API_KEY");
    } else if is_external_idp(account) {
        rb = rb.header("TokenType", "EXTERNAL_IDP");
    }
    let mut rb = rb
        .header("user-agent", responses_user_agent(machine_id))
        .header("x-amzn-kiro-attempt", "1;max=3")
        .header("x-amzn-kiro-fallback", "0;max=0");
    if is_internal_provider(account) {
        rb = rb.header("redirect-for-internal", "true");
    }
    rb.header("host", host_header(&endpoint.base_url))
        .header("connection", "close")
}

/// 条件头判定:auth_method == "external_idp"(大小写不敏感)。
///
/// `pub(crate)`:token.rs 的刷新分流复用同一判定,避免两处各写一份 easily-diverging
/// 的字符串比较(此前只有本文件内部用,token.rs 的刷新分流当时压根没读 auth_method)。
pub(crate) fn is_external_idp(account: &Account) -> bool {
    account
        .extra_str("auth_method")
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("external_idp"))
}

/// external_idp(Azure AD 企业 SSO)账号:给请求补 `TokenType: EXTERNAL_IDP` 头,否则
/// 保持原样。**所有** Kiro API 调用(不止 chat)对 external_idp 号都必须带此头——
/// CodeWhisperer 靠它识别令牌类型并按外部 IdP 校验 Azure JWT,缺了它会静默返回空
/// profile 列表并拒绝数据面调用(getUsageLimits/ListAvailableProfiles 得到 403)。
/// 对齐 Kiro-Go `applyKiroBaseHeaders`(其对每次 Kiro 请求统一注入)。streaming 头
/// 因需保持 static_flow 的严格字段顺序仍内联注入(见 `apply_streaming_headers`);
/// 顺序不敏感的辅助只读调用(配额/profile 发现)复用此助手,避免判定散落多处。
///
/// 位置:调用方在基础头(含 authorization)之后追加本头——与 Kiro-Go
/// `applyKiroBaseHeaders` 一致(那里 TokenType 也排在 Authorization/UA/optout 之后、
/// 位于末尾),故非"随手放末尾"的臆测,而是与参考实现的相对顺序对齐。这两条是 AWS
/// runtime REST 调用(getUsageLimits/ListAvailableProfiles),服务端按 header map 解析,
/// 不做 data-plane 那种逐字节指纹校验,顺序不影响识别。
pub(crate) fn apply_external_idp_token_type(
    rb: reqwest::RequestBuilder,
    account: &Account,
) -> reqwest::RequestBuilder {
    if is_external_idp(account) {
        rb.header("TokenType", "EXTERNAL_IDP")
    } else {
        rb
    }
}

/// Kiro 身份 provider(github/google/builderid/internal...)。注意:**不能**用
/// `Account.provider`(那是适配器家族 "kiro")或 extra["provider"](与顶层字段撞名,
/// JSON 里 `provider` 会被 serde flatten 吃到顶层而非 extra)——故用专用键 `kiro_provider`
/// (审查 Skeptic#2 / Architect#2)。
fn kiro_idp(account: &Account) -> Option<&str> {
    account.extra_str("kiro_provider").map(str::trim).filter(|v| !v.is_empty())
}

/// 调度计价档位:该号的积分是否按**企业号**(贵)计价。供最优控制调度器的号型单价使用,
/// 不影响任何上游请求。
///
/// 判定顺序(先中先得):
/// 1. `extra.price_class` 显式指定 `enterprise` / `personal`(运维手动覆盖);
/// 2. ksk_ API Key 凭据(API Key 账号池)→ 企业;
/// 3. `auth_method = external_idp`(Azure AD 企业 SSO)→ 企业;
/// 4. `kiro_provider ∈ {enterprise, internal}` → 企业;
/// 5. `kiro_provider ∈ {builderid, builder-id, aws, github, google}` → 个人 ——
///    **必须排在 IdC 判定之前**:付费 BuilderID 号同样带 client_id + client_secret,
///    只按 IdC 判会把它们误算成企业号;
/// 6. 其余只要带 IdC 材料(client_id + client_secret)→ 企业;
/// 7. 默认个人。
pub fn is_enterprise_priced(account: &Account) -> bool {
    if let Some(v) = account.extra_str("price_class").map(str::trim) {
        if v.eq_ignore_ascii_case("enterprise") {
            return true;
        }
        if v.eq_ignore_ascii_case("personal") {
            return false;
        }
    }
    if crate::machine_id::is_api_key_credential(account) || is_external_idp(account) {
        return true;
    }
    if let Some(idp) = kiro_idp(account) {
        if idp.eq_ignore_ascii_case("enterprise") || idp.eq_ignore_ascii_case("internal") {
            return true;
        }
        if ["builderid", "builder-id", "aws", "github", "google"]
            .iter()
            .any(|p| idp.eq_ignore_ascii_case(p))
        {
            return false;
        }
    }
    account.extra_str("client_id").is_some_and(|s| !s.is_empty())
        && account.extra_str("client_secret").is_some_and(|s| !s.is_empty())
}

/// 条件头判定:Kiro idp == "internal"(大小写不敏感)。
fn is_internal_provider(account: &Account) -> bool {
    kiro_idp(account).is_some_and(|v| v.eq_ignore_ascii_case("internal"))
}

/// 缺失 profileArn 时的固定兜底(对齐 static_flow `fixed_profile_arn`):
/// github/google → social 共享 ARN;builderid/aws → builder 共享 ARN;其余(企业/内部/
/// external_idp)需动态 ListAvailableProfiles(未实现),返回 None 即省略 profileArn。
pub(crate) fn fixed_profile_arn(account: &Account) -> Option<&'static str> {
    let idp = kiro_idp(account)?;
    if idp.eq_ignore_ascii_case("github") || idp.eq_ignore_ascii_case("google") {
        Some(SOCIAL_PROFILE_ARN)
    } else if idp.eq_ignore_ascii_case("builderid")
        || idp.eq_ignore_ascii_case("builder-id")
        || idp.eq_ignore_ascii_case("aws")
    {
        Some(BUILDER_ID_PROFILE_ARN)
    } else {
        None
    }
}

/// 解析发包用的 profileArn:账号显式值优先,否则按 idp 取固定兜底。
///
/// API Key 凭据**永不带 profileArn**:`TokenType: API_KEY` 让服务端按 key 自身账号解析,
/// 反而带上(错误的)profileArn 会被拒。apikey 号无 kiro_provider,固定兜底本就返回 None,
/// 这里再显式短路一次作防御(万一误配了 profile_arn/kiro_provider)。
pub(crate) fn resolve_profile_arn(account: &Account) -> Option<String> {
    if crate::machine_id::is_api_key_credential(account) {
        return None;
    }
    account
        .extra_str("profile_arn")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| fixed_profile_arn(account).map(|s| s.to_string()))
}

/// 发包用的 bearer token(`Authorization: Bearer <token>` 的值)。
///
/// **凭据来源随类型分流**(单一事实来源,chat 与配额共用):
/// - API Key 账号 → `kiro_api_key`(ksk_,长期有效,由 `TokenType: API_KEY` 头激活);
/// - social / IdC 账号 → `access_token`(运行时刷新换取)。
///
/// 之所以不统一读 `access_token`:apikey 账号可能仅从 YAML/admin 建号、只带 `kiro_api_key`
/// 而无 `access_token`(不该强制用户镜像一份密钥),故按类型取真值。
pub(crate) fn bearer_token(account: &Account) -> Option<&str> {
    let field = if crate::machine_id::is_api_key_credential(account) {
        "kiro_api_key"
    } else {
        "access_token"
    };
    account.extra_str(field).filter(|s| !s.is_empty())
}

/// 把 IdC(AWS SSO OIDC)刷新金标准头加到请求上(头集合 + 值逐字对齐 static_flow
/// `refresh_idc`;含 `accept: */*`)。请求体由调用方 `.json(...)` 附加。
pub(crate) fn apply_idc_refresh_headers(
    rb: reqwest::RequestBuilder,
    region: &str,
    version: &str,
) -> reqwest::RequestBuilder {
    let (x_amz_ua, ua) = idc_refresh_user_agents(version);
    rb.header("content-type", "application/json")
        .header("host", format!("oidc.{region}.amazonaws.com"))
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=4")
        .header("connection", "close")
        .header("x-amz-user-agent", x_amz_ua)
        .header("accept", "*/*")
        .header("user-agent", ua)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enterprise_pricing_classification() {
        // 生产 09-30 实际形态:企业号 = kiro_provider enterprise + IdC 材料。
        assert!(is_enterprise_priced(&account_with(json!({
            "kiro_provider": "enterprise", "auth_method": "IdC",
            "client_id": "cid", "client_secret": "sec"}))));
        // 付费 BuilderID 同样带 client_id+secret,但属个人号(判定必须先看 kiro_provider)。
        assert!(!is_enterprise_priced(&account_with(json!({
            "kiro_provider": "builderid", "client_id": "cid", "client_secret": "sec"}))));
        // 社交号。
        assert!(!is_enterprise_priced(&account_with(json!({"kiro_provider": "google", "auth_method": "social"}))));
        assert!(!is_enterprise_priced(&account_with(json!({"kiro_provider": "github"}))));
        // ksk_ API Key(API Key 账号池)、external_idp、internal → 企业。
        assert!(is_enterprise_priced(&account_with(json!({"kiro_api_key": "ksk_abc", "auth_method": "api_key"}))));
        assert!(is_enterprise_priced(&account_with(json!({"auth_method": "external_idp"}))));
        assert!(is_enterprise_priced(&account_with(json!({"kiro_provider": "internal"}))));
        // 无 kiro_provider 的裸 IdC → 企业;什么都没有 → 个人。
        assert!(is_enterprise_priced(&account_with(json!({"client_id": "c", "client_secret": "s"}))));
        assert!(!is_enterprise_priced(&account_with(json!({}))));
        // 显式覆盖优先。
        assert!(!is_enterprise_priced(&account_with(json!({"price_class": "personal", "kiro_provider": "enterprise"}))));
        assert!(is_enterprise_priced(&account_with(json!({"price_class": "Enterprise", "kiro_provider": "google"}))));
    }

    fn account_with(extra: serde_json::Value) -> Account {
        let mut acc = Account {
            account_id: "acc-1".into(),
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

    #[test]
    fn default_version_tracks_wire_profile() {
        // 版本号必须跟报文形态同属一个时代:legacy 开 → 0.12.155,关 → 1.0.212。
        assert_eq!(default_version_for(false), CURRENT_KIRO_VERSION);
        assert_eq!(default_version_for(true), LEGACY_KIRO_VERSION);
        assert_eq!(LEGACY_KIRO_VERSION, "0.12.155", "旧默认版本钉死,改动即改形态");
    }

    #[test]
    fn host_header_strips_scheme_and_path() {
        assert_eq!(
            host_header("https://runtime.us-east-1.kiro.dev"),
            "runtime.us-east-1.kiro.dev"
        );
        assert_eq!(
            host_header("https://runtime.us-east-1.kiro.dev/generateAssistantResponse"),
            "runtime.us-east-1.kiro.dev"
        );
        assert_eq!(host_header("http://127.0.0.1:19090/x"), "127.0.0.1:19090");
    }

    #[test]
    fn streaming_ua_matches_static_flow_exactly() {
        let (x_amz, ua) = streaming_user_agents("0.12.155", "a".repeat(64).as_str());
        assert_eq!(
            x_amz,
            format!("aws-sdk-js/1.0.34 KiroIDE-0.12.155-{}", "a".repeat(64))
        );
        // 逐字对齐 static_flow kiro_user_agents(Streaming):无 m/E,os/node 带版本。
        assert_eq!(
            ua,
            format!(
                "aws-sdk-js/1.0.34 ua/2.1 os/darwin#24.6.0 lang/js md/nodejs#22.22.0 \
                 api/codewhispererstreaming#1.0.34 KiroIDE-0.12.155-{}",
                "a".repeat(64)
            )
        );
        assert!(!ua.contains("m/E"), "主推理 UA 不应含 m/E:{ua}");
    }

    #[test]
    fn streaming_headers_full_golden_set() {
        let acc = account_with(json!({}));
        let rb = reqwest::Client::new().post("https://runtime.us-east-1.kiro.dev/generateAssistantResponse");
        let rb = apply_streaming_headers(
            rb,
            &acc,
            &crate::endpoint::Endpoint::rest("https://runtime.us-east-1.kiro.dev"),
            "tok-123",
            &"b".repeat(64),
            "0.12.155",
        );
        let req = rb.build().unwrap();
        let h = req.headers();
        assert_eq!(h.get("content-type").unwrap(), "application/json");
        assert_eq!(h.get("accept").unwrap(), "application/vnd.amazon.eventstream");
        assert_eq!(h.get("x-amzn-kiro-agent-mode").unwrap(), "vibe");
        assert_eq!(h.get("x-amzn-codewhisperer-optout").unwrap(), "true");
        assert_eq!(h.get("host").unwrap(), "runtime.us-east-1.kiro.dev");
        assert_eq!(h.get("amz-sdk-request").unwrap(), "attempt=1; max=3");
        assert_eq!(h.get("authorization").unwrap(), "Bearer tok-123");
        assert!(h.get("amz-sdk-invocation-id").is_some());
        // 默认账号(非 external_idp、非 internal)不应带条件头。
        assert!(h.get("TokenType").is_none());
        assert!(h.get("redirect-for-internal").is_none());
    }

    #[test]
    fn responses_headers_match_ide_1_4_121_capture() {
        // 抓包 wire-capture-pro-claude-1.4.121.jsonl 的头集合与顺序(Content-Length 由 HTTP 层补)。
        let acc = account_with(json!({"kiro_endpoint": "responses"}));
        let ep = crate::endpoint::resolve_with(&acc, "us-east-1", None, false);
        let mid = "7".repeat(64);
        let req = apply_responses_headers(
            reqwest::Client::new().post(ep.url()),
            &acc,
            &ep,
            "tok-r",
            &mid,
            Some("arn:aws:codewhisperer:us-east-1:1:profile/X"),
        )
        .build()
        .unwrap();
        assert_eq!(req.url().as_str(), "https://runtime.us-east-1.kiro.dev/v1/responses");
        let names: Vec<&str> = req.headers().keys().map(|k| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "content-type",
                "authorization",
                "x-amzn-kiro-origin",
                "x-amzn-codewhisperer-optout",
                "x-amzn-kiro-profile",
                "user-agent",
                "x-amzn-kiro-attempt",
                "x-amzn-kiro-fallback",
                "host",
                "connection",
            ]
        );
        let h = req.headers();
        assert_eq!(h.get("content-type").unwrap(), "application/json");
        assert_eq!(h.get("authorization").unwrap(), "Bearer tok-r");
        assert_eq!(h.get("x-amzn-kiro-origin").unwrap(), "AI_EDITOR");
        assert_eq!(
            h.get("user-agent").unwrap().to_str().unwrap(),
            format!("KiroIDE 1.4.121 {mid} KAS/0.66.89 acp-client/kiro-ide")
        );
        assert_eq!(h.get("x-amzn-kiro-attempt").unwrap(), "1;max=3");
        assert_eq!(h.get("x-amzn-kiro-fallback").unwrap(), "0;max=0");
        assert_eq!(h.get("host").unwrap(), "runtime.us-east-1.kiro.dev");
        for absent in ["accept", "x-amz-user-agent", "amz-sdk-request", "x-amzn-kiro-agent-mode", "TokenType"] {
            assert!(h.get(absent).is_none(), "不该有 {absent}");
        }
        // 外部 IdP 号:TokenType 夹在 profile 与 User-Agent 之间(客户端 MUs 的对象字面量顺序)。
        let idp = account_with(json!({"kiro_endpoint": "responses", "auth_method": "external_idp"}));
        let req = apply_responses_headers(reqwest::Client::new().post(ep.url()), &idp, &ep, "t", &mid, None)
            .build()
            .unwrap();
        let names: Vec<&str> = req.headers().keys().map(|k| k.as_str()).collect();
        let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(pos("x-amzn-codewhisperer-optout") < pos("tokentype") && pos("tokentype") < pos("user-agent"));
        assert!(req.headers().get("x-amzn-kiro-profile").is_none());
    }

    #[test]
    fn responses_account_reports_1_4_121_everywhere_else_unchanged() {
        let acc = account_with(json!({"kiro_endpoint": "responses"}));
        assert_eq!(kiro_version(&acc), "1.4.121");
        // 刷新/查额度的 UA 都从 kiro_version 取版本;Node 版本跟着走。
        let (_, ua) = streaming_user_agents(&kiro_version(&acc), &"m".repeat(64));
        assert!(ua.contains("md/nodejs#24.22.1") && ua.contains("KiroIDE-1.4.121-"), "{ua}");
        let (_, idc_ua) = idc_refresh_user_agents("1.4.121");
        assert!(idc_ua.contains("md/nodejs#24.22.1"), "{idc_ua}");
        // 其它号逐字节不变。
        let plain = account_with(json!({"kiro_endpoint": "q"}));
        assert_eq!(kiro_version(&plain), default_kiro_version());
        assert_eq!(node_version_for(default_kiro_version()), "22.22.0");
        assert_eq!(node_version_for("0.12.155"), "22.22.0");
    }

    #[test]
    fn streaming_headers_rest_form_has_no_amz_target() {
        // 防回归:默认(REST)形态不得带 x-amz-target,content-type 仍是 application/json。
        let acc = account_with(json!({}));
        let ep = crate::endpoint::Endpoint::rest("https://runtime.us-east-1.kiro.dev");
        let rb = reqwest::Client::new().post(ep.url());
        let req = apply_streaming_headers(rb, &acc, &ep, "t", &"e".repeat(64), "1.0.212")
            .build()
            .unwrap();
        assert!(req.headers().get("x-amz-target").is_none());
        assert_eq!(req.headers().get("content-type").unwrap(), "application/json");
        assert_eq!(
            req.url().as_str(),
            "https://runtime.us-east-1.kiro.dev/generateAssistantResponse"
        );
    }

    #[test]
    fn streaming_headers_legacy_rpc_form_matches_verified_wire() {
        // 老端点 RPC 形态(2026-11-06 实测 opus-5.5 返回 200 的那组):
        // POST q 域名根路径 + x-amz-json-1.0 + x-amz-target,其余头与 REST 形态一致。
        let acc = account_with(json!({"kiro_endpoint": "q"}));
        let ep = crate::endpoint::resolve_with(&acc, "us-east-1", None, false);
        let rb = reqwest::Client::new().post(ep.url());
        let req = apply_streaming_headers(rb, &acc, &ep, "tok-9", &"f".repeat(64), "1.0.212")
            .build()
            .unwrap();
        let h = req.headers();
        assert_eq!(req.url().as_str(), "https://q.us-east-1.amazonaws.com/");
        assert_eq!(h.get("content-type").unwrap(), "application/x-amz-json-1.0");
        assert_eq!(
            h.get("x-amz-target").unwrap(),
            "AmazonCodeWhispererStreamingService.GenerateAssistantResponse"
        );
        assert_eq!(h.get("host").unwrap(), "q.us-east-1.amazonaws.com");
        // 其余金标准头不受影响。
        assert_eq!(h.get("accept").unwrap(), "application/vnd.amazon.eventstream");
        assert_eq!(h.get("x-amzn-kiro-agent-mode").unwrap(), "vibe");
        assert_eq!(h.get("x-amzn-codewhisperer-optout").unwrap(), "true");
        assert_eq!(h.get("amz-sdk-request").unwrap(), "attempt=1; max=3");
        assert_eq!(h.get("authorization").unwrap(), "Bearer tok-9");
        assert!(h.get("TokenType").is_none());
    }

    #[test]
    fn idc_refresh_ua_matches_static_flow_exactly() {
        let (x_amz, ua) = idc_refresh_user_agents("0.12.155");
        assert_eq!(x_amz, "aws-sdk-js/3.980.0 KiroIDE-0.12.155");
        assert_eq!(
            ua,
            "aws-sdk-js/3.980.0 ua/2.1 os/darwin#24.6.0 lang/js md/nodejs#22.22.0 m/E"
        );
        assert!(!ua.contains("api/sso-oidc"), "IdC UA 不应含 api/sso-oidc:{ua}");
    }

    #[test]
    fn idc_refresh_headers_full_golden_set() {
        let rb = reqwest::Client::new().post("https://oidc.us-east-1.amazonaws.com/token");
        let req = apply_idc_refresh_headers(rb, "us-east-1", "0.12.155").build().unwrap();
        let h = req.headers();
        assert_eq!(h.get("content-type").unwrap(), "application/json");
        assert_eq!(h.get("host").unwrap(), "oidc.us-east-1.amazonaws.com");
        assert_eq!(h.get("amz-sdk-request").unwrap(), "attempt=1; max=4");
        assert_eq!(h.get("connection").unwrap(), "close");
        assert_eq!(h.get("x-amz-user-agent").unwrap(), "aws-sdk-js/3.980.0 KiroIDE-0.12.155");
        // static_flow IdC 刷新带 accept: */*(此前缺失,审查 Skeptic#3/Architect#3)。
        assert_eq!(h.get("accept").unwrap(), "*/*");
        assert!(h.get("amz-sdk-invocation-id").is_some());
        assert!(!h.get("user-agent").unwrap().to_str().unwrap().contains("api/sso-oidc"));
    }

    #[test]
    fn profile_arn_explicit_wins_then_fixed_fallback() {
        // 显式 profile_arn 优先。
        let acc = account_with(json!({"profile_arn": "arn:custom:x", "kiro_provider": "github"}));
        assert_eq!(resolve_profile_arn(&acc).as_deref(), Some("arn:custom:x"));
        // 缺失时按 idp 取固定 ARN(对齐 static_flow)。
        let gh = account_with(json!({"kiro_provider": "Github"}));
        assert_eq!(resolve_profile_arn(&gh).as_deref(), Some(SOCIAL_PROFILE_ARN));
        let bid = account_with(json!({"kiro_provider": "builder-id"}));
        assert_eq!(resolve_profile_arn(&bid).as_deref(), Some(BUILDER_ID_PROFILE_ARN));
        // idp 未知 / 缺失 → 省略(不臆造 ARN)。
        let unknown = account_with(json!({"kiro_provider": "enterprise"}));
        assert_eq!(resolve_profile_arn(&unknown), None);
        let none = account_with(json!({}));
        assert_eq!(resolve_profile_arn(&none), None);
    }

    #[test]
    fn internal_provider_reads_kiro_provider_not_top_level() {
        // 撞名修复:用 kiro_provider 而非 extra["provider"]。
        let acc = account_with(json!({"kiro_provider": "Internal"}));
        let rb = reqwest::Client::new().post("https://runtime.us-east-1.kiro.dev/x");
        let req = apply_streaming_headers(rb, &acc, &crate::endpoint::Endpoint::rest("https://runtime.us-east-1.kiro.dev"), "t", &"d".repeat(64), "0.12.155").build().unwrap();
        assert_eq!(req.headers().get("redirect-for-internal").unwrap(), "true");
    }

    #[test]
    fn external_idp_and_internal_conditional_headers() {
        let acc = account_with(json!({"auth_method": "external_idp", "kiro_provider": "Internal"}));
        let rb = reqwest::Client::new().post("https://runtime.us-east-1.kiro.dev/x");
        let rb = apply_streaming_headers(
            rb,
            &acc,
            &crate::endpoint::Endpoint::rest("https://runtime.us-east-1.kiro.dev"),
            "tok",
            &"c".repeat(64),
            "0.12.155",
        );
        let req = rb.build().unwrap();
        let h = req.headers();
        assert_eq!(h.get("TokenType").unwrap(), "EXTERNAL_IDP");
        assert_eq!(h.get("redirect-for-internal").unwrap(), "true");
    }

    #[test]
    fn apply_external_idp_token_type_adds_header_only_for_external_idp() {
        // external_idp 账号:补 TokenType 头(getUsageLimits/ListAvailableProfiles 都靠它)。
        // 大小写/写法不敏感(is_external_idp 用 eq_ignore_ascii_case):导入文件里可能是
        // "External_IDP"/"EXTERNAL_IDP" 等变体,都要命中,否则 Azure 号照样吃 403。
        for am in [
            json!({"auth_method": "external_idp"}),
            json!({"auth_method": "External_IDP"}),
            json!({"auth_method": "EXTERNAL_IDP"}),
            json!({"auth_method": "  external_idp  "}),
        ] {
            let acc = account_with(am.clone());
            let rb = reqwest::Client::new().get("https://q.us-east-1.amazonaws.com/x");
            let req = apply_external_idp_token_type(rb, &acc).build().unwrap();
            assert_eq!(req.headers().get("TokenType").unwrap(), "EXTERNAL_IDP", "{am} 应命中");
        }

        // 非 external_idp(social/idc/builderid):绝不加,免得误标令牌类型。
        for am in [json!({}), json!({"auth_method": "idc"}), json!({"auth_method": "social"})] {
            let acc = account_with(am);
            let rb = reqwest::Client::new().get("https://q.us-east-1.amazonaws.com/x");
            let req = apply_external_idp_token_type(rb, &acc).build().unwrap();
            assert!(req.headers().get("TokenType").is_none());
        }
    }

    #[test]
    fn api_key_account_sets_tokentype_api_key_header() {
        // 实测:runtime.kiro.dev 需要 `TokenType: API_KEY` 才免 profileArn。
        let acc = account_with(json!({"kiro_api_key": "ksk_abc", "auth_method": "api_key"}));
        let rb = reqwest::Client::new().post("https://runtime.us-east-1.kiro.dev/x");
        let req = apply_streaming_headers(
            rb,
            &acc,
            &crate::endpoint::Endpoint::rest("https://runtime.us-east-1.kiro.dev"),
            "ksk_abc", // bearer = 镜像的 ksk_
            &"a".repeat(64),
            "0.12.155",
        )
        .build()
        .unwrap();
        let h = req.headers();
        assert_eq!(h.get("TokenType").unwrap(), "API_KEY");
        assert_eq!(h.get("authorization").unwrap(), "Bearer ksk_abc");
    }

    #[test]
    fn api_key_account_never_carries_profile_arn() {
        // 即便误配了 profile_arn / kiro_provider,apikey 也必须省略 profileArn。
        let acc = account_with(json!({
            "kiro_api_key": "ksk_abc",
            "profile_arn": "arn:should:not:be:used",
            "kiro_provider": "github",
        }));
        assert_eq!(resolve_profile_arn(&acc), None);
    }

    #[test]
    fn bearer_token_source_by_credential_type() {
        // apikey:取 kiro_api_key(即便没有 access_token,如 YAML/admin 建号)。
        let apikey = account_with(json!({"kiro_api_key": "ksk_xyz"}));
        assert_eq!(bearer_token(&apikey), Some("ksk_xyz"));
        // social/IdC:取 access_token。
        let oauth = account_with(json!({"refresh_token": "rt", "access_token": "at-123"}));
        assert_eq!(bearer_token(&oauth), Some("at-123"));
        // 缺凭据 → None。
        assert_eq!(bearer_token(&account_with(json!({"refresh_token": "rt"}))), None);
    }
}
