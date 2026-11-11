//! dario 线「把非 Claude Code 客户端翻译成 Claude Code 报文」的开关(热设置
//! `dario_cc_translate`)。只放取值解析与写侧校验,admin 写侧与 worker 读侧共用同一套规则;
//! 翻译本身在 `gw-dario::cc`。

/// 设置键名。
pub const SETTING_KEY: &str = "dario_cc_translate";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DarioCcMode {
    /// 与改造前逐字节一致(默认)。
    #[default]
    Off,
    /// 照常转发,只记录「会怎么翻译、会不会被拒」。
    Observe,
    /// 真 Claude Code 原样交给 dario;其余认得出的客户端翻译成 Claude Code 报文;认不出的本地拒绝。
    On,
}

impl DarioCcMode {
    /// 大小写与前后空白不敏感;空串按 `off`。
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "off" => Ok(Self::Off),
            "observe" => Ok(Self::Observe),
            "on" => Ok(Self::On),
            other => Err(format!("{SETTING_KEY} 只能是 off / observe / on,收到 {other:?}")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Observe => "observe",
            Self::On => "on",
        }
    }
}

/// 写侧校验:不是本键返回 `None`;是本键返回规范化后的值或错误原因。
pub fn validate_setting(key: &str, v: &serde_json::Value) -> Option<Result<serde_json::Value, String>> {
    if key != SETTING_KEY {
        return None;
    }
    Some(match v.as_str() {
        Some(s) => DarioCcMode::parse(s).map(|m| serde_json::json!(m.as_str())),
        None => Err(format!("{SETTING_KEY} 须为字符串")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_is_lenient_on_case_and_blank() {
        assert_eq!(DarioCcMode::parse(" On ").unwrap(), DarioCcMode::On);
        assert_eq!(DarioCcMode::parse("").unwrap(), DarioCcMode::Off);
        assert_eq!(DarioCcMode::parse("OBSERVE").unwrap(), DarioCcMode::Observe);
        assert!(DarioCcMode::parse("enforce").is_err());
    }

    #[test]
    fn validate_setting_normalizes_and_ignores_other_keys() {
        assert_eq!(validate_setting(SETTING_KEY, &json!(" On ")).unwrap().unwrap(), json!("on"));
        assert!(validate_setting(SETTING_KEY, &json!(1)).unwrap().is_err());
        assert!(validate_setting("cursor_tool_guard", &json!("on")).is_none());
    }
}
