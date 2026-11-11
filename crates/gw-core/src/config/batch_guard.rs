//! 批量任务拦截的配置(`system.yaml` 的 `batch_guard` 段 / 热设置 `batch_guard_*`)。
//!
//! 只放配置类型、解析与写侧校验(admin 与 worker 共用同一套规则)。判定逻辑与进程级
//! 计数状态在编排层 `gw-app/src/worker/gates/batch_guard.rs`(2026-10-28 从 gw-core 迁出)。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BatchGuardMode {
    Off,
    #[default]
    Observe,
    Enforce,
}

impl<'de> Deserialize<'de> for BatchGuardMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_mode(&s).map_err(serde::de::Error::custom)
    }
}

pub fn parse_mode(s: &str) -> Result<BatchGuardMode, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(BatchGuardMode::Off),
        "observe" => Ok(BatchGuardMode::Observe),
        "enforce" => Ok(BatchGuardMode::Enforce),
        other => Err(format!("batch_guard_mode 只能是 off / observe / enforce,收到 {other:?}")),
    }
}

pub fn mode_str(m: BatchGuardMode) -> &'static str {
    match m {
        BatchGuardMode::Off => "off",
        BatchGuardMode::Observe => "observe",
        BatchGuardMode::Enforce => "enforce",
    }
}

/// `system.yaml` 顶层 `batch_guard` 段。结构体级 `#[serde(default)]` + 不拒未知字段:
/// 少写/多写一个键都不能让共用 yaml 的容器起不来(同 [`super::automode::AutoModeConfig`])。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BatchGuardConfig {
    pub mode: BatchGuardMode,
    /// 滑动窗口(秒)。
    pub window_secs: u64,
    /// 窗口内同一模板的脚本形态请求数达到它(且占比达标)即判定。
    pub threshold: u32,
    /// 判定后持续多久(秒)。
    pub block_secs: u64,
    /// 指纹取模板开头多少个字符。
    pub prefix_chars: usize,
    /// 命中即判定(模板开头 `starts_with`)。
    pub block_prefixes: Vec<String>,
    /// 命中即放行。
    pub allow_prefixes: Vec<String>,
}

pub const DEFAULT_WINDOW_SECS: u64 = 600;
pub const DEFAULT_THRESHOLD: u32 = 40;
pub const DEFAULT_BLOCK_SECS: u64 = 6 * 3600;
pub const DEFAULT_PREFIX_CHARS: usize = 80;

impl Default for BatchGuardConfig {
    fn default() -> Self {
        Self {
            mode: BatchGuardMode::default(),
            window_secs: DEFAULT_WINDOW_SECS,
            threshold: DEFAULT_THRESHOLD,
            block_secs: DEFAULT_BLOCK_SECS,
            prefix_chars: DEFAULT_PREFIX_CHARS,
            block_prefixes: Vec::new(),
            allow_prefixes: Vec::new(),
        }
    }
}

const MAX_PREFIXES: usize = 64;
const MAX_PREFIX_LEN: usize = 500;

/// 整份配置校验(worker 热应用前)。规则与 [`validate_setting`] 同源。
pub fn validate(cfg: &BatchGuardConfig) -> Result<(), String> {
    check_range("batch_guard_window_secs", cfg.window_secs, 60, 86_400)?;
    check_range("batch_guard_threshold", u64::from(cfg.threshold), 2, 100_000)?;
    check_range("batch_guard_block_secs", cfg.block_secs, 60, 7 * 86_400)?;
    check_range("batch_guard_prefix_chars", cfg.prefix_chars as u64, 16, 1000)?;
    check_prefixes("batch_guard_block_prefixes", &cfg.block_prefixes)?;
    check_prefixes("batch_guard_allow_prefixes", &cfg.allow_prefixes)?;
    Ok(())
}

fn check_range(key: &str, v: u64, lo: u64, hi: u64) -> Result<(), String> {
    if (lo..=hi).contains(&v) {
        Ok(())
    } else {
        Err(format!("{key} 须在 {lo}..={hi} 之间,收到 {v}"))
    }
}

fn check_prefixes(key: &str, list: &[String]) -> Result<(), String> {
    if list.len() > MAX_PREFIXES {
        return Err(format!("{key} 最多 {MAX_PREFIXES} 条,收到 {}", list.len()));
    }
    for p in list {
        let n = p.trim().chars().count();
        if n == 0 {
            return Err(format!("{key} 不能含空串(空前缀会匹配所有请求)"));
        }
        if n > MAX_PREFIX_LEN {
            return Err(format!("{key} 单条最长 {MAX_PREFIX_LEN} 字符,收到 {n}"));
        }
    }
    Ok(())
}

/// admin `PUT /settings` 写侧校验:`key` 是 `batch_guard_*` 之一时返回归一化后的值,
/// 非法则给出可读原因。不是本模块的键返回 `None`(调用方走别的分支)。
///
/// 写侧与 worker 读侧用同一套规则 —— 否则「存进去了、worker 拒绝并沿用上一份」,
/// 面板却显示已保存。
pub fn validate_setting(key: &str, v: &serde_json::Value) -> Option<Result<serde_json::Value, String>> {
    let num = |lo: u64, hi: u64| -> Result<serde_json::Value, String> {
        let n = v.as_u64().ok_or_else(|| format!("{key} 须为非负整数"))?;
        check_range(key, n, lo, hi).map(|_| serde_json::json!(n))
    };
    let list = || -> Result<serde_json::Value, String> {
        let arr = v.as_array().ok_or_else(|| format!("{key} 须为字符串数组"))?;
        let mut out: Vec<String> = Vec::with_capacity(arr.len());
        for x in arr {
            let s = x.as_str().ok_or_else(|| format!("{key} 须为字符串数组"))?;
            let s = s.trim().to_string();
            if !out.contains(&s) {
                out.push(s);
            }
        }
        check_prefixes(key, &out).map(|_| serde_json::json!(out))
    };
    Some(match key {
        "batch_guard_mode" => match v.as_str() {
            Some(s) => parse_mode(s).map(|m| serde_json::json!(mode_str(m))),
            None => Err("batch_guard_mode 须为字符串".into()),
        },
        "batch_guard_window_secs" => num(60, 86_400),
        "batch_guard_threshold" => num(2, 100_000),
        "batch_guard_block_secs" => num(60, 7 * 86_400),
        "batch_guard_prefix_chars" => num(16, 1000),
        "batch_guard_block_prefixes" => list(),
        "batch_guard_allow_prefixes" => list(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validate_setting_normalizes_and_rejects() {
        assert_eq!(validate_setting("batch_guard_mode", &json!(" Enforce ")).unwrap().unwrap(), json!("enforce"));
        assert!(validate_setting("batch_guard_mode", &json!("block")).unwrap().is_err());
        assert!(validate_setting("batch_guard_threshold", &json!(1)).unwrap().is_err());
        assert_eq!(validate_setting("batch_guard_threshold", &json!(40)).unwrap().unwrap(), json!(40));
        assert!(validate_setting("batch_guard_window_secs", &json!(-5)).unwrap().is_err());
        assert_eq!(
            validate_setting("batch_guard_block_prefixes", &json!([" a ", "a", "b"])).unwrap().unwrap(),
            json!(["a", "b"]),
            "去空白、去重"
        );
        assert!(validate_setting("batch_guard_block_prefixes", &json!(["  "])).unwrap().is_err(), "空前缀会拦所有请求");
        assert!(validate_setting("cache_floor_ratio", &json!(0.5)).is_none(), "不是本模块的键");
    }

    #[test]
    fn config_tolerates_partial_yaml_and_case() {
        let c: BatchGuardConfig = serde_json::from_value(json!({"mode": "ENFORCE"})).unwrap();
        assert_eq!(c.mode, BatchGuardMode::Enforce);
        assert_eq!(c.threshold, DEFAULT_THRESHOLD);
        assert_eq!(BatchGuardConfig::default().mode, BatchGuardMode::Observe, "代码默认只观察");
    }
}
