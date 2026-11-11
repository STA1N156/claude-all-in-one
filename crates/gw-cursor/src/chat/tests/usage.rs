//! chat 测试:用量估算。

use super::*;

/// 线上 usage 口径 = Anthropic 规范:`input_tokens` 是**未命中缓存的新增部分**,
/// 缓存读取单列。钉住这条是因为旧口径(总上下文透传)会让下游把缓存部分重复计费。
///
/// **用量来自上游用量帧**时按上游原样出,不补缓存写 —— 上游明说缓存写为 0
/// (哪怕缓存读也是 0)也不能当估算(审查意见)。
#[test]
fn 收尾_usage_上游自报缓存时原样() {
    let u = ChatUsage {
        input_tokens: 98249,
        output_tokens: 516,
        cache_read_tokens: 98247,
        real_cache_read_tokens: 98247,
        ..Default::default()
    };
    let v = delta_usage_json(&u, 516, false);
    assert_eq!(v["input_tokens"], 2, "总上下文 - 缓存命中 = 增量");
    assert_eq!(v["output_tokens"], 516);
    assert_eq!(v["cache_read_input_tokens"], 98247);
    assert!(v.get("cache_creation_input_tokens").is_none());

    // 上游自报了缓存写:原样出,剩下的才是普通输入。
    let u2 = ChatUsage {
        input_tokens: 1000,
        output_tokens: 5,
        cache_read_tokens: 600,
        real_cache_read_tokens: 600,
        cache_creation_tokens: 300,
        ..Default::default()
    };
    let v2 = delta_usage_json(&u2, 5, false);
    assert_eq!(v2["input_tokens"], 100);
    assert_eq!(v2["cache_read_input_tokens"], 600);
    assert_eq!(v2["cache_creation_input_tokens"], 300);
    assert!(v2.get("cache_creation").is_none(), "上游自报不补 5 分钟档明细");

    // 上游用量帧明说没读也没写:按普通输入出,绝不当成估算去报缓存写。
    let u3 = ChatUsage {
        input_tokens: 50000,
        output_tokens: 9,
        ..Default::default()
    };
    let v3 = delta_usage_json(&u3, 9, false);
    assert_eq!(v3["input_tokens"], 50000);
    assert!(v3.get("cache_creation_input_tokens").is_none());
}

/// **用量是估算的**(CLI 形态没有用量帧:上游没报任何缓存数据)时,没命中的部分按
/// **缓存写**上报(5 分钟档),普通输入为 0 —— 与 Cursor 真实账单的形态一致
/// (2026-11-10 14:46~14:55 全部号:读 86.9%、写 10%、普通输入只有个位数 token;
/// Anthropic 对缓存写按输入价 1.25 倍收)。以前把这部分当普通输入(1 倍)报,少收。
#[test]
fn 收尾_usage_估算时未命中部分报缓存写() {
    let u = ChatUsage {
        input_tokens: 127939,
        output_tokens: 169,
        cache_read_tokens: 120000, // 模拟命中
        ..Default::default()
    };
    let v = delta_usage_json(&u, 169, true);
    assert_eq!(v["input_tokens"], 0);
    assert_eq!(v["cache_read_input_tokens"], 120000);
    assert_eq!(v["cache_creation_input_tokens"], 7939);
    assert_eq!(v["cache_creation"]["ephemeral_5m_input_tokens"], 7939);
    assert_eq!(v["cache_creation"]["ephemeral_1h_input_tokens"], 0);
    // 三项之和 = 总输入(不多算也不少算)
    let sum = v["input_tokens"].as_u64().unwrap()
        + v["cache_read_input_tokens"].as_u64().unwrap()
        + v["cache_creation_input_tokens"].as_u64().unwrap();
    assert_eq!(sum, 127939);

    // 冷启动(模拟没命中):整段都是缓存写,不出 cache_read 字段。
    let u2 = ChatUsage {
        input_tokens: 100,
        output_tokens: 5,
        ..Default::default()
    };
    let v2 = delta_usage_json(&u2, 5, true);
    assert_eq!(v2["input_tokens"], 0);
    assert_eq!(v2["cache_creation_input_tokens"], 100);
    assert!(v2.get("cache_read_input_tokens").is_none());

    // 命中 > 总输入(字段漂移)时夹住:不许下溢,也不出负的缓存写。
    let u3 = ChatUsage {
        input_tokens: 10,
        output_tokens: 1,
        cache_read_tokens: 999,
        ..Default::default()
    };
    let v3 = delta_usage_json(&u3, 1, true);
    assert_eq!(v3["input_tokens"], 0);
    assert!(v3.get("cache_creation_input_tokens").is_none());

    // 输入为 0(没有可计的上下文):什么缓存字段都不出。
    let v4 = delta_usage_json(&ChatUsage::default(), 0, true);
    assert_eq!(v4["input_tokens"], 0);
    assert!(v4.get("cache_creation_input_tokens").is_none());
}

/// 非流式回包由 `gw_core::fold` 把 `message_delta` 的 usage 逐字段并进消息体,
/// 缓存写字段必须一起带上(否则非流式客户按普通输入计费)。
#[test]
fn 非流式折叠后带上缓存写() {
    let u = ChatUsage {
        input_tokens: 1000,
        output_tokens: 7,
        cache_read_tokens: 900,
        ..Default::default()
    };
    let delta = delta_usage_json(&u, 7, true);
    let events = vec![
        SseEvent::new(
            "message_start",
            json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant",
                   "model":"x","content":[],"usage":{"input_tokens":0,"output_tokens":0}}}),
        ),
        SseEvent::new(
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":delta}),
        ),
        SseEvent::new("message_stop", json!({"type":"message_stop"})),
    ];
    let msg = gw_core::fold::fold_sse_to_message(&events).expect("折叠成功");
    assert_eq!(msg["usage"]["cache_read_input_tokens"], 900);
    assert_eq!(msg["usage"]["cache_creation_input_tokens"], 100);
    assert_eq!(msg["usage"]["input_tokens"], 0);
}

/// 无上游 `1.14` 时(tool_use 收口常态)必须从请求体估出 input/output,
/// 绝不能只留下 output —— 那会让 new-api 显示「0 / N」。
/// cache_read 不在此估:统一由 cache_sim 在收尾处给(见 estimate_usage_fallback 注释)。
#[test]
fn 无上游用量时按请求体估出输入与产出() {
    let system = "You are a coding agent."; // 24 chars → 6 tok
    let turns = vec![
        run::Turn {
            text: "a".repeat(40), // 10 tok
            is_user: true,
        },
        run::Turn {
            text: "b".repeat(80), // 20 tok
            is_user: false,
        },
        run::Turn {
            text: "c".repeat(20), // 5 tok —— 本轮用户新增
            is_user: true,
        },
    ];
    let tools = [run::ToolDef {
        name: "Edit".into(),
        description: "edit a file".into(),      // 11
        schema: "{\"type\":\"object\"}".into(), // 16
    }];
    // system 24 + conv 140 + tools (4+11+16=31) = 195 chars → 49 tok
    let u = estimate_usage_fallback(system, &turns, &tools, 12);
    assert!(u.input_tokens >= 40, "input={}", u.input_tokens);
    assert_eq!(u.output_tokens, 3, "12 chars → 3 tok");
    assert_eq!(
        u.cache_read_tokens, 0,
        "兜底估算不再给缓存 —— 缓存归 cache_sim 管(状态化前缀命中)"
    );
    assert_eq!(u.real_cache_read_tokens, 0, "估算路径绝不写对账列");

    // 单轮冷启动:同样只有 input,无缓存
    let cold = estimate_usage_fallback(
        "sys",
        &[run::Turn {
            text: "hello world!!".into(), // 13
            is_user: true,
        }],
        &[],
        0,
    );
    assert_eq!(cold.cache_read_tokens, 0);
    assert!(cold.input_tokens > 0);
}

/// 中文等非 ASCII 文本不能按 4 字符/token 估 —— 那会少计大半(计费口径,
/// 保守取 1 字符/token)。
#[test]
fn 估算对中文不缩水() {
    let ascii = est_text_tokens(&"a".repeat(400));
    assert_eq!(ascii, 100, "ASCII 400 字符 ≈ 100 token");
    let cjk = est_text_tokens(&"汉".repeat(300));
    assert_eq!(cjk, 300, "非 ASCII 按保守 1 字符/token");
    assert!(cjk > 75, "按 /4 只有 75,直接少计 75%");
}

#[tokio::test]
async fn adaptive_同等_透传思考块避免无首字挂死() {
    // 只有思考 + 用量:若 want_thinking=false 会走 EmptyResponse(无字无工具);
    // 透传后必须先有 message_start,客户端才算有首字。
    let usage_payload = [0x0au8, 0x08, 0x72, 0x06, 0x08, 0x64, 0x10, 0x05, 0x18, 0x5a];
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
        Ok(bytes::Bytes::from(wire::frame(&thinking_wire(
            "规划马里奥关卡",
        )))),
        Ok(bytes::Bytes::from(wire::frame(&usage_payload))),
    ];
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "claude-fable-5".into(),
        true, // = client_wants_thinking(adaptive/enabled)
        None,
        None,
        "conv-think".into(),
        std::sync::Arc::new(crate::AssetStore::default()),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        BuiltinXlate::default(),
        ChatUsage::default(),
        0,
        false, // cli_mode:测试默认 IDE 形态
        None,  // upload:本用例不走 wire v2 上传腿
        None,  // 本用例不关心影子捕获
    );
    use futures::StreamExt;
    let items: Vec<_> = out.collect().await;
    let events: Vec<_> = items
        .iter()
        .filter_map(|it| match it {
            Ok(StreamItem::Sse(e)) => Some(e.event.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        events.iter().any(|e| *e == "message_start"),
        "思考透传必须先发 message_start,否则 TTFB 永远空: {events:?}"
    );
    assert!(
        items.iter().any(|it| matches!(
            it,
            Ok(StreamItem::Sse(e))
                if e.event == "content_block_delta"
                    && e.data.get("delta").and_then(|d| d.get("type")).and_then(|t| t.as_str())
                        == Some("thinking_delta")
        )),
        "必须出现 thinking_delta"
    );
}

// ── 内建工具兼容转换层(2026-08-13)──────────────────────────────────

/// 计费估算的口径:同一请求,按完整上下文估出的输入必须覆盖历史(自造历史/续轮时上传给
/// 上游的只有本轮一条,不能拿它记账 —— 2026-11-09 线上少收约九成)。
#[test]
fn 计费按完整上下文估_不按本轮一条() {
    let body = json!({"messages": [
        {"role": "user", "content": "x".repeat(40_000)},
        {"role": "assistant", "content": "y".repeat(40_000)},
        {"role": "user", "content": "继续"}
    ]});
    let raw = to_turns(&body);
    let full = estimate_usage_fallback("sys", &raw, &[], 0).input_tokens;
    let current_only = estimate_usage_fallback("sys", &raw[raw.len() - 1..], &[], 0).input_tokens;
    assert!(full >= 20_000, "完整上下文约 2 万 token: {full}");
    assert!(current_only < 100, "本轮一条只有几个 token: {current_only}");
    // 自造历史:上传形态只剩本轮 → 取完整上下文
    let blob = billing_estimate("sys", &raw, &raw[raw.len() - 1..], &[]).input_tokens;
    assert_eq!(blob, full);
    // 带 PDF 的折叠:上传形态多了文档文字 → 取上传形态
    let with_doc = vec![run::Turn {
        text: "z".repeat(200_000),
        is_user: true,
    }];
    let doc = billing_estimate("sys", &raw, &with_doc, &[]).input_tokens;
    assert!(doc > full, "文档文字层要计入: {doc} vs {full}");
}

/// 估算时报缓存写只限 Claude 系:grok / gpt / composer 没有缓存写加价(审查意见)。
#[test]
fn 只有claude系报缓存写() {
    assert!(model_has_cache_write_surcharge("claude-fable-5-2"));
    assert!(model_has_cache_write_surcharge(" Claude-Opus-5-5 "));
    assert!(!model_has_cache_write_surcharge("grok-4.6"));
    assert!(!model_has_cache_write_surcharge("gpt-5.6-sol"));
    assert!(!model_has_cache_write_surcharge("composer-2.5"));
    assert!(!model_has_cache_write_surcharge("default"));
}
