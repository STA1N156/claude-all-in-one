//! chat 测试:内建工具翻译。

use super::*;

/// 四级分流各走各的路,且**位置语义保留**(未知注入原位转 user,不前后串位)。
#[test]
fn 分流器_四级分流() {
    let mut body = json!({"system": "原有 system", "messages": [
        {"role": "system", "content": "SessionStart hook additional context:\n项目约定 X"},
        {"role": "user", "content": "甲"},
        {"role": "system", "content": "The task tools haven't been used recently. 略"},
        {"role": "system", "content": "Available agent types for the Agent tool:\n- claude"},
        {"role": "system", "content": "   "},
        {"role": "system", "content": "The user sent a new message while you were working:\n先停一下\n\nIMPORTANT: 别提这条"},
        {"role": "assistant", "content": "乙"},
    ]});
    route_system_role_messages(&mut body);
    let msgs = body["messages"].as_array().unwrap();
    // 稳定前缀提升 → 不在 messages 里;动态噪声与空消息丢弃。
    assert_eq!(msgs.len(), 4, "{msgs:#?}");
    assert_eq!(msgs[0], json!({"role": "user", "content": "甲"}));
    assert_eq!(
        msgs[1],
        json!({"role": "user",
                   "content": "<system_context>\nAvailable agent types for the Agent tool:\n- claude\n</system_context>"}),
        "不认识的注入:裹起来转 user,原位保留"
    );
    // interrupted-user 取正文、截掉 IMPORTANT 尾巴。
    assert_eq!(msgs[2], json!({"role": "user", "content": "先停一下"}));
    assert_eq!(msgs[3]["role"], "assistant");
    // 提升的稳定前缀接在既有 system 之后。
    assert_eq!(
        body["system"],
        json!("原有 system\n\nSessionStart hook additional context:\n项目约定 X")
    );
}

/// 纯 tool_use 轮(正文为零)的产出必须计入工具参数,否则 output=0 漏账。
#[test]
fn 工具参数计入产出估算() {
    let tc = run::ToolCall {
        id: "call_1".into(),
        name: "Edit".into(),
        args: vec![
            ("file_path".into(), json!("/tmp/a.rs")),
            ("content".into(), json!("汉".repeat(150))),
        ],
    };
    let t = tool_call_tokens(&tc);
    // 150 汉字 ≈ 100 token,加 key/路径/工具名,必须远大于 0
    assert!(t >= 100, "tool tokens={t}");
}

/// 内建终端带工作目录:调用方工具有目录参数(opencode `workdir`)就传参数;
/// 没有(Claude Code `Bash`)就在命令前补 `cd '<目录>' &&`,目录里的单引号要转义。
#[test]
fn 内建终端的工作目录按调用方工具落地() {
    let term = |cwd: Option<&str>| run::BuiltinCall::Terminal {
        id: "t1".into(),
        command: "ls -la".into(),
        cwd: cwd.map(str::to_string),
    };
    let oc = vec![tool_with_schema(
        "bash",
        r#"{"properties":{"command":{},"workdir":{},"description":{}}}"#,
    )];
    let x = BuiltinXlate::from_tools(&oc);
    let tc = translate_builtin(term(Some("/srv/a")), &x).unwrap();
    assert_eq!(
        tc.args,
        vec![
            ("command".to_string(), json!("ls -la")),
            ("workdir".to_string(), json!("/srv/a")),
        ]
    );
    let tc = translate_builtin(term(None), &x).unwrap();
    assert_eq!(tc.args, vec![("command".to_string(), json!("ls -la"))]);

    let cc = vec![tool_with_schema("Bash", r#"{"properties":{"command":{}}}"#)];
    let x = BuiltinXlate::from_tools(&cc);
    let tc = translate_builtin(term(Some("/srv/it's")), &x).unwrap();
    assert_eq!(
        tc.args,
        vec![("command".to_string(), json!("cd '/srv/it'\\''s' && ls -la"))]
    );
}

/// 内建找文件(`.4` glob)→ 调用方自己的 glob 工具:模式与目录分别落到
/// schema 里认出的键;没有目录就不带;没有 glob 工具就不翻(绝不塞给读文件)。
#[test]
fn 内建找文件翻译成调用方的glob工具() {
    let glob = |id: &str, dir: Option<&str>, pattern: &str| run::BuiltinCall::Glob {
        id: id.into(),
        dir: dir.map(str::to_string),
        pattern: pattern.into(),
    };
    // opencode / Claude Code 形态都是 pattern + path。
    for name in ["glob", "Glob"] {
        let tools = vec![
            tool_with_schema("read", r#"{"properties":{"filePath":{}}}"#),
            tool_with_schema(name, r#"{"properties":{"pattern":{},"path":{}}}"#),
        ];
        let x = BuiltinXlate::from_tools(&tools);
        let tc = translate_builtin(glob("g1", Some("/srv"), "**/*.pdf"), &x).unwrap();
        assert_eq!(tc.name, name);
        assert_eq!(
            tc.args,
            vec![
                ("pattern".to_string(), json!("**/*.pdf")),
                ("path".to_string(), json!("/srv")),
            ]
        );
        let tc = translate_builtin(glob("g2", None, "*.py"), &x).unwrap();
        assert_eq!(tc.args, vec![("pattern".to_string(), json!("*.py"))]);
    }
    // 只有读文件工具、没有 glob 工具 → 不翻(旧 bug:模式被当路径塞给 read)。
    let tools = vec![tool_with_schema(
        "read",
        r#"{"properties":{"filePath":{}}}"#,
    )];
    let x = BuiltinXlate::from_tools(&tools);
    assert_eq!(translate_builtin(glob("g3", None, "**/x"), &x), None);
    // glob 工具的 schema 认不出模式键 → 不翻,不猜。
    let tools = vec![tool_with_schema("glob", r#"{"properties":{"q":{}}}"#)];
    let x = BuiltinXlate::from_tools(&tools);
    assert_eq!(translate_builtin(glob("g4", None, "**/x"), &x), None);
}

/// write 别名:只有编辑工具时补位、有写文件工具时不补、键名认不齐时不补;
/// 回调翻译成编辑工具调用(content→newString,oldString 置空)。
#[test]
fn write别名补位与翻译() {
    let edit_schema = r#"{"type":"object","properties":{
            "filePath":{"type":"string"},
            "oldString":{"type":"string"},
            "newString":{"type":"string"}}}"#;
    // opencode 形态:有 edit 无 write → 补位。
    let tools = vec![tool_with_schema("edit", edit_schema)];
    let x = BuiltinXlate::from_tools(&tools);
    let a = x.write_alias.as_ref().expect("有 edit 无 write 必须补位");
    assert_eq!(a.target, "edit");
    assert_eq!(a.path_key, "filePath");
    // 广告定义:裸名 write,schema 用目标工具的路径键。
    let ad = write_alias_tooldef(a);
    assert_eq!(ad.name, WRITE_ALIAS_NAME);
    let ad_schema: Value = serde_json::from_str(&ad.schema).unwrap();
    assert!(ad_schema["properties"].get("filePath").is_some());
    assert!(ad_schema["properties"].get("content").is_some());
    // 回调翻译:content→newString,oldString 置空,名字换成 edit。
    let tc = a.translate(run::ToolCall {
        id: "call-w".into(),
        name: WRITE_ALIAS_NAME.into(),
        args: vec![
            ("filePath".into(), json!("/tmp/a.html")),
            ("content".into(), json!("<html>…</html>")),
        ],
    });
    assert_eq!(tc.name, "edit");
    assert_eq!(tc.id, "call-w");
    let get = |k: &str| {
        tc.args
            .iter()
            .find(|(kk, _)| kk == k)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(get("filePath"), Some(json!("/tmp/a.html")));
    assert_eq!(get("oldString"), Some(json!("")));
    assert_eq!(get("newString"), Some(json!("<html>…</html>")));
    // 已有写文件工具 → 不补位。
    let tools2 = vec![
        tool_with_schema("edit", edit_schema),
        tool_with_schema(
            "write",
            r#"{"type":"object","properties":{"filePath":{"type":"string"}}}"#,
        ),
    ];
    assert!(BuiltinXlate::from_tools(&tools2).write_alias.is_none());
    // 编辑工具 schema 认不齐键 → 不补位(不猜)。
    let tools3 = vec![tool_with_schema(
        "edit",
        r#"{"type":"object","properties":{"mystery":{"type":"string"}}}"#,
    )];
    assert!(BuiltinXlate::from_tools(&tools3).write_alias.is_none());
}

#[test]
fn 翻译产物与失败回退() {
    let x = BuiltinXlate {
        terminal: Some(("Bash".into(), "command".into())),
        terminal_cwd_key: None,
        read_file: None,
        read_range_keys: (None, None),
        read_offset_zero_based: false,
        terminal_powershell: false,
        grep: None,
        write_alias: None,
        write_file: None,
        web_fetch: None,
        glob: None,
        task: None,
        edit_tool: None,
    };
    let tc = translate_builtin(
        run::BuiltinCall::Terminal {
            cwd: None,
            id: "call-1".into(),
            command: "ls".into(),
        },
        &x,
    )
    .expect("表里有对应工具就必须翻译");
    assert_eq!(tc.name, "Bash");
    assert_eq!(tc.args, vec![("command".to_string(), json!("ls"))]);
    assert_eq!(tc.id, "call-1");

    // 空 id 合成(与 parse_tool_call 的兜底同款,我方从不把 call id 发回上游)。
    let tc = translate_builtin(
        run::BuiltinCall::Terminal {
            cwd: None,
            id: "  ".into(),
            command: "ls".into(),
        },
        &x,
    )
    .unwrap();
    assert!(tc.id.starts_with("call_"), "空 id 必须合成: {}", tc.id);

    // 表里没有对应工具 → None(调用方落回收口 + 纠偏,失败单向)。
    assert_eq!(
        translate_builtin(
            run::BuiltinCall::ReadFile {
                id: "c".into(),
                path: "/a".into(),
                offset: None,
                limit: None,
            },
            &x
        ),
        None
    );
}

/// 内建写文件(`.12`)的翻译:调用方有写工具 → 直译(键名认自 schema);
/// 只有编辑工具 → 借 write 别名(旧串置空);schema 认不齐 → None(收口)。
#[test]
fn 内建写文件的翻译落点() {
    let bc = || run::BuiltinCall::WriteFile {
        id: "call-w-9".into(),
        path: "/tmp/news.html".into(),
        content: "<html>…</html>".into(),
    };
    let get = |tc: &run::ToolCall, k: &str| {
        tc.args
            .iter()
            .find(|(kk, _)| kk == k)
            .map(|(_, v)| v.clone())
    };

    // opencode 形态:有 write(filePath/content) → 直译。
    let tools = vec![tool_with_schema(
        "write",
        r#"{"type":"object","properties":{"filePath":{"type":"string"},"content":{"type":"string"}}}"#,
    )];
    let x = BuiltinXlate::from_tools(&tools);
    let tc = translate_builtin(bc(), &x).expect("有写工具必须直译");
    assert_eq!(tc.name, "write");
    assert_eq!(tc.id, "call-w-9");
    assert_eq!(get(&tc, "filePath"), Some(json!("/tmp/news.html")));
    assert_eq!(get(&tc, "content"), Some(json!("<html>…</html>")));

    // Claude Code 形态:Write(file_path/content) → 大小写不敏感命中 write 家族。
    let tools = vec![tool_with_schema(
        "Write",
        r#"{"type":"object","properties":{"file_path":{"type":"string"},"content":{"type":"string"}}}"#,
    )];
    let x = BuiltinXlate::from_tools(&tools);
    let tc = translate_builtin(bc(), &x).expect("Write 大写也要命中 write 家族");
    assert_eq!(tc.name, "Write");
    assert_eq!(get(&tc, "file_path"), Some(json!("/tmp/news.html")));

    // 只有编辑工具 → 借 write 别名(旧串置空 = 整文件覆写)。
    let edit_schema = r#"{"type":"object","properties":{
            "filePath":{"type":"string"},
            "oldString":{"type":"string"},
            "newString":{"type":"string"}}}"#;
    let tools = vec![tool_with_schema("edit", edit_schema)];
    let x = BuiltinXlate::from_tools(&tools);
    let tc = translate_builtin(bc(), &x).expect("有编辑工具必须借别名翻译");
    assert_eq!(tc.name, "edit");
    assert_eq!(get(&tc, "filePath"), Some(json!("/tmp/news.html")));
    assert_eq!(get(&tc, "oldString"), Some(json!("")));
    assert_eq!(get(&tc, "newString"), Some(json!("<html>…</html>")));

    // 写工具 schema 认不齐键名 → None(不猜,落回收口)。
    let tools = vec![tool_with_schema(
        "write",
        r#"{"type":"object","properties":{"mystery":{"type":"string"}}}"#,
    )];
    let x = BuiltinXlate::from_tools(&tools);
    assert_eq!(translate_builtin(bc(), &x), None);

    // 一个工具都没声明 → None。
    assert_eq!(translate_builtin(bc(), &BuiltinXlate::default()), None);
}

/// 内建网页抓取(`.37`)的翻译:认出「查网页」能力工具 + URL 键名认自 schema。
#[test]
fn 内建网页抓取的翻译落点() {
    let bc = || run::BuiltinCall::WebFetch {
        id: "call-f-3".into(),
        url: "https://example.com".into(),
    };
    let get = |tc: &run::ToolCall, k: &str| {
        tc.args
            .iter()
            .find(|(kk, _)| kk == k)
            .map(|(_, v)| v.clone())
    };

    // opencode 形态:webfetch(url)。
    let tools = vec![tool_with_schema(
        "webfetch",
        r#"{"type":"object","properties":{"url":{"type":"string"},"format":{"type":"string"}}}"#,
    )];
    let x = BuiltinXlate::from_tools(&tools);
    let tc = translate_builtin(bc(), &x).expect("有网页工具必须翻译");
    assert_eq!(tc.name, "webfetch");
    assert_eq!(tc.id, "call-f-3");
    assert_eq!(get(&tc, "url"), Some(json!("https://example.com")));

    // Claude Code 形态:WebFetch(url+prompt),大小写不敏感。
    let tools = vec![tool_with_schema(
        "WebFetch",
        r#"{"type":"object","properties":{"url":{"type":"string"},"prompt":{"type":"string"}}}"#,
    )];
    let x = BuiltinXlate::from_tools(&tools);
    let tc = translate_builtin(bc(), &x).expect("WebFetch 大写也要命中");
    assert_eq!(tc.name, "WebFetch");
    assert_eq!(get(&tc, "url"), Some(json!("https://example.com")));

    // 网页工具 schema 没有 URL 键 → None(不猜)。
    let tools = vec![tool_with_schema(
        "webfetch",
        r#"{"type":"object","properties":{"query":{"type":"string"}}}"#,
    )];
    let x = BuiltinXlate::from_tools(&tools);
    assert_eq!(translate_builtin(bc(), &x), None);

    // 没声明网页工具 → None(落回收口 + 纠偏)。
    assert_eq!(translate_builtin(bc(), &BuiltinXlate::default()), None);
}

/// 「空转中断」的直接回归锁:模型抓无前缀内建终端时,带转换表的流必须下发
/// 标准 `tool_use`(客户端照常执行);不带表(调用方没声明同义工具)则维持
/// 原收口行为(出字前 = EmptyResponse 错误)。
#[tokio::test]
async fn 内建终端调用被翻译成_tool_use_下发() {
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![Ok(bytes::Bytes::from(
        wire::frame(&builtin_terminal_wire("ls -la")),
    ))];
    let xlate = BuiltinXlate {
        terminal: Some(("Bash".into(), "command".into())),
        terminal_cwd_key: None,
        read_file: None,
        read_range_keys: (None, None),
        read_offset_zero_based: false,
        terminal_powershell: false,
        grep: None,
        write_alias: None,
        write_file: None,
        web_fetch: None,
        glob: None,
        task: None,
        edit_tool: None,
    };
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "claude-fable-5".into(),
        false,
        None,
        None,
        "conv-xlate".into(),
        std::sync::Arc::new(crate::AssetStore::default()),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        xlate,
        ChatUsage {
            input_tokens: 10,
            ..Default::default()
        },
        0,
        false, // cli_mode:测试默认 IDE 形态
        None,  // upload:本用例不走 wire v2 上传腿
        None,  // 本用例不关心影子捕获
    );
    use futures::StreamExt;
    let items: Vec<_> = out.collect().await;
    assert!(
        !items.iter().any(|it| it.is_err()),
        "翻译成功的内建调用不许再报错收口"
    );
    let start = items
        .iter()
        .find_map(|it| match it {
            Ok(StreamItem::Sse(e))
                if e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "tool_use" =>
            {
                Some(e)
            }
            _ => None,
        })
        .expect("必须下发 tool_use 块");
    assert_eq!(start.data["content_block"]["name"], "Bash");
    assert_eq!(start.data["content_block"]["id"], "call-b-0");
    let delta = items
        .iter()
        .find_map(|it| match it {
            Ok(StreamItem::Sse(e))
                if e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "input_json_delta" =>
            {
                Some(e)
            }
            _ => None,
        })
        .expect("参数必须经 input_json_delta 下发");
    let input: Value =
        serde_json::from_str(delta.data["delta"]["partial_json"].as_str().unwrap()).unwrap();
    assert_eq!(input, json!({"command": "ls -la"}));

    // 反例:空转换表 → 维持收口(出字前收到内建调用 = EmptyResponse)。
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![Ok(bytes::Bytes::from(
        wire::frame(&builtin_terminal_wire("ls -la")),
    ))];
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "claude-fable-5".into(),
        false,
        None,
        None,
        "conv-xlate-2".into(),
        std::sync::Arc::new(crate::AssetStore::default()),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        BuiltinXlate::default(),
        ChatUsage::default(),
        0,
        false, // cli_mode:测试默认 IDE 形态
        None,  // upload:本用例不走 wire v2 上传腿
        None,  // 本用例不关心影子捕获
    );
    let items: Vec<_> = out.collect().await;
    assert!(
        items.iter().any(|it| it.is_err()),
        "没有转换表时必须维持原收口行为(纠偏机制兜底)"
    );
}

/// 「说完正在写入就 end_turn」生产病根的回归锁:内建写文件(exec `write_args`)必须
/// 翻译成调用方写工具的 tool_use 下发(参数 JSON 含路径与完整内容),
/// 而不是收口吞掉。
#[tokio::test]
async fn 内建写文件调用被翻译成_tool_use_下发() {
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![Ok(bytes::Bytes::from(
        wire::frame(&exec_write_wire("/tmp/news.html", "<html>报纸</html>")),
    ))];
    let xlate = BuiltinXlate {
        write_file: Some(WriteFileTarget::Direct {
            name: "write".into(),
            path_key: "filePath".into(),
            content_key: "content".into(),
        }),
        ..Default::default()
    };
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "grok-4.6".into(),
        false,
        None,
        None,
        "conv-xlate-write".into(),
        std::sync::Arc::new(crate::AssetStore::default()),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        xlate,
        ChatUsage {
            input_tokens: 10,
            ..Default::default()
        },
        0,
        false, // cli_mode:测试默认 IDE 形态
        None,  // upload:本用例不走 wire v2 上传腿
        None,  // 本用例不关心影子捕获
    );
    use futures::StreamExt;
    let items: Vec<_> = out.collect().await;
    assert!(
        !items.iter().any(|it| it.is_err()),
        "翻译成功的内建写文件不许再报错收口"
    );
    let start = items
        .iter()
        .find_map(|it| match it {
            Ok(StreamItem::Sse(e))
                if e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "tool_use" =>
            {
                Some(e)
            }
            _ => None,
        })
        .expect("必须下发 tool_use 块");
    assert_eq!(start.data["content_block"]["name"], "write");
    // exec 帧不带模型侧 call id,沿用 exec 臂的合成 id(工具结果下一轮折进历史,不需对号)。
    assert!(
        start.data["content_block"]["id"]
            .as_str()
            .is_some_and(|i| i.starts_with("toolu_")),
        "{start:?}"
    );
    let delta = items
        .iter()
        .find_map(|it| match it {
            Ok(StreamItem::Sse(e))
                if e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "input_json_delta" =>
            {
                Some(e)
            }
            _ => None,
        })
        .expect("参数必须经 input_json_delta 下发");
    let input: Value =
        serde_json::from_str(delta.data["delta"]["partial_json"].as_str().unwrap()).unwrap();
    assert_eq!(
        input,
        json!({"filePath": "/tmp/news.html", "content": "<html>报纸</html>"})
    );
}

/// 2026-11-09 生产:出字后模型用 Cursor 自带写文件拼整页 HTML,上游只发 `1.8` token
/// 计数(无参数文本),150s 被当成「只有心跳」误杀。token 数 > 0 的帧算干活。
#[tokio::test(start_paused = true)]
async fn 只有token计数帧时不按150秒误杀() {
    use futures::StreamExt;
    let td = vec![0x0au8, 0x04, 0x42, 0x02, 0x08, 0x05]; // 1:{8:{1:5}}
    let mut seq = vec![(0, TEXT_HAO.to_vec())];
    for _ in 0..60 {
        seq.push((5, td.clone())); // 300s 只有 token 计数
    }
    seq.push((1, exec_write_wire("/w/news.html", "<html>…</html>")));
    let xlate = BuiltinXlate {
        write_file: Some(WriteFileTarget::Direct {
            name: "write".into(),
            path_key: "filePath".into(),
            content_key: "content".into(),
        }),
        ..Default::default()
    };
    let items: Vec<_> = run_timed_full(seq, "conv-token-delta", None, true, xlate)
        .collect()
        .await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "持续生成 token 时不该判停滞: {:?}",
        items.iter().find_map(|i| i.as_ref().err())
    );
    let has_tool = items.iter().any(|i| {
        matches!(i, Ok(StreamItem::Sse(ev))
                if ev.event == "content_block_start" && ev.data.to_string().contains("tool_use"))
    });
    assert!(has_tool, "拼完的写文件调用必须交付成 tool_use");
}

/// 2026-11-07 生产事故的回归锁:模型调 Cursor 内建 StrReplace,界面帧 `.12` 只带替换片段,
/// 旧代码把它翻成 OpenCode `write` —— 客户 144 行的脚本被盖成 5 行(同一会话连毁三次)。
/// 现在:`.12` 绝不翻成写调用;流不保持打开时按原收口走(单向失败)。
#[tokio::test]
async fn 编辑帧片段不许翻成整文件写() {
    use futures::StreamExt;
    let frag = "p.add_argument('--putdown',action='store_true')\na=p.parse_args()";
    let seq = vec![
        (0, TEXT_HAO.to_vec()),
        (0, builtin_write_wire("/data/make_ref.py", frag)),
    ];
    let xlate = BuiltinXlate {
        write_file: Some(WriteFileTarget::Direct {
            name: "write".into(),
            path_key: "filePath".into(),
            content_key: "content".into(),
        }),
        edit_tool: Some("edit".into()),
        ..Default::default()
    };
    let items: Vec<_> = run_timed_full(seq, "conv-edit-frag", None, false, xlate)
        .collect()
        .await;
    let wrote = items.iter().any(|i| {
            matches!(i, Ok(StreamItem::Sse(ev))
                if ev.event == "content_block_start" && ev.data["content_block"]["type"] == "tool_use")
        });
    assert!(!wrote, "编辑片段绝不能变成写调用: {items:?}");
}

/// 内建 StrReplace 的后续:服务端对同一路径发 exec 读(要原文件)。我们拿不到客户磁盘,
/// 回读拒绝并点名调用方的编辑工具,**继续读**;模型改口后正常收尾。读不能翻成客户端 read。
#[tokio::test(start_paused = true)]
async fn 内建局部替换的读文件回拒绝并指向编辑工具() {
    use futures::StreamExt;
    let (btx, mut brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    let seq = vec![
        (0, TEXT_HAO.to_vec()),
        (
            0,
            builtin_write_wire("/data/make_ref.py", "a=p.parse_args()"),
        ),
        (1, exec_read_wire("/data/make_ref.py")),
        (1, TEXT_HAO.to_vec()),
        (
            0,
            vec![0x0a, 0x08, 0x72, 0x06, 0x08, 0x64, 0x10, 0x05, 0x18, 0x5a],
        ),
    ];
    let xlate = BuiltinXlate {
        read_file: Some(("read".into(), "filePath".into())),
        write_file: Some(WriteFileTarget::Direct {
            name: "write".into(),
            path_key: "filePath".into(),
            content_key: "content".into(),
        }),
        edit_tool: Some("edit".into()),
        ..Default::default()
    };
    let items: Vec<_> = run_timed_full(seq, "conv-edit-read", Some(btx), false, xlate)
        .collect()
        .await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "{:?}",
        items.iter().find_map(|i| i.as_ref().err())
    );
    let tool = items.iter().any(|i| {
            matches!(i, Ok(StreamItem::Sse(ev))
                if ev.event == "content_block_start" && ev.data["content_block"]["type"] == "tool_use")
        });
    assert!(
        !tool,
        "StrReplace 的读不能翻成客户端 read,也不能变成写: {items:?}"
    );
    let mut sent = Vec::new();
    while let Ok(Ok(b)) = brx.try_recv() {
        sent.push(b);
    }
    let hint = b"call `gwtools-edit`";
    assert!(
        sent.iter()
            .any(|b| b.windows(hint.len()).any(|w| w == hint)),
        "要回读拒绝并点名编辑工具: {sent:?}"
    );
}

/// 内建 Write 的后续是 exec 写帧(服务端给完整内容):照常翻成调用方写工具。
#[tokio::test(start_paused = true)]
async fn 编辑帧之后的_exec_整写照常交付() {
    use futures::StreamExt;
    let (btx, _brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    let seq = vec![
        (0, TEXT_HAO.to_vec()),
        (0, builtin_write_wire("/tmp/new.md", "全文")),
        (1, exec_write_wire("/tmp/new.md", "全文")),
    ];
    let xlate = BuiltinXlate {
        write_file: Some(WriteFileTarget::Direct {
            name: "write".into(),
            path_key: "filePath".into(),
            content_key: "content".into(),
        }),
        edit_tool: Some("edit".into()),
        ..Default::default()
    };
    let items: Vec<_> = run_timed_full(seq, "conv-edit-write", Some(btx), false, xlate)
        .collect()
        .await;
    let start = items
        .iter()
        .find_map(|i| match i {
            Ok(StreamItem::Sse(ev))
                if ev.event == "content_block_start"
                    && ev.data["content_block"]["type"] == "tool_use" =>
            {
                Some(ev)
            }
            _ => None,
        })
        .expect("整写要交付成 tool_use");
    assert_eq!(start.data["content_block"]["name"], "write");
    let args: String = items
        .iter()
        .filter_map(|i| match i {
            Ok(StreamItem::Sse(ev)) if ev.event == "content_block_delta" => {
                ev.data["delta"]["partial_json"].as_str().map(str::to_owned)
            }
            _ => None,
        })
        .collect();
    assert!(
        args.contains("全文") && args.contains("/tmp/new.md"),
        "{args}"
    );
}

/// 编辑工具按名字全等认(大小写不敏感),认不出时拒绝理由退回通用说法。
#[test]
fn 编辑工具名认取与拒绝理由() {
    let oc = tdefs(&["bash", "edit", "write", "read"]);
    assert_eq!(edit_tool_name(&oc).as_deref(), Some("edit"));
    let cc = tdefs(&["Bash", "Edit", "Write", "NotebookEdit"]);
    assert_eq!(edit_tool_name(&cc).as_deref(), Some("Edit"));
    assert_eq!(edit_tool_name(&tdefs(&["NotebookEdit"])), None);
    let x = BuiltinXlate {
        edit_tool: Some("Edit".into()),
        ..Default::default()
    };
    assert!(edit_reject_reason(&x, true).contains("call `Edit`"));
    assert!(edit_reject_reason(&x, false).contains("call `gwtools-Edit`"));
    assert!(!edit_reject_reason(&BuiltinXlate::default(), false).contains("call `"));
}

/// 2026-11-07 生产:fable 派 Cursor 子代理(exec 28)被截断。翻成调用方的子代理工具:
/// OpenCode `task`(类型在描述里写 general)、Claude Code `Task`(general-purpose)。
#[test]
fn 内建子代理翻译成调用方的task工具() {
    let oc = run::ToolDef {
        name: "task".into(),
        description: "Launch a new agent. Available agent types: general, explore".into(),
        schema: json!({"type":"object","properties":{
                "description":{"type":"string"},"prompt":{"type":"string"},
                "subagent_type":{"type":"string"}}})
        .to_string(),
    };
    let x = BuiltinXlate::from_tools(&[oc]);
    let tc = translate_builtin(
        run::BuiltinCall::Task {
            id: "toolu_1".into(),
            prompt: "你是一名只读审查者。\n核对事实".into(),
            subagent_type: "generalPurpose".into(),
        },
        &x,
    )
    .expect("有 task 工具就要翻译");
    assert_eq!(tc.name, "task");
    assert_eq!(tc.id, "toolu_1");
    let args: std::collections::HashMap<_, _> = tc.args.into_iter().collect();
    assert_eq!(args["description"], "你是一名只读审查者。");
    assert_eq!(args["prompt"], "你是一名只读审查者。\n核对事实");
    assert_eq!(args["subagent_type"], "general");

    let cc = run::ToolDef {
        name: "Task".into(),
        description: "Launch a new agent".into(),
        schema: json!({"type":"object","properties":{
                "description":{"type":"string"},"prompt":{"type":"string"},
                "subagent_type":{"type":"string","enum":["Explore","general-purpose"]}}})
        .to_string(),
    };
    let x = BuiltinXlate::from_tools(&[cc]);
    let tc = translate_builtin(
        run::BuiltinCall::Task {
            id: "t".into(),
            prompt: "p".into(),
            subagent_type: String::new(),
        },
        &x,
    )
    .unwrap();
    let args: std::collections::HashMap<_, _> = tc.args.into_iter().collect();
    assert_eq!(args["subagent_type"], "general-purpose");

    // 没有子代理工具 → 不翻译(交给 exec 拒绝,模型换办法)。
    assert!(translate_builtin(
        run::BuiltinCall::Task {
            id: "t".into(),
            prompt: "p".into(),
            subagent_type: String::new()
        },
        &BuiltinXlate::default(),
    )
    .is_none());
}

/// 2026-11-09 生产:新版 OpenCode(Windows PowerShell)290 次「grep 不是 cmdlet」——
/// Cursor 自带 grep 被拼成 POSIX 命令发给 shell。调用方有自己的 grep 就翻到它上面,
/// 参数键从 schema 认(OpenCode `include`、Claude Code `glob`)。
#[test]
fn 内建grep优先翻成调用方自己的搜索工具() {
    let oc = BuiltinXlate::from_tools(&[
        tool(
            "shell",
            "Execute a shell command. Commands run on Windows using powershell.",
            json!({"command":{"type":"string"}}),
        ),
        tool(
            "grep",
            "Search file contents",
            json!({"pattern":{"type":"string"},"path":{"type":"string"},"include":{"type":"string"},"literal":{"type":"boolean"}}),
        ),
    ]);
    let tc = translate_builtin(grep_call(Some("D:\\docs"), Some("*.md")), &oc).unwrap();
    assert_eq!(tc.name, "grep");
    let a = arg_map(tc);
    assert_eq!(a["pattern"], json!("it's"));
    assert_eq!(a["path"], json!("D:\\docs"));
    assert_eq!(a["include"], json!("*.md"));
    let cc = BuiltinXlate::from_tools(&[
        tool(
            "Bash",
            "Executes a bash command",
            json!({"command":{"type":"string"}}),
        ),
        tool(
            "Grep",
            "A powerful search tool built on ripgrep",
            json!({"pattern":{"type":"string"},"path":{"type":"string"},"glob":{"type":"string"},"output_mode":{"type":"string"}}),
        ),
    ]);
    let tc = translate_builtin(grep_call(None, Some("*.py")), &cc).unwrap();
    assert_eq!(tc.name, "Grep");
    let a = arg_map(tc);
    assert_eq!(a["glob"], json!("*.py"));
    assert!(
        !a.contains_key("path"),
        "没给路径就不传(客户端默认工作目录)"
    );
}

/// 没有搜索工具时退回终端命令,按终端类型拼:PowerShell 用 Select-String,其余 POSIX grep。
#[test]
fn 内建grep无搜索工具时按终端类型拼命令() {
    let ps = BuiltinXlate::from_tools(&[tool(
        "shell",
        "Execute a shell command. Commands run on Windows using powershell.",
        json!({"command":{"type":"string"}}),
    )]);
    assert!(ps.terminal_powershell);
    let a = arg_map(translate_builtin(grep_call(Some("D:\\a b"), Some("*.md")), &ps).unwrap());
    assert_eq!(
            a["command"],
            json!("Get-ChildItem -LiteralPath 'D:\\a b' -Recurse -File -Include '*.md' -ErrorAction SilentlyContinue | Select-String -Pattern 'it''s'")
        );
    let sh = BuiltinXlate::from_tools(&[tool(
        "bash",
        "Commands run on Linux using bash.",
        json!({"command":{"type":"string"}}),
    )]);
    assert!(!sh.terminal_powershell);
    let a = arg_map(translate_builtin(grep_call(None, Some("*.py")), &sh).unwrap());
    assert_eq!(
        a["command"],
        json!(r"grep -rn --include='*.py' -- 'it'\''s' '.'")
    );
    // Windows 上跑 bash 的客户端(说明写 Windows using bash)→ 仍按 bash
    let wb = BuiltinXlate::from_tools(&[tool(
        "shell",
        "Commands run on Windows using bash.",
        json!({"command":{"type":"string"}}),
    )]);
    assert!(!wb.terminal_powershell);
    // 说明里只是提到 PowerShell(否定语境)不算
    let gb = BuiltinXlate::from_tools(&[tool(
        "shell",
        "Runs in Git Bash, not PowerShell. Don't use PowerShell syntax.",
        json!({"command":{"type":"string"}}),
    )]);
    assert!(!gb.terminal_powershell);
    // 弯引号同样转义,不能跳出字符串
    assert_eq!(ps_quote("a\u{2019}; rm"), "'a''; rm'");
    assert_eq!(ps_quote("x\u{2018}y\u{201A}z\u{201B}"), "'x''y''z'''");
}

/// 列目录同理;终端带工作目录而工具没有目录参数时,PowerShell 不用 `&&`(5.x 不支持)。
#[test]
fn 列目录与切目录按终端类型拼() {
    let ps = BuiltinXlate::from_tools(&[tool(
        "shell",
        "Commands run on Windows using powershell.",
        json!({"command":{"type":"string"}}),
    )]);
    let ls = run::BuiltinCall::ListDir {
        id: "l".into(),
        path: "C:\\it's".into(),
    };
    let a = arg_map(translate_builtin(ls.clone(), &ps).unwrap());
    assert_eq!(
        a["command"],
        json!("Get-ChildItem -Force -LiteralPath 'C:\\it''s'")
    );
    let sh = BuiltinXlate::from_tools(&[tool("bash", "", json!({"command":{"type":"string"}}))]);
    let a = arg_map(translate_builtin(ls, &sh).unwrap());
    assert_eq!(a["command"], json!(r"ls -la 'C:\it'\''s'"));
    // 空路径 → 当前目录
    let a = arg_map(
        translate_builtin(
            run::BuiltinCall::ListDir {
                id: "e".into(),
                path: " ".into(),
            },
            &sh,
        )
        .unwrap(),
    );
    assert_eq!(a["command"], json!("ls -la '.'"));
    let term = run::BuiltinCall::Terminal {
        id: "t".into(),
        command: "dir".into(),
        cwd: Some("D:\\w".into()),
    };
    let a = arg_map(translate_builtin(term, &ps).unwrap());
    assert_eq!(
        a["command"],
        json!("Set-Location -LiteralPath 'D:\\w'; dir")
    );
}

/// 2026-11-09 生产(论文会话):模型调内建读文件「从第 440 行往后读」,旧翻译只传路径,
/// 客户端从头读、截断后只回前 439 行,模型连读 3 次。行范围要照传(键名认自 schema)。
#[test]
fn 内建读文件的行范围照传给客户端() {
    let tools = vec![run::ToolDef {
            name: "Read".into(),
            description: "Reads a file".into(),
            schema: json!({"type":"object","properties":{
                "file_path":{"type":"string"},"offset":{"type":"number"},"limit":{"type":"number"}}})
            .to_string(),
        }];
    let x = BuiltinXlate::from_tools(&tools);
    assert_eq!(
        x.read_range_keys,
        (Some("offset".into()), Some("limit".into()))
    );
    let call = |offset, limit| run::BuiltinCall::ReadFile {
        id: "r".into(),
        path: "/w/a.tex".into(),
        offset,
        limit,
    };
    let args = |tc: run::ToolCall| {
        tc.args
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>()
    };
    let a = args(translate_builtin(call(Some(440), Some(200)), &x).unwrap());
    assert_eq!(a["file_path"], json!("/w/a.tex"));
    assert_eq!(a["offset"], json!(440));
    assert_eq!(a["limit"], json!(200));
    // 没给范围 → 只传路径(旧行为)
    let a = args(translate_builtin(call(None, None), &x).unwrap());
    assert_eq!(a.len(), 1);
    // 负 offset(从末尾倒数)客户端没有这种语义 → 范围整段不传
    let a = args(translate_builtin(call(Some(-50), Some(10)), &x).unwrap());
    assert_eq!(a.len(), 1, "{a:?}");
    // 客户端读工具没有 offset/limit 参数 → 不传(不猜键名)
    let bare = vec![run::ToolDef {
        name: "Read".into(),
        description: "Reads a file".into(),
        schema: json!({"type":"object","properties":{"file_path":{"type":"string"}}}).to_string(),
    }];
    let xb = BuiltinXlate::from_tools(&bare);
    let a = args(translate_builtin(call(Some(440), Some(200)), &xb).unwrap());
    assert_eq!(a.len(), 1);
    // 说明写 0-based → 减 1;写按字节 → offset 不认(limit 照传)
    let mk = |desc: &str| {
        BuiltinXlate::from_tools(&[run::ToolDef {
            name: "read".into(),
            description: "Reads a file".into(),
            schema: json!({"type":"object","properties":{
                    "filePath":{"type":"string"},
                    "offset":{"type":"number","description":desc},
                    "limit":{"type":"number"}}})
            .to_string(),
        }])
    };
    let a = args(
        translate_builtin(
            call(Some(440), None),
            &mk("The line number to start reading from (0-based)"),
        )
        .unwrap(),
    );
    assert_eq!(a["offset"], json!(439));
    let a =
        args(translate_builtin(call(Some(440), Some(5)), &mk("Byte offset to start at")).unwrap());
    assert!(!a.contains_key("offset"), "{a:?}");
    assert_eq!(a["limit"], json!(5));
    // 线上 OpenCode 实物说明(1-indexed)→ 原样
    let a = args(
        translate_builtin(
            call(Some(440), None),
            &mk("The line number to start reading from (1-indexed)"),
        )
        .unwrap(),
    );
    assert_eq!(a["offset"], json!(440));
}

/// 审查发现(2026-11-07):读 `/assets/` 附件不能翻成客户端 Read —— 客户端磁盘上
/// 没有这个文件,模型就看不到图。必须留给 exec 读帧从资产库回图。
#[test]
fn 读资产附件不翻译成客户端read() {
    let x = BuiltinXlate {
        read_file: Some(("Read".into(), "file_path".into())),
        ..Default::default()
    };
    let asset = run::BuiltinCall::ReadFile {
        id: "r".into(),
        path: "/assets/attach-1-u.png".into(),
        offset: None,
        limit: None,
    };
    assert!(translate_builtin(asset, &x).is_none());
    let normal = run::BuiltinCall::ReadFile {
        id: "r".into(),
        path: "/w/a.py".into(),
        offset: None,
        limit: None,
    };
    assert!(translate_builtin(normal, &x).is_some());
}

#[tokio::test]
async fn 排水段遇到工具调用帧退出排水正常下发() {
    let text_payload = [0x0au8, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2]; // 「好」
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![
        Ok(bytes::Bytes::from(wire::frame(&text_payload))),
        // 提前的 commit(误匹配/乱序):先进入排水段。
        Ok(bytes::Bytes::from(wire::frame(&turn_commit_frame()))),
        // 真正的工具调用帧:必须在排水段里被认出并恢复正常处理。
        Ok(bytes::Bytes::from(wire::frame(&exec_write_wire(
            "/tmp/news.html",
            "<html>报纸</html>",
        )))),
    ];
    let xlate = BuiltinXlate {
        write_file: Some(WriteFileTarget::Direct {
            name: "write".into(),
            path_key: "filePath".into(),
            content_key: "content".into(),
        }),
        ..Default::default()
    };
    let out = stream_to_anthropic(
        futures::stream::iter(chunks),
        "grok-4.6".into(),
        false,
        None,
        None,
        "conv-premature-commit".into(),
        std::sync::Arc::new(crate::AssetStore::default()),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        xlate,
        ChatUsage {
            input_tokens: 10,
            ..Default::default()
        },
        0,
        true, // cli_mode:turn_commit 分支只在 CLI 形态走
        None,
        None,
    );
    use futures::StreamExt;
    let items: Vec<_> = out.collect().await;
    assert!(
        items.iter().all(|i| i.is_ok()),
        "提前 commit 后正常收到工具调用,不许报错: {items:?}"
    );
    let start = items
        .iter()
        .find_map(|it| match it {
            Ok(StreamItem::Sse(e))
                if e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "tool_use" =>
            {
                Some(e)
            }
            _ => None,
        })
        .expect("工具调用必须穿透提前的 commit 下发为 tool_use");
    assert_eq!(start.data["content_block"]["name"], "write");
}
