//! CC 自动模式安全分类器的配置(`system.yaml` / 热设置 `auto_mode_classifier*`)。
//!
//! 只放配置类型、解析与校验。判定逻辑与进程级热应用状态在编排层
//! `gw-app/src/worker/gates/automode/`(2026-10-28 从 gw-core 迁出:gw-core 只放无状态通用件)。

use serde::{Deserialize, Serialize};

/// 怎么处理分类器请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoModeMode {
    /// 不动:原样走上游(= 本功能上线前的行为)。
    Off,
    /// 改写成便宜模型后走上游。
    Haiku,
    /// 本地直接回放行,不碰上游(**默认**)。
    #[default]
    Allow,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// 刻意不加 deny_unknown_fields:全栈共用一份 system.yaml,以后加字段不能让旧镜像起不来。
pub struct AutoModeConfig {
    pub mode: AutoModeMode,
    /// `haiku` 模式改写成哪个模型。各线路的 haiku 名字都认(kiro 的子串兜底会落到 4.5)。
    pub model: String,
}

impl Default for AutoModeConfig {
    fn default() -> Self {
        // 默认放行:分类器请求的输入是整段 transcript,改 haiku 又常被 200k 窗口挡回去,
        // 本地直答才是真正稳定省钱的那条。要恢复旧行为,面板上切 `off` 即可(30 秒生效)。
        Self { mode: AutoModeMode::Allow, model: DEFAULT_HAIKU_MODEL.to_string() }
    }
}

pub const DEFAULT_HAIKU_MODEL: &str = "claude-haiku-4-5";

/// 解析 settings 里的 mode 字符串(写侧校验用,错了要当场说清可选值)。
pub fn parse_mode(s: &str) -> Result<AutoModeMode, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(AutoModeMode::Off),
        "haiku" => Ok(AutoModeMode::Haiku),
        "allow" => Ok(AutoModeMode::Allow),
        other => Err(format!("auto_mode_classifier 只能是 off / haiku / allow,收到 {other:?}")),
    }
}

pub fn mode_str(m: AutoModeMode) -> &'static str {
    match m {
        AutoModeMode::Off => "off",
        AutoModeMode::Haiku => "haiku",
        AutoModeMode::Allow => "allow",
    }
}

pub fn validate(cfg: &AutoModeConfig) -> Result<(), String> {
    if cfg.mode == AutoModeMode::Haiku && cfg.model.trim().is_empty() {
        return Err("auto_mode_classifier=haiku 必须给 auto_mode_classifier_model".into());
    }
    Ok(())
}
