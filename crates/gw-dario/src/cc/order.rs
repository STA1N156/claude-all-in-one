//! 按 CC 的字段顺序写请求体。
//!
//! 工作区的 `serde_json` 没开 `preserve_order`:请求体在 worker 里是 `Value`,再序列化时顶层
//! 字段会变成字母序(context_management, max_tokens, messages, …),而真 CC 是 model, messages,
//! system, tools, …。dario 只在它自己的模板路径上重排(`orderBodyForOutbound`),透传路径原样
//! 发出,所以这一步得由 caio 做。规则与 `orderBodyForOutbound` 相同:先按给定顺序写出现的字段,
//! 其余字段接在后面(不丢字段)。只排顶层,嵌套对象不动(dario 也只排顶层)。
//!
//! ⚠️ 不在清单里的字段(tool_choice、temperature 之类)是**字母序**,不是客户端原来的顺序 ——
//! 原顺序在 worker 把请求体解析成 `Value` 时就丢了。要逐字保住得让 worker 保留原始字节,本次不做。

use std::collections::HashSet;

use serde_json::{Map, Value};

pub(crate) fn to_ordered_bytes(body: &Map<String, Value>, order: &[String]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4096);
    out.push(b'{');
    let mut first = true;
    let mut seen: HashSet<&str> = HashSet::new();
    for key in order {
        if seen.contains(key.as_str()) {
            continue;
        }
        if let Some(v) = body.get(key) {
            write_field(&mut out, &mut first, key, v);
            seen.insert(key.as_str());
        }
    }
    for (key, v) in body {
        if !seen.contains(key.as_str()) {
            write_field(&mut out, &mut first, key, v);
        }
    }
    out.push(b'}');
    out
}

fn write_field(out: &mut Vec<u8>, first: &mut bool, key: &str, v: &Value) {
    if !*first {
        out.push(b',');
    }
    *first = false;
    // 写进 Vec 不会失败(只有 I/O 错误或非字符串键才会,这里两者都不存在)。
    serde_json::to_writer(&mut *out, key).expect("序列化字段名");
    out.push(b':');
    serde_json::to_writer(&mut *out, v).expect("序列化字段值");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn order() -> Vec<String> {
        ["model", "messages", "system", "tools", "stream", "model"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn listed_fields_first_then_the_rest_in_existing_order() {
        let body = json!({"stream": true, "tool_choice": {"type": "auto"}, "model": "m", "messages": [], "a_extra": 1});
        let bytes = to_ordered_bytes(body.as_object().unwrap(), &order());
        let s = String::from_utf8(bytes).unwrap();
        assert_eq!(s, r#"{"model":"m","messages":[],"stream":true,"a_extra":1,"tool_choice":{"type":"auto"}}"#);
        // 内容不变,只是顺序。
        assert_eq!(serde_json::from_str::<Value>(&s).unwrap(), body);
    }
}
