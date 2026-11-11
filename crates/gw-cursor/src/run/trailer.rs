//! end-stream trailer 错误解析。
//!
//! 2026-11-10 由 run.rs 拆出(只搬不改行为)。

use super::*;

/// end-stream trailer(`flag & 0x02`)里的错误。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrailerError {
    /// Connect 层 code,如 `resource_exhausted` / `unauthenticated`。
    pub code: String,
    /// Cursor 自己的错误名,如 `ERROR_RATE_LIMITED_CHANGEABLE`。
    pub debug_error: String,
    pub title: String,
    pub detail: String,
    /// 服务端建议自动切换到的模型(计费降级时出现)。
    pub auto_switch_to_model: String,
    /// `debug.details.analyticsMetadata.actionRequired`:Anthropic 用量政策拦截时是拦截类别
    /// (2026-11-09 实物 `cyber_policy_review`),额度降级时是 `upgrade`。
    pub action_required: String,
    /// `debug.details.additionalInfo.providerStatusCode`:模型提供方(Anthropic 等)回给
    /// Cursor 的 HTTP 状态码。只在 `ERROR_PROVIDER_ERROR` 这类错误里出现
    /// (2026-11-10 实物是字符串 `"400"`;数字形态也认)。
    pub provider_status: Option<u16>,
    /// `debug.details.isRetryable`(只记录进摘要,分类按 `provider_status`)。
    pub is_retryable: Option<bool>,
}

/// 解析 trailer JSON。没有 `error` 字段 → `None`(正常收尾)。
///
/// 结构嵌得很深且各层都可能缺失,所以逐层用 `and_then` 而不是 serde 结构体 ——
/// 另外 `spendLimits` 的值是 JSON **字符串** `"[50,100,200]"` 而不是数组,
/// 用结构体反序列化会直接炸。我们不需要它,索性不碰。
pub fn parse_trailer(json_text: &str) -> Option<TrailerError> {
    let v: Value = serde_json::from_str(json_text).ok()?;
    let err = v.get("error")?;

    let mut out = TrailerError {
        code: str_at(err, &["code"]),
        ..Default::default()
    };

    // error.details[0].debug.{error, details:{title, detail, additionalInfo:{autoSwitchToModel}}}
    let debug = err
        .get("details")
        .and_then(|d| d.as_array())
        .and_then(|a| a.iter().find_map(|d| d.get("debug")));
    if let Some(dbg) = debug {
        out.debug_error = str_at(dbg, &["error"]);
        out.title = str_at(dbg, &["details", "title"]);
        out.detail = str_at(dbg, &["details", "detail"]);
        out.auto_switch_to_model = str_at(dbg, &["details", "additionalInfo", "autoSwitchToModel"]);
        out.action_required = str_at(dbg, &["details", "analyticsMetadata", "actionRequired"]);
        out.provider_status = dbg
            .pointer("/details/additionalInfo/providerStatusCode")
            .and_then(status_code_of);
        out.is_retryable = dbg.pointer("/details/isRetryable").and_then(Value::as_bool);
    }

    // 整条 trailer 里连一个可辨识字段都没有时,也别返回一个全空的 TrailerError ——
    // 那会让上层报一句没有信息量的错。至少把原始 JSON 截一段带出去。
    if out.code.is_empty() && out.debug_error.is_empty() && out.title.is_empty() {
        out.detail = json_text.chars().take(300).collect();
    }
    Some(out)
}

pub(super) fn str_at(v: &Value, path: &[&str]) -> String {
    let mut cur = v;
    for k in path {
        match cur.get(k) {
            Some(next) => cur = next,
            None => return String::new(),
        }
    }
    cur.as_str().unwrap_or_default().to_string()
}

/// HTTP 状态码:字符串 `"400"` 或数字 `400` 都认;认不出(非数字、超出 100..=599)返回 `None`,不猜。
fn status_code_of(v: &Value) -> Option<u16> {
    let n = match v {
        Value::String(s) => s.trim().parse::<u16>().ok()?,
        Value::Number(n) => u16::try_from(n.as_u64()?).ok()?,
        _ => return None,
    };
    (100..=599).contains(&n).then_some(n)
}

impl TrailerError {
    /// 人类可读的一行摘要。
    pub fn summary(&self) -> String {
        let mut s = String::new();
        if !self.code.is_empty() {
            s.push_str(&self.code);
        }
        if !self.debug_error.is_empty() {
            if !s.is_empty() {
                s.push('/');
            }
            s.push_str(&self.debug_error);
        }
        let text = if !self.detail.is_empty() {
            &self.detail
        } else {
            &self.title
        };
        if !text.is_empty() {
            if !s.is_empty() {
                s.push_str(": ");
            }
            s.push_str(text);
        }
        // 提供方状态码是分类依据(4xx = 请求本身的问题),排障时必须一眼看到。
        if let Some(p) = self.provider_status {
            if !s.is_empty() {
                s.push(' ');
            }
            match self.is_retryable {
                Some(r) => s.push_str(&format!("[provider={p} retryable={r}]")),
                None => s.push_str(&format!("[provider={p}]")),
            }
        }
        if s.is_empty() {
            s.push_str("上游未给出错误详情");
        }
        s
    }
}
