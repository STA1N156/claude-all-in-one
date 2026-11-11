//! 翻译用的整理步骤(纯函数,只改传进来的消息)。

use serde_json::{json, Value};

/// 出口合规改写的统计(只记个数,不记内容,用于日志)。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReorderStats {
    /// 改写了几段连续 user 回合。0 = 一个字节都没动。
    pub runs: usize,
    /// 这些段里一共多少个 tool_result(挪到最前)。
    pub tool_results: usize,
    /// 这些段里其余块(图片、文字等)的个数(排到 tool_result 之后)。
    pub other_blocks: usize,
    /// 合并时丢掉的消息级多余键(role/content 以外,如 OpenAI 系中转带的 `name`)个数。
    /// Anthropic 消息本来只有这两个键,多余键上游也不认;计数是为了丢了有迹可查。
    pub dropped_keys: usize,
}

fn is_user(m: &Value) -> bool {
    m.get("role").and_then(Value::as_str) == Some("user")
}

fn is_tool_result(b: &Value) -> bool {
    b.get("type").and_then(Value::as_str) == Some("tool_result")
}

/// 一串消息里有没有「同一段连续 user 回合中,非 tool_result 块排在 tool_result 前面」。
///
/// 段的边界是任何非 user 消息(assistant,或分流后残留的其它角色)。空字符串内容不算块。
fn has_misordered_run(messages: &[Value]) -> bool {
    let mut seen_other = false;
    for m in messages {
        if !is_user(m) {
            seen_other = false;
            continue;
        }
        match m.get("content") {
            Some(Value::String(s)) => {
                if !s.is_empty() {
                    seen_other = true;
                }
            }
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    if is_tool_result(b) {
                        if seen_other {
                            return true;
                        }
                    } else {
                        seen_other = true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// 把一段连续 user 消息合成一条:tool_result 全部在前(保持原顺序),其余块在后(保持原顺序)。
/// 字符串内容转成一个 text 块(空字符串丢掉)。消息上 role/content 以外的键不保留
/// (Anthropic 消息本来只有这两个键),个数记进 `dropped_keys`。
///
/// 段里只会有对象:段成员由 [`is_user`] 判定,非对象取不到 role。
fn merge_run(run: Vec<Value>, stats: &mut ReorderStats) -> Value {
    let mut results: Vec<Value> = Vec::new();
    let mut others: Vec<Value> = Vec::new();
    for m in run {
        let Value::Object(mut obj) = m else { continue };
        let content = obj.remove("content");
        stats.dropped_keys += obj.keys().filter(|k| k.as_str() != "role").count();
        match content {
            Some(Value::String(s)) => {
                if !s.is_empty() {
                    others.push(json!({"type": "text", "text": s}));
                }
            }
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    if is_tool_result(&b) {
                        results.push(b);
                    } else {
                        others.push(b);
                    }
                }
            }
            _ => {}
        }
    }
    stats.runs += 1;
    stats.tool_results += results.len();
    stats.other_blocks += others.len();
    results.extend(others);
    json!({"role": "user", "content": results})
}

/// **出口合规**:每段连续 user 回合里,tool_result 必须排在最前。
///
/// Anthropic 把连续的 user 消息当作同一个回合,并要求 tool_result 排在这个回合的最前面、
/// 紧跟在对应的 tool_use 之后。cursor 编码(`blobhist::encode_message`)按块的原顺序编,
/// 于是「图片 / 文字排在 tool_result 前」会原样变成「两个 tool_result 之间夹一条 user 消息」,
/// 上游提供方回 400(2026-11-10 12:45 事故:客户端把并行 `Read` 的读图结果拆成多条、
/// 图片排在 tool_result 前)。
///
/// 只在确实违规时改写:违规的那一段合成一条 user 消息(tool_result 在前,其余在后);
/// **格式正确的请求一个字节都不动**(快路径不分配)。一段消息后面一旦跟了 assistant 就不会
/// 再变,所以改写结果跨轮稳定,会话指纹与自造历史不会因此分叉。
pub(crate) fn tool_results_first(messages: &mut Vec<Value>) -> ReorderStats {
    let mut stats = ReorderStats::default();
    if !has_misordered_run(messages) {
        return stats;
    }
    let old = std::mem::take(messages);
    let mut out: Vec<Value> = Vec::with_capacity(old.len());
    let mut run: Vec<Value> = Vec::new();
    for m in old {
        if is_user(&m) {
            run.push(m);
            continue;
        }
        flush_run(&mut run, &mut out, &mut stats);
        out.push(m);
    }
    flush_run(&mut run, &mut out, &mut stats);
    *messages = out;
    stats
}

/// 收尾一段:违规就合并改写,否则原样放回。
fn flush_run(run: &mut Vec<Value>, out: &mut Vec<Value>, stats: &mut ReorderStats) {
    if run.is_empty() {
        return;
    }
    let taken = std::mem::take(run);
    if has_misordered_run(&taken) {
        out.push(merge_run(taken, stats));
    } else {
        out.extend(taken);
    }
}
