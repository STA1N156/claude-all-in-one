//! 消息转换、会话指纹、续轮判定、工具回路提醒与折叠重铺。
//!
//! 2026-11-10 由 chat.rs 拆出(只搬不改行为)。

use super::*;

/// 单条消息的逐轮指纹:`sha256(角色标记 + 渲染文本)` 截前 8 字节。
///
/// **当年 CLI 驱动(clidrv,2026-11-10 已删除)的泵在干净收尾后也用它给 assistant 输出补指纹**(多路桶精确分层,
/// 见 `CliConv::append_assistant_fp`)—— 那里算的是「下轮请求历史里那条 assistant
/// 消息」的指纹,必须与这里逐字节一致,所以两处共用同一函数,口径不可能漂移。
/// 渲染文本的口径见 `to_turns` / `extract_text_in_msg`(text 块 '\n' 相连,
/// thinking 不进,tool_use 渲染含参数 JSON 再序列化 —— 后者字节不由我方掌控,
/// 所以工具轮不补,见调用处注释)。
pub fn turn_fp(is_user: bool, text: &str) -> u64 {
    let mut h = Sha256::new();
    h.update([if is_user { b'u' } else { b'a' }]);
    h.update(text.as_bytes());
    let d = h.finalize();
    u64::from_be_bytes(d[..8].try_into().unwrap())
}

/// 调用方历史的逐轮指纹(CLI 形态的分叉检测,见 `ConvRegistry::cli_lookup`)。
///
/// 指纹取 `to_turns` 的逐条消息(折叠之前),与 cache_sim 同一稳定语义层:
/// 折叠形态里闭合标签横在中间,跨轮不再是字节前缀。
pub fn history_fps(body: &Value) -> Vec<u64> {
    to_turns(body)
        .iter()
        .map(|t| turn_fp(t.is_user, &t.text))
        .collect()
}

/// 消息里是否含 tool_use / tool_result 块。
///
/// 2026-09-04 起工具回路**可以**走 CLI 形态:工具轮上游给 commit+描述符
/// (toolcont 探针),工具结果轮用影子表 tool_turn 条目续轮(见 gate 处
/// `tool_result_turn`)。本函数仍用于把「非结果轮的工具块请求」挡回 IDE。
pub(super) fn body_has_tool_blocks(body: &Value) -> bool {
    body.get("messages")
        .and_then(|m| m.as_array())
        .is_some_and(|msgs| {
            msgs.iter().any(|m| {
                m.get("content")
                    .and_then(|c| c.as_array())
                    .is_some_and(|bs| {
                        bs.iter().any(|b| {
                            matches!(
                                b.get("type").and_then(|t| t.as_str()),
                                Some("tool_use") | Some("tool_result")
                            )
                        })
                    })
            })
        })
}

/// Anthropic 请求体 → Run 的会话轮次列表。
///
/// ⚠️ **system 不在这里。** 它的家是 `1.2.17.9.25`(抓包实物证实),由
/// [`extract_system`] 单独取出后交给 [`run::build_frame0`]。早先的实现把 system
/// 折进第一条用户消息 —— 那会让模型把系统指令当成用户说的话。
pub fn to_turns(body: &Value) -> Vec<Turn> {
    to_turns_with_media(body, None)
}

/// [`to_turns`] 的占位感知版。`media` 是 [`to_media`] 同一次扫描产出的占位表:
/// 带表时,tool_result **内嵌**且被收进附件的图片/文档在文本流原位渲染成
/// `[图片见附件 attach-N]`,编号与附件顺序天然一致。不带表 = 旧行为。
/// 生产的 cache_sim 指纹取的是带占位的 raw_turns —— 占位文本由消息内容
/// 决定、跨轮稳定,逐消息指纹仍逐轮一致,模拟缓存不受影响。
pub(super) fn to_turns_with_media(body: &Value, media: Option<&MediaPlaceholders>) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for (mi, m) in msgs.iter().enumerate() {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let text = extract_text_in_msg(
                m.get("content").unwrap_or(&Value::Null),
                media.map(|p| (mi, p)),
            );
            if text.is_empty() {
                // 走到这里 = 这一轮只有非文本块。整条丢掉会让 user/assistant 交替错位,
                // 所以留一个占位。**区分两种情况**:媒体块是真的发出去了(见 to_media),
                // 说"不支持"会误导模型去问用户重发;其它未知块才是真不支持。
                let has_media = m
                    .get("content")
                    .and_then(|c| c.as_array())
                    .is_some_and(|bs| {
                        bs.iter().any(|b| {
                            matches!(
                                b.get("type").and_then(|t| t.as_str()),
                                Some("image") | Some("document")
                            )
                        })
                    });
                turns.push(Turn {
                    text: if has_media {
                        "(见附件)"
                    } else {
                        "(unsupported content omitted)"
                    }
                    .to_string(),
                    is_user: role != "assistant",
                });
                continue;
            }
            turns.push(Turn {
                text,
                is_user: role != "assistant",
            });
        }
    }
    turns
}

/// Anthropic 请求的 `tools` → Cursor 的工具声明。
///
/// Anthropic 侧是 `{name, description, input_schema}`;Cursor 侧要 5 个字段
/// (见 [`run::ToolDef`]),其中命名空间固定成 [`run::TOOL_NS`] —— 它同时是**回调时
/// 认领工具的依据**:模型调 `gwtools-<name>` 回来才是调用方的工具,
/// 否则是 Cursor 服务端自带的内建工具(我们执行不了)。
///
/// `input_schema` 缺失时给一个空 object schema:那一位在真包里 16/16 全都有值,
/// 留空很可能被拒,而"无参数工具"用空 schema 表达是标准做法。
pub(crate) fn to_tools(body: &Value) -> Vec<run::ToolDef> {
    let Some(arr) = body.get("tools").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?.to_string();
            let schema = t
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Some(run::ToolDef {
                name,
                description: t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string(),
                schema: schema.to_string(),
            })
        })
        .collect()
}

/// 增量历史模式的开关。**默认关,而且已被实测否决 —— 不要开。**
///
/// ## 2026-08-15 真号实测:开了就丢历史
///
/// 稳定 `metadata.user_id` 连发三轮(①记住 4712 ②聊件无关的事 ③问那个数字),
/// 同一构建、只翻这个开关:
///
/// | `CURSOR_DELTA_HISTORY` | 第三轮回答 |
/// |---|---|
/// | `1`(只发本轮) | 「这次对话里我没收到过你要我记住的数字」❌ |
/// | `0`(内联全量) | 「4712」✅ |
///
/// 结论:**服务端在我方这种请求形态下并不持有历史。** 模型的记忆百分之百来自
/// 我们内联的那份文本。
///
/// ## 那 `PROTOCOL-agent-run.md` §17.3 的「历史确实在服务端」是怎么来的
///
/// 那个实验有混淆变量:它断言「两个事实跨 4 轮全部答对 → 历史在服务端」,但当时
/// 请求里**同时还内联着全量历史**([`fold_history`] 无条件生效)。答对完全可以只是
/// 因为历史被贴给了模型。上面这次是第一次把内联那份拿掉的干净检验。
///
/// 于是 §17.3 的 98.7% 命中与「新算 token 降到两位数」要重新归因:收益来自
/// **内联文本被上游前缀缓存**,不是来自服务端持有历史。两个机制被混成了一个。
///
/// 真客户端能只发 2121B,靠的是 `1.2.17` + FileSyncService 上传的 4 个 blob 哈希;
/// §17.2 已实测我方走不了那条路(带哈希被拒,不带哈希则服务端静默等一个永不到来的 blob)。
///
/// 代码保留是为了把这次测量连同反例一起留在仓库里(测试锁住行为),
/// **不是留着等人打开**。要重开必须先在协议层拿到「服务端真的替我方存了历史」的直接证据。
pub(super) fn delta_history_enabled() -> bool {
    gw_core::env_flag("CURSOR_DELTA_HISTORY")
}
/// **只发本轮新消息** —— 前提是服务端已持有历史,而 2026-08-15 实测**它并没有**。
/// 见 [`delta_history_enabled`] 的实测表:开这条路会丢历史。**别开。**
///
/// ## 它原本要解决什么(问题仍然存在,只是这个解法不成立)
///
/// [`fold_history`] 把整段对话渲染成**一条**用户消息
/// (`<conversation_history>User: … Assistant: …</conversation_history>` + 本轮)。
/// 上游看到的不是结构化多轮,而是「一个人在一条消息里自问自答了一整段,末尾又问了一句」。
/// 这个形状是 grok 反复重述/绕圈的怀疑对象。
///
/// 本函数试的方向是「声明续写就只发新东西」,与真客户端的**上传字节**行为一致
/// (§12:turn1 98858B → turn2 2121B)。实测否决:真客户端靠 blob 哈希接上前文,
/// 我方拿不到,声明了续写服务端也不会替我们记住。
///
/// 所以「一坨长文本」这个形状目前**没有替代品** —— 要改只能改渲染本身(分隔符、
/// 角色标记、把历史挪进 `1.2.1.2` 上下文块的独立条目),而不是不发。
///
/// ## 「本轮」的边界
///
/// 与 `fold_history` 用同一个锚:**最后一条 user 轮**及其之后的内容(尾随 assistant 是
/// prefill,丢掉会改变语义)。它之前的全部视为服务端已持有的历史,不再重传。
///
/// `task`(工具回路中间轮的用户原始请求复述)照旧附上 —— 那条杠杆治的是
/// 「本轮只有工具返回、一个问题都没有」,与历史在谁手里无关。
pub(super) fn delta_history(turns: &[Turn], task: Option<&str>) -> Vec<Turn> {
    let Some(last) = turns.iter().rposition(|t| t.is_user) else {
        // 没有 user 轮:退回折叠(它对这种畸形输入有兜底),绝不返回空
        // —— `build_frame0` 对空 turns 是 assert,而且空请求上游只回心跳。
        return fold_history(turns, task);
    };
    let mut buf = String::new();
    buf.push_str(&turns[last].text);
    for t in &turns[last + 1..] {
        buf.push_str("\n\n");
        buf.push_str(&t.text);
    }
    if let Some(task) = task {
        push_tool_loop_nudge(&mut buf, task);
    }
    vec![Turn {
        text: buf,
        is_user: true,
    }]
}

/// 工具回路中间轮补的那句提醒的**内置默认正文**(折叠与增量两条路共用,避免两处措辞漂移)。
/// 前面的空行与末尾的「用户的请求是:」由 [`push_tool_loop_nudge`] 补,热配置只管正文。
///
/// 2026-11-09 改写:旧版「还缺信息时才调别的工具;信息够了就直接给出最终回答」把工具说成
/// 只用来**查信息**。编程任务的下一步多半是**动手**(改文件、跑命令),不属于「缺信息」,
/// 模型于是把「接下来我先改 X……」当最终回答写完就 end_turn —— 用户看到的「说完要做什么
/// 就停了」(生产样本 6391176:工具结果轮、折叠重铺,回复一段计划后收尾)。
/// 「不要重复调用已返回结果的工具」那半句保留(当初治 grok 连读同一文件 9 次)。
pub const DEFAULT_TOOL_LOOP_NUDGE: &str = "[继续]上面是你上一步调用的工具返回的结果,已经拿到了。\
请据此继续推进用户的任务:下一步需要读文件、改文件或运行命令时,直接调用对应的工具去做,\
不要只把打算做的事写成文字就结束这一轮;不要用相同参数重复调用已经返回过结果的工具。\
只有任务确实已经完成,或者必须由用户来决定、提供信息或亲自操作时,才用文字收尾。\
(下面这句原始请求只是提醒你目标,你多半已经回应过它,不要再重复致意或确认。)";

/// 热配置的提醒正文上限(与 admin 写侧共用 gw-core 的常量)。
pub(super) const TOOL_LOOP_NUDGE_MAX_CHARS: usize = gw_core::config::CURSOR_TOOL_LOOP_NUDGE_MAX_CHARS;

/// 提醒正文(进程级全局,worker 30s 设置环热应用)。`None` = 用 [`DEFAULT_TOOL_LOOP_NUDGE`]。
pub(super) static TOOL_LOOP_NUDGE: std::sync::RwLock<Option<std::sync::Arc<str>>> =
    std::sync::RwLock::new(None);

/// 当前生效的提醒正文。
pub fn tool_loop_nudge() -> std::sync::Arc<str> {
    let snap = TOOL_LOOP_NUDGE.read().ok().and_then(|g| g.clone());
    snap.unwrap_or_else(|| std::sync::Arc::from(DEFAULT_TOOL_LOOP_NUDGE))
}

/// 热应用提醒正文(`cursor_tool_loop_nudge`)。空 = 回内置默认;过长返回 `Err` 并
/// **保留上一份有效值**(语义同 `set_tool_guard_policy`)。
pub fn set_tool_loop_nudge(text: &str) -> Result<(), String> {
    let trimmed = text.trim();
    let n = trimmed.chars().count();
    if n > TOOL_LOOP_NUDGE_MAX_CHARS {
        return Err(format!(
            "工具回路提醒过长({n} 字符,上限 {TOOL_LOOP_NUDGE_MAX_CHARS})"
        ));
    }
    let next = (!trimmed.is_empty()).then(|| std::sync::Arc::from(trimmed));
    match TOOL_LOOP_NUDGE.write() {
        Ok(mut g) => {
            *g = next;
            Ok(())
        }
        Err(_) => Err("工具回路提醒锁已中毒".to_string()),
    }
}

/// 当前提醒正文的短指纹(给 `/health` 回显,只报指纹不报全文)。
pub fn tool_loop_nudge_rev() -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(tool_loop_nudge().as_bytes());
    h.finalize()[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// 把提醒(前置空行 + 正文 + 「用户的请求是:」)与原始请求接到 `buf` 末尾。
pub(super) fn push_tool_loop_nudge(buf: &mut String, task: &str) {
    buf.push_str("\n\n");
    buf.push_str(&tool_loop_nudge());
    buf.push_str("\n用户的请求是:");
    buf.push_str(task);
}

/// 串行化「改工具回路提醒」的测试(进程级全局,理由同 `GUARD_TEST_LOCK`)。
#[cfg(test)]
pub(crate) static NUDGE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 把多轮历史折成**一条**用户消息(服务端**没有**历史时的兜底形态)。
///
/// ## 为什么必须折
///
/// 实测:请求里只要有多于一条 `1.2.1`,上游就 200 接受、然后永远只发心跳
/// (与缺 `1.2.1.2` 时一模一样的静默挂起)。真客户端**从来只发一条** ——
/// 它的历史在服务端。所以「repeated `1.2.1` 携带历史」这条路不存在。
///
/// Anthropic 客户端每次都重传全量历史,我们只能把它渲染成一段文字塞进当前这轮。
/// 代价是上游看到的是「一条很长的用户消息」而不是结构化对话,但**模型确实读得到**,
/// 这比静默挂起好得多。要真正避开这个形状只能让服务端持有历史,见 [`delta_history`]。
///
/// ## `task` 参数:折叠为什么会让工具回路发散
///
/// 2026-08-07 实测事故:grok 在 opencode 里对同一个文件连调 **9 次** `read`。
/// 原因不在工具解析(参数解得出来、结果也确实进了上下文),而在折叠的形状 ——
/// 工具回路的第二轮,Anthropic 请求的最后一条 user 消息**只有 tool_result**,
/// 于是折完之后本轮的用户消息是一段裸的 `[工具返回]…`,**一个问题都没有**,
/// 原始请求被埋进 `<conversation_history>` 里。模型看到「工具返回了但没人问我什么」,
/// 最合理的动作就是再调一次工具。每轮请求体只涨 1.2KB(多攒的那一对调用/返回),
/// 看起来像死循环。
///
/// 所以本轮消息只有工具返回时,`task` 传入**用户最近一次真实提问**,在工具返回后面
/// 补一句「据此继续,别重复调用」。系统提示能改行为是实证过的(见
/// [`builtin_tool_guard`]),这里用同一个杠杆。
pub(crate) fn fold_history(turns: &[Turn], task: Option<&str>) -> Vec<Turn> {
    if turns.len() <= 1 && task.is_none() {
        return turns.to_vec();
    }
    let last = turns
        .iter()
        .rposition(|t| t.is_user)
        .unwrap_or(turns.len() - 1);
    let mut buf = String::new();
    if last > 0 {
        buf.push_str("<conversation_history>\n");
        for t in &turns[..last] {
            buf.push_str(if t.is_user { "User: " } else { "Assistant: " });
            buf.push_str(&t.text);
            buf.push_str("\n\n");
        }
        buf.push_str("</conversation_history>\n\n");
    }
    buf.push_str(&turns[last].text);
    // 末尾若还有 assistant(prefill),原样附在后面 —— 丢掉会改变语义。
    for t in &turns[last + 1..] {
        buf.push_str("\n\n");
        buf.push_str(&t.text);
    }
    if let Some(task) = task {
        push_tool_loop_nudge(&mut buf, task);
    }
    vec![Turn {
        text: buf,
        is_user: true,
    }]
}

/// 最后一条 user 消息是否**只装 tool_result**(工具回路的中间轮)。
///
/// 判据是块类型而不是折叠后的文本:嗅自己拼出来的 `[工具返回]` 前缀能work,
/// 但那是把渲染格式变成了协议,改一个字就静默失效。
pub(super) fn last_user_is_tool_result_only(body: &Value) -> bool {
    let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) else {
        return false;
    };
    let Some(last) = msgs
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) != Some("assistant"))
    else {
        return false;
    };
    let Some(blocks) = last.get("content").and_then(|c| c.as_array()) else {
        return false;
    };
    !blocks.is_empty()
        && blocks
            .iter()
            .all(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
}

/// 末条 user(tool_result 轮)紧邻的上一条消息是否是带 tool_use 的 assistant。
///
/// 影子表 tool_turn 条目的宽松匹配只校验指纹链、不校验内容 —— 调用方若把中间那次
/// assistant 工具调用编辑掉,链照样匹配,续轮就会把「服务端手里的那次调用」和
/// 「客户端历史里的另一段」拧在一起。结构对不上的结果轮不许进 CLI 续轮
/// (回 IDE 重铺,宁可慢不错续)。
pub(super) fn prev_assistant_has_tool_use(body: &Value) -> bool {
    let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) else {
        return false;
    };
    let mut seen_tail_user = false;
    for m in msgs.iter().rev() {
        let role = m.get("role").and_then(|r| r.as_str());
        if !seen_tail_user {
            if role == Some("user") {
                seen_tail_user = true;
            }
            continue;
        }
        return role == Some("assistant")
            && m
                .get("content")
                .and_then(|c| c.as_array())
                .is_some_and(|bs| {
                    bs.iter()
                        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                });
    }
    false
}

/// 用户**最近一次真实提问**(跳过只装 tool_result 的中间轮)。
///
/// 取最近而不是最初:多轮对话里用户会改主题,拿第一条会把模型带回上个话题。
pub(super) fn latest_real_user_request(body: &Value) -> Option<String> {
    let msgs = body.get("messages").and_then(|m| m.as_array())?;
    for m in msgs.iter().rev() {
        if m.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            continue;
        }
        let content = m.get("content")?;
        // 只装 tool_result 的轮次不是提问,跳过。
        if let Some(blocks) = content.as_array() {
            if !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
            {
                continue;
            }
        }
        let text = extract_text(content);
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}
