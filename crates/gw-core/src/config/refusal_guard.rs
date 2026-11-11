//! 拒答记忆的配置(`system.yaml` 的 `refusal_guard` 段 / 热设置 `refusal_guard_*`)。
//!
//! 只放配置类型、解析与写侧校验(admin 与 worker 共用同一套规则)。判定逻辑与进程级
//! 状态在编排层 `gw-app/src/worker/gates/refusal_guard.rs`。模式枚举复用批量拦截的
//! [`BatchGuardMode`](off / observe / enforce 三档语义完全一样)。

use serde::{Deserialize, Serialize};

pub use super::batch_guard::BatchGuardMode as RefusalGuardMode;
use super::batch_guard::{mode_str, parse_mode};

/// `system.yaml` 顶层 `refusal_guard` 段。结构体级 `#[serde(default)]` + 不拒未知字段:
/// 少写/多写一个键都不能让共用 yaml 的容器起不来(同 [`super::batch_guard::BatchGuardConfig`])。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RefusalGuardConfig {
    pub mode: RefusalGuardMode,
    /// 同一条请求被拒后,还允许再发上游几次(全部钉在第一次被拒的那个号上)。
    /// 0 = 被拒一次之后就在本地回拒答。
    pub retries: u32,
    /// 记录保留多久(秒),从最后一次被拒起算。到期自动放行。
    pub ttl_secs: u64,
}

pub const DEFAULT_RETRIES: u32 = 1;
pub const DEFAULT_TTL_SECS: u64 = 2 * 3600;

const RETRIES_MAX: u64 = 5;
const TTL_MIN: u64 = 300;
const TTL_MAX: u64 = 86_400;

impl Default for RefusalGuardConfig {
    fn default() -> Self {
        Self { mode: RefusalGuardMode::default(), retries: DEFAULT_RETRIES, ttl_secs: DEFAULT_TTL_SECS }
    }
}

/// 整份配置校验(worker 热应用前)。规则与 [`validate_setting`] 同源。
pub fn validate(cfg: &RefusalGuardConfig) -> Result<(), String> {
    check_range("refusal_guard_retries", u64::from(cfg.retries), 0, RETRIES_MAX)?;
    check_range("refusal_guard_ttl_secs", cfg.ttl_secs, TTL_MIN, TTL_MAX)?;
    Ok(())
}

fn check_range(key: &str, v: u64, lo: u64, hi: u64) -> Result<(), String> {
    if (lo..=hi).contains(&v) {
        Ok(())
    } else {
        Err(format!("{key} 须在 {lo}..={hi} 之间,收到 {v}"))
    }
}

/// admin `PUT /settings` 写侧校验:`key` 是 `refusal_guard_*` 之一时返回归一化后的值,
/// 非法则给出可读原因。不是本模块的键返回 `None`(调用方走别的分支)。
pub fn validate_setting(key: &str, v: &serde_json::Value) -> Option<Result<serde_json::Value, String>> {
    let num = |lo: u64, hi: u64| -> Result<serde_json::Value, String> {
        let n = v.as_u64().ok_or_else(|| format!("{key} 须为非负整数"))?;
        check_range(key, n, lo, hi).map(|_| serde_json::json!(n))
    };
    Some(match key {
        "refusal_guard_mode" => match v.as_str() {
            Some(s) => parse_mode(s)
                .map(|m| serde_json::json!(mode_str(m)))
                .map_err(|e| e.replace("batch_guard_mode", "refusal_guard_mode")),
            None => Err("refusal_guard_mode 须为字符串".into()),
        },
        "refusal_guard_retries" => num(0, RETRIES_MAX),
        "refusal_guard_ttl_secs" => num(TTL_MIN, TTL_MAX),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validate_setting_normalizes_and_rejects() {
        assert_eq!(validate_setting("refusal_guard_mode", &json!(" Enforce ")).unwrap().unwrap(), json!("enforce"));
        let e = validate_setting("refusal_guard_mode", &json!("block")).unwrap().unwrap_err();
        assert!(e.contains("refusal_guard_mode"), "报错点名本模块的键: {e}");
        assert_eq!(validate_setting("refusal_guard_retries", &json!(0)).unwrap().unwrap(), json!(0));
        assert!(validate_setting("refusal_guard_retries", &json!(6)).unwrap().is_err());
        assert!(validate_setting("refusal_guard_retries", &json!(-1)).unwrap().is_err());
        assert!(validate_setting("refusal_guard_ttl_secs", &json!(60)).unwrap().is_err(), "太短等于没记");
        assert_eq!(validate_setting("refusal_guard_ttl_secs", &json!(7200)).unwrap().unwrap(), json!(7200));
        assert!(validate_setting("batch_guard_mode", &json!("off")).is_none(), "不是本模块的键");
    }

    #[test]
    fn config_tolerates_partial_yaml_and_defaults_to_observe() {
        let c: RefusalGuardConfig = serde_json::from_value(json!({"mode": "ENFORCE"})).unwrap();
        assert_eq!(c.mode, RefusalGuardMode::Enforce);
        assert_eq!(c.retries, DEFAULT_RETRIES);
        assert_eq!(c.ttl_secs, DEFAULT_TTL_SECS);
        assert_eq!(RefusalGuardConfig::default().mode, RefusalGuardMode::Observe, "代码默认只观察");
        assert!(validate(&RefusalGuardConfig::default()).is_ok());
        assert!(validate(&RefusalGuardConfig { ttl_secs: 10, ..Default::default() }).is_err());
    }
}
