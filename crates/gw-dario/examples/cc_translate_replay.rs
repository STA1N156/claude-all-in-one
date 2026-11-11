//! 离线回放 dario 线「翻译成 Claude Code 报文」:标准输入每行一条 Anthropic 请求体,标准输出
//! 每行一条结果 JSON(透传 / 翻译后的报文 / 拒绝原因,翻译的另带工具往返统计)。只算不发。
//!
//! 用法:`cc_translate_replay <cc-template-data.json> < requests.jsonl > out.jsonl`

use std::io::{BufRead, BufWriter, Write};

fn main() {
    let path = std::env::args().nth(1).expect("用法: cc_translate_replay <cc-template-data.json>");
    let template = std::fs::read_to_string(&path).expect("读模板文件");
    let stdin = std::io::stdin();
    let mut out = BufWriter::new(std::io::stdout().lock());
    for line in stdin.lock().lines() {
        let line = line.expect("读标准输入");
        if line.trim().is_empty() {
            continue;
        }
        let result = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(body) => gw_dario::cc_replay(&body, &template).unwrap_or_else(|e| serde_json::json!({"error": e})),
            Err(e) => serde_json::json!({"error": format!("输入不是 JSON: {e}")}),
        };
        writeln!(out, "{result}").expect("写标准输出");
        out.flush().expect("刷新标准输出");
    }
}
