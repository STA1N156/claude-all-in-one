//! 工具测试。

use super::*;

#[test]
fn 解出外部工具调用的名字与参数() {
    let f = tool_call_frame(
        "get_weather",
        "call-abc-0\nfc_def_0",
        &[("city", json!("北京"))],
    );
    assert!(is_tool_call(&f));
    let tc = parse_tool_call(&f).expect("应解出工具调用");
    // 名字用**裸名**,与调用方 Anthropic 请求里的 name 对得上(不带命名空间前缀)。
    assert_eq!(tc.name, "get_weather");
    // call id 必须原样带回,重新生成就对不上了。
    assert_eq!(tc.id, "call-abc-0\nfc_def_0");
    assert_eq!(tc.args, vec![("city".to_string(), json!("北京"))]);
}

/// ⭐ **锚在抓包实物上**:直接解 2026-08-07 落盘的那一帧。
///
/// 这条与 `解出外部工具调用的名字与参数` 的区别是它**不经过我方 Writer** ——
/// 字段号(`1.2.2.15.1` 那一串)若记错,只有这条会红。
#[test]
fn 解析抓包实物里的外部工具调用帧() {
    let raw = include_bytes!("../../../tests/fixtures/tool_call_external_real.bin");
    assert!(is_tool_call(raw));
    let tc = parse_tool_call(raw).expect("实物帧必须解得出来");
    assert_eq!(tc.name, "get_weather");
    assert!(tc.id.starts_with("call-d167f527-"), "call id: {}", tc.id);
    assert_eq!(tc.args, vec![("city".to_string(), json!("北京"))]);
}

/// ⭐ 同上,内建工具的实物帧(`1.2.2.4` = glob)**必须认不出来**。
#[test]
fn 抓包实物里的内建工具帧不被当成外部工具() {
    let raw = include_bytes!("../../../tests/fixtures/tool_call_builtin_real.bin");
    assert!(is_tool_call(raw), "是工具帧");
    assert!(
        parse_tool_call(raw).is_none(),
        "内建工具没有名字,认成外部工具会给调用方一个它没有的工具"
    );
}

/// ⭐ **锚在 2026-08-10 的抓包实物上**:带图请求的资产落盘调用
/// (`exec_server_message.3` = write_args,字段号经 opencodex 的
/// `agent.v1` schema 交叉核对)。
///
/// 这是「带图请求全模型 502」的直接病因:这一帧走 exec 通道,被误判成
/// 内建工具调用在出字前收口;而只跳过不回执,服务端又 90s 心跳死等。
/// 它必须被解成**写盘调用**(存内存 + 回执),既不是外部工具也不是跳过对象。
#[test]
fn 抓包实物里的资产写调用被认出且不被当成工具() {
    let raw = include_bytes!("../../../tests/fixtures/asset_echo_real.bin");
    assert!(is_tool_call(raw), "它确实走 exec 通道");
    let w = parse_exec_write(raw).expect("资产写调用必须解得出来");
    assert_eq!(
        w.path,
        "/assets/attach-0-6bd00159-1e01-4924-b8c2-12f28cc81e53.png"
    );
    assert_eq!(w.bytes.len(), 73, "1x1 探针 PNG 的原始字节数");
    assert_eq!(w.id, 0, "实物帧里关联 id 缺省");
    assert_eq!(w.exec_id, "");
    assert!(
        parse_tool_call(raw).is_none(),
        "资产写调用没有工具名,绝不能交给调用方当工具执行"
    );
    assert!(parse_exec_read(raw).is_none(), "写不是读");
}

/// 写调用认领条件:绝对路径 + 内容在场(file_bytes 或 file_text),缺任何一个
/// 都不认领 —— 真工具调用帧的 `1.2.3` 也是个字符串(另一个 id,PROTOCOL §13.2),
/// 单靠「能解出字段」会把工具帧吞成写调用,工具回路直接哑掉。
#[test]
fn 写调用缺路径或缺内容都不认领() {
    // 有字节没路径。
    let mut args = Writer::new();
    args.bytes(5, b"xxxx");
    let mut ch = Writer::new();
    ch.message(EXEC_WRITE_ARGS, &args);
    let mut top = Writer::new();
    top.message(RESP_TOOL_CHANNEL, &ch);
    assert!(parse_exec_write(&top.into_bytes()).is_none());

    // 有路径没内容。
    let mut args = Writer::new();
    args.string(1, "/assets/attach-0-x.png");
    let mut ch = Writer::new();
    ch.message(EXEC_WRITE_ARGS, &args);
    let mut top = Writer::new();
    top.message(RESP_TOOL_CHANNEL, &ch);
    assert!(parse_exec_write(&top.into_bytes()).is_none());

    // 相对路径不认领(工具帧 1.2.3 的 id 形状)。
    let mut args = Writer::new();
    args.string(1, "3f2a1b9c-0-x7ab");
    args.bytes(5, b"xxxx");
    let mut ch = Writer::new();
    ch.message(EXEC_WRITE_ARGS, &args);
    let mut top = Writer::new();
    top.message(RESP_TOOL_CHANNEL, &ch);
    assert!(parse_exec_write(&top.into_bytes()).is_none());

    // file_text(field 2)也是合法内容载体。
    let mut args = Writer::new();
    args.string(1, "/assets/note.txt");
    args.string(2, "hello");
    let mut ch = Writer::new();
    ch.uint(EXEC_ID, 42);
    ch.string(EXEC_EXEC_ID, "exec-1");
    ch.message(EXEC_WRITE_ARGS, &args);
    let mut top = Writer::new();
    top.message(RESP_TOOL_CHANNEL, &ch);
    let w = parse_exec_write(&top.into_bytes()).expect("file_text 形态也要认得");
    assert_eq!(w.bytes, b"hello");
    assert_eq!(w.id, 42);
    assert_eq!(w.exec_id, "exec-1");
}

/// `is_tool_call` 保持宽松(字段在就算,不看 wire type)—— 宁可错进
/// 工具分支再分类,也不能漏掉真工具帧陪上游死等心跳。
#[test]
fn 工具通道判定不挑_wire_type() {
    let mut top = Writer::new();
    top.uint(RESP_TOOL_CHANNEL, 0);
    assert!(is_tool_call(&top.into_bytes()));
    // 但解析入口是严的:wire type 不是 length-delimited 就不认领。
    let mut top2 = Writer::new();
    top2.uint(RESP_TOOL_CHANNEL, 0);
    let top2 = top2.into_bytes();
    assert!(parse_exec_write(&top2).is_none());
    assert!(parse_tool_call(&top2).is_none());
}

/// 读调用解析:绝对路径才认领,关联 id 带回。
#[test]
fn 读调用解析() {
    let mut args = Writer::new();
    args.string(1, "/assets/attach-0-x.png");
    let mut ch = Writer::new();
    ch.uint(EXEC_ID, 9);
    ch.message(EXEC_READ_ARGS, &args);
    let mut top = Writer::new();
    top.message(RESP_TOOL_CHANNEL, &ch);
    let r = parse_exec_read(&top.into_bytes()).expect("读调用必须解得出");
    assert_eq!(r.path, "/assets/attach-0-x.png");
    assert_eq!(r.id, 9);
    assert!(
        parse_exec_write(&{
            let mut args = Writer::new();
            args.string(1, "/assets/attach-0-x.png");
            let mut ch = Writer::new();
            ch.message(EXEC_READ_ARGS, &args);
            let mut top = Writer::new();
            top.message(RESP_TOOL_CHANNEL, &ch);
            top.into_bytes()
        })
        .is_none(),
        "读不是写"
    );
}

/// ⭐ 数值参数。这是 2026-08-07 那次「grok 无限重试」的直接病因:
/// 旧解析只读 `google.protobuf.Value` 的 string 档(field 3),
/// 模型传 `limit: 200` 时值在 number 档(field 2, double),解出空串,
/// 客户端收到 `limit=""` → 工具失败 → 模型换个说法再试 → 永远不收敛。
#[test]
fn 数值与布尔参数不再解成空() {
    let f = tool_call_frame(
        "read",
        "call-1-0",
        &[
            ("filePath", json!("/tmp/a.rss")),
            ("offset", json!(0)),
            ("limit", json!(200)),
            ("raw", json!(true)),
        ],
    );
    let tc = parse_tool_call(&f).unwrap();
    let m: std::collections::HashMap<_, _> = tc.args.into_iter().collect();
    assert_eq!(m["filePath"], json!("/tmp/a.rss"));
    // 必须是 JSON **数字**,不是 "200" —— schema 写 number/integer 的工具会拒字符串。
    assert_eq!(m["limit"], json!(200));
    assert!(m["limit"].is_i64(), "整数不能回成浮点或字符串");
    assert_eq!(m["offset"], json!(0));
    assert_eq!(m["raw"], json!(true));
}

/// 手写字节的 `google.protobuf.Value`:嵌套 Struct 与 List。
///
/// 不用 helper —— 这条要钉住的正是"我方对 Value 的字段号理解",
/// 用自己的 Writer 造就白测了。
#[test]
fn 解析嵌套的_struct_与_list_参数() {
    // ListValue{ 1: [ Value{2: double 1.0}, Value{3: "x"} ] }
    let mut list = Vec::new();
    let mut v1 = vec![0x11u8]; // field 2, wt 1
    v1.extend_from_slice(&1.0f64.to_le_bytes());
    list.push(0x0au8); // field 1, wt 2
    list.push(v1.len() as u8);
    list.extend_from_slice(&v1);
    let v2 = vec![0x1au8, 0x01, b'x']; // field 3, wt 2, "x"
    list.push(0x0a);
    list.push(v2.len() as u8);
    list.extend_from_slice(&v2);
    // Value{ 6: list }
    let mut val = vec![0x32u8]; // field 6, wt 2
    val.push(list.len() as u8);
    val.extend_from_slice(&list);
    assert_eq!(decode_pb_value(&val, 0), json!([1, "x"]));

    // Struct{ 1: MapEntry{1:"k", 2: Value{4: true}} } → Value{5: struct}
    let inner_true = vec![0x20u8, 0x01]; // field 4, wt 0, 1
    let mut entry = vec![0x0au8, 0x01, b'k']; // 1: "k"
    entry.push(0x12); // field 2, wt 2
    entry.push(inner_true.len() as u8);
    entry.extend_from_slice(&inner_true);
    let mut st = vec![0x0au8]; // Struct.fields = field 1
    st.push(entry.len() as u8);
    st.extend_from_slice(&entry);
    let mut sval = vec![0x2au8]; // field 5, wt 2
    sval.push(st.len() as u8);
    sval.extend_from_slice(&st);
    assert_eq!(decode_pb_value(&sval, 0), json!({"k": true}));
}

#[test]
fn 认不出的参数值解成_null_而不是空串() {
    // 未知 oneof 档位(field 99)→ null。空串会被工具当成"给了个空值"。
    let unknown = vec![0xfau8, 0x06, 0x01, b'z']; // field 99, wt 2, len 1
    assert_eq!(decode_pb_value(&unknown, 0), Value::Null);
    assert_eq!(decode_pb_value(&[], 0), Value::Null);
}

/// Cursor **内建**工具走别的字段号(实测 `.1` 终端、`.4` 读文件),
/// 必须**认不出来** —— 认出来会让调用方收到一个它没有的工具名。
#[test]
fn 内建工具调用不被当成外部工具() {
    let mut term = Writer::new();
    term.string(1, "ls -la");
    let mut one = Writer::new();
    one.message(1, &term);
    let mut detail = Writer::new();
    detail.message(1, &one); // 1.2.2.1 = 终端工具
    let mut ch = Writer::new();
    ch.string(TC_CALL_ID, "call-x-0");
    ch.message(TC_DETAIL, &detail);
    let mut msg = Writer::new();
    msg.message(RESP_TOOL_CHANNEL, &ch);
    let mut outer = Writer::new();
    outer.message(RESP_MESSAGE, &msg);
    let f = outer.into_bytes();
    assert!(is_tool_call(&f), "仍要认出「这是工具帧」好主动收口");
    assert!(parse_tool_call(&f).is_none(), "但不能当成可转发的外部工具");
}

/// 兼容转换层正例:内建终端帧(§13.2 实证 `1.2.2.1.1.1` = 命令串)要解得出来。
#[test]
fn 内建终端帧解出命令串() {
    let mut term = Writer::new();
    term.string(1, "ls -la");
    let mut one = Writer::new();
    one.message(1, &term);
    let mut detail = Writer::new();
    detail.message(1, &one); // 1.2.2.1 = 终端工具
    let mut ch = Writer::new();
    ch.string(TC_CALL_ID, "call-x-0");
    ch.message(TC_DETAIL, &detail);
    let mut msg = Writer::new();
    msg.message(RESP_TOOL_CHANNEL, &ch);
    let mut outer = Writer::new();
    outer.message(RESP_MESSAGE, &msg);
    assert_eq!(
        parse_builtin_call(&outer.into_bytes()),
        Some(BuiltinCall::Terminal {
            id: "call-x-0".into(),
            command: "ls -la".into(),
            cwd: None,
        })
    );
}

/// 终端帧带工作目录:`ShellArgs.working_directory` = `.1.1.2`(cursor-agent agent.v1)。
#[test]
fn 内建终端帧解出工作目录() {
    // 0A 1C = 1.msg(28); 12 1A = 1.2.msg(26); 0A 04 "id-t";
    // 12 12 = 1.2.2.msg(18); 0A 10 = .1.msg(16); 0A 0E = .1.1.msg(14);
    // 0A 06 "ls -la"; 12 04 "/srv"
    let mut raw = vec![0x0A, 0x1C, 0x12, 0x1A, 0x0A, 0x04];
    raw.extend_from_slice(b"id-t");
    raw.extend_from_slice(&[0x12, 0x12, 0x0A, 0x10, 0x0A, 0x0E, 0x0A, 0x06]);
    raw.extend_from_slice(b"ls -la");
    raw.extend_from_slice(&[0x12, 0x04]);
    raw.extend_from_slice(b"/srv");
    assert_eq!(
        parse_builtin_call(&raw),
        Some(BuiltinCall::Terminal {
            id: "id-t".into(),
            command: "ls -la".into(),
            cwd: Some("/srv".into()),
        })
    );
}

/// 内建读文件帧:`1.2.2.8 = read_tool_call`,`{1:{1:path, 2:offset, 3:limit}}`
/// (cursor-agent `agent.v1.ReadToolArgs`;2026-11-07 实物帧见下一条测试)。
#[test]
fn 内建读文件帧解出路径() {
    // 手写字节:0A 1E = 1.msg(30); 12 1C = 1.2.msg(28);
    // 0A 08 "call-y-0"; 12 10 = 1.2.2.msg(16); 42 0E = .8.msg(14);
    // 0A 0C = .8.1.msg(12); 0A 0A "/tmp/a.png"
    let mut raw = vec![0x0A, 0x1E, 0x12, 0x1C, 0x0A, 0x08];
    raw.extend_from_slice(b"call-y-0");
    raw.extend_from_slice(&[0x12, 0x10, 0x42, 0x0E, 0x0A, 0x0C, 0x0A, 0x0A]);
    raw.extend_from_slice(b"/tmp/a.png");
    assert_eq!(
        parse_builtin_call(&raw),
        Some(BuiltinCall::ReadFile {
            id: "call-y-0".into(),
            path: "/tmp/a.png".into(),
            offset: None,
            limit: None,
        })
    );
}

/// 读文件带行范围:`.8.1.2 offset=440`、`.8.1.3 limit=200`;负 offset(从末尾倒数)原样解出。
/// 字段号照 cursor-agent `agent.v1.ReadToolArgs`(no:2 offset、no:3 limit,int32)。
#[test]
fn 读文件帧解出行范围() {
    // 手写字节:.8.1 = {1:"/a.tex", 2:440, 3:200}
    //   0A 06 "/a.tex";10 B8 03 (440);18 C8 01 (200) → .8.1 长 15
    let mut args = vec![0x0A, 0x06];
    args.extend_from_slice(b"/a.tex");
    args.extend_from_slice(&[0x10, 0xB8, 0x03, 0x18, 0xC8, 0x01]);
    let build = |args: &[u8]| {
        let mut read = vec![0x0A, args.len() as u8];
        read.extend_from_slice(args); // .8.1
        let mut tc = vec![0x42, read.len() as u8];
        tc.extend(read); // .8
        let mut call = vec![0x0A, 0x01, b'c', 0x12, tc.len() as u8];
        call.extend(tc); // 1.2 = {1:id, 2:tool_call}
        let mut upd = vec![0x12, call.len() as u8];
        upd.extend(call);
        let mut raw = vec![0x0A, upd.len() as u8];
        raw.extend(upd);
        raw
    };
    assert_eq!(
        parse_builtin_call(&build(&args)),
        Some(BuiltinCall::ReadFile {
            id: "c".into(),
            path: "/a.tex".into(),
            offset: Some(440),
            limit: Some(200),
        })
    );
    // offset=-50(int32 负数,10 字节补码 varint)
    let mut tail = vec![0x0A, 0x06];
    tail.extend_from_slice(b"/a.tex");
    tail.extend_from_slice(&[
        0x10, 0xCE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01,
    ]);
    match parse_builtin_call(&build(&tail)) {
        Some(BuiltinCall::ReadFile { offset, limit, .. }) => {
            assert_eq!(offset, Some(-50));
            assert_eq!(limit, None);
        }
        other => panic!("{other:?}"),
    }
}

/// ⭐ 读文件实物帧(2026-11-07 fable-5.1 修 bug 任务):`.8.1.1` = 绝对路径。
#[test]
fn 读文件实物帧解出路径() {
    let raw = include_bytes!("../../../tests/fixtures/tool_call_builtin_read_real.bin");
    match parse_builtin_call(raw) {
        Some(BuiltinCall::ReadFile { id, path, .. }) => {
            assert_eq!(id, "toolu_01G94VUY1UfDkZCTx5FAPRz8");
            assert!(path.ends_with("/calc.py"), "{path}");
        }
        other => panic!("应解成读文件: {other:?}"),
    }
}

/// 内建找文件帧:`1.2.2.4 = glob_tool_call`,`{1:{1:target_directory, 2:glob_pattern}}`
/// (cursor-agent `agent.v1.GlobToolArgs`)。**它不是读文件** —— 旧代码把模式
/// 当路径交给客户端的读文件工具,得到「File not found: **/report.pdf」。
#[test]
fn 内建找文件帧解出目录与模式() {
    // 0A 1C = 1.msg(28); 12 1A = 1.2.msg(26); 0A 04 "id-g";
    // 12 12 = 1.2.2.msg(18); 22 10 = .4.msg(16); 0A 0E = .4.1.msg(14);
    // 0A 04 "/srv"; 12 06 "**/*.r"
    let mut raw = vec![0x0A, 0x1C, 0x12, 0x1A, 0x0A, 0x04];
    raw.extend_from_slice(b"id-g");
    raw.extend_from_slice(&[0x12, 0x12, 0x22, 0x10, 0x0A, 0x0E, 0x0A, 0x04]);
    raw.extend_from_slice(b"/srv");
    raw.extend_from_slice(&[0x12, 0x06]);
    raw.extend_from_slice(b"**/*.r");
    assert_eq!(
        parse_builtin_call(&raw),
        Some(BuiltinCall::Glob {
            id: "id-g".into(),
            dir: Some("/srv".into()),
            pattern: "**/*.r".into()
        })
    );
}

/// ⭐ 找文件实物帧(2026-11-07):目录 `/home/iiap`、模式 `**/report.pdf`。
#[test]
fn 找文件实物帧不被当成读文件() {
    let raw = include_bytes!("../../../tests/fixtures/tool_call_builtin_glob_real.bin");
    assert_eq!(
        parse_builtin_call(raw),
        Some(BuiltinCall::Glob {
            id: "toolu_011yoGiSbvB5CQ2VxBwwEzcR".into(),
            dir: Some("/home/iiap".into()),
            pattern: "**/report.pdf".into()
        })
    );
}

/// 内建编辑帧 `.12 = {1:{1:路径, 6:内容}}`(agent.v1 edit_tool_call)。
/// 内容在 Write 时是全文、在 StrReplace 时只是片段,所以只取路径。
#[test]
fn 内建编辑帧只认路径_不当整文件写() {
    let mut wr = Writer::new();
    wr.string(1, "/tmp/news.html");
    wr.string(6, "<!DOCTYPE html><html></html>");
    let mut one = Writer::new();
    one.message(1, &wr);
    let mut detail = Writer::new();
    detail.message(12, &one); // 1.2.2.12 = 写文件
    let mut ch = Writer::new();
    ch.string(TC_CALL_ID, "call-w-0");
    ch.message(TC_DETAIL, &detail);
    let mut msg = Writer::new();
    msg.message(RESP_TOOL_CHANNEL, &ch);
    let mut outer = Writer::new();
    outer.message(RESP_MESSAGE, &msg);
    let f = outer.into_bytes();
    assert!(is_tool_call(&f));
    assert!(parse_tool_call(&f).is_none(), "内建写不能当成外部工具");
    assert_eq!(builtin_tool_ident(&f), Some(12));
    // `.12` 是编辑帧:局部替换时内容只是片段,绝不能当整文件写交付(2026-11-07 事故)。
    assert_eq!(parse_builtin_call(&f), None, "编辑帧不翻成整文件写");
    assert_eq!(edit_call_path(&f).as_deref(), Some("/tmp/news.html"));
}

/// 内建写文件的边界:路径空 / 内容字段缺失 → None(落回收口,单向失败)。
/// 内容空串但字段在 = 合法(写空文件)。
#[test]
fn 内建编辑帧的边界() {
    let build = |path: &str, content: Option<&str>| {
        let mut wr = Writer::new();
        wr.string(1, path);
        if let Some(c) = content {
            wr.string(6, c);
        }
        let mut one = Writer::new();
        one.message(1, &wr);
        let mut detail = Writer::new();
        detail.message(12, &one);
        let mut ch = Writer::new();
        ch.message(TC_DETAIL, &detail);
        let mut msg = Writer::new();
        msg.message(RESP_TOOL_CHANNEL, &ch);
        let mut outer = Writer::new();
        outer.message(RESP_MESSAGE, &msg);
        outer.into_bytes()
    };
    assert_eq!(edit_call_path(&build("  ", Some("x"))), None, "空路径不认");
    assert_eq!(
        edit_call_path(&build("/tmp/a", None)).as_deref(),
        Some("/tmp/a")
    );
    // 生产事故原样:StrReplace 的片段内容,任何形态都不许翻成写调用。
    let frag = "p.add_argument('--putdown')\na=p.parse_args()";
    assert_eq!(parse_builtin_call(&build("/tmp/a.py", Some(frag))), None);
    assert_eq!(parse_builtin_call(&build("/tmp/a", Some(""))), None);
}

/// 内建网页抓取帧(2026-09-06 e2e 实物帧实证 `.37 = {1:{1:URL, 2:call id 回显}}`)。
#[test]
fn 内建网页抓取帧解出_url() {
    let mut wf = Writer::new();
    wf.string(1, "https://news.ycombinator.com");
    wf.string(2, "call-f-0\nfc_echo_0"); // call id 回显,不是参数
    let mut one = Writer::new();
    one.message(1, &wf);
    let mut detail = Writer::new();
    detail.message(37, &one); // 1.2.2.37 = 网页抓取
    let mut ch = Writer::new();
    ch.string(TC_CALL_ID, "call-f-0");
    ch.message(TC_DETAIL, &detail);
    let mut msg = Writer::new();
    msg.message(RESP_TOOL_CHANNEL, &ch);
    let mut outer = Writer::new();
    outer.message(RESP_MESSAGE, &msg);
    let f = outer.into_bytes();
    assert!(is_tool_call(&f));
    assert!(parse_tool_call(&f).is_none());
    assert_eq!(builtin_tool_ident(&f), Some(37));
    assert_eq!(
        parse_builtin_call(&f),
        Some(BuiltinCall::WebFetch {
            id: "call-f-0".into(),
            url: "https://news.ycombinator.com".into()
        })
    );
    // 空 URL → None(单向失败)。
    let mut wf = Writer::new();
    wf.string(1, "  ");
    let mut one = Writer::new();
    one.message(1, &wf);
    let mut detail = Writer::new();
    detail.message(37, &one);
    let mut ch = Writer::new();
    ch.message(TC_DETAIL, &detail);
    let mut msg = Writer::new();
    msg.message(RESP_TOOL_CHANNEL, &ch);
    let mut outer = Writer::new();
    outer.message(RESP_MESSAGE, &msg);
    assert_eq!(parse_builtin_call(&outer.into_bytes()), None);
}

/// 边界钉死:外部工具帧、exec 资产帧(顶层 field 2)、空命令都不许被
/// 当成内建调用 —— 转换层的失败必须是单向的(认不出 → 落回收口)。
#[test]
fn 内建调用解析的边界() {
    // 外部工具(.15)归 parse_tool_call,不归转换层。
    let mut inner = Writer::new();
    inner.string(1, "gwtools-read");
    inner.string(TC_BARE_NAME, "read");
    let mut ext = Writer::new();
    ext.message(TC_INNER, &inner);
    let mut detail = Writer::new();
    detail.message(TC_EXTERNAL, &ext);
    let mut ch = Writer::new();
    ch.string(TC_CALL_ID, "call-z-0");
    ch.message(TC_DETAIL, &detail);
    let mut msg = Writer::new();
    msg.message(RESP_TOOL_CHANNEL, &ch);
    let mut outer = Writer::new();
    outer.message(RESP_MESSAGE, &msg);
    assert_eq!(parse_builtin_call(&outer.into_bytes()), None);

    // exec 资产帧不裹 `1.2`(顶层 field 2 = exec_server_message),它的
    // field 2 恰好是 shell_args —— 越界去解就是张冠李戴翻译出假终端调用。
    let real: &[u8] = include_bytes!("../../../tests/fixtures/asset_echo_real.bin");
    assert_eq!(parse_builtin_call(real), None);

    // 空命令 → None(落回收口),绝不产出空参数的调用。
    let mut term = Writer::new();
    term.string(1, "   ");
    let mut one = Writer::new();
    one.message(1, &term);
    let mut detail = Writer::new();
    detail.message(1, &one);
    let mut ch = Writer::new();
    ch.message(TC_DETAIL, &detail);
    let mut msg = Writer::new();
    msg.message(RESP_TOOL_CHANNEL, &ch);
    let mut outer = Writer::new();
    outer.message(RESP_MESSAGE, &msg);
    assert_eq!(parse_builtin_call(&outer.into_bytes()), None);
}
