//! 工具:工具定义、工具调用帧解析、内建工具帧解析、protobuf 值解码。
//!
//! 2026-11-10 由 run.rs 拆出(只搬不改行为)。

use super::*;

// ── 工具声明(1.2.1.2.7)内部 ───────────────────────────────────────────────
//
// 逐字对齐真包(16 条 `cursor-ide-browser` 的 MCP 工具,5 个字段**全部必填**)。
pub(super) const TOOL_FULL_NAME: u32 = 1; // '<命名空间>-<裸名>'
pub(super) const TOOL_DESC: u32 = 2;
pub(super) const TOOL_NAMESPACE: u32 = 4; // MCP server 名
pub(super) const TOOL_BARE_NAME: u32 = 5;
pub(super) const TOOL_SCHEMA: u32 = 6; // JSON Schema 字符串

/// 我方转发 Anthropic `tools` 时用的命名空间。
///
/// 真包里这一位是 MCP server 名(如 `cursor-ide-browser`)。我们不是真 MCP server,
/// 但这个字段必填,所以取一个固定值 —— 同时它也是**回调时认领工具的依据**:
/// 模型调 `<ns>-<name>` 回来,我们据此判断"这是调用方的工具"而不是 Cursor 内建工具。
pub const TOOL_NS: &str = "gwtools";

/// 一个从 Anthropic 请求 `tools` 里读出来的工具声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// `input_schema` 的 JSON 序列化。
    pub schema: String,
}

impl ToolDef {
    /// IDE 形态的 `McpToolDefinition`(帧0 注册与 `gwtools` 的 mcp_state 应答共用,
    /// 两处字节必须一致):`{1:"gwtools-<名>", 2:描述, 4:"gwtools", 5:<名>, 6:schema JSON}`。
    pub(crate) fn encode(&self) -> Writer {
        let mut w = Writer::new();
        w.string(TOOL_FULL_NAME, &format!("{TOOL_NS}-{}", self.name));
        w.string(TOOL_DESC, &self.description);
        w.string(TOOL_NAMESPACE, TOOL_NS);
        w.string(TOOL_BARE_NAME, &self.name);
        w.string(TOOL_SCHEMA, &self.schema);
        w
    }
}

/// 顶层 `field 2` 与 `1.2` = **工具调用通道**(§10.7:client 事件/工具结果)。
///
/// 2026-08-07 实测的停摆签名:`1.8.1 = 17` → `1.2`(458B)→ 顶层 `field 2`(286B)
/// → 然后每 10 秒一个 4 字节 `1.13` 心跳,**到永远**。
///
/// 语义:模型决定调工具了,正等客户端把结果用请求侧的 `field 2` 帧送回去。
/// 真 IDE 会执行工具并回帧;我们目前不会,所以必须**主动收口**而不是陪它等 ——
/// 陪等的代价是每个"需要动手"的请求都挂满 watchdog 的 90 秒。
pub(super) const RESP_TOOL_CHANNEL: u32 = 2;

/// 内建 **exec 调用**(`ExecServerMessage`,2026-08-10 与 opencodex 项目的
/// `agent.v1` 全量 protobuf schema 交叉核对过字段号):服务端让**客户端**执行
/// 文件/命令操作,客户端在同一条 BiDi 流的请求侧回 `ExecClientMessage`。
/// 对我们最重要的两个:
///   field 3 = write_args {1:path, 2:file_text, 3:tool_call_id, 4:return_content, 5:file_bytes}
///   field 7 = read_args  {1:path, 2:tool_call_id}
///
/// 带图请求的必经流程:服务端把附件字节用 write 推给客户端「落盘」
/// (`/assets/attach-N-<uuid>.png`),模型随后用 read 读同一路径把图看进去。
/// 真 IDE 有真磁盘所以无感;我们不回执的话,服务端 90 秒心跳死等(2026-08-10 实测),
/// 表现就是带图请求全模型 502。
///
/// 实物帧(`tests/fixtures/asset_echo_real.bin`,200B):
///   顶层 field 2(exec_server_message)→ field 3 write_args{1:路径, 5:字节}
///   + field 19 span_context(标准 OTel){1:32hex trace, 2:16hex span, 3:flags} + field 55=0。
/// 注意它**不裹** `1.RESP_MESSAGE`,与工具调用帧(1.2)的包装不对称 —— 两路都要认。
/// 此前这一帧被误判成「内建工具调用」在出字前收口:凡是带图请求,全模型
/// (fable/opus/grok 实测)一律 502 EmptyResponse,客户端重试风暴。
pub(super) const EXEC_WRITE_ARGS: u32 = 3; // exec_server_message.3 = write_args
pub(super) const EXEC_READ_ARGS: u32 = 7; // exec_server_message.7 = read_args
/// exec 消息的关联 id(`uint32 id` / `string exec_id`),回执时原样带回。
/// 实测的资产写调用两个都缺省(0 / "")。
pub(super) const EXEC_ID: u32 = 1;
pub(super) const EXEC_EXEC_ID: u32 = 15;

// ── 工具调用帧(响应侧)的字段号 ────────────────────────────────────────────
//
// 2026-08-07 对真上游实测解出。整条路径:
//   1.2.1        call_id,形如 "call-<uuid>-N\nfc_<uuid>_N"(两段用 \n 连)
//   1.2.2.<N>    N = **工具身份**。内建工具是闭集枚举(.1 终端、.4 读文件…),
//                我们声明的外部/MCP 工具统一落在 **.15**
//   1.2.2.15.1   {1:全名, 2:[参数], 3:call_id, 4/9:命名空间, 5:裸名}
//   参数         repeated {1: key, 2: {3: 字符串值}} —— 值多裹一层,3 = string
pub(super) const TC_CALL_ID: u32 = 1; // 1.2.1
pub(super) const TC_DETAIL: u32 = 2; // 1.2.2
pub(super) const TC_EXTERNAL: u32 = 15; // 1.2.2.15  外部/MCP 工具
pub(super) const TC_INNER: u32 = 1; // 1.2.2.15.1
pub(super) const TC_ARGS: u32 = 2; // …1.2  repeated
pub(super) const TC_ARG_KEY: u32 = 1;
pub(super) const TC_ARG_VAL: u32 = 2;
pub(super) const TC_BARE_NAME: u32 = 5;

// ── 参数值 = `google.protobuf.Value` ───────────────────────────────────────
//
// `1.2.2.15.1.2.2` 里那"多裹的一层"不是我方猜的包装,是标准的
// `google.protobuf.Value` oneof。抓包实物只出现过 field 3(字符串),
// 早先据此把解析写死成"只读 field 3",于是**模型传数字时解出空串**:
// 2026-08-07 实测 opencode 的 `read` 收到 `limit=""` / `offset=""`,
// 工具失败 → 模型改用「数值类型」再试 → 还是空 → 无限重试
// (屏幕上是一串 `→Read … [limit=, offset=]`)。
//
// 字段号照 protobuf 官方定义,不是逆向来的:
pub(super) const PBV_NULL: u32 = 1; // null_value  (enum, varint)
pub(super) const PBV_NUMBER: u32 = 2; // number_value(double → wire type 1)
pub(super) const PBV_STRING: u32 = 3; // string_value
pub(super) const PBV_BOOL: u32 = 4; // bool_value  (varint)
pub(super) const PBV_STRUCT: u32 = 5; // struct_value(Struct)
pub(super) const PBV_LIST: u32 = 6; // list_value  (ListValue)
/// `Struct.fields` / `ListValue.values` 都是 field 1 的 repeated。
pub(super) const PBV_ENTRIES: u32 = 1;
/// 嵌套深度上限。上游不可信,不设限就是一条栈溢出通道。
pub(super) const PBV_MAX_DEPTH: u32 = 12;

/// 解一个 `google.protobuf.Value` 成 JSON。
///
/// 认不出来(空 oneof / 未知字段 / 超深)→ `Value::Null`,让调用方看到
/// `null` 而不是空字符串 —— 空字符串会被工具当成"给了个空值"而静默做错事,
/// `null` 至少是"没给"。
pub(crate) fn decode_pb_value(bytes: &[u8], depth: u32) -> Value {
    if depth > PBV_MAX_DEPTH {
        return Value::Null;
    }
    for (f, v) in Reader::new(bytes) {
        match (f, &v) {
            (PBV_NULL, _) => return Value::Null,
            (PBV_NUMBER, _) => {
                if let Some(n) = v.as_f64() {
                    // 整数值回成 JSON 整数:工具 schema 写 `"type":"integer"` 时
                    // `200.0` 可能过不了校验(而模型要的就是 200)。
                    if n.fract() == 0.0 && n.abs() < 9e15 {
                        return Value::from(n as i64);
                    }
                    return serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number);
                }
            }
            (PBV_STRING, PbValue::Len(s)) => {
                return Value::String(String::from_utf8_lossy(s).into_owned())
            }
            (PBV_BOOL, PbValue::Varint(n)) => return Value::Bool(*n != 0),
            (PBV_STRUCT, PbValue::Len(st)) => {
                let mut map = serde_json::Map::new();
                // Struct.fields = repeated MapEntry{1:key, 2:Value}
                for (ef, ev) in Reader::new(st) {
                    if ef != PBV_ENTRIES {
                        continue;
                    }
                    let PbValue::Len(entry) = ev else { continue };
                    let mut k = String::new();
                    let mut val = Value::Null;
                    for (kf, kv) in Reader::new(entry) {
                        match (kf, kv) {
                            (1, PbValue::Len(b)) => k = String::from_utf8_lossy(b).into_owned(),
                            (2, PbValue::Len(b)) => val = decode_pb_value(b, depth + 1),
                            _ => {}
                        }
                    }
                    if !k.is_empty() {
                        map.insert(k, val);
                    }
                }
                return Value::Object(map);
            }
            (PBV_LIST, PbValue::Len(list)) => {
                let mut arr = Vec::new();
                for (lf, lv) in Reader::new(list) {
                    if lf != PBV_ENTRIES {
                        continue;
                    }
                    if let PbValue::Len(b) = lv {
                        arr.push(decode_pb_value(b, depth + 1));
                    }
                }
                return Value::Array(arr);
            }
            _ => {}
        }
    }
    Value::Null
}

/// 上游发起的一次工具调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// 原样的 call id。回传结果时必须用它对上,所以**绝不能重新生成**。
    pub id: String,
    /// 裸工具名(不含命名空间前缀)—— 与调用方 Anthropic 请求里的 `name` 对得上。
    pub name: String,
    /// 参数 `{key: value}`。值是**已解好的 JSON**:上游用
    /// `google.protobuf.Value` 承载,数字/布尔/对象/数组都进得来
    /// (见 [`decode_pb_value`])。
    pub args: Vec<(String, Value)>,
}

/// 解一帧工具调用。不是外部工具(或根本不是工具帧)→ `None`。
///
/// 只认 `1.2.2.15`(我们声明的工具)。Cursor **内建**工具走别的字段号,
/// 那些我们执行不了 —— 认出来也只能报错,不如让调用方看到"模型跑偏了"。
pub fn parse_tool_call(payload: &[u8]) -> Option<ToolCall> {
    let msg = Reader::new(payload).find_map(|(f, v)| match (f, v) {
        (RESP_MESSAGE, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    let ch = Reader::new(msg).find_map(|(f, v)| match (f, v) {
        (RESP_TOOL_CHANNEL, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;

    let mut id = String::new();
    let mut detail = None;
    for (f, v) in Reader::new(ch) {
        match (f, v) {
            (TC_CALL_ID, PbValue::Len(s)) => id = String::from_utf8_lossy(s).to_string(),
            (TC_DETAIL, PbValue::Len(s)) => detail = Some(s),
            _ => {}
        }
    }
    let inner = Reader::new(detail?)
        .find_map(|(f, v)| match (f, v) {
            (TC_EXTERNAL, PbValue::Len(s)) => Some(s),
            _ => None,
        })
        .and_then(|ext| {
            Reader::new(ext).find_map(|(f, v)| match (f, v) {
                (TC_INNER, PbValue::Len(s)) => Some(s),
                _ => None,
            })
        })?;

    let mut name = String::new();
    let mut args = Vec::new();
    for (f, v) in Reader::new(inner) {
        match (f, v) {
            (TC_BARE_NAME, PbValue::Len(s)) => name = String::from_utf8_lossy(s).to_string(),
            (TC_ARGS, PbValue::Len(kv)) => {
                let mut k = String::new();
                let mut val = Value::Null;
                for (f2, v2) in Reader::new(kv) {
                    match (f2, v2) {
                        (TC_ARG_KEY, PbValue::Len(s)) => k = String::from_utf8_lossy(s).to_string(),
                        // 值是 google.protobuf.Value,**不是**只有字符串一档。
                        (TC_ARG_VAL, PbValue::Len(wrap)) => val = decode_pb_value(wrap, 0),
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
        return None;
    }
    // ⚠️ **空 id 不许放行。** Anthropic 要求 `tool_use.id` 非空、且后续 `tool_result`
    // 靠它对上。上游帧缺 `1.2.1`(或字段号漂移)时这里会是空串,交出去的后果是
    // 客户端要么校验报错、要么回传时对不上,工具回路以很难查的方式断掉。
    // 而字段号漂移恰恰是这个 provider 的常态风险。
    //
    // 合成一个而不是丢弃整个调用:工具名解出来了,说明模型确实想调这个工具 ——
    // 丢掉等于凭空少了一次调用,比 id 难看糟得多。我方从不把 call id 发回上游
    // (每轮都是全新的 Opening 请求),所以合成是安全的。
    if id.is_empty() {
        let synth = format!("call_{}", uuid::Uuid::new_v4().simple());
        tracing::warn!(
            tool = %name, synth = %synth,
            "cursor 工具调用帧缺 call id(1.2.1),合成一个 —— 疑似上游字段号漂移,值得查"
        );
        return Some(ToolCall {
            id: synth,
            name,
            args,
        });
    }
    Some(ToolCall { id, name, args })
}

/// 内建工具调用的**身份字段号**(`1.2.2.<N>` 里的 N)。外部/MCP 工具(`.15`)返回
/// `None` —— 那条路由 [`parse_tool_call`] 处理。
///
/// ## 为什么是字段号而不是名字
///
/// Cursor 的内建工具是**闭集枚举、不带名字**的:身份就编码在字段号里
/// (§13.2 抓包实证:`.1` 终端命令、`.4` 读文件)。所以「模型刚才想干什么」这个
/// 问题在这里只能得到一个数字,认识的数字才翻得成人话。
///
/// 这个数字有两个用处:
/// 1. 下一轮纠偏时**指名**换哪个工具(见 chat.rs 的 `builtin_capability`);
/// 2. 收口日志里把**不认识**的字段号记下来 —— 抓包定枚举那一步的现成线索,
///    不用先猜哪些内建工具在被调。
///
/// 跳过的字段是已知的非身份位:`15` 外部工具、`57` 重复的 call_id、`59` 毫秒时间戳。
pub fn builtin_tool_ident(payload: &[u8]) -> Option<u32> {
    let ch = tool_channel(payload)?;
    let detail = Reader::new(ch).find_map(|(f, v)| match (f, v) {
        (TC_DETAIL, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    Reader::new(detail).find_map(|(f, v)| match (f, v) {
        (TC_EXTERNAL, _) => None,
        (57 | 59, _) => None,
        (f, PbValue::Len(_)) => Some(f),
        _ => None,
    })
}

/// 一次**已实证**的内建工具调用(§13.2 抓包:`.1` 终端命令、`.4` 读文件;
/// 2026-09-06 `CURSOR_DUMP_TOOL_FRAMES` 生产形态实证:`.12` 写文件)。
///
/// 只为这三个存在:它们是收口日志里的绝对大头,且参数字段号有实物依据
/// (`.1` = `{1:{1:'ls -la', 3:超时, 5:'ls', 8:argv, 15:描述}}`,
///  `.4` = `{1:{2:'README'}}`,
///  `.12` = `{1:{1:路径, 6:完整内容}}`,两枚实物帧交叉验证一致)。
/// 其余内建身份(代码检索、网页搜索…)参数形状
/// 未实证,**不猜** —— 翻译错比收口更糟:客户端会执行一个参数张冠李戴的工具。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuiltinCall {
    /// `1.2.2.1`:终端命令,命令串在 `.1.1.1`,工作目录在 `.1.1.2`
    /// (`ShellArgs.working_directory`,可缺)。
    Terminal {
        id: String,
        command: String,
        cwd: Option<String>,
    },
    /// `1.2.2.8`(read_tool_call):读文件,路径在 `.8.1.1`。
    ///
    /// ⚠️ 2026-11-07 前这里写的是 `.4`/`.4.1.2` —— 那其实是 glob(见 [`Self::Glob`]),
    /// 字段号以 cursor-agent 打包的 `agent.v1` 定义为准(`no:4 glob_tool_call`、
    /// `no:8 read_tool_call`)。
    ///
    /// 行范围 `.8.1.2 offset` / `.8.1.3 limit`(cursor-agent `agent.v1.ReadToolArgs`,
    /// int32,可缺):offset 从 1 起的行号、负数 = 从文件末尾倒数,limit = 行数。
    /// 2026-11-09 前不转 → 模型「从第 440 行往后读」被翻成客户端从头读,客户端截断后
    /// 又只回前几百行,同一个文件反复读(生产论文会话末尾连读 3 次)。
    ReadFile {
        id: String,
        path: String,
        offset: Option<i32>,
        limit: Option<i32>,
    },
    /// `1.2.2.4`(glob_tool_call):按通配符找文件,`.4.1.1` = 目录(可缺)、
    /// `.4.1.2` = 模式(必填)。
    Glob {
        id: String,
        dir: Option<String>,
        pattern: String,
    },
    /// 写文件(exec `write_args`):路径 + **服务端算好的完整内容**(`file_text`)。
    ///
    /// ⚠️ 2026-11-07 前界面帧 `1.2.2.12` 也解成它 —— 那一帧其实是 edit_tool_call,
    /// 局部替换时内容只是片段,翻成整文件写会毁掉客户文件(见 [`edit_call_path`])。
    WriteFile {
        id: String,
        path: String,
        content: String,
    },
    /// `1.2.2.37`:网页抓取,URL 在 `.37.1.1`(`.37.1.2` 是 call id 回显)。
    /// 2026-09-06 e2e 实物帧实证(抓取 news.ycombinator.com 的内建调用)。
    WebFetch { id: String, url: String },
    /// 子代理(exec `28 subagent_args`):翻成调用方自己的子代理工具。
    Task { id: String, prompt: String, subagent_type: String },
    /// 搜文件内容(exec `grep_args`):模式必填,路径与文件过滤可缺。
    ///
    /// 2026-11-09 前在 exec 层直接拼成 POSIX `grep -rn` 交给调用方终端 —— 新版 OpenCode
    /// 的 Windows 用户终端是 PowerShell,290 次「grep 不是 cmdlet」,模型原样重试。
    /// 现在由 `chat::translate_builtin` 优先翻成调用方自己的搜索工具,没有才按终端类型拼命令。
    Grep {
        id: String,
        pattern: String,
        path: Option<String>,
        glob: Option<String>,
    },
    /// 列目录(exec `ls_args`):同上,按终端类型拼命令(PowerShell 没有 `ls -la`)。
    ListDir { id: String, path: String },
}

/// 内建工具界面帧的 `(call id, detail 块)`。
///
/// ⚠️ 只认 `1.2`(RESP_MESSAGE → 工具通道)的包装形态,与 parse_tool_call 同款。
/// 不用 tool_channel():那个函数还接受**顶层** field 2 —— 那是 exec 通道
/// (asset-echo 帧,顶层 field 2 = exec_server_message),其 field 2 恰好是
/// shell_args,拿工具通道的字段号去解它是张冠李戴。exec 通道由
/// parse_exec_write/read 处理,这里绝不越界。
pub(super) fn builtin_detail(payload: &[u8]) -> Option<(String, &[u8])> {
    let msg = Reader::new(payload).find_map(|(f, v)| match (f, v) {
        (RESP_MESSAGE, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    let ch = Reader::new(msg).find_map(|(f, v)| match (f, v) {
        (RESP_TOOL_CHANNEL, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    let mut id = String::new();
    let mut detail = None;
    for (f, v) in Reader::new(ch) {
        match (f, v) {
            (TC_CALL_ID, PbValue::Len(s)) => id = String::from_utf8_lossy(s).to_string(),
            (TC_DETAIL, PbValue::Len(s)) => detail = Some(s),
            _ => {}
        }
    }
    Some((id, detail?))
}

/// 内建**编辑**界面帧(`1.2.2.12` edit_tool_call)的目标路径;不是编辑帧返回 `None`。
///
/// 这一帧**不能**直接翻成整文件写(2026-11-07 生产事故):`EditArgs` 只有
/// `{1: path, 6: stream_content}`,模型调 `Write` 时 `stream_content` 是完整内容,
/// 调 `StrReplace` 时却**只是替换后的那一段**。两者同一帧形,分不出来。旧代码一律当
/// 整文件写,把客户的文件盖成了替换片段(OpenCode `write` 收到 5 行,原文件 144 行)。
/// 真正的落盘内容在随后的 exec 帧里:整写是 `write_args{2: file_text}`(完整内容),
/// 局部替换要先 `read_args` 读原文件。调用方据此记下路径,等 exec 帧再决定。
pub fn edit_call_path(payload: &[u8]) -> Option<String> {
    let (_, detail) = builtin_detail(payload)?;
    let body = Reader::new(detail).find_map(|(f, v)| match (f, v) {
        (UI_EDIT, PbValue::Len(b)) => Some(b),
        _ => None,
    })?;
    let args = Reader::new(body).find_map(|(f, v)| match (f, v) {
        (1, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    Reader::new(args)
        .find_map(|(f, v)| match (f, v) {
            (1, PbValue::Len(s)) => std::str::from_utf8(s).ok().map(str::to_owned),
            _ => None,
        })
        .filter(|p| !p.trim().is_empty())
}

/// `ToolCall.edit_tool_call`(agent.v1)。
pub(super) const UI_EDIT: u32 = 12;

/// 解一帧**内建**工具调用的参数(chat.rs 兼容转换层用)。
///
/// 与 [`builtin_tool_ident`] 同源(同一个 detail 块),但只认参数形状有抓包
/// 实证的 `.1` / `.4`(glob) / `.8`(读) / `.37`;参数缺失或为空一律 `None`,让调用方落回收口 ——
/// 转换层的失败必须是单向的(认不出 → 维持原行为),绝不输出错参数的调用。
pub fn parse_builtin_call(payload: &[u8]) -> Option<BuiltinCall> {
    let (id, detail) = builtin_detail(payload)?;
    for (f, v) in Reader::new(detail) {
        let PbValue::Len(body) = v else { continue };
        match f {
            // 终端命令:`.1.1.1` = 命令串。
            1 => {
                let inner = Reader::new(body).find_map(|(f2, v2)| match (f2, v2) {
                    (1, PbValue::Len(s)) => Some(s),
                    _ => None,
                })?;
                let mut command = None;
                let mut cwd = None;
                for (f3, v3) in Reader::new(inner) {
                    match (f3, v3) {
                        (1, PbValue::Len(s)) => {
                            command = Some(String::from_utf8_lossy(s).into_owned())
                        }
                        (2, PbValue::Len(s)) => cwd = std::str::from_utf8(s).ok().map(str::to_owned),
                        _ => {}
                    }
                }
                let command = command.filter(|c| !c.trim().is_empty())?;
                let cwd = cwd.filter(|d| !d.trim().is_empty());
                return Some(BuiltinCall::Terminal { id, command, cwd });
            }
            // 找文件(glob):`.4.1.1` = 目录(可缺)、`.4.1.2` = 模式(必填)。
            4 => {
                let inner = Reader::new(body).find_map(|(f2, v2)| match (f2, v2) {
                    (1, PbValue::Len(s)) => Some(s),
                    _ => None,
                })?;
                let mut dir = None;
                let mut pattern = None;
                for (f3, v3) in Reader::new(inner) {
                    match (f3, v3) {
                        (1, PbValue::Len(s)) => dir = std::str::from_utf8(s).ok().map(str::to_owned),
                        (2, PbValue::Len(s)) => {
                            pattern = std::str::from_utf8(s).ok().map(str::to_owned)
                        }
                        _ => {}
                    }
                }
                let pattern = pattern.filter(|p| !p.trim().is_empty())?;
                let dir = dir.filter(|d| !d.trim().is_empty());
                return Some(BuiltinCall::Glob { id, dir, pattern });
            }
            // 读文件:`.8.1.1` = 路径,`.8.1.2` = offset,`.8.1.3` = limit(int32,可缺)。
            8 => {
                let inner = Reader::new(body).find_map(|(f2, v2)| match (f2, v2) {
                    (1, PbValue::Len(s)) => Some(s),
                    _ => None,
                })?;
                let mut path = None;
                let mut offset = None;
                let mut limit = None;
                for (f3, v3) in Reader::new(inner) {
                    match (f3, v3) {
                        (1, PbValue::Len(s)) => path = std::str::from_utf8(s).ok().map(str::to_owned),
                        // int32 负数按 64 位补码编码:先转 i64 再截 i32 才是原值。
                        (2, PbValue::Varint(n)) => offset = Some(n as i64 as i32),
                        (3, PbValue::Varint(n)) => limit = Some(n as i64 as i32),
                        _ => {}
                    }
                }
                let path = path?;
                if path.trim().is_empty() {
                    return None;
                }
                return Some(BuiltinCall::ReadFile { id, path, offset, limit });
            }
            // `.12` 是**编辑**(edit_tool_call),不是写文件:局部替换时内容只是片段,
            // 不能翻成整文件写(见 [`edit_call_path`])。交给 exec 帧处理。
            // 网页抓取:`.37.1.1` = URL(2026-09-06 e2e 实物帧,
            // `.37.1.2` 是同帧的 call id 回显,不是参数)。URL 空 → None(单向失败)。
            37 => {
                let inner = Reader::new(body).find_map(|(f2, v2)| match (f2, v2) {
                    (1, PbValue::Len(s)) => Some(s),
                    _ => None,
                })?;
                let url = Reader::new(inner).find_map(|(f3, v3)| match (f3, v3) {
                    (1, PbValue::Len(s)) => {
                        std::str::from_utf8(s).ok().map(str::to_owned)
                    }
                    _ => None,
                })?;
                if url.trim().is_empty() {
                    return None;
                }
                return Some(BuiltinCall::WebFetch { id, url });
            }
            _ => {}
        }
    }
    None
}

/// 这一帧是不是「上游在等客户端执行工具」。
///
/// 判定**故意保持宽松**(字段在就算,不看 wire type):这里的首要职责是
/// 「别漏」—— 漏掉一帧真工具调用的代价是陪上游死等心跳到 watchdog。
/// 严的解析交给 [`parse_tool_call`] / [`parse_asset_echo`],认不出再分类。
pub fn is_tool_call(payload: &[u8]) -> bool {
    for (f, v) in Reader::new(payload) {
        if f == RESP_TOOL_CHANNEL {
            return true;
        }
        if f == RESP_MESSAGE {
            if let PbValue::Len(sub) = v {
                if Reader::new(sub).any(|(f2, _)| f2 == RESP_TOOL_CHANNEL) {
                    return true;
                }
            }
        }
    }
    false
}
