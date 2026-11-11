//! 客户端工具 ↔ CC 工具的映射(用户 10-30 定:工具改成 CC 工具名,难映射的客户端不支持)。
//!
//! 每个支持的客户端一张精确的表([`ToolTable`]),按它真实的工具定义逐个写(10-30 从线上请求
//! 取的定义,见各表文件头)。一次请求里的每个工具落到三种去处之一:
//!
//! - **CC 原生**:表里有规则,对应的 CC 工具在模板里、还没被别的工具占用 → 出站用 CC 的名字与
//!   CC 原版定义;参数在两个方向上按规则换(历史里的 tool_use 正向换,回包里的反向换)。
//! - **MCP 改名**:表里没有的零散工具(插件、IDE 专属工具)→ 改名 `mcp__<服务名>__<原名>`,
//!   定义原样。真 CC 挂 MCP 服务器时工具就是这个样子;回包只改回名字,参数不动。
//! - **原样**:本来就是 `mcp__*`,或带 `type` 的服务端工具(如 `web_search_20250305`)。
//!
//! 「CC 工具面」客户端(Claude Code 变体、工具全是 CC 原生名或 `mcp__*`)不需要表,全部原样;
//! 有一个不是这两种形态的工具就不支持。
//!
//! 映射表只由本次请求的工具列表决定(确定性,不存跨请求的状态):下一轮客户端把历史发回来,
//! 重新算出同一张表,历史里的 tool_use 也换成同样的字节,缓存前缀不变。

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::template::CcTemplate;

mod copilot;
mod opencode;

pub(crate) use copilot::TABLE as COPILOT;
pub(crate) use opencode::TABLE as OPENCODE;

pub(crate) type Args = Map<String, Value>;

/// Anthropic 工具名上限。
const MAX_TOOL_NAME: usize = 64;

/// 客户端这次声明的那个工具有哪些参数字段。反向翻译据此决定写哪个字段名(OpenCode 新版
/// 用 `path`、旧版用 `filePath`,同一时刻线上两版都有)。
pub(crate) struct Props(HashSet<String>);

impl Props {
    fn of(tool_def: &Value) -> Self {
        let set = tool_def
            .get("input_schema")
            .and_then(|s| s.get("properties"))
            .and_then(Value::as_object)
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default();
        Self(set)
    }

    pub(crate) fn has(&self, key: &str) -> bool {
        self.0.contains(key)
    }

    /// 候选里客户端声明了的第一个;都没声明就用第一个候选。
    pub(crate) fn pick<'a>(&self, candidates: &[&'a str]) -> &'a str {
        candidates.iter().copied().find(|c| self.has(c)).unwrap_or(candidates[0])
    }
}

/// 这条规则对应哪个 CC 工具。
pub(crate) enum CcPick {
    Fixed(&'static str),
    /// 看客户端工具的描述:提到 PowerShell 就对 CC 的 PowerShell 工具,否则 Bash。
    /// (OpenCode 在 Windows 上的 `shell` 跑的是 PowerShell;模型看到的是 CC 的工具描述,
    /// 对错了它会按 bash 语法写命令。)
    BashOrPowerShell,
}

/// 一条「客户端工具 → CC 原生工具」规则。
pub(crate) struct NativeRule {
    pub client: &'static str,
    pub cc: CcPick,
    /// 客户端参数 → CC 参数(历史里的 tool_use 用)。
    pub to_cc: fn(&Args) -> Args,
    /// CC 参数 → 客户端参数(回包用)。
    pub to_client: fn(&Args, &Props) -> Args,
}

/// 一个客户端的工具表。
pub(crate) struct ToolTable {
    /// MCP 改名用的服务名(工具名会被计费分类器看到,不放客户端品牌名)。
    pub mcp_server: &'static str,
    pub rules: &'static [NativeRule],
    /// 写给模型的客户端执行差异(CC 工具描述与客户端实际行为不一致的地方),附在工具名对照后面。
    pub notes: &'static [&'static str],
}

/// 每个映射用到的 CC 工具,按 dario 模板 v2.1.312 登记的参数清单(反向转换已处理或明知丢弃的)。
/// 模板升级后某个 CC 工具多出没登记的参数,说明它的能力变了、反向转换没跟上:这次就不映射
/// 成 CC 原生工具,改走 MCP 改名(客户端定义原样),并告警 —— 不把客户端兑现不了的能力承诺给模型。
const KNOWN_CC_PROPS: &[(&str, &[&str])] = &[
    ("Read", &["file_path", "offset", "limit", "pages", "allow_large"]),
    ("Write", &["file_path", "content"]),
    ("Edit", &["file_path", "old_string", "new_string", "replace_all"]),
    ("Glob", &["pattern", "path"]),
    ("Grep", &["pattern", "path", "glob", "output_mode", "-B", "-A", "-C", "context", "-n", "-i", "-o", "type", "head_limit", "offset", "multiline"]),
    ("Bash", &["command", "timeout", "description", "run_in_background", "dangerouslyDisableSandbox"]),
    ("PowerShell", &["command", "timeout", "description", "run_in_background", "dangerouslyDisableSandbox"]),
    ("WebFetch", &["url", "prompt", "offset"]),
    ("WebSearch", &["query", "allowed_domains", "blocked_domains"]),
    ("Agent", &["description", "prompt", "subagent_type", "model", "effort", "run_in_background", "isolation"]),
    ("AskUserQuestion", &["questions", "answers", "annotations", "metadata"]),
    ("Skill", &["skill", "args"]),
];

/// 模板里这个 CC 工具的参数都在登记清单里(没有登记的工具一律不当原生用)。
fn cc_tool_compatible(tpl: &CcTemplate, cc: &str) -> bool {
    let Some(known) = KNOWN_CC_PROPS.iter().find(|(n, _)| *n == cc).map(|(_, p)| *p) else {
        return false;
    };
    let Some(def) = tpl.tool(cc) else {
        return false;
    };
    let props = def.get("input_schema").and_then(|s| s.get("properties")).and_then(Value::as_object);
    let unknown: Vec<&str> = props.into_iter().flatten().map(|(k, _)| k.as_str()).filter(|k| !known.contains(k)).collect();
    if !unknown.is_empty() {
        tracing::warn!(tool = cc, ?unknown, "dario CC 模板里的工具多了没登记的参数,本次不映射成原生工具(走 MCP)");
        return false;
    }
    true
}

/// 怎么处理这次请求的工具。
pub(crate) enum Mode {
    Table(&'static ToolTable),
    /// 工具原样发。`strict`:每个工具都得是模板里的 CC 原生工具或 `mcp__*`(认不出来的客户端用);
    /// 不 strict:已认出是 Claude Code 的变体,另外放行首字母大写的工具名 —— CC 有 `ListMcpResourcesTool`、
    /// `Artifact` 这类不在模板快照里的原生工具(10-30 回放 91 条曾因此被误拒),而别家客户端
    /// 的工具大多是小写的(OpenCode 带 CC 身份行时就会被认成 CC,不能放它的小写工具原样过)。
    CcSurface { strict: bool },
}

#[derive(Clone, Copy)]
enum Route {
    Native(&'static NativeRule),
    Renamed,
    Same,
}

struct Entry {
    client_name: String,
    wire_name: String,
    route: Route,
    props: Props,
}

/// 本次请求的映射表。
pub(crate) struct ToolMap {
    entries: Vec<Entry>,
    by_client: HashMap<String, usize>,
    by_wire: HashMap<String, usize>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stats {
    pub native: usize,
    pub renamed: usize,
    pub same: usize,
}

pub(crate) struct Built {
    pub map: ToolMap,
    /// 客户端执行差异说明(见 [`ToolTable::notes`])。
    pub notes: &'static [&'static str],
    /// 出站 tools 数组:CC 原生工具按模板顺序在前,其余按客户端顺序在后(同真 CC + MCP)。
    pub wire_tools: Vec<Value>,
    pub stats: Stats,
}

fn tool_name(t: &Value) -> &str {
    t.get("name").and_then(Value::as_str).unwrap_or("")
}

/// 服务端工具(`web_search_20250305` 之类)带 `type` 字段,由上游执行,不归客户端。
fn is_server_tool(t: &Value) -> bool {
    t.get("type").and_then(Value::as_str).is_some_and(|ty| ty != "custom")
}

pub(crate) fn build(mode: Mode, client_tools: &[Value], tpl: &CcTemplate) -> Result<Built, String> {
    let mut entries: Vec<Entry> = Vec::with_capacity(client_tools.len());
    let mut natives: Vec<(usize, Value)> = Vec::new();
    let mut others: Vec<Value> = Vec::new();
    let mut claimed: HashSet<&'static str> = HashSet::new();
    let mut wire_names: HashSet<String> = HashSet::new();
    let mut seen_client: HashSet<&str> = HashSet::new();
    let mut stats = Stats::default();
    // 原样保留的名字先占住,改名的工具再避开它们(否则「foo 在前、mcp__x__foo 在后」会撞名)。
    let reserved: HashSet<&str> = client_tools
        .iter()
        .filter(|t| is_server_tool(t) || tool_name(t).starts_with("mcp__"))
        .map(tool_name)
        .collect();

    for t in client_tools {
        let name = tool_name(t);
        if name.is_empty() {
            return Err("有工具没有名字".into());
        }
        // 同名工具定义两次(线上见过)只留第一个,同 dario `dedupeToolsByName`;发两个同名工具上游会 400。
        if !seen_client.insert(name) {
            continue;
        }
        let keep_as_is = is_server_tool(t) || name.starts_with("mcp__");
        let route = match mode {
            Mode::CcSurface { strict } => {
                let cc_like = keep_as_is || tpl.tool(name).is_some() || (!strict && is_pascal_case(name));
                if !cc_like {
                    return Err(format!("工具 {name} 既不是 CC 原生工具,也不是 MCP 工具"));
                }
                Route::Same
            }
            Mode::Table(table) if !keep_as_is => match table.rules.iter().find(|r| r.client == name) {
                Some(rule) => {
                    let cc = pick_cc(&rule.cc, t);
                    if cc_tool_compatible(tpl, cc) && claimed.insert(cc) { Route::Native(rule) } else { Route::Renamed }
                }
                None => Route::Renamed,
            },
            Mode::Table(_) => Route::Same,
        };
        let wire_name = match route {
            Route::Native(rule) => pick_cc(&rule.cc, t).to_string(),
            Route::Same => name.to_string(),
            Route::Renamed => {
                let server = match mode {
                    Mode::Table(table) => table.mcp_server,
                    Mode::CcSurface { .. } => unreachable!("CC 工具面不改名"),
                };
                unique_name(mcp_name(server, name), name, &wire_names, &reserved)
            }
        };
        if !wire_names.insert(wire_name.clone()) {
            return Err(format!("工具名重复: {wire_name}"));
        }
        match route {
            Route::Native(_) => {
                let def = tpl.tool(&wire_name).expect("上面判过模板里有").clone();
                natives.push((tpl.tool_rank(&wire_name).unwrap_or(usize::MAX), def));
                stats.native += 1;
            }
            Route::Renamed => {
                let mut def = t.clone();
                def["name"] = Value::String(wire_name.clone());
                others.push(def);
                stats.renamed += 1;
            }
            Route::Same => {
                others.push(t.clone());
                stats.same += 1;
            }
        }
        entries.push(Entry { client_name: name.to_string(), wire_name, route, props: Props::of(t) });
    }

    natives.sort_by_key(|(rank, _)| *rank);
    let mut wire_tools: Vec<Value> = natives.into_iter().map(|(_, d)| d).collect();
    wire_tools.extend(others);
    let by_client = entries.iter().enumerate().map(|(i, e)| (e.client_name.clone(), i)).collect();
    let by_wire = entries.iter().enumerate().map(|(i, e)| (e.wire_name.clone(), i)).collect();
    let notes = match mode {
        Mode::Table(table) => table.notes,
        Mode::CcSurface { .. } => &[],
    };
    Ok(Built { map: ToolMap { entries, by_client, by_wire }, notes, wire_tools, stats })
}

/// `ListMcpResourcesTool` 这种:首字母大写、只有字母数字。
fn is_pascal_case(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) && name.chars().all(|c| c.is_ascii_alphanumeric())
}

fn pick_cc(pick: &CcPick, client_def: &Value) -> &'static str {
    match pick {
        CcPick::Fixed(name) => name,
        CcPick::BashOrPowerShell => {
            let desc = client_def.get("description").and_then(Value::as_str).unwrap_or("");
            if desc.to_ascii_lowercase().contains("powershell") { "PowerShell" } else { "Bash" }
        }
    }
}

/// `mcp__<服务名>__<原名>`;原名里不合法的字符换成 `_`;超长截断并补确定性哈希。
fn mcp_name(server: &str, name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let full = format!("mcp__{server}__{clean}");
    if full.len() <= MAX_TOOL_NAME {
        return full;
    }
    with_hash(&full, name)
}

/// 截到上限、尾部换成原名的 8 位哈希。`full` 只含 ASCII,按字节截安全。
fn with_hash(full: &str, original: &str) -> String {
    let hash = hex8(original);
    let keep = MAX_TOOL_NAME - 1 - hash.len();
    format!("{}_{hash}", &full[..keep.min(full.len())])
}

fn hex8(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// 改名后与已有名字或原样保留的名字撞了(客户端同时有 `foo` 与 `mcp__ide__foo` 之类)就补哈希区分。
fn unique_name(candidate: String, original: &str, taken: &HashSet<String>, reserved: &HashSet<&str>) -> String {
    if !taken.contains(&candidate) && !reserved.contains(candidate.as_str()) {
        return candidate;
    }
    with_hash(&candidate, &format!("dup:{original}"))
}

fn as_args(v: &Value) -> Args {
    v.as_object().cloned().unwrap_or_default()
}

impl ToolMap {
    /// 历史里的 tool_use:客户端名/参数 → 出站名/参数。名字不在表里(或原样)返回 `None`。
    pub(crate) fn forward(&self, client_name: &str, input: &Value) -> Option<(String, Value)> {
        let e = &self.entries[*self.by_client.get(client_name)?];
        match e.route {
            Route::Native(rule) => Some((e.wire_name.clone(), Value::Object((rule.to_cc)(&as_args(input))))),
            Route::Renamed => Some((e.wire_name.clone(), input.clone())),
            Route::Same => None,
        }
    }

    /// 回包里的 tool_use:出站名/参数 → 客户端名/参数。不是我们改过的名字返回 `None`。
    pub(crate) fn backward(&self, wire_name: &str, input: &Value) -> Option<(String, Value)> {
        let e = &self.entries[*self.by_wire.get(wire_name)?];
        match e.route {
            Route::Native(rule) => Some((e.client_name.clone(), Value::Object((rule.to_client)(&as_args(input), &e.props)))),
            Route::Renamed => Some((e.client_name.clone(), input.clone())),
            Route::Same => None,
        }
    }

    /// 回包里这个名字要不要改回(要改的才缓冲,原样的直接放行)。
    pub(crate) fn needs_backward(&self, wire_name: &str) -> bool {
        self.by_wire.get(wire_name).is_some_and(|&i| !matches!(self.entries[i].route, Route::Same))
    }

    /// 客户端名对应的出站名。
    pub(crate) fn wire_name(&self, client_name: &str) -> Option<&str> {
        self.by_client.get(client_name).map(|&i| self.entries[i].wire_name.as_str())
    }

    /// 有没有改过名的工具(没有就不需要回包改写)。
    pub(crate) fn has_renames(&self) -> bool {
        self.entries.iter().any(|e| !matches!(e.route, Route::Same))
    }

    /// 给模型的工具名对照(客户端提示词里按原名提到工具)。没有改名返回空串。
    pub(crate) fn correspondence_note(&self) -> String {
        let pairs: Vec<String> = self
            .entries
            .iter()
            .filter(|e| !matches!(e.route, Route::Same))
            .map(|e| format!("{} -> {}", e.client_name, e.wire_name))
            .collect();
        if pairs.is_empty() {
            return String::new();
        }
        format!(
            "Tool names: the instructions above may refer to tools by other names. Use these tools instead: {}.",
            pairs.join(", ")
        )
    }
}

// ── 规则里共用的小工具 ──────────────────────────────────────────────────────────────

/// 候选键里第一个有值(非 null)的。
pub(crate) fn get<'a>(a: &'a Args, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| a.get(*k).filter(|v| !v.is_null()))
}

/// 有值才写。
pub(crate) fn put(out: &mut Args, key: &str, v: Option<&Value>) {
    if let Some(v) = v {
        out.insert(key.to_string(), v.clone());
    }
}

pub(crate) fn get_str<'a>(a: &'a Args, keys: &[&str]) -> Option<&'a str> {
    get(a, keys).and_then(Value::as_str)
}

/// 整数(客户端定义成 number 的也可能传 1.0)。
pub(crate) fn get_i64(a: &Args, key: &str) -> Option<i64> {
    let v = a.get(key)?;
    v.as_i64().or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
}

pub(crate) fn get_bool(a: &Args, key: &str) -> Option<bool> {
    a.get(key).and_then(Value::as_bool)
}

#[cfg(test)]
mod tests {
    use super::super::template::test_template;
    use super::*;
    use serde_json::json;

    fn tool(name: &str, props: &[&str]) -> Value {
        let p: Map<String, Value> = props.iter().map(|k| (k.to_string(), json!({"type": "string"}))).collect();
        json!({"name": name, "description": format!("{name} desc"), "input_schema": {"type": "object", "properties": p}})
    }

    #[test]
    fn cc_surface_keeps_everything_and_rejects_foreign_tools() {
        let tpl = test_template();
        let tools = vec![tool("Read", &["file_path"]), tool("mcp__x__y", &[]), json!({"type": "web_search_20250305", "name": "web_search"})];
        let b = build(Mode::CcSurface { strict: true }, &tools, &tpl).unwrap();
        assert_eq!(b.stats, Stats { native: 0, renamed: 0, same: 3 });
        assert_eq!(b.wire_tools, tools, "原样发,定义也是客户端自己的");
        assert!(!b.map.has_renames());
        assert!(build(Mode::CcSurface { strict: true }, &[tool("read_file", &[])], &tpl).is_err());
        // 已认出是 CC:模板快照里没有的 CC 原生工具照样原样发,但小写的别家工具不放。
        assert!(build(Mode::CcSurface { strict: false }, &[tool("ListMcpResourcesTool", &[])], &tpl).is_ok());
        assert!(build(Mode::CcSurface { strict: false }, &[tool("read", &[])], &tpl).is_err());
    }

    #[test]
    fn natives_come_first_in_template_order_then_mcp_in_client_order() {
        let tpl = test_template();
        let tools = vec![tool("todowrite", &["todos"]), tool("write", &["path", "content"]), tool("read", &["path"])];
        let b = build(Mode::Table(&OPENCODE), &tools, &tpl).unwrap();
        let names: Vec<&str> = b.wire_tools.iter().map(tool_name).collect();
        assert_eq!(names, ["Read", "Write", "mcp__workspace__todowrite"]);
        assert_eq!(b.wire_tools[0], *tpl.tool("Read").unwrap(), "原生工具用 CC 原版定义");
        assert_eq!(b.stats, Stats { native: 2, renamed: 1, same: 0 });
    }

    #[test]
    fn second_tool_for_the_same_cc_slot_goes_to_mcp() {
        let tpl = test_template();
        let tools = vec![tool("bash", &["command"]), tool("shell", &["command"])];
        let b = build(Mode::Table(&OPENCODE), &tools, &tpl).unwrap();
        assert_eq!(b.map.wire_name("bash"), Some("Bash"));
        assert_eq!(b.map.wire_name("shell"), Some("mcp__workspace__shell"));
    }

    #[test]
    fn long_or_odd_names_are_made_valid_and_stay_reversible() {
        let tpl = test_template();
        let long = "x".repeat(80);
        let tools = vec![tool(&long, &[]), tool("a.b c", &[])];
        let b = build(Mode::Table(&OPENCODE), &tools, &tpl).unwrap();
        let w_long = b.map.wire_name(&long).unwrap().to_string();
        assert_eq!(w_long.len(), MAX_TOOL_NAME);
        assert_eq!(b.map.wire_name("a.b c"), Some("mcp__workspace__a_b_c"));
        let (back, _) = b.map.backward(&w_long, &json!({})).unwrap();
        assert_eq!(back, long);
        // 确定性:再算一遍名字一样。
        let b2 = build(Mode::Table(&OPENCODE), &tools, &tpl).unwrap();
        assert_eq!(b2.map.wire_name(&long).unwrap(), w_long);
    }

    #[test]
    fn renamed_tool_never_takes_a_name_kept_as_is_in_either_order() {
        let tpl = test_template();
        for tools in [vec![tool("foo", &[]), tool("mcp__workspace__foo", &[])], vec![tool("mcp__workspace__foo", &[]), tool("foo", &[])]] {
            let b = build(Mode::Table(&OPENCODE), &tools, &tpl).unwrap();
            assert_eq!(b.map.wire_name("mcp__workspace__foo"), Some("mcp__workspace__foo"));
            let foo = b.map.wire_name("foo").unwrap();
            assert!(foo.starts_with("mcp__workspace__foo_") && foo.len() > "mcp__workspace__foo".len(), "{foo}");
        }
    }

    #[test]
    fn template_tool_with_unregistered_params_falls_back_to_mcp() {
        let mut v: Value = serde_json::from_str(&template_json_with_read_props(&["file_path", "brand_new"])).unwrap();
        v["tools"][0]["name"] = json!("Read");
        let tpl = CcTemplate::parse(&v.to_string()).unwrap();
        let b = build(Mode::Table(&OPENCODE), &[tool("read", &["path"])], &tpl).unwrap();
        assert_eq!(b.map.wire_name("read"), Some("mcp__workspace__read"), "CC Read 多了没登记的参数,不当原生用");
    }

    fn template_json_with_read_props(props: &[&str]) -> String {
        let p: Map<String, Value> = props.iter().map(|k| (k.to_string(), json!({}))).collect();
        json!({"_version": "9", "agent_identity": "x",
               "tools": [{"name": "Read", "description": "d", "input_schema": {"properties": p}}],
               "body_field_order": ["model", "messages"]}).to_string()
    }

    #[test]
    fn duplicate_client_tools_keep_the_first_definition() {
        let tpl = test_template();
        let tools = [tool("read", &["path"]), tool("read", &["filePath"]), tool("Read", &[]), tool("Read", &[])];
        let b = build(Mode::Table(&OPENCODE), &tools[..2], &tpl).unwrap();
        assert_eq!(b.wire_tools.len(), 1);
        assert_eq!(b.map.backward("Read", &json!({"file_path": "/a"})).unwrap().1, json!({"path": "/a"}), "按第一个定义");
        let b = build(Mode::CcSurface { strict: true }, &tools[2..], &tpl).unwrap();
        assert_eq!(b.wire_tools.len(), 1);
    }

    #[test]
    fn note_lists_only_renamed_tools() {
        let tpl = test_template();
        let b = build(Mode::Table(&OPENCODE), &[tool("read", &["path"]), tool("mcp__a__b", &[])], &tpl).unwrap();
        assert_eq!(
            b.map.correspondence_note(),
            "Tool names: the instructions above may refer to tools by other names. Use these tools instead: read -> Read."
        );
    }
}
