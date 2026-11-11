//! 服务端「请示」(`AgentServerMessage.7 interaction_query`)的识别与应答。
//!
//! 模型调用某些 Cursor 托管能力(联网搜索、抓网页、向用户提问、切模式、建计划 …)时,
//! 服务端先推一帧请示,**阻塞等客户端答复**,期间只发心跳。我们以前不认识顶层字段 7,
//! 于是一直不回 —— 2026-11-07 生产 15 次「600s 只有工具参数增量」收口全是这一种
//! (trail 末尾都是 `[7] 47B`),每次白占一个并发名额 10 分钟,把 CUR 组挤满。
//!
//! 字段号取自 cursor-agent 打包的 agent.v1(139 的 `/opt/cursor-agent/index.js`):
//! - `InteractionQuery{1: id, 2 web_search, 3 ask_question, 4 switch_mode, 7 create_plan,
//!   8 setup_vm, 9 web_fetch, 10 pr_management, 11 mcp_auth, 12 generate_image,
//!   13 replace_env, 14 connect_scm}`
//! - 应答 `AgentClientMessage.6 interaction_response = InteractionResponse{1: id, <同号>: …}`
//!
//! 答法对照 OmniRoute `encodeInteractionResponseFrame`:联网搜索 / 抓网页**批准**
//! (由 Cursor 服务端代为执行,结果直接回给模型);提问 / 切模式 / 建计划**拒绝**,
//! 模型会换办法继续。MCP 授权(11)也拒绝,理由写明「不需要授权、直接调工具」
//! (2026-11-07 生产:IDE 形态握手曾对 gwtools 报 0 个工具,模型去授权后干等)。
//! 其余种类不答,交给停滞闸按 150s 收口。

use crate::protobuf::{Reader, Value as PbValue, Writer};

/// `AgentServerMessage.interaction_query`。
const ASM_INTERACTION_QUERY: u32 = 7;
/// `AgentClientMessage.interaction_response`。
const ACM_INTERACTION_RESPONSE: u32 = 6;

const Q_WEB_SEARCH: u32 = 2;
const Q_ASK_QUESTION: u32 = 3;
const Q_SWITCH_MODE: u32 = 4;
const Q_CREATE_PLAN: u32 = 7;
const Q_WEB_FETCH: u32 = 9;
const Q_MCP_AUTH: u32 = 11;

/// 拒绝时给模型看的理由。
const REJECT_REASON: &str = "not available in this client; continue without it";

/// 拒绝 MCP 授权时的理由。我方的 MCP 服务不需要授权;模型去授权通常是因为它以为
/// 工具不可用,这句话要把它拉回「直接调工具」。
const MCP_AUTH_REASON: &str =
    "this MCP server needs no authentication and is already connected; call its tools directly";

/// 一帧请示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Query {
    pub id: u64,
    /// `InteractionQuery` 里 oneof 成员的字段号(2 = 联网搜索 …)。
    pub kind: u32,
}

impl Query {
    /// 写日志用的种类名。
    pub fn kind_name(self) -> &'static str {
        match self.kind {
            Q_WEB_SEARCH => "web_search",
            Q_ASK_QUESTION => "ask_question",
            Q_SWITCH_MODE => "switch_mode",
            Q_CREATE_PLAN => "create_plan",
            8 => "setup_vm",
            Q_WEB_FETCH => "web_fetch",
            10 => "pr_management",
            11 => "mcp_auth",
            12 => "generate_image",
            13 => "replace_env",
            14 => "connect_scm",
            _ => "unknown",
        }
    }
}

/// 识别请示帧;不是请示返回 `None`。
pub(crate) fn parse(payload: &[u8]) -> Option<Query> {
    let body = Reader::new(payload).find_map(|(f, v)| match (f, v) {
        (ASM_INTERACTION_QUERY, PbValue::Len(b)) => Some(b),
        _ => None,
    })?;
    let mut id = 0;
    let mut kind = None;
    for (f, v) in Reader::new(body) {
        match (f, v) {
            (1, PbValue::Varint(n)) => id = n,
            (f, PbValue::Len(_)) if f >= 2 && kind.is_none() => kind = Some(f),
            _ => {}
        }
    }
    Some(Query { id, kind: kind? })
}

/// 应答 payload(未分帧);这种请示没有通用答法时返回 `None`。
pub(crate) fn reply(q: Query) -> Option<Vec<u8>> {
    let mut reason = Writer::new();
    reason.string(1, REJECT_REASON);
    let mut answer = Writer::new();
    match q.kind {
        // `{1: approved{}}`
        Q_WEB_SEARCH | Q_WEB_FETCH => answer.message(1, &Writer::new()),
        // `{1: result{3: rejected{1: reason}}}`
        Q_ASK_QUESTION => {
            let mut result = Writer::new();
            result.message(3, &reason);
            answer.message(1, &result);
        }
        // `{2: rejected{1: reason}}`
        Q_SWITCH_MODE => answer.message(2, &reason),
        // McpAuthRequestResponse `{2: rejected{1: reason}}`
        Q_MCP_AUTH => {
            let mut why = Writer::new();
            why.string(1, MCP_AUTH_REASON);
            answer.message(2, &why);
        }
        // `{1: result{2: error{1: error}}}`
        Q_CREATE_PLAN => {
            let mut result = Writer::new();
            result.message(2, &reason);
            answer.message(1, &result);
        }
        _ => return None,
    }
    let mut resp = Writer::new();
    resp.uint(1, q.id);
    resp.message(q.kind, &answer);
    let mut top = Writer::new();
    top.message(ACM_INTERACTION_RESPONSE, &resp);
    Some(top.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手写:`{7: {1: 5, 2: {1: {1: "rust"}}}}` —— 联网搜索请示。
    fn web_search_query() -> Vec<u8> {
        vec![0x3A, 0x0C, 0x08, 0x05, 0x12, 0x08, 0x0A, 0x06, 0x0A, 0x04, b'r', b'u', b's', b't']
    }

    #[test]
    fn 识别请示帧_非请示不认() {
        assert_eq!(parse(&web_search_query()), Some(Query { id: 5, kind: 2 }));
        // 顶层 1(交互增量)不是请示。
        assert_eq!(parse(&[0x0A, 0x02, 0x0A, 0x00]), None);
        // 有请示壳但没有成员:认不出种类。
        assert_eq!(parse(&[0x3A, 0x02, 0x08, 0x05]), None);
    }

    #[test]
    fn 联网搜索批准_字节() {
        let got = reply(Query { id: 5, kind: 2 }).unwrap();
        // {6: {1: 5, 2: {1: {}}}}
        assert_eq!(got, vec![0x32, 0x06, 0x08, 0x05, 0x12, 0x02, 0x0A, 0x00]);
        let fetch = reply(Query { id: 1, kind: 9 }).unwrap();
        assert_eq!(fetch, vec![0x32, 0x06, 0x08, 0x01, 0x4A, 0x02, 0x0A, 0x00]);
    }

    #[test]
    fn 提问_切模式_建计划回拒绝() {
        let n = REJECT_REASON.len() as u8;
        // 切模式:{6: {1: 3, 4: {2: {1: reason}}}}
        let sw = reply(Query { id: 3, kind: 4 }).unwrap();
        let mut want = vec![0x32, n + 8, 0x08, 0x03, 0x22, n + 4, 0x12, n + 2, 0x0A, n];
        want.extend(REJECT_REASON.as_bytes());
        assert_eq!(sw, want);
        // 提问:{6: {1: 3, 3: {1: {3: {1: reason}}}}}
        let ask = reply(Query { id: 3, kind: 3 }).unwrap();
        let mut want = vec![0x32, n + 10, 0x08, 0x03, 0x1A, n + 6, 0x0A, n + 4, 0x1A, n + 2, 0x0A, n];
        want.extend(REJECT_REASON.as_bytes());
        assert_eq!(ask, want);
        // 建计划:{6: {1: 3, 7: {1: {2: {1: reason}}}}}
        let plan = reply(Query { id: 3, kind: 7 }).unwrap();
        let mut want = vec![0x32, n + 10, 0x08, 0x03, 0x3A, n + 6, 0x0A, n + 4, 0x12, n + 2, 0x0A, n];
        want.extend(REJECT_REASON.as_bytes());
        assert_eq!(plan, want);
    }

    #[test]
    fn mcp授权回拒绝并说明不需要授权() {
        let n = MCP_AUTH_REASON.len() as u8;
        // {6: {1: 0, 11: {2: {1: reason}}}}
        let got = reply(Query { id: 0, kind: 11 }).unwrap();
        let mut want = vec![0x32, n + 8, 0x08, 0x00, 0x5A, n + 4, 0x12, n + 2, 0x0A, n];
        want.extend(MCP_AUTH_REASON.as_bytes());
        assert_eq!(got, want);
    }

    #[test]
    fn 没有通用答法的种类不答() {
        assert_eq!(reply(Query { id: 1, kind: 8 }), None);
        assert_eq!(reply(Query { id: 1, kind: 12 }), None);
        assert_eq!(reply(Query { id: 1, kind: 13 }), None);
    }
}
