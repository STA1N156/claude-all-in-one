//! 号池熔断的配置(`system.yaml` 的 `pool_breaker` 段 / 热设置 `pool_breaker_*`)。
//!
//! 只放配置类型、解析与写侧校验(admin 与 worker 共用同一套规则)。计数与熔断状态在
//! 编排层 `gw-app/src/worker/pool_breaker.rs`。模式枚举复用批量拦截的
//! [`BatchGuardMode`](off / observe / enforce 三档语义一样),但**默认 enforce**:
//! 用户 2026-11-09 明确要求上线即熔断,40 天追踪回放(30 分钟窗口触发 15 次,都落在
//! 已知封号潮上)代替了观察期。

use serde::{Deserialize, Serialize};

pub use super::batch_guard::BatchGuardMode as PoolBreakerMode;
use super::batch_guard::{mode_str, parse_mode};

/// `system.yaml` 顶层 `pool_breaker` 段。结构体级 `#[serde(default)]` + 不拒未知字段:
/// 少写/多写一个键都不能让共用 yaml 的容器起不来(同 [`super::batch_guard::BatchGuardConfig`])。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolBreakerConfig {
    pub mode: PoolBreakerMode,
    /// 滑动窗口(秒)。
    pub window_secs: u64,
    /// 窗口内出事(首次被封 / 掉 FREE)的**不同账号**数达到它即熔断。
    pub threshold: u32,
}

pub const DEFAULT_WINDOW_SECS: u64 = 30 * 60;
/// 「超过 2 个」= 第 3 个号出事时熔断。
pub const DEFAULT_THRESHOLD: u32 = 3;

const WINDOW_MIN: u64 = 300;
const WINDOW_MAX: u64 = 86_400;
const THRESHOLD_MIN: u64 = 2;
const THRESHOLD_MAX: u64 = 100;

impl Default for PoolBreakerConfig {
    fn default() -> Self {
        Self {
            mode: PoolBreakerMode::Enforce,
            window_secs: DEFAULT_WINDOW_SECS,
            threshold: DEFAULT_THRESHOLD,
        }
    }
}

/// 整份配置校验(worker 热应用前)。规则与 [`validate_setting`] 同源。
pub fn validate(cfg: &PoolBreakerConfig) -> Result<(), String> {
    check_range("pool_breaker_window_secs", cfg.window_secs, WINDOW_MIN, WINDOW_MAX)?;
    check_range("pool_breaker_threshold", u64::from(cfg.threshold), THRESHOLD_MIN, THRESHOLD_MAX)?;
    Ok(())
}

fn check_range(key: &str, v: u64, lo: u64, hi: u64) -> Result<(), String> {
    if (lo..=hi).contains(&v) {
        Ok(())
    } else {
        Err(format!("{key} 须在 {lo}..={hi} 之间,收到 {v}"))
    }
}

/// admin `PUT /settings` 写侧校验:`key` 是 `pool_breaker_*` 之一时返回归一化后的值,
/// 非法则给出可读原因。不是本模块的键返回 `None`(调用方走别的分支)。
pub fn validate_setting(key: &str, v: &serde_json::Value) -> Option<Result<serde_json::Value, String>> {
    let num = |lo: u64, hi: u64| -> Result<serde_json::Value, String> {
        let n = v.as_u64().ok_or_else(|| format!("{key} 须为非负整数"))?;
        check_range(key, n, lo, hi).map(|_| serde_json::json!(n))
    };
    Some(match key {
        "pool_breaker_mode" => match v.as_str() {
            Some(s) => parse_mode(s)
                .map(|m| serde_json::json!(mode_str(m)))
                .map_err(|e| e.replace("batch_guard_mode", "pool_breaker_mode")),
            None => Err("pool_breaker_mode 须为字符串".into()),
        },
        "pool_breaker_window_secs" => num(WINDOW_MIN, WINDOW_MAX),
        "pool_breaker_threshold" => num(THRESHOLD_MIN, THRESHOLD_MAX),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validate_setting_normalizes_and_rejects() {
        assert_eq!(validate_setting("pool_breaker_mode", &json!(" Observe ")).unwrap().unwrap(), json!("observe"));
        let err = validate_setting("pool_breaker_mode", &json!("block")).unwrap().unwrap_err();
        assert!(err.contains("pool_breaker_mode"), "{err}");
        assert!(validate_setting("pool_breaker_threshold", &json!(1)).unwrap().is_err(), "阈值 1 等于单号出事就停池");
        assert_eq!(validate_setting("pool_breaker_threshold", &json!(3)).unwrap().unwrap(), json!(3));
        assert!(validate_setting("pool_breaker_window_secs", &json!(60)).unwrap().is_err());
        assert!(validate_setting("pool_breaker_window_secs", &json!("1800")).unwrap().is_err());
        assert!(validate_setting("batch_guard_mode", &json!("off")).is_none(), "不是本模块的键");
    }

    #[test]
    fn defaults_are_enforce_30min_3() {
        let c = PoolBreakerConfig::default();
        assert_eq!(c.mode, PoolBreakerMode::Enforce, "用户要求上线即熔断");
        assert_eq!((c.window_secs, c.threshold), (1800, 3));
        let partial: PoolBreakerConfig = serde_json::from_value(json!({"threshold": 5})).unwrap();
        assert_eq!(partial.mode, PoolBreakerMode::Enforce);
        assert_eq!(partial.window_secs, 1800);
        assert!(validate(&c).is_ok());
    }
}
