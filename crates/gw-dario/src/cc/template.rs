//! dario 的 CC 模板数据(`cc-template-data.json`):版本、身份行、CC 工具定义、请求体字段顺序。
//!
//! 这份文件由 dario 上游的漂移机器人跟着 Claude Code 新版本更新,caio 只读不写:翻译用到的
//! 「CC 长什么样」全部从这里取,dario 升级时自动跟上,不在 caio 里硬编码第二份。
//!
//! 每次请求按路径 stat 一次,mtime/大小没变就用缓存;变了重新解析。读不到或解析失败返回
//! `None`(调用方按 `off` 处理),同一份坏文件只报一次错。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde_json::Value;

#[derive(Debug)]
pub(crate) struct CcTemplate {
    /// CC 版本号(`_version`,如 `2.1.312`),写进计费头占位。
    pub version: String,
    /// system 第二块的身份行(`agent_identity`)。
    pub agent_identity: String,
    /// CC 原生工具的完整定义,保持模板里的顺序(真 CC 的 tools 数组就是这个顺序)。
    tools: Vec<Value>,
    rank: HashMap<String, usize>,
    /// 请求体顶层字段顺序(`body_field_order`)。
    pub body_field_order: Vec<String>,
}

impl CcTemplate {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("不是合法 JSON: {e}"))?;
        let s = |k: &str| -> Result<String, String> {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("缺少字段 {k}"))
        };
        let version = s("_version")?;
        let agent_identity = s("agent_identity")?;
        let tools: Vec<Value> = v
            .get("tools")
            .and_then(Value::as_array)
            .ok_or("缺少字段 tools")?
            .iter()
            .filter(|t| t.get("name").and_then(Value::as_str).is_some())
            .cloned()
            .collect();
        let rank = tools
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.get("name").and_then(Value::as_str).map(|n| (n.to_string(), i)))
            .collect();
        let body_field_order: Vec<String> = v
            .get("body_field_order")
            .and_then(Value::as_array)
            .ok_or("缺少字段 body_field_order")?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if !body_field_order.iter().any(|k| k == "model") || !body_field_order.iter().any(|k| k == "messages") {
            return Err("body_field_order 里没有 model / messages,文件不对".into());
        }
        Ok(Self { version, agent_identity, tools, rank, body_field_order })
    }

    /// CC 原生工具的完整定义(描述与参数结构都是 CC 原版)。
    pub(crate) fn tool(&self, name: &str) -> Option<&Value> {
        self.rank.get(name).map(|&i| &self.tools[i])
    }

    /// 该工具在 CC tools 数组里的位置(决定出站顺序)。
    pub(crate) fn tool_rank(&self, name: &str) -> Option<usize> {
        self.rank.get(name).copied()
    }
}

struct Cached {
    path: String,
    stamp: Option<(SystemTime, u64)>,
    tpl: Option<Arc<CcTemplate>>,
}

static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

/// 按路径取模板(带缓存)。空路径 / 读不到 / 解析失败 → `None`。
pub(crate) fn load(path: &str) -> Option<Arc<CcTemplate>> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let stamp = std::fs::metadata(path).ok().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
    let mut guard = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(c) = guard.as_ref() {
        if c.path == path && c.stamp == stamp {
            return c.tpl.clone();
        }
    }
    let tpl = match stamp {
        None => {
            tracing::error!(path, "dario CC 模板文件读不到,翻译成 CC 报文不生效(按 off 处理)");
            None
        }
        Some(_) => match std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|t| CcTemplate::parse(&t)) {
            Ok(t) => {
                tracing::info!(path, version = %t.version, tools = t.tools.len(), "dario CC 模板已加载");
                Some(Arc::new(t))
            }
            Err(e) => {
                tracing::error!(path, error = %e, "dario CC 模板解析失败,翻译成 CC 报文不生效(按 off 处理)");
                None
            }
        },
    };
    *guard = Some(Cached { path: path.to_string(), stamp, tpl: tpl.clone() });
    tpl
}

/// 测试用的小模板(真实文件的结构,工具定义只保留测试要用的几个)。
#[cfg(test)]
pub(crate) fn test_template() -> CcTemplate {
    let tool = |name: &str, props: Value| serde_json::json!({
        "name": name, "description": format!("CC 原版 {name} 描述"),
        "input_schema": {"type": "object", "properties": props}
    });
    let text = serde_json::json!({
        "_version": "2.1.312",
        "agent_identity": "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
        "tools": [
            tool("Agent", serde_json::json!({"description": {}, "prompt": {}, "subagent_type": {}, "run_in_background": {}})),
            tool("AskUserQuestion", serde_json::json!({"questions": {}})),
            tool("Bash", serde_json::json!({"command": {}, "timeout": {}, "description": {}, "run_in_background": {}})),
            tool("Edit", serde_json::json!({"file_path": {}, "old_string": {}, "new_string": {}, "replace_all": {}})),
            tool("Glob", serde_json::json!({"pattern": {}, "path": {}})),
            tool("Grep", serde_json::json!({"pattern": {}, "path": {}, "glob": {}, "-i": {}, "head_limit": {}})),
            tool("PowerShell", serde_json::json!({"command": {}, "timeout": {}, "description": {}, "run_in_background": {}})),
            tool("Read", serde_json::json!({"file_path": {}, "offset": {}, "limit": {}})),
            tool("Skill", serde_json::json!({"skill": {}, "args": {}})),
            tool("WebFetch", serde_json::json!({"url": {}, "prompt": {}})),
            tool("WebSearch", serde_json::json!({"query": {}})),
            tool("Write", serde_json::json!({"file_path": {}, "content": {}})),
        ],
        "body_field_order": ["model", "messages", "system", "tools", "metadata", "max_tokens",
                             "thinking", "context_management", "output_config", "stream"],
    })
    .to_string();
    CcTemplate::parse(&text).expect("测试模板")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_files_missing_required_fields() {
        assert!(CcTemplate::parse("{}").is_err());
        assert!(CcTemplate::parse(r#"{"_version":"1","agent_identity":"x","tools":[],"body_field_order":["stream"]}"#).is_err());
        let t = test_template();
        assert_eq!(t.version, "2.1.312");
        assert_eq!(t.tool_rank("Agent"), Some(0));
        assert!(t.tool("Read").is_some());
        assert!(t.tool("TodoWrite").is_none());
    }

    #[test]
    fn load_returns_none_for_blank_or_missing_path() {
        assert!(load("").is_none());
        assert!(load("/nonexistent/cc-template-data.json").is_none());
    }
}
