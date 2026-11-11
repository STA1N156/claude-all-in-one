//! Cursor 内建工具调用 → 调用方声明工具的翻译(终端/读写文件/搜索/列目录/抓取/子代理)。
//!
//! 2026-11-10 由 chat.rs 拆出(只搬不改行为)。

use super::*;

/// 内建调用 → 调用方声明工具的兼容转换表(2026-08-13)。
///
/// 收口的替代:模型抓了无前缀的内建工具(`Bash` 而不是 `gwtools-Bash`)时,
/// 把这次调用翻译成调用方**声明过的**同义工具的标准 `tool_use` 下发 ——
/// 客户端照常执行并在下一轮回传 tool_result,「半截回答 + 静默截断」变成
/// 无感成功。生产 12h 内建收口 302 次,终端/读文件是绝对大头。
///
/// 表在请求开头按本次声明的 `tools` 生成一次:
/// - 工具名复用 [`capability_redirects`] 的保守匹配(全等 > 去符号全等 > 前缀);
/// - **参数键名从该工具的 `input_schema.properties` 里认**(Claude Code 是
///   `command`/`file_path`,opencode 是 `command`/`filePath`),候选键都不在
///   schema 里就放弃翻译 —— 猜键名的后果是客户端执行一个参数张冠李戴的调用,
///   比收口糟得多。翻译失败一律落回收口 + 纠偏,风险单向。
#[derive(Debug, Clone, Default)]
pub(super) struct BuiltinXlate {
    /// 终端命令 → `(声明工具裸名, 参数键名)`,如 `("Bash", "command")`。
    pub(super) terminal: Option<(String, String)>,
    /// 终端工具的工作目录参数键(opencode `workdir`);`None` = 该工具没有这个参数,
    /// 翻译时改在命令前补切目录(POSIX `cd '<目录>' &&`,PowerShell `Set-Location …;`)。
    pub(super) terminal_cwd_key: Option<String>,
    /// 调用方终端是 PowerShell(工具名或说明里写着 powershell,如新版 OpenCode 的
    /// 「Commands run on Windows using powershell」)。我方替模型拼的命令(grep 退化、
    /// 列目录、切目录)要按它的语法拼:PowerShell 没有 grep、`ls -la`,5.x 也不认 `&&`。
    pub(super) terminal_powershell: bool,
    /// 内建搜文件内容 → 调用方自己的搜索工具(见 [`GrepTarget`]);`None` 才退化成终端命令。
    pub(super) grep: Option<GrepTarget>,
    /// 读文件 → 同上,如 `("Read", "file_path")`。
    pub(super) read_file: Option<(String, String)>,
    /// 读文件工具的行范围参数键 `(offset 键, limit 键)`,从同一工具的 schema 认
    /// (2026-11-09 线上 5 种读工具 schema:Claude Code / OpenCode / 另两个客户端都叫
    /// `offset`/`limit`,说明全是从 1 起的行号,与 Cursor 同义);schema 里没有就是 `None`,
    /// 不传范围(客户端读整文件,旧行为)。offset 说明写明按字节的不认。
    pub(super) read_range_keys: (Option<String>, Option<String>),
    /// 调用方 offset 从 0 起(说明里写 0-based / 0-indexed / zero-based)→ 传之前减 1。
    pub(super) read_offset_zero_based: bool,
    /// write 别名:调用方只声明了编辑类工具时的补位(见 [`WriteAlias`])。
    pub(super) write_alias: Option<WriteAlias>,
    /// 内建写文件(exec `write_args`,带服务端算好的完整内容)的落点。
    pub(super) write_file: Option<WriteFileTarget>,
    /// 调用方的局部编辑工具裸名(OpenCode `edit`、Claude Code `Edit`)。
    /// 模型调 Cursor 内建 StrReplace 时,拒绝理由里点名让它改用这个(见 [`edit_reject_reason`])。
    pub(super) edit_tool: Option<String>,
    /// 内建网页抓取(`.37`)→ `(声明工具裸名, URL 键名)`,如 `("webfetch", "url")`。
    pub(super) web_fetch: Option<(String, String)>,
    /// 内建找文件(`.4` glob)→ 调用方自己的 glob 工具。
    pub(super) glob: Option<GlobTarget>,
    /// 内建子代理(exec `28`)→ 调用方自己的子代理工具。
    pub(super) task: Option<TaskTarget>,
}

/// 内建子代理的翻译落点(2026-11-07 生产:fable 派「只读审查者」子代理,我们认不出就截断)。
///
/// 只认**名字就是子代理类**的工具(OpenCode `task`、Claude Code `Task`),键名从 schema 认;
/// 子代理类型取 schema 里列出的 general 类(Claude Code `general-purpose`、OpenCode `general`),
/// 认不出就不传 —— 客户端若要求必填会回错,模型看到错误会改用别的办法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TaskTarget {
    pub(super) name: String,
    pub(super) prompt_key: String,
    pub(super) desc_key: Option<String>,
    pub(super) type_key: Option<String>,
    pub(super) type_value: Option<String>,
}

impl TaskTarget {
    pub(super) fn from_tools(tools: &[run::ToolDef]) -> Option<Self> {
        const NAMES: &[&str] = &["task", "agent", "dispatch_agent", "subagent"];
        let def = tools
            .iter()
            .find(|t| NAMES.contains(&t.name.to_ascii_lowercase().as_str()))?;
        let schema: Value = serde_json::from_str(&def.schema).ok()?;
        let props = schema.get("properties")?.as_object()?;
        let pick = |keys: &[&str]| {
            keys.iter()
                .find(|k| props.contains_key(**k))
                .map(|k| (*k).to_string())
        };
        let type_key = pick(&["subagent_type", "agent_type", "agent"]);
        let type_value = type_key.as_ref().and_then(|k| {
            let prop = &props[k];
            // 优先 enum 里的 general 类;没有 enum 就在 schema 描述里找写明的名字。
            if let Some(list) = prop.get("enum").and_then(Value::as_array) {
                let names: Vec<&str> = list.iter().filter_map(Value::as_str).collect();
                return names
                    .iter()
                    .find(|n| n.to_ascii_lowercase().starts_with("general"))
                    .or(names.first())
                    .map(|n| (*n).to_string());
            }
            let text = format!("{} {}", def.description, prop);
            ["general-purpose", "general"]
                .iter()
                .find(|n| text.contains(*n))
                .map(|n| (*n).to_string())
        });
        Some(TaskTarget {
            name: def.name.clone(),
            prompt_key: pick(&["prompt", "task", "instructions"])?,
            desc_key: pick(&["description", "title"]),
            type_key,
            type_value,
        })
    }
}

/// 内建 glob 的翻译落点:`(工具名, 模式键, 目录键)`。目录键可缺 ——
/// 缺了就只传模式,由客户端在自己的工作目录里找。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GlobTarget {
    pub(super) name: String,
    pub(super) pattern_key: String,
    pub(super) dir_key: Option<String>,
}

impl GlobTarget {
    /// 只认**名字就是 glob 类**的工具。「搜代码/找文件」那一档的 grep/search
    /// 参数语义不同(搜内容不是找文件名),塞过去等于参数张冠李戴。
    pub(super) fn from_tools(tools: &[run::ToolDef]) -> Option<Self> {
        const NAMES: &[&str] = &["glob", "glob_file_search", "globfilesearch", "find_files"];
        let def = tools
            .iter()
            .find(|t| NAMES.contains(&t.name.to_ascii_lowercase().as_str()))?;
        let schema: Value = serde_json::from_str(&def.schema).ok()?;
        let props = schema.get("properties")?.as_object()?;
        let pick = |keys: &[&str]| {
            keys.iter()
                .find(|k| props.contains_key(**k))
                .map(|k| (*k).to_string())
        };
        Some(GlobTarget {
            name: def.name.clone(),
            pattern_key: pick(&["pattern", "glob_pattern", "glob"])?,
            dir_key: pick(&["path", "target_directory", "directory", "dir", "cwd"]),
        })
    }
}

/// 内建搜文件内容的翻译落点:调用方自己的 grep 类工具。
///
/// 2026-11-09 线上 5 种 schema:`pattern` 都必填、`path` 都可选;文件过滤 Claude Code 叫
/// `glob`、OpenCode 叫 `include`。只认名字就是 grep 类的工具 —— glob/find 搜的是文件名,
/// 塞内容模式过去是参数张冠李戴。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GrepTarget {
    pub(super) name: String,
    pub(super) pattern_key: String,
    pub(super) path_key: Option<String>,
    pub(super) glob_key: Option<String>,
}

impl GrepTarget {
    pub(super) fn from_tools(tools: &[run::ToolDef]) -> Option<Self> {
        const NAMES: &[&str] = &["grep", "grep_search", "grepsearch", "rg", "ripgrep"];
        let def = tools
            .iter()
            .find(|t| NAMES.contains(&t.name.to_ascii_lowercase().as_str()))?;
        let schema: Value = serde_json::from_str(&def.schema).ok()?;
        let props = schema.get("properties")?.as_object()?;
        let pick = |keys: &[&str]| {
            keys.iter()
                .find(|k| props.contains_key(**k))
                .map(|k| (*k).to_string())
        };
        Some(GrepTarget {
            name: def.name.clone(),
            pattern_key: pick(&["pattern", "regex", "query"])?,
            path_key: pick(&["path", "directory", "dir", "cwd"]),
            glob_key: pick(&["glob", "include", "file_pattern", "filePattern"]),
        })
    }
}

/// POSIX 单引号转义(`'` → `'\''`)。
pub(super) fn posix_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// PowerShell 单引号字符串(内部引号写成 `''`;单引号内不展开变量与转义)。
///
/// PowerShell 把弯单引号 U+2018/2019/201A/201B 也当单引号,只转 ASCII `'` 的话,模式里
/// 一个 `’` 就能跳出字符串注入命令(审查指出)。五种一律换成 `''`。
pub(super) fn ps_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push_str("''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// 终端是 PowerShell 吗:工具名就是 powershell/pwsh,或说明里明确说「用/在 PowerShell 里跑」
/// (新版 OpenCode:「Commands run on Windows using powershell」)。
///
/// 不按子串 `powershell` 判:「runs in Git Bash, not PowerShell」「don't use PowerShell
/// syntax」这类 bash 客户端会被误判,收到 PowerShell 命令照样失败(审查指出)。
pub(super) fn terminal_is_powershell(tools: &[run::ToolDef], name: &str) -> bool {
    const SAYS: &[&str] = &["using powershell", "using pwsh", "in powershell", "in pwsh", "on powershell"];
    tools.iter().find(|t| t.name == name).is_some_and(|t| {
        let n = t.name.to_ascii_lowercase();
        let d = t.description.to_ascii_lowercase();
        n == "powershell" || n == "pwsh" || SAYS.iter().any(|w| d.contains(w))
    })
}

/// 没有搜索工具时把内建 grep 退化成终端命令(按终端类型)。
pub(super) fn grep_command(pattern: &str, path: Option<&str>, glob: Option<&str>, powershell: bool) -> String {
    let path = path.unwrap_or(".");
    if powershell {
        let include = glob.map(|g| format!(" -Include {}", ps_quote(g))).unwrap_or_default();
        format!(
            "Get-ChildItem -LiteralPath {} -Recurse -File{include} -ErrorAction SilentlyContinue | Select-String -Pattern {}",
            ps_quote(path),
            ps_quote(pattern)
        )
    } else {
        let include = glob.map(|g| format!(" --include={}", posix_quote(g))).unwrap_or_default();
        format!("grep -rn{include} -- {} {}", posix_quote(pattern), posix_quote(path))
    }
}

/// 内建写文件调用的翻译落点。
///
/// 与终端/读文件的单参数不同,写文件是**双参数**(路径 + 完整内容),所以
/// 单列一档:
/// - 调用方有写文件工具(write/Write/write_file…)→ 直接映射,键名从其
///   schema 认(opencode 是 `filePath`/`content`,Claude Code 是 `file_path`/`content`);
/// - 调用方只有编辑类工具 → 借 [`WriteAlias`] 的旧串置空语义(= 整文件覆写),
///   与广告侧的 `write` 补位同一逻辑。
#[derive(Debug, Clone)]
pub(super) enum WriteFileTarget {
    /// `(工具裸名, 路径键, 内容键)`。
    Direct {
        name: String,
        path_key: String,
        content_key: String,
    },
    Alias(WriteAlias),
}

/// write 别名(2026-09-04 生产+本地多次实证)。
///
/// grok 系模型**创建文件**时执念调用 `write` —— 调用方(典型:opencode,
/// 写文件归 `edit` 管)清单里没有这个名字,模型调一个未注册名,Cursor 上游
/// **一个帧都不回**,只剩心跳到停滞闸(「出字后卡死」的根因)。护栏文案说了
/// 闭集也不听 —— 那就让清单里**真的有** `write`:广告侧补位(见
/// [`write_alias_tooldef`]),回调时翻译成编辑工具(opencode 的 `edit`
/// 实测支持空 oldString 创建新文件)。
#[derive(Debug, Clone)]
pub(super) struct WriteAlias {
    /// 目标编辑工具裸名(如 `"edit"`)。
    pub(super) target: String,
    /// 目标工具 schema 里的路径键 / 旧串键 / 新串键(键名从 schema 认,不猜)。
    pub(super) path_key: String,
    pub(super) old_key: String,
    pub(super) new_key: String,
}

/// 别名广告的注册名。取最朴素的 `write`:模型执念的就是它。
pub(crate) const WRITE_ALIAS_NAME: &str = "write";

/// MCP 工具的「全名」前缀(2026-09-04 实证)。
///
/// Cursor 服务端在转录里把我们的 MCP 工具称作 `mcp_claude-local_<name>`
/// (outerToolName:`mcp_<provider_identifier>_<tool>`,provider 是我方广告的
/// `claude-local`)。模型从历史/思考里学到这个全名就会直接调它 —— 未注册
/// 则上游静默挂起(「出字后卡死」的又一形态,本地 e2e 抓思考实证:
/// 「实际可用的工具是 mcp_claude-local_write」)。mcpalias 探针证实
/// 全名本身可注册、可路由,故 CLI 形态对每个工具双注册(见帧0 构造处),
/// 回调在工具臂按此前缀归一回裸名。
pub(crate) const MCP_FULL_PREFIX: &str = "mcp_claude-local_";

impl WriteAlias {
    /// 把一次 `write` 回调翻译成目标编辑工具的调用:
    /// `content` → 新串键,旧串键置空(= 创建/整文件写入),路径同键。
    pub(super) fn translate(&self, tc: run::ToolCall) -> run::ToolCall {
        let pick = |keys: &[&str]| {
            tc.args
                .iter()
                .find(|(k, _)| keys.contains(&k.as_str()))
                .map(|(_, v)| v.clone())
        };
        let path = pick(&[&self.path_key, "filePath", "file_path", "path"]).unwrap_or(Value::Null);
        let content =
            pick(&["content", &self.new_key, "newString", "new_string"]).unwrap_or(Value::Null);
        run::ToolCall {
            id: tc.id,
            name: self.target.clone(),
            args: vec![
                (self.path_key.clone(), path),
                (self.old_key.clone(), Value::String(String::new())),
                (self.new_key.clone(), content),
            ],
        }
    }
}

/// 别名广告的 [`run::ToolDef`](CLI 形态进 mcp_tools,IDE 形态自动带 `gwtools-` 前缀)。
pub(super) fn write_alias_tooldef(a: &WriteAlias) -> run::ToolDef {
    let mut props = serde_json::Map::new();
    props.insert(
        a.path_key.clone(),
        json!({"type":"string","description":"文件的绝对路径"}),
    );
    props.insert(
        "content".to_string(),
        json!({"type":"string","description":"要写入的完整内容"}),
    );
    run::ToolDef {
        name: WRITE_ALIAS_NAME.to_string(),
        description: "创建新文件并写入全部内容(只用于文件尚不存在;修改已有文件请用编辑工具做局部替换)"
            .to_string(),
        schema: json!({
            "type":"object",
            "properties": Value::Object(props),
            "required":[a.path_key, "content"]
        })
        .to_string(),
    }
}

impl BuiltinXlate {
    pub(super) fn from_tools(tools: &[run::ToolDef]) -> Self {
        let redirects = capability_redirects(tools);
        let find = |cap: &str, keys: &[&str]| -> Option<(String, String)> {
            let tool = redirects
                .iter()
                .find(|(c, _)| *c == cap)
                .map(|(_, t)| t.clone())?;
            let def = tools.iter().find(|t| t.name == tool)?;
            let schema: Value = serde_json::from_str(&def.schema).ok()?;
            let props = schema.get("properties")?.as_object()?;
            let key = keys.iter().find(|k| props.contains_key(**k))?;
            Some((tool, (*key).to_string()))
        };
        // write 别名:调用方**没有**写文件工具(write/write_file/writefile)、
        // 但有编辑类工具时补位。键名从编辑工具的 schema 认,认不齐就不补
        // (猜键名 = 参数张冠李戴,比不补糟)。
        const WRITE_FAMILY: &[&str] = &["write", "write_file", "writefile"];
        let write_tool = tools
            .iter()
            .find(|t| WRITE_FAMILY.contains(&t.name.to_ascii_lowercase().as_str()));
        let write_alias = if write_tool.is_some() {
            None
        } else {
            redirects
                .iter()
                .find(|(c, _)| *c == "写/改文件")
                .and_then(|(_, tool)| {
                    let def = tools.iter().find(|t| &t.name == tool)?;
                    let schema: Value = serde_json::from_str(&def.schema).ok()?;
                    let props = schema.get("properties")?.as_object()?;
                    let pick = |keys: &[&str]| {
                        keys.iter()
                            .find(|k| props.contains_key(**k))
                            .map(|k| (*k).to_string())
                    };
                    Some(WriteAlias {
                        target: tool.clone(),
                        path_key: pick(&[
                            "filePath",
                            "file_path",
                            "path",
                            "target_file",
                            "absolute_path",
                        ])?,
                        old_key: pick(&["oldString", "old_string", "old_str"])?,
                        new_key: pick(&["newString", "new_string", "new_str"])?,
                    })
                })
        };
        // 内建写文件(exec `write_args`)的落点:优先直译到调用方自己的写文件工具
        // (键名从它的 schema 认,认不齐就放弃直译);没有写工具时借别名。
        let write_file = match write_tool {
            Some(def) => {
                let schema: Option<Value> = serde_json::from_str(&def.schema).ok();
                let props = schema
                    .as_ref()
                    .and_then(|s| s.get("properties"))
                    .and_then(Value::as_object);
                let pick = |keys: &[&str]| {
                    props.and_then(|p| {
                        keys.iter()
                            .find(|k| p.contains_key(**k))
                            .map(|k| (*k).to_string())
                    })
                };
                match (
                    pick(&["filePath", "file_path", "path", "target_file", "absolute_path"]),
                    pick(&["content", "newString", "new_string", "text", "contents"]),
                ) {
                    (Some(path_key), Some(content_key)) => Some(WriteFileTarget::Direct {
                        name: def.name.clone(),
                        path_key,
                        content_key,
                    }),
                    // schema 认不齐键名:不猜,内建写维持收口(单向失败)。
                    _ => None,
                }
            }
            None => write_alias.clone().map(WriteFileTarget::Alias),
        };
        let terminal = find("跑命令/终端", &["command", "cmd", "script"]);
        let terminal_cwd_key = terminal.as_ref().and_then(|(name, _)| {
            let def = tools.iter().find(|t| &t.name == name)?;
            let schema: Value = serde_json::from_str(&def.schema).ok()?;
            let props = schema.get("properties")?.as_object()?;
            ["workdir", "cwd", "working_directory", "workingDirectory"]
                .iter()
                .find(|k| props.contains_key(**k))
                .map(|k| (*k).to_string())
        });
        let read_props = read_file_props(tools, &redirects);
        let prop_desc = |k: &str| -> Option<String> {
            let p = read_props.as_ref()?.get(k)?;
            Some(p.get("description").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase())
        };
        let offset_desc = prop_desc("offset");
        let read_range_keys = (
            offset_desc
                .as_ref()
                .filter(|d| !d.contains("byte"))
                .map(|_| "offset".to_string()),
            prop_desc("limit").map(|_| "limit".to_string()),
        );
        let read_offset_zero_based = offset_desc.as_ref().is_some_and(|d| {
            d.contains("0-based") || d.contains("0-indexed") || d.contains("zero-based")
        });
        let terminal_powershell = terminal
            .as_ref()
            .is_some_and(|(name, _)| terminal_is_powershell(tools, name));
        BuiltinXlate {
            terminal,
            terminal_cwd_key,
            terminal_powershell,
            grep: GrepTarget::from_tools(tools),
            read_range_keys,
            read_offset_zero_based,
            read_file: find(
                "读文件",
                &[
                    "file_path",
                    "filePath",
                    "path",
                    "target_file",
                    "absolute_path",
                ],
            ),
            write_alias,
            write_file,
            web_fetch: find("查网页", &["url", "uri", "link", "href"]),
            glob: GlobTarget::from_tools(tools),
            task: TaskTarget::from_tools(tools),
            edit_tool: edit_tool_name(tools),
        }
    }
}

/// 调用方「读文件」工具 schema 的 `properties`(认不出工具或 schema 解不开 = `None`)。
pub(super) fn read_file_props(
    tools: &[run::ToolDef],
    redirects: &[(&'static str, String)],
) -> Option<serde_json::Map<String, Value>> {
    let tool = redirects.iter().find(|(c, _)| *c == "读文件").map(|(_, t)| t)?;
    let def = tools.iter().find(|t| &t.name == tool)?;
    let schema: Value = serde_json::from_str(&def.schema).ok()?;
    schema.get("properties")?.as_object().cloned()
}

/// 调用方的局部编辑工具(按名字全等认,大小写不敏感;认不出 = `None`)。
pub(super) fn edit_tool_name(tools: &[run::ToolDef]) -> Option<String> {
    const EDIT_NAMES: &[&str] = &["edit", "str_replace", "strreplace", "edit_file", "replace"];
    EDIT_NAMES.iter().find_map(|want| {
        tools
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(want))
            .map(|t| t.name.clone())
    })
}

/// 模型调了 Cursor 内建的局部替换(StrReplace)时的拒绝理由。
///
/// 内建 StrReplace 的流程是:界面帧 `.12`(只带替换片段)→ exec 读原文件 → 服务端替换后
/// exec 写全文。我们拿不到客户磁盘上的原文件,读只能拒绝;理由里点名调用方自己的编辑
/// 工具,模型在**同一轮**里改用它(工具名按注册口径:CLI 形态裸名,IDE 形态带命名空间)。
pub(super) fn edit_reject_reason(x: &BuiltinXlate, bare: bool) -> String {
    match &x.edit_tool {
        Some(t) => {
            let name = if bare {
                t.clone()
            } else {
                format!("{}-{t}", run::TOOL_NS)
            };
            format!(
                "The built-in StrReplace/edit tool is not available in this environment and \
                 the file was NOT modified. To change part of an existing file, call `{name}` \
                 with the exact old text and the new text. Never rewrite a whole existing file \
                 with only the changed lines."
            )
        }
        None => "The built-in StrReplace/edit tool is not available in this environment and \
                 the file was NOT modified. Use the provided tools to edit the file."
            .to_string(),
    }
}

/// 把一次已解出参数的内建调用翻译成调用方工具的 [`run::ToolCall`]。
/// 转换表里没有对应工具 → `None`(调用方落回收口 + 纠偏)。
pub(super) fn translate_builtin(bc: run::BuiltinCall, x: &BuiltinXlate) -> Option<run::ToolCall> {
    // 空 id 兜底与 parse_tool_call 同款:我方从不把 call id 发回上游
    // (每轮都是全新的 Opening 请求),合成是安全的。
    let mkid = |id: String| {
        if id.trim().is_empty() {
            format!("call_{}", uuid::Uuid::new_v4().simple())
        } else {
            id
        }
    };
    match bc {
        // 内建终端:工作目录能当参数传就传;传不了就在命令前 cd 过去,
        // 否则命令会在客户端的默认目录里跑,与模型的意图不符。
        run::BuiltinCall::Terminal { id, command, cwd } => {
            let (name, key) = x.terminal.clone()?;
            let args = match (cwd, &x.terminal_cwd_key) {
                (Some(d), Some(k)) => vec![
                    (key, Value::String(command)),
                    (k.clone(), Value::String(d)),
                ],
                (Some(d), None) => {
                    let cmd = if x.terminal_powershell {
                        format!("Set-Location -LiteralPath {}; {command}", ps_quote(&d))
                    } else {
                        format!("cd {} && {command}", posix_quote(&d))
                    };
                    vec![(key, Value::String(cmd))]
                }
                (None, _) => vec![(key, Value::String(command))],
            };
            Some(run::ToolCall {
                id: mkid(id),
                name,
                args,
            })
        }
        // 内建搜内容:调用方有自己的搜索工具就翻到它上面(参数键认自 schema);
        // 没有才退化成终端命令,按终端类型拼。
        run::BuiltinCall::Grep { id, pattern, path, glob } => {
            if let Some(g) = &x.grep {
                let mut args = vec![(g.pattern_key.clone(), Value::String(pattern))];
                if let (Some(k), Some(p)) = (&g.path_key, path) {
                    args.push((k.clone(), Value::String(p)));
                }
                if let (Some(k), Some(gl)) = (&g.glob_key, glob) {
                    args.push((k.clone(), Value::String(gl)));
                }
                return Some(run::ToolCall { id: mkid(id), name: g.name.clone(), args });
            }
            let command = grep_command(&pattern, path.as_deref(), glob.as_deref(), x.terminal_powershell);
            translate_builtin(run::BuiltinCall::Terminal { id, command, cwd: None }, x)
        }
        // 内建列目录:按终端类型拼命令。
        run::BuiltinCall::ListDir { id, path } => {
            let path = if path.trim().is_empty() { ".".to_string() } else { path };
            let command = if x.terminal_powershell {
                format!("Get-ChildItem -Force -LiteralPath {}", ps_quote(&path))
            } else {
                format!("ls -la {}", posix_quote(&path))
            };
            translate_builtin(run::BuiltinCall::Terminal { id, command, cwd: None }, x)
        }
        run::BuiltinCall::ReadFile { id, path, offset, limit } => {
            // `/assets/` 是服务端推来的附件(见 AssetStore),客户端磁盘上没有:
            // 翻成客户端 Read 只会得到「文件不存在」。留给随后的 exec 读帧从资产库回图。
            if path.starts_with("/assets/") {
                return None;
            }
            let (name, key) = x.read_file.clone()?;
            let mut args = vec![(key, Value::String(path))];
            // 行范围:offset 从 1 起(与 Claude Code / OpenCode 同义)。负 offset 是「从末尾
            // 倒数」,客户端工具没有这种语义 → 整段范围都不传(读整文件,旧行为)。
            if offset.is_none_or(|o| o > 0) {
                let (ok, lk) = &x.read_range_keys;
                if let (Some(k), Some(o)) = (ok, offset) {
                    let o = if x.read_offset_zero_based { o - 1 } else { o };
                    args.push((k.clone(), json!(o)));
                }
                if let (Some(k), Some(l)) = (lk, limit.filter(|l| *l > 0)) {
                    args.push((k.clone(), json!(l)));
                }
            }
            Some(run::ToolCall {
                id: mkid(id),
                name,
                args,
            })
        }
        // 内建找文件(`.4` glob):模式必传,目录有键才传。
        run::BuiltinCall::Glob { id, dir, pattern } => {
            let g = x.glob.as_ref()?;
            let mut args = vec![(g.pattern_key.clone(), Value::String(pattern))];
            if let (Some(k), Some(d)) = (&g.dir_key, dir) {
                args.push((k.clone(), Value::String(d)));
            }
            Some(run::ToolCall {
                id: mkid(id),
                name: g.name.clone(),
                args,
            })
        }
        // 内建子代理:prompt 必传;描述取 prompt 首行前 40 字;类型认得出才传。
        run::BuiltinCall::Task { id, prompt, subagent_type: _ } => {
            let t = x.task.as_ref()?;
            let mut args = Vec::new();
            if let Some(k) = &t.desc_key {
                let first: String = prompt
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("子任务")
                    .trim()
                    .chars()
                    .take(40)
                    .collect();
                args.push((k.clone(), Value::String(first)));
            }
            args.push((t.prompt_key.clone(), Value::String(prompt)));
            if let (Some(k), Some(v)) = (&t.type_key, &t.type_value) {
                args.push((k.clone(), Value::String(v.clone())));
            }
            Some(run::ToolCall {
                id: mkid(id),
                name: t.name.clone(),
                args,
            })
        }
        // 内建网页抓取(`.37`):单 URL 参数。
        run::BuiltinCall::WebFetch { id, url } => {
            let (name, key) = x.web_fetch.clone()?;
            Some(run::ToolCall {
                id: mkid(id),
                name,
                args: vec![(key, Value::String(url))],
            })
        }
        // 内建写文件(exec `write_args`):双参数(路径 + 服务端算好的完整内容),整文件覆写。
        // ⚠️ 界面帧 `.12` 不走这里:那是编辑帧,局部替换时内容只是片段(见 `run::edit_call_path`)。
        run::BuiltinCall::WriteFile { id, path, content } => match x.write_file.as_ref()? {
            WriteFileTarget::Direct {
                name,
                path_key,
                content_key,
            } => Some(run::ToolCall {
                id: mkid(id),
                name: name.clone(),
                args: vec![
                    (path_key.clone(), Value::String(path)),
                    (content_key.clone(), Value::String(content)),
                ],
            }),
            // 借别名的旧串置空翻译:喂给它约定的 `path`/`content` 键,
            // 由它折成目标编辑工具的 (path_key, old="", new=content)。
            WriteFileTarget::Alias(a) => Some(a.translate(run::ToolCall {
                id: mkid(id),
                name: WRITE_ALIAS_NAME.to_string(),
                args: vec![
                    ("path".to_string(), Value::String(path)),
                    ("content".to_string(), Value::String(content)),
                ],
            })),
        },
    }
}
