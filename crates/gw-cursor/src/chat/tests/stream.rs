//! chat 测试:响应帧状态机。

use super::*;

#[test]
fn client_tool_id_drops_the_second_segment() {
    // 上游 call id 是两段用换行连的;换行进 id 会让客户端校验失败、日志断行。
    assert_eq!(
        client_tool_id("call-8268dbb8-a85a-4ea1-8b99-452b916b55de-0\nfc_1fd1cd59_0"),
        "call-8268dbb8-a85a-4ea1-8b99-452b916b55de-0"
    );
    // 没有第二段时原样(去空白)
    assert_eq!(client_tool_id("call-x-0"), "call-x-0");
    assert!(!client_tool_id("a\r\nb").contains(['\r', '\n']));
}

/// ⭐ exec 控制流集成测试(2026-08-10 带图 502 的根治路径):
/// 收到资产写调用帧后 —— ① 回执**真的进了请求体通道**(不是只在日志里说说);
/// ② 资产进了内存库;③ 同流随后的用量帧没被吞,收尾产出 Usage。
/// 解析/编码单测替代不了这条:它钉的是 stream_to_anthropic 的控制流本身。
#[tokio::test]
async fn 资产写调用有回执且不吞用量() {
    // 真上游抓的资产写调用帧(1x1 PNG,73B,write_args)。
    let write_frame: &[u8] = include_bytes!("../../../tests/fixtures/asset_echo_real.bin");
    // 用量帧 {1:{14:{1:100,2:5,3:90}}} —— 手写字节,绕开我方 Writer(自己写自己解是白测)。
    let usage_payload = [0x0au8, 0x08, 0x72, 0x06, 0x08, 0x64, 0x10, 0x05, 0x18, 0x5a];
    // 正文帧 {1:{1:{1:"红"}}}:收尾要求出过字,否则按 EmptyResponse 提前返回(那是
    // 另一条故意的保护路径,不是本测试要钉的)。
    let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2];
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
        Ok(bytes::Bytes::from(wire::frame(write_frame))),
        Ok(bytes::Bytes::from(wire::frame(&text_payload))),
        Ok(bytes::Bytes::from(wire::frame(&usage_payload))),
    ];
    let (btx, mut brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
    let assets = std::sync::Arc::new(crate::AssetStore::default());
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "claude-fable-5".into(),
        false,
        Some(btx),
        None,
        "conv-test".into(),
        assets.clone(),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        BuiltinXlate::default(),
        ChatUsage::default(),
        0,     // 本用例不关心模拟缓存,
        false, // cli_mode:测试默认 IDE 形态
        None,  // upload:本用例不走 wire v2 上传腿
        None,  // 本用例不关心影子捕获
    );
    use futures::StreamExt;
    let items: Vec<_> = out.collect().await;

    // ① 回执进了请求体通道,且是 AgentClientMessage{2: exec_client_message{3: …}}。
    let ack = brx
        .try_recv()
        .expect("资产写调用必须产生请求侧回执")
        .expect("回执帧必须是 Ok");
    let payload = wire::frame_payload(ack[0], &ack[5..]).expect("回执是合法 Connect 帧");
    let ecm = crate::protobuf::Reader::new(&payload)
        .find_map(|(f, v)| match (f, v) {
            (2, crate::protobuf::Value::Len(s)) => Some(s),
            _ => None,
        })
        .expect("回执顶层必须是 field 2(exec_client_message)");
    assert!(
        crate::protobuf::Reader::new(ecm).any(|(f, _)| f == 3),
        "回执必须带 field 3(write_result)"
    );

    // ② 资产进了内存库,路径与实物帧一致。
    let stored = assets
        .get(
            "conv-test",
            "/assets/attach-0-6bd00159-1e01-4924-b8c2-12f28cc81e53.png",
        )
        .expect("资产必须已存进内存库");
    assert_eq!(stored.len(), 73);
    assert!(stored.starts_with(b"\x89PNG"), "存的是原图字节");

    // ③ 用量没被吞:收尾产出 Usage(100/5/90),且 SSE 里 message_delta 带增量口径
    // (100-90=10 新增输入,缓存读取单列)。
    let usage = items.iter().find_map(|it| match it {
        Ok(StreamItem::Usage(u)) => Some(u),
        _ => None,
    });
    let usage = usage.expect("用量帧绝不能被 exec 处理吞掉");
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.cache_read_tokens, 90);
    assert_eq!(usage.real_cache_read_tokens, 90, "cursor 真实缓存应同步");
    let delta = items.iter().find_map(|it| match it {
        Ok(StreamItem::Sse(e)) if e.event == "message_delta" => Some(e),
        _ => None,
    });
    let delta = delta.expect("正常收尾必须发 message_delta");
    let u = &delta.data["usage"];
    assert_eq!(u["input_tokens"], 10, "增量口径:100-90");
    assert_eq!(u["output_tokens"], 5);
    assert_eq!(u["cache_read_input_tokens"], 90);
}

/// 上游用量帧 input/cache 全零 = 不可信(字段漂移/置零),必须落回请求体粗估,
/// 但上游给的 output 仍采用(它比估算准)。cache_read 由模拟器顶替(封顶 input),
/// 且模拟值绝不进 real_cache_read(对账列)。
#[tokio::test]
async fn 上游零用量帧回落请求体粗估() {
    let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2]; // "红"
                                                                                 // {1:{14:{1:0,2:7,3:0}}} —— input/cache 置零、output=7
    let usage_payload: &[u8] = &[0x0a, 0x08, 0x72, 0x06, 0x08, 0x00, 0x10, 0x07, 0x18, 0x00];
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
        Ok(bytes::Bytes::from(wire::frame(&text_payload))),
        Ok(bytes::Bytes::from(wire::frame(usage_payload))),
    ];
    let assets = std::sync::Arc::new(crate::AssetStore::default());
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "claude-fable-5".into(),
        false,
        None,
        None,
        "conv-test".into(),
        assets,
        std::sync::Arc::new(crate::TruncationNotices::default()),
        BuiltinXlate::default(),
        ChatUsage {
            input_tokens: 500,
            output_tokens: 1,
            ..Default::default()
        },
        400,   // 模拟缓存命中,
        false, // cli_mode:测试默认 IDE 形态
        None,  // upload:本用例不走 wire v2 上传腿
        None,  // 本用例不关心影子捕获
    );
    use futures::StreamExt;
    let items: Vec<_> = out.collect().await;
    let usage = items
        .iter()
        .find_map(|it| match it {
            Ok(StreamItem::Usage(u)) => Some(u),
            _ => None,
        })
        .expect("必须产出 Usage");
    assert_eq!(usage.input_tokens, 500, "零帧不可信,用请求体粗估值");
    assert_eq!(usage.cache_read_tokens, 400, "cached=0 时由模拟值顶替");
    assert_eq!(
        usage.real_cache_read_tokens, 0,
        "模拟值不能进对账列(real 只认上游自报)"
    );
    assert_eq!(usage.output_tokens, 7, "上游给的 output 采用");
}

/// 模拟值必须封顶 input_tokens:会话内容突变(压缩/截断)后,上一轮的命中
/// 可能超过本轮总输入,超帽会让增量 input 变 0、缓存部分被重复计费。
#[tokio::test]
async fn 模拟缓存封顶输入总量() {
    let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2]; // "红"
                                                                                 // {1:{14:{1:100,2:5,3:0}}} —— input=100, cached=0
    let usage_payload: &[u8] = &[0x0a, 0x08, 0x72, 0x06, 0x08, 0x64, 0x10, 0x05, 0x18, 0x00];
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
        Ok(bytes::Bytes::from(wire::frame(&text_payload))),
        Ok(bytes::Bytes::from(wire::frame(usage_payload))),
    ];
    let assets = std::sync::Arc::new(crate::AssetStore::default());
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "claude-fable-5".into(),
        false,
        None,
        None,
        "conv-test".into(),
        assets,
        std::sync::Arc::new(crate::TruncationNotices::default()),
        BuiltinXlate::default(),
        ChatUsage::default(),
        99999, // 模拟命中远超上游 input,
        false, // cli_mode:测试默认 IDE 形态
        None,  // upload:本用例不走 wire v2 上传腿
        None,  // 本用例不关心影子捕获
    );
    use futures::StreamExt;
    let items: Vec<_> = out.collect().await;
    let usage = items
        .iter()
        .find_map(|it| match it {
            Ok(StreamItem::Usage(u)) => Some(u),
            _ => None,
        })
        .expect("必须产出 Usage");
    assert_eq!(usage.input_tokens, 100, "上游可信帧的 input 不动");
    assert_eq!(usage.cache_read_tokens, 100, "模拟值必须封顶 input_tokens");
    assert_eq!(usage.real_cache_read_tokens, 0);
}

/// 上游自报真实命中时,模拟值**不许**顶替(真实 > 模拟);且 real 同步写入。
#[tokio::test]
async fn 上游用量同步写入真实缓存字段() {
    let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2]; // "红"
                                                                                 // {1:{14:{1:1000,2:5,3:900}}}
    let usage_payload: &[u8] = &[
        0x0a, 0x0a, 0x72, 0x08, 0x08, 0xe8, 0x07, 0x10, 0x05, 0x18, 0x84, 0x07,
    ];
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
        Ok(bytes::Bytes::from(wire::frame(&text_payload))),
        Ok(bytes::Bytes::from(wire::frame(usage_payload))),
    ];
    let assets = std::sync::Arc::new(crate::AssetStore::default());
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "claude-fable-5".into(),
        false,
        None,
        None,
        "conv-test".into(),
        assets,
        std::sync::Arc::new(crate::TruncationNotices::default()),
        BuiltinXlate::default(),
        ChatUsage {
            input_tokens: 1,
            ..Default::default()
        }, // fallback 不应被用到
        5000,  // 模拟值再大也不许盖过上游真实命中,
        false, // cli_mode:测试默认 IDE 形态
        None,  // upload:本用例不走 wire v2 上传腿
        None,  // 本用例不关心影子捕获
    );
    use futures::StreamExt;
    let items: Vec<_> = out.collect().await;
    let usage = items
        .iter()
        .find_map(|it| match it {
            Ok(StreamItem::Usage(u)) => Some(u),
            _ => None,
        })
        .expect("有用量帧时必须产出 Usage");
    assert_eq!(usage.input_tokens, 1000);
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.cache_read_tokens, 900);
    assert_eq!(
        usage.real_cache_read_tokens, 900,
        "上游用量必须同步写入 real_cache_read"
    );
}

/// 影子模式③:流干净收尾时暂存的尾部 `.3` 才入表;元数据与字节原样,描述符不透明。
///
/// ## ⚠️ 帧序必须照实物,不能自己编(2026-08-23 三轮抓包)
///
/// 实测每轮响应的尾段顺序**完全一致**:
///
/// ```text
/// 提交记录(field-4) → .3 描述符 ×3 → 1.14 用量帧(每轮恰好 1 个,末帧)
/// ```
///
/// 旧版本这个用例编的是「`.3` 然后用量帧、**没有提交记录**」,并且传 `cli_mode:
/// false`。两处都不成立:
/// - 描述符只存在于 CLI 形态,而生产只在 CLI 形态挂 `ShadowFeed`,IDE + 描述符
///   这个组合production 已经产不出来;
/// - 实物里提交记录**永远排在所有 `.3` 之前**,所以
///   `desc_acceptable = draining || commit_frame` 这道门(见捕获处)不会误伤任何
///   真实描述符 —— 缺提交记录的流是编出来的形态。
///
/// 另外 `PROTOCOL-agent-run.md` §20.1 说「CLI 响应无 `1.14` 用量帧」是**错的**:
/// 实测每轮都有,恰好一个,且是最后一帧。
#[tokio::test]
async fn 影子_干净收尾才入库() {
    let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2]; // 「好」
    let usage_payload = [0x0au8, 0x08, 0x72, 0x06, 0x08, 0x64, 0x10, 0x05, 0x18, 0x00];
    let desc = crate::run::descriptor_samples::turn1();
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
        Ok(bytes::Bytes::from(wire::frame(&text_payload))),
        // 提交记录先到 —— 实物顺序,也是 `.3` 被接受的前提。
        Ok(bytes::Bytes::from(wire::frame(&turn_commit_frame()))),
        Ok(bytes::Bytes::from(wire::frame(&desc))),
        Ok(bytes::Bytes::from(wire::frame(&usage_payload))),
    ];
    let shadow = std::sync::Arc::new(crate::DescriptorShadow::default());
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "claude-fable-5".into(),
        false,
        None,
        None,
        "conv-shadow".into(),
        std::sync::Arc::new(crate::AssetStore::default()),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        BuiltinXlate::default(),
        ChatUsage::default(),
        0,
        true, // cli_mode:描述符只存在于 CLI 形态(生产也只在 CLI 挂 ShadowFeed)
        None, // upload:本用例不走 wire v2 上传腿
        Some(crate::ShadowFeed {
            map: shadow.clone(),
            account_id: "acc-1".into(),
            fps: Vec::new(),
        }),
    );
    use futures::StreamExt;
    let items: Vec<_> = out.collect().await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "干净收尾不该产出错误项: {items:?}"
    );
    let (refs, bytes) = shadow
        .get_for_test("conv-shadow", "acc-1")
        .expect("干净收尾后描述符必须入库");
    assert_eq!(refs, 4, "turn1 样本 = 4 个 blob 引用");
    assert_eq!(
        bytes,
        run::descriptor_field3(&desc).expect("样本帧带 .3"),
        "入库字节必须与捕获的 .3 原始 payload 一致(不透明)"
    );
}

/// 影子模式③反面:流**中断**时暂存的 `.3` 绝不入库 —— 错描述符 = 回放时上下文
/// 缺一块 = 模型静默变傻(评审红线)。
///
/// ## ⚠️ 这个用例必须带提交记录帧,否则它为了错的原因通过
///
/// `desc_acceptable = draining || commit_frame` 那道门在「中断不入库」之前就会
/// 拒掉没有提交记录的 `.3`。如果这里不发提交记录,那么即使**中断分支被改坏**
/// (比如误把中断也算干净收尾)这个用例照样绿 —— 它考验的是门,不是中断。
///
/// 实物顺序是「提交记录 → `.3` → 用量帧」,所以这里发到 `.3` 就断:描述符已被
/// 接受进 staged,但流没走到用量帧/trailer,`saw_end` 仍为假 = 真中断。
#[tokio::test]
async fn 影子_流中断不入库() {
    let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2]; // 「好」
    let desc = crate::run::descriptor_samples::turn2();
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
        Ok(bytes::Bytes::from(wire::frame(&text_payload))),
        // 提交记录先到 → `.3` 会被接受进 staged(门放行)。
        Ok(bytes::Bytes::from(wire::frame(&turn_commit_frame()))),
        Ok(bytes::Bytes::from(wire::frame(&desc))),
        // 之后直接断流:实物里这里该来用量帧。
    ];
    let shadow = std::sync::Arc::new(crate::DescriptorShadow::default());
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "claude-fable-5".into(),
        false,
        None,
        None,
        "conv-shadow".into(),
        std::sync::Arc::new(crate::AssetStore::default()),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        BuiltinXlate::default(),
        ChatUsage::default(),
        0,
        false, // cli_mode:测试默认 IDE 形态
        None,  // upload:本用例不走 wire v2 上传腿
        Some(crate::ShadowFeed {
            map: shadow.clone(),
            account_id: "acc-1".into(),
            fps: Vec::new(),
        }),
    );
    use futures::StreamExt;
    let items: Vec<_> = out.collect().await;
    assert!(
        items.iter().any(|i| i.is_err()),
        "中断的流必须产出截断错误,不许伪装成功"
    );
    assert!(
        shadow.get_for_test("conv-shadow", "acc-1").is_none(),
        "中断的流绝不能提交描述符"
    );
}

// ── 挂流续跑(crate::held)────────────────────────────────────────────

/// 2026-11-10:模型调 Cursor **自带**读文件(界面帧 `1.2.2.8` + exec `read_args`)时同样挂起 ——
/// 生产 54% 的重建紧跟在自带工具调用之后(流没挂,下一轮只能按完整历史重建、丢服务端总结)。
/// 界面帧翻成调用方的 Read 交付;exec 帧按 `tool_call_id` 配上后挂起,挂起记录带类型与路径。
#[tokio::test(start_paused = true)]
async fn 自带读文件调用轮挂起上游流() {
    use futures::StreamExt;
    let acc = "acc-hold-builtin-read";
    let (up_tx, up) = chan_upstream();
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    up_tx.send(builtin_read_wire("/w/a.txt")).unwrap();
    up_tx.send(exec_read_args_wire(5, "ex-r", "/w/a.txt", "call-r-1")).unwrap();
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_hold_x(
        up,
        "conv-hold-br",
        Some(btx),
        HoldOpts { account_id: Some(acc.into()), resume: None },
        cc_read_bash_xlate(),
    )
    .collect()
    .await;
    assert!(items.iter().all(|i| i.is_ok()), "{:?}", items.iter().find_map(|i| i.as_ref().err()));
    assert_eq!(tool_use_ids(&items), vec!["call-r-1".to_string()], "只交付一次(exec 帧不能再翻成第二个调用)");
    assert_eq!(stop_reason_of(&items).as_deref(), Some("tool_use"));
    assert!(t0.elapsed() < EXEC_COLLECT_GRACE, "exec 帧到了就收工,不等宽限");
    let run = crate::held::registry().take(acc, &["call-r-1"]).expect("自带读文件调用轮应已挂起");
    assert_eq!(
        run.pending,
        vec![crate::held::PendingExec {
            tool_use_id: "call-r-1".into(),
            id: 5,
            exec_id: "ex-r".into(),
            kind: crate::held::PendingKind::Read { path: "/w/a.txt".into() },
        }]
    );
}

/// 自带终端:实测走流式终端 exec(`shell_stream_args`,分支 14),挂起记录带命令。
#[tokio::test(start_paused = true)]
async fn 自带终端调用轮挂起上游流() {
    use futures::StreamExt;
    let acc = "acc-hold-builtin-shell";
    let (up_tx, up) = chan_upstream();
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    up_tx.send(builtin_terminal_wire("ls -la")).unwrap();
    up_tx.send(exec_shell_stream_args_wire(6, "ex-s", "ls -la", "call-b-0")).unwrap();
    let items: Vec<_> = run_hold_x(
        up,
        "conv-hold-bs",
        Some(btx),
        HoldOpts { account_id: Some(acc.into()), resume: None },
        cc_read_bash_xlate(),
    )
    .collect()
    .await;
    assert!(items.iter().all(|i| i.is_ok()), "{:?}", items.iter().find_map(|i| i.as_ref().err()));
    assert_eq!(tool_use_ids(&items), vec!["call-b-0".to_string()]);
    let run = crate::held::registry().take(acc, &["call-b-0"]).expect("自带终端调用轮应已挂起");
    assert_eq!(run.pending.len(), 1);
    assert_eq!(run.pending[0].id, 6);
    assert_eq!(
        run.pending[0].kind,
        crate::held::PendingKind::ShellStream { cwd: String::new() }
    );
}

/// exec 帧迟迟不来:宽限到期按旧行为收尾(交付 tool_use、不挂起),不拖成停滞。
#[tokio::test(start_paused = true)]
async fn 自带工具等不到exec帧按旧行为收尾() {
    use futures::StreamExt;
    let acc = "acc-hold-builtin-noexec";
    let (up_tx, up) = chan_upstream();
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    up_tx.send(builtin_read_wire("/w/b.txt")).unwrap();
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_hold_x(
        up,
        "conv-hold-bn",
        Some(btx),
        HoldOpts { account_id: Some(acc.into()), resume: None },
        cc_read_bash_xlate(),
    )
    .collect()
    .await;
    assert!(items.iter().all(|i| i.is_ok()));
    assert_eq!(tool_use_ids(&items), vec!["call-r-1".to_string()]);
    assert_eq!(stop_reason_of(&items).as_deref(), Some("tool_use"));
    assert_eq!(t0.elapsed(), EXEC_COLLECT_GRACE);
    assert!(crate::held::registry().take(acc, &["call-r-1"]).is_none());
    drop(up_tx);
}

/// 挂起没开(无挂起账号):界面帧翻译后立即收尾(旧行为,不等 exec 帧)。
#[tokio::test(start_paused = true)]
async fn 不挂起时自带工具照旧立即收尾() {
    use futures::StreamExt;
    let (up_tx, up) = chan_upstream();
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    up_tx.send(builtin_read_wire("/w/c.txt")).unwrap();
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_hold_x(up, "conv-nohold-b", Some(btx), HoldOpts::default(), cc_read_bash_xlate())
        .collect()
        .await;
    assert_eq!(tool_use_ids(&items), vec!["call-r-1".to_string()]);
    assert_eq!(t0.elapsed().as_secs(), 0, "不挂起就不等 exec 帧");
    drop(up_tx);
}

/// 等 exec 期间来了配不上的 exec(调用 id 对不上 / 缺 id / 别的自带工具):不能再翻成第二个
/// tool_use(客户端会执行两次),也不能收口成空回复 —— 按旧行为立即收工、不挂起。
#[tokio::test(start_paused = true)]
async fn 等自带工具exec期间配不上的exec按旧行为收工() {
    use futures::StreamExt;
    let acc = "acc-hold-builtin-mismatch";
    let (up_tx, up) = chan_upstream();
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    up_tx.send(builtin_read_wire("/w/a.txt")).unwrap();
    up_tx.send(exec_read_args_wire(5, "ex-r", "/w/other.txt", "call-zzz")).unwrap();
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_hold_x(
        up,
        "conv-hold-bm",
        Some(btx),
        HoldOpts { account_id: Some(acc.into()), resume: None },
        cc_read_bash_xlate(),
    )
    .collect()
    .await;
    assert!(items.iter().all(|i| i.is_ok()), "{:?}", items.iter().find_map(|i| i.as_ref().err()));
    assert_eq!(tool_use_ids(&items), vec!["call-r-1".to_string()], "配不上的 exec 不能再交付一次");
    assert_eq!(stop_reason_of(&items).as_deref(), Some("tool_use"));
    assert_eq!(t0.elapsed().as_secs(), 0, "配不上就立即收工,不等宽限");
    assert!(crate::held::registry().take(acc, &["call-r-1"]).is_none());
    drop(up_tx);
}

/// 界面帧与 exec 帧之间夹了一个 turn_commit:exec 帧会让排水段退出(清掉收尾标记),
/// 配齐挂起时必须补回,否则正常的工具轮被判成「流中断」。
#[tokio::test(start_paused = true)]
async fn 自带工具exec前夹提交帧照样挂起不报错() {
    use futures::StreamExt;
    let acc = "acc-hold-builtin-commit";
    let (up_tx, up) = chan_upstream();
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    up_tx.send(builtin_read_wire("/w/a.txt")).unwrap();
    up_tx.send(turn_commit_frame()).unwrap();
    up_tx.send(exec_read_args_wire(5, "ex-r", "/w/a.txt", "call-r-1")).unwrap();
    let items: Vec<_> = run_hold_x(
        up,
        "conv-hold-bc",
        Some(btx),
        HoldOpts { account_id: Some(acc.into()), resume: None },
        cc_read_bash_xlate(),
    )
    .collect()
    .await;
    assert!(items.iter().all(|i| i.is_ok()), "{:?}", items.iter().find_map(|i| i.as_ref().err()));
    assert_eq!(tool_use_ids(&items), vec!["call-r-1".to_string()]);
    assert_eq!(stop_reason_of(&items).as_deref(), Some("tool_use"));
    assert!(crate::held::registry().take(acc, &["call-r-1"]).is_some(), "应已挂起");
    drop(up_tx);
}

/// 同一调用的界面帧来了两次(状态更新):只交付一次,exec 到了照样挂起。
#[tokio::test(start_paused = true)]
async fn 自带工具界面帧重复只交付一次() {
    use futures::StreamExt;
    let acc = "acc-hold-builtin-dup";
    let (up_tx, up) = chan_upstream();
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    up_tx.send(builtin_read_wire("/w/a.txt")).unwrap();
    up_tx.send(builtin_read_wire("/w/a.txt")).unwrap();
    up_tx.send(exec_read_args_wire(5, "ex-r", "/w/a.txt", "call-r-1")).unwrap();
    let items: Vec<_> = run_hold_x(
        up,
        "conv-hold-bd",
        Some(btx),
        HoldOpts { account_id: Some(acc.into()), resume: None },
        cc_read_bash_xlate(),
    )
    .collect()
    .await;
    assert!(items.iter().all(|i| i.is_ok()));
    assert_eq!(tool_use_ids(&items), vec!["call-r-1".to_string()]);
    assert!(crate::held::registry().take(acc, &["call-r-1"]).is_some(), "应已挂起");
    drop(up_tx);
}

/// 自带工具与 MCP 工具并行调用:两个调用都交付,本轮不挂起(旧行为)。
#[tokio::test(start_paused = true)]
async fn 自带工具与mcp并行调用都交付且不挂起() {
    use futures::StreamExt;
    let acc = "acc-hold-builtin-mixed";
    let ui = include_bytes!("../../../tests/fixtures/tool_call_external_real.bin").to_vec();
    let mcp = run::parse_tool_call(&ui).expect("实物帧应解出工具调用");
    let (up_tx, up) = chan_upstream();
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    up_tx.send(builtin_read_wire("/w/a.txt")).unwrap();
    up_tx.send(ui).unwrap();
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_hold_x(
        up,
        "conv-hold-bx",
        Some(btx),
        HoldOpts { account_id: Some(acc.into()), resume: None },
        cc_read_bash_xlate(),
    )
    .collect()
    .await;
    assert!(items.iter().all(|i| i.is_ok()), "{:?}", items.iter().find_map(|i| i.as_ref().err()));
    assert_eq!(
        tool_use_ids(&items),
        vec!["call-r-1".to_string(), client_tool_id(&mcp.id)]
    );
    assert_eq!(stop_reason_of(&items).as_deref(), Some("tool_use"));
    assert_eq!(t0.elapsed().as_secs(), 0, "混合并行立即收工");
    assert!(crate::held::registry().take(acc, &["call-r-1"]).is_none());
    drop(up_tx);
}

/// 续跑段:上游再发一次已回结果的自带工具界面帧(完成态回显),不能再交给客户端;
/// 之后的新正文照常交付。
#[tokio::test(start_paused = true)]
async fn 续跑段自带工具回显不重复交付() {
    use futures::StreamExt;
    let acc = "acc-hold-builtin-resume";
    let (up_tx, up) = chan_upstream();
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    up_tx.send(builtin_read_wire("/w/a.txt")).unwrap();
    up_tx.send(exec_read_args_wire(5, "ex-r", "/w/a.txt", "call-r-1")).unwrap();
    let items: Vec<_> = run_hold_x(
        up,
        "conv-hold-br2",
        Some(btx),
        HoldOpts { account_id: Some(acc.into()), resume: None },
        cc_read_bash_xlate(),
    )
    .collect()
    .await;
    assert_eq!(tool_use_ids(&items), vec!["call-r-1".to_string()]);
    let run = crate::held::registry().take(acc, &["call-r-1"]).expect("应已挂起");

    up_tx.send(builtin_read_wire("/w/a.txt")).unwrap();
    up_tx.send(TEXT_HAO.to_vec()).unwrap();
    up_tx.send(turn_commit_frame()).unwrap();
    let crate::held::HeldRun { byte_stream, decoder, body, heartbeat, .. } = run;
    let items2: Vec<_> = run_hold_x(
        byte_stream,
        "conv-hold-br2",
        Some(body),
        HoldOpts {
            account_id: Some(acc.into()),
            resume: Some(ResumeState { decoder, heartbeat, answered_ids: vec!["call-r-1".into()] }),
        },
        cc_read_bash_xlate(),
    )
    .collect()
    .await;
    assert!(items2.iter().all(|i| i.is_ok()), "{:?}", items2.iter().find_map(|i| i.as_ref().err()));
    assert!(tool_use_ids(&items2).is_empty(), "已回结果的自带工具回显不能再交付");
    assert_eq!(stop_reason_of(&items2).as_deref(), Some("end_turn"));
}

/// 开关开:MCP 工具调用轮收尾时挂起上游流,exec 编号与客户端看到的 tool_use id 对上;
/// 客户端带结果回来时从表里取出续跑:上游对已回结果调用的回显被忽略,新正文照常交付。
#[tokio::test(start_paused = true)]
async fn 工具调用轮挂起上游流并能续跑() {
    use futures::StreamExt;
    let acc = "acc-hold-resume";
    let (up_tx, up) = chan_upstream();
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    up_tx.send(TEXT_HAO.to_vec()).unwrap();
    up_tx
        .send(exec_mcp_wire(9, "ex-9", "toolu_hold1", "read", "/w/a.txt"))
        .unwrap();
    let items: Vec<_> = run_hold(
        up,
        "conv-hold",
        Some(btx),
        HoldOpts {
            account_id: Some(acc.into()),
            resume: None,
        },
    )
    .collect()
    .await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "{:?}",
        items.iter().find_map(|i| i.as_ref().err())
    );
    assert_eq!(tool_use_ids(&items), vec!["toolu_hold1".to_string()]);
    assert_eq!(stop_reason_of(&items).as_deref(), Some("tool_use"));

    let run = crate::held::registry()
        .take(acc, &["toolu_hold1"])
        .expect("工具轮收尾后上游流应已挂起");
    assert_eq!(
        run.pending,
        vec![crate::held::PendingExec {
            tool_use_id: "toolu_hold1".into(),
            id: 9,
            exec_id: "ex-9".into(),
            kind: crate::held::PendingKind::Mcp,
        }]
    );

    // 续跑段:上游先回显一次已回结果的调用,再出新正文、本轮提交。
    up_tx
        .send(exec_mcp_wire(9, "ex-9", "toolu_hold1", "read", "/w/a.txt"))
        .unwrap();
    up_tx.send(TEXT_HAO.to_vec()).unwrap();
    up_tx.send(turn_commit_frame()).unwrap();
    let crate::held::HeldRun {
        byte_stream,
        decoder,
        body,
        heartbeat,
        ..
    } = run;
    let items2: Vec<_> = run_hold(
        byte_stream,
        "conv-hold",
        Some(body),
        HoldOpts {
            account_id: Some(acc.into()),
            resume: Some(ResumeState {
                decoder,
                heartbeat,
                answered_ids: vec!["toolu_hold1".into()],
            }),
        },
    )
    .collect()
    .await;
    assert!(
        items2.iter().all(|i| i.is_ok()),
        "{:?}",
        items2.iter().find_map(|i| i.as_ref().err())
    );
    assert!(
        tool_use_ids(&items2).is_empty(),
        "已回结果的调用回显不能再交给客户端"
    );
    assert_eq!(stop_reason_of(&items2).as_deref(), Some("end_turn"));
    let text: String = items2
        .iter()
        .filter_map(|i| match i {
            Ok(StreamItem::Sse(ev)) if ev.event == "content_block_delta" => {
                ev.data["delta"]["text"].as_str().map(str::to_string)
            }
            _ => None,
        })
        .collect();
    assert!(text.contains('红'), "续跑段正文要交付: {text:?}");
    assert!(crate::held::registry()
        .take(acc, &["toolu_hold1"])
        .is_none());
}

/// 生产常见路径:先来带参数的界面帧(`1.2.2.15`,用抓包实物),再来 exec 形态的回声。
/// 收到回声时 exec 编号收齐 → 挂起。
#[tokio::test(start_paused = true)]
async fn 实物界面帧加exec回声时挂起() {
    use futures::StreamExt;
    let acc = "acc-hold-ui";
    let ui = include_bytes!("../../../tests/fixtures/tool_call_external_real.bin").to_vec();
    let tc = run::parse_tool_call(&ui).expect("实物帧应解出工具调用");
    let cid = client_tool_id(&tc.id);
    let (up_tx, up) = chan_upstream();
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    up_tx.send(ui).unwrap();
    up_tx
        .send(exec_mcp_wire(5, "ex-5", &tc.id, &tc.name, "/w/c.txt"))
        .unwrap();
    let items: Vec<_> = run_hold(
        up,
        "conv-hold-ui",
        Some(btx),
        HoldOpts {
            account_id: Some(acc.into()),
            resume: None,
        },
    )
    .collect()
    .await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "{:?}",
        items.iter().find_map(|i| i.as_ref().err())
    );
    assert_eq!(tool_use_ids(&items), vec![cid.clone()]);
    let run = crate::held::registry()
        .take(acc, &[cid.as_str()])
        .expect("应已挂起");
    assert_eq!(
        (run.pending[0].id, run.pending[0].exec_id.as_str()),
        (5, "ex-5")
    );
}

/// 开关关(默认):行为同旧版,什么都不挂。
#[tokio::test(start_paused = true)]
async fn 开关关时不挂起() {
    use futures::StreamExt;
    let (up_tx, up) = chan_upstream();
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    up_tx
        .send(exec_mcp_wire(3, "ex-3", "toolu_nohold", "read", "/w/b.txt"))
        .unwrap();
    let items: Vec<_> = run_hold(up, "conv-nohold", Some(btx), HoldOpts::default())
        .collect()
        .await;
    assert_eq!(tool_use_ids(&items), vec!["toolu_nohold".to_string()]);
    assert!(crate::held::registry()
        .take("acc-nohold", &["toolu_nohold"])
        .is_none());
}

#[test]
fn exec编号配对只认id相等或单发() {
    let call = |id: &str| run::ToolCall {
        id: id.into(),
        name: "read".into(),
        args: vec![],
    };
    let ex = |cid: &str, n: u64| {
        (cid.to_string(), "read".to_string(), n, format!("e{n}"), crate::held::PendingKind::Mcp)
    };
    // 单发:id 写法不同也配上。
    let one = pair_pending_execs(&[call("toolu_a\nfc_1")], &[ex("whatever", 1)]).unwrap();
    assert_eq!((one[0].tool_use_id.as_str(), one[0].id), ("toolu_a", 1));
    // 并行:按 id 配,顺序无关。
    let two = pair_pending_execs(&[call("a"), call("b")], &[ex("b", 2), ex("a", 1)]).unwrap();
    assert_eq!(two.iter().map(|p| p.id).collect::<Vec<_>>(), vec![1, 2]);
    // 并行里有对不上的:不挂。
    assert!(pair_pending_execs(&[call("a"), call("b")], &[ex("a", 1), ex("c", 3)]).is_none());
    // exec 帧不够:不挂。
    assert!(pair_pending_execs(&[call("a"), call("b")], &[ex("a", 1)]).is_none());
}

/// Finding 3:turn_commit **不是流尾**。真实帧序(turn_commit → .3 →
/// .3(ref 更多) → 尾帧 → 流结束)下,影子必须捕到尾部那份 ref 更多的
/// 描述符,而不是 turn_commit 那一刻的旧份。
#[tokio::test]
async fn 影子_turn_commit后继续消费_捕到尾部描述符() {
    let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2]; // 「好」
    let desc_early = crate::run::descriptor_samples::turn1(); // 4 个 blob 引用
    let desc_final = crate::run::descriptor_samples::turn2(); // 6 个 blob 引用
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
        Ok(bytes::Bytes::from(wire::frame(&text_payload))),
        Ok(bytes::Bytes::from(wire::frame(&turn_commit_frame()))),
        Ok(bytes::Bytes::from(wire::frame(&desc_early))),
        Ok(bytes::Bytes::from(wire::frame(&desc_final))),
        // 流自然结束(CLI 形态常见,无 trailer):saw_end 已被 turn_commit
        // 置位 = 干净收尾,影子应提交**最后一份**暂存。
    ];
    let shadow = std::sync::Arc::new(crate::DescriptorShadow::default());
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "claude-fable-5".into(),
        false,
        None,
        None,
        "conv-shadow".into(),
        std::sync::Arc::new(crate::AssetStore::default()),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        BuiltinXlate::default(),
        ChatUsage::default(),
        0,
        true, // cli_mode:turn_commit 分支只在 CLI 形态走
        None, // upload:本用例不走 wire v2 上传腿
        Some(crate::ShadowFeed {
            map: shadow.clone(),
            account_id: "acc-1".into(),
            fps: Vec::new(),
        }),
    );
    use futures::StreamExt;
    let items: Vec<_> = out.collect().await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "干净收尾不该产出错误项: {items:?}"
    );
    let (refs, bytes) = shadow
        .get_for_test("conv-shadow", "acc-1")
        .expect("干净收尾后描述符必须入库");
    assert_eq!(
        refs, 6,
        "必须捕到尾部 ref 更多的那份,而非 turn_commit 时的旧份"
    );
    assert_eq!(
        bytes,
        run::descriptor_field3(&desc_final).expect("样本帧带 .3"),
        "入库的必须是尾部那份的原始字节"
    );
}

/// 一个字、一个工具调用都没出,turn_commit 之后尾部却是**错误 trailer**:
/// 这一轮是上游失败,必须把上游错误原样报出去(gw-app 才会重试/换号),
/// 不能当成功,更不能退化成一句没信息量的「空回复」(2026-11-07 fable 续轮实证)。
#[tokio::test]
async fn 零产出且尾部是错误trailer时报上游错误() {
    let mut trailer = wire::frame(
            br#"{"error":{"code":"unavailable","details":[{"debug":{"error":"ERROR_UPSTREAM","details":{"title":"Model unavailable","detail":"try again"}}}]}}"#,
        );
    trailer[0] = 0x02;
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
        Ok(bytes::Bytes::from(wire::frame(&turn_commit_frame()))),
        Ok(bytes::Bytes::from(trailer)),
    ];
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "claude-fable-5-1".into(),
        false,
        None,
        None,
        "conv-empty-trailer".into(),
        std::sync::Arc::new(crate::AssetStore::default()),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        BuiltinXlate::default(),
        ChatUsage::default(),
        0,
        true,
        None,
        None,
    );
    use futures::StreamExt;
    let items: Vec<_> = out.collect().await;
    let err = items
        .iter()
        .find_map(|i| i.as_ref().err())
        .expect("零产出 + 错误 trailer 必须是错误");
    assert_ne!(err.kind, UpstreamErrorKind::EmptyResponse, "{err:?}");
    assert!(
        err.message.contains("Model unavailable") || err.message.contains("ERROR_UPSTREAM"),
        "要带上游原因: {err:?}"
    );
}

/// 2026-11-07 生产 fable「出字后 150s 只剩心跳」:模型在拼大工具调用时上游流的是
/// `1.7` 工具参数增量。它们要续停滞闸的命,并且给下游交空增量保活。
#[tokio::test(start_paused = true)]
async fn 工具参数增量续停滞闸的命并交ping() {
    use futures::StreamExt;
    let mut seq = vec![(0, TEXT_HAO.to_vec())];
    // 4 个增量各隔 100s:总共 400s,远超 150s,但每段都不到 150s。
    for _ in 0..4 {
        seq.push((100, PARTIAL_TOOL.to_vec()));
    }
    seq.push((1, turn_commit_frame()));
    let items: Vec<_> = run_timed(seq, "conv-work-alive").collect().await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "干活帧期间不该判停滞: {:?}",
        items.iter().find_map(|i| i.as_ref().err())
    );
    let pings = items
        .iter()
        .filter(|i| {
            matches!(i, Ok(StreamItem::Sse(ev))
                if ev.event == "content_block_delta" && ev.data["delta"]["text"] == "")
        })
        .count();
    assert!(pings >= 3, "每段干活都该交空增量保活,实得 {pings}");
}

/// 只有干活帧、没有可见输出的阶段封顶 [`WORK_ONLY_CAP`]:上游病态时不能无限挂着。
#[tokio::test(start_paused = true)]
async fn 只有工具参数增量也有总上限() {
    use futures::StreamExt;
    let mut seq = vec![(0, TEXT_HAO.to_vec())];
    for _ in 0..20 {
        seq.push((100, PARTIAL_TOOL.to_vec()));
    }
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_timed(seq, "conv-work-cap").collect().await;
    let err = items
        .iter()
        .find_map(|i| i.as_ref().err())
        .expect("超过总上限必须报错");
    assert!(err.message.contains("工具参数"), "{err:?}");
    let waited = t0.elapsed().as_secs();
    assert!(
        (600..=700).contains(&waited),
        "应在 600s 上限附近收口,实际 {waited}s"
    );
}

/// 真只有心跳时仍按原停滞闸收口(干活帧判定没有放宽心跳)。
#[tokio::test(start_paused = true)]
async fn 只有心跳仍按150秒收口() {
    use futures::StreamExt;
    let hb = vec![0x0au8, 0x02, 0x6a, 0x00];
    let mut seq = vec![(0, TEXT_HAO.to_vec())];
    for _ in 0..30 {
        seq.push((10, hb.clone()));
    }
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_timed(seq, "conv-hb-only").collect().await;
    let err = items
        .iter()
        .find_map(|i| i.as_ref().err())
        .expect("只有心跳必须报错");
    assert!(err.message.contains("只有心跳"), "{err:?}");
    assert_eq!(t0.elapsed().as_secs(), 150);
}

/// 连心跳都没有(连接「只发不收」)时才报**连接卡死**,调用方据此重建这个号的连接;
/// 一直有心跳(连接活着、上游只是不出字)时不报这个 —— 重建连接无益(审查意见)。
#[tokio::test(start_paused = true)]
async fn 连心跳都没有才报连接卡死() {
    use futures::StreamExt;
    let items: Vec<_> = run_timed(vec![(0, TEXT_HAO.to_vec())], "conv-dead").collect().await;
    let err = items.iter().find_map(|i| i.as_ref().err()).expect("必须报错");
    assert!(is_connection_stall(err), "{err:?}");

    let hb = vec![0x0au8, 0x02, 0x6a, 0x00];
    let mut seq = vec![(0, TEXT_HAO.to_vec())];
    for _ in 0..30 {
        seq.push((10, hb.clone()));
    }
    let items: Vec<_> = run_timed(seq, "conv-hb-alive").collect().await;
    let err = items.iter().find_map(|i| i.as_ref().err()).expect("必须报错");
    assert!(!is_connection_stall(err), "有心跳不算连接卡死: {err:?}");
}

/// 2026-11-07 curtest 实测:模型一次写入长文时,`1.7` 之后上游**只发心跳**直到整个
/// 调用拼完。组装期间不能按 150s 判停滞,要等到工具调用,并且持续交空增量保活。
#[tokio::test(start_paused = true)]
async fn 组装大工具调用期间只有心跳也能等到交付() {
    use futures::StreamExt;
    let hb = vec![0x0au8, 0x02, 0x6a, 0x00];
    let mut seq = vec![(0, TEXT_HAO.to_vec()), (0, PARTIAL_TOOL.to_vec())];
    for _ in 0..30 {
        seq.push((10, hb.clone())); // 300s 只有心跳
    }
    seq.push((1, exec_write_wire("/w/paper.md", "长文全文")));
    let xlate = BuiltinXlate {
        write_file: Some(WriteFileTarget::Direct {
            name: "write".into(),
            path_key: "filePath".into(),
            content_key: "content".into(),
        }),
        ..Default::default()
    };
    let items: Vec<_> = run_timed_full(seq, "conv-assembly", None, true, xlate)
        .collect()
        .await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "组装期间不该判停滞: {:?}",
        items.iter().find_map(|i| i.as_ref().err())
    );
    let has_tool = items.iter().any(|i| {
        matches!(i, Ok(StreamItem::Sse(ev))
                if ev.event == "content_block_start" && ev.data.to_string().contains("tool_use"))
    });
    assert!(has_tool, "拼完的写文件调用必须交付成 tool_use");
    let pings = items
        .iter()
        .filter(|i| {
            matches!(i, Ok(StreamItem::Sse(ev))
                if ev.event == "content_block_delta" && ev.data["delta"]["text"] == "")
        })
        .count();
    assert!(
        pings >= 15,
        "300s 组装期间每 15s 一个空增量保活,实得 {pings}"
    );
}

/// token 计数帧同样受「只干活不出字」600s 封顶:真挂死(计数一直变)也跑不掉。
#[tokio::test(start_paused = true)]
async fn token计数帧也受600秒上限() {
    use futures::StreamExt;
    let td = vec![0x0au8, 0x04, 0x42, 0x02, 0x08, 0x03];
    let mut seq = vec![(0, TEXT_HAO.to_vec())];
    for _ in 0..80 {
        seq.push((10, td.clone()));
    }
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_timed(seq, "conv-token-delta-cap").collect().await;
    let err = items
        .iter()
        .find_map(|i| i.as_ref().err())
        .expect("超过总上限必须报错");
    assert!(err.message.contains("工具参数"), "{err:?}");
    let waited = t0.elapsed().as_secs();
    assert!(
        (600..=700).contains(&waited),
        "应在 600s 上限附近收口,实际 {waited}s"
    );
}

/// token 数为 0 的 `1.8` 不算干活(仍按 150s 收口)。
#[tokio::test(start_paused = true)]
async fn 零token计数帧不算干活() {
    use futures::StreamExt;
    let td0 = vec![0x0au8, 0x04, 0x42, 0x02, 0x08, 0x00];
    let mut seq = vec![(0, TEXT_HAO.to_vec())];
    for _ in 0..30 {
        seq.push((10, td0.clone()));
    }
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_timed(seq, "conv-token-zero").collect().await;
    assert!(items.iter().any(|i| i.is_err()), "零计数帧不能续命");
    assert_eq!(t0.elapsed().as_secs(), 150);
}

/// 组装也有上限:开始组装后 600s 还没拼完,按上游错误收口。
#[tokio::test(start_paused = true)]
async fn 组装超过上限报错() {
    use futures::StreamExt;
    let hb = vec![0x0au8, 0x02, 0x6a, 0x00];
    let mut seq = vec![(0, TEXT_HAO.to_vec()), (0, PARTIAL_TOOL.to_vec())];
    for _ in 0..80 {
        seq.push((10, hb.clone()));
    }
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_timed(seq, "conv-assembly-cap").collect().await;
    let err = items
        .iter()
        .find_map(|i| i.as_ref().err())
        .expect("组装超上限必须报错");
    assert!(err.message.contains("工具参数"), "{err:?}");
    assert_eq!(t0.elapsed().as_secs(), WORK_ONLY_CAP.as_secs());
}

/// 2026-11-07 生产:组装工具参数后服务端推联网搜索请示(顶层 7),我们不答,
/// 它就只发心跳,按组装上限干耗 600s。现在:当场批准,模型继续出字正常收尾。
#[tokio::test(start_paused = true)]
async fn 服务端请示当场应答后继续交付() {
    use futures::StreamExt;
    let (btx, mut brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    let hb = vec![0x0au8, 0x02, 0x6a, 0x00];
    // {7: {1: 5, 2: {1: {1: "rust"}}}}
    let query = vec![
        0x3A, 0x0C, 0x08, 0x05, 0x12, 0x08, 0x0A, 0x06, 0x0A, 0x04, b'r', b'u', b's', b't',
    ];
    let seq = vec![
        (0, TEXT_HAO.to_vec()),
        (0, PARTIAL_TOOL.to_vec()),
        (1, query),
        (5, hb.clone()),
        (5, TEXT_HAO.to_vec()),
        (
            0,
            vec![0x0a, 0x08, 0x72, 0x06, 0x08, 0x64, 0x10, 0x05, 0x18, 0x5a],
        ),
    ];
    let items: Vec<_> = run_timed_with(seq, "conv-iq-answer", Some(btx), false)
        .collect()
        .await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "{:?}",
        items.iter().find_map(|i| i.as_ref().err())
    );
    let mut sent = Vec::new();
    while let Ok(Ok(b)) = brx.try_recv() {
        sent.push(b);
    }
    // 批准帧 {6: {1: 5, 2: {1: {}}}}(分帧后在 5 字节帧头之后)。
    let approve = [0x32u8, 0x06, 0x08, 0x05, 0x12, 0x02, 0x0A, 0x00];
    assert!(
        sent.iter()
            .any(|b| b.windows(approve.len()).any(|w| w == approve)),
        "要在请求侧回批准帧: {sent:?}"
    );
}

/// 答不了的请示(setup_vm):组装计时作废,按 150s 停滞收口,不再耗满 600s。
#[tokio::test(start_paused = true)]
async fn 答不了的请示按停滞闸收口() {
    use futures::StreamExt;
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    let hb = vec![0x0au8, 0x02, 0x6a, 0x00];
    // {7: {1: 1, 8: {}}}
    let query = vec![0x3A, 0x04, 0x08, 0x01, 0x42, 0x00];
    let mut seq = vec![
        (0, TEXT_HAO.to_vec()),
        (0, PARTIAL_TOOL.to_vec()),
        (0, query),
    ];
    for _ in 0..80 {
        seq.push((10, hb.clone()));
    }
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_timed_with(seq, "conv-iq-stall", Some(btx), false)
        .collect()
        .await;
    assert!(items.iter().any(|i| i.is_err()), "答不了要按停滞报错");
    assert!(
        t0.elapsed().as_secs() < 200,
        "不能按组装上限耗满: {}s",
        t0.elapsed().as_secs()
    );
}

/// 审查发现:exec 读帧由资产库应答后,内建工具等待必须清掉,20s 到期不能把轮次掐断。
#[tokio::test(start_paused = true)]
async fn 资产库应答读帧后不被等待计时误收口() {
    use futures::StreamExt;
    let assets = std::sync::Arc::new(crate::AssetStore::default());
    assert!(assets.put("conv-asset-read", "/assets/a.png", b"PNG"));
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    // exec 读请求 {2:{1:5, 7:{1:"/assets/a.png"}}}
    let path = b"/assets/a.png";
    let mut rd = vec![0x0A, path.len() as u8];
    rd.extend_from_slice(path);
    let mut inner = vec![0x08, 0x05, 0x3A, rd.len() as u8];
    inner.extend(rd);
    let mut exec_read = vec![0x12, inner.len() as u8];
    exec_read.extend(inner);
    let seq = vec![
        (0, TEXT_HAO.to_vec()),
        (0, builtin_read_wire("/assets/a.png")),
        (0, exec_read),
        (30, TEXT_HAO.to_vec()), // 30s 后模型看完图接着说
        (
            0,
            vec![0x0a, 0x08, 0x72, 0x06, 0x08, 0x64, 0x10, 0x05, 0x18, 0x5a],
        ),
    ];
    let items: Vec<_> = run_timed_assets(
        seq,
        "conv-asset-read",
        Some(btx),
        false,
        BuiltinXlate::default(),
        assets,
    )
    .collect()
    .await;
    assert!(items.iter().all(|i| i.is_ok()));
    let texts = items
        .iter()
        .filter(|i| {
            matches!(i, Ok(StreamItem::Sse(ev))
                    if ev.event == "content_block_delta" && ev.data.to_string().contains("红"))
        })
        .count();
    assert_eq!(texts, 2, "看完图之后的话必须交付,不能在 20s 时被截断");
}

/// 2026-11-07 生产 5 次「内建工具截断」都在 IDE 形态:翻译不了的界面帧当场收口。
/// 现在 IDE 形态也先等 exec 帧 —— 来了就回拒绝,模型换办法继续,回答不截断。
#[tokio::test(start_paused = true)]
async fn ide形态翻译不了的内建工具等exec帧回拒绝不截断() {
    use futures::StreamExt;
    let (btx, mut brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    // exec 删除请求 {2:{1:3, 4:{1:"/a"}}}:删除永不代做,只能回拒绝。
    let exec_delete = vec![0x12, 0x08, 0x08, 0x03, 0x22, 0x04, 0x0A, 0x02, b'/', b'a'];
    let seq = vec![
        (0, TEXT_HAO.to_vec()),
        (0, builtin_write_wire("/tmp/x.md", "长文")), // 默认转换表 = 翻译不了
        (1, exec_delete),
        (1, TEXT_HAO.to_vec()),
        // IDE 形态的收尾是用量帧 1.14(不是 CLI 的 turn_commit)。
        (
            0,
            vec![0x0a, 0x08, 0x72, 0x06, 0x08, 0x64, 0x10, 0x05, 0x18, 0x5a],
        ),
    ];
    let items: Vec<_> = run_timed_with(seq, "conv-ide-wait", Some(btx), false)
        .collect()
        .await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "{:?}",
        items.iter().find_map(|i| i.as_ref().err())
    );
    let texts = items
        .iter()
        .filter(|i| {
            matches!(i, Ok(StreamItem::Sse(ev))
                    if ev.event == "content_block_delta" && ev.data.to_string().contains("红"))
        })
        .count();
    assert_eq!(
        texts,
        2,
        "拒绝之后模型继续说的话必须交付(不截断): {:?}",
        items
            .iter()
            .filter_map(|i| match i {
                Ok(StreamItem::Sse(ev)) => Some(format!("{} {}", ev.event, ev.data)),
                _ => None,
            })
            .collect::<Vec<_>>()
    );
    let mut sent = Vec::new();
    while let Ok(Ok(b)) = brx.try_recv() {
        sent.push(b);
    }
    let reason = crate::exec::BUILTIN_REJECT_REASON.as_bytes();
    assert!(
        sent.iter()
            .any(|b| b.windows(reason.len()).any(|w| w == reason)),
        "要在请求侧回带拒绝原因的帧"
    );
}

/// exec 帧迟迟不来:[`BUILTIN_EXEC_WAIT`] 到期退回原收口,不拖成 150s 停滞。
#[tokio::test(start_paused = true)]
async fn ide形态等不到exec帧20秒后退回收口() {
    use futures::StreamExt;
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    let seq = vec![
        (0, TEXT_HAO.to_vec()),
        (0, builtin_write_wire("/tmp/x.md", "长文")),
    ];
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_timed_with(seq, "conv-ide-timeout", Some(btx), false)
        .collect()
        .await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "出字后收口 = 截断收尾,不是错误"
    );
    assert_eq!(t0.elapsed().as_secs(), BUILTIN_EXEC_WAIT.as_secs());
}

/// 提前 turn_commit 的回归锁(2026-09-06 e2e 实证):commit 帧之后又来了
/// 工具调用帧时,排水段必须**退出**,把该帧交回正常工具调用处理 ——
/// 客户端必须收到 tool_use,而不是被排水段吞掉后 end_turn 截断。
#[tokio::test]
async fn 步骤提交后模型继续思考出字不被排水段吞掉() {
    // 2026-11-07 生产帧序(简化):出字 → 步骤提交(服务端自答工具定义)→ 模型继续思考
    // → 继续出字 → 真正的整轮提交 → 流结束。第二段正文必须交付。
    let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2]; // 「红」
    let thinking_payload = [0x0au8, 0x07, 0x22, 0x05, 0x0a, 0x03, 0xe6, 0x83, 0xb3]; // 思考「想」
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
        Ok(bytes::Bytes::from(wire::frame(&text_payload))),
        Ok(bytes::Bytes::from(wire::frame(&turn_commit_frame()))),
        Ok(bytes::Bytes::from(wire::frame(&thinking_payload))),
        Ok(bytes::Bytes::from(wire::frame(&text_payload))),
        Ok(bytes::Bytes::from(wire::frame(&turn_commit_frame()))),
    ];
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "claude-opus-5-5".into(),
        false,
        None,
        None,
        "conv-step-commit".into(),
        std::sync::Arc::new(crate::AssetStore::default()),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        BuiltinXlate::default(),
        ChatUsage {
            input_tokens: 10,
            ..Default::default()
        },
        0,
        true,
        None,
        None,
    );
    use futures::StreamExt;
    let items: Vec<_> = out.collect().await;
    assert!(items.iter().all(|i| i.is_ok()), "{items:?}");
    let reds = items
        .iter()
        .filter(|i| {
            matches!(i, Ok(StreamItem::Sse(ev))
                    if ev.event == "content_block_delta" && ev.data.to_string().contains("红"))
        })
        .count();
    assert_eq!(reds, 2, "步骤提交之后的正文必须交付");
}

/// 2026-11-09 生产帧序:提交(被认成本轮结束)→ 总结开始 → 39s 只有心跳 → 总结完成 → 出字。
/// 旧逻辑 20s 排水闸掐成空回复;现在总结开始即退出排水段,正文照常交付。
#[tokio::test(start_paused = true)]
async fn 提交后服务端总结压缩_总结完再出字照常交付() {
    use futures::StreamExt;
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    let seq = vec![
        (0, turn_commit_frame()),
        (0, SUMMARY_STARTED.to_vec()),
        (39, SUMMARY_COMPLETED.to_vec()),
        (5, TEXT_HAO.to_vec()),
        (0, turn_commit_frame()),
    ];
    let items: Vec<_> = run_timed_with(seq, "conv-summary", Some(btx), true)
        .collect()
        .await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "{:?}",
        items.iter().find_map(|i| i.as_ref().err())
    );
    assert_eq!(count_red(&items), 1, "总结之后的正文必须交付");
}

/// 总结比普通首字闸(90s)还长也要等:总结开始后 150s 才完成,照常交付。
#[tokio::test(start_paused = true)]
async fn 总结超过90秒也在上限内等完() {
    use futures::StreamExt;
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    let seq = vec![
        (0, SUMMARY_STARTED.to_vec()),
        (150, SUMMARY_COMPLETED.to_vec()),
        (2, TEXT_HAO.to_vec()),
        (0, turn_commit_frame()),
    ];
    let items: Vec<_> = run_timed_with(seq, "conv-summary-long", Some(btx), true)
        .collect()
        .await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "{:?}",
        items.iter().find_map(|i| i.as_ref().err())
    );
    assert_eq!(count_red(&items), 1);
}

/// 总结一直不完成:到 [`SUMMARY_MAX`] 按可重试的服务错误收口,文案点明是总结超时。
#[tokio::test(start_paused = true)]
async fn 总结永不完成按上限收口() {
    use futures::StreamExt;
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_timed_with(
        vec![(0, turn_commit_frame()), (0, SUMMARY_STARTED.to_vec())],
        "conv-summary-hang",
        Some(btx),
        true,
    )
    .collect()
    .await;
    let err = items.iter().find_map(|i| i.as_ref().err()).expect("应报错");
    assert_eq!(err.kind, UpstreamErrorKind::ServerError);
    assert!(err.message.contains("总结"), "{}", err.message);
    assert_eq!(t0.elapsed().as_secs(), SUMMARY_MAX.as_secs());
}

/// 没有总结帧的出字前提交:闸门不变,仍是排水段 20s 收口成空回复(不放宽、不占并发)。
#[tokio::test(start_paused = true)]
async fn 无总结帧的出字前提交仍按排水闸收口() {
    use futures::StreamExt;
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_timed_with(
        vec![(0, turn_commit_frame())],
        "conv-commit-only",
        Some(btx),
        true,
    )
    .collect()
    .await;
    let err = items.iter().find_map(|i| i.as_ref().err()).expect("应报错");
    assert_eq!(err.kind, UpstreamErrorKind::EmptyResponse);
    assert_eq!(t0.elapsed().as_secs(), DRAIN_STALL_TIMEOUT.as_secs());
}

/// 已经交付了正文、提交之后服务端再总结(为下一轮做准备):照常排水收尾,不等总结。
#[tokio::test(start_paused = true)]
async fn 出字后的总结不拖住本轮收尾() {
    use futures::StreamExt;
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = run_timed_with(
        vec![
            (0, TEXT_HAO.to_vec()),
            (0, turn_commit_frame()),
            (0, SUMMARY_STARTED.to_vec()),
        ],
        "conv-summary-after",
        Some(btx),
        true,
    )
    .collect()
    .await;
    assert!(items.iter().all(|i| i.is_ok()));
    assert_eq!(count_red(&items), 1);
    assert_eq!(
        t0.elapsed().as_secs(),
        DRAIN_STALL_TIMEOUT.as_secs(),
        "按排水闸收尾,不放宽"
    );
}

/// 出字前 commit 之后流自然结束、确实一个字都没有 → 照旧报空回复(不吞错、不干等)。
#[tokio::test(start_paused = true)]
async fn 出字前提交后流结束仍报空回复() {
    use futures::StreamExt;
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> =
        vec![Ok(bytes::Bytes::from(wire::frame(&turn_commit_frame())))];
    let t0 = tokio::time::Instant::now();
    let items: Vec<_> = stream_to_anthropic(
        futures::stream::iter(chunks),
        "claude-opus-5-5".into(),
        false,
        None,
        None,
        "conv-commit-eof".into(),
        std::sync::Arc::new(crate::AssetStore::default()),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        BuiltinXlate::default(),
        ChatUsage::default(),
        0,
        true,
        None,
        None,
    )
    .collect()
    .await;
    let err = items.iter().find_map(|i| i.as_ref().err()).expect("应报错");
    assert_eq!(err.kind, UpstreamErrorKind::EmptyResponse);
    assert!(t0.elapsed().as_secs() < 5, "流已结束就立刻收口,不等停滞闸");
}

/// Finding 1 红线锁①:turn_commit 后的 1.14 usage 帧**不得**进入计费 ——
/// 排水段是纯观测附加段,message_delta 与 StreamItem::Usage 必须与
/// 「turn_commit 即收尾」(无尾帧)逐字一致;影子照常捕到尾部 `.3`。
#[tokio::test]
async fn 影子_排水段usage不进计费_输出逐字不变() {
    let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2]; // 「好」
                                                                                 // 上游 `1.14` 用量帧(input=100 / output=5):排水段若放它进来就会覆写计费。
    let usage_114 = [0x0au8, 0x08, 0x72, 0x06, 0x08, 0x64, 0x10, 0x05, 0x18, 0x00];
    let desc = crate::run::descriptor_samples::turn2(); // 6 个 blob 引用
    let fallback = ChatUsage {
        input_tokens: 500,
        ..Default::default()
    };

    let run = |with_tail_usage: bool, shadow: std::sync::Arc<crate::DescriptorShadow>| {
        let fallback = fallback.clone();
        let desc = desc.clone();
        async move {
            let mut chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
                Ok(bytes::Bytes::from(wire::frame(&text_payload))),
                Ok(bytes::Bytes::from(wire::frame(&turn_commit_frame()))),
                Ok(bytes::Bytes::from(wire::frame(&desc))),
            ];
            if with_tail_usage {
                chunks.push(Ok(bytes::Bytes::from(wire::frame(&usage_114))));
            }
            let out = stream_to_anthropic(
                futures::stream::iter(chunks),
                "claude-fable-5".into(),
                false,
                None,
                None,
                "conv-shadow".into(),
                std::sync::Arc::new(crate::AssetStore::default()),
                std::sync::Arc::new(crate::TruncationNotices::default()),
                BuiltinXlate::default(),
                fallback,
                0,
                true, // cli_mode:排水段只在 CLI 形态走
                None, // upload
                Some(crate::ShadowFeed {
                    map: shadow,
                    account_id: "acc-1".into(),
                    fps: Vec::new(),
                }),
            );
            use futures::StreamExt;
            out.collect::<Vec<_>>().await
        }
    };

    let shadow = std::sync::Arc::new(crate::DescriptorShadow::default());
    let with_tail = run(true, shadow.clone()).await;
    let without_tail = run(
        false,
        std::sync::Arc::new(crate::DescriptorShadow::default()),
    )
    .await;
    // 计费可见面的逐字签名:message_delta 的 usage JSON + StreamItem::Usage。
    let signature = |items: &[Result<StreamItem, UpstreamError>]| -> Vec<String> {
        items
            .iter()
            .filter_map(|it| match it {
                Ok(StreamItem::Sse(e)) if e.event == "message_delta" => Some(e.data.to_string()),
                Ok(StreamItem::Usage(u)) => Some(format!(
                    "usage:{}:{}:{}",
                    u.input_tokens, u.output_tokens, u.cache_read_tokens
                )),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        signature(&with_tail),
        signature(&without_tail),
        "排水段的 1.14 不得改变任何计费输出(改动前 turn_commit 即收尾)"
    );
    assert!(
        with_tail.iter().all(|i| i.is_ok()),
        "轮次必须照常成功: {with_tail:?}"
    );
    // 影子:干净排完(流自然结束)= 提交尾部 `.3`。
    let (refs, _) = shadow
        .get_for_test("conv-shadow", "acc-1")
        .expect("干净排完必须提交影子");
    assert_eq!(refs, 6);
}

/// Finding 1 红线锁②:排水段中途出错(尾帧解压失败),轮次**照常成功**
/// (turn_commit 时结果已定),但影子按「非干净收尾」不提交。
#[tokio::test]
async fn 影子_排水段中断_轮次成功但影子不提交() {
    let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2]; // 「好」
    let desc = crate::run::descriptor_samples::turn1(); // 4 个 blob 引用
                                                        // 坏帧:gzip 标记(flag&1)但内容不是 gzip → 解压必失败。
    let mut bad_frame = vec![0x01u8];
    bad_frame.extend(5u32.to_be_bytes());
    bad_frame.extend(b"xxxxx");
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
        Ok(bytes::Bytes::from(wire::frame(&text_payload))),
        Ok(bytes::Bytes::from(wire::frame(&turn_commit_frame()))),
        Ok(bytes::Bytes::from(wire::frame(&desc))),
        Ok(bytes::Bytes::from(bad_frame)),
    ];
    let shadow = std::sync::Arc::new(crate::DescriptorShadow::default());
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "claude-fable-5".into(),
        false,
        None,
        None,
        "conv-shadow".into(),
        std::sync::Arc::new(crate::AssetStore::default()),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        BuiltinXlate::default(),
        ChatUsage {
            input_tokens: 500,
            ..Default::default()
        },
        0,
        true, // cli_mode
        None, // upload
        Some(crate::ShadowFeed {
            map: shadow.clone(),
            account_id: "acc-1".into(),
            fps: Vec::new(),
        }),
    );
    use futures::StreamExt;
    let items: Vec<_> = out.collect().await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "排水段出错绝不能让已成功的轮次变红: {items:?}"
    );
    assert!(
        items.iter().any(|i| matches!(
            i,
            Ok(StreamItem::Sse(e)) if e.event == "message_delta"
        )),
        "轮次必须照常按 end_turn 收尾"
    );
    assert!(
        shadow.get_for_test("conv-shadow", "acc-1").is_none(),
        "排水段中断 = 非干净收尾,影子绝不能提交"
    );
}
