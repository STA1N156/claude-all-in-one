//! cursor 线按客户端翻译的测试。

use super::*;
use serde_json::json;

fn tool_use(id: &str) -> Value {
    json!({"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": format!("/tmp/{id}.jpg")}})
}

fn tool_result(id: &str) -> Value {
    json!({"type": "tool_result", "tool_use_id": id, "content": "..."})
}

fn image() -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "AAAA"}})
}

fn text(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

fn user(content: Value) -> Value {
    json!({"role": "user", "content": content})
}

fn assistant(content: Value) -> Value {
    json!({"role": "assistant", "content": content})
}

fn body(messages: Vec<Value>) -> Value {
    json!({"model": "claude-opus-5-5", "max_tokens": 1024, "messages": messages})
}

fn types(m: &Value) -> Vec<String> {
    m["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            let t = b["type"].as_str().unwrap();
            match t {
                "tool_result" => format!("tool_result:{}", b["tool_use_id"].as_str().unwrap()),
                _ => t.to_string(),
            }
        })
        .collect()
}

/// 格式正确的请求一个字节都不能动 —— 否则线上会话的指纹/自造历史/缓存全部跟着变。
/// 覆盖线上见过的几种合法形态(都是 11-10 窗口里成功过的)。
#[test]
fn well_formed_requests_are_byte_identical() {
    let cases = vec![
        // 纯文本
        body(vec![user(json!("你好")), assistant(json!("嗨")), user(json!("继续"))]),
        // 工具结果后面单独跟一条图片消息(线上成功形态)
        body(vec![
            user(json!("看图")),
            assistant(json!([text("读一下"), tool_use("a")])),
            user(json!([tool_result("a")])),
            user(json!([image()])),
            user(json!([text("[Image: a.jpg]")])),
        ]),
        // 并行工具的结果拆成多条 user,但每条都只有 tool_result
        body(vec![
            user(json!("跑两个")),
            assistant(json!([tool_use("a"), tool_use("b")])),
            user(json!([tool_result("b")])),
            user(json!([tool_result("a")])),
        ]),
        // 同一条里 tool_result 在前、文字在后
        body(vec![
            user(json!("跑")),
            assistant(json!([tool_use("a")])),
            user(json!([tool_result("a"), text("顺便说一句"), image()])),
            assistant(json!("好")),
            user(json!("继续")),
        ]),
        // 连续 user 文字(没有工具)
        body(vec![user(json!("一")), user(json!([text("二"), image()])), user(json!("三"))]),
    ];
    for (i, original) in cases.into_iter().enumerate() {
        let before = serde_json::to_string(&original).unwrap();
        let mut b = original;
        let t = translate(&mut b);
        assert_eq!(t.reorder.runs, 0, "case {i}");
        assert_eq!(serde_json::to_string(&b).unwrap(), before, "case {i}");
    }
}

/// 2026-11-10 12:45 事故请求(request_logs 6439900)#198~#205 的形态:并行 4 个 Read,
/// 结果拆成多条、顺序打乱、图片排在 tool_result 前。Anthropic 要求 tool_result 排在
/// user 回合最前 → 上游 400 → 当年被误判额度耗尽禁掉 4 个号。
#[test]
fn image_before_tool_result_is_reordered() {
    let mut b = body(vec![
        user(json!("看这几张图")),
        assistant(json!([
            text("..."),
            tool_use("vkpwq4"),
            tool_use("SMzGkA"),
            tool_use("MLgTSf"),
            tool_use("edtANZ")
        ])),
        user(json!([tool_result("MLgTSf")])),
        user(json!([image(), tool_result("vkpwq4")])),
        user(json!([image(), tool_result("edtANZ")])),
        user(json!([image(), tool_result("SMzGkA")])),
        user(json!([image()])),
        user(json!([text("[Image: 1]"), text("[Image: 2]"), text("[Image: 3]"), text("[Image: 4]")])),
        assistant(json!([text("..."), tool_use("32R2LZ")])),
        user(json!([tool_result("32R2LZ")])),
    ]);
    let t = translate(&mut b);
    assert_eq!(t.reorder.runs, 1);
    assert_eq!(t.reorder.tool_results, 4);
    assert_eq!(t.reorder.other_blocks, 8);

    let msgs = b["messages"].as_array().unwrap();
    // 6 条连续 user 合成 1 条:10 条 → 5 条
    assert_eq!(msgs.len(), 5);
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(
        types(&msgs[2]),
        vec![
            "tool_result:MLgTSf",
            "tool_result:vkpwq4",
            "tool_result:edtANZ",
            "tool_result:SMzGkA",
            "image",
            "image",
            "image",
            "image",
            "text",
            "text",
            "text",
            "text"
        ]
    );
    // 后面的轮次原样
    assert_eq!(msgs[3]["role"], "assistant");
    assert_eq!(types(&msgs[4]), vec!["tool_result:32R2LZ"]);
}

/// 第 86~90 条的形态:第一个结果单独一条,第二个结果前面夹着图片,后面还有一条字符串内容。
#[test]
fn string_content_in_run_becomes_text_block() {
    let mut b = body(vec![
        user(json!("开始")),
        assistant(json!([tool_use("JdzLW3"), tool_use("eWzqQ3")])),
        user(json!([tool_result("JdzLW3")])),
        user(json!("插一句")),
        user(json!([image(), tool_result("eWzqQ3")])),
    ]);
    translate(&mut b);
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(
        types(&msgs[2]),
        vec!["tool_result:JdzLW3", "tool_result:eWzqQ3", "text", "image"]
    );
    assert_eq!(msgs[2]["content"][2]["text"], "插一句");
}

/// 中转在 tool_use 与 tool_result 之间注入的 system 消息,兜底分流后会变成一条 user
/// (`<system_context>`),同样把 tool_result 挤到后面 —— 出口合规必须排在分流之后。
#[test]
fn injected_system_between_tool_use_and_result_is_fixed() {
    let mut b = body(vec![
        user(json!("跑")),
        assistant(json!([tool_use("a")])),
        json!({"role": "system", "content": "relay says: be nice"}),
        user(json!([tool_result("a")])),
    ]);
    let t = translate(&mut b);
    assert_eq!(t.reorder.runs, 1);
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(types(&msgs[2]), vec!["tool_result:a", "text"]);
    assert!(msgs[2]["content"][1]["text"].as_str().unwrap().contains("<system_context>"));
}

/// 兜底 = 原有 system 分流,一个字节都不变(和直接调 route_system_role_messages 一致)。
#[test]
fn fallback_keeps_legacy_system_routing() {
    let original = body(vec![
        json!({"role": "system", "content": "SessionStart hook additional context: 项目说明"}),
        user(json!("你好")),
        json!({"role": "system", "content": "The user sent a new message while you were working:\n再看看"}),
    ]);
    let mut legacy = original.clone();
    crate::chat::route_system_role_messages(&mut legacy);
    let mut b = original;
    translate(&mut b);
    assert_eq!(b, legacy);
}

/// 本轮(末段)违规也要修;修完末条以 tool_result 开头。
#[test]
fn tail_run_is_fixed() {
    let mut b = body(vec![
        user(json!("看图")),
        assistant(json!([tool_use("a"), tool_use("b")])),
        user(json!([tool_result("a")])),
        user(json!([image(), tool_result("b")])),
    ]);
    let t = translate(&mut b);
    assert_eq!(t.reorder.runs, 1);
    let msgs = b["messages"].as_array().unwrap();
    let last = msgs.last().unwrap();
    assert_eq!(types(last), vec!["tool_result:a", "tool_result:b", "image"]);
    // 末条带图 → 不是「只有工具结果」的续轮(与改写前同:改写前末条也带图)
    assert!(!crate::chat::last_user_is_tool_result_only(&b));
}

/// 跨轮稳定:一段 user 后面一旦跟了 assistant 就不会再变,第 N 轮与第 N+1 轮翻译后的
/// 共同前缀必须逐字节一致 —— 否则会话指纹每轮分叉、自造历史每轮重铺。
#[test]
fn rewrite_is_stable_across_turns() {
    let turn_n = vec![
        user(json!("看图")),
        assistant(json!([tool_use("a"), tool_use("b")])),
        user(json!([tool_result("a")])),
        user(json!([image(), tool_result("b")])),
    ];
    let mut turn_n1 = turn_n.clone();
    turn_n1.push(assistant(json!("看完了")));
    turn_n1.push(user(json!("然后呢")));

    let mut a = body(turn_n);
    let mut b = body(turn_n1);
    translate(&mut a);
    translate(&mut b);
    let ma = a["messages"].as_array().unwrap();
    let mb = b["messages"].as_array().unwrap();
    assert_eq!(&mb[..ma.len()], &ma[..]);
}

/// 识别走 gw_core 的同一份判据,在分流之前取信号。
#[test]
fn recognizes_clients() {
    let cc = json!({
        "system": [{"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}],
        "messages": [user(json!("hi"))]
    });
    let opencode = json!({"system": "You are OpenCode, the best coding agent.", "messages": [user(json!("hi"))]});
    let grok = json!({
        "system": "You are Grok released by xAI.",
        "tools": [{"name": "spawn_subagent"}, {"name": "get_command_or_subagent_output"}],
        "messages": [user(json!("hi"))]
    });
    let other = json!({"system": "You are a helpful assistant.", "messages": [user(json!("hi"))]});
    for (mut b, want) in [
        (cc, ClientKind::ClaudeCode),
        (opencode, ClientKind::OpenCode),
        (grok, ClientKind::GrokBuild),
        (other, ClientKind::Fallback),
    ] {
        assert_eq!(translate(&mut b).kind, want);
    }
}

/// 没有 messages / messages 不是数组:不 panic,原样返回。
#[test]
fn malformed_body_is_left_alone() {
    for original in [json!({}), json!({"messages": "x"}), json!({"messages": [1, null, {"role": "user"}]})] {
        let mut b = original.clone();
        let t = translate(&mut b);
        assert_eq!(t.reorder.runs, 0);
        assert_eq!(b, original);
    }
}

/// 线上流量回放(默认不跑)。用实现本身翻译真实请求,输出交给独立脚本复核。
///
/// 输入 `CURSOR_REPLAY_IN`:每行 `{"id": <request_logs id>, "body": <客户端请求体>}`。
/// 输出 `CURSOR_REPLAY_OUT`:每行 `{"id", "client", "runs", "legacy_equal", "body"}`,
/// `legacy_equal` = 翻译结果与「只做原有 system 分流」逐字节一致,`body` 是翻译后的请求体。
/// 用法(139,线上镜像里跑,断网):
/// `<测试二进制> --ignored --exact chat::clients::tests::replay_from_env --nocapture`
#[test]
#[ignore]
fn replay_from_env() {
    use std::io::{BufRead, Write};
    let input = std::env::var("CURSOR_REPLAY_IN").expect("CURSOR_REPLAY_IN");
    let output = std::env::var("CURSOR_REPLAY_OUT").expect("CURSOR_REPLAY_OUT");
    let reader = std::io::BufReader::new(std::fs::File::open(&input).unwrap());
    let mut out = std::io::BufWriter::new(std::fs::File::create(&output).unwrap());
    let (mut total, mut changed, mut legacy_diff_without_runs) = (0usize, 0usize, 0usize);
    let mut by_client: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for line in reader.lines() {
        let line = line.unwrap();
        if line.trim().is_empty() {
            continue;
        }
        let rec: Value = serde_json::from_str(&line).unwrap();
        let original = rec["body"].clone();
        let mut legacy = original.clone();
        crate::chat::route_system_role_messages(&mut legacy);
        let mut b = original;
        let t = translate(&mut b);
        let legacy_equal = serde_json::to_string(&b).unwrap() == serde_json::to_string(&legacy).unwrap();
        total += 1;
        *by_client.entry(t.kind.as_str()).or_default() += 1;
        if t.reorder.runs > 0 {
            changed += 1;
        } else if !legacy_equal {
            legacy_diff_without_runs += 1;
        }
        let row = json!({
            "id": rec["id"], "client": t.kind.as_str(), "runs": t.reorder.runs,
            "legacy_equal": legacy_equal, "body": b
        });
        writeln!(out, "{}", serde_json::to_string(&row).unwrap()).unwrap();
    }
    out.flush().unwrap();
    println!("回放 {total} 条 | 出口合规改写 {changed} 条 | 未改写却与兜底不一致 {legacy_diff_without_runs} 条");
    println!("识别分布 {by_client:?}");
    assert_eq!(legacy_diff_without_runs, 0, "没改写的请求必须与兜底逐字节一致");
}

/// 合并时丢掉的消息级多余键要计数(审查意见:不能无声丢数据)。
#[test]
fn dropped_message_keys_are_counted() {
    let mut b = body(vec![
        user(json!("跑")),
        assistant(json!([tool_use("a")])),
        json!({"role": "user", "name": "relay", "content": [image(), tool_result("a")]}),
    ]);
    let t = translate(&mut b);
    assert_eq!(t.reorder.runs, 1);
    assert_eq!(t.reorder.dropped_keys, 1);
    assert_eq!(types(&b["messages"][2]), vec!["tool_result:a", "image"]);
}
