//! 回包改写:把模型调用的 CC 工具(或 MCP 改名后的工具)改回客户端自己的名字与参数。
//!
//! 工具参数是一段段 `input_json_delta` 流过来的,半截 JSON 没法翻译,所以按块缓冲:
//! 遇到需要改写的 `tool_use` 的 `content_block_start` 就先扣下,把它的 `input_json_delta`
//! 攒到 `content_block_stop`,整段解析、换名换参数,再一次性发出「start + 一个完整 delta + stop」。
//! 文本、思考块和不需要改写的工具调用原样直通,不增加延迟。
//!
//! 缓冲只在单个回包流内部(映射表由本次请求的工具列表算出),不需要跨请求或跨进程的状态。
//!
//! 缓冲期间每隔 [`PING_EVERY`] 往外发一个 `ping`(Anthropic 流里本来就有,客户端忽略):worker
//! 只按 provider 有没有吐东西判断上游是否还活着(`STREAM_IDLE_ABORT` 300 秒),模型写一个大文件时
//! 参数能连续流好几分钟,一直扣着不发会被当成卡死掐断(对抗审查 10-30)。
//! 名字不在映射表里(模型臆造了一个工具)或参数解析失败时,按原样发出并记一条告警 ——
//! 客户端会把「工具不存在 / 参数不对」回给模型,模型自己能纠正,比整段回包报错好。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::tools::ToolMap;

struct Pending {
    start: Value,
    json: String,
}

/// 缓冲期间多久发一次 ping(远小于 worker 的 300 秒空闲上限)。
const PING_EVERY: Duration = Duration::from_secs(10);

pub(crate) struct ReverseMapper {
    map: Arc<ToolMap>,
    /// 按块下标缓冲(同一回包里可能有多个工具调用)。
    pending: BTreeMap<u64, Pending>,
    /// 上一次往外发事件的时间。
    last_out: Instant,
    ping_every: Duration,
}

type Event = (String, Value);

fn index_of(data: &Value) -> Option<u64> {
    data.get("index").and_then(Value::as_u64)
}

impl ReverseMapper {
    pub(crate) fn new(map: Arc<ToolMap>) -> Self {
        Self::with_ping_every(map, PING_EVERY)
    }

    fn with_ping_every(map: Arc<ToolMap>, ping_every: Duration) -> Self {
        Self { map, pending: BTreeMap::new(), last_out: Instant::now(), ping_every }
    }

    /// 喂一个 SSE 事件,返回要发给客户端的事件(缓冲中返回空,或者到点了返回一个 ping)。
    pub(crate) fn on_event(&mut self, event: String, data: Value) -> Vec<Event> {
        let mut out = self.route(event, data);
        if out.is_empty() && self.last_out.elapsed() >= self.ping_every {
            out.push(("ping".into(), json!({"type": "ping"})));
        }
        if !out.is_empty() {
            self.last_out = Instant::now();
        }
        out
    }

    fn route(&mut self, event: String, data: Value) -> Vec<Event> {
        match data.get("type").and_then(Value::as_str).unwrap_or(event.as_str()) {
            "content_block_start" => {
                let block = data.get("content_block");
                let is_tool_use = block.and_then(|b| b.get("type")).and_then(Value::as_str) == Some("tool_use");
                let name = block.and_then(|b| b.get("name")).and_then(Value::as_str).unwrap_or("");
                match index_of(&data) {
                    Some(i) if is_tool_use && self.map.needs_backward(name) => {
                        self.pending.insert(i, Pending { start: data, json: String::new() });
                        Vec::new()
                    }
                    _ => vec![(event, data)],
                }
            }
            "content_block_delta" => {
                let partial = data
                    .get("delta")
                    .filter(|d| d.get("type").and_then(Value::as_str) == Some("input_json_delta"))
                    .and_then(|d| d.get("partial_json"))
                    .and_then(Value::as_str);
                match (index_of(&data).and_then(|i| self.pending.get_mut(&i)), partial) {
                    (Some(p), Some(chunk)) => {
                        p.json.push_str(chunk);
                        Vec::new()
                    }
                    _ => vec![(event, data)],
                }
            }
            "content_block_stop" => match index_of(&data).and_then(|i| self.pending.remove(&i).map(|p| (i, p))) {
                Some((i, p)) => self.release(i, p, Some((event, data))),
                None => vec![(event, data)],
            },
            // 流在工具调用中途就收尾了(上游截断):先把攒着的原样放出去,再发收尾事件。
            "message_delta" | "message_stop" | "error" => {
                let mut out = self.finish();
                out.push((event, data));
                out
            }
            _ => vec![(event, data)],
        }
    }

    /// 流结束(或中断)时调用:还没等到 stop 的工具调用按原样放出去,不吞掉。
    pub(crate) fn finish(&mut self) -> Vec<Event> {
        let pending = std::mem::take(&mut self.pending);
        let mut out = Vec::new();
        for (i, p) in pending {
            out.extend(raw(i, p, None));
        }
        out
    }

    fn release(&self, index: u64, p: Pending, stop: Option<Event>) -> Vec<Event> {
        let name = p.start.pointer("/content_block/name").and_then(Value::as_str).unwrap_or("").to_string();
        let parsed: Result<Value, _> = if p.json.trim().is_empty() { Ok(json!({})) } else { serde_json::from_str(&p.json) };
        let mapped = parsed.ok().and_then(|input| self.map.backward(&name, &input));
        let Some((client_name, args)) = mapped else {
            tracing::warn!(tool = %name, "dario CC 回包:工具调用改不回客户端形态,按原样发出");
            return raw(index, p, stop);
        };
        let mut start = p.start;
        start["content_block"]["name"] = Value::String(client_name);
        let delta = json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "input_json_delta", "partial_json": args.to_string()},
        });
        let stop = stop.unwrap_or_else(|| ("content_block_stop".into(), json!({"type": "content_block_stop", "index": index})));
        vec![("content_block_start".into(), start), ("content_block_delta".into(), delta), stop]
    }
}

/// 原样放出:start + 攒下的整段参数(合成一个 delta)+ stop(没有就不补,保持上游原状)。
fn raw(index: u64, p: Pending, stop: Option<Event>) -> Vec<Event> {
    let mut out = vec![("content_block_start".to_string(), p.start)];
    if !p.json.is_empty() {
        out.push((
            "content_block_delta".into(),
            json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": p.json}}),
        ));
    }
    out.extend(stop);
    out
}

#[cfg(test)]
mod tests {
    use super::super::template::test_template;
    use super::super::tools::{build, Mode, OPENCODE};
    use super::*;

    fn mapper() -> ReverseMapper {
        let tpl = test_template();
        let tools = vec![
            json!({"name": "read", "input_schema": {"properties": {"path": {}, "offset": {}}}}),
            json!({"name": "todowrite", "input_schema": {"properties": {"todos": {}}}}),
            json!({"name": "mcp__x__y", "input_schema": {"properties": {}}}),
        ];
        ReverseMapper::new(Arc::new(build(Mode::Table(&OPENCODE), &tools, &tpl).unwrap().map))
    }

    fn ev(data: Value) -> Event {
        (data["type"].as_str().unwrap().to_string(), data)
    }

    fn start(i: u64, name: &str) -> Event {
        ev(json!({"type": "content_block_start", "index": i, "content_block": {"type": "tool_use", "id": format!("toolu_{i}"), "name": name, "input": {}}}))
    }

    fn delta(i: u64, s: &str) -> Event {
        ev(json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": s}}))
    }

    fn stop(i: u64) -> Event {
        ev(json!({"type": "content_block_stop", "index": i}))
    }

    fn feed(m: &mut ReverseMapper, events: Vec<Event>) -> Vec<Event> {
        let mut out = Vec::new();
        for (e, d) in events {
            out.extend(m.on_event(e, d));
        }
        out.extend(m.finish());
        out
    }

    #[test]
    fn args_split_across_frames_are_reassembled_and_mapped_back() {
        let mut m = mapper();
        let out = feed(&mut m, vec![start(1, "Read"), delta(1, "{\"file_pa"), delta(1, "th\": \"/a.rs\", \"off"), delta(1, "set\": 3}"), stop(1)]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].1["content_block"]["name"], "read");
        let args: Value = serde_json::from_str(out[1].1["delta"]["partial_json"].as_str().unwrap()).unwrap();
        assert_eq!(args, json!({"path": "/a.rs", "offset": 3}));
        assert_eq!(out[2].0, "content_block_stop");
    }

    #[test]
    fn text_thinking_and_unmapped_tools_pass_straight_through() {
        let mut m = mapper();
        let text = ev(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "好"}}));
        let thinking = ev(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}));
        assert_eq!(m.on_event(thinking.0.clone(), thinking.1.clone()), vec![thinking]);
        assert_eq!(m.on_event(text.0.clone(), text.1.clone()), vec![text]);
        // 原样路线的 mcp__ 工具不缓冲。
        let s = start(2, "mcp__x__y");
        assert_eq!(m.on_event(s.0.clone(), s.1.clone()), vec![s]);
    }

    #[test]
    fn several_tool_calls_interleaved_with_text() {
        let mut m = mapper();
        let out = feed(
            &mut m,
            vec![
                ev(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})),
                start(1, "mcp__workspace__todowrite"),
                delta(1, "{\"todos\": []}"),
                stop(1),
                start(2, "Read"),
                delta(2, "{\"file_path\": \"/b\"}"),
                stop(2),
                ev(json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}})),
            ],
        );
        let names: Vec<&str> = out.iter().filter_map(|(_, d)| d.pointer("/content_block/name").and_then(Value::as_str)).collect();
        assert_eq!(names, ["todowrite", "read"]);
        assert_eq!(out.last().unwrap().0, "message_delta");
    }

    #[test]
    fn stream_cut_mid_tool_call_releases_raw_and_unknown_tool_stays_raw() {
        let mut m = mapper();
        let out = feed(&mut m, vec![start(1, "Read"), delta(1, "{\"file_pa")]);
        assert_eq!(out.len(), 2, "没等到 stop:start + 攒下的半截参数原样放出,不吞");
        assert_eq!(out[0].1["content_block"]["name"], "Read");
        // 解析失败:原样放出(名字不改)。
        let mut m = mapper();
        let out = feed(&mut m, vec![start(1, "Read"), delta(1, "{oops"), stop(1)]);
        assert_eq!(out[0].1["content_block"]["name"], "Read");
        assert_eq!(out[1].1["delta"]["partial_json"], "{oops");
    }

    #[test]
    fn long_buffering_emits_pings_so_the_worker_does_not_think_upstream_stalled() {
        let tpl = test_template();
        let tools = vec![json!({"name": "read", "input_schema": {"properties": {"path": {}}}})];
        let map = Arc::new(build(Mode::Table(&OPENCODE), &tools, &tpl).unwrap().map);
        let mut m = ReverseMapper::with_ping_every(map, Duration::ZERO);
        let s = start(1, "Read");
        assert_eq!(m.on_event(s.0, s.1)[0].0, "ping");
        let d = delta(1, "{\"file_path\": \"/a\"}");
        assert_eq!(m.on_event(d.0, d.1)[0].0, "ping");
        let st = stop(1);
        let out = m.on_event(st.0, st.1);
        assert_eq!(out.len(), 3, "攒齐后照常发 start + delta + stop");
        assert_eq!(out[0].1["content_block"]["name"], "read");
    }

    #[test]
    fn empty_args_become_empty_object() {
        let mut m = mapper();
        let out = feed(&mut m, vec![start(1, "mcp__workspace__todowrite"), stop(1)]);
        assert_eq!(out[1].1["delta"]["partial_json"], "{}");
    }
}
