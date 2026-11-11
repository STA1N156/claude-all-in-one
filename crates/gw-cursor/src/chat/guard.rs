//! 内建工具护栏话术、调用方工具能力别名表、截断提示。
//!
//! 2026-11-10 由 chat.rs 拆出(只搬不改行为)。

use super::*;

/// 拦住模型去调 Cursor 的**内建**工具。
///
/// ## 为什么需要这道护栏
///
/// Cursor 的内建工具(终端、读写文件、网页搜索)是**服务端自带**的:哪怕我们一个工具都
/// 不声明,模型照样会调。而它们的结果要由客户端在真实机器上执行后回传 —— 反代做不到,
/// 也**不该做**(那等于跑模型选定的任意 shell 命令)。
///
/// 不拦的实际后果(2026-08-07 实测):问一句「帮我查今天的新闻」,模型先输出一句
/// 「先确认今天日期,再查最新新闻」,然后去调 `date '+%Y-%m-%d'`,我方只能收口 ——
/// 用户看到的是一句没头没尾的计划,像卡死了。
///
/// 系统提示能改行为是实证过的(往 `1.2.1.2.25` 塞「只准回答 PINEAPPLE」,模型照做)。
///
/// ## 2026-08-13 第三版:为什么必须列名 + 给替代表
///
/// 生产实测(worker-cursor 12 小时):外部工具成功 **3119** 次,内建工具收口
/// **302** 次(8.8%),其中 **296 次发生在已出字之后** —— 客户端收到的是一段
/// 半截回答 + 正常的 `end_turn`,既没有错误也没有工具调用。用户的报障原话是
/// 「工具调用总失败」,看到的就是这个。落盘的 preview 里,模型要调的是
/// `echo …; grep -nE …`(shell)和「按符号名 + 目录搜代码」——**全都是调用方
/// 已经声明过的能力**(Claude Code 的 `Bash` / `Grep`),模型只是伸手抓了
/// Cursor 服务端那个同义的内建工具。
///
/// 第二版文案(「声明的工具都是真的,但不要调内建的终端/文件读写/网页搜索/
/// 代码库检索」)在这种情况下是**自相矛盾**的:调用方声明的工具就叫 `Bash`、
/// `Read`、`Edit`,能力与被点名禁掉的那几项逐字重合。模型先按第一句认下工具、
/// 写好计划,到了真要动手那一步又撞上第二句的禁令,于是去抓了名字不在禁令里
/// 的那个(内建)。
///
/// 第三版换掉「禁什么」的写法,改成三条正向信息:
/// 1. **把可用工具的全名逐个列出来** —— 闭集,比任何否定句都硬;
/// 2. **给能力→工具的替代表**(要跑命令用哪个、要读文件用哪个),按调用方
///    实际声明的名字生成 —— 模型伸手时有个明确去处,而不只是被拦住;
/// 3. 策略句(「没有的能力怎么办」)交给 [`crate::tool_guard_policy`] 从热配置取,
///    可以在后台改、当场生效,不用重新部署就能 A/B 文案。
///
/// ## 刻意**没有**写进去的东西(gpt-5.6-sol 对抗评审,2026-08-13)
///
/// - **不点名「Cursor 内建的终端 / 文件读写 / 代码库检索」**。这正是第二版翻车的那句:
///   点名的能力与调用方的 `Bash`/`Read`/`Edit` 逐字重合,会把合法工具一起吓退。
/// - **不写「否则你这一轮回答会被截断」**。后果威胁可能让模型对合法工具也变保守,
///   或者转头在正文里跟用户解释网关/Cursor 环境。它作为**热配置里的实验变体**存在,
///   不作第一版默认值 —— 「文案自相矛盾」和「Cursor 服务端 agent prompt 对内建工具
///   有更强偏置」这两个解释目前同样成立,要靠线上分桶才能分开,所以第一版只描述动作规则。
///
/// ## 结构:哪部分是代码、哪部分是配置
///
/// ```text
/// [代码] 前缀说明 + 工具闭集(按本次请求的 tools 生成)
/// [代码] 能力替代表(按本次请求的 tools 生成)
/// [热配] 策略句(默认见 crate::DEFAULT_TOOL_GUARD_POLICY)
/// ```
///
/// 动态部分的**结构**不会变,变的只是那几句自然语言 —— 所以配置里不放
/// `{tools}` / `{redirects}` 占位符:模板一旦拼错,整个闭集会被静默丢掉,
/// 而闭集恰恰是这道护栏最硬的那一半。
pub(super) fn builtin_tool_guard(tools: &[run::ToolDef]) -> String {
    builtin_tool_guard_with(tools, &crate::tool_guard_policy())
}

/// [`builtin_tool_guard`] 的纯函数版:策略句由调用方给,不读进程全局。
///
/// 拆出来是为了可测:护栏文案的断言(列名、替代表、删掉哪几句)与
/// 「热配置能不能改」是两件事。读全局的话每个文案测试都要抢同一把锁,
/// 而它们**本来不关心**全局里此刻是哪一版 —— 那种耦合的表现是
/// 「单跑全绿、全量跑随机红」(2026-08-13 真踩过一次)。
pub(super) fn builtin_tool_guard_with(tools: &[run::ToolDef], policy: &str) -> String {
    if tools.is_empty() {
        // 无工具的分支**不受热配置影响**:302 次内建收口全部发生在有工具的请求上,
        // 这一版文案没有需要 A/B 的东西,多一个旋钮只多一处能配错的地方。
        return "\n\n[运行环境约束]你运行在一个没有任何工具的环境里。\
                **不要**尝试调用终端/命令行、读写文件、网页搜索或代码库检索 —— 调用不会有结果。\
                直接基于已有信息回答;信息不足时说明缺什么,并让用户提供。\
                (特别地:你无法得知当前日期,也无法访问网络。)"
            .to_string();
    }

    let mut s = String::from("\n\n[运行环境]你的工具在协议里以 `");
    s.push_str(run::TOOL_NS);
    s.push_str("-` 前缀注册(声明为 `read` 的,调用名就是 `");
    s.push_str(run::TOOL_NS);
    s.push_str("-read`)。");

    // 工具闭集。按**字符预算**决定列不列,而不是按工具个数卡死 ——
    // 「第 25 个工具让闭集突然消失」是说不通的:挂了 MCP server 的复杂场景恰恰
    // 更需要闭集,而工具的完整 schema 早就比这份名单大一个数量级了。
    let names: Vec<String> = tools
        .iter()
        .map(|t| format!("`{}-{}`", run::TOOL_NS, t.name))
        .collect();
    let listed = names.join("、");
    if listed.chars().count() <= GUARD_LIST_BUDGET_CHARS {
        s.push_str("本次可用的**全部**工具就是这 ");
        s.push_str(&tools.len().to_string());
        s.push_str(" 个:");
        s.push_str(&listed);
        s.push_str("。清单之外**不存在**任何其他工具。");
    } else {
        s.push_str("本次声明的工具就是你的全部工具,清单之外**不存在**任何其他工具。");
    }

    // 能力替代表:模型真正会去抓内建工具的那几个能力,逐个指向调用方声明的同义工具。
    // 只为**确实声明了**对应工具的能力出行 —— 指向一个不存在的工具比不指更糟。
    s.push_str(&render_redirects(tools, &format!("{}-", run::TOOL_NS)));

    // 策略句(热配置;快照由 [`builtin_tool_guard`] 取好传进来,这里不碰锁)。
    if !policy.trim().is_empty() {
        s.push('\n');
        s.push_str(policy.trim());
    }
    s
}

/// MCP/CLI 形态(线协议)的护栏:与 [`builtin_tool_guard_with`] 同构,
/// 但工具以**裸名**注册 —— mcp_tools 目录里没有 `gwtools-` 命名空间前缀,
/// 模型按裸名回调(2026-09-04 探针实证,get_weather 原样回来)。
/// 闭集断言与能力替代表保留:Cursor 内建工具我们依旧执行不了。
pub(super) fn mcp_tool_guard_with(tools: &[run::ToolDef], policy: &str) -> String {
    let mut s = String::from("\n\n[运行环境]你的工具在协议里以**裸名**注册(声明为 `read`,调用名就是 `read`,不带任何前缀)。");

    let names: Vec<String> = tools.iter().map(|t| format!("`{}`", t.name)).collect();
    let listed = names.join("、");
    if listed.chars().count() <= GUARD_LIST_BUDGET_CHARS {
        s.push_str("本次可用的**全部**工具就是这 ");
        s.push_str(&tools.len().to_string());
        s.push_str(" 个:");
        s.push_str(&listed);
        s.push_str("。清单之外**不存在**任何其他工具。");
    } else {
        s.push_str("本次声明的工具就是你的全部工具,清单之外**不存在**任何其他工具。");
    }

    s.push_str(&render_redirects(tools, ""));

    if !policy.trim().is_empty() {
        s.push('\n');
        s.push_str(policy.trim());
    }
    s
}

/// 工具闭集的字符预算(超了就不逐个列名)。
///
/// 1200 个字符 ≈ 300~400 token,大约能装 60 个 `gwtools-Xxx`。到这个量级时
/// 工具声明本身(每个都带 description + JSON Schema)已经是几十 KB,再重复一遍
/// 名单的边际收益压不住成本;而这个量级也基本只出现在挂了大 MCP server 的场景。
pub(super) const GUARD_LIST_BUDGET_CHARS: usize = 1200;

/// 「模型想要的能力」→「调用方声明的同义工具裸名」。
///
/// 表里的能力就是生产日志里模型真的去抓内建工具的那几项。各家客户端命名互不相同
/// (Claude Code `Bash`/`Read`/`Grep`、opencode `bash`/`read`/`grep`、
/// Cursor 风格 `run_terminal_cmd`/`read_file`),所以要按别名表认。
///
/// ## 匹配规则刻意保守(gpt-5.6-sol 评审:宁可不出替代行,也不要指错工具)
///
/// 三档,取最紧的那个;同档取先声明的:
/// 0. 裸名(小写)与别名**全等**;
/// 1. 去掉所有非字母数字后与别名全等(`Read_File` → `readfile`);
/// 2. 以别名**开头**,且别名长度 ≥ 4(`read_file` 命中 `read`)。
///
/// **没有**纯子串档。子串会让 `Thread` 命中 `read`、`fetch_user` 命中网页工具 ——
/// 那是把「读文件请用 gwtools-Thread」这种胡话写进系统提示。代价是
/// `NotebookEdit`、`TodoWrite` 这类中缀命名认不出来,于是该能力干脆不出替代行,
/// 这是刻意选的那一侧。
///
/// 全等档同时解决另一个坑:`TodoWrite` 与 `Write` 并存时,「写/改文件」必须指向
/// `Write`(全等,0 档)而不是先声明的 `TodoWrite`。
pub(super) fn capability_redirects(tools: &[run::ToolDef]) -> Vec<(&'static str, String)> {
    // (能力说法, 已知客户端的别名表)
    const CAPS: &[(&str, &[&str])] = &[
        (
            "跑命令/终端",
            &[
                "bash",
                "sh",
                "zsh",
                "shell",
                "terminal",
                "run_terminal_cmd",
                "runterminalcmd",
                "run_command",
                "runcommand",
                "execute_command",
                "executecommand",
                "exec",
            ],
        ),
        (
            "读文件",
            &[
                "read",
                "read_file",
                "readfile",
                "view_file",
                "viewfile",
                "open_file",
                "openfile",
                "cat",
            ],
        ),
        (
            "写/改文件",
            &[
                "write",
                "write_file",
                "writefile",
                "edit",
                "edit_file",
                "editfile",
                "multiedit",
                "str_replace_editor",
                "strreplaceeditor",
                "apply_patch",
                "applypatch",
                "patch",
            ],
        ),
        (
            "搜代码/找文件",
            &[
                "grep",
                "glob",
                "rg",
                "ripgrep",
                "search",
                "codebase_search",
                "codebasesearch",
                "grep_search",
                "grepsearch",
                "file_search",
                "filesearch",
                "find",
            ],
        ),
        (
            "查网页",
            &[
                "websearch",
                "web_search",
                "webfetch",
                "web_fetch",
                "fetch",
                "browse",
                "browser",
            ],
        ),
    ];
    let mut out = Vec::new();
    for (cap, aliases) in CAPS {
        if let Some(name) = best_alias_match(tools, aliases) {
            out.push((*cap, name));
        }
    }
    // 联网回退(2026-09-04 pi 卡死案):声明里没有网页工具、但有终端工具时,
    // 必须给模型一条**合法出路** —— 用终端跑 curl/wget。没有这句,grok 会去调
    // 它记忆里 Cursor 的 WebSearch/WebFetch(未注册 → 上游只剩心跳,整轮挂死;
    // 本地 e2e 两次实证)。没有终端工具时不出行:指向不存在的出路比不出更糟。
    // 文案带「若允许联网且装有 curl/wget」限定(codex 评审 P2):按名字猜出来的
    // 终端工具未必能出网,不能把受限沙箱宣传成联网通道。
    if !out.iter().any(|(cap, _)| *cap == "查网页") {
        if let Some((_, term)) = out.iter().find(|(cap, _)| *cap == "跑命令/终端") {
            out.push((
                "查网页/联网(若该终端允许联网且装有 curl/wget,用它抓取)",
                term.clone(),
            ));
        }
    }
    out
}

/// 认得出调用方客户端时,告诉模型「你在哪个 agent 里、工具由谁执行」。
///
/// 2026-11-09 用户实测:OpenCode 里写整页 HTML 一直卡,用户明确提示「调用 OpenCode 的
/// write」后一次成功。模型身处 Cursor 服务端的 agent 提示词之下,默认以为自己在 Cursor
/// 里、偏爱 Cursor 自带的写文件/局部编辑(我们执行不了),光有工具清单不够,要点明环境。
/// 认不出(Fallback)时不写:说错环境比不说更糟。
pub(super) fn client_env_note(body: &Value) -> String {
    use gw_core::clients::{recognize, ClientKind, ClientSignals};
    let name = match recognize(&ClientSignals::from_body(body)) {
        ClientKind::ClaudeCode => "Claude Code",
        ClientKind::OpenCode => "OpenCode",
        ClientKind::GrokBuild => "Grok Build",
        ClientKind::Copilot => "VS Code Copilot",
        ClientKind::DeepSeekHarness => "DeepSeek Harness",
        ClientKind::DeveloperRole => "pi",
        _ => return String::new(),
    };
    format!(
        "\n\n[客户端]你运行在用户本机的 {name} 里:下面清单里的工具都由 {name} 在用户的机器上执行,\
         文件路径以用户机器为准。读写文件、跑命令一律调用 {name} 的这些工具。"
    )
}

/// 按 [`capability_redirects`] 的三档保守规则(全等 > 去符号全等 > 前缀且别名 ≥4 字符;
/// 同档取先声明的)在 `tools` 里找最匹配 `aliases` 的工具裸名。
pub(super) fn best_alias_match(tools: &[run::ToolDef], aliases: &[&str]) -> Option<String> {
    tools
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let lower = t.name.to_ascii_lowercase();
            let squashed: String = lower.chars().filter(char::is_ascii_alphanumeric).collect();
            aliases
                .iter()
                .filter_map(|a| {
                    if lower == *a {
                        Some(0u8)
                    } else if squashed == *a {
                        Some(1)
                    } else if a.len() >= 4 && lower.starts_with(a) {
                        Some(2)
                    } else {
                        None
                    }
                })
                .min()
                .map(|tightness| (tightness, i, &t.name))
        })
        .min()
        .map(|(_, _, name)| name.clone())
}

/// 整写类工具的别名(新建 / 整体覆盖一个文件)。
pub(super) const WHOLE_WRITE_ALIASES: &[&str] = &["write", "write_file", "writefile", "create_file", "createfile"];
/// 局部编辑类工具的别名(按原文片段替换 / 打补丁)。
pub(super) const PARTIAL_EDIT_ALIASES: &[&str] = &[
    "edit",
    "edit_file",
    "editfile",
    "multiedit",
    "str_replace_editor",
    "strreplaceeditor",
    "apply_patch",
    "applypatch",
    "patch",
];

/// 调用方**同时**声明了整写工具和局部编辑工具时,返回 `(整写, 局部编辑)`。
///
/// 2026-11-09 生产:替代表把「写/改文件」只指向一个工具(OpenCode 里是先匹配到的
/// `write`),模型改几行也整文件重写 —— 整页 HTML 当工具参数生成要几分钟,期间上游
/// 只发 token 计数,客户端看起来就是卡死。拆成两行后,局部修改有了明确去处。
pub(super) fn file_tool_split(tools: &[run::ToolDef]) -> Option<(String, String)> {
    let w = best_alias_match(tools, WHOLE_WRITE_ALIASES)?;
    let e = best_alias_match(tools, PARTIAL_EDIT_ALIASES)?;
    (w != e).then_some((w, e))
}

/// 护栏里的「能力 → 工具」替代表那一行(两种形态共用;`prefix` 为调用名前缀,
/// 如 `gwtools-`,裸名注册传空串)。没有可指的工具时返回空串。
pub(super) fn render_redirects(tools: &[run::ToolDef], prefix: &str) -> String {
    let redirects = capability_redirects(tools);
    if redirects.is_empty() {
        return String::new();
    }
    let split = file_tool_split(tools);
    let mut items: Vec<String> = Vec::new();
    for (cap, tool) in &redirects {
        match (&split, *cap) {
            (Some((w, e)), "写/改文件") => {
                items.push(format!("新建或整体重写文件调用 `{prefix}{w}`"));
                items.push(format!(
                    "局部修改已有文件调用 `{prefix}{e}`(只给要替换的原文和新内容,不要为改几行重写整个文件)"
                ));
            }
            _ => items.push(format!("{cap}调用 `{prefix}{tool}`")),
        }
    }
    format!("\n需要某项能力时用清单里的对应工具:{}。", items.join(";"))
}

/// 内建工具调用帧 → 它想要的**能力说法**(与 [`capability_redirects`] 的 key 同一套词)。
///
/// 身份是字段号(见 [`run::builtin_tool_ident`]),只有抓包实证过的三个认得出:
/// `.1` 终端命令、`.4` 读文件、`.12` 写文件(2026-09-06 实物帧,见
/// [`run::BuiltinCall::WriteFile`])。其余返回 `None` —— 那意味着「知道模型调了内建工具、
/// 但不知道调的是哪个」,此时纠偏只能给通用话术,不能指名工具。
///
/// ⚠️ 不要凭字段号相邻就往上加映射。猜错的后果是下一轮纠偏对着模型说
/// 「你上次调了内建终端」而它其实调的是代码检索 —— 一句自信的错话比一句模糊的
/// 实话更容易把模型带偏。枚举要靠 `CURSOR_DUMP_TOOL_FRAMES` 抓实物定死。
pub(super) fn builtin_capability(payload: &[u8]) -> Option<&'static str> {
    match run::builtin_tool_ident(payload)? {
        1 => Some("跑命令/终端"),
        // `.4` 是 glob、`.8` 才是读文件(cursor-agent agent.v1 定义,2026-11-07 更正)。
        4 => Some("搜代码/找文件"),
        8 => Some("读文件"),
        // `.12` 写文件:2026-09-06 实物帧实证(见 run::BuiltinCall::WriteFile)。
        12 => Some("写/改文件"),
        // `.37` 网页抓取:2026-09-06 e2e 实物帧实证(见 run::BuiltinCall::WebFetch)。
        37 => Some("查网页"),
        _ => None,
    }
}

/// 上一轮被内建工具截断时,注入本轮用户消息的纠偏话术。
///
/// ## 为什么需要它:模型**收不到**失败信号
///
/// 模型调内建工具 → 我们当场关流。它不知道自己失败了,本轮就结束了。所以
/// 「失败了就换个工具再试」这句话写进系统提示是空转 —— 没有任何触发时机。
/// 生产实测同一个会话会**反复**撞同一面墙(12h 内 302 次收口)。
///
/// 协议内的正解是在同一条 BiDi 流里回一个「工具不可用」的失败结果,让模型在**本轮内**
/// 自己纠偏。但那要求先定死 `1.2.2.<N>` 的身份枚举与请求侧回执消息形状 ——
/// 猜字段号的后果是回执被上游忽略、退化成 90s 心跳死等,比现在的瞬间收口更差。
/// 所以在抓包定死之前,先走这条零协议风险的路:**下一轮**告诉它上次失败了。
///
/// 救不了当前那一轮,但能止住重复 —— 这是用户明确要的那半件事
/// (「还是需要告诉模型你工具调用失败了」)。
///
/// 杠杆与 [`fold_history`] 的 `[继续]` 同源(已在工具回路发散那次实证过)。
#[cfg(test)]
pub(super) fn truncation_notice(cap: Option<&str>, tools: &[run::ToolDef]) -> String {
    truncation_notice_for(cap, tools, false)
}

/// `bare`:工具按裸名注册(CLI 形态,与 [`mcp_tool_guard_with`] 同口径)。
/// 此时话术里**不能**出现 `gwtools-` —— 那是 IDE 形态 frame0 的注册口径,
/// 写进来模型就去找一个不存在的命名空间。
pub(super) fn truncation_notice_for(cap: Option<&str>, tools: &[run::ToolDef], bare: bool) -> String {
    let call_name = |tool: &str| {
        if bare {
            tool.to_string()
        } else {
            format!("{}-{tool}", run::TOOL_NS)
        }
    };
    let mut s = String::from(
        "\n\n[上一轮中断]你上一轮调用了本环境**不提供**的 Cursor 内建工具,\
         那次调用没有任何结果,你的回答也在那里被截断了(用户只看到半句话)。",
    );
    // 认得出能力、且调用方确实声明了同义工具 → 指名换哪个。这是最有用的一句。
    let redirect = cap.and_then(|c| {
        capability_redirects(tools)
            .into_iter()
            .find(|(k, _)| *k == c)
            .map(|(_, tool)| tool)
    });
    match redirect {
        Some(tool) => {
            s.push_str("请改用 `");
            s.push_str(&call_name(&tool));
            s.push_str("` 重做刚才那一步,然后继续完成用户的请求。");
        }
        None if bare => {
            s.push_str(
                "请只用本次声明的工具重做刚才那一步(调用名按声明原样书写,不加任何前缀);\
                 声明的工具里没有能干这件事的,就直接告诉用户你需要什么。",
            );
        }
        None => {
            s.push_str(
                "请只用本次声明的工具重做刚才那一步(调用名带 `gwtools-` 前缀);\
                 声明的工具里没有能干这件事的,就直接告诉用户你需要什么。",
            );
        }
    }
    s
}
