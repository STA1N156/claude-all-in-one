//! 请求帧:字段号常量、会话阶段、附件、模型参数、首帧构造(build_frame0)、预算表。
//!
//! 2026-11-10 由 run.rs 拆出(只搬不改行为)。

use super::*;

// ── 帧 payload 顶层 ─────────────────────────────────────────────────────────
/// 帧0:RunRequest 主体。
pub(super) const FRAME_BODY: u32 = 1;
/// 帧1/帧2:流式上传的上下文条目。
pub(super) const FRAME_CONTEXT: u32 = 3;
/// 帧3/帧4:空字符串,「初始上下文发完」标记。
pub(super) const FRAME_READY: u32 = 7;

// ── 主体(帧0 field 1)内部字段 ─────────────────────────────────────────────
pub(super) const BODY_ENV: u32 = 1; // 1.1  消息与环境块
pub(super) const BODY_CONVERSATION: u32 = 2; // 1.2  会话消息块
pub(super) const BODY_EMPTY: u32 = 4; // 1.4  bin[0]
pub(super) const BODY_CONVERSATION_ID: u32 = 5; // 1.5  conversation_id
pub(super) const BODY_MODEL: u32 = 9; // 1.9  当前选中模型
pub(super) const BODY_FLAG10: u32 = 10; // 1.10 var=0
pub(super) const BODY_MODEL_CATALOG: u32 = 14; // 1.14 可用模型清单
/// `1.25` —— **每轮一个**的 turn id(与会话级的 `1.5` 区分开)。
///
/// 抓包实测:同一会话连续两轮 `1.5` 相同、`1.25` 不同。服务端的会话回显里回的
/// 也是 `1.25`,疑似用它给消息排序/去重。旧代码从不发它。
pub(super) const BODY_TURN_ID: u32 = 25;

// ── 环境块(1.1)内部 ───────────────────────────────────────────────────────
pub(super) const ENV_BUDGET: u32 = 5; // 1.1.5  token 预算分节表
pub(super) const ENV_FLAG10: u32 = 10; // 1.1.10 var=1
pub(super) const ENV_CLIENT_KIND: u32 = 22; // 1.1.22 str='ide'
pub(super) const ENV_TIMESTAMP_MS: u32 = 26; // 1.1.26 毫秒时间戳
pub(super) const ENV_TIMEZONE: u32 = 27; // 1.1.27 时区

// ── 会话块(1.2)内部 ───────────────────────────────────────────────────────
pub(super) const CONV_TURN: u32 = 1; // 1.2.1    每轮一条(repeated)
pub(super) const TURN_MESSAGE: u32 = 1; // 1.2.1.1  轮内消息
/// `1.2.1.2` —— 上下文块,**挂在轮内、与消息并列**,不是会话级的 `1.2.17`。
///
/// ## 这个字段在场与否,决定服务端生不生成
///
/// 2026-08-07 从真包逐帧削出来的结论:去掉它,上游照样回 200、照样回一帧会话回显,
/// 然后每 10 秒一个 4 字节心跳,**永不产出任何文本**;把它加回来(哪怕**长度为 0**)
/// 立刻正常出字。两个请求的字节数差 1(445 vs 446)。
///
/// 也就是说这不是「内容不够」而是「握手没完成」:服务端拿它判断本轮上下文
/// 已经声明完毕。没有任何错误码,所以只看返回值永远查不出来 —— 这是本 crate
/// 卡了整轮的那个 bug。
pub(super) const TURN_CONTEXT: u32 = 2;
/// `1.2.17` —— 会话级上下文块。**后续轮**的上下文声明住在这里。
///
/// 真客户端里它装 `{1,3,5,7}` 的 bin[32] blob 哈希 + `{2,4,6,8}` 长度 + `{9}` 环境详情。
/// 我方**只发 `.9`**:哈希指向 FileSyncService 上传过的内容,我们没上传过,
/// 一发就 `invalid_argument: Failed to resolve request context blobs`(实测)。
#[cfg(test)]
pub(super) const CONV_CONTEXT: u32 = 17;
/// `1.2.17.9` —— 与首轮 `1.2.1.2` **同构**的环境详情块(字段号一模一样)。
#[allow(dead_code)] // 协议字段号说明(我方不发 1.2.17,见 CONV_CONTEXT)
pub(super) const CTX17_DETAIL: u32 = 9;

/// 一次请求处在会话的哪个阶段。**两种阶段的报文形态差 47 倍**(实测首轮 98858B /
/// 后续轮 2121B),不是可选优化 —— 历史由服务端按 `1.5` 持有,后续轮重发历史
/// 既浪费又可能被当成新内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 首轮:全量内联上下文,`1.1` 空,上下文声明在 `1.2.1.2`。
    ///
    /// 也是**降级形态**:会话在服务端不存在(或换了账号)时一律回到这里,
    /// 把全部历史当作首轮重新铺开。
    Opening,
    /// 后续轮:只发新消息,`1.1` 带预算表,上下文声明改在 `1.2.17`。
    Continuation,
}

impl Phase {
    pub fn is_opening(self) -> bool {
        self == Phase::Opening
    }
    pub fn is_continuation(self) -> bool {
        self == Phase::Continuation
    }
}
pub(super) const MSG_TEXT: u32 = 1; // 1.2.1.1.1 纯文本
pub(super) const MSG_UUID: u32 = 2; // 1.2.1.1.2 消息 uuid
/// `1.2.1.1.3` —— **图片附件容器**(repeated)。
///
/// ⚠️ 早先记成「空字符串,抓包实物恒为空」——那是因为当时抓的都是没附件的对话。
/// 2026-08-07 带图抓包证实:它装的是图片,没图时才为空。
pub(super) const MSG_ATTACH: u32 = 3;
pub(super) const MSG_KIND: u32 = 4; // 1.2.1.1.4 var=1
pub(super) const MSG_RICH: u32 = 8; // 1.2.1.1.8 ProseMirror JSON

// ── 上下文块(1.2.1.2)内部 ─────────────────────────────────────────────────
//
// ⚠️ 这些字段号原先记在 `1.2.17.9` 下,是**路径记错了**。2026-08-07 的真包里
// `1.2.17` 只有 `{1,3,5,7}` 的 bin[32] blob 哈希与 `{2,4,6,8}` 的长度,而下面
// 这一串(4/25/26/27 与那堆开关)全部住在 `1.2.1.2`。字段号本身是对的。
//
// **别发 `1.2.17`**:它一旦出现,服务端就会去解析对应的 blob,而我们没有走
// FileSyncService 上传过任何东西 —— 实测直接 `invalid_argument:
// "Failed to resolve request context blobs"`。文件附件(§5 的 L2)做之前一律不发。
pub(super) const DET_ENV: u32 = 4; // 1.2.1.2.4  环境详情
pub(super) const DET_TOOL: u32 = 7; // 1.2.1.2.7  工具声明(repeated)
pub(super) const DET_DOC: u32 = 20; // 1.2.1.2.20 文档附件(repeated,PDF 等)
pub(super) const DET_SYSTEM_PROMPT: u32 = 25; // 1.2.1.2.25 系统提示全文
pub(super) const DET_HOOKS_A: u32 = 26; // 1.2.1.2.26 'enabled'
pub(super) const DET_HOOKS_B: u32 = 27; // 1.2.1.2.27 'enabled'

/// `1.2.1.2` 里一串布尔开关,抓包实物的原值。
///
/// 语义未知(bundle 里是压缩过的字段名),但**逐字照抄**是「完整模拟客户端」的一部分:
/// 少发这些会让我们的请求在服务端侧与真 IDE 可区分。
///
/// ⚠️ 但注意:它们**不是**生成的开关。真正决定生不生成的是 `1.2.1.2` 这个字段
/// 本身在不在场(见 [`TURN_CONTEXT`])—— 实测把这一串连同 `.4`/`.25` 全删光、
/// 只留一个长度为 0 的 `1.2.1.2`,照样正常出字。
pub(super) const DET_FLAGS: &[(u32, u64)] = &[
    (17, 1),
    (24, 1),
    (32, 1),
    (33, 0),
    (35, 0),
    (36, 1),
    (39, 1),
    (40, 1),
    (41, 1),
    (42, 1),
    (43, 1),
    (44, 1),
    (45, 0),
    (50, 1),
];
pub(super) const ENV_OS: u32 = 1; // 1.2.1.2.4.1  'linux 7.0.0-28-generic'
pub(super) const ENV_SHELL: u32 = 3; // 1.2.1.2.4.3  'zsh'
pub(super) const ENV_TZ: u32 = 10; // 1.2.1.2.4.10 时区
pub(super) const ENV_CWD: u32 = 11; // 1.2.1.2.4.11 工作目录

// ── 模型(1.9 / 1.14)内部 ──────────────────────────────────────────────────
pub(super) const MODEL_NAME: u32 = 1;
pub(super) const MODEL_PARAMS: u32 = 3; // repeated {1:key, 2:val}
pub(super) const PARAM_KEY: u32 = 1;
pub(super) const PARAM_VAL: u32 = 2;

pub(super) const ATT_UUID: u32 = 2;
pub(super) const ATT_PATH: u32 = 3; // 客户端本地路径,如 '…/images/粘贴的图像-<uuid>.png'
pub(super) const ATT_DIMS: u32 = 4; // {1: 宽, 2: 高}
pub(super) const ATT_DIM_W: u32 = 1;
pub(super) const ATT_DIM_H: u32 = 2;
pub(super) const ATT_MIME: u32 = 7; // 'image/png'
pub(super) const ATT_CONTENT: u32 = 9; // {1: bin[32] 内容哈希, 2: 原始字节}
pub(super) const ATT_HASH: u32 = 1;
pub(super) const ATT_BYTES: u32 = 2;

// ── 文档附件(1.2.1.2.20)内部 ──────────────────────────────────────────────
//
// 比图片还简单:只有路径 + 内容,没有 mime、没有哈希。
//
// ⚠️ **内容是 proto3 `string`,不是 `bytes`。** 真客户端把 PDF 当 UTF-8 文本读,
// 二进制部分被有损替换成 U+FFFD(抓包实物:`%PDF-1.4\r%` 后面紧跟三个 `\xef\xbf\xbd`)。
// 也就是说 **PDF 只有文本层能用**,这是上游客户端自己的行为,不是我们的损失。
// 我方照做:传进来的字节按 `from_utf8_lossy` 转一遍,与真客户端逐字节同构。
pub(super) const DOC_PATH: u32 = 1;
pub(super) const DOC_CONTENT: u32 = 2;

/// 一张图片附件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAttachment {
    /// mime,如 `image/png`。
    pub mime: String,
    /// 原始字节(**已 base64 解码**)。
    pub bytes: Vec<u8>,
    /// 像素宽高。解不出来时给 `(0, 0)` —— 真客户端总是填真值,
    /// 但我们只解 PNG/JPEG 头,别的格式宁可填 0 也不猜。
    pub width: u32,
    pub height: u32,
}

/// 一份文档附件(PDF 等)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocAttachment {
    /// 展示用的文件名/路径。调用方给的是 base64,没有路径,所以由我方合成。
    pub path: String,
    /// 内容。见 [`DOC_CONTENT`]:上游是 string,二进制会被有损转换。
    pub text: String,
    /// 我方抽出的文本层(`None` = 抽不到,如扫描件)。
    ///
    /// 它**不进 protobuf** —— 由 `chat.rs` 塞进用户消息文本。放在这里只是让
    /// 「一份文档」的信息聚在一处,避免调用点各自维护两个平行的列表。
    pub extracted: Option<String>,
}

impl ImageAttachment {
    /// ⚠️ 外面还有一层 `.1`:真包是 `1.2.1.1.3` → **`.1`** → {2,3,4,7,9}。
    /// 少这一层服务端回 `internal`(结构错的信号,不是 invalid_argument)。
    pub(super) fn encode(&self, seq: usize) -> Writer {
        let mut inner = Writer::new();
        let w = &mut inner;
        let id = uuid::Uuid::new_v4().to_string();
        w.string(ATT_UUID, &id);
        // 路径是客户端本地路径。我们没有真文件,合成一个同形的 —— 上游只是回显/展示用。
        let ext = self.mime.rsplit('/').next().unwrap_or("png");
        w.string(ATT_PATH, &format!("/tmp/gw-cursor/attach-{seq}-{id}.{ext}"));
        if self.width > 0 && self.height > 0 {
            let mut d = Writer::new();
            d.uint(ATT_DIM_W, self.width as u64);
            d.uint(ATT_DIM_H, self.height as u64);
            w.message(ATT_DIMS, &d);
        }
        w.string(ATT_MIME, &self.mime);
        let mut c = Writer::new();
        c.bytes(ATT_HASH, &Sha256::digest(&self.bytes));
        c.bytes(ATT_BYTES, &self.bytes);
        w.message(ATT_CONTENT, &c);
        let mut outer = Writer::new();
        outer.message(1, &inner);
        outer
    }
}

/// 从 PNG / JPEG 头里取像素宽高。**只读头,不解像素** —— 所以没有解压炸弹风险。
pub fn image_dims(bytes: &[u8]) -> (u32, u32) {
    // PNG:魔数 8 字节 + IHDR(长度4+类型4)后紧跟 width/height 各 4 字节大端。
    if bytes.len() >= 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") && &bytes[12..16] == b"IHDR" {
        let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        return (w, h);
    }
    // JPEG:扫 SOFn 段(0xC0..0xCF,排除 C4/C8/CC 这三个非 SOF)。
    if bytes.len() > 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        let mut i = 2usize;
        while i + 9 <= bytes.len() {
            if bytes[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = bytes[i + 1];
            let seg_len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
            if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
                let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
                return (w, h);
            }
            i += 2 + seg_len.max(2);
        }
    }
    (0, 0)
}

/// `1.1.22` 内层仍是字符串 `'ide'`。
///
/// ⚠️ 这与请求头 `x-cursor-client-type: glass` **故意不同** —— 抓包实物就是这样
/// (PROTOCOL §2.2 vs §3.1)。别为了「统一」把这里也改成 `glass`。
pub(super) const ENV_CLIENT_KIND_VALUE: &str = "ide";

/// 消息类型:1 = 用户。
///
/// 助手轮用 2 是**推断**(沿用退役的 ChatService 的 HUMAN=1/AI=2 约定),
/// 抓包只覆盖了用户轮。多轮历史若表现异常,这里是第一个怀疑点。
pub(super) const MSG_KIND_USER: u64 = 1;
pub(super) const MSG_KIND_ASSISTANT: u64 = 2;

/// 一个模型及其参数(`1.9` / `1.14` 同构)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub name: String,
    /// `1.9.3` 的 `{1:key, 2:val}` 列表。
    ///
    /// 值的类型文档没写(`fast=false` / `context=300k` / `effort=high` 混在一起),
    /// 这里一律按字符串编码 —— 与 `effort=high`、`context=300k` 这类字面量一致。
    pub params: Vec<(String, String)>,
    /// 是否进 `1.14` 可用模型清单(每个 Run 请求都带整份)。
    ///
    /// 探测中的模型(还没在真机菜单里证实)必须 `false`:可选中(`1.9` 当前模型),
    /// 但不进清单 —— 未证实条目混进 `1.14` 会污染**所有**模型的请求
    /// (审查 gpt-5.6-sol 高危),也让我们的清单与任何真实客户端版本都对不上。
    pub menu_visible: bool,
}

impl Model {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            params: Vec::new(),
            menu_visible: true,
        }
    }

    pub fn with_params(name: impl Into<String>, params: &[(&str, &str)]) -> Self {
        Self {
            name: name.into(),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            menu_visible: true,
        }
    }

    /// 标记为探测项:可选中但不进 `1.14` 清单(见 `menu_visible` 注释)。
    pub fn probe(mut self) -> Self {
        self.menu_visible = false;
        self
    }

    /// `pub(crate)`:CLI 形态的首轮 `1.14` 目录复用同一编码(见 `cli.rs`)。
    pub(crate) fn encode(&self) -> Writer {
        let mut w = Writer::new();
        w.string(MODEL_NAME, &self.name);
        for (k, v) in &self.params {
            let mut p = Writer::new();
            p.string(PARAM_KEY, k);
            p.string(PARAM_VAL, v);
            w.message(MODEL_PARAMS, &p);
        }
        w
    }
}

/// 帧0 里哪些可疑分节要发。
///
/// 存在的理由见模块头:文档没做过最小化实验,「必需字段集」是未知的。
/// 出问题时靠关掉分节来二分,而不是重写编码器。
#[derive(Debug, Clone, Copy)]
pub struct RunShape {
    /// `1.1` 整个环境块(时间戳/时区/client kind)。
    pub env_block: bool,
    /// `1.1.5` token 预算分节表。
    pub budget_table: bool,
    /// `1.2.1.1.8` ProseMirror 富文本(与纯文本同内容)。
    pub prosemirror: bool,
    /// `1.14` 可用模型清单。
    pub model_catalog: bool,
    /// `1.2.17` 大上下文块(环境详情 + 系统提示)。
    pub context_block: bool,
}

impl Default for RunShape {
    fn default() -> Self {
        // 目标是**尽量完整模拟客户端**,所以默认全发。这些开关的用途是出问题时二分
        // 定位哪一节不被接受,不是用来长期精简请求 —— 少发字段会让我们的请求在
        // 服务端侧与真 IDE 可区分,那正是要避免的。
        Self {
            env_block: true,
            budget_table: true,
            prosemirror: true,
            model_catalog: true,
            context_block: true,
        }
    }
}

/// 一轮对话消息(已从 Anthropic 请求体抽成纯文本)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub text: String,
    pub is_user: bool,
}

/// 本次请求携带的媒体附件。
///
/// 打成一个结构体而不是两个参数:它们**去处不同**(图片挂消息、文档挂上下文块),
/// 但来源相同(调用方同一条消息里的 content 块),放一起不容易漏传。
#[derive(Debug, Clone, Copy, Default)]
pub struct Media<'a> {
    /// 图片 → `1.2.1.1.3`(挂在**最后一条用户消息**上)。
    pub images: &'a [ImageAttachment],
    /// 文档 → `1.2.1.2.20`(挂在上下文块里)。
    pub docs: &'a [DocAttachment],
}

/// 构造帧0 的 payload(**未** gzip、**未**分帧)。
///
/// `catalog` 是 `1.14` 的可用模型清单 —— 真 IDE 每次 Run 都把整份清单回传
/// (它拿来渲染模型下拉框)。为了与真客户端不可区分,这里也照发,
/// 见 [`crate::models::catalog`]。`now_ms` 可注入以便测试确定性。
pub fn build_frame0(
    turns: &[Turn],
    system: &str,
    tools: &[ToolDef],
    media: Media<'_>,
    model: &Model,
    catalog: &[Model],
    conversation_id: &str,
    timezone: &str,
    now_ms: u64,
    shape: RunShape,
    phase: Phase,
) -> Vec<u8> {
    let mut body = Writer::new();

    let system_chars = system.chars().count();
    let conv_chars: usize = turns.iter().map(|t| t.text.chars().count()).sum();

    // 1.1 环境块。
    //
    // ⚠️ **首轮它是空的**(抓包实物 `1.1 <0B>` —— 字段在、长度 0),后续轮才装
    // 预算表 + 时间戳 + 时区。这不是可有可无的差别:首轮那 98KB 上下文是内联发的,
    // 服务端自己看得见;后续轮上下文在服务端手里,客户端只能**报账**告诉它各节多大。
    if shape.env_block {
        let mut env = Writer::new();
        if phase.is_continuation() {
            if shape.budget_table {
                env.message(ENV_BUDGET, &budget_table(system_chars, conv_chars));
            }
            env.uint(ENV_FLAG10, 1);
            env.string(ENV_CLIENT_KIND, ENV_CLIENT_KIND_VALUE);
            env.uint(ENV_TIMESTAMP_MS, now_ms);
            env.string(ENV_TIMEZONE, timezone);
        }
        body.message(BODY_ENV, &env);
    }

    // 1.2 会话块
    //
    // 上下文块 `1.2.1.2` 挂在**最后一轮**(也就是本次要回答的那条用户消息)上,
    // 与真客户端一致 —— 抓包实物里 `1.2.1` 只有一条,历史由服务端按
    // conversation_id 自己持有。挂到每一轮会把同一块内容重复几十 KB。
    let mut detail = Writer::new();
    if shape.context_block {
        let mut envd = Writer::new();
        envd.string(
            ENV_OS,
            &format!("{} {}", crate::wire::CLIENT_OS, env_os_release()),
        );
        envd.string(ENV_SHELL, "bash");
        envd.string(ENV_TZ, timezone);
        envd.string(ENV_CWD, "/");

        detail.message(DET_ENV, &envd);
        // 系统提示。实测有效:塞一句「只准回答 PINEAPPLE」进去,模型就照做,
        // 且它的思考里把这段称作 hooks_context。不发时服务端用自己那套。
        if !system.is_empty() {
            detail.string(DET_SYSTEM_PROMPT, system);
        }
        // 调用方声明的工具。真包里这一位放的是 MCP 工具,格式逐字对齐(见 [`ToolDef`])。
        // ⚠️ Cursor 的**内建**工具(终端/读文件)不在这里 —— 它们是服务端自带的,
        // 所以即使我们一个工具都不声明,模型照样会去调内建工具。
        for t in tools {
            detail.message(DET_TOOL, &t.encode());
        }
        // 文档(PDF 等)。与图片不同,它们挂在**上下文块**里而不是消息上 —— 抓包实物如此。
        for d in media.docs {
            let mut w = Writer::new();
            w.string(DOC_PATH, &d.path);
            w.string(DOC_CONTENT, &d.text);
            detail.message(DET_DOC, &w);
        }
        detail.string(DET_HOOKS_A, "enabled");
        detail.string(DET_HOOKS_B, "enabled");
        for (f, v) in DET_FLAGS {
            detail.uint(*f, *v);
        }
    }

    // `1.2.1.2` 是出不出字的开关(见 TURN_CONTEXT),而空 `turns` 会让下面的循环
    // 一次都不执行 → 造出的正是那个 445B 的静默挂起形态。invariant 收回函数本地:
    // 这是 `pub fn`,不能只靠 `chat_stream` 那一个调用点偶然守住。
    assert!(
        !turns.is_empty(),
        "build_frame0 需要至少一轮消息,否则会造出不生成的请求"
    );

    let mut conv = Writer::new();
    // 上下文块挂**最后一条用户轮**,不是最后一轮。Anthropic 允许以 assistant 消息
    // 结尾(prefill:`[user, assistant:"答案是"]` 让模型续写),那时最后一轮是助手轮 ——
    // 而抓包实物里这个块永远长在用户轮上。挂错轮若让服务端判不出「上下文声明完毕」,
    // 就重新落回静默挂起,且那是一类没有错误码的失败。
    let last = turns
        .iter()
        .rposition(|t| t.is_user)
        .unwrap_or(turns.len() - 1);
    // 后续轮**只发最后那条用户消息**:历史在服务端手里,重发既浪费又可能被当成新内容。
    // 抓包实物 turn2 的 `1.2.1` 就只有一条(整个请求 2121B,对比首轮 98858B)。
    let send: Vec<(usize, &Turn)> = match phase {
        Phase::Opening => turns.iter().enumerate().collect(),
        Phase::Continuation => vec![(last, &turns[last])],
    };
    for (i, t) in send {
        let mut msg = Writer::new();
        msg.string(MSG_TEXT, &t.text);
        msg.string(MSG_UUID, &uuid::Uuid::new_v4().to_string());
        // 附件容器。这一轮有图就装图,没图发空 —— **不能两个都发**,
        // 否则 `1.2.1.1.3` 出现两次,读侧取到的是空的那个。
        let carries_images = i == last && !media.images.is_empty();
        if !carries_images {
            msg.string(MSG_ATTACH, "");
        }
        msg.uint(
            MSG_KIND,
            if t.is_user {
                MSG_KIND_USER
            } else {
                MSG_KIND_ASSISTANT
            },
        );
        if shape.prosemirror {
            // 只有承载附件的那一条带 mention 节点。
            let mentions: Vec<String> = if i == last {
                media.docs.iter().map(|d| d.path.clone()).collect()
            } else {
                Vec::new()
            };
            msg.string(MSG_RICH, &prosemirror_doc(&t.text, &mentions));
        }
        // 图片挂**最后一条用户消息**(与上下文块同一条)。历史轮里的图我们一并挂到
        // 当前这条 —— 反正 fold_history 已经把历史折成一条了。
        if carries_images {
            for (n, img) in media.images.iter().enumerate() {
                msg.message(MSG_ATTACH, &img.encode(n));
            }
        }
        let mut turn = Writer::new();
        turn.message(TURN_MESSAGE, &msg);
        // ⚠️ 「上下文声明」是出不出字的开关,不是可选装饰(见 TURN_CONTEXT)。
        // 但**它在两种轮次里住在不同地方**:首轮挂轮内 `1.2.1.2`,后续轮改挂会话级
        // `1.2.17`(抓包实物 turn2 的 `1.2.1` 里根本没有 `.2`)。所以这里只在首轮发。
        // ⭐ **两种轮次都挂在这里。** 2026-08-08 实测三个变体(见 PROTOCOL §17):
        //   A 后续轮发 `1.2.17.9`         → 挂起(只剩 10s 心跳)
        //   B 后续轮什么都不发            → 2s 返回但**丢历史**
        //   C 后续轮也挂轮内 `1.2.1.2`    → ✅ 出字 + 记得历史 + 98.7% 缓存命中
        // 早先「后续轮改挂会话级 1.2.17」的判断是错的 —— 那是照抄真客户端的
        // 位置,但真客户端那个块里有 FileSync 上传过的 blob 哈希,我们没有;
        // 半个块(只有 `.9`)比不发更糟,服务端会等一个永远不来的 blob。
        if i == last {
            turn.message(TURN_CONTEXT, &detail);
        }
        conv.message(CONV_TURN, &turn);
    }
    // ⚠️ **永远不发 `1.2.17`**(会话级上下文块)。真客户端后续轮发它,但里面是 4 个
    // 经 FileSyncService 上传的 blob 哈希;我们一个都没上传:
    //   带哈希 → `invalid_argument: Failed to resolve request context blobs`
    //   只发 `.9` 不带哈希 → 服务端静默等一个永不到来的 blob,10s 心跳到超时
    // 声明改挂轮内 `1.2.1.2`(上面那段),两种轮次同一个位置。
    body.message(BODY_CONVERSATION, &conv);

    // 1.4 空 bytes(抓包里恒为 bin[0])
    body.bytes(BODY_EMPTY, &[]);
    // 1.5 conversation_id
    body.string(BODY_CONVERSATION_ID, conversation_id);
    // 1.9 当前模型
    body.message(BODY_MODEL, &model.encode());
    // 1.10 var=0
    body.uint(BODY_FLAG10, 0);
    // 1.25 本轮 turn id。每轮必须换 —— 复用同一个值等于告诉服务端「还是那一轮」。
    body.string(BODY_TURN_ID, &uuid::Uuid::new_v4().to_string());
    // 1.14 可用清单(repeated,每个模型一条;探测项不进清单,见 Model::menu_visible)
    if shape.model_catalog {
        for m in catalog {
            if !m.menu_visible {
                continue;
            }
            body.message(BODY_MODEL_CATALOG, &m.encode());
        }
    }

    let mut frame = Writer::new();
    frame.message(FRAME_BODY, &body);
    frame.into_bytes()
}

/// 上下文窗口上限,抓包实物里 `1.1.5.2` 恒为 256000。
pub(super) const BUDGET_MAX_TOKENS: u64 = 256_000;

/// 客户端自报的 OS 版本(拼进 `1.2.17.9.4.1`,真值形如 `linux 7.0.0-28-generic`)。
pub(super) fn env_os_release() -> &'static str {
    crate::wire::CLIENT_OS_VERSION
}

/// 一个 token 预算分节 `{1:key, 2:显示名, 3:tokens, 4:chars}`。
///
/// tokens 用 chars/4 粗估 —— 我们没有 Cursor 的分词器,而这张表是客户端自报的账。
pub(super) fn budget_section(key: &str, display: &str, chars: usize) -> Writer {
    let mut w = Writer::new();
    w.string(1, key);
    w.string(2, display);
    w.uint(3, (chars as u64).div_ceil(4));
    w.uint(4, chars as u64);
    w
}

/// `1.1.5` token 预算表。
///
/// ⚠️ 结构是**三层**,不是一层:`{1: 合计tokens, 2: 上限, 3: {1: 合计, 2: 上限, 3: [分节…]}}`。
/// 早先按一层写(分节直接挂 `1.1.5`)会让服务端解析崩掉并回 `internal`。
/// 合计必须等于各分节 tokens 之和 —— 抓包实物里
/// `467+9517+3246+5081+2243+1106+0+8891 = 30551` 正好等于 `1.1.5.1`。
///
/// 真客户端报 8 节;反代只有 system_prompt 与 conversation 是真的,其余恒 0 ——
/// **这是有意的**:声明 tools/mcp/subagents 会让模型回工具调用,而我们无法应答
/// (抓包里那条 `cursor-app-control-move_agent_to_root` 就是这么来的)。
///
/// `pub(crate)`:CLI 形态的后续轮预算表复用同一张表(见 `cli.rs`)。
pub(crate) fn budget_table(system_chars: usize, conv_chars: usize) -> Writer {
    let sections: [(&str, &str, usize); 8] = [
        ("system_prompt", "System prompt", system_chars),
        ("tools", "Tool definitions", 0),
        ("rules", "Rules", 0),
        ("skills", "Skills", 0),
        ("mcp", "MCP & dynamic tools", 0),
        ("subagents", "Subagent definitions", 0),
        ("summarized_conversation", "Summarized conversation", 0),
        ("conversation", "Conversation", conv_chars),
    ];
    let total: u64 = sections
        .iter()
        .map(|(_, _, c)| (*c as u64).div_ceil(4))
        .sum();

    let mut inner = Writer::new();
    inner.uint(1, total);
    inner.uint(2, BUDGET_MAX_TOKENS);
    for (k, d, c) in sections {
        inner.message(3, &budget_section(k, d, c));
    }

    let mut outer = Writer::new();
    outer.uint(1, total);
    outer.uint(2, BUDGET_MAX_TOKENS);
    outer.message(3, &inner);
    outer
}

/// 同一句话的富文本形态。真 IDE 对每条用户消息同时发纯文本与 ProseMirror JSON。
pub(super) fn prosemirror_doc(text: &str, mentions: &[String]) -> String {
    let mut content: Vec<Value> = Vec::new();

    // 文档「提及」节点。**这是模型知道有附件的唯一途径** ——
    // `1.2.1.2.20` 只是内容登记表,靠**路径**连接;真客户端把路径同时写进用户文本
    // 和这个 mentionNode 里。少了它,模型看到的只是一句没有指向的问话,
    // 于是它会说「我来找这个文件」然后无从下手(实测如此)。
    if !mentions.is_empty() {
        let nodes: Vec<Value> = mentions
            .iter()
            .map(|p| {
                let uri = format!("file:file://{}", percent_encode_path(p));
                json!({
                    "type": "mentionNode",
                    "attrs": {
                        "id": uri,
                        "label": p.rsplit('/').next().unwrap_or(p),
                        "mentionSuggestionChar": "@",
                        "uuid": uri,
                        "rawText": p,
                        "plainText": Value::Null,
                        "chipIcon": Value::Null,
                        "mentionTypeRaw": "file",
                        "mentionType": "file",
                        "secondaryText": p.rsplitn(2, '/').nth(1).unwrap_or(""),
                        "payload": {
                            "case": "fileSelection",
                            "uri": { "scheme": "file", "path": p }
                        }
                    }
                })
            })
            .collect();
        content.push(json!({"type":"paragraph","content": nodes}));
    }

    if text.is_empty() {
        if content.is_empty() {
            content.push(json!({"type": "paragraph"}));
        }
    } else {
        for line in text.split('\n') {
            content.push(if line.is_empty() {
                json!({"type": "paragraph"})
            } else {
                json!({"type":"paragraph","content":[{"type":"text","text":line}]})
            });
        }
    }
    json!({"type": "doc", "content": content}).to_string()
}

/// 路径的百分号编码(只转非 ASCII 与空格,`/` 保留)。
///
/// 真客户端的 mentionNode id 里中文路径是 `%E4%B8%8B%E8%BD%BD` 这种形态。
pub(super) fn percent_encode_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for b in p.as_bytes() {
        match b {
            b'/' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            b if b.is_ascii_alphanumeric() => out.push(*b as char),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 帧0 之后紧跟的开场帧序列(逐字照抄抓包实物)。
///
/// ## ⚠️ 它们**不是**生成的必要条件(2026-08-07 更正)
///
/// 这里原先写着「只发帧0 上游就只心跳、永不生成」—— **那个归因是错的**。
/// 从真包做减法证明:只发帧0(丢掉后面全部 26 帧)照样正常出字;真正的开关是帧0 里的
/// `1.2.1.2`(见 [`TURN_CONTEXT`])。当时两件事同时缺,把因果记到了帧上。
///
/// 仍然照发,是因为目标是**模拟一个完整的客户端**:真 IDE 发它们,不发就是一处
/// 可被服务端区分的差异。但别再把它们当成"修复"——删掉不会影响出字。
///
/// 真客户端(3.14.27)紧跟着发的是:
///
/// ```text
/// 帧1  {3: {3: ""}}            field 3,2 字节
/// 帧2  {3: {1: 1, 3: ""}}      field 3,4 字节
/// 帧3  {7: ""}                 field 7,2 字节
/// 帧4  {7: ""}                 field 7,2 字节
/// ```
///
/// 之后真客户端才发 field 2(MCP 工具定义,9.5KB×3)—— **那部分我们故意不发**:
/// 声明了工具,模型就会回工具调用,而文本代理无法应答
/// (抓包里那条 `cursor-app-control-move_agent_to_root` 就是这么来的)。
///
/// 全部是**未压缩**帧(flag=0x00),抓包实物如此 —— 几字节的东西压了反而更大。
pub fn build_prelude_frames() -> Vec<Vec<u8>> {
    // 帧1:{3:{3:""}}
    let mut a_inner = Writer::new();
    a_inner.string(3, "");
    let mut a = Writer::new();
    a.message(FRAME_CONTEXT, &a_inner);

    // 帧2:{3:{1:1, 3:""}}
    let mut b_inner = Writer::new();
    b_inner.uint(1, 1);
    b_inner.string(3, "");
    let mut b = Writer::new();
    b.message(FRAME_CONTEXT, &b_inner);

    // 帧3/帧4:{7:""}
    let mut c = Writer::new();
    c.string(FRAME_READY, "");
    let mut d = Writer::new();
    d.string(FRAME_READY, "");

    vec![
        a.into_bytes(),
        b.into_bytes(),
        c.into_bytes(),
        d.into_bytes(),
    ]
}
