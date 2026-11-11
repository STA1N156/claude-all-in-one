//! 探针:模型调 Cursor **自带**工具(读文件 / 终端 / 搜索)时,在原流上回「成功」结果,
//! 验证服务端接受、模型读得到结果并正常继续。
//!
//! 背景(2026-11-10 生产统计):真正发出的自造历史重建里 54% 紧跟在「模型调了 Cursor 自带
//! 工具」之后 —— 自带工具调用被翻成调用方工具交给客户端,本轮就此结束,流不挂起,下一轮只能
//! 按完整历史重建;长会话一重建,服务端刚做好的总结就丢了。要让自带工具也能挂流续跑,先得
//! 确认三件事:界面帧之后来的是哪种 exec(普通终端 2 还是流式终端 14)、我们编码的「成功」
//! 结果服务端认不认、模型看不看得到。
//!
//! 用法(在 139 上跑,令牌不出服务器):
//! ```bash
//! PROBE_TOKEN_FILE=/root/x/tok PROBE_PROXY=http://… PROBE_MODEL=claude-sonnet-5-5 ./probe_builtin
//! ```
//! 只发 **1 个**请求(不提供任何客户端工具,模型只能用 Cursor 自带工具)。令牌只打印长度。
//!
//! `PROBE_MODE=error`:回「出错」结果(读文件 `ReadError`、流式终端退出码 1),验证续跑段
//! 出错编码服务端也认、模型看得到错误内容。默认回成功结果(读文件不带总行数,同生产编码)。

use std::time::{Duration, Instant};

use futures::StreamExt;
use gw_cursor::protobuf::{Reader, Value as PbValue, Writer};
use gw_cursor::{cli, run, wire, wirev2};

const HOST: &str = "agentn.global.api5.cursor.sh";
const TURN_DEADLINE: Duration = Duration::from_secs(180);
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
const TIMEZONE: &str = "Asia/Tokyo";
const CWD: &str = "/home/probe/project";
/// 读文件结果里埋的暗号。
const SECRET: &str = "蓝色鲸鱼-7731";
/// 终端结果里埋的输出。
const SHELL_OUT: &str = "hello-from-probe-5520";
/// 出错模式:读文件错误文本里埋的标记。
const READ_ERR: &str = "ENOENT: no such file (probe-err-4417)";
/// 出错模式:终端输出里埋的标记(退出码 1)。
const SHELL_ERR_OUT: &str = "fatal: probe-fail-8830";

fn read_token() -> Option<String> {
    let path = std::env::var("PROBE_TOKEN_FILE").ok()?;
    let tok = std::fs::read_to_string(&path).ok()?.trim().to_string();
    if tok.is_empty() {
        return None;
    }
    eprintln!("[凭证] {path} len={}", tok.len());
    Some(tok)
}

/// 响应顶层 field 2(exec 通道)→ `(id, exec_id, 分支号, 分支体)`。元数据字段 15/19/57 不算分支。
fn exec_frame(payload: &[u8]) -> Option<(u64, String, u32, Vec<u8>)> {
    let ch = Reader::new(payload).find_map(|(f, v)| match (f, v) {
        (2, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    let (mut id, mut exec_id, mut variant) = (0u64, String::new(), None);
    for (f, v) in Reader::new(ch) {
        match (f, v) {
            (1, PbValue::Varint(n)) => id = n,
            (15, PbValue::Len(s)) => exec_id = String::from_utf8_lossy(s).into_owned(),
            (19 | 57, _) => {}
            (n, PbValue::Len(s)) if variant.is_none() => variant = Some((n, s.to_vec())),
            _ => {}
        }
    }
    let (n, body) = variant?;
    Some((id, exec_id, n, body))
}

fn str_field(b: &[u8], want: u32) -> String {
    Reader::new(b)
        .find_map(|(f, v)| match (f, v) {
            (n, PbValue::Len(s)) if n == want => Some(String::from_utf8_lossy(s).into_owned()),
            _ => None,
        })
        .unwrap_or_default()
}

/// 请求侧 ExecClientMessage 外壳:`{2: {1: id, 15: exec_id, <结果字段>: result}}`。
fn ecm(id: u64, exec_id: &str, field: u32, result: &Writer) -> Vec<u8> {
    let mut m = Writer::new();
    if id != 0 {
        m.uint(1, id);
    }
    if !exec_id.is_empty() {
        m.string(15, exec_id);
    }
    m.message(field, result);
    let mut top = Writer::new();
    top.message(2, &m);
    top.into_bytes()
}

/// 按 exec 分支编出结果帧(可能多帧),编码同生产 `held::encode_result`。认不出的分支返回空。
fn result_frames(id: u64, exec_id: &str, variant: u32, body: &[u8], error: bool) -> Vec<Vec<u8>> {
    match variant {
        // read_args → ReadResult{1: ReadSuccess{1 path, 2 content}} / {2: ReadError{1 path, 2 error}}
        7 => {
            let path = str_field(body, 1);
            let content = if error {
                READ_ERR.to_string()
            } else {
                format!("# 探针文件\n暗号:{SECRET}\n第三行\n")
            };
            let mut s = Writer::new();
            s.string(1, &path);
            s.string(2, &content);
            let mut r = Writer::new();
            r.message(if error { 2 } else { 1 }, &s);
            vec![ecm(id, exec_id, 7, &r)]
        }
        // shell_stream_args → ShellStream 事件:start → stdout → exit,最后关流。
        14 => {
            let ev = |member: u32, inner: Writer| {
                let mut r = Writer::new();
                r.message(member, &inner);
                ecm(id, exec_id, 14, &r)
            };
            let mut out = Writer::new();
            out.string(1, &format!("{}\n", if error { SHELL_ERR_OUT } else { SHELL_OUT }));
            let mut exit = Writer::new();
            exit.uint(1, u64::from(error));
            exit.string(2, CWD);
            let mut close_inner = Writer::new();
            close_inner.uint(1, id);
            let mut ctl = Writer::new();
            ctl.message(1, &close_inner);
            let mut top = Writer::new();
            top.message(5, &ctl);
            vec![ev(4, Writer::new()), ev(1, out), ev(3, exit), top.into_bytes()]
        }
        _ => Vec::new(),
    }
}

#[tokio::main]
async fn main() {
    let Some(token) = read_token() else {
        eprintln!("缺 PROBE_TOKEN_FILE");
        return;
    };
    let mut b = reqwest::Client::builder().connect_timeout(Duration::from_secs(20));
    b = match std::env::var("PROBE_PROXY").ok().filter(|p| !p.is_empty()) {
        Some(p) => b.proxy(reqwest::Proxy::all(&p).expect("PROBE_PROXY 无效")),
        None => b.no_proxy(),
    };
    let client = b.build().expect("client");
    let name = std::env::var("PROBE_MODEL").unwrap_or_else(|_| "claude-sonnet-5-5".into());
    let model = gw_cursor::catalog()
        .into_iter()
        .find(|m| m.name == name)
        .unwrap_or_else(|| run::Model::with_params(&name, &[]));
    eprintln!("[模型] {} params={:?}", model.name, model.params);
    let sections = std::sync::Arc::new(gw_cursor::ContentSections::default());
    sections.load_model_library(&name);

    let conversation_id = uuid::Uuid::new_v4().to_string();
    let turn_id = uuid::Uuid::new_v4().to_string();
    let error_mode = std::env::var("PROBE_MODE").is_ok_and(|m| m == "error");
    eprintln!("[模式] {}", if error_mode { "出错结果" } else { "成功结果" });
    let prompt = std::env::var("PROBE_PROMPT").unwrap_or_else(|_| {
        if error_mode {
            return format!(
                "请先用读文件工具读取 {CWD}/SECRET.txt,再用终端执行 `git status`。\
                 然后只用一句话回答:读文件返回了什么错误原文、终端的退出码和输出原文是什么。不要重试,不要做别的事。"
            );
        }
        format!(
            "请先用读文件工具读取 {CWD}/SECRET.txt,再用终端执行 `echo probe`。\
             然后只用一句话回答:文件里的暗号是什么、终端输出了什么。不要做别的事。"
        )
    });
    let catalog = cli::cli_catalog_lan();
    let frame0 = cli::build_frame0_cli(&prompt, &model, &catalog, &conversation_id, &turn_id, cli::CliTurn::Opening, &[]);
    let context = cli::build_context_frame_cli("", &token, &conversation_id, TIMEZONE, CWD, &[]);
    let mut framed: Vec<Vec<u8>> = cli::cli_request_frames(&frame0, &context)
        .iter()
        .map(|(p, c)| if *c { wire::frame_compressed(p).expect("gzip") } else { wire::frame(p) })
        .collect();
    let deferred = framed.split_off(1);
    let (btx, brx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    let _ = btx.try_send(Ok(bytes::Bytes::from(framed.remove(0))));
    let body = reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(brx));
    // 客户端心跳 `{7:''}` 每 5s 一发(同生产)。
    let hb_tx = btx.clone();
    let hb = tokio::spawn(async move {
        let mut w = Writer::new();
        w.bytes(7, &[]);
        let frame = wire::frame(&w.into_bytes());
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if hb_tx.send(Ok(bytes::Bytes::from(frame.clone()))).await.is_err() {
                break;
            }
        }
    });

    let url = format!("https://{HOST}/agent.v1.AgentService/Run");
    let rb = cli::cli_headers(&token, &conversation_id, &turn_id)
        .into_iter()
        .fold(client.post(&url), |rb, (k, v)| rb.header(k, v));
    let started = Instant::now();
    let resp = match tokio::time::timeout(TURN_DEADLINE, rb.body(body).send()).await {
        Ok(Ok(r)) => r,
        other => {
            eprintln!("请求失败: {other:?}");
            hb.abort();
            return;
        }
    };
    eprintln!("[HTTP] {}", resp.status());
    let mut driver = wirev2::WireDriver::new(deferred, sections.clone());
    let sink = Some(&btx);
    let mut stream = resp.bytes_stream();
    let mut decoder = wire::FrameDecoder::new();
    let mut text = String::new();
    let mut answered = 0usize;
    let mut n = 0usize;
    let mut last = Instant::now();
    'outer: loop {
        if started.elapsed() > TURN_DEADLINE {
            eprintln!("触顶 TURN_DEADLINE");
            break;
        }
        let chunk = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(driver.deferred_deadline().into()), if driver.has_deferred() => {
                driver.flush_deferred(sink, "兜底期限").await;
                continue;
            }
            r = tokio::time::timeout(STALL_TIMEOUT.saturating_sub(last.elapsed()), stream.next()) => r,
        };
        let chunk = match chunk {
            Ok(Some(Ok(c))) => c,
            Ok(None) => {
                eprintln!("流结束");
                break;
            }
            Ok(Some(Err(e))) => {
                eprintln!("读流失败: {e}");
                break;
            }
            Err(_) => {
                eprintln!("{STALL_TIMEOUT:?} 无新字节,停滞");
                break;
            }
        };
        decoder.feed(&chunk);
        while let Some((flag, raw)) = decoder.next_frame() {
            let Ok(payload) = wire::frame_payload(flag, &raw) else { continue };
            n += 1;
            if !run::is_heartbeat(&payload) {
                eprintln!(
                    "[帧#{n:03} +{:>5}ms] top={:?} inner={:?} {}B",
                    started.elapsed().as_millis(),
                    run::top_fields(&payload),
                    run::inner_fields(&payload),
                    payload.len()
                );
            }
            if let Some(bc) = run::parse_builtin_call(&payload) {
                eprintln!("    界面帧:自带工具调用 {bc:?}");
            }
            if let Some((id, exec_id, variant, body)) = exec_frame(&payload) {
                eprintln!(
                    "    exec 帧:id={id} exec_id={exec_id} 分支={variant} 参数字段={:?} 字段1={:?}",
                    Reader::new(&body).map(|(f, _)| f).collect::<Vec<_>>(),
                    str_field(&body, 1).chars().take(120).collect::<String>()
                );
                for f in result_frames(id, &exec_id, variant, &body, error_mode) {
                    let _ = btx.send(Ok(bytes::Bytes::from(wire::frame(&f)))).await;
                    answered += 1;
                }
                if variant == 7 || variant == 14 {
                    eprintln!("    → 已回{}结果(分支 {variant})", if error_mode { "出错" } else { "成功" });
                }
            }
            driver.on_frame(&payload, sink).await;
            let fr = run::parse_frame(&payload);
            if !fr.text.is_empty() {
                text.push_str(&fr.text);
                last = Instant::now();
            }
            if !fr.thinking.is_empty() || !fr.text.is_empty() {
                last = Instant::now();
            }
            if run::is_turn_commit(&payload) && !text.is_empty() {
                eprintln!("    turn_commit(已有正文)");
                // 再读 3 秒收尾帧
                let _ = tokio::time::timeout(Duration::from_secs(3), async {
                    while let Some(Ok(c)) = stream.next().await {
                        decoder.feed(&c);
                    }
                })
                .await;
                break 'outer;
            }
            if flag & 0x02 != 0 {
                eprintln!("    结束帧: {}", String::from_utf8_lossy(&payload).chars().take(400).collect::<String>());
            }
        }
    }
    hb.abort();
    eprintln!("\n=== 回过 {answered} 帧结果;耗时 {}ms", started.elapsed().as_millis());
    eprintln!("=== 正文: {text}");
    if error_mode {
        eprintln!(
            "=== 看到读文件错误: {}   看到终端出错输出: {}",
            text.contains("4417"),
            text.contains("8830")
        );
    } else {
        eprintln!(
            "=== 读到暗号: {}   读到终端输出: {}",
            text.contains(SECRET),
            text.contains(SHELL_OUT)
        );
    }
}
