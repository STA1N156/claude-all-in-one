//! 模型名映射与上下文窗口 —— **单一权威表**。
//!
//! `KIRO_MODELS` 是唯一事实源:对外裸名 ↔ 上游 Kiro 模型 ↔ 身份短名 ↔ 上下文窗口 ↔
//! thinking 能力 ↔ 日期快照别名。`/v1/models` 目录、chat 路由(`map_model`)、身份规范化
//! (`normalize::requested_model_identity`)、窗口推导(`get_context_window_size`)**全部从此表派生**,
//! 杜绝历史上"四处列表不一致 → 公告了 chat 却 None / 真实代号泄漏"的漂移。
//!
//! 对外形态由 [`resolve_base`] 统一归一到基础行:plain、`-thinking` / `-reasoning`
//! 后缀、日期别名及其对应后缀变体。

use crate::text_tokens::Tokenizer;

/// 一个基础 Kiro 模型(对外裸名维度)。日期/thinking 变体由它派生,不单列。
pub struct KiroModel {
    /// 对外裸名(客户端看到、NewAPI 拉取),如 `claude-opus-4-8`。
    pub advertised_id: &'static str,
    /// 展示名,如 `Claude Opus 4.8`。
    pub display_name: &'static str,
    /// 上游真实 Kiro 模型 id(发包用),如 `claude-opus-4.8`。
    pub kiro_model: &'static str,
    /// 身份行短名(`You are powered by the model named {short}`),防 `claude-quince` 泄漏。
    pub identity_short: &'static str,
    /// 上下文窗口 token 数。
    pub context_window: i32,
    /// 该裸名是否支持 thinking(thinking 变体恒 true,此处是 plain 的天然能力)。
    pub supports_thinking: bool,
    /// 官方日期快照别名(有才填),如 `claude-opus-4-5-20251101`;用于公告目录。
    pub dated_alias: Option<&'static str>,
    /// 该模型**实际可用**的 thinking effort 档位,升序。空 = 上游没给
    /// `additionalModelRequestFieldsSchema` → **一个字节都不该发**该字段。
    ///
    /// 逐模型不同,见 [`EFFORTS_WITH_XHIGH`] / [`EFFORTS_NO_XHIGH`] 的说明。
    pub effort_levels: &'static [&'static str],
    /// schema 里标注的 `default`。请求档位不可用时回落到它(对齐真客户端 `A7`)。
    pub default_effort: Option<&'static str>,
    /// thinking 签名的归属标识方式:f2.f1.f6 里的**上游内部模型代号**,或(v4 信封起)
    /// 无代号、只能按信封版本匹配。
    ///
    /// 2026-08-19 探针实测(`kiro_think_probe4/5/6.py`):opus-4.8=`claude-quince`、
    /// opus-5=`claude-honey`、sonnet-5=`claude-saffron`、opus-4.7 的代号就是官方名
    /// `claude-opus-4-7` 本身;opus-4.6 / sonnet-4.6 / 4.5 系 / haiku **没有原生签名推理**
    /// (无 reasoningContentEvent 签名帧,或 inline `<thinking>` 文本形态),它们的客户端
    /// 签名是我方合成的假签名,上行必过不了验签 → `Unsigned`,历史 thinking 不上传
    /// (对齐官方 `reasoning.unsigned.dropped`)。2026-09-26:opus-5.5 为 v4 匿名信封。
    ///
    /// 透传原则下代号不做改写/反写,本字段的用途是:「该模型可否做结构化
    /// reasoningContent 历史上传」的门控 + 签名归属匹配(见 [`SignatureId`])。
    pub signature_id: SignatureId,
    /// 上游**只认**结构化 `additionalModelRequestFields.output_config.effort`,忽略旧文本标签
    /// `<thinking_effort>`。为 true 时即便开着 legacy 线缆(`KIRO_LEGACY_WIRE`),也必须随请求
    /// 发结构化字段,否则客户端设的思考强度一律不生效、按上游默认档跑。
    ///
    /// 2026-09-29 真号 A/B(同一账号、同一道题,max 档):opus-5.5 结构化字段 → 签名 35~41k、
    /// 输出 ~3000 token、~145s,low→xhigh→max 严格单调;旧标签 → 5~15k、~450 token,与 low 无差。
    /// opus-5 / 4.8 旧标签照常生效(max 档 5~14k 字符思考、93~243s),保持 false 不动线缆。
    /// 2026-10-03 sonnet-5.5 同 opus-5.5(旧标签不分档,结构化严格分档)→ true。
    pub structured_effort_only: bool,
    /// 上游会把「在正文里写 `<thinking>`」的指令判成 `REASONING_EXTRACTION` 拒答
    /// (`metadataEvent.stopDetails.refusal`,零产出)。为 true 时,转换器把 Claude Code
    /// 自动模式安全分类器第二段的这类指令改写成不索取可见推理的等价说法,见
    /// `converter::classifier_prompt`。
    ///
    /// 2026-09-29 真号直放实测(opus-5.5,同一报文):原样 → 拒答;去掉结构化 effort 仍拒答
    /// (与 effort 字段无关);改写后两条样本都正常出 `<block>`,模型走原生推理(约 800 字符)。
    /// sonnet-5 同一类请求 11/11 正常,保持 false 不动。2026-10-03 sonnet-5.5 原句同样被拒 → true。
    pub rewrite_visible_thinking_directive: bool,
    /// 分词器代际:输出 token 按字符估算时选系数(见 [`crate::text_tokens::Tokenizer`])。
    /// 2026-10-06 实测:opus-4.8 / opus-5 / sonnet-5 / *-5.5 是新分词器,opus-4.6 是旧的;
    /// opus-4.7 按发布代际归新,4.5 系 / sonnet-4.6 / haiku-4.5 归旧。
    pub tokenizer: Tokenizer,
    /// 思考强度字段走哪条 schema 路径,见 [`EffortSchema`]。
    pub effort_schema: EffortSchema,
    /// 只派给付费档(PRO 及以上),**FREE 号不派**。依据是 FREE 号的上游目录里没有它
    /// (派过去只会吃 400 INVALID_MODEL_ID 再换号)。opus 的 FREE 限制是另一条更早的子串规则
    /// (`account_supports_model` 第③条),各 opus 行这里仍填 false,两条是「或」的关系。
    pub pro_only: bool,
}

/// 一个模型的 thinking 签名**怎样标识归属** —— 决定历史 thinking 能否以结构化
/// `reasoningContent` 回传上游(回传失败 = 多轮对话丢推理 = 用户感知「降智」)。
///
/// 官方客户端是把「签发模型 id」和推理**一起存**、回传时比对(reasoningModelId 不匹配即丢);
/// 我们无状态,只能从签名明文里读归属,而信封格式随模型代际在变:
///
/// | 信封 | 顶层 f1 | f2.f1.f6 代号 | 实测模型 |
/// |---|---|---|---|
/// | 旧 | 无 | 有 | opus-4.8 |
/// | v2 | 2 | 有 | opus-5 |
/// | v4 | 4 | **无**(f5 也无) | opus-5.5、sonnet-5.5、haiku-5.5 |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureId {
    /// 无原生签名推理(无 reasoningContentEvent 签名帧,或 inline `<thinking>` 文本形态)。
    /// 客户端手里的签名只可能是我方合成的,上行必过不了验签 → 历史 thinking 不上传
    /// (对齐官方 `reasoning.unsigned.dropped`)。
    Unsigned,
    /// 签名 f2.f1.f6 携带上游内部代号,必须逐字相等才上传。
    Codename(&'static str),
    /// 信封**不带**模型代号,只能按信封版本匹配:顶层 f1 == `envelope_version` 且确无 f6
    /// 才上传。跨模型误挂(另一个同版本信封的模型签发)无法在本地识别,由上游验签兜底
    /// (`THINKING_SIGNATURE_INVALID` → 剥全历史 reasoning 重试一次,见 chat.rs)。
    ///
    /// 2026-10-03 实测这不是假设:opus-5.5 与 sonnet-5.5 都是 v4,且签名明文头逐字段相同
    /// (f1=18 / f2=1 / f3=2 / f7=1 / f8=`thinking`,顶层 f3=1),同一会话中途换模型时
    /// 会把对方签发的推理一并回传。
    Anonymous { envelope_version: u64 },
}

/// 思考强度写在 `additionalModelRequestFields` 的**哪条路径**下。上游按模型分流
/// (`extension.js` 的 `schemaPath` 分支):Anthropic 系走 `output_config`,
/// 非 Anthropic 系(gpt-5.6 系、kimi-k3)走 `reasoning`。
///
/// 这是**真理来源唯一化**的字段:别在构造字段的地方按模型名 `contains("kimi")` 判 ——
/// 那会让「某模型走哪条路径」散落在两处,迟早漂。发错路径不报错,只是强度设置**静默失效**,
/// 客户端以为在用 max 档、实际按上游默认跑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortSchema {
    /// `{"output_config": {"effort": "..."}}` —— Anthropic 系。
    OutputConfig,
    /// `{"reasoning": {"effort": "..."}}` —— kimi-k3 等非 Anthropic 系。
    Reasoning,
}

/// **3 档,没有 `medium`/`xhigh`**。kimi-k3 的 `reasoning.effort` enum 实测就这三个
/// (2026-10-23 真号 `List-Available-Models` 回包)。中间缺一档不是笔误 —— 上游就这么给的。
const EFFORTS_KIMI_K3: &[&str] = &["low", "high", "max"];

/// 5 档全集。opus-5 / sonnet-5 / opus-4.8 / opus-4.7 实测为此形态。
const EFFORTS_WITH_XHIGH: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// **4 档,没有 `xhigh`**。opus-4.6 / sonnet-4.6 实测为此形态 —— 4.6 系直接从 `high`
/// 跳到 `max`。此前 caio 对所有 Opus 硬编码 `xhigh`,对这两个模型就是发了一个
/// 上游 enum 里根本不存在的值。
const EFFORTS_NO_XHIGH: &[&str] = &["low", "medium", "high", "max"];

/// 上游没有 `additionalModelRequestFieldsSchema` 的模型(4.5 系、haiku)。
/// 真客户端此时 `additionalModelRequestFields: undefined`(`extension.js:223145`),
/// 我们也必须一个字段都不发。
const EFFORTS_NONE: &[&str] = &[];

/// **权威表**。新增/下线模型只改这里——派生点自动跟随。
///
/// `effort_levels` / `default_effort` 抄自 2026-07-28 用真号打
/// `ListAvailableModels` 取回的 `additionalModelRequestFieldsSchema`
/// (存档 `/root/caio-backup/kiro-models-1.0.212.json`)。**不要凭印象改** ——
/// 发一个该模型 enum 里没有的档位既可能 400,也是可被规则化的形态差异。
pub const KIRO_MODELS: &[KiroModel] = &[
    KiroModel {
        // 2026-09-26 上游新增(目录标注 "Experimental preview",rateMultiplier 2.0,比 opus-5 的
        // 2.2 便宜)。与 4.x 同规律:上游 modelId 带点号 `claude-opus-5.5`,对外名用连字符
        // `claude-opus-5-5`(对齐 Anthropic 官方 id)。原始目录存档 `kiro-models-20260926.json`(与 1.0.212 那份同目录)。
        //
        // 与 opus-5 的 schema 差异只有两处:
        // - `output_config.effort.default` = `medium`(opus-5 是 `high`);
        // - `thinking.type` 的 enum **只有 `adaptive`,没有 `disabled`** —— 思考关不掉。
        //   caio 对 disabled 本就是「不发 additionalModelRequestFields」(见 thinking_policy),
        //   从不把 `disabled` 写上线缆,故无需特判。
        //
        // 线缆上的差异:thinking 签名换成 **v4 信封,不再携带模型代号**(见 [`SignatureId`])。
        // 若沿用「读 f6 比代号」,5.5 的历史推理会被**全部**丢弃、多轮对话降智。
        advertised_id: "claude-opus-5-5",
        display_name: "Claude Opus 5.5",
        kiro_model: "claude-opus-5.5",
        identity_short: "Opus 5.5",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
        effort_levels: EFFORTS_WITH_XHIGH,
        default_effort: Some("medium"),
        signature_id: SignatureId::Anonymous { envelope_version: 4 },
        structured_effort_only: true,
        rewrite_visible_thinking_directive: true,
        tokenizer: Tokenizer::Current,
        effort_schema: EffortSchema::OutputConfig,
        pro_only: false,
    },
    KiroModel {
        // 2026-07-25 上游新增。与 sonnet-5 同规律:modelId 是主版本裸名
        // `claude-opus-5`(无 x.y 点号,与 4.x 各行不同)。
        advertised_id: "claude-opus-5",
        display_name: "Claude Opus 5",
        kiro_model: "claude-opus-5",
        identity_short: "Opus 5",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
        effort_levels: EFFORTS_WITH_XHIGH,
        default_effort: Some("high"),
        signature_id: SignatureId::Codename("claude-honey"),
        structured_effort_only: false,
        rewrite_visible_thinking_directive: false,
        tokenizer: Tokenizer::Current,
        effort_schema: EffortSchema::OutputConfig,
        pro_only: false,
    },
    KiroModel {
        advertised_id: "claude-opus-4-8",
        display_name: "Claude Opus 4.8",
        kiro_model: "claude-opus-4.8",
        identity_short: "Opus 4.8",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
        effort_levels: EFFORTS_WITH_XHIGH,
        default_effort: Some("high"),
        signature_id: SignatureId::Codename("claude-quince"),
        structured_effort_only: false,
        rewrite_visible_thinking_directive: false,
        tokenizer: Tokenizer::Current,
        effort_schema: EffortSchema::OutputConfig,
        pro_only: false,
    },
    KiroModel {
        advertised_id: "claude-opus-4-7",
        display_name: "Claude Opus 4.7",
        kiro_model: "claude-opus-4.7",
        identity_short: "Opus 4.7",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
        effort_levels: EFFORTS_WITH_XHIGH,
        // 全表唯一 default 不是 high 的模型(上游 schema 逐字如此)。
        default_effort: Some("xhigh"),
        signature_id: SignatureId::Codename("claude-opus-4-7"),
        structured_effort_only: false,
        rewrite_visible_thinking_directive: false,
        tokenizer: Tokenizer::Current,
        effort_schema: EffortSchema::OutputConfig,
        pro_only: false,
    },
    KiroModel {
        advertised_id: "claude-opus-4-6",
        display_name: "Claude Opus 4.6",
        kiro_model: "claude-opus-4.6",
        identity_short: "Opus 4.6",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
        effort_levels: EFFORTS_NO_XHIGH,
        default_effort: Some("high"),
        signature_id: SignatureId::Unsigned,
        structured_effort_only: false,
        rewrite_visible_thinking_directive: false,
        tokenizer: Tokenizer::Legacy,
        effort_schema: EffortSchema::OutputConfig,
        pro_only: false,
    },
    KiroModel {
        advertised_id: "claude-opus-4-5",
        display_name: "Claude Opus 4.5",
        kiro_model: "claude-opus-4.5",
        identity_short: "Opus 4.5",
        context_window: 200_000,
        supports_thinking: true,
        dated_alias: Some("claude-opus-4-5-20251101"),
        effort_levels: EFFORTS_NONE,
        default_effort: None,
        signature_id: SignatureId::Unsigned,
        structured_effort_only: false,
        rewrite_visible_thinking_directive: false,
        tokenizer: Tokenizer::Legacy,
        effort_schema: EffortSchema::OutputConfig,
        pro_only: false,
    },
    KiroModel {
        // 2026-10-03 上游目录新增(付费号实拉,第 21 个模型)。与 opus-5.5 同规律:
        // 上游 modelId 带点号 `claude-sonnet-5.5`,对外名用连字符 `claude-sonnet-5-5`。
        // schema:1M 入 / 128K 出(sonnet-5 是 64K 出),rateMultiplier 1.3(与 sonnet-5 同价),
        // `output_config.effort` 5 档、default=`high`。
        // 末三列均为 2026-10-03 真号直放实测(见 CHANGELOG [sonnet-5.5]),与 opus-5.5 完全同形:
        // - 签名:v4 无代号信封,明文头与 opus-5.5 逐字段相同;带签名的历史推理回传 → 200 正常收尾;
        // - 思考强度:旧标签 low/xhigh 思考量互相重叠(1.1k~4k 字符),结构化 low ≈1.1k / max ≈7~8k;
        // - 分类器第二段原句 → REASONING_EXTRACTION 拒答,改写后正常出 <block>。
        advertised_id: "claude-sonnet-5-5",
        display_name: "Claude Sonnet 5.5",
        kiro_model: "claude-sonnet-5.5",
        identity_short: "Sonnet 5.5",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
        effort_levels: EFFORTS_WITH_XHIGH,
        default_effort: Some("high"),
        signature_id: SignatureId::Anonymous { envelope_version: 4 },
        structured_effort_only: true,
        rewrite_visible_thinking_directive: true,
        tokenizer: Tokenizer::Current,
        effort_schema: EffortSchema::OutputConfig,
        pro_only: false,
    },
    KiroModel {
        // 2026-07-02 上游 ListAvailableModels 实测新增(标注 experimental preview),
        // modelId 本身就是 `claude-sonnet-5`(无 x.y 点号,与其余行不同)。
        advertised_id: "claude-sonnet-5",
        display_name: "Claude Sonnet 5",
        kiro_model: "claude-sonnet-5",
        identity_short: "Sonnet 5",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
        effort_levels: EFFORTS_WITH_XHIGH,
        default_effort: Some("high"),
        signature_id: SignatureId::Codename("claude-saffron"),
        structured_effort_only: false,
        rewrite_visible_thinking_directive: false,
        tokenizer: Tokenizer::Current,
        effort_schema: EffortSchema::OutputConfig,
        pro_only: false,
    },
    KiroModel {
        advertised_id: "claude-sonnet-4-6",
        display_name: "Claude Sonnet 4.6",
        kiro_model: "claude-sonnet-4.6",
        identity_short: "Sonnet 4.6",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
        effort_levels: EFFORTS_NO_XHIGH,
        default_effort: Some("high"),
        signature_id: SignatureId::Unsigned,
        structured_effort_only: false,
        rewrite_visible_thinking_directive: false,
        tokenizer: Tokenizer::Legacy,
        effort_schema: EffortSchema::OutputConfig,
        pro_only: false,
    },
    KiroModel {
        advertised_id: "claude-sonnet-4-5",
        display_name: "Claude Sonnet 4.5",
        kiro_model: "claude-sonnet-4.5",
        identity_short: "Sonnet 4.5",
        context_window: 200_000,
        supports_thinking: true,
        dated_alias: Some("claude-sonnet-4-5-20250929"),
        effort_levels: EFFORTS_NONE,
        default_effort: None,
        signature_id: SignatureId::Unsigned,
        structured_effort_only: false,
        rewrite_visible_thinking_directive: false,
        tokenizer: Tokenizer::Legacy,
        effort_schema: EffortSchema::OutputConfig,
        pro_only: false,
    },
    KiroModel {
        advertised_id: "claude-haiku-4-5",
        display_name: "Claude Haiku 4.5",
        kiro_model: "claude-haiku-4.5",
        identity_short: "Haiku 4.5",
        context_window: 200_000,
        supports_thinking: false,
        dated_alias: Some("claude-haiku-4-5-20251001"),
        effort_levels: EFFORTS_NONE,
        default_effort: None,
        signature_id: SignatureId::Unsigned,
        structured_effort_only: false,
        rewrite_visible_thinking_directive: false,
        tokenizer: Tokenizer::Legacy,
        effort_schema: EffortSchema::OutputConfig,
        pro_only: false,
    },
    KiroModel {
        // 2026-11-03 接入。上游 modelId `claude-haiku-5.5`(付费号目录 22 个模型之一),
        // rateMultiplier **0.06**(haiku-4.5 是 0.4)。schema:1M 入 / 64K 出,`output_config.effort`
        // 5 档、default=`medium`。
        // - **FREE 号目录里没有它**(11-03 实拉 FREE 号,8 个模型)→ `pro_only`,否则派给 FREE
        //   先吃一次 400 INVALID_MODEL_ID 再换号。
        // - 签名:11-03 真号直放实测(PRO MAX 号,1 条,metering 0.00055 积分)—— 有原生
        //   reasoningContentEvent + 签名,信封顶层 f1=4、无 f5/f6 代号,与两个 5.5 同属 v4 匿名信封
        //   → 历史推理照 opus-5.5 完整回传。(头字段 f1 是 19,两个 5.5 是 18,本地暂不据此区分。)
        // - `structured_effort_only` / `rewrite_visible_thinking_directive` 照 5.5 家族填,**未实测**:
        //   填 true 时多发的结构化字段对认旧标签的模型也无害,反过来漏发则强度静默失效。
        advertised_id: "claude-haiku-5-5",
        display_name: "Claude Haiku 5.5",
        kiro_model: "claude-haiku-5.5",
        identity_short: "Haiku 5.5",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
        effort_levels: EFFORTS_WITH_XHIGH,
        default_effort: Some("medium"),
        signature_id: SignatureId::Anonymous { envelope_version: 4 },
        structured_effort_only: true,
        rewrite_visible_thinking_directive: true,
        tokenizer: Tokenizer::Current,
        effort_schema: EffortSchema::OutputConfig,
        pro_only: true,
    },
    KiroModel {
        // 2026-10-23 接入。**本表第一个非 Anthropic 模型** —— 它存在的理由是池里有一批
        // BuilderId PRO MAX 号的上游目录一个 claude 都没有(9 个模型全是非 claude),
        // 在 caio 里可用模型数为 0,只在反复吃 400 INVALID_MODEL_ID。
        //
        // 字段全部抄自真号 `List-Available-Models` 回包,未标定的一律取保守值:
        // - `reasoning.effort` enum 只有 low/high/max,`default: "max"`(见 EFFORTS_KIMI_K3);
        // - effort 走 `reasoning` 路径而非 `output_config`(见 EffortSchema);
        // - 1,000,000 in / 131,072 out;`supportedInputTypes: [TEXT, IMAGE]`;
        // - `rateMultiplier` 2.4 Credit(上游计费,我方不参与换算);
        // - `promptCaching.supportsPromptCaching` 为 true,但两个 checkpoint 参数都是 null
        //   —— cache_point 对它的行为**未验证**。
        //
        // ⚠️ 上游 description 把它标为 "Kiro Excluded Model",含义未查证(可能涉及数据保留/
        // 赔偿条款与 Anthropic 系不同)。对外卖之前值得先看一眼 Kiro 文档。
        //
        // `signature_id: Unsigned` 是**保守选择而非实测结论**:没有证据表明它有原生签名
        // 推理帧。若判错,代价是多轮对话不回传历史 thinking(不报错、只降智),
        // 而判反了(误设 Codename/Anonymous)会让上游验签失败、整条历史被剥。
        advertised_id: "kimi-k3",
        display_name: "Kimi K3",
        kiro_model: "kimi-k3",
        identity_short: "Kimi K3",
        context_window: 1_000_000,
        supports_thinking: true,
        dated_alias: None,
        effort_levels: EFFORTS_KIMI_K3,
        default_effort: Some("max"),
        signature_id: SignatureId::Unsigned,
        // 旧文本标签 `<thinking_effort>` 是 Anthropic 系的东西,对它只会是噪声:
        // 必须始终发结构化字段,否则客户端设的强度一律不生效。
        structured_effort_only: true,
        // 无拒答证据,不猜(该开关是给上游 REASONING_EXTRACTION 判定用的,见 classifier_prompt)。
        rewrite_visible_thinking_directive: false,
        // ⚠️ 默认档,**不是标定值**。两档系数都是按 claude 标定的(见 text_tokens.rs),
        // kimi 这类大词表分词器在中文上大概率更低。
        //
        // 不要被「估算」二字误导:这个值经 chat.rs 的 tracker.output.tokens() 进
        // billing::finalize,**就是向客户计费的 output**(对抗评审 M3 纠正了本注释先前
        // 「只影响展示」的错误说法)。它不影响 Kiro 收我方的钱(那边按自己的 metering
        // 事件算),影响的是我方收客户的钱 —— 系数偏高即多收。
        //
        // 没有用「Legacy 的 ASCII 系数更保守」来换:在标定之前,用一个同样没依据的值
        // 替掉另一个没依据的值不叫改进。真号跑一次 contextUsage 差值标定(手法见
        // text_tokens.rs 2026-10-06 那段)再定。
        tokenizer: Tokenizer::Current,
        effort_schema: EffortSchema::Reasoning,
        pro_only: false,
    },
];

/// 把一个 effort 档位夹到该模型实际支持的档位上。
///
/// `requested = None` 表示「**没有可用的请求档位**」—— 调用方拿到的是脏值/未知值,
/// 此时应当落到该模型 schema 的 `default`,而不是先替它猜一个全局默认。
///
/// 返回 `None` = 该模型上游没有 effort schema → 调用方**不得**发
/// `additionalModelRequestFields`(与真客户端 `additionalModelRequestFields: undefined` 同形)。
///
/// 夹取规则对齐真客户端 `A7`(`extension.js:140071-140076`)与设置面板
/// (`:340057`):**在 enum 里就原样用**。不在 enum 时取**不超过请求档的最高可用档**,
/// 再没有才回落 schema 的 `default`(客户端在没有 `default` 时取 `levels[0]`,这里同)。
///
/// 「不超过请求档的最高可用档」这条是 2026-10-23 从「一律回落 default」改过来的
/// (对抗评审 M2)。旧规则在 **default 恰好是最高档**的模型上不单调:kimi-k3 的
/// enum 是 low/high/max、default 是 max,请求 `medium` 会换来 `max` —— 比 `high`
/// 还强,而它是 2.4 倍率加最慢档。这与本函数原本就写明的立场("宁可少一档,
/// 也不擅自升到 max 制造真客户端不会出现的形态")自相矛盾,所以改的是规则不是立场。
///
/// **对 Anthropic 系逐字节不变**:5 档全集的模型任何合法请求都精确命中;唯一缺档的
/// 4.6 系(缺 `xhigh`)请求 `xhigh` 时,新规则取 `high`,与旧规则回落 default(也是
/// `high`)同值。`clamp_rule_change_is_byte_identical_for_anthropic_models` 遍历
/// 权威表 × 全部合法档位钉死这一点。
pub fn clamp_effort_for_model(model: &str, requested: Option<&str>) -> Option<&'static str> {
    let m = lookup_model(model)?;
    if m.effort_levels.is_empty() {
        return None;
    }
    if let Some(req) = requested {
        if let Some(hit) = m.effort_levels.iter().find(|lv| lv.eq_ignore_ascii_case(req)) {
            return Some(*hit);
        }
        // 请求档不在本模型 enum 里:沿全局阶梯(VALID_EFFORTS,升序)往下找最高的可用档。
        // 脏值不会走到这里(调用前已由 normalize_effort 归一),故查不到 rank 即视为无诉求。
        if let Some(req_rank) = effort_rank(req) {
            if let Some(best) = m
                .effort_levels
                .iter()
                .filter(|lv| effort_rank(lv).is_some_and(|r| r <= req_rank))
                .max_by_key(|lv| effort_rank(lv).unwrap_or(0))
            {
                return Some(*best);
            }
        }
    }
    // 请求缺席,或请求档比本模型的最低档还低 → 回落**本模型** schema 的 default;
    // default 也不在表里(不该发生)时取最低档。
    m.default_effort
        .and_then(|d| m.effort_levels.iter().find(|lv| **lv == d).copied())
        .or_else(|| m.effort_levels.first().copied())
}

/// 全部模型行:内置权威表在前,热追加行(`kiro_extra_models`,见 [`super::extra_models`])在后。
/// 所有派生点都经它查表,热追加的行因此自动进入路由 / 公告 / 身份 / 档位 / 签名 / 分词器。
/// 热追加行与内置行的 id、kiro_model 都不撞名(写入时校验),所以「谁在前」不影响查找结果。
pub(crate) fn all_models() -> impl Iterator<Item = &'static KiroModel> {
    KIRO_MODELS.iter().chain(super::extra_models::extra_rows().iter())
}

/// 档位在全局阶梯 [`crate::anthropic_types::VALID_EFFORTS`](升序)里的名次。
/// 不在阶梯里返回 None(只可能来自手写的脏表,调用方按「无诉求」处理)。
fn effort_rank(level: &str) -> Option<usize> {
    crate::anthropic_types::VALID_EFFORTS
        .iter()
        .position(|v| v.eq_ignore_ascii_case(level))
}

/// 该模型是否只认结构化思考强度字段(见 [`KiroModel::structured_effort_only`])。
/// 未知模型返回 false(不改线缆形态)。
pub fn requires_structured_effort(model: &str) -> bool {
    lookup_model(model).is_some_and(|m| m.structured_effort_only)
}

/// 该模型是否只派给付费档(见 [`KiroModel::pro_only`])。未知模型返回 false(不额外限制,
/// opus 的 FREE 限制由调用方另判)。
pub fn denies_free(model: &str) -> bool {
    lookup_model(model).is_some_and(|m| m.pro_only)
}

/// 该请求最终是否路由到 **Anthropic 系**上游模型。
///
/// 等价于 `map_model(model)` 的结果以 `claude` 开头,但**不分配**:`map_model` 会产生
/// 三次分配(两次 `to_lowercase` + 一次 `to_string`),而本函数在调度热路径上被调用 ——
/// 一次选号要对全组求值,kiro 组有 1400+ 个号,且 `gw_core` 的准入谓词契约是锁内零分配
/// (`gw-core/src/account.rs` 的说明)。(对抗评审 L6)
///
/// 权威表命中走表(零分配);未命中才退回子串兜底,那条路本身就要 lowercase 一次。
pub fn routes_to_anthropic(model: &str) -> bool {
    if let Some(m) = resolve_base(model) {
        return m.kiro_model.starts_with("claude");
    }
    map_model_substring(&model.to_lowercase()).is_some_and(|m| m.starts_with("claude"))
}

/// 该模型的思考强度写在 `additionalModelRequestFields` 的哪条路径下,见 [`EffortSchema`]。
///
/// 未知模型回 [`EffortSchema::OutputConfig`]:能走到这里的未列名模型只可能是 claude 异名
/// (`map_model_substring` 的词根只有 sonnet/opus/haiku),给它们 Anthropic 系的路径是对的。
pub fn effort_schema_for(model: &str) -> EffortSchema {
    lookup_model(model).map_or(EffortSchema::OutputConfig, |m| m.effort_schema)
}

/// 该模型是否需要改写「正文写 `<thinking>`」类指令(见
/// [`KiroModel::rewrite_visible_thinking_directive`])。未知模型返回 false(不改报文)。
pub fn rewrites_visible_thinking_directive(model: &str) -> bool {
    lookup_model(model).is_some_and(|m| m.rewrite_visible_thinking_directive)
}

/// 静态表与上游实际目录的一处不一致。
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct EffortDrift {
    /// 上游的 `modelId`。
    pub model: String,
    /// 静态表里的档位(该模型不在表里时为 `None`)。
    pub table: Option<Vec<String>>,
    /// 上游当下的档位。
    pub upstream: Vec<String>,
    /// 静态表里的 default。
    pub table_default: Option<String>,
    /// 上游当下的 default。
    pub upstream_default: Option<String>,
}

/// 拿一份上游目录,逐条比对静态表,报出**会导致我们发错档位**的漂移。
///
/// 为什么需要它:热路径的档位来自本文件的静态表(编译期常量),而上游随时可能给某个模型
/// 增删档位。两个事实源之间没有自动同步 —— 这个函数不负责同步,只负责**让漂移可见**,
/// 免得下一次协议变动又变成线上发出上游不认的值才被发现。
///
/// 只报静态表**认识**的模型(`kiro_model` 能对上的),上游新增的模型不算漂移(那是"待接入",
/// 不是"发错值");上游**下线**了我们表里还有的模型也不报(那会体现为请求直接失败,不是静默错档)。
pub fn effort_drift(upstream: &[(String, Vec<String>, Option<String>)]) -> Vec<EffortDrift> {
    let mut out = Vec::new();
    for (model_id, levels, default) in upstream {
        let Some(m) = all_models().find(|m| m.kiro_model == model_id) else {
            continue; // 上游新增,静态表还没有 —— 不是漂移。
        };
        let table: Vec<String> = m.effort_levels.iter().map(|s| s.to_string()).collect();
        let table_default = m.default_effort.map(str::to_string);
        if &table != levels || &table_default != default {
            out.push(EffortDrift {
                model: model_id.clone(),
                table: Some(table),
                upstream: levels.clone(),
                table_default,
                upstream_default: default.clone(),
            });
        }
    }
    out
}

/// 按上游 kiro_model 查 thinking 签名的归属标识方式(`signature_id` 字段的门控读口)。
/// 不在权威表里的模型按 [`SignatureId::Unsigned`] 处理(宁可不回传,也不猜)。
pub fn signature_id_for(kiro_model: &str) -> SignatureId {
    all_models()
        .find(|m| m.kiro_model == kiro_model)
        .map_or(SignatureId::Unsigned, |m| m.signature_id)
}

/// 按对外名找权威表行:先精确归一([`resolve_base`]),再走子串兜底反查 `kiro_model`。
/// 兜底这一步让老客户端的异名(`openrouter/claude-opus-5-preview`)也能拿到正确档位表。
fn lookup_model(model: &str) -> Option<&'static KiroModel> {
    if let Some(m) = resolve_base(model) {
        return Some(m);
    }
    let mapped = map_model_substring(&model.to_lowercase())?;
    all_models().find(|m| m.kiro_model == mapped)
}

/// 去掉末尾的 `-YYYYMMDD` 日期段(末段恰好 8 位 ASCII 数字才剥;无 regex)。
/// 例:`claude-sonnet-4-5-20250929` → `claude-sonnet-4-5`;`claude-opus-4-8` 原样
/// (末段 "8" 非 8 位数字,不误剥)。
fn strip_date_suffix(id: &str) -> &str {
    match id.rfind('-') {
        Some(pos) => {
            let tail = &id[pos + 1..];
            if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
                &id[..pos]
            } else {
                id
            }
        }
        None => id,
    }
}

/// 对外模型名是否通过任一 Thinking 后缀显式请求思考。
pub(super) fn strip_thinking_suffix(model: &str) -> &str {
    for suffix in ["-reasoning", "-thinking"] {
        if model.len() >= suffix.len()
            && model[model.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
        {
            return &model[..model.len() - suffix.len()];
        }
    }
    model
}

pub fn has_thinking_suffix(model: &str) -> bool {
    strip_thinking_suffix(model).len() != model.len()
}

/// 把对外请求名(plain / `-thinking` / `-reasoning` / 日期变体)归一到权威表基础行。
/// 顺序:先剥 Thinking 后缀,再剥日期,最后对 `advertised_id` 精确匹配。
pub fn resolve_base(requested: &str) -> Option<&'static KiroModel> {
    let no_thinking = strip_thinking_suffix(requested);
    let lower = no_thinking.to_lowercase();
    let base = strip_date_suffix(&lower);
    all_models().find(|m| m.advertised_id == base)
}

/// 模型映射:对外名 → 上游 Kiro 模型 id。
///
/// 先走权威表([`resolve_base`]),未命中**保留子串兜底**(老客户端/未列名,如旧 opus-4.5
/// 异名仍可路由),都不中返回 None。
pub fn map_model(model: &str) -> Option<String> {
    if let Some(m) = resolve_base(model) {
        return Some(m.kiro_model.to_string());
    }
    map_model_substring(&model.to_lowercase())
}

/// 子串兜底映射(权威表未命中时;严格对照版本号)。
fn map_model_substring(model_lower: &str) -> Option<String> {
    if model_lower.contains("sonnet") {
        // 2026-05-29: Kiro 上游尚未支持 sonnet-4.8（INVALID_MODEL_ID），未来上线再加 4-8 分支
        if model_lower.contains("sonnet-5-5")
            || model_lower.contains("sonnet-5.5")
            || model_lower.contains("sonnet5.5")
            || model_lower.contains("sonnet5-5")
        {
            // 2026-10-03: 上游新增 claude-sonnet-5.5。**必须排在 sonnet-5 分支之前** —— 否则
            // `sonnet-5-5` 含 `sonnet-5` 邻接串,会被**静默降级**成 claude-sonnet-5
            // (客户以为在用 5.5,实际拿到 5,且不报错)。与 opus-5.5 同一个坑。
            Some("claude-sonnet-5.5".to_string())
        } else if model_lower.contains("4-6") || model_lower.contains("4.6") {
            Some("claude-sonnet-4.6".to_string())
        } else if model_lower.contains("4-5") || model_lower.contains("4.5") {
            Some("claude-sonnet-4.5".to_string())
        } else if model_lower.contains("sonnet-5") || model_lower.contains("sonnet5") {
            // 2026-07-02: 上游新增 claude-sonnet-5（无 x.y 点号,权威表已列;此兜底覆盖异名写法,
            // 如带前缀/后缀的未列名 `openrouter/claude-sonnet-5-preview`)。
            // ⚠️必须锚定 `sonnet-5` 邻接串,不能用裸 `contains('5')`——否则历史名
            // `claude-3-5-sonnet`(Claude 3.5 Sonnet,sonnet 在尾部)会被误吞成 sonnet-5。
            Some("claude-sonnet-5".to_string())
        } else {
            None
        }
    } else if model_lower.contains("opus") {
        if model_lower.contains("opus-5-5")
            || model_lower.contains("opus-5.5")
            || model_lower.contains("opus5.5")
            || model_lower.contains("opus5-5")
        {
            // 2026-09-26: 上游新增 claude-opus-5.5。**必须排在 opus-5 分支之前** —— 此前
            // `opus-5-5` 含 `opus-5` 邻接串,会被下一分支**静默降级**成 claude-opus-5
            // (客户以为在用 5.5,实际拿到 5,且不报错)。
            Some("claude-opus-5.5".to_string())
        } else if model_lower.contains("opus-5") || model_lower.contains("opus5") {
            // 2026-07-25: 上游新增 claude-opus-5(无 x.y 点号,权威表已列;此兜底覆盖
            // 异名写法,如 `openrouter/claude-opus-5-preview`)。
            // ⚠️必须锚定 `opus-5` 邻接串,不能用裸 contains("5")——否则未来的
            // `claude-opus-4-5`/`4.5` 会被误吞(它们在下面各自分支处理)。
            // 本分支置于最前:opus-5 的写法不含 4-x,与下方分支互斥。
            Some("claude-opus-5".to_string())
        } else if model_lower.contains("4-8") || model_lower.contains("4.8") {
            Some("claude-opus-4.8".to_string())
        } else if model_lower.contains("4-5") || model_lower.contains("4.5") {
            Some("claude-opus-4.5".to_string())
        } else if model_lower.contains("4-6") || model_lower.contains("4.6") {
            Some("claude-opus-4.6".to_string())
        } else if model_lower.contains("4-7") || model_lower.contains("4.7") {
            Some("claude-opus-4.7".to_string())
        } else {
            None
        }
    } else if model_lower.contains("haiku") {
        if model_lower.contains("haiku-5-5")
            || model_lower.contains("haiku-5.5")
            || model_lower.contains("haiku5.5")
            || model_lower.contains("haiku5-5")
        {
            // 2026-11-03: 上游新增 claude-haiku-5.5。此前 haiku 一律兜底到 4.5,带前缀的异名
            // (`openrouter/claude-haiku-5-5`)会被**静默降级**,与 opus/sonnet-5.5 同一个坑。
            Some("claude-haiku-5.5".to_string())
        } else {
            // 其余 haiku 写法(含 4.8 等上游没有的版本)仍兜底到 4.5。
            Some("claude-haiku-4.5".to_string())
        }
    } else {
        None
    }
}

/// 根据模型名返回上下文窗口大小。权威表优先,兜底子串(1M 仅 opus-5.5/opus-5/sonnet-5.5/sonnet-5/sonnet-4.6/opus-4.6/4.7/4.8)。
pub fn get_context_window_size(model: &str) -> i32 {
    if let Some(m) = resolve_base(model) {
        return m.context_window;
    }
    match map_model_substring(&model.to_lowercase()) {
        Some(mapped)
            if mapped == "claude-opus-5.5"
                || mapped == "claude-haiku-5.5"
                || mapped == "claude-opus-5"
                || mapped == "claude-sonnet-5.5"
                || mapped == "claude-sonnet-5"
                || mapped == "claude-sonnet-4.6"
                || mapped == "claude-opus-4.6"
                || mapped == "claude-opus-4.7"
                || mapped == "claude-opus-4.8" =>
        {
            1_000_000
        }
        _ => 200_000,
    }
}

/// 按**上游** Kiro 模型 id(如 `claude-opus-4.6`)查分词器代际;表外模型按新分词器算。
pub fn tokenizer_for_kiro_model(kiro_model: &str) -> Tokenizer {
    all_models()
        .find(|m| m.kiro_model == kiro_model)
        .map_or(Tokenizer::Current, |m| m.tokenizer)
}

/// 对外公告的一个模型条目(由权威表展开:基础版 + 两种 Thinking 后缀 + 日期变体)。
pub struct AdvertisedModel {
    pub id: String,
    pub display_name: String,
    pub supports_thinking: bool,
}

/// 从权威表生成 `/v1/models` 完整对外目录。每个基础模型展开:
/// 每个名字生成基础版、`-thinking`、`-reasoning`;有 dated_alias 时再展开日期名。
pub fn advertised_models() -> Vec<AdvertisedModel> {
    let mut out = Vec::with_capacity(KIRO_MODELS.len() * 6);
    for m in all_models() {
        out.push(AdvertisedModel {
            id: m.advertised_id.to_string(),
            display_name: m.display_name.to_string(),
            supports_thinking: m.supports_thinking,
        });
        for suffix in ["-thinking", "-reasoning"] {
            out.push(AdvertisedModel {
                id: format!("{}{suffix}", m.advertised_id),
                display_name: format!("{} ({})", m.display_name, &suffix[1..]),
                supports_thinking: true,
            });
        }
        if let Some(dated) = m.dated_alias {
            let date_label = dated_label(dated);
            out.push(AdvertisedModel {
                id: dated.to_string(),
                display_name: format!("{}{date_label}", m.display_name),
                supports_thinking: m.supports_thinking,
            });
            for suffix in ["-thinking", "-reasoning"] {
                out.push(AdvertisedModel {
                    id: format!("{dated}{suffix}"),
                    display_name: format!("{} ({}){date_label}", m.display_name, &suffix[1..]),
                    supports_thinking: true,
                });
            }
        }
    }
    out
}

/// 从日期别名末段 `-YYYYMMDD` 生成展示后缀 ` (YYYY-MM-DD)`;无法解析则空串。
fn dated_label(dated: &str) -> String {
    match dated.rfind('-') {
        Some(pos) => {
            let tail = &dated[pos + 1..];
            if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
                format!(" ({}-{}-{})", &tail[0..4], &tail[4..6], &tail[6..8])
            } else {
                String::new()
            }
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 签名标识表回归(codex 审查补):实测有原生签名推理的模型必须各有正确标识方式,
    /// 其余模型必须 Unsigned(门控静默丢弃,不上传 reasoningContent)。
    /// 代号来自 2026-08-19 线上探针(kiro_think_probe4/5/6),**不要凭印象改**。
    #[test]
    fn signature_id_table_matches_probe_evidence() {
        use SignatureId::*;
        let expect: &[(&str, SignatureId)] = &[
            // 2026-09-26 实测:v4 信封,无 f5/f6。
            ("claude-opus-5.5", Anonymous { envelope_version: 4 }),
            // 2026-10-03 实测:同为 v4 信封,明文头与 opus-5.5 逐字段相同(f1=18/f2=1/f3=2/f7=1/f8)。
            ("claude-sonnet-5.5", Anonymous { envelope_version: 4 }),
            ("claude-opus-5", Codename("claude-honey")),
            ("claude-opus-4.8", Codename("claude-quince")),
            ("claude-opus-4.7", Codename("claude-opus-4-7")), // 代号=官方名,实测如此
            ("claude-sonnet-5", Codename("claude-saffron")),
            ("claude-opus-4.6", Unsigned),
            ("claude-opus-4.5", Unsigned),
            ("claude-sonnet-4.6", Unsigned),
            ("claude-sonnet-4.5", Unsigned),
            ("claude-haiku-4.5", Unsigned),
            // 2026-11-03 实测:v4 信封,无 f5/f6(头 f1=19,两个 5.5 是 18)。
            ("claude-haiku-5.5", Anonymous { envelope_version: 4 }),
            ("some-future-model", Unsigned),
        ];
        for (kiro_model, want) in expect {
            assert_eq!(
                signature_id_for(kiro_model),
                *want,
                "{kiro_model} 的签名标识方式与探针实测不符"
            );
        }
    }

    /// 不变量:每个**公告**对外名都必须能 `map_model`(否则公告了 chat 拿不到)且有正窗口。
    #[test]
    fn every_advertised_model_maps() {
        for am in advertised_models() {
            assert!(
                map_model(&am.id).is_some(),
                "公告模型 {} 未能 map_model → /v1/models 列了但 chat 会 None",
                am.id
            );
            assert!(get_context_window_size(&am.id) >= 200_000, "{} 窗口异常", am.id);
        }
    }

    #[test]
    fn advertised_has_no_duplicate_ids() {
        let mut ids: Vec<String> = advertised_models().into_iter().map(|m| m.id).collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(before, ids.len(), "advertised_models 有重复 id");
    }

    #[test]
    fn advertised_includes_thinking_reasoning_and_dated() {
        // 热追加是进程全局状态:与改它的测试串行,条数才对得上。
        let _g = super::super::extra_models::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ids: Vec<String> = advertised_models().into_iter().map(|m| m.id).collect();
        assert!(ids.contains(&"claude-opus-4-8".to_string()));
        assert!(ids.contains(&"claude-opus-4-8-thinking".to_string()));
        assert!(ids.contains(&"claude-opus-4-8-reasoning".to_string()));
        assert!(ids.contains(&"claude-sonnet-4-5-20250929".to_string()));
        assert!(ids.contains(&"claude-sonnet-4-5-20250929-thinking".to_string()));
        assert!(ids.contains(&"claude-haiku-4-5-20251001".to_string()));
        // 展开数从表派生(有 dated_alias=6 形态,否则 3),而非硬钉常量——
        // 加/减模型或日期别名时测试仍校验"展开逻辑"而非快照(审查 Minimalist#3)。
        let expected: usize = all_models()
            .map(|m| if m.dated_alias.is_some() { 6 } else { 3 })
            .sum();
        assert_eq!(ids.len(), expected);
    }

    #[test]
    fn map_model_resolves_thinking_reasoning_and_dated() {
        assert_eq!(map_model("claude-opus-4-8-thinking").as_deref(), Some("claude-opus-4.8"));
        assert_eq!(map_model("claude-opus-4-8-reasoning").as_deref(), Some("claude-opus-4.8"));
        assert_eq!(
            map_model("claude-opus-4-5-20251101-thinking").as_deref(),
            Some("claude-opus-4.5")
        );
        assert_eq!(
            map_model("claude-sonnet-4-5-20250929").as_deref(),
            Some("claude-sonnet-4.5")
        );
    }

    // ───────── kimi-k3(2026-10-23 接入) ─────────

    /// 对外名两种写法都要路由到上游 `kimi-k3`,且进得了公告目录。
    #[test]
    fn kimi_k3_routes_and_is_advertised() {
        assert_eq!(map_model("kimi-k3").as_deref(), Some("kimi-k3"));
        assert_eq!(map_model("kimi-k3-thinking").as_deref(), Some("kimi-k3"));
        let ids: Vec<String> = advertised_models().into_iter().map(|m| m.id).collect();
        assert!(ids.iter().any(|i| i == "kimi-k3"), "公告目录应含 kimi-k3");
        assert!(ids.iter().any(|i| i == "kimi-k3-thinking"));
        assert_eq!(get_context_window_size("kimi-k3"), 1_000_000);
    }

    /// 上游 schema 的 `reasoning.effort` enum **只有 low/high/max**,default 是 `max`。
    /// 发一个它 enum 里没有的档位既可能 400,也是可被规则化的形态差异 —— 这条是那道铁律的回归。
    #[test]
    fn kimi_k3_effort_clamps_to_its_own_enum() {
        for lv in ["low", "high", "max"] {
            assert_eq!(clamp_effort_for_model("kimi-k3", Some(lv)), Some(lv), "{lv} 应原样");
        }
        // medium / xhigh 不在它的 enum 里 → 取**不超过请求档的最高可用档**,
        // 绝不原样发出去,也绝不夹到更强的档(单调性见 kimi_k3_effort_clamp_is_monotonic)。
        assert_eq!(clamp_effort_for_model("kimi-k3", Some("medium")), Some("low"));
        assert_eq!(clamp_effort_for_model("kimi-k3", Some("xhigh")), Some("high"));
        assert_eq!(clamp_effort_for_model("kimi-k3", None), Some("max"));
    }

    /// **对抗评审 M2**:夹取必须单调 —— 请求一个更弱的档位,绝不能换来一个更强的。
    /// 旧规则「不在 enum 就回落 default」在 kimi 上会把 `medium` 变成 `max`:
    /// 比 `high` 还高,而 kimi 是 2.4 倍率 + 最慢档。
    #[test]
    fn kimi_k3_effort_clamp_is_monotonic() {
        let rank = |lv: &str| {
            crate::anthropic_types::VALID_EFFORTS.iter().position(|v| *v == lv).unwrap()
        };
        let mut prev = 0usize;
        for req in crate::anthropic_types::VALID_EFFORTS {
            let got = clamp_effort_for_model("kimi-k3", Some(req)).unwrap();
            assert!(
                rank(got) <= rank(req),
                "请求 {req} 却夹到更强的 {got}"
            );
            assert!(rank(got) >= prev, "{req} 夹取结果相对上一档回退了:{got}");
            prev = rank(got);
        }
        // 具体取值:缺档时取「不超过请求档的最高可用档」。
        assert_eq!(clamp_effort_for_model("kimi-k3", Some("medium")), Some("low"));
        assert_eq!(clamp_effort_for_model("kimi-k3", Some("xhigh")), Some("high"));
        // 请求缺席仍回落 schema 的 default。
        assert_eq!(clamp_effort_for_model("kimi-k3", None), Some("max"));
    }

    /// 与上一条配对:新夹取规则对**所有** claude 条目逐字节不变。
    /// 唯一缺档的是 4.6 系缺 `xhigh`,新旧规则都落到 `high`。
    #[test]
    fn clamp_rule_change_is_byte_identical_for_anthropic_models() {
        for m in KIRO_MODELS {
            if m.advertised_id == "kimi-k3" {
                continue;
            }
            for req in crate::anthropic_types::VALID_EFFORTS {
                let got = clamp_effort_for_model(m.advertised_id, Some(req));
                let want = if m.effort_levels.is_empty() {
                    None
                } else if m.effort_levels.contains(req) {
                    Some(*req)
                } else {
                    // 旧规则:不在 enum → 回落 default。
                    m.default_effort
                        .and_then(|d| m.effort_levels.iter().find(|lv| **lv == d).copied())
                        .or_else(|| m.effort_levels.first().copied())
                };
                assert_eq!(got, want, "{} 请求 {req} 的夹取结果变了", m.advertised_id);
            }
        }
    }

    /// kimi-k3 只认结构化字段(旧文本标签是 Anthropic 系的东西)。
    #[test]
    fn kimi_k3_requires_structured_effort() {
        assert!(requires_structured_effort("kimi-k3"));
        assert!(requires_structured_effort("kimi-k3-thinking"));
    }

    /// 非 claude 模型没有原生签名推理的证据 → Unsigned,历史 thinking 不上传。
    ///
    /// 断言打在**权威表那一行**上而不是 `signature_id_for`:后者对表外模型的回落值
    /// 也是 Unsigned,所以删掉 kimi-k3 整行它照样能过,测不到东西。(对抗评审 L5)
    #[test]
    fn kimi_k3_thinking_is_unsigned() {
        let row = resolve_base("kimi-k3").expect("kimi-k3 必须在权威表里");
        assert_eq!(row.signature_id, SignatureId::Unsigned);
        assert_eq!(signature_id_for("kimi-k3-thinking"), SignatureId::Unsigned);
    }

    /// 钉死「谁走哪条 schema 路径」:11 个既有条目全是 OutputConfig,只有 kimi-k3 是
    /// Reasoning。发错路径 = 思考强度静默失效(不报错,客户端以为在 max 档)。
    ///
    /// (这不是「防漏填」—— 结构体字面量漏字段本来就是编译错误。它防的是**填错值**:
    /// 以后加 Anthropic 系模型时顺手抄成 Reasoning,编译照过、线上静默失效。)
    #[test]
    fn only_kimi_k3_uses_the_reasoning_effort_schema() {
        for m in KIRO_MODELS {
            let want = if m.advertised_id == "kimi-k3" {
                EffortSchema::Reasoning
            } else {
                EffortSchema::OutputConfig
            };
            assert_eq!(m.effort_schema, want, "{} 的 effort schema 路径错了", m.advertised_id);
        }
    }

    #[test]
    fn map_model_substring_fallback_still_works() {
        // 未列于权威表的异名(老客户端)仍走子串兜底。
        assert_eq!(map_model("anthropic/claude-3.5-sonnet-4.6").as_deref(), Some("claude-sonnet-4.6"));
        assert_eq!(map_model("claude-opus-4.7-beta").as_deref(), Some("claude-opus-4.7"));
        assert!(map_model("gpt-4o").is_none());
    }

    #[test]
    fn opus_5_resolves_via_table_and_fallback() {
        // 权威表精确匹配(plain/-thinking)。
        assert_eq!(map_model("claude-opus-5").as_deref(), Some("claude-opus-5"));
        assert_eq!(map_model("claude-opus-5-thinking").as_deref(), Some("claude-opus-5"));
        assert_eq!(resolve_base("claude-opus-5").map(|m| m.identity_short), Some("Opus 5"));
        assert_eq!(get_context_window_size("claude-opus-5"), 1_000_000);
        // 子串兜底(未列全名的异名)也给 1M 窗口。
        assert_eq!(map_model("openrouter/claude-opus-5-preview").as_deref(), Some("claude-opus-5"));
        assert_eq!(get_context_window_size("openrouter/claude-opus-5-preview"), 1_000_000);
        // 关键边界:opus-5 分支置于最前,但绝不能吞掉 4.x 各版本。
        assert_eq!(map_model("claude-opus-4-8").as_deref(), Some("claude-opus-4.8"));
        assert_eq!(map_model("claude-opus-4-7").as_deref(), Some("claude-opus-4.7"));
        assert_eq!(map_model("claude-opus-4-6").as_deref(), Some("claude-opus-4.6"));
        assert_eq!(map_model("claude-opus-4-5").as_deref(), Some("claude-opus-4.5"));
        assert_eq!(map_model("claude-opus-4.5").as_deref(), Some("claude-opus-4.5"));
        // 日期变体仍归到 4-5 基础行,不被 opus-5 误吞。
        assert_eq!(map_model("claude-opus-4-5-20251101").as_deref(), Some("claude-opus-4.5"));
    }

    #[test]
    fn opus_5_5_resolves_and_is_not_downgraded_to_opus_5() {
        // 权威表精确匹配(plain/-thinking)→ 上游点号 id。
        assert_eq!(map_model("claude-opus-5-5").as_deref(), Some("claude-opus-5.5"));
        assert_eq!(map_model("claude-opus-5-5-thinking").as_deref(), Some("claude-opus-5.5"));
        assert_eq!(resolve_base("claude-opus-5-5").map(|m| m.identity_short), Some("Opus 5.5"));
        assert_eq!(get_context_window_size("claude-opus-5-5"), 1_000_000);
        // 回归(2026-09-23 实测坑):各种 5.5 写法都不能被 opus-5 分支吞成 claude-opus-5。
        for alias in [
            "claude-opus-5.5",
            "Claude-Opus-5.5-thinking",
            "openrouter/claude-opus-5-5-preview",
            "anthropic/claude-opus5.5",
            "claude-opus5-5",
        ] {
            assert_eq!(map_model(alias).as_deref(), Some("claude-opus-5.5"), "{alias}");
            assert_eq!(get_context_window_size(alias), 1_000_000, "{alias}");
        }
        // opus-5 本身不受影响。
        assert_eq!(map_model("claude-opus-5").as_deref(), Some("claude-opus-5"));
        assert_eq!(map_model("openrouter/claude-opus-5-preview").as_deref(), Some("claude-opus-5"));
        // 档位表与上游 schema 一致:5 档全集,default=medium(opus-5 是 high)。
        assert_eq!(clamp_effort_for_model("claude-opus-5-5", None), Some("medium"));
        assert_eq!(clamp_effort_for_model("claude-opus-5-5", Some("xhigh")), Some("xhigh"));
        assert_eq!(clamp_effort_for_model("claude-opus-5-5", Some("max")), Some("max"));
        // 公告目录里有它。
        let ids: Vec<String> = advertised_models().into_iter().map(|m| m.id).collect();
        assert!(ids.contains(&"claude-opus-5-5".to_string()));
        assert!(ids.contains(&"claude-opus-5-5-thinking".to_string()));
    }

    #[test]
    fn sonnet_5_5_resolves_and_is_not_downgraded_to_sonnet_5() {
        // 权威表精确匹配(plain/-thinking)→ 上游点号 id。
        assert_eq!(map_model("claude-sonnet-5-5").as_deref(), Some("claude-sonnet-5.5"));
        assert_eq!(map_model("claude-sonnet-5-5-thinking").as_deref(), Some("claude-sonnet-5.5"));
        assert_eq!(resolve_base("claude-sonnet-5-5").map(|m| m.identity_short), Some("Sonnet 5.5"));
        assert_eq!(get_context_window_size("claude-sonnet-5-5"), 1_000_000);
        // 回归(2026-10-03 上线前发现):各种 5.5 写法都不能被 sonnet-5 分支吞成 claude-sonnet-5。
        for alias in [
            "claude-sonnet-5.5",
            "Claude-Sonnet-5.5-thinking",
            "openrouter/claude-sonnet-5-5-preview",
            "anthropic/claude-sonnet5.5",
            "claude-sonnet5-5",
        ] {
            assert_eq!(map_model(alias).as_deref(), Some("claude-sonnet-5.5"), "{alias}");
            assert_eq!(get_context_window_size(alias), 1_000_000, "{alias}");
        }
        // sonnet-5 与 4.x 不受影响;历史名 claude-3-5-sonnet 仍不被任何 5.x 分支误吞。
        assert_eq!(map_model("claude-sonnet-5").as_deref(), Some("claude-sonnet-5"));
        assert_eq!(map_model("openrouter/claude-sonnet-5-preview").as_deref(), Some("claude-sonnet-5"));
        assert_eq!(map_model("claude-sonnet-4-5").as_deref(), Some("claude-sonnet-4.5"));
        assert_eq!(map_model("claude-sonnet-4-5-20250929").as_deref(), Some("claude-sonnet-4.5"));
        assert_eq!(map_model("claude-sonnet-4-6").as_deref(), Some("claude-sonnet-4.6"));
        assert_eq!(map_model("claude-3-5-sonnet").as_deref(), None);
        // 档位表与上游 schema 一致:5 档全集,default=high。
        assert_eq!(clamp_effort_for_model("claude-sonnet-5-5", None), Some("high"));
        assert_eq!(clamp_effort_for_model("claude-sonnet-5-5", Some("xhigh")), Some("xhigh"));
        assert_eq!(clamp_effort_for_model("claude-sonnet-5-5", Some("max")), Some("max"));
        // 公告目录里有它。
        let ids: Vec<String> = advertised_models().into_iter().map(|m| m.id).collect();
        assert!(ids.contains(&"claude-sonnet-5-5".to_string()));
        assert!(ids.contains(&"claude-sonnet-5-5-thinking".to_string()));
    }

    #[test]
    fn haiku_5_5_resolves_and_is_not_downgraded_to_haiku_4_5() {
        assert_eq!(map_model("claude-haiku-5-5").as_deref(), Some("claude-haiku-5.5"));
        assert_eq!(map_model("claude-haiku-5-5-thinking").as_deref(), Some("claude-haiku-5.5"));
        assert_eq!(resolve_base("claude-haiku-5-5").map(|m| m.identity_short), Some("Haiku 5.5"));
        // 回归(2026-11-03 上线前):此前 haiku 一律兜底到 4.5,5.5 被静默降级。
        for alias in [
            "claude-haiku-5.5",
            "Claude-Haiku-5.5-thinking",
            "openrouter/claude-haiku-5-5-preview",
            "anthropic/claude-haiku5.5",
            "claude-haiku5-5",
        ] {
            assert_eq!(map_model(alias).as_deref(), Some("claude-haiku-5.5"), "{alias}");
            assert_eq!(get_context_window_size(alias), 1_000_000, "{alias}");
            assert!(denies_free(alias), "{alias} 应只给付费档");
        }
        // haiku-4.5 及其它 haiku 写法不受影响。
        assert_eq!(map_model("claude-haiku-4-5").as_deref(), Some("claude-haiku-4.5"));
        assert_eq!(map_model("claude-haiku-4-5-20251001").as_deref(), Some("claude-haiku-4.5"));
        assert_eq!(map_model("claude-3-5-haiku").as_deref(), Some("claude-haiku-4.5"));
        assert_eq!(map_model("claude-haiku-4-8").as_deref(), Some("claude-haiku-4.5"));
        assert_eq!(get_context_window_size("claude-haiku-4-5"), 200_000);
        assert!(!denies_free("claude-haiku-4-5"));
        // 档位表与上游 schema 一致:5 档全集,default=medium;只认结构化字段。
        assert_eq!(clamp_effort_for_model("claude-haiku-5-5", None), Some("medium"));
        assert_eq!(clamp_effort_for_model("claude-haiku-5-5", Some("max")), Some("max"));
        assert!(requires_structured_effort("claude-haiku-5-5"));
        // 签名:v4 匿名信封(11-03 实测)→ 历史推理可回传。
        assert_eq!(signature_id_for("claude-haiku-5.5"), SignatureId::Anonymous { envelope_version: 4 });
        let ids: Vec<String> = advertised_models().into_iter().map(|m| m.id).collect();
        assert!(ids.contains(&"claude-haiku-5-5".to_string()));
        assert!(ids.contains(&"claude-haiku-5-5-thinking".to_string()));
    }

    /// 只有标了 `pro_only` 的行才拒 FREE;其余 claude 行(含 opus,它走 lib.rs 的子串规则)一律 false,
    /// 保证加字段对存量模型逐字节不变。
    #[test]
    fn only_haiku_5_5_is_pro_only_in_static_table() {
        for m in KIRO_MODELS {
            assert_eq!(m.pro_only, m.advertised_id == "claude-haiku-5-5", "{} 的 pro_only", m.advertised_id);
        }
    }

    #[test]
    fn sonnet_5_resolves_via_table_and_fallback() {
        // 权威表精确匹配(plain/-thinking)。
        assert_eq!(map_model("claude-sonnet-5").as_deref(), Some("claude-sonnet-5"));
        assert_eq!(map_model("claude-sonnet-5-thinking").as_deref(), Some("claude-sonnet-5"));
        assert_eq!(resolve_base("claude-sonnet-5").map(|m| m.identity_short), Some("Sonnet 5"));
        // 子串兜底(未列全名的异名),且不误吞 sonnet-4.5/4.6。
        assert_eq!(map_model("openrouter/claude-sonnet-5-preview").as_deref(), Some("claude-sonnet-5"));
        assert_eq!(map_model("claude-sonnet-4-5").as_deref(), Some("claude-sonnet-4.5"));
        assert_eq!(map_model("claude-sonnet-4-6").as_deref(), Some("claude-sonnet-4.6"));
        // 历史名 claude-3-5-sonnet(Claude 3.5 Sonnet)含 sonnet+散落的 5,绝不能被误吞成 sonnet-5。
        // 它不含 "4-5"/"4.5"/"sonnet-5",落到子串兜底的 None(该老模型 Kiro 本就不支持,冤案排除)。
        assert_eq!(map_model("claude-3-5-sonnet").as_deref(), None);
        assert_eq!(map_model("claude-3-5-sonnet-20241022").as_deref(), None);
    }

    #[test]
    fn context_window_from_table_and_fallback() {
        assert_eq!(get_context_window_size("claude-opus-4-8"), 1_000_000);
        assert_eq!(get_context_window_size("claude-opus-4-8-thinking"), 1_000_000);
        assert_eq!(get_context_window_size("claude-sonnet-4-5-20250929"), 200_000);
        assert_eq!(get_context_window_size("claude-haiku-4-5"), 200_000);
        assert_eq!(get_context_window_size("claude-sonnet-5"), 1_000_000);
        assert_eq!(get_context_window_size("openrouter/claude-sonnet-5-preview"), 1_000_000);
    }

    /// 分词器代际按上游 id 查(2026-10-06 实测:opus-4.6 旧,opus-4.8 / 5 / 5.5 系新);表外按新。
    #[test]
    fn tokenizer_by_kiro_model() {
        for id in ["claude-opus-5.5", "claude-sonnet-5.5", "claude-opus-5", "claude-sonnet-5", "claude-opus-4.8"] {
            assert_eq!(tokenizer_for_kiro_model(id), Tokenizer::Current, "{id}");
        }
        for id in ["claude-opus-4.6", "claude-sonnet-4.5", "claude-haiku-4.5"] {
            assert_eq!(tokenizer_for_kiro_model(id), Tokenizer::Legacy, "{id}");
        }
        assert_eq!(tokenizer_for_kiro_model("claude-opus-6"), Tokenizer::Current);
        // 对外名(连字符)不是上游 id,查不到 → 按新算;调用方必须传上游 id。
        assert_eq!(tokenizer_for_kiro_model("claude-opus-4-6"), Tokenizer::Current);
    }

    /// 权威表自洽:声明了档位就必须声明 default,且 default 必须真在档位里;
    /// 没档位的行不许留 default。防止手改表时两列脱节。
    #[test]
    fn effort_table_is_self_consistent() {
        for m in KIRO_MODELS {
            if m.effort_levels.is_empty() {
                assert!(
                    m.default_effort.is_none(),
                    "{} 无档位表却留了 default_effort",
                    m.advertised_id
                );
                continue;
            }
            let d = m.default_effort.unwrap_or_else(|| {
                panic!("{} 有档位表但缺 default_effort", m.advertised_id)
            });
            assert!(
                m.effort_levels.contains(&d),
                "{} 的 default_effort={d} 不在它自己的档位表 {:?} 里",
                m.advertised_id,
                m.effort_levels
            );
            // 每个档位都必须是全集里的合法值,否则会发出上游不认的串。
            for lv in m.effort_levels {
                assert!(
                    crate::anthropic_types::VALID_EFFORTS.contains(lv),
                    "{} 的档位 {lv} 不在 VALID_EFFORTS 全集里",
                    m.advertised_id
                );
            }
        }
    }

    #[test]
    fn clamp_effort_matches_upstream_schema() {
        // 支持的档位原样返回。
        assert_eq!(clamp_effort_for_model("claude-opus-5", Some("max")), Some("max"));
        assert_eq!(clamp_effort_for_model("claude-opus-5", Some("xhigh")), Some("xhigh"));
        // 4.6 系没有 xhigh → 回落到它 schema 的 default(high),而不是硬发 xhigh。
        assert_eq!(clamp_effort_for_model("claude-opus-4-6", Some("xhigh")), Some("high"));
        assert_eq!(clamp_effort_for_model("claude-sonnet-4-6", Some("xhigh")), Some("high"));
        // 但 4.6 系**有** max,顶格请求不该被连累。
        assert_eq!(clamp_effort_for_model("claude-sonnet-4-6", Some("max")), Some("max"));
        // 4.7 是全表唯一 default=xhigh 的模型。
        assert_eq!(clamp_effort_for_model("claude-opus-4-7", None), Some("xhigh"));
        // 无 schema 的模型 → None(调用方据此完全不发该字段)。
        assert_eq!(clamp_effort_for_model("claude-opus-4-5", Some("high")), None);
        assert_eq!(clamp_effort_for_model("claude-haiku-4-5", Some("low")), None);
        // 未列名的异名走子串兜底,仍能拿到正确的档位表。
        assert_eq!(
            clamp_effort_for_model("openrouter/claude-opus-5-preview", Some("max")),
            Some("max")
        );
        // 完全未知的模型 → None,不猜。
        assert_eq!(clamp_effort_for_model("gpt-4o", Some("high")), None);
        // 大小写不敏感(上游只认小写形态,这里归一到表里的静态串)。
        assert_eq!(clamp_effort_for_model("claude-opus-5", Some("MAX")), Some("max"));
    }

    /// `requested = None`(调用方拿到脏值、没有可用诉求)必须落到**该模型自己的** default,
    /// 而不是某个全局默认。审查 Architect#6:此前脏值先被归一成全局 `xhigh`,
    /// 于是支持 xhigh 的模型(opus-5/4.8)会照发 xhigh,而客户端 `A7` 的行为是回落 `high`。
    #[test]
    fn absent_request_falls_back_to_each_models_own_default() {
        assert_eq!(clamp_effort_for_model("claude-opus-5", None), Some("high"));
        assert_eq!(clamp_effort_for_model("claude-opus-4-8", None), Some("high"));
        assert_eq!(clamp_effort_for_model("claude-sonnet-4-6", None), Some("high"));
        // 4.7 的 schema default 就是 xhigh —— 同一段代码对它给出不同答案,才说明是按模型取的。
        assert_eq!(clamp_effort_for_model("claude-opus-4-7", None), Some("xhigh"));
        // 无 schema 的模型仍然是"什么都不发",而不是回落到某个档位。
        assert_eq!(clamp_effort_for_model("claude-haiku-4-5", None), None);
    }

    #[test]
    fn effort_drift_flags_only_actionable_mismatches() {
        // 与静态表一致 → 无漂移。
        let same = vec![(
            "claude-opus-5".to_string(),
            vec!["low".into(), "medium".into(), "high".into(), "xhigh".into(), "max".into()],
            Some("high".to_string()),
        )];
        assert!(effort_drift(&same).is_empty(), "一致时不该报漂移");

        // 上游删掉了 max → 必须报(否则我们会继续发一个它不认的值)。
        let removed = vec![(
            "claude-opus-5".to_string(),
            vec!["low".into(), "medium".into(), "high".into(), "xhigh".into()],
            Some("high".to_string()),
        )];
        let d = effort_drift(&removed);
        assert_eq!(d.len(), 1, "上游删档位必须报出来");
        assert_eq!(d[0].model, "claude-opus-5");

        // default 变了也算漂移(会改变回落目标)。
        let new_default = vec![(
            "claude-opus-5".to_string(),
            vec!["low".into(), "medium".into(), "high".into(), "xhigh".into(), "max".into()],
            Some("xhigh".to_string()),
        )];
        assert_eq!(effort_drift(&new_default).len(), 1, "default 变化也要报");

        // 上游新增的模型静态表里没有 → **不是**漂移(是"待接入",不会发错值)。
        let unknown = vec![("brand-new-model".to_string(), vec!["low".into()], None)];
        assert!(effort_drift(&unknown).is_empty(), "未接入的新模型不该算漂移");
    }

    #[test]
    fn strip_date_suffix_boundaries() {
        assert_eq!(strip_date_suffix("claude-sonnet-4-5-20250929"), "claude-sonnet-4-5");
        // 裸名末段非 8 位数字,不误剥。
        assert_eq!(strip_date_suffix("claude-opus-4-8"), "claude-opus-4-8");
        assert_eq!(strip_date_suffix("claude-opus-4-5"), "claude-opus-4-5");
        // 7 位或 9 位都不剥。
        assert_eq!(strip_date_suffix("claude-x-2025092"), "claude-x-2025092");
    }

    #[test]
    fn resolve_base_handles_both_thinking_aliases_and_dates() {
        let want = "claude-sonnet-4-5";
        for form in [
            "claude-sonnet-4-5",
            "claude-sonnet-4-5-thinking",
            "claude-sonnet-4-5-reasoning",
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-5-20250929-thinking",
            "claude-sonnet-4-5-20250929-reasoning",
        ] {
            assert_eq!(resolve_base(form).map(|m| m.advertised_id), Some(want), "{form}");
        }
    }
}
