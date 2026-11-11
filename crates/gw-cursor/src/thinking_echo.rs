//! 我方下发过的思考文本的回声表:防止它被客户端当成普通正文回传、再被我们折进历史。
//!
//! ## 为什么需要
//!
//! 线协议拿不到 Anthropic 的思考签名,我们下发的 thinking 块**没有 signature**。
//! OpenCode(AI SDK)这类客户端回传历史时,对不带签名的推理块只能降级成普通 `text`
//! 块。于是下一轮的 assistant 消息里出现一段模型自己的推理原文,我们照常把它折进历史
//! 发上游 —— 等于把模型的推理当成对话正文喂回给它。Anthropic 的分类器会把这判成
//! 「推理提取」,时有时无地拦截(2026-11-07 e2e 实证:trailer
//! `Request blocked by Anthropic … Usage Policy`,fable-5.1 工具回路第二轮零产出)。
//!
//! ## 做法
//!
//! 内容寻址:每条响应下发完的思考文本记 sha256(去首尾空白);下一轮转换历史时,
//! assistant 消息里**整块等于**某条记录的 `text` 块就地置空(不删块,块下标要与媒体
//! 占位表对齐)。只认我方下发过的原文,客户自己写的内容一个字都不动。
//!
//! 全局一张表、按插入顺序淘汰:哈希 32 字节,上限 [`CAPACITY`] 条约 1MB 量级。
//! 进程重启后表清空,那之后的旧会话退化为原行为(折进历史),不出错。

use std::collections::{HashSet, VecDeque};
use std::sync::{LazyLock, Mutex};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// 表容量。按每轮一条记录算,够覆盖几万轮;超出淘汰最早的。
const CAPACITY: usize = 32_768;

/// 太短的思考不记:几个字的片段与客户正文撞车的概率不可忽略,而它也拦不出什么。
const MIN_CHARS: usize = 16;

struct Table {
    order: VecDeque<[u8; 32]>,
    set: HashSet<[u8; 32]>,
}

static TABLE: LazyLock<Mutex<Table>> = LazyLock::new(|| {
    Mutex::new(Table {
        order: VecDeque::new(),
        set: HashSet::new(),
    })
});

fn key(text: &str) -> Option<[u8; 32]> {
    let t = text.trim();
    if t.chars().count() < MIN_CHARS {
        return None;
    }
    Some(Sha256::digest(t.as_bytes()).into())
}

/// 记一条已下发的思考原文。
pub(crate) fn remember(text: &str) {
    let Some(k) = key(text) else { return };
    let mut t = TABLE.lock().unwrap_or_else(|p| p.into_inner());
    if !t.set.insert(k) {
        return;
    }
    t.order.push_back(k);
    while t.order.len() > CAPACITY {
        if let Some(old) = t.order.pop_front() {
            t.set.remove(&old);
        }
    }
}

/// 这段文本是不是我方下发过的思考原文。
pub(crate) fn is_echo(text: &str) -> bool {
    let Some(k) = key(text) else { return false };
    TABLE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .set
        .contains(&k)
}

/// 把 assistant 消息里回传的思考原文就地置空。没有命中时返回 `None`(调用方用原 body)。
pub(crate) fn strip_from_history(body: &Value) -> Option<Value> {
    let msgs = body.get("messages")?.as_array()?;
    let hit = msgs.iter().any(|m| {
        m.get("role").and_then(Value::as_str) == Some("assistant")
            && m.get("content")
                .and_then(Value::as_array)
                .is_some_and(|bs| bs.iter().any(is_echo_block))
    });
    if !hit {
        return None;
    }
    let mut out = body.clone();
    let mut stripped = 0usize;
    if let Some(ms) = out.get_mut("messages").and_then(Value::as_array_mut) {
        for m in ms.iter_mut() {
            if m.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(bs) = m.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            for b in bs.iter_mut() {
                if is_echo_block(b) {
                    b["text"] = Value::String(String::new());
                    stripped += 1;
                }
            }
        }
    }
    tracing::debug!(stripped, "cursor:历史里回传的思考原文已置空(不折进上游)");
    Some(out)
}

fn is_echo_block(b: &Value) -> bool {
    b.get("type").and_then(Value::as_str) == Some("text")
        && b.get("text").and_then(Value::as_str).is_some_and(is_echo)
}

/// 一条响应里实际下发的思考文本累加器;流结束(任何出口)时落表。
#[derive(Default)]
pub(crate) struct Recorder {
    buf: String,
}

impl Recorder {
    pub(crate) fn push(&mut self, delta: &str) {
        self.buf.push_str(delta);
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            remember(&self.buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 回传的思考原文在assistant里置空_其余不动() {
        // 每条测试用独有文本,表是全局的,避免与并行测试互相影响。
        let thought = "I'm checking the png files next; the tool names look inconsistent (test-a).";
        {
            let mut r = Recorder::default();
            r.push("I'm checking the png files next; ");
            r.push("the tool names look inconsistent (test-a).");
        } // drop → 落表
        let body = json!({"messages":[
            {"role":"user","content":[{"type":"text","text":thought}]},
            {"role":"assistant","content":[
                {"type":"text","text":format!("\n{thought}\n")},
                {"type":"text","text":"我来搜索这两类文件。"},
                {"type":"tool_use","id":"t1","name":"glob","input":{"pattern":"**/*.png"}}
            ]}
        ]});
        let out = strip_from_history(&body).expect("应命中");
        let a = &out["messages"][1]["content"];
        assert_eq!(a[0]["text"], "", "回传的思考原文要置空");
        assert_eq!(a[1]["text"], "我来搜索这两类文件。", "正文不动");
        assert_eq!(a.as_array().unwrap().len(), 3, "块数不变(下标要与媒体占位对齐)");
        assert_eq!(out["messages"][0]["content"][0]["text"], thought, "user 消息不动");
    }

    #[test]
    fn 没下发过的文本与过短片段不命中() {
        let body = json!({"messages":[
            {"role":"assistant","content":[{"type":"text","text":"a sentence nobody recorded (test-b)"}]}
        ]});
        assert!(strip_from_history(&body).is_none());
        remember("short");
        assert!(!is_echo("short"), "过短不记");
        let empty = Recorder::default();
        drop(empty); // 空累加器不落表、不 panic
    }
}
