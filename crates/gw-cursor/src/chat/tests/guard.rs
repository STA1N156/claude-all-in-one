//! chat 测试:护栏话术与能力别名。

use super::*;

/// 转换表的键名必须来自声明工具的 `input_schema.properties`,绝不猜:
/// Claude Code 是 `command`/`file_path`,opencode 是 `command`/`filePath`,
/// schema 里没有候选键就放弃翻译(落回收口)。
#[test]
fn 转换表按声明工具的_schema_认参数键名() {
    // Claude Code 形态。
    let tools = vec![
        tool_with_schema(
            "Bash",
            r#"{"type":"object","properties":{"command":{"type":"string"}}}"#,
        ),
        tool_with_schema(
            "Read",
            r#"{"type":"object","properties":{"file_path":{"type":"string"}}}"#,
        ),
    ];
    let x = BuiltinXlate::from_tools(&tools);
    assert_eq!(x.terminal, Some(("Bash".into(), "command".into())));
    assert_eq!(x.read_file, Some(("Read".into(), "file_path".into())));

    // opencode 形态(小写名 + filePath)。
    let tools = vec![
        tool_with_schema("bash", r#"{"properties":{"command":{}}}"#),
        tool_with_schema("read", r#"{"properties":{"filePath":{}}}"#),
    ];
    let x = BuiltinXlate::from_tools(&tools);
    assert_eq!(x.terminal, Some(("bash".into(), "command".into())));
    assert_eq!(x.read_file, Some(("read".into(), "filePath".into())));

    // schema 没有任何候选键 → 放弃翻译,绝不猜键名。
    let tools = vec![tool_with_schema(
        "Bash",
        r#"{"properties":{"script_text":{}}}"#,
    )];
    assert_eq!(BuiltinXlate::from_tools(&tools).terminal, None);

    // 一个工具都没声明 → 两项都空(转换层整体失活,行为回到收口)。
    let x = BuiltinXlate::from_tools(&[]);
    assert_eq!(x.terminal, None);
    assert_eq!(x.read_file, None);
}

#[test]
fn 护栏逐个列出可用工具全名() {
    // 列名是闭集,比任何否定句都硬 —— 这是第三版护栏的主要杠杆。
    let g = guard(&tdefs(&["Read", "Bash"]));
    assert!(g.contains("`gwtools-Read`"), "{g}");
    assert!(g.contains("`gwtools-Bash`"), "{g}");
    assert!(g.contains("这 2 个"), "{g}");
    assert!(g.contains("清单之外**不存在**任何其他工具"), "{g}");
}

#[test]
fn 护栏给能力替代表而不是只列禁令() {
    // 生产 302 次内建收口里模型要的就是 shell 与搜代码,必须有明确去处。
    let g = guard(&tdefs(&["Read", "Bash", "Grep", "Edit", "WebSearch"]));
    assert!(g.contains("跑命令/终端调用 `gwtools-Bash`"), "{g}");
    assert!(g.contains("读文件调用 `gwtools-Read`"), "{g}");
    assert!(g.contains("搜代码/找文件调用 `gwtools-Grep`"), "{g}");
    assert!(g.contains("查网页调用 `gwtools-WebSearch`"), "{g}");
    // 有专用网页工具时不出现 curl 回退行(见下条测试的反面)。
    assert!(!g.contains("curl/wget"), "{g}");
}

#[test]
fn 无网页工具有终端时联网回退到curl() {
    // 2026-09-04 pi 卡死案:没有网页工具时 grok 会去编 Cursor 的
    // WebSearch/WebFetch(未注册 → 上游挂死)。给它一条合法出路。
    let g = guard(&tdefs(&["read", "bash", "edit", "write"]));
    assert!(g.contains("curl/wget"), "{g}");
    assert!(g.contains("调用 `gwtools-bash`"), "{g}");
    // MCP/CLI 裸名版同样要有这条出路。
    let m = mcp_tool_guard_with(
        &tdefs(&["read", "bash", "edit", "write"]),
        crate::DEFAULT_TOOL_GUARD_POLICY,
    );
    assert!(m.contains("curl/wget"), "{m}");
    assert!(m.contains("调用 `bash`"), "{m}");
}

#[test]
fn 连终端也没有时不加联网回退() {
    // 指向不存在的出路比不出更糟。
    let g = guard(&tdefs(&["Read"]));
    assert!(!g.contains("curl/wget"), "{g}");
}

/// 2026-11-09:同时有整写与局部编辑工具时,替代表拆成两行,局部修改指向编辑工具。
#[test]
fn 替代表拆分整写与局部编辑() {
    let g = guard(&tdefs(&["read", "write", "edit", "bash"]));
    assert!(g.contains("新建或整体重写文件调用 `gwtools-write`"), "{g}");
    assert!(g.contains("局部修改已有文件调用 `gwtools-edit`"), "{g}");
    assert!(!g.contains("写/改文件调用"), "拆分后不再出合并行: {g}");
    let m = mcp_tool_guard_with(&tdefs(&["Write", "Edit", "Bash"]), "");
    assert!(m.contains("新建或整体重写文件调用 `Write`"), "{m}");
    assert!(m.contains("局部修改已有文件调用 `Edit`"), "{m}");
    // 只有一种时照旧合并成一行(不指向不存在的工具)。
    let only_write = mcp_tool_guard_with(&tdefs(&["write", "bash"]), "");
    assert!(only_write.contains("写/改文件调用 `write`"), "{only_write}");
    assert!(!only_write.contains("局部修改"), "{only_write}");
}

#[test]
fn 认得出客户端时点明运行环境() {
    let oc = json!({
        "system": "You are OpenCode, the best coding agent on the planet.",
        "tools": [{"name": "write"}, {"name": "edit"}],
        "messages": [{"role": "user", "content": "写个网页"}]
    });
    let note = client_env_note(&oc);
    assert!(note.contains("OpenCode"), "{note}");
    assert!(note.contains("用户的机器上执行"), "{note}");
    let unknown = json!({"system": "You are a helpful assistant.", "messages": []});
    assert_eq!(client_env_note(&unknown), "", "认不出客户端时不写环境说明");
}

/// gpt-5.6-sol 评审的三处删减,钉成断言 —— 这三句删掉是**刻意的**,
/// 后果威胁与内建工具点名单要作为热配置里的实验变体存在,不进第一版默认。
/// 没有这个测试,下一个人「顺手把话说狠一点」就会把第二版翻车的原因原样种回来。
#[test]
fn 默认护栏不含后果威胁也不点名内建工具() {
    let g = guard(&tdefs(&["Read", "Bash", "Edit"]));
    assert!(!g.contains("截断"), "后果威胁不进默认版: {g}");
    assert!(!g.contains("半句话"), "后果威胁不进默认版: {g}");
    assert!(!g.contains("Cursor"), "不点名 Cursor 内建工具: {g}");
    assert!(!g.contains("那类调用"), "指代不清的说法要删掉: {g}");
    // 保留的那半句:「不会返回任何结果」+「别解释命名规则」。
    assert!(g.contains("不会返回任何结果"), "{g}");
    assert!(g.contains("不要向用户解释这套工具命名规则"), "{g}");
}

#[test]
fn 替代表按匹配紧密度取而不是声明顺序() {
    // TodoWrite 子串里带 write,排在 Write 前面时不能把「写文件」指向改待办的工具。
    let g = guard(&tdefs(&["TodoWrite", "Write"]));
    assert!(g.contains("写/改文件调用 `gwtools-Write`"), "{g}");
    assert!(
        !g.contains("`gwtools-TodoWrite`。"),
        "不该把写文件指向改待办的工具: {g}"
    );
    // 只有 TodoWrite 时:纯子串档已被去掉,该能力干脆不出替代行(宁缺勿错)。
    let only_todo = guard(&tdefs(&["TodoWrite"]));
    assert!(!only_todo.contains("写/改文件"), "{only_todo}");
    // 同理 Thread 不该被当成读文件工具(子串 read 命中过的那个坑)。
    let thread = guard(&tdefs(&["Thread"]));
    assert!(!thread.contains("读文件"), "{thread}");
    // 而 read_file / run_terminal_cmd 这类真别名要认得出。
    let aliases = guard(&tdefs(&[
        "read_file",
        "run_terminal_cmd",
        "codebase_search",
    ]));
    assert!(
        aliases.contains("读文件调用 `gwtools-read_file`"),
        "{aliases}"
    );
    assert!(
        aliases.contains("跑命令/终端调用 `gwtools-run_terminal_cmd`"),
        "{aliases}"
    );
    assert!(
        aliases.contains("搜代码/找文件调用 `gwtools-codebase_search`"),
        "{aliases}"
    );
}

#[test]
fn 没有对应工具的能力不出替代行() {
    // 指向一个不存在的工具比不指更糟。
    let g = guard(&tdefs(&["Read"]));
    assert!(g.contains("读文件调用 `gwtools-Read`"), "{g}");
    assert!(!g.contains("跑命令/终端"), "{g}");
    assert!(!g.contains("查网页"), "{g}");
}

#[test]
fn 一个可替代工具都没有时给兜底话术() {
    let g = guard(&tdefs(&["TaskCreate", "Skill"]));
    assert!(g.contains("`gwtools-TaskCreate`"), "{g}");
    assert!(g.contains("直接向用户说明你需要什么"), "{g}");
}

#[test]
fn 工具过多时不列名只留通则() {
    // 名字长度不定,所以按**字符预算**而不是个数卡:堆到超预算为止。
    let many: Vec<String> = (0..400).map(|i| format!("tool_number_{i}")).collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    let g = guard(&tdefs(&refs));
    assert!(
        !g.contains("`gwtools-tool_number_0`"),
        "超预算不该逐个列名: {g}"
    );
    assert!(g.contains("清单之外**不存在**任何其他工具"), "{g}");
    // 预算内则要列。
    let few: Vec<&str> = refs[..5].to_vec();
    assert!(guard(&tdefs(&few)).contains("`gwtools-tool_number_0`"));
}

#[test]
fn 无工具时仍是那版无工具约束() {
    let g = guard(&[]);
    assert!(g.contains("没有任何工具的环境"), "{g}");
    assert!(g.contains("无法得知当前日期"), "{g}");
    assert!(!g.contains("gwtools-"), "{g}");
}

#[test]
fn 策略句可热改且空值回内置默认() {
    let _g = crate::GUARD_TEST_LOCK.lock().unwrap();
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = crate::set_tool_guard_policy("");
        }
    }
    let _r = Restore;

    crate::set_tool_guard_policy("测试策略句:只准回答 PINEAPPLE。").unwrap();
    let g = builtin_tool_guard(&tdefs(&["Read"]));
    assert!(g.contains("只准回答 PINEAPPLE"), "{g}");
    assert!(
        !g.contains("不会返回任何结果"),
        "热配版应整段顶掉默认策略句: {g}"
    );
    // 闭集与替代表是**代码生成**的,不受配置影响 —— 这是不做占位符模板的理由。
    assert!(g.contains("`gwtools-Read`"), "{g}");
    assert!(g.contains("读文件调用 `gwtools-Read`"), "{g}");

    // 空 = 回内置默认。
    crate::set_tool_guard_policy("   ").unwrap();
    assert!(builtin_tool_guard(&tdefs(&["Read"])).contains("不会返回任何结果"));
}

#[test]
fn 策略句过长被拒且保留上一份有效值() {
    let _g = crate::GUARD_TEST_LOCK.lock().unwrap();
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = crate::set_tool_guard_policy("");
        }
    }
    let _r = Restore;

    crate::set_tool_guard_policy("良好策略句 ALPHA").unwrap();
    let too_long = "啊".repeat(3000);
    let err = crate::set_tool_guard_policy(&too_long).expect_err("过长必须被拒");
    assert!(err.contains("过长"), "{err}");
    // **不静默回默认**:护栏效果正在按 guard_rev 分桶比对,悄悄换一版会让数据作废。
    let g = builtin_tool_guard(&tdefs(&["Read"]));
    assert!(g.contains("ALPHA"), "校验失败要保留上一份有效值: {g}");
}

#[test]
fn 截断后下一轮注入纠偏且只注入一次() {
    let n = crate::TruncationNotices::default();
    // 认得出能力 + 调用方声明了同义工具 → 指名换哪个。
    n.record("conv-a", Some("跑命令/终端"));
    let cap = n.take("conv-a").expect("标记应在");
    let notice = truncation_notice(cap, &tdefs(&["Bash", "Read"]));
    assert!(notice.contains("上一轮中断"), "{notice}");
    assert!(notice.contains("`gwtools-Bash`"), "{notice}");
    // 消费语义:取过就没了,不会在整个会话里反复出现。
    assert!(n.take("conv-a").is_none(), "标记应已被消费");
}

#[test]
fn 认不出能力时纠偏给通用话术不瞎指工具() {
    // 「你上次调了内建终端」这种自信的错话比模糊的实话更容易把模型带偏。
    let notice = truncation_notice(None, &tdefs(&["Bash", "Read"]));
    assert!(notice.contains("上一轮中断"), "{notice}");
    assert!(
        !notice.contains("`gwtools-Bash`"),
        "认不出就别指名: {notice}"
    );
    assert!(
        notice.contains("gwtools-"),
        "仍要说清只能用带前缀的那些: {notice}"
    );

    // 认得出能力、但调用方没声明同义工具 → 同样退回通用话术。
    let notice = truncation_notice(Some("跑命令/终端"), &tdefs(&["TaskCreate"]));
    assert!(!notice.contains("`gwtools-TaskCreate`"), "{notice}");
}

#[test]
fn 裸名注册时纠偏话术不带_gwtools_前缀() {
    // CLI 形态(线协议默认)的工具按裸名注册,护栏早已切到裸名版;
    // 纠偏话术若还写 `gwtools-`,模型会去找一个不存在的命名空间,
    // 回用户「gwtools 命名空间不可用」(2026-11-07 用户实际遇到)。
    let notice = truncation_notice_for(Some("跑命令/终端"), &tdefs(&["Bash", "Read"]), true);
    assert!(notice.contains("`Bash`"), "{notice}");
    assert!(!notice.contains("gwtools"), "{notice}");

    let notice = truncation_notice_for(None, &tdefs(&["Bash", "Read"]), true);
    assert!(notice.contains("上一轮中断"), "{notice}");
    assert!(!notice.contains("gwtools"), "{notice}");
    assert!(
        notice.contains("原样"),
        "仍要说清只能用声明的名字: {notice}"
    );
}

#[test]
fn 内建身份字段号翻成能力说法() {
    // 只认有实证/官方定义的字段号:.1 终端、.4 glob(找文件)、.8 读文件
    // (2026-11-07 按 cursor-agent agent.v1 更正:.4 不是读文件),其余一律 None。
    // 不认识的返回 None 是**刻意**的:枚举要靠抓实物定死,不靠字段号相邻猜。
    let frame = |ident: u32| {
        let mut inner = crate::protobuf::Writer::new();
        inner.string(1, "ls -la");
        let mut detail = crate::protobuf::Writer::new();
        detail.message(ident, &inner);
        let mut ch = crate::protobuf::Writer::new();
        ch.string(1, "call-1");
        ch.message(2, &detail);
        let mut msg = crate::protobuf::Writer::new();
        msg.message(2, &ch);
        let mut top = crate::protobuf::Writer::new();
        top.message(1, &msg);
        top.into_bytes()
    };
    assert_eq!(builtin_capability(&frame(1)), Some("跑命令/终端"));
    assert_eq!(builtin_capability(&frame(4)), Some("搜代码/找文件"));
    assert_eq!(builtin_capability(&frame(8)), Some("读文件"));
    assert_eq!(
        builtin_capability(&frame(7)),
        None,
        "没实证过的字段号不许猜"
    );
    // 外部工具(.15)走 parse_tool_call,不该被当成内建。
    assert_eq!(builtin_capability(&frame(15)), None);
}
