//! 控制面接口:健康快照、设置同步实况、账号运维(重置 / 刷新 / 探针 / 额度 / 超额 / 模型目录)、`/v1/models`。

use super::*;

/// `GET /settings-sync` —— 设置热调实况(轻量,不碰 scheduler 与配额)。
pub(super) async fn settings_sync(State(st): State<Arc<WorkerState>>) -> impl IntoResponse {

    Json(serde_json::json!({
        "role": "worker",
        "instance": st.instance,
        "group": st.group,
        "provider": st.provider.family(),
        "settings": settings_view(&st),
    }))
}

/// 设置同步实况的 JSON 投影(`/health` 与 `/settings-sync` 共用,免得两处分叉)。
pub(super) fn settings_view(st: &WorkerState) -> serde_json::Value {
    let s = st.settings_sync.read();
    let mut v = serde_json::json!({
        "applied_at": s.applied_at,
        // 距今多少秒。轮询周期 30s,所以 >90 基本就是同步停了。-1 = 一次都没成功过。
        "age_secs": if s.applied_at > 0 { now_unix() - s.applied_at } else { -1 },
        "error": s.error,
        "unknown": s.unknown,
        "effective": s.effective,
        // false = provider 级设置(缓存计费等)对本 worker 要重启才生效。
        "provider_hot": s.provider_hot,
    });
    // 上游自报的设置指纹(如 cursor 的 `cursor_guard_rev`),见 Provider::settings_fingerprints。
    if let Some(map) = v.as_object_mut() {
        map.extend(st.provider.settings_fingerprints());
    }
    v
}

pub(super) async fn health(State(st): State<Arc<WorkerState>>) -> impl IntoResponse {
    // 每账号运行态 + 配额(配额读缓存,陈旧时后台刷新,不阻塞本响应)。
    let accounts_status: Vec<serde_json::Value> = st
        .scheduler
        .status_snapshot()
        .into_iter()
        .map(|s| {
            let quota = st.quota_cached_or_refresh(&s.account_id);
            let mut v = serde_json::to_value(&s).unwrap_or(serde_json::Value::Null);
            if let serde_json::Value::Object(map) = &mut v {
                map.insert("quota".into(), quota_to_json(quota));
            }
            v
        })
        .collect();
    Json(serde_json::json!({
        "role": "worker",
        "build_commit": crate::BUILD_COMMIT,
        "instance": st.instance,
        "egress": st.egress_desc,
        "group": st.group,
        "provider": st.provider.family(),
        "accounts": st.scheduler.total(),
        // 每账号运行态(冷却/封禁/并发占用)+ 配额,admin 账号页经 router 聚合展示。
        "accounts_status": accounts_status,
        // 排队实况(等待数 / 容量 / 已开号数),admin 账号页展示。
        "queue": st.scheduler.queue_stats(),
        // 本 worker **实际在跑**的选号算法(能力 + 设置同时满足才是 cost_index)。
        // 与 settings 里的配置意图分开报:cursor/dario worker 配置为 cost_index 时这里仍是 affinity。
        "scheduler_algorithm_active": st.scheduler.active_algorithm(),
        // usage 是否在落库:库打开失败时为 false(降级,usage 不入库),便于运维发现。
        "usage_persist": st.usage_sink.is_some(),
        // 设置热调的实况:本 worker **真正在用**的值 + 最近一次同步的结果。
        // 面板据此回答「我保存的设置到底生效没有」—— 见 [`SettingsSync`]。
        "settings": settings_view(&st),
        // 批量任务拦截实况:模式、累计判定/拦截数、当前已判定的模板(开头 40 字)。
        "batch_guard": gates::batch_guard::snapshot(std::time::Instant::now()),
        // 逐条筛查(探针 / 超大输入)累计命中与拦截数。
        "request_screen": gates::request_screen::snapshot(),
        // 拒答记忆:模式、在记的被拒请求、累计钉号 / 本地拒答次数。
        "refusal_guard": gates::refusal_guard::snapshot(std::time::Instant::now()),
        // 号池熔断:模式、是否熔断、窗口内出事的号。
        "pool_breaker": super::pool_breaker::snapshot(),
        "status": "ok"
    }))
}

/// `POST /accounts/{id}/reset` —— 内网管理:人工救号。清运行时禁用/冷却/失败计数
/// (含 429 节流窗口、RPM 滑动窗口、(账号,模型) 不可用标记;配置层 disabled 不动,
/// 那走 PATCH)。由 admin(router 进程)扇出调用;
/// worker 只绑 loopback,信任内网调用方。404 = 账号不在本 worker 组。
pub(super) async fn reset_account(
    State(st): State<Arc<WorkerState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    if st.scheduler.reset_account(&id) {
        Json(serde_json::json!({"reset": true, "account_id": id})).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"reset": false, "account_id": id})),
        )
            .into_response()
    }
}

/// router→worker 换码请求体。`proxy`=该号 extra.proxy(None=组默认出口),`code`/`verifier`=PKCE,
/// `state`=上号会话状态(上游换码报文要求带,与官方客户端对齐;老 router 不带时为空串)。
#[derive(serde::Deserialize)]
pub(super) struct OAuthExchangeBody {
    #[serde(default)]
    proxy: Option<String>,
    code: String,
    verifier: String,
    #[serde(default)]
    state: String,
}

/// `POST /oauth/exchange` —— **无状态**铸 token:`authorization_code` → token set。
///
/// 走**本 worker 的 egress**(由 `provider.oauth_exchange` 经 `egress_client_for` 选)=该号
/// 将来 refresh/chat 同一出口 IP(铸≠发=关联封号)。router 只把请求扇给目标组(account_group
/// 匹配)的 worker,故出口正确;非 dario provider 默认实现回 BadRequest(400)。
/// 账号此刻尚未入库——纯换码,不碰 scheduler。token 明文只回给同机 router 落库,不进日志。
pub(super) async fn oauth_exchange(
    State(st): State<Arc<WorkerState>>,
    Json(body): Json<OAuthExchangeBody>,
) -> axum::response::Response {
    match st
        .provider
        .oauth_exchange(body.proxy.as_deref(), &body.code, &body.verifier, &body.state)
        .await
    {
        Ok(tokens) => Json(tokens).into_response(),
        // 运维端点:换码失败的原因(上游 OAuth 报文)要原样给面板。
        Err(e) => admin_error_response(&e),
    }
}

/// `POST /accounts/{id}/refresh` —— 人工**强制刷新该账号 token**(rt→at 换一次)。
///
/// 安全性:这就是后台配额轮询 / 按需刷新本来就在做的 OIDC token 交换,**不是** chat,
/// 不触发风控(见 no-chat-test-on-real-accounts 记忆)。操作者可借此验证 refresh_token
/// 仍可用、或在轮换 rt 后立刻换到新 access_token。
///
/// 仅本组持有该账号的 worker 命中;账号不在本组 → 404(admin 据此向其余 worker 续问)。
/// 上游刷新失败(invalid_grant / 网络 / 5xx)→ 经 `upstream_error_response` 透出错误,
/// admin 层据状态码区分"无人持有(404)"与"持有方刷新失败(502/400)"。
pub(super) async fn refresh_account(
    State(st): State<Arc<WorkerState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    let Some(account) = st.scheduler.account(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"refreshed": false, "account_id": id})),
        )
            .into_response();
    };
    // 开工时快照 suspend 世代:本次刷新可能跑几秒,期间账号若发生状态转换
    // (suspend/复活/恢复),迟到的上报就是旧世代回声,不得改写新状态
    // (对抗审查阻断#7;本路径不走 lease,必须自己快照)。
    let gen = st.scheduler.current_suspend_gen(&id);
    match st.force_refresh(account).await {
        Ok(refreshed) => {
            // 绝不回传 token 明文;只给 expires_at 让操作者确认新 token 的有效期窗口。
            let expires_at = refreshed.extra_str("expires_at").map(|s| s.to_string());
            Json(serde_json::json!({
                "refreshed": true,
                "account_id": id,
                "expires_at": expires_at,
            }))
            .into_response()
        }
        Err(e) => {
            // 反映真实状态(审查 Skeptic#1):与 chat 路径一致地 report_failure——
            // rt 永久失效(TokenInvalid)→ 立即标 invalid_refresh_token 禁用,仪表盘即时
            // 见死号、不再被路由到;transient(网络/5xx)→ 仅计失败数(救号一键清)。
            // 不在此换号/重试(人工动作就是要看这一次结果)。
            st.scheduler.report_failure_with_detail(
                &id,
                e.kind,
                gen,
                &e.to_string().chars().take(300).collect::<String>(),
            );
            // 运维端点:人工点"刷新"就是要看这一次的真实失败原因。
            admin_error_response(&e)
        }
    }
}

/// `POST /accounts/{id}/quota` —— 按需验活:确保 token 有效(必要时刷新,只读 OIDC
/// 交换)→ getUsageLimits 查配额。导入对话框逐账号验活用。**全程只读,绝不发 chat**
/// (见 no-chat-test-on-real-accounts 记忆)。
///
/// 仅本组持有该账号的 worker 命中;不在本组 → 404(admin 据此向其余 worker 续问)。
/// 上游失败 → report_failure(与 refresh 同理:死号立即标禁用,导入即见)+ 透出错误。
/// **人工探针**:钉住指定账号发一次**最小** chat,看上游到底出不出词。
///
/// 为什么需要它:`/quota` 的 `verified:true` 只证明**控制面**凭据活着,`/models` 只证明
/// 目录里有这个模型 —— 两者都不代表数据面能出词。实测过「有额度、有目录,一发 chat
/// 恒 `ModelNotAvailable`」的号。判定一个停用号能不能复活,只有真的收到 delta 才算数。
///
/// 风控约束(见 memory caio-kiro-key-suspend-lesson):`max_tokens=16`、单轮 "hi"、
/// 非流式语义下只读到首个文本 delta 就断流,走该账号自己的出口。**调用方必须串行 + 限速**,
/// 短时高频 chat 验号历史上直接导致过 `TEMPORARILY_SUSPENDED`。
///
/// **不上报账号健康**:这是人工动作,失败不该计入与真实流量共用的失败池/冷却,
/// 否则批量探测会把好号探成 `too_many_failures`(与 `/quota` 的取舍一致)。
pub(super) async fn probe_account(
    State(st): State<Arc<WorkerState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use futures::StreamExt;

    let model = q
        .get("model")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("claude-haiku-4.5")
        .to_string();

    let Some(account) = st.scheduler.account(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"replied": false, "account_id": id})),
        )
            .into_response();
    };
    // 与配额探测共用信号量:人工探针不突破既有上游压力边界。
    let _permit = match st.quota_sem.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"replied": false, "account_id": id})),
            )
                .into_response();
        }
    };

    let account = match st.ensure_credentialed(account).await {
        Ok(a) => a,
        Err(e) => {
            return Json(serde_json::json!({
                "replied": false, "account_id": id, "model": model,
                "stage": "credential", "error_kind": format!("{:?}", e.kind),
                "error": e.message.clone(),
            }))
            .into_response();
        }
    };
    let account = st.ensure_profile_arn(account).await;

    let req = gw_core::provider::ChatRequest::from_anthropic_body(serde_json::json!({
        "model": model,
        "max_tokens": 16,
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}],
    }));
    let ctx = gw_core::provider::CallCtx {
        account: account.clone(),
        session_id: format!("probe-{id}"),
        cache_key: format!("probe-{id}"),
    };

    let started = std::time::Instant::now();
    // 定频准入:人工探针也是一次真实的上游调用,同样受有效 RPM 上限约束(含暖机)。
    // 达限就如实报"被定频拦住"、绝不硬发 —— 探一个已接近上限的号把它推过阈值,
    // 正是定频闸门要防的事。探针无分组上下文,按 default_rank 口径(低频人工操作,
    // 取舍见 effective_rpm_limit 注释)。
    if !st.scheduler.note_upstream_call(&ctx.account.account_id, None) {
        return Json(serde_json::json!({
            "replied": false, "account_id": id, "model": model,
            "stage": "rpm_limited",
            "error": "该号已达有效 RPM 上限(含暖机),探针未发出",
        }))
        .into_response();
    }
    let stream = match st.provider.chat(req, &ctx).await {
        Ok(s) => s,
        Err(e) => {
            return Json(serde_json::json!({
                "replied": false, "account_id": id, "model": model,
                "stage": "connect", "error_kind": format!("{:?}", e.kind),
                "error": e.message.clone(),
            }))
            .into_response();
        }
    };

    // 收到首个文本 delta 即判定"能出词"并断流 —— 不把 16 个 token 读完,少烧一点是一点。
    let mut stream = stream;
    let mut text = String::new();
    let mut err: Option<(String, String)> = None;
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(90));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => {
                err = Some(("Timeout".into(), "探针 90s 未收到任何内容".into()));
                break;
            }
            item = stream.next() => {
                match item {
                    None => break,
                    Some(Err(e)) => {
                        err = Some((format!("{:?}", e.kind), e.message.clone()));
                        break;
                    }
                    Some(Ok(gw_core::provider::StreamItem::Usage(_))) => {}
                    // 探针只认首个文本即断流,掐流信号与探针判定无关,忽略。
                    Some(Ok(gw_core::provider::StreamItem::UpstreamCut)) => {}
                    Some(Ok(gw_core::provider::StreamItem::Sse(ev))) => {
                        if let Some(t) = ev
                            .data
                            .get("delta")
                            .and_then(|d| d.get("text"))
                            .and_then(|t| t.as_str())
                        {
                            text.push_str(t);
                            if !text.trim().is_empty() {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    drop(stream);

    let replied = !text.trim().is_empty();
    Json(serde_json::json!({
        "replied": replied,
        "account_id": id,
        "model": model,
        "elapsed_ms": started.elapsed().as_millis() as u64,
        // 只回显前 40 字符:够判断"是真回复还是拒答模板",又不至于把内容灌进日志。
        "text": text.chars().take(40).collect::<String>(),
        "error_kind": err.as_ref().map(|(k, _)| k.clone()),
        "error": err.as_ref().map(|(_, m)| m.clone()),
    }))
    .into_response()
}

pub(super) async fn quota_account(
    State(st): State<Arc<WorkerState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    if st.scheduler.account(&id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"verified": false, "account_id": id})),
        )
            .into_response();
    }
    // 与后台轮询/预热同走 quota_sem(审查 Architect#2/Minimalist#1):并发验活
    // 不突破 QUOTA_MAX_CONCURRENCY,不绕开既有上游压力边界。
    let _permit = match st.quota_sem.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"verified": false, "account_id": id})),
            )
                .into_response();
        }
    };
    match st.try_fetch_quota(&id).await {
        Ok(q) => {
            // 预检与取数之间账号可能被并发 /sync 移除(审查 Skeptic#3):try_fetch_quota
            // 对"不在本组"也回 Ok(None),与"上游无配额数据"撞语义——复查持有,不在则
            // 404 让 admin 继续问真正的持有方,而非误报 200"无配额"。
            if st.scheduler.account(&id).is_none() {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"verified": false, "account_id": id})),
                )
                    .into_response();
            }
            // 写共享配额缓存:表格积分列(/health runtime)立刻反映,且按 TTL
            // 节流后台轮询(刚验过的号本轮 sweep 自动跳过,不产生双倍流量)。
            st.quota_cache
                .lock()
                .insert(id.clone(), (q.clone(), std::time::Instant::now()));
            // verified=false + 200:账号可刷新但上游无配额数据(非硬失败,前端显示"—")。
            Json(serde_json::json!({
                "verified": q.is_some(),
                "account_id": id,
                "quota": quota_to_json(q),
            }))
            .into_response()
        }
        Err(e) => {
            // 只读探测失败**只对真死号惩罚**(审查三人共识):TokenInvalid(invalid_grant/
            // 403)→ report_failure 标 invalid_refresh_token,死号导入即现形;瞬时错误
            // (网络/5xx/429)只透传给前端展示,不计入与 chat 共用的失败池/冷却——
            // 否则上游抖动期间批量验活会把好号验成 too_many_failures。
            if matches!(e.kind, UpstreamErrorKind::TokenInvalid) {
                st.scheduler.report_failure_detail(
                    &id,
                    e.kind,
                    &e.to_string().chars().take(300).collect::<String>(),
                );
            }
            // 失败也写"尝试时刻"(None),与后台轮询同一节流口径。
            st.quota_cache
                .lock()
                .insert(id.clone(), (None, std::time::Instant::now()));
            // 运维端点:导入对话框要按这段原文区分"死号"与"上游抖动"。
            admin_error_response(&e)
        }
    }
}

/// `POST /accounts/{id}/on-demand` —— 设置该号的超额(on-demand)额度上限。
///
/// body: `{"limit_usd": 50}`(美元整数);`0` 或 `null` = **关闭**超额。
///
/// ⚠️ 与 `/quota` 不同,这是**写**操作:它改上游账号的计费设置,开启后套餐用尽会产生
/// 真实费用。只由 admin 面板的显式运维动作触发,绝不在任何轮询路径上。
///
/// 成功后**顺带刷新配额缓存**:否则面板要等一个 TTL 才看到新上限,运维会以为没生效。
pub(super) async fn on_demand_account(
    State(st): State<Arc<WorkerState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: Option<Json<serde_json::Value>>,
) -> axum::response::Response {
    if st.scheduler.account(&id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "account_id": id})),
        )
            .into_response();
    }
    if !st.provider.on_demand_supported() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": {"message": format!("provider {} 不支持超额额度设置", st.provider.family())}
            })),
        )
            .into_response();
    }
    // limit_usd 缺省/null/0 → 关闭超额。负数或超 i32 的值直接拒:上游字段是 i32,
    // 静默截断会把「设 $50」变成别的数字。
    let raw = body
        .as_ref()
        .and_then(|Json(v)| v.get("limit_usd").cloned())
        .unwrap_or(serde_json::Value::Null);
    let limit_usd: Option<u32> = if raw.is_null() {
        None
    } else {
        match raw.as_i64() {
            Some(n) if (0..=i32::MAX as i64).contains(&n) => Some(n as u32),
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": {"message": "limit_usd 必须是 0..=2147483647 的整数(美元),0 或 null = 关闭超额"}
                    })),
                )
                    .into_response();
            }
        }
    };

    // 与配额查询同走 quota_sem:控制面对上游的并发压力边界只有这一处。
    let _permit = match st.quota_sem.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"ok": false, "account_id": id})),
            )
                .into_response();
        }
    };
    let Some(account) = st.scheduler.account(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "account_id": id})),
        )
            .into_response();
    };
    let account = match st.ensure_credentialed(account).await {
        Ok(a) => a,
        Err(e) => return admin_error_response(&e),
    };
    if let Err(e) = st.provider.set_on_demand_limit(&account, limit_usd).await {
        // 上游拒绝(如未绑支付方式的 failed_precondition)按原文透出:运维要能区分
        // 「我们发错了」和「这号上游不允许」。写操作失败**不计失败池**:它与 chat
        // 可用性无关,一次计费设置被拒不该让号进冷却。
        return admin_error_response(&e);
    }
    // 回读一次(顺带刷新面板缓存)。回读失败不算设置失败:上游已经接受了。
    let quota = match st.try_fetch_quota(&id).await {
        Ok(q) => {
            st.quota_cache
                .lock()
                .insert(id.clone(), (q.clone(), std::time::Instant::now()));
            q
        }
        Err(e) => {
            tracing::debug!(account = %id, "超额设置成功但回读失败: {e}");
            None
        }
    };
    Json(serde_json::json!({
        "ok": true,
        "account_id": id,
        "quota": quota_to_json(quota),
    }))
    .into_response()
}

/// `POST /accounts/{id}/models` —— 用该账号拉一次上游**模型目录**并落库。
///
/// 目的是把 `rateMultiplier`(定价)与逐模型的 thinking 档位表从"代码里写死的印象"
/// 换成"上游当下的事实"。**全程只读**,与 `/quota` 同为控制面 GET,绝不发 chat
/// (见 no-chat-test-on-real-accounts 记忆)。
///
/// 仅本 worker 持有该账号时命中;不持有 → 404(admin 据此向其余 worker 续问)。
/// 与配额查询共用 `quota_sem`,不额外突破对上游的并发压力边界。
///
/// 目录是**全局**事实(不随账号变),所以落 `settings` 表单键覆盖;`fetched_by`
/// 记下是哪个号拉的,便于回溯不同订阅档位看到的模型集差异。
pub(super) async fn models_account(
    State(st): State<Arc<WorkerState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    // 预检只为快速 404,**不能**拿这里的副本去刷新 —— 见下方等锁后的复查。
    if st.scheduler.account(&id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"fetched": false, "account_id": id})),
        )
            .into_response();
    }
    let _permit = match st.quota_sem.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"fetched": false, "account_id": id})),
            )
                .into_response();
        }
    };
    // ⚠️ 等信号量期间账号可能被 `/sync` 移走、由**另一个 worker 接管**。此时若拿等锁前
    // 的旧副本去 ensure_credentialed,两个进程会同时用同一枚 rolling refresh_token 刷新
    // —— 一方拿到新 token,另一方 invalid_grant,账号报废(禁用池里 8 个号就是这么死的)。
    // 故:**拿到 permit 之后重新取**,取不到就交还给真正的持有方。
    let Some(account) = st.scheduler.account(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"fetched": false, "account_id": id})),
        )
            .into_response();
    };
    let account = match st.ensure_credentialed(account).await {
        Ok(a) => a,
        Err(e) => return admin_error_response(&e),
    };
    // 企业/IdC 号的控制面调用同样要求 profileArn,缺则先发现+持久化(与配额路径同款)。
    let account = st.ensure_profile_arn(account).await;
    match st.provider.model_catalog(&account).await {
        Ok(Some(catalog)) => {
            // 空目录**绝不落库**:上游/代理以 200 返回 `{}`、或字段名变更导致解析出空数组时,
            // 若照写会把上一份好快照(19 个模型 + 倍率)冲掉,而调用方还看到 persisted:true。
            // 宁可保留旧快照并如实说明没写。
            let model_count = catalog
                .get("models")
                .and_then(|m| m.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let persisted = match (&st.store, serde_json::to_string(&catalog)) {
                _ if model_count == 0 => {
                    tracing::warn!("模型目录为空，拒绝落库以保护既有快照");
                    false
                }
                (Some(store), Ok(json)) => {
                    match store.upsert_kv(gw_store::SqliteStore::KEY_MODEL_CATALOG, &json) {
                        Ok(()) => true,
                        Err(e) => {
                            // 落库失败不算整个请求失败:目录本身已经拿到,先透出给调用方,
                            // 免得一次 DB 抖动就白打一次上游。
                            tracing::warn!("模型目录落库失败: {e}");
                            false
                        }
                    }
                }
                // 无库 = 降级模式(账号只来自 yaml 快照),此时只透出不落库。
                _ => false,
            };
            Json(serde_json::json!({
                "fetched": true,
                "persisted": persisted,
                "model_count": model_count,
                "account_id": id,
                "catalog": catalog,
            }))
            .into_response()
        }
        // provider 不支持(如 dario)——不是错误,如实说明。
        Ok(None) => Json(serde_json::json!({
            "fetched": false,
            "persisted": false,
            "account_id": id,
            "reason": "该 provider 不提供模型目录",
        }))
        .into_response(),
        Err(e) => {
            // ⚠️ **绝不 report_failure。** 配额路径那样做是因为它兼职"导入验活",要让死号
            // 立刻现形;而本端点只是拉一份**全局**目录,与"这个号好不好"无关。
            //
            // 若照抄配额的惩罚逻辑,会有一条报废健康账号的路:导入的号没有 `expires_at`,
            // `has_fresh_token` 误判 token 新鲜 → 不预刷 → 目录调用 401 → 按 TokenInvalid
            // 永久标 invalid_refresh_token。配额路径为此专门做了"强刷 + 强制发现 profileArn
            // 再重试一次"的兜底(见 try_fetch_quota),本路径没有,也不该为此把那套复制过来 ——
            // 一个管理员手点的目录刷新,失败就如实报错,不该有任何禁号副作用。
            admin_error_response(&e)
        }
    }
}

/// `GET /accounts/{id}/models/local` —— 该号的模型可用清单(**纯本地,绝不打上游**):
/// provider 静态目录 × (档位静态支持 `account_supports_model` − 已学 INVALID_MODEL_ID
/// 标记)。面板「查看模型」按钮的数据源。
///
/// 「可用」口径是**本地认知**:未观察到拒绝 ≠ 上游保证(半死号要真发一次才知道,
/// 上游真相用 `POST /accounts/{id}/models` 拉目录 + `/probe` 验)。本端点零风险,
/// 随便点。仅本 worker 持有该账号时命中;不持有 → 404(admin 据此向其余 worker 续问)。
pub(super) async fn models_local(
    State(st): State<Arc<WorkerState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    let Some(account) = st.scheduler.account(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"account_id": id})),
        )
            .into_response();
    };
    let catalog = match st.provider.list_models().await {
        Ok(c) => c,
        Err(e) => return admin_error_response(&e),
    };
    let marks = st.scheduler.model_marks_for(&id);
    let mark_secs: std::collections::HashMap<&str, u64> = marks
        .iter()
        .map(|m| (m.model.as_str(), m.remaining_secs))
        .collect();
    let mut in_catalog: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let models: Vec<serde_json::Value> = catalog
        .iter()
        .map(|m| {
            in_catalog.insert(m.id.as_str());
            let supported = st.provider.account_supports_model(&account, &m.id);
            let mark = mark_secs.get(m.id.as_str()).copied();
            serde_json::json!({
                "id": m.id,
                "display_name": m.display_name,
                "supported": supported,
                "mark_remaining_secs": mark,
                "available": supported && mark.is_none(),
            })
        })
        .collect();
    // 目录外的标记(上游新模型/请求变体名打不中目录行)单独透出,不悄悄丢掉。
    let off_catalog_marks: Vec<serde_json::Value> = marks
        .iter()
        .filter(|m| !in_catalog.contains(m.model.as_str()))
        .map(|m| {
            serde_json::json!({"model": m.model, "remaining_secs": m.remaining_secs})
        })
        .collect();
    Json(serde_json::json!({
        "account_id": id,
        "models": models,
        "off_catalog_marks": off_catalog_marks,
    }))
    .into_response()
}
/// admin 导入账号后主动捅一下,消除"导入后 30s 内按号操作报无人持有"的窗口。
/// 无库(降级模式)/读库失败 → 503,调用方按 best-effort 忽略。
pub(super) async fn sync_now(State(st): State<Arc<WorkerState>>) -> axum::response::Response {
    match st.sync_accounts_from_db().await {
        Some((added, removed)) => Json(serde_json::json!({
            "synced": true,
            "added": added,
            "removed": removed,
        }))
        .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"synced": false})),
        )
            .into_response(),
    }
}

/// `GET /v1/models` —— 暴露 provider 的模型目录(Anthropic 线缆格式)。
/// provider 一处实现 `list_models`,框架在此映射成对外响应(写一次,各 provider 共享)。
pub(super) async fn models(State(st): State<Arc<WorkerState>>) -> axum::response::Response {
    // created_at 占位:Kiro 不提供每模型创建时间(见 kiro-model-versioning 记忆),
    // 但 Anthropic /v1/models 条目带 created_at,严格类型客户端会校验——给个固定占位值
    // 以保证兼容(非真实日期,仅占位)。
    const MODEL_CREATED_AT: &str = "2025-01-01T00:00:00Z";
    // OpenAI 形状用的 unix 秒占位(与上面那个 ISO 串同一个时刻,只是另一种编码)。
    const MODEL_CREATED_UNIX: i64 = 1_735_689_600;
    // OpenAI 字段**只在开了 OpenAI 入口的 worker 上**追加。
    //
    // 这是个共享端点:kiro / dario / claude-subprocess 也走它。给所有家族无条件加字段
    // 就是为了迁就一种协议去改另一种协议的共享响应 —— 按 JSON Schema 严校验的客户端、
    // 或对响应体做快照/哈希的代理都会看到不兼容变化(对抗评审 Architect#2)。
    // 只有 cursor 的客户端会说 OpenAI,也只有它需要这几个键。
    let openai_shape = st.provider.openai_wire();
    match st.provider.list_models().await {
        Ok(list) => {
            // 按本 worker 持有的账号裁目录(如 cursor 纯 bot 池),见 Provider::models_for_accounts。
            let list = st.provider.models_for_accounts(list, &st.scheduler.accounts_snapshot());
            let data: Vec<serde_json::Value> = list
                .iter()
                .map(|m| {
                    let mut item = serde_json::json!({
                        "type": "model",
                        "id": m.id,
                        "display_name": m.display_name.clone().unwrap_or_else(|| m.id.clone()),
                        "created_at": MODEL_CREATED_AT,
                    });
                    if openai_shape {
                        if let Some(o) = item.as_object_mut() {
                            o.insert("object".into(), serde_json::json!("model"));
                            o.insert("created".into(), serde_json::json!(MODEL_CREATED_UNIX));
                            o.insert("owned_by".into(), serde_json::json!(st.provider.family()));
                        }
                    }
                    item
                })
                .collect();
            let first = list.first().map(|m| m.id.clone());
            let last = list.last().map(|m| m.id.clone());
            let mut body = serde_json::json!({
                "data": data,
                "has_more": false,
                "first_id": first,
                "last_id": last,
            });
            if openai_shape {
                if let Some(o) = body.as_object_mut() {
                    o.insert("object".into(), serde_json::json!("list"));
                }
            }
            Json(body).into_response()
        }
        Err(e) => {
            // 目录当前是本地生成、恒 Ok;一旦改成真实上游查询,原文只剩这一条日志路。
            tracing::warn!(kind = ?e.kind, "模型目录获取失败: {e}");
            upstream_error_response(&e)
        }
    }
}
