//! 请求主链路:Anthropic / OpenAI 入口 → 入口策略 → 选号 → 调 provider(换号重试、过载退避)。

use super::*;

/// 换号重试上限:按错误类别分档。
///
/// 良性冷却类错误(RateLimited/QuotaExhausted)自限流(命中即进冷却 → 后续请求的
/// `eligible_ids` 直接跳过该号、不再重复打上游)且不传播毒性,故解绑紧上限、允许沿
/// **优先级阶梯**下探到组内所有号(cap=total):高优先级层全限流/额度耗尽时,请求自动
/// 落到低优先级兜底池,而不是把限流错误抛回客户端。
///
/// 其余可换号错误(TokenInvalid/ServerError/Network/Other)仍守 `general_cap`(默认
/// max_switch_attempts=2)—— 2026-06 大面积封号雪崩正是这类「毒请求/高频重试」逐个打爆
/// 全池,绝不放开。(EmptyResponse/TemporarilyBlocked/BadRequest 更早被
/// `worth_switching_account()=false` 命中首个号即止,根本进不来这里。)
pub(super) fn switch_cap(kind: UpstreamErrorKind, total: usize, general_cap: usize) -> usize {
    if matches!(
        kind,
        UpstreamErrorKind::RateLimited
            | UpstreamErrorKind::QuotaExhausted
            // ModelNotAvailable 同属良性(不禁号/不传播毒性):需沿组内遍历到有该模型的号,
            // 且每失败一次即把该 (号,模型) 标记不可用、下一跳自动跳过,不会重复打同一个号。
            | UpstreamErrorKind::ModelNotAvailable
    ) {
        total.max(1)
    } else {
        general_cap
    }
}

/// 解析 router 透传的**分组名**头。
///
/// - 头缺席 → `Ok(None)`:worker 用全量账号池(未分组请求 / 旧 router 滚动升级期间),
///   行为与本次重构之前逐字节相同。
/// - 头存在但为空 → `Err`:**必须拒绝而不是当成缺席**。当成缺席等于把一个本该受成员集
///   限制的请求以全量权限放行,且没有任何痕迹。头是内网自产的,畸形只可能是版本不匹配
///   或被篡改,两种情况都该硬失败。
pub(super) fn parse_group_header(headers: &HeaderMap) -> Result<Option<String>, String> {
    let Some(raw) = headers.get(crate::GROUP_HEADER) else {
        return Ok(None);
    };
    let name = raw.to_str().map_err(|_| "分组头含非 ASCII 字符".to_string())?.trim();
    if name.is_empty() {
        return Err("分组头为空".into());
    }
    Ok(Some(name.to_string()))
}

/// 该 provider 的 worker 是否赋予最优控制调度能力:只给 kiro(计费模型只在 Kiro 上标定过),
/// 且未被环境变量急停。
pub(super) fn cost_index_allowed(provider: &dyn Provider) -> bool {
    provider.cost_index_capable() && cost_index_env_enabled()
}

/// 本 worker 是否允许最优控制调度器(环境变量急停开关)。`KIRO_COST_INDEX` 取
/// `0/false/off/no`(大小写不敏感)即关闭;不设或其它值 = 允许(真正生效还看设置)。
pub(super) fn cost_index_env_enabled() -> bool {
    !matches!(
        std::env::var("KIRO_COST_INDEX")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

/// 会话亲和键**按分组分命名空间**。
///
/// 必须这么做的原因:scheduler 的亲和表是全 worker 共用、按会话键索引的,而会话键由
/// **请求内容**派生(`Provider::affinity_key`),不含分组。两组若共用一个条目:某个正常
/// 客户的会话若钉在只属于正常组的号上,一个键名碰撞的低价请求会因该号不是自己组的成员
/// 而把它判为不合格 → 落进 `select_id` 的"primary 不可用 → 改选并当场转正、永不迁回"
/// 分支 → **永久改写正常客户的钉扎**,上游前缀缓存冷启动。分了命名空间,低价流量就只能
/// 动自己那一份条目。组名与会话键都不含 NUL,用 NUL 分隔。
///
/// `client` 非空时**再按客户 key 分一层**,见 `Provider::affinity_scoped_by_client`。
pub(super) fn group_scoped_affinity_key(
    group: Option<&str>,
    client: Option<&str>,
    key: Option<String>,
) -> Option<String> {
    let key = key?;
    let scoped = match client.filter(|c| !c.is_empty()) {
        Some(c) => format!("{c}\u{0}{key}"),
        None => key,
    };
    Some(match group {
        Some(g) => format!("{g}\u{0}{scoped}"),
        None => scoped,
    })
}


/// 一条消息的 `content` 是否为空。
///
/// 空的判定与 Anthropic 官方 API 对齐:空串、纯空白、空数组、以及**含有空 text 块**的数组
/// 都会被上游拒。注意最后一种——`[{"type":"text","text":""}]` 看起来"有内容",但上游同样拒收。
///
/// 非文本块(image / tool_use / tool_result / thinking …)不参与判空:一条只带图片、
/// 没有文字的消息是合法的。
/// 空 text 块判定:空串/纯空白/缺 text 字段(上游按块校验,不是按整条消息)。
pub(super) fn is_empty_text_block(b: &serde_json::Value) -> bool {
    b.get("type").and_then(|t| t.as_str()) == Some("text")
        && b.get("text")
            .and_then(|t| t.as_str())
            .is_none_or(|t| t.trim().is_empty())
}

/// 空内容占位符 —— **不能用纯空白**:2026-08-19 线上实测 Kiro 对纯空白 user content
/// 确定性 400 REQUEST_BODY_INVALID(assistant 纯空白则放行)。用可见的「(空)」,
/// 与 kiro converter 的 EMPTY_USER_CONTENT_PLACEHOLDER 同文,日志里一眼可辨。
pub(super) const EMPTY_MSG_PLACEHOLDER: &str = "(空)";

/// 入站自愈:清掉 `messages` 里的空 content,而不是整条请求打回去。
///
/// 返回 `Ok(修复记录)` —— 空 Vec 表示报文本来就干净;`Err(人话)` 表示无法修复。
///
/// 为什么不再硬拒(2026-08-19):旧版在此 400 并点名下标(当时实测零假阳性),但空块的
/// 头号来源是**被掐断的流** —— 流在 text 块开头后零增量就断,Claude Code 会把带空
/// `{"type":"text","text":""}` 的 assistant 轮写进本地会话历史,之后该会话每个请求都被拦,
/// 永久变砖。自动修复让会话自愈;每条修复落 warn 日志,发垃圾的客户端照样看得见。
///
/// 修复动作(保序、不动消息条数 —— 会话亲和键与计费口径不漂移):
/// - content 数组里的空 text 块:删除,其余块原样保留;
/// - 整条 content 因此变空(或本来就是空串/null/缺字段):按角色选占位符顶替 ——
///   user 用可见的「(空)」(Kiro 拒纯空白 user content),assistant 用单空格
///   (上游放行纯空白 assistant;不给模型塞可见文本);
/// - **例外仍拒**:最后一条当前轮 user 消息为空(不能替用户编造输入),报错点名下标。
pub(super) fn sanitize_message_contents(body: &mut serde_json::Value) -> Result<Vec<String>, String> {
    let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        // 缺 messages / 类型不对:交给下游既有路径处理,这里只管空 content 这一件事。
        return Ok(Vec::new());
    };
    let mut repairs = Vec::new();
    let last = msgs.len().saturating_sub(1);
    for (i, m) in msgs.iter_mut().enumerate() {
        let role = m
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("?")
            .to_string();
        // 非对象消息本就不合法,留给下游/上游报错,这里不猜。
        let Some(obj) = m.as_object_mut() else {
            continue;
        };
        let whole_empty = match obj.get_mut("content") {
            None | Some(serde_json::Value::Null) => true,
            Some(serde_json::Value::String(s)) => s.trim().is_empty(),
            Some(serde_json::Value::Array(blocks)) => {
                let before = blocks.len();
                blocks.retain(|b| !is_empty_text_block(b));
                if blocks.len() != before {
                    repairs.push(format!(
                        "messages[{i}] (role={role}) 删除 {} 个空 text 块",
                        before - blocks.len()
                    ));
                }
                blocks.is_empty()
            }
            // 其余类型(数字/布尔/对象)本就不是合法 content,交给上游报错,这里不动。
            Some(_) => false,
        };
        if whole_empty {
            if i == last && role == "user" {
                return Err(format!(
                    "messages[{i}] (role=user) 的 content 为空且是当前轮,无法自动修复\
                     (不能替用户编造输入)。请填入非空内容。"
                ));
            }
            let placeholder = if role == "user" {
                EMPTY_MSG_PLACEHOLDER
            } else {
                " "
            };
            obj.insert(
                "content".into(),
                serde_json::json!([{"type": "text", "text": placeholder}]),
            );
            repairs.push(format!(
                "messages[{i}] (role={role}) content 为空,已用占位符顶替"
            ));
        }
    }
    Ok(repairs)
}

/// 未挂 OpenAI 入口的 worker 对 `/v1/chat/completions`、`/v1/responses` 的明确拒绝
/// (哪些上游挂入口由 `Provider::openai_wire` 声明)。
pub(super) async fn unsupported_openai_endpoint() -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        [(crate::ENDPOINT_UNAVAILABLE_HEADER, "1")],
        Json(serde_json::json!({"error":{"message":"当前入口不可用","type":"invalid_request_error"}})),
    ).into_response()
}

/// `POST /v1/messages` —— 原生 Anthropic 入口(全部非 cursor 流量走这条)。
pub(super) async fn messages(
    State(st): State<Arc<WorkerState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    handle_chat(st, headers, body, Wire::Anthropic).await
}

/// `POST /v1/chat/completions` —— OpenAI ChatCompletions 入口(**仅 cursor 家族挂载**)。
///
/// 入站转成 Anthropic body 后走的就是 [`messages`] 那条链路,一个分支都不分:
/// 选号、会话亲和、租约、重试、计费、请求日志全部共用。
pub(super) async fn chat_completions(
    State(st): State<Arc<WorkerState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    openai_entry(st, headers, body, gw_core::openai::chat_req::convert_request).await
}

/// `POST /v1/responses` —— OpenAI Responses 入口(**仅 cursor 家族挂载**)。
pub(super) async fn responses(
    State(st): State<Arc<WorkerState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    openai_entry(st, headers, body, gw_core::openai::resp_req::convert_request).await
}

/// 两条 OpenAI 入口的共同前半段:解析 → 转换 → 交给 `handle_chat`。
///
/// **收 `Bytes` 而不是 `Json<Value>`**:后者的 extractor 在畸形 JSON 上直接返回 axum 的
/// 默认拒绝体(纯文本 / 非 OpenAI 形状),而那是在我方 handler 之前发生的,wire-aware
/// 的错误壳根本来不及套上 —— OpenAI SDK / NewAPI 按 `error` schema 解不出来
/// (对抗评审 Minimalist#4)。自己解就能保证**这条路径上每一个错误**都是 OpenAI 形状。
pub(super) async fn openai_entry(
    st: Arc<WorkerState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
    convert: fn(
        &serde_json::Value,
    ) -> Result<gw_core::openai::Converted, gw_core::openai::ConvertError>,
) -> axum::response::Response {
    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return openai_convert_error(&gw_core::openai::ConvertError::new(format!(
                "无效 JSON: {e}"
            )))
        }
    };
    match convert(&parsed) {
        Ok(c) => {
            // 静默降级留痕:客户端声明了我方没有的托管工具(web_search 等),
            // 我方丢掉后照常回答 —— 响应里几乎看不出来,不打日志就永远查不到。
            if !c.dropped_tools.is_empty() {
                tracing::warn!(
                    dropped = ?c.dropped_tools,
                    "OpenAI 入站请求声明了本通道不支持的托管工具,已丢弃(其余照常处理)"
                );
            }
            handle_chat(st, headers, c.body, c.wire).await
        }
        Err(e) => openai_convert_error(&e),
    }
}

/// 入站转换失败 → OpenAI 形状的 400。
///
/// **不占账号、不消耗配额**:与 `sanitize_message_contents` 同样的「在门口处理」思路,
/// 而且 `param` 会点名出问题的字段,客户端能直接定位。
pub(super) fn openai_convert_error(e: &gw_core::openai::ConvertError) -> axum::response::Response {
    tracing::debug!("OpenAI 入站请求非法,本地拒绝: {e}");
    (
        StatusCode::BAD_REQUEST,
        Json(gw_core::openai::openai_error_body(
            400,
            &e.message,
            e.param.as_deref(),
            None,
        )),
    )
        .into_response()
}

pub(super) async fn handle_chat(
    st: Arc<WorkerState>,
    headers: HeaderMap,
    body: serde_json::Value,
    wire: Wire,
) -> axum::response::Response {
    let mut req = ChatRequest::from_anthropic_body(body);
    // 请求日志(#③)采集:进入即计时。报文序列化(client/kiro)推迟到收尾的 blocking 任务里做,
    // 不在热路径(handler 入口)同步跑(审查 Skeptic#1)。
    let started_at = std::time::Instant::now();
    // 客户 key 归属:router 鉴权后经内网头透传(对外 Authorization 不到 worker)。
    let client_key = headers
        .get(crate::CLIENT_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    // 入站空 content 自愈:断流会在 Claude Code 会话历史里留下空 text 块,硬拒会让该会话
    // 永久变砖(2026-08-19 起由 400 改为修复;旧实测依据:失败样本命中 8/173、成功样本
    // 0/400)。修复不打断请求、不占账号;无法修复(当前轮 user 为空)才拒,报错点名下标。
    match sanitize_message_contents(&mut req.body) {
        Ok(repairs) if !repairs.is_empty() => {
            tracing::warn!(
                model = %req.model,
                client_key = %client_key,
                ?repairs,
                "入站请求含空 content,已自动修复"
            );
        }
        Ok(_) => {}
        Err(msg) => {
            tracing::warn!(model = %req.model, client_key = %client_key, "入站请求空 content 无法修复,拒绝: {msg}");
            return error_response(wire, StatusCode::BAD_REQUEST, &msg);
        }
    }
    // 客户 key 归属:router 鉴权后经内网头透传(对外 Authorization 不到 worker)。
    // 请求所属分组:头由 router 依据 key 的分组生成、经内网白名单转发(客户端伪造的
    // 同名头在白名单转发时被丢弃)。头缺席 = 未分组请求 → 全量池,与重构前逐字节相同。
    // 畸形头**拒绝而非忽略**:忽略等于把受限分组的请求当全量放行,静默提权。
    let group = match parse_group_header(&headers) {
        Ok(g) => g,
        Err(msg) => {
            tracing::error!("分组头非法,拒绝请求: {msg}");
            // `error.type` 必填,少了它严格校验的客户端解析不了(见 anthropic_error_type)。
            return error_response(wire, StatusCode::INTERNAL_SERVER_ERROR, &msg);
        }
    };
    // 组名 → 成员视图。**查不到的组给空视图**(→ GroupEmpty/503),绝不回落全量池:
    // 那等于让一个成员边还没同步过来的分组瞬间拿到全部账号。
    let view = group
        .as_deref()
        .map(|g| st.group_views.read().get(g).cloned().unwrap_or_default());
    // CC 自动模式的安全分类器(见 gates::automode):按热开关处理,默认 allow。
    //   allow:caio 本地按分类器语法直接放行(不占号、不走上游);
    //   haiku:改写模型(安全闸还在,只是便宜);off:原样走上游。
    // 位置:在**分组头校验之后、选号之前**。分组之后 —— allow 也要受「畸形分组头拒绝」
    // 约束,不能因为本地直答就绕过提权保护(2026-10-11 评审项,10-23 重建时丢失、10-27 补回);
    // 选号之前 —— haiku 要在 `account_supports_model` 过滤前改掉模型,否则选出来的号是按
    // 老模型挑的。只认 Anthropic 线缆:分类器是 CC 自己发的,OpenAI 入口不会有这种请求。
    // 改写前的原模型:改写后若这一组没有号能服务 haiku,必须退回原模型重选一次 —— 否则
    // 对外是 400「换模型」,而 CC 对分类器的任何非正常回答都按「拦截」处理。
    let mut automode_original_model: Option<String> = None;
    if matches!(wire, Wire::Anthropic) {
        match gates::automode::decide(&req.body) {
            gates::automode::Action::Pass => {}
            gates::automode::Action::Answer { kind } => {
                return gates::automode::answer::answer(&st, &req, &client_key, kind, started_at);
            }
            gates::automode::Action::Rewrite { model, kind } => {
                tracing::info!(
                    from = %req.model, to = %model, kind = %kind.as_str(),
                    "自动模式分类器:改写成便宜模型"
                );
                automode_original_model = Some(req.model.clone());
                // body 是权威源,两处必须一起改(见 ChatRequest 文档)。
                req.body["model"] = serde_json::json!(model);
                req.model = model;
            }
        }
    }
    // 号池熔断(见 worker::pool_breaker):短时间内多个号被封 / 掉 FREE → 整池暂停接单,
    // 等管理员手动恢复。放在自动模式分类器之后:分类器本地直答不占号、不碰上游,没必要拦
    // (CC 把分类器的任何异常回答都当「拦截」);其余一切选号之前都挡掉。
    if let Some(resp) = pool_breaker_gate(wire) {
        return resp;
    }
    // 批量任务拦截(见 gates::batch_guard):放在分类器之后(分类器本地直答的请求不计数)、
    // 选号之前(被拦的请求不占号、不碰上游)。两种线缆都看:批量脚本走 OpenAI 入口一样会封号。
    if let Some(resp) = batch_guard_gate(&st, &req, &client_key, wire, started_at) {
        return resp;
    }
    // 逐条筛查(见 gates::request_screen):测试探针 / 超大输入。同在选号之前,跟随
    // batch_guard_mode。10-14 上线、10-23 重建时丢失、10-28 补回。
    if let Some(resp) = request_screen_gate(&st, &req, &client_key, wire, started_at) {
        return resp;
    }
    // 拒答记忆(见 gates::refusal_guard):同一条请求被上游分类器拒答过 → 超次数本地回拒答;
    // 没超就**只许用第一次被拒的那个号**(下面选号的过滤闭包),钉的号用不了也本地回拒答,
    // 绝不换号 —— 客户端原样重发 + 每次重新选号,正是一条违规内容打满整个池的原因。
    let refusal_pin = match refusal_guard_gate(&st, &req, &client_key, wire, started_at) {
        Ok(pin) => pin,
        Err(resp) => return *resp,
    };
    // 会话亲和键 = provider 派生的 conversationId(Kiro)。None → 无亲和按负载选号。
    // cursor 还要再按客户 key 分一层 —— 不分就是跨客户串话,见 `affinity_scoped_by_client`。
    let client_scope = st.provider.affinity_scoped_by_client().then_some(client_key.as_str());
    let affinity_key = group_scoped_affinity_key(
        group.as_deref(),
        client_scope,
        st.provider.affinity_key(&req),
    );

    // 选号 + 发起 chat 的重试循环:token 失效(403/401)时刷新该号并对账号生命周期上报,
    // 换号重试;首包前的可重试错误最多走 total 个账号。committed(首包已出)后不重试。
    let total = st.scheduler.total().max(1);
    // 换号重试上限按错误类别分档(见 `switch_cap`):
    // - 良性冷却类(限速/额度耗尽):沿优先级阶梯下探到组内所有号(cap=total),落低优先级兜底池;
    // - 其余可换号错误:守 general_cap(默认 max_switch_attempts=2),防换号雪崩封号
    //   (2026-06 大面积封号根因正是让一个毒请求逐个打爆全池)。
    let general_cap = st.scheduler.max_switch_attempts().min(total).max(1);
    // **请求级重试总时限**。限流类错误的 switch_cap 是 `total`(全组),配上「排队等冷却」
    // 后每轮取号还各有一份 queue_wait 预算 —— 两者相乘,一个一直撞 429 的请求理论上能
    // 循环好几分钟。而下游 yapi 是 **300s 无 event 即中止**,超了客户只会看到一个
    // 更难查的中断。故在此处硬封顶:到点就把当前错误返回,让客户拿到明确结果。
    const RETRY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);
    let retry_started = std::time::Instant::now();
    // 最优控制调度器的选号提示。钉号键细到「会话 + 线程」(子代理各有各的热号);
    // 它只影响选号 —— `affinity_key` 照旧进 CallCtx,计费 / cache_sim / cursor 续聊不受影响。
    // 具备能力就传(两种算法下都据此维护热度表,切换算法时不会一波冷启动)。
    let pin_key = if st.scheduler.cost_index_capable() {
        group_scoped_affinity_key(group.as_deref(), client_scope, st.provider.thread_key(&req))
    } else {
        None
    };
    let pick_hints = scheduler::PickHints {
        pin_key: pin_key.as_deref(),
        // 输入量粗估只在新算法生效时算(一次遍历正文;不含 tool_result/图片,偏低)。
        // 该线程有实测值时调度器取两者较大者 —— 真正起作用的场景(有热号可选)必有实测值。
        input_tokens_est: if st.scheduler.cost_index_active() {
            gw_core::text_tokens::count_request_tokens(&req.body)
        } else {
            0
        },
        // 等热号的预算从请求开始算,跨换号重试不叠加。
        ci_wait_deadline: Some(retry_started + st.scheduler.cost_index_max_wait()),
    };
    let mut attempts = 0;
    loop {
        attempts += 1;
        // 1. 按会话亲和取并发租约(持有到流结束)。合格账号须支持本次模型
        //    (FREE 订阅不支持 opus,过滤掉避免 403 误杀,对齐 kiro.rs supports_opus);
        //    并剔除已知对本模型返 INVALID_MODEL_ID 的号(区域/档位未上线),路由到有该
        //    模型的号——否则亲和会反复选中同一不支持的号(ModelNotAvailable 不禁号)死循环。
        let lease = match st
            .scheduler
            .acquire_with_hints(
                affinity_key.as_deref(),
                &pick_hints,
                |a| {
                    if refusal_pin.as_ref().is_some_and(|p| p.account_id != a.account_id) {
                        return false;
                    }
                    if !st.provider.account_supports_model(a, &req.model)
                        || st.scheduler.is_model_unavailable(&a.account_id, &req.model)
                    {
                        return false;
                    }
                    true
                },
                view.as_ref(),
                // 「降层前先等高优先层的节流窗口」只在**请求开头的一小段时间窗**内允许。
                // 高优先层挂的是自购速刷号,被 429 节流那几百毫秒里降层 = 把量白送给低优先级
                // 兜底池(实测占比 14%)。但**必须封顶**:限流类错误的 switch_cap 是全组
                // (见 `switch_cap`),不封的话一个持续撞 429 的请求能在同一个号上来回弹到
                // 180s 总时限,省钱变成客户干等。窗口外照常降层 —— 结果与开关关闭时一致,
                // 绝不把错误抛给客户;窗口远小于 RETRY_DEADLINE,等待也就够不到那条硬线。
                //
                // ⚠️ 用**墙上时钟**而不是 `attempts`:后者对所有失败类别共用(凭证刷新失败、
                // ModelNotAvailable 都在消耗它),拿它当等待额度会出现「前两轮被无关错误吃掉、
                // 真撞上 429 时反而不许等」——正好是本开关要治的那个病。
                {
                    let w = st.scheduler.tier_hold_window();
                    !w.is_zero() && retry_started.elapsed() < w
                },
                // 请求级绝对截止:把 `RETRY_DEADLINE` 一路传进选号里的四条等待分支。
                // 不传的话每次 acquire 都重开一份本地预算,「两次换号各等一轮」就能
                // 越过 180s(对抗评审 [高])——而下游 yapi 是 300s 无 event 即中止,
                // 越线的代价是客户拿到一个比明确报错更难查的中断。
                Some(retry_started + RETRY_DEADLINE),
            )
            .await
        {
            Ok(l) => l,
            Err(e) => {
                // 拒答记忆钉的号用不了(被封 / 禁用 / 不在本组 / 等到时限):本地回拒答,不换号。
                // 必须排在 haiku 回退之前 —— 那条分支会 `continue` 重新选号。
                if let Some(p) = &refusal_pin {
                    tracing::warn!(
                        model = %req.model, account = %p.account_id, strikes = p.strikes,
                        "拒答记忆:钉的号此刻用不了({e}),本地回拒答"
                    );
                    return refusal_repeat_response(&st, &req, &client_key, wire, started_at, &p.message, p.strikes);
                }
                // 分类器改写成 haiku 后这一组没号能服务它(白名单/不可用窗口):退回原模型
                // 再选一次,而不是对外报 400 —— 对 CC 来说 400 和被拦截是一回事。
                if matches!(e, scheduler::AcquireError::NoModelSupport) {
                    if let Some(orig) = automode_original_model.take() {
                        tracing::warn!(
                            haiku = %req.model, back_to = %orig,
                            "自动模式分类器:改写后无号可服务,退回原模型"
                        );
                        req.body["model"] = serde_json::json!(orig.clone());
                        req.model = orig;
                        continue;
                    }
                }
                let code = acquire_error_status(&e);
                // 内部原因(哪一档耗尽 / 组里压根没成员)只进日志:它描述的是账号池形态,
                // 对客户既没用又泄底。客户端只拿到"换模型"还是"稍后重试"。
                tracing::warn!(
                    group = %group.as_deref().unwrap_or("-"),
                    model = %req.model,
                    "选号失败: {e}"
                );
                return error_response(wire, code, e.client_message());
            }
        };
        let account_id = lease.account_id().to_string();
        // 选号时的 suspend 世代快照:本 lease 后续所有成功/失败上报都带它,
        // 调度器据此丢弃「状态转换前就在途」的迟到回声(见 suspend_gen)。
        let suspend_gen = lease.suspend_gen;

        // 2. 确保该号有未过期 access_token(按需刷新,带 expires_at 检查 + 单飞)。
        //    刷新失败按 kind 处理:invalid_grant(TokenInvalid)永久禁用;transient
        //    (网络/5xx/429)只记 transient 失败、换号重试,不永久打死健康号。
        let account = match st.ensure_credentialed(lease.account.clone()).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(account = %account_id, kind = ?e.kind, "凭证刷新失败: {e}");
                st.scheduler.report_failure_with_detail(
                    &account_id,
                    e.kind,
                    suspend_gen,
                    &e.to_string().chars().take(300).collect::<String>(),
                );
                drop(lease);
                if !e.kind.worth_switching_account()
                    || retry_started.elapsed() >= RETRY_DEADLINE
                    || attempts >= switch_cap(e.kind, total, general_cap)
                {
                    return upstream_error_response_wire(wire, &e);
                }
                continue;
            }
        };
        // 企业/IdC 号确保带 profileArn(缺则 ListAvailableProfiles 发现+持久化);
        // social/builderid/已有值直接短路。发现失败不阻断,让上游自然报 400。
        let account = st.ensure_profile_arn(account).await;

        let ctx = CallCtx {
            account,
            // session_id / cache_key 用亲和键(= conversationId),与 cache_sim 同源。
            session_id: affinity_key.clone().unwrap_or_default(),
            cache_key: affinity_key.clone().unwrap_or_default(),
        };

        // 3. 发起上游 chat。首包前错误(committed=false)可处理:
        //    - TokenInvalid(403):access_token 失效 → **同号强制刷新一次并重试同号**
        //      (refresh-then-retry,不立刻换号,保住会话亲和/缓存);刷新或重试仍失败才换号。
        //    - BadRequest:请求本身问题,换号无益,直接返回。
        //    - Overloaded(模型级容量不足):**同号退避重试**(见 chat_with_overload_backoff),
        //      不换号、不记账号失败;退避用尽才透出 → 对外 529。
        //    - 其他可重试错误:上报失败、换号重试。
        match chat_with_overload_backoff(&st, &req, &ctx, &account_id, view.as_ref()).await {
            Ok(stream) => {
                // 服务端 web search:客户端声明了 `web_search_20250305` → 进执行回环
                // (反代自搜 + 注回),把首轮流当 round 1 续跑。其余流量字节一致走原路径。
                if let Some(spec) = crate::websearch::detect_web_search(&req.body) {
                    return finish_web_search_response(
                        st.clone(),
                        lease,
                        stream,
                        &req,
                        &client_key,
                        ctx,
                        spec,
                        started_at,
                        view.clone(),
                        wire,
                    )
                    .await;
                }
                return finish_response(
                    st.clone(),
                    lease,
                    stream,
                    &req,
                    &client_key,
                    ctx,
                    started_at,
                    wire,
                )
                .await;
            }
            // API Key 403:ksk_ 无可刷新,同号刷新是空操作、retry 同 key 必再 403 且放大上游
            // (审查 Skeptic#3/Architect#2)。故 apikey 的 403 **不走**同号刷新重试,直接落到下方
            // 通用失败分支:report_failure(TokenInvalid) 禁用该号 + 换号(误伤可 admin reset 复活)。
            Err(e) if e.delivery.may_have_executed() => {
                return fail_pre_first_byte(&st, lease, &req, &client_key, &ctx.account,
                    started_at, wire, &e, None, true);
            }
            Err(e)
                if e.kind == UpstreamErrorKind::TokenInvalid
                    && !st.provider.refresh_exempt(&ctx.account) =>
            {
                tracing::info!(account = %account_id, "chat 403 token 失效,尝试同号刷新后重试");
                // 传入这枚被拒的 access_token:并发 403 时,只有第一个进锁者真刷,
                // 其余发现 token 已被换掉即复用,避免 N 次重复上游刷新(审查 Skeptic#2)。
                let rejected = ctx.account.extra_str("access_token").map(|s| s.to_string());
                match st
                    .refresh_after_rejection(ctx.account.clone(), rejected.as_deref())
                    .await
                {
                    Ok(refreshed) => {
                        let retry_ctx = CallCtx {
                            account: refreshed,
                            session_id: affinity_key.clone().unwrap_or_default(),
                            cache_key: affinity_key.clone().unwrap_or_default(),
                        };
                        // 定频准入:这是同一个 lease 上的**第二次**上游调用,不拦就会超频
                        // (暖机 RPM=2 的号一次刷新重试就能发出第 3 次)。达限则**不硬发**:
                        // 该号没犯错(rt 刚证有效),绝不上报失败(那会按 TokenInvalid 永久禁用),
                        // 直接换号;预算/时限用尽则把原始错误透给客户。
                        if !st
                            .scheduler
                            .note_upstream_call(&retry_ctx.account.account_id, view.as_ref())
                        {
                            tracing::info!(account = %account_id,
                                "刷新后重试被 RPM 准入拦住(含暖机),换号");
                            drop(lease);
                            if retry_started.elapsed() >= RETRY_DEADLINE
                                || attempts >= switch_cap(e.kind, total, general_cap)
                            {
                                return upstream_error_response_wire(wire, &e);
                            }
                            continue;
                        }
                        match st.provider.chat(req.clone(), &retry_ctx).await {
                            Ok(stream) => {
                                if let Some(spec) =
                                    crate::websearch::detect_web_search(&req.body)
                                {
                                    return finish_web_search_response(
                                        st.clone(),
                                        lease,
                                        stream,
                                        &req,
                                        &client_key,
                                        retry_ctx,
                                        spec,
                                        started_at,
                                        view.clone(),
                                        wire,
                                    )
                                    .await;
                                }
                                return finish_response(
                                    st.clone(),
                                    lease,
                                    stream,
                                    &req,
                                    &client_key,
                                    retry_ctx,
                                    started_at,
                                    wire,
                                )
                                .await;
                            }
                            Err(e2) => {
                                // 刷新已成功(rt 证明有效)。若重试仍是 403 TokenInvalid,极可能
                                // profileArn 套错——付费 builderid 号被免费层固定共享 ARN 短路、
                                // 拿不到自己的 profile。镜像配额路径:强制发现真实 ARN 持久化后用真
                                // ARN 再重试一次,成功即救回(治「导入即入活跃池、客户 chat 抢在验活
                                // force_discover 前命中、用错 ARN 403」的竞态)。
                                let e2 = if e2.kind == UpstreamErrorKind::TokenInvalid
                                    && !e2.delivery.may_have_executed() {
                                    match st
                                        .discover_paid_profile_arn(
                                            &retry_ctx.account,
                                            "profileArn(chat 403 兜底强制发现)",
                                        )
                                        .await
                                    {
                                        Some(healed) => {
                                            let heal_ctx = CallCtx {
                                                account: healed,
                                                session_id: affinity_key
                                                    .clone()
                                                    .unwrap_or_default(),
                                                cache_key: affinity_key
                                                    .clone()
                                                    .unwrap_or_default(),
                                            };
                                            // 定频准入:profileArn 修复后的验证调用同样是真实
                                            // 上游调用;达限(含暖机)则不硬发。
                                            // ⚠️ 被闸门拦住 ≠ 修复失败(对抗审查复审 [高]):
                                            // ARN 已修好并持久化,号是好的 —— 绝不能把 e2 透给
                                            // 下方的 TokenInvalid 上报(那会把刚救回的号永久
                                            // 误禁 invalid_refresh_token;暖机一期 RPM=2 下这是
                                            // 确定性路径)。照 token 刷新分支语义:释放 lease
                                            // 换号,预算/时限用尽才透原始错误。
                                            if !st.scheduler.note_upstream_call(
                                                &heal_ctx.account.account_id,
                                                view.as_ref(),
                                            ) {
                                                tracing::info!(account = %account_id,
                                                    "profileArn 修复验证被 RPM 准入拦住(含暖机),换号(不上报失败)");
                                                drop(lease);
                                                if retry_started.elapsed() >= RETRY_DEADLINE
                                                    || attempts
                                                        >= switch_cap(e2.kind, total, general_cap)
                                                {
                                                    return upstream_error_response_wire(wire, &e2);
                                                }
                                                continue;
                                            }
                                            match st.provider.chat(req.clone(), &heal_ctx).await {
                                                Ok(stream) => {
                                                    if let Some(spec) =
                                                        crate::websearch::detect_web_search(
                                                            &req.body,
                                                        )
                                                    {
                                                        return finish_web_search_response(
                                                            st.clone(),
                                                            lease,
                                                            stream,
                                                            &req,
                                                            &client_key,
                                                            heal_ctx,
                                                            spec,
                                                            started_at,
                                                            view.clone(),
                                                            wire,
                                                        )
                                                        .await;
                                                    }
                                                    return finish_response(
                                                        st.clone(),
                                                        lease,
                                                        stream,
                                                        &req,
                                                        &client_key,
                                                        heal_ctx,
                                                        started_at,
                                                        wire,
                                                    )
                                                    .await;
                                                }
                                                // 带真 ARN 仍失败 → 真死号/真封禁,携 e3 继续统一失败处理。
                                                Err(e3) => e3,
                                            }
                                        }
                                        None => e2,
                                    }
                                } else {
                                    e2
                                };
                                // 刷新成功后仍失败:上报**真实** e2.kind + 换号。heal 已把可救的
                                // profileArn 套错救回(救回则上面直接 return);走到这里=该号确实当前
                                // 不能服务——e3(带真 ARN 仍 403)是最强死号信号、或非付费/发现失败,
                                // kiro 一律保留原分类语义(TokenInvalid→invalid_refresh_token 永久禁用),
                                // 不弱化死号识别。注:「刷新成功⇒rt 有效」只证认证有效,不代表 entitlement
                                // 未被服务端撤销,故 kiro 不能据此把 TokenInvalid 一律降级(对抗审查 HIGH)。
                                // cursor 例外:旧 inference 面 2026-09-09 起对**所有**号退役,
                                // 重试的 401 与号无关,降级 Other(见 Provider::reclassify_retry_error)。
                                let e2 = st.provider.reclassify_retry_error(e2);
                                tracing::warn!(account = %account_id, kind = ?e2.kind, "刷新后重试仍失败: {e2}");
                                // 连带回声不扣账号健康(口径同 should_report_inference_failure)。
                                if !e2.collateral_failure {
                                    st.scheduler.report_failure_with_detail(
                                        &account_id,
                                        e2.kind,
                                        suspend_gen,
                                        &e2.to_string().chars().take(300).collect::<String>(),
                                    );
                                }
                                drop(lease);
                                if e2.delivery.may_have_executed()
                                    || !e2.kind.worth_switching_account()
                                    || retry_started.elapsed() >= RETRY_DEADLINE
                                    || attempts >= switch_cap(e2.kind, total, general_cap)
                                {
                                    return upstream_error_response_wire(wire, &e2);
                                }
                                continue;
                            }
                        }
                    }
                    Err(re) => {
                        // 刷新失败:invalid_grant→永久禁用;transient→换号重试。
                        tracing::warn!(account = %account_id, kind = ?re.kind, "同号刷新失败: {re}");
                        st.scheduler.report_failure_with_detail(
                            &account_id,
                            re.kind,
                            suspend_gen,
                            &re.to_string().chars().take(300).collect::<String>(),
                        );
                        drop(lease);
                        if !re.kind.worth_switching_account()
                            || retry_started.elapsed() >= RETRY_DEADLINE
                            || attempts >= switch_cap(re.kind, total, general_cap)
                        {
                            return upstream_error_response_wire(wire, &re);
                        }
                        continue;
                    }
                }
            }
            Err(e) => {
                let kind = e.kind;
                tracing::warn!(account = %account_id, kind = ?kind, "chat 失败: {e}");
                // 连带回声不扣账号健康(口径同 should_report_inference_failure)。
                if !e.collateral_failure {
                    st.scheduler.report_failure_with_detail(
                        &account_id,
                        kind,
                        suspend_gen,
                        &e.to_string().chars().take(300).collect::<String>(),
                    );
                }
                // 该号对本模型不可用(INVALID_MODEL_ID):记 (号,模型) 不可用,后续选号跳过它、
                // 路由到有该模型的号(该号**不禁用**,仍服务其它模型)。
                if kind == UpstreamErrorKind::ModelNotAvailable {
                    st.scheduler.mark_model_unavailable(&account_id, &req.model);
                }
                drop(lease);
                if !kind.worth_switching_account()
                    || retry_started.elapsed() >= RETRY_DEADLINE
                    || attempts >= switch_cap(kind, total, general_cap)
                {
                    // 终态失败(首包前):落一条失败请求日志,让"失败"筛选能看到上游 400/耗尽
                    // (生产 400 风暴正是此类)。无 usage/ttfb;detach 到 blocking 线程池。
                    // 与客户端实收状态码同源(见 upstream_status):
                    // BadRequest/ModelNotAvailable→400、Overloaded→529、其余 502。
                    let status = Some(upstream_status(kind).as_u16() as i64);
                    spawn_request_log_blocking(
                        &st,
                        req.clone(),
                        Some(ctx.account.clone()),
                        client_key.clone(),
                        req.stream,
                        false,
                        status,
                        Some(format!("{kind:?}")),
                        Some(started_at.elapsed().as_millis() as i64),
                        None,
                        None,
                        ResponseLog::None, // 首包前失败:无模型回复。
                    );
                    return upstream_error_response_wire(wire, &e);
                }
                continue;
            }
        }
    }
}

/// 终态首包前失败的统一出口(零产出/上游拒答/web search 回环失败):
/// 可选上报 + 失败请求日志 + 释放 lease + 对外错误响应,四处口径一致。
///
/// - `report_failure=true`:与主 handler「chat 失败」分支及流内 Err 分支同口径
///   (report_failure_with_gen);账号健康隔离纪律由 scheduler 按 kind 自担
///   (BadRequest/ModelNotAvailable/Overloaded 不扣健康,见 spares_account_health)。
/// - `report_failure=false`:零产出流,刻意**既不报成功也不报失败**(旧路径
///   StreamCtx::Drop 会 report_success_observed(false) 顺带清 429 连击,这里保守不动)。
#[allow(clippy::too_many_arguments)]
pub(super) fn fail_pre_first_byte(
    st: &Arc<WorkerState>,
    lease: scheduler::AccountLease,
    req: &ChatRequest,
    client_key: &str,
    account: &Arc<Account>,
    started_at: std::time::Instant,
    wire: Wire,
    e: &gw_core::error::UpstreamError,
    // 窥探缓冲里抢救出的 Usage(若有)。失败请求上游已产出的 token 该计还得计
    // (口径同 StreamCtx::drop 的 #130 块,account_ok=false)。
    usage: Option<ChatUsage>,
    report_failure: bool,
) -> axum::response::Response {
    if report_failure && should_report_inference_failure(st.provider.as_ref(), account, e) {
        st.scheduler.report_failure_with_detail(
            lease.account_id(),
            e.kind,
            lease.suspend_gen,
            &e.to_string().chars().take(300).collect::<String>(),
        );
    }
    if let (Some(sink), Some(u)) = (st.usage_sink.clone(), usage.clone()) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let guard = st.pending_writes.enter();
            let account_id = account.account_id.clone();
            let model = req.model.clone();
            let client_key = client_key.to_string();
            handle.spawn(async move {
                let _guard = guard;
                finalize_usage(Some(&sink), &account_id, &model, &client_key, Some(&u), false)
                    .await;
            });
        }
    }
    spawn_request_log_blocking(
        st,
        req.clone(),
        Some(account.clone()),
        client_key.to_string(),
        req.stream,
        false,
        Some(upstream_status(e.kind).as_u16() as i64),
        Some(format!("{:?}", e.kind)),
        Some(started_at.elapsed().as_millis() as i64),
        None,
        usage,
        ResponseLog::Error(upstream_error_payload(e)),
    );
    drop(lease);
    upstream_error_response_wire(wire, e)
}

/// 停止重放不等于已证明 refresh token 永久失效；续收鉴权失败保留诊断而不判死。
/// API key 无可刷新凭据，维持既有判定。
///
/// provider 标了「已处置故障的连带回声」(`collateral_failure`)的报错一律不上报:
/// 同一个根因只该扣一次账号健康(见 [`UpstreamError::as_collateral_failure`])。
pub(super) fn should_report_inference_failure(provider: &dyn Provider, account: &Account, error: &UpstreamError) -> bool {
    if error.collateral_failure {
        return false;
    }
    !(error.delivery.may_have_executed()
        && error.kind == UpstreamErrorKind::TokenInvalid
        && !provider.refresh_exempt(account))
}

/// `Overloaded` 的同号退避梯度(毫秒)。长度即重试次数上限。
///
/// 只覆盖"上游容量秒级抖动"这一种场景,故总等待封顶约 3s——再长客户端体感就不如直接
/// 吐 529 让它自己重试。2026-07-25 实测尖峰持续约 10 分钟但**单点**空窗在秒级。
pub(super) const OVERLOAD_BACKOFF_MS: &[u64] = &[250, 750, 2_000];

/// 0~40% 的正向抖动。**必需**:并发请求会在同一波容量抖动里齐刷刷失败,无抖动会同步重撞,
/// 把重试变成新的尖峰。熵取 uuid v4 的一个字节(workspace 已依赖 uuid+fast-rng,不为此引入 rand)。
pub(super) fn jittered(base_ms: u64) -> std::time::Duration {
    // 先把熵字节(0..=255)折成百分比 0..=40,再按百分比加成——直接拿字节做系数容易算错倍率。
    let pct = uuid::Uuid::new_v4().as_bytes()[0] as u64 * MAX_JITTER_PCT / 255;
    std::time::Duration::from_millis(base_ms + base_ms * pct / 100)
}

/// 抖动上限(百分比)。测试 `overload_backoff_is_bounded_and_jittered_upward` 据此断言区间。
pub(super) const MAX_JITTER_PCT: u64 = 40;

/// 发一次上游 chat,并按**模型级过载窗口**校正错误类型(见 [`correct_overload_kind`])。
///
/// 校正放在这一个点上,让下游(退避判定、`report_failure`、状态码、请求日志)全部看到
/// 同一个已校正的 kind,不必各自重算。
pub(super) async fn chat_once_corrected(
    st: &Arc<WorkerState>,
    req: &ChatRequest,
    ctx: &CallCtx,
) -> Result<gw_core::provider::ChatStream, gw_core::error::UpstreamError> {
    match st.provider.chat(req.clone(), ctx).await {
        Ok(s) => Ok(s),
        Err(mut e) => {
            // 本机容量排满(如 cursor CLI 进程名额)不是上游信号:不开模型过载窗口,
            // 否则别的号在这个模型上的普通 5xx 也会被改判成过载(见 local_capacity)。
            if !e.local_capacity {
                e.kind = correct_overload_kind(&st.scheduler, &req.model, e.kind);
            }
            Err(e)
        }
    }
}

/// 按模型级过载窗口校正错误类型。
///
/// - 收到**显式**过载(`Overloaded`,即上游给了 `MODEL_TEMPORARILY_UNAVAILABLE`)→ 开窗并原样返回。
/// - 窗口内的通用 `ServerError` → 升级为 `Overloaded`。依据:2026-07-25 实测 176 条
///   `reason:null` 的通用 5xx 中 **84.7% 与显式过载落在同一分钟**,是同一波容量抖动;
///   上游只是有时不填 `reason`。
/// - 其余一律原样返回。**窗口外绝不升级**——重分类必须有上游显式信号背书,不靠猜。
///
/// 只在**首包前**的错误上调用(2026-07-25 观测到的 276 次 5xx 全部 `ttfb=NULL`,即都发生在
/// 首包前)。流中途冒出的 5xx 仍走 `finish_response` 的原路径,不做校正。
pub(super) fn correct_overload_kind(
    scheduler: &AccountScheduler,
    model: &str,
    kind: UpstreamErrorKind,
) -> UpstreamErrorKind {
    if kind == UpstreamErrorKind::Overloaded {
        scheduler.mark_model_overloaded(model);
        return kind;
    }
    if kind == UpstreamErrorKind::ServerError && scheduler.is_model_overloaded(model) {
        tracing::info!(
            model = %model,
            "通用 5xx 落在该模型的过载窗口内,按过载处理(不惩罚账号/不换号)"
        );
        return UpstreamErrorKind::Overloaded;
    }
    kind
}

/// 发起上游 chat,遇 [`UpstreamErrorKind::Overloaded`] 时**在同一个号上**退避重试。
///
/// 与上方 TokenInvalid 的 refresh-then-retry 同构:握着同一枚 lease、同一个 `ctx` 重发。
/// 这样做的三个理由:
/// 1. 上游说的是"这个**模型**现在没容量",与用哪个号无关——换号打的还是同一个模型端点。
/// 2. 保住会话 cache 亲和。实测一次 opus-5 请求 `cache_read` 可达 10.7 万 token,换号全部重算。
/// 3. **爆炸半径 = 1 个号**,比换号重试(默认 2 个号)更小,故与 2026-06 防雪崩的
///    `max_switch_attempts` 硬上限不冲突,**无需放开后者**。
///
/// 退避用尽仍过载 → 原样返回 `Err`,由调用方走终态路径(`worth_switching_account()=false`
/// → 不换号,直接对外 529)。
pub(super) async fn chat_with_overload_backoff(
    st: &Arc<WorkerState>,
    req: &ChatRequest,
    ctx: &CallCtx,
    account_id: &str,
    // 成员视图:追加调用的 RPM 准入与选号同口径(见 note_upstream_call)。
    view: Option<&scheduler::GroupView>,
) -> Result<gw_core::provider::ChatStream, gw_core::error::UpstreamError> {
    let mut last = match chat_once_corrected(st, req, ctx).await {
        Ok(s) => return Ok(s),
        Err(e) if !e.delivery.may_have_executed() && e.kind.worth_same_account_backoff() => e,
        Err(e) => return Err(e),
    };
    for (i, base_ms) in OVERLOAD_BACKOFF_MS.iter().enumerate() {
        let wait = jittered(*base_ms);
        tracing::info!(
            account = %account_id, model = %req.model, attempt = i + 1,
            wait_ms = wait.as_millis() as u64,
            "上游模型过载,同号退避重试(不换号/不记账号失败)"
        );
        tokio::time::sleep(wait).await;
        // 定频准入在退避**完成之后、发出之前**(对抗审查复审 [中]):先记账再睡的话,
        // 任务在睡眠期被取消会留下从未发送的假 hit,且滑动窗口按预留时刻过期,
        // 窗口边界会提前放出名额。首发名额已在 select_id 预留,这里拦的是追加调用;
        // 检查+记账原子(见 note_upstream_call)。达限(含暖机)不再硬发:按退避用尽
        // 处理,返回最近一次过载错误 → 对外 529(客户端自动重试),与预算耗尽显同一路径。
        if !st.scheduler.note_upstream_call(account_id, view) {
            tracing::info!(account = %account_id, model = %req.model,
                "过载退避重试被 RPM 准入拦住(含暖机),按退避用尽处理");
            return Err(last);
        }
        match chat_once_corrected(st, req, ctx).await {
            Ok(s) => {
                tracing::info!(account = %account_id, model = %req.model, attempt = i + 1,
                    "同号退避重试成功");
                return Ok(s);
            }
            // 退避期间错误类型可能变(如容量恢复但换成 403):非过载即刻透出,别用过载预算硬打。
            Err(e) if !e.delivery.may_have_executed() && e.kind.worth_same_account_backoff() => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}
