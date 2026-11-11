//! VS Code Copilot Chat(agent 模式)的工具表。
//!
//! 依据 10-30 线上 11 条 Copilot 请求(每条都是同一套 53 个工具)。只把语义能对上的 7 个映射到
//! CC 原生工具;其余(编辑、笔记本、浏览器、VS Code 专属、任务列表……)走 MCP 改名,服务名用
//! `ide`(真 CC 接 VS Code 时,IDE 工具就挂在 `mcp__ide__*` 下)。
//!
//! 有损的地方(两边语义不完全一样,按「客户端能正确执行」取舍):
//! - `read_file` 要求 startLine/endLine 都给,CC Read 用 offset/limit:没给的按从第 1 行、读 2000 行补;
//! - `replace_string_in_file` 没有「全部替换」,映射成 CC Edit 会把 replace_all 静默丢掉、改出模型
//!   没要求的结果,所以不映射,走 MCP(对抗审查 10-30);
//! - `grep_search` 的 isRegexp=false 在历史里转成转义后的正则;回包一律按正则发,CC Grep 的 `path`
//!   并进 includePattern,`-i` 写成正则内联标志 `(?i)`(VS Code 搜索底层是 ripgrep,认这个写法);
//! - `fetch_webpage` 一次可抓多个网址,CC WebFetch 一次一个,历史里只取第一个。

use serde_json::Value;

use super::{get, get_bool, get_i64, get_str, put, Args, CcPick, NativeRule, Props, ToolTable};

pub(crate) static TABLE: ToolTable = ToolTable { mcp_server: "ide", rules: &RULES, notes: &[] };

static RULES: [NativeRule; 7] = [
    NativeRule { client: "read_file", cc: CcPick::Fixed("Read"), to_cc: read_to_cc, to_client: read_to_client },
    NativeRule { client: "create_file", cc: CcPick::Fixed("Write"), to_cc: create_to_cc, to_client: create_to_client },
    NativeRule { client: "run_in_terminal", cc: CcPick::BashOrPowerShell, to_cc: terminal_to_cc, to_client: terminal_to_client },
    NativeRule { client: "file_search", cc: CcPick::Fixed("Glob"), to_cc: file_search_to_cc, to_client: file_search_to_client },
    NativeRule { client: "grep_search", cc: CcPick::Fixed("Grep"), to_cc: grep_to_cc, to_client: grep_to_client },
    NativeRule { client: "fetch_webpage", cc: CcPick::Fixed("WebFetch"), to_cc: fetch_to_cc, to_client: fetch_to_client },
    NativeRule { client: "runSubagent", cc: CcPick::Fixed("Agent"), to_cc: subagent_to_cc, to_client: subagent_to_client },
];

/// CC Read 不给 limit 时默认读 2000 行。
const DEFAULT_READ_LINES: i64 = 2000;
/// CC 内置的子 agent 类型。Copilot 的 agentName 是「指定用户配置的某个 agent」,这些名字在
/// Copilot 那边不存在,反向时不写(交给 Copilot 用当前 agent)。
const CC_BUILTIN_AGENTS: [&str; 6] = ["general-purpose", "fork", "Explore", "Plan", "claude-code-guide", "statusline-setup"];

fn read_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    put(&mut o, "file_path", get(a, &["filePath"]));
    let start = get_i64(a, "startLine");
    let end = get_i64(a, "endLine");
    if let Some(s) = start {
        o.insert("offset".into(), Value::from(s));
    }
    if let (Some(s), Some(e)) = (start, end) {
        if e >= s {
            o.insert("limit".into(), Value::from(e - s + 1));
        }
    }
    o
}

fn read_to_client(a: &Args, _p: &Props) -> Args {
    let mut o = Args::new();
    put(&mut o, "filePath", get(a, &["file_path"]));
    let start = get_i64(a, "offset").filter(|n| *n >= 1).unwrap_or(1);
    let lines = get_i64(a, "limit").filter(|n| *n >= 1).unwrap_or(DEFAULT_READ_LINES);
    o.insert("startLine".into(), Value::from(start));
    o.insert("endLine".into(), Value::from(start + lines - 1));
    o
}

fn create_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    put(&mut o, "file_path", get(a, &["filePath"]));
    put(&mut o, "content", get(a, &["content"]));
    o
}

fn create_to_client(a: &Args, _p: &Props) -> Args {
    let mut o = Args::new();
    put(&mut o, "filePath", get(a, &["file_path"]));
    put(&mut o, "content", get(a, &["content"]));
    o
}

/// run_in_terminal 必填 command/explanation/goal/mode。CC Bash 只有一句 description,
/// 反向时 explanation 与 goal 都用它(没有就用命令本身)。
fn terminal_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    put(&mut o, "command", get(a, &["command"]));
    put(&mut o, "timeout", get(a, &["timeout"]));
    put(&mut o, "description", get(a, &["explanation", "goal"]));
    let background = get_str(a, &["mode"]) == Some("async") || get_bool(a, "isBackground") == Some(true);
    if background {
        o.insert("run_in_background".into(), Value::Bool(true));
    }
    o
}

fn terminal_to_client(a: &Args, _p: &Props) -> Args {
    let mut o = Args::new();
    let command = get_str(a, &["command"]).unwrap_or("");
    let said = get_str(a, &["description"]).filter(|d| !d.trim().is_empty()).unwrap_or(command);
    o.insert("command".into(), Value::String(command.into()));
    o.insert("explanation".into(), Value::String(said.into()));
    o.insert("goal".into(), Value::String(said.into()));
    let mode = if get_bool(a, "run_in_background") == Some(true) { "async" } else { "sync" };
    o.insert("mode".into(), Value::String(mode.into()));
    put(&mut o, "timeout", get(a, &["timeout"]));
    o
}

fn file_search_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    put(&mut o, "pattern", get(a, &["query"]));
    o
}

/// Copilot 的 glob 从工作区根匹配,没有单独的目录参数;模型给了 path 就拼进模式里。
fn file_search_to_client(a: &Args, _p: &Props) -> Args {
    let mut o = Args::new();
    let pattern = get_str(a, &["pattern"]).unwrap_or("");
    // 绝对路径 Copilot 不认(从工作区根匹配),不拼进去,宁可多搜。
    let query = match get_str(a, &["path"]).map(|p| p.trim().trim_end_matches('/')).filter(|p| !p.is_empty() && *p != "." && !is_absolute(p)) {
        Some(dir) if !pattern.starts_with('/') => format!("{}/{pattern}", dir.trim_start_matches("./")),
        _ => pattern.to_string(),
    };
    o.insert("query".into(), Value::String(query));
    o
}

fn grep_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    let query = get_str(a, &["query"]).unwrap_or("");
    let pattern = if get_bool(a, "isRegexp") == Some(false) { regex::escape(query) } else { query.to_string() };
    o.insert("pattern".into(), Value::String(pattern));
    put(&mut o, "glob", get(a, &["includePattern"]));
    put(&mut o, "head_limit", get(a, &["maxResults"]));
    o
}

fn grep_to_client(a: &Args, _p: &Props) -> Args {
    let mut o = Args::new();
    let pattern = get_str(a, &["pattern"]).unwrap_or("");
    let query = if get_bool(a, "-i") == Some(true) { format!("(?i){pattern}") } else { pattern.to_string() };
    o.insert("query".into(), Value::String(query));
    o.insert("isRegexp".into(), Value::Bool(true));
    if let Some(inc) = include_pattern(get_str(a, &["path"]), get_str(a, &["glob"])) {
        o.insert("includePattern".into(), Value::String(inc));
    }
    put(&mut o, "maxResults", get(a, &["head_limit"]));
    o
}

/// CC Grep 的 `path` + `glob` → Copilot 的 includePattern(相对工作区根的 glob)。
///
/// - rg 的 `--glob '*.ts'` 在任意深度匹配,Copilot 的 `*.ts` 只匹配根目录,所以不带 `/` 的 glob 前面补 `**/`;
/// - 绝对路径 Copilot 不认(includePattern 是相对工作区的),宁可不限目录、多搜一些,也不能写成
///   永远匹配不到的模式让模型以为「没有」;
/// - 末段带扩展名的当成文件,直接用它本身(写成 `文件/**` 什么都匹配不到)。
fn include_pattern(path: Option<&str>, glob: Option<&str>) -> Option<String> {
    let glob = glob.map(str::trim).filter(|g| !g.is_empty());
    let dir = path
        .map(|p| p.trim().trim_end_matches('/'))
        .filter(|p| !p.is_empty() && *p != "." && !is_absolute(p))
        .map(|p| p.trim_start_matches("./"));
    match (dir, glob) {
        (Some(d), _) if looks_like_file(d) => Some(d.to_string()),
        (Some(d), Some(g)) if g.contains('/') => Some(format!("{d}/{g}")),
        (Some(d), Some(g)) => Some(format!("{d}/**/{g}")),
        (Some(d), None) => Some(format!("{d}/**")),
        (None, Some(g)) if g.contains('/') => Some(g.to_string()),
        (None, Some(g)) => Some(format!("**/{g}")),
        (None, None) => None,
    }
}

fn is_absolute(p: &str) -> bool {
    p.starts_with('/') || p.starts_with('\\') || p.as_bytes().get(1) == Some(&b':')
}

fn looks_like_file(p: &str) -> bool {
    p.rsplit('/').next().is_some_and(|last| last.contains('.') && !last.starts_with('.') && !last.contains('*'))
}

fn fetch_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    put(&mut o, "url", a.get("urls").and_then(Value::as_array).and_then(|u| u.first()));
    put(&mut o, "prompt", get(a, &["query"]));
    o
}

fn fetch_to_client(a: &Args, _p: &Props) -> Args {
    let mut o = Args::new();
    if let Some(url) = get(a, &["url"]) {
        o.insert("urls".into(), Value::Array(vec![url.clone()]));
    }
    put(&mut o, "query", get(a, &["prompt"]));
    o
}

fn subagent_to_cc(a: &Args) -> Args {
    let mut o = Args::new();
    put(&mut o, "description", get(a, &["description"]));
    put(&mut o, "prompt", get(a, &["prompt"]));
    put(&mut o, "subagent_type", get(a, &["agentName"]));
    o
}

fn subagent_to_client(a: &Args, _p: &Props) -> Args {
    let mut o = Args::new();
    put(&mut o, "prompt", get(a, &["prompt"]));
    put(&mut o, "description", get(a, &["description"]));
    if let Some(agent) = get_str(a, &["subagent_type"]).filter(|s| !CC_BUILTIN_AGENTS.contains(s)) {
        o.insert("agentName".into(), Value::String(agent.into()));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::super::super::template::test_template;
    use super::super::{build, Mode, Stats};
    use super::*;
    use serde_json::{json, Map};

    fn tool(name: &str, props: &[&str], desc: &str) -> Value {
        let p: Map<String, Value> = props.iter().map(|k| (k.to_string(), json!({}))).collect();
        json!({"name": name, "description": desc, "input_schema": {"type": "object", "properties": p}})
    }

    fn tools() -> Vec<Value> {
        vec![
            tool("read_file", &["filePath", "startLine", "endLine"], ""),
            tool("create_file", &["filePath", "content"], ""),
            tool("replace_string_in_file", &["filePath", "oldString", "newString"], ""),
            tool("run_in_terminal", &["command", "explanation", "goal", "mode", "isBackground", "timeout"], "persistent bash terminal"),
            tool("file_search", &["query", "maxResults"], ""),
            tool("grep_search", &["query", "isRegexp", "includePattern", "maxResults"], ""),
            tool("fetch_webpage", &["urls", "query"], ""),
            tool("runSubagent", &["prompt", "description", "agentName", "model"], ""),
            tool("manage_todo_list", &["todoList"], ""),
            tool("vscode_renameSymbol", &["symbol", "newName"], ""),
        ]
    }

    #[test]
    fn seven_natives_and_the_rest_under_mcp_ide() {
        let tpl = test_template();
        let b = build(Mode::Table(&TABLE), &tools(), &tpl).unwrap();
        assert_eq!(b.stats, Stats { native: 7, renamed: 3, same: 0 });
        assert_eq!(b.map.wire_name("replace_string_in_file"), Some("mcp__ide__replace_string_in_file"));
        assert_eq!(b.map.wire_name("manage_todo_list"), Some("mcp__ide__manage_todo_list"));
        assert_eq!(b.map.wire_name("run_in_terminal"), Some("Bash"));
    }

    #[test]
    fn read_file_line_range_conversion() {
        let tpl = test_template();
        let b = build(Mode::Table(&TABLE), &tools(), &tpl).unwrap();
        let (_, c) = b.map.backward("Read", &json!({"file_path": "/a", "offset": 10, "limit": 20})).unwrap();
        assert_eq!(c, json!({"filePath": "/a", "startLine": 10, "endLine": 29}));
        let (_, c) = b.map.backward("Read", &json!({"file_path": "/a"})).unwrap();
        assert_eq!(c, json!({"filePath": "/a", "startLine": 1, "endLine": 2000}), "没给范围就从头读 2000 行");
        let (_, cc) = b.map.forward("read_file", &json!({"filePath": "/a", "startLine": 10.0, "endLine": 29})).unwrap();
        assert_eq!(cc, json!({"file_path": "/a", "offset": 10, "limit": 20}));
    }

    #[test]
    fn terminal_fills_required_fields() {
        let tpl = test_template();
        let b = build(Mode::Table(&TABLE), &tools(), &tpl).unwrap();
        let (_, c) = b.map.backward("Bash", &json!({"command": "npm test", "run_in_background": true})).unwrap();
        assert_eq!(c, json!({"command": "npm test", "explanation": "npm test", "goal": "npm test", "mode": "async"}));
        let (_, cc) = b.map.forward("run_in_terminal", &json!({"command": "ls", "explanation": "列目录", "goal": "看看", "mode": "sync"})).unwrap();
        assert_eq!(cc, json!({"command": "ls", "description": "列目录"}));
    }

    #[test]
    fn grep_and_fetch_and_subagent() {
        let tpl = test_template();
        let b = build(Mode::Table(&TABLE), &tools(), &tpl).unwrap();
        let (_, cc) = b.map.forward("grep_search", &json!({"query": "a.b(", "isRegexp": false, "includePattern": "**/*.ts"})).unwrap();
        assert_eq!(cc, json!({"pattern": "a\\.b\\(", "glob": "**/*.ts"}));
        let (_, c) = b.map.backward("Grep", &json!({"pattern": "fn \\w+", "head_limit": 5})).unwrap();
        assert_eq!(c, json!({"query": "fn \\w+", "isRegexp": true, "maxResults": 5}));
        let (_, c) = b.map.backward("Grep", &json!({"pattern": "secret", "path": "src/auth/", "-i": true})).unwrap();
        assert_eq!(c, json!({"query": "(?i)secret", "isRegexp": true, "includePattern": "src/auth/**"}), "目录与大小写不丢");
        assert_eq!(include_pattern(Some("src"), Some("*.ts")), Some("src/**/*.ts".into()), "目录下任意深度");
        assert_eq!(include_pattern(None, Some("*.ts")), Some("**/*.ts".into()), "rg 的 *.ts 在任意深度匹配");
        assert_eq!(include_pattern(Some("/home/u/proj/src/main.rs"), None), None, "绝对路径不认,宁可多搜");
        assert_eq!(include_pattern(Some("src/main.rs"), Some("*.rs")), Some("src/main.rs".into()), "文件就用它本身");
        assert_eq!(include_pattern(Some("/abs/dir"), Some("*.go")), Some("**/*.go".into()));
        let (_, c) = b.map.backward("Agent", &json!({"description": "d", "prompt": "p", "subagent_type": "Explore"})).unwrap();
        assert_eq!(c, json!({"prompt": "p", "description": "d"}), "CC 内置 agent 名不传给 Copilot");
        let (_, c) = b.map.backward("WebFetch", &json!({"url": "https://x", "prompt": "标题"})).unwrap();
        assert_eq!(c, json!({"urls": ["https://x"], "query": "标题"}));
        let (_, c) = b.map.backward("Agent", &json!({"description": "d", "prompt": "p", "subagent_type": "general-purpose"})).unwrap();
        assert_eq!(c, json!({"prompt": "p", "description": "d"}));
        let (_, c) = b.map.backward("Glob", &json!({"pattern": "**/*.rs", "path": "src/"})).unwrap();
        assert_eq!(c, json!({"query": "src/**/*.rs"}));
    }
}
