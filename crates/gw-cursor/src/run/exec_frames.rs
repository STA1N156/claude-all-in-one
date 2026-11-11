//! exec 读写帧的解析与应答。
//!
//! 2026-11-10 由 run.rs 拆出(只搬不改行为)。

use super::*;

/// 一次服务端写盘调用(`write_args`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecWrite {
    /// 关联 id,回执原样带回(实测的资产写调用里是缺省 0)。
    pub id: u64,
    pub exec_id: String,
    pub path: String,
    /// 要落盘的内容:`file_bytes`(field 5)优先,只有 `file_text`(field 2)时取其字节。
    pub bytes: Vec<u8>,
}

/// 一次服务端读文件调用(`read_args`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRead {
    pub id: u64,
    pub exec_id: String,
    pub path: String,
}

/// 取 exec 消息的关联 id(`1`=uint32、`15`=string),缺省即 0 / ""。
pub(super) fn exec_ids(ch: &[u8]) -> (u64, String) {
    let mut id = 0u64;
    let mut exec_id = String::new();
    for (f, v) in Reader::new(ch) {
        match (f, v) {
            (EXEC_ID, PbValue::Varint(n)) => id = n,
            (EXEC_EXEC_ID, PbValue::Len(s)) => exec_id = String::from_utf8_lossy(s).to_string(),
            _ => {}
        }
    }
    (id, exec_id)
}

/// 解服务端写盘调用。不是 write_args(或结构对不上)→ `None`。
///
/// ⚠️ 认领条件:路径必须是**绝对路径**(以 `/` 开头)且内容在场
/// (`file_bytes` 或 `file_text`)。真工具调用帧的 `1.2.3` 也是个字符串
/// (另一个 id,形如 `<uuid>-0-<4字符>`,见 PROTOCOL §13.2)—— 单靠
/// 「field 3 能解出字符串」会把工具帧吞掉,工具回路直接哑掉。
pub fn parse_exec_write(payload: &[u8]) -> Option<ExecWrite> {
    let ch = tool_channel(payload)?;
    let args = Reader::new(ch).find_map(|(f, v)| match (f, v) {
        (EXEC_WRITE_ARGS, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    let mut path = String::new();
    let mut bytes = None;
    for (f, v) in Reader::new(args) {
        match (f, v) {
            (1, PbValue::Len(s)) => path = String::from_utf8_lossy(s).to_string(),
            (2, PbValue::Len(s)) => {
                if bytes.is_none() {
                    bytes = Some(s.to_vec());
                }
            }
            (5, PbValue::Len(s)) => bytes = Some(s.to_vec()),
            _ => {}
        }
    }
    if !path.starts_with('/') {
        return None;
    }
    let (id, exec_id) = exec_ids(ch);
    Some(ExecWrite {
        id,
        exec_id,
        path,
        bytes: bytes?,
    })
}

/// 解服务端读文件调用。不是 read_args → `None`。认领只需绝对路径:
/// read_args 只有 {1:path, 2:tool_call_id} 两个字段,没有可混淆的同号字符串。
pub fn parse_exec_read(payload: &[u8]) -> Option<ExecRead> {
    let ch = tool_channel(payload)?;
    let args = Reader::new(ch).find_map(|(f, v)| match (f, v) {
        (EXEC_READ_ARGS, PbValue::Len(s)) => Some(s),
        _ => None,
    })?;
    let mut path = String::new();
    for (f, v) in Reader::new(args) {
        if let (1, PbValue::Len(s)) = (f, v) {
            path = String::from_utf8_lossy(s).to_string();
        }
    }
    if !path.starts_with('/') {
        return None;
    }
    let (id, exec_id) = exec_ids(ch);
    Some(ExecRead { id, exec_id, path })
}

/// 客户端 exec 回执的公共骨架:`AgentClientMessage{2: exec_client_message{…}}`。
/// `result_field` 与结果体外层由调用方定(write_result=3 / read_result=7,与服务侧同号)。
pub(crate) fn exec_client_frame(id: u64, exec_id: &str, result_field: u32, result: &Writer) -> Vec<u8> {
    let mut ecm = Writer::new();
    if id != 0 {
        ecm.uint(EXEC_ID, id);
    }
    if !exec_id.is_empty() {
        ecm.string(EXEC_EXEC_ID, exec_id);
    }
    ecm.message(result_field, result);
    let mut top = Writer::new();
    top.message(RESP_TOOL_CHANNEL, &ecm);
    top.into_bytes()
}

/// 编码写盘成功回执:`write_result{1: write_success{1:path, 3:file_size}}`。
pub fn encode_write_success(id: u64, exec_id: &str, path: &str, file_size: usize) -> Vec<u8> {
    let mut ws = Writer::new();
    ws.string(1, path);
    ws.uint(3, file_size as u64);
    let mut wr = Writer::new();
    wr.message(1, &ws); // WriteResult.success
    exec_client_frame(id, exec_id, EXEC_WRITE_ARGS, &wr)
}

/// 编码读文件成功回执(二进制):`read_result{1: read_success{1:path, 4:file_size, 5:data}}`。
/// 图片必须走 `data`(bytes)而不是 `content`(string)—— PNG 不是合法 UTF-8。
pub fn encode_read_success_data(id: u64, exec_id: &str, path: &str, data: &[u8]) -> Vec<u8> {
    let mut rs = Writer::new();
    rs.string(1, path);
    rs.uint(4, data.len() as u64);
    rs.bytes(5, data);
    let mut rr = Writer::new();
    rr.message(1, &rs); // ReadResult.success
    exec_client_frame(id, exec_id, EXEC_READ_ARGS, &rr)
}
