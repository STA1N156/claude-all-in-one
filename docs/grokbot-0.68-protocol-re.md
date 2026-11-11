# GrokBot 0.68.1 协议复核与 2026-10-08 404 根因定位

## 结论先行

**2026-10-08 的 `SendGrokBotUserMessage` 404(connect code `not_found`,message
`"Error"`)与 GrokBot 桌面端升级到 0.68.1、与 temporal harness 被废弃/迁移**
**均无关。根因是 `sandchat.rs` 自己往请求里塞的 `session_id` 字段。**

> **2026-10-08 第二轮核查更新(见「五、第二轮核查」)**:第一轮给的修法
> ("留空发首条 → 从 `WatchGrokBotTranscripts` 的 `rows.session_id` 读回服务端
> 派发的真实值")**被证伪**——个人账号的 temporal agent 根本不存在"服务端给你
> 派发第二个 session_id"这件事,**任何**非空 `session_id`(不管是 `caio-<fp>`
> 还是标准格式 UUID)在这类账号上都是 404,**没有例外**。正确结论和修法方向见
> 第五节,第三节保留作记录但**不要照抄**。

逐项实锤(全部在 139 用 测试号 账号、经账号自己的固定代理、手写 protobuf 实测,
见下文「实测记录」):

1. `GetGrokBotRuntimeCapabilities` 返回 `temporal_creation_enabled=1`(及其余四个
   能力位全 1)—— 测试号 的 temporal harness **没有被降级或关闭**。
2. `ListGrokBotAgents` 照常返回报告里提到的那两个 `caio-relay` / `harness=temporal`
   agent(`2d28d92d-f8f6-...`、`fa6becee-f4a0-...`)。
3. 对其中一个 agent,**只要请求里的 `session_id` 字段留空**(或是服务端自己发过的
   值),`SendGrokBotUserMessage` 立即 200、`delivery=ACCEPTED_TEMPORAL`,拿回复
   也完全正常,连续两轮验证了多轮记忆(第二轮问"我第一句话说的是什么",模型正确
   答出 `"ping-repro"`)。
4. **只要把 `session_id` 换成 `sandchat.rs` 实际在发的那种客户端自造格式**
   (`caio-<会话指纹>`),对**同一个**、刚刚还能正常收发的 agent,立刻原样复现
   报告里的 404:

   ```json
   {"code":"not_found","message":"Error","details":[{"type":"aiserver.v1.ErrorDetails",
    "debug":{"error":"ERROR_NOT_FOUND",
             "details":{"title":"Resource not found.",
                         "detail":"Grok Bot agent not found.","isRetryable":false},
             "isExpected":true}, ...}]}
   ```

   服务端的 `detail` 文案说"agent not found"，但紧邻那条请求里 agent_id 和前一条
   成功请求完全相同——**这是个误导性文案，真正被判定为"not found"的是
   `session_id`，不是 agent。**
5. `x-cursor-client-version` 用 `0.39.0` 和 `0.68.1` 发出去效果完全一样
   （两个值在同一个 agent 上都拿到 200）——**不存在版本门控**，不用改头表。

也就是说：旧 BOT 池这条路没有烂，`gw-cursor/src/sandchat.rs` 现在发出去的请求本身
就带着一个服务端不认的字段。下面先列 0.68.1 相对 0.39.0 的协议差异（供存档/二期
参考），再给 `sandchat.rs` 的具体改法建议。

## 一、0.68.1 相对 0.39.0 的协议差异

静态比对 `/tmp/.../re/gb068/dist/electron-main/proto.cjs`（0.68.1）与
`/tmp/.../re/gb039/dist/electron-main/proto.cjs`（0.39.0）的
`aiserver.v1.GrokBotService` 方法表（提取后去重，各 270 / 125 个方法）：

- `GrokBotService` 的 RPC 从 125 个涨到 270 个，几乎全是增量（Email、Passkey、
  Marketplace、Meeting、Slack、PullRequestChat、OnePassword 等整类新功能）；
  **只减少了 2 个**：`ListGrokBotMemoryShards` / `PutGrokBotMemoryShard`
  （被 `ListGrokBotUserBotMemories` / `PromoteGrokBotMemoriesToTeam` 取代，
  与我们的发消息路径无关）。
- `SendGrokBotUserMessageRequest` 字段表完全印证了你给的字段号（1~19），
  0.39→0.68 新增的是 10 traceparent / 11 enter_epoch_ms / 12 composed_at_ms /
  16 machine_id / 17 one_password_app_installed / 18 initiator / 19 task_id，
  **全部 optional，客户端实测不发也不影响请求成功**（我们的探针完全没带这些字段，
  200 照拿）。**没有 model 字段**，与 0.39 一致，GrokBot 仍是纯 grok 单模型。
- `source`(字段13,enum `GrokBotClientSurface`:UNSPECIFIED/DESKTOP/MOBILE/GLASS)
  —— 0.68 桌面端默认值就是 `"desktop"`(主进程 `iv="desktop"`，`sendPrompt` 缺省
  自动补这个值)，与现有代码的 `SOURCE_DESKTOP=1` 常量一致，不用改。
- `initiator`(字段18,新枚举 `GrokBotUserMessageInitiator`:UNSPECIFIED/USER/
  TEMPLATE_IMPORT/TEMPLATE_SHARE/CARD_ACTION/PLUGIN_SETUP/TEACH_RECORDING)——
  0.68 客户端构造请求时**始终带这个字段**，但普通用户敲字发消息那条路径在渲染层
  没有显式设成 `"user"` 的字面量调用点（没抓到），main-app.cjs 里不传时映射成
  `UNSPECIFIED(0)`。**不是必填的拦截字段**：我们的探针完全不发它，三次 Send
  都 200。可以不加，加了也无害（建议留空/0，成本为零，收益也为零）。
- **新增枚举 `GrokBotTemporalHarnessMode`**(UNSPECIFIED/OFF/SHADOW/LIVE/BOX)。
  这是你怀疑"harness 被迁移"的最直接证据来源，但**客户端代码里完全没有调用点
  读取或使用这个枚举**——它只出现在 proto 描述符里，大概是服务端内部状态机的
  描述，通过 `GetGrokBotHarnessMigrationStatusInternal`（见下）这类 `Internal`
  后缀 RPC 对内暴露，**普通桌面客户端的发消息流程根本不查它**。
- **你提到的 `GetGrokBotSessionBox` / `ListGrokBotSessionBoxes` 实际不存在**
  （无前缀版本）——proto 里只有 `AdminListGrokBotSessionBoxes` /
  `AdminGetGrokBotSessionBox`，这是运营后台专用 RPC（连同
  `AdminReapGrokBotSessionBoxNow`、`AdminHibernateGrokBotSessionBox`、
  `AdminForceDeleteGrokBotSessionBox` 等一整组 `Admin*` 方法），desktop 客户端
  代码里（`main-app.cjs`/`main-core.cjs`/`node-agent-coordinator`/
  `local-exec-daemon` 全扫过）**零调用点**。正常发消息流程不需要也不能碰它们。
- **`EnsureGrokBotBoxHarnessMigrationPass`、`EnsureGrokBotCursorMainBot`**
  同样在整个 0.68.1 客户端 bundle 里**零调用点**（只在 proto 描述符和 RPC 分发表
  里存在）。前者名字像是"把某账号从 temporal 批量迁到 box"的迁移任务入口，
  后者的 "CursorMainBot" 命名暗示是给 Cursor IDE（代码编辑器）侵入式集成用的
  主 bot，不是 GrokBot 桌面聊天应用的发消息路径。两者都**不需要客户端主动调用**，
  不是我们该接的 RPC。
- **`EnsureGrokBotDefaultMainAgent` + `SetGrokBotMainAgent` 是真实被调用的新流程**
  （main-app.cjs 里有确切调用点 `t.ensureGrokBotDefaultMainAgent({},r)`），用途是
  管理"账号默认主 agent"这个新概念（outcome 枚举：CREATED/PRESENT/
  HAS_MAIN_AGENT/TOMBSTONED/PINNED_FULL/BUSY），是给 0.68 新增的"单一长期陪伴
  agent"UI 用的，**和我们这种"每会话建一个临时 temporal agent 发消息"的用法是
  两条不同的产品路径**，不是 `ListGrokBotAgents` 挑 agent 这条路的替代品——
  后者本次实测仍然完全可用，不需要切过去。
- Header / checksum 算法不变：`zyg` 混淆公式、checksum 只带 machineId（无
  macMachineId 后缀）、`x-sand-box-namespace: prod`、`x-cursor-client-type: sand`
  全部照常生效，0.68.1 版本号也被服务端照常接受。

## 二、实测记录（139,测试号,账号自身代理,全部手写 protobuf,2026-10-08）

读操作 2 次 + 写操作（SendGrokBotUserMessage）3 次，CreateGrokBotTemporalAgent
0 次（全程复用现有 agent，没有新建 `caio-` agent）：

| # | RPC | session_id | client-version | 结果 |
|---|---|---|---|---|
| 1 | GetGrokBotRuntimeCapabilities | - | 0.39.0 | 200,全部能力位=1 |
| 2 | ListGrokBotAgents | - | 0.39.0 | 200,两个 `caio-relay`/temporal agent,与报告一致 |
| 3 | SendGrokBotUserMessage(`ping-repro`) | *(空)* | 0.39.0 | 200,`delivery=ACCEPTED_TEMPORAL`,拿到回复"Pong, ..." |
| 4 | SendGrokBotUserMessage(token+回忆第一句) | *(空)* | **0.68.1** | 200,回复正确带出 token 且准确复述"ping-repro"→**多轮记忆正常，且 0.68.1 头表无门控** |
| 5 | SendGrokBotUserMessage(同 agent) | `caio-85a6626edaaedb9b`(模拟 sandchat.rs 自造格式) | 0.39.0 | **404 `not_found`,"Grok Bot agent not found"——复现报告症状** |

第 5 次之前第 3/4 次刚刚成功过同一个 agent_id，唯一变量是 session_id 的来源
（空 vs 客户端自造），结果从 200 变 404，定位干净。

## 三、`sandchat.rs` 的改法建议（第一轮,已被第五节修正,仅作记录）

当前 `fresh_session_id()`（`sandchat.rs:1526`）无条件生成
`caio-<conv_fp>[-<随机后缀>]`，并且 `SandLookup::New` 分支（`chat_stream` 里，
约行 1406）新会话也直接用这个自造值作为 `session_id` 发首条消息、开
`WatchGrokBotTranscripts`。协议文档早就写了"新会话不带 session_id，服务端生成"
（`docs/grokbot-0.39-protocol-re.md` 第 101-102 行），但实现上这条没有真正落地
——`caio-<fp>` 一直被当成"服务端会认的会话句柄"在用，这是本次 404 的直接原因。

建议的修法方向（供评审，不是最终 diff）：

1. **新会话首条消息：`SendGrokBotUserMessage.session_id` 留空**（proto3 不发该
   字段，对应 `encode_send` 的 `session_id` 参数传 `""`，`Writer::string` 省略
   写该字段，或显式不调用）。
2. **真正的 session_id 要从服务端拿**：protobuf 描述符里
   `GrokBotTranscriptWatchRows`（`WatchGrokBotTranscripts` 响应 oneof 的
   `rows` 分支）有 `6 session_id string`，这是服务端在每批 transcript 行里
   回显的权威会话句柄（本次探针为了省事用的是 unary
   `ListGrokBotTranscriptEntries`，这个方法本身**不返回 session_id**，只有
   streaming 的 `WatchGrokBotTranscripts.rows` 才带，`sandchat.rs` 现有代码已经
   在用这条 RPC，只是目前只解析 `entries`，没有读 `rows.session_id`）。
   新会话应该：留空发送 → 开 watch(cursor.session_id 也留空或省略) → 从第一个
   `rows` 帧里读出真正的 `session_id` → 写回 `SandSessions`（`conv_fp` →
   服务端 session_id 的映射，取代现在的 `conv_fp` → `caio-<fp>` 映射）→
   后续续传该会话时用这个服务端值。
3. `SandLookup::Restart`（分叉/prefill 降级重开）目前也调用
   `fresh_session_id(&conv_fp, true)` 生成新的自造 ID——同样需要改成"留空发首条
   → 等服务端新发一个 session_id"，不能延用自造后缀方案。
4. 二期再谈的问题：**一个账号的 temporal agent 到底能不能"多会话并行"**——
   0.39 文档说 session_id 是 temporal 专属能力、支持同一 agent 跑多个独立线程；
   但本次没有验证"同时给同一个 agent 发两个不同的新会话、分别留空 session_id，
   服务端是否真的会分配出两个不同的 session_id 并互相隔离"，还是说不带
   session_id 时服务端其实只认一个"默认会话"（类似 box agent 的隐式默认会话）。
   这个问题决定了改完之后"同账号多会话并发"要不要继续排队串行，建议作为
   fix 落地后的单独验证项（会多花 1~2 次真实 Send 配额，且需要两个独立
   conv_fp 同时打同一个 agent，建议留给专门的验证窗口而不是本次顺手做）。
5. 已确认**不需要**动的部分：header 表、checksum 算法、`BASE` URL、
   `AGENT_NAME="caio-relay"` 认领逻辑、`CLIENT_VERSION` 常量（0.39.0 继续可用，
   若想贴近最新客户端也可以升到 0.68.1，两者本次测试结果一致，不是必须项）、
   `ensure_agent` 里"排除已死 id 重建"的逻辑本身（它现在找不到 bug 触发场景，
   因为两个现存 agent 根本没死，这条防御逻辑留着无害，等 session_id 修好之后
   如果还有 agent 真的被回收，它依然有用）。

## 四、（已被第五节取代，不要看这节的总结）

~~改 `sandchat.rs` 让新会话留空 session_id、从 `WatchGrokBotTranscripts` 的
`rows.session_id` 读回服务端派发的真实值即可修复~~ ——**这条路走不通**，服务端
对个人账号的 temporal agent 根本不会派发"第二个 session_id"。正确总结见第五节。

## 五、第二轮核查：到底能不能在一个 agent 上开出第二个独立会话

### 5.1 问题

第一轮的两次成功实测都是 `session_id` 留空，这**不是"服务端每次都新开一个
会话"，而是"落进了这个 agent 唯一的默认会话"**——`sandchat.rs` 如果照第一轮
建议改成"新会话留空"，不同客户/不同 Claude 对话会全部落进同一条 grok 对话里，
互相看到对方内容，是真实的跨客户泄漏，不能这么改。

### 5.2 代码/协议侧证据

- 0.68.1 渲染层(`dist/renderer/assets/index.eager-app-xagcrANC.js`)里有一处
  `const it=""` 作为"空=默认会话"的哨兵常量，和一个
  `ds(e){ return e.sessionId===it ? Zo(e.agentId) : ... }` 的 transcript 缓存键
  函数——**空字符串就是"MAIN 会话"的字面表示**，不是"每次留空服务端都发一个新
  值"的占位符。
- `GrokBotAgent` 的完整字段表（之前只核对到 `id/name/agent_id/harness/kind`，
  这次把剩下的也读出来了）：
  `...14 role|15 visibility?|16 team_id?|17 viewer_is_owner?|`
  **`18 viewer_session_id`**`|19 kind #1|20 member_agent_ids*|21 owner_display_name?|
  22 people*|...`。渲染层 `addressedSessionId()` 函数读的就是这个
  `viewer_session_id`，配合一个 `isMultiplayerSessionsEnabled` 功能开关——这是
  **团队共享 bot 场景下"每个查看者自己的一条会话"**的机制（配合
  `GrokBotAgentSessionKind` 枚举：`MAIN/SLACK_DM/SLACK_THREAD/DM/GROUP`，后四种
  全部对应外部集成场景：Slack 私信、Slack 线程、团队内其他查看者、群聊），
  **不是"同一个人/同一个账号在同一个 agent 上开 N 个并行会话"的通用机制**。
- 实测 `ListGrokBotAgents` 对 测试号 的两个 `caio-relay` agent，`viewer_session_id`
  字段**缺省/为空**（个人账号，没开团队多人功能），`kind=1`(MAIN)。印证这两个
  agent 只有唯一一条 MAIN 会话。
- 协议里**没有任何 Create/Start/Ensure 类的"新建会话"RPC**（`ListGrokBotAgentSessions`
  只读、不写；搜了全部 270 个 0.68.1 方法名，`Session` 相关的只有只读的
  `ListGrokBotAgentSessions` 和一组 `Admin*SessionBox`，后者是 box 运维后台，
  跟 temporal 的"会话"不是一个东西）。渲染层构造 `sendPrompt` 参数时，`sessionId`
  字段只在"继续已知会话"时才带上（`...He==null?{}:{sessionId:He}`，`He` 来自
  已有状态，不是现场生成的新值），**官方客户端本身从不会凭空发明一个
  `session_id` 去发起新会话**。

### 5.3 真实验证（新增，139/测试号，手写 protobuf）

在**同一个**、第一轮已证明健康的 agent（`2d28d92d-f8f6-...`）上，这次用**标准
格式 UUID**（不带 `caio-` 前缀，排除"格式不对"这个猜测）当 `session_id` 发消息：

```
status 404
{"code":"not_found","message":"Error",
 "debug":{"error":"ERROR_NOT_FOUND",
          "detail":"Grok Bot agent not found.","isRetryable":false}}
```

**和 `caio-<hex>` 格式完全一样 404**——不是 UUID 格式问题，是**服务端压根不接受
任何它没有登记过的 `session_id` 值**，无论长什么样。结合 5.2 的协议证据：
个人账号的 temporal agent **没有"客户端自己发起、服务端据此新开一条会话"这条
路**，可用的会话只有隐式的 MAIN(空字符串)一条。

本轮新增 1 次真实 `SendGrokBotUserMessage`（标准 UUID 测试，404，没有触发真实
推理，不计费）；没有新建任何 `caio-` agent（仍是 0 个，继续复用第一轮的两个
现存 agent）。第一轮 + 第二轮累计：真实 Send 4 次（2 次成功落地推理，2 次在
session 校验阶段就被拒、未跑模型），新建 agent 0 个。

### 5.4 结论

1. **"隔离/记忆"测试的前提本身不成立**：个人账号在一个 temporal agent 上
   只有一条会话，没有"开第二个独立会话再测隔离"这个操作可做——所有非空
   `session_id` 都 404，不存在"隔离但能用"的中间态。
2. **真正的隔离单位是 agent，不是 session_id**。0.39 doc 里 box 降级路径早就是
   "每个客户端会话创建独立 box agent"（见 `docs/grokbot-0.39-protocol-re.md`
   "账号侧"/"请求流"一节），这个设计是对的；`sandchat.rs` 的 temporal 路径
   当时为了省 agent 数量，改成"一个共享 agent + 自造 session_id 做多路复用"，
   这条捷径在协议层面从未真正成立（大概率这次之前能跑，是因为服务端曾经对
   未知 session_id 静默降级成 MAIN 会话，没有强校验；这次的 harness 调整大概率
   是加上了强校验，把"静默兜底"变成了"硬 404"，表现为"今天突然坏了"）。

### 5.5 建议的修法方向（供评审，替换第三节）

把 temporal 路径的 agent 粒度从"每账号一个共享 `caio-relay`"改成"每个
`conv_fp`(逻辑会话指纹)一个独立 temporal agent"，与 box 路径的粒度对齐：

1. `SandSessions` 的键从 `account_id → agent_id` 改成
   `(account_id, conv_fp) → agent_id`；`ensure_agent()` 按 `conv_fp` 查/建，
   agent 名字可以仍用 `caio-relay` 前缀加短指纹后缀方便人工识别和回收
   （比如 `caio-relay-<conv_fp 前 8 位>`），避免和老的两个无后缀 `caio-relay`
   agent 混在一起挑错。
2. **所有 `SendGrokBotUserMessage` / `WatchGrokBotTranscripts` 的 `session_id`
   字段恒为空**（删掉 `fresh_session_id()` 整个函数和它的调用点，`encode_send`
   / `encode_watch` 的 `session_id` 参数可以直接去掉，不用再传）。隔离靠
   "一个 conv_fp 一个 agent"天然保证，不靠会话句柄。
3. 需要补的现实问题（都没测，标注给维护者评估）：
   - **account 下能建多少个 temporal agent 有没有上限**——`EnsureGrokBotDefaultMainAgent`
     的 outcome 里有 `PINNED_FULL`，暗示"主 agent 槽位"有限，但那是另一套
     （"默认主 agent"）机制；`CreateGrokBotTemporalAgent` 本身目前没见到报过
     数量上限的错误码，但两个月前的旧 agent 没清理掉还一直占着位置
     ——需要补一条"conv_fp 过期后调 `DeleteGrokBotAgent` 回收"的清理逻辑，
     否则长期跑下去 agent 数量只增不减。
   - 旧的两个 `caio-relay`（`2d28d92d-...`、`fa6becee-...`）历史遗留，没有
     对应任何活跃 `conv_fp`，可以在确认新方案上线后手动/脚本清掉（本次调查
     没有删除它们，仅做了收发验证）。
   - `ensure_agent()` 现有的"按 `exclude` 排除已死 id 重建"逻辑在新模型下可以
     整段去掉（不再有"同账号多个候选 agent 里挑一个"的场景，一个 `conv_fp`
     只对应一个 agent，建不出来就是建不出来，不用挑替补）。
4. 不受影响、继续沿用：header 表、checksum 算法、`BASE` URL、
   `CreateGrokBotTemporalAgent` 的字段编码本身（`encode_create_agent`），
   `CLIENT_VERSION` 用 0.39.0 或 0.68.1 均可（两版头表本轮再次验证等效）。
