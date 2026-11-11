//! 响应帧状态机:把 Cursor Run 的响应帧流翻成 Anthropic SSE(正文/思考/工具调用/收尾),含停滞闸、排水段、服务端总结、挂流续跑。
//!
//! 2026-11-10 由 chat.rs 拆出(只搬不改行为)。

use super::*;

/// 上游连续多久「只有心跳、零进展」就判定本轮废了(首字前)。
///
/// 心跳是 10 秒一个,取 90 秒 = 容忍 9 个心跳。首 token 前的正常等待实测在 2 秒内,
/// 长思考的模型可能更久,但那期间会持续来 `1.4` 思考帧(算进展),不会触发。
/// 这个值宁可偏大:误判的代价是把一次本来会成功的慢请求变成失败。
pub(super) const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// 已出字后的零进展上限。
///
/// 出字之后再沉默,有两种完全不同的情形:
/// - **必死**:模型调用了未注册的工具名,上游一个帧都不回(只剩心跳);
/// - **大件组装**:模型在服务端逐 token 拼一个巨型工具调用(典型:write 一整份
///   HTML,几十 KB 参数),期间同样一个帧都不来。组装 500 行 HTML 需要 60~120s。
/// 30s 会把后者误杀(2026-09-04 本地 e2e 实证:写报纸页必死)。取 150s:
/// 覆盖大件组装,必死轮次晚两分钟宣判可接受(重试终究会来)。
pub(super) const STALL_TIMEOUT_STARTED: std::time::Duration = std::time::Duration::from_secs(150);

/// 排水段(turn_commit 之后)的零进展上限。
///
/// 正常排水 1~2s 内尾帧到齐、流自关;超过这个值只剩一种形态 —— 上游
/// 挂住只剩心跳(典型:模型调了未注册工具,服务端在永不返回地等结果)。
/// 此时轮次在 commit 已成功,多等一秒都是纯延迟:判排水停滞,按
/// 「轮次照常成功、影子不提交」收口(停滞闸的 draining 分支)。
pub(super) const DRAIN_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// 「只有干活帧、没有任何可见输出」的总上限(干活帧见 `run::is_work_update`)。
///
/// 干活帧会续停滞闸的命 —— 这是对的(模型在拼大工具调用),但不能无限续:上游病态地
/// 只发 token 计数时,没有这道封顶请求就永远挂着。600s 足够拼一整篇长文。
pub(super) const WORK_ONLY_CAP: std::time::Duration = std::time::Duration::from_secs(600);

/// 干活期间多久给 gw-app 交一个 `ping`。gw-app 的静默保活是 20s、硬上限 300s
/// (`worker::response::STREAM_IDLE_ABORT`),它只认我们交出的项。
pub(super) const WORK_PING_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

/// 服务端总结压缩上下文(`1.10 summary_started` → `1.11 summary_completed`)的等待上限,
/// 从总结开始计时,心跳不续命。
///
/// 2026-11-09:长会话(47 万 token)自造历史后服务端先总结,实测一次 39s;期间只有心跳。
/// 排水闸 20s 把它掐成空回复,客户端重试又触发一次总结(白花钱)。一刀切把首字前放宽到
/// 90s 则让「永远不回」的请求也干占并发,23:32 号池被占满(见 CHANGELOG)。所以只在
/// **确实收到总结开始帧**时放宽,其它情形闸门不变。
pub(super) const SUMMARY_MAX: std::time::Duration = std::time::Duration::from_secs(180);

/// 内建工具界面帧翻译不了时,等它的 exec 帧(带参数,映射或回拒绝)最多多久。
/// 实测 exec 帧紧跟界面帧(毫秒级);到期没来就退回原收口。
pub(super) const BUILTIN_EXEC_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// 内建工具既翻译不了、也等不到 exec 帧回拒绝时的收口结果。
pub(super) struct BuiltinClose {
    /// 出字前收口 → 报错(客户端拿到可重试的错误,而不是空回答)。
    pub(super) error: Option<UpstreamError>,
    /// 出字后收口 → 回答被截断(仍按 end_turn 收尾,日志留痕)。
    pub(super) truncated: bool,
}

/// 「模型调了我们执行不了的内建工具」的收口:记纠偏标记、打日志、给出错误或截断标记。
pub(super) fn close_untranslatable_builtin(
    payload: &[u8],
    started: bool,
    output_chars: usize,
    notices: &crate::TruncationNotices,
    conversation_id: &str,
) -> BuiltinClose {
    let preview: String = String::from_utf8_lossy(payload)
        .chars()
        .map(|c| if c.is_control() { '·' } else { c })
        .take(300)
        .collect();
    tracing::warn!(
        bytes = payload.len(), started, output_chars,
        // 护栏文案的版本指纹:按 guard_rev 分桶才比得出「换了文案有没有用」。
        guard_rev = %crate::tool_guard_rev(),
        // 这一帧里模型要的能力(认得出的话),下一轮纠偏时指名换哪个工具。
        cap = ?builtin_capability(payload),
        ident = ?run::builtin_tool_ident(payload),
        preview = %preview,
        "cursor Run:模型调用了 Cursor 内建工具(我们无法执行)—— 收口"
    );
    // 记下来,下一轮同会话时告诉模型「上次那个工具不存在」。出字前/出字后都记:
    // 两种情况下客户端历史里都留着一次没有结果的尝试,模型都会重复撞墙。
    notices.record(conversation_id, builtin_capability(payload));
    if !started {
        return BuiltinClose {
            error: Some(UpstreamError::new(
                UpstreamErrorKind::EmptyResponse,
                "Cursor 模型选择调用其内建工具,gw-cursor 无法代为执行",
            )),
            truncated: false,
        };
    }
    // 已经出过字了。首包既已 committed,报错也换不了号,把已有的字交出去比丢掉整个
    // 回答好 —— 但这**确实是一次截断**,必须留一条显式痕迹,否则和正常收尾长得一样。
    tracing::warn!(
        output_chars,
        "cursor Run:内建工具收口发生在已出字之后 —— 本次回答是截断的,\
         但仍按 end_turn 收尾(首包已 committed,无法重试)"
    );
    BuiltinClose { error: None, truncated: true }
}

/// 给下游续命的空增量帧(与 gw-app `keepalive_frame` 同形):挂在当前开着的
/// 文本/思考块上,拼空串等于没拼。
///
/// 不发官方 `ping`:下游 NewAPI 的 Claude→OpenAI 转换器会丢掉它,而它又会重置
/// gw-app 的静默计时、压掉 gw-app 自己的空增量保活 —— 两头落空。没有开着的块时
/// 什么都不发(此时 `message_start` 可能还没发,抢先冒帧会打乱事件顺序),交给 gw-app。
pub(super) fn keepalive_delta(open_block: Option<(usize, &'static str)>) -> Option<SseEvent> {
    let (idx, delta) = match open_block? {
        (i, "text") => (i, json!({"type":"text_delta","text":""})),
        (i, "thinking") => (i, json!({"type":"thinking_delta","thinking":""})),
        _ => return None,
    };
    Some(SseEvent::new(
        "content_block_delta",
        json!({"type":"content_block_delta","index":idx,"delta":delta}),
    ))
}

/// 停滞收口日志里保留的最近非心跳帧数。
pub(super) const FRAME_TRAIL_CAP: usize = 24;

/// 停滞闸的到期时刻。
///
/// - 常规:最后进展 + 档位上限;
/// - 工具调用组装中(见 `run::is_tool_assembly`):上游只发心跳是正常的,
///   放宽到「开始组装 + [`WORK_ONLY_CAP`]」;
/// - 只有干活帧、没有可见输出的阶段:以「开始只干活 + [`WORK_ONLY_CAP`]」封顶。
pub(super) fn stall_deadline(
    last_progress: tokio::time::Instant,
    stall: std::time::Duration,
    work_only_since: Option<tokio::time::Instant>,
    assembling_since: Option<tokio::time::Instant>,
    summarizing_since: Option<tokio::time::Instant>,
) -> tokio::time::Instant {
    let mut d = last_progress + stall;
    if let Some(a) = assembling_since {
        d = d.max(a + WORK_ONLY_CAP);
    }
    // 服务端总结中:从总结开始算 [`SUMMARY_MAX`](心跳不续命)。
    if let Some(sm) = summarizing_since {
        d = d.max(sm + SUMMARY_MAX);
    }
    if let Some(w) = work_only_since {
        d = d.min(w + WORK_ONLY_CAP);
    }
    d
}

/// 「N 秒内只有心跳」停滞的文案标记:连接是活的(心跳在来),只是上游不出字。
pub(crate) const STALL_HEARTBEAT_ONLY: &str = "内只有心跳";

/// 「连接一个字节都没收到」的文案标记(生成见流循环的停滞闸,识别见 [`is_connection_stall`])。
pub(crate) const CONN_DEAD_MARK: &str = "连接没收到任何字节";

/// 停滞时连接多久一个字节都没收到,就判连接卡死。心跳 10 秒一个,30 秒 = 连丢 3 个。
pub(crate) const CONN_DEAD_SILENCE: std::time::Duration = std::time::Duration::from_secs(30);

/// 这个错误是不是**连接卡死**:停滞期间连心跳都没有,连接「只发不收」。
///
/// 2026-11-10 生产:139 上看到某号的连接 lastrcv 133s、lastsnd 0.7s,这个号上所有请求
/// 一起卡 90~150s,号被禁、冷却后仍用这条坏连接,反复卡。调用方据此把这个号的连接丢掉重建
/// (`provider::heal_stalled_connection`)。**只有心跳、不出字**不算(连接是活的,重建无益),
/// 「服务端总结压缩超时」「只有工具参数增量」也不算。
pub(crate) fn is_connection_stall(e: &UpstreamError) -> bool {
    e.kind == UpstreamErrorKind::ServerError && e.message.contains(CONN_DEAD_MARK)
}

/// 把「最后一次进展之后」到达的非心跳帧压成一行:`+秒 顶层/内层 字节`。
/// 没有 = 真的只有心跳;有 = 我们漏认了某种进展帧,字段号就是下一步要查的。
pub(super) fn format_trail(
    trail: &std::collections::VecDeque<(tokio::time::Instant, Vec<u32>, Vec<u32>, usize)>,
    last_progress: tokio::time::Instant,
) -> String {
    let after: Vec<String> = trail
        .iter()
        .filter(|(t, ..)| *t > last_progress)
        .map(|(t, top, inner, n)| {
            format!(
                "+{}s {:?}/{:?} {}B",
                t.saturating_duration_since(last_progress).as_secs(),
                top,
                inner,
                n
            )
        })
        .collect();
    if after.is_empty() {
        "只有心跳".to_string()
    } else {
        after.join("; ")
    }
}

/// 最近 [`FRAME_TRAIL_CAP`] 个非心跳帧,按流起点算相对毫秒:`+1234ms [顶层]/[1.x] 字节`。
///
/// 空回复诊断用(2026-11-09:长会话「出字前提交 → 20s 无进展 → 空回复」,
/// 上游照样计费几百~几千输出 token,要看提交之后上游到底发了什么)。
pub(super) fn format_trail_since(
    trail: &std::collections::VecDeque<(tokio::time::Instant, Vec<u32>, Vec<u32>, usize)>,
    origin: tokio::time::Instant,
) -> String {
    if trail.is_empty() {
        return "无非心跳帧".to_string();
    }
    trail
        .iter()
        .map(|(t, top, inner, n)| {
            format!(
                "+{}ms {:?}/{:?} {}B",
                t.saturating_duration_since(origin).as_millis(),
                top,
                inner,
                n
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// 交互更新里值得单独记一笔的帧(诊断):9/10/11 总结、14 本轮结束、16/17 步骤开始/完成
/// (字段号见 cursor-agent `agent.v1.InteractionUpdate`)。字段号都 < 32,用 u32 位图去重。
pub(super) const NOTABLE_UPDATE_FIELDS: &[u32] = &[9, 10, 11, 14, 16, 17];

/// 工具轮 commit 到手后,再等尾部描述符的宽限。
///
/// 探针实测工具轮的尾序是 回调 → KV checkpoint → turn_commit → `.3`×N —— 描述符
/// 在 commit 之后很快到齐,但流**永不自关**(上游持 BiDi 等工具结果)。宽限给足
/// 描述符到达的时间就主动收:太短会漏掉 ref 更全的第二份 `.3`,太长每个工具轮
/// 都白等(客户端的工具执行已经排在这段后面了)。
pub(super) const TOOL_COMMIT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// 上游的 call id 变成能给客户端的 `tool_use.id`。
///
/// Cursor 的 call id 是**两段用换行连起来**的:`call-<uuid>-N\nfc_<uuid>_N`
/// (见 `run::ToolCall::id`)。原样交出去有两个后果:客户端若校验 id 形态会拒收;
/// 日志里这一行会被劈成两半(实测 `tool=read id=call-… ⏎ fc_…_0 args=1`,
/// 排查时看起来像日志损坏)。
///
/// 只取第一段:`call-<uuid>-N` 本身就唯一。**上游侧不受影响** —— 我们从不把
/// call id 发回 Cursor(每轮都是全新的 Opening 请求,上一轮的 call id 被丢弃);
/// 将来真要走请求侧 `field 2` 工具通道回传结果,那里必须用 `ToolCall::id` 的**原值**,
/// 不能用这里这个。
pub(crate) fn client_tool_id(raw: &str) -> String {
    raw.split(['\n', '\r'])
        .next()
        .unwrap_or(raw)
        .trim()
        .to_string()
}

/// 本轮失败:通知调用方别把会话记成「服务端已建立」。
pub(super) fn fail(outcome: &Option<OutcomeHook>) {
    if let Some(h) = outcome {
        h(false);
    }
}

/// 挂流续跑选项(见 [`crate::held`])。
#[derive(Default)]
pub(crate) struct HoldOpts {
    /// `Some(账号)` = 本轮若是纯 MCP 工具调用轮,收尾时把上游流挂起等结果。
    pub account_id: Option<String>,
    /// `Some` = 本段是续跑:接着读挂起的流(解码器与心跳原样带过来)。
    pub resume: Option<ResumeState>,
}

/// 续跑段要接上的挂起状态。
pub(crate) struct ResumeState {
    pub decoder: wire::FrameDecoder,
    pub heartbeat: Option<crate::held::HeartbeatGuard>,
    /// 本段开头已在原流上回过结果的调用(客户端 id)。
    pub answered_ids: Vec<String>,
}

/// 并行工具调用的 exec 帧收齐宽限(见 `exec_collect_deadline`)。
pub(super) const EXEC_COLLECT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// 把本轮交给客户端的工具调用与收到的 exec mcp_args 一一配对。
///
/// 只认两种:调用 id(清洗后)完全相等;或本轮恰好一个调用、一个 exec 帧。
/// 多个并行调用里有对不上的就返回 `None`(不挂起)—— 按顺序猜会把结果喂给错的调用。
pub(super) fn pair_pending_execs(
    calls: &[run::ToolCall],
    execs: &[(String, String, u64, String, crate::held::PendingKind)],
) -> Option<Vec<crate::held::PendingExec>> {
    if calls.is_empty() || execs.len() < calls.len() {
        return None;
    }
    if calls.len() == 1 && execs.len() == 1 {
        let (_, _, id, exec_id, kind) = &execs[0];
        return Some(vec![crate::held::PendingExec {
            tool_use_id: client_tool_id(&calls[0].id),
            id: *id,
            exec_id: exec_id.clone(),
            kind: kind.clone(),
        }]);
    }
    let mut out = Vec::with_capacity(calls.len());
    for c in calls {
        let want = client_tool_id(&c.id);
        let (_, _, id, exec_id, kind) = execs.iter().find(|(cid, ..)| client_tool_id(cid) == want)?;
        out.push(crate::held::PendingExec { tool_use_id: want, id: *id, exec_id: exec_id.clone(), kind: kind.clone() });
    }
    Some(out)
}

/// Cursor 帧流 → Anthropic SSE(不挂起、不续跑)。
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn stream_to_anthropic(
    byte_stream: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    model: String,
    want_thinking: bool,
    body_keepalive: BodyKeepalive,
    outcome: Option<OutcomeHook>,
    conversation_id: String,
    assets: std::sync::Arc<crate::AssetStore>,
    notices: std::sync::Arc<crate::TruncationNotices>,
    xlate: BuiltinXlate,
    usage_fallback: ChatUsage,
    sim_cache_read: u64,
    cli_mode: bool,
    upload: Option<WireUpload>,
    shadow: Option<crate::ShadowFeed>,
) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    stream_to_anthropic_with(
        byte_stream,
        model,
        want_thinking,
        body_keepalive,
        outcome,
        conversation_id,
        assets,
        notices,
        xlate,
        usage_fallback,
        sim_cache_read,
        cli_mode,
        upload,
        shadow,
        HoldOpts::default(),
    )
}

/// Cursor 帧流 → Anthropic SSE。
#[allow(clippy::too_many_arguments)]
pub(super) fn stream_to_anthropic_with(
    byte_stream: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    model: String,
    // 调用方是否请求了 thinking(见 `chat_stream` 里的取值处)。
    want_thinking: bool,
    body_keepalive: BodyKeepalive,
    outcome: Option<OutcomeHook>,
    // 服务端 write/read 调用要用:资产按会话存取(见 [`crate::AssetStore`])。
    conversation_id: String,
    assets: std::sync::Arc<crate::AssetStore>,
    // 内建工具收口时往这里记一笔,下一轮同会话请求据此注入纠偏
    // (见 [`truncation_notice`] 与 [`crate::TruncationNotices`])。
    notices: std::sync::Arc<crate::TruncationNotices>,
    // 内建调用 → 声明工具的兼容转换表(按本次请求的 tools 生成,见 [`BuiltinXlate`])。
    // 认得出的内建调用翻译成标准 tool_use 下发;认不出的落回收口 + 纠偏。
    xlate: BuiltinXlate,
    // 上游没给 `1.14` 时的用量兜底(见 [`estimate_usage_fallback`])。
    usage_fallback: ChatUsage,
    // 模拟的 cache_read(见 [`crate::cache_sim`]):上游 `1.14` 的 cached 为 0 时
    // 在收尾处顶替,封顶 input_tokens;上游报了真实命中时不用它。
    sim_cache_read: u64,
    // CLI 形态(见 `cli.rs`):响应没有 `1.14` 用量帧,收尾认 `is_turn_commit` 回显。
    cli_mode: bool,
    // wire v2 上传腿:推迟的 ENV/控制帧 + 应答 `4.2` 点名的节库(见 [`WireUpload`])。
    upload: Option<WireUpload>,
    // 续轮描述符影子捕获(纯观测,见 [`crate::DescriptorShadow`]);`None` = 不捕
    // (测试里不关心影子的老用例)。
    shadow: Option<crate::ShadowFeed>,
    hold: HoldOpts,
) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamItem, UpstreamError>>(32);

    tokio::spawn(async move {
        use futures::StreamExt;
        // 全程持有发送端:除了「不 half-close」,服务端的 write/read 调用回执
        // 也经它从请求侧送回去(见下面的 exec 分支)。任务结束时随栈 drop,流才收。
        let mut body_keepalive = body_keepalive;
        // ── 请求侧保活:`{7:}` client_heartbeat,每 5s 一发,覆盖整轮 ─────────────
        //
        // 2026-09-04 探针钉死(examples/probe_wire_v2,对照 YeautyYE fork client.rs:633):
        // 官方 CLI 全程每 ~5s 发一个 `{7:}` 空帧;我方此前只在初始帧序里发一次,
        // 之后干等,服务端把这条流当死客户端 → 200 + 只回心跳、永不生成的「裸态」。
        // 补发后裸态消失,整轮 3-6s 干净收尾。帧与内容槽回执交错是实物形态
        // (08-23 抓包里 `{7:}` 本就夹在 slot 7/8 之间),不会干扰 KV 子协议。
        // 防护:本任务所有出口经 Drop abort;心跳任务自身发送失败(流已关)也会退出。
        // 续跑段沿用挂起时那个心跳任务(挂起期间它一直在发);新流才起新的。
        let HoldOpts { account_id: hold_account, resume } = hold;
        let (resume_decoder, resume_hb, answered_ids) = match resume {
            Some(r) => (Some(r.decoder), r.heartbeat, r.answered_ids),
            None => (None, None, Vec::new()),
        };
        let resumed = resume_decoder.is_some();
        let mut _hb: Option<crate::held::HeartbeatGuard> = match resume_hb {
            Some(g) => Some(g),
            None => body_keepalive
                .as_ref()
                .map(|sink| crate::held::spawn_client_heartbeat(sink.clone())),
        };
        let msg_id = format!("msg_{}", uuid::Uuid::new_v4().simple());

        let mut decoder = resume_decoder.unwrap_or_default();
        // ⚠️ message_start 必须等到**真的有内容**再发。一旦首字节写给客户端,这次请求
        // 就 committed 了,gw-app 再也不能换号重试。而 Run 的错误恰恰是在流**末尾**的
        // trailer 里报的 —— 先发 message_start 等于把所有可重试的错误变成不可重试。
        let mut started = false;
        let mut output_chars: usize = 0;
        // 本轮 assistant 输出的渲染文本,用来算入库链的末节指纹。
        // 口径必须与 clidrv 的 `append_assistant_fp` 一致(同一个 `turn_fp`):
        // 正文增量拼接、thinking 不进(它走 `fr.thinking`,本来就不在这里)。
        let mut assistant_text = String::new();
        // 正文里的非 ASCII 字数:收尾估 output token 时中文不能按 4 字符/token 算
        // (见 est_text_tokens)。
        let mut output_nonascii: usize = 0;
        let mut trailer_err: Option<UpstreamError> = None;
        // 上游自报的用量(`1.14`)。有它就不用 chars/4 估 —— 那个估法在中文上偏差很大。
        let mut upstream_usage: Option<ChatUsage> = None;
        // 距上一次「有进展的帧」过了多久。**这是本 provider 唯一的失败兜底**:
        // 上游那个「200 + 每 10 秒心跳、永不生成」的状态**不会结束流、不发 trailer、
        // 不报任何错**,所以下面 `while let Some(...)` 会永远等下去,末尾那个
        // EmptyResponse 分支根本到不了。这正是本轮修的 bug 的形态 —— 而协议还会漂移,
        // 下次同样的形态一定会再出现一次。上游不给错误码,就得我们自己造一个。
        let mut last_progress = tokio::time::Instant::now();
        // 「只有干活帧(工具参数增量 / token 计数…,见 run::is_work_update)、没有可见输出」
        // 从什么时候开始。干活帧会续停滞闸的命,这个时刻给它封顶([`WORK_ONLY_CAP`])。
        let mut work_only_since: Option<tokio::time::Instant> = None;
        // 工具调用开始组装(`1.7`/`1.15`)的时刻;来了正文就清掉。组装期间上游只发心跳。
        let mut assembling_since: Option<tokio::time::Instant> = None;
        // 服务端开始总结压缩上下文的时刻(`1.10`);`1.11` 或模型开口即清。见 [`SUMMARY_MAX`]。
        let mut summarizing_since: Option<tokio::time::Instant> = None;
        // 上次给 gw-app 交 ping 的时刻(干活期间续 gw-app 的静默计时,见 [`WORK_PING_EVERY`])。
        let mut last_ping = tokio::time::Instant::now();
        // 最近的非心跳帧 `(到达时刻, 顶层字段, 内层字段, 字节数)`:停滞收口时把
        // 「最后一次进展之后到底来了什么」打进日志,下次不用再猜。
        let mut frame_trail_seq: u32 = 0;
        // 本条流的起点:空回复诊断里的帧序按它算相对时间(见 [`format_trail_since`])。
        let stream_began = tokio::time::Instant::now();
        // 已记过 INFO 的总结/步骤/轮次结束帧字段(位图,每条流每种只记一次,防刷屏)。
        let mut notable_logged: u32 = 0;
        let mut frame_trail: std::collections::VecDeque<(tokio::time::Instant, Vec<u32>, Vec<u32>, usize)> =
            std::collections::VecDeque::new();
        // 翻译不了、正在等 exec 帧的内建工具界面帧 `(开始等的时刻, 原帧)`(见 [`BUILTIN_EXEC_WAIT`])。
        let mut builtin_wait: Option<(tokio::time::Instant, Vec<u8>)> = None;
        // 本轮见过的内建编辑界面帧(`.12`)的目标路径。随后对同一路径的 exec 读 =
        // 内建 StrReplace 在取原文件,要回拒绝(见 [`edit_reject_reason`]),不能翻成客户端读。
        let mut pending_edit: Option<String> = None;
        // 见过明确的收尾(用量帧或 end-stream trailer)。**没见过就不许报 end_turn** ——
        // 上游或中间代理用 HTTP/2 END_STREAM 干净截断时,底层流只是返回 `None`,
        // 于是一个「答了一半」的回复会被当成成功收尾:客户拿到残缺答案,
        // 账号健康度和请求日志却毫发无损,没有任何地方看得出来出过事。
        let mut saw_end = false;
        // ── 影子模式(纯观测)─────────────────────────────────────────────
        // 本轮捕到的**最后一份**续轮描述符(顶层 `.3` 原始字节 + ref 数),
        // 只是暂存:流**干净收尾**(trailer / 用量帧 / turn_commit 后排水段排完)
        // 才提交进影子表。中途捕获的尾部 `.3` 可能装着**未完结**轮次的状态(等工具
        // 结果的轮、被截断的轮)—— 提交错描述符 = 将来回放时上下文缺一块 =
        // 模型静默变傻(claude 评审红线)。
        let mut shadow_staged: Option<(Vec<u8>, usize)> = None;
        // 本条响应实际下发的思考文本;流结束(任何出口)时落进回声表,
        // 下一轮客户端若把它当普通正文回传,转历史时据此置空(见 crate::thinking_echo)。
        let mut thinking_echo = crate::thinking_echo::Recorder::default();
        // wire v2:反应式帧序 + 点名应答由 `wirev2::WireDriver` 统一承担
        // (**探针驱动同一个实现**,见该模块文档:两份实现会让探针结论不可迁移)。
        // 只在 CLI 形态挂 —— IDE 没有这套内容寻址。
        let (mut wire_driver, mcp_tools) = match upload {
            Some(u) => (
                Some(crate::wirev2::WireDriver::new(u.deferred, u.sections).with_overlay(u.overlay)),
                u.mcp_tools,
            ),
            None => (None, Vec::new()),
        };
        // wire v2 判据的计数**全在 driver 里**(主段/排水段分离,见
        // `WireDriver::set_draining`)。这里只留「见过可接受的描述符」这一个本地状态。
        let mut wire_saw_desc = false;
        // 排水失败(停滞/读错/解压错)必须**作废已存描述符**(codex 审查 #2):
        // 只是不提交本轮的话,下一轮 lookup 仍会返回上一轮那份 —— 那份缺了本轮,
        // 于是只发增量却配一个缺一轮的清单 = 静默失忆。
        let mut shadow_must_invalidate = false;
        // 收尾是否「干净可提交」。只在三种干净收尾分支里置位;
        // 内建截断不置位;工具轮在「commit+描述符宽限收工」时置位(2026-09-04 起,
        // 见 await_tool_commit);turn_commit 本身不置位 —— 它只是排水段的开始(见下)。
        let mut shadow_committable = false;
        // turn_commit 之后的**纯排水段**标记(codex 三轮 Finding 1):只继续读帧
        // 给影子捕尾部 `.3`,其余一律忽略 —— 尾帧不再过完整状态机(usage 不覆写、
        // 不产 SSE、不动计费),保证「观测零出站变化」。
        let mut draining = false;
        // 工具轮续捕(2026-09-04 toolcont 探针):置位 = 已把 tool_use 交给调用方
        // (客户端侧已收尾),流继续读**只为捕工具轮的 turn_commit 与尾部描述符**。
        // 拿到 commit 的时刻记进 `tool_commit_at`,宽限期到即收(工具轮的流永不
        // 自关 —— 上游持 BiDi 等工具结果,等 EOF 等于陪跑到停滞闸)。
        let mut await_tool_commit = false;
        let mut tool_commit_at: Option<tokio::time::Instant> = None;
        // ── 挂流续跑(见 crate::held)────────────────────────────────────────
        // 能挂:开关开、本段是 CLI 形态、请求流保持打开。
        let hold_on = hold_account.is_some() && body_keepalive.is_some() && cli_mode;
        // 本轮见到的、上游在等结果的 exec:(调用 id, 工具名, exec id, exec_id, 种类)。
        // MCP 的 mcp_args 与可挂起的自带工具(读文件 / 终端,见 `crate::held::PendingKind`)都进这里。
        let mut mcp_execs: Vec<(String, String, u64, String, crate::held::PendingKind)> = Vec::new();
        // 本轮交付过可挂起的自带工具调用、正在等它的 exec 帧(读文件 / 终端,界面帧之后同毫秒到)。
        let mut builtin_hold_pending = false;
        // 并行调用的 exec 帧没收齐时最多再等多久(见 [`EXEC_COLLECT_GRACE`])。
        let mut exec_collect_deadline: Option<tokio::time::Instant> = None;
        // 收齐了:收尾时挂起。
        let mut hold_ready = false;
        // wire v2:**所有失败出口都必经的作废点**(codex 三轮 #3)。
        //
        // 早 return 的路径(读流失败 / 错误 trailer / 空回复 / 流断)以前绕过作废,
        // 于是最典型的失败链——「`4.2` 点名 → 拿不出来 → 上游等内容 → stall/EOF」——
        // 既拿不到判决也不作废旧描述符,下一轮继续拿同一份清单撞同一面墙。
        //
        // 判据故意粗:**只要本轮是失败退出就作废**,不再算判决。失败轮的描述符状态
        // 本就不可信,多作废一次只是下轮重铺(不省钱),漏作废是静默失忆。
        let wire_invalidate = |why: &str| {
            if !cli_mode {
                return;
            }
            if let Some(feed) = &shadow {
                let dropped = feed.map.invalidate(&conversation_id, &feed.account_id);
                if dropped > 0 {
                    tracing::warn!(
                        conversation_id = %conversation_id,
                        account = %feed.account_id,
                        dropped, why,
                        "cursor Run:本轮失败退出,作废该会话描述符(下轮重铺)"
                    );
                }
            }
        };
        // 上游要求执行的工具(若有)。收尾时变成一个 `tool_use` 块 + `stop_reason: tool_use`。
        // 本轮模型发起的全部工具调用(并行调用帧逐个收,不再只留最后一个 ——
        // 旧实现并行调用会静默丢掉除最后一发外的全部,客户端只见一发,
        // 模型却等三发结果)。post-loop 逐个下发 tool_use。
        let mut tool_calls: Vec<run::ToolCall> = Vec::new();
        // 内建工具收口发生在「已经出过字」之后 = 本次回答被截断,但仍会按 end_turn 收尾。
        // 单独记一个标记,让收尾处能把它写进日志 —— 不然它和正常收尾在日志里没有区别。
        let mut builtin_truncated = false;
        // 正文开始后被丢掉的思考字数(见思考透传那一段的 else 分支)。
        let mut dropped_thinking: usize = 0;
        // 当前开着的内容块 `(索引, 种类)`,以及下一个可用索引。
        // Anthropic 的块索引必须**连续且不重复**,而 thinking / text / tool_use
        // 三种块都可能缺席,所以索引只能顺序分配,不能写死 0 和 1。
        let mut open_block: Option<(usize, &'static str)> = None;
        let mut next_idx: usize = 0;
        let mut byte_stream: crate::held::ByteStream = Box::pin(byte_stream);
        // 最后一次从上游收到**任何字节**(心跳也算)的时刻:停滞时据此区分「连接是活的、上游不出字」
        // 与「连接只发不收」(见 is_connection_stall)。
        let mut last_bytes_at = tokio::time::Instant::now();

        'outer: loop {
            let next = tokio::select! {
                biased;
                // 下游放弃了(客户端断连 / gw-app 换号重试)就立刻收工。
                // 不看这个的话:心跳状态下我们永远不会尝试 send,也就永远发现不了
                // receiver 已经没了,任务连同请求体 sender 一起长住,HTTP/2 流泄漏。
                _ = tx.closed() => {
                    tracing::debug!("cursor Run:下游已关闭,提前收工");
                    return;
                }
                chunk = byte_stream.next() => chunk,
                // ⚠️ **独立的推迟帧兜底期限分支**(codex 终审 #2)。
                //
                // 只在「收到响应帧时」检查这个期限是不够的:ack 之后上游若静默,根本
                // 没有帧来触发检查,ENV 就一路拖到 90s stall 闸才被发现 —— 而 ENV 迟发
                // 本身可能就是上游不出字的原因,那就成了自己造的死锁。
                _ = tokio::time::sleep_until(
                    wire_driver.as_ref().map(|d| tokio::time::Instant::from_std(d.deferred_deadline()))
                        .unwrap_or_else(|| tokio::time::Instant::now() + STALL_TIMEOUT)
                        .into()
                ), if wire_driver.as_ref().is_some_and(|d| d.has_deferred()) => {
                    if let Some(d) = wire_driver.as_mut() {
                        d.flush_deferred(body_keepalive.as_ref(), "兜底期限").await;
                    }
                    last_progress = tokio::time::Instant::now();
                    continue;
                }
                // 并行调用的 exec 帧没在宽限内收齐:不挂起,按旧行为收工。
                _ = tokio::time::sleep_until(
                    exec_collect_deadline.unwrap_or_else(tokio::time::Instant::now).into()
                ), if exec_collect_deadline.is_some() => {
                    tracing::info!(
                        conversation_id = %conversation_id,
                        calls = tool_calls.len(),
                        execs = mcp_execs.len(),
                        "cursor Run:并行工具调用的 exec 编号没收齐,本轮不挂起"
                    );
                    // 工具调用已交付,是正常的 tool_use 收尾。中途若有排水段被打断,
                    // saw_end 会被清掉,不补的话收尾按「流中断」报错。
                    saw_end = true;
                    break 'outer;
                }
                // 工具轮宽限收工:commit 到手且描述符已暂存,宽限一到就收。
                // 工具轮的流永不自关(上游持 BiDi 等结果),等 EOF/停滞闸是白等 ——
                // 客户端的工具执行排在我们的 message_stop 后面,每晚一秒都是
                // 工具回路的纯延迟。 biased 序在停滞闸之前:宽限更紧,先判它。
                _ = tokio::time::sleep_until(
                    (tool_commit_at.unwrap_or_else(tokio::time::Instant::now)
                        + TOOL_COMMIT_GRACE)
                    .into()
                ), if await_tool_commit && tool_commit_at.is_some() => {
                    if wire_saw_desc {
                        shadow_committable = true;
                        tracing::debug!("cursor Run:工具轮 commit+描述符到手,宽限收工(描述符可提交)");
                    } else {
                        tracing::debug!("cursor Run:工具轮 commit 到手但宽限内无描述符,按链中断作废旧料");
                        shadow_must_invalidate = true;
                    }
                    break 'outer;
                }
                // 等内建工具 exec 帧超时:退回原收口(出字前报错 / 出字后截断)。
                _ = tokio::time::sleep_until(
                    builtin_wait
                        .as_ref()
                        .map(|(t, _)| *t + BUILTIN_EXEC_WAIT)
                        .unwrap_or_else(tokio::time::Instant::now)
                        .into()
                ), if builtin_wait.is_some() => {
                    let (_, frame) = builtin_wait.take().expect("守卫已确认 Some");
                    // 轮次已经有了结果(工具调用已收、在等 commit、已进排水段):
                    // 这次内建调用只是附带的,丢掉等待即可,绝不能把成功的轮次改判成错误。
                    if draining || await_tool_commit || !tool_calls.is_empty() {
                        tracing::debug!(
                            conversation_id = %conversation_id,
                            "cursor Run:等内建工具 exec 帧超时,但轮次已有结果,忽略"
                        );
                        continue;
                    }
                    tracing::warn!(
                        conversation_id = %conversation_id,
                        cli_mode,
                        "cursor Run:等内建工具 exec 帧超时,退回收口"
                    );
                    let close = close_untranslatable_builtin(
                        &frame, started, output_chars, &notices, &conversation_id,
                    );
                    trailer_err = trailer_err.or(close.error);
                    builtin_truncated |= close.truncated;
                    saw_end = true;
                    break 'outer;
                }
                _ = tokio::time::sleep_until(
                    // 三档:排水段 20s(尾帧本该 1~2s 到齐,只剩心跳 = 挂死),
                    // 首字前 90s(冷启动/长考留余量),出字后 150s
                    // (大件组装余量,见 STALL_TIMEOUT_STARTED)。
                    stall_deadline(
                        last_progress,
                        if draining {
                            DRAIN_STALL_TIMEOUT
                        } else if started {
                            STALL_TIMEOUT_STARTED
                        } else {
                            STALL_TIMEOUT
                        },
                        work_only_since,
                        // 排水段不享受组装放宽:commit 之后只剩心跳就是挂死。
                        if draining { None } else { assembling_since },
                        if draining { None } else { summarizing_since },
                    )
                    .into()
                ) => {
                    let stall = if draining {
                        DRAIN_STALL_TIMEOUT
                    } else if started {
                        STALL_TIMEOUT_STARTED
                    } else {
                        STALL_TIMEOUT
                    };
                    // 实际触发的上限与常规上限不同 = 是组装/干活封顶那条在起作用。
                    let work_capped = stall_deadline(
                        last_progress,
                        stall,
                        work_only_since,
                        if draining { None } else { assembling_since },
                        if draining { None } else { summarizing_since },
                    ) != last_progress + stall;
                    let trail = format_trail(&frame_trail, last_progress);
                    // 排水段的停滞**不是错误**:轮次在 turn_commit 已经成功,
                    // 只是影子失去「干净收尾」资格(不提交),轮次照常收尾。
                    // 工具轮等 commit 时的停滞同义:tool_use 早已交付,轮次是好的,
                    // 只是续轮料没拿到 —— 作废旧描述符(=旧行为),不判错误。
                    if draining || await_tool_commit {
                        tracing::debug!(
                            secs = stall.as_secs(),
                            tool_turn = await_tool_commit,
                            "cursor Run:尾流/工具轮停滞,放弃影子提交并作废旧描述符(轮次照常成功)"
                        );
                        // 诊断:提交之后排水段停滞、而本轮一个字一个工具调用都没有 —— 收尾会报空回复。
                        if draining && output_chars == 0 && tool_calls.is_empty() {
                            tracing::warn!(
                                conversation_id = %conversation_id,
                                elapsed_ms = stream_began.elapsed().as_millis() as u64,
                                started,
                                trail = %format_trail_since(&frame_trail, stream_began),
                                "cursor Run:提交后排水段无进展且零输出(将报空回复),帧序见 trail"
                            );
                        }
                        shadow_must_invalidate = true;
                        break 'outer;
                    }
                    tracing::warn!(
                        conversation_id = %conversation_id,
                        secs = stall.as_secs(),
                        started,
                        output_chars,
                        work_capped,
                        work_only_secs = work_only_since.map(|t| t.elapsed().as_secs()),
                        assembling_secs = assembling_since.map(|t| t.elapsed().as_secs()),
                        trail = %trail,
                        "cursor Run:上游只发心跳、无任何进展,按上游错误收口"
                    );
                    // 停滞是**瞬态服务故障**(可重试),不是内容级拒答 —— 不能用
                    // EmptyResponse(它对内容是确定性的、对外映射 400 止重试,
                    // 见 upstream_status)。ServerError:非流式 → 502 可重试;流式
                    // 窥探 10s 超时早已提交 200,客户端拿到流内 api_error SSE
                    // (语义同为"服务端问题、可重试",只是形态不同)。
                    // (2026-08-24 codex 审查:两者混用会把瞬态停滞误表述成客户请求有错。)
                    let why = if let Some(sm) = summarizing_since.filter(|_| !draining) {
                        format!(
                            "Cursor Run 服务端总结压缩上下文 {}s 仍未完成",
                            sm.elapsed().as_secs()
                        )
                    } else if work_capped {
                        format!(
                            "Cursor Run {}s 内只有工具参数/计数增量、没有任何可见输出",
                            WORK_ONLY_CAP.as_secs()
                        )
                    } else if last_bytes_at.elapsed() >= CONN_DEAD_SILENCE {
                        // 连心跳都没有:连接「只发不收」,调用方据此丢弃这个号的连接重建。
                        format!(
                            "Cursor Run {}s 内无进展,{CONN_DEAD_MARK} {}s(连接疑似卡死)",
                            stall.as_secs(),
                            last_bytes_at.elapsed().as_secs()
                        )
                    } else {
                        format!(
                            "Cursor Run {}s {STALL_HEARTBEAT_ONLY}、没有任何文本或用量帧",
                            stall.as_secs()
                        )
                    };
                    trailer_err = Some(UpstreamError::new(UpstreamErrorKind::ServerError, why));
                    break 'outer;
                }
            };
            let Some(chunk) = next else {
                // 流自然结束:排水段排到这里 = 干净收尾,影子可提交(尾部 .3 已暂存)。
                if draining {
                    shadow_committable = true;
                }
                break 'outer;
            };
            let bytes = match chunk {
                Ok(b) => {
                    last_bytes_at = tokio::time::Instant::now();
                    b
                }
                Err(e) => {
                    // 排水段读失败同理:轮次已成功,只放弃影子,绝不让已成功的
                    // 轮次变红(见 turn_commit 分支的排水段注释)。
                    if draining {
                        shadow_must_invalidate = true;
                        tracing::debug!(error = %e, "cursor Run:turn_commit 后尾流读失败,放弃影子提交并作废旧描述符(轮次照常成功)");
                        break 'outer;
                    }
                    wire_invalidate("读取流失败");
                    fail(&outcome);
                    let _ = tx
                        .send(Err(UpstreamError::network(format!(
                            "读取 Cursor 流失败: {e}"
                        ))))
                        .await;
                    return;
                }
            };
            decoder.feed(&bytes);
            while let Some((flag, raw)) = decoder.next_frame() {
                // 先按 flag bit0 解压,再谈这一帧是什么。顺序反了的话,压缩过的
                // trailer(flag=0x03)会被当成明文 JSON 解析失败并被静默吞掉。
                let payload = match wire::frame_payload(flag, &raw) {
                    Ok(p) => p,
                    Err(e) => {
                        if draining {
                            shadow_must_invalidate = true;
                            tracing::debug!(error = %e, "cursor Run:turn_commit 后尾帧解压失败,放弃影子提交并作废旧描述符(轮次照常成功)");
                            break 'outer;
                        }
                        trailer_err = Some(UpstreamError::new(
                            UpstreamErrorKind::ServerError,
                            format!("Cursor 帧解压失败(flag={flag:#04x}): {e}"),
                        ));
                        break 'outer;
                    }
                };

                if flag & 0x02 != 0 {
                    // 排水段排到 trailer = 干净收尾,提交影子;trailer 内容不再
                    // 解析 —— 轮次在 turn_commit 已成功,错误 trailer 也改不了它。
                    if draining {
                        let text = String::from_utf8_lossy(&payload);
                        if let Some(t) = run::parse_trailer(&text) {
                            // 记下原文:这一轮若一个字都没出,客户只会看到「空回复」,
                            // 真正的上游原因就在这里(2026-11-07 fable 续轮空回复排查)。
                            let snippet: String = text.chars().take(600).collect();
                            tracing::warn!(
                                started,
                                tool_calls = tool_calls.len(),
                                trailer = %snippet,
                                "cursor Run:turn_commit 后尾部出现错误 trailer"
                            );
                            // 已经出过字或工具调用:轮次已交付,错误改不了它,照旧收尾。
                            // 什么都没出:这是一次上游失败 —— 报上游错误(可重试/换号),
                            // 而不是落到下面的 EmptyResponse(对外 400、止重试)。
                            if !started && tool_calls.is_empty() {
                                trailer_err = Some(trailer_to_error(&t));
                                break 'outer;
                            }
                        }
                        saw_end = true;
                        shadow_committable = true;
                        break 'outer;
                    }
                    let text = String::from_utf8_lossy(&payload);
                    if let Some(t) = run::parse_trailer(&text) {
                        trailer_err = Some(trailer_to_error(&t));
                    }
                    // 干净的 end-stream trailer 也是正常收尾 —— 与用量帧同级。
                    saw_end = true;
                    shadow_committable = true;
                    break 'outer;
                }

                if !run::is_heartbeat(&payload) {
                    // 排障用:设了 CURSOR_DUMP_FRAMES=<目录> 才把每个非心跳帧按序落盘
                    // (生产不设)。「200 但没字」这类问题要看整条帧序,工具帧落盘不够。
                    if let Ok(dir) = std::env::var("CURSOR_DUMP_FRAMES") {
                        let f = format!(
                            "{dir}/{}_{:05}_{}.bin",
                            conversation_id,
                            frame_trail_seq,
                            payload.len()
                        );
                        let _ = std::fs::write(&f, &payload);
                    }
                    frame_trail_seq += 1;
                    let inner_now = run::inner_fields(&payload);
                    let fresh: u32 = inner_now
                        .iter()
                        .filter(|f| NOTABLE_UPDATE_FIELDS.contains(f))
                        .fold(0, |m, f| m | (1u32 << f))
                        & !notable_logged;
                    if output_chars == 0 && tool_calls.is_empty() && fresh != 0 {
                        notable_logged |= fresh;
                        tracing::info!(
                            conversation_id = %conversation_id,
                            fields = ?inner_now,
                            elapsed_ms = stream_began.elapsed().as_millis() as u64,
                            draining,
                            "cursor Run:出字前收到总结/步骤/轮次结束帧(诊断)"
                        );
                    }
                    if frame_trail.len() >= FRAME_TRAIL_CAP {
                        frame_trail.pop_front();
                    }
                    frame_trail.push_back((
                        tokio::time::Instant::now(),
                        run::top_fields(&payload),
                        run::inner_fields(&payload),
                        payload.len(),
                    ));
                }
                let fr = run::parse_frame(&payload);
                // 上游帧种类很多(会话回显 field 4、计时 field 8、状态 1.8、思考 1.4…),
                // 只有 1.1.1 是正文。debug 级把每帧点名,排查「200 但没字」时不用再抓包。
                // 文本/思考带 120 字预览:排「模型说完半句就停」时必须看到它想干什么。
                tracing::debug!(
                    flag,
                    bytes = payload.len(),
                    fields = ?run::top_fields(&payload),
                    inner = ?run::inner_fields(&payload),
                    status = ?run::status_code(&payload),
                    text_len = fr.text.chars().count(),
                    thinking_len = fr.thinking.chars().count(),
                    // 排障四元组(2026-09-04 pi 卡死案留下):停滞闸不 firing 时,
                    // 靠 idle_secs 与 started/draining/await_tool_commit 一眼定位
                    // 是哪个状态在续命。debug 级,常态无成本。
                    idle_secs = last_progress.elapsed().as_secs(),
                    started,
                    draining,
                    await_tool_commit,
                    text_preview = %fr.text.chars().take(120).collect::<String>(),
                    thinking_preview = %fr.thinking.chars().take(120).collect::<String>(),
                    "cursor Run 响应帧"
                );

                // 影子捕获:响应帧顶层带 `.3` 就暂存(一帧多份取尾部,见
                // `run::descriptor_field3`)。**只观测,绝不动出站**;日志照打,
                // 但此时一律「未收尾不入库」—— 提交点在循环外的干净收尾处。
                // ── wire v2:存回显 / 应答点名 / 按登记通知发推迟帧 ───────────
                //
                // 三件事都在 `WireDriver::on_frame` 里,**与探针共用同一份实现**。
                // 只在 CLI 形态挂 driver(IDE 没有这套内容寻址,见 driver 的构造处)。
                if let Some(d) = wire_driver.as_mut() {
                    let eff = d.on_frame(&payload, body_keepalive.as_ref()).await;
                    // 主段/排水段的隔离已收进 driver(见 `WireDriver::set_draining`),
                    // 这里不再自己加计数 —— 探针读同一份 getter,两边自动同源。
                    if eff.demanded > 0 {
                        tracing::warn!(
                            conversation_id = %conversation_id,
                            demanded = eff.demanded,
                            unavailable = eff.unavailable,
                            draining,
                            counted = !draining,
                            "cursor Run:上游点名要内容分节(field-4 `4.2`)"
                        );
                    }
                    if eff.demanded > 0 || eff.sections_stored > 0 || eff.deferred_sent > 0 {
                        last_progress = tokio::time::Instant::now();
                    }
                }

                // ── exec 握手(mcp_state / list_mcp_resources,见 `crate::exec`)──────
                //
                // 阻塞式:不回服务端只发心跳。必须排在排水段与工具臂**之前** —— 它也挂在
                // 顶层 field 2 下,旧判据 `run::is_tool_call` 会把它当「内建工具」收口
                // (10-03 起 CLI 形态带工具请求 0 交付的根因)。任何状态下都应答。
                if let Some(ef) = crate::exec::parse(&payload) {
                    if let Some(h) = ef.handshake() {
                        let default_ident = if cli_mode {
                            crate::cli::MCP_PROVIDER_CLAUDE_LOCAL
                        } else {
                            run::TOOL_NS
                        };
                        let reply = wire::frame(&ef.reply(&h, &mcp_tools, default_ident));
                        match wire_send_frame(&body_keepalive, reply).await {
                            Ok(()) => tracing::info!(
                                conversation_id = %conversation_id,
                                handshake = ?h,
                                tools = mcp_tools.len(),
                                "cursor Run:已应答 exec 握手"
                            ),
                            Err(why) => tracing::warn!(
                                conversation_id = %conversation_id,
                                handshake = ?h,
                                why,
                                "cursor Run:exec 握手应答发送失败,服务端将停在心跳"
                            ),
                        }
                        last_progress = tokio::time::Instant::now();
                        continue;
                    }
                    if ef.variant == crate::exec::VAR_REQUEST_CONTEXT && (ef.id != 0 || !ef.exec_id.is_empty()) {
                        // 我方主动推的上下文帧不带 id;服务端若带 id 来要,说明它在等一个
                        // 对得上 id 的应答 —— 这条日志是排查「首字前卡住」的第一线索。
                        tracing::warn!(
                            conversation_id = %conversation_id,
                            id = ef.id, exec_id = %ef.exec_id,
                            "cursor Run:服务端带 id 索要 request_context(我方主动推送的帧不带 id)"
                        );
                    }
                }

                // ── 服务端请示(顶层 7 interaction_query,见 `crate::interaction`)──────
                //
                // 阻塞式,和握手一样任何状态下都要答。请示到达说明工具参数已拼完,
                // 组装计时作废:答得了就算进展;答不了就交给 150s 停滞闸,
                // 不能再按组装上限干耗 600s(2026-11-07 生产 15 次,每次占一个并发名额)。
                if let Some(q) = crate::interaction::parse(&payload) {
                    assembling_since = None;
                    work_only_since = None;
                    match crate::interaction::reply(q) {
                        Some(p) => match wire_send_frame(&body_keepalive, wire::frame(&p)).await {
                            Ok(()) => {
                                tracing::info!(
                                    conversation_id = %conversation_id,
                                    kind = q.kind_name(), id = q.id,
                                    "cursor Run:已应答服务端请示"
                                );
                                last_progress = tokio::time::Instant::now();
                            }
                            Err(why) => tracing::warn!(
                                conversation_id = %conversation_id,
                                kind = q.kind_name(), id = q.id, why,
                                "cursor Run:服务端请示应答发送失败,将按停滞收口"
                            ),
                        },
                        None => tracing::warn!(
                            conversation_id = %conversation_id,
                            kind = q.kind_name(), id = q.id,
                            "cursor Run:服务端请示没有通用答法,将按停滞收口"
                        ),
                    }
                    continue;
                }

                // ⚠️ **描述符只在 turn_commit 同帧或之后才作数**(codex 终审 #1)。
                //
                // commit 之前收到的 `.3` 是**中间态**:它装的是一个还没完结的轮次的
                // 状态。旧写法把本轮任意时刻的 `.3` 都记有效,于是「只在 commit 前收到
                // 中间态 `.3`、commit 后正常 EOF」这条路径上,`NoDescriptor` 不触发、
                // 中间态描述符被提交到新指纹链,下一轮精确匹配成功后回放一个缺一轮的
                // 状态 —— 精确匹配挡不住它,因为链是对的、内容是错的。
                //
                // 中间态照常打日志(观测有价值),但不进 staged、不置 wire_saw_desc。
                let commit_frame = run::is_turn_commit(&payload);
                let desc_acceptable = crate::wirev2::descriptor_acceptable(draining, commit_frame);
                if let (Some(feed), Some(d3)) = (&shadow, run::descriptor_field3(&payload)) {
                    let refs = run::descriptor_ref_count(d3);
                    let fp = crate::fnv1a64(d3);
                    let changed = shadow_staged
                        .as_ref()
                        .map(|(prev, _)| crate::fnv1a64(prev) != fp);
                    tracing::info!(
                        conversation_id = %conversation_id,
                        account = %feed.account_id,
                        model = %model,
                        refs,
                        bytes = d3.len(),
                        fp = %format!("{fp:016x}"),
                        changed = ?changed,
                        acceptable = desc_acceptable,
                        "cursor Run:捕到续轮描述符(顶层 .3)"
                    );
                    if desc_acceptable {
                        wire_saw_desc = true;
                        shadow_staged = Some((d3.to_vec(), refs));
                    } else {
                        tracing::debug!(
                            "cursor Run:该 `.3` 在 turn_commit 之前 = 未完结轮次的中间态,不作数"
                        );
                    }
                }

                // ── 纯排水段(codex 三轮 Finding 1)───────────────────────────
                // turn_commit 之后的尾帧**只**给影子捕 `.3`(上面已捕),其余一律
                // 忽略:usage(1.14)不得覆写 `upstream_usage`、不产任何
                // SSE/StreamItem、不改任何计费与客户端可见输出。改动前 CLI 形态
                // 在 turn_commit 就收尾,这些尾帧本来不会被处理 —— 排水段是纯
                // 观测附加段,它的存在不得改变轮次的任何外部表现。
                if draining {
                    // ⚠️ **排水段里来工具调用帧 = 之前的 turn_commit 是提前的**
                    // (误匹配或上游乱序;2026-09-06 e2e 实证:commit 后 367B 内建
                    // 抓取帧被排水段吞掉,opencode 新闻任务整轮空转)。commit 不是尾、
                    // 工具调用才是 —— 退出排水段,让这一帧落回下方正常状态机。
                    // commit 段暂存的描述符同属「未完结轮次的中间态」,一并撤掉
                    // (与 codex 终审#1「commit 前的 .3 不作数」同一条规则)。
                    //
                    // 2026-11-07 生产(testa 空回复 ×5):服务端对 MCP 工具做延迟加载 ——
                    // 模型首次调用时服务端自己回工具定义(`1.3` 完成)、写一条**步骤**提交,
                    // 模型读完接着思考、再拼一次正式调用。那条提交被认成整轮结束,后面的
                    // 思考与长时间参数组装都在排水段被吞,20s 后空回复收口。真正的整轮
                    // 结束之后上游不会再发正文/思考/参数组装帧,所以见到它们同样说明
                    // commit 是中间步骤。已交出工具调用、在等 commit 的工具轮不在此列。
                    // 2026-11-09:出字前服务端开始总结压缩(`1.10`)同理 —— 提交是中间步骤,
                    // 总结完模型才开口。只认还没交付任何正文的轮次(交付完再总结的,照常排水收尾)。
                    let summary_resume = run::is_summary_started(&payload) && output_chars == 0;
                    let model_resumed = (!fr.text.is_empty()
                        || !fr.thinking.is_empty()
                        || run::is_tool_assembly(&payload)
                        || summary_resume)
                        && tool_calls.is_empty()
                        && !await_tool_commit;
                    if run::is_tool_call(&payload) || model_resumed {
                        tracing::warn!(
                            conversation_id = %conversation_id,
                            bytes = payload.len(),
                            model_resumed,
                            "cursor Run:排水段遇到模型继续干活的帧,turn_commit 是中间步骤 —— 退出排水段恢复正常处理"
                        );
                        draining = false;
                        saw_end = false;
                        shadow_staged = None;
                        wire_saw_desc = false;
                        if let Some(d) = wire_driver.as_mut() {
                            d.clear_draining();
                        }
                        // 不落 continue:继续走下方正常处理(工具调用臂会认出它)。
                    } else {
                        // ⚠️ 排水段只认「有意义的帧」作进展(codex 评审 P2,2026-09-04
                        // pi 卡死案):心跳/状态帧/未知回显每 10s 一个,把它们算进展
                        // 会让停滞闸永不开火,挂死流最后由 gw-app 300s 硬上限代收 ——
                        // 客户端干等 5 分钟。有意义的口径:描述符 `.3`、正文/思考/用量
                        // (KV 效果的刷新在上方 driver 分支已做)。真尾帧 1~2s 内到齐,
                        // 收紧后由 [`DRAIN_STALL_TIMEOUT`] 快速收口。
                        if run::descriptor_field3(&payload).is_some()
                            || !fr.text.is_empty()
                            || !fr.thinking.is_empty()
                            || fr.usage.is_some()
                        {
                            last_progress = tokio::time::Instant::now();
                        }
                        continue;
                    }
                }

                // ⚠️ **用量必须最后判。** 「一帧只装一样东西」只是对抓包的观察,
                // protobuf 层没有任何东西阻止上游把最后一个正文增量和用量并进同一帧。
                // 先判用量就 break 的话,那一帧里的正文被静默丢掉,而客户还是收到
                // `end_turn` —— 表现为「回答少了结尾」且无任何错误。
                let usage = fr.usage;
                let text = fr.text;
                // 思考帧**一律算进展**:它是上游正在干活的确凿信号。旧写法在客户端没要
                // thinking 时不算(want_thinking 门控):上游明明在持续发 `1.4`,90s
                // stall 闸却把这条健康的长思考流当「只有心跳」掐死(2026-09-04 生产
                // grok 号大量此类误杀)。真出故障(只思考永不出字)有 gw-app 300s 静默
                // 硬上限兜底;且没要 thinking 的请求如今 effort 已降 low(见
                // models::apply_thinking_pref),长考窗口本身就短。
                if run::is_tool_assembly(&payload) {
                    assembling_since.get_or_insert_with(tokio::time::Instant::now);
                }
                if run::is_summary_started(&payload) && summarizing_since.is_none() {
                    summarizing_since = Some(tokio::time::Instant::now());
                    tracing::info!(
                        conversation_id = %conversation_id,
                        elapsed_ms = stream_began.elapsed().as_millis() as u64,
                        "cursor Run:服务端开始总结压缩上下文,等待上限放宽到总结开始后 {}s",
                        SUMMARY_MAX.as_secs()
                    );
                }
                if run::is_summary_completed(&payload) {
                    if let Some(sm) = summarizing_since.take() {
                        tracing::info!(
                            conversation_id = %conversation_id,
                            summary_ms = sm.elapsed().as_millis() as u64,
                            "cursor Run:服务端总结压缩完成"
                        );
                    }
                    last_progress = tokio::time::Instant::now();
                }
                // 需要给下游续命的情形:组装中的心跳(不算进展,上限由组装时刻封顶),
                // 或干活帧(算进展)。gw-app 的静默硬上限只看我们交没交东西。
                let mut keepalive_due = false;
                if !text.is_empty() || usage.is_some() || !fr.thinking.is_empty() {
                    last_progress = tokio::time::Instant::now();
                    work_only_since = None;
                    // 模型接着出内容了:组装(若有)已结束或作废;总结(若有)也已结束。
                    assembling_since = None;
                    summarizing_since = None;
                    if !text.is_empty() {
                        // 模型接着说话 = 没在等那个内建调用的结果,等待作废。
                        builtin_wait = None;
                    }
                } else if (assembling_since.is_some() || summarizing_since.is_some())
                    && run::is_heartbeat(&payload)
                {
                    // 组装中 / 服务端总结中只有心跳:不算进展,但要给下游续命。
                    // ⚠️ 只在已开内容块时真能发出去(`keepalive_delta` 需要块索引)。总结多发生在
                    // 出字前、没有开着的块,这时下游续命靠 gw-app 自己的 20s 静默保活
                    // (`worker::response::STREAM_IDLE_KEEPALIVE`);[`SUMMARY_MAX`] 180s 小于
                    // gw-app 静默硬上限 300s(`STREAM_IDLE_ABORT`),不会被它中止。
                    keepalive_due = true;
                } else if run::is_work_update(&payload) || run::is_token_delta(&payload) {
                    last_progress = tokio::time::Instant::now();
                    work_only_since.get_or_insert(last_progress);
                    keepalive_due = true;
                }
                if keepalive_due && last_ping.elapsed() >= WORK_PING_EVERY {
                    last_ping = tokio::time::Instant::now();
                    if let Some(ev) = keepalive_delta(open_block) {
                        if tx.send(Ok(StreamItem::Sse(ev))).await.is_err() {
                            return;
                        }
                    }
                }

                // ── 思考透传 ────────────────────────────────────────────────
                //
                // **只在调用方请求了 thinking 时发。** 没请求却发 thinking 块,
                // 客户端要么报未知块类型、要么把推理当正文显示 —— 两种都比不发坏。
                // 不发时思考照旧丢掉(绝不能混进正文,见 run::RespFrame)。
                if want_thinking && !fr.thinking.is_empty() {
                    if !started {
                        if tx
                            .send(Ok(StreamItem::Sse(message_start(&msg_id, &model))))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        started = true;
                    }
                    // thinking 块必须排在 text 之前(Anthropic 约定),而 text 一旦开过
                    // 就不能再回头开 thinking —— 那会让块顺序倒置。
                    if open_block.is_none() {
                        let _ = tx
                            .send(Ok(StreamItem::Sse(SseEvent::new(
                                "content_block_start",
                                json!({"type":"content_block_start","index":next_idx,
                                       "content_block":{"type":"thinking","thinking":""}}),
                            ))))
                            .await;
                        open_block = Some((next_idx, "thinking"));
                    }
                    if let Some((idx, "thinking")) = open_block {
                        thinking_echo.push(&fr.thinking);
                        let _ = tx
                            .send(Ok(StreamItem::Sse(SseEvent::new(
                                "content_block_delta",
                                json!({"type":"content_block_delta","index":idx,
                                       "delta":{"type":"thinking_delta","thinking":fr.thinking}}),
                            ))))
                            .await;
                    } else {
                        // 正文块已经开着 → 这段思考只能丢(Anthropic 不允许 thinking 排在
                        // text 之后)。**丢是对的,静默不对**:长回答里模型会二次规划,
                        // 那部分内容无声消失,排查「思考缺了一段」时没有任何痕迹。
                        dropped_thinking += fr.thinking.chars().count();
                        tracing::debug!(
                            chars = fr.thinking.chars().count(),
                            total = dropped_thinking,
                            "cursor Run:正文已开始,丢弃后续思考增量(块顺序不允许回头)"
                        );
                    }
                }

                if !text.is_empty() {
                    output_chars += text.chars().count();
                    assistant_text.push_str(&text);
                    output_nonascii += text.chars().filter(|c| !c.is_ascii()).count();
                    if !started {
                        if tx
                            .send(Ok(StreamItem::Sse(message_start(&msg_id, &model))))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        started = true;
                    }
                    // 思考块开着就先收掉 —— 一个索引上只能有一种块。
                    if let Some((idx, "thinking")) = open_block {
                        let _ = tx
                            .send(Ok(StreamItem::Sse(SseEvent::new(
                                "content_block_stop",
                                json!({"type":"content_block_stop","index":idx}),
                            ))))
                            .await;
                        open_block = None;
                        next_idx += 1;
                    }
                    if open_block.is_none() {
                        let _ = tx
                            .send(Ok(StreamItem::Sse(SseEvent::new(
                                "content_block_start",
                                json!({"type":"content_block_start","index":next_idx,
                                       "content_block":{"type":"text","text":""}}),
                            ))))
                            .await;
                        open_block = Some((next_idx, "text"));
                    }
                    if let Some((idx, _)) = open_block {
                        let _ = tx
                            .send(Ok(StreamItem::Sse(SseEvent::new(
                                "content_block_delta",
                                json!({"type":"content_block_delta","index":idx,
                                       "delta":{"type":"text_delta","text":text}}),
                            ))))
                            .await;
                    }
                }

                // CLI 形态的会话登记通知(2026-08-16 抓包新帧,见 run::is_session_notice):
                // 顶层 field 2 的 request_context,真客户端不回任何帧 —— 忽略即可。
                // 早期实现把它误收成「内建工具调用」,首轮还没出字就被掐断。
                if run::is_session_notice(&payload) {
                    last_progress = tokio::time::Instant::now();
                    continue;
                }

                // CLI 形态的「本轮已提交」回显(见 run::is_turn_commit):轮次到此
                // **已经成功**(saw_end),客户端结果已定。但它**不是流尾** ——
                // 2026-08-23 官方抓包实证帧序为 turn_commit → .3 → .3(ref 更多) →
                // 小尾帧 → 流结束。所以从此进入**纯排水段**(draining):继续读帧
                // 只为影子捕尾部 `.3`,尾帧不再过状态机(见上方排水段注释)。
                // 排水段的任何失败(停滞/读错/解压错)都只放弃影子提交,
                // 绝不影响这个已经成功的轮次。
                if cli_mode && commit_frame {
                    // commit 本身就是有意义的进展(codex 评审 P1):进排水段必须重置
                    // 计时,否则「正文帧在 t=0、commit 在 t=25s 才到」的健康轮次,
                    // 排水闸的 20s 从上一个正文帧算起,等于进段即判停滞。
                    last_progress = tokio::time::Instant::now();
                    if let Some(d) = wire_driver.as_mut() {
                        d.set_draining();
                    }
                    // 合帧防御:同帧若有用量先收下(目前 CLI 实测没有,留这条不亏)。
                    if let Some(wu) = usage {
                        upstream_usage = Some(usage_from_upstream(wu));
                    }
                    // 工具轮的 commit:起宽限计时,宽限臂(commit+3s)来收尾。
                    if await_tool_commit && tool_commit_at.is_none() {
                        tool_commit_at = Some(tokio::time::Instant::now());
                    }
                    saw_end = true;
                    draining = true;
                }

                // 工具调用:上游在等客户端执行工具并回帧。
                //
                // ⚠️ **这段必须排在正文/思考发射之后。** 与「用量必须最后判」是同一条
                // 原则:protobuf 层没有任何东西阻止上游把最后一个正文增量和工具调用
                // 并进同一帧。先判工具就 break 的话,那一帧的正文被静默丢掉,而客户
                // 收到的是一个看起来正常的 `tool_use` —— 回答少了结尾,无错误、无日志。
                // 早先 usage 帧防了这一手、工具帧没防,是同一条原则只执行了一半。
                if run::is_tool_call(&payload) {
                    // 逆向期用:把工具调用帧原样落盘,好离线解字段号。
                    // 设了 CURSOR_DUMP_TOOL_FRAMES=<目录> 才写,生产不留这条路径。
                    if let Ok(dir) = std::env::var("CURSOR_DUMP_TOOL_FRAMES") {
                        let f = format!("{dir}/toolframe_{}_{}.bin", msg_id, payload.len());
                        let _ = std::fs::write(&f, &payload);
                        tracing::warn!(file = %f, "已落盘工具调用帧");
                    }
                    // 服务端内建 exec 调用(ExecServerMessage,字段号见 run.rs 常量区):
                    // **写盘** = 带图流程的资产落盘(服务端把附件字节推给客户端,
                    // 模型随后用**读文件**把同一路径的图看进去)。真 IDE 有真磁盘;
                    // 我们没有 —— 写:存进内存资产库并回执成功;读:从内存库回图。
                    // 只接「写 /assets/ 前缀」和「读我们存过的路径」两种 ——
                    // 代执行模型选定的任意文件/命令是红线,其余内建调用维持收口。
                    let mut exec_handled = false;
                    if let Some(w) = run::parse_exec_write(&payload) {
                        if w.path.starts_with("/assets/") {
                            last_progress = tokio::time::Instant::now();
                            match &body_keepalive {
                                Some(btx) => {
                                    if !assets.put(&conversation_id, &w.path, &w.bytes) {
                                        tracing::warn!(
                                            path = %w.path, bytes = w.bytes.len(),
                                            "cursor 资产超内存上限,未存(后续 read 会落空)"
                                        );
                                    }
                                    let ack = wire::frame(&run::encode_write_success(
                                        w.id,
                                        &w.exec_id,
                                        &w.path,
                                        w.bytes.len(),
                                    ));
                                    // 回执是协议关键帧:try_send 丢了会退化成 90s 死等,
                                    // 所以**等容量**而不是丢;只有真发出去才算已处理。
                                    // 通道对端是 hyper 在持续消费请求体,不会死锁。
                                    if btx.send(Ok(bytes::Bytes::from(ack))).await.is_err() {
                                        tracing::warn!(
                                            path = %w.path,
                                            "cursor 资产写回执发送失败(请求流已关),按未处理走收口"
                                        );
                                    } else {
                                        exec_handled = true;
                                        tracing::debug!(
                                            path = %w.path, bytes = w.bytes.len(),
                                            "cursor 资产写调用:已存内存并回执"
                                        );
                                    }
                                }
                                None => tracing::warn!(
                                    path = %w.path,
                                    "cursor 资产写调用无法回执(请求流未保持打开),按未处理走收口"
                                ),
                            }
                        }
                    } else if let Some(r) = run::parse_exec_read(&payload) {
                        if let Some(bytes) = assets.get(&conversation_id, &r.path) {
                            last_progress = tokio::time::Instant::now();
                            match &body_keepalive {
                                Some(btx) => {
                                    let reply = wire::frame(&run::encode_read_success_data(
                                        r.id, &r.exec_id, &r.path, &bytes,
                                    ));
                                    if btx.send(Ok(bytes::Bytes::from(reply))).await.is_err() {
                                        tracing::warn!(
                                            path = %r.path,
                                            "cursor 资产读回执发送失败(请求流已关),按未处理走收口"
                                        );
                                    } else {
                                        exec_handled = true;
                                        tracing::debug!(
                                            path = %r.path, bytes = bytes.len(),
                                            "cursor 资产读调用:已从内存回图"
                                        );
                                    }
                                }
                                None => tracing::warn!(
                                    path = %r.path,
                                    "cursor 资产读调用无法回执(请求流未保持打开),按未处理走收口"
                                ),
                            }
                        }
                    }
                    // 界面帧 `1.2.2.15` 是主来源;exec 形态的 mcp_args 平时是它的「回声」
                    // (下面 None 臂按 await_tool_commit 忽略)。只有本轮**还没交付过任何
                    // 调用**时才拿 exec 这份兜底 —— OmniRoute 只认 exec 这份,服务端完全
                    // 可能不发界面帧;已交付过就仍按回声处理,免得同一调用交两次。
                    let call = run::parse_tool_call(&payload).or_else(|| {
                        if await_tool_commit || !tool_calls.is_empty() {
                            return None;
                        }
                        let tc = crate::exec::mcp_call(&payload)?;
                        tracing::info!(
                            tool = %tc.name, id = %client_tool_id(&tc.id),
                            "cursor Run:界面帧缺席,按 exec 形态的 mcp_args 交付工具调用"
                        );
                        Some(tc)
                    });
                    match call {
                        // 我们声明过的工具 → 映射成 Anthropic `tool_use`,交给调用方执行。
                        Some(tc) => {
                            // 全名回调归一:双注册的全名变体(mcp_claude-local_<name>)
                            // 折回裸名,后续别名翻译/下发都按裸名口径。
                            let tc = match tc.name.strip_prefix(MCP_FULL_PREFIX) {
                                Some(rest) if !rest.is_empty() => {
                                    tracing::info!(
                                        bare = %rest,
                                        "cursor Run:全名工具回调已归一为裸名"
                                    );
                                    run::ToolCall {
                                        name: rest.to_string(),
                                        ..tc
                                    }
                                }
                                _ => tc,
                            };
                            // write 别名回调:折回调用方的编辑工具(见 [`WriteAlias`])。
                            // 客户端只见它声明过的工具名,别名只存在于上游广告里。
                            let tc = match &xlate.write_alias {
                                Some(a) if tc.name == WRITE_ALIAS_NAME => {
                                    tracing::info!(
                                        target = %a.target,
                                        "cursor Run:write 别名回调已翻译成编辑工具调用"
                                    );
                                    a.translate(tc)
                                }
                                _ => tc,
                            };
                            // 续跑段:刚在原流上回过结果的那几个调用,上游可能再回显一次
                            // (完成态界面帧),不是新调用 —— 再交给客户端会重复执行。
                            if answered_ids.iter().any(|a| *a == client_tool_id(&tc.id)) {
                                tracing::debug!(
                                    id = %client_tool_id(&tc.id),
                                    "cursor Run:续跑段收到已回结果的调用回显,忽略"
                                );
                                last_progress = tokio::time::Instant::now();
                                continue;
                            }
                            tracing::info!(
                                // 用清洗后的 id:原值带换行,会把这行日志劈成两半。
                                tool = %tc.name, id = %client_tool_id(&tc.id), args = tc.args.len(),
                                "cursor Run:模型调用工具,转成 tool_use 交给调用方"
                            );
                            // 界面帧缺席、本帧就是 exec 形态的 mcp_args:服务端已在等结果,
                            // 它的 exec 编号就在这一帧里。
                            let exec_here = crate::exec::parse(&payload)
                                .filter(|f| f.variant == crate::exec::VAR_MCP)
                                .map(|f| (f.id, f.exec_id.clone(), crate::exec::mcp_ids(&f)));
                            tool_calls.push(tc);
                            // 自带工具 + MCP 混合并行:两份调用都交给客户端,本轮不挂起(旧行为)。
                            // 两种 exec 帧的到达顺序没实测过,混在一起配对容易漏配,不值得冒险。
                            if builtin_hold_pending {
                                if let Some(wu) = usage {
                                    upstream_usage = Some(usage_from_upstream(wu));
                                }
                                tracing::info!(
                                    conversation_id = %conversation_id,
                                    calls = tool_calls.len(),
                                    "cursor Run:自带工具与 MCP 工具并行调用,本轮不挂起"
                                );
                                saw_end = true;
                                break 'outer;
                            }
                            if hold_on {
                                if let Some((id, exec_id, Some((cid, name)))) = exec_here {
                                    mcp_execs.push((cid, name, id, exec_id, crate::held::PendingKind::Mcp));
                                    if pair_pending_execs(&tool_calls, &mcp_execs).is_some() {
                                        hold_ready = true;
                                    }
                                }
                            }
                            // 同帧若已带 `1.14` 用量,绝不能丢 —— 合帧丢 usage 会让
                            // 面板/new-api 看到 input=0(见 PROTOCOL §残余假设 #2)。
                            if let Some(wu) = usage {
                                upstream_usage = Some(usage_from_upstream(wu));
                            }
                            // 2026-09-04 探针(probe_wire_v2 toolcont)实证:**工具轮上游
                            // 照样给 turn_commit 与尾部描述符**,就在回调帧之后 —— 旧写法
                            // 在这里 break 全扔了,还顺手作废旧链,于是每个工具结果轮都被迫
                            // IDE 形态全量重铺(10~25s、cache=0,工具回路卡慢的根源)。
                            // 改为:客户端侧照常 tool_use 收尾(saw_end),但**流继续读**,
                            // 等 commit+描述符(await_tool_commit)拿到才算真正收工;
                            // 等不到(停滞闸)才退回旧行为作废旧链。
                            // 仅 CLI 形态:IDE 没有描述符这套,维持即时收口(白等一个
                            // 停滞闸周期是纯退化)。
                            saw_end = true;
                            if hold_ready {
                                tracing::info!(
                                    conversation_id = %conversation_id,
                                    calls = tool_calls.len(),
                                    "cursor Run:工具调用随 exec 帧到达,本轮收工并挂起上游流"
                                );
                                break 'outer;
                            }
                            if cli_mode {
                                await_tool_commit = true;
                            } else {
                                break 'outer;
                            }
                        }
                        // 已处理的 exec 调用(资产写/读):**不能 break** ——
                        // protobuf 层没有任何东西阻止上游把 exec 帧和 usage 并进
                        // 同一帧,在这里收口会把同帧的用量吞掉,陪上游死等心跳到
                        // watchdog。落出本分支,让下面的 usage/文本判断照常跑。
                        None if exec_handled => {
                            // 等的就是这一帧(典型:读 /assets/ 附件),已应答,别让等待计时误收口。
                            builtin_wait = None;
                        }
                        // Cursor 服务端**内建**的工具(终端 / 读文件 …)。它们是闭集枚举、
                        // 不带名字 —— 但其中两种(.1 终端 / .4 读文件)的参数形状有抓包
                        // 实证,可走**兼容转换层**(2026-08-13):翻译成调用方声明工具的
                        // 标准 tool_use 下发,客户端照常执行,「半截回答 + 静默截断」变成
                        // 无感成功。翻译不了(身份/参数认不出、调用方没声明同义工具)才
                        // 落回收口 + 纠偏 —— 失败单向,绝不输出参数张冠李戴的调用。
                        // preview 留在日志里,是为了万一要扩内建工具映射时有据可查。
                        None => {
                            // 「MCP 工具调用开始」的界面帧(`1.2.2.44`,不带参数,见
                            // `exec::is_dynamic_mcp_started`):参数随后在 exec mcp_args 里到,
                            // 由上面的兜底交付。这里**继续读**,绝不能当内建工具收口。
                            if crate::exec::is_dynamic_mcp_started(&payload) {
                                last_progress = tokio::time::Instant::now();
                                tracing::debug!(
                                    bytes = payload.len(),
                                    "cursor Run:MCP 工具调用开始(1.2.2.44),等 exec mcp_args 带参数"
                                );
                                continue;
                            }
                            // MCP 回调的 **exec 形态回声**(同一 call id、claude-local
                            // 路由):官方客户端用它渲染 exec UI,不是第二次调用。
                            // 1.2.2.15 那一份已交给调用方(await_tool_commit 在等
                            // commit),这份必须忽略 —— 收口会丢掉工具轮的
                            // commit+描述符,还会误注纠偏把模型绕进重调循环
                            // (2026-09-04 e2e 实证:webfetch 三连调后停滞)。
                            if await_tool_commit {
                                // exec 形态的 mcp_args = 服务端已转入「等客户端回 McpResult」。
                                // 2026-11-06 线协议实测:此后**不会**再有 turn_commit,继续等
                                // 只会干耗到停滞闸(实测多等 ~90s)—— 立即收工。
                                if let Some(f) = crate::exec::parse(&payload)
                                    .filter(|f| f.variant == crate::exec::VAR_MCP)
                                {
                                    if hold_on {
                                        if let Some((cid, name)) = crate::exec::mcp_ids(&f) {
                                            mcp_execs.push((cid, name, f.id, f.exec_id.clone(), crate::held::PendingKind::Mcp));
                                        }
                                        if pair_pending_execs(&tool_calls, &mcp_execs).is_some() {
                                            hold_ready = true;
                                            tracing::info!(
                                                conversation_id = %conversation_id,
                                                calls = tool_calls.len(),
                                                "cursor Run:服务端已在等工具结果,exec 编号已收齐,本轮收工并挂起上游流"
                                            );
                                            // 本帧会让排水段退出并清掉 saw_end,这里补回(正常 tool_use 收尾)。
                                            saw_end = true;
                                            break 'outer;
                                        }
                                        // 并行调用:其余 exec 帧随后到,短等收齐。
                                        exec_collect_deadline.get_or_insert_with(|| {
                                            tokio::time::Instant::now() + EXEC_COLLECT_GRACE
                                        });
                                        last_progress = tokio::time::Instant::now();
                                        continue;
                                    }
                                    tracing::info!(
                                        conversation_id = %conversation_id,
                                        "cursor Run:服务端已在等工具结果(exec mcp_args),本轮收工"
                                    );
                                    break 'outer;
                                }
                                tracing::debug!(
                                    bytes = payload.len(),
                                    "cursor Run:忽略工具调用的 exec 回声帧(1.2.2.15 那份已交付)"
                                );
                                continue;
                            }
                            if let Some(bc) = run::parse_builtin_call(&payload) {
                                // 读文件 / 终端可挂流续跑(2026-11-10):上游随后同毫秒发 exec(read_args /
                                // shell_stream_args)等结果。先交付、不收工,等 exec 帧配上调用 id 再挂起
                                // (见下方 exec 分支与 `crate::held::PendingKind`)。生产 54% 的重建紧跟在
                                // 自带工具调用之后 —— 流不挂,下一轮只能按完整历史重建,丢服务端总结。
                                let holdable = hold_on
                                    && matches!(
                                        bc,
                                        run::BuiltinCall::ReadFile { .. } | run::BuiltinCall::Terminal { .. }
                                    );
                                if let Some(tc) = translate_builtin(bc, &xlate) {
                                    // 续跑段:刚在原流上回过结果的自带工具调用,上游可能再发一次完成态
                                    // 界面帧;等 exec 期间同一调用也可能再来一帧。都不是新调用 ——
                                    // 再交给客户端会重复执行。
                                    let cid = client_tool_id(&tc.id);
                                    if answered_ids.iter().any(|a| *a == cid)
                                        || tool_calls.iter().any(|t| client_tool_id(&t.id) == cid)
                                    {
                                        tracing::debug!(
                                            id = %cid,
                                            "cursor Run:自带工具调用的回显帧(已交付或已回结果),忽略"
                                        );
                                        last_progress = tokio::time::Instant::now();
                                        continue;
                                    }
                                    tracing::info!(
                                        tool = %tc.name,
                                        id = %client_tool_id(&tc.id),
                                        guard_rev = %crate::tool_guard_rev(),
                                        holdable,
                                        "cursor Run:内建工具调用已翻译成调用方工具的 tool_use(兼容转换层)"
                                    );
                                    tool_calls.push(tc);
                                    // 同帧用量同样要收(与外部工具臂同一条合帧原则)。
                                    if let Some(wu) = usage {
                                        upstream_usage = Some(usage_from_upstream(wu));
                                    }
                                    // 与外部工具臂同款:正常的 tool_use 收尾,不是截断。
                                    saw_end = true;
                                    if holdable {
                                        // 等 exec 帧;宽限内没来就按旧行为收工(不挂),失败单向。
                                        builtin_hold_pending = true;
                                        exec_collect_deadline.get_or_insert_with(|| {
                                            tokio::time::Instant::now() + EXEC_COLLECT_GRACE
                                        });
                                        last_progress = tokio::time::Instant::now();
                                        continue;
                                    }
                                    break 'outer;
                                }
                            }
                            // ── 等自带工具 exec 期间的工具帧 ──────────────────────────────────
                            // 只收「配得上已交付调用」的 exec(读文件 / 流式终端,tool_call_id 对得上),
                            // 配齐就挂起。其余任何工具帧(别的自带工具的 exec、缺调用 id、后台终端、
                            // 翻译不了的界面帧 …)一律按旧行为收工、不挂起:既不能落到下面的 exec
                            // 翻译(会再交一个 tool_use,客户端执行两次),也不能落到收口(本轮已有
                            // 工具调用,收口会误判成空回复/截断)。旧行为在第一个自带工具界面帧就收工,
                            // 后面的帧本来就读不到,所以这里收工不丢任何东西。
                            //
                            // 并行的多个自带工具:第一个调用配齐时第二个的界面帧可能还没到,这时
                            // 照样挂起 —— 第二个调用会在续跑段交付,多一次往返,结果不错。
                            if builtin_hold_pending {
                                let paired = crate::exec::parse(&payload).and_then(|ef| {
                                    let (kind, call_id) = crate::exec::holdable_exec(&ef)?;
                                    let want = client_tool_id(&call_id);
                                    let name = tool_calls
                                        .iter()
                                        .find(|t| client_tool_id(&t.id) == want)?
                                        .name
                                        .clone();
                                    // 同一调用的 exec 只收一份;第二份按配不上处理。
                                    if mcp_execs.iter().any(|(cid, ..)| *cid == want) {
                                        return None;
                                    }
                                    Some((want, name, ef.id, ef.exec_id.clone(), kind))
                                });
                                let Some(entry) = paired else {
                                    tracing::info!(
                                        conversation_id = %conversation_id,
                                        bytes = payload.len(),
                                        calls = tool_calls.len(),
                                        "cursor Run:等自带工具 exec 期间来了配不上的工具帧,本轮不挂起"
                                    );
                                    if let Some(wu) = usage {
                                        upstream_usage = Some(usage_from_upstream(wu));
                                    }
                                    saw_end = true;
                                    break 'outer;
                                };
                                mcp_execs.push(entry);
                                last_progress = tokio::time::Instant::now();
                                if pair_pending_execs(&tool_calls, &mcp_execs).is_some() {
                                    hold_ready = true;
                                    tracing::info!(
                                        conversation_id = %conversation_id,
                                        calls = tool_calls.len(),
                                        "cursor Run:自带工具的 exec 编号已收齐,本轮收工并挂起上游流"
                                    );
                                    // 本帧会让排水段退出并清掉 saw_end,这里补回(正常 tool_use 收尾)。
                                    saw_end = true;
                                    break 'outer;
                                }
                                continue;
                            }
                            // ── exec 形态的内建工具(带参数,见 `crate::exec::Builtin`)──────
                            //
                            // 界面帧翻译不了的种类(列目录、grep …)在下面会先 `continue`,
                            // 参数随后在这一帧到。能映射就交付成调用方工具;映射不了就回
                            // 带类型的拒绝、**继续读** —— 模型会换办法,而不是给客户一个空回答
                            // (对照 OmniRoute `buildExecRejection`)。
                            if let Some(ef) = crate::exec::parse(&payload) {
                                if let Some(b) = ef.builtin() {
                                    // 内建 StrReplace 在读原文件:拒绝并点名调用方的编辑工具,继续读。
                                    let edit_read = matches!(
                                        (&b, pending_edit.as_deref()),
                                        (crate::exec::Builtin::Read { path }, Some(p)) if path == p
                                    );
                                    if edit_read && body_keepalive.is_some() {
                                        let reason = edit_reject_reason(&xlate, cli_mode);
                                        let mut sent = true;
                                        for p in b.rejection(&ef, &reason) {
                                            if wire_send_frame(&body_keepalive, wire::frame(&p)).await.is_err() {
                                                sent = false;
                                            }
                                        }
                                        tracing::info!(
                                            builtin = ?b, sent, edit_tool = ?xlate.edit_tool,
                                            "cursor Run:内建局部替换要读原文件,已回拒绝并指向调用方编辑工具"
                                        );
                                        pending_edit = None;
                                        if sent {
                                            last_progress = tokio::time::Instant::now();
                                            builtin_wait = None;
                                            continue;
                                        }
                                    }
                                    if matches!(b, crate::exec::Builtin::Write { .. }) {
                                        pending_edit = None;
                                    }
                                    let synth_id = format!("toolu_cx{}", uuid::Uuid::new_v4().simple());
                                    if let Some(tc) = b
                                        .to_builtin_call(&synth_id)
                                        .and_then(|bc| translate_builtin(bc, &xlate))
                                    {
                                        tracing::info!(
                                            tool = %tc.name, builtin = ?b,
                                            "cursor Run:exec 形态内建工具已翻译成调用方工具的 tool_use"
                                        );
                                        tool_calls.push(tc);
                                        if let Some(wu) = usage {
                                            upstream_usage = Some(usage_from_upstream(wu));
                                        }
                                        saw_end = true;
                                        break 'outer;
                                    }
                                    if body_keepalive.is_some() {
                                        let mut sent = true;
                                        for p in b.rejection(&ef, crate::exec::BUILTIN_REJECT_REASON) {
                                            if wire_send_frame(&body_keepalive, wire::frame(&p)).await.is_err() {
                                                sent = false;
                                            }
                                        }
                                        tracing::info!(
                                            builtin = ?b, sent,
                                            "cursor Run:内建工具映射不了,已回拒绝让模型换办法继续"
                                        );
                                        if sent {
                                            last_progress = tokio::time::Instant::now();
                                            builtin_wait = None;
                                            continue;
                                        }
                                    }
                                }
                            }
                            // 界面帧(`1.2.2.<N>`)翻译不了:参数随后会在 exec 帧到,由上面那段
                            // 处理(映射或回拒绝让模型换办法)。这里先别收口。
                            //
                            // 2026-11-07 起 IDE 形态(带图/PDF、或历史里有工具块)也走这条:
                            // 生产 5 次「内建工具截断」全是 IDE 形态的 glob/grep,当时这里只放行
                            // CLI 形态,于是直接收口截断。IDE 形态同样收得到 exec 帧(资产写盘就是
                            // exec 帧),但万一没来,[`BUILTIN_EXEC_WAIT`] 到期退回原收口,不拖成停滞。
                            if body_keepalive.is_some()
                                && crate::exec::parse(&payload).is_none()
                                && run::builtin_tool_ident(&payload).is_some()
                            {
                                if let Some(p) = run::edit_call_path(&payload) {
                                    pending_edit = Some(p);
                                }
                                last_progress = tokio::time::Instant::now();
                                tracing::debug!(
                                    ident = ?run::builtin_tool_ident(&payload),
                                    cli_mode,
                                    "cursor Run:内建工具界面帧翻译不了,等 exec 帧再处理"
                                );
                                builtin_wait.get_or_insert((tokio::time::Instant::now(), payload.clone()));
                                continue;
                            }
                            let close = close_untranslatable_builtin(
                                &payload, started, output_chars, &notices, &conversation_id,
                            );
                            trailer_err = trailer_err.or(close.error);
                            builtin_truncated |= close.truncated;
                            // 同帧用量同样要收(与外部工具臂同一条合帧原则)。
                            if let Some(wu) = usage {
                                upstream_usage = Some(usage_from_upstream(wu));
                            }
                            // 内建工具支下面会照常走收尾流程(而不是报错),所以算「见过收尾」。
                            saw_end = true;
                            break 'outer;
                        }
                    }
                }

                // 用量帧 = 本轮结束。BiDi 流永远不会自己关,不在这里收口的话
                // 每个请求都要挂到客户端超时 —— 「答完了却一直转圈」。
                if let Some(wu) = usage {
                    upstream_usage = Some(usage_from_upstream(wu));
                    saw_end = true;
                    shadow_committable = true;
                    break 'outer;
                }
            }
        }

        if let Some(err) = trailer_err {
            wire_invalidate("错误 trailer");
            fail(&outcome);
            let _ = tx.send(Err(err)).await;
            return;
        }

        // 上游 200 却一个字都没有。旧版会照常发完整收尾,gw-app 看到的是一次
        // 「成功的空回复」—— 客户拿到空白,而账号健康度毫发无损。明确报 EmptyResponse,
        // 让 empty-fallback 策略接管。
        // 有工具调用时,「一个字都没出」是**正常**的:模型可以直接决定调工具而不先说话。
        // 那时也必须补一个 message_start,否则后面的块没有依附。
        if !started && !tool_calls.is_empty() {
            let _ = tx
                .send(Ok(StreamItem::Sse(message_start(&msg_id, &model))))
                .await;
        } else if !started {
            wire_invalidate("空回复(一个字都没出)");
            fail(&outcome);
            let _ = tx
                .send(Err(UpstreamError::new(
                    UpstreamErrorKind::EmptyResponse,
                    format!(
                        // 内部消息(只进 tracing「流中上游错误」那行;对外走 client_message 固定文案)。
                        "Cursor Run 返回 200 但没有任何文本帧 | 耗时 {}ms 帧序: {}",
                        stream_began.elapsed().as_millis(),
                        format_trail_since(&frame_trail, stream_began)
                    ),
                )))
                .await;
            return;
        }

        // 出过字、但流是**断掉**的而不是收尾的 —— 报截断,不许伪装成 end_turn。
        // 首包已经发出去了,gw-app 换不了号,但至少让这次请求在日志里是红的。
        if !saw_end {
            wire_invalidate("流断而非收尾");
            fail(&outcome);
            let _ = tx
                .send(Err(UpstreamError::new(
                    UpstreamErrorKind::ServerError,
                    format!("Cursor Run 流在收到用量帧之前中断(已输出 {output_chars} 字)"),
                )))
                .await;
            return;
        }

        // 影子提交:**只有干净收尾才走到这里**(trailer_err / !saw_end 已在上面
        // return;工具调用与内建截断分支不置 shadow_committable)。「同号/换号」
        // 「同模型/换模型」是评估描述符回放可行性的核心统计 —— 账号轮转决定
        // 描述符的有效寿命。
        // ── wire v2 判决(见 `run::WireTurnOutcome`)────────────────────────────
        //
        // 这里 `saw_end` 必为真(中断/错误 trailer 已在上面 return)。能在这里翻船的
        // 是「收尾了、甚至出了字,但服务端点名了我方拿不出来的内容分节」——
        // 那种轮次的回答**可能是基于缺历史生成的**,记成功等于把错答案留给客户,
        // 并且把一份指向不可用内容的描述符留给下一轮。
        // ⚠️ **判决只在 CLI 形态执行**(codex 终审 #6)。IDE 形态没有描述符,
        // 无条件跑判决会让每个成功轮都算出 `NoDescriptor` 并打一条 WARN ——
        // 那是给生产默认路径凭空加噪声,而且会误导排查。
        let wire_verdict = if !cli_mode {
            run::WireVerdict::Ok
        } else {
            run::WireTurnOutcome {
                saw_descriptor: wire_saw_desc,
                saw_finish: saw_end,
                content_chars: output_chars,
                demanded: wire_driver.as_ref().map_or(0, |d| d.demanded()),
                unavailable: wire_driver.as_ref().map_or(0, |d| d.unavailable()),
            }
            .verdict()
        };
        if wire_verdict != run::WireVerdict::Ok {
            // 工具调用轮的 NoDescriptor 是**设计内**的链中断(服务端停在
            // 「等工具结果」的中间态,本轮没有 turn_commit),不是异常 ——
            // 生产每次工具调用都 WARN 会把真异常淹没。其余情形照旧 WARN。
            // (tracing 的 level 必须是常量,两支写开。)
            if !tool_calls.is_empty() {
                tracing::info!(
                    conversation_id = %conversation_id,
                    verdict = ?wire_verdict,
                    saw_descriptor = wire_saw_desc,
                    output_chars,
                    demanded = wire_driver.as_ref().map_or(0, |d| d.demanded()),
                    unavailable = wire_driver.as_ref().map_or(0, |d| d.unavailable()),
                    "cursor Run:wire 判决非 Ok(工具调用轮,设计内)"
                );
            } else {
                tracing::warn!(
                    conversation_id = %conversation_id,
                    verdict = ?wire_verdict,
                    saw_descriptor = wire_saw_desc,
                    output_chars,
                    demanded = wire_driver.as_ref().map_or(0, |d| d.demanded()),
                    unavailable = wire_driver.as_ref().map_or(0, |d| d.unavailable()),
                    "cursor Run:wire 判决非 Ok"
                );
            }
        }

        // 描述符作废:三种情形合流 —— ① 排水失败(旧条目缺本轮,codex #2);
        // ② 判决要求回退;③ 本轮拿不出内容分节。作废后下一轮走首轮形态重铺。
        let must_invalidate = shadow_must_invalidate || wire_verdict.needs_reflow();
        if must_invalidate {
            if let Some(feed) = &shadow {
                let dropped = feed.map.invalidate(&conversation_id, &feed.account_id);
                // 工具调用轮同上的理由:设计内中断,降为 info。
                if !tool_calls.is_empty() {
                    tracing::info!(
                        conversation_id = %conversation_id,
                        account = %feed.account_id,
                        dropped,
                        drain_failed = shadow_must_invalidate,
                        verdict = ?wire_verdict,
                        "cursor Run:作废该会话描述符,下轮重铺(工具调用轮,设计内)"
                    );
                } else {
                    tracing::warn!(
                        conversation_id = %conversation_id,
                        account = %feed.account_id,
                        dropped,
                        drain_failed = shadow_must_invalidate,
                        verdict = ?wire_verdict,
                        "cursor Run:作废该会话描述符,下轮重铺"
                    );
                }
            }
            shadow_staged = None;
        }

        // ⭐ **判决要真正作用于本轮**(codex 审查 #3)。
        //
        // 早先这里只作废下一轮的料就继续走 message_stop + h(true) —— 那让「基于缺
        // 历史生成的答案」被记成成功轮:ConvRegistry 会 confirm、客户拿到错答案、
        // 而我们以为一切正常。回退必须体现为**本轮失败**,让上层按既有语义重试
        // (重试时描述符已作废 → 走 Opening 全量重铺)。
        if wire_verdict.should_fallback() {
            fail(&outcome);
            let _ = tx
                .send(Err(UpstreamError::new(
                    UpstreamErrorKind::ServerError,
                    match wire_verdict {
                        run::WireVerdict::ContentUnavailable { missing, demanded } => format!(
                            "Cursor Run:上游点名 {demanded} 个内容分节,我方拿不出 {missing} 个\
                             (描述符清单指向未持有的内容,已作废,下轮重铺)"
                        ),
                        _ => {
                            format!("Cursor Run:上游接受请求但未产出内容(已输出 {output_chars} 字)")
                        }
                    },
                )))
                .await;
            return;
        }

        if shadow_committable {
            if let (Some(feed), Some((desc, refs))) = (&shadow, shadow_staged.take()) {
                // 本轮 assistant 输出指纹。**工具轮不算**:它的渲染含参数 JSON 的
                // 调用方再序列化,字节不由我方掌控(键序/空白都可能变),算出来的
                // 指纹下一轮对不上,反而制造假分叉 —— 与 clidrv 同一条红线。
                // 工具轮条目以 tool_turn=true 入库,lookup 走「链减一条」的宽松
                // 匹配(见 DescriptorShadow::lookup),不再因缺指纹被硬拒。
                let assistant_fp = if !tool_calls.is_empty() {
                    None
                } else {
                    // 空正文(纯 thinking 等)沿用 `to_turns` 的占位串口径。
                    let text = if assistant_text.is_empty() {
                        "(unsupported content omitted)"
                    } else {
                        assistant_text.as_str()
                    };
                    Some(turn_fp(false, text))
                };
                let c = feed.map.commit(
                    &conversation_id,
                    &feed.account_id,
                    &model,
                    &feed.fps,
                    assistant_fp,
                    !tool_calls.is_empty(),
                    &desc,
                    refs,
                );
                tracing::info!(
                    conversation_id = %conversation_id,
                    account = %feed.account_id,
                    model = %model,
                    refs,
                    bytes = desc.len(),
                    fp = %format!("{:016x}", c.fp),
                    changed = ?c.changed,
                    account_shift = ?c.same_account.map(|b| if b { "同号" } else { "换号" }),
                    model_shift = ?c.same_model.map(|b| if b { "同模型" } else { "换模型" }),
                    "cursor Run:流干净收尾,续轮描述符已入影子表"
                );
            }
        }

        // 上游自报优先,但「input=0 且 cache=0」的用量帧**不可信** —— 请求体显然
        // 非空,全零是上游字段漂移/置零,不是真实零输入,落回请求体粗估。
        // 没有用量帧时同样粗估(绝不能只估 output —— tool_use 轮会让 new-api
        // 显示「输入 0 / 缓存 0」)。
        // 续跑段:上游 `1.14` 报的可能是整个 Run 的累计数(含挂起前那段),直接用会重复计费。
        // 按冷启动同口径报(请求体估输入 + 模拟缓存),客户账单与不挂起时一致。
        let upstream_usage = if resumed { None } else { upstream_usage };
        // 第二个值 = 用量是不是估算的(没用上游用量帧):决定没命中的部分是否按缓存写上报
        // (见 delta_usage_json)。按**来源**判,不按数字是否为零猜。
        let (mut usage, usage_estimated) = match upstream_usage {
            Some(u) if u.input_tokens > 0 || u.cache_read_tokens > 0 => (u, false),
            zeroed => {
                if zeroed.is_some() {
                    tracing::warn!(
                        tool = tool_calls.first().map(|t| t.name.as_str()),
                        "cursor Run:上游用量帧 input/cache 全零,按请求体粗估用量"
                    );
                }
                let mut u = usage_fallback;
                // output = 正文出字(ASCII/4 + 非 ASCII/1.5)+ 工具参数(纯 tool_use
                // 轮的产出大头是参数 JSON)。上游若给了 output(哪怕 input/cache
                // 置零),它比估算准,用它的。
                let text_tokens =
                    ((output_chars - output_nonascii) as u64).div_ceil(4) + output_nonascii as u64;
                // 并行调用每发的参数都是产出,逐个合计。
                u.output_tokens = text_tokens
                    + tool_calls.iter().map(tool_call_tokens).sum::<u64>();
                if let Some(up) = &zeroed {
                    if up.output_tokens > 0 {
                        u.output_tokens = up.output_tokens;
                    }
                }
                if u.input_tokens == 0 {
                    tracing::warn!(
                        output_chars,
                        tool = tool_calls.first().map(|t| t.name.as_str()),
                        "cursor Run:无上游用量且请求体估出 input=0 —— 计费会失真"
                    );
                } else {
                    tracing::debug!(
                        input = u.input_tokens,
                        output = u.output_tokens,
                        cache = u.cache_read_tokens,
                        tool = tool_calls.first().map(|t| t.name.as_str()),
                        "cursor Run:无上游 1.14,使用请求体粗估用量"
                    );
                }
                (u, true)
            }
        };
        // 缓存按**字段**单独决策(不能整对象二选一):上游 `1.14` 的 cached 报了
        // 真实命中(>0)就用上游;没报(=0)用模拟值顶替,封顶 input_tokens。
        // 上游的 0 是「没命中」还是「没统计」无法分辨,而生产实测它几乎恒 0
        // (见 cache_sim 模块注释),不顶替客户侧就永远全价。模拟不是事实断言:
        // real_cache_read_tokens 在此**不动**,仍只认上游自报(对账列)。
        if usage.cache_read_tokens == 0 && sim_cache_read > 0 {
            usage.cache_read_tokens = sim_cache_read.min(usage.input_tokens);
        }
        let output_tokens = usage.output_tokens;
        // 关掉还开着的那个块(可能是 thinking,可能是 text,也可能一个都没开)。
        if let Some((idx, _)) = open_block {
            let _ = tx
                .send(Ok(StreamItem::Sse(SseEvent::new(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":idx}),
                ))))
                .await;
            // 这里只需要推进索引:块已经收掉,`open_block` 之后不再被读。
            next_idx += 1;
        }

        // 工具调用块(可多个:并行调用逐发下发)。索引接在文本块之后 ——
        // Anthropic 的块索引必须连续且不重复。
        //
        // `input` 一次性给全,所以 `input_json_delta` 只发一帧。分片发没有意义:
        // 我们是从 protobuf 一次解出来的,本来就没有增量。
        let stop_reason = if !tool_calls.is_empty() {
            for tc in &tool_calls {
                let idx = next_idx;
                // 参数值已经是解好的 JSON(数字仍是数字、对象仍是对象)。
                // 早先这里是 `Value::String(v.clone())` —— 把一切都变成字符串,
                // `{"limit": 200}` 会发成 `{"limit": "200"}`,schema 写 number 的工具直接拒。
                let input: serde_json::Map<String, Value> = tc
                    .args
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let _ = tx
                    .send(Ok(StreamItem::Sse(SseEvent::new(
                        "content_block_start",
                        json!({"type":"content_block_start","index":idx,
                               "content_block":{"type":"tool_use","id":client_tool_id(&tc.id),
                                                "name":tc.name,"input":{}}}),
                    ))))
                    .await;
                let _ = tx
                    .send(Ok(StreamItem::Sse(SseEvent::new(
                        "content_block_delta",
                        json!({"type":"content_block_delta","index":idx,
                               "delta":{"type":"input_json_delta",
                                        "partial_json": Value::Object(input).to_string()}}),
                    ))))
                    .await;
                let _ = tx
                    .send(Ok(StreamItem::Sse(SseEvent::new(
                        "content_block_stop",
                        json!({"type":"content_block_stop","index":idx}),
                    ))))
                    .await;
                next_idx += 1;
            }
            "tool_use"
        } else {
            "end_turn"
        };

        // ⚠️ usage 要**在 SSE 里也说全**。
        //
        // `message_start` 只能写 0:那时上游还没告诉我们输入量(`1.14` 是流末的帧)。
        // 但收尾这一帧是知道的,而早先这里只发 `output_tokens` —— 于是客户在 SSE 里
        // 看到输入恒为 0,却按非零输入被计费(计费走的是不转发客户端的
        // `StreamItem::Usage`)。**客户拿响应对不上账单**,而且 kiro/dario 的口径
        // 也没法横向比。现在两条路给的是同一组数。
        // 估算用量时只对 Claude 系把没命中的部分报成缓存写(grok/gpt 等没有缓存写加价)。
        let delta_usage = delta_usage_json(
            &usage,
            output_tokens,
            usage_estimated && model_has_cache_write_surcharge(&model),
        );
        // ── 挂起上游流(见 crate::held)────────────────────────────────────
        // 必须在 message_stop **之前**登记:客户端收到 stop 后可能立刻带结果回来,
        // 晚登记会让那次请求取不到、走冷启动,而这条流白挂到超时。
        let mut held_token: Option<(String, u64)> = None;
        if hold_ready && stop_reason == "tool_use" {
            match (hold_account.as_ref(), pair_pending_execs(&tool_calls, &mcp_execs), body_keepalive.take()) {
                (Some(acc), Some(pending), Some(body)) => {
                    let n = pending.len();
                    let run = crate::held::HeldRun::new(
                        conversation_id.clone(),
                        byte_stream,
                        std::mem::take(&mut decoder),
                        body,
                        _hb.take(),
                        pending,
                    );
                    let token = crate::held::registry().insert(acc, run);
                    tracing::info!(
                        conversation_id = %conversation_id,
                        account = %acc,
                        calls = n,
                        resumed,
                        "cursor Run:上游流已挂起,等客户端带工具结果回来续跑"
                    );
                    held_token = Some((acc.clone(), token));
                }
                (_, _, body) => {
                    // 配不齐(理论上 hold_ready 已保证):不挂,按旧行为收尾(任务结束即关流)。
                    drop(body);
                }
            }
        }
        if builtin_truncated || dropped_thinking > 0 {
            tracing::info!(
                builtin_truncated,
                dropped_thinking,
                output_chars,
                "cursor Run 收尾(带保留意见:见上面的 warn)"
            );
        }
        let sent_delta = tx
            .send(Ok(StreamItem::Sse(SseEvent::new(
                "message_delta",
                json!({"type":"message_delta",
                       "delta":{"stop_reason":stop_reason,"stop_sequence":null},
                       "usage": delta_usage}),
            ))))
            .await
            .is_ok();
        let sent_stop = tx
            .send(Ok(StreamItem::Sse(SseEvent::new(
                "message_stop",
                json!({"type":"message_stop"}),
            ))))
            .await
            .is_ok();
        // 客户端没收到工具调用(已断开):它不会带结果回来,挂着的流放掉。
        if let Some((acc, token)) = &held_token {
            if !(sent_delta && sent_stop) && crate::held::registry().remove(acc, *token) {
                tracing::info!(
                    conversation_id = %conversation_id,
                    "cursor Run:客户端已断开,释放刚挂起的上游流"
                );
            }
        }
        // 走到这里 = 正常收尾。服务端已经持有这一轮,下次可以只发新消息。
        if let Some(h) = &outcome {
            h(true);
        }
        let _ = tx.send(Ok(StreamItem::Usage(usage))).await;
    });

    tokio_stream::wrappers::ReceiverStream::new(rx)
}
