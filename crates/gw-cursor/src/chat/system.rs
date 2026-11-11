//! 客户端 system 提取、中段 system 消息分流、思考/档位读取。
//!
//! 2026-11-10 由 chat.rs 拆出(只搬不改行为)。

use super::*;

/// 客户端是否要求透传思考块。
///
/// - `enabled` / `adaptive`(Claude Code):要透传。
/// - `disabled` / 缺省 / 未知:不透传(只影响**发不发 thinking 块**;
///   思考帧无论透不透传都刷新 stall 计时,见流循环里 last_progress 的注释)。
pub(crate) fn client_wants_thinking(thinking: Option<&Value>) -> bool {
    matches!(
        thinking
            .and_then(|t| t.get("type"))
            .and_then(|t| t.as_str()),
        Some("enabled" | "adaptive")
    )
}

/// 从 Anthropic content(string 或 block 数组)抽出纯文本。
pub(crate) fn extract_text(content: &Value) -> String {
    extract_text_in_msg(content, None)
}

/// [`extract_text`] 的占位感知版。`media` = `(消息下标, to_media 的占位表)`:
/// 只有 [`to_turns_with_media`] 带着它 —— tool_result **内嵌**的 image/document
/// 在文本流里留下 `[图片见附件 attach-N]` 占位,编号与 [`to_media`] 收集顺序
/// 严格一致(占位表与附件出自同一次扫描,不是两边各数各的)。
/// system / 亲和键等其它调用点不带表,行为与旧版逐字相同。
pub(super) fn extract_text_in_msg(content: &Value, media: Option<(usize, &MediaPlaceholders)>) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut out = String::new();
            for (bi, b) in blocks.iter().enumerate() {
                // 工具块渲染成文本。**这就是工具回路的闭合方式**:
                // 我们把历史折成一条消息(见 fold_history),所以调用方回传的
                // tool_use / tool_result 也只能以文字形态进上下文。上游那条
                // 「请求侧 field 2 帧」的中途回传通道我们不用 —— 那要求把流一直
                // 挂着等调用方执行完,而反代是一问一答的。
                let kind = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let rendered = match kind {
                    "text" => b.get("text").and_then(|t| t.as_str()).map(str::to_string),
                    "tool_use" => {
                        let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                        let input = b.get("input").cloned().unwrap_or(Value::Null);
                        Some(format!("[调用工具 {name},参数 {input}]"))
                    }
                    "tool_result" => {
                        // content 可能是字符串,也可能是块数组。块数组逐块渲染:
                        // text 原样,**内嵌 image/document 留占位文本**指向 to_media
                        // 收进的真附件。曾经这里把媒体块丢给 `_ => None`,注释声称
                        // 「它们走 to_media 内联」,而 to_media 只扫顶层块 —— 两边
                        // 互相推诿,工具返回的图片就静默消失了(2026-08-13 生产事故)。
                        let c = b.get("content").cloned().unwrap_or(Value::Null);
                        let body = match &c {
                            Value::String(s) => s.clone(),
                            Value::Array(nested) => {
                                let mut nb = String::new();
                                for (ni, n) in nested.iter().enumerate() {
                                    let nk = n.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                    let piece = match nk {
                                        "text" => n
                                            .get("text")
                                            .and_then(|t| t.as_str())
                                            .map(str::to_string),
                                        // 占位表里没有 = 该块没被收(非 base64 / 超限 /
                                        // 调用点不带表),沿旧行为不渲染,编号不空洞。
                                        "image" | "document" => media
                                            .and_then(|(mi, map)| map.get(&(mi, bi, ni)).cloned()),
                                        // CC 工具搜索的延迟工具引用:渲染成占位文本,
                                        // 别让模型看不到它引用了哪个工具。
                                        "tool_reference" => Some(format!(
                                            "[引用工具 {}]",
                                            n.get("tool_name")
                                                .and_then(|t| t.as_str())
                                                .unwrap_or("?")
                                        )),
                                        _ => None,
                                    };
                                    if let Some(p) = piece {
                                        if !nb.is_empty() {
                                            nb.push('\n');
                                        }
                                        nb.push_str(&p);
                                    }
                                }
                                nb
                            }
                            other => extract_text(other),
                        };
                        let err = b.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                        Some(format!(
                            "[工具{}返回]\n{body}",
                            if err { "出错" } else { "" }
                        ))
                    }
                    // CC 工具搜索的延迟工具引用(顶层块):渲染成一行占位文本。
                    "tool_reference" => Some(format!(
                        "[引用工具 {}]",
                        b.get("tool_name").and_then(|t| t.as_str()).unwrap_or("?")
                    )),
                    // 顶层 image / document 不进文本 —— 它们走 `to_media` 内联成真附件
                    // (图片 → 1.2.1.1.3,文档 → 1.2.1.2.20),渲染成文字反而会丢内容。
                    _ => None,
                };
                if let Some(t) = rendered {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&t);
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// 取顶层 `system` 文本,并**剥掉每请求都变的滚动指纹行**。
///
/// ## 为什么这一行剥不掉就什么都不对
///
/// Claude Code 在 system 顶部拼一行
/// `x-anthropic-billing-header: …; cch=<5位16进制>;`,`cch` 是**每请求都变**的 body 哈希
/// (详见 [`gw_core::normalize::strip_rolling_fingerprints`])。而本函数的产物同时喂给
/// **三个**地方:
///
/// 1. [`affinity_key_from_body`] —— 会话亲和键 / 上游 `conversation_id` 的来源;
/// 2. `1.2.1.2.25`(`DET_SYSTEM_PROMPT`)—— 发给上游的系统提示,prefix cache 的前缀;
/// 3. [`crate::cache_sim`] 的指纹 —— 命中率统计。
///
/// 不剥的连锁后果(三条都表现为「莫名其妙就是不命中」):
///
/// - 会话键每请求都变 → 调度层的账号钉扎失效、来回换号;
/// - `conversation_id` 每轮都变 → [`crate::ConvRegistry::phase_for`] 永远拿不到
///   `Continuation`,每轮都按首轮形态发(环境块/预算表按 `Opening` 走);
/// - 缓存指纹每轮都变 → 报出来的命中率恒为 0。
///
/// ⚠️ **它治不了「模型绕圈重复」**。那个症状的成因是 [`fold_history`] 把历史折成一条
/// 假对话记录,而折叠是**无条件**执行的 —— `ctx.phase` 只传给 `build_frame0` 决定环境块,
/// 从不决定要不要折(见 `chat_stream` 里 `fold_history` 的调用点)。也就是说即便
/// `Continuation` 命中,上游收到的仍是那条长文本。要治绕圈得单独改折叠这条路。
/// (第一版注释在这里把两件事连成了因果,是错的;对抗评审 Skeptic#2 指出。)
///
/// gw-kiro 早有这道处理(`converter::normalize`),cursor 这边一直没有 —— 同一份实现
/// 现已上收到 gw-core,两边共用,不会再各自漂移。
pub(crate) fn extract_system(body: &Value) -> String {
    let raw = body.get("system").map(extract_text).unwrap_or_default();
    gw_core::normalize::strip_rolling_fingerprints(&raw)
}

/// `SessionStart` hook 注入的稳定前缀标记(跨轮不变,值得提升进 system)。
pub(super) const SESSION_START_PREFIX: &str = "SessionStart hook additional context:";
/// Claude Code / Agent SDK 的身份行:出现即说明这条 system 消息是稳定前缀。
pub(super) const CC_IDENTITY_LINE: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
pub(super) const SDK_IDENTITY_LINE: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
/// 用户中途插话被中转框成 system 消息时的固定开头。
pub(super) const INTERRUPTED_USER_PREFIX: &str = "The user sent a new message while you were working:";

/// 稳定 system 前缀:跨轮不变,提升进顶层 `system` 反而利于前缀缓存。
pub(super) fn is_stable_system_prefix(text: &str) -> bool {
    let head = text.trim_start();
    head.starts_with(SESSION_START_PREFIX)
        || text
            .lines()
            .map(str::trim_start)
            .any(|l| l == CC_IDENTITY_LINE || l == SDK_IDENTITY_LINE)
}

/// **每轮都变、对模型零信息增量**的注入,丢弃。
///
/// 这里的判据必须精确到「整条消息就是这个东西」:一旦放宽成子串匹配,用户正文里
/// 提到 `<total_tokens>` 就会让半条消息凭空消失。
pub(super) fn is_dynamic_system_noise(text: &str) -> bool {
    let t = text.trim();
    // ① 剩余预算计数器:`<total_tokens>15000000 tokens left</total_tokens>`。
    //    **它就是本次事故的元凶**(见 route_system_role_messages 的文档)。
    (t.starts_with("<total_tokens>") && t.ends_with("</total_tokens>"))
        // ② 「最近没用 task 工具」的周期性催促。
        || t.starts_with("The task tools haven't been used recently.")
}

/// interrupted-user 特例:实为用户插话,被中转框成了 system。取出正文当 user。
pub(super) fn interrupted_user_payload(text: &str) -> Option<String> {
    let body = text.trim_start().strip_prefix(INTERRUPTED_USER_PREFIX)?;
    let payload = body
        .split_once("\n\nIMPORTANT:")
        .map_or(body, |(p, _)| p)
        .trim();
    (!payload.is_empty()).then(|| payload.to_string())
}

/// 把 `messages[]` 里**代理链中段注入的 `role:"system"` 消息**分流掉,原地改写 `body`。
///
/// ## 这道处理不做的后果(2026-08-17 生产事故,用户报障原话「grok 收到空消息」)
///
/// 真实流量里,客户端(经中转)会在**每条用户消息之后**再追一条
/// `{"role":"system","content":"<total_tokens>15000000 tokens left</total_tokens>"}`。
/// 而 [`to_turns`] 判 `is_user` 用的是 `role != "assistant"` —— 于是这条计数器成了
/// **最后一轮**。两条路径同时被带偏:
///
/// 1. **CLI 驱动(clidrv,2026-11-10 已删除)的 prompt 只发最后一轮**,发出去的
///    整条 prompt 就是那行计数器。grok 的 thinking 原文:
///    "The user's message contains only a token count indicator." —— 用户看到的是
///    「你这条消息是空的」,而他明明打了一大段。
/// 2. 工具结果轮的判定(当年 CLI 驱动的 `last_tool_results`,现为 [`last_user_is_tool_result_only`])要求末条是用户侧,尾巴是 system 就认不出
///    → 挂起的桥调用**永远接不上** → 模型反复说「MCP 读取被中断了」,
///    并伴随 90s stall 与 `incomplete_stream`。
///
/// 线协议那条路侥幸没露:[`fold_history`] 无条件全量重铺,尾巴上多一行计数器无伤。
/// 所以症状是「切了 CLI 驱动才开始空」,很容易误判成 CLI 驱动本身坏了。
///
/// ## 四级分流
///
/// | 类别 | 处置 | 理由 |
/// |---|---|---|
/// | 稳定前缀(hook / 身份行) | 提升进顶层 `system` | 跨轮不变,进 system 才吃得到前缀缓存 |
/// | 动态噪声(预算计数器等) | 丢弃 | 每轮都变、零信息增量,留着只会毒化尾轮与指纹 |
/// | interrupted-user | 转 `role:"user"` 原位保留 | 那本来就是用户说的话 |
/// | 其余未知 | 裹 `<system_context>` 转 user 原位保留 | 不认识就不敢丢:保内容、保位置语义 |
///
/// 空 system 消息直接丢。**无 system-role 消息时一个字节都不改**(绝大多数流量的快路径)。
///
/// gw-kiro 早有同一道处理(`converter::normalize::route_system_role_messages`,注释里
/// 写着「代理链中段注入」),cursor 通道一直没有。**没有把实现上收到 gw-core 共用**是
/// 刻意的:kiro 那份跑在 `Vec<Message>` 强类型上,共用要动 kiro 的转换管线,而 kiro 是
/// 生产主力面;这份跑在裸 `Value` 上,且多一条 kiro 不需要的规则(动态噪声里的预算
/// 计数器)。两份的分类口径若要合并,应作为一次独立改动、单独回归 kiro。
pub(crate) fn route_system_role_messages(body: &mut Value) {
    let has_system_role = body
        .get("messages")
        .and_then(|m| m.as_array())
        .is_some_and(|msgs| {
            msgs.iter()
                .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
        });
    if !has_system_role {
        return; // 快路径:原样,零拷贝。
    }
    let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };

    let mut out: Vec<Value> = Vec::with_capacity(msgs.len());
    let mut promoted: Vec<String> = Vec::new();
    for m in msgs.drain(..) {
        if m.get("role").and_then(|r| r.as_str()) != Some("system") {
            out.push(m);
            continue;
        }
        let text = gw_core::normalize::strip_rolling_fingerprints(&extract_text(
            m.get("content").unwrap_or(&Value::Null),
        ));
        if text.trim().is_empty() {
            continue;
        }
        if is_stable_system_prefix(&text) {
            promoted.push(text);
        } else if let Some(payload) = interrupted_user_payload(&text) {
            out.push(json!({"role": "user", "content": payload}));
        } else if is_dynamic_system_noise(&text) {
            // 丢弃
        } else {
            out.push(json!({
                "role": "user",
                "content": format!("<system_context>\n{text}\n</system_context>"),
            }));
        }
    }
    *msgs = out;

    if !promoted.is_empty() {
        // 提升的文本接在既有 system 之后。**这里把 system 拍平成字符串是安全的**:
        // 本 crate 只有 `extract_system` 读它,而那本来就是拍平取文本
        // (grep `get("system")` 全仓仅一处)。
        let mut sys = body.get("system").map(extract_text).unwrap_or_default();
        for p in promoted {
            if !sys.is_empty() {
                sys.push_str("\n\n");
            }
            sys.push_str(&p);
        }
        body["system"] = Value::String(sys);
    }
}
