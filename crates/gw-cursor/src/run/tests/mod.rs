use super::*;

mod exec_frames;
mod request;
mod response;
mod tools;
mod trailer;

/// 测试包装:绝大多数用例不关心 `1.14` 清单,统一传空。
fn f0(
    turns: &[Turn],
    model: &Model,
    conversation_id: &str,
    timezone: &str,
    now_ms: u64,
    shape: RunShape,
    phase: Phase,
) -> Vec<u8> {
    build_frame0(
        turns,
        "",
        &[],
        Media::default(),
        model,
        &[],
        conversation_id,
        timezone,
        now_ms,
        shape,
        phase,
    )
}

/// 按字段号路径取一个 length-delimited 子消息。
fn dig<'a>(buf: &'a [u8], path: &[u32]) -> Option<&'a [u8]> {
    let mut cur = buf;
    for want in path {
        let mut found = None;
        for (f, v) in Reader::new(cur) {
            if f == *want {
                if let PbValue::Len(s) = v {
                    found = Some(s);
                    break;
                }
            }
        }
        cur = found?;
    }
    Some(cur)
}

fn string_at(buf: &[u8], field: u32) -> Option<String> {
    Reader::new(buf).find_map(|(f, v)| match (f, v) {
        (ff, PbValue::Len(s)) if ff == field => Some(String::from_utf8_lossy(s).to_string()),
        _ => None,
    })
}

fn varint_at(buf: &[u8], field: u32) -> Option<u64> {
    Reader::new(buf).find_map(|(f, v)| match (f, v) {
        (ff, PbValue::Varint(n)) if ff == field => Some(n),
        _ => None,
    })
}

fn one_user(text: &str) -> Vec<Turn> {
    vec![Turn {
        text: text.into(),
        is_user: true,
    }]
}

/// 造一帧 `1.<slot>.{1: 文本, 2: 1}`。
fn delta_frame(slot: u32, text: &str) -> Vec<u8> {
    let mut delta = Writer::new();
    delta.string(RESP_DELTA_TEXT, text);
    delta.uint(2, 1);
    let mut msg = Writer::new();
    msg.message(slot, &delta);
    let mut outer = Writer::new();
    outer.message(RESP_MESSAGE, &msg);
    outer.into_bytes()
}

/// 造一帧真实形态的外部工具调用。
///
/// ⚠️ 这个 helper 用的是与 `parse_tool_call` **同一套常量**,所以它只能证明
/// 「编码解码互逆」,证明不了「与真上游一致」——字段号整套写错也照样绿。
/// 真正的锚点是 `tests/fixtures/*.bin`(抓包实物),见
/// [`解析抓包实物里的外部工具调用帧`]。本 helper 只用来覆盖那些实物里没有的组合
/// (数值参数、多参数)。
fn tool_call_frame(bare: &str, call_id: &str, args: &[(&str, Value)]) -> Vec<u8> {
    let mut inner = Writer::new();
    inner.string(TOOL_FULL_NAME, &format!("{TOOL_NS}-{bare}"));
    for (k, v) in args {
        let mut wrap = Writer::new();
        match v {
            Value::String(sv) => wrap.string(PBV_STRING, sv),
            Value::Number(n) => wrap.double(PBV_NUMBER, n.as_f64().unwrap()),
            Value::Bool(b) => wrap.uint(PBV_BOOL, u64::from(*b)),
            Value::Null => wrap.uint(PBV_NULL, 0),
            other => panic!("helper 不支持 {other:?},请手写字节"),
        }
        let mut kv = Writer::new();
        kv.string(TC_ARG_KEY, k);
        kv.message(TC_ARG_VAL, &wrap);
        inner.message(TC_ARGS, &kv);
    }
    inner.string(3, call_id);
    inner.string(4, TOOL_NS);
    inner.string(TC_BARE_NAME, bare);
    let mut ext = Writer::new();
    ext.message(TC_INNER, &inner);
    let mut detail = Writer::new();
    detail.message(TC_EXTERNAL, &ext);
    detail.string(57, call_id);
    let mut ch = Writer::new();
    ch.string(TC_CALL_ID, call_id);
    ch.message(TC_DETAIL, &detail);
    let mut msg = Writer::new();
    msg.message(RESP_TOOL_CHANNEL, &ch);
    let mut outer = Writer::new();
    outer.message(RESP_MESSAGE, &msg);
    outer.into_bytes()
}

/// 2026-11-09 线上实物(caio-worker-cursor 日志逐字,`value` 的 base64 省略):
/// Anthropic 用量政策拦截。`analyticsMetadata.actionRequired` 是拦截类别。
pub(crate) const ANTHROPIC_CYBER_BLOCK_TRAILER: &str = r#"{"error":{"code":"invalid_argument","message":"Error","details":[{"type":"aiserver.v1.ErrorDetails","debug":{"error":"ERROR_OPENAI","details":{"title":"Request blocked by Anthropic","detail":"We are unable to complete this request because it was blocked under Anthropic's Usage Policy.","isRetryable":false,"analyticsMetadata":{"actionRequired":"cyber_policy_review"}},"isExpected":true}}]}}"#;

/// 2026-11-10 12:45:48(北京)线上实物(caio-worker-cursor 日志逐字,`value` 的 base64 省略):
/// 模型提供方(Anthropic)对**请求本身**回 400,Cursor 包成 `resource_exhausted` +
/// `ERROR_PROVIDER_ERROR`。当时被判成 QuotaExhausted,同一个请求 30 秒内禁掉 4 个号。
pub(crate) const PROVIDER_400_TRAILER: &str = r#"{"error":{"code":"resource_exhausted","message":"Error","details":[{"type":"aiserver.v1.ErrorDetails","debug":{"error":"ERROR_PROVIDER_ERROR","details":{"title":"Provider Error","detail":"We're having trouble connecting to the model provider. This might be temporary - please try again in a moment.","isRetryable":false,"additionalInfo":{"providerStatusCode":"400"}},"isExpected":true}}]}}"#;
