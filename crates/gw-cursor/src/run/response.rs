//! 响应帧:帧解析、心跳/提交/总结判定、续轮描述符、KV 点名、轮次判决。
//!
//! 2026-11-10 由 run.rs 拆出(只搬不改行为)。

use super::*;

// ── 响应 ────────────────────────────────────────────────────────────────────
pub(super) const RESP_MESSAGE: u32 = 1; // 响应 field 1 = 流式消息
/// `1.1` = **正文**增量块,`1.1.1` 是本体。
///
/// ⚠️ 曾经错把 `1.4` 当正文。`1.4` 是**思考**流 —— 两者都是 `{1: 文本, 2: 1}`,
/// 结构一模一样,只有字段号不同,所以解错了不会报任何错:请求正常、有字出来,
/// 只是出来的是模型的推理过程而不是它的回答,而且回答本身一个字都收不到。
/// 实测一次「只回答 PINEAPPLE」的会话:`1.4.1` 是「用户要求…」,
/// `1.1.1` 才是 `P` / `INE` / `APPLE`。
pub(super) const RESP_TEXT: u32 = 1; // 1.1  正文增量块
pub(super) const RESP_THINKING: u32 = 4; // 1.4  思考增量块
pub(super) const RESP_DELTA_TEXT: u32 = 1; // 1.{1,4}.1 增量本体
                                // ── 图片附件(1.2.1.1.3)内部 ───────────────────────────────────────────────
                                //
                                // 逐字对齐 2026-08-07 带图抓包。**内联原始字节,不走 FileSync** ——
                                // 文档 §5 猜的「图片走 blob 上传」是错的:带图那次请求里没有任何 FSSyncFile 调用。
/// CLI 形态的「本轮已提交」回显(CLI 响应**没有 `1.14` 用量帧**,2026-08-16 抓包实证):
/// 正文/思考增量之后,服务端按序回显 thinking 摘要、正文消息,最后一帧 field-4 回显是
/// 提交记录 —— `4.3.2.1` = `{1: bin[32], 2: bin[32]×N(消息哈希), 3: 本轮 turn id,
/// 4: 172 字符签名, 5: 0}`。认出它就该收口,否则要等 90s 心跳看门狗。
pub fn is_turn_commit(payload: &[u8]) -> bool {
    for (f, v) in Reader::new(payload) {
        if f != 4 {
            continue;
        }
        let PbValue::Len(echo) = v else { continue };
        let mut cur = Some(echo);
        for want in [3u32, 2, 1] {
            let Some(bytes) = cur else { break };
            cur = None;
            for (f2, v2) in Reader::new(bytes) {
                if f2 == want {
                    if let PbValue::Len(s) = v2 {
                        cur = Some(s);
                    }
                    break;
                }
            }
        }
        let Some(rec) = cur else { continue };
        // 提交记录的指纹:field 3 = 36 字符 turn id,field 4 = 长签名串。
        let mut has_turn = false;
        let mut has_sig = false;
        for (f3, v3) in Reader::new(rec) {
            match (f3, v3) {
                (3, PbValue::Len(s)) => has_turn = s.len() == 36,
                (4, PbValue::Len(s)) => has_sig = s.len() > 100,
                _ => {}
            }
        }
        if has_turn && has_sig {
            return true;
        }
    }
    false
}

/// 上游心跳帧(§10.6):4 字节的 `{1: {13: ""}}` —— 顶层 RESP_MESSAGE 里裹一个
/// 空的 field 13。它唯一的语义是「流还活着」,**不含任何轮次进展**。
///
/// 2026-09-04 pi 卡死案的直接教训:排水段把每个心跳都算作「进展」刷新停滞计时,
/// 于是 commit 之后上游只剩心跳的挂死流永远撞不上停滞闸,要等 gw-app 300s
/// 硬上限兜底。判读它是为了在计时口径里把它**排除**出去。
pub fn is_heartbeat(payload: &[u8]) -> bool {
    // 严格形态(codex 评审):整帧**有且仅有**一个顶层 RESP_MESSAGE,
    // 子消息里有且仅有一个 field 13、且是空串。值非空、字段重复、
    // 合帧夹带、截断畸形,一律否 —— 误判的代价是把真进展当零进展,
    // 排水段会提前放弃影子提交。
    let mut top = Reader::new(payload);
    let mut seen = false;
    while let Some((f, v)) = top.next() {
        if f != RESP_MESSAGE || seen {
            return false;
        }
        seen = true;
        let PbValue::Len(sub) = v else { return false };
        let mut inner = Reader::new(sub);
        let mut inner_seen = false;
        while let Some((f2, v2)) = inner.next() {
            if inner_seen {
                return false;
            }
            inner_seen = true;
            match (f2, v2) {
                (13, PbValue::Len(s)) if s.is_empty() => {}
                _ => return false,
            }
        }
        // 截断防御:子消息没读完就停的,不是心跳。
        if !inner_seen || !inner.is_done() {
            return false;
        }
    }
    seen && top.is_done()
}

/// Run 响应的「续轮描述符」(顶层 field 3,2026-08-23 抓包实证):服务端把跨轮
/// 状态当**不透明字节**回声给客户端,客户端下一轮原样回放(请求侧 `.1`)。
///
/// 实测形态(官方 CLI 两轮抓包,/tmp/cursor-capture):turn1(新会话)响应尾部
/// 两帧(resp-011/013)顶层各带一份 `.3`,内容逐字节相同;turn2(resume)尾部
/// 同样是两份相同的 `.3`,但 blob 引用从 4 个涨到 6 个(多出 turn1 产生的新
/// blob)。schema 与请求 `.1` 相同:{repeated .1 = 32B blob 引用, .5 = 预算表,
/// .8 = 32B, .9 = cwd, .10 = mode, .22 = 'cli', .26 = ts, .27 = tz}。
///
/// 对我们是**不透明字节:永不解析修改、永不构造**,只捕获存储(影子模式)。
/// 这里唯一的"读"是定位顶层 field 3 本身。一帧内多次出现时取**最后一份**
/// (尾部更接近流尾,状态最新)。
pub fn descriptor_field3(payload: &[u8]) -> Option<&[u8]> {
    let mut last = None;
    for (f, v) in Reader::new(payload) {
        if f == 3 {
            if let PbValue::Len(s) = v {
                // ⚠️ **零长度不算捕获**(codex 审查 #4)。空 `.3` 会让上层
                // `is_some()` 判成「有料」→ 选增量消息 + `1.1` 零长度 = 服务端没有
                // 历史、我方也没发 = 静默失忆。空描述符没有任何回放价值,当没有。
                if !s.is_empty() {
                    last = Some(s);
                }
            }
        }
    }
    last
}

/// 数描述符里 32B blob 引用(`.1`)的个数 —— 影子日志的元数据。
/// 这是影子模式对描述符内容的**唯一**一次读取,除此之外不碰内部字节。
pub fn descriptor_ref_count(desc: &[u8]) -> usize {
    Reader::new(desc)
        .filter(|(f, v)| *f == 1 && matches!(v, PbValue::Len(s) if s.len() == 32))
        .count()
}

/// KV 子协议请求(2026-09-04 对齐 YeautyYE/claude-cursor-proxy 的 `agent.v1` schema):
/// 顶层 field 4 = `kv_server_message` `{1: id(varint), 2: get_blob_args{1: blob_id},
/// 3: set_blob_args{1: blob_id, 2: blob_data}}`。
///
/// ⚠️ 这钉死了早先两个按形状猜的解析:
/// 1. 回执里的 `.1` 不是「按点名顺序的自增 slot」,是 **KV 请求 id 回显**
///    (08-23 实物里恰好是 0,1,2…,两者巧合同值);
/// 2. `4.3`(set_blob)不只是「回显让我们存」,**还必须回执 `set_blob_result{}`**
///    (= `kv_client_message{1: id, 3: {}}`,即 [`crate::cli::frame_field3_slot`])。
///    不回执的代价:服务端不出 checkpoint,90s 心跳死等 —— 2026-09-04 探针实证
///    (出字后 4 帧 set_blob 无人回执,`.3` 描述符与用量尾帧全部不来)。
pub enum KvRequest<'a> {
    /// get_blob_args:服务端点名要内容分节。回 `kv_client_message{id, get_blob_result{1: data}}`
    /// (= [`crate::cli::content_slot_frame`])。
    Get { id: u64, hash: [u8; 32] },
    /// set_blob_args:服务端推来一个内容节让我们存。存下并回
    /// `kv_client_message{id, set_blob_result{}}`(= [`crate::cli::frame_field3_slot`])。
    Set { id: u64, data: &'a [u8] },
}

/// 解析一帧里的全部 KV 请求(一帧可含多个顶层 field 4;单个 kv_server_message
/// 只会带 get/set 之一)。
pub fn kv_requests(payload: &[u8]) -> Vec<KvRequest<'_>> {
    let mut out = Vec::new();
    for (f, v) in Reader::new(payload) {
        if f != 4 {
            continue;
        }
        let PbValue::Len(kv) = v else { continue };
        // id 缺省 0(proto3 默认值不上线,实物里 id=0 的请求就是没有 `.1`)。
        let id = Reader::new(kv)
            .find_map(|(f2, v2)| match (f2, v2) {
                (1, PbValue::Varint(n)) => Some(n),
                _ => None,
            })
            .unwrap_or(0);
        for (f2, v2) in Reader::new(kv) {
            let PbValue::Len(args) = v2 else { continue };
            match f2 {
                // get_blob_args {1: blob_id}
                2 => {
                    for (f3, v3) in Reader::new(args) {
                        if f3 == 1 {
                            if let PbValue::Len(s) = v3 {
                                if let Ok(h) = <[u8; 32]>::try_from(s) {
                                    out.push(KvRequest::Get { id, hash: h });
                                }
                            }
                        }
                    }
                }
                // set_blob_args {1: blob_id, 2: blob_data}
                3 => {
                    for (f3, v3) in Reader::new(args) {
                        if f3 == 2 {
                            if let PbValue::Len(data) = v3 {
                                if !data.is_empty() {
                                    out.push(KvRequest::Set { id, data });
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// 服务端「要求补传内容分节」的哈希回显个数(field-4 通道的 `4.2 = {1: bin[32]}`)。
///
/// ## 这是描述符路线唯一的地基风险
///
/// PROTOCOL §20.2 实测:我方**合成**的续轮(帧级照抄真包)被服务端用一串
/// `4.2={1:bin[32]}` 卡住 —— 那是内容寻址存储(CAS)在要客户端按需把内容分节
/// 补上来。而 §P3a(2026-08-23)观察官方 CLI 的 resume 时**零 FileSync/零上传**,
/// 于是结论写成「blob 闸门不存在,捕获 `.3` 回放即可」。
///
/// 两件事不矛盾但**不能互推**:官方首轮 Run 期间内容已经在服务端落好了,
/// 它 resume 时当然不用补;我方回放描述符时服务端手里有没有那些分节,
/// **没有任何证据**。所以每轮都数一次:一旦这个数 > 0,说明描述符回放在我方
/// 流量上还缺一条 CAS 上传腿,`.3` 回放不足以走通,必须回退并把这件事报上来。
///
/// 与 [`is_turn_commit`] 的区别:提交记录住在 `4.3.2.1`,哈希需求住在 `4.2.1`,
/// 同一个 field-4 回显通道的两个不同分支。
pub fn content_hash_echo(payload: &[u8]) -> usize {
    content_hash_demands(payload).len()
}

/// 服务端点名要的内容分节哈希(field-4 通道的 `4.2.1 = bin[32]`)。
///
/// ## 机制(2026-08-23 turn4 抓包 + 时序实证,已推翻早先的定性)
///
/// 这**不是错误信号,是正常协议事件**。turn4(resume,距首轮 2.4h)的时序:
///
/// ```text
/// .042  req-000  帧0(带描述符)
/// .365  resp-000
/// 212.236–.244  resp-001..007  ← 服务端 7 个 4.2 需求,一帧一个哈希
/// 212.246–.255  req-001..007   ← 客户端 2ms 后交出 7 个内容槽
/// 213.495 req-008  ENV         ← ENV 在内容交换之后
/// ```
///
/// 需求 7 个、上传 7 个、**逐一配对**(每个上传节的 sha256 正好等于被点名的哈希),
/// slot 序号 = 需求顺序 0..6。turn1/turn2 的响应里 `4.2` **零出现** —— 服务端当时
/// 还缓存着内容,不需要问。
///
/// 所以描述符 `.1`/`.8` 是一张**内容寻址清单**:`.1` 装消息节哈希、`.8` 装轮次提交
/// 记录哈希(实测 slot1 就是上一轮的 commit 记录 `{1:bin32, 2:bin32×N, 3:turn uuid,
/// 4:182B 签名, 5:3}`)。回放清单只是第一条腿,**按需交出内容是第二条腿**。
pub fn content_hash_demands(payload: &[u8]) -> Vec<[u8; 32]> {
    kv_requests(payload)
        .into_iter()
        .filter_map(|r| match r {
            KvRequest::Get { hash, .. } => Some(hash),
            _ => None,
        })
        .collect()
}

/// 服务端回显的内容分节(field-4 通道的 `4.3.2` 那一层的**原始字节**)。
///
/// ## 这是内容供给的唯一来源(2026-08-23 实证)
///
/// 服务端把它创建的每一个内容节都在响应流里回显一次,节字节就是 `4.3.2` 这层的原始
/// 字节 —— `sha256(那段字节)` 正好等于描述符里的引用。逐条验过:
///
/// ```text
/// turn2 resp-006  590B → 49efdd84…  = turn4 slot6(user 消息)
/// turn2 resp-008  330B → 8ef11f32…  = turn4 slot1(轮次提交记录)
/// turn2 resp-010 1269B → 055fd9c0…  = turn4 slot0(assistant 消息)
/// turn1 resp-002 2025B → 83ccb7a5…  = Composer 系统提示节
/// ```
///
/// turn4 描述符的 8 个引用(6 refs + 2 f8)**8/8** 都能在前几轮的回显里找到。
/// 官方 CLI 的 `~/.cursor/chats/*/*/store.db` `blobs` 表就是这么攒出来的 ——
/// 它不是知识库,是**回显缓存**(实测 70/70 条 `id == sha256(data)`)。
///
/// 所以我方**不需要自建任何节**,也不需要复现服务端的 JSON 序列化(那本来是做不到的:
/// hash 差一个字节就不认)。只要把回显存下来,后续被点名就能原样交回去。
pub fn section_echoes(payload: &[u8]) -> Vec<&[u8]> {
    kv_requests(payload)
        .into_iter()
        .filter_map(|r| match r {
            KvRequest::Set { data, .. } => Some(data),
            _ => None,
        })
        .collect()
}

/// 一轮 wire(CLI 形态)请求的收尾体检表 —— **正向判据**。
///
/// ## 为什么回退判据不能写成「服务端拒绝描述符」
///
/// 描述符不被接受时上游**不报错**:200 + 每 10s 心跳 + 永不 trailer,与 IDE 形态缺
/// `1.2.1.2` 同族。等一个 4xx 永远等不到 —— 请求会挂到超时再返回空,而那正是生产上
/// 偶发 `EmptyResponse` 的形态(3 次/60s → 该号冷却 60s)。所以判据是「**该出现的
/// 没出现**」,不是「出现了错误」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WireTurnOutcome {
    /// 见到顶层 `.3` 续轮描述符(= 下一轮回放的清单)。
    pub saw_descriptor: bool,
    /// 见到干净收尾:CLI 的 turn_commit 回显,或用量帧 / trailer。
    pub saw_finish: bool,
    /// 正文增量累计字符数。**思考不计** —— 只吐思考不吐正文对客户就是空回复。
    pub content_chars: usize,
    /// 服务端点名要的内容分节个数(见 [`content_hash_demands`])。
    pub demanded: usize,
    /// 其中**我方拿不出来**的个数。这才是失败信号。
    pub unavailable: usize,
}

/// [`WireTurnOutcome`] 的判决。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireVerdict {
    /// 有正文、有收尾。
    Ok,
    /// 服务端点名的分节里有我方拿不出来的 —— 描述符清单指向我方未持有的内容。
    ///
    /// 已知必然踩中的一类:每份描述符的第一个 `.1` 引用都是 Cursor 自己那 2025B 的
    /// Composer 系统提示节(`{"role":"system","content":"You are an AI coding
    /// assistant, powered by Composer…"}`),它**不在任何 ENV 帧里**(turn1/turn4 实测
    /// 都没有),是官方 CLI 自己持有的。我方无法自建,被点名就只能重铺。
    ContentUnavailable { missing: usize, demanded: usize },
    /// 裸态:接受了请求,一个字没吐,也没收尾。
    Barren,
    /// 干净收尾但**没有新的 `.3`**。
    ///
    /// 官方每轮尾部都给新描述符(实测 turn1/turn2/turn4 各 3 份 `.3`),不给就是异常。
    /// 后果不对称:本轮的回答是好的(不该扔),但**旧描述符已经缺了本轮** ——
    /// 留着它下轮回放就是只发增量却配一张少一轮的清单 = 静默失忆。
    /// 所以 `needs_reflow` 为真、`should_fallback` 为假。
    NoDescriptor,
    /// 出了字但没干净收尾:轮次可用,但**描述符不可信**(可能装着未完结的状态)。
    NoFinish,
}

impl WireVerdict {
    /// 这个判决是否要求**作废描述符**(下一轮重铺)。
    ///
    /// 比 [`WireTurnOutcome::should_fallback`] 宽一档:`NoFinish` 不让本轮失败
    /// (出了字的轮次对客户有价值),但它的描述符**不可信** —— 可能装着未完结轮次的
    /// 状态,留着下轮回放就是静默失忆。所以它进作废、不进本轮回退。
    pub fn needs_reflow(self) -> bool {
        !matches!(self, WireVerdict::Ok)
    }

    /// 本轮是否该判失败并回退成内联全量重铺。
    ///
    /// `NoFinish` **不**回退:出了字的轮次对客户是有价值的,扔掉等于白烧一次上游
    /// 额度;它的代价由 [`Self::needs_reflow`](作废描述符)承担。
    pub fn should_fallback(self) -> bool {
        matches!(
            self,
            WireVerdict::ContentUnavailable { .. } | WireVerdict::Barren
        )
    }
}

impl WireTurnOutcome {
    pub fn verdict(self) -> WireVerdict {
        // ⚠️ 顺序:拿不出内容排最前 —— 它通常同时表现为裸态,但它是解释裸态成因的
        // 那个诊断,判成 Barren 会把根因盖掉。
        //
        // 注意**只有 `unavailable > 0` 才是失败**。`demanded > 0` 本身是正常协议事件
        // (服务端缓存过期后按需索取,见 `content_hash_demands`);早先把
        // 「出现 4.2」当 NO-GO 是错的,会把一次健康的 resume 判成方案不可行。
        if self.unavailable > 0 {
            return WireVerdict::ContentUnavailable {
                missing: self.unavailable,
                demanded: self.demanded,
            };
        }
        if self.content_chars == 0 && !self.saw_finish {
            return WireVerdict::Barren;
        }
        if !self.saw_finish {
            return WireVerdict::NoFinish;
        }
        if !self.saw_descriptor {
            return WireVerdict::NoDescriptor;
        }
        WireVerdict::Ok
    }

    /// 本轮是否该判失败并回退成内联全量重铺(委托给 [`WireVerdict::should_fallback`])。
    ///
    /// ⚠️ **`saw_descriptor == false` 不进这个判断。** 那说的是「下一轮没有回放的料」,
    /// 不是「这一轮失败了」—— 首轮本来就没有描述符可捕获,把它算进回退会让每条新会话
    /// 的首轮都白跑一次。没料的处理是下一轮走首轮形态,不是重试这一轮。
    pub fn should_fallback(self) -> bool {
        self.verdict().should_fallback()
    }
}

/// 会话登记通知(CLI 形态抓包新帧,2026-08-16):顶层 field 2 的
/// `request_context`(`{10: {2: conversation_id}, 19: span_context, 55: 0}`)。
///
/// 真 CLI 收到它**不回任何帧**(上下文已经在请求侧的 2.10 帧里推过了),
/// 生成照常开始。我方早期实现把它误收成「内建工具调用」,首轮还没出字就掐断。
/// 它与真正的 exec 请求的区分:只有 tracing 类字段(10/19/55),没有任何
/// 可执行负载(2 shell / 3 write / 7 read / 11 mcp / 14 shell_stream …)。
pub fn is_session_notice(payload: &[u8]) -> bool {
    /// exec 通道里「需要客户端动作」的负载字段号(PROTOCOL §18 的 schema 实证枚举)。
    const ACTIONABLE: &[u32] = &[2, 3, 4, 5, 7, 8, 9, 11, 14];
    for (f, v) in Reader::new(payload) {
        if f != RESP_TOOL_CHANNEL {
            continue;
        }
        let PbValue::Len(sub) = v else { continue };
        let mut has_request_context = false;
        for (f2, v2) in Reader::new(sub) {
            match f2 {
                10 => {
                    // request_context:{2: conversation_id}
                    if let PbValue::Len(inner) = v2 {
                        has_request_context = Reader::new(inner).any(|(f3, _)| f3 == 2);
                    }
                }
                f if ACTIONABLE.contains(&f) => return false,
                _ => {}
            }
        }
        return has_request_context;
    }
    false
}

/// 取出工具通道(`field 2`)的字节。工具调用帧裹在 `1.2` 里,资源回显帧
/// 直接挂在顶层 field 2(见 [`RESP_ASSET`] 的注释),两路都要认。
///
/// 与 [`is_tool_call`] 不同,这里是**解析**入口,只接受 length-delimited;
/// wire type 漂移的帧会落回调用方的「认不出」分支,而不是被静默跳过。
pub(super) fn tool_channel(payload: &[u8]) -> Option<&[u8]> {
    for (f, v) in Reader::new(payload) {
        match (f, v) {
            (RESP_TOOL_CHANNEL, PbValue::Len(s)) => return Some(s),
            (RESP_MESSAGE, PbValue::Len(sub)) => {
                if let Some(s) = Reader::new(sub).find_map(|(f2, v2)| match (f2, v2) {
                    (RESP_TOOL_CHANNEL, PbValue::Len(s)) => Some(s),
                    _ => None,
                }) {
                    return Some(s);
                }
            }
            _ => {}
        }
    }
    None
}

/// `1.14 {1: 输入, 2: 输出, 3: 缓存命中}` —— 用量,**同时是本轮的收尾信号**。
///
/// BiDi 流不会自己结束:发完这一帧上游就转成每 10 秒一个 4 字节心跳,永远挂着。
/// 不认它的话,每个请求都要等客户端超时才返回 —— 表现为「答完了但一直转圈」。
pub(super) const RESP_USAGE: u32 = 14;

/// `1.14` 用量帧的分解。`input` 是**总量**(含 `cache_read` + `cache_write`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WireUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// 上游一帧里可能有的东西。一帧只会命中其中一样,但用一个结构体承载
/// 比三个各扫一遍的函数省两次遍历,也让调用方不可能漏处理某一类。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RespFrame {
    /// `1.1.1` 正文增量。
    pub text: String,
    /// `1.4.1` 思考增量。**不能混进正文** —— 那是模型的推理过程,
    /// 拼进回答里客户会看到「用户要求我…」这种自言自语。
    pub thinking: String,
    /// `1.14` 用量 `(输入, 输出, 缓存命中)`。出现即表示本轮结束。
    /// `1.14`(`InteractionUpdate.turn_ended`)的用量分解。
    ///
    /// 字段号经 2026-08-18 本机取证钉死(钩 `cursor-agent` 的 http2,原始帧落盘):
    /// `1 input_tokens / 2 output_tokens / 3 cache_read_tokens /
    ///  4 cache_write_tokens / 5 reasoning_tokens`。
    /// **field 1 是总量**(含 3 与 4),客户端自己显示时才减 —— 与 CLI `result` 事件
    /// 的口径相反(那边发的是减过的未命中量),见 `clidrv::usage_from_result`。
    ///
    /// `reasoning_tokens` 不入账:实测 `output=34 / reasoning=33`,它是 output 的
    /// **子集**,单独计会重复收费。
    pub usage: Option<WireUsage>,
}

/// 解一帧响应。非 `field 1` 的帧(会话回显 `field 4`、计时 `field 8`)一律返回空。
pub fn parse_frame(payload: &[u8]) -> RespFrame {
    let mut out = RespFrame::default();
    for (field, val) in Reader::new(payload) {
        if field != RESP_MESSAGE {
            continue;
        }
        let PbValue::Len(sub) = val else { continue };
        for (f2, v2) in Reader::new(sub) {
            let PbValue::Len(inner) = v2 else { continue };
            match f2 {
                RESP_TEXT | RESP_THINKING => {
                    for (f3, v3) in Reader::new(inner) {
                        if f3 == RESP_DELTA_TEXT {
                            if let PbValue::Len(b) = v3 {
                                let s = String::from_utf8_lossy(b);
                                if f2 == RESP_TEXT {
                                    out.text.push_str(&s);
                                } else {
                                    out.thinking.push_str(&s);
                                }
                            }
                        }
                    }
                }
                RESP_USAGE => {
                    let mut u = [0u64; 5];
                    let mut seen = 0;
                    for (f3, v3) in Reader::new(inner) {
                        if let (1..=5, PbValue::Varint(n)) = (f3, v3) {
                            u[(f3 - 1) as usize] = n;
                            if f3 <= 3 {
                                seen += 1;
                            }
                        }
                    }
                    // 三个数没齐 = 上游改了 wire type 或增减了字段。仍然收口(不收口是
                    // 永久挂起,更坏),但必须留下痕迹 —— 否则表现是「用量莫名归零」,
                    // 和「本来就没用量」分不开。
                    if seen != 3 {
                        tracing::debug!(seen, "cursor 用量帧字段不全,计费数可能失真");
                    }
                    out.usage = Some(WireUsage {
                        input: u[0],
                        output: u[1],
                        cache_read: u[2],
                        cache_write: u[3],
                    });
                    if u[4] > 0 {
                        tracing::debug!(
                            reasoning = u[4],
                            output = u[1],
                            "cursor 用量帧带推理 token(是 output 的子集,不入账)"
                        );
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// 一帧 payload 的顶层字段号列表(排查用:看清上游到底发了什么种类的帧)。
pub fn top_fields(payload: &[u8]) -> Vec<u32> {
    Reader::new(payload).map(|(f, _)| f).collect()
}

/// `field 1` 里面那一层的字段号(排查用)。
///
/// 顶层几乎恒为 `[1]`,信息量全在内层:`1.1`=正文、`1.4`=思考、`1.8`=状态、
/// `1.14`=用量、`1.17`=计数……而**没被认出来的内层字段号就是「模型在等什么」的线索**。
/// 没有它的话,「200 但卡住」在日志里只是一串 `fields=[1]`,看不出区别。
pub fn inner_fields(payload: &[u8]) -> Vec<u32> {
    let mut out = Vec::new();
    for (f, v) in Reader::new(payload) {
        if f == RESP_MESSAGE {
            if let PbValue::Len(sub) = v {
                out.extend(Reader::new(sub).map(|(f2, _)| f2));
            }
        }
    }
    out
}

/// InteractionUpdate(响应 `1.x`)里「上游在干活、但不带正文/思考」的分支。
///
/// 字段号取自 cursor-agent 打包的 `agent.v1.InteractionUpdate`(2026-11-07 在 139 的
/// `/opt/cursor-agent/index.js` 核对):7 partial_tool_call(工具参数文本增量)、
/// 9/10/11 summary(上下文摘要)、12 shell_output_delta、15 tool_call_delta、
/// 16/17 step_started/completed。心跳是 13,不在其中。
///
/// **不含 8 token_delta**:旧排障记录说它在卡住时也一直在变。它改由
/// [`is_token_delta`] 单独认(只认 token 数 > 0 的帧),同样受 600s「只干活」封顶 ——
/// 2026-11-09 生产:opus 用 Cursor 自带写文件拼整页 HTML 时上游只发 `1.8`,150s 被误杀。
/// 也不含 2/3(工具调用本身另有处理)。
///
/// 为什么要单列:停滞闸原先只认正文/思考/用量。模型逐 token 拼一个大工具调用
/// (写整篇论文、整页 HTML)时上游流的是 `1.7`/`1.8`,我们当成「只有心跳」,
/// 150s 判失败 —— 2026-11-07 生产 fable 7 次「出字后只剩心跳」都是这一形态的候选。
pub(super) const WORK_UPDATE_FIELDS: &[u32] = &[7, 9, 10, 11, 12, 15, 16, 17];

/// 这一帧是不是「工具调用开始组装」:`1.7 partial_tool_call` / `1.15 tool_call_delta`。
///
/// 2026-11-07 curtest 实测(fable 一次写入 6000 字论文):思考结束 → `1.7` → 一串 `1.8`
/// → **之后只剩心跳**,直到整个工具调用拼完才一次性下发。组装期间上游不发任何内容帧,
/// 所以这个状态要单独放宽停滞上限,而不能指望干活帧续命。
pub fn is_tool_assembly(payload: &[u8]) -> bool {
    inner_fields(payload).iter().any(|f| *f == 7 || *f == 15)
}

/// 服务端开始总结压缩上下文:`1.10 summary_started`(cursor-agent `agent.v1.InteractionUpdate`)。
///
/// 2026-11-09 生产诊断:长会话(47 万 token)自造历史后,服务端先发会话检查点与一帧被认成
/// 「本轮已提交」的 KV 回显,接着 `{1:{10:""}}`(4 字节)开始总结,之后只有心跳;总结实测
/// 39s 后才 `1.11 summary_completed`,模型再开口。不认它,排水闸 20s 就把轮次掐成空回复。
pub fn is_summary_started(payload: &[u8]) -> bool {
    inner_fields(payload).contains(&10)
}

/// 服务端总结压缩完成:`1.11 summary_completed`。
pub fn is_summary_completed(payload: &[u8]) -> bool {
    inner_fields(payload).contains(&11)
}

/// 这一帧是不是「上游在干活」的 InteractionUpdate(见 [`WORK_UPDATE_FIELDS`])。
pub fn is_work_update(payload: &[u8]) -> bool {
    inner_fields(payload)
        .iter()
        .any(|f| WORK_UPDATE_FIELDS.contains(f))
}

/// 这一帧是不是「模型正在生成、但内容不在正文/思考里」的 `1.8 token_delta`(token 数 > 0)。
///
/// 典型:模型用 Cursor 自带写文件/编辑工具生成整份文件,参数文本不下发,只有 token 计数。
/// 算「干活」而不是「进展」:续停滞闸的命,但总时长受 `WORK_ONLY_CAP` 封顶,真挂死跑不掉。
pub fn is_token_delta(payload: &[u8]) -> bool {
    status_code(payload).is_some_and(|n| n > 0)
}

/// `1.8.1` 的取值(实测见过 1/2/3/6/9)。按 agent.v1 定义 `1.8` 是 `token_delta`,
/// 这个数就是本帧新生成的 token 数 —— 所以「卡住时它是唯一还在变的东西」= 模型其实在生成,
/// 只是生成的内容不在正文/思考字段里(见 [`is_work_update`])。
pub fn status_code(payload: &[u8]) -> Option<u64> {
    for (f, v) in Reader::new(payload) {
        if f != RESP_MESSAGE {
            continue;
        }
        let PbValue::Len(sub) = v else { continue };
        for (f2, v2) in Reader::new(sub) {
            if f2 != 8 {
                continue;
            }
            let PbValue::Len(inner) = v2 else { continue };
            for (f3, v3) in Reader::new(inner) {
                if let (1, PbValue::Varint(n)) = (f3, v3) {
                    return Some(n);
                }
            }
        }
    }
    None
}
