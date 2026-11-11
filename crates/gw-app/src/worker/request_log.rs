//! 用量落库与请求日志(环形表,含发给上游的报文与媒体去重)。

use super::*;

/// 流结束时把本次 usage 落库(#130)。`usage=None`(空/错误流无终结用量)或 `sink=None`
/// (控制面库打开失败降级)时直接跳过。落库失败仅告警,**绝不影响**已发给客户端的响应。
///
/// `client_key` = router 经内网头透传的客户 key(无则空串,归到"未归属"桶);连同
/// `account_id` 一起落库,支撑按账号 + 按客户(apikey)两个维度的用量/成本统计。
pub(super) async fn finalize_usage(
    sink: Option<&Arc<dyn UsageSink>>,
    account_id: &str,
    model: &str,
    client_key: &str,
    usage: Option<&ChatUsage>,
    success: bool,
) {
    let (Some(sink), Some(u)) = (sink, usage) else {
        return;
    };
    let rec = UsageRecord {
        client_key_id: client_key.to_string(),
        account_id: account_id.to_string(),
        model: model.to_string(),
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_tokens: u.cache_read_tokens,
        cache_creation_tokens: u.cache_creation_tokens,
        real_cache_read_tokens: u.real_cache_read_tokens,
        metering_credit: u.metering_credit,
        success,
    };
    if let Err(e) = sink.record(rec).await {
        tracing::warn!(account = %account_id, "usage 落库失败(不影响响应): {e}");
    }
}

/// 按客户端 `stream` 标志分发:provider 一律产流,这里决定回 SSE 还是折叠成单个
/// 非流式 Messages 响应(折叠逻辑写一次,见 [`gw_core::fold`])。两条路径都做同一套
/// 收尾(账号生命周期上报 + usage 落库)。
/// 请求日志环形保留条数(最新 N 条;`insert_request_log` 按此裁旧)。
///
/// 生产按用户口径只保留最新 2000 条,避免完整请求报文长期占用过多磁盘。
pub(super) const REQUEST_LOG_CAP: u64 = 2_000;
/// 单条报文(client/kiro)入库前的**文本**体积上限(截断兜底)。报文经 gzip 压缩入库
/// (`gw-store`,文本压 5-10 倍),且图片/文档已抽到去重 blob 表,故**全文存储不再截断**;
/// 此上限抬到 16MiB 仅作防御性护栏——媒体 base64 已抽走,剩下的文本报文远小于出站上限
/// (`DEFAULT_MAX_BODY_BYTES`),真实报文不触顶,只挡住非常规超大输入(防单行无界内存/库占用)。
pub(super) const MAX_LOG_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// 流式模型回复采集的**累计字节**上限(真正的内存护栏:按 SSE data 序列化字节计,而非按条数——
/// 单个 `input_json_delta`/工具增量可能很大,按条数封顶无法挡住"少量超大事件"撑爆内存)。
/// 与入库截断阈值同量级:缓存超过最终会被截断的体量没有意义。触顶后停止累积(已采集部分仍折叠)。
pub(super) const RESPONSE_LOG_MAX_BYTES: usize = MAX_LOG_PAYLOAD_BYTES;

/// 一次请求"模型回复"的采集结果,交给落库任务(blocking)折叠/序列化——把重活留在 blocking,
/// 且让非流式路径**复用已折叠的响应体**(不二次折叠,避免与下发客户端的响应产生分歧)。
pub(super) enum ResponseLog {
    /// 无回复(选号前失败 / 无 message_start)。
    None,
    /// 非流式:已折叠好的 Anthropic Messages(下发客户端的同一份),直接序列化入库。
    Folded(serde_json::Value),
    /// 流式:转发期间按字节预算采集的 SSE 事件,落库任务里折叠成 Messages。
    Events(Vec<SseEvent>),
    /// 失败请求:下发客户端的 Anthropic 形状错误体(`{"type":"error",...}`)。
    /// 让详情页看得到客户实际收到的那句话(如分类器拒答的类别与说明)。
    Error(serde_json::Value),
}

/// 请求日志里分类器拒答的 error_kind 取值 —— 与 `format!("{:?}", UpstreamErrorKind::ContentFiltered)`
/// 同串(测试锁住),面板据此标红。
pub(super) const CONTENT_FILTERED_KIND: &str = "ContentFiltered";

/// 该 SSE 事件是否为 `stop_reason=refusal` 的 message_delta(上游分类器中途拦截)。
pub(super) fn is_refusal_delta(ev: &SseEvent) -> bool {
    ev.event == "message_delta"
        && ev.data.pointer("/delta/stop_reason").and_then(|v| v.as_str()) == Some("refusal")
}

/// 折叠后的非流式消息是否以 refusal 收尾。
pub(super) fn is_refusal_message(msg: &serde_json::Value) -> bool {
    msg.get("stop_reason").and_then(|v| v.as_str()) == Some("refusal")
}

/// 报文入库前处理:**按字段把图片/文档 base64 抽到 blob 列表**(`data`/`bytes` 键的长值,
/// 替换为 `blob:<hash>` 引用;对话正文走 `text` 键,绝不误抽),**再**按字符边界截断文本兜底。
/// 返回(处理后报文文本, 抽出的 blob)。非 JSON(如转换错误串)原样截断、无 blob。
pub(super) fn prepare_log_payload(raw: String) -> (String, Vec<gw_core::store::LogBlob>) {
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(mut v) => {
            let blobs = gw_core::store::extract_log_blobs(&mut v);
            let text = serde_json::to_string(&v).unwrap_or(raw);
            (truncate_log_payload(text), blobs)
        }
        Err(_) => (truncate_log_payload(raw), Vec::new()),
    }
}

/// 把折叠后的模型回复(Anthropic Messages)序列化入库。超 [`MAX_LOG_PAYLOAD_BYTES`] 时**不**像
/// 普通报文那样硬截成半截非法 JSON(会让详情页"格式化"视图解析失败),而是替换成一条**合法**的
/// 占位 Messages(保留 formatted 渲染 + 明确标注截断,见 `_truncated`)。
pub(super) fn serialize_response_capped(msg: &serde_json::Value) -> String {
    let s = serde_json::to_string(msg).unwrap_or_default();
    if s.len() <= MAX_LOG_PAYLOAD_BYTES {
        return s;
    }
    serde_json::json!({
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "text",
            "text": format!("<模型回复过大(约 {} 字节),已省略未入库>", s.len()),
        }],
        "_truncated": true,
    })
    .to_string()
}

/// 按 UTF-8 字符边界安全截断报文(超 [`MAX_LOG_PAYLOAD_BYTES`] 时)。
pub(super) fn truncate_log_payload(s: String) -> String {
    if s.len() <= MAX_LOG_PAYLOAD_BYTES {
        return s;
    }
    let mut end = MAX_LOG_PAYLOAD_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…<已截断,原 {} 字节>", &s[..end], s.len())
}

/// 同步构造并落一条请求日志(环形保留最新 [`REQUEST_LOG_CAP`] 条)。**两份报文都在此序列化**
/// (用户原始 `client_payload` = req.body;发 Kiro 前 `kiro_payload` = gw-kiro 纯渲染助手重渲染,
/// 不跑 cache_sim/体积护栏,无副作用)——故必须经 [`spawn_request_log_blocking`] 丢到 blocking
/// 线程池跑,**绝不在收入热路径**(handler 入口 / SSE async poll)同步执行(审查 Skeptic#1/#3)。
/// `account=None`(选号前即失败)→ account_id/kiro_payload 留空。落库失败仅告警。
#[allow(clippy::too_many_arguments)]
pub(super) fn write_request_log(
    store: Arc<SqliteStore>,
    req: ChatRequest,
    account: Option<Arc<Account>>,
    client_key: String,
    is_stream: bool,
    success: bool,
    status_code: Option<i64>,
    error_kind: Option<String>,
    duration_ms: Option<i64>,
    ttfb_ms: Option<i64>,
    usage: Option<ChatUsage>,
    response: ResponseLog,
    provider: &dyn Provider,
) {
    let (input, output, cache_read, cache_creation, real_cache_read, metering_credit) = usage
        .as_ref()
        .map(|u| {
            (
                u.input_tokens,
                u.output_tokens,
                u.cache_read_tokens,
                u.cache_creation_tokens,
                u.real_cache_read_tokens,
                u.metering_credit,
            )
        })
        .unwrap_or((0, 0, 0, 0, 0, 0.0));
    // 抽出两份报文里的媒体 blob(图片/文档);合并后入库按 hash 去重(同图复用一行)。
    let (client_payload, mut blobs) =
        prepare_log_payload(serde_json::to_string(&req.body).unwrap_or_default());
    // 渲染与否由 **worker 的 provider** 决定,而非 account.provider 字段——
    // filter_by_provider 放行 provider 字段为空的账号进 kiro worker,
    // 若按 account.provider 判断会丢掉空-provider kiro 号的报文渲染。
    let (account_id, kiro_payload) = match &account {
        Some(a) => match provider.render_upstream_payload(&req, a) {
            Some(rendered) => {
                let (kp, kb) = prepare_log_payload(rendered);
                blobs.extend(kb);
                (a.account_id.clone(), kp)
            }
            None => (a.account_id.clone(), String::new()),
        },
        None => (String::new(), String::new()),
    };
    // 模型回复:折叠/序列化都在此 blocking 任务内做,不占热路径。失败请求/无回复 → 空串(详情页不展示)。
    let response_payload = match response {
        ResponseLog::None => String::new(),
        // 非流式:复用下发客户端的同一份折叠结果,不二次折叠(避免与客户端响应分歧)。
        ResponseLog::Folded(msg) => serialize_response_capped(&msg),
        ResponseLog::Error(body) => serialize_response_capped(&body),
        ResponseLog::Events(events) if events.is_empty() => String::new(),
        ResponseLog::Events(events) => match gw_core::fold::fold_sse_to_message(&events) {
            Ok(msg) => serialize_response_capped(&msg),
            Err(_) => {
                // 折叠失败(协议违例 / 缺 message_start / 提前断流):无合法回复可存。
                // success 请求却折叠失败值得一条诊断(上游格式变更会在此先冒头)。
                if success {
                    tracing::debug!(account = %account_id, "模型回复折叠失败,response_payload 留空");
                }
                String::new()
            }
        },
    };
    let log = RequestLog {
        client_key_id: client_key,
        account_id,
        model: req.model.clone(),
        stream: is_stream,
        success,
        status_code,
        error_kind,
        duration_ms,
        ttfb_ms,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_creation,
        // "报"口径本次上报总 token(input 已含放大后的 cache_read);前端展示"报 N"。
        reported_tokens: input.saturating_add(output),
        // "真":上游真实命中;"credit":Kiro 原生计费。诊断/优化用,不参与计费。
        real_cache_read_tokens: real_cache_read,
        metering_credit,
        client_payload,
        kiro_payload,
        response_payload,
        blobs,
    };
    if let Err(e) = store.insert_request_log(&log, REQUEST_LOG_CAP) {
        tracing::warn!(error = %e, account = %log.account_id, "请求日志落库失败");
    }
    // 账号累计成功/失败计数(监控用,非计费)。与请求日志同处 blocking 任务,不占热路径;
    // 每次上游调用终态收尾恰好一次(成功/终态失败),account_id 空则内部跳过。
    if let Err(e) = store.bump_account_counters(&log.account_id, success) {
        tracing::warn!(error = %e, account = %log.account_id, "账号计数更新失败");
    }
}

/// 把请求日志落库 detach 到 **blocking 线程池**(两份报文序列化 + 同步 SQLite 写均为阻塞工作),
/// 不占用 async worker 线程、不拖慢活跃 SSE poll(审查 Skeptic#3)。store=None(库降级)或无
/// 运行时上下文则跳过。guard 跟随任务存活:停机排空经 pending_writes.wait_idle 等这批收尾。
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_request_log_blocking(
    st: &Arc<WorkerState>,
    req: ChatRequest,
    account: Option<Arc<Account>>,
    client_key: String,
    is_stream: bool,
    success: bool,
    status_code: Option<i64>,
    error_kind: Option<String>,
    duration_ms: Option<i64>,
    ttfb_ms: Option<i64>,
    usage: Option<ChatUsage>,
    response: ResponseLog,
) {
    // 拒答记忆的记账(另一个收口在非流式收尾,见 gates::refusal_guard_record)。
    // 放在「库降级就跳过」之前。
    let client_message = match &response {
        ResponseLog::Error(v) => v.pointer("/error/message").and_then(serde_json::Value::as_str),
        _ => None,
    };
    refusal_guard_record(&req, account.as_deref(), success, error_kind.as_deref(), client_message);
    let Some(store) = st.store.clone() else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let guard = st.pending_writes.enter();
    let provider = st.provider.clone();
    handle.spawn_blocking(move || {
        let _guard = guard;
        write_request_log(
            store, req, account, client_key, is_stream, success, status_code, error_kind,
            duration_ms, ttfb_ms, usage, response, provider.as_ref(),
        );
    });
}
