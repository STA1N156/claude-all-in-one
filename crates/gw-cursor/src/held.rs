//! 挂流续跑:工具回路不关上游连接、不重发历史(2026-11-09)。
//!
//! # 为什么要有
//!
//! Cursor 的 `agent.v1.AgentService/Run` 是一条双向流。模型调用 MCP 工具时,服务端在流上发
//! exec `mcp_args`(带 `id` / `exec_id`),然后**停在原地等客户端在同一条流上回 `McpResult`**,
//! 这一轮的全部状态都在这条流里。Anthropic 接口是一问一答:我们必须结束本次 HTTP 响应才能把
//! `tool_use` 交给客户端,客户端执行完再发**新请求**带 `tool_result` 回来。
//!
//! 旧实现在交出 `tool_use` 时把上游流一起丢了,下一轮只能新开一条 Run、把整段历史折叠重发
//! (日志「影子表无描述符,降级重铺」;工具轮停在等结果,永远拿不到续轮描述符)。生产 11-09:
//! 40 分钟 292 次重铺,每轮输入 3 万~31 万 token,首包慢、积分烧得快。
//!
//! 做法照 OmniRoute(MIT)`open-sse/services/cursorSessionManager.ts`:工具轮收尾时把流**挂起**,
//! 客户端带结果回来时按工具调用 id 取回,在原流上回 `McpResult`,接着读 —— 一个字的历史都不重发。
//!
//! # 安全边界(当年 CLI 驱动 clidrv 桥接踩过的坑,同样适用)
//!
//! - **只按「带回的结果 id 集合 == 挂起时等待的 id 集合」取**。对不上(别的会话/别家 provider 的
//!   结果、客户端改过历史)就不碰挂起的流,走冷启动重铺 —— 绝不喂错结果。
//! - 取出即从表里摘掉:同一条流不会被两个请求同时续。
//! - 客户端拿到 `tool_use` 后再也不回来:闲置 [`IDLE_TTL`] 回收(drop = 关流 + 停心跳)。
//! - 每号最多 [`MAX_PER_ACCOUNT`] 条、全局 [`MAX_TOTAL`] 条,超了回收最旧的。
//! - 续跑时上游若已超时断流:出第一个字之前报可重试错误,gw-app 重试时挂起的流已被取走,
//!   自然走冷启动 —— 客户端无感。
//!
//! 热开关 `cursor_hold_tool_streams`(默认关),关掉 = 旧行为,30 秒生效。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::protobuf::Writer;
use crate::wire::FrameDecoder;

/// 挂起的流闲置多久回收。与 OmniRoute 的 `CURSOR_SESSION_IDLE_TTL_MS` 默认值一致。
pub(crate) const IDLE_TTL: Duration = Duration::from_secs(5 * 60);
/// 每个号最多同时挂起几条(挂起的流不占调度器的并发租约,这道上限防单号挂太多)。
pub(crate) const MAX_PER_ACCOUNT: usize = 8;
/// 全进程上限。
pub(crate) const MAX_TOTAL: usize = 100;
/// 回给上游的单条工具结果文本上限(字节)。与 OmniRoute `CURSOR_MCP_TEXT_MAX_BYTES` 一致。
pub(crate) const MCP_TEXT_MAX_BYTES: usize = 38_000;
/// 截断提示预留的字节。
const NOTICE_RESERVE_BYTES: usize = 200;
/// exec 分支:`mcp_result`(与请求侧 `mcp_args` 同号)。
const VAR_MCP_RESULT: u32 = 11;

/// 上游响应字节流(类型擦除后才能存进表)。
pub(crate) type ByteStream =
    Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;
/// 请求体发送端(保持请求流不 half-close,回执/结果也经它送回)。
pub(crate) type BodySender = tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// 热开关(`cursor_hold_tool_streams`)。
pub(crate) fn set_enabled(on: bool) {
    let was = ENABLED.swap(on, Ordering::Relaxed);
    if was != on {
        tracing::info!(enabled = on, "cursor 挂流续跑开关已热应用");
    }
    if !on {
        // 关掉时把已挂起的流全部放掉:关闭开关就是要回到旧行为,不能留着半截状态。
        let n = registry().clear();
        if n > 0 {
            tracing::info!(released = n, "cursor 挂流续跑已关闭,释放全部挂起的流");
        }
    }
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 请求侧客户端心跳任务的守卫:drop 即停。
///
/// 官方 CLI 全程每 ~5s 发一个 `{7:}` 空帧;不发的话服务端把这条流当死客户端。
/// 挂起期间同样要发,所以守卫跟着流一起进挂起表。
pub(crate) struct HeartbeatGuard(Option<tokio::task::JoinHandle<()>>);

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        if let Some(h) = &self.0 {
            h.abort();
        }
    }
}

/// 起一个每 5 秒发 `{7:}` 的心跳任务(发送失败 = 请求流已关,自行退出)。
pub(crate) fn spawn_client_heartbeat(sink: BodySender) -> HeartbeatGuard {
    HeartbeatGuard(Some(tokio::spawn(async move {
        let frame = crate::wire::frame(&crate::cli::frame_field7_empty());
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if sink
                .send_timeout(Ok(bytes::Bytes::from(frame.clone())), Duration::from_secs(5))
                .await
                .is_err()
            {
                break;
            }
        }
    })))
}

/// 一个等结果的工具调用:客户端看到的 `tool_use` id ↔ 上游 exec 的 `id` / `exec_id`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingExec {
    pub tool_use_id: String,
    pub id: u64,
    pub exec_id: String,
    /// 上游在等哪种结果(决定续跑时怎么编码,见 [`encode_result`])。
    pub kind: PendingKind,
}

/// 挂起时上游在等的 exec 结果种类。
///
/// MCP 是我方经 `claude-local` 声明的工具;其余是 Cursor **自带**工具 —— 模型调它们时我方
/// 翻成调用方工具交给客户端执行,结果回来后按 cursor-agent `agent.v1` 的结果消息编码回上游
/// (2026-11-10 实验号实测:读文件 `read_args` 与流式终端 `shell_stream_args` 的结果服务端都认,
/// 模型读得到)。只放这两种:类型上保证续跑段一定编得出上游认的结果,编不出的种类根本不挂
/// (见 `crate::exec::holdable_exec`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingKind {
    Mcp,
    /// `read_args`(7)→ `read_result`。
    Read { path: String },
    /// `shell_stream_args`(14)→ `shell_stream` 事件序列 + 关流。
    ShellStream { cwd: String },
}

/// 按挂起种类把客户端带回的工具结果编码成回给上游的帧(未分帧,可能多帧)。
///
/// - 读文件:`ReadResult{1 success: ReadSuccess{1 path, 2 content}}`;出错
///   `{2 error: ReadError{1 path, 2 error}}`。不报 `3 total_lines`:客户端的读文件结果常带行号、
///   常只读一段(offset/limit),数出来的行数既不是文件总行数也不是内容行数,报了反而误导模型。
/// - 流式终端:`ShellStream` 事件 `4 start{}` → `1 stdout{1 data}` → `3 exit{1 code, 2 cwd}`,
///   最后关流(见 [`crate::exec::stream_close_frame`])。
///
/// 客户端的结果文本没有退出码,出错与否只看 `is_error`(Claude Code / OpenCode 的终端工具
/// 非零退出时置位),出错按退出码 1 报;文本超长按 [`fit_tool_output`] 截断(与 MCP 同一上限)。
pub(crate) fn encode_result(p: &PendingExec, text: &str, is_error: bool) -> Vec<Vec<u8>> {
    use crate::exec::{VAR_READ, VAR_SHELL_STREAM};
    let frame = |field: u32, result: &Writer| crate::run::exec_client_frame(p.id, &p.exec_id, field, result);
    match &p.kind {
        PendingKind::Mcp => vec![encode_mcp_result(p.id, &p.exec_id, text, is_error)],
        PendingKind::Read { path } => {
            let text = fit_tool_output(text, MCP_TEXT_MAX_BYTES);
            let mut inner = Writer::new();
            inner.string(1, path);
            inner.string(2, &text);
            let mut r = Writer::new();
            r.message(if is_error { 2 } else { 1 }, &inner);
            vec![frame(VAR_READ, &r)]
        }
        PendingKind::ShellStream { cwd } => {
            let text = fit_tool_output(text, MCP_TEXT_MAX_BYTES);
            let event = |member: u32, inner: Writer| {
                let mut r = Writer::new();
                r.message(member, &inner);
                frame(VAR_SHELL_STREAM, &r)
            };
            let mut out = Writer::new();
            out.string(1, &text);
            let mut exit = Writer::new();
            exit.uint(1, u64::from(is_error));
            exit.string(2, cwd);
            vec![
                event(4, Writer::new()),
                event(1, out),
                event(3, exit),
                crate::exec::stream_close_frame(p.id),
            ]
        }
    }
}

/// 一条挂起的 Run。
pub(crate) struct HeldRun {
    /// 挂起时的会话 id(只用于日志核对;取用按工具调用 id)。
    pub conversation_id: String,
    pub byte_stream: ByteStream,
    /// 解码器里可能还有已收到、未处理的字节,原样带到续跑段。
    pub decoder: FrameDecoder,
    pub body: BodySender,
    pub heartbeat: Option<HeartbeatGuard>,
    pub pending: Vec<PendingExec>,
    held_at: Instant,
    token: u64,
}

impl HeldRun {
    pub(crate) fn new(
        conversation_id: String,
        byte_stream: ByteStream,
        decoder: FrameDecoder,
        body: BodySender,
        heartbeat: Option<HeartbeatGuard>,
        pending: Vec<PendingExec>,
    ) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            conversation_id,
            byte_stream,
            decoder,
            body,
            heartbeat,
            pending,
            held_at: Instant::now(),
            token: NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// 挂了多久(日志用)。
    pub(crate) fn held_for(&self) -> Duration {
        self.held_at.elapsed()
    }

    fn matches(&self, ids: &[&str]) -> bool {
        if ids.len() != self.pending.len() {
            return false;
        }
        let mut want: Vec<&str> = self.pending.iter().map(|p| p.tool_use_id.as_str()).collect();
        let mut got: Vec<&str> = ids.to_vec();
        want.sort_unstable();
        got.sort_unstable();
        want == got
    }
}

/// 挂起表:账号 → 该号挂起的流。
#[derive(Default)]
pub(crate) struct HeldRuns {
    inner: Mutex<HashMap<String, Vec<HeldRun>>>,
}

pub(crate) fn registry() -> &'static HeldRuns {
    static R: OnceLock<HeldRuns> = OnceLock::new();
    R.get_or_init(HeldRuns::default)
}

impl HeldRuns {
    /// 登记一条挂起的流。超额时回收最旧的。返回本条的回收凭证(见 [`Self::remove`])。
    pub(crate) fn insert(&self, account_id: &str, run: HeldRun) -> u64 {
        let token = run.token;
        let mut dropped: Vec<HeldRun> = Vec::new();
        {
            let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            Self::sweep_locked(&mut map, &mut dropped);
            let list = map.entry(account_id.to_string()).or_default();
            list.push(run);
            while list.len() > MAX_PER_ACCOUNT {
                dropped.push(list.remove(0));
            }
            let total: usize = map.values().map(Vec::len).sum();
            if total > MAX_TOTAL {
                // 回收全局最旧的那条。
                if let Some((acc, idx)) = map
                    .iter()
                    .flat_map(|(a, l)| l.iter().enumerate().map(move |(i, r)| (a.clone(), i, r.held_at)))
                    .min_by_key(|(_, _, t)| *t)
                    .map(|(a, i, _)| (a, i))
                {
                    if let Some(l) = map.get_mut(&acc) {
                        dropped.push(l.remove(idx));
                    }
                }
            }
        }
        if !dropped.is_empty() {
            tracing::info!(released = dropped.len(), "cursor 挂流:超额或过期,回收挂起的流");
        }
        // 锁外 drop:关流/停心跳不占锁。
        drop(dropped);
        // 闲置到期主动清一次(没有新请求进来时,过期的流也要放掉)。
        tokio::spawn(async {
            tokio::time::sleep(IDLE_TTL + Duration::from_secs(1)).await;
            registry().sweep();
        });
        token
    }

    /// 按「带回的结果 id 集合」取出该号挂起的流(取出即摘掉)。对不上返回 `None`,表不动。
    pub(crate) fn take(&self, account_id: &str, ids: &[&str]) -> Option<HeldRun> {
        if ids.is_empty() {
            return None;
        }
        let mut dropped: Vec<HeldRun> = Vec::new();
        let found = {
            let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            Self::sweep_locked(&mut map, &mut dropped);
            let list = map.get_mut(account_id)?;
            let idx = list.iter().position(|r| r.matches(ids));
            let found = idx.map(|i| list.remove(i));
            if list.is_empty() {
                map.remove(account_id);
            }
            found
        };
        drop(dropped);
        found
    }

    /// 按凭证摘掉一条(登记后才发现客户端已断开时用)。
    pub(crate) fn remove(&self, account_id: &str, token: u64) -> bool {
        let removed = {
            let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(list) = map.get_mut(account_id) else { return false };
            let idx = list.iter().position(|r| r.token == token);
            let removed = idx.map(|i| list.remove(i));
            if list.is_empty() {
                map.remove(account_id);
            }
            removed
        };
        removed.is_some()
    }

    /// 清掉过期的。
    pub(crate) fn sweep(&self) {
        let mut dropped = Vec::new();
        {
            let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            Self::sweep_locked(&mut map, &mut dropped);
        }
        if !dropped.is_empty() {
            tracing::info!(released = dropped.len(), "cursor 挂流:闲置超时,回收挂起的流");
        }
    }

    fn sweep_locked(map: &mut HashMap<String, Vec<HeldRun>>, dropped: &mut Vec<HeldRun>) {
        for list in map.values_mut() {
            let mut i = 0;
            while i < list.len() {
                if list[i].held_at.elapsed() > IDLE_TTL {
                    dropped.push(list.remove(i));
                } else {
                    i += 1;
                }
            }
        }
        map.retain(|_, l| !l.is_empty());
    }

    /// 全部放掉(关开关时用)。返回放掉的条数。
    pub(crate) fn clear(&self) -> usize {
        let all: Vec<HeldRun> = {
            let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            map.drain().flat_map(|(_, l)| l).collect()
        };
        all.len()
    }

    /// 当前挂起的条数(`/health` 用)。
    pub(crate) fn len(&self) -> usize {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.values().map(Vec::len).sum()
    }
}

/// 截到 `max` 字节以内:按行截,保留开头,末尾附一句截断说明(与 OmniRoute
/// `fitCursorToolOutput` 同口径)。单行就超的按字节截,不劈开多字节字符。
pub(crate) fn fit_tool_output(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let budget = max.saturating_sub(NOTICE_RESERVE_BYTES);
    let mut kept = String::new();
    for line in text.split('\n') {
        let add = line.len() + usize::from(!kept.is_empty());
        if kept.len() + add > budget {
            break;
        }
        if !kept.is_empty() {
            kept.push('\n');
        }
        kept.push_str(line);
    }
    if kept.is_empty() {
        let mut end = budget.min(text.len());
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        kept.push_str(&text[..end]);
    }
    format!(
        "{kept}\n\n[输出过长,已截断:原文 {} 字节,只保留前 {} 字节]",
        text.len(),
        kept.len()
    )
}

/// 编码 MCP 工具结果(未分帧):
/// `{2: {1: id, 15: exec_id, 11: {1: {1: [{1: {1: text}}], 2: is_error}}}}`
/// (`McpResult.success{content: [McpToolResultContentItem{text: McpTextContent{text}}], is_error}`,
/// 字段号对照 OmniRoute `encodeExecMcpResult`)。
pub(crate) fn encode_mcp_result(id: u64, exec_id: &str, text: &str, is_error: bool) -> Vec<u8> {
    let text = fit_tool_output(text, MCP_TEXT_MAX_BYTES);
    let mut text_content = Writer::new();
    text_content.string(1, &text);
    let mut item = Writer::new();
    item.message(1, &text_content);
    let mut success = Writer::new();
    success.message(1, &item);
    if is_error {
        success.bool(2, true);
    }
    let mut result = Writer::new();
    result.message(1, &success);
    crate::run::exec_client_frame(id, exec_id, VAR_MCP_RESULT, &result)
}

/// 末条 user 消息里的工具结果 `(tool_use_id, 文本, is_error)`,供续跑回传。
///
/// 只认「末条是 user、至少一个 tool_result、每个都带 id」的形态;同条里的文字块
/// (Claude Code 常把 system-reminder / hook 输出和结果放在一起)附到**最后一个**结果后面,
/// 不能丢。带图片/文档的结果不归这里(那种请求本来就不走 CLI 形态)。
pub(crate) fn tool_results(body: &serde_json::Value) -> Option<Vec<(String, String, bool)>> {
    let msgs = body.get("messages")?.as_array()?;
    let last = msgs.last()?;
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return None;
    }
    let blocks = last.get("content")?.as_array()?;
    let mut out: Vec<(String, String, bool)> = Vec::new();
    let mut extra: Vec<String> = Vec::new();
    for b in blocks {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("tool_result") => {
                let id = b.get("tool_use_id")?.as_str()?.to_string();
                let text = b
                    .get("content")
                    .map(crate::chat::extract_text)
                    .unwrap_or_default();
                let is_error = b.get("is_error").and_then(|e| e.as_bool()) == Some(true);
                out.push((id, text, is_error));
            }
            Some("text") => {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    if !t.trim().is_empty() {
                        extra.push(t.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    if out.is_empty() {
        return None;
    }
    if !extra.is_empty() {
        if let Some(last) = out.last_mut() {
            last.1.push_str("\n\n");
            last.1.push_str(&extra.join("\n\n"));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protobuf::{Reader, Value as PbValue};
    use serde_json::json;

    fn sub(buf: &[u8], field: u32) -> Option<&[u8]> {
        Reader::new(buf).find_map(|(f, v)| match (f, v) {
            (ff, PbValue::Len(s)) if ff == field => Some(s),
            _ => None,
        })
    }

    #[test]
    fn mcp结果帧字段号与外壳() {
        let p = encode_mcp_result(7, "ex-7", "文件内容", false);
        let ecm = sub(&p, 2).expect("顶层 2 = exec 客户端消息");
        let id = Reader::new(ecm).find_map(|(f, v)| match (f, v) {
            (1, PbValue::Varint(n)) => Some(n),
            _ => None,
        });
        assert_eq!(id, Some(7));
        assert_eq!(sub(ecm, 15), Some(&b"ex-7"[..]));
        let res = sub(ecm, 11).expect("11 = mcp_result");
        let success = sub(res, 1).expect("1 = success");
        let item = sub(success, 1).expect("content[0]");
        let tc = sub(item, 1).expect("text content");
        assert_eq!(sub(tc, 1), Some("文件内容".as_bytes()));
        assert!(
            !Reader::new(success).any(|(f, _)| f == 2),
            "非出错时不写 is_error"
        );
        let e = encode_mcp_result(1, "x", "boom", true);
        let s = sub(sub(sub(&e, 2).unwrap(), 11).unwrap(), 1).unwrap();
        assert!(Reader::new(s).any(|(f, v)| f == 2 && matches!(v, PbValue::Varint(1))));
    }

    fn pending(kind: PendingKind) -> PendingExec {
        PendingExec { tool_use_id: "t".into(), id: 3, exec_id: "ex-3".into(), kind }
    }

    /// 读文件结果:`{2:{1:3, 15:"ex-3", 7:{1:{1:path, 2:content}}}}`,不带总行数;出错走 `7.2`。
    #[test]
    fn 自带读文件结果帧() {
        let f = encode_result(&pending(PendingKind::Read { path: "/w/a.txt".into() }), "第一行\n第二行", false);
        assert_eq!(f.len(), 1);
        let ecm = sub(&f[0], 2).unwrap();
        assert_eq!(sub(ecm, 15), Some(&b"ex-3"[..]));
        let ok = sub(sub(ecm, 7).expect("7 = read_result"), 1).expect("1 = success");
        assert_eq!(sub(ok, 1), Some(&b"/w/a.txt"[..]));
        assert_eq!(sub(ok, 2), Some("第一行\n第二行".as_bytes()));
        assert!(!Reader::new(ok).any(|(f, _)| f == 3), "不报总行数");
        let e = encode_result(&pending(PendingKind::Read { path: "/w/x".into() }), "File does not exist", true);
        let err = sub(sub(sub(&e[0], 2).unwrap(), 7).unwrap(), 2).expect("2 = error");
        assert_eq!(sub(err, 1), Some(&b"/w/x"[..]));
        assert_eq!(sub(err, 2), Some(&b"File does not exist"[..]));
    }

    /// 流式终端:start → stdout → exit → 关流,共 4 帧;出错退出码 1。
    #[test]
    fn 自带流式终端结果帧序() {
        let k = PendingKind::ShellStream { cwd: "/w".into() };
        let f = encode_result(&pending(k.clone()), "a.txt", false);
        assert_eq!(f.len(), 4);
        let ev = |b: &[u8]| sub(sub(b, 2).unwrap(), 14).unwrap().to_vec();
        assert!(sub(&ev(&f[0]), 4).is_some(), "第 1 帧 start");
        assert_eq!(sub(sub(&ev(&f[1]), 1).unwrap(), 1), Some(&b"a.txt"[..]), "第 2 帧 stdout");
        let exit = ev(&f[2]);
        let exit = sub(&exit, 3).expect("第 3 帧 exit");
        assert!(!Reader::new(exit).any(|(f, v)| f == 1 && matches!(v, PbValue::Varint(n) if n != 0)), "成功退出码 0");
        assert_eq!(sub(exit, 2), Some(&b"/w"[..]));
        assert_eq!(f[3], crate::exec::stream_close_frame(3), "第 4 帧关的是本 exec id");
        let e = encode_result(&pending(k), "boom", true);
        let exit = sub(sub(sub(&e[2], 2).unwrap(), 14).unwrap(), 3).unwrap().to_vec();
        assert!(Reader::new(&exit).any(|(f, v)| f == 1 && matches!(v, PbValue::Varint(1))));
    }

    /// MCP 照旧走 `11`。
    #[test]
    fn mcp结果帧不变() {
        let m = encode_result(&pending(PendingKind::Mcp), "r", false);
        assert_eq!(m, vec![encode_mcp_result(3, "ex-3", "r", false)]);
        assert!(sub(sub(&m[0], 2).unwrap(), 11).is_some());
    }

    #[test]
    fn 超长结果按行截断并附说明() {
        let long: String = (0..5000).map(|i| format!("第 {i} 行\n")).collect();
        let fitted = fit_tool_output(&long, 2000);
        assert!(fitted.len() <= 2000 + 100, "{}", fitted.len());
        assert!(fitted.starts_with("第 0 行"));
        assert!(fitted.contains("已截断"));
        // 单行超长:按字节截且不劈开汉字。
        let one = "汉".repeat(5000);
        let f1 = fit_tool_output(&one, 1000);
        assert!(f1.contains("已截断"));
        assert_eq!(fit_tool_output("短", 1000), "短");
    }

    #[test]
    fn 工具结果带id与出错标记且附上同条文字() {
        let body = json!({"messages": [
            {"role": "user", "content": "改一下"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_a", "name": "read", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_a", "content": "内容A"},
                {"type": "tool_result", "tool_use_id": "toolu_b", "content": [{"type": "text", "text": "坏了"}], "is_error": true},
                {"type": "text", "text": "<system-reminder>提醒</system-reminder>"}
            ]}
        ]});
        let r = tool_results(&body).unwrap();
        assert_eq!(r[0], ("toolu_a".into(), "内容A".into(), false));
        assert_eq!(r[1].0, "toolu_b");
        assert!(r[1].2);
        assert!(r[1].1.starts_with("坏了") && r[1].1.contains("提醒"), "{:?}", r[1]);
        let no_id = json!({"messages": [{"role": "user", "content": [{"type": "tool_result", "content": "x"}]}]});
        assert!(tool_results(&no_id).is_none(), "缺 id 不续跑");
    }

    fn dummy_run(ids: &[&str]) -> (HeldRun, tokio::sync::mpsc::Receiver<Result<bytes::Bytes, std::io::Error>>) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let run = HeldRun::new(
            "conv".into(),
            Box::pin(futures::stream::pending()),
            FrameDecoder::new(),
            tx,
            None,
            ids.iter()
                .enumerate()
                .map(|(i, id)| PendingExec { tool_use_id: (*id).into(), id: i as u64 + 1, exec_id: format!("e{i}"), kind: PendingKind::Mcp })
                .collect(),
        );
        (run, rx)
    }

    #[tokio::test]
    async fn 只按完全相同的id集合取出且取出即摘掉() {
        let reg = HeldRuns::default();
        let (run, _rx) = dummy_run(&["a", "b"]);
        reg.insert("acc", run);
        assert!(reg.take("acc", &["a"]).is_none(), "少一个结果不续");
        assert!(reg.take("acc", &["a", "b", "c"]).is_none(), "多一个结果不续");
        assert!(reg.take("other", &["a", "b"]).is_none(), "别的号取不到");
        assert_eq!(reg.len(), 1, "对不上时表不动");
        let got = reg.take("acc", &["b", "a"]).expect("顺序无关");
        assert_eq!(got.pending.len(), 2);
        assert_eq!(reg.len(), 0, "取出即摘掉");
        assert!(reg.take("acc", &["a", "b"]).is_none(), "不能取两次");
    }

    #[tokio::test]
    async fn 每号超额回收最旧的且按凭证可摘() {
        let reg = HeldRuns::default();
        let mut tokens = Vec::new();
        let mut rxs = Vec::new();
        for i in 0..(MAX_PER_ACCOUNT + 2) {
            let id = format!("t{i}");
            let (run, rx) = dummy_run(&[id.as_str()]);
            rxs.push(rx);
            tokens.push(reg.insert("acc", run));
        }
        assert_eq!(reg.len(), MAX_PER_ACCOUNT);
        assert!(reg.take("acc", &["t0"]).is_none(), "最旧的已被回收");
        assert!(reg.remove("acc", *tokens.last().unwrap()));
        assert_eq!(reg.len(), MAX_PER_ACCOUNT - 1);
        assert_eq!(reg.clear(), MAX_PER_ACCOUNT - 1);
    }

    #[tokio::test]
    async fn 回收挂起的流会停掉心跳并关闭请求体() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let reg = HeldRuns::default();
        let run = HeldRun::new(
            "conv".into(),
            Box::pin(futures::stream::pending()),
            FrameDecoder::new(),
            tx,
            None,
            vec![PendingExec { tool_use_id: "a".into(), id: 1, exec_id: "e".into(), kind: PendingKind::Mcp }],
        );
        reg.insert("acc", run);
        assert_eq!(reg.clear(), 1);
        assert!(rx.recv().await.is_none(), "发送端随挂起的流一起 drop,请求体关闭");
    }
}
