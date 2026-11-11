//! dario 线:按客户端翻译成 Claude Code 报文(用户 10-30 定的方向)。
//!
//! - **真 Claude Code**(与 dario `isGenuineCCClient` 同一判据)→ 原样交给 dario,只修 caio 自己
//!   造成的两处偏差:请求体顶层字段被 serde_json 排成了字母序;每个请求都被补了 `service_tier`
//!   (真 CC 平时不发,只有 `/fast` 时发 `auto`,那时才改成 `standard_only` 挡掉加价档)。
//! - **认得出且支持的客户端**(OpenCode、Copilot、CC 工具面的变体)→ 翻译成「dario 眼里的真 CC」,
//!   dario 走透传、不再套它自己的模板;回包里的工具调用改回客户端的名字与参数(见 [`reverse`])。
//! - **认不出 / 不支持的客户端** → 本地 400,不发上游、不扣号(`BadRequest` 不计入号的失败)。
//!
//! 为什么不交给 dario 的模板路径:它只按一张通用别名表猜工具参数(`cmd || command`),不认识
//! pi 的 developer 角色、new-api 插的 `...`;客户端的知识应该只有一份,在 caio(识别在
//! `gw_core::clients`,两条线共用)。
//!
//! 开关 `dario_cc_translate`(热设置,30 秒生效):`off` 与改造前逐字节一致;`observe` 照常转发、
//! 只记录会怎么翻译;`on` 生效。模板文件读不到时一律按 `off` 处理。

mod cleanup;
mod genuine;
mod order;
pub(crate) mod reverse;
mod scrub;
mod shape;
pub(crate) mod template;
mod tools;

use std::sync::atomic::{AtomicU8, Ordering};

use gw_core::clients::{recognize, ClientKind, ClientSignals};
use gw_core::config::dario_cc::DarioCcMode;
use serde_json::Value;

pub(crate) use reverse::ReverseMapper;
use template::CcTemplate;

static MODE: AtomicU8 = AtomicU8::new(0);

fn mode_to_u8(m: DarioCcMode) -> u8 {
    match m {
        DarioCcMode::Off => 0,
        DarioCcMode::Observe => 1,
        DarioCcMode::On => 2,
    }
}

pub(crate) fn set_mode(m: DarioCcMode) {
    MODE.store(mode_to_u8(m), Ordering::Relaxed);
}

pub(crate) fn mode() -> DarioCcMode {
    match MODE.load(Ordering::Relaxed) {
        1 => DarioCcMode::Observe,
        2 => DarioCcMode::On,
        _ => DarioCcMode::Off,
    }
}

/// 本条请求怎么发给 dario。
pub(crate) struct Prepared {
    /// `None` = 按改造前的方式(`prepare_forwarded_body` + 序列化);`Some` = 直接发这些字节。
    pub bytes: Option<Vec<u8>>,
    pub reverse: Option<ReverseMapper>,
    /// 走了透传或翻译时记下客户端类型(回包日志用)。
    pub client: Option<ClientKind>,
}

impl Prepared {
    pub(crate) fn legacy() -> Self {
        Self { bytes: None, reverse: None, client: None }
    }
}

/// 不支持的客户端:原因进日志,也经 `UpstreamError::bad_request_visible` 发给客户端(文案全由我方生成)。
#[derive(Debug)]
pub(crate) struct Rejected {
    pub client: ClientKind,
    pub reason: String,
}

/// 不看开关的纯判定(observe 与 on 共用,也给离线回放用)。
pub(crate) enum Plan {
    Passthrough { body: Vec<u8> },
    Translated(shape::Translated, ClientKind),
    Rejected(Rejected),
}

pub(crate) fn plan(body: &Value, tpl: &CcTemplate) -> Plan {
    if genuine::is_genuine_cc(body) {
        return Plan::Passthrough { body: passthrough_bytes(body, tpl) };
    }
    let kind = recognize(&ClientSignals::from_body(body));
    match shape::translate(kind, body, tpl) {
        Ok(t) => Plan::Translated(t, kind),
        Err(reason) => Plan::Rejected(Rejected { client: kind, reason }),
    }
}

/// 按当前开关决定怎么发。拒绝时返回 `Err`(调用方转成本地 400)。
pub(crate) fn prepare(body: &Value, tpl: Option<&CcTemplate>) -> Result<Prepared, Rejected> {
    let mode = mode();
    if mode == DarioCcMode::Off {
        return Ok(Prepared::legacy());
    }
    let Some(tpl) = tpl else {
        return Ok(Prepared::legacy());
    };
    let plan = plan(body, tpl);
    log_plan(mode, &plan);
    if mode == DarioCcMode::Observe {
        return Ok(Prepared::legacy());
    }
    match plan {
        Plan::Passthrough { body } => Ok(Prepared { bytes: Some(body), reverse: None, client: Some(ClientKind::ClaudeCode) }),
        Plan::Translated(t, kind) => Ok(Prepared {
            bytes: Some(t.body),
            reverse: t.map.map(ReverseMapper::new),
            client: Some(kind),
        }),
        Plan::Rejected(r) => Err(r),
    }
}

fn log_plan(mode: DarioCcMode, plan: &Plan) {
    match plan {
        Plan::Passthrough { .. } => {
            tracing::info!(target: "dario_cc", mode = mode.as_str(), client = "claude_code", route = "passthrough", "dario CC");
        }
        Plan::Translated(t, kind) => {
            tracing::info!(
                target: "dario_cc",
                mode = mode.as_str(),
                client = kind.as_str(),
                route = "translated",
                native = t.stats.native,
                renamed = t.stats.renamed,
                same = t.stats.same,
                "dario CC"
            );
        }
        Plan::Rejected(r) => {
            tracing::info!(target: "dario_cc", mode = mode.as_str(), client = r.client.as_str(), route = "rejected", reason = %r.reason, "dario CC");
        }
    }
}

/// 离线回放用:一条请求在 `on` 模式下会怎么发(只算不发,不打日志)。翻译过的请求另外把
/// 历史里每个客户端形态的 tool_use 做一次「客户端 → CC → 客户端」往返,按工具统计是否无损,
/// 用真实参数检验映射表。
pub(crate) fn replay_summary(body: &Value, tpl: &CcTemplate) -> Value {
    match plan(body, tpl) {
        Plan::Passthrough { body } => serde_json::json!({
            "route": "passthrough", "client": "claude_code", "body": String::from_utf8_lossy(&body),
        }),
        Plan::Translated(t, kind) => {
            let mut round_trip = serde_json::Map::new();
            if let Some(map) = &t.map {
                for (name, input) in history_tool_uses(body) {
                    let Some((wire, cc_args)) = map.forward(&name, &input) else { continue };
                    let back = map.backward(&wire, &cc_args).map(|(_, a)| a);
                    let slot = round_trip.entry(name).or_insert_with(|| serde_json::json!({"ok": 0, "lossy": 0}));
                    let key = if back.as_ref() == Some(&input) { "ok" } else { "lossy" };
                    slot[key] = serde_json::json!(slot[key].as_u64().unwrap_or(0) + 1);
                }
            }
            serde_json::json!({
                "route": "translated", "client": kind.as_str(),
                "native": t.stats.native, "renamed": t.stats.renamed, "same": t.stats.same,
                "round_trip": round_trip, "body": String::from_utf8_lossy(&t.body),
            })
        }
        Plan::Rejected(r) => serde_json::json!({"route": "rejected", "client": r.client.as_str(), "reason": r.reason}),
    }
}

/// 原始请求历史里 assistant 发出的 tool_use(客户端形态)。
fn history_tool_uses(body: &Value) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for m in body.get("messages").and_then(Value::as_array).into_iter().flatten() {
        if m.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        for b in m.get("content").and_then(Value::as_array).into_iter().flatten() {
            if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                let name = b.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                out.push((name, b.get("input").cloned().unwrap_or(Value::Null)));
            }
        }
    }
    out
}

/// 真 CC:字段按 CC 顺序写回;强制流式(dario 恒以 SSE 返回);只有 `/fast`(`service_tier=auto`)
/// 时改成 `standard_only`,没发就不补。
fn passthrough_bytes(body: &Value, tpl: &CcTemplate) -> Vec<u8> {
    let mut m = body.as_object().cloned().unwrap_or_default();
    m.insert("stream".into(), Value::Bool(true));
    if m.get("service_tier").and_then(Value::as_str) == Some("auto") {
        m.insert("service_tier".into(), Value::String("standard_only".into()));
    }
    order::to_ordered_bytes(&m, &tpl.body_field_order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn genuine_body() -> Value {
        json!({
            "stream": false,
            "model": "claude-opus-5-5",
            "messages": [{"role": "user", "content": "hi"}],
            "system": [{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.312;"},
                       {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}],
            "max_tokens": 64000
        })
    }

    #[test]
    fn genuine_cc_is_passed_through_in_cc_field_order_without_service_tier() {
        let tpl = template::test_template();
        let Plan::Passthrough { body } = plan(&genuine_body(), &tpl) else { panic!("应走透传") };
        let s = String::from_utf8(body).unwrap();
        assert!(s.starts_with(r#"{"model":"claude-opus-5-5","messages":"#), "{s}");
        assert!(!s.contains("service_tier"), "真 CC 没发就不补");
        assert!(s.contains(r#""stream":true"#));
        let mut fast = genuine_body();
        fast["service_tier"] = json!("auto");
        let Plan::Passthrough { body } = plan(&fast, &tpl) else { panic!() };
        assert!(String::from_utf8(body).unwrap().contains(r#""service_tier":"standard_only""#));
    }

    #[test]
    fn mode_gates_behaviour() {
        let tpl = template::test_template();
        // 带工具的 Grok Build。
        let unsupported = json!({"model": "m", "system": "You are Grok released by xAI",
                                 "tools": [{"name": "spawn_subagent"}], "messages": [{"role": "user", "content": "hi"}]});
        set_mode(DarioCcMode::Off);
        assert!(prepare(&unsupported, Some(&tpl)).unwrap().bytes.is_none());
        set_mode(DarioCcMode::Observe);
        assert!(prepare(&unsupported, Some(&tpl)).unwrap().bytes.is_none(), "observe 只记录,照常转发");
        set_mode(DarioCcMode::On);
        assert!(prepare(&unsupported, None).unwrap().bytes.is_none(), "模板读不到按 off");
        let r = prepare(&unsupported, Some(&tpl)).err().expect("on 模式拒绝不支持的客户端");
        assert_eq!(r.client, ClientKind::GrokBuild);
        assert!(prepare(&genuine_body(), Some(&tpl)).unwrap().bytes.is_some());
        set_mode(DarioCcMode::Off);
    }
}
