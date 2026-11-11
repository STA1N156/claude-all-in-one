//! Anthropic Messages → Cursor `agent.v1.AgentService/Run`,并把 ConnectRPC 响应流
//! 桥接成 Anthropic SSE。
//!
//! 端点、头表、请求体结构全部依据 `PROTOCOL-agent-run.md`(2026-08-06 对本机 3.14.27
//! 抓真包逐字节解码)。请求体的 protobuf 构造在 [`crate::run`]。
//!
//! ## 这个文件曾经错在哪
//!
//! 旧版打的是 `aiserver.v1.ChatService/StreamUnifiedChatWithTools`,并在注释里断言
//! 「存在一道只作用于推理路径的客户端完整性门」。**那道门不存在**:真 IDE 的推理
//! 根本不调那个服务,服务端对还在打退役端点的请求回「请升级」,字面为真。
//! 那段注释连同 `CURSOR_METHOD` 环境变量开关、`wrap` 分支、`slow_pool` 参数
//! 一并删除 —— 它们都是那个错误判断的产物。

use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::{ChatRequest, ChatStream, ChatUsage, SseEvent, StreamItem};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::run::{self, Model, RunShape, Turn};
use crate::wire;

mod stream;
mod usage;
mod system;
mod guard;
mod builtin;
mod history;
mod media;
mod session;
mod errors;
pub(crate) mod clients;
pub(crate) use self::stream::*;
pub(crate) use self::usage::*;
pub(crate) use self::system::*;
use self::guard::*;
pub(crate) use self::builtin::*;
pub(crate) use self::history::*;
pub(crate) use self::media::*;
pub(crate) use self::session::*;
pub(crate) use self::errors::*;

/// 从发出请求到收到**响应头**的上限。
///
/// 实测正常情况是 300–800ms(见日志 `响应头已到`)。取 60s 是给代理链路留足余量,
/// 同时保证「上游收了连接但永不回话」的情形有个尽头 —— 它不在
/// [`STALL_TIMEOUT`] 的管辖范围内(那个 watchdog 在流循环里,进不去)。
const HEADER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// 一次 Run 调用需要的、与账号绑定的全部参数。
///
/// 打成一个结构体而不是长参数列表:这些值**必须整组随号冻结**(PROTOCOL §7),
/// 散成 8 个参数很容易在调用点漏传或串号。
#[derive(Debug, Clone)]
pub struct RunCtx {
    pub host: String,
    pub token: String,
    pub machine_id: String,
    pub mac_machine_id: Option<String>,
    pub config_version: String,
    pub timezone: String,
    pub conversation_id: String,
    /// 模拟缓存([`crate::cache_sim`])用的**稳定会话 ID**(= provider 的 `base_conversation_id`,
    /// 由调用方会话亲和键派生,跨轮不变)。
    ///
    /// **不能用 [`Self::conversation_id`]**:那是发给 Cursor 的会话 ID,会话被判分叉
    /// (例如在两个号之间来回跳,回到原号时登记的历史已落后)时,provider 每轮按全部历史
    /// 指纹重算一个新的 → 模拟键每轮都变 → 永远判不命中。2026-11-10 实测:来回换号的会话
    /// 同号连续 5 轮模拟报 0,而 Cursor 账单上每轮读 14~17 万缓存,客户被按全价收。
    /// 上游缓存只按**号**隔离、与 Cursor 会话 ID 无关(账单实证),键见 [`sim_session_key`]。
    pub sim_conversation_id: String,
    /// 账号 id:模拟缓存的会话键要按账号隔离(上游缓存按号隔离,换号 = 整段重写,
    /// 2026-11-10 账单实证),见 [`crate::cache_sim`]。
    pub account_id: String,
    /// 本次请求处在会话的哪个阶段。由 [`crate::ConvRegistry`] 判定:
    /// 服务端已有这个会话(且还是同一个号)→ [`Phase::Continuation`],否则降级回
    /// [`Phase::Opening`] 把历史整个重铺。
    pub phase: run::Phase,
    /// 请求形态(IDE / CLI)。见 [`crate::cli::Profile`]。
    pub profile: crate::cli::Profile,
    /// 帧0 发哪些分节。默认全发(完整模拟客户端);出问题时二分用。
    pub shape: RunShape,
    /// 是否补发 `field 3` 上下文帧(真客户端初始发 3 帧:帧0 + 两个 field 3)。
    pub context_frames: bool,
    /// 发完初始帧后是否**保持请求流不关闭**。
    ///
    /// 真客户端是 BiDi:发完 3 帧继续发 keepalive/增量,不 half-close。
    /// `false` 则一次性发完 body 就关流(HTTP 语义上是 half-close)。
    /// 两种行为服务端可能区别对待,做成开关以便实测。
    ///
    /// ⚠️ 关掉它的代价不止是形态差异:服务端 write/read 调用(带图流程)的
    /// 回执要从请求侧送回去,流关了就没法回 —— 带图请求会 90s 死等。
    pub keep_stream_open: bool,
    /// 服务端 write_args 推来的资产内存库(带图流程的回图来源)。见 [`crate::AssetStore`]。
    pub assets: std::sync::Arc<crate::AssetStore>,
    /// 「上一轮被内建工具截断」的待发纠偏标记。见 [`crate::TruncationNotices`]。
    pub notices: std::sync::Arc<crate::TruncationNotices>,
    /// 续轮描述符影子表(见 [`crate::DescriptorShadow`])。
    ///
    /// 2026-08-23 起**不再是纯观测**:流循环写(干净收尾才提交),出站路径按
    /// `wire_descriptor_replay` 读来回放。读侧只认同号同模型,见
    /// [`crate::DescriptorShadow::lookup`]。
    pub shadow: std::sync::Arc<crate::DescriptorShadow>,
    /// 本轮 history fps(**减末轮**,与 `ConvRegistry::cli_lookup` 同口径):
    /// 影子表 **lookup** 用的指纹:本轮请求历史**减末条 user**。IDE 形态为空。
    pub shadow_fps: Vec<u64>,
    /// 影子表 **commit** 用的指纹:本轮请求历史**完整**(含末条 user)。
    /// 两者差一条,混用会让链错位一轮 —— 那正是静默漏轮。
    pub shadow_fps_full: Vec<u64>,
    /// 续轮回放描述符(见 [`crate::RunTuning::wire_descriptor_replay`])。
    pub wire_descriptor_replay: bool,
    /// 内容分节库:应答服务端 `4.2` 点名(见 [`crate::ContentSections`])。
    pub sections: std::sync::Arc<crate::ContentSections>,
}

/// 模拟缓存([`crate::cache_sim`])的会话键:账号 + 稳定会话 ID(见 [`RunCtx::sim_conversation_id`])。
///
/// `\x1f` 分隔不会撞:account_id 过 validate_account_id(只允许字母数字与 -_.~),
/// 稳定会话 ID 是 UUID,两边都不可能有控制字符。
pub(super) fn sim_session_key(ctx: &RunCtx) -> String {
    format!("{}\x1f{}", ctx.account_id, ctx.sim_conversation_id)
}

/// 发起一次 Cursor Run,返回 Anthropic SSE 事件流。
///
/// `client` 必须能协商 HTTP/2(Cursor 的流式端点强制 h2,降级到 h1 会被 ALB 回 464)。
/// 本轮终态回调:`true` = 正常收尾(服务端已持有这一轮),`false` = 失败。
///
/// 用回调而不是把 `ConvRegistry` 传进来:流循环不该知道会话注册表长什么样,
/// 它只知道「这轮成没成」。也让测试能直接观察终态判定。
pub type OutcomeHook = std::sync::Arc<dyn Fn(bool) + Send + Sync>;

pub async fn chat_stream(
    client: reqwest::Client,
    ctx: RunCtx,
    req: ChatRequest,
    outcome: Option<OutcomeHook>,
) -> Result<ChatStream, UpstreamError> {
    // 调用方是否请求了 thinking。两处都要用:
    // ① 决定发给上游的模型参数(见 apply_thinking_pref);
    // ② 决定收到 `1.4` 帧后是否转成 Anthropic thinking 块 —— **没请求就不发**,
    //    客户端要么报未知块类型、要么把推理当正文显示,两种都比不发坏。
    //
    // ⚠️ Claude Code 的 `adaptive` **必须算「要思考」**:只认 `enabled` 时上游仍开
    // thinking(目录默认 true),收侧却丢掉 `1.4` —— 客户端整段长考期间一个 SSE 都
    // 收不到,首字被吃光(2026-09-04 起思考帧无论透不透传都刷新 last_progress,
    // 所以后果不再是 90s 误杀,而是纯粹的无首字干等)。
    let thinking = req.body.get("thinking").cloned();
    let want_thinking = client_wants_thinking(thinking.as_ref());

    // 未知模型名直接 400,绝不静默归一成 `default`(交接规格第 8 条危险点):
    // 旧行为叠加含 default 的白名单时,任意拼错的模型名都会放行,客户端
    // **静默拿到另一个模型**。已知家族的有意降级(目录外 haiku→composer、
    // gemini→default)不受影响 —— None 只给「任何家族都不沾边」的真未知。
    let Some(cursor_model) = crate::models::resolve_cursor_model(&req.model) else {
        // bad_request_visible:客户端必须看到原文(知道自己拼错了什么才改得了)。
        // 文案我方完全掌控、不含上游响应体/账号标识,符合该构造器的准入条件。
        return Err(UpstreamError::bad_request_visible(format!(
            "cursor: 未知模型名 {:?} —— 不在模型目录且不属于任何已知家族(claude/gpt/grok/gemini/kimi/glm)。\
             用 GET /v1/models 查可用模型",
            req.model
        )));
    };
    // ⚠️ **替换必须留痕。** 我们按族把客户要的模型归一到 Cursor 侧的名字
    // (`claude-haiku-4-5` → `composer-2.5`、未知名 → `default`),而 SSE 回显的
    // `message_start.model` 是**客户请求的那个名字**(协议要求如此,客户端按它对账)。
    // 结果是「执行的模型」在任何对外面上都看不见 —— 客户点 haiku 实际跑 composer,
    // 而账单按 haiku 计价。至少让它在日志里可查。
    if cursor_model != req.model {
        tracing::info!(
            requested = %req.model, upstream = %cursor_model,
            "cursor 模型归一:实际执行的与客户请求的不是同一个(SSE 回显仍是客户请求的名字)"
        );
    }
    let mut model: Model = crate::models::model_by_name(&cursor_model);
    // 把客户端的 thinking 意图**真的发上去**。
    //
    // 早先 `want_thinking` 只是收侧过滤器:客户端开不开 thinking,我方发给上游的请求
    // 一模一样(模型参数恒为目录里那份 `thinking=true`)。两个后果:
    // - 客户明确不要推理时,上游照样按推理档跑 —— 白烧上游额度、也慢。
    // - 客户要推理时我方没有任何可拉的杠杆,「上游到底发不发 `1.4`」完全查不动。
    //
    // ⚠️ 只在**目录本来就声明了该参数**的模型上覆盖(claude 系)。给
    // `composer-2.5` / `gpt-5.6-*` 塞一个它们没有的参数是在猜,而猜错的代价是
    // 整个请求被拒(`invalid_argument`)。
    crate::models::apply_thinking_pref(&mut model, thinking.as_ref());
    // Claude Code 的档位在 `output_config.effort`(不发 budget_tokens),见 apply_effort_pref。
    crate::models::apply_effort_pref(
        &mut model,
        thinking.as_ref(),
        req.body
            .get("output_config")
            .and_then(|c| c.get("effort"))
            .and_then(|e| e.as_str()),
    );
    let catalog = crate::models::catalog();
    // 工具回路的中间轮:本轮 user 消息只有 tool_result,折完就没有问题了 ——
    // 必须把用户的请求复述回本轮,否则模型会重复调同一个工具(见 fold_history)。
    let task = last_user_is_tool_result_only(&req.body)
        .then(|| latest_real_user_request(&req.body))
        .flatten();
    // 附件收集在文本渲染**之前**:to_media 的同一次扫描产出 tool_result 内嵌
    // 媒体的占位表,to_turns_with_media 按它在文本原位补 `[图片见附件 attach-N]`,
    // 编号与附件收集顺序同源,不存在两边各数各的漂移。
    let (images, docs, media_placeholders) = to_media(&req.body);
    // 客户端把我方下发的(无签名)思考降级成普通 text 回传时,置空后再转历史 ——
    // 推理原文折进上游会被 Anthropic 判成推理提取而拦截(见 crate::thinking_echo)。
    let echo_stripped = crate::thinking_echo::strip_from_history(&req.body);
    let turn_body = echo_stripped.as_ref().unwrap_or(&req.body);
    let raw_turns = to_turns_with_media(turn_body, Some(&media_placeholders));
    let mut tools = to_tools(&req.body);
    // 兼容转换表按**调用方声明的** tools 生成(别名判定也看它,不看补位后的)。
    let xlate = BuiltinXlate::from_tools(&tools);
    // write 别名补位:调用方没有写文件工具时,给上游广告补一个 `write`
    // (grok 创建文件执念调它,清单没有就会调未注册名 → 上游静默挂起)。
    // 护栏清单/mcp_tools/IDE 工具表都从这份补位后的 tools 出 —— 模型看得到、
    // 调得通;回调经 `WriteAlias::translate` 折回编辑工具,客户端无感。
    if let Some(a) = &xlate.write_alias {
        tracing::info!(
            target = %a.target,
            "cursor:write 别名已补位进上游广告(调用方无写文件工具)"
        );
        tools.push(write_alias_tooldef(a));
    } else if !tools.is_empty() {
        tracing::debug!(
            tools = ?tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            "cursor:write 别名未补位(调用方已有写工具或无编辑工具)"
        );
    }

    // ── CLI 形态(2026-08-16 抓包,见 `cli.rs` 模块文档)─────────────────────
    // 纯文本、无附件、用户轮结尾的请求走 CLI 极简形态:只发最后一条新消息,
    // 历史由服务端持有(实测跨进程记得,且上游真缓存命中回来)。
    // 2026-09-04 起**工具请求也走这条**:工具经帧0 `1.4` 的 mcp_tools 目录广告
    // (探针实证模型按裸名回调),不再绕行 clidrv。仍不接的:附件与 tool_result
    // 轮(带 tool_use/tool_result 块的请求维持 IDE 形态全量重铺,那是今天的路)。
    // 工具回路续轮(2026-09-04 toolcont 探针实证 + 影子表 tool_turn 宽松匹配):
    // 末条 user 只装 tool_result、且紧邻上一条是带 tool_use 的 assistant = 工具结果轮,
    // 允许进 CLI 形态 —— 影子表有描述符就续轮只发增量(工具结果渲染文本),
    // 没料照旧降级重铺。其它带工具块的请求(本轮不是结果轮)维持回 IDE 形态。
    let tool_result_turn =
        last_user_is_tool_result_only(&req.body) && prev_assistant_has_tool_use(&req.body);
    // 自造会话状态能接的请求(见 crate::blobhist):工具调用/结果按结构写进历史,
    // 带工具块的普通提问轮也不必退回 IDE 形态整段折叠。
    let blob_eligible = crate::blobhist::enabled() && crate::blobhist::eligible(turn_body);
    // 末条消息带图:描述符续轮只发本轮文字、挂流续跑回传只带文字,都会丢图 →
    // 这一轮一律走自造会话状态重建(图片编进 blob)。
    let last_has_images = crate::blobhist::last_message_has_images(turn_body);
    let cli_mode = ctx.profile.is_cli()
        && ((images.is_empty() && docs.is_empty()) || blob_eligible)
        && (!body_has_tool_blocks(&req.body) || tool_result_turn || blob_eligible)
        && raw_turns.last().is_some_and(|t| t.is_user);

    // wire v2:续轮的**前提是拿得到描述符**,所以先查影子表,再决定 turns 怎么铺。
    //
    // ⚠️ 顺序不能反。`1.2` 只带增量消息、而 `1.1` 里没有描述符 = 服务端手里没有历史、
    // 我方也没发 → 模型看不到前几轮却照样答(静默失忆)。所以「阶段说 Continuation」
    // 不足以只发末条,必须**同时**有描述符。没料就降级重铺,代价只是这轮不省钱。
    //
    // model 键必须用 `req.model`(Anthropic 侧名),**不是** `model.name`(cursor 侧名,
    // 如 `default`/`grok-4.6`):提交侧存的就是 `req.model`(见 `stream_to_anthropic`
    // 的 `model_name` 参数)。两边用不同的名 = lookup 永远 miss,而且是静默的。
    let cli_descriptor: Option<Vec<u8>> =
        if cli_mode && ctx.phase.is_continuation() && ctx.wire_descriptor_replay && !last_has_images {
            ctx.shadow.lookup(
                &ctx.conversation_id,
                &ctx.account_id,
                &req.model,
                &ctx.shadow_fps,
            )
        } else {
            None
        };
    let cli_continuation = cli_descriptor.is_some();
    if cli_mode && ctx.phase.is_continuation() && !cli_continuation {
        tracing::info!(
            conversation_id = %ctx.conversation_id,
            account = %ctx.account_id,
            model = %req.model,
            replay_enabled = ctx.wire_descriptor_replay,
            "cursor Run:阶段是续轮但影子表无描述符,降级重铺(防静默失忆)"
        );
    }

    // 服务端已持有本会话历史(Continuation)且拿到了描述符 → 只发本轮新消息。
    // 否则照旧折叠成一条(见 `fold_history` / `delta_history`)。
    // 自造会话状态(见 crate::blobhist):CLI 形态、拿不到续轮描述符、要重铺多轮历史时,
    // 不再把整段历史折成一条(那会让上游每轮整段重写缓存),而是按客户端完整历史
    // 重建服务端同款的消息 blob 列表放进会话状态 `.1`。重建不了(有媒体 / prefill /
    // 开关关)就照旧折叠。
    let rebuilt: Option<crate::blobhist::Rebuilt> =
        if cli_mode && !cli_continuation && crate::blobhist::enabled() {
            // 末条没有文字时(只有工具结果 / 只有图片)本轮用户消息:工具回路提醒 + 原始请求。
            let fallback = match task.as_deref() {
                Some(t) => {
                    let mut s = String::new();
                    push_tool_loop_nudge(&mut s, t);
                    s.trim_start().to_string()
                }
                None => tool_loop_nudge().to_string(),
            };
            crate::blobhist::rebuild(turn_body, Some(fallback))
        } else {
            None
        };
    // 重建的 blob 只放本请求自带的表(WireUpload.overlay),**不进全局节库**:后者按条数
    // 封顶、不按字节,大图会话每请求灌几百 MB 会撑爆内存,且插入在全局锁下扫描(代码审查)。
    // 之后若走续轮描述符而服务端又来要这些 blob,交不出 → 本轮重铺(再走重建),不影响正确性。
    if let Some(r) = &rebuilt {
        tracing::info!(
            conversation_id = %ctx.conversation_id,
            account = %ctx.account_id,
            messages = r.messages,
            blob_bytes = r.bytes,
            "cursor Run:自造会话状态(消息 blob 历史),不折叠重铺"
        );
    }
    let turns = if let Some(r) = &rebuilt {
        vec![Turn {
            text: r.current.clone(),
            is_user: true,
        }]
    } else if cli_mode {
        if cli_continuation {
            // 只发最后一条新消息(gate 已保证它是 user 轮)。
            vec![raw_turns.last().cloned().expect("cli gate 保证非空")]
        } else if raw_turns.len() == 1 {
            raw_turns.clone()
        } else {
            // 重铺(新会话/历史分叉):Phase 1 先折叠进首条消息,与线上同口径。
            // TODO(§20):CLI 形态的原生多条 1.2.1 重铺还没做消融实验,证实可行后换掉。
            fold_history(&raw_turns, task.as_deref())
        }
    } else if delta_history_enabled() && ctx.phase.is_continuation() {
        delta_history(&raw_turns, task.as_deref())
    } else {
        fold_history(&raw_turns, task.as_deref())
    };
    let system = {
        let mut sys = extract_system(&req.body);
        if !tools.is_empty() {
            sys.push_str(&client_env_note(&req.body));
        }
        // CLI 形态的工具走帧0 `1.4` 的 mcp_tools 目录(裸名注册,模型按裸名回调,
        // 2026-09-04 探针实证),护栏文案必须用裸名版 —— 带 `gwtools-` 前缀版是
        // IDE 形态 frame0 的注册口径,写进去会让模型去调不存在的名字。
        if cli_mode && !tools.is_empty() {
            sys.push_str(&mcp_tool_guard_with(&tools, &crate::tool_guard_policy()));
        } else {
            sys.push_str(&builtin_tool_guard(&tools));
        }
        sys
    };

    // 文档路径要**写进用户文本**。真客户端就是这么干的(实测:文本 =
    // "<路径> <问题>"),因为 `1.2.1.2.20` 只是按路径索引的内容登记表 ——
    // prompt 里不提路径,模型根本不知道有附件,只会说"我来找这个文件"。
    let turns = if docs.is_empty() {
        turns
    } else {
        let mut t = turns;
        if let Some(i) = t.iter().rposition(|x| x.is_user) {
            let mut pre = String::new();
            for d in &docs {
                match &d.extracted {
                    Some(txt) => {
                        // 抽到了就直接给模型看,别让它去调工具读文件 —— 那条路我们答不了。
                        pre.push_str(&format!(
                            "<document path=\"{}\">\n{}\n</document>\n\n",
                            d.path, txt
                        ));
                    }
                    None => {
                        // 抽不到(扫描件 / 非 Flate 压缩 / 图片型)。**明确告诉模型**,
                        // 否则它会反复尝试读文件,而我们每次都只能收口。
                        pre.push_str(&format!(
                            "<document path=\"{}\" note=\"无法抽取文本层(可能是扫描件或图片型 PDF);\
                             请直接告知用户无法读取,不要尝试调用工具读文件\"/>\n\n",
                            d.path
                        ));
                    }
                }
            }
            t[i].text = format!("{pre}{}", t[i].text);
        }
        t
    };
    // 上一轮被内建工具截断过 → 在本轮用户消息末尾补一句纠偏(取走即消费)。
    //
    // 放在**最后**:模型对轮末的指令最敏感,而这句话要盖过它上一轮的行为惯性。
    // 也放在 `[继续]` 之后 —— 那句说的是「工具结果已拿到」,这句说的是
    // 「上次那个工具根本不存在」,后者是更强的纠正。
    //
    // 不影响缓存指纹:`sim_fps` 取的是 `raw_turns`(折叠之前),见下方注释。
    let turns = match ctx.notices.take(&ctx.conversation_id) {
        Some(cap) => {
            tracing::info!(
                conversation_id = %ctx.conversation_id,
                cap = ?cap,
                "cursor:上一轮被内建工具截断,本轮注入纠偏"
            );
            // 与上面护栏同一判据:CLI 形态按裸名注册。
            let notice = truncation_notice_for(cap, &tools, cli_mode);
            let mut t = turns;
            match t.iter().rposition(|x| x.is_user) {
                Some(i) => t[i].text.push_str(&notice),
                // 理论上不会:fold_history 恒返回一条 user 轮。真出现就丢掉这句,
                // 绝不把纠偏话术塞进 assistant 轮(那会变成「模型自己说过的话」)。
                None => tracing::warn!("cursor:没有 user 轮可挂纠偏,本轮跳过"),
            }
            t
        }
        None => turns,
    };

    let media = run::Media {
        images: &images,
        docs: &docs,
    };
    if turns.is_empty() {
        return Err(UpstreamError::bad_request("cursor: 请求里没有任何消息"));
    }

    // Prefix 缓存命中模拟(与 kiro 同一家口径,见 [`crate::cache_sim`] 模块注释):
    // 上游 `1.14` 几乎从不报缓存命中,客户侧计费要与 kiro 通道对齐只能靠模拟。
    // 键 = 账号 + 稳定会话 ID(见 [`sim_session_key`]:上游缓存按号隔离,与 Cursor
    // 会话 ID 无关,分叉换 ID 不能让模拟失忆)。指纹取**稳定语义层**(to_turns 的逐条消息,
    // fold_history 之前)——折叠形态里闭合标签横在中间,第二轮就不再是上一轮的
    // 字节前缀,而逐消息指纹跨轮逐字节稳定。两步走:这里 peek 拿计费值;
    // **成功收尾才 commit**(失败轮服务端没落下这一轮,指纹不能进模拟表 ——
    // 与 ConvRegistry 的 forget 语义一致),所以把 commit 包进 outcome 回调。
    let sim_key = sim_session_key(&ctx);
    let sim_fps =
        crate::cache_sim::fingerprints_from_context(&system, &tools, &raw_turns, est_text_tokens);
    let (sim, sim_gen) = crate::cache_sim::peek(&sim_key, &req.model, &sim_fps);
    let sim_cache_read = sim.cache_read_tokens as u64;
    let outcome = outcome.map(|h| {
        let key = sim_key.clone();
        let model = req.model.clone();
        let fps = sim_fps.clone();
        // 先 commit 再通知调用方:两方看到的是同一份终态。commit 带代际 CAS:
        // 同会话并发请求后完成的那个会被拒(宁可少记一轮,不乱序覆盖)。
        // 注意内建工具截断路径同样走 h(true):截的是**响应**,请求轮本身服务端
        // 已处理(模型已出字),ConvRegistry 在同路径 confirm 是既有语义,
        // 模拟缓存与它保持一致,不另开三态。
        std::sync::Arc::new(move |ok: bool| {
            if ok {
                crate::cache_sim::commit(&key, &model, fps.clone(), sim_gen);
            }
            h(ok);
        }) as OutcomeHook
    });

    // ── 挂流续跑(见 crate::held)────────────────────────────────────────────
    // 开关开、CLI 形态、请求流保持打开时,本轮若是纯 MCP 工具调用轮,收尾时挂起上游流。
    let hold_account =
        (crate::held::enabled() && cli_mode && ctx.keep_stream_open).then(|| ctx.account_id.clone());
    // 工具结果轮:按带回的结果 id 取挂起的流,在原流上回结果、接着读,不重发历史。
    // 取不到(没挂、id 对不上、过期、换了号)或回结果失败 → 落到下面的冷启动重铺。
    // 工具结果带图:续跑只能回传文字(会丢图),不走续跑;把挂起的那条流当场释放,
    // 免得它空等到闲置超时(本轮改走自造会话状态重建,图片编进 blob)。
    if let (Some(acc), true) = (hold_account.as_ref(), tool_result_turn && last_has_images) {
        if let Some(results) = crate::held::tool_results(&req.body) {
            let ids: Vec<&str> = results.iter().map(|(id, ..)| id.as_str()).collect();
            if crate::held::registry().take(acc, &ids).is_some() {
                tracing::info!(
                    conversation_id = %ctx.conversation_id,
                    account = %acc,
                    "cursor Run:工具结果带图,不走续跑(会丢图),已释放挂起的流"
                );
            }
        }
    }
    if let (Some(acc), true) = (hold_account.as_ref(), tool_result_turn && !last_has_images) {
        if let Some(results) = crate::held::tool_results(&req.body) {
            let ids: Vec<&str> = results.iter().map(|(id, ..)| id.as_str()).collect();
            match crate::held::registry().take(acc, &ids) {
                Some(run) => {
                    let held_for = run.held_for();
                    let held_conv = run.conversation_id.clone();
                    let sink: BodyKeepalive = Some(run.body.clone());
                    let mut sent = true;
                    for (tid, text, is_error) in &results {
                        let Some(p) = run.pending.iter().find(|p| &p.tool_use_id == tid) else {
                            sent = false;
                            break;
                        };
                        // 按上游在等的种类编码(MCP / 自带读文件 / 终端 / 流式终端,可能多帧)。
                        for payload in crate::held::encode_result(p, text, *is_error) {
                            if let Err(e) = wire_send_frame(&sink, wire::frame(&payload)).await {
                                tracing::warn!(error = e, "cursor Run:续跑回传工具结果失败,改走冷启动重铺");
                                sent = false;
                                break;
                            }
                        }
                        if !sent {
                            break;
                        }
                    }
                    if sent {
                        tracing::info!(
                            conversation_id = %ctx.conversation_id,
                            held_conversation = %held_conv,
                            account = %acc,
                            results = results.len(),
                            held_secs = held_for.as_secs(),
                            "cursor Run:工具结果已在原流上回传,续跑(不重发历史)"
                        );
                        // 计费按客户端发来的**完整上下文**估(见下方冷启动处的说明),不按续跑段实际上传的那点。
                        let usage_fallback = billing_estimate(&system, &raw_turns, &turns, &tools);
                        let crate::held::HeldRun { byte_stream, decoder, body, heartbeat, .. } = run;
                        return Ok(Box::pin(stream_to_anthropic_with(
                            byte_stream,
                            req.model.clone(),
                            want_thinking,
                            Some(body),
                            outcome,
                            ctx.conversation_id.clone(),
                            ctx.assets.clone(),
                            ctx.notices.clone(),
                            xlate,
                            usage_fallback,
                            sim_cache_read,
                            true,
                            Some(WireUpload {
                                deferred: Vec::new(),
                                sections: ctx.sections.clone(),
                                mcp_tools: tools.clone(),
                                overlay: None,
                            }),
                            Some(crate::ShadowFeed {
                                map: ctx.shadow.clone(),
                                account_id: ctx.account_id.clone(),
                                fps: ctx.shadow_fps_full.clone(),
                            }),
                            HoldOpts {
                                account_id: Some(acc.clone()),
                                resume: Some(ResumeState {
                                    decoder,
                                    heartbeat,
                                    answered_ids: ids.iter().map(|s| s.to_string()).collect(),
                                }),
                            },
                        )));
                    }
                }
                None => tracing::debug!(
                    conversation_id = %ctx.conversation_id,
                    results = ids.len(),
                    "cursor Run:工具结果轮没有可续跑的挂起流,冷启动重铺"
                ),
            }
        }
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let url = format!("https://{}/agent.v1.AgentService/Run", ctx.host);

    // CLI 形态与 IDE 形态分岔:帧序列与头表完全不同(见 `cli.rs` 模块文档)。
    let (frames, rb) = if cli_mode {
        // 全名双注册(仅 CLI 形态;mcpalias 探针实证可路由):服务端转录把我们
        // 的工具称作 mcp_claude-local_<name>(见 MCP_FULL_PREFIX),模型学到全名
        // 就会调它 —— 未注册则上游静默挂起。每个工具补注册一个全名变体;
        // 回调在工具臂归一回裸名。IDE 形态不做(它的注册全名是 gwtools- 前缀,
        // 与这套命名无关)。护栏清单与 cache_sim 指纹都用**未扩展**的 tools,
        // 全名变体只是广告里的路由别名,不该进任何计费/文案口径。
        let ad_tools: Vec<run::ToolDef> = tools
            .iter()
            .flat_map(|t| {
                let mut full = t.clone();
                full.name = format!("{MCP_FULL_PREFIX}{}", t.name);
                [t.clone(), full]
            })
            .collect();
        // turn_id 同时是 `1.25` 与 x-request-id/x-original-request-id ——
        // 真 CLI 里这三个值同源(重试时 x-request-id 换新、1.25 与 original 不变;
        // 我方每个网关请求都是新的逻辑尝试,全部同值即可)。
        let turn_id = uuid::Uuid::new_v4().to_string();
        // 首轮 / 续轮由「有没有描述符」决定(见上面 cli_descriptor 的注释):
        // 这个 enum 让「续轮但没有描述符」不可表示。
        let phase = match cli_descriptor.as_deref() {
            Some(desc) => crate::cli::CliTurn::Continuation(desc),
            // 自造会话状态与续轮描述符同处 `1.1`,同样逐字节放入(见 crate::blobhist)。
            None if rebuilt.is_some() => crate::cli::CliTurn::Continuation(
                &rebuilt.as_ref().expect("刚判过 is_some").css,
            ),
            None => crate::cli::CliTurn::Opening,
        };
        // ⚠️ 探针阶段写死 lan 的 9 条目录(见 cli_catalog_lan 的生产化告警)。
        let cli_catalog = crate::cli::cli_catalog_lan();
        let frame0 = crate::cli::build_frame0_cli(
            &turns[0].text,
            &model,
            &cli_catalog,
            &ctx.conversation_id,
            &turn_id,
            phase,
            &ad_tools,
        );
        let context = crate::cli::build_context_frame_cli(
            &system,
            &ctx.token,
            &ctx.conversation_id,
            &ctx.timezone,
            "/",
            // 裸名工具进 RequestContext 的 `.7`/`.34`(全名变体只是帧0 的路由别名)。
            &tools,
        );
        if let Ok(dir) = std::env::var("CURSOR_DUMP_REQ") {
            let f = format!("{dir}/cli_frames_{}.bin", uuid::Uuid::new_v4().simple());
            let _ = std::fs::write(&f, [frame0.as_slice(), b"\n----\n", &context].concat());
            tracing::warn!(file = %f, "已落盘 CLI 帧0+上下文帧 payload");
        }
        tracing::debug!(
            phase = ?ctx.phase,
            frame0_bytes = frame0.len(),
            context_bytes = context.len(),
            "cursor Run 请求已构造(CLI 形态)"
        );
        let payloads = crate::cli::cli_request_frames(&frame0, &context);
        let mut frames = Vec::with_capacity(payloads.len());
        for (p, compress) in &payloads {
            // 逐帧按真包决定是否 gzip(首轮帧0 不压,两个大帧压,小帧裸发)。
            if *compress {
                frames.push(wire::frame_compressed(p).map_err(|e| {
                    UpstreamError::new(UpstreamErrorKind::Other, format!("gzip 请求帧失败: {e}"))
                })?);
            } else {
                frames.push(wire::frame(p));
            }
        }
        (frames, apply_cli_headers(client.post(&url), &ctx, &turn_id))
    } else {
        let frame0 = run::build_frame0(
            &turns,
            &system,
            &tools,
            media,
            &model,
            &catalog,
            &ctx.conversation_id,
            &ctx.timezone,
            now_ms,
            ctx.shape,
            ctx.phase,
        );
        // 逆向期用:把帧0 的 payload(未 gzip)原样落盘,好与真客户端逐字节对比。
        if let Ok(dir) = std::env::var("CURSOR_DUMP_REQ") {
            let f = format!("{dir}/frame0_{}.bin", uuid::Uuid::new_v4().simple());
            let _ = std::fs::write(&f, &frame0);
            tracing::warn!(file = %f, bytes = frame0.len(), "已落盘帧0 payload");
        }
        tracing::debug!(
            phase = ?ctx.phase,
            turns = turns.len(),
            frame0_bytes = frame0.len(),
            "cursor Run 请求已构造"
        );

        // 帧0 走 gzip(6KB 级,压缩有意义);开场四帧是 2-4 字节的裸帧,
        // 抓包实物就是 flag=0x00 不压缩 —— 压几字节只会更大。
        let mut frames = vec![wire::frame_compressed(&frame0).map_err(|e| {
            UpstreamError::new(UpstreamErrorKind::Other, format!("gzip 请求帧失败: {e}"))
        })?];
        if ctx.context_frames {
            frames.extend(run::build_prelude_frames().iter().map(|p| wire::frame(p)));
        }
        (frames, apply_headers(client.post(&url), &ctx))
    };

    // wire v2:CLI 形态**只先发帧0**,ENV + 控制帧推迟到会话登记通知之后
    // (见 [`WireUpload`] 的时序说明)。非 CLI 形态维持一次性灌帧。
    let mut frames = frames;
    let wire_deferred: Vec<Vec<u8>> = if cli_mode && ctx.keep_stream_open && frames.len() > 1 {
        frames.split_off(1)
    } else {
        Vec::new()
    };

    // 保持请求流打开时,body 走一个 channel:初始帧先灌进去,发送端在响应读完前
    // 一直不 drop,于是 HTTP/2 的请求流不会 half-close —— 与真 BiDi 客户端一致。
    let (rb, body_keepalive) = if ctx.keep_stream_open {
        // 容量要装下初始帧全集(CLI 形态 13 帧),否则 try_send 会静默丢帧。
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(16);
        for f in frames {
            let _ = tx.try_send(Ok(bytes::Bytes::from(f)));
        }
        let body = reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
        (rb.body(body), Some(tx))
    } else {
        (rb.body(frames.concat()), None)
    };

    // ⚠️ **只给「等响应头」这一段设超时,不给整个请求设。**
    //
    // 账号配了 proxy 时用的是 `build_proxy_client`,那个 client **故意不设总超时** ——
    // Run 是流式的,总超时会把长回复掐断。代价是:TCP/TLS 都成功、h2 流也开了,
    // 但上游(或中间代理)永远不发响应头时,`send()` 会一直等下去。而 `STALL_TIMEOUT`
    // 的 watchdog 在流循环里,**拿不到响应根本进不去**。此时 scheduler 的并发租约被
    // 这个请求永久占住,几个这样的请求就能把号的并发槽耗光,表现是「号没坏但永远 busy」。
    //
    // 响应头到了就交给现有 watchdog 管,所以这里只需要一个握手期的上限。
    let resp = tokio::time::timeout(HEADER_TIMEOUT, rb.send())
        .await
        .map_err(|_| {
            UpstreamError::network(format!(
                "Cursor Run {}s 内没有返回响应头(连接已建立),放弃",
                HEADER_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|e| UpstreamError::network(format!("Cursor Run 请求失败: {e}")))?;

    // 降级到 HTTP/1.1 的表现是 ALB 回 464,错误体没有任何线索。先在这里点名,
    // 免得下次又去翻头表。
    if resp.version() != reqwest::Version::HTTP_2 {
        tracing::warn!(
            version = ?resp.version(),
            "Cursor Run 未走 HTTP/2(ALPN 协商失败或代理降级),上游大概率回 464"
        );
    }

    let status = resp.status();
    tracing::debug!(status = status.as_u16(), version = ?resp.version(), "cursor Run 响应头已到");
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_http_error(status.as_u16(), &text));
    }

    let model_name = req.model.clone();
    // tool_use 收口时常没有上游 `1.14`,用请求体预估一版兜底(见 estimate_usage_fallback)。
    // output_chars 在流里才知道,这里先按 0 估输入/缓存;收尾处会用真实 output 覆盖。
    //
    // ⚠️ **按客户端发来的完整上下文(`raw_turns`)估,不按实际上传给上游的 `turns`。**
    // CLI 形态上游不发用量帧,客户账单全靠这里;直连 API 的口径是「整段 prompt 都算输入,
    // 命中前缀的部分按缓存读」。`turns` 在自造历史时只剩本轮一条(历史在 blob 里)、
    // 续轮描述符时只剩最后一条新消息 —— 2026-11-09 22:09 开自造历史后,线上记账输入只有
    // 报文实际大小的 4%~30%(637 条 / 约 30.7 万 token 的请求只记 11,909),yapi cursor 渠道
    // 每请求扣费掉到约 1/3。缓存模拟本来就按 `raw_turns` 算,封顶在输入上,所以也跟着变小。
    //
    // 两份取大:PDF 文字层只拼进了 `turns`(`raw_turns` 里是媒体占位),带文档的折叠请求
    // 以 `turns` 为准;自造历史/续轮时 `turns` 只剩本轮,以 `raw_turns` 为准(审查指出)。
    let usage_fallback = billing_estimate(&system, &raw_turns, &turns, &tools);
    Ok(Box::pin(stream_to_anthropic_with(
        resp.bytes_stream(),
        model_name,
        want_thinking,
        body_keepalive,
        outcome,
        ctx.conversation_id.clone(),
        ctx.assets.clone(),
        ctx.notices.clone(),
        xlate,
        usage_fallback,
        sim_cache_read,
        cli_mode,
        if cli_mode {
            // 节库:该模型的系统提示节(每模型一份、跨会话稳定)。装不上就是
            // 「被点名时这一节交不出来」→ 本轮重铺,不影响正确性。
            ctx.sections.load_model_library(&model.name);
            Some(WireUpload {
                deferred: wire_deferred,
                sections: ctx.sections.clone(),
                mcp_tools: tools.clone(),
                overlay: rebuilt.as_ref().map(|r| r.blobs.clone()),
            })
        } else {
            ide_kv_upload(ctx.keep_stream_open, &ctx.sections, &tools)
        },
        // 描述符只存在于 CLI 形态(IDE 形态没有顶层 `.3`)。**IDE 路径不挂**
        // (codex 审查 #6):否则生产默认形态也要逐帧扫 `.3`/`4.2`、写 info 日志、
        // 往影子表堆条目 —— 纯开销,且 IDE 路径的 `shadow_fps` 恒空,血脉判据退化成
        // 「所有会话挤一个键」。
        if cli_mode {
            Some(crate::ShadowFeed {
                map: ctx.shadow.clone(),
                account_id: ctx.account_id.clone(),
                // commit 口径:完整链(含末条 user)。
                fps: ctx.shadow_fps_full.clone(),
            })
        } else {
            None
        },
        HoldOpts {
            account_id: hold_account,
            resume: None,
        },
    )))
}

/// IDE 形态(带图片/文档时的回退路径)也要应答服务端的 KV 子协议。
///
/// 服务端每存一个内容节都推一帧 `set_blob`,**等客户端回执**;要回已存的节时发
/// `get_blob`。这套应答原先只挂在 CLI 形态,IDE 路径没人回 —— 服务端推完 set 后
/// 只剩心跳,90 秒后被停滞闸掐断(2026-11-07 e2e:读 PDF 挂死,读图偶发零产出;
/// 时有时无取决于这一轮服务端要不要存/取内容节)。
///
/// 只挂 KV 应答与握手:没有推迟帧(IDE 一次性灌帧)。握手时服务端会问 `gwtools`
/// (帧0 注册工具的服务名)有哪些工具,要按帧0 同一份清单报回去 —— 报空表的话模型
/// 以为该服务没授权,去调 mcp_auth 然后干等(2026-11-07 生产)。
/// 请求流不保持打开时回不了帧,不挂。
fn ide_kv_upload(
    keep_stream_open: bool,
    sections: &std::sync::Arc<crate::ContentSections>,
    tools: &[run::ToolDef],
) -> Option<WireUpload> {
    keep_stream_open.then(|| WireUpload {
        deferred: Vec::new(),
        sections: sections.clone(),
        mcp_tools: tools.to_vec(),
        overlay: None,
    })
}

/// wire v2 的上传腿状态:推迟发送的帧 + 应答 `4.2` 点名用的节库。
///
/// ## 为什么帧0 之后要推迟
///
/// 2026-08-23 实物时序(三轮一致):帧0 → 服务端 ack `{1:{13:}}` → (turn4:7 个 `4.2`
/// 点名 → 客户端交 7 个内容槽) → **会话登记通知** → 客户端才发 ENV + 控制帧。
///
/// 早先实现把 13 帧一次性灌进 body channel,于是 ENV 抢在内容交换之前。触发器是
/// **会话登记通知**(不是「内容交换完成」)—— turn4 通知来得晚,是因为服务端要拿到
/// 内容才能登记会话;turn1/turn2 零点名,通知直接来,ENV 随即发。按通知触发对两种
/// 情形都成立;若写成「等内容交换」,零点名的会话会等一个永不到来的事件。
pub(crate) struct WireUpload {
    /// ENV + 控制帧。等会话登记通知(或兜底期限)后按序发出。
    pub deferred: Vec<Vec<u8>>,
    /// 按 hash 供给内容分节。
    pub sections: std::sync::Arc<crate::ContentSections>,
    /// 应答 `mcp_state` 握手时报给服务端的工具(裸名,见 [`crate::exec`])。
    pub mcp_tools: Vec<run::ToolDef>,
    /// 本请求自造会话状态引用的 blob(见 crate::blobhist),点名时优先从这里交。
    pub overlay: Option<std::sync::Arc<std::collections::HashMap<[u8; 32], Vec<u8>>>>,
}

/// 往请求体通道发一帧的**上限**(codex 终审 #3)。
///
/// 裸 `btx.send().await` 会在 HTTP/2 流控停排时无界等待,而响应读取、stall 看门狗、
/// 下游关闭检测全都在同一个任务里 —— 一次停排就把三件事一起冻住,表现是「号没坏
/// 但永远 busy」。加上限之后最坏情况是这一帧发失败,由调用方按内容供给失败走判决。
const WIRE_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 带期限地往请求体通道发一帧。`Ok(())` = 发出去了。
async fn wire_send_frame(btx: &BodyKeepalive, frame: Vec<u8>) -> Result<(), &'static str> {
    let Some(btx) = btx else {
        return Err("请求流未保持打开");
    };
    match tokio::time::timeout(WIRE_SEND_TIMEOUT, btx.send(Ok(bytes::Bytes::from(frame)))).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err("请求流已关"),
        Err(_) => Err("发送超时(HTTP/2 流控停排?)"),
    }
}

/// 请求流的「不要关」把手。/// 请求流的「不要关」把手。持有它 = 请求体的 channel 发送端还活着 =
/// HTTP/2 请求流不 half-close。读完响应后随任务一起 drop,流才收。
type BodyKeepalive = Option<tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>>;

#[cfg(test)]
mod tests;
