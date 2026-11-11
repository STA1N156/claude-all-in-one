//! 与 Kiro-Go 一致：显式参数优先，其次模型后缀，普通模型名默认关闭。
//! 关闭时省略上游思考强度字段；是否支持关闭由上游模型决定。

use crate::anthropic_types::{MessagesRequest, OutputConfig, Thinking};

fn budget_to_effort(budget: i32) -> &'static str {
    match budget {
        b if b <= 0 => crate::anthropic_types::DEFAULT_EFFORT,
        b if b <= 2048 => "low",
        b if b <= 8192 => "medium",
        _ => "high",
    }
}

/// 只发送该模型支持的档位；没有 effort schema 时省略字段。
pub fn effective_effort(req: &MessagesRequest) -> Option<&'static str> {
    let thinking = req.thinking.as_ref()?;
    if !thinking.is_enabled() {
        return None;
    }
    let raw = req.output_config.as_ref().and_then(|c| c.effort.as_deref());
    let want = if raw.is_some() {
        let (effort, invalid) = crate::anthropic_types::normalize_effort(raw);
        if invalid {
            None
        } else {
            Some(effort)
        }
    } else if thinking.thinking_type == "enabled" {
        Some(budget_to_effort(thinking.budget_tokens))
    } else {
        Some(crate::anthropic_types::DEFAULT_EFFORT)
    };
    crate::converter::clamp_effort_for_model(&req.model, want)
}

/// 根据模型的 schema 选择 output_config 或 reasoning，保持原来的上游报文形态。
pub fn additional_model_request_fields(req: &MessagesRequest) -> Option<serde_json::Value> {
    let effort = effective_effort(req)?;
    match crate::converter::effort_schema_for(&req.model) {
        crate::converter::EffortSchema::Reasoning => {
            Some(serde_json::json!({ "reasoning": { "effort": effort } }))
        }
        crate::converter::EffortSchema::OutputConfig => {
            Some(serde_json::json!({ "output_config": { "effort": effort } }))
        }
    }
}

/// 保留旧协议兼容开关：只认结构化强度的模型仍发送结构化字段。
pub fn wire_additional_model_request_fields(
    req: &MessagesRequest,
    legacy: bool,
) -> Option<serde_json::Value> {
    if legacy && !crate::converter::requires_structured_effort(&req.model) {
        return None;
    }
    additional_model_request_fields(req)
}

pub fn override_thinking_from_model_name(req: &mut MessagesRequest) {
    // 不再抬高客户端预算，也不再用模型后缀覆盖显式开关。
    if let Some(thinking) = req.thinking.as_ref() {
        if thinking.thinking_type == "adaptive"
            && req.output_config.as_ref().and_then(|c| c.effort.as_ref()).is_none()
        {
            req.output_config.get_or_insert(OutputConfig { effort: None, format: None }).effort =
                Some(crate::anthropic_types::DEFAULT_EFFORT.into());
        }
        return;
    }
    let explicit_effort = req.output_config.as_ref().and_then(|c| c.effort.as_deref());
    let structured_output = req
        .output_config
        .as_ref()
        .and_then(|c| c.json_schema())
        .is_some();
    let enabled = explicit_effort.is_some()
        || (!structured_output && crate::converter::has_thinking_suffix(&req.model));
    req.thinking = Some(Thinking {
        thinking_type: if enabled { "adaptive" } else { "disabled" }.into(),
        display: None,
        budget_tokens: 0,
    });
    if enabled && explicit_effort.is_none() {
        req.output_config
            .get_or_insert(OutputConfig {
                effort: None,
                format: None,
            })
            .effort = Some(crate::anthropic_types::DEFAULT_EFFORT.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(model: &str) -> MessagesRequest {
        serde_json::from_value(json!({
            "model": model, "max_tokens": 32000,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap()
    }

    #[test]
    fn plain_models_default_off_and_both_suffixes_enable_high() {
        for base in [
            "claude-opus-4-6",
            "claude-opus-5",
            "claude-opus-5-5",
            "claude-sonnet-5-5",
        ] {
            let mut plain = request(base);
            override_thinking_from_model_name(&mut plain);
            assert_eq!(plain.thinking.as_ref().unwrap().thinking_type, "disabled");
            assert_eq!(additional_model_request_fields(&plain), None);
            for suffix in ["-thinking", "-reasoning"] {
                let mut r = request(&format!("{base}{suffix}"));
                override_thinking_from_model_name(&mut r);
                assert_eq!(
                    additional_model_request_fields(&r),
                    Some(json!({"output_config": {"effort": "high"}}))
                );
            }
        }
    }

    #[test]
    fn explicit_parameters_override_suffix_without_raising_budget() {
        for model in ["claude-opus-5-reasoning", "claude-sonnet-5-5-thinking"] {
            let mut r = request(model);
            r.thinking = Some(Thinking {
                thinking_type: "disabled".into(),
                display: None,
                budget_tokens: 0,
            });
            override_thinking_from_model_name(&mut r);
            assert_eq!(additional_model_request_fields(&r), None);
            r.thinking = Some(Thinking {
                thinking_type: "enabled".into(),
                display: None,
                budget_tokens: 1024,
            });
            override_thinking_from_model_name(&mut r);
            assert_eq!(r.thinking.as_ref().unwrap().budget_tokens, 1024);
            assert_eq!(effective_effort(&r), Some("low"));
        }
        for (budget, effort) in [
            (0, "high"),
            (2048, "low"),
            (8192, "medium"),
            (20000, "high"),
        ] {
            assert_eq!(budget_to_effort(budget), effort);
        }
    }

    #[test]
    fn explicit_effort_survives_all_three_api_conversions() {
        for level in [
            "low", "medium", "high", "xhigh", "max", "none", "off", "disabled",
        ] {
            for suffix in ["", "-reasoning"] {
                let model = format!("claude-opus-5{suffix}");
                let bodies = [
                    json!({"model": model, "max_tokens": 32000, "messages": [{"role":"user","content":"q"}],
                        "thinking": {"type": if ["none", "off", "disabled"].contains(&level) { "disabled" } else { "adaptive" }},
                        "output_config": {"effort": level}}),
                    gw_core::openai::chat_req::convert_request(&json!({"model":model,"messages":[{"role":"user","content":"q"}],"reasoning_effort":level})).unwrap().body,
                    gw_core::openai::resp_req::convert_request(&json!({"model":model,"input":"q","reasoning":{"effort":level}})).unwrap().body,
                ];
                for body in bodies {
                    let mut r: MessagesRequest = serde_json::from_value(body).unwrap();
                    override_thinking_from_model_name(&mut r);
                    let want = if ["none", "off", "disabled"].contains(&level) {
                        None
                    } else {
                        Some(level)
                    };
                    assert_eq!(effective_effort(&r), want, "model={model}, effort={level}");
                }
            }
        }
    }

    #[test]
    fn model_schema_and_legacy_wire_remain_respected() {
        for (model, effort, want) in [
            ("claude-opus-4-6", "xhigh", Some("high")),
            ("claude-opus-5", "max", Some("max")),
            ("kimi-k3", "medium", Some("low")),
            ("claude-opus-4-5", "high", None),
            ("unknown-model", "high", None),
        ] {
            let mut r = request(model);
            r.output_config = Some(OutputConfig {
                effort: Some(effort.into()),
                format: None,
            });
            override_thinking_from_model_name(&mut r);
            assert_eq!(effective_effort(&r), want);
            if let Some(fields) = additional_model_request_fields(&r) {
                let key = if model == "kimi-k3" {
                    "reasoning"
                } else {
                    "output_config"
                };
                assert_eq!(fields[key]["effort"], want.unwrap());
                let expected = if crate::converter::requires_structured_effort(model) {
                    Some(fields)
                } else {
                    None
                };
                assert_eq!(wire_additional_model_request_fields(&r, true), expected);
            }
        }
    }

    #[test]
    fn structured_output_preserves_format_and_explicit_thinking() {
        let mut r = request("claude-opus-5-reasoning");
        r.output_config = Some(OutputConfig {
            effort: None,
            format: Some(crate::anthropic_types::OutputFormat {
                format_type: "json_schema".into(),
                schema: Some(json!({"type":"object"})),
            }),
        });
        override_thinking_from_model_name(&mut r);
        assert_eq!(effective_effort(&r), None);
        assert!(r.output_config.as_ref().unwrap().format.is_some());
        r.thinking = Some(Thinking {
            thinking_type: "adaptive".into(),
            display: Some("summarized".into()),
            budget_tokens: 0,
        });
        override_thinking_from_model_name(&mut r);
        assert_eq!(effective_effort(&r), Some("high"));
        assert!(r.output_config.as_ref().unwrap().format.is_some());
    }
}
