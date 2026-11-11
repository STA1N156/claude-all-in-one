//! 历史报文回放:标准输入每行一个 Anthropic 请求体(JSON),标准输出每行一个转换结果。
//!
//! 用途:改转换器之前/之后各编一份,喂同一批线上请求,逐条比对发往上游的 conversationState
//! (见 CHANGELOG「按客户端翻译」一节)。只走 `override_thinking_from_model_name` +
//! `convert_request` 这两步,与 `chat::render_kiro_payload` 同口径;不发网络请求。
//!
//! 输出:`{"ok":true,"cs":<conversationState>}` 或 `{"ok":false,"err":"..."}`,一行一条、与输入顺序一致。

use std::io::{BufRead, BufWriter, Write};

/// 固定的账号作用域盐:两份二进制必须一致,conversationId 才可比。
const REPLAY_SCOPE: &str = "replay";

fn convert_line(line: &str) -> serde_json::Value {
    let mut req: gw_kiro::anthropic_types::MessagesRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return serde_json::json!({"ok": false, "err": format!("parse: {e}")}),
    };
    gw_kiro::thinking_policy::override_thinking_from_model_name(&mut req);
    match gw_kiro::converter::convert_request(&req, REPLAY_SCOPE) {
        Ok(c) => serde_json::json!({"ok": true, "cs": c.conversation_state}),
        Err(e) => serde_json::json!({"ok": false, "err": e.to_string()}),
    }
}

fn main() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut out = BufWriter::new(std::io::stdout().lock());
    for line in stdin.lock().lines() {
        let line = line?;
        serde_json::to_writer(&mut out, &convert_line(&line))?;
        out.write_all(b"\n")?;
        // 调用方逐行一问一答,每条都要立刻送出去。
        out.flush()?;
    }
    Ok(())
}
