//! 响应收尾:非流式抽干折叠、流式转发(保活、首包窥探、流内 401 重试、线缆换形)。

use super::*;

/// 服务端 web search 收尾:跑执行回环(反代自搜 DDG + 注回续跑),把组装好的标准
/// `server_tool_use`+`web_search_tool_result`+text 响应合成为流,再交给 [`finish_response`]
/// 复用全部流式/非流式分发、日志、usage 上报机器。回环失败按上游错误回显(不重试:已开始
/// 消费首轮流,符合 v60 不放大错误契约)。
#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_web_search_response(
    st: Arc<WorkerState>,
    lease: scheduler::AccountLease,
    first_stream: gw_core::provider::ChatStream,
    req: &ChatRequest,
    client_key: &str,
    ctx: CallCtx,
    spec: crate::websearch::WebSearchSpec,
    started_at: std::time::Instant,
    // 成员视图:续轮的 RPM 准入与选号同口径(见 note_upstream_call)。
    view: Option<scheduler::GroupView>,
    wire: Wire,
) -> axum::response::Response {
    let account = ctx.account.clone();
    let account_id = account.account_id.clone();
    // 定频准入回调:web search 的每一次续轮上游调用都要过该号的有效 RPM 闸
    // (含暖机),达限即停发、优雅降级收尾。
    let st_for_rpm = st.clone();
    let acct_for_rpm = account_id.clone();
    let on_call = move || st_for_rpm.scheduler.note_upstream_call(&acct_for_rpm, view.as_ref());
    match crate::websearch::run_loop(
        st.provider.clone(),
        &ctx,
        req,
        spec,
        first_stream,
        &on_call,
    )
    .await
    {
        Ok((events, usage)) => {
            let synth = crate::websearch::synth_stream(events, usage);
            finish_response(st, lease, synth, req, client_key, ctx, started_at, wire).await
        }
        Err(e) => {
            tracing::warn!(account = %account_id, kind = ?e.kind, "web search 回环失败: {e}");
            // 统一出口:上报 + 失败请求日志(原先此分支漏记,失败在日志里不可见)
            // + 释放 lease + 对外错误响应(2026-08-24 codex 审查 major#3)。
            // websearch 的零产出**已烧过上游搜索回环**,按失败上报(report=true)——
            // 与普通流的零事件窥探分支(report=false)不同是有意的:那条没烧上游。
            fail_pre_first_byte(&st, lease, req, client_key, &account, started_at, wire, &e, None, true)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_response(
    st: Arc<WorkerState>,
    lease: scheduler::AccountLease,
    stream: gw_core::provider::ChatStream,
    req: &ChatRequest,
    client_key: &str,
    ctx: CallCtx,
    started_at: std::time::Instant,
    wire: Wire,
) -> axum::response::Response {
    if req.stream {
        // 流式:首项窥探后返回 SSE 响应,收尾走 StreamCtx::Drop(同步上报 + detach 落库 usage+请求日志)。
        stream_response(
            st,
            lease,
            stream,
            req.clone(),
            client_key.to_string(),
            ctx,
            started_at,
            wire,
            false,
        )
        .await
    } else {
        // 非流式:此处即时抽干流、折叠成单个 Messages JSON。
        collect_response(
            &st.scheduler,
            st.usage_sink.as_ref(),
            st.store.clone(),
            Some(st.pending_writes.clone()),
            lease,
            stream,
            req.clone(),
            client_key.to_string(),
            ctx.account.clone(),
            started_at,
            &st.provider,
            wire,
            Some((&st, &ctx)),
            false,
        )
        .await
    }
}

/// 非流式路径:抽干 provider 流,折叠成单个 Anthropic Messages JSON 响应。
///
/// 抽干期间持有 `lease`(占并发槽);抽干完成后按结果上报账号生命周期 + usage 落库
/// (与流式 [`stream_response`] 同口径)。流中出现硬错误 / SSE `error` 事件 → 回上游
/// 错误响应(不重试:已开始消费流,符合 v60 不放大错误契约)。
///
/// 取显式依赖(scheduler / usage_sink)而非整个 WorkerState,便于单测。
/// `retry` 是流内 TokenInvalid「同号刷新重试」所需的 worker 全态 + 原 CallCtx;
/// 测试传 None = 不重试(旧行为)。`token_retried` 防递归重试。
#[allow(clippy::too_many_arguments)]
pub(super) async fn collect_response(
    scheduler: &AccountScheduler,
    usage_sink: Option<&Arc<dyn UsageSink>>,
    store: Option<Arc<SqliteStore>>,
    pending_writes: Option<Arc<PendingWrites>>,
    lease: scheduler::AccountLease,
    mut stream: gw_core::provider::ChatStream,
    req: ChatRequest,
    client_key: String,
    account: Arc<Account>,
    started_at: std::time::Instant,
    provider: &Arc<dyn Provider>,
    wire: Wire,
    retry: Option<(&Arc<WorkerState>, &CallCtx)>,
    token_retried: bool,
) -> axum::response::Response {
    /// 非流式抽干的事件数上限(OOM 粗护栏:正常响应 < 数万事件,远低于此;
    /// 超出视为异常上游,回受控错误而非无界吃内存。审查 #3)。
    const MAX_NONSTREAM_EVENTS: usize = 500_000;

    let account_id = lease.account_id().to_string();
    // 出站换形状要的两样东西(`req` 稍后被 move 进落库任务,先取):兜底模型名,
    // 以及 Responses 对象要回显的请求参数。
    let req_model = req.model.clone();
    let req_echo = gw_core::openai::resp_out::RequestEcho::from_anthropic(&req.body);
    let mut events: Vec<SseEvent> = Vec::new();
    let mut last_usage: Option<ChatUsage> = None;
    let mut hard_err: Option<UpstreamError> = None;
    let mut over_cap = false;

    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamItem::Sse(ev)) => {
                if events.len() >= MAX_NONSTREAM_EVENTS {
                    over_cap = true;
                    break;
                }
                events.push(ev);
            }
            Ok(StreamItem::Usage(u)) => {
                last_usage = Some(u);
            }
            // 非流式抽干:掐流信号只对流式主链路的软冷却有意义,这里忽略
            // (响应完整性由下方的 message_stop 折叠校验兜底,行为不变)。
            Ok(StreamItem::UpstreamCut) => {}
            Err(e) => {
                // 对外只发中性文案(见 upstream_error_response),原文必须在这里落日志,
                // 否则这条路径的上游报文就彻底没人记了。
                tracing::warn!(account = %account_id, kind = ?e.kind, "非流式抽干时上游错误: {e}");
                hard_err = Some(e);
                break;
            }
        }
    }

    // 流内 TokenInvalid(cursor 系 401 以流内错误帧形态到达,主 handler 的同步
    // refresh-after-rejection 摸不到):先同号刷新重试一次,刷新也失败才按 refresh
    // 的 kind 定生死 —— 与 stream_response 窥探分支同口径(2026-09-08 ultra-test
    // 事故:RT 活着却被一次流内 401 永久误杀)。
    if let (Some(e), Some((st, ctx))) = (&hard_err, retry) {
        if e.kind == UpstreamErrorKind::TokenInvalid
            && !e.delivery.may_have_executed()
            && !token_retried
            && !st.provider.refresh_exempt(&account)
        {
            {
                let rejected = ctx.account.extra_str("access_token").map(|s| s.to_string());
                match st
                    .refresh_after_rejection(ctx.account.clone(), rejected.as_deref())
                    .await
                {
                    Ok(refreshed) => {
                        if !st.scheduler.note_upstream_call(&refreshed.account_id, None) {
                            // 被 RPM 拦住 ≠ 修复失败(号刚证明健康):按 Overloaded 走下方
                            // 统一失败路径 —— 该 kind 不伤账号健康,客户端拿 529 自行重试。
                            tracing::info!(account = %account_id,
                                "非流式 401 刷新成功,重试被 RPM 准入拦住(不上报失败)");
                            hard_err = Some(gw_core::error::UpstreamError::new(
                                UpstreamErrorKind::Overloaded,
                                "刷新后重试被 RPM 准入拦截",
                            ));
                        } else {
                            let retry_ctx = CallCtx {
                                account: refreshed,
                                session_id: ctx.session_id.clone(),
                                cache_key: ctx.cache_key.clone(),
                            };
                            match st.provider.chat(req.clone(), &retry_ctx).await {
                                Ok(new_stream) => {
                                    tracing::info!(account = %account_id,
                                        "非流式 401 同号刷新成功,透明重试");
                                    return Box::pin(collect_response(
                                        &st.scheduler,
                                        st.usage_sink.as_ref(),
                                        st.store.clone(),
                                        Some(st.pending_writes.clone()),
                                        lease,
                                        new_stream,
                                        req,
                                        client_key,
                                        retry_ctx.account.clone(),
                                        started_at,
                                        &st.provider,
                                        wire,
                                        Some((st, &retry_ctx)),
                                        true,
                                    ))
                                    .await;
                                }
                                // 重试的真实 kind 走统一失败;是否降级由上游自己判
                                // (Provider::reclassify_retry_error,cursor 降级、kiro 维持原判)。
                                Err(e2) => hard_err = Some(st.provider.reclassify_retry_error(e2)),
                            }
                        }
                    }
                    Err(e2) => hard_err = Some(e2), // 刷新失败的 kind 才是真实判据
                }
            }
        }
    }

    // 透明重试(递归调用,token_retried=true)的 401 以**流内错误帧**形态在抽干
    // 阶段到达:上面的 demote 只包得住 chat() 同步 Err,包不住这里 —— cursor 旧
    // inference 死面对所有号 401,刷新刚成功就证明凭据活着,补降级防误杀
    // (kiro 不适用,见各上游的 Provider::reclassify_retry_error)。
    if token_retried {
        hard_err = hard_err.map(|e| provider.reclassify_retry_error(e));
    }

    // 先定结果,再统一收尾(账号生命周期 + usage 落库),保证 success 与真实结果一致
    // (审查 #1:不能在折叠失败前就抢报 success / 记 success=true)。
    enum Outcome {
        Ok(serde_json::Value),
        Upstream(UpstreamError),
        Bad(serde_json::Value),
    }
    let outcome = if let Some(e) = hard_err {
        Outcome::Upstream(e)
    } else if over_cap {
        Outcome::Bad(serde_json::json!({"type":"error","error":{"type":"api_error",
            "message":"非流式响应事件数超上限,已中止"}}))
    } else {
        match gw_core::fold::fold_sse_to_message(&events) {
            Ok(msg) => Outcome::Ok(msg),
            Err(err_data) => Outcome::Bad(err_data),
        }
    };

    let success = matches!(outcome, Outcome::Ok(_));
    // 非流式状态码:成功 200;上游错误与客户端实收同源(见 upstream_status:BadRequest/
    // ModelNotAvailable→400、Overloaded→529、其余 502),折叠失败 502。
    // 详情据此回显,不留空(审查 low#7)。
    let (status_code, error_kind): (Option<i64>, Option<String>) = match &outcome {
        // 答到一半被分类器拦下:成功口径不变,只打标记(见流式同名判定)。
        Outcome::Ok(msg) if is_refusal_message(msg) => {
            (Some(200), Some(CONTENT_FILTERED_KIND.to_string()))
        }
        Outcome::Ok(_) => (Some(200), None),
        Outcome::Upstream(e) => (
            Some(upstream_status(e.kind).as_u16() as i64),
            Some(format!("{:?}", e.kind)),
        ),
        Outcome::Bad(_) => (Some(502), Some("bad_gateway".to_string())),
    };
    if success {
        // 非流式折叠成功 = 完整成功(消息体已收齐),可清 suspend 退避进度。
        scheduler.report_success_observed(&account_id, lease.suspend_gen, true);
        // 成功出过完整响应:该线程在这个号上变热,并记下实测输入量(最优控制调度器用)。
        scheduler.note_thread_usage(&lease, last_usage.as_ref().map(|u| u.input_tokens), true);
    } else {
        let (kind, detail) = match &outcome {
            Outcome::Upstream(e) => (
                e.kind,
                e.to_string().chars().take(300).collect::<String>(),
            ),
            _ => (UpstreamErrorKind::ServerError, String::new()),
        };
        if !matches!(&outcome, Outcome::Upstream(e) if !should_report_inference_failure(provider.as_ref(), &account, e)) {
            scheduler.report_failure_with_detail(&account_id, kind, lease.suspend_gen, &detail);
        }
        // 防御:理论上 INVALID_MODEL_ID 是首包前 400 走主循环,但若上游 mid-stream 冒出
        // 也在此记 (号,模型) 不可用,与主循环口径一致(不禁号 + 后续选号跳过该号)。
        if kind == UpstreamErrorKind::ModelNotAvailable {
            scheduler.mark_model_unavailable(&account_id, &req.model);
        }
    }
    finalize_usage(
        usage_sink,
        &account_id,
        &req.model,
        &client_key,
        last_usage.as_ref(),
        success,
    )
    .await;
    drop(lease); // 释放并发槽(响应已抽干);请求日志在其后 detach,不再占槽(审查 medium#4)。
    // 拒答记忆的记账(非流式收口;流式与失败路径在 spawn_request_log_blocking 里)。
    // 放在「库降级就跳过落库」之前。
    {
        let client_message = match &outcome {
            Outcome::Upstream(e) => Some(e.client_message()),
            _ => None,
        };
        refusal_guard_record(&req, Some(&*account), success, error_kind.as_deref(), client_message.as_deref());
    }
    // 请求日志落库:detach 到 blocking 线程池(序列化两份报文 + 写库),不延迟非流式响应、
    // 不占用 async worker 线程。store/pending_writes 缺失(测试/降级)→ 跳过。
    if let (Some(store), Some(pw), Ok(handle)) = (
        store,
        pending_writes,
        tokio::runtime::Handle::try_current(),
    ) {
        let guard = pw.enter();
        let duration_ms = Some(started_at.elapsed().as_millis() as i64);
        // 非流式:复用上面已折叠、即将下发客户端的同一份响应体(成功才有),不二次折叠——
        // 保证入库的 response_payload 与客户端实际收到的响应严格一致。失败 → 下发客户端的错误体。
        let response = match &outcome {
            Outcome::Ok(msg) => ResponseLog::Folded(msg.clone()),
            Outcome::Upstream(e) => ResponseLog::Error(upstream_error_payload(e)),
            // 与下方下发同一闸门:客户收到的是中性化后的错误体(原文已在 warn 日志里)。
            Outcome::Bad(body) => ResponseLog::Error(sanitize_upstream_error_payload(body)),
        };
        let log_provider = provider.clone();
        handle.spawn_blocking(move || {
            let _guard = guard;
            write_request_log(
                store,
                req,
                Some(account),
                client_key,
                false,
                success,
                status_code,
                error_kind,
                duration_ms,
                None,
                last_usage,
                response,
                log_provider.as_ref(),
            );
        });
    }

    match outcome {
        // 成功体按线缆换形状。**入库的仍是 Anthropic 那份**(上面的 ResponseLog::Folded),
        // 与流式路径同口径 —— 日志/语料链路只认一种形状。
        Outcome::Ok(msg) => {
            let body = match wire {
                Wire::Anthropic => msg,
                Wire::OpenAiChat { .. } => {
                    gw_core::openai::chat_out::fold_completion(&msg, &req_model)
                }
                Wire::OpenAiResponses => gw_core::openai::resp_out::fold_response(
                    &msg,
                    &req_model,
                    &req_echo,
                ),
            };
            (StatusCode::OK, Json(body)).into_response()
        }
        Outcome::Upstream(e) => upstream_error_response_wire(wire, &e),
        // 折叠失败体有两个来源:我方的折叠诊断,以及 `fold_sse_to_message` 对上游 error
        // 事件的**原样回传**(fold.rs:49)。后者是上游报文,不能直发客户 —— 与流式路径
        // 同一个闸门,原文落日志。
        Outcome::Bad(data) => {
            tracing::warn!(account = %account_id, "非流式折叠失败,回中性错误: {data}");
            let neutral = sanitize_upstream_error_payload(&data);
            let body = if wire.is_openai() {
                gw_core::openai::error::from_anthropic_error(&neutral, 502)
            } else {
                neutral
            };
            (StatusCode::BAD_GATEWAY, Json(body)).into_response()
        }
    }
}

/// 把 provider 的 StreamItem 流转成 axum SSE 响应,并在流结束时按结果上报账号生命周期
/// + 把终结 usage 落库(#130)。
///
/// 关键:`lease`(并发许可)被 move 进流的状态,持有到流耗尽才 Drop → 整个响应期间
/// 占用该账号一个并发槽,符合 v52 并发语义。流内出现 error 事件 / Err → 上报失败;
/// 干净结束 → 上报成功。usage 事件不转发客户端(缓存到 `last_usage`,流终态统一落库)。
/// 上游静默多久后开始发保活帧,并按此间隔重复。必须**远小于**下游客户端的空闲判定
/// 阈值:2026-07-26 取证四次卡死的静默时长为 120.5/123.3/124.8/126.0 秒,可推定客户端
/// 阈值约 120s,取 20s 留 6 倍余量。
pub(super) const STREAM_IDLE_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(20);
/// 上游连续静默的硬上限:超过即主动中止,按 `upstream_idle_abort` 收尾。
///
/// 它的意义是让本机制在**两种尚未证实的情形下都正确**:上游只是慢 → 保活已让客户端
/// 等得起,在本窗口内恢复即救回;上游已经死了 → 客户端拿到明确错误,而不是像现在这样
/// 静默两分钟被客户端自己砍断、库里还记成 200 成功。上线后两类日志的占比,即可回答
/// "慢还是死"——这是不额外消耗上游配额就能拿到该答案的唯一途径。
pub(super) const STREAM_IDLE_ABORT: std::time::Duration = std::time::Duration::from_secs(300);

/// 流式响应的**首项窥探**超时(2026-08-24 用户报障:拒答/空流只能看到 200 裸断)。
///
/// 上游拒答(疑 guardrail)/零产出错误的形态是"首项即 Err 或零项流",且来得很快
/// (生产实测 2-6s);而 grok 隐藏推理的首个内容要 20-60s。10s 足以干净切开:
/// 窗口内拿到 Err/None → 直接回真实错误状态与文案;超时 → 判定为慢首 token,
/// 回退现有 200 + keepalive 行为(零回归)。取值若需调整,按上游漂移再改,
/// 宁可保守(大)不可冒进(小)——小了会把长思考误判成"快路径"而毫无意义。
pub(super) const PEEK_FIRST_ITEM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 当前开着的内容块种类(带块索引)。保活帧必须**贴合当前块类型**:Anthropic SSE 里
/// 往 tool_use 块塞 text_delta 是非法的,客户端解析器会错乱。只跟踪这三种能安全附加
/// 零增量的块;其余(如 redacted_thinking,它没有增量形态)一律视为"无可附着的块"。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum OpenBlock {
    Text(u64),
    Thinking(u64),
    ToolUse(u64),
}

/// 据**已转发给客户端**的事件维护"当前开着哪个块"。只认 content_block_start/stop
/// 与 message_stop,其余事件不改变状态。
pub(super) fn track_open_block(cur: Option<OpenBlock>, ev: &SseEvent) -> Option<OpenBlock> {
    match ev.event.as_str() {
        "content_block_start" => {
            let Some(idx) = ev.data.get("index").and_then(serde_json::Value::as_u64) else {
                return cur;
            };
            match ev
                .data
                .get("content_block")
                .and_then(|b| b.get("type"))
                .and_then(serde_json::Value::as_str)
            {
                Some("text") => Some(OpenBlock::Text(idx)),
                Some("thinking") => Some(OpenBlock::Thinking(idx)),
                Some("tool_use") => Some(OpenBlock::ToolUse(idx)),
                // 其余块类型没有可安全附加的零增量形态 → 视为无块可附着。
                _ => None,
            }
        }
        "content_block_stop" | "message_stop" => None,
        _ => cur,
    }
}

/// 构造一个语义为 no-op 的保活帧(客户端把增量逐段拼接,拼空串等于没拼)。
///
/// **为什么不发 Anthropic 官方的 `ping`**:本链路下游是 NewAPI,其 Claude→OpenAI 转换器
/// 只识别 message_start / content_block_{start,delta} / message_{delta,stop} 五种事件,
/// 且**没有 default 兜底**(实测 `to_oai_chat_resp.go`)——`ping` 到那里直接消失,不会变成
/// 任何下游事件,客户端照样判定空闲。故改用"空增量":它属于上述五种之一,能被转换成一个
/// 真实的下游 chunk,从而重置客户端的空闲计时。
pub(super) fn keepalive_frame(open: Option<OpenBlock>) -> SseEvent {
    let delta = match open {
        Some(OpenBlock::ToolUse(i)) => {
            (i, serde_json::json!({"type":"input_json_delta","partial_json":""}))
        }
        Some(OpenBlock::Text(i)) => (i, serde_json::json!({"type":"text_delta","text":""})),
        Some(OpenBlock::Thinking(i)) => {
            (i, serde_json::json!({"type":"thinking_delta","thinking":""}))
        }
        // 没有开着的块时零增量无处附着,只能退回官方 ping。下游可能吃掉它,但此刻
        // 确实没有任何合法帧可发——总比什么都不发强,且对原生 Anthropic 客户端有效。
        None => return SseEvent::new("ping", serde_json::json!({"type":"ping"})),
    };
    SseEvent::new(
        "content_block_delta",
        serde_json::json!({"type":"content_block_delta","index":delta.0,"delta":delta.1}),
    )
}

/// 出站线缆转换器。`None`(= [`Wire::Anthropic`])时整条转发路径**一行都不改**。
///
/// 一条 Anthropic 事件在 OpenAI 侧可能产出 0..N 帧(如 `content_block_start(tool_use)`
/// 会同时开条目和发首个参数帧),所以接口是「进一条、出一个 Vec」,由调用方排队下发。
pub(super) enum OutConv {
    Chat(gw_core::openai::chat_out::ChatStreamOut),
    Responses(gw_core::openai::resp_out::ResponsesStreamOut),
}

impl OutConv {
    /// 按线缆建转换器;Anthropic 返回 `None`。
    ///
    /// `body` 是**转换后的 Anthropic 请求体**:Responses 的 `Response` 对象要回显
    /// `instructions` / `tools` / `tool_choice` / `parallel_tool_calls`,这些信息在 IR
    /// 里全都还在,反向映射即可 —— 不必把原始 OpenAI 请求一路带下来。
    fn for_wire(wire: Wire, model: &str, body: &serde_json::Value) -> Option<Self> {
        match wire {
            Wire::Anthropic => None,
            Wire::OpenAiChat { include_usage } => Some(Self::Chat(
                gw_core::openai::chat_out::ChatStreamOut::new(model, include_usage),
            )),
            Wire::OpenAiResponses => Some(Self::Responses(
                gw_core::openai::resp_out::ResponsesStreamOut::new(model).with_echo(
                    gw_core::openai::resp_out::RequestEcho::from_anthropic(body),
                ),
            )),
        }
    }

    fn push(&mut self, ev: &SseEvent) -> Vec<WireFrame> {
        match self {
            Self::Chat(c) => c.push(ev),
            Self::Responses(r) => r.push(ev),
        }
    }

    /// 流走到尽头(正常结束 / 上游断流 / 我方中止)时补齐终止序列。幂等。
    fn finish(&mut self) -> Vec<WireFrame> {
        match self {
            Self::Chat(c) => c.finish(),
            Self::Responses(r) => r.finish(),
        }
    }

    /// 硬错误 → 该线缆的失败形状。`data` 须**已中性化**。
    fn fail(&mut self, data: &serde_json::Value) -> Vec<WireFrame> {
        match self {
            Self::Chat(c) => c.fail(data),
            Self::Responses(r) => r.fail(data),
        }
    }

    /// 保活帧。可能多于一帧:Responses 在首个上游事件迟到时要先补 `response.created`。
    fn keepalive(&mut self) -> Vec<WireFrame> {
        match self {
            Self::Chat(c) => vec![c.keepalive()],
            Self::Responses(r) => r.keepalive(),
        }
    }

    /// 终止序列已发过。之后**不得再发保活** —— `[DONE]` / `response.completed`
    /// 之后冒出新帧,严格客户端会当成协议违例。
    fn is_finished(&self) -> bool {
        match self {
            Self::Chat(c) => c.is_finished(),
            Self::Responses(r) => r.is_finished(),
        }
    }
}

/// [`WireFrame`] → axum SSE 事件(gw-core 不认识 axum,转换只此一处)。
pub(super) fn wire_event(f: WireFrame) -> Event {
    match f.event {
        Some(name) => Event::default().event(name).data(f.data),
        None => Event::default().data(f.data),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn stream_response(
    st: Arc<WorkerState>,
    lease: scheduler::AccountLease,
    mut stream: gw_core::provider::ChatStream,
    req: ChatRequest,
    client_key: String,
    ctx: CallCtx,
    started_at: std::time::Instant,
    wire: Wire,
    // 是否已做过一次「流内 401 → 同号刷新重试」(防无限递归)。
    token_retried: bool,
) -> axum::response::Response {
    // ctx 只在「流内 401 刷新重试」分支需要(session/cache 亲和键原样带进重试);
    // 其余路径沿用解构出的 account,行为与旧签名一致。
    let CallCtx {
        account,
        session_id,
        cache_key,
    } = ctx;
    // 首项窥探(2026-08-24 用户报障):上游拒答(疑 guardrail)/空流的形态是"前导帧
    // 之后 Err 或流尽",老路同步返回 200 SSE,客户端只看到裸断(Claude Code 只会说
    // "check any proxy")。在响应头定稿前窥探:窗口内压前导帧,Err/流尽 → 回真实
    // 错误状态+文案;内容帧/超时 → 缓冲拼回流首,后续逐字节不变。router 等 worker
    // 响应头再转发(router/mod.rs:634),状态如实穿透,无需改 router。
    //
    // ⚠️ 不能见到任意 Ok 首项就提交(codex 审查 blocker):kiro 空响应会**先急发
    // message_start**、确认零内容后才发 Err(EmptyResponse)(gw-kiro chat.rs:885/1175);
    // dario 的零 SSE 流也会先产内部 Usage(gw-dario chat.rs:536)。所以窗口内要把
    // **前导帧**(message_start / Usage / UpstreamCut)压进缓冲继续窥探,直到:
    // ① Err → 按首包前失败回显真实状态;② 流尽 → 零产出按 EmptyResponse 回显;
    // ③ 内容帧(其余一切 SSE)→ 缓冲原样拼回流首,后续路径逐字节不变;
    // ④ 超时(长思考慢首 token)→ 同样拼回,回退现有 200 + keepalive 行为。
    enum Verdict {
        /// 提交 200:见到内容帧(缓冲含它)或窥探超时。
        Commit,
        /// 窗口内拿到上游硬错误。
        Fail(gw_core::error::UpstreamError),
        /// 窗口内流尽:零产出。
        Empty,
    }
    let deadline = tokio::time::Instant::now() + PEEK_FIRST_ITEM_TIMEOUT;
    let mut buffered: Vec<Result<StreamItem, gw_core::error::UpstreamError>> = Vec::new();
    let verdict = loop {
        // `stream.next()` 取消安全(元素只在 Poll::Ready 时取走),超时丢弃 future
        // 不会吞掉任何事件 —— 超时的 Commit 只是把缓冲拼回首位,零丢失。
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Err(e))) => break Verdict::Fail(e),
            Ok(Some(Ok(item))) => {
                match &item {
                    // dario 会把上游 error 帧包成 Ok(Sse)(gw-dario chat.rs:510):
                    // 首内容就是 error 时 Commit 成 200 会把拒答藏回流内,按上游
                    // 硬错误处理(2026-08-24 codex 复审 blocker#1)。
                    StreamItem::Sse(ev) if ev.event == "error" => {
                        let kind = sse_error_event_kind(ev);
                        break Verdict::Fail(gw_core::error::UpstreamError::new(
                            kind,
                            format!("上游流内 error 事件(窥探期): {}", ev.data),
                        ));
                    }
                    // 内容帧(除 message_start/ping 外的 SSE)才提交 200。
                    // ping 是保活帧,不证明内容;message_start 见上方 blocker 注释。
                    StreamItem::Sse(ev) if ev.event != "message_start" && ev.event != "ping" => {
                        buffered.push(Ok(item));
                        break Verdict::Commit;
                    }
                    // 前导帧(message_start / ping / Usage / UpstreamCut):压缓冲继续窥探。
                    _ => buffered.push(Ok(item)),
                }
            }
            Ok(None) => break Verdict::Empty,
            Err(_) => break Verdict::Commit,
        }
    };
    let stream: gw_core::provider::ChatStream = match verdict {
        Verdict::Commit => Box::pin(futures::stream::iter(buffered).chain(stream)),
        Verdict::Fail(e) => {
            // 流内 TokenInvalid(cursor 系 401 以流内错误帧形态到达,主 handler 同步
            // 分支的 refresh-after-rejection 永远摸不到它):过去落到这就直接永久禁号
            // —— 2026-09-08 ultra-test 事故,RT 明明活着,一次流内 401 被秒杀。
            // 与同步分支同口径:先同号刷新重试一次,刷新也失败才按 refresh 的 kind
            // 定生死(TokenInvalid=真死;transient=对应轻罚,不误杀)。
            if e.kind == UpstreamErrorKind::TokenInvalid
                && !e.delivery.may_have_executed()
                && !token_retried
                && !st.provider.refresh_exempt(&account)
            {
                let rejected = account.extra_str("access_token").map(|s| s.to_string());
                match st
                    .refresh_after_rejection(account.clone(), rejected.as_deref())
                    .await
                {
                    Ok(refreshed) => {
                        // 定频准入:这是同一 lease 上的第二次上游调用(口径同同步分支);
                        // 被拦 ≠ 修复失败 —— 号刚证明健康,绝不上报失败,回 529 让客户重试。
                        if !st
                            .scheduler
                            .note_upstream_call(&refreshed.account_id, None)
                        {
                            tracing::info!(account = %lease.account_id(),
                                "流内 401 刷新成功,但重试被 RPM 准入拦住(不上报失败)");
                            let gate = gw_core::error::UpstreamError::new(
                                UpstreamErrorKind::Overloaded,
                                "刷新后重试被 RPM 准入拦截",
                            );
                            let usage = buffered_usage(&buffered);
                            return fail_pre_first_byte(
                                &st, lease, &req, &client_key, &account, started_at, wire,
                                &gate, usage, false,
                            );
                        }
                        let retry_ctx = CallCtx {
                            account: refreshed,
                            session_id,
                            cache_key,
                        };
                        match st.provider.chat(req.clone(), &retry_ctx).await {
                            Ok(new_stream) => {
                                tracing::info!(account = %lease.account_id(),
                                    "流内 401 同号刷新成功,透明重试");
                                return Box::pin(stream_response(
                                    st, lease, new_stream, req, client_key, retry_ctx,
                                    started_at, wire, true,
                                ))
                                .await;
                            }
                            Err(e2) => {
                                // 重试仍败:上报真实 kind 后按首包前失败回显;是否降级由上游
                                // 自己判(Provider::reclassify_retry_error)。
                                let e2 = st.provider.reclassify_retry_error(e2);
                                let usage = buffered_usage(&buffered);
                                return fail_pre_first_byte(
                                    &st, lease, &req, &client_key, &retry_ctx.account,
                                    started_at, wire, &e2, usage, true,
                                );
                            }
                        }
                    }
                    Err(e2) => {
                        // 刷新失败:e2.kind 才是真实判据(TokenInvalid=永久,其余=轻罚)。
                        let usage = buffered_usage(&buffered);
                        return fail_pre_first_byte(
                            &st, lease, &req, &client_key, &account, started_at, wire, &e2,
                            usage, true,
                        );
                    }
                }
            }
            // 首项(或前导后)即上游错误 = 首包前失败:与主 handler「chat 失败」分支
            // 同口径(report_failure + 失败请求日志 + 对外错误状态),只是把 200 空流
            // 换成真实状态码。不另起换号重试:与今天流内 Err 分支的行为一致。
            //
            // 递归重试(token_retried=true)的 401 同样是**流内错误帧**形态,上面
            // chat() 同步 Err 上的 demote 包不住它 —— cursor 旧 inference 死面对
            // 所有号 401,刷新刚成功即证明凭据活着,补降级防误杀(kiro 不适用,
            // 见各上游的 Provider::reclassify_retry_error)。
            let e = if token_retried {
                st.provider.reclassify_retry_error(e)
            } else {
                e
            };
            tracing::warn!(
                account = %lease.account_id(),
                kind = ?e.kind,
                "流在零产出阶段即上游错误,按首包前失败回显: {e}"
            );
            let usage = buffered_usage(&buffered);
            return fail_pre_first_byte(
                &st, lease, &req, &client_key, &account, started_at, wire, &e, usage, true,
            );
        }
        Verdict::Empty => {
            // 零产出流:**既不报成功也不报失败**(比旧口径保守 —— 旧路径
            // StreamCtx::Drop 会 report_success_observed(false) 顺带清 429 连击,
            // 此处刻意不动账号健康状态),但客户端必须拿到明确错误而非 200 空流。
            let e = gw_core::error::UpstreamError::new(
                UpstreamErrorKind::EmptyResponse,
                "上游空响应(零事件流)".to_string(),
            );
            tracing::warn!(
                account = %lease.account_id(),
                "流零内容结束(缓冲仅前导帧),按 EmptyResponse 回显"
            );
            let usage = buffered_usage(&buffered);
            return fail_pre_first_byte(
                &st, lease, &req, &client_key, &account, started_at, wire, &e, usage, false,
            );
        }
    };

    /// unfold 累积态:lease 持有到流结束;reported 防重复上报;last_usage 缓存终结用量。
    struct StreamCtx {
        st: Arc<WorkerState>,
        account_id: String,
        model: String,
        client_key: String,
        /// 选号时的 suspend 世代快照(见 `_lease`);Drop 上报时原样带回。
        suspend_gen: u64,
        _lease: scheduler::AccountLease,
        inner: gw_core::provider::ChatStream,
        saw_error: bool,
        reported: bool,
        last_usage: Option<ChatUsage>,
        // 请求日志(#③)采集态(client_payload 在 detach 的 blocking 任务里从 req.body 序列化,
        // 不在此持有,避免热路径成本):
        req: ChatRequest,
        account: Arc<Account>,
        started_at: std::time::Instant,
        first_byte_at: Option<std::time::Instant>,
        error_kind: Option<String>,
        // 模型回复采集(#③):流过期间累积转发给客户端的 SSE 事件(克隆一份),收尾时折叠成
        // Anthropic Messages 写入 response_payload。按**累计字节**(resp_bytes)封顶 RESPONSE_LOG_MAX_BYTES
        // 防超长/超大回复无界占内存(触顶后停止累积,折叠仍尽力而为;正常回复远低于此)。
        resp_events: Vec<SseEvent>,
        resp_bytes: usize,
        /// 首包后上游 Err 时下发客户端的 SSE error 载荷。有值则请求日志存它(与首包前 /
        /// 非流式失败同口径:response_payload = 客户实收的错误体),而不是折叠不成的半截回复。
        client_error: Option<serde_json::Value>,
        /// 流是否走到了 `message_stop`(= 客户端拿到了完整响应)。
        saw_message_stop: bool,
        /// 是否因上游静默超过 [`STREAM_IDLE_ABORT`] 被我方主动中止。
        ///
        /// ⚠️ 它与 `saw_error` **必须严格分开**:`saw_error` 驱动账号健康上报
        /// (report_failure → failure_count → 撞 max_failures 就禁号)。而"流没收完"的成因
        /// 既可能是上游静默、也可能是客户端主动断开(用户 Ctrl-C),拿它去扣账号健康会重演
        /// 2026-07-25 那场 35 秒禁光 7 个号的事故——上游抖动时所有账号会一起中招。
        /// 故本标志与 `saw_message_stop` **只**影响请求日志的 success/error_kind,
        /// 绝不参与账号生命周期上报。
        idle_aborted: bool,
        /// provider 显式上报过「上游静默掐流」(`StreamItem::UpstreamCut`,目前仅 Kiro:
        /// 见过真实上游 payload 但未收终止事件就 EOF)。实测是封号前兆。
        ///
        /// ⚠️ 与 `idle_aborted` 同款隔离纪律,且**更严**:Drop 里它走独立分支——
        /// 跳过 `report_success_observed`(它会清 failure_count/429 strikes/paced_until,
        /// 等于改写健康状态,codex 对抗评审#2),也绝不走 report_failure;
        /// 只喂 scheduler 的软冷却(report_upstream_cut)与请求日志 error_kind。
        upstream_cut: bool,
        /// 选号时的软冷却代际快照(随 lease 携带),Drop 上报时原样带回。
        cut_epoch: u64,
        /// 当前开着的内容块(决定保活帧的形态)。
        open_block: Option<OpenBlock>,
        /// 最近一次收到上游事件的时刻(空闲时长的基准)。
        last_event_at: std::time::Instant,
        /// 出站线缆转换器。`None` = 原生 Anthropic,转发路径与引入本字段之前逐字节相同。
        conv: Option<OutConv>,
        /// 待下发帧队列:一条上游事件在 OpenAI 侧可能产出多帧,而 `unfold` 每次只能
        /// 交出一个 —— 多出来的排这里,下一轮先排空再去拉上游。
        pending: std::collections::VecDeque<Event>,
        /// 上游流已耗尽且终止序列已排入 `pending`,排空后即结束。
        drained: bool,
    }

    // 收尾(账号生命周期上报 + usage 落库)统一放 Drop:无论流跑到 None 正常结束,
    // **还是客户端中途断开导致 axum 直接 drop 响应体**(此时 unfold 不再被 poll、永远到不了
    // None 分支),Drop 都会触发,确保 usage 不漏记、账号信号不丢(审查 Skeptic#1/Architect#1)。
    // 生命周期上报是同步的(parking_lot,Drop 内直接做);usage 落库是 async,detach 到运行时
    // 异步执行——既不阻塞 SSE 收尾 poll(审查 Skeptic#2),又不依赖流被读到 EOF。
    impl Drop for StreamCtx {
        fn drop(&mut self) {
            // 账号生命周期上报(一次)。Err 分支可能已按具体 kind 上报过(reported=true)。
            // ⚠️ **仍然只看 `saw_error`**:流没收完(客户端断开 / 上游静默中止)不扣账号健康,
            // 理由见 `idle_aborted` 字段注释——那会在上游抖动时把所有账号一起打死。
            let account_ok = !self.saw_error;
            if !self.reported {
                self.reported = true;
                if self.upstream_cut && account_ok {
                    // upstream_cut 独立分支:与健康/禁用体系完全隔离——
                    // **跳过** report_success_observed(它会清 failure_count/429
                    // strikes/paced_until,等于把掐流当成成功去改写健康状态,
                    // codex 对抗评审#2),也绝不走 report_failure。
                    // 只喂软冷却信号;代际不符(账号已 reset/重启用)由调度器丢弃。
                    self.st
                        .scheduler
                        .report_upstream_cut(&self.account_id, self.cut_epoch);
                } else if account_ok {
                    // suspend 退避清零只认「同世代 + 流走到 message_stop」的完整成功;
                    // 客户端断开/上游静默中止的半流仍清 429 连击等既有口径(方法内分层)。
                    self.st.scheduler.report_success_observed(
                        &self.account_id,
                        self.suspend_gen,
                        self.saw_message_stop,
                    );
                    // 真的出过数据(拿到用量或走到 message_stop)才算把上游缓存填热了;
                    // 只有完整结束才喂耗时估计(半截流的时长不代表请求耗时)。
                    if self.last_usage.is_some() || self.saw_message_stop {
                        self.st.scheduler.note_thread_usage(
                            &self._lease,
                            self.last_usage.as_ref().map(|u| u.input_tokens),
                            self.saw_message_stop,
                        );
                    }
                } else {
                    self.st.scheduler.report_failure_with_gen(
                        &self.account_id,
                        UpstreamErrorKind::ServerError,
                        self.suspend_gen,
                    );
                }
            }
            // detach 到当前运行时,做 usage 落库(#130)+ 请求日志落库(#③)。
            // 请求日志**总是**尝试(失败请求也要记),故不像 #130 那样门控 usage/sink;
            // finalize_usage / finalize_request_log 各自对 None 降级。无运行时上下文
            // (理论上不会:SSE body 总在 tokio 内 drop)则跳过。guard 跟随任务存活:
            // 停机排空经 pending_writes.wait_idle 等这批落库收尾(审查 Skeptic#1/Architect#1)。
            // 请求日志的成败判定比账号健康**更严**:上游没报错、**且**流确实走到了
            // message_stop,才算客户端拿到了完整响应。此前这类"中途卡死"一律记成 200 成功,
            // 在 admin UI 和告警里完全隐形——2026-07-26 排查用户报障就栽在这:错误日志
            // 一片干净,故障却真实存在,只能靠翻 `success=1 AND output_tokens=0` 的空流才发现。
            let incomplete = if self.idle_aborted {
                Some("upstream_idle_abort")
            } else if self.upstream_cut {
                // 上游静默掐流(封号前兆):即使 provider 合成了 message_stop 收尾,
                // 也按 upstream_cut 落库(替代 incomplete_stream),供面板聚合分析。
                Some("upstream_cut")
            } else if !self.saw_message_stop {
                Some("incomplete_stream")
            } else {
                None
            };
            let success = account_ok && incomplete.is_none();
            let duration_ms = Some(self.started_at.elapsed().as_millis() as i64);
            let ttfb_ms = self
                .first_byte_at
                .map(|t| t.duration_since(self.started_at).as_millis() as i64);
            if let Some(kind) = incomplete {
                tracing::warn!(
                    account = %self.account_id,
                    model = %self.model,
                    duration_ms = ?duration_ms,
                    kind,
                    "流按未完成口径收尾(**不**影响账号健康)"
                );
            }
            // status_code 记的是**实际发出过的 HTTP 状态**,故仍按 account_ok 判定——
            // 卡死那次的响应头确实是 200,新增信息由 success/error_kind 承载。
            let status_code: Option<i64> = if account_ok { Some(200) } else { None };
            let usage = self.last_usage.take();
            let error_kind = self
                .error_kind
                .take()
                .or_else(|| incomplete.map(str::to_string));
            // usage 落库(#130):async,detach 到运行时(sink.record 是 async)。
            if let (Some(sink), Some(u)) = (self.st.usage_sink.clone(), usage.clone()) {
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let guard = self.st.pending_writes.enter();
                    let account_id = self.account_id.clone();
                    let model = self.model.clone();
                    let client_key = self.client_key.clone();
                    handle.spawn(async move {
                        let _guard = guard;
                        finalize_usage(
                            Some(&sink),
                            &account_id,
                            &model,
                            &client_key,
                            Some(&u),
                            // 计量口径**不动**:仍用 account_ok 而非更严的 success。
                            // 流没收完但上游已产出的 token 该计还得计,把它改成失败会
                            // 悄悄改变历史统计与计费语义,不属于本次改动的范围。
                            account_ok,
                        )
                        .await;
                    });
                }
            }
            // 请求日志落库(#③):detach 到 blocking 线程池(序列化两份报文 + 写库)。失败请求也记,
            // 故不门控 usage/sink;client_payload 在 blocking 任务里从 req.body 序列化(离开热路径)。
            spawn_request_log_blocking(
                &self.st,
                self.req.clone(),
                Some(self.account.clone()),
                self.client_key.clone(),
                true,
                success,
                status_code,
                error_kind,
                duration_ms,
                ttfb_ms,
                usage,
                match self.client_error.take() {
                    Some(body) => ResponseLog::Error(body),
                    None => ResponseLog::Events(std::mem::take(&mut self.resp_events)),
                },
            );
        }
    }

    let account_id = lease.account_id().to_string();
    let model = req.model.clone();
    // 转换器建好再交给结构体:`req` 会在字段列表中途被 move,而字段按书写顺序求值。
    let conv = OutConv::for_wire(wire, &req.model, &req.body);
    let init = StreamCtx {
        st,
        account_id,
        model,
        client_key,
        suspend_gen: lease.suspend_gen,
        cut_epoch: lease.cut_epoch,
        _lease: lease,
        inner: stream,
        saw_error: false,
        reported: false,
        last_usage: None,
        req,
        account,
        started_at,
        first_byte_at: None,
        error_kind: None,
        resp_events: Vec::new(),
        resp_bytes: 0,
        client_error: None,
        saw_message_stop: false,
        idle_aborted: false,
        upstream_cut: false,
        open_block: None,
        last_event_at: std::time::Instant::now(),
        conv,
        pending: std::collections::VecDeque::new(),
        drained: false,
    };

    let sse = futures::stream::unfold(init, |mut ctx| async move {
        // 单步内循环跳过 usage 事件,直到拿到一个可转发事件或流结束(避免递归类型膨胀)。
        loop {
            // 队列优先:一条上游事件产出的多帧要挨个交出去,期间不去拉上游。
            if let Some(ev) = ctx.pending.pop_front() {
                return Some((Ok::<Event, std::convert::Infallible>(ev), ctx));
            }
            // 终止序列已排空 → 真结束(返回 None 会 drop ctx,收尾走 StreamCtx::drop)。
            if ctx.drained {
                return None;
            }
            // 上一轮已因静默撞上硬上限并发过错误事件 → 结束流。返回 None 会 drop ctx,
            // 连带 drop `inner` 从而真正切断上游连接(不留悬挂请求)。
            if ctx.idle_aborted {
                return None;
            }
            // 取上游事件带空闲超时:静默超过 [`STREAM_IDLE_KEEPALIVE`] 就先给客户端补一个
            // 保活帧再回来继续等。`StreamExt::next` 是取消安全的(元素只在 Poll::Ready 时
            // 取走),超时丢弃这个 future 不会吞掉任何事件。
            let item = match tokio::time::timeout(STREAM_IDLE_KEEPALIVE, ctx.inner.next()).await {
                Ok(item) => {
                    ctx.last_event_at = std::time::Instant::now();
                    item
                }
                Err(_) => {
                    // 转换器已发过终止序列 → 客户端**已经拿到完整响应**,此刻还挂着只是
                    // 在等上游 EOF(为了收 message_stop 之后才到的 usage 事件)。
                    // 这时候既不能发保活(终态之后再冒帧是协议违例),也**不能**判 idle abort ——
                    // 那会把一个已经成功交付的请求在日志里记成 `upstream_idle_abort`
                    // (对抗评审 Architect#5)。静默等 EOF 即可。
                    if ctx.conv.as_ref().is_some_and(OutConv::is_finished) {
                        continue;
                    }
                    let idle = ctx.last_event_at.elapsed();
                    if idle >= STREAM_IDLE_ABORT {
                        // 静默到硬上限:主动中止并给客户端一个**明确错误**,而不是让它继续
                        // 空等、最后自己超时——那条老路我方还会记成 200 成功(见 Drop 注释)。
                        ctx.idle_aborted = true;
                        tracing::warn!(
                            account = %ctx.account_id,
                            model = %ctx.model,
                            idle_secs = idle.as_secs(),
                            "上游静默超过硬上限,主动中止本次流"
                        );
                        let payload = serde_json::json!({"type":"error","error":{
                            "type":"api_error",
                            "message": format!(
                                "upstream stalled: no event for {}s",
                                idle.as_secs()
                            )}});
                        // OpenAI 线缆:转成该协议的失败形状(chat 是 error 帧 + [DONE],
                        // responses 是 response.failed),否则客户端收到一个它读不懂的
                        // Anthropic error 事件,表现和「干等到超时」没区别。
                        if let Some(conv) = ctx.conv.as_mut() {
                            for f in conv.fail(&payload) {
                                ctx.pending.push_back(wire_event(f));
                            }
                            continue;
                        }
                        let out = Event::default().event("error").data(payload.to_string());
                        return Some((Ok::<Event, std::convert::Infallible>(out), ctx));
                    }
                    tracing::debug!(
                        account = %ctx.account_id,
                        model = %ctx.model,
                        idle_secs = idle.as_secs(),
                        "上游静默,发保活帧维持下游空闲计时"
                    );
                    // 保活帧**不**进 resp_events:它不是模型回复内容,且对折叠是 no-op,
                    // 混进去只会平白占掉 RESPONSE_LOG_MAX_BYTES 预算。
                    //
                    // OpenAI 线缆有自己的保活形态(空 delta / response.in_progress),
                    // **不能**把 Anthropic 的零增量喂给转换器 —— 那会在 chat 侧变成一条
                    // 真的空内容增量、在 responses 侧被算进 output_text 的全量文本。
                    if let Some(conv) = ctx.conv.as_mut() {
                        // 已发过终止序列(收到 message_stop 但底层流还没 EOF)→ 静默等 EOF。
                        // 再发保活等于在 [DONE] / response.completed 之后又冒出一帧。
                        // 这里**不能**顺手 drop 上游流:usage 事件常在 message_stop **之后**
                        // 才到(见 gw-cursor),提前切断就是丢计费。
                        if conv.is_finished() {
                            continue;
                        }
                        for f in conv.keepalive() {
                            ctx.pending.push_back(wire_event(f));
                        }
                        continue;
                    }
                    let ka = keepalive_frame(ctx.open_block);
                    let out = Event::default().event(ka.event.clone()).data(ka.data.to_string());
                    return Some((Ok::<Event, std::convert::Infallible>(out), ctx));
                }
            };
            match item {
                Some(Ok(StreamItem::UpstreamCut)) => {
                    // provider 显式上报的「上游静默掐流」前兆:只置标志,**不转发客户端**
                    // (它不是 Anthropic SSE 事件)。收尾在 StreamCtx::drop 的独立分支处理。
                    ctx.upstream_cut = true;
                    continue;
                }
                Some(Ok(StreamItem::Sse(mut ev))) => {
                    // 首个转发事件时刻 = TTFB(请求日志 #③)。
                    if ctx.first_byte_at.is_none() {
                        ctx.first_byte_at = Some(std::time::Instant::now());
                    }
                    if ev.event == "error" {
                        ctx.saw_error = true;
                        if ctx.error_kind.is_none() {
                            ctx.error_kind = Some("stream_error".to_string());
                        }
                        // provider 把上游的 error 帧当**普通 SSE 事件**产出时(dario 直透
                        // Anthropic 流即如此),这条路绕开 UpstreamError,原样转发就等于
                        // 把上游报文发给客户。此处落原文 + 改写载荷,是这条路的唯一闸门。
                        tracing::warn!(
                            account = %ctx.account_id,
                            model = %ctx.model,
                            "上游流内 error 事件: {}", ev.data
                        );
                        ev.data = sanitize_upstream_error_payload(&ev.data);
                    }
                    // 完整性判据:只有真的收到 message_stop 才算客户端拿到完整响应。
                    if ev.event == "message_stop" {
                        ctx.saw_message_stop = true;
                    }
                    // 答到一半被上游分类器拦下(stop_reason=refusal):客户端拿到的是完整消息,
                    // success 口径不变,只在请求日志里打 ContentFiltered 标记(面板标红)。
                    if ctx.error_kind.is_none() && is_refusal_delta(&ev) {
                        ctx.error_kind = Some(CONTENT_FILTERED_KIND.to_string());
                    }
                    // 跟踪当前开着的块,供保活帧选形态(必须在转发前更新:下一轮静默时要用)。
                    ctx.open_block = track_open_block(ctx.open_block, &ev);
                    // OpenAI 线缆:转换后排队下发。请求日志的采集口径**不变** ——
                    // 存的仍是 Anthropic 事件(与 kiro 同口径,yapi 语料链路不用适配)。
                    if let Some(conv) = ctx.conv.as_mut() {
                        let frames = conv.push(&ev);
                        if ev.event != "error" && ctx.resp_bytes < RESPONSE_LOG_MAX_BYTES {
                            let n = ev.data.to_string().len();
                            ctx.resp_bytes = ctx.resp_bytes.saturating_add(n);
                            ctx.resp_events.push(ev);
                        }
                        for f in frames {
                            ctx.pending.push_back(wire_event(f));
                        }
                        continue;
                    }
                    let out = match ev.to_wire() {
                        Ok(_) => {
                            // 转发本就要把 data 序列化成字符串,这里复用它:既当字节预算度量,又喂给下游,
                            // 避免二次 to_string。模型回复采集(#③):error 不入(非回复内容);
                            // 按累计字节封顶(真正的内存护栏,挡少量超大事件),触顶停采。
                            let data = ev.data.to_string();
                            if ev.event != "error" && ctx.resp_bytes < RESPONSE_LOG_MAX_BYTES {
                                ctx.resp_bytes = ctx.resp_bytes.saturating_add(data.len());
                                ctx.resp_events.push(ev.clone());
                            }
                            Event::default().event(ev.event).data(data)
                        }
                        Err(e) => {
                            // 序列化失败也算本次响应损坏 → 收尾按失败上报(审查 Architect#9)。
                            ctx.saw_error = true;
                            Event::default().event("error").data(
                                serde_json::json!({"type":"error","error":{
                                    "type":"api_error",
                                    "message": format!("serialize sse: {e}")}})
                                    .to_string(),
                            )
                        }
                    };
                    return Some((Ok::<Event, std::convert::Infallible>(out), ctx));
                }
                Some(Ok(StreamItem::Usage(u))) => {
                    tracing::debug!(
                        account = %ctx.account_id,
                        input = u.input_tokens,
                        output = u.output_tokens,
                        cache_read = u.cache_read_tokens,
                        "chat usage (缓存待流终态落库 #130)"
                    );
                    // 缓存终结用量,流干净/失败收尾时统一落库(success 以最终状态为准)。
                    ctx.last_usage = Some(u);
                    continue; // 不转发客户端,取下一个。
                }
                Some(Err(e)) => {
                    ctx.saw_error = true; // 硬错误 → 本次响应失败(usage success=false)。
                    // 对外只发中性文案,原文只此一处落日志(首包已出,主循环的 chat 失败
                    // 告警不会再覆盖流中错误)。
                    tracing::warn!(
                        account = %ctx.account_id,
                        model = %ctx.model,
                        kind = ?e.kind,
                        "流中上游错误: {e}"
                    );
                    if ctx.error_kind.is_none() {
                        ctx.error_kind = Some(format!("{:?}", e.kind));
                    }
                    ctx.client_error.get_or_insert_with(|| sse_error_payload(&e));
                    if !ctx.reported {
                        ctx.reported = true;
                        if should_report_inference_failure(ctx.st.provider.as_ref(), &ctx.account, &e) {
                          ctx.st.scheduler.report_failure_with_detail(
                            &ctx.account_id,
                            e.kind,
                            ctx.suspend_gen,
                            &e.to_string().chars().take(300).collect::<String>(),
                          );
                        }
                    }
                    if let Some(conv) = ctx.conv.as_mut() {
                        for f in conv.fail(&sse_error_payload(&e)) {
                            ctx.pending.push_back(wire_event(f));
                        }
                        ctx.drained = true;
                        continue;
                    }
                    return Some((Ok(sse_error_event(&e)), ctx));
                }
                None => {
                    // 流正常结束。收尾(生命周期上报 + usage 落库)由 StreamCtx::drop 统一处理,
                    // 与"客户端中断致响应体被 drop"走同一条路径,避免两处逻辑分叉。
                    //
                    // OpenAI 线缆多一步:补齐终止序列(末帧 / 未收口的条目 / [DONE])。
                    // 上游没发 message_stop 就断掉时,这一步是客户端唯一能拿到收尾的地方;
                    // 已经收过尾则 `finish()` 幂等返回空。
                    if let Some(conv) = ctx.conv.as_mut() {
                        let frames = conv.finish();
                        ctx.drained = true;
                        if !frames.is_empty() {
                            for f in frames {
                                ctx.pending.push_back(wire_event(f));
                            }
                            continue;
                        }
                    }
                    return None;
                }
            }
        }
    });

    Sse::new(sse).into_response()
}
