//! 上游响应流归一:把两种传输都变成同一种「事件帧」流,供 chat 的 Anthropic 状态机复用。
//!
//! - 旧协议(GenerateAssistantResponse):AWS eventstream 二进制帧,原样解码([`aws_events`])。
//! - `/v1/responses`(Kiro IDE 1.4.121 CreateResponse):OpenAI Responses 形态的 SSE,
//!   逐行翻译成旧协议的同名事件([`responses_events`])。
//!
//! 翻译表逐条对齐客户端 `GUs`(extension.js@5717439,2026-11-08 解包),客户端自己也是把
//! SSE 翻成 `assistantResponseEvent` / `reasoningContentEvent` / `toolUseEvent` /
//! `metadataEvent` / `contextUsageEvent` / `meteringEvent` 再交给旧的处理流程,所以下游
//! 状态机一行不用改:
//!
//! | SSE | 翻成 |
//! |---|---|
//! | `response.output_text.delta` | `assistantResponseEvent{content}` |
//! | `response.reasoning_text.delta` / `response.reasoning.delta` | `reasoningContentEvent{text}` |
//! | `response.output_item.done`(reasoning) | 信封里有签名 → `reasoningContentEvent{signature}`;redacted → `{redactedContent}` |
//! | `response.refusal.delta` / `.done` | 当普通正文发(没有 delta 时才用 done 的整串) |
//! | `response.output_item.added`(function_call) | `toolUseEvent` 开始(`call_id ?? id`) |
//! | `response.function_call_arguments.delta` | 归到「当前调用」的参数增量(客户端也不看 item_id) |
//! | `response.function_call_arguments.done` | 当前调用 stop |
//! | `response.output_item.done`(function_call) | 没收过增量就用完整 arguments 补发;stop 暂存到 completed |
//! | `response.completed` | 补发暂存/未关闭的 stop → `metadataEvent{END_TURN}` → 上下文占比 → 积分 |
//! | `response.incomplete` | 不补 stop;`incomplete_details.reason` 映射 stopReason |
//! | `error` / `response.failed` | 直接按错误分类结束流([`crate::error_map::classify_responses_stream_error`]) |
//! | `kiro:keep_alive` | 忽略 |
//!
//! 客户端的 incomplete 原因表(`rRp`)没解出全集,只列了解包里看到的三个,其余按 END_TURN。

use std::collections::{HashSet, VecDeque};
use std::pin::Pin;

use futures::{Stream, StreamExt};
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use serde_json::{json, Value};

use crate::parser::decoder::EventStreamDecoder;

type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;
/// 归一后的事件流。`Err` 表示流已终止(读流失败 / 缓冲溢出 / 上游在流里报错)。
pub type EventStream = Pin<Box<dyn Stream<Item = Result<EventFrame, UpstreamError>> + Send>>;

/// 与 [`crate::parser::frame::Frame`] 同接口的事件帧(旧协议直接转换,新协议合成)。
#[derive(Debug)]
pub struct EventFrame {
    message_type: Option<String>,
    event_type: Option<String>,
    exception_type: Option<String>,
    error_code: Option<String>,
    payload: Vec<u8>,
}

/// 新协议里没有对应旧事件的 SSE(created / in_progress / content_part.* 等)统一用这个名字,
/// 只用来标记「见过上游真实数据」;刻意不沿用原名,免得含 `error` 字样的事件名被状态机
/// 的「异常事件名兜底」误判成上游异常。
pub(crate) const PASSTHROUGH_EVENT: &str = "responsesPassthrough";

impl EventFrame {
    fn event(name: &str, payload: Value) -> Self {
        Self {
            message_type: Some("event".into()),
            event_type: Some(name.into()),
            exception_type: None,
            error_code: None,
            payload: serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    fn exception(name: &str, payload: Value) -> Self {
        Self {
            message_type: Some("exception".into()),
            event_type: None,
            exception_type: Some(name.into()),
            error_code: None,
            payload: serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    pub fn message_type(&self) -> Option<&str> {
        self.message_type.as_deref()
    }

    pub fn event_type(&self) -> Option<&str> {
        self.event_type.as_deref()
    }

    pub fn exception_type(&self) -> Option<&str> {
        self.exception_type.as_deref()
    }

    pub fn error_code(&self) -> Option<&str> {
        self.error_code.as_deref()
    }

    pub fn payload_as_json<T: serde::de::DeserializeOwned>(&self) -> serde_json::Result<T> {
        serde_json::from_slice(&self.payload)
    }

    pub fn payload_as_str(&self) -> String {
        String::from_utf8_lossy(&self.payload).to_string()
    }
}

impl From<crate::parser::frame::Frame> for EventFrame {
    fn from(frame: crate::parser::frame::Frame) -> Self {
        Self {
            message_type: frame.message_type().map(str::to_string),
            event_type: frame.event_type().map(str::to_string),
            exception_type: frame.exception_type().map(str::to_string),
            error_code: frame.error_code().map(str::to_string),
            payload: frame.payload,
        }
    }
}

/// 旧协议:AWS eventstream 解码。行为与原先内联在 chat 里的循环一致 —— 缓冲溢出/读流失败
/// 终止;单帧坏包跳过本批剩余字节、继续读下一批。
pub fn aws_events(
    byte_stream: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
) -> EventStream {
    struct State {
        bytes: ByteStream,
        decoder: EventStreamDecoder,
        ready: VecDeque<EventFrame>,
        done: bool,
    }
    let state = State {
        bytes: Box::pin(byte_stream),
        decoder: EventStreamDecoder::new(),
        ready: VecDeque::new(),
        done: false,
    };
    Box::pin(futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(frame) = state.ready.pop_front() {
                return Some((Ok(frame), state));
            }
            if state.done {
                return None;
            }
            match state.bytes.next().await {
                Some(Ok(bytes)) => {
                    if state.decoder.feed(&bytes).is_err() {
                        state.done = true;
                        return Some((
                            Err(UpstreamError::new(UpstreamErrorKind::Other, "eventstream 缓冲溢出")),
                            state,
                        ));
                    }
                    while let Ok(Some(frame)) = state.decoder.decode() {
                        state.ready.push_back(frame.into());
                    }
                }
                Some(Err(error)) => {
                    state.done = true;
                    return Some((
                        Err(UpstreamError::network(format!("读取上游流失败: {error}"))),
                        state,
                    ));
                }
                None => state.done = true,
            }
        }
    }))
}

/// 单行上限。客户端是 `iRp`(值没解出);这里给 4MiB,远大于任何正常事件。
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default)]
struct ResponsesDecoder {
    buffer: Vec<u8>,
    /// `buffer` 里已确认不含换行的前缀长度:下一批只从这里往后找换行,
    /// 大事件分很多块到达时不必每批从头重扫(否则是平方级)。
    scanned: usize,
    /// 已开始、尚未 stop 的调用(客户端 `n`)。
    open_calls: HashSet<String>,
    /// 参数增量归属的「当前调用」(客户端 `r`)。
    current_call: Option<String>,
    /// 收到过参数增量的调用(客户端 `o`)—— 这些在 output_item.done 时不再用完整 arguments 重放。
    calls_with_delta: HashSet<String>,
    /// 已完成、stop 留到 completed 再发的调用(客户端 `s`)。
    held_stops: Vec<String>,
    refusal_delta_seen: bool,
}

impl ResponsesDecoder {
    /// 喂一批字节,返回能解出的事件,以及(若有)终止本流的错误 —— 上游在流里报错或单行超限。
    /// 错误之前已解出的事件照样交出去(先给客户已生成的部分,再报错)。
    fn feed(&mut self, bytes: &[u8]) -> (Vec<EventFrame>, Option<UpstreamError>) {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(offset) = self.buffer[self.scanned..].iter().position(|b| *b == b'\n') {
            let pos = self.scanned + offset;
            let line: Vec<u8> = self.buffer.drain(..=pos).collect();
            self.scanned = 0;
            if let Err(error) = self.handle_line(&line, &mut out) {
                return (out, Some(error));
            }
        }
        self.scanned = self.buffer.len();
        if self.buffer.len() > MAX_LINE_BYTES {
            let error = UpstreamError::new(UpstreamErrorKind::Other, "/v1/responses SSE 单行超过 4MiB");
            return (out, Some(error));
        }
        (out, None)
    }

    /// 流结束时处理最后一行(没有换行结尾的残留)。
    fn finish(&mut self) -> (Vec<EventFrame>, Option<UpstreamError>) {
        let line = std::mem::take(&mut self.buffer);
        self.scanned = 0;
        let mut out = Vec::new();
        let error = if line.is_empty() {
            None
        } else {
            self.handle_line(&line, &mut out).err()
        };
        (out, error)
    }

    fn handle_line(&mut self, line: &[u8], out: &mut Vec<EventFrame>) -> Result<(), UpstreamError> {
        // 按字节切行后再整行解码,UTF-8 多字节字符跨批次不会被切坏。
        let line = String::from_utf8_lossy(line);
        let Some(data) = line.trim_end_matches(['\r', '\n']).strip_prefix("data:") else {
            return Ok(());
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            // 客户端同样跳过解析失败的行;这里留痕,免得正文/工具增量无声消失查不到。
            tracing::warn!(len = data.len(), head = %data.chars().take(120).collect::<String>(),
                "/v1/responses SSE 行不是完整 JSON,已跳过");
            return Ok(());
        };
        self.map_event(&value, out)
    }

    fn map_event(&mut self, v: &Value, out: &mut Vec<EventFrame>) -> Result<(), UpstreamError> {
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let delta = v.get("delta").and_then(Value::as_str);
        match kind {
            "kiro:keep_alive" => {}
            "response.output_text.delta" => {
                if let Some(d) = delta {
                    out.push(EventFrame::event("assistantResponseEvent", json!({"content": d})));
                }
            }
            "response.reasoning_text.delta" | "response.reasoning.delta" => {
                if let Some(d) = delta {
                    out.push(EventFrame::event("reasoningContentEvent", json!({"text": d})));
                }
            }
            "response.refusal.delta" => {
                if let Some(d) = delta {
                    self.refusal_delta_seen = true;
                    out.push(EventFrame::event("assistantResponseEvent", json!({"content": d})));
                }
            }
            "response.refusal.done" => {
                if !self.refusal_delta_seen {
                    if let Some(text) = v.get("refusal").and_then(Value::as_str) {
                        out.push(EventFrame::event("assistantResponseEvent", json!({"content": text})));
                    }
                }
            }
            "response.output_item.added" if item_type(v) == Some("function_call") => {
                let item = &v["item"];
                let id = call_id(item);
                self.open_calls.insert(id.clone());
                self.current_call = Some(id.clone());
                out.push(EventFrame::event(
                    "toolUseEvent",
                    json!({
                        "toolUseId": id,
                        "name": item.get("name").and_then(Value::as_str).unwrap_or(""),
                        "input": "",
                        "stop": false,
                    }),
                ));
            }
            "response.function_call_arguments.delta" => {
                if let (Some(d), Some(id)) = (delta, self.current_call.clone()) {
                    self.calls_with_delta.insert(id.clone());
                    out.push(EventFrame::event(
                        "toolUseEvent",
                        json!({"toolUseId": id, "input": d, "stop": false}),
                    ));
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(id) = self.current_call.take() {
                    if self.open_calls.remove(&id) {
                        // 客户端在这里只发 stop;若一个增量都没收到,它会发出空参数的调用。
                        // 我们多做一步:用本事件自带的完整 arguments 补上(只影响下行,不改上游报文)。
                        if !self.calls_with_delta.remove(&id) {
                            if let Some(args) = v.get("arguments").and_then(Value::as_str).filter(|a| !a.is_empty()) {
                                out.push(EventFrame::event(
                                    "toolUseEvent",
                                    json!({"toolUseId": id, "input": args, "stop": false}),
                                ));
                            }
                        }
                        out.push(EventFrame::event("toolUseEvent", json!({"toolUseId": id, "stop": true})));
                    }
                }
            }
            "response.output_item.done" if item_type(v) == Some("function_call") => {
                let item = &v["item"];
                let id = call_id(item);
                if self.open_calls.remove(&id) {
                    if !self.calls_with_delta.remove(&id) {
                        out.push(EventFrame::event(
                            "toolUseEvent",
                            json!({
                                "toolUseId": id,
                                "input": item.get("arguments").and_then(Value::as_str).unwrap_or(""),
                                "stop": false,
                            }),
                        ));
                    }
                    if item.get("status").and_then(Value::as_str) != Some("incomplete") {
                        self.held_stops.push(id);
                    }
                }
            }
            "response.output_item.done" if item_type(v) == Some("reasoning") => {
                if let Some(frame) = reasoning_envelope(&v["item"]) {
                    out.push(frame);
                }
            }
            "response.completed" | "response.incomplete" => self.terminal(kind, v, out),
            "error" | "response.failed" => {
                // 客户端只读 `error` / `response.error`;标准 Responses 的 `error` 事件把 code/message
                // 放在顶层,找不到嵌套对象时用整个事件兜底,免得额度/封号码丢失后被当成普通 5xx 换号扩散。
                let nested = if kind == "error" {
                    v.get("error")
                } else {
                    v.pointer("/response/error")
                };
                let error = nested.filter(|e| e.is_object()).or(Some(v));
                self.open_calls.clear();
                self.held_stops.clear();
                self.current_call = None;
                return Err(crate::error_map::classify_responses_stream_error(
                    error.unwrap_or(&Value::Null),
                ));
            }
            _ => out.push(EventFrame::event(PASSTHROUGH_EVENT, json!({"type": kind}))),
        }
        Ok(())
    }

    /// 客户端 `handleTerminal`:completed 才补发暂存/未关闭的工具 stop;incomplete 不补
    /// (被截断的调用不该被当成完整调用执行)。
    fn terminal(&mut self, kind: &str, v: &Value, out: &mut Vec<EventFrame>) {
        let response = v.get("response").unwrap_or(&Value::Null);
        let incomplete = kind == "response.incomplete"
            || response.get("status").and_then(Value::as_str) == Some("incomplete");
        let held = std::mem::take(&mut self.held_stops);
        let open = std::mem::take(&mut self.open_calls);
        self.current_call = None;
        self.calls_with_delta.clear();
        if !incomplete {
            let mut seen = HashSet::new();
            for id in held.into_iter().chain(open) {
                if seen.insert(id.clone()) {
                    out.push(EventFrame::event("toolUseEvent", json!({"toolUseId": id, "stop": true})));
                }
            }
        }
        let reason = response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str);
        let stop_reason = match (incomplete, reason) {
            (true, Some("max_output_tokens")) => {
                // 旧协议的「到输出上限」是 ContentLengthExceededException,状态机据此收成 max_tokens。
                out.push(EventFrame::exception(
                    "ContentLengthExceededException",
                    json!({"message": "max_output_tokens"}),
                ));
                "MAX_TOKENS"
            }
            (true, Some("content_filter")) => "CONTENT_FILTERED",
            (true, Some("compaction_required_for_model_handoff")) => {
                "COMPACTION_REQUIRED_FOR_MODEL_HANDOFF"
            }
            (true, other) => {
                tracing::warn!(reason = ?other, "/v1/responses incomplete 原因未识别,按 END_TURN 收尾");
                "END_TURN"
            }
            (false, _) => "END_TURN",
        };
        out.push(EventFrame::event("metadataEvent", json!({"stopReason": stop_reason})));
        let usage = response.get("usage");
        if let Some(pct) = usage
            .and_then(|u| u.get("kiro:context_usage_percentage"))
            .and_then(Value::as_f64)
            .filter(|p| p.is_finite())
        {
            out.push(EventFrame::event("contextUsageEvent", json!({"contextUsagePercentage": pct})));
        }
        if let Some(credits) = usage.and_then(|u| u.get("kiro:credits")) {
            if let Some(value) = credits.get("value").and_then(Value::as_f64).filter(|x| x.is_finite()) {
                out.push(EventFrame::event(
                    "meteringEvent",
                    json!({
                        "usage": value,
                        "unit": credits.get("unit"),
                        "unitPlural": credits.get("unit_plural"),
                    }),
                ));
            }
        }
    }
}

fn item_type(v: &Value) -> Option<&str> {
    v.pointer("/item/type").and_then(Value::as_str)
}

fn call_id(item: &Value) -> String {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// reasoning item 的 `encrypted_content` 信封(`EUs`):redacted → redactedContent;
/// 签名思考 → 只发签名(文本已经由 reasoning_text.delta 发过,这里不重放);解不开或无签名 → 不发。
fn reasoning_envelope(item: &Value) -> Option<EventFrame> {
    let raw = item.get("encrypted_content").and_then(Value::as_str)?;
    let envelope: Value = serde_json::from_str(raw).ok()?;
    if let Some(redacted) = envelope
        .get("redactedContent")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Some(EventFrame::event(
            "reasoningContentEvent",
            json!({"redactedContent": redacted}),
        ));
    }
    let signature = envelope
        .pointer("/reasoningText/signature")
        .and_then(Value::as_str)?;
    envelope.pointer("/reasoningText/text").and_then(Value::as_str)?;
    Some(EventFrame::event(
        "reasoningContentEvent",
        json!({"text": "", "signature": signature}),
    ))
}

/// 新协议:Responses SSE → 事件帧。
pub fn responses_events(
    byte_stream: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
) -> EventStream {
    struct State {
        bytes: ByteStream,
        decoder: ResponsesDecoder,
        ready: VecDeque<EventFrame>,
        /// 已解出帧之后要报的错(帧交完再报)。
        error: Option<UpstreamError>,
        eof: bool,
        done: bool,
    }
    let state = State {
        bytes: Box::pin(byte_stream),
        decoder: ResponsesDecoder::default(),
        ready: VecDeque::new(),
        error: None,
        eof: false,
        done: false,
    };
    Box::pin(futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(frame) = state.ready.pop_front() {
                return Some((Ok(frame), state));
            }
            if let Some(error) = state.error.take() {
                state.done = true;
                return Some((Err(error), state));
            }
            if state.done {
                return None;
            }
            let (frames, error) = if state.eof {
                state.done = true;
                state.decoder.finish()
            } else {
                match state.bytes.next().await {
                    Some(Ok(bytes)) => state.decoder.feed(&bytes),
                    Some(Err(error)) => (
                        Vec::new(),
                        Some(UpstreamError::network(format!("读取上游流失败: {error}"))),
                    ),
                    None => {
                        state.eof = true;
                        continue;
                    }
                }
            };
            state.ready.extend(frames);
            if error.is_some() {
                state.error = error;
                state.done = true;
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn collect(chunks: Vec<&[u8]>) -> Vec<Result<EventFrame, UpstreamError>> {
        let chunks: Vec<reqwest::Result<bytes::Bytes>> = chunks
            .into_iter()
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect();
        responses_events(futures::stream::iter(chunks)).collect().await
    }

    fn ok(items: Vec<Result<EventFrame, UpstreamError>>) -> Vec<EventFrame> {
        items.into_iter().collect::<Result<Vec<_>, _>>().unwrap()
    }

    fn names(frames: &[EventFrame]) -> Vec<&str> {
        frames
            .iter()
            .map(|f| f.event_type().or(f.exception_type()).unwrap_or("?"))
            .collect()
    }

    /// 真机抓包(`wire-capture-pro-claude-1.4.121.jsonl`,opus-5.5)的完整事件序列,
    /// 按 7 字节切块喂入,中文跨块。
    #[tokio::test]
    async fn captured_text_turn_maps_to_legacy_events() {
        let raw = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"sequence_number\":0}\n\n",
            "data: {\"type\":\"response.in_progress\",\"sequence_number\":1}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n",
            "data: {\"type\":\"response.content_part.added\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"KIRO_\"}\n\n",
            "data: {\"type\":\"kiro:keep_alive\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"你好\"}\n\n",
            "data: {\"type\":\"response.output_text.done\"}\n\n",
            "data: {\"type\":\"response.content_part.done\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"error\":null,\"usage\":{\"kiro:credits\":{\"value\":0.322349,\"unit\":\"credit\",\"unit_plural\":\"credits\"},\"kiro:context_usage_percentage\":10.5}}}\n\n",
            "data: [DONE]\n\n",
        );
        let chunks: Vec<&[u8]> = raw.as_bytes().chunks(7).collect();
        let frames = ok(collect(chunks).await);
        let text: String = frames
            .iter()
            .filter(|f| f.event_type() == Some("assistantResponseEvent"))
            .map(|f| f.payload_as_json::<Value>().unwrap()["content"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(text, "KIRO_你好");
        let n = names(&frames);
        let tail = &n[n.len() - 3..];
        assert_eq!(tail, ["metadataEvent", "contextUsageEvent", "meteringEvent"]);
        assert!(!n.contains(&"response.output_item.added"), "非工具事件不得沿用原名: {n:?}");
        let meter = frames.last().unwrap().payload_as_json::<Value>().unwrap();
        assert_eq!(meter["usage"], 0.322349);
        let meta = frames[frames.len() - 3].payload_as_json::<Value>().unwrap();
        assert_eq!(meta["stopReason"], "END_TURN");
    }

    #[tokio::test]
    async fn function_call_with_deltas_stops_only_at_completed() {
        let frames = ok(collect(vec![
            b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"p\\\":\"}\n\n",
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"1}\"}\n\n",
            b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"arguments\":\"{\\\"p\\\":1}\",\"status\":\"completed\"}}\n\n",
            b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        ]).await);
        let tools: Vec<Value> = frames
            .iter()
            .filter(|f| f.event_type() == Some("toolUseEvent"))
            .map(|f| f.payload_as_json().unwrap())
            .collect();
        assert_eq!(tools.len(), 4, "开始 + 两段增量 + completed 时的 stop(不用完整参数重放)");
        assert_eq!(tools[0]["name"], "read_file");
        assert_eq!(tools[1]["input"], "{\"p\":");
        assert_eq!(tools[3], json!({"toolUseId":"c1","stop":true}));
    }

    #[tokio::test]
    async fn function_call_without_deltas_replays_full_arguments() {
        let frames = ok(collect(vec![
            b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"c2\",\"name\":\"noop\"}}\n\n",
            b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"c2\",\"arguments\":\"{}\"}}\n\n",
            b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        ]).await);
        let tools: Vec<Value> = frames
            .iter()
            .filter(|f| f.event_type() == Some("toolUseEvent"))
            .map(|f| f.payload_as_json().unwrap())
            .collect();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[1]["input"], "{}");
        assert_eq!(tools[2]["stop"], true);
    }

    #[tokio::test]
    async fn incomplete_does_not_stop_open_calls_and_maps_reasons() {
        let frames = ok(collect(vec![
            b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c3\",\"name\":\"w\"}}\n\n",
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"a\\\"\"}\n\n",
            b"data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
        ]).await);
        assert!(!frames.iter().any(|f| f.event_type() == Some("toolUseEvent")
            && f.payload_as_json::<Value>().unwrap()["stop"] == true));
        assert!(frames.iter().any(|f| f.exception_type() == Some("ContentLengthExceededException")));
        let filtered = ok(collect(vec![
            b"data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"content_filter\"}}}\n\n",
        ]).await);
        let meta = filtered
            .iter()
            .find(|f| f.event_type() == Some("metadataEvent"))
            .unwrap()
            .payload_as_json::<Value>()
            .unwrap();
        assert_eq!(meta["stopReason"], "CONTENT_FILTERED");
    }

    #[tokio::test]
    async fn reasoning_text_then_signature_envelope() {
        let frames = ok(collect(vec![
            b"data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"think\"}\n\n",
            b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"{\\\"reasoningText\\\":{\\\"text\\\":\\\"think\\\",\\\"signature\\\":\\\"SIG\\\"}}\"}}\n\n",
            b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"{\\\"reasoningText\\\":{\\\"text\\\":\\\"x\\\"}}\"}}\n\n",
            b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"{\\\"redactedContent\\\":\\\"QUJD\\\"}\"}}\n\n",
            b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        ]).await);
        let r: Vec<Value> = frames
            .iter()
            .filter(|f| f.event_type() == Some("reasoningContentEvent"))
            .map(|f| f.payload_as_json().unwrap())
            .collect();
        assert_eq!(r.len(), 3, "无签名的信封不发: {r:?}");
        assert_eq!(r[0]["text"], "think");
        assert_eq!(r[1], json!({"text":"","signature":"SIG"}));
        assert_eq!(r[2], json!({"redactedContent":"QUJD"}));
    }

    #[tokio::test]
    async fn refusal_done_only_used_without_deltas() {
        let with_delta = ok(collect(vec![
            b"data: {\"type\":\"response.refusal.delta\",\"delta\":\"no\"}\n\n",
            b"data: {\"type\":\"response.refusal.done\",\"refusal\":\"no\"}\n\n",
        ]).await);
        assert_eq!(
            with_delta.iter().filter(|f| f.event_type() == Some("assistantResponseEvent")).count(),
            1
        );
        let only_done = ok(collect(vec![b"data: {\"type\":\"response.refusal.done\",\"refusal\":\"no\"}\n\n"]).await);
        assert_eq!(only_done.len(), 1);
    }

    #[tokio::test]
    async fn stream_error_events_end_the_stream_with_classified_error() {
        let items = collect(vec![
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}\n\n",
            b"data: {\"type\":\"error\",\"error\":{\"type\":\"too_many_requests\",\"code\":\"rate_limit_exceeded\",\"message\":\"slow\"}}\n\n",
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"never\"}\n\n",
        ])
        .await;
        assert_eq!(items.len(), 2);
        assert!(items[0].is_ok());
        assert_eq!(items[1].as_ref().unwrap_err().kind, UpstreamErrorKind::RateLimited);
        let failed = collect(vec![
            b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"insufficient_quota\",\"code\":\"credit_balance_exhausted\"}}}\n\n",
        ])
        .await;
        assert_eq!(failed[0].as_ref().unwrap_err().kind, UpstreamErrorKind::QuotaExhausted);
    }

    #[tokio::test]
    async fn arguments_done_without_deltas_carries_full_arguments() {
        let frames = ok(collect(vec![
            b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c9\",\"name\":\"read\"}}\n\n",
            b"data: {\"type\":\"response.function_call_arguments.done\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}\n\n",
            b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c9\",\"arguments\":\"{}\"}}\n\n",
            b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        ]).await);
        let tools: Vec<Value> = frames
            .iter()
            .filter(|f| f.event_type() == Some("toolUseEvent"))
            .map(|f| f.payload_as_json().unwrap())
            .collect();
        assert_eq!(tools.len(), 3, "{tools:?}");
        assert_eq!(tools[1]["input"], "{\"path\":\"a.rs\"}");
        assert_eq!(tools[2]["stop"], true);
    }

    #[tokio::test]
    async fn top_level_error_fields_are_classified() {
        let items = collect(vec![b"data: {\"type\":\"error\",\"code\":\"credit_balance_exhausted\",\"message\":\"empty\"}\n\n"]).await;
        assert_eq!(items[0].as_ref().unwrap_err().kind, UpstreamErrorKind::QuotaExhausted);
    }

    #[tokio::test]
    async fn long_line_split_into_many_chunks_is_reassembled() {
        let text = "x".repeat(200_000);
        let raw = format!("data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{text}\"}}\n\n");
        let chunks: Vec<&[u8]> = raw.as_bytes().chunks(1000).collect();
        let frames = ok(collect(chunks).await);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload_as_json::<Value>().unwrap()["content"].as_str().unwrap().len(), 200_000);
    }

    #[tokio::test]
    async fn trailing_line_without_newline_is_still_parsed() {
        let frames = ok(collect(vec![b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"z\"}"]).await);
        assert_eq!(names(&frames), ["assistantResponseEvent"]);
    }
}
