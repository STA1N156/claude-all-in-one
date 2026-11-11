//! SSE 用量帧与 token 估算(客户账单用的用量兜底)。
//!
//! 2026-11-10 由 chat.rs 拆出(只搬不改行为)。

use super::*;

/// `message_start` 事件。抽出来是因为三条路径都要发它(思考先到 / 正文先到 /
/// 纯工具调用),而漏发会让后面的块没有依附。
pub(super) fn message_start(msg_id: &str, model: &str) -> SseEvent {
    message_start_pub(msg_id, model)
}

/// `pub(crate)`:历史上 CLI 驱动(clidrv,2026-11-10 已删除)也复用它。
pub(crate) fn message_start_pub(msg_id: &str, model: &str) -> SseEvent {
    SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": msg_id, "type": "message", "role": "assistant",
                "model": model, "content": [],
                "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        }),
    )
}

/// 收尾帧(`message_delta`)的 usage JSON。
///
/// **线上口径 = Anthropic 规范**(与 gw-kiro `usage.rs::build_usage_json` 一致):
/// `input_tokens` 只算**未命中缓存的新增部分**,缓存读取单列。上游给的
/// input 是含缓存命中的总上下文,直接透传的话,按 Anthropic 语义解读的下游
/// (Claude Code 的用量显示、new-api 的客户计费)会把缓存部分重复计费。
///
/// 缓存写分两种情形(由调用方按**用量来源**传 `estimated`,不按数字是否为零猜 ——
/// 上游用量帧明说缓存写为 0 时不能当估算):
/// - **用量来自上游用量帧**(`estimated = false`):只认上游给的缓存写,剩下的是普通输入。
/// - **用量是估算的**(`estimated = true`:CLI 形态没有用量帧,或用量帧全零不可信;
///   缓存读来自 [`crate::cache_sim`] 模拟):
///   没命中的部分**全部按 5 分钟档缓存写**上报,`input_tokens` = 0。依据是 Cursor
///   真实账单:2026-11-10 14:46~14:55 全部号读 86.9%、写 10%、普通输入只有个位数
///   token —— Cursor 把缓存断点放在末尾,没命中的部分都会被写进缓存,Anthropic 按
///   输入价 1.25 倍收。以前把这部分当普通输入(1 倍)报,客户少付;new-api 读
///   `message_delta` 里的缓存写(大于 0 才覆盖),`input_tokens` 为 0 时保留
///   `message_start` 的值(也是 0),不会改用本地估算。
///
/// ⚠️ 这只是**给客户端的线上口径**;`StreamItem::Usage` 的入库口径不变
/// (仍是总上下文、缓存写列不填,计价方自己相减,见 `gw_core::pricing` 模块文档),
/// 所以 caio 后台成本统计不会把这部分重复计算。
pub(super) fn delta_usage_json(usage: &ChatUsage, output_tokens: u64, estimated: bool) -> Value {
    delta_usage_json_impl(usage, output_tokens, estimated)
}

pub(super) fn delta_usage_json_impl(usage: &ChatUsage, output_tokens: u64, estimated: bool) -> Value {
    let cache_read = usage.cache_read_tokens.min(usage.input_tokens);
    // 估算时没命中的部分全是缓存写;上游自报时只认上游给的缓存写(见函数文档)。
    let cache_creation = if estimated {
        usage.input_tokens - cache_read
    } else {
        usage.cache_creation_tokens.min(usage.input_tokens - cache_read)
    };
    let uncached_input = usage.input_tokens - cache_read - cache_creation;
    let mut m = serde_json::Map::new();
    m.insert("input_tokens".into(), json!(uncached_input));
    m.insert("output_tokens".into(), json!(output_tokens));
    if cache_read > 0 {
        m.insert("cache_read_input_tokens".into(), json!(cache_read));
    }
    if cache_creation > 0 {
        m.insert("cache_creation_input_tokens".into(), json!(cache_creation));
        if estimated {
            // 估算时明确是 5 分钟档(Anthropic 官方格式),不让下游按自己的默认去猜。
            m.insert(
                "cache_creation".into(),
                json!({"ephemeral_5m_input_tokens": cache_creation, "ephemeral_1h_input_tokens": 0}),
            );
        }
    }
    Value::Object(m)
}

/// 估算用量时,没命中的部分要不要报成缓存写:只有 **Claude 系**(Anthropic 对缓存写按
/// 输入价 1.25 倍收)。grok / gpt / composer 等经 Cursor 的其它模型没有缓存写加价,
/// 照旧按普通输入报(审查意见:否则这些模型的客户被按缓存写价多收)。
pub(super) fn model_has_cache_write_surcharge(model: &str) -> bool {
    model.trim().to_ascii_lowercase().starts_with("claude")
}

/// 文本的粗略 token 估算(计费兜底口径)。
///
/// ASCII ≈ 4 字符/token;非 ASCII 按 1 字符/token 的保守值 —— 中文实测多在
/// 1~1.5 字符/token,emoji/组合字符甚至超过 1 token/字符。这是计费代码,
/// 估不准就宁多勿少(低估 = 运营方贴钱)。
pub(crate) fn est_text_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut other = 0u64;
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    ascii.div_ceil(4) + other
}

/// 上游没给 `1.14` 用量时的回退估算(典型场景:**外部 tool_use 收口**)。
///
/// 反代是一问一答:认出外部工具就 `break` 并把 `tool_use` 交给调用方,不会在同
/// 一条 BiDi 流里等服务端再吐用量(协议文档也写了 tool_use 轮通常不带 `1.14`)。
/// 旧回退只估 `output = chars/4`,于是 new-api / 面板看到 **输入=0、缓存=0**,
/// 只有输出 —— 生产上 fable 近两小时 39/40 条成功请求都是这个形态。
///
/// 本函数**只估 input/output,不估缓存**:cache_read 一律由 [`crate::cache_sim`]
/// 在收尾处统一给(按会话指纹的真实前缀命中模拟,与 kiro 同口径)。早先这里用
/// 「input − 本轮新增」的一次性启发式估缓存,但生产上 `fold_history` 把多轮折成
/// 单条 Turn,`turns.len() > 1` 恒不成立,那段代码实际是死的;而且新会话首轮
/// 粘贴长历史会被它误判成命中。模拟器两条都不沾。
pub(super) fn estimate_usage_fallback(
    system: &str,
    turns: &[run::Turn],
    tools: &[run::ToolDef],
    output_chars: usize,
) -> ChatUsage {
    let input_tokens = est_text_tokens(system)
        + turns.iter().map(|t| est_text_tokens(&t.text)).sum::<u64>()
        + tools
            .iter()
            .map(|t| {
                est_text_tokens(&t.name)
                    + est_text_tokens(&t.description)
                    + est_text_tokens(&t.schema)
            })
            .sum::<u64>();
    let output_tokens = (output_chars as u64).div_ceil(4);

    ChatUsage {
        input_tokens,
        output_tokens,
        // 缓存归模拟器管,这里恒 0(见函数级注释)。
        cache_read_tokens: 0,
        ..Default::default()
    }
}

/// 客户账单用的输入估算:客户端完整上下文(`raw_turns`)与实际上传形态(`turns`)两份取大。
///
/// - 自造历史 / 续轮描述符 / 挂流续跑:`turns` 只剩本轮或最后一条,以完整上下文为准;
/// - 带 PDF 的折叠请求:文字层只拼进 `turns`,以上传形态为准。
pub(super) fn billing_estimate(
    system: &str,
    raw_turns: &[run::Turn],
    turns: &[run::Turn],
    tools: &[run::ToolDef],
) -> ChatUsage {
    let full = estimate_usage_fallback(system, raw_turns, tools, 0);
    let sent = estimate_usage_fallback(system, turns, tools, 0);
    if sent.input_tokens > full.input_tokens {
        sent
    } else {
        full
    }
}

/// 工具调用的「出字量」估算:工具名 + 整个参数对象的 JSON 序列化(token 粗估)。
///
/// tool_use 轮的产出大头就是参数 JSON(纯工具调用轮正文可以是零)——只数正文
/// delta 会得到 output≈0,把工具参数漏出账单。整对象一次序列化,引号/逗号/
///  braces 等结构字符也算进估算(短 key 多时占比不小)。
pub(super) fn tool_call_tokens(tc: &run::ToolCall) -> u64 {
    let args: serde_json::Map<String, Value> = tc.args.iter().cloned().collect();
    let args_tokens = serde_json::to_string(&Value::Object(args))
        .map(|s| est_text_tokens(&s))
        .unwrap_or(0);
    est_text_tokens(&tc.name) + args_tokens
}

/// 把上游 `1.14` 三元组收成 [`ChatUsage`]。
///
/// 上游自报的缓存命中是**真实**命中,`cache_read` 与 `real_cache_read` 同值 ——
/// 漏填 `real_cache_read` 会让面板「真实缓存」列永远是 0。注意 cursor 现在是
/// 双轨口径(与 kiro 同构):收尾处若上游 cached=0,`cache_read` 会被
/// [`crate::cache_sim`] 的模拟值顶替(客户计费列),而 `real_cache_read`
/// 永远只认这里填的上游自报值(对账列)。
pub(super) fn usage_from_upstream(u: crate::run::WireUsage) -> ChatUsage {
    ChatUsage {
        input_tokens: u.input,
        output_tokens: u.output,
        cache_read_tokens: u.cache_read,
        real_cache_read_tokens: u.cache_read,
        // 缓存**写入**(线缆 `1.14.4`)。2026-08-18 取证前这一列全平台恒 0 ——
        // 它对应 Anthropic 的 `cache_creation_input_tokens`,是总输入的一部分。
        cache_creation_tokens: u.cache_write,
        ..Default::default()
    }
}
