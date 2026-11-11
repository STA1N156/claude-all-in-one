//! exec 通道(响应顶层 field 2 = `ExecServerMessage`)的解析与握手应答。
//!
//! # 为什么单独成模块
//!
//! 服务端在 `AgentService/Run` 的双向流里会向**客户端**发 exec 请求,客户端在请求侧回
//! `ExecClientMessage`。其中有两类不是「模型要执行什么」,而是**握手**:
//!
//! - `36 mcp_state`:问客户端有哪些 MCP server、各有哪些工具。**阻塞式** —— 不回就只剩心跳。
//! - `17 list_mcp_resources`:问 MCP 资源列表,同样阻塞。
//!
//! 旧实现(`run::is_tool_call`)把顶层 field 2 的帧一律当「工具调用」,握手帧于是落进
//! 「Cursor 内建工具 → 收口」分支:10-03 起 CLI 形态带工具的请求 44 次 0 次交付,就是这条。
//! 对照 OmniRoute(MIT,PR #14737,字段号取自 Cursor Agent CLI 自带的 agent.v1 schema)补齐。
//!
//! # 帧形状
//!
//! ```text
//! 服务端 → {2: {1: id(varint), 15: exec_id(str), <分支字段>: {...}, 19: span_context, 57: machine_id}}
//! 客户端 → {2: {1: id, 15: exec_id, <同号结果字段>: {...}}}     // 外壳见 run::exec_client_frame
//! ```
//!
//! `id` / `exec_id` 原样带回才对得上;实测握手帧两者常缺省(0 / ""),缺省时外壳也不写。

use crate::protobuf::{Reader, Value as PbValue, Writer};
use crate::run::{ToolCall, ToolDef};

/// 响应顶层的 exec 通道。
const EXEC_CHANNEL: u32 = 2;
const EXEC_ID: u32 = 1;
const EXEC_EXEC_ID: u32 = 15;
/// 不是分支、而是元数据的 length-delimited 字段:exec_id / span_context / machine_id。
const META_FIELDS: &[u32] = &[EXEC_EXEC_ID, 19, 57];

/// `request_context_args`。CLI 形态由我方在请求侧**主动**推上下文帧
/// (`cli::build_context_frame_cli`),收到它只当会话登记通知(见 `run::is_session_notice`)。
pub(crate) const VAR_REQUEST_CONTEXT: u32 = 10;
/// `mcp_args`:模型调用 MCP 工具。
pub(crate) const VAR_MCP: u32 = 11;
/// `list_mcp_resources_exec_args`(阻塞握手)。
pub(crate) const VAR_LIST_MCP_RESOURCES: u32 = 17;
/// `mcp_state_exec_args`(阻塞握手)。
pub(crate) const VAR_MCP_STATE: u32 = 36;

/// 一帧 exec 请求的外壳。
#[derive(Debug)]
pub(crate) struct ExecFrame<'a> {
    pub id: u64,
    pub exec_id: String,
    /// 分支字段号(10 / 11 / 17 / 36 / 内建工具各号 …)。
    pub variant: u32,
    pub body: &'a [u8],
}

/// 解出响应**顶层** field 2 的 exec 请求。工具调用的界面帧(`1.2`,裹在 field 1 里)
/// 不归这里管。认不出分支字段(只有元数据)→ `None`。
pub(crate) fn parse(payload: &[u8]) -> Option<ExecFrame<'_>> {
    let ch = Reader::new(payload).find_map(|(f, v)| match (f, v) {
        (EXEC_CHANNEL, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    let mut id = 0u64;
    let mut exec_id = String::new();
    let mut variant: Option<(u32, &[u8])> = None;
    for (f, v) in Reader::new(ch) {
        match (f, v) {
            (EXEC_ID, PbValue::Varint(n)) => id = n,
            (EXEC_EXEC_ID, PbValue::Len(s)) => exec_id = String::from_utf8_lossy(s).into_owned(),
            (f, PbValue::Len(s)) if !META_FIELDS.contains(&f) && variant.is_none() => {
                variant = Some((f, s))
            }
            _ => {}
        }
    }
    let (variant, body) = variant?;
    Some(ExecFrame { id, exec_id, variant, body })
}

/// 需要我方应答的握手。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Handshake {
    /// 服务端点名要的 server(可为空 = 「你有的全部」)。
    McpState { servers: Vec<String> },
    ListMcpResources,
}

impl ExecFrame<'_> {
    pub(crate) fn handshake(&self) -> Option<Handshake> {
        match self.variant {
            VAR_MCP_STATE => {
                // McpStateExecArgs.server_identifiers = repeated string(field 1)。
                let servers = Reader::new(self.body)
                    .filter_map(|(f, v)| match (f, v) {
                        (1, PbValue::Len(s)) => Some(String::from_utf8_lossy(s).into_owned()),
                        _ => None,
                    })
                    .collect();
                Some(Handshake::McpState { servers })
            }
            VAR_LIST_MCP_RESOURCES => Some(Handshake::ListMcpResources),
            _ => None,
        }
    }

    /// 握手应答(未分帧的 payload)。`tools` = 调用方声明的工具(裸名);
    /// `default_ident` = 服务端没点名时用的服务名(CLI 形态 `claude-local`,IDE 形态 `gwtools`)。
    pub(crate) fn reply(&self, h: &Handshake, tools: &[ToolDef], default_ident: &str) -> Vec<u8> {
        match h {
            Handshake::McpState { servers } => {
                // 服务端点名了哪个就答哪个(首轮会点名、之后传空表);没点名用我方的标识。
                let ident = servers
                    .iter()
                    .find(|s| !s.trim().is_empty())
                    .map(String::as_str)
                    .unwrap_or(default_ident);
                encode_mcp_state_result(self.id, &self.exec_id, ident, tools)
            }
            Handshake::ListMcpResources => encode_list_mcp_resources_result(self.id, &self.exec_id),
        }
    }
}

/// 这一帧是不是握手。主路径在收帧循环里先 `parse` 再应答(早于排水段与工具臂),
/// 用不到这个谓词;留给测试断言「握手 / 非握手」的分界。
#[cfg(test)]
pub(crate) fn is_handshake(payload: &[u8]) -> bool {
    parse(payload).is_some_and(|f| f.handshake().is_some())
}

/// `mcp_state_exec_result{1: success{1: servers{1: name, 2: identifier, 5: tools…, 7: "ready"}}}`。
///
/// server 没有标成 ready 或缺席,模型会报「命名空间不可用」并退回内建工具。
pub(crate) fn encode_mcp_state_result(
    id: u64,
    exec_id: &str,
    server_identifier: &str,
    tools: &[ToolDef],
) -> Vec<u8> {
    let mut server = Writer::new();
    server.string(1, server_identifier);
    server.string(2, server_identifier);
    // IDE 形态的工具在帧0 注册在 `gwtools` 下(全名 `gwtools-<名>`);问到它时按帧0 同一份
    // 编码报回去。2026-11-07 生产:这里曾对 gwtools 报 0 个工具,模型以为该服务没授权,
    // 去调 Cursor 的 mcp_auth,服务端请示后干等(见 `crate::interaction`)。
    let ide = server_identifier == crate::run::TOOL_NS;
    for t in tools {
        let def = if ide { t.encode() } else { crate::cli::mcp_tool_definition(t) };
        server.message(5, &def);
    }
    server.string(7, "ready");
    let mut success = Writer::new();
    success.message(1, &server);
    let mut result = Writer::new();
    result.message(1, &success);
    crate::run::exec_client_frame(id, exec_id, VAR_MCP_STATE, &result)
}

/// `list_mcp_resources_exec_result{1: success{}}` —— 我们只暴露工具,不暴露资源。
pub(crate) fn encode_list_mcp_resources_result(id: u64, exec_id: &str) -> Vec<u8> {
    let mut result = Writer::new();
    result.message(1, &Writer::new());
    crate::run::exec_client_frame(id, exec_id, VAR_LIST_MCP_RESOURCES, &result)
}

// ── 内建工具(模型调了 Cursor 自带的工具)─────────────────────────────────────────
//
// 分支字段号与结果字段号同号(请求 `2 shell_args` ↔ 应答 `2 shell_result`),拒绝成员
// 的字段号逐类不同。全部对照 OmniRoute `execResults.ts` / `execRejections.ts`
// (取自 Cursor Agent CLI 自带 schema)。

const VAR_SHELL: u32 = 2;
const VAR_WRITE: u32 = 3;
const VAR_DELETE: u32 = 4;
const VAR_GREP: u32 = 5;
pub(crate) const VAR_READ: u32 = 7;
const VAR_LS: u32 = 8;
const VAR_DIAGNOSTICS: u32 = 9;
pub(crate) const VAR_SHELL_STREAM: u32 = 14;
const VAR_BG_SHELL: u32 = 16;
const VAR_FETCH: u32 = 20;
const VAR_WRITE_SHELL_STDIN: u32 = 23;
/// `subagent_args`:模型要派一个 Cursor 子代理(2026-11-07 生产实物:`generalPurpose` +
/// 「你是一名只读审查者…」)。字段:1 tool_call_id、2 subagent_type、3 model_id、4 prompt。
const VAR_SUBAGENT: u32 = 28;

/// 模型调用的 Cursor 内建工具(exec 形态,带参数)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Builtin {
    Shell { command: String, cwd: String },
    ShellStream { command: String, cwd: String },
    BackgroundShell { command: String, cwd: String },
    Read { path: String },
    Write { path: String, text: String },
    Delete { path: String },
    Ls { path: String },
    Grep { pattern: String, path: String, glob: String },
    Fetch { url: String },
    Diagnostics,
    WriteShellStdin,
    /// 子代理:翻成调用方自己的子代理工具(OpenCode `task` / Claude Code `Task`)。
    Subagent { tool_call_id: String, subagent_type: String, prompt: String },
}

fn str_field(body: &[u8], field: u32) -> String {
    Reader::new(body)
        .find_map(|(f, v)| match (f, v) {
            (ff, PbValue::Len(s)) if ff == field => Some(String::from_utf8_lossy(s).into_owned()),
            _ => None,
        })
        .unwrap_or_default()
}

impl ExecFrame<'_> {
    /// 这一帧是不是内建工具调用;是就解出参数。握手 / MCP / request_context → `None`。
    pub(crate) fn builtin(&self) -> Option<Builtin> {
        let b = self.body;
        Some(match self.variant {
            VAR_SHELL => Builtin::Shell { command: str_field(b, 1), cwd: str_field(b, 2) },
            VAR_SHELL_STREAM => Builtin::ShellStream { command: str_field(b, 1), cwd: str_field(b, 2) },
            VAR_BG_SHELL => Builtin::BackgroundShell { command: str_field(b, 1), cwd: str_field(b, 2) },
            VAR_READ => Builtin::Read { path: str_field(b, 1) },
            VAR_WRITE => Builtin::Write { path: str_field(b, 1), text: str_field(b, 2) },
            VAR_DELETE => Builtin::Delete { path: str_field(b, 1) },
            VAR_LS => Builtin::Ls { path: str_field(b, 1) },
            VAR_GREP => Builtin::Grep {
                pattern: str_field(b, 1),
                path: str_field(b, 2),
                glob: str_field(b, 3),
            },
            VAR_FETCH => Builtin::Fetch { url: str_field(b, 1) },
            VAR_DIAGNOSTICS => Builtin::Diagnostics,
            VAR_WRITE_SHELL_STDIN => Builtin::WriteShellStdin,
            VAR_SUBAGENT => Builtin::Subagent {
                tool_call_id: str_field(b, 1),
                subagent_type: str_field(b, 2),
                prompt: str_field(b, 4),
            },
            _ => return None,
        })
    }
}

impl Builtin {
    /// 能映射成「调用方声明的同类工具」的,先转成兼容转换层认得的形状
    /// (`run::BuiltinCall`,由 `chat::translate_builtin` 按调用方工具表落地)。
    ///
    /// 列目录 / grep 交给 `chat::translate_builtin`:优先调用方自己的搜索工具,没有才按调用方
    /// 终端类型(PowerShell / POSIX)拼命令;删除、后台终端、诊断等**不映射**(删除是不可逆
    /// 操作,不替模型做主),走拒绝。
    pub(crate) fn to_builtin_call(&self, id: &str) -> Option<crate::run::BuiltinCall> {
        use crate::run::BuiltinCall as B;
        let id = id.to_string();
        let nonempty = |s: &str| !s.trim().is_empty();
        match self {
            Builtin::Shell { command, cwd } | Builtin::ShellStream { command, cwd } if nonempty(command) => {
                let cwd = Some(cwd.clone()).filter(|d| nonempty(d));
                Some(B::Terminal { id, command: command.clone(), cwd })
            }
            Builtin::Read { path } if nonempty(path) => {
                Some(B::ReadFile { id, path: path.clone(), offset: None, limit: None })
            }
            Builtin::Write { path, text } if nonempty(path) => {
                Some(B::WriteFile { id, path: path.clone(), content: text.clone() })
            }
            Builtin::Fetch { url } if nonempty(url) => Some(B::WebFetch { id, url: url.clone() }),
            Builtin::Ls { path } => {
                let p = if nonempty(path) { path.clone() } else { ".".to_string() };
                Some(B::ListDir { id, path: p })
            }
            Builtin::Grep { pattern, path, glob } if nonempty(pattern) => Some(B::Grep {
                id,
                pattern: pattern.clone(),
                path: Some(path.clone()).filter(|p| nonempty(p)),
                glob: Some(glob.clone()).filter(|g| nonempty(g)),
            }),
            Builtin::Subagent { tool_call_id, subagent_type, prompt } if nonempty(prompt) => {
                // 服务端给的 tool_call_id 就是模型侧的 id(toolu_…),有就沿用。
                let id = if nonempty(tool_call_id) { tool_call_id.clone() } else { id };
                Some(B::Task { id, prompt: prompt.clone(), subagent_type: subagent_type.clone() })
            }
            _ => None,
        }
    }

    /// 带类型的拒绝(未分帧的 payload 列表;流式终端要两帧:拒绝 + 关流)。
    ///
    /// 拒绝而不是收口:模型收到「这个工具不可用」会改用别的办法继续,而收口给客户的是
    /// 一个没头没尾的空回答(10-03 起的老毛病)。
    pub(crate) fn rejection(&self, f: &ExecFrame, reason: &str) -> Vec<Vec<u8>> {
        let path_rej = |path: &str| {
            let mut w = Writer::new();
            w.string(1, path);
            w.string(2, reason);
            w
        };
        let shell_rej = |command: &str, cwd: &str| {
            let mut w = Writer::new();
            w.string(1, command);
            w.string(2, cwd);
            w.string(3, reason);
            w
        };
        let wrap = |result_field: u32, member: u32, inner: Writer| {
            let mut r = Writer::new();
            r.message(member, &inner);
            crate::run::exec_client_frame(f.id, &f.exec_id, result_field, &r)
        };
        match self {
            Builtin::Read { path } => vec![wrap(VAR_READ, 3, path_rej(path))],
            Builtin::Ls { path } => vec![wrap(VAR_LS, 3, path_rej(path))],
            Builtin::Write { path, .. } => vec![wrap(VAR_WRITE, 6, path_rej(path))],
            Builtin::Delete { path } => vec![wrap(VAR_DELETE, 6, path_rej(path))],
            Builtin::Shell { command, cwd } => vec![wrap(VAR_SHELL, 4, shell_rej(command, cwd))],
            Builtin::BackgroundShell { command, cwd } => {
                vec![wrap(VAR_BG_SHELL, 3, shell_rej(command, cwd))]
            }
            Builtin::ShellStream { command, cwd } => {
                // 流式终端的拒绝是一个终止事件(ShellStream.rejected=5),随后关流
                // (`{5: exec_client_control{1: stream_close{1: id}}}`)。
                let rej = wrap(VAR_SHELL_STREAM, 5, shell_rej(command, cwd));
                vec![rej, stream_close_frame(f.id)]
            }
            Builtin::Grep { .. } | Builtin::WriteShellStdin | Builtin::Subagent { .. } => {
                // 三者的结果都是 oneof {1 success, 2 error},error 首字段是原因文本。
                let mut e = Writer::new();
                e.string(1, reason);
                let field = match self {
                    Builtin::Grep { .. } => VAR_GREP,
                    Builtin::Subagent { .. } => VAR_SUBAGENT,
                    _ => VAR_WRITE_SHELL_STDIN,
                };
                vec![wrap(field, 2, e)]
            }
            Builtin::Fetch { url } => {
                let mut e = Writer::new();
                e.string(1, url);
                e.string(2, reason);
                vec![wrap(VAR_FETCH, 2, e)]
            }
            // 诊断没有拒绝成员:回空的成功。
            Builtin::Diagnostics => vec![wrap(VAR_DIAGNOSTICS, 1, Writer::new())],
        }
    }
}

/// 流式终端的关流控制帧(未分帧):`{5: exec_client_control{1: stream_close{1: id}}}`。
///
/// 流式终端无论拒绝还是回结果,最后都要关流,否则服务端一直等后续事件。
pub(crate) fn stream_close_frame(id: u64) -> Vec<u8> {
    let mut close_inner = Writer::new();
    close_inner.uint(1, id);
    let mut ctl = Writer::new();
    ctl.message(1, &close_inner);
    let mut top = Writer::new();
    top.message(5, &ctl);
    top.into_bytes()
}

/// 可挂流续跑的自带工具 exec:`(挂起种类, tool_call_id)`。
///
/// 只认两种(2026-11-10 实验号实测结果格式服务端认、模型读得到,见 `crate::held::PendingKind`):
/// 读文件 `read_args`(7,`ReadArgs{1 path, 2 tool_call_id}`)与流式终端 `shell_stream_args`
/// (14,`ShellArgs{1 command, 2 working_directory, 4 tool_call_id}`)。普通终端 `shell_args`(2)
/// CLI 实测从不出现、结果格式也没验证过,不挂。
/// 缺 `tool_call_id` 不认 —— 配不上界面帧那次调用就不挂,宁可退回旧行为也不把结果配错调用。
pub(crate) fn holdable_exec(f: &ExecFrame) -> Option<(crate::held::PendingKind, String)> {
    use crate::held::PendingKind as K;
    let (kind, call_id) = match f.variant {
        VAR_READ => (K::Read { path: str_field(f.body, 1) }, str_field(f.body, 2)),
        VAR_SHELL_STREAM => (K::ShellStream { cwd: str_field(f.body, 2) }, str_field(f.body, 4)),
        _ => return None,
    };
    (!call_id.trim().is_empty()).then_some((kind, call_id))
}

/// 拒绝内建工具时给模型看的理由。
pub(crate) const BUILTIN_REJECT_REASON: &str =
    "Tool not available in this environment. Use the provided tools instead.";

/// 界面帧 `1.2`(tool_call_started)里的工具类型:`.44` = 经 MCP 元工具发起的动态工具调用。
const UI_DYNAMIC_MCP: u32 = 44;

/// 这一帧是不是「MCP 工具调用开始」的界面帧(`1.2.2.44{1:{1: server, 2: tool, 4: id}}`)。
///
/// 2026-11-06 线协议实测(claude-sonnet-5-5,补上 RequestContext 的 `.34` 描述符之后):
/// 模型调我们的工具时,界面帧的工具类型从旧的 `.15` 变成了 `.44`,而且**不带参数**——
/// 参数随后在 exec 形态的 mcp_args(field 11)里才到。旧代码把 `.44` 当成 Cursor 内建
/// 工具,参数没到就收口截断。调用方见到它应当**继续读**,等 mcp_args 再交付。
pub(crate) fn is_dynamic_mcp_started(payload: &[u8]) -> bool {
    let Some(msg) = Reader::new(payload).find_map(|(f, v)| match (f, v) {
        (1, PbValue::Len(s)) => Some(s),
        _ => None,
    }) else {
        return false;
    };
    let Some(started) = Reader::new(msg).find_map(|(f, v)| match (f, v) {
        (2, PbValue::Len(s)) => Some(s),
        _ => None,
    }) else {
        return false;
    };
    Reader::new(started).any(|(f, v)| match (f, v) {
        (2, PbValue::Len(detail)) => {
            Reader::new(detail).any(|(f2, v2)| f2 == UI_DYNAMIC_MCP && matches!(v2, PbValue::Len(_)))
        }
        _ => false,
    })
}

/// exec 形态 MCP 调用的 `(tool_call_id, 工具名)`(不合成缺省 id:挂流续跑要按真值配对)。
pub(crate) fn mcp_ids(f: &ExecFrame) -> Option<(String, String)> {
    if f.variant != VAR_MCP {
        return None;
    }
    let mut name = str_field(f.body, 5);
    if name.is_empty() {
        name = str_field(f.body, 1);
    }
    Some((str_field(f.body, 3), name))
}

/// exec 形态的 MCP 调用(`mcp_args{1: name, 2: map<key, Value>…, 3: tool_call_id, 5: tool_name}`)。
///
/// 正常情况下同一次调用先来界面帧 `1.2.2.15`(`run::parse_tool_call` 解它),exec 这份是
/// 「回声」。只有界面帧缺席时,调用方才用这里兜底 —— OmniRoute 只认 exec 这份,说明
/// 服务端完全可能只发它。
pub(crate) fn mcp_call(payload: &[u8]) -> Option<ToolCall> {
    let f = parse(payload)?;
    if f.variant != VAR_MCP {
        return None;
    }
    let mut name = String::new();
    let mut fallback_name = String::new();
    let mut call_id = String::new();
    let mut args = Vec::new();
    for (fnum, v) in Reader::new(f.body) {
        match (fnum, v) {
            (5, PbValue::Len(s)) => name = String::from_utf8_lossy(s).into_owned(),
            (1, PbValue::Len(s)) => fallback_name = String::from_utf8_lossy(s).into_owned(),
            (3, PbValue::Len(s)) => call_id = String::from_utf8_lossy(s).into_owned(),
            (2, PbValue::Len(entry)) => {
                let mut k = String::new();
                let mut val = serde_json::Value::Null;
                for (ef, ev) in Reader::new(entry) {
                    match (ef, ev) {
                        (1, PbValue::Len(b)) => k = String::from_utf8_lossy(b).into_owned(),
                        (2, PbValue::Len(b)) => val = crate::run::decode_pb_value(b, 0),
                        _ => {}
                    }
                }
                if !k.is_empty() {
                    args.push((k, val));
                }
            }
            _ => {}
        }
    }
    if name.is_empty() {
        name = fallback_name;
    }
    if name.is_empty() {
        return None;
    }
    // 空 id 与 run::parse_tool_call 同一口径:合成而不是丢调用。
    if call_id.is_empty() {
        call_id = format!("call_{}", uuid::Uuid::new_v4().simple());
    }
    Some(ToolCall { id: call_id, name, args })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手写:`{2: {1: 7, 0xA2 0x02(=field 36,LEN) {1:"claude-local"}, 15: "ex1", 19: {1:"t"}}}`。
    fn mcp_state_req() -> Vec<u8> {
        let mut inner = vec![0x08, 0x07]; // id = 7
        inner.extend([0xA2, 0x02, 0x0E, 0x0A, 0x0C]); // field 36,len 14 → {1: len 12}
        inner.extend(b"claude-local");
        inner.extend([0x7A, 0x03]); // field 15,len 3
        inner.extend(b"ex1");
        inner.extend([0x9A, 0x01, 0x03, 0x0A, 0x01, b't']); // field 19 span_context(元数据)
        let mut top = vec![0x12, inner.len() as u8];
        top.extend(inner);
        top
    }

    #[test]
    fn 认出_mcp_state_握手并取出点名的_server() {
        let req = mcp_state_req();
        let f = parse(&req).expect("exec 帧");
        assert_eq!((f.id, f.exec_id.as_str(), f.variant), (7, "ex1", VAR_MCP_STATE));
        assert_eq!(
            f.handshake(),
            Some(Handshake::McpState { servers: vec!["claude-local".into()] })
        );
        assert!(is_handshake(&req));
    }

    #[test]
    fn list_mcp_resources_应答字节() {
        // 手写期望:{2: {1:7, 15:"ex1", 17:{1:{}}}}
        //   内层 = 08 07 | 7A 03 'e' 'x' '1' | 8A 01 02 0A 00 → 12 字节
        let got = encode_list_mcp_resources_result(7, "ex1");
        let want = [
            0x12, 0x0C, 0x08, 0x07, 0x7A, 0x03, b'e', b'x', b'1', 0x8A, 0x01, 0x02, 0x0A, 0x00,
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn mcp_state_应答字节_无工具() {
        // {2: {36: {1: {1: {1:"s", 2:"s", 7:"ready"}}}}},id/exec_id 缺省不写。
        //   server = 0A 01 's' 12 01 's' 3A 05 "ready"          → 13 字节
        //   success = 0A 0D <server>                             → 15
        //   result  = 0A 0F <success>                            → 17
        //   ecm     = A2 02 11 <result>                          → 20
        let got = encode_mcp_state_result(0, "", "s", &[]);
        let mut want = vec![0x12, 20, 0xA2, 0x02, 17, 0x0A, 15, 0x0A, 13];
        want.extend([0x0A, 0x01, b's', 0x12, 0x01, b's', 0x3A, 0x05]);
        want.extend(b"ready");
        assert_eq!(got, want);
    }

    #[test]
    fn mcp_state_应答带上工具定义且与帧0同一份编码() {
        let t = ToolDef {
            name: "Read".into(),
            description: "读文件".into(),
            schema: r#"{"type":"object"}"#.into(),
        };
        let got = encode_mcp_state_result(3, "x", "claude-local", std::slice::from_ref(&t));
        let one = crate::cli::mcp_tool_definition(&t).into_bytes();
        // 工具定义原样嵌在 server.5 里(同一份编码,不各写各的)。
        assert!(got.windows(one.len()).any(|w| w == one.as_slice()));
        assert!(got.windows(5).any(|w| w == b"ready"));
    }

    /// 2026-11-07 生产:IDE 形态服务端问 `gwtools`,曾回 0 个工具 → 模型去调 mcp_auth 干等。
    /// 现在按帧0 注册的同一份编码报回(全名 `gwtools-Read`、provider `gwtools`)。
    #[test]
    fn 问到gwtools按帧0编码报工具() {
        let t = ToolDef {
            name: "Read".into(),
            description: "读文件".into(),
            schema: r#"{"type":"object"}"#.into(),
        };
        let got = encode_mcp_state_result(3, "x", "gwtools", std::slice::from_ref(&t));
        let frame0 = t.encode().into_bytes();
        assert!(got.windows(frame0.len()).any(|w| w == frame0.as_slice()));
        assert!(got.windows(12).any(|w| w == b"gwtools-Read"));
        // 不能混进 CLI 口径的 claude-local 定义。
        assert!(!got.windows(12).any(|w| w == b"claude-local"));

        // IDE 形态服务端没点名:默认服务名是 gwtools。
        let empty = ExecFrame { id: 0, exec_id: String::new(), variant: VAR_MCP_STATE, body: &[] };
        let r = empty.reply(&empty.handshake().unwrap(), std::slice::from_ref(&t), crate::run::TOOL_NS);
        assert!(r.windows(frame0.len()).any(|w| w == frame0.as_slice()));
    }

    #[test]
    fn 握手按点名作答_没点名用我方标识() {
        let req = mcp_state_req();
        let f = parse(&req).unwrap();
        let h = f.handshake().unwrap();
        let r = f.reply(&h, &[], crate::cli::MCP_PROVIDER_CLAUDE_LOCAL);
        assert!(r.windows(12).any(|w| w == b"claude-local"));

        let empty = ExecFrame { id: 0, exec_id: String::new(), variant: VAR_MCP_STATE, body: &[] };
        let r2 = empty.reply(&empty.handshake().unwrap(), &[], crate::cli::MCP_PROVIDER_CLAUDE_LOCAL);
        assert!(r2.windows(12).any(|w| w == b"claude-local"));
    }

    #[test]
    fn 内建工具与_request_context_不是握手() {
        // {2: {7: {1: "/a"}}} = read_args
        let read = [0x12, 0x06, 0x3A, 0x04, 0x0A, 0x02, b'/', b'a'];
        assert!(parse(&read).is_some_and(|f| f.variant == 7));
        assert!(!is_handshake(&read));
        // {2: {10: {2: "c"}}} = request_context(会话登记通知)
        let rc = [0x12, 0x05, 0x52, 0x03, 0x12, 0x01, b'c'];
        assert!(parse(&rc).is_some_and(|f| f.variant == VAR_REQUEST_CONTEXT));
        assert!(!is_handshake(&rc));
        // 界面帧 1.2(裹在 field 1 里)不归这里。
        let ui = [0x0A, 0x04, 0x12, 0x02, 0x0A, 0x00];
        assert!(parse(&ui).is_none());
    }

    fn frame_of(variant_tag: &[u8], body: &[u8], id: u8) -> Vec<u8> {
        let mut inner = vec![0x08, id];
        inner.extend(variant_tag);
        inner.push(body.len() as u8);
        inner.extend(body);
        let mut top = vec![0x12, inner.len() as u8];
        top.extend(inner);
        top
    }

    #[test]
    fn 内建列目录_解析_映射成列目录_拒绝字节() {
        // {2: {1:4, 8: {1:"/w"}}}  (field 8 LEN tag = 0x42)
        let raw = frame_of(&[0x42], &[0x0A, 0x02, b'/', b'w'], 4);
        let f = parse(&raw).unwrap();
        let b = f.builtin().unwrap();
        assert_eq!(b, Builtin::Ls { path: "/w".into() });
        assert_eq!(
            b.to_builtin_call("x"),
            Some(crate::run::BuiltinCall::ListDir { id: "x".into(), path: "/w".into() })
        );
        // 拒绝:{2: {1:4, 8: {3: {1:"/w", 2:"R"}}}}
        //   path_rej = 0A 02 "/w" 12 01 "R"  (7)  → member 3: 1A 07 …  (9) → field 8: 42 09 … (11)
        let got = b.rejection(&f, "R");
        let mut want = vec![0x12, 13, 0x08, 0x04, 0x42, 0x09, 0x1A, 0x07, 0x0A, 0x02, b'/', b'w'];
        want.extend([0x12, 0x01, b'R']);
        assert_eq!(got, vec![want]);
    }

    #[test]
    fn 流式终端拒绝是两帧_拒绝加关流() {
        // shell_stream_args = {1:"ls", 2:"/"},field 14 LEN tag = 0x72
        let raw = frame_of(&[0x72], &[0x0A, 0x02, b'l', b's', 0x12, 0x01, b'/'], 9);
        let f = parse(&raw).unwrap();
        let b = f.builtin().unwrap();
        assert_eq!(b, Builtin::ShellStream { command: "ls".into(), cwd: "/".into() });
        let frames = b.rejection(&f, "R");
        assert_eq!(frames.len(), 2);
        // 关流:{5: {1: {1: 9}}} = 2A 04 0A 02 08 09
        assert_eq!(frames[1], vec![0x2A, 0x04, 0x0A, 0x02, 0x08, 0x09]);
        // 拒绝走 field 14 的 member 5,带 command/cwd/reason
        let shell_rej = [0x0A, 0x02, b'l', b's', 0x12, 0x01, b'/', 0x1A, 0x01, b'R'];
        assert!(frames[0].windows(shell_rej.len()).any(|w| w == shell_rej));
        assert!(frames[0].windows(2).any(|w| w == [0x72, 0x0C]), "field 14 包 12 字节的 member 5");
    }

    /// 2026-11-07 生产实物形状:{2:{1:id, 28:{1:"toolu_x", 2:"generalPurpose", 3:"m", 4:"审查"}}}。
    #[test]
    fn 子代理_解析_映射_拒绝字节() {
        let body: Vec<u8> = [
            &[0x0A, 0x07][..], b"toolu_x",
            &[0x12, 0x0E][..], b"generalPurpose",
            &[0x1A, 0x01][..], b"m",
            &[0x22, 0x06][..], "审查".as_bytes(),
        ]
        .concat();
        // field 28 LEN tag = 28<<3|2 = 226 → varint E2 01
        let mut inner = vec![0x08, 0x06, 0xE2, 0x01, body.len() as u8];
        inner.extend(&body);
        let mut raw = vec![0x12, inner.len() as u8];
        raw.extend(inner);
        let f = parse(&raw).unwrap();
        assert_eq!(f.variant, 28);
        let b = f.builtin().unwrap();
        assert_eq!(
            b,
            Builtin::Subagent {
                tool_call_id: "toolu_x".into(),
                subagent_type: "generalPurpose".into(),
                prompt: "审查".into()
            }
        );
        assert!(matches!(
            b.to_builtin_call("synth"),
            Some(crate::run::BuiltinCall::Task { ref id, ref prompt, .. }) if id == "toolu_x" && prompt == "审查"
        ));
        // 拒绝:{2:{1:6, 28:{2:{1:"R"}}}} = 12 0A | 08 06 E2 01 05 12 03 0A 01 52(内层 10 字节)
        assert_eq!(
            b.rejection(&f, "R"),
            vec![vec![0x12, 0x0A, 0x08, 0x06, 0xE2, 0x01, 0x05, 0x12, 0x03, 0x0A, 0x01, b'R']]
        );
    }

    #[test]
    fn 删除不替模型做主_只拒绝() {
        let raw = frame_of(&[0x22], &[0x0A, 0x02, b'/', b'a'], 1); // field 4 = delete
        let f = parse(&raw).unwrap();
        let b = f.builtin().unwrap();
        assert_eq!(b, Builtin::Delete { path: "/a".into() });
        assert!(b.to_builtin_call("x").is_none());
    }

    /// grep / 列目录不在这里拼终端命令(2026-11-09 生产:拼出的 POSIX `grep -rn` 发给
    /// Windows PowerShell 客户端,290 次「grep 不是 cmdlet」)。交给 chat 按调用方工具表落地。
    #[test]
    fn grep与列目录交给调用方工具表落地() {
        use crate::run::BuiltinCall as B;
        let b = Builtin::Grep { pattern: "it's".into(), path: "".into(), glob: "*.py".into() };
        assert_eq!(
            b.to_builtin_call("x"),
            Some(B::Grep { id: "x".into(), pattern: "it's".into(), path: None, glob: Some("*.py".into()) })
        );
        let b = Builtin::Ls { path: "".into() };
        assert_eq!(b.to_builtin_call("y"), Some(B::ListDir { id: "y".into(), path: ".".into() }));
    }

    /// 读文件 / 流式终端的 exec 帧取出挂起种类与 tool_call_id;缺 id 不认;别的分支不认。
    #[test]
    fn 可挂起的自带工具exec() {
        use crate::held::PendingKind as K;
        // {2: {1:5, 15:"ex", 7: {1:"/a", 2:"call-1"}}}
        let mut a = Writer::new();
        a.string(1, "/a");
        a.string(2, "call-1");
        let mut em = Writer::new();
        em.uint(1, 5);
        em.string(15, "ex");
        em.message(VAR_READ, &a);
        let mut top = Writer::new();
        top.message(2, &em);
        let raw = top.into_bytes();
        let f = parse(&raw).unwrap();
        assert_eq!(holdable_exec(&f), Some((K::Read { path: "/a".into() }, "call-1".into())));
        // 流式终端 {14: {1:"ls", 2:"/w", 4:"call-2"}}
        let mut a = Writer::new();
        a.string(1, "ls");
        a.string(2, "/w");
        a.string(4, "call-2");
        let mut em = Writer::new();
        em.uint(1, 6);
        em.message(VAR_SHELL_STREAM, &a);
        let mut top = Writer::new();
        top.message(2, &em);
        let raw = top.into_bytes();
        assert_eq!(
            holdable_exec(&parse(&raw).unwrap()),
            Some((K::ShellStream { cwd: "/w".into() }, "call-2".into()))
        );
        // 普通终端(2)结果格式没实测过,不挂
        let mut a = Writer::new();
        a.string(1, "ls");
        a.string(4, "call-3");
        let mut em = Writer::new();
        em.message(VAR_SHELL, &a);
        let mut top = Writer::new();
        top.message(2, &em);
        let raw = top.into_bytes();
        assert_eq!(holdable_exec(&parse(&raw).unwrap()), None);
        // 缺 tool_call_id 不认
        let mut a = Writer::new();
        a.string(1, "/a");
        let mut em = Writer::new();
        em.message(VAR_READ, &a);
        let mut top = Writer::new();
        top.message(2, &em);
        let raw = top.into_bytes();
        assert_eq!(holdable_exec(&parse(&raw).unwrap()), None);
    }

    #[test]
    fn 握手与_mcp_不是内建工具() {
        let req = mcp_state_req();
        assert!(parse(&req).unwrap().builtin().is_none());
    }

    #[test]
    fn 动态_mcp_工具开始帧_按抓包实物识别() {
        // 2026-11-06 testa / claude-sonnet-5-5 实物:1.2.2.44{1:{1:"claude-local",2:"read_file",4:id}}
        let raw = include_bytes!("../tests/fixtures/tool_call_dynamic_mcp_started_real.bin");
        assert!(is_dynamic_mcp_started(raw));
        // 它不是 exec 帧(顶层是 field 1),也不是握手。
        assert!(parse(raw).is_none());
        assert!(!is_handshake(raw));
        // 旧的 `.15` 外部工具帧与内建工具帧都不是它。
        let ext = include_bytes!("../tests/fixtures/tool_call_external_real.bin");
        let builtin = include_bytes!("../tests/fixtures/tool_call_builtin_real.bin");
        assert!(!is_dynamic_mcp_started(ext));
        assert!(!is_dynamic_mcp_started(builtin));
    }

    #[test]
    fn exec_形态的_mcp_调用解出名字id与各类参数() {
        // mcp_args = {1:"x", 5:"Read", 3:"call-1", 2:{1:"path", 2:{3:"/a"}}, 2:{1:"limit", 2:{2:double 5}}}
        let mut body = vec![0x0A, 0x01, b'x', 0x2A, 0x04];
        body.extend(b"Read");
        body.extend([0x1A, 0x06]);
        body.extend(b"call-1");
        // entry = 0A 04 "path" | 12 04 {1A 02 "/a"} → 12 字节
        body.extend([0x12, 0x0C, 0x0A, 0x04]);
        body.extend(b"path");
        body.extend([0x12, 0x04, 0x1A, 0x02, b'/', b'a']);
        body.extend([0x12, 0x12, 0x0A, 0x05]);
        body.extend(b"limit");
        body.extend([0x12, 0x09, 0x11]);
        body.extend(5.0f64.to_le_bytes());
        let mut inner = vec![0x5A, body.len() as u8]; // field 11
        inner.extend(body);
        let mut frame = vec![0x12, inner.len() as u8];
        frame.extend(inner);

        let tc = mcp_call(&frame).expect("mcp 调用");
        assert_eq!(tc.name, "Read"); // tool_name(5)优先于 name(1)
        assert_eq!(tc.id, "call-1");
        assert_eq!(
            tc.args,
            vec![
                ("path".to_string(), serde_json::json!("/a")),
                ("limit".to_string(), serde_json::json!(5)),
            ]
        );
        assert!(!is_handshake(&frame));
    }
}
