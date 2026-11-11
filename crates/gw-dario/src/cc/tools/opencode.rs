//! OpenCode 的工具表。
//!
//! 依据 10-30 线上 595 条 OpenCode 请求的工具定义,两个版本同时在跑:
//! - 新版(546 条):`read/write/edit` 用 `path`,`skill` 用 `id`,有 `shell`(Windows 上是 PowerShell)、`subagent`;
//! - 旧版(49 条):`read/write/edit` 用 `filePath`,`skill` 用 `name`,有 `bash`、`task`。
//!
//! 两版 `read.offset` 都是从 1 开始的行号,与 CC 的 Read 一致,不用换算。
//! 没有 CC 对应的(todowrite、execute、插件工具)不在表里,走 MCP 改名。
//! `task` / `subagent` 也走 MCP:它们能凭 task_id / sessionID 续接子 agent 会话、按 OpenCode 自己的
//! agent 名派活,CC 的 Agent 没有这些能力,硬映射会让模型失去续接(对抗审查 10-30)。

use serde_json::Value;

use super::{get, get_bool, get_str, put, Args, CcPick, NativeRule, Props, ToolTable};

pub(crate) static TABLE: ToolTable = ToolTable { mcp_server: "workspace", rules: &RULES, notes: &NOTES };

/// CC Bash/PowerShell 的说明里写着「工作目录在调用之间保留」,OpenCode 的 shell 每次是新进程。
static NOTES: [&str; 1] = [
    "Shell commands run in a fresh process each time: the working directory does not persist between calls, so put `cd <dir> && <command>` in the same call or use absolute paths.",
];

static RULES: [NativeRule; 11] = [
    NativeRule { client: "read", cc: CcPick::Fixed("Read"), to_cc: read_to_cc, to_client: read_to_client },
    NativeRule { client: "write", cc: CcPick::Fixed("Write"), to_cc: write_to_cc, to_client: write_to_client },
    NativeRule { client: "edit", cc: CcPick::Fixed("Edit"), to_cc: edit_to_cc, to_client: edit_to_client },
    NativeRule { client: "glob", cc: CcPick::Fixed("Glob"), to_cc: glob_to_cc, to_client: glob_to_client },
    NativeRule { client: "grep", cc: CcPick::Fixed("Grep"), to_cc: grep_to_cc, to_client: grep_to_client },
    NativeRule { client: "bash", cc: CcPick::Fixed("Bash"), to_cc: shell_to_cc, to_client: shell_to_client },
    NativeRule { client: "shell", cc: CcPick::BashOrPowerShell, to_cc: shell_to_cc, to_client: shell_to_client },
    NativeRule { client: "webfetch", cc: CcPick::Fixed("WebFetch"), to_cc: webfetch_to_cc, to_client: webfetch_to_client },
    NativeRule { client: "websearch", cc: CcPick::Fixed("WebSearch"), to_cc: websearch, to_client: websearch_back },
    NativeRule { client: "question", cc: CcPick::Fixed("AskUserQuestion"), to_cc: question_to_cc, to_client: question_to_client },
    NativeRule { client: "skill", cc: CcPick::Fixed("Skill"), to_cc: skill_to_cc, to_client: skill_to_client },
];

const PATH_KEYS: [&str; 2] = ["path", "filePath"];
/// OpenCode 的 webfetch 没有「要从页面里找什么」这个参数,CC 的 WebFetch 必填,历史里补一句固定的。
pub(super) const WEBFETCH_PROMPT: &str = "Return the content of this page.";

fn read_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    put(&mut o, "file_path", get(a, &PATH_KEYS));
    put(&mut o, "offset", get(a, &["offset"]));
    put(&mut o, "limit", get(a, &["limit"]));
    o
}

fn read_to_client(a: &Args, p: &Props) -> Args {
    let mut o = Args::new();
    put(&mut o, p.pick(&PATH_KEYS), get(a, &["file_path"]));
    put(&mut o, "offset", get(a, &["offset"]));
    put(&mut o, "limit", get(a, &["limit"]));
    o
}

fn write_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    put(&mut o, "file_path", get(a, &PATH_KEYS));
    put(&mut o, "content", get(a, &["content"]));
    o
}

fn write_to_client(a: &Args, p: &Props) -> Args {
    let mut o = Args::new();
    put(&mut o, p.pick(&PATH_KEYS), get(a, &["file_path"]));
    put(&mut o, "content", get(a, &["content"]));
    o
}

fn edit_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    put(&mut o, "file_path", get(a, &PATH_KEYS));
    put(&mut o, "old_string", get(a, &["oldString"]));
    put(&mut o, "new_string", get(a, &["newString"]));
    put(&mut o, "replace_all", get(a, &["replaceAll"]));
    o
}

fn edit_to_client(a: &Args, p: &Props) -> Args {
    let mut o = Args::new();
    put(&mut o, p.pick(&PATH_KEYS), get(a, &["file_path"]));
    put(&mut o, "oldString", get(a, &["old_string"]));
    put(&mut o, "newString", get(a, &["new_string"]));
    put(&mut o, "replaceAll", get(a, &["replace_all"]));
    o
}

fn glob_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    put(&mut o, "pattern", get(a, &["pattern"]));
    put(&mut o, "path", get(a, &["path"]));
    o
}

fn glob_to_client(a: &Args, _p: &Props) -> Args {
    glob_to_cc(a)
}

/// OpenCode 的 grep:pattern/path/include,新版另有 caseSensitive/literal/limit。
/// `literal`(按字面匹配)在历史里转成转义后的正则,`limit` ↔ CC 的 `head_limit`;CC Grep 的输出
/// 模式、上下文行数等 OpenCode 没有,反向时丢掉(由 OpenCode 按它自己的格式返回匹配行)。
fn grep_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    let pattern = get_str(a, &["pattern"]).unwrap_or("");
    let pattern = if get_bool(a, "literal") == Some(true) { regex::escape(pattern) } else { pattern.to_string() };
    o.insert("pattern".into(), Value::String(pattern));
    put(&mut o, "path", get(a, &["path"]));
    put(&mut o, "glob", get(a, &["include"]));
    if get_bool(a, "caseSensitive") == Some(false) {
        o.insert("-i".into(), Value::Bool(true));
    }
    put(&mut o, "head_limit", get(a, &["limit"]));
    o
}

fn grep_to_client(a: &Args, p: &Props) -> Args {
    let mut o = Args::new();
    put(&mut o, "pattern", get(a, &["pattern"]));
    put(&mut o, "path", get(a, &["path"]));
    put(&mut o, "include", get(a, &["glob"]));
    if p.has("caseSensitive") && get_bool(a, "-i") == Some(true) {
        o.insert("caseSensitive".into(), Value::Bool(false));
    }
    if p.has("limit") {
        put(&mut o, "limit", get(a, &["head_limit"]));
    }
    o
}

/// bash / shell:CC 没有工作目录参数,历史里把 `workdir` 写进命令(`cd "<dir>"; ` 在 bash 与
/// PowerShell 里都成立);回包里模型自己写的 `cd` 原样留在命令里,由客户端执行。
fn shell_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    let command = get_str(a, &["command"]).unwrap_or("");
    let command = match get_str(a, &["workdir"]).filter(|d| !d.trim().is_empty()) {
        Some(dir) => format!("cd \"{dir}\"; {command}"),
        None => command.to_string(),
    };
    o.insert("command".into(), Value::String(command));
    put(&mut o, "timeout", get(a, &["timeout"]));
    put(&mut o, "description", get(a, &["description"]));
    put(&mut o, "run_in_background", get(a, &["background"]));
    o
}

fn shell_to_client(a: &Args, p: &Props) -> Args {
    let mut o = Args::new();
    put(&mut o, "command", get(a, &["command"]));
    put(&mut o, "timeout", get(a, &["timeout"]));
    if p.has("description") {
        put(&mut o, "description", get(a, &["description"]));
    }
    if p.has("background") {
        put(&mut o, "background", get(a, &["run_in_background"]));
    }
    o
}

fn webfetch_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    put(&mut o, "url", get(a, &["url"]));
    o.insert("prompt".into(), Value::String(WEBFETCH_PROMPT.into()));
    o
}

/// CC WebFetch 没有格式参数;客户端声明了 `format` 就写它的默认值 markdown(有的版本把它设成必填)。
fn webfetch_to_client(a: &Args, p: &Props) -> Args {
    let mut o = Args::new();
    put(&mut o, "url", get(a, &["url"]));
    if p.has("format") {
        o.insert("format".into(), Value::String("markdown".into()));
    }
    o
}

fn websearch(a: &Args) -> Args {
    let mut o = Args::new();
    put(&mut o, "query", get(a, &["query"]));
    o
}

fn websearch_back(a: &Args, _p: &Props) -> Args {
    websearch(a)
}

/// 两边的问题结构几乎一样(OpenCode 照着 CC 做的),只有「可多选」的字段名不同:
/// OpenCode `multiple` ↔ CC `multiSelect`。
fn map_questions(a: &Args, from: &str, to: &str) -> Args {
    let mut o = Args::new();
    let Some(Value::Array(items)) = a.get("questions") else {
        put(&mut o, "questions", a.get("questions"));
        return o;
    };
    let mapped: Vec<Value> = items
        .iter()
        .map(|q| match q.as_object() {
            Some(obj) => {
                let mut q2 = obj.clone();
                if let Some(v) = q2.remove(from) {
                    q2.insert(to.to_string(), v);
                }
                Value::Object(q2)
            }
            None => q.clone(),
        })
        .collect();
    o.insert("questions".into(), Value::Array(mapped));
    o
}

fn question_to_cc(a: &Args) -> Args {
    map_questions(a, "multiple", "multiSelect")
}

fn question_to_client(a: &Args, _p: &Props) -> Args {
    map_questions(a, "multiSelect", "multiple")
}

fn skill_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    put(&mut o, "skill", get(a, &["id", "name"]));
    o
}

fn skill_to_client(a: &Args, p: &Props) -> Args {
    let mut o = Args::new();
    put(&mut o, p.pick(&["id", "name"]), get(a, &["skill"]));
    o
}

#[cfg(test)]
mod tests {
    use super::super::super::template::test_template;
    use super::super::{build, Mode};
    use super::*;
    use serde_json::{json, Map};

    fn tool(name: &str, props: &[&str], desc: &str) -> Value {
        let p: Map<String, Value> = props.iter().map(|k| (k.to_string(), json!({}))).collect();
        json!({"name": name, "description": desc, "input_schema": {"type": "object", "properties": p}})
    }

    /// 回包(CC 形态)→ 客户端 → 下一轮历史(CC 形态):能无损往返的工具必须回到原样。
    fn round_trip(tools: &[Value], client: &str, cc_args: Value) {
        let tpl = test_template();
        let b = build(Mode::Table(&TABLE), tools, &tpl).unwrap();
        let wire = b.map.wire_name(client).unwrap().to_string();
        let (back_name, client_args) = b.map.backward(&wire, &cc_args).unwrap();
        assert_eq!(back_name, client);
        let (fwd_name, cc_again) = b.map.forward(client, &client_args).unwrap();
        assert_eq!(fwd_name, wire);
        assert_eq!(cc_again, cc_args, "{client} 往返后变了: 客户端形态 {client_args}");
    }

    #[test]
    fn new_version_uses_path_and_id() {
        let tools = [
            tool("read", &["path", "offset", "limit"], ""),
            tool("edit", &["path", "oldString", "newString", "replaceAll"], ""),
            tool("skill", &["id"], ""),
        ];
        round_trip(&tools, "read", json!({"file_path": "/a.rs", "offset": 10, "limit": 50}));
        round_trip(&tools, "edit", json!({"file_path": "/a.rs", "old_string": "x", "new_string": "y", "replace_all": true}));
        round_trip(&tools, "skill", json!({"skill": "pdf"}));
        let tpl = test_template();
        let b = build(Mode::Table(&TABLE), &tools, &tpl).unwrap();
        let (_, c) = b.map.backward("Read", &json!({"file_path": "/a.rs"})).unwrap();
        assert_eq!(c, json!({"path": "/a.rs"}));
    }

    #[test]
    fn old_version_uses_file_path_and_name() {
        let tools = [tool("read", &["filePath", "offset", "limit"], ""), tool("skill", &["name"], "")];
        let tpl = test_template();
        let b = build(Mode::Table(&TABLE), &tools, &tpl).unwrap();
        assert_eq!(b.map.backward("Read", &json!({"file_path": "/a"})).unwrap().1, json!({"filePath": "/a"}));
        assert_eq!(b.map.backward("Skill", &json!({"skill": "s"})).unwrap().1, json!({"name": "s"}));
        // 历史里旧版字段也能正向认出来。
        assert_eq!(b.map.forward("read", &json!({"filePath": "/a"})).unwrap().1, json!({"file_path": "/a"}));
    }

    #[test]
    fn shell_on_windows_maps_to_powershell_and_workdir_goes_into_command() {
        let tools = [tool("shell", &["command", "timeout", "workdir", "background"], "Commands run on Windows using powershell.")];
        let tpl = test_template();
        let b = build(Mode::Table(&TABLE), &tools, &tpl).unwrap();
        assert_eq!(b.map.wire_name("shell"), Some("PowerShell"));
        let (_, cc) = b.map.forward("shell", &json!({"command": "dir", "workdir": "C:\\p", "background": true})).unwrap();
        assert_eq!(cc, json!({"command": "cd \"C:\\p\"; dir", "run_in_background": true}));
        let (_, back) = b.map.backward("PowerShell", &json!({"command": "dir", "description": "列目录", "run_in_background": false})).unwrap();
        assert_eq!(back, json!({"command": "dir", "background": false}), "OpenCode shell 没有 description 字段");
        round_trip(&tools, "shell", json!({"command": "dir", "timeout": 1000, "run_in_background": true}));
    }

    #[test]
    fn grep_case_flag_and_include() {
        let tools = [tool("grep", &["pattern", "path", "include", "caseSensitive", "literal", "limit"], "")];
        let tpl = test_template();
        let b = build(Mode::Table(&TABLE), &tools, &tpl).unwrap();
        let (_, c) = b.map.backward("Grep", &json!({"pattern": "fn", "glob": "*.rs", "-i": true, "output_mode": "content"})).unwrap();
        assert_eq!(c, json!({"pattern": "fn", "include": "*.rs", "caseSensitive": false}));
        round_trip(&tools, "grep", json!({"pattern": "fn", "path": "src", "glob": "*.rs", "-i": true, "head_limit": 20}));
        let tpl = test_template();
        let b = build(Mode::Table(&TABLE), &tools, &tpl).unwrap();
        let (_, cc) = b.map.forward("grep", &json!({"pattern": "a.b(", "literal": true, "limit": 5})).unwrap();
        assert_eq!(cc, json!({"pattern": "a\\.b\\(", "head_limit": 5}), "按字面匹配转成转义后的正则");
    }

    #[test]
    fn question_webfetch_and_subagents_stay_on_mcp() {
        let tools = [
            tool("subagent", &["agent", "description", "prompt", "background", "sessionID"], ""),
            tool("task", &["description", "prompt", "subagent_type", "task_id"], ""),
            tool("question", &["questions"], ""),
            tool("webfetch", &["url", "format"], ""),
        ];
        round_trip(&tools, "question", json!({"questions": [{"question": "q?", "header": "h", "options": [{"label": "a", "description": "x"}], "multiSelect": true}]}));
        round_trip(&tools, "webfetch", json!({"url": "https://x", "prompt": WEBFETCH_PROMPT}));
        let tpl = test_template();
        let b = build(Mode::Table(&TABLE), &tools, &tpl).unwrap();
        assert_eq!(b.map.wire_name("subagent"), Some("mcp__workspace__subagent"));
        assert_eq!(b.map.wire_name("task"), Some("mcp__workspace__task"));
        assert_eq!(b.notes.len(), 1, "OpenCode 带工作目录说明");
    }
}
