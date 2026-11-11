# Changelog

## [cursor-one-strike-per-dead-conn] - 2026-11-10
### 背景
- 15:23 上线连接自愈后实测:自愈本身有效(被禁的 3 个号人工重置后在新连接上 71/71、50/50、40/40 全部成功),但同一条坏连接上常有 3~9 个并发流,几秒内各自超时报 `ServerError`。调度器把它们算成「连续失败 3 次」,号照样被禁(07:26:54 第一个流已重建连接,07:27:12、07:27:18 两个同连接的流凑够 3 次),只能人工重置。
- 被禁一个号就少 10 个并发名额;07:17~07:30 号池「组内所有账号并发已满」,chengzi 站收到约 1,500 次 429。
### 变更
- **gw-core**:`UpstreamError` 新增 `collateral_failure: bool`(默认 false)和 `as_collateral_failure()`:表示「同一故障的连带回声,provider 已就地处置根因」。kind、对外文案、状态码、投递状态都不变,客户端照常收到同一个错误。
- **gw-app**:`should_report_inference_failure`(流中错误、非流式抽干两条路径共用)遇到该标记不上报调度器;首包前的两个上报位置(`chat 失败`、`刷新后重试仍失败`)同样跳过,口径一致。
- **gw-cursor**:`heal_stalled_connection` 由 `inspect` 改为 `map`。连接卡死时,第一个发现者(`evict()` 返回 true)照常上报;这一代连接已被丢弃(`evict()` 返回 false)的其余流打连带回声标记。
### 设计取舍
- 一条坏连接只扣一次账号健康,而不是完全不扣:重建后的新一代连接如果又卡死,`evict()` 照样返回 true 并计数,连续 3 代都坏仍会禁号,避免「号不停、错误一直在」。
- 没有改 kind(例如改成 `Overloaded`):那会改变对外 529 文案,还会开模型级过载窗口,波及别的号。
- 其它 provider(kiro / claude-dario / claude-subprocess)都经 `UpstreamError::new` 构造、默认不打标记,行为不变。
- 方案经 Kimi Code K3.1 评审:无高/中问题;两条低级建议(首包前上报口也看标记、字段名与 `UpstreamErrorKind::spares_account_health()` 易混)已采纳。
### 测试
- 新增 3 条:`collateral_failure_defaults_off_and_changes_nothing_else`(gw-core)、`spared_echo_failures_are_not_reported`(gw-app,kiro/cursor 各测一遍)、`heal_counts_one_failure_per_dead_connection`(gw-cursor:同代第二个流打标、新一代首个不打、旧代迟到的打标且不丢新连接、心跳型与无句柄不打)。
- 反向验证:去掉 gate 判断或去掉打标后,对应测试失败。`cargo test --workspace` 全绿。

## [cursor-cache-sim-fix] - 2026-11-10
### 背景
- 用户发现 chengzi 站一条 `claude-fable-5-2` 请求(输入 127,939、输出 169)按「完全没命中缓存」收了全价;Cursor 账单显示这条上游读 177,726 缓存、只写 1,118。CLI 形态没有上游用量帧,用量按客户端完整上下文估,缓存读来自 `cache_sim` 模拟。
- **根因一(bug)**:模拟键 = 账号 + `ctx.conversation_id`,而后者是发给 Cursor 的会话 ID。会话在两个号之间来回跳时,回到原号被判分叉,provider 每轮按全部历史指纹重算新 ID → 模拟键每轮变 → 永远判 0(会话 `13a9918f` 同号连续 5 轮模拟 0,上游每轮读 14~17 万)。
- **根因二(口径缺失)**:模拟从不报缓存写。Cursor 真实账单(14:46~14:55 全部号)读 86.9%、写 10%、普通输入只有个位数 token;Anthropic 对缓存写按输入价 1.25 倍收,caio 却按普通输入(1 倍)报。
- 同窗口全部号:模拟读 92.5% vs 真实 86.9%,整体客户少付;来回换号的会话则被误判全价、客户多付。
- 已确认**上游缓存按号隔离**(同会话换号后上游写 149,049、读 8,279),所以键里保留账号。
### 变更
- `chat::RunCtx` 新增 `sim_conversation_id`(provider 填 `base_conversation_id`,由会话亲和键派生、分叉不变);模拟键改由 `sim_session_key` = 账号 + 稳定会话 ID 生成。挂流续跑用同一个模拟值,不另改。
- `delta_usage_json`:上游没报任何缓存数据(`real_cache_read_tokens == 0 && cache_creation_tokens == 0`,即估算用量)时,没命中的部分全部按缓存写上报(`cache_creation_input_tokens` + `cache_creation.ephemeral_5m_input_tokens`),`input_tokens` = 0;上游自报缓存时原样。缓存读夹在总输入内。
- **入库口径不变**(`StreamItem::Usage` 的缓存写列不填),caio 后台成本统计不会重复计算,不用部署 router。非流式经 `gw_core::fold` 逐字段合并 usage,缓存写自动带上。
- new-api 兼容性核对:`message_delta` 里缓存写 >0 才覆盖;`input_tokens` 为 0 时保留 `message_start` 的值(caio 也是 0);Claude 流式收尾刻意不用本地估算顶替输入。
### 同版:连接卡死自愈
- 背景:15:00 前后又出现成片「90/150s 只有心跳」,集中在少数号且按时段轮换。202 内存已正常(内存压力 0);卡住与上下文大小无关(各尺寸卡住率都约 3%),与「挂起流数 + 在跑数」只有弱相关(中位 15 vs 14,挂起流占满 Cursor 名额的猜想不成立)。139 上看到某号连接「只发不收」(411MB 连接 lastrcv 133s、lastsnd 0.7s;另一条累计重传 7014 次)。号连续失败 3 次被禁、冷却结束后**仍用这条坏连接**,于是反复卡、反复禁,只有重启 worker 才好。
- `cursor_provider.rs`:缓存条目带代际号(`CachedClient`),新增 `client_and_handle` 同时交出连接句柄 `ConnHandle`;`ConnHandle::evict` 只在缓存里仍是本请求那一代时才丢(同一条坏连接上陆续超时的请求不会把已重建的新连接再丢一遍)。并发建同一号的 client 时用先进缓存的那个。
- `chat/stream.rs`:流循环记「最后一次收到任何字节」的时刻(心跳也算)。停滞时若已 ≥30s(`CONN_DEAD_SILENCE`,心跳 10s 一个=连丢 3 个)一个字节都没收到,报「…连接没收到任何字节 Ns(连接疑似卡死)」(标记常量 `CONN_DEAD_MARK`);一直有心跳的停滞照旧报「N 秒内只有心跳」——连接是活的,重建无益(按 /code-review 意见收窄)。
- `provider.rs::heal_stalled_connection`:`chat()` 返回的流外包一层,看到 `chat::is_connection_stall`(ServerError 且含 `CONN_DEAD_MARK`)就丢掉这个号的连接、记一条 warn;只有心跳、服务端总结超时、只有工具参数增量都不算。默认出口(没配代理)无句柄、原样返回。
- 「只发不收」的根因(202→Cursor 那段断、还是 HTTP/2 流控卡死)未定;本修复对两者都有效。更快的发现(HTTP/2 PING 心跳)未做。
- 测试 4 条(句柄只丢同一代、停滞分类、包装层只在连接卡死时丢、帧序列上连心跳都没有才报卡死而有心跳不报);去掉自愈、代际判断或把卡死阈值置 0 后对应行为测试确认失败。
- 同版还按 /code-review 改了:用量是否估算由 stream.rs 按**来源**显式传入(不按数字是否为零猜,上游用量帧明说 0 时不当估算);估算时报缓存写**只限 Claude 系**(`model_has_cache_write_surcharge`;grok/gpt/composer 没有缓存写加价,照旧按普通输入);`build_proxy_client` 文档挂错位置。gw-cursor 416 通过,`cargo test --workspace` 通过。
- 审查意见记下未改:续跑的挂起流在旧连接上卡死时可能误丢已重建的新连接(最多多一次重连);缓存条目不清理、无代理的号不自愈(cursor 号都配了代理,号变动少);HTTP/2 PING 心跳未加(用户之前选了不加)。

### 不在本次范围
- 会话来回换号(调度 `affinity_hold_ms=1500`、`cost_index_kappa=0.2` 照 Kiro 调,对 Cursor 每次换号上游整段重写 13~17 万 token、约 $1~2):用户决定先不动;这两个是全局设置,改要给 cursor 单独配。
### 验证
- 新增/改写单测 4 条(模拟键不随分叉换的会话 ID 变且按号隔离并端到端命中;估算时报缓存写且三项之和等于总输入;上游自报缓存时原样;非流式折叠带上缓存写);改回旧逻辑后其中 3 条行为测试确认失败。gw-cursor 411 通过。
### 注意
- 只换 worker-cursor;回滚改回 `cursor-perconn-20261110`。上线后按同一时间窗用 Cursor 账单对账(读、写占比)。

## [cursor-per-account-conn] - 2026-11-10
### 背景
- 11-10 13:30 起「Cursor Run 90s 只有心跳」卡住暴增(13:40~13:44 每 2 分钟 13~34 条),**按出口扎堆**:出口 8(3 个号,含大流量号)30~40% 卡住、出口 9 约 10%、出口 7(2 个小号)为 0。日本出口代理本身正常(经代理连 api2.cursor.sh 0.75s、ping 0 丢包),139 网卡入 87 / 出 45 Mbps,不是带宽打满;139 上看出口 8 那条连接送达速率约 1 Mbps 且有积压。13:45 重启(连接全部重建)后卡住立刻归零、首包 17.9s→5.4s。
- 根因:`client_for` 的 client 缓存按**代理 URL** 为键 → 同出口的所有号共用一个 reqwest client、一条 HTTP/2 连接;一个号的流量把连接堵住,同出口所有号的流一起卡。多号令牌走同一条连接也比只共用 IP 多一个关联维度。
### 变更
- `cursor_provider.rs::client_for`:缓存键改为 `proxy_client_key(account_id, proxy_url)` → 每个号一个 client、一条连接;出口仍是该号自己配的代理(键里带代理 URL,换出口自然建新 client)。`lib.rs` 字段说明同步。
### 验证
- 新增单测 `proxy_client_is_per_account_and_per_proxy`(同出口两号各一个 client、同号命中缓存、换出口建新 client、键带各自代理);改回旧键后确认失败。gw-cursor 408 通过。
### 注意
- 未做:HTTP/2 PING 心跳检测、卡住后主动换连接(用户选择只做这一步)。

## [cursor-client-translate] - 2026-11-10
### 背景
- 11-10 12:45:48~12:46:17(北京)4 个能用的 cursor 号间隔 10 秒依次被标「额度耗尽」,号池停摆,人工 reset 后立即恢复(号没问题)。
- 元凶是一个 Claude Code 会话(291 条消息、20 张图):assistant 并行发 4 个 `Read` 读图,客户端把结果拆成多条 user 消息、顺序打乱、**图片排在 tool_result 前**。`blobhist::encode_message` 按块原顺序编码 → 两个 tool_result 之间夹了一条图片消息 → Anthropic 400 → Cursor 包成 `resource_exhausted / ERROR_PROVIDER_ERROR`(`providerStatusCode=400`、`isRetryable=false`)→ `trailer_to_error` 把所有 `resource_exhausted` 判成 QuotaExhausted(持久禁用、不自愈)并换号 → 同一个请求 30 秒禁掉 4 个号。
- 当时窗口内 1021 条 cursor 请求,合并连续 user 后「tool_result 不在最前」只有这一个请求(4 次全败);孤立 tool_use / tool_result、重复 id 都是 0。
- 客户端翻译责任链此前只在 Kiro 线(`gw-kiro::converter::clients`),cursor 线只借用了识别。
### 变更
- **cursor 线责任链**(新模块 `chat/clients/`):识别(`gw_core::clients`,分流前取信号)→ ① 兜底 = 原 system 角色分流(冻结,原样搬入)→ ② 按客户端步骤(本次为空)→ ③ 出口合规 `tool_results_first`(按形态,所有客户端)。`provider.rs::chat()` 第一行改调 `chat::clients::translate`(原来就是 system 分流的位置,早于会话 ID / 指纹 / envnorm / 自造历史);「cursor 路由」日志加 `client`,改写时打一条只含个数的 info。
- **出口合规**:一段连续 user 回合里只要有非 tool_result 块排在 tool_result 前,就把这一段合成一条 user:tool_result 原序在前,其余块(图片、文字;字符串内容转 text 块)原序在后。格式正确的请求一个字节都不动。放在最后是因为 ① 可能把中段注入的 system 变成 user,插到 tool_use 与 tool_result 之间。
- **提供方错误分类**:`TrailerError` 解析 `providerStatusCode`(字符串/数字)与 `isRetryable`,摘要带 `[provider=400 retryable=false]`。`trailer_to_error` 在按 code 分类前认出提供方错误(`ERROR_PROVIDER_ERROR`,或 `ERROR_RESOURCE_EXHAUSTED` 带 provider 标记):4xx(429 除外)→ BadRequest(对客户端可见的我方文案,不罚号、不换号);5xx / 429 / 无状态码 → Overloaded(同号退避,不罚号)。没有提供方标记的 `resource_exhausted` 仍是 QuotaExhausted。
### 设计考虑
- Kiro 那两步 `...` 占位清理(pi / Grok Build)**没搬**:cursor 流量里 Grok 61 条有 57 条带 `...`、其它客户端 109 条,但带文字的回复里以 `...` 开头的是 0(Kiro 线当时 19%/18%);搬过来会改几百条正常请求的字节、进行中的会话重铺一轮。
- 出口合规按形态而不按客户端:任何客户端(或其前面的中转)发来这种结构上游都会 400;只在违规时改写,对合规流量零影响,符合 10-30 定下的「中转痕迹按形态修放链上、按客户端的放链里、兜底冻结」。
- 改写只依赖一段连续 user 本身,段后跟了 assistant 就不再变,所以跨轮稳定,会话指纹与自造历史不会分叉。
- 不在本次范围:调度层「同一请求在多个号上报同样的账号级错误 → 判为请求问题、停换号、撤销处罚」的通用兜底(要动 gw-app 通用调度,会影响 Kiro 线,需单独讨论);孤立 tool_use / 重复 id 修复(cursor 流量里没有)。
### 验证
- 新增单测 13 条(翻译 9、错误分类 3、尾部帧解析 1),另有 1 条默认忽略的线上回放入口 `replay_from_env`;去掉两处修复后其中 6 条行为测试确认失败。gw-cursor 403 通过;`cargo test --workspace` 通过(gw-app 674、gw-core 213 等)。
- 线上流量回放(实现本身,139 线上镜像断网跑,数据不出 139、用完即删):1237 条真实 cursor 请求翻译结果与「只做原 system 分流」**逐字节一致**,独立 Python 复核合规 1237/1237;识别分布 claude_code 611 / opencode 255 / fallback 176 / grok_build 112 / deepseek_harness 83。变异回放:在 1114 条真实请求最后一个工具结果消息前插文字块造违规,翻译后 1114 条全部与独立推算的期望**完全一致**(违规段合成一条、其余消息逐字节不变),复核合规 1114/1114。
### 注意
- 只换 worker-cursor;回滚 compose 改回 `cursor-builtinhold-20261110`。

## [cursor-hold-builtin-tools] - 2026-11-10
### 背景
- 长会话反复被服务端总结:同一会话 20 分钟总结 29 次,每次 20~90 秒。日志还原的循环:自造历史重建 → 挂流续跑一两轮 → 上下文过阈值,服务端总结 → **总结后模型改调 Cursor 自带工具** → 自带工具调用不挂流 → 下一轮按客户端完整历史重建,摘要丢失 → 再总结。
- 数据(计费修复版上线后):真正发出的 482 次重建里 54% 紧跟在自带工具调用之后;自带工具占全部工具调用约 20%,但**总结后的第一次工具调用 81% 是自带工具**(总结压掉了历史里的 MCP 用法示例)。分模型:fable 0%、opus 约 17%、haiku 约 30%、sonnet 约 33%。自带工具里终端 143、读文件 103(界面帧路径),grep 62(exec 路径)。
- 实验(`examples/probe_builtin.rs`,cursor-01M2H3NGCGHE,sonnet-5.5,1 个请求):不给客户端工具逼模型用自带工具 —— 读文件界面帧之后同毫秒到 `read_args`(7),终端到的是 **`shell_stream_args`(14)**;按 cursor-agent `agent.v1` 编的「成功」结果(`ReadSuccess`;`ShellStream` start→stdout→exit→关流)服务端都认,模型答出了两个暗号,本轮正常结束。
### 变更
- `held.rs`:`PendingExec` 加 `kind: PendingKind`(Mcp / Read / ShellStream);`encode_result` 按种类编码回上游的结果:读文件 `ReadSuccess{path, content}` / 出错 `ReadError{path, error}`;流式终端 start→stdout→exit(出错退出码 1)→关流;文本按 MCP 同一上限截断。读文件**不报总行数**:客户端的结果常带行号、常只读一段,数出来的行数既不是文件总行数也不是内容行数。
- `exec.rs`:`holdable_exec` 从读文件 / 流式终端的 exec 帧取出种类与 `tool_call_id`(`ReadArgs.2` / `ShellArgs.4`;缺 id 不认)。普通终端 `shell_args`(2)CLI 实测从不出现、结果格式没验证过,**不挂**。流式终端的关流帧抽成 `stream_close_frame`,拒绝和回结果共用;续跑编码复用 exec 的分支常量。
- `chat/stream.rs`:
  - 界面帧翻出读文件 / 终端调用且本轮能挂起时,交付 tool_use 后**不收工**,起 3s 收齐宽限等 exec 帧;exec 帧按 `tool_call_id` 配上已交付的调用 → 进挂起收集表,配齐即挂起;宽限内没来按旧行为收工、不挂。
  - 等 exec 期间**只收配得上的 exec**;其余任何工具帧(调用 id 对不上、缺 id、后台终端、别的自带工具、翻译不了的界面帧)一律按旧行为立即收工、不挂起 —— 既不能落到「exec 形态内建工具」翻译(会再交一个 tool_use,客户端执行两次),也不能落到收口(本轮已有工具调用,收口会误判成空回复)。旧行为在第一个自带工具界面帧就收工,后面的帧本来读不到,所以不丢东西。
  - 自带工具与 MCP 工具并行调用:两份调用都交付,本轮不挂起。
  - 自带工具界面帧查「已回结果」名单(续跑段的完成态回显)与本段已交付名单(同一调用重复来帧),都不再交付。
  - 配齐挂起、收齐宽限到期两处补上「已收尾」标记:中途若夹了 turn_commit,下一个工具帧会让排水段退出并清掉它,不补的话正常工具轮被判成流中断(MCP 配齐挂起处同样补上)。
  - 并行的多个自带工具:第一个配齐时第二个的界面帧可能还没到,照样挂起,第二个调用在续跑段交付(多一次往返,结果不错)。
- `chat/mod.rs`:续跑回传改用 `encode_result`(多帧逐帧发)。
- grep(exec 5)的成功结果结构较深,本次未做,仍按旧行为。
- 未处理的老问题(记录):MCP 调用在前、自带工具界面帧在后时,自带工具帧会被 `await_tool_commit` 分支当成回声吞掉,模型这次自带工具调用丢失。不是本次引入,单独处理。
### 验证
- 新增单测:自带读文件 / 流式终端调用轮挂起且只交付一次、挂起记录带种类与参数;等不到 exec 帧 3s 后按旧行为收尾;不挂起时立即收尾;配不上的 exec 立即收工且不重复交付;exec 前夹 turn_commit 照样挂起不报错;界面帧重复只交付一次;与 MCP 并行时两个都交付且不挂起;续跑段回显不重复交付;结果帧字段与帧序(读文件不带总行数);exec 识别缺 id 不认、普通终端不认。其中 3 条在去掉对应修复后确认会失败。gw-cursor 390 通过,`cargo test --workspace` 通过。
- 审查(`/code-review medium`)10 条意见:正确性 7 条已改;并行自带工具拆多轮(接受,写进注释);`PendingKind` 保留独立枚举(只放续跑编得出结果的种类,类型上保证不会挂起一个编不出结果的调用),分支常量与关流帧已复用。
### 注意
- 未部署;上线前应在隔离 worker 上用真号跑一次完整链路(自带工具调用 → 挂起 → 客户端结果回来 → 续跑出字)。

## [cursor-remove-dead-settings] - 2026-11-10
### 变更
- 删除随 CLI 驱动 / 旧推理面 / sandchat 一起失效的 7 个设置键:`cursor_cli_notice`、`cursor_cli_phase_timeout_secs`、`cursor_cli_tools_models`、`cursor_default_driver`、`cursor_field9_tools`、`cursor_text_tools`、`cursor_sandchat`。
  - gw-core:`SystemConfig` / `SystemSettings` 字段、`apply_to` 合并、`from_effective` 全量视图、默认值函数与常量(`DEFAULT_CURSOR_CLI_PHASE_TIMEOUT_SECS`、`DEFAULT_CURSOR_DRIVER`、`CURSOR_DRIVER_CHOICES`、`DEFAULT_CURSOR_CLI_TOOLS_MODELS`)、2 个测试。
  - gw-app 管理后台:4 个写侧校验块与 3 个测试。
  - admin-ui:设置页的「cursor CLI 单阶段超时」输入框、表单/补丁逻辑、类型字段、中英文文案、测试夹具。
### 兼容性
- 读侧 `SystemConfig`(system.yaml)与 `SystemSettings`(库内设置)都不拒未知键:线上库里残留的 `cursor_cli_phase_timeout_secs`、`cursor_default_driver` 会被忽略,不影响启动;线上 system.yaml 不含这 7 个键。写侧对库里预先存在的未知键本就容忍(既有测试 `put_tolerates_preexisting_unknown_keys_from_newer_image`),保存设置不受影响;部署后可择机从库里清掉这两个残留键。
### 验证
- `cargo test --workspace` 通过(gw-app 674、gw-core 213、gw-cursor 377);admin-ui `bun run build`(含 tsc 严格检查)与 `bun run test`(118)通过。

## [cursor-lib-split] - 2026-11-10
### 变更(纯搬移,不改行为)
- `gw-cursor/src/lib.rs`(2517 → 526 行)里的独立状态表与 Provider 实现搬进各自文件:`tool_guard.rs`(护栏策略句)、`account_config.rs`(账号字段说明/配置版本缓存/RunTuning)、`conv_registry.rs`(会话注册表)、`assets.rs`(附件库)、`sections.rs`(内容分节库)、`shadow.rs`(续轮描述符影子表)、`notices.rs`(截断提示表)、`cursor_provider.rs`(CursorProvider 内部方法)、`provider.rs`(`impl Provider for CursorProvider` 与辅助函数)。lib.rs 只留模块声明、顶层导入、`CursorProvider` 结构体、`fnv1a64` 与测试。
- crate 根以 `use`/`pub use self::<模块>::*;` 接回,`crate::Xxx` 与 `gw_cursor::Xxx` 路径不变。原 lib.rs 的顶层 `use` 写在文件中段,拆分时随护栏段落被带走,已挪回 lib.rs 顶部。
### 验证
- 测试名单 377 个逐条一致;库编译 0 警告、测试编译无新增警告;`cargo test --workspace` 通过。
### 整理后
- gw-cursor:3.9 万行 → 2.56 万行,60 个文件。仍超 800 行的:`chat/stream.rs`(2062,单个状态机函数 1730 行)、`models.rs`(1265)、`cli.rs`(952)、`usage.rs`(904)、`chat/mod.rs`(844,`chat_stream` 670 行)、`cache_sim.rs`(842),以及两个测试文件。两个大函数的内部拆分要动控制流,另行处理。

## [cursor-run-split] - 2026-11-10
### 变更(纯搬移,不改行为)
- `gw-cursor/src/run.rs`(4522 行,其中测试约 2280 行)拆成目录模块 `run/`:`request.rs`(743,请求帧字段号/会话阶段/附件/模型参数/首帧构造)、`tools.rs`(560,工具定义/工具调用与内建工具帧解析/protobuf 值解码)、`response.rs`(652,响应帧解析/心跳·提交·总结判定/续轮描述符/KV 点名/判决)、`exec_frames.rs`(135)、`trailer.rs`(98);`mod.rs`(92)保留模块文档与测试用描述符样本。`pub use self::<子模块>::*;` 重新导出,`gw_cursor::run::X` 路径不变。
- 测试拆进 `run/tests/`(76 个测试按调用的功能模块分到 request/tools/response/exec_frames/trailer,共用辅助在 `tests/mod.rs`);删掉一行原有的孤立文档注释(`/// **绝不发 \`1.2.17\`**`,与下面的测试无关,完整说明在另一处)。
### 验证
- 测试名单 377 个按函数名逐条一致;库与测试编译 0 新增警告;`cargo test --workspace` 通过。

## [cursor-remove-dead-drivers] - 2026-11-10
### 背景
- gw-cursor 3.9 万行里约 1.3 万行是线上不再走的路径(用户决定删除):
  - **CLI 驱动**(`clidrv.rs` 5293 行 + `mcpbridge.rs`):起 cursor-agent 子进程;只剩 `driver=cli` 回滚开关,线上 30 分钟 772 个请求 0 个走它(用户此前已定「出问题修协议,不切回 CLI」)。
  - **旧推理面**(`inference.rs` 4344 行):`InferenceService/Stream`,2026-09-09 起上游对任何 cursor 令牌一律 401。
  - **Bot 周池面**(`sandchat.rs` + `sandchat/`,2642 行):`pool=bot` 账号专用;BOT 组 23 个号全部禁用,worker-bot 24 小时 0 请求。
### 变更
- 删除上述文件与 4 个配套示例(`dump_inference_req`/`e2e_inference`/`e2e_runinference`/`e2e_sandchat`);gw-app 去掉 `--mode cursor-mcp-bridge` 与 `--sock`/`--tools` 参数。
- `lib.rs`(3579 → 约 2510 行):`chat()` 只留 wire 一条路(路由日志保留 `route="wire"`);删 CLI 说明、带工具走 CLI 的模型前缀、默认驱动、驱动路由函数及其测试;删 Provider 的 CLI/sandchat 字段、CLI 分流锁与 `poll_token_updates` 覆盖(回落 trait 默认空实现);热设置不再读 `cursor_cli_phase_timeout_secs`/`cursor_field9_tools`/`cursor_text_tools`/`cursor_sandchat`/`cursor_cli_tools_models`/`cursor_default_driver`/`cursor_cli_notice`。
- 随之变成死代码的 CLI 驱动辅助一并删除:`cli_eligible`、`latest_user_input`、`history_answers_tool_use`、`tail_after_last_assistant`、`last_tool_results`、`client_effort`、`delta_usage_json_pub`;原有死代码 `hex32`、`wire_flush_deferred`、`WIRE_DEFERRED_DEADLINE` 删除;只给测试用的 `stream_to_anthropic`、`to_cursor_model`、`CONV_CONTEXT` 标 `#[cfg(test)]`;`CTX17_DETAIL` 作协议说明保留(允许未使用)。gw-cursor 编译警告 9 → 0。
- **保留** `cli.rs` 与 `cli_catalog.rs`:它们是 wire 线协议的「CLI 形态」(请求头/帧结构/客户端自报模型清单),不是 CLI 驱动。
- 两条测试中段 system 分流的用例原本借 CLI 辅助断言,改用 wire 口径(`to_turns` 末轮 / `last_user_is_tool_result_only`)。
### 验证
- 测试 537 → 377:少掉的 160 条中 155 条属于被删模块(clidrv 69、inference 46、sandchat 31、CLI 说明 5、驱动路由 4),另 5 条是被删 CLI 辅助的专测;无误删、无新增(按函数名集合差核对)。`cargo test --workspace` 全部通过。
- `cargo doc` 断链数与删除前相同(8 条,均为原有中文方括号写法),无指向已删项的断链。
### 行为变化 / 遗留
- 账号 `extra.driver`、`extra.pool`、环境变量 `CURSOR_DRIVER` 不再生效(线上无号使用)。
- gw-core 配置与管理后台里上述 7 个设置键仍在(定义/校验/测试),现为无效设置,待单独清理;`pool=bot` 的模型目录逻辑(models.rs / lib.rs `models_for_pool`)也随之失去意义,待清理。
- worker-bot 容器(旧镜像 cursor-resume-20261008b)仍在空跑,是否停掉待定。
- 本次未部署;线上 `cursor-billing-20261110` 功能与本提交在 wire 路径上一致。

## [cursor-chat-split] - 2026-11-10
### 变更(纯搬移,不改行为)
- `gw-cursor/src/chat.rs`(9668 行,其中测试 4087 行)拆成目录模块 `chat/`:
  - `mod.rs`(868):请求入口 `chat_stream`、请求上下文、帧上传;
  - `stream.rs`(2061):响应帧状态机 `stream_to_anthropic_with`、停滞闸常量、收口;
  - `builtin.rs`(681):内建工具 → 调用方工具翻译;`guard.rs`(468):护栏话术、能力别名、截断提示;
  - `history.rs`(574):消息转换、指纹、续轮判定、工具回路提醒与折叠;`system.rs`(293):system 提取与分流;
  - `media.rs`(179)、`session.rs`(202)、`errors.rs`(150)、`usage.rs`(170)。
  - 测试拆进 `chat/tests/`:共用辅助在 `tests/mod.rs`,122 个测试按调用的功能模块分到 `stream/builtin/history/guard/errors/system/usage/session/media/entry` 各文件。
- 子模块 `use super::*;`,`mod.rs` 以 `pub(crate) use self::<子模块>::*;` 接回,原私有项改 `pub(super)`,原 `crate::chat::xxx` 路径不变。
### 验证
- 拆分前(cb40e09)后对比:库编译警告逐条一致;测试编译警告逐条一致;测试名单(537 个,按函数名)逐条一致;`cargo test --workspace` 全部通过。
- 只对 `chat/tests/` 跑了 rustfmt(从 `mod tests {}` 搬出多一层缩进);功能代码未格式化,保持原样。
### 后续
- `chat_stream`(670 行)与 `stream_to_anthropic_with`(1730 行)两个大函数内部未拆(要动控制流,另行处理);`tests/stream.rs`(1349)、`tests/builtin.rs`(1086)仍偏大。

## [cursor-billing-full-context] - 2026-11-10
### 背景
- 用户看 yapi 消费记录:cursor 渠道「输入 0、缓存读 8K~70K」,同一用户连续几条缓存读一模一样(8,263),与真实上下文(十几万到几十万 token)对不上。
- CLI 形态上游不发 `1.14` 用量帧,客户用量全靠 `estimate_usage_fallback(system, turns, tools)` 估;缓存读由 cache_sim 按完整上下文模拟,但**封顶在估出的输入上**。`turns` 是「实际上传给上游的消息」:折叠重铺时是整段历史折成的一条(估得对);**自造历史时只剩本轮一条**(历史在 blob 里);续轮描述符与挂流续跑时只剩最后一条新消息。
- 线上实测(00:19~00:20,12 条):记账输入只有报文实际大小的 4%~30%(637 条 / 约 30.7 万 token 的请求只记 11,909)。yapi cursor 渠道每请求平均扣费:开关前(18:00~22:09,3873 条)28,538 → 自造历史后(2868 条)9,052,约 1/3。
### 变更
- `chat.rs`:冷启动与挂流续跑两处用量估算改为 `billing_estimate`:客户端完整上下文 `raw_turns` 与实际上传形态 `turns` 两份取大(与直连 API 口径一致:整段 prompt 算输入,命中前缀按缓存读)。自造历史 / 续轮描述符 / 挂流续跑以完整上下文为准;带 PDF 的折叠请求文字层只在 `turns` 里,以上传形态为准(/code-review low 指出)。
- 缓存读比例维持 cursor 原口径(cache_sim 原始前缀命中,封顶到总输入),**不**对齐 kiro 的倍率/保底/上限(线上 kiro floor 0.5 / cap 0.94)。用上线后 177 条真实记录测算,对齐 kiro 反而少收约 24%(重启后冷启动占 20%,50% 保底让利多于 94% 上限多收);是否对齐属定价决策,用户决定不对齐。
### 验证
- 新增单测:同一请求按完整上下文估的输入覆盖历史、本轮一条只有几个 token;自造历史取完整上下文、带文档折叠取上传形态。gw-cursor 537 通过。
### 注意
- 续轮描述符与挂流续跑在自造历史之前就一直少记(记忆「wire 记账 input 低 10 倍」);修复后这两条路径也按完整上下文计费,部分请求的收费会高于 11-09 22:09 之前的水平。
- 图片按占位文字估(与折叠时一致),不计图片 token。

## [cursor-server-summary] - 2026-11-09
### 背景
- 长会话(500+ 条、47 万 token)自造历史后的空回复,诊断版(`cursor-diag-20261109`,23:52 上线)抓到实物帧序,两例完全一致(毫秒从请求开始算):
  `+0 [3] 检查点 21KB; +254 [1]/[3,25]; +259 [1]/[8]; +267 [4]×4(其中一帧被认成「本轮已提交」→ 进排水段); +415 [3] 检查点 19KB; +415 [1]/[10] 4B` —— `1.10` 是 `summary_started`(cursor-agent `agent.v1.InteractionUpdate`),**服务端开始总结压缩上下文**,之后只有心跳,排水闸 20s 掐成空回复。另一会话完整记录到 `+319ms summary_started → +39153ms summary_completed`,总结 39 秒后模型才开口。
- 因果:自造历史把完整的长历史交给服务端 → 服务端判定过长先总结(几十秒)→ 我方把总结前的提交当整轮结束 20s 掐断 → 客户端重试又触发一次总结(白花钱,账单上这类请求缓存读取是上下文的 3~6 倍、输出几千 token)。90s 一刀切版本(已回滚)占满号池,是因为它不认总结帧、也不给下游保活,对「永远不回」的请求同样干等。
### 变更
- `run.rs`:`is_summary_started`(`1.10`)、`is_summary_completed`(`1.11`)。
- `chat.rs`:
  - 排水段里收到总结开始、且本轮还没交付正文也没有工具调用 → 之前的提交是中间步骤,退出排水段(与「模型继续干活」同一套复位)。已交付正文后的总结照常排水收尾,不拖住本轮。
  - 总结开始到完成之间:等待上限从总结开始算 `SUMMARY_MAX`=180s(心跳不续命);总结完成或模型开口即恢复常规闸门。超时按可重试服务错误收口,文案「服务端总结压缩上下文 Ns 仍未完成」。下游续命:总结多在出字前、没有开着的内容块,我方的零增量保活发不出去,靠 gw-app 自己的 20s 静默保活;180s 小于 gw-app 静默硬上限 300s(/code-review low 指出原注释夸大了我方保活)。
  - 总结开始/完成各记 INFO(带耗时),用来调上限。
  - 没有总结帧的情形闸门一律不变(出字前提交仍 20s 收口)。
### 验证
- 新增单测(帧字节照生产诊断日志手写):总结帧识别;提交 → 总结 39s → 出字照常交付;总结 150s(超过普通首字闸 90s)仍等完;总结永不完成 180s 收口为服务错误;无总结帧仍 20s 空回复;出字后的总结按排水闸收尾。gw-cursor 536 通过。
- 诊断版上线 5 分钟的线上统计:服务端总结 17 次、涉及 11 个会话;完成的 10 次耗时 0.2~74.7s(1200+ 条消息的会话 64~75s),180s 是最长实测的 2.4 倍。
### 注意
- 每次自造历史都把完整历史交给服务端,长会话每次重建都会重新总结一遍(费用与延迟;线上同一会话 5 分钟总结 4 次)。挂流续跑与续轮描述符沿用服务端已总结的会话状态,不受影响;重建频率与总结次数要上线后统计再定是否需要进一步处理。

## [cursor-grep-windows] - 2026-11-09
### 背景
- 用户反馈 grep 一直出问题。扫 cursor 请求日志 2323 次 grep 类调用:`shell` 工具失败 330 次,其中 **290 次是 `grep : The term 'grep' is not recognized as the name of a cmdlet`**(另有 4 次是 GBK 乱码的同一句)。调用方是新版 OpenCode 的 Windows 用户(工具说明「Commands run on Windows using powershell」),同一条 `grep -rn -- '三折' 'D:\日常问答'` 被原样重试 4 次以上。客户端自带的 `grep`/`Grep` 工具 199 次只失败 8 次。
- 根因在我方:模型调 Cursor 自带 grep(exec `grep_args`)时,`exec.rs` 直接拼成 POSIX `grep -rn -- '<模式>' '<路径>'` 交给调用方终端;列目录拼 `ls -la`,带工作目录时拼 `cd '<目录>' && …`。PowerShell 没有 grep、`ls -la` 参数不认,5.x 也不认 `&&`。而这些客户端本来就有自己的 grep 工具。
### 变更
- `run.rs`:`BuiltinCall` 新增 `Grep { pattern, path, glob }`、`ListDir { path }`。
- `exec.rs`:grep / 列目录不再在 exec 层拼终端命令,交给 `chat::translate_builtin`。
- `chat.rs`:
  - 内建 grep 优先翻成调用方自己的搜索工具(`GrepTarget`:名字是 grep/grep_search/rg/ripgrep 的工具;`pattern` 必认,路径键 `path` 等,过滤键 Claude Code `glob` / OpenCode `include`)。
  - 没有搜索工具才退化成终端命令,按终端类型拼:终端工具名或说明里有 powershell/pwsh → `Get-ChildItem -LiteralPath '<路径>' -Recurse -File [-Include '<过滤>'] | Select-String -Pattern '<模式>'`;否则照旧 POSIX `grep -rn`。
  - 列目录:PowerShell `Get-ChildItem -Force -LiteralPath '<路径>'`,否则 `ls -la`。
  - 终端带工作目录而工具没有目录参数时:PowerShell 用 `Set-Location -LiteralPath '<目录>'; …`,否则 `cd '<目录>' && …`。
  - PowerShell 字符串用单引号、内部 `'` 写成 `''`。
### 验证
- 新增单测:OpenCode(include)/ Claude Code(glob)两种 schema 翻到客户端搜索工具、无路径不传;无搜索工具时 PowerShell / bash 两种命令(含引号转义)、「Windows using bash」仍按 bash;列目录与切目录按终端类型。gw-cursor 533 通过。
### 代码审查后修正(/code-review low)
- `ps_quote` 同时转义弯单引号 U+2018/2019/201A/201B(PowerShell 也当单引号,否则模式里一个 `’` 能跳出字符串注入命令)。
- PowerShell 判定收窄为工具名就是 powershell/pwsh,或说明里写「using/in/on powershell」;「not PowerShell」「don't use PowerShell syntax」的 bash 客户端不误判。
- 列目录空路径按当前目录。
### 注意
- PowerShell 的 Select-String 用 .NET 正则,与 ripgrep 语法个别细节不同;主流客户端都有自己的搜索工具,退化路径只是兜底。

## [cursor-early-commit-policy-block] - 2026-11-09
### 背景
- `cursor_blob_history` 热开后 32 分钟:cursor 1022 个请求失败 156 个(15%),其中 Anthropic 用量政策拦截 85 次、空回复 58 次。
- **拦截与自造历史无关**:被拦的主要是 3 个会话(改 IEEE TII 论文 LaTeX 修改稿 63 次、RoboMaster 比赛规则手册「打击移动靶」12 次、末条「你好」2 次),内容本身没有安全问题,是上游网络安全分类器误判。RoboMaster 那位用户(yapi 1169 号)在开关打开**之前**(20:19、20:48、21:52、21:59、22:05)就一直被同样拦截。之前「72 小时里全部出现在开关后」的说法是错的:容器 22:08 重建,日志只覆盖开关前 1 分钟。
- 拦截对外只显示「请求无效,请检查请求体后重试」:上游 trailer 的 code 是 `invalid_argument`,按 code 落成 BadRequest。用户不知道是被判了网络安全,OpenCode 当成报文问题自动重试,一个会话 30 分钟被拒 63 次。
- **空回复是自造历史带出来的协议问题**:同口径(yapi cursor 渠道)空回复从开关前 2 小时 3 次(0.16%)涨到开关后 46 分钟 13 次(1.7%)。按会话长度分:<100 条 0/238,100~299 条 4.3%,500 条以上 17.2%;与档位无关。每次空回复耗时都卡在 20.3~20.6s,而账单显示上游照样输出了几百到几千 token。日志:服务端在模型开口**之前**就发 turn_commit(步骤提交)448 次,其中 423 次模型 20s 内开口、排水段的「模型继续干活」分支正常退出;长会话首字超过 20s 时,排水闸(`DRAIN_STALL_TIMEOUT`)先到,收口成「返回 200 但没有任何文本帧」。
### 变更
- `run.rs`:`TrailerError` 新增 `action_required`(`debug.details.analyticsMetadata.actionRequired`)。
- `chat.rs` `trailer_to_error`:title/detail 带用量政策字样或 `actionRequired=*_policy_review` → `ContentFiltered`(类别取 actionRequired 大写,缺失记 `USAGE_POLICY`)。效果:对外 400 带类别与上游原话;不换号、不罚号;进入 worker 拒答记忆(同一请求原样重发不再反复打上游)。HTTP 非 2xx 带结构化体同样生效。
- gw-core `refusal_category_gloss`:新增 `CYBER_POLICY_REVIEW`(说明可能误判、开新会话或压缩上下文通常可继续)与 `USAGE_POLICY` 的中文释义。
- ~~`chat.rs` 帧状态机:出字前、无工具调用的 turn_commit 视为中间步骤,不进排水段,改由首字前常规闸(90s)收口。~~ **已撤回(见下方「上线事故」)**。改为诊断:
  - 排水段停滞且本轮零输出、零工具调用(收尾会报空回复)时,WARN 打出全程帧序(最近 24 个非心跳帧,按流起点算毫秒,`format_trail_since`);
  - 空回复的内部错误信息带上耗时与帧序(只进容器日志「流中上游错误」那行;对外仍是固定文案,请求日志存的是对外错误体,所以重启会丢);
  - 零输出时收到总结(1.9/1.10/1.11)、本轮结束(1.14)、步骤(1.16/1.17)帧记 INFO,每条流每种只记一次(/code-review low 指出步骤帧会刷屏)。
### 上线事故(2026-11-09 23:14 上线 `cursor-earlycommit-20261109`,23:43 回滚)
- 空回复降到 0,但出字前提交之后**一部分请求 90 秒内始终没有任何帧**(caio 47 次「只发心跳」),每次占住账号并发 90 秒;23:32 起号池「组内所有账号并发已满」329 次,yapi 3 分钟 165 次 429、19 个用户。23:43 换回 `cursor-blobhist-20261109`,之后每分钟并发已满为 0。
- 结论:「出字前的提交之后模型只是开口慢」只对了一部分;另一部分上游在提交后长时间不发任何帧,原因未查明。协议里有服务端总结压缩帧(`InteractionUpdate` 10 `summary_started` / 11 `summary_completed`),是首要怀疑对象,但回滚时容器日志随容器删除,帧序证据丢失 —— 所以先上诊断再对症修。延长等待前必须先确认被截断的请求多等是否真会成功。
- `run.rs` / `chat.rs`:Cursor 内建读文件的行范围照传。`agent.v1.ReadToolArgs` 是 `{1 path, 2 offset, 3 limit, 5 include_line_numbers}`(本机 cursor-agent 2026.08.11 定义;offset 从 1 起、负数=从末尾倒数,源码 `Math.max(0, offset-1)`)。旧解析只取路径,模型「从第 440 行往后读」被翻成客户端从头读、截断后又只回前 439 行,生产论文会话末尾连读 3 次。现在 offset>0 / limit>0 时按调用方读工具 schema 里的 `offset`/`limit` 键照传;负 offset 或 schema 没有这两个键时不传(旧行为)。线上 5 种读工具 schema(Claude Code `Read` ×2、OpenCode `read`(filePath)、另两个客户端 `read`)的 offset 说明全是从 1 起;说明写 0-based 的减 1、写按字节的不传 offset(/code-review low 第二轮意见)。负 offset(从末尾倒数)客户端表达不了,仍读整文件。
### 代码审查后修正(/code-review medium,9 条处理 6 条)
- 拦截判定只认 `invalid_argument`,并排在额度降级(`ERROR_RATE_LIMITED_CHANGEABLE`/autoSwitch)之后:封号/鉴权/额度类错误文案里提到用量政策也不会误判成内容拒答(否则死号不罚、留在轮转里)。
- 类别只在 `actionRequired` 是 `*_policy_review` 时外露(大小写、空白不敏感),其余记 `USAGE_POLICY`。
- 对外文案的厂商指纹表加 `cursor`、`anysphere`。
- CYBER_POLICY_REVIEW 释义改为「请勿原样反复重发,确认与网络安全无关时可开新会话重试」(反复重发会让更多号吃到违规记录)。
- 「出字前」判据从 `!started` 改为 `output_chars == 0`:透传思考时 started 已置位,先思考 → 步骤提交 → 慢出字同样会被排水闸掐掉。
- 未改:CLI 形态不发用量帧,中间 commit 之后的用量帧不单独处理;中间 commit 同帧暂存的描述符是缺本轮回复的中间态,丢掉与已有「模型继续干活」分支同口径。
### 验证
- 新增单测:拦截 trailer 实物解析、判成 ContentFiltered 且对外文案带类别/原话/不泄露通道、无类别时退回 USAGE_POLICY、普通 invalid_argument 仍是 BadRequest;出字前提交后流结束仍报空回复。
- 新增单测(审查后):封号类错误提到用量政策不判拒答、`upgrade` 类别不外露、读文件帧解出 offset/limit(含负数补码)、行范围按 schema 照传/负数不传/无键不传。
- `cargo test --workspace` 全部通过(gw-cursor 530、gw-app 686、gw-core 215)。
### 注意
- 拦截文案会带出 Anthropic 的原话(含「Anthropic」字样);通道名(Cursor)不会出现在对外文案里。

## [cursor-blob-history] - 2026-11-09
### 背景
- cursor 线路远比直连 API 贵(用户:「1100 次请求花了 700 多刀,直连 API 1000 次约 250 刀」)。Cursor 计费明细 `GetFilteredUsageEvents` 显示:11-09 16~20 点 5 个号实扣 $2761,按 Anthropic 标准价拆 **87% 是缓存写入**;会话内非首次调用 2363 次整段重写(花 $2471),opus 平均每次写 23 万 token;90 万 token 级会话每次请求约 $10(18 次 $146)。
- 根因:接不上服务端会话时(工具轮之后、描述符缺失、客户端改了历史、带工具块的提问轮走 IDE 形态……)把整段历史**折成一条** user 文本重发。Anthropic 只在消息块边界命中缓存,一个越长越新的大块每轮整段重写。
- Cursor 的会话状态其实托管在客户端:服务端把每条消息做成 JSON blob 经 KV `set_blob` 推来,会话状态 `.1` 只存 blob 的 sha256,下一轮按哈希来要。客户端自造 `.1` 服务端照单全收(OmniRoute 用它放系统提示)。实验号实测:另开新会话、用同样的 blob 重建历史 → 直接读到前一请求的缓存;追加两条 → 服务端只要新增的两个 blob,写 130 token。
### 变更
- 新模块 `blobhist.rs`:按客户端完整历史重建服务端同款消息 blob(AI SDK 形态,照服务端推来的实物):user 包 `<user_query>`;assistant 正文 + 工具调用写成 `CallDynamicTool{namespace:"claude-local", toolName, arguments}`;工具结果写成 `tool-result`(`result` + `experimental_content`,出错带 `isError`)。系统提示不放(服务端自己加,实测不重复);`<user_info>` 用最近一次服务端推来的那份。末条只有工具结果时本轮消息用工具回路提醒 + 原始请求。思考不回传(无签名),回声表认出的思考原文丢弃。
- `chat.rs`:CLI 形态拿不到续轮描述符、需要重铺多轮历史时改走自造会话状态(帧0 `1.1` 放重建的 `.1` 列表);带工具块的普通提问轮在可重建时也走 CLI 形态。续轮描述符与挂流续跑不变。有图片/文档、末条是 assistant 时照旧折叠。
- `wirev2.rs`:WireDriver 带本请求的 blob 表(点名时优先从这里交,不受全局节库淘汰影响);KV `set_blob` 时学习 `<user_info>`。另加排障开关 `CURSOR_DUMP_KV=<目录>`(落盘服务端推来的 blob,含对话原文,生产不设)。
- 热开关 `cursor_blob_history`(gw-core `SystemConfig`/`SystemSettings`,默认关;`/health` 设置指纹回显)。
### 验证(隔离测试 worker + caio 真实会话重放,实验号 cursor-01M2H40TC9P5)
- f693d532(sonnet,11 个请求,约 60KB):自造历史 缓存写 25,821 / $0.10,每请求写 570~830;折叠重铺 58,494 / $0.18,每请求写 4.2K→6.3K 随历史线性增长。两遍 11 个请求全部成功调出工具。
- 自造历史下模型改为经 `CallDynamicTool` 调客户端 MCP 工具(折叠时多调 Cursor 自带工具),这类轮次可以挂流续跑,进一步减少重铺。
### 注意
- 重放中个别请求耗时 3~5 分钟:账单显示是模型那一步输出了 2~3 万 token(大段 Bash 写文件),**不是**并发排队(曾误判为挂起的流占名额,已用计费明细排除)。
- 长会话 71a8db90(opus,上下文 11~13 万 token)自造历史重放:第 7 个请求起每次缓存写 339、$0.02~0.04;折叠重铺同等请求约每次整段重写十几万、$0.7 左右。
- 图片已支持(格式照服务端实物):用户消息里 `{"type":"image","image":{"__type":"Uint8Array","hex"},"mimeType"}`,工具结果里 `experimental_content` 的 `{"type":"image","data":<base64>,"mimeType"}`;本轮带的图片单独一条历史 user 消息。末条带图时不走续轮描述符与挂流续跑(两者都只回传文字,会丢图),一律重建;可重建的带图会话走 CLI 形态。文档(PDF 等)与非 base64 图片仍折叠。新号 sonnet-5.5 三轮实测(左红右蓝图,后两轮各用新会话强制重建):答「红色」「蓝色」正确;缓存写 21654 → 2412 → 108。
- 请求日志里的大附件是 `blob:<sha256>` 引用(去重存 `log_blobs`),回放前要还原,否则图片是假的。
### 代码审查后修正(/code-review medium,10 条全部处理)
- 预判 `eligible` 与 `rebuild` 统一为同一套条件,预判通过必定重建成功:末条没有文字时本轮消息用工具回路提醒,再没有就用占位(只有图片 →「(见上方图片)」);去掉多余的轮数门槛。否则被放进 CLI 形态后退回只带文字的折叠,图片和工具结构会丢。
- **不再放 `<user_info>`**:原实现用一份全进程共享、被任意号/会话/时区推送覆盖的 blob 打头,前缀哈希来回变(缓存失效)且串会话。客户端系统提示里本来就有环境信息。
- 重建的 blob 只放本请求自带的表,不再插全局节库(全局表按条数封顶不按字节,大图会话会撑爆内存、在全局锁下反复扫描)。
- 十六进制改查表编码;base64 去空白并兼容 URL 安全字母表,仍解不开时告警并在原位留占位文字,不再静默丢图。
- 末条工具结果带图时不取挂起的流(续跑只回传文字),并当场释放那条流。
- `CURSOR_DUMP_KV` 只读一次环境变量,写盘失败告警。
- 档位映射只在客户端开了思考(enabled / adaptive)时生效:没开思考时收侧会过滤思考帧,照样抬到 xhigh/max 会让上游长考、客户端久等首字。
- 工具回路提醒长度上限改由 gw-core `CURSOR_TOOL_LOOP_NUDGE_MAX_CHARS` 统一,admin 与 worker 共用。
- 删掉 blobhist 里重复的思考回声过滤(传入的历史已由 `thinking_echo::strip_from_history` 处理)。
- 测试:gw-cursor 519 通过;测试 worker 冒烟(带图三轮、工具往返含工具结果带图)通过。
- 测试:gw-cursor 517 通过(blobhist 6 个用例)。

## [cursor-stop-midtask] - 2026-11-09
### 背景
- 用户反馈:任务做一半就停,模型说完「接下来要做什么」就结束这一轮。生产 30 分钟 656 个带工具的回复里,宣布要动手、没在问用户、却 end_turn 的有 3~5 条(<1%,但长任务几百轮,落到一个任务上就是好几次)。典型样本 6391176:工具结果轮、会话带图走 IDE 形态折叠重铺,回复一段「接下来我先改 `gen_dataset.py`……」后收尾,上游无任何工具调用。另一条 6392298 是 Claude Code 的「离开回顾」请求,正常。
- 根因(推断,措辞与样本对得上):折叠重铺时附在工具结果后的提醒写着「还缺信息时才调别的工具;信息够了就直接给出最终回答」,把工具说成只用来查信息;编程任务的下一步多半是动手,模型于是把计划当最终回答写完就收尾。OmniRoute 针对同一现象用「Do NOT describe what you are about to do as prose and then stop — call the tool」,实测工具调用率 53%→88%。
### 变更
- 提醒改写(`DEFAULT_TOOL_LOOP_NUDGE`):下一步要读、改、运行就直接调对应工具,不要只把打算写成文字就结束;只有任务完成或须用户决定/操作时才用文字收尾;保留「不要用相同参数重复调用已返回结果的工具」(治 grok 连读)。
- 做成热配置 `cursor_tool_loop_nudge`(gw-core `SystemConfig`/`SystemSettings`,写侧限 1000 字;空=内置默认;过长保留上一份有效值)。配置只管正文,前置空行与「用户的请求是:」由代码补。`/health` 设置指纹新增 `cursor_loop_nudge_rev`。
- 热配置护栏 `cursor_tool_guard` 于 19:39 追加一句「说了下一步要做什么就在同一条回复里直接调工具,不要只写计划就结束;只有任务完成或须用户决定/操作时才收尾」(不随本版本,guard_rev 0989ad80→3f683eed;旧值备份 139 `/root/kiro-fix/cursor-tool-guard-before-stopfix-1794224325.json`)。
### 测试
- gw-cursor 511 通过(新增提醒措辞约束 1 个、热覆盖 1 个;两个依赖提醒原文的旧用例改为加锁并更新断言)。

## [cursor-effort-tier] - 2026-11-09
### 背景
- 用户反馈 cursor 线路「过思考」。Claude Code 只发 `thinking:{type:adaptive}` + `output_config.effort`(不发 `budget_tokens`),而 `apply_thinking_pref` 只认 `budget_tokens`,于是所有请求都落在目录默认的 `high`:近 30 分钟 sonnet-5.5 要 medium 的 185 个、haiku-5.5 要 medium 的 44 个被抬到 high(多想);opus-5.5 要 xhigh 的 173 个、要 max 的 23 个被压到 high(少想)。实时目录里裸名 `claude-opus-5-5` 在 Cursor 自己那边默认是 medium。参考实现 OmniRoute 也按 `output_config.effort` 发档位。
### 变更
- `models::apply_effort_pref`:按客户端 `output_config.effort`(low/medium/high/xhigh/max)改写模型的 `effort` / `reasoning_effort` 参数;客户明确关思考、取值不认识、模型未核实分档时一律不动;要的档模型没有时取不高于它的最高可用档。
- 已核实分档的模型:opus-5.5、fable-5.1、opus-4.8(实时 `AvailableModels` 有五档变体)与 sonnet-5.5、haiku-5.5(线协议逐档实测 200 出字)。
### 调查结论(不改代码,记录在此免得重查)
- **跨轮思考 Cursor 上游不保留**:「心里想一个数,下一轮问」测试中第二轮走的是 Cursor 原生描述符续轮(描述符引用数 4→6,无重铺、无缺块),模型仍说看不到上一轮推理。签名其实在:每轮结束服务端经 KV `set_blob` 把完整 assistant 消息(`reasoning` + 928 字符签名 + `anthropicNativeContent` 原生 thinking 块)推给客户端保存;但服务端拼下一轮上下文时不放上一轮思考。
- `UserMessageAction.7 conversation_history`(cursor-agent 隐藏参数 `--conversation-history-file` 的通道)对普通账号无效:带或不带 reasoning,模型都看不到这段历史。
- 把思考原文折回 user 文本仍会被 Anthropic 判推理提取(11-07 实证),所以 cursor 线路没有让模型看到上一轮思考的可行通道;同轮工具回路内的思考由挂流续跑保住。
- 探针 `examples/probe_wire_v2.rs` 新增 `kvdump`(落盘服务端托管的 KV blob,`PROBE_DUMP_KV=<目录>`)与 `guess2`(首轮 + 描述符续轮猜数字)两个模式,`PROBE_PROXY` 指定出口。
### 测试
- gw-cursor 509 通过(新增档位映射 2 个);全工作区通过。

## [cursor-hold-tool-streams] - 2026-11-09
### 背景
- 每次工具调用后都整段重发历史(日志「影子表无描述符,降级重铺」,部署 cursor-stall 后 40 分钟 292 次):Cursor `AgentService/Run` 是双向流,模型调 MCP 工具时服务端发 exec `mcp_args` 后**停在原流上等 `McpResult`**;我们交出 `tool_use` 时把上游流一起丢了,下一轮只能新开 Run、把全部历史折叠重发。工具轮停在等结果,永远拿不到续轮描述符,所以「工具轮续轮描述符」机制在生产上从未命中。
- 参考 OmniRoute(MIT)`open-sse/services/cursorSessionManager.ts`:工具轮不关流,下一请求按工具调用 id 取回,在原流上回 `encodeExecMcpResult`,接着读。
### 变更
- 新模块 `gw-cursor/src/held.rs`:挂起表(按账号存;取用要求「带回的结果 id 集合 == 挂起时等待的集合」,取出即摘掉;闲置 5 分钟回收,每号 8 条、全局 100 条)、MCP 结果编码 `{2:{1:id,15:exec_id,11:{1:{1:[{1:{1:text}}],2:is_error}}}}`(文本上限 38000 字节按行截断)、`tool_results`(带 id / is_error,同条文字附到最后一个结果)、客户端心跳守卫(挂起期间继续每 5 秒发 `{7:}`)。
- 收帧循环(`stream_to_anthropic_with`,原 `stream_to_anthropic` 改为默认选项的包装):开关开、CLI 形态、请求流保持打开时,收集每个工具调用对应的 exec 编号(`pair_pending_execs`:只认 id 相等或「单调用单 exec 帧」;并行调用最多再等 3 秒收齐,收不齐不挂),收齐后在发 `message_stop` **之前**登记挂起;客户端没收到 stop(已断开)就按凭证摘掉。续跑段忽略上游对已回结果调用的回显,不重复下发。
- `chat_stream` 入口:工具结果轮按 id 取挂起的流,逐个回 `McpResult` 后接着读;取不到或回传失败走原冷启动重铺。
- 热开关 `cursor_hold_tool_streams`(gw-core `SystemConfig` / `SystemSettings`,默认关;关掉立即释放全部挂起的流);`/health` 的设置指纹里报 `cursor_hold_tool_streams` 与 `cursor_held_runs`。
### 设计取舍
- 续跑段报给下游的用量与冷启动同口径(请求体估输入 + 模拟缓存),不用上游 `1.14`:它可能是整个 Run 的累计数,直接用会重复计费。客户账单不变,只是我方上游成本下降。
- 第一步只挂纯 MCP 工具调用轮;混有被翻译成客户端工具的 Cursor 自带工具的轮次照旧冷启动。自带工具的「带类型成功回执」桥接(自带局部编辑先读原文件被拒、模型反复重试的根治)放第二步。
- 续跑时上游若已超时断流:出第一个字前报可重试错误,gw-app 重试时挂起的流已被取走 → 自然冷启动。
### 注意
- 线上 router 是旧版,不认 `cursor_hold_tool_streams`:打开时直接写 control.db `settings[system]`(同 deploy.sh 写 `cursor_default_driver` 的做法),worker 30 秒内应用。
- 测试:gw-cursor 507 通过(新增挂起表 6 个、挂起/续跑流程 3 个、配对 1 个)。

## [cursor-tool-stall] - 2026-11-09
### 背景
- 11-09 用户在 OpenCode 里用 opus-5.5 写整页 HTML,连续 5 次卡住(388s / 288s / 71s / 44s 后被打断,另有一次 445s 被判「只有心跳」)。模型说完「现在写文件」后客户端再无输出。
- 原因(日志与代码核对):① 模型用 Cursor 自带写文件 / 局部编辑生成整份文件,期间上游只发 `1.8` token 计数,不发参数文本,150s 被停滞闸误杀;② 自带局部编辑我们执行不了,拒绝后模型改口重来,整份文件再生成一遍;③ 替代表把「写/改文件」只指向一个工具(OpenCode 里是 `write`),改几行也整文件重写;④ 热配置护栏里「静默无响应、卡死、重做一遍」的说法让模型反复怀疑工具失效。用户实测:明确提示「调用 OpenCode 的 write」后一次成功。
### 变更
- `run::is_token_delta`:token 数 > 0 的 `1.8` 帧算「干活」——续停滞闸、给下游交空增量保活,总时长仍受 `WORK_ONLY_CAP`(600s)封顶;token 数为 0 的不算。
- 护栏替代表:调用方同时声明整写与局部编辑工具时拆成两行(「新建或整体重写文件调用 `write`」「局部修改已有文件调用 `edit`(只给要替换的原文和新内容)」);只有一种时维持原合并行。两种护栏形态共用 `render_redirects`,别名匹配抽成 `best_alias_match`。
- `client_env_note`:用 `gw_core::clients::recognize` 认出客户端(OpenCode / Claude Code / Grok Build / Copilot / DeepSeek Harness / pi)时,在护栏前点明「你运行在用户本机的 X 里,工具由 X 在用户机器上执行」;认不出不写。
- 热配置 `cursor_tool_guard`(11-09 16:56 已热改,不随本版本):删掉「静默卡死、重做」说法,改成「局部改用编辑工具、超过约 150 行先写骨架再分块补、不需要先测试工具」。旧值备份 139 `/root/kiro-fix/cursor-tool-guard-before-20261109.json`。
### 设计取舍
- `1.8` 原先刻意不算进展(旧记录说真挂死时它也在变)。现在算「干活」而非「进展」,仍有 600s 封顶:最坏情况是真挂死多等到 600s,换来大文件生成不被 150s 误杀。
- 内部能力键「写/改文件」不拆:内建写文件翻译、编辑工具查找、「上一轮中断」提示都依赖它,只改给模型看的渲染。
### 注意
- 工具回路仍是每轮整段重发历史(日志「影子表无描述符,降级重铺」),挂流续跑另做。
- 测试:gw-cursor 497 通过(新增 5 个:替代表拆分、客户端环境说明、token 计数帧不误杀 / 受 600s 封顶 / 零计数不算)。

## [pool-breaker] - 2026-11-09
### 背景
- 封号是成片来的:11-08 20:30 一分钟内封了 7 个,10-29 一天 27 个。元凶通常是某个新出现的批量负载在号池里换着号打,等人看到时已经死了一片。
- 用户要求:短时间内封号超过 2 个就把整个号池熔断,暂停接受所有请求;掉 FREE 也算封号。口径(用户 11-09 定):30 分钟窗口、第 3 个号出事就熔断、只能手动恢复、每月 1~2 日(北京时间)掉 FREE 不算(月初订阅到期会批量掉档)。
### 变更
- `worker/pool_breaker.rs`(新):熔断状态机 + 进程级实例。
  - 出事 = 首次被封(调度器 `TemporarilyBlocked` 且封禁连击 0→1;复活后又被封不重复计)或**在役号**的付费档掉成 FREE(额度轮询写回档位时对比旧值;已停用 / 冷却 / 被封过的号掉档不计,避免同一个死号算两次,也避免停用号的月初掉档拖到 3 号才被看到)。同一个号在窗口内只计一次。令牌失效、额度用完、限流不计。
  - 熔断先在内存生效(下一条请求就挡),热设置同步循环每 30 秒与库对账。库里分两个键:`pool_breaker_state` 只由 worker 写(熔断时间 + 出事的号),`pool_breaker_reset` 只由 router 写(恢复时间)。worker 落库并发通知 / 重启后从库里接过熔断 / 看到恢复时间后解除并清空窗口;多个 Kiro worker 的事件取并集,不互相覆盖。
  - 只在跑 Kiro 号池且有控制面库的 worker 上启用(`suspend_lifecycle` 同口径);cursor / dario worker 共用 settings 表也不会被连带停掉。
- 入口闸门:自动模式分类器之后(分类器本地直答不占号,不拦)、选号之前,熔断中回 503(下游 new-api 换渠道);在途请求不受影响。被挡的请求**不写请求日志**,只在 `/health` 计数 —— 请求日志只留 2000 行,全量 503 几分钟就会把熔断前的日志冲掉,而查元凶靠的就是那批日志。
- 补货:熔断中暂停自动买号(号池停着,买来的号只会闲着死掉);面板上手动「立即补一个」照常。
- 通知:复用补货的 `notify_url`(企业微信 / 钉钉 / 飞书 / Slack / 通用 JSON),没填就只有日志、`/health` 和后台横幅。
- 管理面:`GET /admin/api/pool-breaker`(库 + 各 worker `/health` 合成:熔断还没落库或写库失败时也能看到)、`POST /admin/api/pool-breaker/reset`(只写恢复时间,不要求库里先有熔断);worker `/health` 新增 `pool_breaker` 快照。
- 热设置 `pool_breaker_mode`(off / observe / enforce,**默认 enforce**)、`pool_breaker_window_secs`(默认 1800)、`pool_breaker_threshold`(默认 3),写侧与 worker 读侧同一套校验。
- 后台:每个页面顶部的红色熔断横幅(出事的号、时间、「恢复接单」按钮,二次确认);设置页新增「号池熔断」卡片。
### 设计取舍
- 默认 enforce 而不是先 observe:用户明确要求;用 10-02~11-09 的请求追踪回放(实现本身跑 `replay_real_trace`),每一波封号潮第一次熔断的时间点都对得上(10-02、10-03、10-07、10-14、10-22、10-23、10-27、10-29 三波、11-01、11-06、11-08 20:30、11-09 09:52),约 3 天一次,没有落在平静期的。
- 只能手动恢复:熔断的意义是让人先查清是哪个负载在封号;自动恢复的话原因还在,恢复后接着封。worker 重启也从库里接过熔断,不会变成「重启即恢复」。
- 503 而不是 400:400 不换渠道,客户只能干等;503 让 new-api 把流量转去别的渠道,且不触发自动禁用渠道(那是 401)。
- 切到 off 是应急口:熔断中切 off 立即放行,切回 enforce 仍是熔断状态,必须手动恢复。
### 注意
- FREE 号被封同样计数(11-09 09:52 那次就是 3 个 FREE 号)。
- 掉 FREE 靠额度轮询发现(每号约 4 分钟一次),所以掉档事件最多晚几分钟进窗口;历史上没有掉档的时间记录,回放只覆盖了封号。
- 恢复最多 30 秒生效(worker 下一轮同步)。补货只看库,熔断后最多 30 秒内仍可能买号。
- 计数窗口在 worker 进程内存里:现在只有 worker0 一个 Kiro worker;以后拆成多个时要把窗口挪进库。重启会丢掉还没到阈值的计数。
- 审查(`/code-review high`)10 条:修了 9 条(恢复按钮二次熔断后卡死、被挡请求冲掉日志、死号重复计数、停用号月初掉档误算、worker 回写覆盖管理员恢复、多 worker 事件互相覆盖、熔断中补货照买、没落库时后台看不到也恢复不了、分类器直答被拦);「每个进程各自计数」现阶段只有一个 Kiro worker,记在上一条。
- 测试:新增状态机 14 个(含真 SQLite 对账、坏文档不放行、两个 worker 合并)、配置 2 个、admin 接口 3 个、设置写侧 1 个、前端 2 个;`cargo test --workspace` 2305 通过、0 失败,前端构建与 125 个测试通过。

## [kiro-responses] - 2026-11-09
### 背景
- Kiro IDE 1.4.121(2026-11-05 发布)被服务端分到 `create_response` 实验组时,主对话不再走 GenerateAssistantResponse,改走 `POST runtime.{region}.kiro.dev/v1/responses`:请求体是 OpenAI Responses 形态(`input` / `function_call` / `reasoning`),响应是 SSE。2026-11-08 拆包 + 两个号真机抓包确认(opus-5.5 / sonnet-5.5 均 200)。
- 是否走新协议由服务端按账号/Profile 下发的实验组决定(`GetFeatureConfiguration`),IDE 与 CLI 3.1.0 都看实验组,不是固定走新协议。
- 先前另一条线(Kimi,工作区 `claude-all-in-one` 分支 `feat/fanout-guard`)写过一版,但基线落后线上 12 个提交、全进程一个开关、冒充 Kiro CLI;本版在线上基线上重写,吸收了那一版的转换与流解析骨架。
### 变更
- `extra.kiro_endpoint` 新增取值 `responses`(`endpoint.rs`):runtime 域名 + `/v1/responses`。按账号热改,不写就是原样。
- `responses.rs`:KiroRequest → `/v1/responses` 请求体,逐条对齐客户端 `PUs` / `PEr` / `kUs` / `REr` / `RUs` / `OUs`:
  - 顶层键序 `model,input,tools,stream,origin,profileArn,metadata,reasoning`;`origin: AI_EDITOR`;不写 `instructions`(system 照旧作为 user + "I will follow these instructions." 铺在 input,与抓包一致)。
  - 工具结果 → `function_call_output`(text 块单换行拼接);附件 → `input_image` / `input_file` data URL;只有文本时 content 是字符串。
  - 历史思考 → `reasoning` item,`encrypted_content` 为 JSON 字符串信封 `{"reasoningText":{text,signature}}`;没有签名或没有文本的思考不回放(客户端同样过滤)。
  - `metadata`:`kiro:agent_mode` / `kiro:mode_identifier` = vibe,会话 ID 加 `sess_` 前缀并规整成标准 v4 UUID(caio 派生值的版本位/变体位是随机的,只改这 6 位、其余原样,同一会话仍恒等,缓存不丢;旧协议的号不动),root = 会话 ID,`kiro:agent_continuation_id` 每轮新生成、同一请求内的验签重试复用。
  - 思考强度走 `reasoning:{type,effort,summary}`(由 additionalModelRequestFields 映射,缺省字段省略)。
- `stream.rs`:两种上游传输归一成同一种事件帧。旧协议的 eventstream 解码从 chat 里原样搬出;新协议按客户端 `GUs` 把 SSE 翻成旧事件(文本、思考与签名、拒答、工具调用开始/增量/结束、completed/incomplete、上下文占比、积分),chat 的 Anthropic 状态机一行逻辑没改,只把「读字节→解帧」换成消费事件流。
- `error_map.rs`:`classify_responses_error` / `classify_responses_stream_error`,照客户端 `$Us` 由 `error.type/code` 定状态码,再交给原有分类 —— 封号、额度、限流、模型不可用、媒体上限提示都沿用旧策略。流中途的 `error` / `response.failed` 也按此分类,不再一律当 ServerError。
- `headers.rs`:`apply_responses_headers`,头集合与顺序对齐 1.4.121 抓包(`Content-Type, Authorization, x-amzn-kiro-origin, optout, x-amzn-kiro-profile, [TokenType], User-Agent, x-amzn-kiro-attempt, x-amzn-kiro-fallback, Host, Connection`),UA `KiroIDE 1.4.121 {machineId} KAS/0.66.89 acp-client/kiro-ide`。
- 整号对齐 1.4.121:`kiro_version` 对 `responses` 号默认返回 1.4.121,刷新、查额度、profile 列表的 UA 跟着变;UA 里的 Node 版本随客户端版本走(1.4.121 → 24.22.1,其余不变)。查额度改用 1.4.121 的形态(management 域名 + `/getUsageLimits` + `resourceType` + runtime UA + `max=3`)。
- 旧协议正文里的思考标签对 `responses` 号关闭:converter 新增 `convert_request_for(req, scope, ConvertWire)`,`convert_request` 保持原签名(= `ConvertWire::Kiro`),报文形态显式传到 `build_history` / `apply_thinking_prefix_to_current_turn`。
- 毒报文备忘录的指纹去掉每轮随机的续接 ID,否则同一份坏报文换号重发永远对不上。
- 查额度参数顺序按 1.4.121 抓包(`origin, profileArn, resourceType`);其它形态顺序不变。
- env 整串覆盖(`KIRO_UPSTREAM_BASE_URL` 等)存在时,`responses` 号的对话退回 REST,刷新/查额度也不再自报 1.4.121(同口径)。
- 后台:Kiro 号编辑弹窗「上游端点」新增「新协议（1.4.121）」,列表加「新协议」徽章。
### 设计取舍
- 只模拟 IDE、不模拟 CLI:我们的号都是 IDE 社交登录签发的 token,刷新也是 IDE 形态;冒充 CLI 会让同一个号刷新像 IDE、对话像 CLI。IDE 与 CLI 在新协议下请求体字段完全一样,差别只在请求头。
- 按账号而不是全局开关:真客户端是按账号分组的,全局一开就是全池同时换指纹;按账号可以先挑几个号灰度。复用已有的 `kiro_endpoint`,管理面写入校验、换端点清「模型不可用」标记都自动生效。
- `optout` 沿用旧协议的 `true`:真客户端按「内容收集」设置取值(抓包机是 false);同一个用户升级客户端不会顺手改隐私设置,同号前后一致更重要。
- `kiro:model_requires_user_opt_in` 归为模型不可用而不是客户端的 403,避免落进「403 = token 失效 → 刷新换号」。
### 注意
- 真机证据只覆盖纯文本回答(抓包里没有思考输出、工具调用、图片/文档、工具结果回传);这些形状来自客户端代码,上线前要用少量号按真实客户端报文实测。
- 并行工具调用的参数增量按「当前调用」归属(客户端也是这样),服务端若交错发多个调用的增量会归错;客户端同样有此问题,未改。
- 没改:模型目录(1.4.121 是 `ListAvailableModels` RPC,caio 仍是 1.0.212 的 GET;`responses` 号手动拉目录时 UA 自报 1.4.121、形态却是 1.0.212,只在后台手动拉目录时出现)、`GetFeatureConfiguration` / 会话标题这类客户端附带请求;HTTP 版本(caio 开着 HTTP/2,真客户端这几类请求都是 HTTP/1.1,全池原有差异)。
- 流解析两处比客户端多做的兜底(只影响下行,不改上游报文):`function_call_arguments.done` 前一个增量都没有时,用事件自带的完整 arguments 补上(客户端会发出空参数调用);标准 `error` 事件把 code/message 放顶层时也能分类。解析失败的 SSE 行记 warn。
- 审查(`/code-review high`)10 条意见全部处理(含把「线程内作用域」改成显式参数);只剩模型目录半条没改,见「注意」。
- 测试:新增 responses 6 个、stream 11 个、error_map 4 个、headers 2 个、usage_limits 1 个、history 1 个、chat 整链 4 个、endpoint 1 个;`cargo test --workspace` 2287 通过、0 失败,前端构建与 123 个测试通过。

## [cursor-no-warmup] - 2026-11-07
### Features
- 调度器暖机限速对 provider=cursor 的号一律不生效(`CredentialState::warmup_cap`),不再依赖 `warmup_group_policies` 逐组写 `hours=0`。
### Design Rationale
- 用户要求:新号暖机不用在 cursor 上。暖机是给 kiro 补货新号防封设计的;线上 CUR 组靠热设置 `hours=0` 豁免、BOT 组靠全局开关关着,新开 cursor 组或有人打开全局开关就会漏。
### Notes & Caveats
- 当前线上本来就没有对 cursor 生效(全局关 + CUR 显式 0),本改动随下次 worker-cursor 部署生效,行为不变。新增测试 `warmup_never_applies_to_cursor_accounts`。

## [cursor-wire-edit-fragment] - 2026-11-07
### Features
- 界面帧 `1.2.2.12`(agent.v1 `edit_tool_call`)不再翻成调用方的整文件写工具,只记下目标路径(`run::edit_call_path`)。
- 随后对同一路径的 exec `read_args` = 内建 StrReplace 在取原文件:回带类型的读拒绝,理由点名调用方自己的编辑工具(OpenCode `edit` / Claude Code `Edit`,IDE 形态带 `gwtools-` 前缀),流继续读,模型在同一轮改口。
- 内建 Write 仍然可用:落盘内容取自随后的 exec `write_args.file_text`(服务端给的完整内容)。
### Design Rationale
- 2026-11-07 15:21 起生产事故:OpenCode 用户同一会话里三次「整个文件被盖成 5 行」。`EditArgs` 只有 `{1 path, 6 stream_content}`,模型调 Write 时是全文、调 StrReplace 时只是替换后的片段,两者同一帧形分不出来;旧代码一律当整文件写。约 25 分钟日志窗口里 16 次写翻译至少 6~7 次是片段。
- 真号重放(testa,原样客户报文)抓帧确认流程:`.12`(片段)→ exec `read_args`(同路径)→ 拒绝 → `1.3` 完成帧带回拒绝理由 → 模型改用 MCP 工具。
- 我们拿不到客户磁盘上的原文件,读只能拒绝;不把读翻成客户端 `read`(那是模型没发起的调用,会把它绕进循环)。
### Notes & Caveats
- exec 写的 tool_use id 是合成的 `toolu_cx…`(exec 帧不带模型侧 id),工具结果下一轮折进历史,不需要对号。
- 流没有保持打开(无法回拒绝)时,`.12` 按原收口 + 纠偏走,不会再写文件。
- 测试:gw-cursor 492 通过(新增 4 条:编辑片段不翻写、读拒绝并点名编辑工具、exec 整写照常交付、编辑工具名认取)。

## [cursor-wire-step-commit] - 2026-11-07

### 背景
- testa 上一条请求被客户端重试 5 次,每次约 50s 收到「模型未对该请求产出任何内容」。curtest 重放 + 全帧落盘查明:服务端对 MCP 工具做**延迟加载** —— 模型首次调 `run_code` 时服务端自己回工具定义(`1.3` 完成,结果是 `{"tool":…,"inputSchema":…}`)并写一条**步骤**提交;模型读完接着思考、再拼一次正式调用。
- 那条步骤提交被 `is_turn_commit` 认成整轮结束,进入排水段;之后的思考与长时间参数组装全被吞,20s 排水停滞后空回复收口。

### Features
- 排水段见到正文 / 思考 / 工具参数组装帧(且本轮没交出过工具调用、不在等工具轮 commit)即判定先前的 commit 是中间步骤,退出排水段恢复正常处理(与既有「排水段遇到工具调用帧」同一条规则的扩展)。
- 调试开关 `CURSOR_DUMP_FRAMES=<目录>`:按序落盘每个非心跳帧(生产不设)。

### Notes & Caveats
- 测试:gw-cursor 488 通过(新增「步骤提交后模型继续思考出字不被排水段吞掉」);curtest 重放生产请求:退出排水段后交付 `run_code` tool_use(生产同请求 5 次空回复)。

## [cursor-wire-ide-mcp-state] - 2026-11-07

### 背景
- 07e 上线后剩下的卡顿全是 `mcp_auth` 请示:IDE 形态(带图、或历史有工具块的新用户轮)下服务端问 `gwtools` 有哪些工具,我们回「ready + 0 个工具」,模型以为该服务没授权,调 Cursor 的 mcp_auth,服务端请示后干等,150s 收口。生产同一条请求被客户端重试 4 次全失败,Leonia 号因连续失败被自动禁用。
- 根因是 11-06 补握手应答时把 IDE 形态的工具清单写死为空(`ide_kv_upload` 的 `mcp_tools: Vec::new()`)。

### Features
- IDE 形态握手按帧0 注册的同一份 `McpToolDefinition` 报回工具(全名 `gwtools-<名>`、provider `gwtools`、schema JSON);服务端没点名时 IDE 形态默认答 `gwtools`、CLI 形态答 `claude-local`。
- `mcp_auth` 请示(11)回拒绝,理由写明「不需要授权、直接调工具」,作兜底。

### Design Rationale
- 帧0 的 IDE 工具注册本身就是 `agent.v1.McpToolDefinition`(字段 1/2/4/5/6),握手应答直接复用 `ToolDef::encode`,两处字节一致。

### Notes & Caveats
- request_logs 存档会把图片截成 69 字符占位,原样重放会走成 CLI 形态;复现 IDE 形态要换回真实图片。
- 测试:gw-cursor 487 通过;curtest 重放生产失败请求(换回 10 张小图)握手 `gwtools tools=34`、7s 交付 Bash 调用(生产同请求 4 次 170s 失败)。

## [cursor-wire-interaction-query] - 2026-11-07

### 背景
- 07d 上线后 CUR 组 411 次请求失败 44 次,其中 15 次是「600s 内只有工具参数/计数增量」收口。trail 末尾都是顶层字段 7:服务端的请示 `interaction_query`(联网搜索、抓网页等先请示客户端批准),我们不认识、从不回,服务端只发心跳干等。
- 每次卡满 600s(07c 为长文件组装放宽的上限)、占一个并发名额,CUR 组 12:08 起「并发已满」约 955 次。

### Features
- 新模块 `gw-cursor/src/interaction.rs`:识别 `AgentServerMessage.7`,按 `AgentClientMessage.6 interaction_response` 应答。联网搜索(2)/抓网页(9)批准,由 Cursor 服务端代执行;提问(3)/切模式(4)/建计划(7)回拒绝;其余种类不答。
- 收帧循环在 exec 握手之后处理请示:任何状态下都应答;请示到达即作废组装计时,答不了的按 150s 停滞闸收口,不再耗满 600s。

### Design Rationale
- 答法照 OmniRoute `encodeInteractionResponseFrame`,字段号逐个对照 cursor-agent 打包的 agent.v1(InteractionQuery/Response、AskQuestionResult、SwitchModeRequestResponse、CreatePlanResult)。
- 搜索/抓网页选批准而不是拒绝:托管执行结果直接回模型,比让模型改调客户端工具少一轮往返。

### Notes & Caveats
- 抓网页批准后服务端会推 exec 写盘(把结果存到 `~/.cursor/projects/agent-tools/*.txt`),仍按内建写盘回拒绝,模型照常继续(实测不影响结果)。
- 测试:gw-cursor 485 通过(新增 6);curtest 真号 sonnet 联网查询 15s 完成(两次请示均应答);fable 端到端 7/7。
- 部署:worker-cursor=`cursor-wire-20261107e`(sha cf108dcd91208689,13:12 +08),回滚改 compose 回 `cursor-wire-20261107d`。

## [cursor-wire-stall-fix] - 2026-11-07
### 背景
- 协议模式上线后,真实客户 70 分钟内出现三类问题:出字后 150 秒只剩心跳判失败(fable 7 次)、模型调内建 glob/grep 时回答被截断(5 次)、带图会话的图片存不进内存资产库(37 张)。
- 曾热切回 CLI 模式止血,用户纠正:CLI 有问题才做协议模式,要按报错修协议模式。同口径对比 CLI 并不更好(fable 失败率 CLI 5.1%、协议 5.8%,CLI 那边是 360 秒杀进程)。
### 变更
- **工具调用组装期不判停滞**:curtest 复现生产失败(fable 一次写入长文):思考结束 → `1.7 partial_tool_call` → 一串 `1.8` → **之后只剩心跳**直到整个工具调用拼完。新增「组装中」状态(`run::is_tool_assembly`),停滞上限从 150 秒放宽到组装开始后 600 秒;期间每 15 秒给 gw-app 交 `ping`,续它的 300 秒静默计时。
- **干活帧算进展**:按 agent.v1 `InteractionUpdate` 定义,`1.7/1.8/1.9~1.12/1.15/1.16/1.17` 是上游在干活(`run::is_work_update`),续停滞闸;只有干活帧、没有可见输出的阶段封顶 600 秒。
- **停滞日志带帧轨迹**:收口时把最后一次进展之后到达的非心跳帧(字段号、字节数)和 conversation_id 打进 warn,下次不用再猜。
- **IDE 形态的内建工具也先等 exec 帧再回拒绝**:原先只有 CLI 形态这么做,IDE 形态(带图/PDF、历史里有工具块)直接截断。等 20 秒没等到 exec 帧退回原收口。
- **内建子代理翻成调用方的子代理工具**:07c 上线后 11 分钟 105 次请求 0 失败,但仍有 2 次截断 —— 模型调了 Cursor 自带的子代理(exec `28 subagent_args`:1 tool_call_id、2 subagent_type、4 prompt)。现在翻成 OpenCode `task` / Claude Code `Task`(类型取 schema 写明的 general 类);调用方没有子代理工具就回 `SubagentResult.error` 拒绝,模型换办法。
- **读 `/assets/` 附件不再翻成客户端 Read**(审查发现的线上老 bug):那是服务端推来的附件,客户端磁盘上没有,翻过去模型看不到图;留给 exec 读帧从资产库回图。
- **资产库会话内淘汰最旧文件**:客户端每轮重传附件、服务端每次给新 uuid 路径,旧副本占满 24MB 会话额度后新图被拒。现在装满先淘汰本会话最早写入的文件。
### Design Rationale
- 组装期只能靠上限兜底:上游不发进度,无法区分「在拼」和「挂了」。600 秒覆盖长文写入;gw-app 的 300 秒静默闸靠 ping 续命,所以真挂死的组装最长 600 秒收口。
- 收帧循环的计时从 `std::time::Instant` 换成 `tokio::time::Instant`(接口相同,生产行为不变),停滞类测试才能用暂停时钟跑几百秒。
### Notes & Caveats
- 模型调了未注册的工具名时同样会先发 `1.7` 再只剩心跳,这类「必死」请求现在要 600 秒才收口(原来 150 秒)。
- 保活用空增量帧(与 gw-app `keepalive_frame` 同形)而不是 `ping`:下游 NewAPI 会吞掉 `ping`,且它会重置 gw-app 的静默计时、压掉 gw-app 自己的保活。没有开着的块时不发。
- `1.8 token_delta` 刻意不算进展(旧排障记录:卡住时它也在变)。
- 测试:gw-cursor 479、gw-app 676、gw-core 215 全过;curtest fable 6/6 + 回归 3/3。
- 上线:07c(02:23 UTC)→ 07d(02:43 UTC,加子代理翻译)。

## [cursor-wire-default] - 2026-11-07
### 背景
- cursor 带工具的请求原先默认走 CLI 驱动(每会话一个 cursor-agent 进程,约 620MB、冷启动约 7 秒、工作区串味),用户反馈很难用。协议模式(线协议 CLI 形态)从 10-03 起带工具请求 0 交付。
- 参考 OmniRoute 3.8.53(9-29 修好工具调用),11-06 在 testa 上对照实测后补全协议,并把协议模式设为默认。
### 变更
- **exec 握手**(新文件 `gw-cursor/src/exec.rs`):应答服务端 `36 mcp_state`(回工具清单 + ready)与 `17 list_mcp_resources`;旧代码把这类帧当成内建工具收口,是 0 交付的根因之一。
- **RequestContext 补 `.7` 工具清单与 `.34` MCP 描述符**:缺了模型会认为工具命名空间不存在、改调内建工具。
- **新工具帧**:`1.2.2.44`(MCP 调用开始,不带参数)不再被当成内建工具;收到 exec `11 mcp_args` 即收工,不再干等 turn_commit(单次工具调用 96 秒 → 6 秒)。
- **模型参数名**:sonnet/opus/haiku 5.5、grok-4.7 按 `AvailableModels` 实时目录改正(写错上游回 `ERROR_BAD_MODEL_NAME`)。
- **内建工具**:能映射到客户端同类工具的就映射,映射不了的(删除、无工具请求等)回「不可用」,模型换办法继续,不再截断。
  - 字段号按 cursor-agent 打包的 `agent.v1` 定义更正:`1.2.2.4` 是 glob(目录 + 模式),`1.2.2.8` 才是读文件。旧代码把 `.4` 当读文件,把通配模式当路径交给客户端的读工具(「File not found: **/report.pdf」)。
  - glob 只翻译到名字就是 glob 类的客户端工具(opencode `glob`、Claude Code `Glob`),参数键从其 schema 认;没有就不翻。
  - 终端调用带上 `ShellArgs.working_directory`:客户端工具有目录参数(opencode `workdir`)就传参数,没有(Claude Code `Bash`)就在命令前补 `cd '<目录>' &&`。
- **截断纠偏话术**:协议模式按裸名注册工具,纠偏话术不再写 `gwtools-` 前缀(模型会回「gwtools 命名空间不可用」)。
- **零产出 + 错误 trailer**:turn_commit 之后尾部是错误 trailer、且本轮一个字和一个工具调用都没出时,改为原样报上游错误(gw-app 可重试/换号),不再当成功、再退化成对外 400 的「空回复」。错误 trailer 原文记 warn 日志。
- **带图片/PDF 的请求(IDE 形态回退路径)也应答 KV 子协议**:服务端每存一个内容节都推一帧 `set_blob` 并等客户端回执,要回已存内容时发 `get_blob`。这套应答原先只在 CLI 形态挂,IDE 路径没人回,服务端推完 set 后只剩心跳、90 秒后被停滞闸掐断(e2e:读 PDF 挂死、读图偶发零产出,时有时无取决于这一轮服务端要不要存内容)。现在 IDE 路径也挂只做 KV 应答的驱动(无推迟帧、握手不报 MCP 工具)。
- **回传的思考原文不再折进上游**(新文件 `thinking_echo.rs`):线协议拿不到思考签名,下发的 thinking 块无签名,OpenCode 等客户端回传历史时把它降级成普通 text;我们再折进历史发上游,等于把模型的推理当正文喂回,被 Anthropic 判成推理提取拦截(trailer `Request blocked by Anthropic … Usage Policy`,fable 工具回路偶发零产出的真因)。现在按内容哈希记下我方下发的思考原文,下一轮 assistant 消息里整块等于它的 text 就地置空(不删块,保持媒体占位下标);客户自己写的内容不动。
- **默认驱动热设置** `cursor_default_driver`:`wire`(默认)/ `cli`。`cli` 即回到旧行为,不用重新部署。
### 设计取舍
- 内建工具翻译坚持单向失败:认不出参数形状、客户端 schema 认不齐键名,一律不翻(回不可用或维持原行为),不猜参数。
- 工具回路内挂流续跑(省去每个工具轮的冷重铺)与按客户端精确翻译,留到下一版。
### 注意
- 生产 router 仍是旧版,不认 `cursor_default_driver`,后台接口 PUT 会 400;改它要直接写 control.db 的 settings[system],或先换 router。
- 端到端(opencode → 隔离测试 worker,fable-5.1 / sonnet-5.5 × 跑命令 / 读 PDF / 读图片 / 找文件 / 修 bug / 抓网页做 HTML):第二轮 11/12,唯一失败为 fable 续轮零产出(即本版修的错误 trailer 场景)。
- 测试:gw-cursor 467、gw-core 215、gw-app 676。

## [kiro-legacy-quota-400] - 2026-11-06
### 背景
- 21:20(北京)走老端点的 4 个号额度用完。老端点把额度耗尽报成 `400 ServiceQuotaExceededException`(`reason: MONTHLY_REQUEST_COUNT`),新端点报的是 402。
- caio 把这条 400 当成 BadRequest:号不禁用、不换号,400 直接回给客户(客户看到「请求体无效」)。这 4 个号是最高优先级,一直被选中,到 21:39 回退 worker0 前共失败 557 次。
### 变更
- `error_map.rs`:400 + `ServiceQuotaExceededException` / `MONTHLY_REQUEST_COUNT` 归为 QuotaExhausted,和 402 一样禁用该号并换号。
### 注意
- 回归测试用的是线上原样报文。老端点还可能有别的报错形态与新端点不同,上线老端点前应先把各类报错样本收齐。
- 测试:新增 `legacy_endpoint_quota_400_is_quota_exhausted_not_bad_request`。

## [kiro-routing-ui] - 2026-11-06
### 背景
- 用户要求在管理后台直接配置老端点和中档优先(此前只能用 admin API 写 extra)。
- 审查发现两个缺口:换端点不会清掉号身上已有的「模型不可用」标记(内存里 6 小时),FREE 号还要 `allow_paid_models` 才会被派 opus;只改端点,号仍然接不到 opus。
### 变更
- 账号 PATCH 新增三个定点字段,都只改自己那个键、不碰凭据:
  - `kiro_endpoint`:`""` = 清除(写 null,回默认),其余取值来自 `gw_kiro::ENDPOINT_CHOICES`,大小写不敏感、落库存小写,认不出的值 400。
  - `preferred_load`:`0` = 关闭(写 null),`(0, 64]` = 开启,其余 400。
  - `allow_paid_models`:布尔。
- 两个数值/字符串字段在函数开头先校验,不合法时整单 400,同一请求里的其它改动也不落库。
- 账号列表顶层回显 `kiro_endpoint`、`preferred_load`、`allow_paid_models`。
- worker 同步账号时,号的 `kiro_endpoint` 变了就清掉该号的「模型不可用」标记(旧落点上的结论作废);端点没变的周期同步不动标记。
- 前端:Kiro 号的编辑弹窗新增「上游端点」「FREE 号也派付费模型」「中档优先目标负载」;列表 Provider 列加「老端点」「中档 N」徽章。
### 设计取舍
- 清标记放在 worker 同步里而不是让前端去调「重置」:重置还会复活被运行时禁用的号,副作用太大;放在同步里,API 和 UI 两条路都覆盖到。
- 中档负载输入用文本框而不是数字框:数字框遇到「1,5」这类输入会把值变成空串(= 关闭),还会因 step/max 拦住整张表单。只有改过才校验,库里已有的上限外旧值不会挡住改别的字段。
- 号上已有排查用取值(`runtime`/`q-rest`)时按打开弹窗时的原值显示第三个选项,切走了还能切回来。
- 上限 64:真实号并发是个位数,目标负载再大就等于永远不忙,效果和高优先级一样,应该去改组内优先级。
### 注意
- 管理面在 router 上,worker 的清标记逻辑在 worker 上,两者都要换镜像才生效。
- 测试:新增 PATCH 读写/清除/拒绝测试 4 个、同步清标记测试 1 个、取值表一致性测试 1 个、前端解析测试 2 组;本地起临时 router 用浏览器点过保存和清除。`cargo test --workspace` 全绿,前端 123 个测试通过。

## [kiro-account-endpoint] - 2026-11-06
### 背景
- 降级成 FREE 的号在新端点(runtime 域名)上请求 opus 会被 400 `INVALID_MODEL_ID`,随后被调度器打上 6 小时「模型不可用」标记。
- 11-06 实测:同样的号走老端点(`q.{region}.amazonaws.com`)+ RPC 形态(`POST /` + `application/x-amz-json-1.0` + `x-amz-target`)时 opus-5.5 返回 200;REST 形态打 q 域名、RPC 形态打 runtime 域名都仍是 `INVALID_MODEL_ID`。与 UA 版本、body 里有没有 `agentMode` 无关。4 个号都验证过。
### 变更
- 新增 `gw-kiro/src/endpoint.rs`:按账号的 `extra.kiro_endpoint` 决定请求发到哪、用哪种形态。
  - 不写或写了不认识的值:跟随全局开关(现状 = runtime REST,新号默认就是这个)。不认识的值按账号告警一次。
  - `runtime`:runtime 域名 + REST;`q-rest`:q 域名 + REST;`q`:q 域名 + RPC(本次 4 个号用的就是这个)。
  - 设了 `KIRO_RUNTIME_UPSTREAM_BASE_URL` / `KIRO_UPSTREAM_BASE_URL` 环境变量时一律按旧行为走 REST,账号字段不生效。
- `headers.rs`:`apply_streaming_headers` 改为接收 `Endpoint`,RPC 形态多带 `x-amz-target`、换 `content-type`,其余请求头不变。
- `chat.rs`:主请求和 `THINKING_SIGNATURE_INVALID` 重试共用同一个落点。
- body、eventstream 解析、错误分类、计费都没有改:老端点的报文和响应与新端点同形。
### 设计取舍
- 不做「忽略模型不可用标记」这类开关:FREE 号上 opus 报错时,换号上限是整组大小,全靠标记让下一跳跳过它;去掉标记会在 FREE 号之间来回重试,有封号风险。正确做法是让请求一开始就打到能用的端点。
- 只用一个账号字段,现有 `PATCH extra_merge` 就能热改;管理后台的开关见 `[kiro-routing-ui]`。
### 注意
- 只验证过 us-east-1。Token 刷新、额度查询仍走原端点。
- 签名回传(11-06 实测):老端点会验签,认它自己签发的签名(200),签名改坏一个字符返回 400 `THINKING_SIGNATURE_INVALID`。新端点签发的签名在老端点上是否通过没测;不通过时由现有的「去掉历史签名重试一次」兜底。
- 测试:新增 endpoint 模块单测、两条请求头形态测试、RPC 报错体分类回归测试。`cargo test --workspace` 2250 通过、0 失败。


## [kiro-preferred-load] - 2026-11-04
### 背景
- 上一版 `[kiro-preferred-threads]` 按「挂着的会话数」封顶,上线后实测两个各 30 分钟的时段:
  - 名额 3 时,三个号合计占全池 10.0%,排名第 1、3、33;
  - 名额 5 时反而降到 6.9%,排名第 6、12、16。
  - 名额基本总是满的,号上平均却只有 0.3–0.6 个请求在跑:Claude Code 起标题、子任务这类附带请求各算一个会话,只发一两条就占 5 分钟名额。哪个号分到忙会话全凭运气。
- 用户要求改为「只关心号实际上有多忙」。
### 变更
- 开关换成 `extra.preferred_load: 目标负载`(正数,单位是同时在跑的请求数),热写。旧键 `preferred_threads` 不再读取。
- 号的忙碌度 = max(最近 120 秒平均同时在跑数, 此刻在途数)。新会话来时,同时满足下面三条就越过所有层先派给它,否则退回原组内优先级:
  - 有空闲并发;
  - 最近一次请求没失败(`failure_count == 0`);
  - 忙碌度低于目标。
- 「只抬新会话、不拉走已有家的会话」「中档号并发满时新会话不等它」两条照旧。忙碌度只在新会话时计算。
- 每号运行态新增 `recent_done`(最近跑完请求的起止时刻):lease 释放时由在途守卫追加,并丢掉窗口外的条目。它和在途表各自一把锁,从不嵌套持有。
- 状态快照字段改为 `preferred_load` 和 `busy_level`,去掉 `preferred_threads` / `active_threads`。
- 上一版加的首轮登记表保留:它同时让「活跃会话少者优先」这条打平规则算得准,对普通号也有用(审查意见)。
### 设计取舍
- 平均数只算请求真正在跑的时长,会话两轮之间的空档不计,安静的会话不占任何东西。
- 拿「此刻在途数」当下限:刚起跑的请求时长约为 0,不加下限的话,一阵新会话会在平均数涨起来之前全派过去。在途登记和选号在同一临界区,下一个新会话立刻看得见。
- 只为开了开关的号、且只在新会话时算忙碌度,其他号和老会话的选号开销不变;快照里所有号都算,方便和普通号对比。
- **要求有空闲并发**:目标设得比并发还大时,在途下限够不着目标,满载的号仍会被当成不忙,新会话就会去排它的队(审查发现)。
- **要求最近一次没失败**:快速报错的号每次只占几毫秒,看起来永远不忙,会把新会话一个接一个吸过去全部失败(审查发现)。`failure_count` 只计服务端、网络类错误,成功一次就清零,所以一个坏号最多吃一条。等它在老会话上重新成功一次,再恢复优先。
### 注意
- 测试:新增 `ci_preferred_loses_preference_after_a_failure_until_it_succeeds_again`、`ci_preferred_target_above_concurrency_still_yields_when_full`、`busy_level_averages_window_overlap_and_floors_at_live_count`、`released_lease_is_recorded_as_recent_busy_time`、`ci_preferred_takes_new_threads_ahead_of_every_tier_while_not_busy`、`ci_preferred_respects_recent_average_load_not_session_count`;沿用「老会话不被拉走」「满了不排队」两条(严格分层开和关各跑一遍)。`cargo test --workspace` 全绿。

## [kiro-preferred-threads] - 2026-11-04
### 背景
- 用户要让 3 个带 500 赠送积分的号优先调度,但不希望所有请求都挤到这几个号上。
- 现有优先级只有「层」一种用法。11-04 只读抽样(最近 2000 条,约 34 分钟):全部有号请求落在 100 层,0 层和 1 层这段时间 0 条。如果把这 3 个号挂到 100 以下,它们就是高层仅有的活号:每个新会话都先落到它们身上,号上挂的会话数没有上限,老会话要为保缓存在原号上排队(最长 30 秒)。这就是 11-03 撤回高优时用户说的「太卡」。留在 100 又和其他约 100 个号完全平等。
### 变更
- `worker/scheduler.rs`:新增账号级开关 `extra.preferred_threads: N`(正整数,热写)。
  - 只作用于**新会话**落点(本线程在记忆时长内没在任何号上跑过)。号上近一个缓存 TTL 内的活跃会话数小于 N 时,它在 `select_ci` 的候选里排在所有层之前;达到 N 后退回原本的组内优先级。
  - 活跃会话数复用 `CiState::active_threads`:新会话派过去时由 `note_new_home` 在同一把锁内当场加 1,并发请求冲不过上限。
- `worker/cost_index.rs`:`CiState` 新增首轮登记表 `pending`。
  - 原先每 30 秒清扫只按热度表重算活跃数,而会话要等首轮跑完才进热度表,首轮还在跑的会话会被清零(审查发现:首轮 90 秒时,名额 3 的号能挂上 9 个会话)。
  - 现在派出时登记,首轮跑完(`touch`)时移除,超过一个缓存 TTL 仍没跑完就过期;同一会话重试时覆盖原登记,不重复计数。
- 每号状态快照新增 `preferred_threads` 和 `active_threads` 两个字段。
### 设计取舍
- **封顶会话数,不封并发**:会话在两轮之间并发为 0,按并发封顶照样会被塞满,卡顿就是这么来的。
- **只抬新会话,不抬已经有家的线程**:严格分层(`affinity_migrate_up`)打开时,一个空着名额、能立刻起跑的冷号会把准入层压到最低,把老会话从热号上拉走。只抬新会话,开和关两种模式下老会话都留在原号。
- **名额号并发满时不排队**:它只是一个「要等的候选」,和别处能立刻起跑的号比代价,等待计成本,所以新会话直接去别处。
- 不新增层、不改成员边:名额是号的属性,不是组的排序。改成员边要动表结构和管理界面,收益为零。
### 注意
- 只在代价指数算法里生效;旧算法 `affinity` 和退回旧循环那条路(`ci_fallbacks`)忽略它。
- 没带会话键的请求不进热度表,只在清扫前的 30 秒内计入活跃数。
- 首轮失败且客户端不重试的会话会多占名额最多 5 分钟,只会让名额号少接,偏保守。
- 顺带修掉审查发现的两处小问题:`token_only_accounts_get_distinct_ids_from_refresh_token` 标了两次 `#[test]`,被跑了两遍;`sanitize_account_id` 和 `cost_index_param_range` 的文档注释贴错了函数。
- 测试:新增 `first_round_threads_stay_counted_across_sweeps_until_touched_or_expired`、`ci_preferred_quota_holds_across_sweep_while_first_rounds_are_running`、`ci_preferred_quota_takes_new_threads_ahead_of_every_tier_until_full`、`ci_preferred_quota_never_pulls_threads_that_already_have_a_home`(严格分层开和关各跑一遍)、`ci_preferred_account_that_is_full_does_not_make_new_threads_wait`、`ci_status_snapshot_reports_preferred_quota_and_active_threads`。`cargo test --workspace` 全绿,不设开关的既有调度测试零改动通过。

## [kiro-import-token-only-id] - 2026-11-04
### 背景
- 只带 accessToken/refreshToken、没有 email/userId/id 的号,导入时 ID 统一落到常量 `kiro-account`。07-02 已有一个同名死号(external_idp,0 次成功)占住了这个名字,11-04 导入的一个社交号撞上它 → 走合并路径 → 合并不写 token 字段 → 判「重复」,新号被静默丢弃,界面也看不出和谁重复。
### 变更
- `gw-kiro::import`:新增 `account_id_or_token_derived`,email/userId/id 都缺(或只有空白)时按 refresh token 的 sha256 前 12 位派生 `kiro-rt-<hex>`;嵌套(KiroManager)与扁平两种格式共用。
### 设计取舍
- 照搬 apikey 号的 `kiro-apikey-<sha12>` 做法:不同号不撞名、同一份重导仍得同一 ID(判重照常)、ID 不含 token 原文。
- 已有的 `kiro-account` 不改名(ID 是主键,改名牵连组成员/用量记录),由运维删除。
### 注意
- 只影响新导入;导入在 router 上跑,需换 router 镜像。测试 `token_only_accounts_get_distinct_ids_from_refresh_token`,`cargo test --workspace` 全绿。

## [kiro-allow-paid-models] - 2026-11-04
### 背景
- 两个新导入的 gmail IdC 号(jenniferhernandezx883 / ambrabartimus2207618)上游标题是 `KIRO FREE`,但带 500 赠送积分(limit=550,全池启用中的 471 个 FREE 号都是 50),导入后档位未回填的窗口里已实测跑通 opus-5.5。标题回填成 FREE 后被第③条 FREE 过滤挡掉 opus / haiku-5.5。
### 变更
- `gw-kiro`:新增账号级开关 `extra.allow_paid_models`(JSON 布尔 true 才生效),跳过 `account_supports_model` 第③条 FREE 过滤;第①条白名单、第②条非 claude 报名照旧先判。
### 设计取舍
- 不改 `subscription_title`:每轮配额刷新都会按 getUsageLimits 回写,手改撑不过一轮。
- 不复用 `model_allowlist`:线上 35 个 FREE 号设了 `["claude*","kimi-k3"]`,让白名单抬 FREE 会把它们全放进 opus。
- 不按 limit>50 自动判:用户明确只放这两个号;赠送积分的模型权限未逐号验证。
### 注意
- 开关用 `PATCH /admin/api/accounts/{id}` + `extra_merge: true` 热写,不需重启;赠送积分用完或上游收回权限后,号会吃 400 被打「模型不可用」6h 标记,届时把开关删掉即可。
- 测试:`allow_paid_models_flag_lifts_free_gate_for_that_account_only`;`cargo test --workspace` 全绿。

## [kiro-haiku55-extra-models] - 2026-11-03

### 背景
- Kiro 上游目录(11-03 POWER 号重拉,22 个模型)已有 `claude-haiku-5.5`,倍率 0.06(haiku-4.5 是 0.4)。caio 模型表没有这一行,`claude-haiku-5-5` 被子串兜底**静默换成 haiku-4.5**、按 4.5 计积分。
- 同日拉 FREE 号目录:8 个模型,**没有 haiku-5.5**。原有 FREE 过滤只挡 opus,直接加行会把它派给 FREE → 400 再换号。
- Kiro 线没有热改模型的设置,每上一个新模型都要重新构建部署。用户要求:先上 haiku-5.5(只给付费档),同时加热追加机制。

### 改动
- **haiku-5.5 内置行**(`converter/model_map.rs`):1M 窗口、5 档 effort 默认 medium、`structured_effort_only`/`rewrite_visible_thinking_directive` 照 5.5 家族(未实测)。签名 `Anonymous{v4}`:11-03 真号直放 1 条实测有原生签名推理,信封顶层 f1=4、无 f5/f6 代号(头 f1=19,两个 5.5 是 18),历史推理与 opus-5.5 一样完整回传。子串兜底认 `haiku-5-5/5.5` 写法,不再掉到 4.5。
- **`KiroModel::pro_only`** + `converter::denies_free`:`account_supports_model` 第③条改为「opus 或 pro_only」且标题含 FREE → 不派。存量各行填 false,行为不变。
- **热追加 `kiro_extra_models`**(`converter/extra_models.rs`,DTO 在 `gw-core::config::KiroExtraModelSpec`):每条必须 `like` 一条内置行,行为字段(签名、分词器、effort 路径、5.5 系开关)只继承不开放;可写 id / kiro_model / 展示名 / 身份短名 / 窗口 / 档位 / 默认档 / `pro_only`(默认 true)。整组校验,非法整组拒收并沿用旧表,每轮作为设置同步错误回报面板(日志只打一次)。所有派生点经 `model_map::all_models()` 查表(内置在前、热追加在后),`/v1/models`、路由、身份行、档位、签名、分词器、FREE 过滤、漂移检查自动跟上。`/health` 回显 `kiro_extra_models_active`。
- **admin 写侧**:`PUT /settings` 对 `kiro_extra_models` 用同一个 `validate_extra_models` 校验并拦未知键,不合法 400。

- **(同日补丁)客户端历史里的 `redacted_thinking` 不再上传成 `redactedContent`**(`converter/history.rs::build_reasoning_content`)。上线后一个 1103 条消息的会话切到 haiku-5.5,连换 3 个号都 400 `REQUEST_BODY_INVALID`;原样重放失败、只去掉 reasoningContent 即 200(去 effort / 去工具仍 400)。caio 从不向客户端下发 redacted_thinking,这类块必然来自别的上游,对 Kiro 无效;原设想的「上游验签失败 → 剥离重试」兜底并不成立(错误码不是 THINKING_SIGNATURE_INVALID)。haiku-4.5 无签名推理、从不上传推理,所以该会话以前被降成 4.5 时一直正常。用户先怀疑 max_tokens 128000 超过 6.4 万上限,核实后排除:caio 从不把 max_tokens 发给 Kiro,失败报文里没有这个字段。

### 设计取舍
- 热追加行字段是 `&'static str`(派生点到处借用),生效时 `Box::leak` 后在 `RwLock` 里换切片引用;只在设置内容真变时泄漏一份(几百字节),30s 轮询同一份设置不泄漏。gpt 评审提过「反复改会累积」,接受:量级由运维改设置的次数决定。
- 只能追加、不能改内置行:内置行的行为字段都是真号实测得出的,开放热改等于允许「填错值、编译照过、线上静默失效」。
- DTO 不加 `deny_unknown_fields`:worker 解析整份叠加层,新字段让旧 worker 解析失败会冻结全部热设置;拼错键改由 admin 写侧拦。
- FREE 判据是「标题不含 FREE」,标题缺失与 STUDENT 放行(与 opus 现有口径一致)。STUDENT 档目录里有没有 haiku-5.5 未查;派错的代价是一次 400 + 6h 模型不可用标记。

### 验证
- `cargo test --workspace` 全过(gw-kiro 684、gw-app 665、gw-core 439 等);clippy 改动行无新警告。
- 新增测试:haiku-5.5 各写法映射/窗口/档位/签名/公告;只有它 `pro_only`;FREE 拒、PRO/PRO+/PRO MAX/POWER/STUDENT/标题缺失放行、FREE 仍可用 haiku-4.5/sonnet-4.5;热追加生效后全部派生查找可见、继承规则、相同设置不重建不泄漏、非法整组拒收保留旧表、清空、13 类非法形态;admin 写侧 400 与 null 清空;gw-core 叠加层往返且容忍未知字段。

## [dario-cc-translate] - 2026-10-30

### 背景
- dario 线(caio worker → 每号一个原版 dario 进程 → Anthropic 订阅)以前对请求体几乎不动,客户端识别只在 Kiro 线;dario 自己按一张通用别名表(`cmd || command`)把第三方客户端的工具猜成 CC 工具,不认识 pi 的 developer 角色、new-api 插的 `...`。
- 查出 caio 自己造成的两处偏差,真 CC 透传路径也受影响:worker 把请求体解析成 `serde_json::Value`(工作区未开 `preserve_order`)再序列化,**顶层字段变成字母序**,而 dario 只在模板路径上按 CC 顺序重排;`prepare_forwarded_body` 给每个请求补 `service_tier`,真 CC 平时不发。dario 的研究(`docs/research/system-prompt-classifier-study.md`)表明计费分类器看字段顺序、beta、工具名等结构特征,不看 system 文本。
- 用户 10-30 定方向:真 Claude Code 原样交给 dario;其余客户端由 caio 统一翻译成 Claude Code 报文;工具改成 CC 工具名,难映射的客户端不支持(兜底报错);回包缓冲拼回客户端形态。

### 改动
- **客户端识别搬到公共层** `gw-core/src/clients.rs`(责任链 `CHAIN`,Kiro 线与 dario 线共用),新增 `ClientKind::Copilot`。判据只看 `ClientSignals`(system 全文、工具名、首个 assistant 前有无 developer),由各线自己准备输入:Kiro 线仍用截掉末尾 prefill 后的消息,保证判定与改造前逐条一致。`gw-kiro/converter/clients/` 删掉各客户端文件,改成按认出的类型挑 Kiro 整理步骤。
- **dario 线翻译**(`gw-dario/src/cc/`):
  - 真 CC(判据逐字照搬 dario `isGenuineCCClient`)→ 原样交 dario,字段按 CC 顺序写回;没发 `service_tier` 不补,只有 `/fast`(`auto`)改成 `standard_only`。
  - OpenCode、Copilot、Claude Code 变体(有身份行没计费块)、「CC 工具面」的认不出客户端(工具全是模板里的 CC 原生名或 `mcp__*`)→ 翻译成 dario 眼里的真 CC:system = [计费头占位, 模板身份行, 引导语 + 擦掉客户端名的客户端指令 + 工具名对照与客户端执行差异说明];字段按模板 `body_field_order`;max_tokens 64000、自适应思考与 effort 规则照 dario 模板路径(客户端强制调用工具时不开思考,两者同开上游 400);去掉 temperature / top_p / top_k / stop_sequences;developer/system 角色并进 system,`...` 占位、空文本块与历史 `thinking` 块清掉(同 dario 模板路径,翻译后 dario 走透传不再替我们清),相邻同角色合并(tool_result 排前)。
  - 工具:每个客户端一张精确映射表(OpenCode 11 条、Copilot 7 条,两个方向的参数换算,按客户端这次声明的字段名回写,OpenCode 新旧两版字段名都认);兑现不了 CC 定义能力的工具不映射,走 MCP 改名 `mcp__<服务名>__<原名>`(OpenCode=`workspace`、Copilot=`ide`,不放客户端品牌名),超 64 字符截断补哈希;原样保留的名字先占位再给改名工具分配;同名工具重复定义只留第一个。每个 CC 工具登记 v2.1.312 的参数清单,模板升级后多出没登记的参数就退回 MCP 并告警。
  - 回包(`reverse.rs`):需要改回的 `tool_use` 按块缓冲,攒齐 `input_json_delta` 后整段换名换参数再发,缓冲期间每 10 秒发一个 `ping`(worker 300 秒无输出会当卡死掐断);文本、思考块与原样工具直通;流中途断开时缓冲的内容原样放出不吞;改不回(模型臆造工具名、参数不是 JSON)按原样发出并告警。
  - 带工具但认不出 / 不支持的客户端 → 本地 400(`bad_request_visible`:原因发给客户端,不换号、不扣号、不发上游)。不带工具的请求只放行认出来的支持客户端(OpenCode、Copilot、Claude Code 变体),认不出的一律拒。用户 10-30 定:不给它们补工具冒充 agent 请求 —— 真 CC 自己的辅助调用(起标题、压缩、安全检查)本来就不带工具,补一整套 CC 工具(约一两万 token,还得 `tool_choice: none`)反而和真 CC 对不上,宁可拒。代价:系统提示与主对话不同的辅助调用(如 OpenCode 的 title generator)会被认成兜底而拒掉。
  - Claude Code 变体(有身份行没计费块)的工具要是 CC 风格(模板原生、首字母大写如 `ListMcpResourcesTool` / `Artifact`、`mcp__*`);不是的话 system 里有 OpenCode 那一行就按 OpenCode 的表翻译(OpenCode 走 Anthropic 时前面会多一块 CC 身份行),否则拒绝 —— 不让别家的小写工具名套着真 CC 外形原样出去。
  - 其余请求字段:max_tokens 按模型取 CC 的上限(opus-4/4.1 为 32000、3.5 系为 8192,其余 64000);强制调用工具时支持自适应思考的模型显式 `thinking:{type:"disabled"}`、effort 压到 high 以内(Opus 5 不写 thinking 也会思考,`disabled` 在 effort 高于 high 时 400);没有 tools 就不带 tool_choice;客户端的 `output_config.format`(结构化输出)保留;tool_result 只留 `type/tool_use_id/content/is_error`(同 dario 模板路径)。
- **开关** `dario_cc_translate`(热设置,30 秒生效):`off`(默认,与改造前逐字节一致,且不读模板文件)/ `observe`(照常转发,只记录会怎么翻译、会不会被拒)/ `on`。写侧与 worker 读侧共用 `gw_core::config::dario_cc` 的校验;模板文件指向 dario 产物里的 `cc-template-data.json`(跟着 dario 升级走),读不到按 `off`。翻译/透传过的请求记录 Anthropic 回包的 `anthropic-ratelimit-unified-representative-claim`(看是否仍按订阅额度)。`settings_fingerprints` 回显开关与模板版本;`hot_settings_supported` 仍为 false(缓存计费等 provider 级设置 dario 依旧不热应用)。
- 模板路径是 system.yaml **顶层**键 `dario_cc_template_path`(不放 `dario` 段:那里带 `deny_unknown_fields`,只升级 worker-dario 时其余老镜像读到新字段会起不来,同 `node_hub_url`)。
- gw-kiro 的 `safe_prefix` 改为复用 `gw_core::clients::safe_prefix`(同一份实现)。
- 新增 `gw-dario/examples/cc_translate_replay.rs`(离线回放,只算不发)。

### 设计取舍
- **不注入 CC 自己的系统提示词**(dario 模板路径会注入):研究实测提示词内容、长度、块数不影响计费分类;注入每轮多花 token,还会和客户端指令打架。
- **识别放公共层、整理留在各线**:「这是谁」只有一份;Kiro 与 dario 的上游需要不同,整理步骤各管各的。
- **映射表由本次请求的工具列表确定性算出,不存跨请求状态**(用户提过必要时上 Redis):拼回只发生在单个回包流内,下一轮历史重新算出同一张表、同样的字节,缓存前缀不变,所以不需要 Redis。
- 改不回的工具调用不报错而是原样发出:客户端会把「工具不存在 / 参数不对」回给模型,模型能自己纠正,比整段回包失败好。
- 映射口径:**兑现不了的能力不承诺给模型**。CC 定义里有、客户端做不到会改变操作含义的(Copilot `replace_string_in_file` 没有全部替换;OpenCode `task`/`subagent` 能按 task_id/sessionID 续接子 agent、CC Agent 没有)走 MCP;做不到但只影响结果格式或范围的接受并记在表文件头(CC Grep 的输出模式与上下文行、Read 的 `pages`、WebFetch 的提取提示与分页 —— 客户端返回的是更完整的原始结果;Copilot `read_file` 范围换算、`run_in_terminal` 的 goal 用 description 补;OpenCode `webfetch` 补固定 prompt、`workdir` 写进命令,并在 system 里说明「工作目录不跨调用保留」)。
- 往返不闭合不影响缓存:缓存前缀止于上一轮请求末尾,不含模型自己那次输出;工具列表变化时 tools 在最前、整段缓存本来就失效。影响只是模型看到的历史与它当时的输出略有差别。
- 消息正文不擦客户端名字(dario 模板路径会擦「正文安全子集」):正文不是计费分类器的输入,擦了反而可能改坏用户代码与工具结果。只擦 system。
- 本期不支持 pi、DeepSeek Harness、Grok Build(先报错,按需加表);Kiro 线的 `...` 清理挪进公共层会改变 Kiro 输出,单独做。

### 验证
- 新增测试:gw-core 识别 6 条 + 开关 2 条;gw-dario 翻译 50 余条(真 CC 判据、字段顺序、清理、擦名、两张工具表的正反向与往返、MCP 改名与撞名、模板兼容检查、回包缓冲含跨帧参数 / 多工具交错 / 断流 / ping、强制工具关思考、CC 身份行后面的 OpenCode、无工具放行、开关三态)。workspace 全量 2215 条通过、0 失败;clippy 改动处无新警告。
- **Kiro 线不回归**(`replay_convert` 用 d875828 与本分支各编一份,10-29 证据快照 + 当日库共 6992 条线上请求,数据不出服务器;最终版又跑一遍):各客户端 Kiro 报文全部逐字节一致(Claude Code 3091、OpenCode 1859、兜底 743、Grok Build 505、pi 406、DeepSeek Harness 388),解释不了的差异 0。
- **dario 线翻译回放**(`examples/cc_translate_replay.rs`,同一批 6992 条,两个进程各跑一遍比确定性;翻译结果再交给 dario 线上产物自己的 `isGenuineCCClient` / `orderBodyForOutbound` 在 bun 里判):
  - 出错 0、两进程输出不一致 0;透传与翻译出去的 5018 条 **100% 被 dario 判为真 CC、字段顺序 100% 与 dario 排序一致**(最终版;此前三轮 5082 / 5189 / 5133 条同样 100%)。
  - 最终版分流:Claude Code 透传 2982 / 翻译 144;OpenCode 翻译 1837;Copilot 翻译 31;认不出但 CC 工具面的 24 条翻译;pi 529、DeepSeek Harness 395、Grok Build 332、认不出 718 条(其中不带工具 216 条)本地拒绝。
  - 按 Anthropic 规则检查(角色、交替、工具配对、空文本、工具名):原始请求里 OpenCode 连续同角色 1163、system 角色 146、空文本 15,Copilot 非法工具名 31,翻译后全部清零;只剩 OpenCode 4 条以 assistant 结尾(客户端 prefill,与 dario 透传行为一致)和 7 条历史引用了当前未声明的工具(原始请求就有)。
  - 用真实历史参数做「客户端 → CC → 客户端」往返:OpenCode read/edit/write/grep 损失 ≤ 0.2%;shell/bash 的损失来自 `workdir` 写进命令、webfetch 来自补 prompt,均为预期;Copilot `run_in_terminal` 315/315 损失 goal(预期)。
- **对抗审查**(codex 登录失效,改用 opencode/gpt-6-astra 三个视角):采纳 high 4 条(历史 thinking 绕过 dario 清理、强制工具与自适应思考冲突、Copilot Edit 丢全部替换、回包缓冲撞 300 秒空闲超时)与 medium 若干(OpenCode 子 agent 续接、Grep 语义参数、MCP 撞名顺序、模板升级兼容、shell 工作目录、正文擦名、off 时读模板);「往返不闭合 / 工具表随列表变化破坏缓存前缀」不采纳(理由见设计取舍)。
- **/code-review**(15 条)采纳:模板路径移到顶层键、OpenCode 带 CC 身份行时小写工具原样出去、Copilot Grep 路径换算(文件 / 绝对路径 / 递归)、无 tools 带 tool_choice、Opus 5 强制工具仍会思考、拒绝原因到不了客户端、OpenCode webfetch 的 format、max_tokens 与结构化输出、tool_result 多余字段、order.rs 文档、startup.rs 注释、`safe_prefix` 重复。不采纳:`hot_settings_supported` 维持 false(它在面板上表示缓存计费等也热生效,dario 不支持;已改注释说明);真 CC 透传的两处修正只在 `on` 时生效(`off` 必须与改造前逐字节一致);observe 模式与多次复制的开销(dario 线流量很小,以后再优化);dario 与 Kiro 两套清理规则合并(两边消息类型不同,只先合并了 `safe_prefix`);「无工具的辅助调用被拒」—— 一度改成全放行,用户定为只放行认出来的支持客户端(见改动)。
- **未做**:实号测试(花用户自己的订阅额度,届时先问)。

### 上线(2026-11-01 凌晨,北京时间)
- 镜像 `cctranslate-9b33ffa`(经 `git archive` 从提交构建):router、worker0、router-dario、node-hub;worker-dario 跑修复版 `cctranslate-b9b2877`。cursor 两个 worker 不动。逐个换、逐个验 `/health.build_commit`。
- compose:worker-dario 只读挂载 `/root/dario-upstream-v6.18.2/dist/cc-template-data.json` → `/app/dario/cc-template-data.json`(与 sidecar 挂的 dario 6.18.2 同一份产物,模板 2.1.312 = 翻译器登记的版本);system.yaml 顶层 `dario_cc_template_path` 指向它。重建的 5 个容器加了 json-file 日志轮转(200M×5;此前不轮转,worker0 两天 512M)。
- `dario_cc_translate=on`(用户定)。**上线后发现** `dario_cc_template_path` 没进 provider 工厂 cfg(`worker/startup.rs` 只挑了几个顶层键),dario 拿到空路径、开关 on 也不翻译 —— `/health` 的 `dario_cc_template` 指纹为 null 且无报错日志,单测与离线回放都绕开了这条启动路径。`b9b2877` 修复并加测试,只重建 worker-dario;修复后指纹 `2.1.312`、日志「dario CC 模板已加载 tools=37」。
- 观察:worker0 重启后 38 秒内约 760 次 402(月额度已尽的号被逐个重新撞到、禁用 —— 额度耗尽状态只在内存里,重启即丢),被换号吸收,客户成功率 96.9% → 4 分钟后 99.5%(换前 98.7%)。router 重启切断 35 条在途流(`incomplete_stream`,3 秒内)。
- 回滚标签:router `kimik3-20261023`、worker0 `clients-d875828`、router-dario `dario-nodes-20261012c`、worker-dario `dario-nodes-20261012f`、node-hub `dario-nodes-20261012`。

## [client-translators] - 2026-10-29

### 背景
- 转换器只有一个通用入口,各家客户端的特殊处理是按内容特征零散加在里面的判断(CC 计费头 / SessionStart / 提醒块 / 分类器原句、developer 提升……),代码里没有「这是哪个客户端」的概念。pi 把系统提示放在 `role=developer`,之前被静默丢了很久才发现(10-29 修)。
- 10-29 线上约 7000 条请求的客户端分布:Claude Code 3172、OpenCode 约 1970、DeepSeek Harness 617、pi(developer 角色)311、Grok Build 287、其余零散。
- 两家已确认的怪癖:
  - **pi**:首条 user 是 `...` 占位,developer 提升后与首句合并成 "...\n首句";纯工具回合的 assistant 正文 82% 是 `...`,模型回复 63/311 以 `...` 开头(在学历史里的自己)。
  - **Grok Build**:纯工具回合的 assistant 正文 84% 是 `...`。

### 改动
- 新增 `converter/clients/`:按客户端翻译,**责任链**。`CHAIN = [ClaudeCode, DeveloperRole, GrokBuild, OpenCode, DeepSeekHarness, Fallback]`,第一个认出请求的环节负责整理,链尾 `Fallback` 恒接。
- **兜底 = 原有整套处理,逐字节不变**。原 `normalize::route_system_role_messages`(system / developer 分流)搬到 `clients/steps.rs`,作为所有环节的标准步骤。
- 专用步骤只加在认出来的客户端上:
  - DeveloperRole(pi):去掉 developer 提升后留在最前面的 `...` 占位 user;删掉「`...` 文本 + tool_use」里的 `...` 文本块。
  - GrokBuild:删掉「`...` 文本 + tool_use」里的 `...` 文本块(以独有工具名识别,扛得住用户改写系统提示)。
  - ClaudeCode / OpenCode / DeepSeekHarness:只认出来分口径,步骤同兜底。
- `ConversionResult.client` 记录认出的客户端,`chat_stream` 打 debug 日志。
- 新增 `crates/gw-kiro/examples/replay_convert.rs`:历史报文回放,新旧两版逐条比对用。

### 设计取舍
- 兜底保留全部旧规则(含 CC 专属):这些规则自带判据,对别家请求本来不生效;CC 请求万一没被认出来,兜底照样处理对。把 CC 规则从兜底里拿掉不影响正确性、只会削弱保护,不做。
- `...` 清理放在翻译层直接整理 Anthropic 消息,不往下游 history 构建里传开关:通用转换器一行不改。
- conversationId / 调度亲和仍在翻译之前用原始 messages 计算:翻译不让在途会话换 ID、换号。
- 本期不做:Grok Build 系统提示「You are Grok」与我方 Claude 身份覆盖的冲突、Grok Build 把 76% 旧工具结果换成 `[Tool result omitted — too old]` 导致的缓存问题(都要先拿数据验证);请求日志落 `client_kind` 列。

### 验证
- 新增 16 条测试(翻译链 9 条:各客户端识别正反例、链顺序、兜底零拷贝且不清理、Grok 工具名去重与词边界;转换入口 7 条:pi 占位、`...` 清理、会话 ID 不变)。gw-kiro 673 条、全量 workspace 测试通过。
- **历史报文回放**(`examples/replay_convert.rs`,main `b90c811` 与本分支各编一份,喂同一批 10-29 线上请求 6992 条,数据不出服务器):

  | 客户端 | 条数 | 结果 |
  |---|---|---|
  | Claude Code | 2908 | 全部逐字节一致 |
  | OpenCode | 1918 | 全部逐字节一致 |
  | 兜底 | 694 | 全部逐字节一致 |
  | DeepSeek Harness | 362 | 全部逐字节一致 |
  | pi(developer 角色) | 825 | 全部有变化:开头 `...` 去掉 825 处、assistant `...` → 空格 26001 处 |
  | Grok Build | 285 | 274 条有变化,全部是 assistant `...` → 空格(37540 处);11 条没有 `...`、不变 |

  解释不了的差异 0 条;新版输出体检(残留 `...`、会被 Kiro 拒的空 user、pi 系统提示是否进了 history[0])0 个问题;两版都没有转换失败。
- 对抗审查(opencode 两个镜头)采纳 3 条:Grok 特征工具按不同名字计数(原来重复定义两次也算命中两个)、`You are Grok` 要求完整词、`Translation::standard` 改名 `legacy` 并注明是冻结的旧路径;`ClientKind` 加 `#[non_exhaustive]`。

### 注意
- pi / Grok Build 在跑的会话,上线后第一轮历史字节变化,整段缓存失效一次,之后恢复。

## [refusal-guard] - 2026-10-29

### 背景
- 10-29 下午一批「DLL 逆向源码恢复」子 agent 反复触发上游安全分类器拒答(CYBER)。worker 本身不会因拒答换号(`worth_switching_account` 对 ContentFiltered 为 false),但**客户端会原样重发**,网关每次重新选号:
  - 前后 32 小时共 863 次拒答,打到 202 个「会话 × 号」组合;
  - 拒答后同一会话的下一条请求 816/822 是原样重发,重发约 79% 再被拒、约 16% 碰运气通过,三分之一被换到别的号;
  - 一个会话被拒 79 次、涉及 27 个号,其中 15 个随后被封。每次拒答都在那个号上记一次违规,扩散本身就是封号的放大器。

### 改动
- 新增入口闸门 `gates::refusal_guard`(拒答记忆),接在逐条筛查之后、选号之前:同一条请求被零产出拒答后,重发**只许用第一次被拒的那个号**,超过 `refusal_guard_retries` 次就本地回 400(上游原文 + 网关说明,日志 `error_kind = RefusalRepeat`);钉的号用不了(被封 / 禁用 / 不在本组)也本地回拒答,绝不换号。重发拿到回复即清除记录。
- 「同一条请求」= `model ‖ system ‖ messages` 规范化后的 sha256(键排序、丢 `cache_control`、system 剥计费头行);另存 `(model, 消息条数)` 索引,索引里没有的请求不做哈希。
- 记账在两个收口:`spawn_request_log_blocking`(流式、首包前失败、本地拦截)与非流式收尾,均在「库降级跳过落库」之前。
- 热设置:`refusal_guard_mode`(off / observe / enforce,默认 observe)、`refusal_guard_retries`(0–5,默认 1)、`refusal_guard_ttl_secs`(300–86400,默认 7200)。`/health` 新增 `refusal_guard` 快照;日志面板对 `RefusalRepeat` 标琥珀色「重复拒答拦截」。

### 设计取舍
- **默认允许再发 1 次、钉原号**(用户选定):模拟 32 小时 trace,上游拒答 863 → 273 次,本地拦 702 条,其中约 75 条本来重发能通过;每个违规会话最多牵连 1 个号。再放 0 次拒答更少(181)但误拦 201 条。
- 只做整条精确匹配,不做前缀匹配:拒答后接着往下聊的只有 5/822。
- 含 model:上游原话建议「换个模型」,换模型必须放行。
- 不按任务模板做跨会话累计(批量子 agent 各自首次被拒仍各打 1 个号),留作下一步。

### 验证
- 新增 18 条测试:指纹口径、状态机(钉号 / 拦截 / 清除 / 过期 / 容量 / observe / off)、入口级三条(流式与非流式都钉回原号且第三次不碰上游、钉的号被禁用时不扩散、observe 不改路由)、admin 写侧校验、前后端日志标记字面量。全量 2144 条 Rust 测试通过;admin-ui 118 条通过、构建通过。

### 未做 / 注意
- 未部署。写侧校验在 router 上,改热设置需要 router 与 worker0 一起换镜像。上线先 observe,核对「本应钉号 / 本应拦截」日志后再切 enforce。
- 状态纯内存、单进程,重启清空。

## [refactor-layering] - 2026-10-28

### 背景
- 原始架构设想:调度统一、每个进程只写自己的处理(一个进程一种上游,多进程并发)、通用 util 放一个包。诊断时骨架没偏(各上游包只依赖 gw-core,scheduler 对具体上游零引用),偏在「东西放错层」:
  - 通用 `worker/mod.rs`(8316 行,其中生产代码约 5000 行)里有 24 处按上游名分支、23 处直接调用 gw_kiro / gw_cursor;
  - gw-core 里塞进了带进程级全局状态的入口策略(分类器直答 / 批量拦截 / 逐条筛查);
  - 10-23 起三次回归都源于在不受 git 管的服务器源码目录里构建。

### 改动(纯搬家与合并,行为不变)
- **源码只认 git**:`scripts/build-image.sh` 从提交构建,`/health` 回显 `build_commit`。
- **入口策略回编排层**:gw-core `automode` / `batch_guard` / `request_screen` → `gw-app/src/worker/gates/`;配置类型留在 `gw_core::config::{automode, batch_guard}`(config.rs 改为目录);三份「跳过开头计费头」合并为 `gw_core::normalize::strip_leading_billing_lines`。
- **各上游的处理收回 Provider 接口**:新增 `openai_wire` / `affinity_scoped_by_client` / `suspend_lifecycle` / `cost_index_capable` / `enterprise_priced` / `refresh_exempt` / `validate_on_load` / `reclassify_retry_error` / `settings_fingerprints` / `render_upstream_payload` / `models_for_accounts` / `startup_warnings` / `region_from_profile_arn`,默认值即此前「其它上游」那条分支;`apply_hot_settings` 改为返回 `Result`,cursor / kiro 各自读自己的热设置。worker 生产代码里按上游名分支与直接调用具体上游均清零。
- **token 估算通用部分并入 gw-core**(`gw_core::text_tokens`),按模型分词器折算输出的部分留在 gw-kiro。
- **拆分 `worker/mod.rs`**:8316 行 → `mod.rs` 1016 + `startup` / `control` / `chat` / `errors` / `request_log` / `response` + `tests.rs` / `error_shape_tests.rs`;入口闸门函数并入 `gates/mod.rs`。

### 验证
- 每一步单独提交并跑全量测试;最终 2125 条全部通过(新增 2 条:计费头剥离、cursor 热设置报错)。
- 搬家核对:测试模块忽略行首空白后与原文逐行一致;生产代码按行多重集比较,只差模块声明 / 文件头与闸门函数里 6 行路径前缀。

### 未做 / 注意
- 未部署。`scheduler.rs`(7791 行,两套选号算法)、`gw-store/lib.rs`、`config/mod.rs` 按上游拆设置段留待下一轮。
- 健康接口:非 cursor worker 不再回显与它无关的 `cursor_guard_rev` 默认指纹(面板未使用该字段)。

## [screen-20261028] - 2026-10-28

### 背景
- 10-14 因 某下游用户的集中测试(接到的 4 个号 4~24 分钟内全被封)上线了「逐条筛查」:拒测试探针 + 拒超大输入,enforce 跑了 9 天。
- 该规则的源码只写在临时目录 `/tmp/opencode/caio-solo`,从未进主代码树、从未提交。10-23 部署 kimi-k3 时 worker0 从服务器源码重建,规则随之消失;之后的 `kimik3-20261023`、`automode-20261027b`、`notier-20261028` 二进制里都没有 `ProbeBlocked`。用户反馈「怎么又开始封号了」后查出。

### 改动
- 新增 `gw_core::request_screen`,接入 worker 请求入口(`request_screen_gate`,紧跟 `batch_guard_gate`、在选号之前),`/health` 新增 `request_screen` 计数。
- **探针**:单轮 + 不带 tools + 没有 system(剥掉计费头行后)+ `max_tokens ≤ 64` → 400 `ProbeBlocked`。
- **超大输入**:估算输入 > 150 万 token → 400 `OversizeBlocked`。估算覆盖 system / 文本 / tool_result(含对象形态)/ tool_use 参数 / thinking / 文本来源 document / 工具定义,不计图片与 base64 文档;超过阈值立即停止计数。
- 模式跟随 `batch_guard_mode`(线上 enforce),`batch_guard_allow_prefixes` 豁免;只筛带客户 key 的请求。不新增设置键。
- `batch_guard::prompt_head` 改为 `pub(crate)` 供复用。

### 设计取舍
- **超大输入阈值从原来的 100 万改为 150 万**。用线上 request_logs 校准:成功请求的上游实际输入最大 99.4 万(上下文封顶),本估算对大请求偏高最多 1.17 倍;按 100 万拦,9 分钟内就误伤 2 条实际 89 万、成功返回的长会话。150 万 ÷ 1.17 ≈ 128 万,超过它的请求上游本就装不下。
- **探针判据保持 10-14 原样**。线上回放 2000 条:`max_tokens ≤ 64` 的 161 条全部带 system(157 条 `## Context`、4 条 CC 起标题),零误伤;命中的 2 条是 haiku `max_tokens=1`、opus `max_tokens=32` 的探测。单轮无 system 的短任务(起标题、分类)会被拒,这是规则本身的取舍。
- 测试用线程级模式覆盖(`request_screen::override_mode`)跑 enforce 入口测试,不改全局,避免误伤并行测试;生产代码从不设置它。

### 测试与评审
- 新增 gw-core 10 条、gw-app 2 条(observe 接线 + enforce 入口拒绝);两次变异验证(删掉闸门 / 调用但不返回)都会失败。全量 2137 条通过。
- 对抗评审(opencode gpt-6-astra):采纳估算漏算对象形态 tool_result 与文本 document(high)、超限后仍扫全文、接线测试锁不住 enforce;「短任务误伤」按用户 10-14 定的规则保留。

### 部署
- 只换 worker0(`screen-20261028`),router 不动。回滚换回 `notier-20261028`。

## [notier-20261028] - 2026-10-28

### 背景
- 10-20 `[strict-tier]` 加了「档位 → 组内优先级自动归位」:worker 每次查额度(`backfill_subscription_title`)都把归属组成员边按档位改写,`KIRO POWER` → `0`(高),其余 → `100`(低)。判据只看档位,不分个人号和企业号,也没有和运维确认过。
- 10-28 导入的 5 个 Google 登录个人 POWER 号被自动抬进高层、接走全组流量,40 分钟内 2 个被降成 FREE。运维的规矩是个人 POWER 号只能低优先级;手工改回 `100` 后,几分钟内又被下一轮配额轮询改回 `0`。

### 改动
- **删除自动归位**:`backfill_subscription_title` 只写档位,不再动成员边。删掉 `WorkerState::reconcile_tier_priority`、整个 `gw_kiro::tier` 模块、`SqliteStore::retarget_membership_priority` 及其测试。
- 组内优先级从此只由运维设定(管理界面,或 `POST /admin/api/groups/{name}/members {"account_id","priority"}`)。
- 新增回归测试 `backfill_never_touches_member_priority`:个人 / 企业 POWER 在低层、PRO MAX 在高层、FREE 在层内手工值 7、被撤的边,走真实回填链路后全部原样不变。

### 设计取舍
- 曾先写过「只有企业号的 POWER 才升层」的收窄版,用户否决:优先级是业务决定,不该有任何自动改写。直接删除比收窄判据少一份会漂的规则。
- 10-20 那版要解决的「导入文件谎报档位、废号占高层」不再由系统兜底,改由运维导入后核对。准入层(`ci::decide` 的 gate)保留不动,它只读成员边,不写。

### 部署
- 只换 worker0(`notier-20261028`),router 不动。回滚换回 `automode-20261027b`。
- 上线前用守护脚本 `/root/kiro-check/power-low-guard.py` 每 20s 把个人 POWER 号拉回 `100` 顶着;上线后停掉。

## [automode-20261027] - 2026-10-27

### 背景
- 用户反馈「自动模式的分类器请求又直接发给 kiro 了」。线上抽样(最近 2000 条请求,22 分钟):分类器请求 **169 条**,本地接住的只有 **9 条(5.3%)**,其余全部打到上游,占同期积分 **8.5%**。
- 根因有两层:
  1. **回归,不是 CC 改提示词**:2026-10-23 的 `kimik3-20261023` 是在服务器源码目录 `/root/claude-all-in-one` 直接构建的,而那份源码里的分类器代码是 10-11 之前的旧版,丢了 `4c5a516`(判据逐块找前缀)、`bf5e210`(95% 缓存口径)以及「本地直答放在分组头校验之后」三处修复。CC 2.1.30x/31x 把 `x-anthropic-billing-header: …` 单独放在 `system[0]`,旧判据把各块拼起来只看开头 → 凡是带计费头的请求全部漏判。被接住的 9 条恰好都是不带计费头的。
  2. **CC 确实新增了一个变体**:二阶段复核请求。第一阶段(`max_tokens=64` + `stop_sequences=["</block>"]`)判 yes 之后,CC 再发一条**没有 `stop_sequences`**、`max_tokens=8192` 的请求,末段指令是 `Use <thinking> before responding with <block>`。只认 `stop_sequences` 的判据(含 10-11 的修复版)认不出它。抽样里 17 条。

### 改动
- `gw-core/src/automode.rs`:判据改为 system **逐块**找前缀(剥掉开头的计费头行);种类先看 `stop_sequences`;**完全没有**标记(缺字段/空数组)时,要求最后一条消息是 user、且其最后一段文本含生产原文 `Use <thinking> before responding with <block>.` 才认作判定类(评分类的无标记变体生产里没出现过,不收)。补回 `BILLING_HEADER_PREFIX` / `ALLOW_CACHE_HIT_PCT=95` 常量;删掉没有任何调用方的 `LocalUsage`。
- `gw-app/src/worker/automode.rs`:本地直答对下游的 usage 恢复 10-11 用户定的口径 —— `cache_read = 估算 × 95%`、`input = 其余`(两项相加正好等于估算);内部账(`real_cache_read_tokens` / `metering_credit`)保持真实的 0。
- `gw-app/src/worker/mod.rs`:分类器拦截挪到**分组头校验之后**、批量任务拦截之前(本地直答也要受「畸形分组头拒绝」约束)。

### 设计取舍
- 无标记路径**只认指令原句,不按 `<block>` 子串推断**(对抗评审 high,两位审查员一致):复用这段 system 的人完全可能在**讨论**标签(「解释一下 `<block>` 的作用」),那种请求必须走上游。另外只看末段(transcript 里的字样不算)、带了别的 stop 不认、带 assistant 预填不认。CC 再加新变体时先抓样再往 `STAGE2_INSTRUCTIONS` 里加。
- 二阶段请求本地答 `<block>no</block>`(不带 `<thinking>`):CC 的解析是「删掉 thinking 后 `<block>(yes|no)` 恰好一次」,thinking 不是必需的。正常情况下第一阶段已被本地放行,二阶段根本不会发出来;这里接住的是兜底。
- 只换 `worker0` 镜像,router 不动(分类器只在 worker 里执行),38991 入口不中断。

### 实测与其口径
- 用生产 `detect` 回放线上 2000 条请求,真值独立于实现(system 任一处含 security monitor **且**回答形如 `<block>`/`<severity>` 或为本地合成):**真阳 169 / 误报 0 / 漏判 0 / 真阴 1831**;其中 `</block>` 117、`</severity>` 35、无标记二阶段 17。
- `cargo test --workspace` **2130 条**全绿:gw-core 分类器单测 9 条(新增 3 条用生产原文形态,含评审给的误伤反例);worker 新增一条走真实 `handle_chat` 的入口测试,锁住「带计费头/二阶段都本地放行」与「畸形分组头先拒」。
- 收紧后再回放一次,结果不变(169/0/0/1831)。

### 评审
- 对抗评审(opencode `yapi/gpt-6-astra`,Skeptic + Architect)。采纳:无标记判据收紧(high)、入口级顺序回归测试(medium)。
- **未修(已知)**:流式本地直答走 `websearch::synth_sse`,其 `message_start.usage` 不带 `cache_read_input_tokens`,流式下游看不到那 95% 的命中量。生产抽样 216 条分类器请求**全部是非流式**,当前不触发;改 `synth_sse` 会动到 websearch 的共用路径,留作单独改动。

### 注意
- **服务器源码 ≠ 本地工作树**:本地多出未部署的 mojibake-notice 改动;这次是只把三个分类器文件覆盖到服务器源码后在服务器构建的。下次在服务器构建前务必先核对两边差异(这次的回归就是这么来的)。
- 开 `allow` 期间 CC 的这层安全闸等于关掉,这是 10-11 用户定的运营决策;面板切 `off` 30 秒内回退。

## [kimi-k3] - 2026-10-23

### 背景
- 池里有 8 个 BuilderId PRO MAX 号(`carolleem016` 等)长期显示「模型不可用×6」。查明**不是故障**:直打上游 `List-Available-Models`,8 个号目录完全一致、**9 个模型 0 个 claude**(`gpt-5.6-sol/terra/luna`、`deepseek-3.2`、`minimax-m2.5/m2.1`、`glm-5`、`qwen3-coder-next`、`kimi-k3`)。
- 排除过「profileArn 套错」:同一 token 三组对照 —— 不带 ARN → 400 `Invalid profileArn`;BuilderId 免费层 ARN(caio 实际发的那个)→ 200 且回 `KIRO PRO MAX 5000`;社交 ARN → 403 `Invalid token`。**只有 caio 正在发的那个能通**,所以 ARN 是对的,目录就是这么窄。这批号还查不了自己的真 profile(`ListAvailableProfiles` 一律 403 `User is not authorized to access this feature`),`force_discover_profile_arn` 那条自愈路对它们也走不通。
- 而 caio 的 `map_model()` 目标清一色 `claude-*`(子串兜底的词根也只有 sonnet/opus/haiku),于是这 8 个号在 caio 里**可用模型数 = 0**,只在反复消耗调度:被选中 → 400 `INVALID_MODEL_ID` → 标记不可用 → 换号,每次约 1.5s;标记 TTL 只有 6h,到期后整轮重来。

### 改动
1. **权威表接入 kimi-k3**(`gw-kiro/src/converter/model_map.rs`)。字段全部抄自真号 `List-Available-Models` 回包:`reasoning.effort` enum 只有 `low`/`high`/`max`(新增 `EFFORTS_KIMI_K3`,**没有 medium/xhigh**),`default: "max"`,1M in / 131072 out,`rateMultiplier` 2.4。`advertised_models()` 自动派生 `kimi-k3` 与 `kimi-k3-thinking`,`/v1/models` 随之带上。
2. **effort 字段路径按模型分流**。`KiroModel` 新增 `effort_schema: EffortSchema`(`OutputConfig` / `Reasoning`),11 个既有条目一律 `OutputConfig`,kimi-k3 为 `Reasoning`;`thinking_policy::additional_model_request_fields` 改为查表分发,新增查询函数 `converter::effort_schema_for`。
3. **非 claude 模型的准入改为 fail-closed**(`gw-kiro/src/lib.rs`)。`account_supports_model` 从一条规则变成三条:①账号级模型白名单(复用 `gw_core::account::model_allowlist_allows`);②非 claude 模型必须在白名单里**显式报名**;③opus 对 FREE 档的既有过滤。三条是**与**的关系。
4. 白名单匹配对**请求名**和归一后的**基础名**各试一次(`allowlist_allows`),所以运维写 `["kimi-k3"]` 就能同时覆盖 `kimi-k3-thinking`,不用写 `kimi-k3*`。

### 设计取舍
- **`effort_schema` 做成权威表的字段,而不是在构造字段处按模型名 `contains("kimi")` 判。** 后者会让「某模型走哪条路径」散落两处,迟早漂。发错路径**不报错**,只是强度设置静默失效(客户端以为在 max 档、实际按上游默认跑),是最难发现的那类缺陷,所以真理来源必须只有一个。
- **准入第②条单独写,不能只靠第①条。** `model_allowlist_allows` 对「没有这个键」返回 `true`(不限),而**线上 1469 个 kiro 号没有一个设过这个键**,只靠①等于全放行 —— kimi-k3 会被派给 1400+ 个目录里根本没有它的号。②把语义反过来:claude 是默认供应,非 claude 默认不供应、逐号报名。
- **第②条的判据落在「这次请求最终向上游要哪个模型」(`map_model` 的结果)上。** 第一版按**请求名的字面前缀** `starts_with("claude")` 判,自测时发现这是条 high:`anthropic/claude-3.5-sonnet-4.6`、`openrouter/claude-opus-5-preview` 这类带前缀的异名走的是 `map_model` 的子串兜底、今天能正常路由,按字面前缀会被判成非 claude → 要求显式报名 → **全池无号支持 → NoModelSupport**,把一类今天正常的请求直接打死。已加回归测试 `prefixed_claude_aliases_are_still_default_available`。
  也**不是**「查权威表里有没有这一行」:那会让「表里加了模型」自动等于「所有号都能接」,正是这条规则要防的事。
- **敢加第①条是因为存量为零。** 全库 `model_allowlist` 设置数:kiro 0 / cursor 59 个号里 0 / claude-dario 0。所以这条对现存 1469 个 kiro 号是逐字节无变化,只是让这个早就存在的通用键在 kiro 上也生效(cursor 一直在用)。
- **没有改成「按号存上游目录」**,那才是真理来源(每个号自己的 `ListAvailableModels`),但要新增 extra 字段、同步任务、失效策略。本次只上一个模型,白名单是现成的、可热改的、零新键的通道。上第二个非 claude 模型时再谈。
- **`signature_id: Unsigned` 是保守选择不是实测结论。** 判错的代价是多轮对话不回传历史 thinking(不报错、只降智);判反了(误设 Codename/Anonymous)会让上游验签失败、整条历史被剥。两害取轻。

### 实测与其口径
- 新增 23 条单测:`model_map` 7 条、`thinking_policy` 7 条、`converter::history` 2 条、`lib`(准入)7 条。先写测试跑出 RED 再实现;评审修复同样先写测试。
- 其中两条是**防回归**而非验新功能:`only_kimi_k3_uses_the_reasoning_effort_schema` 遍历权威表确认只有 kimi-k3 走 `Reasoning`;`claude_models_still_use_output_config_byte_for_byte` 遍历权威表逐个确认 claude 系线缆形态未变(`effort_levels` 为空的条目如 haiku-4-5 仍然一个字节都不发)。
- `cargo test --workspace`:**2112 条通过,0 失败**(基线 2089)。
- **本版未部署**(用户 10-23 明确)。所以这些改动**没有任何生产证据**,只有单测覆盖。它会和 `[apikey-region]` 那次的改动一起躺在工作树里,下次重建镜像时一并上线。

### 对抗评审回合(opencode / yapi 渠道 opus-5.5)
报了 1 high / 4 medium / 7 low,逐条核实后**全部采纳并修复**(只有一条建议未采纳,见末尾)。

- **H1 准入闸可被日常操作绕过**(已修)。第②条用 `contains_key` 判「有没有显式报名」,而 `gw_core` 对 `null` 返回「不限」。**面板清空白名单写的正是 `{"model_allowlist": null}` 且不删键** —— `admin/accounts.rs:1890` 的注释明写这一点,`accounts.rs:2991` 还有测试钉死。于是**任何一个设过白名单又清空的号**都会被当成「报了名」,非 claude 模型派给它 → 400 风暴,规模等于被清空过的号数。改成 `matches!(..., Some(v) if !v.is_null())`,补 `null_allowlist_is_not_an_optin`。
- **M1 生产跑 legacy 线缆,kimi 会收到 Anthropic 文本标签**(已修)。`docker inspect caio-worker0` 确认 `KIRO_LEGACY_WIRE=1` + `KIRO_THINKING_IN_HISTORY0=1`。`generate_thinking_prefix` 不区分模型,会往 history[0] 写 `<thinking_effort>`,legacy 词表还把 max 改写成 **xhigh** —— 那个档位不在 kimi 的 enum 里。改成对 `effort_schema == Reasoning` 的模型返回 None。**判据刻意不用 `requires_structured_effort`**:那对 opus-5.5 / sonnet-5.5 也为真,会改掉它们的 history[0] 字节,而那是缓存前缀第一块,改了所有在途会话下一轮全量 miss。配套测试 `anthropic_models_keep_their_text_tags_byte_for_byte` 遍历权威表逐条钉死字节。
- **M2 夹取在 kimi 上不单调**(已修)。旧规则「不在 enum 就回落 default」,而 kimi 的 default 恰好是最高档:请求 `medium` 换来 `max`,比 `high` 还强,叠加 2.4 倍率。这与本函数原本就写明的立场("宁可少一档,也不擅自升到 max")自相矛盾,所以**改的是规则不是立场**:缺档时取「不超过请求档的最高可用档」。对 Anthropic 系逐字节不变 —— 唯一缺档的 4.6 系请求 `xhigh` 时新旧都落 `high`,`clamp_rule_change_is_byte_identical_for_anthropic_models` 遍历权威表 × 全部合法档位钉死。
- **M3 tokenizer 注释说错了**(已更正,见「注意」段)。
- **M4 白名单只认两种写法**(已修)。`gw_core` 的契约是调用方传上游名(cursor 就是这么做),而 kiro 只试请求名与对外基础名,运维从上游目录照抄带点号的 `claude-opus-4.8` 会匹配不上 —— fail-closed 不越权,但会把号意外摘出轮转。现在三种写法都认。
- **L5 我的测试有实质问题**(已修)。① `lib.rs` 有两个 `#[test]` 叠在同一函数上(插入时插进了前一个属性和它的函数之间),该测试被注册两次 —— 之前的输出里它确实出现过两遍。② `kimi_k3_thinking_is_unsigned` 删掉 kimi 整行也能过(表外模型回落值也是 Unsigned),改成断言权威表那一行。③ `claude_models_still_use_output_config_byte_for_byte` 名字说逐字节、实际只查键存不存在,也没跑生产真正走的 `legacy=true` 路径 —— 现在精确比对 effort 取值并把两种线缆形态都跑一遍。④ 补了子串异名(`effort_schema_for` 回落分支唯一真正起作用的场景)与「kimi 不带 thinking 不发字段」两条。⑤ 「防批量改漏」的说法不成立(结构体字面量漏字段是编译错误),注释改成「防填错值」。
- **L6 调度热路径上的分配**(已修)。`is_default_available_model` 原先走 `map_model`,每号每次求值 3 次分配,而一次选号要对全组 1400+ 个号求值、`gw_core` 的准入谓词契约是锁内零分配。新增 `converter::routes_to_anthropic`:权威表命中走表零分配,未命中才退子串兜底。
- **L7 可见性**(已修)。生产代码直接摸 `converter::model_map::resolve_base` 偏离了统一 re-export 的惯例。现在 `resolve_base`/`routes_to_anthropic` 进 `pub use`,`mod model_map` 收回私有,权威表本体改成 `#[cfg(test)] pub(crate) use` 只给测试遍历。
- **L1/L3/L4 记录未做**:`effort_drift` 不比对 schema 路径(`models_api.rs` 已解析出 `effort_schema_path`,没用上);`kimi-k3-thinking` 经预算映射反而比裸名浅;`supportedInputTypes` 只有 TEXT/IMAGE 而 converter 照发 documents,PDF 打到 kimi 的行为未验证。
- **唯一未采纳的建议**:评审建议 tokenizer 先改 `Legacy`(ASCII 系数更保守)。在标定之前,用一个同样没依据的值替掉另一个没依据的值不叫改进,保持 `Current` 并把不确定性写进注释,等真号标定一次再定。

### 注意
- **上线后还要做一步才会生效**:给那 8 个号打白名单 `PATCH /admin/api/accounts/{id} {"extra":{"model_allowlist":["kimi-k3"]},"extra_merge":true}`(热改)。在那之前它们照旧接不到任何活。部署前设没用 —— 当前线上二进制的 kiro provider 根本不读这个键。
- **kimi-k3 的倍率是 2.4**,PRO MAX 5000 积分约合 2083 次请求,8 个号合计约 16600 次就烧完。
- 上游把它标为 **"Kiro Excluded Model"**,含义未查证(可能涉及数据保留/赔偿条款与 Anthropic 系不同)。对外卖之前值得先看一眼 Kiro 文档。
- `tokenizer: Current` 是**默认档不是标定值**,非 claude 分词器没做过字符→token 真机标定。⚠️ **初稿在这里写错了**:它不是「只影响展示」—— 这个值经 `chat.rs` 的 `tracker.output.tokens()` 进 `billing::finalize`,**就是向客户计费的 output**。不影响 Kiro 收我方的钱(那边按自己的 metering 事件算),影响的是我方收客户的钱,系数偏高即多收。kimi 这类大词表分词器在中文上大概率比 claude 低。
- `promptCaching.supportsPromptCaching` 为 true,但上游给的两个 checkpoint 参数都是 `null`,cache_point 对它的行为**未验证**。
- 为让测试能遍历权威表,`converter::model_map` 的可见性从 `mod` 放宽到 `pub(crate) mod`(仅 crate 内)。

## [apikey-region] - 2026-10-22

### 背景
- 用户导入一个 `ksk_` 号,面板显示「凭据失效」,**删号重导依然如此**,一度判成「号坏了」。号商给的 JSON 是分离区号写法:

  ```json
  {"version":"kiro-rs-import","accounts":[{"authMethod":"api_key",
    "kiroApiKey":"ksk_…","authRegion":"us-east-1","apiRegion":"eu-central-1"}]}
  ```

- 只读探测把号本身摘清楚了:`management.eu-central-1.kiro.dev` 两个端点都 200(KIRO POWER、0/10000、18 个模型含 opus-5.5),同一把 key 打 `us-east-1` 两个端点都 403 `Invalid token`。**号是好的,打错区了。**
- 根因:`map_flat` 认出 `kiroApiKey` 后走 `map_api_key`,而**这条路径只读扁平 `region` 一个字段**,`apiRegion`/`authRegion` 一律不看。会正确处理分离区号的 `insert_region_split` 只在非 apikey 分支调用。于是 extra 里一个区号都没有 → `usage_limits.rs` 与 `chat.rs` 双双回落 `DEFAULT_REGION = us-east-1` → 每个请求 403 → scheduler 判 `TokenInvalid` 永久禁用 → UI 的 `accounts.status.invalidToken`。
- 删号重导无效是因为 JSON 形状没变,region 照样丢。这个洞一直没暴露,是因为 drop 那家的号都在 us-east-1,默认值恰好是对的。

### 改动
1. **`map_api_key` 读分离区号**(`gw-kiro/src/import.rs`):新增 `insert_api_key_region_split`,复用 `insert_region_split`(同一份真理来源),读 `region` / `api_region` / `auth_region`,snake 与 camel 都认。顺带:扁平 `region` 形态现在也显式写 `api_region`;未知区(非 `STANDARD_PROFILE_REGIONS`)归一到 `us-east-1`,与非 apikey 分支同口径(改动前原样存,会让请求打到不存在的 `management.ap-southeast-1.kiro.dev`)。
2. **区号字段改用新的 `flat_region_str`**(两条分支都改,不留不对称)。`flat_str` 只要 snake 键存在且是字符串就返回**哪怕是空串**、不再回退 camel;对区号来说,`{"api_region":"", "apiRegion":"eu-central-1"}` 会因此被判成「没带区号」→ 回落 us-east-1 → 欧洲区号每个请求 403。新函数按「两种拼写都试、取第一个 trim 后非空」取值,只用于区号三兄弟 —— 别的字段丢了只是少个展示值,区号丢了是整个号不可用。
3. **合并层加耦合字段守卫**(`gw-app/src/admin/accounts.rs`):现账号已有非空 `region` 时,智能合并不再单独补 `api_region`。

### 设计取舍
- **加了一道「有才写」的闸,而不是无条件调 `insert_region_split`。** 三个区号字段全缺时直接返回,extra 保持不含区号。理由:drop 那家的号不带区号,`restock::onboard` 走的就是这条路,而 restock P1 敢单独上线的全部依据是「无区号的号经过新载荷后与裸串路径逐字节等价」,`restock/engine.rs` 有一条 `assert!(!extra.contains_key("region"))` 钉死它。无条件写 `region: us-east-1` 虽然**行为**等价(下游一律回落同一个默认值),却会推翻那条不变量,把一次只影响欧洲区号的修复变成全量改号。
- **`profile_arn` 固定传 `None`。** apikey 凭据永不带 profileArn(`headers::resolve_profile_arn`),服务端按 key 自身解析账号。即便号商在同一条目里塞了 arn,我方也不会发它;拿它去覆盖显式 `apiRegion` 只会多一个意外来源。
- **认证区照搬进 `auth_region`。** 已逐条核过 apikey 不会走 OIDC 刷新:自动续期在 `worker/mod.rs` 判免刷新、配额 401/403 兜底排除免刷新账号、手工 `force_refresh` 也在 `gw-kiro/src/lib.rs` 的 `refresh_auth` 里对 apikey 先 early-return(早于 `token::refresh_auth`)。所以这个字段对 apikey 是纯记录,留着只为排障时看得出号商声明了什么。
- **第 3 条的守卫放在合并层,而不是在 import 里少写一个字段。** `region` 与 `api_region` 由 `insert_region_split` 写成**同一个值**,两者从不冲突;冲突只由「只补缺失字段」这条合并规则对耦合字段的误用产生:现账号 `region=eu-central-1` 缺 `api_region` 时,导入一份服务区算成 us-east-1 的 JSON → region 因已有而不覆盖、api_region 被补成 us-east-1 → **chat(读 `api_region > region`)打美国、配额与模型目录(只读 `region`)打欧洲**,两边分家且没有任何报错。所以修在规则那一侧。反之不拦:现账号三个区号全缺时整组补齐,这正是「不删号自愈」的路径。
- 这是**既存**隐患,不是本次引入 —— 非 apikey 分支一直在写 `api_region`。本次改动把 apikey 也纳入,而 apikey 恰好是运维最常手工补 region 的那类号,所以一并堵掉。

### 实测与其口径
- 新增 11 条单测(`import.rs` 9 条 + `admin/accounts.rs` 2 条)。其中 `apikey_separated_regions_land_in_extra` 是本次生产事故的回归测试,`apikey_without_region_info_is_unchanged` 是那道零行为变化的闸,`import_merge_does_not_split_region_from_api_region` 与它的反面各钉一条。先写测试确认 RED 再实现转 GREEN。
- `cargo test --workspace`:**2089 条通过,0 失败**。含 `restock/engine.rs` 那两条逐字节等价测试原样通过。
- **分区脑裂的真实暴露面已量过**:线上 75 个号是「有 region 缺 api_region」形态(46 个无 auth_method + 19 个 apikey + 其余 IdC/social/external_idp),**全部 disabled=1**;启用中且 region ≠ us-east-1 又缺 api_region 的是 **0 个**。所以这条是潜在的、不是正在发生的 —— 但复活那 12 个欧洲区死号正好会踩中。
- **线上那个号不是靠这段代码修好的**,是热改绕过去的:`PATCH /admin/api/accounts/{id} {"extra":{"region":"eu-central-1","api_region":"eu-central-1"},"extra_merge":true}` + `POST .../reset`。reset 后 10 秒开始接活,10 分钟 40 次成功 0 失败,`subscription_title` 自动回填 `KIRO POWER`。
- **本版代码未部署**(用户明确:流量太大),所以这三条改动的生产证据为零,只有单测覆盖。
- 对抗评审走 opencode/`gpt-6-astra`(codex 的 refresh token 仍是 revoked,`codex exec` 401)。它报的两条 medium 就是上面的改动 2 与改动 3,low 档为空。

### 注意
- **按用户要求不部署。** 下次重建 worker0 / router 时会随镜像带上。在那之前,再导分离区号的 `ksk_` 号仍需手工补 region —— 最省事的办法是导入前在 JSON 条目里加一个扁平 `"region": "eu-central-1"`,旧代码就认这个字段。
- 导入只在**映射**时生效,不会追认已落库的号;存量错号要么 `extra_merge` 热改,要么等部署后重导一次(合并会补齐缺失的区号,**不必删号**;但它不负责清掉已有的禁用状态,还得补一次 `POST .../reset`)。

## [strict-tier] - 2026-10-20

### 背景
- 用户导入一个 POWER 企业号(`G0 @ priority 0`,20 并发),发现「流量不往上调度」。排查:号本身完全健康 —— 运行态 ALIVE、无冷却无封禁、0 失败、额度 28%、**18/20 个并发位空着**,而且它是 `G0` 最高优先层里**唯一一个活号**(该层另外 180 多个全是 disabled)。它只拿到 8% 流量,剩下的被 10 个 `priority 100`、permit 全满的 PRO MAX 吃掉。
- 根因是「优先级」在 cost_index 算法里只做了一半:`ci::index()` 的公式里**没有 rank**,rank 只决定「冷候选能否入围」。于是形成一个 30 分钟周期的来回:

  | 步 | 状态 | 结果 |
  |---|---|---|
  | 1 | 线程 30 分钟内没碰过高优号 → 它在冷池、rank 0 | `top_now=0`,低层热号被剔 → 强制迁上来 |
  | 2 | 跑完一轮,它对该线程变成 `touched` → 退出冷池 | `top_now` 退回 100,剔除条件 `100 > 100` 为假 → 谁都不剔 |
  | 3 | 纯比指数,企业号单价 3 倍 | 热个人号 1.44 对热企业号 3.96 → **被弹回低层** |
  | 4 | `MEMORY_TTLS`(6×300s)过期 | 回到第 1 步 |

  实测稳定在每线程每 30 分钟迁一次,约 4 请求/分钟。
- 生产口径(输入中位 30 万 token、κ=0.2):热个人号指数 1.44,冷企业号 11.78,差 10.34 积分;要热号等 **51.7 秒**才轮到企业号,而等待预算只有 30 秒 —— 结构性赢不了。

### 改动
- **`ci::decide` 新增准入层**:`gate` = 此刻**能立刻起跑**的候选里最优的 rank;有它时更差层的候选整体不参与比较。组内优先级因此成为硬约束,代价指数退回只在层内优化。`index()` 公式、层内定价/热度/平局规则**一字未改**。
- **删掉 `select_ci` 里原有的 `migrate_up && rank > top_now` 入围子句**。它被准入层严格蕴含(`top_now` 那个冷候选恒在 `picks` 里且 `wait == 0`,故 gate ≤ `top_now`),留着就是两处各算一套层;以后给准入层加迟滞会被它悄悄抵消。`top_now` 变量本身保留 —— 它还负责「有空位的最高层里能立刻起跑的冷号」这个入围名额。
- 由 `affinity_migrate_up` 控制,**不新增设置键**:「允许老会话向上迁移」与「更优层优先」本来是同一件事的两种说法,拆成两个旋钮只会出现「允许迁移但不认层」这种没有意义的组合。
- **档位 → 成员边优先级自动归位**(`gw_kiro::tier` + `store.retarget_membership_priority`):`KIRO POWER` → 高层 `0`,其余 → 兜底层 `100`。挂在 `worker/mod.rs::backfill_subscription_title`,即 `getUsageLimits` 回包的落库点。

### 设计取舍
- **准入层放在 `ci::decide` 这个纯函数里,而不是过滤 `select_ci` 的 `picks`**。`gate` 只从 `wait == 0` 的候选里取,那个候选自己恒满足 `rank <= gate`,所以**过滤后集合不可能变空** —— 换成过滤 `picks` 则存在「draining 且已过 TTL 的 rank-0 号把候选清空」的路径,而空候选会让 `select_ci` 返回 `Exhausted`、静默退回旧版循环(不是 503,但新语义白写),得再写一个 fail-open 兜底分支。纯函数无锁,边界也能直接单测。
- **归位按成员边的当前值对账,不按档位字符串的变化**。第一版按「档位变了吗」判,对抗评审查出这是条 high:`import.rs` 导入时就把文件里的 `subscriptionTitle` 写进 extra,所以一个文件里写对了 POWER 的号**档位从头到尾没变过**,边却一直停在导入默认值 —— B 在最常见的路径上完全不生效。另外「`before = None` 就等于 100」也不成立:成员边初值是 `COALESCE(extra.priority, 100)`,以 `priority: 0` 导入的号一开始就在高层。按状态对账顺带是幂等 + 自愈的:写库失败或进程中途被杀,下一轮照样重试。
- **只在跨层时写,且只 UPDATE 不 INSERT**。跨层才写 → 层内的手工微调(高层里把某个号调成 @7 排同层最前)不会被每轮归位压平。只 UPDATE → 运维把号从某组撤下之后,自动归位不会把边插回去;成员边是授权(谁能用这个号)而不是纯排序,重建等于**扩大授权**。判定与写入在同一事务里,不存在「读到旧值 → 期间被改 → 写回陈旧值」的窗口。
- 代价:一个被刻意放在高层的**非 POWER** 号会被压回兜底层。这正是「导入文件谎报档位、废号占着高层」要防的场景 —— 有一批文件写着 `KIRO POWER` / 额度 10000,live 查出来是 `KIRO FREE` / 额度 50 且已用超;照文件定档等于把废号直接塞进高层,现在 rank 0 层堆着 180 多个死号大概就是这么来的。
- **档位匹配是精确匹配而非 `contains`**,与同仓 opus 能力过滤那条 `!contains("FREE")` 方向相反:那条是放行、宁松勿严;升层意味着让一个号接走全组流量,必须 fail-closed,认不出的值(cursor 写进该字段的 `"USD"`、拼错、`KIRO POWERFUL`)一律落兜底层。另按 `account.provider == "kiro"` 显式设闸 —— fail-closed 只是「恰好不会出事」,不等于「说清了不该做」。
- **只动归属组那条边**(`accounts.group_name`)。同一个号在别的组里可能是兜底角色,那些边归运维管。

### 实测与其口径
- 本次改动**没有**生产数据。线上先用热设置验证过的是另一个组合:`affinity_migrate_up` false→true **加** `cost_index_enterprise_price` 3.0→1.0(旧代码),POWER 份额 8% → 53.6% → 75%,每分钟 8→29,0 失败,额度 28%→38.5%。`ci_cold_move` 8 分钟仅 +4 而 `ci_warm` +222,说明迁移是一次性的、迁过去的线程留住了。
- 严格分层这段代码本身只有单测覆盖(`cost_index` 5 条、`scheduler` 3 条、`gw-store` 1 条、`gw-kiro` 2 条、`worker` 2 条;既有 26 条 CI 路径测试原样通过)。上线后把 `enterprise_price` 改回 3.0 做对照、份额不掉,才算证明跨层已由 rank 硬管。
- **回滚不是回到 53.6% 那个状态**。`affinity_migrate_up=false` 会同时关掉准入层和旧版路径的向上迁移,退回的是 8% 那个状态。想要中间态只能靠 `enterprise_price`。

### 注意
- **严格分层会把流量集中到最高优先层。** POWER 号单号 RPM 从 4~11/分钟升到 29+,而 10-13 那批被封号被封前最忙一分钟是**中位 7 / p90 10**(见 `[batch-guard-shape]`)。本版按用户要求不加限速;要加闸是热改该号 `extra.rpm_limit`,打满后它退出准入层、个人号自动顶上,不堵客户。
- **自动升到高层会豁免新号暖机**:`WARMUP_MIN_RANK = 100`,rank < 100 完全不限速(那是刻意设计,高优号是花钱买的主力)。但 B 现在会让一个只导了 rt 的 POWER 新号在首次配额查询后几秒内升到 0,于是专为「新号上线被灌满」设的两期暖机对它自动失效。生产当前 `warmup_enabled=false`,所以这条是潜在的、不是正在发生的。
- **跨层不再考虑 429 风险**:`p429·(κ·3+η)` 这一项原来能把部分流量从正在 429 的号引到低层,现在高优层只要能起跑就优先。排队型企业号撞 429 后只被节流(熔断阈值 0,永不进冷却),节流一结束准入层就弹回高层。符合「能立刻起跑就优先」的意图,但等于把分流交给了 pacing。
- **高优层饱和线附近有冷启动代价**:空出一个 permit 时,第一个到达的低层热线程会被强制迁上来,可能把一个原本热在高层的线程挤到等待。旧版路径有 `MIGRATE_UP_DEBOUNCE` 去抖,准入层是个没有迟滞的阶跃函数。没加去抖是因为:每个线程被拉上来**一次**之后就留在高层(它自己就是 rank 0),不构成往复;生产 8 分钟 117 个线程只产生 4 次 `ci_cold_move`,与往复不符。真撞上持续饱和,先抬该号的 `max_concurrency`。
- 存量 1387 个号的成员边不在本次代码范围内:用现成的 `POST /admin/api/groups/{name}/members/bulk {owner, subscription_title, priority}` 一次性归位。⚠️ 它按 `subscription_title` **精确字符串**比较(`bulk_add_members` 的 SQL),不做 `gw_kiro::tier` 那套空白/大小写归一,所以多空格变体会漏 —— 批量归位后要核对一遍条数。
- 档位自动归位**没有热开关**(映射是常量),这是为了不新增设置键、不连带重建 router。要停只能换回旧镜像,或用 `members/bulk` 把边改回来。

## [import-dedup] - 2026-10-16

### 背景
- 用户报「上了 15 个号,面板只多了 11 个」。`account_id` 由邮箱派生,但**同一个 gmail 分别用 Google 和 GitHub 登录 Kiro 是两个真号**,它们派生出同一个 ID。老逻辑两道判据对社交号全都不触发,整条跳过、号白买:
  - 孪生判定只比 `client_id` —— 社交号根本没有 `client_id`(只有 IdC 企业号有)。
  - 碰撞防护按「user_id 优先,否则 email」各取一个再比 —— 一边有 userId、另一边只有 email 时,拿 userId 去和 email 比必然不等。
- 全量对账(32 个导出文件 × 148 个唯一账号 vs 库里 1334 行):文件里有而库里没有的 0 个,同一 user_id 占两行的 0 个;但有 **221 个号既没存 user_id 也没存 client_id**,两道判据对它们全瞎。

### 改动
- `distinct_identity()` 取代 `identity_of()`:**同名字段对同名字段比**。两边都有 userId 就只看 userId(上游稳定身份,权威);都没有才退到邮箱。对不上号的组合一律判「无法确证」,交给调用方走合并而不是整条丢弃 —— 合并只补空字段、不碰 token,误判的代价远小于丢号。顺带让旧导出(没带 userId)重导时把缺失的 user_id 补进库,上面那片盲区随日常重复导入自愈。
- **派生新 ID 之前先查 `user_id → account_id` 全库索引**。同邮箱的第二个真号上次已按 `base-2` 收下,再导同一份文件时邮箱仍派生出 `base`、身份一比不同 → 又建 `base-3`,重导几次就是一串持有**同一个上游账号**凭据、却被分到不同出口的重复行 —— 正是反关联最怕的(上线后实测踩到)。现在按 user_id 找到已有那行直接走合并。
- **不同上游账号不写 `twin_of`**。`twin_of` 会让 worker 在自己 RT 失效时借源号的 token 链,两个真号互借等于两行共用一个上游账号。只有 `client_id` 那种「同一上游账号的第二条链」才保留。凡因撞 ID 改过 ID 的记 `derived_from`,仅供导入结果对账,不影响运行时行为。
- 导入结果新增 `duplicate`:「库里已有且没有任何可补字段」从 `skipped` 里拆出来,`skipped` 只留非法 ID / 并发已存在 / 派生耗尽。以前混在一起,每天重复上号的人分不出哪些是真出了问题。

### 面板(admin-ui)
- **取消导入后自动验活**:逐号刷 token + 查配额、串行,worker 没同步还要重试,号一多就是分钟级。改成结果页上的按需按钮。
- 结果页换成三个大数(新增上号 / 库里已有 / 没收下,非零变红)+ 小字说明构成;清单里没收下的排最前并带红色原因。口径逐条从 `items` 算,兼容老后端。

### 注意
- **自动补货不走这套判重**:`restock/engine.rs` 直接 `store.create_account()`(INSERT OR IGNORE),撞 ID 只打一行 warn,买来的号会直接丢。

## [batch-guard-shape] - 2026-10-14

### 背景
- 上一版(`[batch-guard]`)按「system 开头 + 只数单轮」计数。但 Claude Code、OpenCode、Agent SDK 这类通用客户端**所有用户共用同一句开头**,子代理的第一条请求也是单轮 —— 一个用户 10 分钟内开 40 个以上子代理,就会把整个池子的 CC 流量判成批量任务、拦 6 小时(含正在进行的多轮对话)。上线当晚即切回 `observe`,改判据。
- 生产形态对比(10-13 晚两段共约 2.5 小时、约 4,000 条):

  | 模板 | 单轮占比 | 流式 | 带 tools |
  |---|---|---|---|
  | VFX 审图批量任务 | 100% | 0% | 0% |
  | CC / OpenCode / Agent SDK / TraeCode / Grok CLI 等十几种编程客户端 | 0~3% | ~100% | ~100% |

### 改动
- **只数脚本形态**:单轮(`messages.len() == 1`)且不带 tools(非空 `tools` 数组)。编程客户端的主对话是多轮、子代理带 tools,都不计数。
- **加占比门槛**:最近 `threshold` 条脚本请求都在窗口内,**并且**这段时间跨度里同模板全部请求中脚本形态占 ≥ 90%(代码常量 9/10),才判定。通用客户端偶尔发的无 tools 单轮辅助请求(起标题、摘要)会被它自己的对话流量稀释。对话流量只在模板已建计数表后才记,队列上限 `threshold`(占比门槛 ≥ 50% 时数满即可否定)。
- **判定后只拦脚本形态**:多轮、带 tools 的照常放行,误判也掐不断进行中的对话。手动黑名单 `block_prefixes` 仍拦所有形态(运营意图明确)。
- **手动黑名单按规则单独记**:已判定表里按规则原文建键,不再用请求的归一化指纹 —— 否则归一后撞指纹、原文却不匹配规则的请求会被连带拦掉(上一版按 80 字符截断时也有同样问题:规则长于 80 字符时,只有前 80 字符相同的请求也会被拦)。
- **模板归一**:一段连续十六进制字符,纯数字(编号、日期)不论长短、或含数字且 ≥ 4 位(随机标签、哈希),换成 `#` 再取前 `prefix_chars` 字符。生产里有脚本每条带随机标签(`[session e09b26bff4be] …`),不归一会被拆成无数个模板;递增编号(`Item 7`)同理。不含数字的单词(`decade`)和短混合串(`a1`)不动。手动名单仍按原文开头匹配。
- **热改 `threshold` / `window_secs` 清计数表**(保留已判定):计数队列按旧阈值截断、按旧窗口清理,拿它按新参数算占比会误判。
- 没有新增设置键;router 与面板不用动。

### 验证
- 单测 28 个 + 1 个默认跳过的回放(新增:子代理扇出不触发、占比门槛 4/5 条对话流量的边界、跨度外对话流量不稀释、对话流量队列有界、随机标签与编号合并、归一保留单词且超长串只缓存需要的、手动黑名单不看形态且不扩散、热改的作废范围)。
- 对抗评审(opencode + gpt-5.6-sol,Skeptic / Architect 两个视角):采纳 4 条(手动黑名单扩散、热改阈值误判、编号未归一、超长十六进制串整段复制),均已修。未采纳:「计数是单进程的」—— Kiro 池只由 worker0 一个进程服务;「阈值 10 万可撑爆内存」—— 每个时间点对应一条真实到达的请求,实际上限由流量决定;「全局锁瓶颈」—— 锁内微秒级、当前约 1 条/秒;「模板超 4096 个时淘汰丢计数」—— 只有脚本形态才建表,10 分钟远到不了。
- 生产回放(`replay_production_sample`,`#[ignore]`,用 Rust 实现本身逐条喂两段导出样本):阈值 40 只判中 VFX;阈值 20 多一个反复问同一问题的测试脚本;阈值 10 再多两个带随机标签的脚本 —— 任何阈值下都没有判中编程客户端。

### 注意
- 盲区:经带 tools 的 agent(如 `claude -p` 循环)跑的批量任务、没有 system 且 user 开头每条都不同的脚本、伪装成多轮或挂一个用不上的 tool 的脚本,都不会被计数识别,只能靠 `block_prefixes`。同一共享开头下混入少量正常流量也能把占比稀释到 90% 以下。**这道防线挡的是不知情的批量脚本,不防刻意规避** —— 堵这些洞的代价是误伤交互式客户端。
- 残余误伤风险:不带 tools 的网页/手机聊天客户端,如果大多数用户只问一句就走(没有后续多轮稀释),10 分钟内 40 个新对话会被判定、之后 6 小时它的新对话首问被拒。observe 期间看面板确认;真出现就把它的开头加进 `allow_prefixes`(热改)。
- 回放样本只有晚间约 2.5 小时(request_logs 只留最近 1 万条),没有白天高峰数据。上线先 `observe` 跑一天,看面板「已判定模板」没有误判再切 `enforce`。

## [batch-guard] - 2026-10-14

### 背景
- 2026-10-13 22:45–23:24(+08)Kiro 封号潮:40 分钟内 13 个新号首次 403「unusual user activity」,另有 3 个号只被降到 FREE。同晚 21:10 起出现一个 VFX 审图批量任务:system 固定开头「You are a strict, calibrated senior VFX art director…」、单轮、非流式、每条 1~4 张图,48 分钟 770 条,分散到 29 个号;它停下(23:24)后封号也停了。
- 按单号比较区分不出被封号与幸存号(被封号接的 VFX 反而更少、RPM 更低),但用户多次因批量处理封号 —— Kiro 像是把「跑同一个自动化任务的一组号」成批处理。所以**限单号速率没用,只能在入口把批量任务整体挡掉**。
- 单号 RPM 参考数据(被封号被封前,21:00 起):最忙一分钟请求数中位 7、p90 10;5 分钟平均 RPM 中位 3.0、p90 4.6。同批幸存号:中位 9 / 4.2(更高)。

### 改动
- **批量任务拦截** `gw_core::batch_guard`(新模块,热设置 `batch_guard_*`,30 秒生效):
  - 指纹 = system 第一个非计费头文本块的前 `prefix_chars`(默认 80)字符;没有 system 取第一条 user 文本开头。只给单轮请求计数;全池滑动窗口 `window_secs`(600)内达到 `threshold`(40)→ 判定 `block_secs`(21600 = 6h),期间该模板的请求(含多轮)按模式处理。
  - `block_prefixes` 命中即判定;`allow_prefixes` 命中即放行;CC 自动模式分类器前缀内置豁免。
  - 模式 `off` / `observe`(**代码默认**,只记录)/ `enforce`(拒绝)。拒绝点在选号之前,不占号、不碰上游;回 400 `invalid_request_error`,文案避开下游 new-api 的自动禁用关键词。请求日志 `error_kind=BatchBlocked`(面板琥珀徽章 + 整行琥珀底)。
  - worker `/health` 新增 `batch_guard` 实况(模式、累计判定/拦截数、已判定模板开头/剩余时间),经 `/admin/api/accounts/runtime` 透传;设置页新增「批量任务拦截」卡片(配置 + 实况)。
- **小请求不保底** `cache_floor_min_input`(热设置,默认 0 = 旧行为):总输入低于它的请求保底为 0(sim / credit_correct 冷判 / credit_fit 三条路径都改),≥ 它的仍用 `cache_floor_ratio`。生产计划设 100000。
- **账号页 RPM 列**:显示 worker 滑动 60 秒的 `rpm_used`(设了上限时 `used/limit`),≥7 标黄、≥10 标红;排序新增「RPM 高→低」。**只显示不限速**(用户定)。

### 设计取舍
- 按模板而非按单号:证据显示 Kiro 的判定不在单号粒度;按模板拦是唯一能在批量任务刚出现时就止损的口径。gpt-5.6-sol 第二意见建议按租户分桶 —— caio 侧只有一把客户 key(下游 new-api 汇聚多客户),做不到,故只靠阈值留余量(生产正常模板 ≤13 条/10 分钟,批量 ≈160)。
- 400 而非 429:429 会被 new-api 重试放大;401/403 带关键词会触发自动禁用整条渠道。
- 热切换不清状态:observe 期间判定的模板切到 enforce 立即生效;重启清空(纯内存)。
- 保底改造影响(按 10-06~10-13 回放):<100k 且贴保底的请求 7,399 条/周,改后每周多收约 ¥370(+1.1%);≥100k 不变。

### 注意
- 已判定表与计数表都在进程内存,worker 重启即清空。
- 多轮的批量任务、或每条模板开头都不同的批量任务不会被计数识别,只能靠 `block_prefixes` 手动拉黑。
- 部署要同时换 router(设置白名单要认新键 + 面板)与 worker0(拦截与计费);旧镜像 worker(cursor/bot/dario)读到新键只告警不受影响。

## [dario-sidecar-no-pool] - 2026-10-12

- **dario 组经 router 全灭(0ms 502 Network)的根修**:`ctx.session_id` 是组命名空间亲和键(`组\0键`),dario 是唯一把它塞进 HTTP 头(`x-session-id`)的 provider —— **NUL 是非法头值**,reqwest 直接 builder error。直连 worker(不带组头)永远正常,经 router(必带组头)必挂,所以排查时「直连通、网关挂」。修复:进头前把不可见字符折叠成 `~`(它仅是 dario 侧 `--session-per-client` 的会话键,确定性即可)。
- **回环 client 不再留空闲连接池**(`pool_max_idle_per_host(0)`,worker 与子机 agent 同改):sidecar 的 bun 对 keep-alive 空闲连接关闭得快,复到死连接会让 POST 瞬败;回环建连亚毫秒,池化没有收益。
- **瞬时失败留证据**:调度器 `Network/ServerError/Other` 分支此前只累加计数不记文案(本次排查前期 8 次失败零日志,全靠盲猜),现在 WARN 记 kind + 上游错误原文。

## [oauth-json-exchange] - 2026-10-12

- **换码报文改回 JSON + `state`,与官方客户端字节对齐**:上游 token 端点已拒绝 form 表单形态(400 "Invalid request format" —— 换码在格式层就被拒,真假 code 一视同仁)。对着本机 CC v2.1.296 二进制逆向确认:`axios.post(TOKEN_URL, {grant_type, code, redirect_uri, client_id, code_verifier, state}, Content-Type: application/json)`。`state` 参数自此贯穿 admin → worker 与 admin → hub → 子机 agent 全链(各环节缺省空串兜底,老对端不炸但换码会失败——子机要换码需用新安装包重装/升级)。
- 影响面:仅 `authorization_code` 换码;refresh 链路(仍 form)未动 —— 它一直正常,不动能跑的东西。

## [oauth-err-text] - 2026-10-12

- **换码错误透出真实原因**:Claude token 端点的错误报文里 `error` 字段可能是**对象**(API 形态,如限流 429 的 `{"type":"rate_limit_error","message":"…"}`)而非字符串,旧的字符串解析把它吞成空串,面板只剩「换码 400 」无从下手。现两种形态都挖出 message/type 与 `error_description`,形如 `dario oauth 换码 429: Rate limited. Please try again later.`。

## [node-install-short-url] - 2026-10-12

### 改动:安装命令再瘦身 —— `curl -sSL <对外地址>/i/<短票据> | sudo bash` 一行
- **新增公开口 `GET /i/{ticket}`**:返回一段内嵌票据、sha256 校验、`--master/--fp` 的安装脚本;下载 + 校验 + 执行全在脚本里,用户要敲的只剩一行(约 90 字符,原来约 450)。后台「新建子机」同时给两条:有 sudo(系统级服务)/ 无 sudo(用户态 ~/.local,脚本里自动转交 install-user.sh)。
- **票据变短**:密钥段从 32 字节收成 12 字节(base64url 16 字符),整条 URL 66 字符上下。
- **票据即安装凭证**:脚本 `--token` 传的就是票据密钥(去 `caiob1.` 前缀正好是短令牌形态),不再把长期加入令牌放进命令。hub 的 join 校验在令牌摘要对不上时回落认票据密钥,且**不看过期**(装好的子机拿它当长期密钥重连);防克隆照旧由 install_id 绑定检查兜底,不受新增的第二条密钥影响。
- **下载口 24h 封死**:票据库值从裸摘要改成 `{"h": 摘要, "exp": 到期}`;`/node-bundle` 与 `/i/` 都校验过期(旧格式裸摘要视为不过期,升级前签发的票据不受影响)。URL 留在 shell 历史/访问日志里的泄漏窗口从「永久」收成 24h。
- **报错响应体也是可执行 shell**(echo 原因 + exit 1):命令是 `curl | bash` 形态,出错时用户看到人话而不是 shell 语法错误 —— 所以命令用 `-sS` 而不带 `-f`(带 -f 时 curl 会把响应体吞掉只剩一句「The requested URL returned error」)。
- 脚本里的 `--master/--fp` 取自 hub 身份 KV 并做字符白名单收口(防管理员误配引入 shell 注入);对外地址仍走 X-Forwarded-Host/Host 白名单推导。

### 测试
- gw-node:票据密钥长度/字符集(base64url,不含 `.`)。
- router:`/i/` 脚本内容(短令牌嵌入、sha256 校验行、`--master/--fp`、不出现完整令牌前缀)、鉴权失败/过期 403 且响应体可执行、过期后 `/node-bundle` 同样封死。
- admin:双命令形态(sudo/无 sudo)、票据 JSON 值(摘要与命令里票据对应 + exp)、无安装包时退回旧形态(测试钉死环境变量防并发串扰)。
- hub(node_modes):票据密钥可当加入凭证、install_id 绑定照常、第二台机器拿同票据被拒、票据过期不影响已装子机重连。

## [dario-nodes-oneliner-exec] - 2026-10-11

### 改动:一条命令装好子机 + 后台直接跑子机上的 bash
- **安装命令收成一串 bash**:router 新增公开口 `GET /node-bundle?ticket=<下载票据>`(不过 admin 鉴权)。后台「新建子机」在 ①主机上放好了安装包 ②知道对外地址时,直接给出
  `f="${TMPDIR:-/tmp}/caio-node.run" && curl -fsSL '<对外地址>/node-bundle?ticket=…' -o "$f" && echo '<sha256>  '"$f" | sha256sum -c --quiet && sudo bash "$f" --token '<令牌>' && rm -f "$f"`
  一条;缺任何一样退回旧的「先 scp 再执行」形态,不会给出跑不通的命令。
- **两种安装形态,两个脚本**:有 root 用 `install.sh`(系统级:/opt/caio + caio 用户 + systemd 系统服务);没 sudo 用 `install-user.sh`(用户态:~/.local 下、当前用户跑,有 systemd 用户会话就装用户服务并尝试 `loginctl enable-linger`,没有就 nohup + start.sh/stop.sh 兜底)。包里两个都带,install.sh 在非 root 时自动转交 install-user.sh —— 一行命令把 `sudo bash` 换成 `bash` 即可。
- **远端 bash**:后台「子机」页每台在线子机多出「终端」—— `POST /admin/api/nodes/{id}/exec` → hub(130s deadline)→ 隧道 → 子机 `POST /exec`。默认 30s、上限 120s 超时即杀;stdout/stderr 各留 256KB(截断有标记,读者持续读到 EOF,不堵管道);关对话框即取消(AbortController 一路断到子机,进程组被杀);exec 环境重建时带 HOME/USER 与 XDG_RUNTIME_DIR/DBUS(用户态节点上 systemctl --user 才能用)。
- 安装包放置:主机 `data/node-bundle/caio-node.run`(`CAIO_NODE_BUNDLE` 可换路径),由 `scripts/build-node-bundle.sh` 产出后 scp 上去。

### 安全设计(经 opus5.5 对抗评审后收紧,评审报告在 .review-dario-nodes-opus55.md)
- **下载口用「下载票据」而不是加入令牌**:安装命令的 URL 会留在 shell 历史和访问日志里;票据(库里只存 sha256、吊销子机即删)泄了只值「能再下载一次安装包」,令牌泄了则能在 install_id 绑定前把节点据为己有。命令里还带安装包的 sha256 校验(`sha256sum -c` 不过就不执行)。
- **exec 是独立的第二把钥匙 `node_exec_key`**:dario.api_key 的持有面太广(管理器、worker、每个 dario 进程),不能让它等于「全舰队的 shell」。hub 对 `/exec` 在本地口 key 之外再校验这把钥匙;没配 = exec 完全关闭(fail-closed)。子机侧还留了 `CAIO_EXEC=0` 的总开关(机器的主人可以拒绝开放)。
- **如实标注权限边界**:exec 以 caio 用户在子机上跑,`/etc/caio/node.env`(0640 root:caio)和 `/var/lib/caio/dario` 对它本来就可读 —— env_clear 只防「环境变量顺嘴泄进输出」,挡不住刻意读取。exec 的权限上限 = caio 本身,文档与后台提示都这么写。命令(截 200 字符)与退出码进**主机与子机两侧**审计日志,输出不进日志。
- **克隆防护改为探活,不再押连接年龄**:同一 node_id 已有在线连接且主机名/来源 IP 不同 → 先给旧连接发 `/info` 探活,活着就拒新保旧(拒绝可重试、非 fatal),死了才让新连接接管。旧的「连接年龄 < 120s」判定有两个洞:老连接活过窗口后克隆机可以顶掉它(两台机器轮流持号),同 NAT 同主机名的克隆根本不触发。
- **控制头收归各自产生者**:`x-caio-node` 只有 hub 能写(回程剥离,原有);`x-caio-stage` 只有 agent 能写(回程从 dario 响应里剥掉,本次补的)——不然被改过的子机/dario 能伪造「ensure 失败」,把一次已执行的对话伪装成没发出去(换号重发 = 双烧额度),或把真实错误洗成「远端基础设施故障」而不计入账号健康。
- **agent 自己产的 502/504 打 `x-caio-stage: dario`**:worker 侧映射成 `RateLimited + Unknown`(就地失败、不换号重发、不进账号病历)——原先裸 502 会被分类成 ServerError+Untracked,既换号重发又累加失败计数,等于「远端抖动打死号」从后门回来。
- **上号即清对应待删记录**:hub 补删 tombstone 前,先跳过「库里当前仍挂在这台子机上」的号 —— 不然「子机离线时删号 → 同 ID 重新上号 → 子机重连补删」会把新令牌删掉,而主机的 refresh_token 交接时已清空,号救不回来。
- **hub 公网口握手并发上限 64**(超出直接丢连接):裸 TCP 零成本,不封顶会被未鉴权洪峰耗死。
- **exec 的非 2xx 一律包成 502 说人话**:hub 的 401 若原样透传,前端 axios 见 401 就清 token 跳登录 —— 点一下「执行」被登出的怪坑就来自这里;老版本子机没有 /exec 时提示「子机版本过旧」。
- **exec 杀的是整个进程组,不只是 bash**:`process_group(0)` 让 bash 自立组长,正常结束/超时/调用方消失(Drop guard)三条路都 `killpg(SIGKILL)`;收割读者的 3 秒宽限到期后 abort 任务并标 `output_incomplete`(原先 take() 进 async 块的写法在超时 drop 时只是 detach,reader 会永久泄漏满速读管道)。已知限制:进程自己 `setsid` 能逃出进程组,那需要 cgroup 才兜得住。
- **子机路径段拒反斜杠**(`safe_segment`):`\` 在 http crate 里合法、在 url crate 里被当 `/` 消解 `..`,能绕白名单。
- **install.sh 三处 shell 坑**(pipefail + `grep -q` 的 SIGPIPE 会让好机器误报;参数缺值时 `shift 2` 静默退出):glibc 检测与「已连上主机」判定都改为先捕获输出再匹配;带值参数先验值再 shift;`caio` 用户加入 `systemd-journal` 组(exec 的第一用途是看日志);安装时把 install.sh 留一份在 `/opt/caio/install.sh`(一行命令装完会自删 .run,卸载要用这份)。
- **`.run` 解包目录自适应**:小内存 VPS 的 /tmp 常是 tmpfs(实测 476MB),装到一半 No space left —— /tmp 可用空间不足 512MB 时自动换 /var/tmp;一行命令的下载路径也带 `${TMPDIR:-/tmp}` 兜底。
- **构建脚本**:bun 的 SHASUMS 校验改为 fail-closed(取不到就停);解压兜底从 `bunx unzip`(会拉 npm 包)改为 alpine 容器的 busybox unzip;`bun install` 加 `--frozen-lockfile`;`.run` 头部去掉 `exec`(它会让 EXIT trap 永不触发,/tmp 每装一次留一份完整解包目录)。
- **已知限制**:主机到子机的安装命令链路若全程明文 HTTP(没套 HTTPS 反代),能中间人这条链路的攻击者可以同时改安装包和命令里的 sha256 —— 票据+校验防的是被动泄露与投毒缓存,不是主动双向 MitM;在意就把 admin 套进 HTTPS。

### 测试
- gw-node 集成 21 个:exec 端到端(回显/非零退出码/超时杀进程/300KB→256KB 截断/环境无令牌/排空可用/离线标记)、exec 独立钥匙(缺/错 403、未配 fail-closed)、进程组残留(`sleep 217 &` 不得存活)、克隆探活(旧活拒新、旧死接管)、路径白名单(含 `\`)。
- admin:exec 转发与标记映射、401 包装、参数校验;一串 bash 形态(票据 + sha256 + Host 推导)。
- router:bundle 票据鉴权(无/错/吊销/拿加入令牌冒充 全 403)、字节一致、缺文件 404。
- 真机端到端(两台 Vultr 备用机,公网往返):一行命令下载+安装+上线;exec 跑 journalctl/systemctl;断连重连;`--uninstall --purge` 清场。

## [dario-nodes] - 2026-10-11

### 背景:订阅号要分散到多台机器的真实 IP 上,主机还要能统一调度
- 一个 Claude 订阅号的刷新令牌**只能有一个持有者**:令牌交给某台机器上的 dario 进程后就由它独占刷新,别处再刷就把两边一起作废。所以「号」天然是绑机器的。
- 主机单机的容量、出口 IP 数量都有限;机器挂了那批号就整批不可用。
- 要的形态:在任意一台 Linux 上跑一个安装文件就成为「子机」,子机**主动连**主机(NAT 后、无公网 IP 也能用),主机把请求发给它,流式结果原路返回。

### 切分点:worker ↔ dario 管理器之间
- worker 与共享 SQLite 强耦合(账号加载、30s 热同步、计费、request_logs、令牌回写),整个搬到子机要先改造数据通道;而 dario 线本来就有一道 HTTP 接缝(`Supervisor` 只认一个 base URL)。
- 于是:**主机保留 router-dario + worker-dario**(账号库、调度、计费、会话亲和都不动),**子机只跑「节点程序 + dario 管理器 + 每号一个 dario 进程」,不放数据库**。
- 号归哪台子机记在 `extra.dario_node`;为空 = 本机管理器,与改动前逐字节相同。

### 改动
- 新 crate `gw-node`:加入令牌编解码、hub 自签证书与子机侧 SPKI 钉扎、握手帧、反向 HTTP/2 隧道、hub 端与子机端。
- 同一个二进制新增两个角色:`--mode hub`(主机,公网 TLS 口 + 回环本地口)、`--mode node`(子机,systemd 跑)。
- 子机侧只放行四类路径,其余 403:`/sup/accounts…`(交接/改状态/删号)、`/a/<号>/v1/…`(本机先 ensure 再转给该号的 dario)、`/oauth/exchange`(用子机本机 IP 换码)、`/info`。
- 选号前过滤:`Provider::account_routable` 新钩子,gw-dario 每 2 秒拉一次 hub `/status`,离线或排空子机上的号直接不参与选号;拉不到或快照超 10 秒按不可用处理(fail-closed)。
- 全组号都因此被滤空时对外报 503 而不是 400:那是我方运行态问题,不是客户模型选错。
- 带 `x-caio-node` 标记的 503 在分类之前识别,映射成 `RateLimited + NotSubmitted`(可换遍全组、不进池级 429 统计);**刻意不用 ServerError** —— 它会被升级成模型过载并累加失败计数,把一个本身健康的号永久停用。
- 新表 `nodes` / `node_tombstones`(都是 `CREATE TABLE IF NOT EXISTS`,老镜像不读);子机离线时删的号记进待删清单,重连后由 hub 补删。
- 后台:子机管理页(新建/改名/排空/吊销/解除机器绑定、在线状态与出口 IP、子机上各号的进程状态),账号页可按子机上号、账号行显示子机徽章。
- 一键安装包 `scripts/build-node-bundle.sh` → `caio-node-<版本>.run`:musl 静态二进制 + 与主机同版本的 bun + **原版** dario dist + 管理器源码 + 两个 systemd unit,自带 MANIFEST 校验。

### 设计取舍
- **容灾做到号级,号不自动迁移**:令牌只能有一个持有者,自动迁移等于让两台机器各握一份凭证。子机离线 → 它的号退出轮转、请求换别的号;要换机器只能删号后在新机器重新登录。后台吊销还有号的子机必须显式 `force`。
- **号所属子机只能在上号时写一次**:store 层守卫,admin 以 Replace 方式编辑时也保留这个键;改成别的值(含从无到有、从有到无)一律拒绝。防的就是双持有。
- **子机号强制直连、不配代理**:它的卖点就是用子机本机的真实 IP,再套代理等于换了出口。出口均衡、导入自动分配、OAuth 选出口三处都跳过子机号,保存时两者同时有值直接拒绝。
- **不往 `system.yaml` 的 `dario` 段加字段**:`DarioSidecarConfig` 带 `deny_unknown_fields` 而生产所有容器共用一份 system.yaml,往那里加字段会让还没升级的老镜像起不来。新配置只用顶层可选字段(`node_hub_url`)和命令行参数。
- **hub 单独一个容器**:worker-dario 发版不会断掉所有子机的长连接;以后别的线路也能复用这条隧道。
- **安全**:自签证书 + 令牌里钉 SPKI(与 `curl --pinnedpubkey` 同口径);secret 库里只存 sha256,可吊销、吊销 5 秒内断线;首连把令牌绑定到子机的 `install_id`,同一令牌换台机器来连直接拒;hub 本地口只绑回环且要 key;转发时剥掉主机侧鉴权头(子机注入本机 key)与逐跳头;路径按原始字节切分、不解码(账号 ID 是百分号编码)。

### 对抗审查(三视角)找出并已修的高危
- **远端抖动不再打死本地号**:hub 的 `offline/disabled/lost` 标记与「子机上 ensure 失败」一律映射成 `RateLimited`。原先 `lost` 走 `Network`、ensure 失败走 `ServerError`,而这两者在 `report_failure` 里同属一个分支、会累加 `failure_count`,满 5 次把号打成 `TooManyFailures` **永久禁用** —— 子机跑在 NAT 后的小机器上,WAN 抖动是常态,等于让远端网络状态把完好的号一个个禁掉。投递语义仍区分:离线/禁用 = `NotSubmitted`(可换号重发),`lost` = `Unknown`(不重发)。
- **选号也看子机上的管理器健不健康**:`routable()` 之前只看隧道在不在。`caio-node` 活着而 `caio-dario` 崩了(真机上「机器在、服务不在」比整机离线常见)时,号照样被选中、每条请求白等一个 ensure 超时。现在 hub `/info` 里管理器明确报错的子机直接不参与选号(没拉到 info 不算坏,不给冷启动开洞)。
- **克隆机器被挡住**:`install_id` 存在 `/etc/caio/node.env` 里,整盘克隆或拷配置会把它一起带走,两台机器都能通过认证 → 两边的 dario 各刷同一条 refresh token → 号整族作废且不可恢复。现在同一个 node_id 带着不同主机名/来源 IP 连进来时**拒新连、保旧连**并告警(新增集成测试锁住)。
- **`x-caio-node` 收归 hub 独占**:回程无条件剥掉这个头(hub 侧与子机侧各一处)。它是「请求一个字节都没发出去」的判据,若子机或 dario 也能写,一个已执行的请求就能伪称没发出去 → 主机换号重发 → 同一次对话烧两份额度。
- **热路径不再有无界等待**:hub 转发的 `sender.ready()` 纳入 deadline(子机 `max_concurrent_streams(256)` 占满时它会无限期挂住,同时 worker 这条请求还占着账号租约);子机转给 dario 的请求加了 100s 「等响应头」上限。
- **不变量变可观测**:后台子机列表新增对账 —— `orphan_accounts`(子机上有、主机没有 = 残留凭证,将来「双持有」的火种)、`missing_accounts`(主机有、子机没有 = 令牌已丢,号其实已死)、`pending_deletions`。数据本来就在 `/info` 里,只是从没跟主机库做过差集。
- **吊销不再留僵尸**:带 `force` 吊销会原样回传「就此作废的号」清单(它们的 `dario_node` 指向不存在的子机 → 永远选不中,而该字段又不可改 → 只能删号),后台弹窗列出来并提示那台机器上的凭证仍然有效。
- **证书指纹变了会吼**:忘挂 `./data` 卷或清过目录 → 证书重新生成 → 所有子机 TLS 握手失败,而主机侧只看到「全部离线」、新发的令牌又带着新指纹,极易误判成网络问题。现在指纹与库里不一致时打 error 指出真因。
- **协议版本不符改成可重试**:原先判 fatal → 子机退避 5 分钟。升级/回滚期间版本不符是暂态,现在按可重试处理(1~30 秒退避),主机一就位子机自己回来。
- **安装脚本三处**:`caio-node.service` 里的 `Environment=CAIO_SUP_URL` 会覆盖 `EnvironmentFile`,导致 `--sup-url` 参数**永远不生效**(已删);「已连上主机」的判定改用本次服务启动时刻做 `--since`(原先读「最近 2 分钟」,改完参数重跑时会把上一轮的日志读成本次成功);MANIFEST 文案不再承诺防篡改(清单与文件在同一个未签名包里,它只防传输/解压出错),并补上「克隆机器前先删 node.env」「卸载后保留的令牌仍然有效」两处警告。

### 已知限制(评审提出、本版未改)
- **号绑子机是「写一次」而不是租约**:这保住了「绝不双持有」,但也把「事后再平衡容量」和「换机器恢复」一起堵死了 —— 只能删号后在新机器重新登录。更对的形态是 `{node, epoch}` + 一条串行化的 transfer(旧机停进程删凭证 → epoch+1 → 新机领取),留作下一版。
- **hub 是单点**:它挂了,所有子机号集体退出轮转(fail-closed,宁可换号也不把请求发进黑洞)。换来的是「worker-dario 发版不断长连接」。
- **自研隧道 vs WireGuard**:tls/tunnel/join 这约 480 行安全关键代码 + 4 个 crypto 依赖,换的是「子机不用装额外东西」。评审建议下次扩展前先评估 WireGuard/`ssh -R`。
- **子机上跑的是完整网关二进制**,`--mode node` 只是其中一个分支;更对称的做法是单独出一个只含 gw-node 的 bin。
- 子机与主机的 bun / dario 版本没有互相校验(dario 的 TLS 指纹一致性靠它们同版本),`/info` 目前只上报 caio 自己的版本。

### 回滚地板与回滚步骤
- 这一版是 **worker-dario 的回滚地板**:老 worker 不认 `dario_node`,会把子机号的凭证交给**本机**管理器,与子机形成双持有,两边各刷一次令牌会把号整个作废。
- 所以上号到子机前会先查目标组 worker 的 `/health` 能力位 `dario_nodes`,没有就拒绝。
- 要回滚:**先停用全部子机号**,再停 hub,然后换回旧镜像。其它线路(kiro / cursor)不受影响。

### 注意
- `data/node-hub/`(hub 自签证书)必须随库一起备份:证书变了 = 指纹变了 = 所有子机连不上,只能重发令牌。
- 子机上 `/var/lib/caio/dario` 是各号的令牌,同样要备份;丢了那些号要重新登录。
- 子机号用 haiku 之外的模型选号时不受影响,但**所有子机号都依赖 hub 可达**:hub 挂了,它们集体退出轮转(宁可换号,也不把请求发进黑洞)。

## [auto-mode-classifier] - 2026-10-11

### 背景:自动模式的安全分类器在按主模型的价钱买一句话
- Claude Code 自动模式每要动手一次,都会先拿**同一个模型**问一遍安全分类器:system 以 `You are a security monitor for autonomous AI coding agents.` 开头,把整段对话塞进去,回答只有 `<block>no</block>` 或 `<severity>8</severity>` 这么几个字。
- 输入是整段 transcript、输出十几个 token,等于按 opus 的价钱买一句话。

### 改动
- 新增热开关 `auto_mode_classifier`(`SystemSettings` + 后台设置页,30 秒内生效):
  - **`allow`(默认)**:caio 按分类器的语法**本地直接回放行**,一个上游字节都不发;不占号、不占并发。用量按 **100% 缓存命中**记(估算出的输入全部记在 `cache_read` 上、`input_tokens` 归零,输出按回答的实际字节算),请求日志照记(family=`automode`)。
  - `haiku`:把模型改写成 `auto_mode_classifier_model`(默认 `claude-haiku-4-5`),请求照常走上游 —— CC 的安全闸还在,只是便宜。
  - `off`:原样走上游(本功能上线前的行为)。
- 判据收得很紧:system 前缀 + `stop_sequences` 里的 `</block>` / `</severity>`,两者都对上才动手;只认 Anthropic 线缆。对不上就照旧走上游(没省到钱,但绝不误伤真实对话)。

### 设计取舍
- **默认选 `allow` 而不是 `haiku`**:分类器请求带整段 transcript,而 haiku-4.5 的上下文只有 200k(主模型可能是 1M)。超过 `MAX_BODY_BYTES`(600KB ≈ 15 万 token)的请求在 haiku 模式下只能照原价走,长会话恰恰是最贵的那批 —— 改 haiku 省不到关键的那部分。本地直答才是稳定省钱的那条。
- **体积护栏是常量不是配置项**:它按模型窗口算出来,不是运营调的旋钮;换窗口更大的模型就连同常量一起改(改配置救不了「改写后上游 400」这种事后才知道的失败)。
- **用量报 100% 缓存命中,而不是报 0**:这条请求的输入是真实存在的,报 0 会让对账时凭空少掉一大块输入、看不出这些请求去哪了。记成「输入全部命中缓存」既符合事实的性质(同一段 transcript 每轮重复发,本来就该是命中),又让计费落在 cache_read 这档最便宜的价上。
- **回答的字节形态由 CC 的解析器决定,不能随手改**。实测 CC 2.1.296:判定变体要求 `<block>(yes|no)` 恰好命中一次(闭合标签可省、`<thinking>` 要先删掉),评分变体要求 `<severity>\d+` 恰好一处且 `stop_reason ∈ {stop_sequence, end_turn}`。CC 侧是 **fail-closed**:解析不出来一律按「拦截」。所以本地回答给 `<block>no</block>` / `<severity>0</severity>`(0 低于已知的全部阈值 15/25/45)、`stop_reason=end_turn`,并由单测锁住字节形态与「合成流能折回同一份」。
- **拦截点放在 worker 的 `handle_chat`、选号之前**:`haiku` 必须在这里(要在 `account_supports_model` 过滤之前改掉模型,否则选出来的号是按老模型挑的);`allow` 理论上可以提到 router 省一跳,但 worker 这边请求体本来就已经解析好、请求日志与用量机器也都在这里,提到 router 等于再复制一套日志口径。代价只是一次本机回环转发,换来的是「两种模式一个判据、一处日志」。
- **`allow` 是产品决策,不是性能开关**:开了等于关掉 CC 自动模式的安全审查(它判高危的那些也放行)。所以后台单独标红、文案写明代价;要恢复旧行为切 `off` 即可。
- `haiku` 模式下改写后若该组没有号能服务 haiku(模型白名单、或该模型在不可用窗口里),会**退回原模型重选一次**而不是对外报 400 —— 对 CC 来说 400 和被拦截是一回事。

### 注意
- 这一版默认就是开着的(`allow`)。上线后分类器相关的 `ContentFiltered` 红行会归零,而那正是用来发现「CC 升级导致提示词改写失配」的信号 —— 开 `allow` 期间这个 drift 检测等于失明。
- 判据依赖 CC 的提示词常量;CC 改了开头 → 退化成 `off` 的行为(照常走上游),不会误伤。但**若 CC 改的是回答语法**,本地回答就会被判「拦截」,表现是用户每一步都被挡住 —— 首次上线请盯一次真实的 CC 自动模式会话。

## [cursor-cli-capacity] - 2026-10-09

### 背景:今晚要上一批 Cursor 号,CLI 方式在高并发下先撞磁盘和内存
- **磁盘**:CLI 每次启动都在后台把新版本下载进每个号的 HOME(`~/.local/share/cursor-agent/versions`,一份约 525MB),而我们跑的是镜像里自带的那份,根本不用它。21 个号已占约 10G,生产机只剩 13G,上 25 个号左右就会写满根分区(同机还有 kiro worker、router 数据库)。另外 CLI 的会话库(`.cursor/chats`)、项目目录(`.cursor/projects`)从不清理,一个号一个月攒了约 1.5G、2 万多个目录。
- **内存**:实测一个 CLI 会话 = 3 个进程、峰值 RSS 约 620MB;带工具的会话在等调用方执行工具时进程一直活着(最长 280s)。宿主 32G、无 swap、容器不设内存上限,约 30 个并发会话就会耗尽内存;账号并发上限只管正在处理的请求,管不住挂着的进程。
- **延迟(本版不解决)**:每个请求(含续接的每一轮)都新起进程,空闲时冷启动中位 7s、最长 42s。

### 改动
- CLI 启动加 `--disable-auto-update`(隐藏参数,CLI 代码里设了就跳过后台更新)。
- 全局 CLI 进程上限(默认 24,env `CURSOR_CLI_MAX_PROCS` 可改):名额在起进程前拿、随进程退出释放,所以挂着等 tool_result 的进程也算在内。满了排队最多 45s,超时按 `Overloaded` 返回:不罚号、不换号,worker 同号退避重试,对外 529。错误带「本机容量」标记(`UpstreamError::local_capacity`),worker 不据此开模型级过载窗口——那个窗口跨号共享,否则 60s 内别的号的普通 5xx 也会被改判成过载、不再换号(代码审查高危)。续喂 tool_result 用的是已在跑的进程,不需要名额,所以不会把自己卡死。
- 按号每小时清一次 CLI 的过期数据:超过 24h 的会话库与项目目录,以及自动更新下载的版本目录。续接只在 2h 内发生,不受影响。

### 注意
- 上限 24 是按这台机器算的(24 × 620MB ≈ 15G)。换机器或加内存要跟着调。
- 并发真正上去以后需要横向扩:把 Cursor 号分到另一台机器上的第二个 worker。

## [cursor-cli-usage-overhead] - 2026-10-08

### 背景
- **思考强度不生效**:CLI 驱动一律选 `-high` 档,客户端传 low/xhigh 都按 high 跑。
- **末轮用量虚高,客户端误判上下文超限**:
  - 工具回路里中间各轮只能自估,一轮结束才拿到真值,差额全补在最后一条回复上。
  - opencode / Claude Code 把最后一条回复的用量当成当前上下文。实测一轮真实上下文 4.3 万,末轮报 38.5 万;opencode 判定超过 200k 后自动压缩,压缩改写历史,下一轮只能新开 Cursor 会话,缓存只剩公共前缀。
  - 差额来自两处:
    - 自估没算 Cursor 每次调用额外带的那部分。逐次账单实测,小工具集约 1.1 万/次,opencode 约 2 万/次。
    - 客户端看不到的内部调用。模型每用一个新工具就调一次 GetDynamicTools 加载,一轮多出 4 次。
- **运行说明写错了加载工具的名字**:说明里教模型调 `GetMcpTools`、说 `GetDynamicTools` 不存在。生产近 3 天 232 次加载、调用全是 `GetDynamicTools` / `CallDynamicTool`(CLI 自带,协议名 getMcpToolsToolCall),`GetMcpTools` 一次都没有。模型没听说明,所以没出事。

### 改动
- **按 `output_config.effort` 选 CLI 档**(`clidrv::cli_model_with_effort`):只改写以 `-high` 结尾的名字;目录里没有对应档时,高于 high 的逐级退到 high(grok-4.7 无 max → xhigh),低于 high 的保持 high。yapi 的 OpenAI 入口会丢掉 `reasoning_effort`,只有 Anthropic 原生入口能带到这里。
- **自估轮加上「上游开销」**(`SsePhase::reported_input`):
  - 学法:每次运行结束后用真值反推,开销 = 真值平均每次调用 − 我方自估平均每阶段。
  - 调用次数从 CLI 输出流里工具事件的 `model_call_id` 去重得出,再加收尾 1 次;实测与真实次数一致。
  - 续接沿用同一会话上次学到的值,新会话用全局滑动平均(初值 1 万)。
  - 自估只用学到值的 85%:估低了末轮会补上,估高了等于多收客户。
  - 多收护栏(代码审查高危:原补差 `max(真值−已报, 末轮自估)` 使总计费 = max(真值, 各阶段自估之和),估高就多收且收不回):
    - 末轮补差改为:自估之和超过真值时,末轮只按可见上下文兜底,不再叠开销;并打告警 `cursor-cli:自估之和超过上游真值`。
    - 同一会话的标定值与上一次取平均,不单次全信;新会话的初值不超过 1 万,不随全局值抬高。
    - 调用次数至少取阶段数(每阶段至少一次调用)。工具事件不带 `model_call_id` 时自动退回,不会把标定值推高。
  - 这部分按命中计(Cursor 的固定提示上游几乎每次都命中),也不进下一阶段的上下文基准,不会逐阶段叠加。
  - 每次标定打一行日志 `cursor-cli:上游开销标定`。
- **运行说明**:按功能描述加载工具,新旧名字都写上;要求会话第一次需要工具时按 namespace 一次加载 gwtools 全部工具,不逐个加载、不每轮重载;不要去加载 `cursor` 命名空间里已被禁用的自带工具(近 3 天白调 10 次)。

### 设计取舍
- 不改 yapi:曾考虑把差额放进单独的用量字段、由 yapi 计费,这样客户端能看到精确的上下文。用户选择只改 caio。turn 合计仍由末轮补差对齐真值,计费总额不变;末轮剩下的只有客户端看不到的内部调用。

### 注意
- grok 系列有一部分调用会随机只命中所有会话共用的前缀,三个 grok 模型都有,频率随时间变化(同一天晚上测得 1/13~3/13,一个半小时前 4.7 约一半)。这是上游行为,本版不涉及。实验脚本(逐次轮询计费明细拆每次调用)不在仓库里。

## [grokbot-0.68] - 2026-10-08

### 背景:GrokBot(pool=bot)整条路 404
- 对现有 temporal agent 发消息一律 404 `not_found` / 「Grok Bot agent not found.」;`ListGrokBotAgents` 正常。
- 逆向 GrokBot 0.68.1(`docs/grokbot-0.68-protocol-re.md`)并在 测试号 上实测:
  - 404 与客户端版本、harness 废弃都无关。session_id 留空时同一个 agent 收发正常;填我们自造的 `caio-<指纹>` 或任意 UUID 都 404。被拒的是 session,不是 agent,报错文案是误导。
  - **个人号的 temporal agent 只有一条 MAIN 会话**:协议里没有新建会话的 RPC,`session_id` / `viewer_session_id` 属于团队共享 bot 的多查看者机制。
  - 以前「一个共享 agent + 每会话一个 session_id」能用,是因为服务端把认不出的 session 静默兜底成 MAIN。也就是说,不同客户其实落在同一条对话里。现在改成了硬 404。
- 同日实测还发现两处协议变化:
  - temporal 迁到了与 box 网关同一套 harness:回复从 `message/role:assistant` 变成 `send-message{message:{type:text,content}}`,用户回声变成 `message/role:user` 带 clientNonce;
  - agent 自带云端电脑。opencode 让它查本机系统信息时,它回一句「正在查看」就去云端执行了。

### 改动
- **每个客户端会话一个独立 temporal agent**(名字 `caio-relay-<指纹前 8 位>`),所有请求的 session_id 都不填(MAIN)。
  - `SandSessions` 改成 (号, 会话指纹) → agent。
  - 建 agent 后先登记为「未确认」,一轮干净完成才确认。失败、断开都置回未确认,下次换新 agent 全量重铺,不在可能残缺的会话上续增量。
- **agent 回收**(`DeleteGrokBotAgent {1 id}`):
  - 会话过期、分叉重开时删除;
  - 每号超过 20 个时挤掉最久未用的(用户拍板的上限);
  - 每个进程首次用某个号时,删掉名字以 `caio-relay` 开头的 temporal agent。会话表只在内存里,上个进程留下的就是孤儿;旧版共享的两个 agent 也一并清掉。
    列表在账号闸门里同步取快照,删除放到后台并行执行(总预算 60s),不挡首个用户请求。只删快照里的 id,不会误删本进程新建的 agent(代码审查中危)。
- 回复解析两种形态都认:
  - 带 clientNonce 的 `send-message` 按用户回执处理,不当回复;
  - 回复的 requestId 必须与本轮回声一致。
- 带工具时套用 box 同款转发说明(`client_tool_prompt`):本地操作必须写成 `<tool_call>` 交给客户端执行。
- agent_state 显示仍在运行时,不按「定稿后静默 3s」收尾。
- 请求头 `x-cursor-client-version` 升到 0.68.1(0.39.0 实测也仍被接受)。

### 验证
- 五轮验收全过:文本、多轮记忆、会话隔离、工具调用、工具结果续轮。
- 本地 opencode 经 139 上的隔离测试 worker 实测:连调两次本地终端查系统信息并正确汇总。
- 删除接口实测可用。

### 注意
- 每个客户端会话一个 agent,在 Cursor 侧看起来是这个号频繁建、删 agent。单号能建多少个 agent 没有实测,靠每号 20 个的上限兜底。
- 同账号仍全程串行(run gate)。
- 已知小问题(未修):没有 `system.yaml` 时 `SystemConfig::default()` 把 `cursor_sandchat` 置为 false,门面被关。生产有 system.yaml,不受影响。

## [cursor-cli-resume] - 2026-10-08

### 背景:Claude 系多轮对话经 Cursor 每轮整段重写缓存
- 运营反馈:Cursor 第三方额度 400 美元,按我方账面只卖出约 32 美元就用完了。
- 用 Cursor 自己的计费明细对账(`DashboardService/GetAggregatedUsageEvents`、`GetFilteredUsageEvents`):
  - 测试号 近 40 天 claude-fable 缓存写 1,755 万、缓存读只有 642 万。
  - 典型会话每轮缓存读恒约 7.4 万(Cursor 自己的固定提示),另外 14～15 万 token 的历史**每轮重新写一遍**,单轮约 $1.8。
  - 我方按缓存模拟器把 85% 报成「缓存读」;思考输出只记了 4.3 万,Cursor 实收 47 万。
- **根因**:所有路径都把多轮历史「折叠成一条消息」每轮重发。Cursor 的缓存发生在模型厂商侧,前缀由 Cursor 服务端按**它自己保管的会话历史**拼出来:
  - Claude 是断点式缓存:折叠后断点所在那条消息每轮都变,整段作废;
  - grok 是最长前缀自动缓存:折叠文本只在末尾追加,照样命中(历史账单逐轮 85～100%);
  - 协议上不能一次发多条消息(上游静默挂起),只能让 Cursor 保管历史,也就是 CLI 的 `--resume`。
- **测试号 实测**:
  - 续接时 fable 第 2/3 轮只写 220/185 token,0.9¢/轮(首轮 13.9¢);composer 续接命中约 98%。
  - 新会话加折叠历史:每轮全价。
- 带工具的请求原先不走 CLI,是因为 `--mode ask` 的只读 preamble 让 opus/fable 拒活(09-11 客诉)。
- **线协议带工具的请求目前全部失效**(同日基线实测):
  - 模型调用我们声明的工具时,上游先发(或只发)「exec 回声帧」,网关把它当成 Cursor 内建工具收口截断;
  - worker-cursor 自 10-03 启动以来 44 次工具调用,成功交付 0 次:grok 被截成半句并按 end_turn 返回,fable 报 400 但 Cursor 照样扣费;
  - 线协议的记账也少记约 10 倍输入:按请求体粗估,漏了 Cursor 约 8k 的前缀。
- **生产路径基线实测**(`/root/sched-trace/cursor-mt-bench.py`,CLI 无工具多轮):
  - grok 第 2～8 轮每轮约 1.0¢;fable 第 2～5 轮每轮只写约 300 token、平均 1.67¢;
  - caio 记账与 Cursor 计费明细逐轮一致。

### 改动
- 新热设置 `cursor_cli_tools_models`:模型前缀列表,命中的**带工具**请求也走 CLI 驱动,靠 `--resume` 续接。
  - 默认 `["*"]` = 全部模型;`[]` = 关闭,回到 10-08 前的全走线协议。
  - 路由判定抽成纯函数 `use_cli_driver`,补表驱动单测;每请求打一行 `cursor 路由` 日志(route=cli|wire|sandchat)。
- 去掉 `--mode ask`,改为每会话工作区 `.cursor/cli.json` 的拒绝规则:
  - 禁 `Shell(*)`、`Write(**)`、`WebFetch(*)`,本轮无图片时连 `Read(**)` 也禁;
  - 放行 `Mcp(gwtools:*)`;仍绝不加 `--force`。
  - 实测:不拒活,内建写被拦,不会在沙箱里幽灵编辑。
- 运行环境说明(`DEFAULT_CURSOR_CLI_NOTICE`)重写:
  - 本地工作目录是与用户无关的空沙箱;路径用用户的;
  - 读写执行走 gwtools,工具成功即完成、别去本地核实。
  - 删掉不实的「GetDynamicTools/CallDynamicTool 可用」,改为如实说明没有,改用 GetMcpTools。
  - 实测有这段说明后,fable 直接用 gwtools 写到用户路径。
- MCP 配置从每号 HOME 挪到每会话工作区 `.cursor/mcp.json`(项目级,实测 CLI 认),**去掉全进程 spawn 锁**:旧版所有账号共用一把,带工具时最长卡 10s。
- 续接可靠性:
  - tool_result 与文字(system-reminder 等)混在同一条 user 消息时照样接续挂起的工具调用;旧版判为非工具轮,对同一个 sid 再起一个 CLI。
  - `is_error` 透传为 `[工具出错返回]`。
  - 续接时只注入本轮新消息里的附件。
  - 会话桶容量 8 → 32(Claude Code 子 agent 共用 session,会挤掉主会话)。
- **会话键漂移找回**(`CliConversations::find_by_history`):
  - 问题:没有 `metadata.user_id` 的客户端(opencode 等),会话键是 hash(system + 首条 user),system 中途一变就被当成新对话重铺。
    实测:本机两份同名技能,opencode 每次启动随机挑一份写进 system,第 3 轮续不上。
  - 修法:本键下没有任何桶时,在同号所有会话里找「已登记指纹是本轮历史前缀」的条目,沿用它的键。
  - 至少两轮往返(4 条指纹)才找,并列歧义不找;历史不够长直接返回,不扫全表。
  - 代码审查严重项一并修复:常规续接与找回都**跳过挂着工具调用的会话**。
    旧版常规查表会选中正在等 tool_result 的会话,客户端没带结果就发新消息时,会用同一个 sid 再起一个 CLI,并发写同一条上游会话;
    找回会把这个空洞从同键放大到跨键。
  - 端到端实测:第 3 轮改 system 后成功续接,缓存读 9,943 / 未命中 103。
- 模型名:
  - CLI 内置目录换成 10-08 实拉的 256 项;CLI 名只在目录里挑,按客户端是否要思考选 `-thinking-high` 或 `-high`。旧版一律 thinking 档,没要思考也付思考输出费。
  - 内置目录补 `claude-opus-5-5`、`claude-sonnet-5-5`、`claude-haiku-5-5`、`claude-fable-5-1`、`grok-4.7`(探测项)。旧版会把它们静默降到 5 系、composer 或 grok-4.5。
  - 族归一不许静默降级:请求的版本比目录里**同族最高版本**还新(如 opus-9-9、grok-4.8)按未知处理,返回 400;
    介于已知版本之间的(opus-5-1)照旧族归一。比较基准不能用归一目标,`claude-opus-5` 只有 [5],会把所有 5.x 误判成更新。
  - CLI 目录里写法不规则的 Claude 4.5/4.6 查表映射(如 `claude-4.6-opus-high`)。
    CLI 已下架的 `claude-haiku-4-5`、`gemini-2.5-flash` 同族升级到 haiku-5.5、gemini-3.5-flash。
    回归测试:内置目录里每个模型都必须映射到 CLI 真实存在的名字。
- **取消「对外 grok-4.6、实际按 grok-4.5 执行」**(09-13 为省美元月池而设,回显仍是 4.6;10-08 用户叫停)。
  - grok-4.6 现在如实执行,烧美元月池,成本靠定价覆盖。
  - 同类的静默降级一并修掉:
    - 目录精确匹配改为大小写不敏感(以前 `GROK-4.6` 掉进族归一被降到 4.5);
    - 带后缀的变体(`grok-4.6-fast`、`claude-opus-4-8-thinking`)先按版本号找目录里的同族型号,找不到才归一到族旗舰。
- CLI 未成功收尾时按 result 与 stderr 文本分类,不再一律 `Other`:
  - 额度、模型不可用 → `ModelNotAvailable`(不判整号额度耗尽;只认具体短语,不认 `quota`、`429` 这类通用词);
  - 登录失效 → `TokenInvalid`;限流 → `RateLimited`;要求升级 → `ServerError`;网络 → `Network`。
- 其他:
  - `prepare_home` 只 chown 自己写的几个路径,不再每请求 `chown -R` 整个 HOME;
  - 工具桥临时文件超过 6h 清理。

### 设计取舍
- 默认不分模型全部搬到 CLI:
  - 起初只打算搬 Claude 系,因为 grok 在线协议上前缀缓存照样命中;
  - 基线实测发现线协议带工具的请求已全部失效,CLI 的工具调用走我方 MCP 桥,不依赖上游帧形态;
  - 代价是每个新的用户轮多约 6s 进程冷启动,同一轮内的工具往返复用同一进程。
- 读权限没法只开 `assets/` 一个口子(deny 优先于 allow),所以按「本轮有没有图片」整体开关。
- 额度类失败判成模型级而不是整号:与线协议对 `ERROR_RATE_LIMITED_CHANGEABLE` 的处置一致,误判的代价小。

### 注意
- 生产 DB 有 `cursor_cli_notice` 热覆盖(旧的 ask 模式文案),上线时要一并清空或改写,否则新默认文案不生效。
- 存量会话切到 CLI 后,第一轮按新会话折叠重铺一次,之后才开始续接。
- 会话表在进程内存里,重启 worker-cursor 后所有会话重铺一次。
- 回滚:设置 `cursor_cli_tools_models: []`(30s 生效,带工具的请求回线协议),或换回旧镜像。

## [credit-enterprise] - 2026-10-08

### 背景:企业号的缓存计费一直靠模拟器猜
- 生产缓存计费策略是 `credit_correct`:按 Kiro 实扣积分反推命中比例,≥0.5 报封顶 95%,否则报保底 50%。
- 但企业计价号(IdC / ksk_ / Azure SSO)被排除在外,退回缓存模拟器,理由是积分参数只在个人号上标定过。
- 运营在日志里看到一条 IdC 企业号请求:
  - 96 万输入只扣 3.26 积分,全冷应扣约 11.8,明显是热的;
  - 模拟器判冷,按保底报了 50%;
  - 「真」列也显示 miss。
- 生产 48h 数据(opus-5.5 / sonnet-5.5,输出 ≤1500):
  - 企业号按个人号参数反推,命中比例分布与个人号一致:opus 企业号冷 14% / 热 65%,个人号 18% / 62.5%;参数通用。
  - 模拟器在企业号上只判对约 79%:13% 实际热却按保底报,3% 实际冷却按封顶报。
- 「真」列只显示 Kiro 回报的真实缓存读,但 Kiro 号已不回报(最近 3 天只有 cursor composer-2.5 有值)。
  所以 Kiro 号一律显示 miss,热请求被误看成冷请求。

### 改动
- 新热设置 `cache_credit_enterprise`(YAML `cache.credit_enterprise`,**默认开**):
  - 开:企业号与个人号一样用积分反推;
  - 关:回到旧行为,企业号退回模拟器。
  - 设置页「缓存计费策略」卡片加开关,worker 实然值卡片显示该项。
- 积分反推的前提检查抽成公开函数 `billing::hit_ratio_from_credit`,计费收尾与日志诊断共用。
- 日志列表与详情每行附 `credit_hit_ratio`:router 按当前生效的积分参数现算,不落库。
- 「真」列改名「命中」,口径依次为:
  1. 上游真实命中;
  2. 按积分反推的比例(≥50% 绿,否则琥珀,小字「积分反推」);
  3. Kiro 请求但不满足反推前提时显示「—」;
  4. 其余仍显示 miss。

### 设计取舍
- 默认开:用户明确要求企业号也走积分反推;做成热开关是为了不重启就能回退。
- 回放上线后的影响:企业号上报缓存占输入从 86.4% 升到 90.0%,48h 内 673 条上调、163 条下调。
  企业号客户的输入计费整体略降。
- 日志页的反推比例按**当前**参数现算,不是当时的计费结果。参数调过之后,旧日志显示的比例会随之变化。

### 注意
- 企业号计费改动在 worker,要重启 worker0 才生效;日志页改动只需重启 router。
- 只升级 router、不升级 worker 时:设置页会显示开关为开,但 worker 仍按旧行为退回模拟器。
  以「worker 实然值」卡片为准。

## [credits-by-account] - 2026-10-08

### 背景:按号对账只能手工跑 SQL
- 运营怀疑一批 PRO+ 新号积分消耗不对,要逐号核对「我们日志里记的积分」和「Kiro 显示的剩余」。
- 用量页只有按模型、按 API Key 两个维度,没有按账号的积分统计,只能上服务器手写 SQL。
- 手工对账结论:7 个号日志累计与 Kiro 已用相差在 ±3% 以内,对得上。
  - 日志里的 `metering_credit` 是 Kiro 每次请求回传的原始计费值,不是估算。
  - 小正差:没拿到用量就失败的请求不入 `usage_records`(只有拿到 usage 的请求才落库),上游却可能已扣积分。

### 改动
- 新增 `SqliteStore::usage_by_account`:
  - 按上游账号聚合请求数、token、积分、最后使用时间;
  - LEFT JOIN `accounts` 带出 provider、分组、停用、档位;已删除的号这些字段为 NULL;
  - 可选 `account_ids` 限定只算几个号,`account_id IN (…)` 走 `idx_usage_account`。
- 新路由 `GET /usage/by-account`:复用时间窗与 key 筛选;`ids=a,b` 逗号分隔,最多 200 个,超出回 400。
- 用量页新增「按账号积分统计」表:
  - 只列 Kiro 号和已删除的号;
  - 支持搜索,默认显示积分最高的 50 个,表头显示合计。
- 账号页积分格:
  - 当前页 Kiro 号的全时段日志累计每 60s 查一次;
  - 悬停显示「日志累计 · Kiro 已用 · 差」;
  - Kiro 已用比日志多出 max(30, 5%) 时标黄「对不上」,提示可能被别处使用,或导入前已用过。

### 设计取舍
- 命中率列用缓存模拟器的 `cache_read_tokens`,不用 `real_cache_read_tokens`:Kiro 号的真实命中生产上恒为 0。
- 对账只报正差(Kiro 记得比我们多)。负差是日志多于 Kiro:停流 40 分钟后复查不变,不是计数滞后,
  是 Kiro 实扣略少于逐条回传的计费值(实测最多 2.8%),对我方无害。
- 对账查询按当前页的号 id 限定,不跟运行态 15s 轮询走:
  - 生产全表全时段聚合约 3s;
  - 7 个号约 0.01s。
- 只改 router(admin API + 内嵌 UI),不动 worker、调度与计费。

### 注意
- 导入前就有用量的号,对账会一直显示正差。新买的号从 0 起步,不受影响。
- 没拿到用量就失败的请求不落 `usage_records`,所以日志累计可能略少于真实扣费;阈值 max(30, 5%) 会吸收这部分。

## [env-normalize] - 2026-10-06

### 背景:客户端把本机环境写进报文,原样转给上游
- 抽查 2000 条客户端报文,环境信息原样转给上游(Kiro,以及 dario 线的 Anthropic):
  - Claude Code `# Environment` 的 `OS Version: Windows 11 Home China …`(地区版 Windows,几百条);
  - opencode `<env>` 与 oh-my-opencode `<omo-env>` 的 `Timezone: Asia/Shanghai`、`Locale: zh-CN`;
  - Codex `<environment_context>` 的 `<timezone>`;
  - Copilot 类插件的 `Respond in the following locale: zh-cn`(编辑器界面语言);
  - `gitStatus` 里的 `Git user: <真实姓名>`。
- 这些对模型干活没用,却直接暴露使用者所在地区与身份。同一个号会先后服务多个客户,上游看到的环境来回跳。
  原版 dario 只改写请求头,不改系统提示词里的环境段。
- 报文里没有输入法、键盘布局;dario 也从不发送 Accept-Language。

### 改动
- 新增 `gw_core::envnorm`(与线缆卫生 `gw_core::normalize` 并列,三条线共用):
  - **6 套内置标准人设**:Windows 版本 + 内部版本号、macOS/Linux 内核串、Git 用户名;
  - **按号固定**:`extra.env_persona` 优先,否则按号哈希;
  - **时区**:号的 `extra.timezone` 优先(cursor 已用它表示与出口 IP 一致的时区),否则用全局配置。
  - **规则**:
    - `OS Version` 按原写法换成人设值;
    - `Timezone:` 行与 `<timezone>` 换成该号时区;
    - `Locale:` 换成全局 locale;
    - `Git user:` 换成人设姓名;
    - `Respond in the following locale: X` 换成「用用户使用的语言回复」。
  - **范围**:扫 system 与**所有** user 消息的 text 块,保证多轮前缀字节一致;不碰 tool_result、assistant、
    工具定义,不改块数与 `cache_control`;认不出的格式不动;确定性、幂等。
- 接入点:kiro / claude-dario / cursor 三家 provider 的 chat 入口,用当次选中的号(换号重试自然换人设)。
  - kiro:早于转换与缓存模拟;
  - cursor:在 system 分流之后;
  - dario:拿到进程端口之后。
- 热设置 `env_normalize_providers` / `_timezone` / `_locale`:
  - 进程级全局,worker 启动与 30s 同步;非法值保留旧值并报出;
  - router 写侧校验;
  - 面板新增「报文环境标准化」卡片。

### 设计取舍
- 平台大类、Shell、工作目录、日期是 agent 的功能性信息(模型据此发命令、拼路径),保持真实;只换非功能性细节。
  所以 dario 请求头写 Linux、请求体写 Windows 的不一致仍在,要彻底对齐得按客户端平台分进程,代价大,暂不做。
- 按号固定人设而非全局一套:号与号之间不共用同一套环境;上游缓存本来就按号隔离,换号不会额外失效。
- 不走 `Provider::apply_hot_settings`:dario 没实现它,开关会静默失效,所以照 cursor 护栏用进程级全局配置。

### 测试
- envnorm 单测 7 个:6 种结构逐条覆盖、保留功能字段、幂等、确定性、人设分散与覆盖、未知格式不动。
- 全局开关集成测试 1 个。
- dario 端到端 1 个:开关关闭报文不变;打开后到达 dario 进程的报文已替换,两个号拿到不同人设。
- router 设置校验 1 个;admin-ui 新增 3 个。

### 注意
- 默认全关。上线后先开 `claude-dario`。kiro / cursor 打开时 conversationId 与上游前缀缓存会各失效一次,选低峰时段。
- 人设表只追加、不改已有项:改动会让对应号的上游缓存失效一次,并且相当于换了一个人。
- 规则按行首关键字匹配,不要求处在 `<env>` / `# Environment` 等外壳里:用户自己贴进 user 消息的
  `Timezone: …` / `Git user: …` 行同样会被替换。`systeminfo` 的 `OS Version: 10.0…`(不以系统名开头)
  与 `Time Zone:` 不受影响;实际误伤面待上线后用 envcatalog 观察。

## [dario-per-account] - 2026-10-06

### 背景:注入式 sidecar 与上游 dario 的「一切按号」模型不兼容
- 旧接入:一个打了 fork 补丁的 dario 进程,每个请求经 `x-dario-upstream-token` 等头临时注入
  某个号的令牌、身份、出口。号对 dario 是匿名的。
- dario v5 起把会话 ID、请求节奏、beta 拒绝记录、超额保护等都按「池里的号」存放;匿名注入的号
  全落进同一个 `ACCOUNT_KEY_APIKEY` 位置(v6.12.28 的 proxy.ts 里 11 处)。后果:
  - 所有号共用一个上游会话 ID,有关联风险;
  - 所有号共用 500ms 请求节奏;
  - 一个号触发超额保护,整个进程停 30 分钟。
- 维护成本:合并上游 v6 有 9 个冲突,另有一处自动合并、没报冲突的地方会让所有注入请求 503。
- dario 线现状:30 天 0 请求(router-dario 只监听回环);只有 2 个号,令牌 6 月已过期。

### 改动
- **新增 `sidecars/dario-supervisor/`**(Bun/TS,零依赖):每个 claude-dario 号一个**原版** dario 进程。
  - 独立 HOME 与端口,有出口代理的号带 `--upstream-proxy`,启动参数还有
    `--session-per-client --no-ledger --no-midstream-continue --no-live-capture`;
  - 账号文件 alias 固定为 `seat`(不能用 `login`:dario 会从其它凭证源回填);
  - 按需启动、闲置 30 分钟停止(dario 每 5 秒一次在线心跳、不能关);崩溃退避;
    端口被占用时拒绝启动(防同号双进程);
  - 轮询 `/accounts`:`action=regrant` 或过期超过 20 分钟 → 需要重新登录;
  - 接口:PUT 交接 / PATCH 出口 / POST ensure / DELETE / GET,回环 + key 鉴权。
- **gw-dario**:
  - 新增 `supervisor` 模块:交接、出口同步、`ensure` 拿端口;
  - chat 发往该号进程,只带 `x-api-key` 与 `x-session-id`;
  - 交接后经 `poll_token_updates` 清空 caio 侧 refresh_token,并记录 `dario_rev` / `dario_handover_at`;
  - `refresh_auth` 改为空操作,删除 caio 侧刷新代码;
  - 额度轮询时顺带交接;
  - 错误映射:401 与「pool 无可用号」视为瞬时(dario 自己会刷新),超额保护 → 临时封禁冷却,
    管理器 409 → 需重新登录。
- **worker**:新增 `refresh_exempt`(API Key 号 + claude-dario),替换原来只豁免 API Key 号的 6 处。
- **router admin**:OAuth 上号写 `granted_at`(交给 dario 做 28 天授权到期提醒);删除 claude-dario 号时同步删管理器。
- **docker-compose**:`dario-sidecar` 改为运行管理器,镜像固定 `oven/bun:1.3.14-slim`,`init: true`,
  停机宽限 100s,挂载原版 dist、管理器代码和数据卷。

### 设计取舍
- **令牌只有一个写者**:dario 原生不支持外部注入令牌,并且明确是唯一刷新者——两方刷新同一条
  refresh token 会被 Anthropic 判为复用、整族作废。所以交接后 caio 清空自己的 refresh_token,
  从物理上保证只有 dario 能刷新;rev(refresh_token 的哈希)区分「重复交接(不动令牌)」与「重新登录(替换)」。
- **为什么每号一进程而不是继续打补丁**:
  - 原版 dario 的 `--upstream-proxy` 替换全局 fetch,刷新、心跳、对话天然同一出口;
  - 按号隔离靠 dario 自己的机制,不必跟着上游每次新加的按号功能改补丁;
  - 以后升级 = 换上游 tag 重建 dist。
- **按需启动 + 闲置停止**:24 小时挂着心跳不像真人,也省内存(空闲进程约 24MB)。代价是冷启动多 1-3 秒。
- 不改 gw-core 逻辑(只更新了 `DarioSidecarConfig` 的文档注释);Provider 没有账号生命周期钩子,
  所以交接放在 chat 与额度轮询里,删号放在 router admin。

### 测试
- 管理器:bun test 21 个,其中 1 个用真实 dario v6.12.28 联调,不访问 Anthropic。
- gw-dario:57 个,含模拟管理器 + 模拟 dario 进程的端到端:请求头不再含令牌,强制流式,额度头照常捕获。
- 工作区:1927 个全部通过。

### 注意
- 部署需要:`/root/dario-upstream`(上游 tag 构建的 dist)、`/root/dario-accounts`(0700,含令牌,纳入备份)、
  替换 router-dario / worker-dario 镜像。旧的 `/root/dario` 保留作回滚。
- 管理器数据卷丢失 = 所有号要重新登录(caio 已不保存 refresh_token)。
- 两个旧号令牌已死,验证需要重新登录一个号。
- router-dario 仍只监听回环;是否对外暴露 / 接入 newapi 另行决定。
- 断流续接先关闭,保持 caio 的用量口径不变,以后再评估。

## [billing-io-fix] - 2026-10-06

### 背景:输入多算了本轮输出,输出整体少算约 37%
- 输入:Kiro 对 Claude 模型不下发 tokenUsageEvent,网关用流末唯一一次 contextUsage(占比 × 窗口)当总输入。
  实测它是**生成之后**的上下文:让模型逐字复述 1 万字中文,上下文比只回 OK 多出约 1.25 万 token,
  正好是复述的量。所以可见输出既按输入价又按输出价各收了一次。
- 输出:上游不给,网关按字符估算(英文 4 字符/token、中文 1.5 字/token、逐帧各自取整)。用 contextUsage 差值
  实测真实分词(2026-10-06,共花约 1.9 积分):新分词器下英文文档 2.67 字符/token、TS 源码 2.39、工具调用 JSON 2.33;
  旧公式在 8 段样本上低估 23%~48%。生产最近 1 小时 1842 条响应按新公式重算:新/旧 = 1.37~1.42
  (opus-4.6 为 1.19)。逐帧取整还额外抬高了约 16%,与真实分词无关。
- 分词器分两代:opus-4.8 / opus-5 / sonnet-5 / opus-5.5 / sonnet-5.5 实测同一套;opus-4.6 同一段源码只有新分词器的
  0.755 倍,中文两代差不到 2%。

### 改动
- 新热调开关 `cache_io_correction`(面板「缓存计费策略」卡片「输入输出口径修正」),**默认关**:
  关时上报与改动前逐字节一致(旧输出估算并行保留为 `legacy_output_tokens`);开时按下面的新口径上报。
  积分反推不看开关,恒用新口径(参数表按新口径标定),算出的命中比例乘到当前上报口径的输入上。
- `text_tokens::OutputTally`:整条流累计 ASCII / 中日韩文字 / 其它非 ASCII 三类字符,收尾时按模型分词器一次折算
  (新:0.39 / 1.2 / 1;旧:0.30 / 1.2 / 1 token/字符)。删除旧 `estimate_output_tokens`。工具参数也走它
  (此前按 UTF-8 字节 /4,中文参数每字只计 0.75)。
- 模型权威表 `KiroModel` 加 `tokenizer` 列;`tokenizer_for_kiro_model()` 按上游 id 查,表外模型按新分词器。
- `UsageSignals::billed_input()`:用 contextUsage 时减去本轮输出;tokenUsage(本身是输入口径)与模拟器总量不减。
  落库 `input_tokens` 随之变为「计费输入(含缓存部分、不含本轮输出)」。
- 积分反推改用同一口径(计费输入、新输出),内置 `cache_credit_models` 按新口径重标定
  (10-01~10-04 轨迹,输出按各模型新/旧比换算;有真实响应的 1 小时样本逐条精确重算):

  | 上游模型 | miss | hit | out | base |
  |---|---|---|---|---|
  | claude-opus-5.5 | 1.2324 | 0.3595 | 0.3029 | −0.0119 |
  | claude-sonnet-5.5 | 0.8117 | 0.2381 | 0.2275 | −0.0167 |
  | claude-sonnet-5 | 0.4218 | 0.2364 | 0.1450 | −0.0060 |
  | claude-haiku-4.5 | 0.1416 | 0.0947 | 0.0328 | −0.0005 |

  miss/hit 与旧口径拟合一致,只有 out 按输出放大的比例变小。10-05 留出集判对率与旧口径持平或略高
  (opus-5.5 热 99.5%、单轮 98.9%;sonnet-5.5 热 99.5%、单轮 91.9%)。

### 历史回放(139 `/root/sched-trace/billeval.py`,按 opus ¥3/15/0.3、sonnet ¥1.8/9/0.18 每百万)
- 10-05 留出日(30,458 条,38,158 积分),每积分收入:现状 ¥0.0977;只修输入输出 ¥0.1002(+2.6%);
  修输入输出 + `credit_correct` ¥0.1038(+6.3%,+¥235/天,其中输出口径 +¥116、输入扣输出与缓存判定 +¥119);
  修输入输出 + `credit_fit` ¥0.1078(+10.3%);只上 `credit_correct` 不修输入输出 ¥0.1010(+3.4%)。
- 全部 5.5 天(254,200 条):`credit_correct` + 输入输出修正 +17.2%(¥0.0855 → ¥0.1002,约 +¥900/天);
  早期几天模拟器少收更多。
- 单条请求(留出日,方案同上):中位 +1.2%,27% 多收超过 5%,12% 少收超过 5%。

### 设计取舍
- 系数取各样本偏低一侧(英文推理摘要 0.375 与代码 0.42 之间取 0.39),宁少勿多;8 段样本误差 ±11% 以内。
- 只累计字符、收尾一次折算:与帧边界无关,也不再有逐帧取整的系统性偏差。
- 系数是实测常量不做热调:换分词器属于模型代际变化,应在权威表里给新模型标 `tokenizer`。

### 注意
- 打开开关后客户账单:输出 token 约 ×1.37,输入略降。按留出日估算,只修输入输出时总收入约 +2.6%。
- contextUsage 减去的只是可见输出(推理摘要 + 正文 + 工具参数);上下文里其余部分仍按输入计。
- 落库与 sched-trace 的 `in`/`out` 自打开开关起换口径,跨切换时点做对比或重新标定时不能直接混用;
  `calibrate.py` 需等切换后积累满训练 + 留出天数再跑,或用 `billeval.py` 的换算方式。
- `cache_credit_max_output` 的含义随之变成新口径的输出 token(同一请求数值约大 37%),默认仍 1500。
- 上一条 `[cache-credit-strategy]` 里的内置参数表已被本条替换。
- 联网搜索路径(`websearch.rs`)的 usage 是另一套,不在本次范围。

## [cache-credit-strategy] - 2026-10-06

### 背景:模拟器判错冷热,两个方向都有
- 客户反馈「积分很低的请求被按未命中扣了大量费用」。用上游 meteringEvent 的真实积分反推冷热后核对
  (2026-10-05,约 2.6 万条输出 ≤1500 的可靠样本):前缀模拟器判错 20%。实际命中却报冷 2983 条
  (客户多付约 ¥329/天),实际冷却报热 2224 条(客户少付约 ¥351/天)。
- 根因:
  - 模拟器每会话只记上一条请求,而 Kiro 同号会同时保留多份前缀(5 分钟)。客户端在开/关思考之间来回切时,
    估计器每次都拿另一种模式比,判冷,而 Kiro 还留着同模式的缓存。
  - 新会话若与别的会话共用系统提示词,Kiro 在同号上直接命中,模拟器不知道。
  - 会话键取前两条用户消息,前几轮会变,模拟器当成新会话。
  - 模拟器把 currentMessage 也算可命中,Kiro 实际不缓存它(把整段对话塞进当前消息的单轮客户端每次都是冷的,
    模拟器在这类请求上只判对 3%–25%)。
  - 线上 `cache_sim_ttl_secs=1800`,而 Kiro 实际有效期 5 分钟。

### 改动
- 重构(cfc409a,行为不变):计费收尾抽成 `gw-kiro/src/billing.rs`。帧循环只记录 `UsageSignals`
  (tokenUsage / contextUsage / metering),纯函数 `finalize()` 产出上报拆分;两处重复的模拟器观测合并;
  缓存计费默认值收归 gw-core 一组常量。
- 新增可热切换的上报策略 `cache_strategy`(设置面板「缓存计费策略」卡片):
  - `sim`(默认):原有前缀模拟器。
  - `credit_correct`:按模型参数由积分反推命中比例
    `h = (miss·I + out·O + base − 积分) / ((miss − hit)·I)`(I = 总输入/1e5,O = 输出/1e3),
    h ≥ 0.5 按 `cache_cap_ratio` 上报,否则按 `cache_floor_ratio`。
  - `credit_fit`:按 h 本身上报,受 cap/floor 夹限。
  - 两者在以下情况退回模拟器:没收到积分或积分 ≤ 0、模型不在 `cache_credit_models`、输出超过
    `cache_credit_max_output`(默认 1500)、总输入 < 1000、企业计价号。零产出仍报 0。
  - `cache_credit_discount_only`:积分策略算出的缓存比模拟器少时仍取模拟器值(客户只会少付)。
- `cache_credit_models` 内置表(10-03~10-04 个人号轨迹拟合,单位:积分/10 万输入、积分/千输出):

  | 上游模型 | miss | hit | out | base |
  |---|---|---|---|---|
  | claude-opus-5.5 | 1.230 | 0.365 | 0.440 | −0.012 |
  | claude-sonnet-5.5 | 0.815 | 0.246 | 0.256 | −0.014 |
  | claude-sonnet-5 | 0.424 | 0.239 | 0.199 | −0.005 |
  | claude-haiku-4.5 | 0.129 | 0.077 | 0.040 | 0.001 |

- 面板:策略三选一、最大输出、只纠正多收、参数表行编辑与「恢复内置参数」。worker 实然值卡片的比较改为深比较
  (表格类字段此前恒报不一致)。

### 设计取舍
- 积分是上游真实扣费,比任何本地模拟都直接。10-05 留出集上 opus-5.5 热样本判对 99.4%(模拟器 94.1%),
  单轮客户端判对 99.2%(模拟器 24.5%);sonnet-5.5 热样本 99.2%、单轮 93.4%(模拟器 2.9%)。
- 只对短输出用积分:输出占积分大头时,我们的输出 token 是本地估算,输出系数在几天间漂移(opus-5.5 0.34–0.53),
  反推出的输入命中不可靠(长输出上拟合器判对率反而低于模拟器)。
- 默认 `sim`:上线不改变任何账单;由运营在面板切换。模拟器仍每请求 observe,保证切换前后状态连续。
- 改用积分后客户账单的方向:按 10-05 数据,`credit_correct` 全量启用时客户多付 ¥279、少付 ¥195
  (净 +¥84/14.5 小时)。多付集中在模拟器原本少收的两类:单轮全 user 客户端、距上一轮 >5 分钟。

### 注意
- 企业号没有标定数据,一律退回模拟器。新模型上线后需重新标定才进表。
- 已知局限:可见输出很短、但上游隐藏思考很长的请求,多出来的积分会被当成输入未命中,客户被多收。
  10-06 实验证实上游返回的思考只是摘要(opus-5.5 max 档复述 1.25 万 token:实际生成约 3.95 万,返回的思考
  只有 5,088 字符),且思考按输出价计费。需要彻底避免时开 `cache_credit_discount_only`。
- 同一实验发现的计费口径问题(本次未改):Kiro 只在流末尾发一次 contextUsage,它包含本轮输出与隐藏思考,
  而网关把它当 input_tokens 上报,客户的「输入」= 真实输入 + 本轮输出 + 隐藏思考。
- 重新标定:139 的 `/root/sched-trace/calibrate.py` 用会话轨迹按模型拟合,输出可直接 PUT 的
  `cache_credit_models`。
- 回滚:设置面板把策略改回「前缀模拟器」(`cache_strategy: sim`),30 秒生效。
- 测试:workspace 1909 通过(+11);admin-ui 88 通过(+6);Rust 实现与离线拟合器对 10-05 全天 21,540 条逐条对拍一致。

## [sched-warmth-fix] - 2026-10-04

### 背景:每号 1000 积分从约 1150 次掉到约 660 次
- 用户反馈:以前每个号平均能请求 1000 次左右,现在只有几百次。`usage_records` 逐条拆解(输出 = 倍率×0.0002×输出
  token,热输入 = 倍率×(0.05+0.18×输入/10万),其余记为冷启动多花):

  | 日期 | 积分/次 | 千积分可用次数 | 主要涨幅 |
  |---|---|---|---|
  | 09-15..25(基线) | 0.87 | ~1150 | — |
  | 09-29 | 1.25 | ~800 | 冷启动 +0.22,输出 +0.10 |
  | 10-01 | 1.47 | ~680 | 冷启动 +0.43,输出 +0.14 |
  | 10-03 | 1.50 | ~665 | 上下文 +0.23(平均输入 24 万,平时 15.5 万),冷启动 +0.24,输出 +0.16 |

- 输出变长:09-29 修好 opus-5.5 的结构化思考强度后,客户端要求的强度才真正生效(此前实际一直按最低档思考)。
  属正确行为,未改。
- 冷启动:按 sched-trace 会话轨迹分类,10-01 冷启动多花约 4.1 万积分(当天 25%),其中 90% 是「原号还活着却换了号」。
  只看 opus-5.5、同等每号负载对比:旧算法冷启动 9–17%,新算法 15%(低负载)到 33–49%(中高负载)。

### 找到的逻辑缺陷(不是参数问题)
1. **热度是 0/1**。线程过了 300 秒 TTL(从上一轮**起跑**算)就被当成新线程,平局规则(在途少 → 最久未选)
   反而**避开**它刚用过的号。现场计数佐证:09:37 重启后 9282 次决策里「热线程被迁走」只有 27 次,其余跳号都被
   计成了 `cold_new`。10-03 低负载时,这类占跳号浪费的 62%。
2. **等原号还是换号,漏算了冷启动本身更慢**。冷预填首包每 10 万输入多约 1.2 秒(10-03 低负载实测,冷/热首包 p50 之差),
   原公式只给冷号记积分差,20 万上下文最多只肯等 1.7 秒,换号后照样多等 2–3 秒。10-01 跳号浪费的 59%
   发生在原号 2 个并发位都满的时候。
3. **冷号平局只看此刻在途数和最久未选**,看不到号上挂着多少活跃会话。新线程会挤到「会话恰好在两轮之间」的号上,
   原会话回来就撞车溢出。
4. **会话指纹忽略首条消息的附件**。「同一句指令 + 每次不同文档」的单轮批处理被当成一条线程,调度器以为能命中缓存,
   把它们全压在同一个号上排队,实际一次也命中不了。

### 改动
- 热度改为实测曲线 `WARMTH_CURVE`(按 `cost_index_cache_ttl_secs` 等比伸缩):回到某号能省下的冷热差价比例。
  刚跑过 0.92,TTL 处 0.58,略过 TTL 骤降到 0.28,之后缓慢降到 0.12。线程在哪些号上跑过记 6 个 TTL
  (默认 30 分钟,与旧版 `affinity_ttl` 一致)。
  - 标定:09-30..10-03 约 25 万次同号续轮。30 秒内命中 93%,4–5 分钟 67%,5–5.5 分钟 43%,10–30 分钟约 33%;
    换到陌生号约 20%(同一客户的 system/工具定义被别的会话缓存过)。
- 代价指数:`单价·(b0 + x·(w·r热 + (1-w)·r冷)/1e5) + κ·(E[W] + (1-w)·1.2·x/1e5) + P429·(κ·t_retry + η)`。
  κ=1 时 20 万上下文的等待上限从 1.71 秒提到约 3.8 秒(热度 0.92)。
- 平局次序:等待短 → 更热 → **近期以该号为家的活跃线程少** → 在途少 → 最久未选。活跃线程数在 30 秒一次的清扫时重算,
  两次清扫之间新派到冷号的线程先 +1。
- 本线程跑过的号(含过了 TTL 的老家)全部进候选;主号满时,优先去近期跑过的伙伴号,不再每次挑一个陌生号。
- 会话指纹:首条 user 消息带图片/文档时,并入「类型 + 来源 + 媒体类型 + 数据长度 + 首尾各 32 字符」的摘要。
  纯文本首条消息的指纹逐字节不变,上线不打散在途线程。
- 新计数 `ci_stale`:回到已过 TTL 的老家。`cold_new` 改为「线程在任何号上都没有记录」。

### Design Rationale
- 只修逻辑,不动 κ、η、等待上限等参数(用户 09-30 定的取值不变)。等待上限变长,完全来自补上的冷预填时间。
- 热度用「相对陌生号能省下的比例」,而不是同号命中率本身:陌生号也有约 20% 命中,直接用命中率会高估回原号的价值。
- 曲线按 TTL 伸缩,而不是另起一组秒数参数:运维调 TTL 时曲线跟着走,只有一个旋钮。
- 活跃线程数用 30 秒一次的批量重算,而不是逐次增减:热度表的条目会因过期、截断而消失,增量维护容易漂移;
  而且选号在全局锁内,不能每次都扫全表。

### Notes & Caveats
- **opus-5.5 有一部分冷启动修不掉**。同号、上一轮结束后 60 秒内,发往上游的前缀与上一轮逐字节相同,仍有约 5–7%
  未命中(opus-5 约 1%),属上游行为。另有一个客户的单轮批处理,本来就不可能命中缓存。
- 估算口径提醒:opus-5.5 即使客户端没开思考,caio 也默认按 high 强度让它思考,而用量里的输出 token 不含这部分。
  用「积分减去输出计价」去判冷,会把隐藏思考的费用误判成冷启动。上表冷启动一栏对 5.5 偏高;更稳的口径是直接对比
  「留在原号 / 换号」的同类请求(10-01:每 10 万输入 0.50 对 1.26 积分)。
- 热度表从 5 分钟延长到 30 分钟,条目约为原来的 6 倍(每条只存号名与时刻,量级为几千条)。
- 代码评审(code-reviewer)提出 1 条并已修:过了 TTL 的老家若正在 draining(软冷却),不再把老线程拉回去,按陌生号对待;
  全池 draining 时照旧 fail-open。
- 测试:后端 1888 个全过(1880 → 1888)。新增 cost_index 3 条(曲线、冷预填阈值、活跃线程平局)、scheduler 4 条(回老家、软冷却老家、伙伴号优先、
  新线程避开拥挤号)、converter 1 条(附件指纹);改写 3 条已有用例。
- **上线**:2026-10-03 16:14:01 UTC(北京时间 10-04 00:14),镜像 `claude-all-in-one:sched-warmth-20261004`
  (ID 1ed91115b482),只重建 worker0;router 仍是 `cost-index-fix1-20260930`。compose 备份
  `docker-compose.yml.bak-schedwarmth-20261003-161401`。
  - 回滚:compose 把 worker0 改回 `sonnet55-20261003`;或在设置里把 `scheduler_algorithm` 切到 `affinity`(30 秒热生效,不重启)。
  - 观察脚本:139 上 `/root/sched-trace/observe.py '<起 UTC>' '<止 UTC>' <分桶分钟>`(只读 sched-trace 轨迹)。
  - 上线前基线(10-03 12:00–16:00 UTC):每次 1.36–1.74 积分;opus-5.5 大请求冷启动 17–21%;紧接续轮跳号 3–9%;
    首包 p50 约 4.4 秒、p90 约 8.5 秒。

## [sonnet-5.5] - 2026-10-03

### 新增:接入 claude-sonnet-5.5
- 上游 `ListAvailableModels` 新增 `claude-sonnet-5.5`(10-03 用一个付费号拉取,目录变成 21 个模型):
  1M 输入 / 128K 输出(sonnet-5 是 64K)、rateMultiplier 1.3(与 sonnet-5 同价,opus-5.5 是 2.0)、
  `output_config.effort` 5 档、默认 `high`。
- 权威模型表新增 `claude-sonnet-5-5`(上游 `claude-sonnet-5.5`),`/v1/models` 自动带上 plain 与 `-thinking`
  两个名字。
- 修复静默降级:此前子串兜底会把 `claude-sonnet-5-5` / `5.5` 写法当成 `claude-sonnet-5` 发上游,不报错,
  客户以为在用 5.5。现在 5.5 分支排在 sonnet-5 之前,和 09-26 opus-5.5 的修法一样。上线前 3 天没有客户端
  请求过这个名字,没有已发生的影响。

### 真号直放实测(决定模型表末三列)
- 方法:用新代码的 `render_kiro_payload` 离线生成报文;在 139 上用 python 直放上游,每个号走它自己的 proxy,
  顺序发送、不并发。共约 15 次请求(含 1 次撞上该号 429),花费约 8 积分。
- **签名**:v4 无代号信封(顶层 f1=4、f3=1;明文头 f1=18 / f2=1 / f3=2 / f7=1 / f8=`thinking`,无 f5 / f6),
  与生产日志里 opus-5.5 的签名明文头**逐字段相同** → `SignatureId::Anonymous { envelope_version: 4 }`。
  第 2 轮带上第 1 轮的 reasoningContent 和签名回传,上游 200、正常 END_TURN。
- **思考强度**(5×5 行列式题,两轮全部答对 276364):

  | 条件 | 思考字符数 | 耗时 |
  |---|---|---|
  | 结构化 low(+旧标签,生产形态) | 1069 / 1172 | 16–17s |
  | 结构化 max(+旧标签 xhigh) | 8038 / 7224 | 79s |
  | 只有旧标签 low | 1151 / 4005 | 21 / 46s |
  | 只有旧标签 xhigh | 3923 / 2541 | 50 / 29s |

  只有旧标签时 low 与 xhigh 的思考量互相重叠,等于不分档;结构化字段严格分档 →
  `structured_effort_only = true`。legacy 线缆下照发 `additionalModelRequestFields`,与 opus-5.5 相同。
- **自动模式分类器第二段**:用 Claude Code 2.1.284 原版 system 提示,配一段编造的危险命令转录(不用客户数据)。
  原句 `Use <thinking> before responding with <block>.` 用时 2.8s,被判 `REASONING_EXTRACTION`,零产出;
  改写为 `Think it through …` 后用时 6.7s,正常给出 `<block>yes</block><category>Irreversible Local Destruction</category>…`
  → `rewrite_visible_thinking_directive = true`。

### Design Rationale
- 三列全部按实测填写,没有照抄 opus-5.5。sonnet-5 和 opus-5.5 在这三项上的行为恰好相反,不能凭家族名猜。
- 已知局限:两个 5.5 模型的 v4 签名明文头完全相同,本地分不出是哪个模型签发的。同一会话中途在两者之间切换时,
  会把对方签发的推理一起回传,只能交给上游验签兜底:上游静默丢弃,或报 `THINKING_SIGNATURE_INVALID` 后
  caio 剥掉推理重试。这条路径很少见,不为它另造机制;测试里钉住了这个现状。

### Notes & Caveats
- sonnet 不像 Opus 那样默认开思考:请求不带 `thinking` 时不发强度字段。这是既有策略,与 sonnet-5 一致;
  Claude Code 会显式带 `thinking`。
- 只需重建 kiro 的 worker0。router 的 `/v1/models` 是转发给 worker 的,不用动。
  回滚:compose 把 worker0 改回 `cost-index-fix1-20260930`。
- 测试:后端 1880 个全过(1877 → 1880)。新增 3 个用例:映射与防降级、v4 签名回传门控、分类器改写;
  另扩充了 3 个已有用例:结构化强度、`/v1/models`、`convert_request` 改写。
- **上线**:2026-10-03 09:37:22 UTC,镜像 `claude-all-in-one:sonnet55-20261003`,只重建了 worker0
  (compose 备份 `docker-compose.yml.bak-sonnet55-20261003-093711`)。router、cursor 未动。
- **上线后全链路验证**:test key 临时启用,验证完已禁用。
  - 多轮对话:第 2 轮发往上游的 history 里,assistant 消息带 `reasoningContent`,签名与推理文本和第 1 轮
    下发给客户端的**逐字节一致**。
  - 工具循环(先思考再 tool_use,再回 tool_result):同一条 history assistant 消息同时带 `toolUses` 和
    `reasoningContent`,签名同样逐字节一致,上游正常 end_turn。
  - 上游 `modelId=claude-sonnet-5.5`,`additionalModelRequestFields.output_config.effort` 按请求下发。
- 重启副作用:8 个企业号(IdC)的凭证已失效(刷新报 `invalid_grant Resource not found`),但仍在配置中启用、
  优先级 1。重启后它们被优先挨个尝试,各失败 3 次才被禁用。09:38–09:39 两分钟首包平均 8–11s、最长 64s,
  没有失败请求。

## [cost-index-scheduler-fix1] - 2026-09-30

### 修复:新调度器下排队号(企业号)撞 429 会进限流冷却
- 事故:`cost-index-scheduler` 07:42 UTC 上线,约 1 小时后用户发现开了排队的号进入「限流」,08:45 UTC
  回退到旧镜像。原因在 `report_failure_with_detail` 的 429 分支:新算法下排队号改用新参数
  `cost_index_pace_ms` / `cost_index_pace_max_strikes`(默认 1000ms / 熔断 3 次),绕过了生产上的
  `rate_limit_pace_ms=500`、`rate_limit_pace_max_strikes=0`(永不因 429 下线)。企业号连撞 3 次就被拉出轮转。
- 修正:排队号的 429 口径**与算法无关**,两种算法都只认 `rate_limit_pace_*`;`cost_index_pace_*` 收窄为
  「个人号短节流」专用(仅 `cost_index_pace_personal=true` 时生效)。设置页把 `rate_limit_pace_*` 放到两种算法
  的面板里都显示,并改了两组参数的说明文字。

### Design Rationale
- 排队号是企业号形态,不会被封:429 是跨租户竞争,下线只会把流量推去烧别的号;唯一的下线信号是 403 suspend。
  这是既有设计(`queue_account_never_cools_down_on_429_by_default`),新算法不该改它。
- 原测试只打一次 429,测不到熔断次数这条路径。新增回归测试连打 50 次,并断言节流时长取 `rate_limit_pace_ms`;
  同时断言 `rate_limit_pace_ms=0` 时新旧算法一致(都走冷却)。

### Notes & Caveats
- 设置键不变(没有迁移)。已存下来的 `cost_index_pace_ms` / `cost_index_pace_max_strikes` 值现在只影响开启了
  短节流的个人号。
- 测试:后端 1877 个全过(1876 → 1877),前端 82 个全过(81 → 82),`bun run build` 通过。

## [cost-index-scheduler] - 2026-09-30

### 新增:最优控制调度器(代价指数),默认启用,设置页可切回旧版
- 背景:用户反映 09-29 后积分明显变贵(40 万上下文从约 1.7 积分涨到 5.4)。查证单价没变,贵在
  **换号冷启动**:上游 prompt 缓存按账号隔离,换号即按全价重读整段上下文。历史 5 天,日请求从
  1.4 万涨到 11 万时,冷启动多花的积分占比从 1% 涨到 16%;最忙那一小时(09-29 09 UTC)大请求冷启动
  25.2%。回放显示其中 83% 的换号来自并发溢出、17% 来自 429 后永久改钉。
  研究报告:`caio-sched-sim/report/kiro-scheduling-optimal-control.pdf`(本地,不入仓)。
- 新算法:每次选号对候选号算预期代价
  `I = 单价·(0.086 + x·(热 0.342 | 冷 1.197)/1e5) + κ·预计等待 + P(429)·(κ·3s + η)`,取最小
  (常数来自生产计费标定)。推论:热号上值得等的上限与上下文大小成正比(`W* = 单价·x·Δr/κ`),
  旧版对所有请求一律等 1.5s,对 40 万上下文等得太短、对 3 万上下文等得太长。
  - 候选:本线程仍热的号(缓存 TTL 内成功服务过),加至多两个冷号(最优先层里代价最低的、
    有空位的最高层里能立刻起跑的)。优先层语义沿用 `affinity_migrate_up`。
  - 能立刻起跑就在**同一个 `entries` 临界区**里领并发许可、做 RPM 预留;否则按 0.5s 切片等待并
    重新评估(滚动时域)。等待预算从请求开始算(默认 30s,跨重试不叠加);用完仍无号可立刻起跑,
    本次取号**整段退回旧版循环**。
  - 预计等待:节流剩余、429 冷却剩余(冷却中的热号是「要等的候选」,不再被永久抛弃)、RPM 窗口、
    个人号起跑间隔,以及并发满时按在途请求的剩余时长排队估计(耗时中位数用对数 EWMA 在线估)。
  - 429 概率:个人号 / 企业号各一个按时间衰减的池级 429 率(半衰期 10 分钟),按本号 20 秒内 /
    300 秒内是否 429 过加成优势比(e^1.08 / e^0.86,联合 logistic 拟合)。
- 钉号粒度改为「会话 + 线程」:新 `Provider::thread_key`(默认 None),Kiro 实现
  `converter::thread_key_from_body` = system 前 8KB(剥 billing 指纹)+ 首条 user 纯文本的指纹,
  从第 1 轮起跨轮稳定。Claude Code 同会话的子代理不再挤同一个号的并发位。
  **只影响选号**:`CallCtx.session_id/cache_key`、计费、cache_sim、router 亲和全部不变。
- 号型计价 `gw_kiro::headers::is_enterprise_priced`:显式 `extra.price_class` > ksk_ API Key /
  external_idp / kiro_provider=enterprise|internal → 企业;builderid/aws/github/google → 个人
  (必须先于 IdC 判定:付费 BuilderID 也带 client_secret);其余 IdC → 企业。
- 设置(全部热调,`PUT /admin/api/settings` 严格校验):`scheduler_algorithm`(cost_index / affinity)、
  `cost_index_kappa` 1.0、`cost_index_eta` 3.0、`cost_index_start_spacing_ms` 1000、`cost_index_pace_ms` 1000、
  `cost_index_pace_max_strikes` 3、`cost_index_pace_personal` false、`cost_index_max_wait_ms` 30000、
  `cost_index_enterprise_price` 3.0、`cost_index_cache_ttl_secs` 300。设置页新增「调度器」卡片,
  同时补上旧版 7 个一直没有界面的参数。
- 可观测:worker `/health` 新增 `scheduler_algorithm_active`(本 worker 实际在跑的算法)与
  `queue.ci_*`(决策数 / 走热号 / 首次冷 / 有热号仍换冷 / 等待次数与毫秒 / 退回旧版次数 / 热度表条目)。

### Design Rationale
- **能力在构造期注入,不靠配置默认值**:`AccountScheduler::new()` 永远是旧版,只有生产 kiro worker
  经 `.with_cost_index(分类器)` 获得能力。既有 148 个调度测试一行未改、全部照过;cursor/dario worker
  即使设置为 cost_index 也恒走旧版(计费模型只在 Kiro 上标定)。
- **合格谓词仍只有一处**:`entry_ok_ignoring_busy` 改为「`entry_gate` 返回 Ok」,新算法用同一个
  `entry_gate` 拿到「还要等多久」,不另写一份(判据漂移的历史教训)。
- **退回旧版要整段退**:新模式下 `busy` 恒空,只复用旧版「无候选」分支会把饱和池误报成 AllDisabled(503)。
- **个人号 429 默认不短节流**:历史上定频探测曾 22 分钟送走 5 个号。默认只对 `queue_enabled` 的号节流;
  个人号照旧冷却,但不再永久改钉。要试个人号短节流,打开 `cost_index_pace_personal` 并小批量观察。
- **热度只在成功出首包后记**,时刻取起跑时刻;失败 / 429 的尝试没有填上游缓存,不记热。
  具备能力时两种算法下都维护热度表,切换算法不会出现一波冷启动。
- 研究中的限流实验:首轮 3→4→5 路持续并发看似「超过 3 路 429 陡升」,交替复核(3→5→3)推翻,
  是时间漂移;所以本版**不**放宽任何号的 `max_concurrency`,并发上限仍按号上配置。

### Notes & Caveats
- **部署即默认启用**(设置里没有 `scheduler_algorithm` 时取默认 cost_index)。回退:设置页选「亲和调度
  (旧版)」,30 秒内热生效;单个 worker 急停:环境变量 `KIRO_COST_INDEX=0` 后重启该 worker。
- 需重建镜像:router(设置页与 PUT 校验)+ worker0(调度)。旧镜像读到新键只会显示为未知键,不影响运行。
- `SchedulerAlgorithm` 带 `#[serde(other)] Unknown` 兜底:未来新增取值被本镜像读到时,整份设置不会失效
  (叠加层跳过该键);控制面写入未知值直接 400。
- 指数策略是一步前瞻,忽略了当前选择对后续请求的影响;反事实只在模拟器里验证过,上线后按 30 分钟一段
  交替开关对照冷启动占比与每 10 万输入积分。
- 测试:后端 1876 个全过(1834 → 1876;新增 cost_index 纯函数 10、调度集成 18、设置 PUT 2、
  worker 接线 2、配置 4、Kiro 线程键与号型 6),另做了 6 处变异验证(均被测试抓到);
  前端 81 个全过(新增 14 个),`bun run build` 通过,375px / 1280px 截图核对。
- 代码评审修掉两处:热度表原先只在新算法选号时清扫,切到旧版后会只增不删(现在回写时也清扫,
  有回归测试);选号时不再为空闲号加锁读在途表(全局临界区里只做必要的事)。

## [kiro-media-count] - 2026-09-29

### 修复:单请求图片/文档总数超 100 让长会话每轮 400
- 群友报 `400 请求无效,请检查请求体后重试`。定位:Claude Desktop 用 Claude in Chrome 操作浏览器,
  每步截图以 tool_result 图片进历史;1262 条消息的会话攒到 151 张(147 张截图 + 4 张用户贴图),
  上游 `400 {"message":"too many inline media segments: 151 exceeds limit 100","reason":"IMAGE_COUNT_EXCEEDED"}`。
  客户端每轮重发全部历史,这个会话从此每轮 400(05:50–06:08 连续 5 次),换号无用。
- 以前的媒体预检只有逐项规则(单图 5MB / 边长 8000px / 单文档 4.5MB / 每条 5 个文档),没有单请求总数。
- 修法:`converter/media_limits.rs` 在逐项预检之后按**请求**数图片 + 文档(history + current 合计,
  文档是否计入上游未单独实测,保守计入),超 `MAX_MEDIA_PER_REQUEST=100` 就从最老的历史媒体剔起,
  被剔的轮次末尾留一句模型可读的说明;历史剔光仍超才动当前轮(剔到刚好 100)。

### 对外报错说清是附件问题
- 上游 400 的 JSON `reason` 是已知媒体限制(IMAGE_COUNT/IMAGE_SIZE/IMAGE_DIMENSION/DOCUMENT_SIZE/
  DOCUMENT_COUNT_EXCEEDED)时,对外给我方固定中文提示(如「对话中的图片/文档总数超出单次请求上限(100 个)」),
  不再是笼统的「请求无效」。正常情况下预检已拦住,这是上游改限制时的兜底。
- 新构造器 `UpstreamError::bad_request_with_hint(message, hint: &'static str)`:提示只能是编译期字面量,
  上游响应体在类型层面进不了对外文案;内部 message 照旧保留上游原文。

### Design Rationale
- **按步长剔**:超限时剔除数按 20 向上取整(151 → 剔 60 剩 91),不是刚好压到 100。截图型会话每轮
  新增图片,刚好压到上限会让剔除边界每轮后移一张,而最老的被剔媒体在会话很靠前处,上游前缀缓存每轮
  从那里起全失效;按步长剔,边界每 20 张新图才动一次。代价是最多多剔 19 张最老的图。
- 说明文字只写本轮剔除数,不写请求总数(总数每轮变,写进去会让前缀每轮变字节)。
- 同轮内文档先于图片剔,与体积护栏 `shed` 同序;总数检查放在逐项预检之后,逐项剔掉的不占名额。

### Notes & Caveats
- 只改 worker(converter + 错误分类),router 不受影响。
- 测试:后端 1834 个全过(1823 → 1834,新增 media_limits 7 个、error_map 4 个)。
- 客户端 compact 之后历史变短、媒体变少,剔除边界会重新计算,这是正常的一次性缓存失效。
- 缓存稳定只对数量护栏单独生效:若同一请求还超 24MB 出站体积(`chat.rs` 体积护栏,逐张滑动剔除,
  占位语也不同),两道护栏叠加时边界仍可能每轮移动。本次事故的请求只有 2.6MB,未触发。
- 上游 400 分类里,已知媒体 `reason` 现在排在按正文关键词的「瞬时 invalid model」判据之前。

## [kiro-classifier-refusal] - 2026-09-29

### 修复:Claude Code 自动模式第二段安全分类器在 opus-5.5 上被上游拒答
- 用户报错 `400 模型未对该请求产出任何内容:可能是内容触发了上游安全审核...`。定位:Claude Code
  「自动模式」两段式安全分类器,第一段判为需拦截后进入第二段,第二段末尾要求
  `Use <thinking> before responding with <block>.`(把推理写进正文)。opus-5.5 在 Kiro 上把这句
  判成 `REASONING_EXTRACTION`,`metadataEvent` 返回
  `stopReason=CONTENT_FILTERED` + `stopDetails.refusal{category, explanation}`,零正文。
  同一会话 6 分钟内 3 次确定性复现;第一段(只许输出 `<block>`)不受影响。
- 真号直放 A/B(同一报文):原样 → 拒答;去掉结构化 effort → **仍拒答**(与 kiro-media-limits
  的 effort 修复无关,是早已存在的问题);只把那一句改写成
  `Think it through before responding with <block>.` → 两条被拒样本都正常返回,模型走原生推理
  (隐藏,约 800 字符),正文 `<block>no</block>` / `<block>yes</block><category>…</category><reason>…</reason>`。
- 修法:新模块 `converter/classifier_prompt.rs`,对模型表 `rewrite_visible_thinking_directive=true`
  (仅 opus-5.5)的请求,只改当前轮**最后一个 `</transcript>` 之后**(分类器后缀)与 Claude Code
  原句逐字相同的指令;没有 `</transcript>` 的请求、转录里引用到的同一句话一律不动;`<severity>` 版本
  (`Use <thinking> first, then respond with <severity>N</severity>`)同机制一并改写。
  sonnet-5 同类请求 11/11 正常,不动。

### 分类器拒答单独归类、原因完整返回用户、面板标红
- 新 `UpstreamErrorKind::ContentFiltered`:**不换号**(内容级确定性)、**不扣账号健康**(以前混在
  EmptyResponse 里,同一会话 3 次就把健康号冷却掉)、对外 400 `invalid_request_error`。
- 对外文案(用户要求「完整返回给用户」):`请求被模型的内容安全分类器拒绝(类别:REASONING_EXTRACTION,
  上游判定该请求在索取模型的内部推理过程)。上游说明:The selected model cannot continue this
  conversation. ...`。`UpstreamError::content_filtered` 是继 `bad_request_visible` 之后第二个允许对外
  文案的受控构造器:只收结构化的类别与说明两个字段,类别只留 `[A-Za-z0-9_]`,说明去控制字符、封顶
  600 字符,带上游厂商/接口指纹时整段不发。
- 「已有答案」只认真实正文块/工具调用,且在 thinking-only 兜底补空格正文之前判定;只有推理
  就被拒仍按 ContentFiltered 报错。拒答发生在已有正文/工具调用之后:按 Anthropic 原生语义
  `stop_reason=refusal` 收尾,请求日志打 `error_kind=ContentFiltered`(success 口径不变)。
- 类别与说明都过厂商/接口指纹检查(按单词,`laws`/`flaws` 不误伤),命中则对外省略、内部留原文。
- 请求日志:失败请求的 `response_payload` 现在存**下发客户端的错误体**(以前为空;首包前、
  非流式、首包后 SSE error 三条路径同口径,非流式折叠失败存中性化后的版本),详情页能看到
  客户实际收到的那句话。
- admin-ui:`ContentFiltered` 整行标红 + 「分类器拦截」徽章,详情页红色横幅显示客户实收错误;
  其余失败行在徽章下显示 error_kind。

### Notes & Caveats
- 改写逐字依赖 Claude Code 2.1.284 的提示词原句;Claude Code 改措辞后改写自动失效(退回拒答,
  但至少文案完整、账号不受罚)。拒答仍会出现在请求日志里标红,可据此发现。
- `<severity>` 版本未单独真号实测。
- router 同日一并换到新镜像(此前停在 09-08,一并带上了其后的 router/admin 改动:未知送达不重放、
  账号增量写事务化等;无库表结构变更)。重启瞬间切断 24 条在途流,之后恢复正常。
- 对抗评审(codex,Skeptic/Architect/Minimalist 三镜头,无 high):一致指出「推理计入 output_tokens
  导致只推理就被拒被当成已回答」、改写范围过宽会改动被审查的转录、`aws` 子串误伤 `laws`、非流式
  折叠失败日志存的是净化前错误体、首包后错误体未落库、类别可绕过指纹检查 —— 均已修复并补测试。
- 测试:workspace 1823 条通过(新增 gw-core 4、gw-kiro 11、gw-app 4 条),admin-ui 67 条通过。
- 生产验证(只换 worker0):分类器第二段请求发往上游的报文已是改写后的句子,正常返回 `<block>`;
  拒答探针返回 400 + 完整类别与说明,请求日志 `error_kind=ContentFiltered` 且存了客户实收错误体,
  被拒账号 failure_count=0、未冷却。
- 上线后首个真实拦截:主会话(adaptive thinking)推理已流出后被判 `CYBER`,客户在流内收到带类别与
  说明的错误;按旧逻辑会以「只有思考」收尾,客户拿到一条 stop_reason=max_tokens 的空答案。

## [kiro-media-limits] - 2026-09-29

### 修复:opus-5.5 思考强度设置不生效
- 用户反馈 5.5「思考强度设置有问题」。真号 A/B(同一账号、同一道题)实测:**opus-5.5 完全忽略
  legacy 线缆的旧文本标签 `<thinking_effort>`,只认结构化 `output_config.effort`**。
  | 发法 | low | xhigh | max |
  |---|---|---|---|
  | 结构化字段 | 签名 1.9~4k / 输出 41~268 tok | 7~17k / 535~1110 | **35~41k / 2800~3200 / ~145s** |
  | 旧文本标签(线上) | 5~11k / 178~647 | 6~12k / 364~649 | 5~15k / 398~501 |
  opus-5 / 4.8 旧标签照常生效(max 档 5~14k 字符思考、93~243s)。
- 修法(用户拍板「只给 5.5 发结构化字段」,影响面最小):模型表新增 `structured_effort_only`,
  仅 opus-5.5 为 true;`thinking_policy::wire_additional_model_request_fields` 在 legacy 线缆下
  对这类模型照发 `additionalModelRequestFields.output_config.effort`,其余模型与其余字段
  (UA 0.12.155、无 agentMode)不变。history[0] 的旧标签照写(5.5 忽略、无害),缓存前缀字节不变。
  客户端 max 原样发 max(不走旧标签词表 max→xhigh)。
- 旁证:sonnet-5 在旧标签下**确实在思考**(57~250s、难题答对)但上游**从不下发推理事件**;
  只发结构化 effort 反而不思考(1 token、答错)。属上游行为,caio 未丢任何内容,本次不改。

### 背景
- 用户反馈「opus-5.5 上下文似乎只有 600k」,实为 Claude Code + 大量附件撞 caio 出站体积闸:
  `API Error: 400 请求体剔除历史媒体后仍 6504598 字节,超出体积上限 6300000 字节`。
  图片/PDF 的 token 很少、字节很大,token 才 60 万时请求体就过 6.3MB。
- 排查中发现更严重的静默丢附件:Claude Code 的 Read 读 PDF 时,tool_result 是
  `[text, document]`,caio 只从 tool_result 里抽 image,**PDF 被整个丢掉**,模型只看到一句
  "PDF 已读取";OpenAI 形态 `image_url`(data URL)也被丢(最近 2000 条请求里 3 个 PDF、
  12 个 image_url)。

### 上游媒体限制重新定标(claude-opus-5.5,绕开 caio 闸门直打上游)
| 限制 | 实测 | 上游 reason |
|---|---|---|
| 请求体总量 | 31,454,526B 过;32.6MB 起确定性拒 | CONTENT_LENGTH_EXCEEDS_THRESHOLD |
| 单图 | base64 长度 ≤ 5,242,880 | IMAGE_SIZE_EXCEEDED |
| 图片边长 | 8000px 过、8200px 拒 | IMAGE_DIMENSION_EXCEEDED |
| 单文档 | 3.9MB 过、7.9MB 拒(文案 "4.5MB") | DOCUMENT_SIZE_EXCEEDED |
| 每条消息文档数 | 5 过、6 拒 | DOCUMENT_COUNT_EXCEEDED |
| 跨轮文档数 | history 里 6 个(每轮 1 个)照常可读 | 只按单条消息计 |
- 16 张 1.9MB 图(30.7MB)通过、17 张(32.6MB)被拒:**~32MB 是整请求总量,不是单图**。
- 旧 6.3MB 定标(2026-06)已过时;当年 7.3MB 那批 400 的会话带 PDF,更可能是撞了单文档 4.5MB。

### 修复
- **tool_result 里的 PDF/文档不再静默丢弃**:`extract_media_from_tool_result_content` 同时抽
  image / image_url / document,挂到同一条 userInputMessage(与官方客户端 tool 消息同位)。
- **`image_url` data URL 转图**(顶层与 tool_result 内);远程 URL 仍跳过(Kiro 要内联字节)。
- **文档名**:认 Anthropic 标准字段 `title`(此前只读 `name`,而 document 块根本没有 name);
  去重/净化扩到 tool_result 内嵌文档 —— 否则同会话 Read 两个 PDF 都叫 `document`,上游重名 400。
  content.rs 与 document_name.rs 取名口径一致:`name` 优先,其次 `title`。
- **图片压缩覆盖 tool_result 与 image_url**:生产 5607 张图里 5039 张在 tool_result
  (Claude Code 读截图 / browser 截图),旧实现以"体量小"为由不压 —— 它们才是请求体膨胀主因,
  也会撞单图 5MB / 边长 8000px。加进程内压缩结果记忆(sha256 寻址,128MB + 2 万条双上限,
  淘汰后归还容量):长会话每轮重发全部截图,同一张图只压一次。
  单/多图档改按**所在消息自己的图片数**判定(原为全请求):否则单/多图参数不同时,后来追加的图
  会让历史旧图被按另一档重压、字节改变、缓存前缀断裂(生产两档参数相同,不触发)。
- **逐项上限预检**(新 `converter::media_limits`):单文档 >4.5MB、单条 >5 个文档、单图 base64
  >5,242,880B、图片边长 >8000px 的附件就地剔除,原位留一句模型可读的说明(如"改用分页读取")。
  此前一个超限附件会让整轮 400,而客户端每轮重发历史、超限附件一直在,会话此后每轮 400。
  边长必须在出站前读图片头再校:压缩是可关、可失败回退的优化(超 1 亿像素的解压炸弹护栏原样透传),
  「尝试过压缩」不等于「已合规」。
- **出站总体积闸 6.3MB → 24MB**(实测上限 ~32MB 的 ~75%,刻意不贴上限)。

### 评审
- adversarial-review(codex,Skeptic/Architect/Minimalist 三视角)一致提出 4 条,全部接受并已修:
  [high] 边长预检缺失;[medium] 多图档按全请求计致历史图字节漂移;[medium] 记忆缓存记账低估;
  [low] 剔图说明混用原始字节与 base64 口径。另纠正:poison_memo 需 ≥2 个号 600s 内同一 400 才判毒,
  并非一次 400 永久封会话。

- 复审一(codex):#3/#4 关闭;#1 新发现 `image::ImageReader::into_dimensions` 会解压 PNG iCCP 等
  元数据(32KB 图可展开 32MiB,且同步跑在 async 线程)+ base64 含空白/URL-safe 时解码失败绕过检查
  → 改为手写只读尺寸字段的头解析(PNG IHDR / GIF / WebP / JPEG 按段长跳到 SOFn,不解压任何数据)
  + 宽松 base64 前缀解码;#2 按消息计档在「同一轮拆成多条 user 消息」时被绕过 → 改按连续 user
  消息组计数,与 converter 合并口径一致。

### 注意
- 本次上线后,带图的在途会话历史图片字节会变(新压缩),Kiro 前缀缓存一次性 miss。
- 入站闸(`max_request_body_bytes`,生产 32MB)不变;超 24MB 的部分仍先走历史媒体剔除。

## [kiro-opus-5-5] - 2026-09-26

### 新增
- **上线 `claude-opus-5-5`**(上游 modelId `claude-opus-5.5`,目录标注 Experimental preview,
  积分倍率 2.0,比 opus-5 的 2.2 便宜)。权威表加行后 chat 路由 / 身份行 / 上下文窗口(1M)/
  `/v1/models`(含 `-thinking` 变体)全部自动跟随。
- 与 opus-5 的上游 schema 差异只有两处:effort 默认 `medium`(opus-5 是 `high`);
  `thinking.type` 只允许 `adaptive`(没有 `disabled`)。caio 对客户端的 disabled 本就是
  「不发该字段」,从不把 `disabled` 写上线缆,故无需特判。

### 修复
- **子串兜底把 opus-5.5 静默降级成 opus-5**:`opus-5-5` / `opus-5.5` 含 `opus-5` 邻接串,
  被 opus-5 分支吞掉 —— 客户以为在用 5.5,实际拿到 5,且不报错。5.5 分支现排在 opus-5 之前。
- **opus-5.5 多轮对话历史推理全部丢失(降智)**:5.5 的 thinking 签名换成 **v4 信封**
  (顶层 f1=4,header 里没有 f5,也**没有 f6 模型代号**)。旧回传门控靠读 f6 代号比对归属,
  读不出即丢 → 5.5 的历史 thinking 100% 不上传。
  - 权威表的 `signature_codename: Option<&str>` 换成三态 `SignatureId`:
    `Unsigned`(无原生签名推理,不上传)/ `Codename(..)`(f6 必须逐字相等,旧行为)/
    `Anonymous { envelope_version }`(无代号信封:版本相等且确无 f6 才上传)。
  - 新增 `signature::read_envelope`,只读明文轮廓(信封版本 + 可选代号),必须能下钻到
    f2.f1 header 才算签名,防止任意 base64 被当成无代号信封放行。
  - 跨模型隔离不变:opus-5 签发的(带代号)不会挂到 5.5 上,5.5 的 v4 签名也不会挂到
    opus-5 上;我方合成签名恒带代号,不可能冒充 v4。

### 设计理由
- 官方客户端是把签发模型 id 与推理**一起存**、回传时比对;我们无状态,只能读签名明文。
  v4 信封里没有任何模型标识,「同版本信封的另一个模型签发」在本地无法识别 —— 这一残余
  由上游验签兜底(`THINKING_SIGNATURE_INVALID` → 剥全历史 reasoning 重试一次,既有逻辑)。
- 不按「无代号就放行」一刀切:未来格式(v5 或无版本号)不猜,宁可少回传一段。

### 验证
- 生产日志审计(改动前,最近 150 条 opus-5 请求):客户端回传带签名 thinking 1821 段 →
  上传 reasoningContent 1821 段(100%,v2 信封 `claude-honey`);opus-4.8 198/198。
  opus-5 / 4.8 的回传链路本身无问题,本次改动对其行为逐字节不变(回归测试覆盖)。
- 真号探针(生产同款 `KIRO_LEGACY_WIRE=1` + `KIRO_THINKING_IN_HISTORY0=1` 线缆):
  5.5 单轮 / 显式关思考 / 难题(3249 字符思考、签名 14344B)均正常;新代码下 5.5 的历史
  reasoningContent 进入上游报文且被接受。上游对 v4 签名确有校验:有效签名组行为系统性改变,
  篡改签名(改加密体 1 字节)被**静默丢弃**(无 400,表现与不回传相同)。
- 对照官方客户端 Kiro 1.0.212(`convertToGenerateAssistantMessages` / `er5` / `oe12`):
  **全部历史 AI 消息都回传**,每条带该回合封存的最后一段推理,门控 = 签发模型 == 当前模型
  且推理文本非空 + 有签名。caio 生产配置 `history_thinking_turns=-1` 与之一致;生产报文中
  4287 段上传推理无一空文本。
- 部署(只换 `caio-worker0`,镜像 `opus55-20260926`)后:opus-5 118 请求 2508/2508 段
  回传(无回归);admin 探针经生产 worker 发 `claude-opus-5-5` 正常出词;`/v1/models` 含
  `claude-opus-5-5` / `-thinking`。
- 新增测试:`opus_5_5_resolves_and_is_not_downgraded_to_opus_5`、
  `signature_id_table_matches_probe_evidence`、`read_envelope_*` ×2、
  `test_opus_5_5_v4_anonymous_signature_is_uploaded`、
  `test_opus_5_5_rejects_foreign_envelopes_and_vice_versa`;cargo test --workspace 全绿
  (gw-kiro 546)。
- 对抗评审(codex ×3,无 high):Skeptic#1「f6 存在但损坏被当成无代号」已修(header 严格解析,
  解析失败 ≠ 无代号);Architect「5.5 实际走 high 而非上游默认 medium」不改 —— 缺省 high 是
  既有保智力策略(4.7 同理有测试锁定)。

### 已知未处理
- 上游拒答(`metadataEvent.stopDetails.refusal`,如 `REASONING_EXTRACTION`)目前被归为
  EmptyResponse(记账号失败 + 客户端收错误),而非透传 `stop_reason: refusal`。生产频率低
  (约 2 小时 2000 请求中 3 条),未在本版处理。

## [kiro-tool-spec-stub] - 2026-09-14

### 修复
- **新版 Claude Code(≥2.1.270)整会话被 Kiro 确定性 400 的根因与修复**:新版 CC 自带
  工具暴涨到 35 个且 schema 显著变长(Artifact 单工具 kiro 格式 26.9KB),用户再叠加
  MCP 大 schema 工具(lark `docx_v1_documentBlock_batchUpdate` 37KB、
  `docx_v1_documentBlockChildren_create` 245KB)后,**单个 toolSpecification 超上游
  ~32KB 硬限制**,整个请求被 "Improperly formed request" 拒死,重试/换号均无效。
- 线上探测实锤阈值:单工具 kiro 格式 26932B 成功、38291B 确定性 400;与工具总数、
  tools 段总量(188KB 可过)、报文总体积(489KB 失败 vs 664KB 成功)均无关。
- 修法:`convert_tools` 输出侧按序列化字节判定,单工具 > 31KB(留余量)替换为
  **同名 stub**(保留名字 + 短描述 + 空 object schema),WARN 记录原名与原体积。
  不做整工具剔除 —— 历史 toolUse 引用了 tools 列表里不存在的名字,上游同样可能 400;
  stub 几十字节,行为最接近"工具仍在"。

### 验证
- 生产链路探测(同一 pig key、同一号池):原 78 工具 400 → 仅 stub 两个 lark 大工具后
  200;新增单测 `oversize_tool_is_replaced_with_same_name_stub` /
  `normal_size_tool_is_not_stubbed`;cargo test --workspace 全绿(gw-kiro 541)。

## [cursor-fork-heal] - 2026-09-13

### 修复
- **wire/CLI 形态会话分叉后永不自愈**(pi 长会话 grok 零缓存 + 每轮 33~41s 首字的根因):
  `cli_lookup` 一旦判分叉就换 digest 新 id 重铺,但成功登记(confirm)写进的是那个
  每轮一变的新 id,而下一轮 lookup 查的永远是亲和键派生的基础 id —— 基础条目停在
  旧指纹,于是每轮都重新分叉、换新 id、全量重铺。生产实测:6 小时 527 次分叉日志、
  30 个会话全残;pi 193k 上下文每轮 ttfb 33~41s、`cache_read`/`real_cache_read` 双 0
  (模拟缓存的 key 跟着每轮新 id 走,永不命中)。
- 修法:`ConvEntry` 新增 `wire_id`(服务端实际持有的会话 id)。登记一律落**基础 id**
  条目并带上本轮实际发出的 wire id;`CliLookup::Continue` 携带它回指,续轮直接发去
  分叉轮重铺出的新会话。分叉重铺轮失败只忘新 id(保留基础条目,下轮再次分叉重试),
  其余轮失败照旧忘基础条目 —— 失败语义不变。

### 设计理由
- 换 digest id 重铺的保守语义保留(旧 id 服务端还有旧历史,直接重铺会粘);只是把
  「下一次怎么找回这个新会话」补上了 —— 旧设计漏了这条回链。
- 真实上游缓存(`1.14`)在 CLI 形态本就没有用量帧,grok-4.5 auto 池真实前缀缓存又只有
  ~32k 容量(生产分布:32k 以下命中正常、48k 以上断崖),长会话的客户侧缓存折扣本来就
  只能靠模拟器 —— 模拟器 key 稳定(= 基础 id 回指的 wire id)是它能工作的前提。

### 验证
- 新增回归测试 `分叉重铺后基础条目回指上游会话`(分叉→重铺登记→下轮 Continue 回指→
  再分叉/换号/普通会话各分支);cargo test --workspace 全绿(gw-cursor 416)。

## [kiro-tool-union-strip] - 2026-09-11

### 修复
- **MCP 工具联合类型 schema 被 Bedrock 确定性 400 并毒化账号失败计数**:客户侧
  Claude Code 挂的 codex_app MCP 工具(如 `mcp__codex_app__automation_update`)
  input_schema 顶层就是 `anyOf` + `$defs`,上游直接 400 `TOOL_SCHEMA_INVALID`
  ("input_schema does not support oneOf, allOf, or anyOf at the top level");
  该错误按 ServerError 计入账号失败,坏报文随重试迁徙,7 个账号连续失败被拉起。
- 修法:转换器恒走 `strip_union_keywords`(原「含图才降级」口径作废 —— 纯文本
  请求带联合类型同样 400):`anyOf`/`oneOf` 取第一个对象变体(`$ref` 就地内联,
  外层 description/title/default 补上),`allOf` 浅合并(properties 深合并、
  required 去重),`$defs`/`definitions` 内联后删除,残留 `$ref` 宽松 object
  兜底,递归自引用深度上限防死循环。尽量保留参数信息,不做整体置空。

### 验证
- 新增单测:顶层 anyOf+$defs 内联、嵌套属性 anyOf、allOf 合并、普通 schema 原样、
  递归 $ref 终止;`multimodal_request_downgrades_complex_tool_schema` 口径同步
  更新(纯文本也不再保留 anyOf);cargo test --workspace 全绿。

## [cursor-tools-back-to-wire] - 2026-09-11

### 功能
- CUR 池(pool=api)带 tools 的请求恢复走 **wire 线协议**(同日凌晨取消缺省混合时被
  `pool_api` 项误并回 clidrv;wire 同属 cli 身份烧月池,不违反池钉);无 tools 的 chat
  仍走 clidrv。
- 撤掉同日上线的尾部恒定授权声明(`clidrv::TAIL_AUTH_REMINDER` 常量、组装点与测试
  全删)—— wire 路径本就零注入,clidrv 只剩无 tools 流量,不再需要对抗文案。

### 设计理由
- cursor-agent `--mode ask` 注入 "Ask mode is active… supersedes any other
  instructions" 的 preamble,opus/fable 严格服从:自称只读、拒绝写入、让客户
  「切 Agent 模式」(生产客诉,客户报文 + 冒烟逐字引用双重实锤)。
- 提示词对抗三连败并各自定位了死因:AGENTS.md 强化文案(「仓库文档」层级,模型原话
  「一个仓库文件不能授予我 harness 没有给的权限」)→ 用户轮 `<system-reminder>`
  尾部授权(被归档为「附加声明」,不如真人指令)→ 温和引导话术(0/3,模型反定性
  「约束来自运行框架本身,文本无法解除」)。前沿 Anthropic 模型的指令层级在
  preamble 面前不可撼动,且强硬措辞本身就是注入 pattern、触发防注入抵抗。
- wire 由我们渲染报文,preamble 物理上不存在;且 2026-09-04~09-11 tools 已在 wire
  上生产跑过一周(「整条工具回路纯协议化」),不是新路。无 tools 的 chat 留 clidrv:
  没有写操作可拒,模式话术只是显示问题。

### 注意事项
- 无数据库迁移。显式 `driver="cli"`(账号 extra / `CURSOR_DRIVER`)仍是整组回滚闸,
  压过本路由;prefill 等 cli 接不了的形态照旧落 wire。
- cargo test -p gw-cursor 绿(411;尾部声明那 +1 测试随撤回一并删除)。

## [cursor-pool-no-mixed-default] - 2026-09-11

### 功能
- 取消缺省混合:cursor 账号未钉 `pool` 一律按 api 对待(只走 cli 身份烧月池),推理面整体跳过;
  只有显式 `pool="bot"` 才走 sandchat 面烧 Bot 周池,语义不变。
- 管理端 `pool` 字段的注释/报错文案同步改为「清除 = 回缺省(月池)」。

### 设计理由
- 旧 InferenceService 面 2026-09-09 起对任何 cursor JWT 一律 401,缺省混合的
  「推理面为主、故障落 clidrv」只剩先撞死面再把 401 回显给客户的纯损耗(生产 502 的
  直接来源之一)。
- grokbot 0.39 起没有模型字段、服务端永远是同一个 Grok,Bot 周池必须是显式选择,
  不能让一个未钉号静默烧到它。

### 注意事项
- 无数据库迁移。存量未钉号(cur15、lan 等)行为变化:inference 面不再尝试,tools/composer
  形态直接落 cli/wire —— 这些形态本就在死面上 401,现在是把必然的失败换成直连活路。
- 全量 cargo test --workspace 绿(gw-app 514 / gw-cursor 411)。

## [architecture-r2-worker-extra] - 2026-09-10

### 功能
- worker 待写状态保存字段基准、目标值与本地版本；重试不再合并完整旧 extra。
- SQLite 条件合并保留管理员的新凭据与无关配置；认证链和注册上下文整组比较。
- OAuth 刷新与 profileArn 发现保留网络调用前的身份基准；拒绝旧身份的 ARN 和派生服务区。
- 相同/旧 token 不再导致同批 agent id 丢失；旧写确认不清掉后追加的增量。
- 同步读取前捕获版本，发布时保护已成功落库并清脏的新凭据；待写时仍接纳无关配置更新。

### 设计理由
- 账号锁解决进程内编排，数据库条件写解决管理员并发修改，快照版本解决读后发布竞态。
- profileArn 是依赖身份的结果，不能因管理员修改 ARN 就丢掉仍然有效的 token 轮换。

### 注意事项
- 无数据库迁移。账号删除不记作写成功；强杀或最终数据库持续不可写仍可能丢失内存待写项。
- 条件写不能替代 OAuth 单一所有者；升级必须排空并停止旧写入方。生产方退出收尾继续在 R2c 完成。

## [architecture-r2-admin-extra] - 2026-09-10

### 功能
- 管理端 extra 合并、脱敏哨兵恢复、凭据删除守卫和账号修改统一在 SQLite Immediate 写事务内执行。
- 显式允许删除凭据且没有哨兵的整体替换仍可修复损坏的旧 extra。

### 设计理由
- 哨兵表示保留写入当时的值，避免 handler 提前读取后用旧 token 覆盖 worker 的新轮换。
- 校验失败拒绝整份 patch；普通 merge 保留无关字段，整体替换保留既有语义。

### 注意事项
- 无数据库迁移。并发验证使用两个独立 SQLite 连接；部署时必须一并替换旧管理写入方。
- worker 待写增量及快照准入在 R2b 继续修复，本提交不代表所有写者已统一。

## [architecture-r1-provider-delivery] - 2026-09-10

### 功能
- 上游错误携带推理送达事实，worker 对 Unknown/Submitted 停止换号、过载退避及认证恢复重放。
- box 区分提交确认丢失与明确未接纳；box/Temporal 接纳后的续收错误统一标为 Submitted。
- Temporal 提交网络错误标为 Unknown；Kiro、Dario 与 Cursor inference 的 POST 发送错误同样
  保守标注，Cursor 内部驱动回退也检查送达状态。
- 已提交或送达未知的 OAuth 推理 401 不再未经刷新验证就永久禁用账号；API key 保留原有健康分类。

### 设计理由
- 错误健康类别、推理送达事实和客户端输出阶段分别处理，不伪装成请求错误来阻止重试。
- 既有非流式已产出判断、请求恢复状态及 RPM 视图问题继续按 R3 修复，不扩大普通流内恢复范围。

### 注意事项
- Untracked 是旧路径兼容状态，不能解释为已证明未提交；本次未全面改写 CLI/wire 内部恢复
  或所有 HTTP 错误响应分类。非 box 的 POST 发送错误保守停止重放，包括连接失败，可能降低自动恢复率。
- 已验证 TCP 提交后断开/坏 JSON/截断/503、明确连接拒绝，以及 worker 的同步/流内错误、
  流式/非流式响应、认证/过载/换号调用次数。端到端真实客户端将在后续重构完成后重新验收。

## [architecture-r1-router-delivery] - 2026-09-10

### 功能
- router 仅对确定连接未建立的错误保留同组受限迁移；未知送达返回错误并保持会话亲和。
- OpenAI 入口缺失由 worker 明确标记，普通 404 不再被当作协议能力证据。
- 内网 client 禁止重定向及系统代理，避免后续连接失败被误认为原请求未送达。

### 设计理由
- 响应头前断开也可能已提交上游；客户端没有收到响应不是允许重发的依据。
- 保留同组连接失败恢复，不新增路由开关或能力注册系统。

### 注意事项
- 新 router 配旧 worker 的无标记 404 会原样返回；先升级 router，再升级对应 worker。
- 本地 TCP 回归先证实旧行为失败，再验证三种入口的断连、404、重定向、组隔离及亲和。

## [grokbot-client-tools-and-attachments] - 2026-09-10

### 功能

- box 网关上传内联 PNG/JPEG/WebP/GIF 与 PDF，通过 `attachmentPaths` /
  `attachmentNames` 随消息提交；包含 `tool_result` 中的附件。
- 明确区分客户端工具与云端原生工具，Bash/Read/Edit 等通过 TextEmu 返回
  本地客户端执行，修正上游在云端寻找客户端本机路径的问题。
- 等 box 准备成功再交出流对象，准备失败交回 worker 原有换号预算，避免
  create/upload/send 的错误因过早返回流对象而直接向客户端回显 502。

### 设计依据

- Claude Code 与 OpenCode 实际测试发现：普通回复、简单虚拟工具通过，
  不代表本地 Bash 可用；必须校验客户端真实执行及结果续轮。
- 附件先校验数量、base64 和解码预算，再渲染；内容摘要进入会话指纹，
  避免更换图片/PDF 后仍误复用旧历史。box 不再预先抽取随后丢弃的 PDF。

### 注意事项

- 一次请求历史最多 16 个附件、解码总量 20 MiB；暂不抓取 URL 附件。
- 原生工具与本地工具隔离仍依赖上游遵守提示契约，协议未提供禁用原生
  工具的能力。图片/PDF 新上传能力仅接在 box 路径。
- 本地全工作区测试 1,744 项通过；公网入口、客户端及部署验证见
  `docs/grokbot-client-e2e-20260910.md`。

## [grokbot-box-gateway] - 2026-09-09

### 功能

- `grok_bot_auto` 在账号未开放 Temporal 时使用官方云端 box 网关，接入
  `EnsureSandBox`、JSON 命令与 SSE transcript，复用 TextEmu 和 Anthropic 输出。
- 客户端会话各用独立 agent，同会话增量续轮；历史分叉或 system/tools 变化
  时新建，防止串话和沿用过期工具定义。

### 设计依据

- 独立探针已在 bot13 上验证 headless 两轮回复；不再把 api2 的 box 发消息
  失败推广为所有 box 路径不可用。
- 按 `clientNonce → requestId` 过滤回声和旧轮重放；结算后补读最终正文，
  避免 SSE 与快照交错时提前收尾。保留固定出口和账号刷新保护。

### 注意事项

- box 网络 token 与 OAuth token 分开处理，并禁止网关重定向泄漏自定义头。
- 每账号串行执行，进程内最多保留 64 个会话，2 小时过期；进程异常退出
  可能遗留云端 agent。重启后创建新 agent 并发送完整历史。
- token 用量仍为估算，图片/附件支持仍受原 sandchat 限制；不支持指定 BOT
  底层模型。可用性以账号实际返回为准。

## [cursor-tool-continuation] 工具轮续轮:工具回路不再全量重铺 + 卡死根因全清 — 2026-09-04

### 背景

两件事其实是同一条链:① 用户 grok 会话反复卡死(输出计划后流挂起);
② 每个工具调用都特别慢(工具结果轮 IDE 形态全量重铺,10 万+ token 会话
每轮重传 10~25s、cache=0)。

### 根因链(逐层实证,每层都有抓帧/重放证据)

1. **工具轮的续轮料被自己扔了**。toolcont 探针证实:工具调用轮上游**照样给**
   turn_commit 与尾部描述符(在回调帧之后),旧代码在 tool_call 处 break,
   commit/描述符全没读,还顺手作废旧链 → 工具结果轮只能全量重铺。
2. **MCP 回调有 exec 形态回声**。同一 call id 发两遍(1.2.2.15 + exec 回声帧,
   后者给官方客户端渲染 exec UI)。续读commit 时撞上回声帧,被旧「内建工具
   收口」分支截断 + 误注纠偏,模型被绕进重调循环。
3. **并行工具调用只下发最后一发**。`tool_call: Option` 逐帧覆盖,模型并行
   3 个调用客户端只见 1 个。
4. **模型会调「不存在的工具名」,上游静默挂起**。三种形态全抓到实证:
   - 清单里压根没有 write(opencode 写文件归 edit)→ 幻觉 `gwtools-write`;
   - 服务端转录把工具称作 `mcp_claude-local_<name>`,模型学了去调全名
     (抓思考实证:「实际可用的工具是 mcp_claude-local_write」);
   - 未注册名上游**一个帧都不回**,只剩 1.13 心跳到停滞闸。
5. **大件组装静默 ≠ 卡死**。模型在服务端逐 token 拼巨型工具调用(写 316 行
   HTML ≈ 2 分 10 秒零帧),30s 停滞闸把合法大写文件误杀 —— 这才是
   「写页面之前必卡」的主因,之前归因于幻觉工具名只对了一半。

### 改动(gw-cursor;探针 examples/probe_wire_v2 新增 toolcont/mcpalias)

- **工具轮续捕 commit**:tool_call 后不再 break,客户端照常 tool_use 收尾,
  流继续读到 commit+描述符(3s 宽限),拿到即提交影子表;等不到才退回
  旧行为作废旧链。IDE 形态维持即时收口。
- **影子表 tool_turn 条目**:工具轮 assistant 输出指纹算不准(调用方再序列化),
  链 = 工具轮请求历史;lookup 对这类条目放宽为「链 == 历史减两条」,
  结构校验(末条 tool_result + 上一条 assistant tool_use)在 CLI 门前做。
- **工具结果轮进 CLI 续轮**:增量只发工具结果渲染文本(探针实证模型连贯
  使用结果、链恢复、3s/轮 vs 重铺 10~25s)。
- **exec 回声帧忽略**(await_tool_commit 时);**并行调用全部下发**
  (tool_calls: Vec,参数合计入用量估算)。
- **write 别名**:调用方无写文件工具时广告补位 `write`,回调翻译成编辑
  工具(键名从 schema 认;opencode edit 空 oldString 实测可创建文件)。
- **全名双注册**:每个工具同时注册 `mcp_claude-local_<name>`(mcpalias
  探针实证可路由),回调归一回裸名;护栏清单/计费指纹仍用未扩展列表。
- **出字后停滞闸 30s → 150s**:区分「必死(未注册名)」与「大件组装
  (合法大参数)」在帧层不可分,取覆盖大件组装的值。

### 验证

- 探针:toolcont 三轮全绿(回调/commit/续轮/链恢复);mcpalias 证实全名可路由。
- 单测 gw-cursor 350 绿(+3:tool_turn 宽松匹配、prev_assistant 结构校验、
  write 别名补位与翻译);workspace 三连全绿。
- **opencode 端到端全绿**(本地 Ultra 号 × grok-4.6 × 线协议):
  查 3 源新闻 → 写 316 行图文并茂 HTML(write 组装静默 2m10s 不再被掐)→
  read×3 校验 → 续会话提意见修改(edit 46px 标题/日期)→ 再校验。
  全程描述符链 refs 3→6→9→12→16 连续推进,零重铺。

## [cursor-stall30] 幻觉工具名卡死止血:护栏后果句 + 出字后停滞闸 30s — 2026-09-04

### 背景

用户 grok 会话反复卡死:输出一句计划(「现在用已查到的原文把报纸做成更深的图文版」)
后流挂起 90s 被掐,重试后模型重复同一计划再卡 —— 「一旦卡住就反复重复同一个问题」。
本地用生产失败报文原样重放完整复现,debug 抓帧钉死根因:

- 模型(opencode 渠道)幻觉调用了**未注册**的工具名 `gwtools-write`(opencode 清单里
  没有 write,写文件归 `edit`)。
- 对未注册名,Cursor 上游**一个帧都不回**:正文之后只剩每 10s 一个 `1.13` 心跳,
  直到 stall 看门狗掐断。重试 = 全量重铺同样历史 = 模型再犯同样错误 → 死循环。
- 加重因素:热配置里的旧护栏策略句写于 IDE 全名时代,让模型「必须用带前缀的完整
  名字」—— 而线协议 CLI 形态是**裸名**注册,同一句策略在 CLI 形态下是在教模型
  加前缀(= 教它造未注册名)。

### 改动

1. **护栏策略句热改**(免部署,`PUT /settings` 的 `cursor_tool_guard`):去掉
   「带前缀」错误指令,改为形态中立 + 后果明示 —— 「严禁发明或猜测清单之外的
   工具名:未注册的名字上游静默无响应、本轮直接卡死失败;新建文件也必须用清单里
   声明的编辑/写文件工具;工具名严格按清单原样调用」。
2. **出字后停滞闸两档**(`chat.rs`):首字前维持 90s(冷启动/长考留余量);
   **已出字后 30s** —— 出字后再沉默最常见的是上述必死模式,90s 才宣判会让客户端
   对着半截回答干等、重试也晚。正文/思考帧照常刷新计时,正常生成不受影响。

### 验证

- 复现:生产失败报文本地重放,同款 90s 心跳死等(成功率约 50%,非确定性 ——
  模型调对名字就正常,调 `gwtools-write` 就挂)。
- gw-cursor 347 测试绿,clippy 无新增。

## [cursor-grok-effort-low] grok 首字慢修复:无 thinking 请求 effort 降 low + 思考帧算流进展 — 2026-09-04

### 背景

用户普遍反映 grok 首字慢。生产数据:grok-4.6 首字 p50 13.1s / p90 42s / p99 84s,
同窗口 claude 系 p50 只有 3~5s;且 lan 号大量「90s 内只有心跳」误杀(incomplete_stream)。

### 根因(两条叠加)

1. **grok 目录参数写死 `effort=high`**(`models.rs`),`apply_thinking_pref` 只碰
   claude 系的 `thinking` 键 —— 客户端没请求 thinking 的主流量也按高强度长考,
   而思考帧按收侧过滤不透传,首字 = 整段思考时间。
2. **未透传的思考帧不算流进展**(`chat.rs` 旧 `want_thinking` 门控):上游明明在
   持续发 `1.4`,90s stall 闸把健康的长思考流当「只有心跳」掐死 → 报错重试,
   重试再占并发,雪上加霜。

### 改动

- `models::apply_thinking_pref`:客户端没提 thinking(或显式 disabled)时,对
  「有 effort 键、无 thinking 键」的模型(grok 系)把 effort 降到 `low`;claude 系
  保持目录默认不动(作用未知的旋钮不赌);客户经 `thinking:enabled/adaptive` 或
  OpenAI 侧 `reasoning_effort` 显式要思考时仍按 budget 分档,不受影响。
- `chat.rs`:思考帧**一律**刷新 last_progress。真故障(只思考永不出字)由 gw-app
  300s 静默硬上限兜底;effort 降 low 后长考窗口本身已短。

### 验证

- 单测 gw-cursor 347 绿(+1:`grok_effort_drops_to_low_when_client_skips_thinking`),
  workspace 全绿。
- 本地 Ultra 号 e2e:90KB 上下文请求流全程 128s(思考帧持续到达),旧行为 90s 必被
  掐,现干净收尾;`effort=low` 上游接受(200,无 invalid_argument);thinking 请求
  照常有 thinking 块(回归无)。
- 生产对照实验(部署前):同锚点会话钉同号,缓存命中 ~97%,粘性无涉本问题。

## [cursor-wire-agent-tools] 纯协议路线全通:裸态门真相 + KV 回执 + mcp_tools + AGENT 模式 — 2026-09-04

### 背景

clidrv(官方 CLI 子进程)工作区磁盘成本高,一直想把工具流量也收回纯协议。
朋友推荐两个开源反代(raine/claude-code-proxy、YeautyYE/claude-cursor-proxy,
Rust),证实**纯 rustls 裸栈即可打通 AgentService** —— 直接推翻「裸态门 =
TLS/h2 指纹」的旧假设(wreq/BoringSSL 指纹库路线未开工即废弃,Cargo.toml
已还原)。旧「注入实验」的共变量:帧注入官方进程时官方自己也在发心跳,
「成功」算到了进程身份头上。

### 三根柱子(全部本地 Ultra 号探针实证,examples/probe_wire_v2)

1. **裸态门 = BiDi 生命周期**。官方 CLI 全程每 ~5s 发 `client_heartbeat`
   (`{7:}` 空帧);我方只在初始帧序发一次就干等,服务端把流当死客户端 →
   200 + 只心跳、永不生成。探针补发后裸态立刻消失。已生产化
   (`chat.rs` 请求侧保活,Drop abort + 发送失败自退)。
2. **KV 子协议必须回执**。对照 fork 的 `agent.v1` schema 钉死:顶层 field 4 是
   `kv_server_message{id, get_blob_args, set_blob_args}`;**set_blob 不回执
   (set_blob_result)服务端就不出 checkpoint**,90s 心跳死等 —— 这是出字后
   停滞、`.3`/用量尾帧不来的根因。回执 `.1` 是**请求 id 回显**不是自增 slot
   (08-23 实物巧合同值,当时误读)。`wirev2::WireDriver` 改按 KV 语义反应式
   回执(生产与探针仍共用一份实现);初版「照实物帧序」预发的 9 条回执证实是
   无害噪声(服务端忽略未知 id)。探针成绩:turn1 5.9s 干净收尾,turn2 答出
   暗号(持史 ✔),turn3 cacheRead **98%**,负控判据正确触发。
3. **mcp_tools 是工具广告唯一实证通道**。帧0 `1.4` 装 `McpTools`
   (`{1:name, 2:desc, 3:Value(schema), 4:'claude-local', 5:tool_name}`;
   tag 3 必须 Value 包 struct,缺 4/5 Fable 忽略整表)。探针实证模型按**裸名**
   回调、参数正确;`run::parse_tool_call`(1.2.2.15)本就能解,无需 ENV .14。

### 顺带钉死的一个旧误读

消息 `.4` 不是「消息 kind=2」,是 **AgentMode 枚举**:08-23 实物是 2=**ASK**
(抓的 CLI 就在 Ask 态),照抄导致模型「不能写文件」自我设限(e2e 实证拒写)。
改发 1=AGENT 后 Write/Bash 全通。写/改许可归调用方工具体系,网关不设限。

### 改动

- `cli.rs`:`mcp_tools_message`(帧0 `1.4` 广告目录);`cli_user_message`
  `.4` 改发 AGENT;`build_frame0_cli` 加 tools 参数。
- `run.rs`:`kv_requests` 解析器(KvRequest::Get/Set,id 回显);
  `content_hash_demands`/`section_echoes` 改为委托,签名不变。
- `wirev2.rs`:WireDriver 按 KV 语义回执(set 存+回、get 按 id 供),
  移除自增 slot;锁存语义不变。
- `chat.rs`:请求侧 5s 心跳生产化;cli_mode 放开 tools;护栏文案裸名版
  `mcp_tool_guard_with`(gwtools- 前缀版是 IDE 口径);工具轮显式作废描述符
  (服务端停在等工具结果的中间态,链**设计内**中断,下轮重铺);
  工具轮的判决/作废日志降为 INFO(防 WARN 淹没真异常)。
- `lib.rs` 路由:带 tools 的请求默认**不再进 clidrv**(整条工具回路纯协议化);
  显式 `driver="cli"`(账号 extra / CURSOR_DRIVER)仍强制 clidrv = 回滚闸。
  cli_mode 前提 `CURSOR_PROFILE=cli`;`CURSOR_DRIVER=wire` 可全量走线协议。

### 验证

- 单元:gw-cursor **346 绿**(新增 set_blob 回执手写字节、id 回显、mcp_tools
  帧0 形状、AgentMode 四条断言)。
- 探针:3 正 + 2 负全绿(持史 ✔、98% 命中、负控判据触发)。
- e2e:opencode × grok-4.6(CURSOR_DRIVER=wire + CURSOR_PROFILE=cli)
  Glob/Read/Bash/Write 工具环全通、多轮记忆正常、worker 日志零 WARN。

### 现状限制

- tool_result 轮仍走 IDE 形态全量重铺(与 fork 的 ClientOnly 同义):工具密集
  会话每次工具交换付一次全史。要省需把结果回进挂起的原流(fork live.rs 的
  BiDi 桥,架构成本高,未做)。
- InferenceService 的 tools 绕行(`tools_skip_inference`)保留:它管的是
  inference 面,与本条 AgentService 面互补。

## [cursor-inference-tools-bypass] 带 tools 的非 composer 请求绕行 inference — 2026-09-04

### 事故

opencode(pi 系)经 inference 直连访问 grok 全部 529。当日凌晨 xAI 故障恢复后
tool-less 请求正常,但**任何带 `tools` 的请求**稳定 `ERROR_PROVIDER_ERROR`
(providerStatusCode 422;claude 系为 400),被归类 Overloaded 快败不烧号,
用户侧表现为"一直繁忙"。

### 取证(全部本地 Ultra 号实弹,零生产影响)

- 二分生产报文(request_logs 抓原件):触发点是 `tools` 数组,其余字段
  (thinking/output_config/context_management/system 消息/system 提示)逐个排除。
- 单个 289B 的工具、手写最简 schema、甚至 `{"type":"object"}` 都 422;
  **不带 parameters 的空壳工具正常**;composer 全系带 tools 正常。
- 与官方客户端库(grokbot 重构源码内 generated `inference_pb.ts` + `@bufbuild/protobuf`)
  序列化的同构请求**逐字节对比**:初差 Struct 键序(我方 BTreeMap 字母序 vs 官方
  插入序),修正后仅 uuid 不同 —— 上游照拒。键序不是根因。
- 官方字节 + 4 个 client-version(0.18.0/2026.08.11/1.7.44/2.0.0)+ maxMode 两态 +
  builtInModel + acceptedUnadvertisedToolNames 全组合:**均 422**。
- 结论:平台侧拒收「带 parameters 的 AgentTool」(疑 xAI 故障期开始的回归或有意
  收紧),字节层面无解。composer 不受影响。

### 改动

- `inference.rs`:新增 `tools_skip_inference` —— tools 非空且模型非 composer 系,
  绕行 inference(落 clidrv,AgentService 面工具链成熟);`struct_writer` 键序
  改 `type` 首位(贴近官方惯用序,无害;完整保序需 serde_json preserve_order,
  kiro 字节对齐面未审计,暂缓)。
- `lib.rs`:inference 分支加该门控。clidrv/wire 回退链不动,`CURSOR_DRIVER` 等
  退出机制不变。
- 上游若恢复(可用 `e2e_inference` + 任意带 parameters 的工具复测),把门控改热
  配置或删除即可。

### 验证

- `tools绕行门控` 单测(grok/claude 绕行、composer 不绕行、空 tools 不绕行)。
- `cargo test -p gw-cursor` 344 全绿;探针回归:composer-2.5 + 真实 opencode
  28 工具报文 → 正常出字(inference 面未受影响)。
- 本地网关 + 本机 opencode 端到端:grok 带工具请求走 clidrv 收敛。

## [cursor-inference-default] cursor 默认驱动翻转为 InferenceService 直连 — 2026-09-03

clidrv(子进程包裹 cursor-agent)从 2026-08-23 起就是**临时方案**;这次把纯协议路线
转正。取证与结论全部写进 `../docs-cursor-protocol-re-2026-08-23.md` §八。

### 取证(本机 Ultra 号 + `e2e_inference` 探针,零生产影响)

- 三模型基线全通:grok-4.6 / composer-2.5 / claude-sonnet-5,流式 + usage 帧 + 干净收尾。
- **429 压测未复现**:并发阶梯 1→2→4→8→16 共 93 发全 200,零流内 error。此前
  「一直 429」在本面不可复现,更可能源于非 Ultra 号或额度耗尽;Pro 号 2026-08-26
  起官方已开放此面,但限速阈值未实测,灰度期盯。
- 缓存无漂移:24.7k tok 前缀第 2 发起命中 99.9%(24704/24723)。
- 门控实测:prefill 上游 200 接受(放开);`stop_sequences` 字段接受(记档);
  `thinking: disabled` 不影响上游发思考帧(客户端侧丢弃,行为不变);
  `tool_choice` proto 无字段(维持全通道不支持)。

### 改动

- `inference.rs`:document 块(base64 PDF)接入 —— 构建期 `pdf.rs` 抽文本层内联,
  与 cli/wire 同形态(含「无法抽取」明示分支,防模型反复调读文件工具);prefill
  门控放开;URL 媒体维持不接(主动出网是 SSRF 面)。实弹:埋哨兵的真 PDF,
  模型答出哨兵数字。
- `lib.rs`:默认驱动 clidrv → inference;`extra.driver="cli"` / `CURSOR_DRIVER=cli`
  保留秒级回退,**clidrv 代码不删**。inference 不接的形态(URL 媒体)自动落回 cli。
- admin-ui:驱动三档(Inference 直连(默认)/CLI 子进程(回退)/线协议(退出)),
  表格徽章改挂非默认项;后端 `normalize_driver` 本已接受三值,只更新注释。
- 2026-08-17「裸名被上游收走」的教训**不适用**于 inference 面:裸名 + parameters
  是官方 host 自己在发的形态,不是逆出来的私有面(见 inference.rs 模块文档)。

### 验证

- `cargo test --workspace` 全绿(含门控新用例:prefill 接、PDF 接、非 PDF 文档拒、
  document 注入文本层断言);admin-ui `tsc + vite build + 64 vitest` 全绿。
- 部署纪律:灰度先 1-2 个号 `extra.driver=inference` 跑 24h(盯 429 率/缓存命中/
  auto 池消耗),无异常才切全量;异常时 `CURSOR_DRIVER=cli` 整 worker 秒回滚。

### codex 复审(0 blocker / 3 major / 1 minor)与修复

- **major#1 运行时故障无回退**:inference 建流前失败(未产出任何内容,重放安全)按
  错误性质分流 —— 账号/请求级(TokenInvalid/RateLimited/QuotaExhausted/ModelNotAvailable/
  TemporarilyBlocked/Overloaded)原样上抛交调度层;驱动级(ServerError/Network/Other/
  BadRequest)在 CLI 能接时**回退 clidrv**。EmptyResponse 故意排除(内容级 guardrail
  空流,换路径重发是"放大"动作)。流内错误仍归 `CURSOR_DRIVER=cli` 人工回滚。
- **major#2 门控与编码器深度不一致**:门控改 fail-closed —— 只放行编码器认识的块
  集合(text/image/document/tool_use/thinking/redacted_thinking + 一层 tool_result),
  未知块/嵌套 tool_result 落回 cli/wire,不再静默丢失。
- **major#3 PDF 解压炸弹面**:`pdf.rs` 流数量封顶移进扫描阶段(原 take 只管消费)、
  新增 64MB 累计解压上限;`build_request` 经 `spawn_blocking` 挪出 Tokio worker 线程。
- minor(URL/非法附件回退后模型看到「(见附件)」占位但拿不到内容)是 cli/wire 共有
  的存量行为,与本次翻转无关,记档不动。

### codex 二轮复核(0 blocker / 4 major / 2 minor)与修复

- **M1 prefill 请求兜底链被掐断**(回退要求 cli_eligible,prefill 不满足):驱动级失败
  改为落向下游 —— cli 能接走 clidrv,不能接走 wire,不再直接 `return Err`。
- **M2 流内错误不自动回退**:**设计取舍,不改**。窥探首帧再决定是否回退会把每个
  请求的首字延迟搭进去;协议漂移有 warn 日志 + 监视器,人工 `CURSOR_DRIVER=cli`
  秒级回滚覆盖。
- **M3 回退路径(clidrv/wire)的 PDF 抽取仍是同步**:存量行为,翻转前生产就如此,
  非本次回归,记档。
- **M4 门控消息级不够严**(非对象消息、非标量 content、tool_result 的对象 content
  会过门控但被编码器丢):门控补到消息级 fail-closed。
- minor:PDF 抽取加**进程级信号量**(4 并发槽,仅含 document 的请求排队);
  单流解压按剩余总量预算截取;accounts.rs 过时注释更正。

### 首次上线即事故(2026-09-03 晚)与真凶

默认翻转部署后几分钟全通道 502。**真凶是 xAI(grok)供应商故障,不是翻转** ——
回退 clidrv 后 grok 照样全挂、composer 全好;本地 Ultra 探针同挂;各号 quota 充足。
真正被我方放大的环节:END trailer 的 `ERROR_PROVIDER_ERROR`(provider 422)/
`ERROR_RESOURCE_EXHAUSTED` 被 `map_connect_error("resource_exhausted")` 误判成
**RateLimited** → 账号批量冷却 → 健康的 composer 流量被「选号失败」株连。
历史上「inference 一直 429」的体验大概率同一根因。

修复:trailer 按 `details[0].debug.error` 细分,供应商故障 → **Overloaded**
(模型级语义:不罚号、不换号、同号退避),回归测试用生产原文钉死。
详见 `../docs-cursor-protocol-re-2026-08-23.md` §八附录。

## [cursor-turn-trueup] 末轮补差:turn 合计对齐上游真总量 — 2026-08-18

用本机抓包先钉死了一条前提,再据它做补差。**做完这条,之前提的"手填地板常量"方案
可以彻底扔掉** —— 地板本来就含在真总量里,补差自动吸收。

### 前提:`turn_ended.input` 是本 turn **所有内部模型调用之和**

三次受控实测(钩 `cursor-agent` 的 http2 读原始帧;三个文件各 6 字节,所以工具轮
之间上下文只涨百来 token):

```text
工具调用 0 次 → 内部调用 1 次 → turn_ended.input = 16,664   (16,664/次)
工具调用 1 次 → 内部调用 2 次 → turn_ended.input = 28,550   (14,275/次)
工具调用 3 次 → 内部调用 4 次 → turn_ended.input = 67,786   (16,946/次)
```

**反证**:若它是"最后一次调用的上下文",三次应当都 ≈16.7k。实际是 1× / 1.7× / 4.1×,
正比于内部调用数。

同时确认(这条是坏消息但确定):三次都**只有一个** `turn_ended` 帧,协议里没有任何
per-call 用量帧 —— `stepCompleted` 只带 `step_id` + `duration_ms`。**所以"每个调用方
HTTP 轮的真值"在协议层不存在**,换 wire 驱动也拿不到。之前我说 `turn_ended` 是
"没人捡的金矿",只对了一半:它是真值,但粒度是整个 turn。

### 做法

`CliConv` 加 `billed_input`(会话级累加器,一个 CLI 进程 = 一个 turn,天然按 turn 归零):

- 每个自估轮(`finish_tool_use`)把报给调用方的 `input_tokens` 累进去
- 末轮拿到真值后:`上报 = max(真总量 − 已报之和, 本轮自己的估值)`

`max` 是护栏:前几轮若高估导致差额为负,末轮至少按自己那份走,不会出现 0 或负数。

**缓存按同一比例缩放,不是硬夹。** 补差后的 input 通常小于 turn 总量,而真值的 cache 是
整个 turn 的 —— 不缩放就会 `cache > input`,线缆侧 `input − cache` 归 0,客户又看到
「输入 0」(用户 08-17 报的那个)。按比例缩放同时保住**折扣率**。
`real_cache_read_tokens` **不缩放**:它是对账列,记上游实际做了多少命中,整个 turn 的
真值归在末轮这一条,前几轮是 0,合计正确。

### 为什么记末轮而不是回改历史行

前几轮的记录已经写库并上报给计费侧,回改会让下游对账错乱。记末轮只让**一条**记录
偏大,而 turn 合计正确 —— 钱是按合计收的。

### 影响:相对上一版是**降**,相对正确值是**准**

⚠️ 本条初稿把方向写成"修的是少收",**是错的**,部署后看日志才发现。三个口径分清:

- **相对上一版(`wire-truth`)**:是**降**。真值轮原本报整个 turn 的总和 `T`,而前面
  各轮已经各报了自己那份 —— 等于把 `Σ估值` 重复收了一遍。实测那条:
  `truth_total=43,221 / already_billed=455 / trued_up=42,766`,turn 合计从
  43,676 降到精确的 43,221。
- **相对正确值**:精确。`Σ估值 + 补差 = T`,而 `T` 由构造含全部内部调用与地板。
- **相对今天下午之前**:仍是**涨**。此前估值只有几十 token(那个 `input=10` 的 bug),
  重复量可以忽略;把估值改成真实上下文(十几万)之后,这个重复才变成大头 ——
  **也就是说 `ctx-basis` 那一版引入了一个约 2 倍的多收,窗口 13:30→14:50**。
  实测该窗口内真值轮极少(容器日志里只有 1 条,且是我自己的受控测试),
  因为长 agent turn 多半没走到干净的 `result`,绝大多数轮次是自估。曝露面小,
  但这是我引入的,记在这里。

地板的账仍然收得回来:末轮报的是"自己那份 + (n−1) 份地板",n 份地板全在 `T` 里。

隐藏的内部调用(`auto` 的模型选择器、自动摘要等)也计入真总量 —— 客户没直接触发,
但我方账号确实被扣,真值口径下自然算进去。这是定价决策,已确认按真值收。

### 验证

- `cargo test -p gw-cursor`:**261 passed / 0 failed**(+3)
- 新增三条:①turn 合计精确等于真总量;②前几轮高估时退回本轮估值(不得 0/倒挂);
  ③单轮 turn 补差是恒等变换
- `cargo check --workspace --all-targets` 干净

## [cursor-usage-wire-truth] 线缆取证钉死 usage 口径 + 补 cache_write — 2026-08-18

本机装了 `cursor-agent 2026.08.11-e8db854`,钩住它的 http2 把
`agent.v1.AgentService/Run` 的服务端帧原样落盘,**同一次调用两侧对照** —— 一次把
反复改口三次的 usage 口径问题定死了。

### 取证

```
线缆 InteractionUpdate.turn_ended(field 14),13 字节:
  field 1 input_tokens       = 16664      ← 总量
  field 2 output_tokens      = 34
  field 3 cache_read_tokens  = 12160
  field 4 cache_write_tokens = 0
  field 5 reasoning_tokens   = 33
CLI 的 result.usage:
  {"inputTokens":4504,"outputTokens":34,"cacheReadTokens":12160,"cacheWriteTokens":0}
  16664 − 12160 − 0 = 4504  ✓
```

CLI 源码(`1931.index.js`)就是这么算的,`result` 事件发的正是这个减过的对象:

```js
case "turnEnded": { ... ke = f({inputTokens: Number(e.inputTokens), ...}) }
f = e => ({...e, inputTokens: Math.max(e.inputTokens - e.cacheReadTokens - e.cacheWriteTokens, 0)})
... {type:"result", ..., usage: ke}
```

**结论:同一个字段名在两条路上口径相反。** 线缆 `input` 是总量;CLI `result` 的
`inputTokens` 是**已减掉 cacheRead 与 cacheWrite 的未命中量**。

### 修的两处

**① CLI 真值轮少报了一个缓存量的输入。** `usage_from_result` 把那个未命中量当总量填,
而 `ChatUsage::input_tokens` 的契约是总输入。现在
`total = inputTokens + cacheReadTokens + cacheWriteTokens`。

**② `cache > input` 从来不是异常,旧代码把合法缓存丢了。** 生产 39% 的记录
`cache_read > input_tokens` —— 命中大于未命中量是**常态**(取证那次 2.7 倍)。
旧逻辑判它"上游口径异常"并置 0,等于扔掉客户应得的折扣,同时输入也少报。丢弃逻辑删除。

**③ 线缆侧补读 field 4/5。** `run.rs` 原来只解 `1..=3`,三元组换成 `WireUsage`
结构体,`cache_write` 落到 `cache_creation_tokens`(2026-08-18 抽查生产 6,430 条
该列恒 0,即这块一直没收)。`reasoning_tokens` **不入账** —— 实测
`output=34 / reasoning=33`,它是 output 的子集,单独计会重复收费。

### ⚠️ 这条口径我改口过三次,三次的错法都记在 `usage_from_result` 的文档里

1. **08-17 第一版**:据「39% 的记录 cache > input」推断是未命中语义,改成
   `up_in + cached`。方向对,但**当时没有证据**。
2. **08-17 撤回**:我用「10 字符的提示词不可能有 6779 token 的新增输入」把自己说服了,
   判定 6779 是总量并撤回。**那个论证是错的** —— 新会话的未命中量本来就含
   AGENTS.md + tools + Cursor 注入的服务端 system(实测 16k 量级)。用"这个数看起来
   太大"反推字段语义,和用聚合统计反推一样不成立。
3. **08-18 定案**:钩 http2,两侧同时读同一次调用。**只有这种取证能定语义** ——
   一侧的数字无论多少条都推不出另一侧的口径。

### 影响:又是涨价(修的是少收)

以取证那一次为例(¥0.4/M 输入、缓存 0.1×):
旧 `4504 × 0.4 = 1,801 µ¥` → 新 `4504 × 0.4 + 12160 × 0.04 = 2,288 µ¥`,**+27%**。
只作用于**真值轮**(约占 CLI 请求 28%);自估轮不经这条路。

### 验证

- `cargo test -p gw-cursor`:**258 passed / 0 failed**
- 重写 4 条锁旧口径的测试;新增 `缓存写入计进总量且落对账列`、
  `缓存大于未命中量是正常的不得丢弃`
- 顺带删掉 `SsePhase::sim_total` 死字段(基准改 `ctx_base` 后没人读)
- `cargo check --workspace --all-targets` 干净

### 副产物:一套可复用的抓包装置

`scratchpad/hook.js` —— `NODE_OPTIONS=--require` 注入,patch `http2.connect`,
只对 `AgentService/Run` 的流 append 落盘,不改任何字节。配沙箱 HOME
(`auth.json` 只需 `{accessToken, refreshToken}`)即可用池号跑任意实验。
本次一并确认:出口主机 `agentn.global.api5.cursor.sh`(与我方 `lib.rs` 已有的一致)。

## [cursor-cli-ctx-basis-2] 首阶段基准:补 system + 去掉一处重复计费 — 2026-08-18

紧接上一条。部署后做了一次**受控两轮工具回路实测**(自己发,cursor 流量当时已停),
结果暴露同一个基准还差两块。

### 实测

```
第 1 轮(新会话,自估)  客户侧输入 3      缓存 45   → DB input 48
第 2 轮(工具轮,真值)  客户侧输入 16,366 缓存 455  → DB input 16,821
```

第 2 轮是 `end_turn`,CLI 进程退出、`result` 给了真值 —— **这个新会话的真实首轮输入
是 16,821**,而我方第 1 轮报 **48**。工具轮的四个数量级修好了,首轮还差 350 倍。

### 缺陷 1:新会话基准漏了 system(我方能算的那半地板)

差的 16.5k 是**每请求的固定地板**:我方写盘的 AGENTS.md(调用方 system)+ Cursor
注入的服务端 system(CHANGELOG 早前实测约 26k)。`first_in_tally` 只算 prompt + tools,
把 system 排除在外 —— 那条注释("算进本轮新增输入会重复计费")是为**旧基准**
(`fresh_in + cache_read`)写的,基准改成上下文总量后它就反了:system 每轮都被 CLI
重读、上游照样计费,漏算就是我方贴钱。现在计上。

Cursor 注入的那部分我方算不出来,仍缺;它是"每请求固定地板",属于定价要考虑的输入。

### 缺陷 2:续会话基准重复计了 tools 与 prompt(我自己上一条引入的)

上一条写成 `ctx_base = sim_total` **同时**叠 `first_in_tally`(prompt + tools),而
`sim_total` 的口径本来就是 `system + tools + 全部历史`
(见 `fingerprints_from_context`)。tools 对 Claude Code 一类客户能有上万 token,
等于每条会话的首轮凭空多收一份工具清单。

修法:抽出纯函数 `first_phase_basis(resumed_sim_total, system, prompt, tools)`,
**两条分支互斥、绝不相加** ——

| 会话 | ctx_base | in_tally |
|---|---|---|
| `--resume` 老会话 | `sim_total` | **空** |
| 新开/重铺 | 0 | `system + tools + prompt` |

### 验证

- `cargo test -p gw-cursor`:**257 passed / 0 failed**(+2)
- 新增两条:①续会话 `in_tally` 必须为空(锁重复计费);②新会话基准必须含 system
- `cargo check --workspace --all-targets` 干净

## [cursor-cli-ctx-basis] CLI 驱动:自估轮 input 基准改累加式 — 2026-08-18

用户从生产账单发现:「很多特别低的缓存命中」。查出来是**同一个基准错误的两个表现**,
其中主因是我上一批为躲另一个坑而引入的。

### 病征(生产实测,grok-4.6 同一条会话)

```
id       输入 i    输出 o   缓存 c    占比      耗时
1585721  121,483  19,487  115,409  0.9500  131.5s   ← 有上游真值的轮
1585725       10   1,145       10  1.0000   14.6s   ← 紧接着的工具轮
1585727       10     802       10  1.0000    0.06s
1585730       10     460       10  1.0000    0.07s
1585737       10     308       10  1.0000    0.08s
1585739       10   1,191       10  1.0000    0.07s
1585744       10     529       10  1.0000    9.9s
1585749       10     490       10  1.0000    0.08s
1585766  132,812     442  126,171  0.9500   42.1s
```

**病 1:`占比` 恒等于 `cap_ratio`,缓存列不含任何信息。** 它就是 `cap × 输入` ——
模拟器算出来的真实命中值恒大于夹限,永远被削掉。

**病 2(主因):基准本身错了。** `10` = 喂回的 `tool_result` JSON 约 40 个 ASCII 字符。
上游每轮读的是**整个会话文件**,不是我方这轮送的那几十字节 —— 真实上下文 12 万,
我方报 10,差四个数量级,客户按 10 个 token 付输入。

全量统计(1,513 条 cursor 记录):**782 条(52%)报 input < 1000**,而同批会话里
其他轮显示 10k–100k;其中 **183 条(12%)`cache == input`**(占比 1.0)。

### 根因:两个方向的误差,我先后各踩一次

`input` 基准有三个候选,前两个都错:

| 基准 | 错法 |
|---|---|
| `sim_total`(上一批用过) | 标定 10 条样本中位 **0.741**(少收 25%),重铺轮离群 **26.6 倍**(超收 27 倍) |
| `fresh_in`(这一批之前用) | 工具轮只量喂回的 tool_result,**低估四个数量级** |
| **`ctx_base + fresh_in`(现版)** | 只跟"我方实际喂过什么"走 |

两个方向的误差其实是**同一个判据的两侧**:上游那条会话里到底装着多少。

- `--resume` 老会话 → 上游确实有调用方给的那段历史 → 从 `sim_total` 起算
  (偏低约 25%,少的是 Cursor 注入的服务端 system ≈26k,方向安全)
- **新开会话(含重铺)** → 调用方照旧带全量历史,但我方只把最后一段用户输入喂了
  上去,那段历史上游根本没有 → 从 **0** 起算。标定里那条 26.6 倍正是这种轮次
- 续阶段 → `上一阶段的基准 + 上一阶段的输出`,会话又长了这么多

累加式对上游真实吃进去的量,重铺后自动归零重算,两个方向的病因都被挡住。

### 顺带修:`round` 让 cap 在小基准上失效

`(input × cap).round()`:input 10、cap 0.95 → 9.5 → **round 得 10** → 占比 1.0000,
余量归零,等于没夹。改 `floor`。上面那 7 条 `10/10` 就是这么来的
(两处都改:`estimated_usage` 与 `fallback_cache_from_sim`)。

### 影响:这是**涨价**

工具轮从"按几十 token 收输入"变成"按真实上下文收输入(其中约 95% 走缓存价 0.1×)"。
以 1585725 那条为例(输出 1,145):¥0.00143 → 约 ¥0.0085,涨 6 倍。

这个方向是对的 —— 上游每次内部调用确实重读整个会话,kiro 通道也是每轮报全量上下文
(它每轮重传历史,被迫如此)。**两条通道的客户账单必须可比**,否则客户拿 cursor 的
单价对 kiro 的账会对不上。但它确实是客户可见的价格变化,不是纯 bug 修复。

### 验证

- `cargo test -p gw-cursor`:**255 passed / 0 failed**
- 新增两条回归测试:①自估轮基准 = `ctx_base + 新增量`(锁四个数量级那个洞);
  ②新开会话首阶段 `ctx_base` 必须为 0(锁 26.6 倍超收)
- 既有那条「夹到 cap 而非全额」的测试改用 `floor` 期望值
- `cargo check --workspace --all-targets` 干净

### 遗留

- 真值轮拿到的仍是**整个 CLI 进程的累计值**,不是那一轮的量 —— 这是结构性的
  (`result` 只在进程退出时发一次),换 wire 驱动也一样(`turn_ended` 只在 turn
  结束时来,工具轮我方在 `tool_use` 处 break,上游根本不发)。真正的出路是官方
  事后对账 API(`/teams/filtered-usage-events`,按 `conversationId` join),未验证。
- wire 路径只解 `turn_ended` 的 field 1..3,**field 4 `cache_write_tokens` /
  field 5 `reasoning_tokens` 全丢**。生产抽查 6,430 条 `cache_creation_tokens` 恒 0。

## [cursor-cli-cache-calibration] CLI 驱动:接上计费旋钮 + 两次自我修正 — 2026-08-17

接上一批。用户指出上一批一个根本性口径错误:

> 「你这里说缓存＜输入,这是 openai 的计算方式,anthropic 协议下,输入是指抛去缓存的
> 输入,你看 kiro,那个是对的」

对的。上一批把「`cache < input`」当健康态,那是**落库口径(total)**;线缆是
**Anthropic 口径(input 已剔除缓存)**。顺着这条线查下去,发现 cursor 通道**从来没有接过
kiro 那套计费闸门** —— 三个旋钮、sim store 的 TTL,全都没接。

### 缺陷 4:`apply_hot_settings` 在 cursor 上是空实现

`gw-kiro/src/usage.rs::reported_cache_read` 那三个旋钮
(`cache_read_multiplier` / `cache_cap_ratio` / `cache_floor_ratio`)cursor 侧根本不存在,
面板改了没有任何效果。

修:`cache_sim.rs` 移植 `CacheBilling` + `reported_cache_read`(与 kiro 同公式:
`frac = hit/sim_total`,`reported = clamp(frac × total × mult, total×floor, total×cap)`),
`gw-cursor/src/lib.rs` 实现 `apply_hot_settings`(仅present字段)并让
`hot_settings_supported()` 返回 true —— 否则按上一批 `claude-dario` 那个 no-op 的教训,
面板会显示"已生效"而实际是哑的。

**⚠️ `multiplier` 在 cursor 路径上是死参数**。标定实测 `frac` 落在 0.79~0.998,
乘 1.8 必然越过 cap —— 有命中的样本 4/4 全部撞 cap。要调让利幅度只能动 `cap_ratio`。
(kiro 上 multiplier 有意义是因为它每轮重发全量历史,frac 分布低得多。)

### 缺陷 5:cursor 的 sim store 从未接 TTL 配置

`worker/mod.rs` 的启动与 30s 热重载**只同步 `gw_kiro` 的 store**,cursor 那份用编译期
常量 300s,而线上配的是 1800s —— **短 6 倍**,大量会话被误判冷启动。

`CacheSimStore` 当时连 setter 都没有(`set_ttl_secs` / `set_max_sessions` 本次补),
所以这个值在生产上从来改不动。第一条标定样本就是证据:上游真值命中 8180,
我方 `sim_raw_hit=0`。

### 缺陷 6:真值轮丢弃上游 cache 后不回落模拟值 —— 用户报的现象

上一批对不可信的上游 cache 采取「丢弃」,但**丢弃后没有回落到模拟器**。于是:
自估轮有缓存、拿到真值的那一轮缓存为 0 —— 客户看到的就是
**「突然完全没有任何缓存命中」**。线协议侧早就有这道回落闸(`chat.rs`),CLI 驱动没有。

实测代价:一条 `input=85951` 的记录,上游 cache 162,176 被丢弃归 0,而模拟器手里有
57,336 —— 客户被收 ¥0.02744,应收约 ¥0.0103,**多付 2.4 倍**。

修:`fallback_cache_from_sim()`,真值轮 cache 为 0 且模拟器有命中时回落,并夹到
`input × cap_ratio`。

### ❌ 我引入又撤掉的两个缺陷(必须记下)

**(a) 自估基准误用 `sim_total`。** 理由当时是"和 kiro 对齐"——错了:kiro 每轮重发
全量历史,所以它的上下文总量 == 上游 input;CLI 驱动不重发(历史在 `--resume`
会话文件里)。生产标定 10 个样本 `sim_total / upstream_input`:
min 0.042 / P25 0.609 / **median 0.741** / P75 0.847 / max **26.633** ——
9 个低估(系统性少收约 25%),而那个 26.6 倍出现在**重铺轮**(超收 27 倍)。
已退回 `fresh_in`(本轮真实喂进 CLI 的量)。

**(b) 缓存夹到 `input` 本身。** 结果 `cache == input` → 线缆 `input − cache = 0`,
既是"本轮 100% 命中"的假断言,又让整轮按 0.1× 计。生产实测
**57/63 条(90%)全额折扣**,740,153/822,784 = 90.0% 的输入按缓存价走。
**造成约 20 分钟(15:26–15:47)的真实收入损失,不可追回。**

坏估计的根因值得单独记:我拿**真值轮**的样本比例(19%)去推全体,而量在**自估轮**——
自估轮 `input` 只量本轮新增(几十~几千),模拟命中却覆盖全上下文(几万),两者量级差
几十倍。**样本选择偏差**:少数派轮次的比例不能外推到多数派轮次。

修:两处(`estimated_usage` / `fallback_cache_from_sim`)统一夹到 `input × cap_ratio`。

### 标定观测(仅记录不计费)

真值轮打一条 `cursor-cli:缓存标定样本`,同时记上游原始值、我方计费值、模拟命中、
模拟总量。**坑**:第一版读的是 `usage.real_cache_read_tokens` —— 那已经被丢弃逻辑清零,
于是"上游报了但被我丢弃"被记成"上游真值=0",算出假的 `ratio=0.000`。
改为在 `usage_from_result` **之前**捕获原始值。

`real_cache_read_tokens` 是对账列(admin `usage.rs` 拿它算真实成本),
**模拟值一律不写进去** —— 生产验证 `rc=0` 全部正确。

### 生产验证(`cache-cap-20260817`)

| 指标 | 报障时 | 上一版(b) | 现在 |
|---|---|---|---|
| 零缓存(用户报的现象) | 频发 | 0 | 0 |
| 全额折扣(整轮 0.1×) | — | 57/63 = 90% | 0/3 |
| 客户侧输入 | 常显示 0 | 常为 0 | 35 / 13 / 33,531 全为正 |

两条小额记录占比 **0.9499 / 0.9494**,精确卡在线上 `cap_ratio=0.95`,证明夹限生效。
`caio-router` / `caio-worker0` 全程未重建(`--no-deps`)。

### 遗留

- `cacheWriteTokens` / `reasoningTokens` 仍未计入(`cache_creation_tokens` 恒 0)。
  前者对应 Anthropic 的 `cache_creation_input_tokens`,通常比普通输入更贵 —— 计不计是定价决策。
- 生产 `floor_ratio=0.75` 意味着**冷启动零命中也按 75% 缓存价**,这是明确的让利。
- 根本问题没解:CLI `result` 事件只在**进程退出**时发一次,中途的
  `thinking`/`assistant`/`tool_call` 事件不带 token 字段,而一个 CLI 进程横跨调用方
  多个 HTTP 轮次(实测见过 21 次工具调用)。所以**末轮=真值(28%)、中间轮=估值(72%)**
  是结构性的,不是实现缺陷。要拿到每轮真值只能改走协议模拟(`agent.v1.AgentService/Run`)。


## [cursor-cli-cache-coherence] CLI 驱动:三个缓存计费口径 bug — 2026-08-17

同日第七批。用户原话:「只要有输入,就没有缓存命中,这是个严重的计费bug」。
查出来是**三个**独立的口径错误,外加一处对抗评审揪出的**发布顺序竞态**。

⚠️ 本批有一个**我方向搞错、被实测推翻**的修复(Bug 1),过程留在下面 —— 那类
「从聚合统计反推字段语义」的推理方式本身是错的,记下来防复发。

### 口径基础(三处判断都建立在这上面)

`ChatUsage.input_tokens` 的契约是**总输入(含缓存命中)**:

- 发客户前 `chat::delta_usage_json_impl` 做 `input_tokens = input − cache_read`
  (Anthropic 语义:input 只算未命中);
- 落库 `request_logs.input_tokens` 存 **total**;
- 计费 `reported_tokens = input + output`。

两个口径差一层 —— 搞混会得出完全相反的结论(见记忆 caio-cache-billing-and-hot-settings)。

### Bug 1:`cache_read > input` 让客户侧「输入」显示 0

生产实测(近 4 小时 grok-4.6 有缓存记录 127 条):**49 条(39%)`cache_read > input`**,
线缆侧 `input − cache_read` 的 `saturating_sub` 把 `input_tokens` 归 0 —— 客户面板
显示「输入 0 / 缓存 36 万」。真实样本 `id=1572918`:`input=72811 / cache=366720`;
最极端比值 **239 倍**(`input=54 / cache=12928`)。

**⚠️ 我第一版的诊断是错的,已被对抗评审顶回来。** 当时据这 39% 推断
「上游 `inputTokens` 是未命中新增」,于是改成 `input + cached` —— 那会给**每个正常轮次
凭空加一倍输入**。判据是实测单次调用(生产抓的原始 NDJSON,提示词「只回一个词:ok」
约 10 字符):

```json
{"inputTokens":6779,"outputTokens":35,"cacheReadTokens":6016,"cacheWriteTokens":0}
```

10 字符的提示词不可能有 6779 token 的**新增**输入 —— 6779 是总量(AGENTS.md/system 等),
其中 6016 命中。所以 `inputTokens ⊇ cacheReadTokens`,与"总输入"契约天然一致,
**原样填**。`cache > input` 不能反推字段语义。

真实成因:CLI 一次会话内部会发起多次模型调用,`cacheReadTokens` 是跨内部调用**累加**的,
`inputTokens` 不是 —— 两个字段不同口径,不可相减。

处理(**这里也走了一次弯路,两版都记下**):不去重构总量(重构不出可信值),对不可信的
cache 只能二选一 ——

- ❌ 第一版**封顶到 input**:不变式是恢复了,但 `cache == input` → 线缆侧
  `input − cache` **恒 0**,客户面板「输入」列照旧是 0。部署后实测 5 条里 **3 条(60%)**
  被封成全等,比修之前的 39% 还高 —— **用户报的现象一点没好**。而且 `cache == input`
  等于断言"本轮输入 100% 命中缓存",是假断言。
- ✅ 现版**丢弃该 cache(置 0)**:`input` 经单次调用实测证明可信,cache 与它不同口径、
  两者没有可信的相对关系,那就只留可信的那个。客户侧 `input − 0 = input`,输入列显示
  真实总量;代价是这些轮次不给缓存折扣(客户按全价付)。方向:**宁可少给折扣,
  也不谎报命中、更不把输入显示成 0**。

丢弃时 warn:既是上游口径异常的信号,也是这些轮次没拿到折扣的原因,必须可查。

顺带修:`real_cache_read_tokens` 原来漏填 → 面板「真实缓存」列恒 0(不可信的值同样
不进对账列)。

### Bug 2:自估轮 cache_read 结构性恒 0(我方贴钱)

CLI 驱动这条路**从来没接过 `cache_sim`** —— peek/commit 只写在 `chat.rs` 线协议侧。
工具回路的每一轮都是调用方独立的一次 HTTP 请求,发 `tool_use` 那一刻上游还没给
`result`,只能自估(`SsePhase::estimated_usage`),而自估路径的 `cache_read` 写死 0。

生产实测:grok-4.6 近 4 小时 674 条成功请求,**555 条(82%)缓存为 0**。
客户看到的现象就是「同一条会话里有的轮有缓存、有的轮一点都没有」。

修法:入口备好指纹材料(`SimRequest`),在 `start_conv` / `resume_conv` 里 peek 成
`SimSlot`,泵**独占**本阶段那一份、构造 `SsePhase` 时读它的 `cache_read`。命中是
**加**在自估新增量上 —— `in_tally` 记的是本轮真实喂进 CLI 的新文本,若改成从里面减,
`saturating_sub` 会把它归零,又变成「输入 0」。

### Bug 3:tools 双重计费(多收客户,自查发现)

写完 Bug 2 自查"加模拟缓存会不会双重计费"时发现真的会:指纹序列是
`[system][tools][各轮消息]`,而首轮 `in_tally` 是 `prompt + tools` —— **tools 在两边都有**。
命中覆盖 tools 时两者相加,把上万 token 的工具清单算了两次。

判据**不能**是"有没有命中":工具清单变更时前缀正断在 tools 块上(只剩 system 命中,
见 `tools_change_breaks_prefix_after_system`),那一轮 tools 确实是新发的、必须计费。
所以按**命中是否越过 tools 块**判(`SimSlot::covers_tools`,阈值 = system+tools 段
的 token 总量),为真时 `first_in_tally` 不再 push tools。

### Design Rationale

- **模拟值绝不写 `real_cache_read_tokens`**:模拟是计费策略、不是事实断言,对账列只认
  上游自报 —— 与 `chat.rs:2640` 的补偿闸同一条纪律。
- **槽的所有权跟着"哪一轮把结果喂回来"走,不放会话上**(对抗评审 high#2)。
  一个 `SimSlot` 描述的是**某一次调用方 HTTP 请求**,而 `CliConv` 是会话级、同
  `conversation_id` 的并发请求共享它 —— 会话单槽会被后来者覆盖,于是 A 的账单用到 B 的
  命中数、甚至 A 的 commit 提交了 B 的指纹(代际 CAS 只保护表的一致性,保护不了
  「谁的账单用了谁的槽」)。改成:泵持有本阶段的槽(局部变量,独占),
  下一阶段的槽经 `PendingSlot.responder` 通道随 tool_result 一起送进来。
  结构上不可能错配,也消掉了"装槽晚于唤醒"的竞态(两者现在是同一次 `send`)。
- **peek 尽量靠后**(对抗评审 high#3):代际号从读出到 commit 之间越久越易被推进。
  入口只造 `SimRequest`(未 peek 的材料),`resume_conv` 里**先校验 tool_result 匹配、
  后 peek** —— 错配的请求根本不算一次有效轮次,让它先读状态只会白白拉长竞态窗口。
- **commit 判据 = 「这个 Anthropic 响应已交付给调用方」**,不是「整条 CLI 会话成功」
  (对抗评审 high#1/medium#4)。tool_use 处交付了就提交:对调用方而言那是一次完整成功
  响应(有正文、有 tool_use、`stop_reason=tool_use`),它已据此计费,下一轮带
  tool_result 回来时那段历史确实在上游手里。之后断线/报错**不回滚** —— 那是下一轮的失败,
  且真发生时 `ConvRegistry` 分叉校验会换掉 conversation_id(= 换掉模拟键),旧条目自然
  命不中。两处收尾由此统一:提交时机不再取决于模型是否恰好走了 tool_use 分支。
- `covers_tools` 只在 `start_conv` 消费:续轮 `in_tally` 只含 tool_result,不含 tools,
  天然无重复。

### Notes & Caveats

- **这三处都改的是口径,不是定价**。Bug 2 修完客户付得更多(原来是运营方贴钱)、
  Bug 3 修完客户付得更少、Bug 1 只影响显示与缓存列上限。想借机让利应调 `floor_ratio`
  (记忆里 0.6→0.9 = 输入侧整体让利 41%),不要靠留着 bug 来实现。
- 存量记录不追溯修正(`request_logs` 硬限 10000 行,15.2h 就滚掉)。
- **过程教训**:Bug 1 第一版方向搞反了(把封顶写成了加法),靠的是**实测单次调用样本**
  才判出来 —— 聚合统计(39% 的记录 `cache > input`)证明不了字段语义,单次调用的
  原始 NDJSON 才行。对抗评审(`gpt-5.6-sol`)顶回了这一条,以及槽所有权 / peek 时机 /
  commit 判据三条。
- **仍未做**(评审提的、判为可延后):`cache_sim::peek` 目前返回 token 数,`covers_tools`
  是拿 token 阈值反推结构边界(`cache_read >= header_tokens`)。当前指纹语义下等价且有
  三态测试,但更稳的接口是让模拟器直接返回**命中的指纹条数**。若将来改成允许部分
  fingerprint 命中,这个判据会失真。
- **`cacheWriteTokens` 仍未处理**:上游 result 实测有第四个字段(见上面那条 NDJSON),
  而 `cache_creation_tokens` 至今恒 0。它对应 Anthropic 的 `cache_creation_input_tokens`,
  按量计费通常比普通输入更贵 —— 待评估是否计入。
- **`result.usage` 的累计范围已取证澄清(原为上线阻断,现已排除)**:开
  `CURSOR_CLI_DUMP_NDJSON` 抓真实会话后确认两件事 ——
  ① `cacheReadTokens` 跨 CLI **内部**多次模型调用累加而 `inputTokens` 只算末次,
  故 `cache > input` **只出现在走 MCP 桥的多轮会话**上(桥会话 `input=9844/cache=35840`;
  单轮会话 `input=50227/cache=5888` 一律正常);
  ② `result` **不跨调用方 HTTP 轮累计** —— 四条走桥的多轮会话(桥调用 6/6/4/4 次)
  其 result 都精确等于**单独一条** `request_log`,不是多轮之和。所以桥挂起期间各轮
  走自估、末轮走 result 真值,两者**不重叠、不重复计费**,评审的这条阻断级担忧不成立。
  ⚠️ 若将来改动收尾时机(让 result 补算整条会话),这条不变式即失效,须重新取证。
  取证后已撤掉转储变量并 `shred` 掉转储文件(含客户对话)。
- 新增 7 条回归测试(口径三态 + `covers_tools` 三态 + 无槽基线);全量 1,401 passed / 0 failed。

## [cursor-cli-notice-bootstrap] CLI 驱动:提示词与本轮 tools 解耦 — 2026-08-17

同日第六批。用户原话:「用户用 claude 总是说模型不会工具调用」。查出来是我方提示词
把模型劝退的 —— **87% 的会话被写进了一句否定断言**。

### 故障现场

线上 835 份 `AGENTS.md`,**729 份(87%)**内容是「你没有任何工具可用:没有网页搜索、
终端、文件读写」。只有 106 份带工具清单、103 份含 `GetMcpTools` 指引。

模型不是不会调工具,是**在服从指令**:它每轮重读这句话,然后据此拒绝调用。

### 因果链

CLI 驱动的 system 经 `AGENTS.md` 投递(CLI 原生 rules 位置),而:

- CLI **每轮重读**该文件 —— 这正是选它的理由:`--resume` 只喂对话增量、不含 system,
  所以文件通道不受"续轮输入被截断"影响(见 `clidrv.rs` 的 `--resume` 注释);
- 但 system 是按**本轮** `req.body.tools` 重算后**覆写**同一个文件的。

于是:

```
首轮  客户端带 tools  → 写入「你只能调用这些工具…」+ GetMcpTools 指引
续轮  客户端不带 tools → 覆写成「你没有任何工具可用」   ← 否定断言,污染整个会话后续
```

文件跨轮持久,一次错误覆写污染后续所有轮次。内容比对那层(`if read != system`)
防不住 —— 内容确实变了。87% 这个比例说明**续轮不带 tools 是常态**,不是偶发。

根子上的错:API 语义里「本轮缺省」与「确实无工具」**不可区分**,所以不能从空 `tools`
推出任何能力断言。

### Fixes

- **注入逻辑删掉 `tools.is_empty()` 分叉**(`lib.rs`),恒定注入同一段说明。
  这同时回答了"怎么保证只注入一次":内容不随轮变化 → 幂等 → `prepare_home` 原有的
  内容比对天然生效,不需要会话态记忆、也不需要"首轮才写"的判断。
  `tools` 仍传给工具桥(决定是否起 MCP server),只是不再影响文案。

- **文案改为稳定 bootstrap**,四段全是环境事实,**不含任何能力断言**:
  1. 工具由 gwtools(MCP)提供、**不在初始函数表里** —— 找不到是预期现象,
     先调 `GetMcpTools` 拿清单与 schema(不说这条,模型会判定网关在说谎然后放弃);
  2. **必须用清单里的完整名字**,给出可照抄的对照示例(`mcp__gwtools__read_file`
     而不是 `read_file`),并说清短名调用的后果是**静默截断**(实测最高频失败);
  3. 工具跑在**调用方机器**上,有完整读写与执行权限;
  4. ask 的只读限制**只管 CLI 自己的本地沙箱**(空临时目录,不是用户仓库)——
     别拒绝任务、别让用户切模式(他不在 Cursor 界面里)。

  顺带修掉一个长期隐患:代码默认值此前**没有** `GetMcpTools` 那段,该指引只活在
  生产 DB 热覆盖里 —— 任何让 overlay 失效的操作(如旧 admin UI 保存设置冲掉该键)
  都会让线上退回无指引版本。现在代码默认值自带。

### 设计取舍(经对抗评审修正)

原方案是「会话内单调」:把见过的工具清单记进 `CliConv`,续轮为空时沿用不降级。
评审否掉了它 —— 现状是**过度禁用**,单调合并是**过度授权**,后者风险更高(shell/
写文件/凭据类工具)。还有几条未考虑到的:同名工具 schema 变了不能当同一能力;
并发续轮无 revision/CAS 仍会乱序覆写;清单只增不减会让 prompt 越来越长。

采纳的结论:**持久 prompt 不保存瞬时能力状态,只教模型如何查事实**。真实清单由
`GetMcpTools` 返回,那是唯一权威 —— 消除了「`AGENTS.md` 声称一套能力、`req.body.tools`
声称另一套」的双重真相。评审另建议的 MCP 侧按会话过滤清单 + 网关层强制 allowlist
属权限模型改造,本次**未做**(独立加固项)。

### 注意

- **存量未清**:那 729 份被污染的 `AGENTS.md` 仍在磁盘上,进行中的老会话下一轮
  仍会读到 —— 新代码只在有请求进来时重写该文件。
- **DB 热覆盖仍生效并覆盖代码默认值**。线上那份内容接近(也讲 GetMcpTools),但
  **没有**新增的"完整名字 + 全名示例"那段。要让它生效需清空该键或同步更新。
- 新文案约 700 字且**无条件注入**,纯问答会话也会读到这段讲工具的说明。判断这个
  代价远小于 87% 污染,但它确实是笔开销。

### 测试

新增两条回归锁:必须含 `GetMcpTools` + 完整名字要求 + 全名示例 + 截断后果;
以及**禁止**出现「你没有任何工具可用」/「你只能调用这些工具」。242 项全绿。

---

## [cursor-cli-proc-leak] CLI 驱动:进程泄漏(循环等待 + 孤儿帮工)— 2026-08-17

同日第五批。用户原话:「38个进程没有回收这个问题也必须查清楚,这个问题很严重」。
查的时候已经涨到 **74 个进程 / ~13GB**(总内存 32GB,free 只剩 1.9GB)。

### Fixes

- **`CliConv::kill_procs` 是个空壳**(`fn kill_procs(&self) {}`,注释还写着"所有权在
  pump 里,kill_on_drop 兜底")。那个假设不成立,而且错法是**循环等待**:

  1. 泵因为「不是 CLI 自己退出」的原因 break —— `CLI_TIMEOUT` 240s 或桥连接中断;
  2. 调 `kill_procs()`,空壳,子进程还活着;
  3. 紧接着无限 `stderr_task.await` —— 那个 task 在读子进程 stderr **等 EOF**,
     子进程活着就永远不 EOF → **永久阻塞**;
  4. 泵任务永不返回 → `PumpArgs.cli`(`Child`)永不 drop → `kill_on_drop(true)`
     **永不触发**;
  5. 子进程只有被杀才会死,而唯一的杀手正卡在等它。

  **现场取证**:泄漏进程的 stderr 读端**全部**仍被 worker 持有(= 卡在第 3 步);
  6 小时内「会 break 但 CLI 不自退」的事件 **19 次**(12 次 `单轮超过` + 7 次 `桥连接中断`),
  与当时 >600s 的泄漏进程数 **13** 同量级。另有 26 次 90s 空闲掐流 —— 那个只掐调用方
  一侧的流、**不动泵**,泵会继续跑到 CLI_TIMEOUT,正是上面 12 次的来源。

  **修法**:`kill_procs` 真的去杀,而且杀**整个进程组**;收尾顺序改成
  「先杀组 → tokio `kill().await` 兜一手兼收尸 → 收 stderr **带 2s 超时**,超时 `abort()`」。
  即便 kill 因为任何原因没生效,泵也一定返回、`Child` 一定 drop。

- **孤儿 `worker-server`(33 个 × ~163MB ≈ 5.4GB)**。`cursor-agent` 会自己再拉起
  `node index.js worker-server` 帮工。SIGKILL **不传播**,只杀主进程会把帮工留下;
  而 worker 在容器里是 **PID 1**(`/proc/<pid>/status` 的 `NSpid: <host> 1` 实测),
  孤儿于是重挂到 worker 名下 —— 但 worker 是 tokio 进程,只 wait 自己的 `Child` 句柄,
  **不会收养/回收陌生孤儿**,于是永久驻留。
  **修法**:spawn 时 `process_group(0)` 让 CLI 自成进程组,`kill -KILL -- -<pgid>`
  一次收干净。不引 libc(与本文件 `chown` 的既有做法一致)。

### Design Rationale

- **为什么记进程组而不是 pid**:要连孙进程一起收。`CliConv.pgid` 在 spawn 后立刻填,
  会话淘汰(`CliConversations` 换新)与泵收尾两条路都靠它。
- **为什么 stderr 收集要带超时**:它是这次死锁的直接绞索。杀组之后正常会立刻 EOF,
  超时纯属兜底 —— 但**必须有**,否则任何"杀不掉"的新原因都会重现同一个死锁。
- **测试用真进程组验,不用 mock**:`sh -c 'sleep 300 & sleep 300'` 造一个"父+孙",
  断言孙进程跟着死。已验证它对旧空壳实现会**干净地 FAILED**(2s 内),不是空测试。
  测试里刻意**先判定再收尸** —— 反过来写的话 `child.wait()` 会等满 300s,
  表现为挂死而非断言失败(实测过)。

### Notes & Caveats

- ⚠️ **90s 空闲掐流不会终止会话**,只掐调用方这一侧的流。CLI 与泵继续跑到
  `CLI_TIMEOUT`(240s)才收 —— 这也是「`tool_result` 与挂起的 `tool_use` 不匹配」
  那个 400 的温床:会话还活着、挂起槽还在,而调用方那边已经重试并被**线协议驱动**
  接走了(线协议透传上游 id,形如 `call-<uuid>-0`,与 CLI 驱动的 `toolu_<32hex>`
  天然对不上)。同一个会话在两个驱动之间弹 = 我方问题,不是客户带错 id。
  **本批未修这条**:它要改工具回路的语义(掐流时是否连带终止会话),有把正常多轮
  循环搞坏的风险,单独评估。
- 存量泄漏进程需要**手动清一次**:>600s 的必然是泄漏(正常上限 = CLI_TIMEOUT 240s
  + 一次 PENDING_TTL 280s = 520s),清掉还能顺带解开卡死的泵。

## [cursor-cli-tool-round-usage] CLI 驱动:工具轮次不再记 0 用量 — 2026-08-17

同日第四批。用户报障原话:「有大量的空输出,这是为什么?但是用户说是有回复的」,
外加 new-api 面板截图 —— 一连串 `115,834 / 0`、`116,130 / 0`。

### Fixes

- **以工具调用收尾的请求,用量全部记 0**(占 cursor 全部请求的 **56%**)。
  `clidrv::SsePhase::finish_tool_use` 里硬写着 `{"input_tokens":0,"output_tokens":0}`,
  而且**全仓只有 `finish_done` 一处** push `StreamItem::Usage` —— 于是这些请求既不报给
  客户、也不落库。客户那边正文照收(所以"有回复"是真的),账面上是空的。

  生产实测(近 3 小时 1259 条,team4/5/6 + test/test1/ultra-test):

  | 收尾方式 | 请求数 | 落库用量 | 送出的正文+thinking 字符 |
  |---|---|---|---|
  | 成功 / `end_turn` | 545 | 全部 > 0 | 698,818 |
  | 成功 / `tool_use` | **632** | **全部 = 0**(仅 1 例外) | **226,736** |

  相关性 100%。yapi 侧显示「大输入 / 0 输出」是因为它的输入是自己数的,输出取我方 usage。

  **为什么结构上是 0**:抓 `cursor-agent` 原始 ndjson 确认,用量**只出现在整个会话结束时
  的那一条 `result` 事件**里(`thinking`/`assistant` 事件一个 token 字段都没有)。工具
  回路里每一轮是调用方独立的一次 HTTP 请求,发 `tool_use` 那一刻上游还没给数。

  **而且不是"延后到最后一轮结算"**:按会话深度(cache_read+input)分桶看
  `真值/估算` 比值 —— 无缓存 1.77 / 浅 2.28 / 中 3.06 / 深 2.74,**不随深度增长**。
  若是整会话累计,深桶会高出一个量级。所以那 22.6 万字符是真丢了。

  **修法**:工具轮次改自估(线协议侧早有 `estimate_usage_fallback` 兜同一个坑,CLI 驱动
  是后加的,漏了)。新增 `TokenTally` 累加 `thinking` + 正文 + **tool_use 参数 JSON**
  (纯工具轮的产出大头就是参数,正文可以是零个字),收尾时报进 `message_delta.usage`
  并 push `StreamItem::Usage`。

### Design Rationale

- **为什么攒字符再算一次,而不是每段调 `est_text_tokens` 相加**:那个函数对 ASCII 是
  `ceil(n/4)`,每段都向上取整;流式几百个 delta 累加下来光取整误差就能虚高一倍多。
  `TokenTally` 攒总数最后算一次,结果与"整段一次性估算"逐字节相同(有测试锁)。
- **校正系数 1.85 是标定出来的,不是拍的**。拿 901 条**有上游真值**的 `end_turn` 请求
  反标定:`est_text_tokens(正文+thinking+参数)` 只有真值的 **0.54 倍**(中位 0.58,
  P5=0.17 / P95=0.97),总量对齐需要 1.85。差额来源是**隐藏推理** —— 流里的 `thinking`
  是摘要,上游按完整 CoT 计费,与「加密 CoT ≈ 摘要 2.3 倍」那次测量同向。
- **系数只乘 output**。input 侧没有对应的隐藏量,乘了等于凭空多收。
- **input 只算"本轮新增"**:首轮 = prompt + 工具定义,续轮 = 喂回去的 tool_result。
  不含 system(走 AGENTS.md,CLI 每轮重读,在上游基本全是缓存命中)、不含会话历史
  (`--resume` 的历史在 CLI 自己的会话文件里,我方这轮并没送上去)。算进来会重复计费,
  还会搅乱 cache_read 口径。
- **cache_read 恒 0**,统一由 `cache_sim` 在收尾处给 —— 与线协议同一条纪律:同一个量
  不在两处估。

### Notes & Caveats

- ⚠️ **这是估算**,只保证总量口径对得上。单条请求可能偏 2 倍以上(见 P5/P95)。取值
  偏保守一侧(1.85 是总量对齐值而非 P75),宁可少收也不多收。重新标定的口径:
  `stop_reason='end_turn'` 且自身正文 ≥50 字符的样本算总量比。
- 线协议侧的 `estimate_usage_fallback` **没有**这个系数,大概率同样低估。没跟着改:
  那条路是生产主力且它的数字是历史计费值,动它要单独标定 + 单独评审。
- 顺手补了一处**可观测性缺口**:`take_pending_matching` 的 tool_use 错配以前只回给
  客户、不落日志(生产 grep 6 小时日志 0 命中,只能从 `request_logs` 的 400 反查)。
  现在打 warn 并带上双方 id —— 实测客户会带回自己框架生成的 `call-<uuid>-0`,与我方
  `toolu_<32hex>` 天然对不上,不打出来根本判不出是谁的问题。

## [cursor-tier-and-cli-default] 账号档位可见 + CLI 驱动转默认 + CLI 出口代理透传 — 2026-08-17

同日第三批。前两批见下面两条 —— 这批全部来自那之后 40 分钟的线上排查。

### Fixes

- **档位在后台完全看不出来**(用户报障原话:「为什么 team 号显示不出额度」)。三个当天
  新买的号在 20 分钟内被上游降级成 FREE,而面板上只有一片空白。三个原因叠在一起:
  1. FREE 档的 `GetCurrentPeriodUsage` **不给** `includedSpend`/`limit`(免费号没有
     套餐内额度),而 `parse_period_usage` 对此一律报错 → **整个配额查询失败** →
     额度栏空白,与"查询失败"长得一模一样;
  2. 档位回填拿的是 `q.currency`。那对 kiro 成立(currency 装的就是 `KIRO PRO` 这类
     档位名),对 cursor **不成立**:gw-cursor 写死 `currency = "USD"`,于是每个 cursor
     号的 `subscription_title` 都被填成字符串 `"USD"`;
  3. `warm_subscription_titles` 只补「缺这个字段」的号 → **填错一次永不复查**;而判付费
     用的是「`subscription_title` 存在且不含 FREE」——`"USD"` 不含 FREE,于是**降级号
     一直被当付费号**,继续被派发它已经没权限的模型。第 3 条正是 grok / composer 反复
     `ModelNotAvailable` 的下游成因。
  **修法**:`AccountQuota` 新增 `plan_tier`;cursor 按「有没有套餐内额度」判 `FREE`/`PAID`;
  FREE 不再报错(`used`/`limit` = 0 是事实而非未知);worker 回填**优先 `plan_tier`、
  `currency` 兜底**(kiro 走兜底分支,行为逐字节不变);`/health` 与账号表暴露该字段,
  FREE 直接打红标 + 悬浮提示「额度栏空白 ≠ 查询失败」。
- **CLI 驱动静默直连**(默认化路上踩出来的,比原问题严重)。`start_conv` 里 `env_clear()`
  之后只塞了 `HOME`/`PATH` —— 什么都不继承,所以**账号配的代理压根没进 CLI 子进程**。
  实测:三个代理号切到 CLI 驱动后全部失败(表象是 `cursor-cli 桥连接中断`),而记忆里
  直连出口的封号率是 **59.5%**(代理 0%)。也就是说漏这段不只是不通,是在烧号。
  **修法**:从 `extra.proxy` 派生 `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`(大小写两套)
  塞进子进程,回环走 `NO_PROXY` 免得把 MCP 桥绕进代理。

### Features

- **CLI 驱动成为默认**,线协议改成显式退出(`extra.driver="wire"` 单号退出、
  `CURSOR_DRIVER=wire` 整个 worker 退出;历史值 `"cli"` 仍合法 = 默认)。后台开关与
  表格徽章同步翻转:默认档不再打标,**只对退出线协议的号打标**(异常才该显眼)。

### Design Rationale

- **为什么把默认翻过来**。线协议给上游发的是**裸模型名**(`grok-4.6`),CLI 驱动发的是
  CLI 那套名字(`cursor-grok-4.6-high`)。当天 06:50 前后上游停止接受裸 `grok-4.6`、
  随后 `grok-4.5` 也一样,线协议上的三个号在几分钟内全部 `ModelNotAvailable`,而同一批号
  切到 CLI 驱动**立刻恢复** —— `cursor-agent --list-models` 实证上游只有
  `cursor-grok-4.x-*` 那一族。裸名那套是我方逆出来的、会被上游单方面收走;CLI 用的是
  **官方客户端自己在用的名字**,后者才是长期站得住的一侧。加上 CLI 驱动本来就带真实
  usage(含 `cacheReadTokens`)与 MCP 工具桥,没有理由再让它当可选项。
- **代理那段是实测过才写的,不是照惯例猜的**。cursor-agent 是 Node 程序,而 Node 默认
  并不认 `HTTPS_PROXY`,所以必须先证伪:把 `HTTPS_PROXY` 指向 `127.0.0.1:1`,
  `--list-models` 当场 `ECONNREFUSED 127.0.0.1:1`;不设则正常 →它走的是认环境变量的
  HTTP 客户端,`NODE_USE_ENV_PROXY` 不需要(加了也不变)。
- **档位只报能证实的二分,不猜档位名**。`GetMe` 实测只有 `authId`/`email`/`country`/
  `isEnterpriseUser` 这些,**没有会员字段**;`GetTeams` 对这些号返回 `{}`(所以它们
  根本不在任何 team 里,名字只是我方台账);其余 7 个猜测的方法名全 404。唯一稳的判据
  就是「有没有套餐内额度」,那就只报 `FREE`/`PAID`。
- **只缺一个金额字段仍然报错**。原注释的顾虑是对的(`unwrap_or(0)` 会把故障显示成
  零额度),只是漏了 FREE 这个第三种形态。所以判据收窄成「两个都缺 = FREE,只缺一个 =
  上游改了字段」,两个方向都有单测钉着。

### Notes & Caveats

- ⚠️ **部署必须连带清库**:已有 cursor 号的 `extra.subscription_title` 是被污染的 `"USD"`,
  不清掉新逻辑也不会复查(只补缺字段的号)。清理语句见部署记录(`json_remove` 该键,
  条件 `= 'USD'`,不动别的值)。
- ⚠️ **这批把每个 cursor 号都推上了 CLI 驱动**(除显式 `wire`)。每请求 spawn 一个
  `cursor-agent` 子进程,并发上限按号累加 —— 上线后要看一眼宿主内存。
- team1/2/3 当天的临时措施(`driver=wire` + 排 grok 的白名单)在部署后一并撤掉,统一回
  默认;它们仍是 FREE + disabled,不会因此吃流量。

## [cursor-cli-ws-isolation] CLI 工作区改每会话一份(修跨会话提示串味)+ 调用方侧空闲闸 — 2026-08-17

上一条 `[cursor-inject-routing]` 部署后盯了 30 分钟,盯出两件新事。

### Fixes

- **跨会话提示串味 + 工具闭集错乱**(真事故,线上取证)。`prepare_home` 把调用方**本次请求的
  system 提示**写进 `AGENTS.md`,而工作区是**每号一份**(`<home>/ws`)—— 同号并发的两个
  会话互相覆盖这个文件。`ultra-test` 同时在跑两种流量:一个无工具的角色扮演会话
  和多个带 24~98 个工具的 Claude Code 会话。取证时刻的 `AGENTS.md` 里装的是
  **「你没有任何工具可用」+ 角色人设**,而同期带工具的请求的 CLI 读的就是那份。
  后果在模型自己的 thinking 里:
  - 「已确认可用工具为 Shell、Read、Write 等 Cursor 本地工具,**gwtools 未直接列入 MCP 列表**」
  - 「注意到 **Ask 模式限制与 gwtools MCP 工具使用之间的冲突**,需按工作区规则处理」

  除功能错乱,这还是**跨会话的提示泄漏**:甲客户的 system 提示躺在乙客户的 CLI 工作区里。
  HOME 按号隔离(`account_uid` + 700)挡的是**号与号**,同号内的会话之间此前没有边界。
  **修法**:工作区改 `<home>/ws/<conversation_id>/`,`AGENTS.md` 与 `assets/` 随之隔离;
  `auth.json` / `mcp.json` / `permissions.json` 仍每号一份(它们本来就是号级的)。
  另加**迁移清理**:旧版那个 `ws/AGENTS.md` 恰好落在新工作区的**父目录**,而 Cursor 会
  往上层找 rules —— 不删等于把串味原地保留,所以见到就删。
- **调用方侧 300s 空等**。`resume_conv` 把 tool_result 喂进挂起槽后直接返回 `drain_stream`,
  而那个流**没有任何超时**:CLI 之后若一声不出,调用方一路等到 gw-app 的 300s
  `STREAM_IDLE_ABORT` 才收到一个空响应。实测占 tool_result 接续请求的 1.0%(切换前)~
  7.0%(切换后 30 分钟,n=43)。**修法**:`drain_stream` 加 90s 空闲闸
  (`DRAIN_IDLE_TIMEOUT`,与线协议 `chat::STALL_TIMEOUT` 同值),超时发一条带原因的
  错误并终止本次响应,**不动 CLI 进程**(调用方重试走 `cli_lookup`,该重铺就重铺)。

### Design Rationale

- **为什么不是去修 `CLI_TIMEOUT`**。它的注释写着「单轮 CLI 调用的硬上限……保证先于我方
  上层超时干净地杀掉进程」,读起来正是该管这件事的那个闸。但它量的**不是墙上时钟**:
  检查点在泵 `select!` 的 500ms tick 分支,而桥挂起是在 `call` 分支内部 await 一个 280s
  的 timeout,那段时间 select 停转、tick 不响 —— 挂起等待**不计入**预算。
  而这恰恰是**必要的**:一个 CLI 进程横跨调用方的多个 HTTP 回合,10 轮工具往返的墙上
  时钟轻易超过 240s;把挂起算进去会误杀正常的长工具回路。所以该加闸的地方是**调用方
  那一侧**,不是进程侧。注释已按实际语义改写(原注释是错的,会把下一个人引到误修上)。
- **GC 判据用显式写下的 `.last`,不用目录 mtime**。目录 mtime 只在增删目录项时变,一个
  跑了半小时、期间只读文件的活会话其目录 mtime 可能很旧 —— 拿它当判据会删掉**正在用的
  工作区**,而"CLI 的 cwd 被删掉之后会怎样"是另一场排查。没有 `.last` 的目录一律当过期
  (只可能是旧版残骸或写标记失败的残骸),当前会话用**路径**排除而不是名字比较。
- **会话 id 为空时落 `_noconv` 而不是退回父目录**。退回父目录会静默恢复串味;给一个确定
  的兜底目录,最坏情况是所有无 id 请求共用一个,但不会污染有 id 的会话。

### Notes & Caveats

- ⚠️ **磁盘**:每会话一个目录,靠 `WS_TTL`(2h,与 `SESSION_TTL` 同值)回收,回收在
  `prepare_home` 里顺手做。`chown -R` 仍作用于整个 HOME,目录多了会略慢 —— GC 保证有界。
- ⚠️ 上线后活跃会话的工作区会**换路径**(从 `ws/` 到 `ws/<conv>/`),CLI 的 `--resume`
  靠 HOME 下的会话存储而不是 cwd,所以续写不受影响;但**图片附件的绝对路径变了**,
  上一轮提示里引用的旧路径在新目录下不存在。影响面限于"刚发过图、下一轮立刻续问"。
- 本条与 `[cursor-inject-routing]` 的两条修复同属 CLI 驱动,`caio-worker-cursor` 独立容器,
  仍**不动 kiro 数据面**。

## [cursor-inject-routing] 分流中转注入的 `role:"system"` 消息 + CLI 驱动 ask 模式说明 — 2026-08-17

### Fixes

- **「grok 收到空消息」**(用户报障;线上从 02:11 `ultra-test` 切 `driver=cli` 起持续
  ~3 小时)。真实流量里客户端经中转会在**每条用户消息之后**再追一条
  `{"role":"system","content":"<total_tokens>15000000 tokens left</total_tokens>"}`,
  而 `chat::to_turns` 判 `is_user` 用的是 `role != "assistant"` —— 于是这条**每轮都变的
  预算计数器成了最后一轮**。两条路径同时被带偏:
  1. CLI 驱动的 prompt 取 `raw_turns.last()`,发给上游的整条 prompt 就是那行计数器。
     取证来自 grok 自己的 thinking:*"The user's message contains only a token count
     indicator."* —— 用户明明打了一大段,看到的回复是「你这条消息是空的」。
  2. `chat::last_tool_results` 要求末条是 `role=="user"`,尾巴是 system 就返回 `None`
     → 挂起的 MCP 桥调用**永远接不上** → 模型反复说「MCP 读取被中断了」,
     伴随 90s stall 与 `incomplete_stream`。
  **线协议那条路侥幸没露**:`fold_history` 无条件全量重铺,尾巴上多一行计数器无伤。
  所以症状表现为「切了 CLI 驱动才开始空」,极易误判成 CLI 驱动本身坏了。
- **修法(三处,均在 gw-cursor)**:
  - 新增 `chat::route_system_role_messages`,在 `Provider::chat` 的**第一行**原地改写
    `body`,四级分流:稳定前缀(SessionStart hook / CC 身份行)提升进顶层 `system`;
    动态噪声(预算计数器、task-tools 催促)丢弃;interrupted-user 取正文转 `user`;
    其余未知裹 `<system_context>` 转 `user` **原位**保留。无 `role:"system"` 消息时
    **一个字节都不改**(快路径)。
  - 新增 `chat::latest_user_input`:CLI 驱动的 prompt 取「最后一条 assistant 之后的
    整段」,不再取末条。正常形态下与旧行为逐字节相同。
  - `chat::affinity_key_from_body` 的锚点跳过 `role=="system"`:该函数由 worker 在
    `chat()` **之前**用原始 body 调用,那时分流还没跑;锚点取到每轮都变的计数器
    等于亲和归零。

- **模型自我审查成「只读」并要求用户切 Agent 模式**(同日同一用户报障的第二个问题)。
  CLI 驱动把 `cursor-agent` 钉在 `--mode ask`,模型据此拒活:生产 thinking 原文
  「当前处于 Ask 模式,仅可读取与分析,无法修改代码」,正文则要求用户
  「在 Cursor 把模式切到 Agent,再发一句 execute now」—— **用户根本不在 Cursor 里**,
  这条建议无从执行,任务就此卡死在"请你切模式 / 我不是 ask 模式"的死循环上。
  **`--mode` 无解**:`cursor-agent --help` 实证只接受 `plan` / `ask` 两个值、都是只读;
  写权限只能整个不传 `--mode`。
  **修法**:新增热配置 `cursor_cli_notice`,在**有工具声明**的 CLI 驱动请求上追加一段
  说明,把真相讲清楚 —— ask 的只读限制只作用于 CLI 自己的本地沙箱(那是个空的临时
  目录,不是用户的仓库),真正的执行通道是 gwtools 工具、跑在**用户自己的机器**上、
  有完整读写权限;因此不要拒活,也不要让用户去切模式。

### Design Rationale

- **为什么在入口一次性改写 body,而不是逐个函数打补丁**。被这条尾巴带偏的不止两处:
  `to_turns` / `history_fps` / `cli_eligible` / `fold_history` / `cache_sim` 全都假定
  `messages` 里只有 user/assistant。逐个加「跳过 system」是五处各自记得,漏一处就是
  下一次同类事故;在入口把不变量建立起来,下游一处都不用改。
- **两道修复刻意冗余**。`route_system_role_messages` 治根(已知注入形态),
  `latest_user_input` 是兜底:分流器的最后一档是「不认识就裹起来转 user」,那种未知
  注入照样会落在尾巴上。取整段而不是取末条,这一类只让 prompt 多一段说明,
  而不是把用户的话整条替换掉。回归测试里两条各自独立断言,证明**任一条单独**
  都能让用户的话回到 prompt 里。
- **动态噪声的判据收窄到「整条消息就是这个东西」**。放宽成子串匹配的话,用户正文里
  提到 `<total_tokens>` 就会让半条消息凭空消失 —— 静默丢用户内容比多带一行噪声坏得多。
  单测正反两向都钉住了。
- **没有把实现上收到 gw-core 与 kiro 共用**(`strip_rolling_fingerprints` 当初是那么做的)。
  kiro 那份 `converter::normalize::route_system_role_messages` 跑在 `Vec<Message>` 强类型上,
  共用要动 kiro 的转换管线,而 kiro 是生产主力面;这份跑在裸 `Value` 上,且多一条 kiro
  没有的规则(预算计数器)。两份口径若要合并,应作为一次独立改动、单独回归 kiro。
- **顺带把 `system` 拍平成字符串是安全的**:全仓 `get("system")` 只有 `extract_system`
  一处读它,而那本来就是拍平取文本。
- **为什么留着 ask 模式而不是放开写权限**。不传 `--mode` 会一并放开 CLI **自己**的
  写文件/终端工具,而它们作用在容器里那个空工作区 —— 模型会报告「已改好文件」,
  调用方机器上什么都没变。那是**幽灵编辑**:比"拒活"坏得多,因为拒活看得见。
  ask 模式是刻意保留的安全闸,代价(自我审查)用提示词补。
- **文案走热配置而不是硬编码**。`cursor_tool_guard` 的历史已经证明这类文案必然要
  反复调(第二版因为"点名禁掉的能力与调用方声明的工具逐字重合"把合法工具一起吓退,
  第三版才换成正向写法)。这段同理:上限 3000 字符、写侧就拦长度、空串=回内置默认、
  **校验失败保留上一份有效值绝不静默回默认**(否则一次误配置会表现成"文案悄悄变了一版",
  而效果正在被按小时对比)。
- **只在有工具声明时追加**。一个工具都没声明时模型确实什么也改不了,那时告诉它
  「你能改文件」才是骗它 —— 会换来一轮伸手抓不存在工具的空转。

### Notes & Caveats

- ⚠️ **kiro 也在吃这条噪声,本次没治**。kiro 的三级分流会把预算计数器归到「未知」
  → 裹 `<system_context>` 转 user 保留,于是**每轮多一条内容都在变的 user 消息**,
  前缀缓存在那个位置断掉。kiro 靠全量重铺不会像 cursor 那样功能性失效,所以本次
  按「别碰 kiro 数据面」的约束留着。要治就是给 kiro 的 `is_dynamic_system_noise`
  加同一条规则 —— 独立改动 + 独立回归。
- ⚠️ **上线后活跃会话各会重铺一次**:`history_fps` 的取材变了(注入消息不再计入),
  CLI 驱动的 `cli_lookup` 前缀比对首轮必然不匹配 → 走 Fresh 折叠重铺。一次性代价。
- ✅ **已部署**:镜像 `claude-all-in-one:inject-routing-20260817`(05:51 UTC 构建),
  `caio-worker-cursor` 05:52:49 UTC 起跑它。`caio-router` / `caio-worker0` /
  dario 全部未动(仍 `overflow-20260814-061145` / `poison-fix`),**kiro 数据面零变更**。
  线上验证:切换后带注入 `role:"system"` 的真请求成功返回(105k input / 7.3k output /
  6k cache_read),响应里既没有「消息是空的」也没有「Ask 模式 / 切到 Agent」;
  `ultra-test` 工作区的 `AGENTS.md` 已含新 CLI 说明与工具闭集。
- ⚠️ **本仓库的 `docker-compose.yml` 里没有 `worker-cursor` 这个服务** —— 它只存在于
  服务器侧那份(rsync 一直排除 compose,两份早已分叉)。别照着仓库这份得出
  「cursor worker 不受 compose 管理」的结论。
- 本次顺手消掉两处 compose 漂移(服务器侧):镜像标签从 `cli-driver-20260817-2`
  改成实际在跑的版本,并补上容器一直手工带着、compose 却漏了的 `CURSOR_DELTA_HISTORY=0`
  —— 漏这一条时任何人跑一次 `docker compose up -d` 都会**静默改掉行为**。
  容器此前是 `docker run` 起的(compose 不认它,`up -d` 会报名字冲突),
  本次 `docker rm -f` + `compose up -d --no-deps worker-cursor` 之后由 compose 接管。
- ⚠️ **重启把 `pro3` 从内存禁用里放了出来**:它的 `TooManyFailures` 是进程内状态,
  重启即清,于是欠费号重新进轮转、3 发 0 成功。已 `PATCH disabled=true` 落库止血
  (这次是持久状态,重启不会再复活)。**下次重启 cursor worker 前先看一眼禁用池里
  有没有靠内存态压着的死号。**

## [cursor-driver-switch] 驱动形态成为后台可切项 + `data/` 穿越权限修复 — 2026-08-17

### Fixes

- **CLI 驱动上线即 EACCES 的根因**:`clidrv::start_conv` 用 `cmd.uid()` 降权 +
  `cmd.current_dir(ws)`,而 Rust `std` 的 `do_exec` **先 setuid 再 chdir**;CLI 的 HOME
  在 `/app/data/cursor-cli/<acc>/ws`,而上一条 CHANGELOG 要求的 `data/` **700**
  让降权 uid 连 `/app/data` 都穿不过去 → `chdir` EACCES,被报成「启动 cursor-agent 失败」。
  **不是二进制权限**(它是 755;`docker exec -u <uid> -w <ws>` 能跑通,因为 docker 的
  `-w` 是 root 身份先 chdir 再降权 —— 正好反证顺序)。共享 nobody 时同样挡,与每账号
  uid 改造无关。
  **修法(运维侧,已在 139 生效)**:`chmod 711 data/`(o+x 可穿越、不可列目录)+
  `chmod 600 data/gw.db data/state.db`。安全性质不变,已逐条实测:自己 HOME 能 chdir、
  `control.db`/`gw.db`/`state.db` 全 denied、`ls /app/data` denied、换个 uid 进不了别号 HOME。
- `models.rs::list_reports_real_context_windows` 漏取 `CATALOG_TEST_LOCK`,与热追加
  测试撞车偶发 `34 vs 35`(实测三跑必中一次)。补锁后连跑三轮 221/221。

### Features

- `PATCH /admin/api/accounts/{id}` 新增定点字段 **`driver`**:`"cli"` = 子进程驱动
  官方 cursor-agent,`""` = 清除回线协议,缺省不动。走 `merge_account_extra`,绝不碰
  凭据;认不出的值 **400**(fail-closed —— 静默收下会让 UI 显示与实际跑的驱动不符)。
  `GET` 侧顶层回显 `driver`。
- 后台账号编辑弹窗加「上游驱动」两档切换(**仅 cursor 家族**显示:别的 provider
  读不到 `extra.driver`,露出来只会误导);账号表 provider 列给 CLI 驱动的号挂徽章 ——
  同一 provider 下两种上游形态,出问题时第一件要分辨的就是「这号走哪条路」。
  切换约 30s 内经 worker sync 生效,不用重启。

### Design Rationale

- **为什么用一个权限位修,而不是改代码。** 三条候选:①`chmod 711 data/`;②把 CLI HOME
  挪出 `data/` 挂成独立卷;③自己接管 fork/exec 让 chdir 排在降权之前。选 ① 的理由是
  它零代码改动、**立即生效不用重启**(正扛生产流量的容器不能动),而且安全性质可以
  **逐条实测**给出结论:`o+x` 只给穿越、不给列目录,`control.db` 仍是 600 root。
  ② 要改 compose + 迁移已有 HOME,③ 等于自己重写 `Command` 的 spawn 序列 —— 两者
  的风险都远大于一个权限位。代价见 Caveats:①是**运维侧状态,不在代码里**。
- **`driver` 走定点 merge 而不是整块 `extra` 替换**,与 `proxy_url` / `priority` /
  `queue_enabled` 同一条纪律:整块替换会把「打开弹窗那一刻的凭据快照」写回去,
  期间若发生过 OAuth 轮换就把新 token 冲掉。定点合并绝不碰凭据字段。
- **认不出的驱动名为什么 400 而不是静默回落线协议。** 读侧语义是
  `opt_str("driver") == Some("cli")`,即**除 `"cli"` 以外的一切值都会跑线协议** ——
  静默回落不是可能,是必然。那样后台写着 CLI、实际跑线协议,排障的人会先去怀疑
  上游而不是怀疑自己填错了字。fail-closed 把错误钉死在写入侧。
- **UI 只对 cursor 家族显示这个开关**:`extra.driver` 只有 `gw-cursor` 读。给 kiro 的号
  露出一个写了也没用的旋钮,比不给更糟 —— 它会让人以为 kiro 也能这么切。
- **表里挂徽章而不是新开一列**:这个信息只有一个 provider 有,单开一列要动表头和
  所有行的排布,而「哪条上游路径」本就是 provider 的属性,挂在 provider 列旁边语义更贴。

### 2026-08-17 生产实测

`chmod` 后先用 pro3 的 uid 手工直跑 CLI(不经调度器):进程起来、`apiKeySource=login`、
模型解析成 `Cursor Grok 4.6 High`,倒在 `ActionRequiredError: unpaid invoice` —— **号欠费**,
不是代码。换有额度的 `ultra-test` 同法直跑:思考帧 + 正文 + `result.usage`
`{input 3068, output 41, cacheRead 9728}` 全回来,3.75s 收尾。

随后把 `ultra-test` 切到 `driver=cli` 接生产流量,15 分钟:**grok-4.6 55 成 / composer-2.5
37 成 / grok-4.5 8 成 1 败(99%)**,`cursor-cli:模型调用桥接工具` **27 次** —— MCP 桥回路
在容器里也成立。`选号失败:并发已满` 从约 12 条/分降到约 7 条/分。

### Notes & Caveats

- ⚠️ **本地 spawn 失败被记进账号健康**:`scheduler.rs` 把 `UpstreamErrorKind::Other`
  计入 `failure_count`,3 次即 `TooManyFailures` 停用且**不自动恢复**。于是「我方文件
  权限配错」把 pro3 判成了坏号(02:11:36 三连 EACCES → 自动禁用,至今未回轮转)。
  同段注释里 `Overloaded`/`ModelNotAvailable` 都刻意不惩罚账号,spawn 失败该进那一档。**未修。**
- `pro3` / `test1` 上游均报 unpaid invoice;cursor 池实际可用并发只剩 `test`(2)+
  `ultra-test`(4),这是 503 的真正来源,属付款/补号范畴。
- ⚠️ **`data/` 的 711 是运维侧状态,不在代码里。** 换机、重建卷、或照着上一条
  CHANGELOG 的「部署要求 `data/` 700」重做一遍,CLI 驱动就会**再次** EACCES,
  而报错文案指向 cursor-agent、不指向权限位。要根治只有两条路:把 711 写进部署
  脚本/文档,或把 CLI HOME 移出 `data/`(那时 `data/` 可以回到 700)。
- ⚠️ **本条的 API/UI 改动尚未部署。** UI 内嵌进二进制、admin 平面跑在 `caio-router`
  上,所以后台要看到这个开关必须**重建镜像并重启 router** —— 而 router 是 kiro 的入口。
  重启时必须照抄 worker0 的 env(`KIRO_LEGACY_WIRE=1` + `KIRO_THINKING_IN_HISTORY0=1`),
  并且**不能用 `docker compose up -d`**:compose 里 worker-cursor 仍钉
  `cli-driver-20260817-2` 且缺 `CURSOR_DELTA_HISTORY=0`,一跑就把已上线的 cursor worker
  退回旧镜像。线上 `ultra-test` 的 `driver=cli` 目前是直接 SQL 写进 `extra` 的,
  与本次 API 写入的形态**逐字节等价**(`json_set(extra,'$.driver','cli')`),
  所以先不部署也不影响它继续跑。
- 重建 router 会一并把 `b1ed1ab` 之后的累积改动带上 kiro 数据平面。已核过影响面:
  **无 DB 迁移**(共享 `control.db` 不会被写进旧二进制不认识的 schema)、
  `Provider::poll_token_updates` 是**默认空实现且 gw-kiro 没覆盖**、kiro 编译单元里
  只有 `converter/normalize.rs` 改成委托 `gw-core/normalize.rs`(501 条测试锁死逐字节等价)。

## [cursor-cli-hardening] CLI 驱动安全/完整性加固(对抗审查共识落地) — 2026-08-17

### Security

- **每账号独立 uid**(替代所有 CLI 共用 nobody):`clidrv::account_uid` 按
  account_id 派生稳定 uid(100_000..500_000),HOME chown 给该 uid 且 700。
  共用 nobody 时同 uid 下 700/600 不构成边界 —— 被 prompt 注入的 A 号 CLI 能
  `cat` 走 B 号的 auth.json(评审共识 S0-1)。
- **桥 socket 收口**:0666 → 0600 且属主=本账号 uid。共享 nobody 时任何被注入
  的 CLI 都能连上别人的桥 socket 注入工具结果(S0-2);bridge/ 目录本就藏在
  700 的 HOME 里(路径不可达),socket 属主是第二道。

### Fixes

- **token 双写者捕获**(评审共识:唯一不靠流量/攻击者、定时自炸的一条):
  CLI 自刷新会回写 auth.json,号库里的旧 refresh_token 随之作废 —— 不捕获的话
  gw-app 下次 OAuth 刷新 invalid_grant,号被永久误判死。现两道捕获:
  `prepare_home` 按 JWT exp 对账(文件新→上报;号库新→覆写文件)+ 泵任务每 5s
  观测 auth.json 变更;gw-app sync 循环经新增的 `Provider::poll_token_updates`
  取走,CAS(同 token 跳过 / expires_at 不更新跳过)后增量落库。
- **桥挂起槽按 tool_use_id 键控**(S1-7):`resume_conv` 只消费 id 匹配的
  tool_result,错配显式报错且保留槽位 —— 静默喂错结果是语义损坏,比报错严重。
  `chat::last_tool_results` 替代旧拼接版,严格形态必须带 id。
  (跨客户串话此前已由 cursor 专属的 client-key 亲和命名空间挡住,见
  `affinity_scoped_by_client`,本次未改。)
- 思考帧一律透传(不再看客户端开没开 thinking):丢思考 = 丢进展信号,还会让
  stall 看门狗误判掐流。
- CLI 模型映射一律非 fast 档(fast 变体加急计费,成本高):grok-4.6/4.5 →
  `cursor-grok-4.x-high`,opus-5 → `claude-opus-5-thinking-high`,
  composer-2.5 去 -fast。

### Test

- 新增 6 条:uid 派生稳定性/区间、prepare_home 双向对账(文件新上报/号库新
  覆写/同 token 不重写)、pending 键控消费(错配保留槽位)、last_tool_results
  严格形态、adopt_provider_token_updates 落库 + 拒旧回声。

## [cursor-cli-driver] cursor 通道子进程驱动(包裹官方 cursor-agent CLI) — 2026-08-17

### Features

- 新增 `gw-cursor/src/clidrv.rs`:以子进程驱动官方 `cursor-agent` CLI 作为上游。
  每号独立 HOME(token 写 `auth.json` 即登录,CLI 自刷新),`-p stream-json`
  逐事件转 Anthropic SSE,`--resume` 跨进程续会话(服务端持史实测成立),
  usage 为上游**真实值**(含 cacheReadTokens)。
- 新增 `--mode cursor-mcp-bridge`(`gw-cursor/src/mcpbridge.rs`):MCP stdio 桥,
  把调用方声明的 tools 经 `gwtools` MCP server 暴露给 CLI;调用挂起→网关发
  tool_use→调用方带回 tool_result→CLI 续跑,输出桥接到新响应(live bridge)。
- 图片:附件落每号工作区 + 提示词带路径(ask 模式只读工具读图,实测识别正确);
  PDF 沿用文本层抽取内联。
- 会话分叉防线:逐轮指纹前缀校验,分叉换新会话重铺(见 PROTOCOL §20.2)。
- 线协议侧新增 CLI 形态(`cli.rs`,`CURSOR_PROFILE=cli`,默认关):单轮已实测
  出字+正常收尾;续轮持史缺「哈希回显」握手,留作后续攻坚(§20.2/§20.5)。
- 开关:账号 `extra.driver="cli"` 或 `CURSOR_DRIVER=cli`(灰度用),默认关,
  线协议 IDE 形态不变。

### Security

- CLI 固定 `--mode ask`(read-only),绝不加 `--force`;MCP 白名单
  `{"mcpAllowlist":["gwtools:*"]}` 只放行桥工具。
- ask 模式仍允许只读 shell:CLI/桥子进程降权 **nobody(65534)** 运行;
  **部署要求 `data/` 700、`control.db` 600**,否则模型可 `cat` 走号库。
- 镜像烘入 cursor-agent 2026.08.11-e8db854(钉版本,官方 tarball,约 +200MB)。

### Test

- 单测:全 workspace 1487 条全绿(新增 NDJSON 事件分类/去重、CLI 帧结构、
  会话通知识别、提交回显收尾等 9 条)。
- e2e 真号(宿主机直跑):纯文本三轮记忆(8866→9000 推理正确)、MCP 桥工具
  回路(桥挂起→tool_result→续答,哨兵串往返)、多轮+工具混合、图片理解,全过。

## [cursor-cch] cursor 会话键被滚动指纹污染:缓存命中恒 0 的根因 — 2026-08-15

### Features

- 新增 `gw-core/src/normalize.rs`:`strip_rolling_fingerprints` 的**唯一实现**。
  gw-kiro 的同名函数改为薄委托(501 条测试原样全绿 = 逐字节等价),死代码删除。
- `gw-cursor::chat::extract_system` 现在剥掉 Claude Code 的滚动 billing 指纹行。
- 新增 `delta_history()`:`Continuation` 相位下**只发本轮**,历史交服务端。
  开关 `CURSOR_DELTA_HISTORY`,**默认关 —— 且已被真号实测否决,不要开**(见下)。

### Design Rationale

- **根因**:Claude Code 在 system 顶部拼一行
  `x-anthropic-billing-header: …; cch=<5位16进制>;`,`cch` 是**每请求都变**的 body 哈希。
  `gw-cursor` 的 `extract_system` 是裸透传,而它的产物同时喂给三处:会话亲和键
  (→ 上游 `conversation_id`)、发给上游的系统提示、`cache_sim` 的指纹。于是:
  会话键每请求都变 → 账号钉扎失效 + `conversation_id` 每轮都变 →
  `ConvRegistry::phase_for` **一次都没返回过 `Continuation`** → 缓存指纹每轮都变、
  命中率恒 0。kiro 早有这道处理,cursor 一直没有。
- 代码里记着 Continuation 实测把命中率从 **32.6% 拉到 49.8%**(单轮最高 98.7%)——
  那份收益 ccmax 流量从来没吃到过。这次修完才第一次可达。
- **`delta_history` 为什么默认关**:开了之后我方不再重传历史,正确性完全押在
  「服务端真按 `conversation_id` 存住了前几轮」这个**我方无法直接验证**的假设上
  (只能靠模型答得对不对间接判断)。押错 = 模型静默丢上下文,比"贵但对"糟得多。
  代码默认因此保持关闭。

### 2026-08-15 上线实测:cch 修复✅ / delta 模式❌(已关)

镜像 `cch-20260815-030821`,只换 `caio-worker-cursor`(router / worker0 / 其余 22 个容器未动)。

**探针 A —— cch 修复,通过。** 不带 `metadata.user_id`、每轮 `cch` 都不同的 Claude Code
形状请求发两轮:两轮落在**同一个账号**(`ultra-test`),`cache_read` 从 2176 涨到 6656。
会话键不再被滚动指纹搅动,前缀缓存第一次真的开始命中。

**探针 B —— delta 模式,否决。** 稳定 `metadata.user_id` 连发三轮(①记住 4712
②聊件无关的事 ③问那个数字),同一构建只翻开关:

| `CURSOR_DELTA_HISTORY` | 第三轮回答 |
|---|---|
| `1`(只发本轮) | 「这次对话里我没收到过你要我记住的数字」❌ |
| `0`(内联全量) | 「4712」✅ |

**服务端在我方这种请求形态下并不替我们持有历史。** 已把容器改回 `-e CURSOR_DELTA_HISTORY=0`。

**连带订正 `PROTOCOL-agent-run.md` §17.3**:那句「两个事实跨 4 轮全部答对 →
历史确实在服务端」有混淆变量 —— 当时请求里**同时内联着全量历史**,分不开
「服务端记得」和「我们自己贴了」。上面这次是第一次把内联那份拿掉的干净检验。
于是 §17.3 的 98.7% 命中要重新归因:收益来自**内联文本被前缀缓存**,
与服务端存不存历史无关。有状态声明真正买到的是**缓存命中**,不是**免传历史**。

### Notes & Caveats

- ⚠️ **上线瞬间所有命中该指纹行的 cursor 会话会一次性迁移**:会话键变了 → 账号钉扎重来、
  服务端会话冷启动。命中率会先掉一下再回升。带显式 `metadata.user_id` 的客户端
  (如 opencode)不受影响 —— 它们本来就绕开内容哈希。
- **`fold_history` 与 `phase` 此前完全无关**(第一版我误判成有因果):`ctx.phase` 只传给
  `build_frame0` 决定环境块/预算表,从不决定要不要折。所以 cch 修复**治不了模型绕圈** ——
  那需要 `delta_history`(本次已备好,待实验后开启)。对抗评审 Skeptic 指出了这个误判。
- 在 `Continuation` 相位下折叠曾被我判为**矛盾状态**(一边声明「历史在你那儿」,
  一边又内联同一段历史 = 喂两遍),`delta_history` 就是为消掉它而写。
  实测之后这个判断也要收回:服务端根本没在存,所以内联**不是**重复,而是唯一的来源。
- ⚠️ **grok 绕圈因此仍未解决。** 「一坨长文本」这个形状目前**没有替代品** ——
  免传历史走不通(本条),repeated `1.2.1` 走不通(上游只回心跳)。
  剩下的着力点是**改渲染本身**:分隔符/角色标记,或把历史挪进 `1.2.1.2` 上下文块的
  独立条目(这才是「结构化报文」在本协议下唯一可能的形态,尚未验证)。
  已上线的 `TOOL_LOOP_NUDGE` 治的是另一个病(本轮只有工具返回、一个问题都没有)。
- 剥离判据锚定 `x-anthropic-billing-header:` **行首**(不依赖 `cc_*` 字段名,CC 升级会改)。
  代价:用户正文里若有以该前缀开头的行也会被删。这是 kiro 长期沿用的取舍
  (它的测试就锁了"中间行也剥"),沿用不改。

## [openai-wire] cursor 通道开 OpenAI 入口(chat/completions + responses)— 2026-08-14

### Features

- **`gw-core/src/openai/`**:新的边界适配层,八个文件、零新依赖(复用已有 `sha2`/`uuid`)。
  - 入站:`chat_req.rs` / `resp_req.rs` 把两种 OpenAI 请求转成 Anthropic Messages body;
    `inbound.rs` 放两者共用的零件(tools / tool_choice / 内容分片 / 采样参数 / 工具返回值)。
  - 出站:`chat_out.rs` / `resp_out.rs` 各一个流式状态机 + 一个非流式折叠;
    `usage.rs` 管两种用量形状,`error.rs` 管错误形状。
- **worker 多两条入口**:`POST /v1/chat/completions`、`POST /v1/responses`,
  **只在 `provider.family() == "cursor"` 时挂载**。转换完立刻接回 `handle_chat`
  (原 `messages` 的主体),选号 / 会话亲和 / 租约 / 重试 / 计费 / 请求日志一行没改。
- **router 多两条转发**:复用 `forward` 的全部逻辑,只把 worker 侧 URL 参数化;
  `parse_session_id` 增加 `gw_core::openai::session_hint` 回退(OpenAI body 取不到会话)。
- **`/v1/models` 同时给两套字段**:追加 `object`/`created`/`owned_by`,原 Anthropic 字段
  一个没动。一个端点同时喂饱 Anthropic SDK 与 NewAPI 的「获取模型列表」。

### Design Rationale

- **内部 IR 仍是 Anthropic,只在边界适配** —— `docs/ARCHITECTURE.md:194` 早就定了这条。
  cursor 上游本来就供 gpt / grok / gemini / kimi / glm(目录 33 项),客户却只能用
  Anthropic 协议去要它们;而 kiro / ccmax 的主链路全程 Anthropic、零转换,那是刻意保住的
  资产(thinking 签名透传、cache_read 计费),不该被 OpenAI 入口稀释。所以**只给 cursor 开**。
- **顺带砍掉一道有损转换**:今天 caio 对 NewAPI 说 Anthropic,NewAPI 再用
  `to_oai_chat_resp.go` 转成 OpenAI —— 那个转换器只认 5 种事件、没有 default 兜底
  (我方 2026-07 为保活帧实测过)。我方原生说 OpenAI,这一跳连同它的损耗一起消失。
- **闸门是结构性的,不是文档约定**:路由按 family 条件挂载,非 cursor 的 worker 上这两条
  路径根本不存在(实测 404)。策略单拎成 `mount_openai_wire()` 以便被测试点名。
- **出站只在最后一步分叉**:新增 `Wire` 枚举一路传到 `finish_response`,`Wire::Anthropic`
  分支原样调用旧函数。所有选号/重试/收尾逻辑共用一份,不复制第二条链路。
- **`reasoning_content` 而不是丢掉思考**:OpenAI 官方 ChatCompletions 没有思考字段,
  用 DeepSeek 系带起来的事实标准(NewAPI 认它);Responses 侧走 `reasoning_summary_text`。
- **请求日志与语料链路口径不变**:入库的仍是**转换后的 Anthropic body / Messages**,
  与 kiro 同一种形状,yapi 那边不用适配。

### 对抗评审(3 个 codex 视角)改掉的东西

评审报了 2 个 high + 一批 medium,**两个 high 都是真的**:

- **[高] 传输 EOF 被当成协议成功终态**(Architect#1 / Skeptic#1)。上游半截断流(没发
  `message_stop`)时,原实现给 chat 补 `finish_reason:"stop"` + `[DONE]`、给 responses 补
  `response.completed` —— 把一个**可检测的截断**变成了静默的假成功。已改成:没见过
  `message_stop` 就 EOF → 报错终态(见 `error::truncated_stream_payload`)。变异测试验过。
- **[高] cursor 的会话身份不含客户维度 = 跨客户串话**(Minimalist#1)。cursor 是唯一有
  服务端会话续写的上游(`ConvRegistry::phase_for` → `Phase::Continuation`,且
  `CURSOR_STATEFUL` 默认开),而 conversation_id 是 `hash(system + 第一条 user)` 纯内容派生。
  同组两个客户只要 system 与开场白相同,就会续在对方的服务端会话上。**这条在本次改动前
  就存在**(Anthropic 入口打 cursor worker 同样成立),但把入口开给 NewAPI 这种多租户中转
  正好凑齐碰撞前提。已在 worker 侧给 cursor 的亲和键补上客户 key 维度
  (`affinity_scoped_by_client`),kiro / dario 不动(它们没有服务端续写,共享键只影响缓存命中)。

其余已改:`/v1/models` 的 OpenAI 字段收窄到只在 cursor worker 追加(共享端点不该为一种协议
改另一种协议的响应);保活不再抢在 `response.created` 前面、也不再出现在终止事件之后;
终态之后的事件一律不再产出帧;`max_tokens` 截断改发 `response.incomplete` 且**不给半截工具
调用补 done**;`Response` 对象改为从 IR 反推真实请求参数(并写明三处有损);
`tool_choice:"none"` 改成**真的撤掉 tools**(上游只读 tools,光写字段是摆设);
空工具输出给占位、分片数组走同一套映射(两者原样透传都会被上游拒);
无 tools 却要求必须用工具 → 400;重复的块起始被忽略;快照按 `output_index` 排序;
旧版 `function_call` / `role:"function"` 往返打通;router 的 404 能力转移改为逐个排除、
不再一次就宣告失败;router 与 worker 边界前的错误(401/503/502/畸形 JSON)也换成 OpenAI 形状。

**驳回**:「删掉 Responses 省 39% 代码」(用户明确要的就是这个协议);
「`max_tokens`/`temperature` 等参数 cursor 不读,应当拒绝」——
Anthropic 入口今天对同一个 provider 同样是静默忽略,让 OpenAI 入口更严没有道理,
只有 `tool_choice:none` 是能兑现的,已单独兑现。

### Notes & Caveats

- **`max_tokens` 缺席时我方补 64k**。Anthropic 侧它必填,OpenAI 侧可选。补一个小值等于
  替客户截断回答,而截断在流式里表现为「话说一半就停」——最难被认出是网关干的那种故障。
- **`previous_response_id` / `item_reference` 明确 400**,不静默忽略:忽略等于把「续上一轮」
  变成「重新开一轮」,客户端拿到一个上下文凭空丢失的回答,查都没法查。
- **Responses 回传的 `reasoning` 条目收下即丢**。cursor 通道拿不到可回放的加密 CoT
  (见记忆 `caio-thinking-blob-extraction`),把 summary 当 thinking 塞回去会得到一个
  **没有签名**的思考块,Anthropic 家族上游直接拒收。
- **`stream_options.include_usage` 为假时不发用量帧**:严格按 `choices[0]` 解析的客户端
  会被空 `choices` 噎住。为真时用量帧必须排在 `[DONE]` **之前**,否则 NewAPI 记 0 用量。
- **保活帧按线缆分形态**:chat 发空 delta chunk,responses 发 `response.in_progress`。
  都不用 SSE 注释 —— 注释在标准解析器里被跳过、不会变成下游事件,客户端照样判定空闲。
- **`reasoning_tokens` 恒 0**:Anthropic 线缆把思考文本算进 `output_tokens`,不单独给数。
  报一个猜出来的值会让客户按它对账,比留 0 更糟。
- **托管工具(`web_search` / `code_interpreter`)静默跳过**:cursor 上游没有对应物。
  为一个我方不认识的工具把整条请求打回去,比丢掉它更糟。
- **尚未对真实上游跑过流式 happy path**:本机没有可用 cursor token 的号,且真发请求要消耗
  用户订阅额度。协议形状由单测(罐装事件序列逐帧断言)+ worker 级 SSE 字节断言覆盖;
  路由、鉴权、转发、错误形状已在本机实跑验证。

## [affinity-r4] 会话亲和:修完对抗评审两轮的高危 + 一个观测计数器 — 2026-08-14

### Features

- **溢出伙伴不再被临时候选覆盖**(第三轮 [高])。`select_id` 的溢出分支现在**再分一次
  原因**:伙伴仅仅并发满 → 本轮临时另选、**不写回** `ent.overflow`;伙伴真失效 → 才换人
  并记住。之前是无条件写回,于是「固定伙伴」在高并发下沿全池漂移,契约等于没实现。
- **上迁目标恰好是现任伙伴时对调,而不是留下重复 ID**(第四轮 [高],三个评审员各自
  独立复现)。时序 `primary=lo, overflow=hi` → 上迁 → 两个字段双双指向 hi → 此后 hi 一忙
  就被同时当成「主号忙」和「伙伴忙」,伙伴槽位被自己占死。处置是把旧 primary 换到伙伴位
  (第五轮 [中] 的建议:清空会让下次溢出按分层 LRU 另挑陌生号,平白多触达一个身份)。
- **`affinity_primary_if_only_busy` 改为委托 `entry_ok_ignoring_busy`**(第三轮 [中])。
  手写版与共享版有真实语义差异(无条件拒 `probation`、跳过 RPM 懒清理顺序),
  「判据不可能漂移」的目标名存实亡。判据只能有一个来源。
- **`sync_accounts` 删号时同时清匹配的 `overflow`,且两张表在同一临界区改**
  (第三轮 [低] + 第四轮 [中])。原先只在被删号是 `primary` 时删整条亲和项;
  且 `drop(entries)` 在亲和清理之前 —— 两个重叠的 `sync_accounts` 之间有窗口能让
  「新 B + 旧 overflow=B」复活本轮要消灭的陈旧伙伴关系。
- **新增 `affinity_spill_total`**(`QueueStats` → worker `/health` 的 `queue`):
  「两个当前槽位双双并发满、本轮临时用第三个号」的事件计数。只观测,不拦截。

### Design Rationale

- **砍掉了做了一半的 `session_account_cap` / `_enforce` 硬上限**(第四轮三人一致 [高])。
  cap 检查只长在 `partner_only_busy` 这一个分支上,而真实失效路径(primary 死了改钉、
  伙伴死了换人)全部绕过它,`AffinityEntry` 也不记历史触达集合 —— 那个「上限」拦不住
  它声称要拦的东西,却引入了三条 high(误报 503、`AllBusy` 语义污染、`busy` 陈旧)。
  加上实测触发面 **0/224 会话**(峰值在途并发 13 / 总槽位 36),决定只留最便宜的先行指标。
- 计数器口径**故意保守**:每事件 +1 而非每请求、`try_lease` 可能还会失败、`busy` 表示
  「本请求此前抢 permit 失败过」而非「此刻仍忙」—— 它是趋势信号(0 → 非 0 = 该补号),
  不是台账。

### Notes & Caveats

- ⚠️ **`affinity_hold_ms=0` 不等于回到改造前的行为**(第五轮 [高],原文档撒了谎,已改)。
  固定溢出伙伴**没有开关**、始终生效:`0` 只关掉「等 primary」这一段。
  想整体回滚只能换镜像。
- ⚠️ **`affinity_spill_total` 有已知盲区**:伙伴真失效被换人、primary 真失效被改钉,
  这两条路上会话都确实多触达了一个身份,但计数器不涨。真实「一个会话触达几个号」
  只能从 `request_logs` 按未加盐逻辑会话键重算(脚本 hopnow.py)。
  测试 `dead_partner_is_replaced_and_remembered` 把这个盲区**显式钉住**了。
- **知情不改的两条**(第五轮 [中],均为既有架构性质,不是本轮引入):
  1. 亲和状态在**选号**阶段就提交(改 primary/overflow、记 rebind),而真实触达发生在
     `try_lease` 成功之后;失败只退 RPM、标 busy,不回滚亲和。要修得引入「租约成功后
     提交」的事务边界。
  2. 每次带 session 的选号都在持 `entries` + `affinity` 双锁时对**全表**做 TTL `retain`,
     高基数 session 下是 O(N²) 并把 acquire 串行化。TTL 只限时间不限条目数。
- `sync_accounts` 的测试只覆盖**顺序**契约,没构造真正的并发窗口(把 `drop(entries)`
  移回去测试仍会绿)。写确定性竞态测试代价过高,这里靠代码注释交接。
- 四条不变量都做了**变异验证**(逐个破坏 → 对应测试 FAILED):忙伙伴不覆盖、
  上迁对调、改钉作废旧伙伴、hold=0 不改钉。534 测试通过,亲和相关 20 条连跑 5 次无抖动。


## [已完成] 账号级可用模型白名单(规格存档)

> 状态:**已于 2026-08-13 按本规格完整落地并发版**(镜像 `allowlist-20260813-105344`,
> 见下方 [model-allowlist] 条目)。本节保留为设计规格存档;
> 文末「另外两条待办」(内建截断伪装成功 / 协议内回失败结果)**仍是待做**,交接给下一位。

### 需求

Cursor 账号的**模型权限不齐**:一部分号只有 Cursor 自家模型(`composer-2.5` / `default`)
和 `grok-*`;claude / gpt 这些第三方前沿模型要另外的计费额度。现状是每个 claude 请求都
可能落到没权限的号上,换来一次上游拒绝(`ERROR_RATE_LIMITED_CHANGEABLE`,带
`autoSwitchToModel`)。调度层的动态学习会把 `(号, 模型)` 记 6h 不可用并换号,
**但那是先赔一次首包前失败才学会**:每个新上线的号、每次 6h TTL 过期都要再赔一次。
静态白名单把这次失败挪到调度之前,一次上游请求都不发。

用户原话:「有的只支持 grok 和 composer,有的都支持,需要配置每个账号哪些模型可用,
不然都支持的账号压力太大,只支持 grok 的又分摊不到压力。」

### 已有的半成品(2026-08-13 随 cursor-tool-guard-hot 一起部署,休眠)

- `crates/gw-cursor/src/models.rs`:`allow_list()` + `pub fn account_supports(account, requested)`
  —— 读 `extra.models`,收逗号串与 JSON 数组,支持 `前缀*` 通配,**判定前先过
  `to_cursor_model()` 归一**(白名单写上游侧名字,客户发来的是带日期后缀/族别名的名字)。
  已有 5 个测试(未配=不限 / 只有自家模型的号挡 claude / 归一口径 / 全名精确不吃前缀 /
  大小写与数组写法)。
- `crates/gw-cursor/src/lib.rs`:`CursorProvider::account_supports_model` 已调它。
- **刻意没进 `CURSOR_ACCOUNT_SCHEMA`**(那里留了一段注释说明原因)→ 后台不渲染输入框
  → 没人能设 → 休眠。测试 `schema_declares_credentials_and_anti_correlation_fields`
  里有一条 `assert!(!has("models"))` 钉着这个状态,做的时候记得一起改。

### 要改什么

1. **改名** `extra.models` → `extra.model_allowlist`。它是通用路由策略,不是 cursor 私有。
2. **纯匹配器挪到 `gw-core`**(如 `account.rs`:`pub fn model_allowlist_allows(account, upstream_model) -> bool`)。
   **不要**新增 `normalize_model_id` trait 方法 —— 各 provider 在自己的
   `account_supports_model` 里与核心匹配器求**逻辑与**,并复用它**真正发包时用的那个映射函数**:
   - cursor:`cursor_native_support(..) && core::model_allowlist_allows(a, &to_cursor_model(m))`
   - kiro:**暂不接线**(保留 FREE-no-opus 原逻辑)—— 用户硬约束「不影响 kiro」
   - ccmax(claude-subprocess)/ dario:**完全不接**
   理由:`Provider::account_supports_model` 的语义已经是「该账号最终能否服务该模型」,
   在 worker 外面再挂第二个判断,等于把最终答案拆给两个所有者。
3. **规范存储 = JSON 字符串数组**;CSV 只当 UI 输入与旧配置兼容。
4. **fail-open 只适用于「字段不存在」**,其余一律在**写侧** fail-closed:

   | 情况 | 行为 |
   |---|---|
   | 字段缺失 | 不限(兼容存量,绝不能因元数据缺失把健康号摘出池) |
   | 合法非空列表 | 按列表限制 |
   | `null` | 规范化成**删除该键** |
   | 空串 / 空数组 | 保存时拒绝,或明确规范化成删除键 —— **不能**解释成「不限」 |
   | 类型错 / 通配符不在末尾 | 400 拒绝更新 |
   | 真要「全禁」 | 用账号 `disabled`,不要靠白名单表达 |

   这条是 gpt 评审里最要紧的一条:运维填了个空值,本意多半是「全禁」,当成「全放」是要出事的。
5. 通配符**只许出现在末尾**;文档里写明 `grok*` 会自动放行未来的 Grok 型号
   (所以它不是严格静态白名单)。
6. **写侧接口**:加一个定点 PATCH 字段,照 `proxy_url` / `priority` 那个模式
   (`crates/gw-app/src/admin/accounts.rs` 约 1482–1508 行)走 `st.store.merge_account_extra`,
   **绝不碰凭据字段**。⚠️ 整体替换 `extra` 那条路**走不通**:`GET /accounts` 回来的凭据是
   `***` 掩码,读不回来也不能原样发回(写侧会拒绝含掩码的回传值)。
   记得把新字段加进 `has_patch` / `poke_workers_sync` 的触发条件(同文件约 1521 行),
   否则改完要等 worker 最多 30s 的周期 sync 才生效。
7. **前端**:`admin-ui/src/features/accounts/components/CursorAccountDialog.tsx` 是**手写字段**
   不是 schema 驱动的,所以要手动加输入框;加完再把 `FieldSpec` 放回
   `CURSOR_ACCOUNT_SCHEMA` 并改掉上面那条 `assert!(!has(...))`。
   **必须适配手机端**(断点从窄往宽写、宽内容裹 `overflow-x-auto`、375px 自查)。
8. **一个独立的危险点**(与白名单无关,但同一片代码):`models.rs::to_cursor_model` 把
   **未知模型名归一成 `default`**。于是只要白名单含 `default`,任意拼错的模型名都会放行,
   客户端**静默拿到另一个模型**。要把「客户端明确要 `default`」与「未知名字回退 `default`」
   分开,后者直接 400。

### 实现约束

- `account_supports_model` 由调度器**在锁内**对每个候选账号调用(`worker/mod.rs` 约 2374 行),
  所以必须**无副作用且快**:只读 `extra`,不查上游,别在里面分配 `String`
  —— kiro 那边是 370 个号的规模。
- 部署铁律:rsync **绝不** `--delete`(排除 `config/`、`data/`、`docker-compose*.yml`、
  `*.bak*`、`.git/`、`target/`、`node_modules/`、`admin-ui/dist/`);`docker build --network=host`;
  改前端要**重建整个镜像**(UI 内嵌二进制)。本次只需重启 `router` + `worker-cursor`
  (compose 第 11 / 41 行),`worker0`(kiro,第 23 行)与 dario(78 / 92 行)一律不动。
- 本地构建/测试必须限内存:
  `systemd-run --user --scope -q -p MemoryMax=10G -p MemorySwapMax=0 cargo …`

### 另外两条待办(同一片代码,优先级低于上面)

1. **内建截断仍伪装成成功**。`gw-cursor/src/chat.rs` 的 `builtin_truncated` 分支照样发
   `stop_reason=end_turn` + `message_stop`,于是 worker 判 `saw_message_stop=true` →
   **请求日志记 200 成功**,还 `h(true)` 确认了会话。12h 内 296 次半截回答在面板上全绿,
   只能靠用户报障发现。要改成不发正常 `end_turn`(发 SSE error 或换 `stop_reason`),
   并按未完成口径落库。注意首包已 committed、换不了号,目标只是「日志里是红的、
   客户端知道被截断」。**会让面板成功率掉下来,那是真实数字**(已跟用户说明)。
2. **协议内回失败结果**(根治内建工具收口)。要先用 `CURSOR_DUMP_TOOL_FRAMES=<目录>`
   环境变量抓几十帧真实内建调用帧(**需要重启 worker-cursor**,用户已同意开这个口子),
   定死 `1.2.2.<N>` 的身份枚举与**请求侧客户端回执消息形状**,然后在同一条 BiDi 流里
   回一个「工具不可用,请改用 `gwtools-X`」的失败结果,让模型在**本轮内**自己纠偏
   (纠偏次数封顶 1~2 次防循环)。
   ⚠️ **绝不猜字段号**:回执被上游忽略 = 90s 心跳死等,比现在的瞬间收口更差。
   已知线索:这些帧在**工具通道 `1.2.2.<N>`** 而不是 exec 通道(收口日志 preview 尾部有
   `<uuid>-N-<4字符>` 的 `1.2.3` 签名);exec 通道的字段号已有 schema 实证可参照
   (`2=shell, 3=write, 4=delete, 5=grep, 7=read, 8=ls, 9=diagnostics, 11=mcp, 14=shell_stream`,
   客户端回执同号),但**两条通道的回执消息不一定是同一个**。
   收口日志已经带上 `cap=`(认出的能力)与不认识的字段号,是这一步的现成线索。
   做完之后还有第三步:能可靠映射时直接把内建调用**翻译**成调用方工具的 `tool_use`,
   把失败变成无感成功。

## [model-allowlist] - 2026-08-13

账号级可用模型白名单(`extra.model_allowlist`)按上方规格完整落地并发版;
顺带补齐 cursor 账号三条用量展示与「收到」刷屏文案修正。
镜像 `claude-all-in-one:allowlist-20260813-105344`,重启 `caio-router` + `caio-worker-cursor`,
kiro(worker0)与 dario 容器不动。

### Features

- **白名单核心**:纯匹配器挪到 `gw-core::account`(`MODEL_ALLOWLIST_KEY` +
  `model_allowlist_allows`,`前缀*` 通配仅限末尾);cursor 侧 `account_supports`
  改为 `cursor_native_support && core 匹配器` 求逻辑与,kiro / dario / ccmax 零接线。
- **语义定稿**(规格第 4 条,写侧 fail-closed):字段缺失/null = 不限;
  空表/类型错/中置通配 = 400 拒绝;规范存储小写 JSON 字符串数组,CSV 只当 UI 输入。
- **未知模型名不再静默回退 `default`**(规格第 8 条危险点):
  `resolve_cursor_model` 未知名返回 None → chat.rs 报 `bad_request_visible` 400,
  拼错模型名的客户端拿到明确错误而不是悄悄换模型。
- **写侧**:`PATCH /accounts/{id}` 定点字段 `model_allowlist`(照 `proxy_url` 模式走
  `merge_account_extra`,绝不碰凭据;空串=清除写 null),`normalize_model_allowlist`
  校验+规范化,进 `poke_workers_sync` 触发条件,`redacted_view` 顶层回显数组。
- **前端**:`EditAccountDialog` 加输入框(不传=不动/空串=清除/变更才进 patch),
  `types.ts` 补 `AccountRow.model_allowlist` 与 `UpdateAccountPayload.model_allowlist`,
  i18n 中英文案;schema 放回 `model_allowlist` FieldSpec。
- **cursor 三条用量**:`usage.rs` 解析 `autoPercentUsed` / `apiPercentUsed` 装进
  `QuotaWindow`(label "auto"/"api",非法值不造窗口),`AccountTableRow` credits 分支
  渲染百分比窗口 —— 此前只显示 on-demand 美元一条。
- `fold_history` 注入文案加「不要重复致意」,修「收到」刷屏根因。

### 验证

- `cargo test --workspace` 全绿(452 通过;`cursor_login_transient_failure_keeps_session`
  并行偶发失败、单跑通过,与本批改动无关的既有 flake)。
- admin-ui:`tsc --noEmit` + vite build 通过,dist 重建后嵌入二进制(sha256 `b9d2650f…`)。
- 生产写侧端到端:设置 `"default, composer*, GROK*"` 落库为
  `["default","composer*","grok*"]`;`gr*k`(中置通配)400;空串清除回显 null,
  测试账号已恢复不限,无脏数据。worker health 正常(8 账号)。

### 注意事项

- `grok*` 会自动放行未来的 Grok 型号,不是严格静态白名单(规格第 5 条,刻意为之)。
- 真要「全禁」用账号 `disabled`,不要靠白名单表达。
- 上方规格存档里的「另外两条待办」(内建截断伪装成功 / 协议内回失败结果)仍未做。

## [cursor-tool-guard-hot] - 2026-08-13

Cursor 内建工具护栏改版 + **文案热更新**;内建截断后下一轮向模型纠偏。

### 背景

Cursor 的内建工具(终端、读写文件、代码库检索、网页搜索)是**服务端自带**的:
哪怕我方一个工具都不声明,模型照样会调,而反代执行不了 —— 只能收口。
worker-cursor 12 小时日志:外部工具成功 **3119** 次,内建工具收口 **302** 次,
其中 **296 次发生在已出字之后** —— 客户端收到「半截回答 + 正常 end_turn」,
无错误、无工具调用。用户报障原话是「工具调用总失败」,看到的就是这个。

落盘 preview 里模型要调的是 `echo …; grep -nE …`(shell)和「按符号名+目录搜代码」,
**全都是调用方已经声明过的能力**(Claude Code 的 `Bash` / `Grep`)。第二版文案
(「声明的工具都是真的,但不要调内建的终端/文件读写/网页搜索/代码库检索」)在这种
局面下**自相矛盾**:被点名禁掉的能力与调用方的 `Bash`/`Read`/`Edit` 逐字重合。

### 改动

- **护栏文案 v3**(`gw-cursor::chat::builtin_tool_guard`):从「禁什么」改成三条正向信息
  —— ① 逐个列出可调用工具的**全名闭集**;② 能力→工具**替代表**(按调用方实际声明的
  名字生成);③ 策略句。工具个数的硬阈值换成 1200 字符预算。
- **能力匹配去掉纯子串档**,只留「全等 > 去符号全等 > 前缀(≥4字符)」:子串会让
  `Thread` 命中 `read`、`TodoWrite` 抢到「写文件」。宁可不出替代行也不指错工具。
- **文案热更新**:`SystemConfig.cursor_tool_guard` + `SystemSettings` overlay
  → worker 启动初载 & 30s 设置环 → `gw_cursor::set_tool_guard_policy()`
  (`cursor_extra_models` 同款先例)。**只有策略句是配置**,闭集与替代表由代码生成。
- **可观测**:`/health` 回显 `cursor_guard_rev`(4 字节指纹,**不回显全文**);
  内建收口 WARN 带 `guard_rev` + `cap`(认出的能力)。
- **下一轮纠偏**(`TruncationNotices`):内建收口按 conversation 记一笔,下次同会话
  请求在用户消息末尾补「上一轮你调了本环境不提供的内建工具,请改用 `gwtools-X` 重做」。
- 写侧校验:`PUT /admin/api/settings` 拦 `cursor_tool_guard` 超长(2000 字符)。

### 设计取舍(gpt-5.6-sol 对抗评审)

- **不做 `{tools}`/`{redirects}` 占位符模板**:动态部分的结构不会变,变的只是那几句
  自然语言。模板一旦拼错会**静默丢掉整个闭集**,而闭集是这道护栏最硬的一半。
- **默认文案不含后果威胁、不点名 Cursor 内建工具**:「否则回答会被截断」可能让模型
  对合法工具也变保守、或转头在正文里解释网关环境;点名内建能力则重演 v2 的自相矛盾。
  两者作为**热配置里的实验变体**存在(改设置即可试),不作第一版默认。已钉成测试
  `默认护栏不含后果威胁也不点名内建工具` —— 防止后人「顺手把话说狠一点」种回旧病。
- **校验失败保留上一份有效值**,不静默回默认:护栏效果正按 `guard_rev` 分桶比对
  收口率,悄悄换一版会让那份数据作废。
- **纠偏认不出能力时不指名工具**:字段号只认抓包实证过的 `.1` 终端 / `.4` 读文件,
  其余记日志返回 None。「你上次调了内建终端」这种自信的错话比模糊的实话更容易带偏模型。
- **纠偏不挂在 `ConvRegistry` 上**:那张表被 `CURSOR_STATEFUL` 门控,而半截回答在两种
  模式下都在客户端历史里 —— 挂进去等于让一个退路开关顺手关掉一个不相干的修复。
- **命名空间隔离**:设置项走 `cursor_` 前缀、代码全在 `gw-cursor`、apply 函数对非
  cursor worker 是无害 no-op。kiro / claude-dario / claude-subprocess(ccmax)零改动。

### 注意事项

- 这是**缓解不是根治**。根治要在同一条 BiDi 流里回一个「工具不可用」的失败结果,
  让模型在**本轮内**自己纠偏。但内建调用在**工具通道 `1.2.2.<N>`** 而不是 exec 通道
  (preview 尾部有 `<uuid>-N-<4字符>` 的 `1.2.3` 签名),客户端回执消息形状未定;
  **猜字段号的代价是回执被忽略 → 90s 心跳死等,比现在的瞬间收口更差**。
  下一步靠 `CURSOR_DUMP_TOOL_FRAMES` 抓实物定死。
- 已知未修:内建截断仍发 `stop_reason=end_turn` + `message_stop`,于是 worker 判
  `saw_message_stop=true` → **请求日志记 200 成功**。那 296 次在面板上是全绿的,
  只能靠用户报障发现。单独一条待办。
- 账号级模型白名单(`extra.models`)逻辑已在 `models::account_supports` 并接上
  `account_supports_model`,但**刻意未进 account schema**(后台不渲染):空值/非法值的
  fail-open 语义与字段命名还没定稿,先上会让人照一版将来要改的语义配一遍。

### 验证

- `cargo test --workspace`:**1335 通过 0 失败**。
- 顺带修一个真 flake:文案测试原本读进程全局策略句,与热配置测试互相顶
  (「单跑全绿、全量跑随机红」)。拆出纯函数 `builtin_tool_guard_with(tools, policy)`
  后文案断言完全不碰全局。

## [cursor-hot-catalog] - 2026-08-13

Cursor 模型目录支持**热追加/覆盖**:加模型、试新模型不再需要重新部署。

### 背景

cursor 上游不提供菜单查询,模型表(含每模型参数)只能内置在代码里
(`gw-cursor::models::catalog`,2026-08-10 对齐真机菜单 33 项)。代价是每加一个
模型都要改代码、构建镜像、切容器 —— 08-13 加 grok-4.6 时又走了一遍全流程,
而它本来只是一次「上游认不认这个名字」的试探。

### 改动

- `gw-core`:`SystemConfig.cursor_extra_models: Vec<ExtraModelSpec{name, params, menu}>`
  + `SystemSettings` 同名 overlay 字段(整表替换语义,与 warmup_group_policies 一致)。
- `gw-cursor`:进程级 `EXTRA_MODELS`(RwLock)+ `set_extra_models()`(lib 单点导出);
  `catalog()` = 内置目录 + 热追加项(**同名整体覆盖**,参数一起换)。
  `menu=false`(默认)= 探测位:可被点名、出现在 `/v1/models`,但**不进** `1.14`
  清单(每个 Run 请求都带的那份)——热配置只允许试,菜单位要回代码对齐真机快照
  才转正(08-13 gpt-5.6-sol 一审高危:未证实条目混进 1.14 会污染所有模型的请求)。
- `gw-app`:`apply_cursor_extra_models()` 接进 worker 启动初载与 30s 设置环
  (cache_sim 同款先例);非 cursor worker 是无害 no-op。空名条目读侧丢弃。
- admin-ui 无需改动:设置页按字段构造 patch,不会冲掉这个键。

### 验证

- `cargo test --workspace`:14 套件 1315 通过 0 失败(新增:
  热追加覆盖/追加/probe 标志保留/清空恢复;frame0 编码层探测项不进 1.14;
  测试间并发用 CATALOG_TEST_LOCK 串行)。
- 线上热配验证:DB 写探测项 → 30s 内 `/v1/models` 可见 → 清空即消失。

## [cursor-on-demand] - 2026-08-12

Cursor 账号的**超额(on-demand / usage-based)额度**在面板里可读可改:账号表新增
「超额(已用/上限)」列,点击即开设置弹窗(预设档 / 自定义金额 / 关闭)。

### 背景

`ultra-test` 在 08-11 事故里被 2 RPM 卡住,同期 `test` 号是另一种死法:**$20 账期
烧穿**后上游直接拒。套餐额度面板早就看得见,但「超额开没开、上限多少、已经烧了
多少」只能去 cursor.com 网页翻 —— 号一多就等于没有监控。这一版把它搬进面板。

### 上游协议(解包客户端 + 真号实测得到,非文档)

`aiserver.v1.DashboardService`(与已有 `GetCurrentPeriodUsage` 同一个 service):

- `GetHardLimit` → `{hard_limit, no_usage_based_allowed}`。**proto3 零值不序列化**,
  所以 `no_usage_based_allowed` 缺省 = false = **已开启**(不是"未知")。
- `SetHardLimit`:开启 `{hardLimit:N, noUsageBasedAllowed:false,
  preserveHardLimitPerUser:true}`;关闭 `{noUsageBasedAllowed:true, ...}`。
- **两个接口单位不一致**,是这一版最容易踩的坑:`hard_limit` 是**美元整数**,
  `GetCurrentPeriodUsage.spendLimitUsage` 是**美分**。内部统一归一成美元。
- `hardLimit = 2147483647`(i32::MAX)是**不限额哨兵值**,不是真上限 —— 照原样显示
  会变成 "$2147483647",故单独归一成 `unlimited`。

### 改动

- `gw-core`:`OnDemandQuota{enabled, limit, used, unlimited}`,挂到 `AccountQuota.on_demand`;
  Provider trait 加 `on_demand_supported()`(默认 false)/ `set_on_demand_limit()`
  (默认 Unsupported)—— 别的 provider 零改动。
- `gw-cursor`:三端点调用抽出共用 `dashboard_call()`;`account_quota` 合并
  `GetHardLimit`。**这一跳失败只 debug 日志、不让整个配额查询失败**:套餐额度是账号页
  主信息,不该被一个附加字段拖垮(超额栏退化成"—")。已用金额只有用量接口有,
  合并时从上一步结果接回,否则会被覆盖成 0。
- `gw-app`:`POST /accounts/{id}/on-demand`(worker + admin 顺序扇出)。
  `limit_usd` 校验 `0..=i32::MAX`,**超范围直接拒不静默截断**(会把"设 $50"变成别的数);
  `0`/`null` = 关闭。走 `quota_sem`(控制面对上游并发只此一处),成功后回读并刷配额缓存
  (否则运维要等一个 TTL 才看到新值,会以为没生效)。
- **写失败不计失败池**:一次计费设置被拒与 chat 可用性无关,不该让号进冷却。
  上游拒绝原文(如 `Payment method required`)从 Connect `details[].debug` 抽出透传到面板。
- admin-ui:超额列**仅 cursor 视图出现**('all' 混着多种 provider,给非 cursor 行挂一列
  「—」像是查不到而非不支持);已用达上限 80% 标黄;未开启显示「关」、离线/未采集显示
  「—」(不显示假的 $0)。中英文案齐全。

### 已知限制

- 超额上限**只支持整数美元**,因为上游字段就是 i32。
- 未绑支付方式的号开不了:上游回 400 `failed_precondition` /
  `onDemandPaymentMethodRequired`,**客户端同样开不了**,不是本功能的 bug ——
  需先去 cursor.com/dashboard 绑卡。线上 `ultra2` 正处于此状态。

### 验证

- workspace 1312 测试全绿;gw-cursor 新增 9 条(含实测响应做金标准样例:
  `{"hardLimit":75}` + `individualLimit:7500` 美分,正好覆盖单位不一致这个坑)。
- admin-ui tsc + build 通过,60 测试全绿(新增 7 条覆盖 off/unlimited/缺省上限/
  80% 阈值/美元格式)。
- 真号实测:`ultra-test` 设 `hardLimit=75` 成功并回读一致。

## [group-warmup-and-rpm-wait] - 2026-08-11

两件事同源于 2026-08-11 CUR 组 503 事故:唯一合格的 cursor 号 `ultra-test`
被 **2 RPM** 卡住时,客户请求一秒不等直接 503,日志还报「组内所有账号均已禁用」。

### 根因(与最初怀疑的「亲和钉死」不符,代码走读后订正)

1. 那 2 RPM 不是账号配置,是**全局新号暖机**(默认开,rank ≥ 100、适应期 2h@2rpm)
   按 rank 一刀切,cursor 订阅号也被限 —— 暖机本是为 kiro 补货新号设计的
   (2026-08-10 ha7477062)。
2. 「等 RPM 窗口腾名额」的分支绑死在排队模式预算(`queue_wait_ms`,默认 0)上,
   没开排队的组直接跌落 `AllDisabled`(文案谎称"全禁用",客户 503)。

### 改动

- **RPM 闸等待与排队解耦**:新预算 `scheduler.rpm_wait_ms`(默认 10000,热调,
  钳到 ≤ RPM 窗口 60s),与 queue_wait 取大;等待不消耗换号预算、分片 ≤200ms
  重选、等待者上限 64(RAII 名额,超出快速失败,防突发堆积+惊群抢锁)。
  预算耗尽仍等不到 → 报新变体 **AllRpmLimited**(503 与客户端文案不变,
  但日志不再谎称「全禁用」或「并发满」)。
- 混合「A 并发满 + B 被 RPM 闸住」时,busy 等待分支不再烧 attempts 抢跑,
  让给 RPM 分支(不耗预算、分片重选,期间 permit 释放照样命中)。
- **按分组暖机策略** `scheduler.warmup_group_policies`(热调,设置页可视化编辑):
  `{组名: {rpm, hours}}`。命中的组完全接管(单期,到期毕业;`hours=0` = 该组
  显式关闭暖机,全局开关也压不住);未列出的组走全局两期,kiro 补货保护不变。
  GroupView 带上分组名,暖机口径跟随请求的成员视图。
- 设置写侧宽松化:overlay 里残留「更新镜像写入、本版本不认识」的键不再锁死
  普通写入(只拒**本次 patch** 里的未知键;显式 `null` 可清残留)——回滚
  旧 router 后设置面板仍可用。
- 设置页新增「新号暖机」卡片(全局开关 + 两期参数 + 分组策略编辑器)与
  调度卡的 rpm_wait_ms 输入;此前暖机只有 yaml/API、面板不可见。

### 即时处置(运维,非代码)

- `ultra-test` 优先级 100→0(高优豁免暖机),CUR 组容量当场恢复;
  `test`($20 账期烧穿,fable 被限)手动禁用。

### 验证

- gw-app 447 全绿(+4:策略接管/豁免/更严、RPM 等待、AllBusy 分类),
  workspace 全套件零失败;admin-ui tsc + 53 测试全绿。

## [kiro-legacy-wire-switch] - 2026-08-11

`KIRO_LEGACY_WIRE=1`:Kiro 上游报文**整体**退回 2026-07-28(`58b6f27` 对齐 1.0.212)
之前的线缆形态。默认关 = 现状一字节不变。gw-kiro 相关测试全绿。

### 背景

08 上旬 PRO+ 付费号批量 TEMPORARILY_SUSPENDED(实测 0/35 申诉恢复 = 永封)。
周末调查(`.ban-investigation.md`,E1–E8)未锁定根因,但"07-28 线缆形态变更引入
新指纹"在可疑内因里排位靠前:旧形态(UA 0.12.155 时代)跑了几个月无大规模封号。
本开关是最低成本的可逆判别实验 —— 不用 git revert,这两周的新功能
(ListAvailableModels/模型目录/RPM 闸门/conversationId 加盐等)全部保留。

### 总开关协调的维度(只支持两种自洽形态,杜绝混搭)

| 维度 | 默认(1.0.212) | legacy(0.12.155 时代) |
|---|---|---|
| UA 自报版本 | 1.0.212 | 0.12.155 |
| body 顶层 `agentMode` | 必发 `"vibe"` | 不发 |
| `additionalModelRequestFields` | 按策略发 | 不发(改走旧文本标签) |
| 当前消息空 `userInputMessageContext` | 省略 | 补发 `{}` |
| 思考强度载体 | 结构化字段 | `<thinking_mode>/<thinking_effort>` 标签 |
| 配额/profiles 控制面域名 | management.*.kiro.dev | q.*.amazonaws.com |

### 实现要点

- 新增 `gw-kiro::wire_profile`:总开关与形态说明的单一出处。形态切换全部落在
  **构造点与 serde 谓词**上(chat.rs 置 None、conversation.rs 谓词读开关),
  不做序列化后改写 —— struct 字段顺序就是线缆字节顺序(与真客户端 key 顺序
  逐字对齐过),二次 Value 往返会排成字母序,反而偏离金标准。
- legacy 下旧文本标签词表同步回旧:`max` 写作 `xhigh`(max 是 1.0.212 才进
  enum 的档,旧线缆从未出现)。该翻译**只**随总开关生效;单独开
  `KIRO_LEGACY_THINKING_TAGS` 的混搭路径保持既有字节(max 原样)。
- `KIRO_LEGACY_THINKING_TAGS` / `KIRO_LEGACY_QUOTA_ENDPOINT` 两个细粒度开关
  保留原语义(只翻单一维度,作应急);总开关与它们是 OR 关系。

### 刻意的例外(功能保留,不回退)

- `ListAvailableModels` 继续走 management 域(旧域无此操作;admin 手动触发,
  不在逐请求热路径)。
- conversationId 按账号加盐(08-07 防关联修复)不回退。
- 思考预算下限 8192、默认档位 high 是策略(值),不是协议形态,保留。

### 部署注意

- 开关只需加在 **worker**(报文在 worker 侧生成);router 不碰上游。
- 生产 worker 开着 `KIRO_THINKING_IN_HISTORY0=1`:若当前 env 没有
  `KIRO_LEGACY_THINKING_TAGS=1`(即现在不发标签),开总开关会让标签重新注入
  history[0] = 缓存前缀第一块字节变化,在途会话下一轮全量 miss 一次。低峰切换。
  若当前已设 `KIRO_LEGACY_THINKING_TAGS=1`,则前缀无变化,随时可切。

## [cursor-quota-and-adaptive-thinking] - 2026-08-10

两件事:① Claude Code `thinking.type=adaptive` 导致无首字挂满 5 分钟;
② Cursor 账号表接官方账期用量(`GetCurrentPeriodUsage`)。

### adaptive 无首字 / upstream stalled 300s

生产请求 `thinking: {type:"adaptive"}` 时,收侧只认 `enabled` → 不透传 `1.4`,
但思考帧仍刷新 `last_progress`,90s 心跳 watchdog 失效;客户端等到 gw-app
`STREAM_IDLE_ABORT`(300s)才报 `upstream stalled: no event for 300s`。

- `client_wants_thinking` 认 `enabled` **与** `adaptive`
- 未透传的思考不再刷新 `last_progress`
- `apply_thinking_pref` 对 `adaptive` 显式开上游 `thinking=true`

### 官方额度表(方案 A)

- `gw-cursor::usage`:只读 `DashboardService/GetCurrentPeriodUsage`(美分→美元)
- `CursorProvider::account_quota` 走账号专属出口(与 refresh/chat 同 IP)
- admin-ui:`quotaKindForProvider('cursor')` → `credits` 列展示剩余/上限美元

## [cursor-tool-use-usage-fallback] - 2026-08-10

cursor 通道 `tool_use` 收口时输入/缓存为 0 的计费空洞。gw-cursor 相关测试覆盖。

### 背景

生产 `claude-fable-5` 近两小时成功请求里约 39/40 条 `input_tokens=0`、
`cache_read=0`,只有 output —— new-api 面板显示「0 / N」。根因:外部工具轮
反代一问一答会在 tool_use 处 `break`,上游通常不给 `1.14` 用量帧;旧回退只估
`output = chars/4`,输入与缓存恒为 0。有 `1.14` 的 end_turn 轮不受影响。

### 修复

- 无上游用量时按请求体(system + turns + tools)chars/4 粗估 input;多轮把非本轮
  前缀估成 `cache_read` / `real_cache_read`。
- 工具/内建收口前若同帧已带 `1.14`,先收下再 break(合帧不丢)。
- 上游自报用量同步写入 `real_cache_read_tokens`(cursor 无模拟/真实双轨)。

## [cursor-model-catalog-33] - 2026-08-10

gw-cursor 模型目录 8 → 33 项。gw-cursor **120 项测试通过**。

### 背景

旧目录是 2026-08-07 抓包逆向时的客户端菜单快照(Run 1.14 字段,仅 8 项),
`claude-opus-4-8` / `claude-haiku-4-5` 这类名字会被按族归并到 5 系/composer,
面板 `/v1/models` 显得"模型很少"。

### 来源与口径

新目录取自真机 Cursor 客户端 `state.vscdb` 内服务器下发的
`availableDefaultModels2`(33 项,含每模型 parameterDefinitions)——比抓包
快照新且权威。参数默认值按菜单声明范围选取(claude 系 thinking=true/effort=high、
gpt 系 reasoning=medium、gemini-3.6-flash effort=medium 等)。目录已有的精确名
原样透传(不再归并),目录外名字仍按族归一。菜单=上游提供,能否实调取决于
账号计费状态(pro 号第三方前沿模型超量会降级 grok-4.5),这是既有上游行为。

## [low-priority-warmup] - 2026-08-10

低优先新号暖机(动态有效 RPM)。全量 **1230 项测试通过**(+10)。

### 背景:补货新号上线即被鲸客流量灌死

2026-08-10 `ha7477062` 封号排查:restock 补货的新号一上线就全速进轮转,
被鲸客的瞬时流量几分钟打到 `TEMPORARILY_SUSPENDED`。上游对新号的节奏容忍
远低于老号,而既有 `rpm_limit` 只能逐号手配 —— 补货号落地即高优流量的
泄洪口,等不到人去配。

### 方案(gpt-5.6-sol 评审修订定稿)

低优先号(调度 rank >= 100)按号龄(`accounts.created_at`,对补货号≈上游
激活时间)获得更低的**有效 RPM 上限**;高优号(rank < 100,成员边 @0 的
POWER/PRO MAX 等)**完全不受影响**:

- 适应期:号龄 0 ~ 2h,有效 RPM = 2
- 爬坡期:号龄 2 ~ 24h,有效 RPM = 6(期边界左闭右开:恰好 2h 进爬坡,恰好 24h 毕业)
- 毕业:> 24h,恢复账号自身配置
- 有效 RPM = **min(账号 `extra.rpm_limit`, 暖机上限)** —— 暖机只收紧,
  绝不放宽账号已有的更严上限。

### 实现要点与取舍

- **单点抽象防口径分叉**:`CredentialState::effective_rpm_limit(rank, warmup, now)`
  是所有 RPM 判定/记账的唯一入口,`eligible_ids` / `tier_hold_wait` /
  `queue_tier_hold_wait` / `select_id` 原子预留+退还 / `queue_probe` /
  `queue_stats` / 状态快照全部走它;`rpm_*` 方法族改为显式接收算好的上限,
  编译器保证没有调用点能绕过。
- **rank 口径**:有分组视图的调用点用成员边 rank(与选号分层同源,同号在不同
  组视图下跟随该次请求的层);无视图的点(`queue_stats` / `status_snapshot` /
  `note_upstream_call`)用 `default_rank` 兜底 —— 只影响观测/记账,60s 滑窗自愈,
  真正的准入判定(生产每请求都带视图)不受影响。取舍写在
  `effective_rpm_limit` 注释里。
- **`Account.created_at`**(serde default 0):0 = 未知号龄(accounts.yaml 降级
  加载/手工构造)→ 按已毕业处理,**fail-open 到旧行为**,绝不因元数据缺失把
  老号当新号限流;时钟回拨按号龄 0(吃最严档)。`load_owned_accounts` 直通
  DB 列;token 刷新走 clone 回写,created_at 天然存活。各 crate 测试桩补
  `created_at: 0`,既有调度测试零改动。
- **不改并发、不改 tiered_lru 排序、不改会话亲和**:RPM 达限后的等待/迁移/503
  行为完全沿用既有定频语义(软状态,不进禁用统计,不触发 AllDisabled)。
- **配置热更**:`scheduler.warmup_*`(enabled 默认 true,phase1 2h/2,phase2 24h/6)
  走 settings overlay(`SystemSettings` 同名字段),30s 内生效;RPM 上限 clamp ≥1
  (0 = "一次都不许"的笔误会静默停掉所有新号,想停有总开关)。
- **可观测**:`AccountStatusSnapshot` 加 `warmup_phase`(null=不暖机/高优,
  0=适应期,1=爬坡期),`rpm_limit` 字段改报**有效**上限(含暖机 min)——
  经 /health → router 聚合 serde 直通,前端本期不改。

### 测试(+10)

- `warmup_phase_cap_period_boundaries`:三期边界(恰好 2h/24h 左闭右开)。
- `warmup_phase_cap_exemptions_and_edge_cases`:高优豁免、未知号龄 fail-open、
  时钟回拨吃最严档、开关关闭。
- `warmup_never_relaxes_stricter_configured_rpm`:min 语义(配置 10→2、配置 1→1、
  老号 10→10)。
- `warmup_caps_new_low_priority_account_old_account_takes_over`:打满退出合格集、
  老号承接、快照三字段。
- `warmup_phase2_allows_six_per_minute`:爬坡期 6 格,第 7 次溢出。
- `warmup_exempts_high_priority_new_account`:高优新号连打 8 次不限。
- `warmup_disabled_means_no_cap_for_new_accounts`:开关关闭 = 旧行为。
- `warmup_tuning_hot_update_takes_effect`:update_tuning 热关立即不限。
- `settings_warmup_apply_to_overrides_and_preserves`(gw-core):overlay 覆盖/保留语义。
- `load_owned_accounts_carries_created_at_for_warmup`(gw-store):号龄列直通。

### 审查修复(gpt-5.6-sol 暖机复审,1 高 2 中;+3 测试)

- **[高] 同一 lease 的追加调用能突破暖机硬上限**:`note_upstream_call` 从「只记不拦」
  改为**原子准入**——同一把 entries 锁内检查有效上限(含暖机),达限返回 `false`
  且不多记,调用方不得硬发。各调用点语义:token 刷新重试被拦 → 不上报失败
  (rt 刚证有效,按 TokenInvalid 报会永久误禁)、直接换号;profileArn 修复重试被拦 →
  按「无法修复」落入统一失败处理;Overloaded 退避被拦 → 按退避用尽返回最近一次
  过载错误(对外 529,客户端自动重试);web search 续轮被拦 → 优雅降级收尾
  (已得内容照常返回,保 usage,`OnUpstreamCall` 改 `Fn() -> bool`);人工探针被拦 →
  如实回 `stage: "rpm_limited"`。
- **[中] 追加调用的 rank 口径与准入同视图**:`note_upstream_call` 加 `view` 参数,
  生产链路(刷新重试/修复重试/过载退避/web search 续轮)全部传请求的成员视图 ——
  「成员边低优但 extra.priority 高优」的号,追加调用不再被 default_rank 错误豁免、
  不再漏记真实调用。仅人工探针(无分组上下文)传 None 按 default_rank。
- **[中] RPM 退还竞态**:预留改发**句柄**(entry 内递增序号,deque 存
  `(Instant, u64)`),`select_id` 随选中 id 返回句柄,`rpm_refund` 按句柄精确删除 —
  不再无脑 `pop_back`(并发下尾部可能是别人的真实调用,退错 = 替别人的超频开窗)。
  退还也随之不再需要回传视图重算口径。

### 复审修复(gpt-5.6-sol 第三轮,1 高 1 中 1 低;+1 测试)

- **[高] profileArn 修复验证被拦会误杀刚修好的号**:验证调用被 RPM 闸门拦住时,
  上一版把修复前的 e2 透给统一失败处理 → 按 TokenInvalid 上报 → 永久误禁
  `invalid_refresh_token`。暖机一期 RPM=2 下这是**确定性路径**(首发 1 格 →
  刷新重试第 2 格遇旧 ARN 403 → 新 ARN 发现成功 → 验证调用达限)。修为照
  token 刷新分支语义:被拦 ≠ 修复失败,不上报、释放 lease 换号,预算/时限
  用尽才透原始错误。
- **[中] 过载退避的记账时机**:准入(检查+记账)从退避睡眠**之前**挪到睡眠
  完成后、调用发出前 —— 任务在睡眠期被取消不再留下从未发送的假 hit,
  滑动窗口也不会按过早的预留时刻提前放出名额。
- **[低] 补 worker 调用点分支测试**(`heal_retry_blocked_by_rpm_does_not_report_or_disable_account`):
  mock provider(chat 恒 403、refresh 成功、强制发现 ARN 成功)直调 messages
  handler,断言验证调用未发出(chat 计数 = 2)、账号未禁用、无 TokenInvalid
  上报、被拦调用未记账。这是本仓库第一个 handler 级 mock provider 测试。

## [account-models-local] - 2026-08-10

「账号可用模型列表」收尾 + 修一个线上确诊的扇出超时 bug。全量 **1220 项测试通过**(+5),
admin-ui **53 项**(+4),`bun run build`(tsc strict)绿。

### 背景:面板看不到「这个号还能用哪些模型」

model-marks 落地后,状态徽章能告诉你「这号被逐模型拒过 N 个」,但回答不了
「那它到底还能用哪些」——要看档位静态支持表 × 已学标记的乘积,只能翻代码心算。
本次把它做成一个端点 + 一个弹窗。

### 账号可用模型清单(纯本地,零上游)

- **worker**:`GET /accounts/{id}/models/local` 已先行落地(见 worker/mod.rs
  `models_local`)——静态目录 × `account_supports_model` 档位支持 − 已学
  INVALID_MODEL_ID 标记,纯内存读,毫秒级;目录外的标记(上游新模型/变体名)
  单独透出不丢掉。「可用」口径是**本地认知**:未观察到拒绝 ≠ 上游保证,
  上游真相仍靠「拉目录 / 探针」。
- **admin 扇出**:`GET /admin/api/accounts/{id}/models/local`,与 quota/models
  同款顺序扇出(2xx 透出,404 问下一个,其余记首个错误;全不持有 → 404
  「没有 worker 持有该账号」)。
- **面板**:kiro 行操作区新增「查看模型」图标按钮(其它 provider 无此概念不显示),
  弹窗三态展示:可用 / 被标记(附剩余重探时间 `formatMarkTtl`)/ 档位不支持,
  外加目录外标记区;三态推导 `deriveModelAvailability` 是纯函数(标记剩余 0s
  仍算标记中,与后端 `is_none()` 对齐),配 Vitest。

### 修复:quota/models 扇出 2s 超时误报「没有 worker 持有该账号」

线上确诊:`models_account`/`quota_account` 扇出用 `st.http`(2s 管理面聚合 client),
而 worker 侧要真打上游控制面(拉目录/查配额),>2s 是常态 —— 传输超时在扇出循环里
等价于「该 worker 离线」,全部轮完误报 404,直连同一 worker 却是 200。probe 端点
注释里早有同款踩坑记录(用 120s)。修法:三个扇出共用 `control_plane_http()`
(30s;控制面 GET 正常秒级,30s 兜住抖动又不挂死管理页),`st.http` 的其它用途
(聚合 /health 等需要快速跳过离线 worker 的场景)不动。回归测试用 3s 慢应答
worker 锁死:旧 client 必现的误判,现在拿到 200。

### 审查修复(gpt-5.6-sol 二审,2 中 3 低)

- **[中] models/local 扇出误用 30s client**:纯本地端点毫秒级返回,串行扇出时一个
  挂住的 worker 会把面板请求拖 30s。改回 2s 的 `st.http`(与其它纯本地管理面调用
  一致),30s 只留给真打上游的 quota/models;两处注释同步改正。
- **[中] 弹窗数据新鲜度**:全局默认 staleTime 是 30s(此前注释误写 0),重开弹窗
  可能展示旧缓存。`useAccountModelsLocal` 加 `staleTime: 0` + `refetchOnMount:
  'always'`(每次打开必重新取快照);i18n 剩余时间表述从「倒计时」口径改为
  「查询时快照」口径(zh/en)。
- **[低] 慢应答回归测试挪位**:models/local 改回 2s 后,3s 慢应答测试打在它上面
  必挂;移到真正出过事的 quota 扇出(3s 慢应答假 worker 打 `POST /accounts/{id}/quota`,
  断言 200 而非 404)。
- **[低] 弹窗标题名不副实**(列表含不支持/被标记条目):zh 改「模型支持情况」、
  en 改 "Model availability",集合公式文案改为「静态目录 + 档位支持判断 + 已学标记」。
- **[低] 前端类型过宽**:`AccountModelEntry` 的 `display_name`/`mark_remaining_secs`
  由可选改为必填可空(`string | null` / `number | null`),与 worker 实际输出对齐;
  测试桩同步补齐。

## [model-marks-observability] - 2026-08-10

封号潮排障实录(2026-08-10):唯一活号 `kiro-apikey-d8ac5a33253d` 面板显示「正常」
(未禁用/无冷却/额度 9988/10000),实际服务全 503——它对上游的每个模型都被
`INVALID_MODEL_ID` 拒绝(上游"半死":配额接口还读得出、模型调用全拒),caio 按设计
打了 `(账号,模型)` 不可用标记并绕开,但标记是纯内存、面板和 runtime API 完全看不到,
排查只能翻日志。本次把这个观测盲区补上。全量 **1214 项测试通过**(+2),
admin-ui **38 项**(+2)。

- **runtime API 暴露标记**:`AccountStatusSnapshot` 新增 `model_unavailable`
  数组(模型名 + 标记剩余重探秒数),经 worker `/health` → router 聚合 → 面板,
  全链路 serde 直通,旧 worker 缺字段时前端按"无标记"降级。
- **面板**:状态徽章旁新增橙色「模型不可用×N」徽章,title 逐模型列出剩余重探时间
  (`formatMarkTtl`:<1h 显分钟,否则显小时)。
- **救号按钮覆盖该状态**:挂着模型标记的号也会出现救号按钮,且 worker 侧
  `reset_account` 现在**一并清除该号的模型不可用标记**——否则 reset 返回成功、
  号仍被标记挡在选号池外直到 6h TTL 自然过期(与此前清 RPM 窗口同款理由,
  锁顺序 entries → model_unavailable,与选号谓词一致)。

### 审查修复(gpt-5.6-sol 一审)

- **[高] 剩余重探秒数方向写反**:`t.saturating_duration_since(now)`(标记时刻 − 现在)
  恒饱和为 0,导致过期标记滤不掉、剩余时间恒为 TTL。修为 `now - t`,并补
  「剩余递减 + 过期消失」的针对性测试(旧断言只查"在范围内",抓不住方向错误)。
- **[中] 自动退役被配置停用遮蔽**:退役落库 `disabled=1`,而 `deriveAccountStatus`
  先判 `row.disabled`,红「已退役」标签成死代码。修为 runtime 在线且报
  `suspended_retired` 时优先展示退役,并补优先级/分桶测试。

## [suspend-lifecycle] - 2026-08-10

封号调查(证据见 HANDOFF-2026-08-10-ban-investigation.md)的落地修复,
按 gpt-5.6-sol 方案审查的三轮意见做全。全量 **1212 项测试通过**(+15)。

### 背景:死号每小时整批复活,把客户原文泼向尸体

生产实锤:一个真实客户请求 32 秒内被泼到 12 个账号(11 个复活中的死企业号 +
1 个活 PRO+)。机制链:suspend 固定 3600s 冷却 → 到期整批复活 → 客户端重试的
新请求挨个踩中刚复活的尸体。同一段客户原文反复跨身份重放,是喂给上游风控的
池关联证据。同时 ~100 个假冷却号让 restock 的 healthy 计数频繁触底
(`min_healthy=1`),3 小时白买 6 个号(¥30/个)。

### suspend 生命周期状态机(仅 kiro 启用)

- **指数退避 + 抖动**:第 k 次连续 suspend 冷却 = `suspended_cooldown_secs * 2^(k-1)`,
  封顶 `suspended_backoff_cap_secs`(默认 24h),±20% 确定性抖动(FNV(账号,档位),
  不引 rand 依赖)——「整批死号同一秒复活」的同步性被打散。
- **复活观察期(probation 单飞)**:冷却到期不再满血回轮转,并发限 1,直到一次
  **完整成功**(非流式折叠成功 / 流式走到 `message_stop`)才满血;再失败吃下一档
  退避。观察期占用的号在取号分类里按"忙"认领,不掉进 AllDisabled 误报 503。
- **世代闸门(suspend_gen)**:lease 快照选号时的世代;旧世代在途请求的迟到
  成功/失败一律不改写新状态——同批并发里第一个 suspend 计击后,其余回声与
  迟到成功都不再干扰退避(审查阻断 #1/#2)。
- **自动退役**:连续 `suspend_retire_strikes`(默认 3,0=关闭,可热调)次复活失败
  → 持久禁用 + `disabled=1` 落库,面板「已退役」,restock 直接判死(可回收),
  不再每小时复活闪烁。
- **状态落库**(新表 `account_lifecycle`):退避档位/retry_at/退役标记全部持久化,
  每次状态转换即入队、worker sync 循环落库;重启按库水合——不重抽抖动、
  不让"已退到 24h 档"的号因发版提前复活(审查阻断 #5)。
- **人工恢复原子化**:面板「启用」= `disabled=0` + 删生命周期行,单事务
  (`restore_account`);worker sync 的配置翻转路径同步清运行态 + 前进世代 +
  入队清零行兜底(审查阻断 #4)。
- **provider 作用域**:只对 kiro 启用(「0/35 不可恢复」的实测只覆盖 Kiro 号);
  dario 等 provider 经 `suspend_policy_scoped` 闸保持旧的平冷却、不退役,
  构造与 30s 热更两条路径同一口径(审查阻断 #6)。

### 排队号不再因空响应下线

空响应达阈值冷却保留给非排队号;排队号(企业共享池)的空响应多是跨租户拥挤的
上游抖动,下线只会缩小健康池、让 restock 把假冷却当缺口一直买号(与 429 同哲学:
唯一 hold 下线信号是 403 suspend)。真毒报文由 poison_memo 多账号确认机制兜底
(EmptyResponse 本来就不换号,不扩大喷洒)。

### 面板

新状态标签:封禁冷却(带剩余秒)、已退役(红)、复活观察期(橙)。
顺带修了「运行态封禁冷却被显示成已停用」的歧义(原 deriveAccountStatus 把
`temporarily_suspended` 归并进"已停用")。

### 配置

`system.yaml` / 设置面板新增:`suspended_backoff_cap_secs`(默认 86400)、
`suspend_retire_strikes`(默认 3,0=不退役恢复旧行为)。均支持热更。

## [cursor-review-fixes-2] - 2026-08-07

把审查里剩下的 12 条做掉。全量 **1148 项测试通过**。

### ⭐ 所有 provider 的错误体都缺 `error.type`(不只 cursor)

用户实测撞到:opencode 报 `Type validation failed … path:["error","type"] … expected string,
received undefined`,**真正的 message 被吞掉**。根因是除 `Overloaded` 外,
`upstream_error_response` / `sse_error_payload` 都只发 `{"type":"error","error":{"message":…}}`,
而 Anthropic 协议里 `error.type` 是**必填**;按 schema 严格校验的客户端连错误都解析不了。
**这条影响生产的 kiro / dario,不只 cursor。**

新增 `error_type_for_status`:type 跟着状态码走(400→`invalid_request_error`、
401/403/404/413/429→各自的、503/529→`overloaded_error`、其余 5xx→`api_error`),
两条对外路径共用。`sanitize_upstream_error_payload` 在上游没给 type 时也补 `api_error`。

⚠️ **两条既有测试断言的正是这个 bug**(「非过载错误不带 error.type」「不该凭空造 type」)。
原来的顾虑「造错 type 会误导客户端重试」合理,但结论反了 —— 根本解析不了比 type 不精确糟得多。
已改断言并在测试里写清为什么。选 `api_error` 是保守解:不指认客户的 key/请求有问题。

### 同一条原则只执行了一半:工具帧吞掉同帧正文

`is_tool_call` 的判断原先排在正文发射**之前**,而 usage 帧那里特意写了「用量必须最后判」
的防御。protobuf 层不阻止上游把最后一个正文增量和工具调用并进同一帧,所以工具帧那条路上
那一帧的正文会被静默丢掉,客户收到一个看起来正常的 `tool_use`。已把工具判断移到发射之后。

另外:内建工具收口发生在**已出字之后**时,仍按 `end_turn` 收尾(首包已 committed,报错也
换不了号),但这**确实是截断** —— 加了 `builtin_truncated` 标记 + warn,否则它在日志里
和一次正常收尾完全一样。

### 其余

- **空 `tool_use.id` 不再放行**:合成 `call_<uuid>` + warn(工具名解出来了说明模型真想调,
  丢掉比 id 难看更糟;我方从不把 call id 发回上游,合成是安全的)。
- **`expires_at` 回写**:不写它,gw-app 的 `has_fresh_token` 会把号当「永鲜」→ 从不主动刷 →
  每个过期号先吃一次 403,而 403 的分类本身就是雷区。
- **403 不再一律判 `TokenInvalid`**:只有 401 和**结构化** Connect 错误才动账号健康;
  裸 403(ALB/CF 拦 IP 的 HTML 错误页)判 `Other` —— 坏的是 IP 不是号。
- **`config_fetch_gate` 改按账号单飞** + 失败负缓存(60s)+ 缓存键带**身份指纹**。
  全局一把锁会把 N 个各有独立代理的冷号串成 N×6 秒,任一个卡住整池排队;
  失败不缓存则 api2 故障期每个请求各付一次完整超时。指纹进键 = 换凭据自动换条目。
- **`send()` 等响应头加 60s 超时**:配代理的号用的 client 故意不设总超时(流式),
  代价是「上游收了连接但永不回话」时永久占住并发租约,而 `STALL_TIMEOUT` 在流循环里进不去。
- **SSE `message_delta.usage` 补齐 `input_tokens` / `cache_read_input_tokens`**:
  此前客户在 SSE 里看到输入恒 0 却按非零输入被计费,拿响应对不上账单。
- **`ConvRegistry`**:`stateful` 改构造期读一次(不再每请求查环境变量);关闭时**完全不写表**
  (原先每次成功都插入 + 全表 retain,纯浪费还把所有流的收尾串在一把锁上);
  锁改毒化即恢复(原先 unwrap,毒化后每个请求都 panic = provider 整体下线)。
- **`config.rs` 时区透传**:原先硬编码 `Asia/Shanghai`,账号配别的时区就造出一个
  **内部自相矛盾**的指纹(unary 报上海、推理报美西),比配错更可疑。
  `client-type: ide` 保留(与推理的 `glass` 不同是抓包实物),但注释写清了。
- **traceparent 换全熵随机**:UUIDv4 的第 13/17 位被版本/变体位钉死,那是一个
  **每请求广播两次**的稳定特征。加 `getrandom` 依赖,并加测试抽 200 个 id 检查位分布。
- **落盘开关启动告警**:`CURSOR_DUMP_REQ` / `CURSOR_DUMP_TOOL_FRAMES` 会把含用户对话全文的
  帧明文写盘且不清理;构造时 warn 一条,免得排查完忘了摘。
- **思考被丢弃时留痕**:正文块已开时后续思考只能丢(Anthropic 不允许 thinking 排在 text 后),
  但原先是静默的 —— 现在计数 + debug 日志。
- **`validate_account` 对 cursor 生效**:此前 gw-app 只对 `claude-dario` 调它,
  cursor 的加载期校验是死代码。代理配错的号现在挡在池外 + 启动告警。
- **模型归一化留痕**:实际发上游的模型名与客户请求的不同时记一条 info
  (SSE 回显仍是客户请求的名字 —— 协议要求如此,客户端按它对账)。
- **缺价目不再静默算 0**:`price_for` 只认 opus/sonnet/haiku,而 cursor 广播 8 个 id。
  **不猜价**(定价是业务决策),但有 token 却算不出成本时 warn 一条。

### ⭐ 实测顺带确认两件事

**① Cursor 的 `1.14.1`(input)确实包含 `1.14.3`(cache_read)。**
实测:`input_tokens=26483` / `cache_read_input_tokens=26376`,差 107,而我方请求体只有 1429B。
差值量级吻合。这与 caio 全局口径(`input` 是总上下文、`cache_read` 是子集,计价方相减)
**一致**,不存在重复计费。此前审查把这条列为「作者相信为真但没证明」,现已证明。

**② Cursor 会注入约 26k token 的服务端系统提示,每个请求都算进 input。**
一句「27 乘 43」也报 26447 input。好消息是缓存命中率 99.6%,边际成本小;
但它是一个**每请求的固定地板**,且客户按它被计费 —— 客户拿自己的请求长度对不上账单。
sonnet 价目下约 $0.008/请求。**这是定价要考虑的输入,不是 bug。**

### 没能确认的一条

**thinking 透传这一轮没能复现。** 上游连续两次请求 `thinking_len=0`(一个 `1.4` 帧都没发),
我方的丢弃计数是 0 —— 所以**不是我方吞掉的**,是上游没发。上一轮实测它发过
(11–19 字的简短推理摘要)。判别方法:看日志的 `thinking_len=` —— 大于 0 却没到客户端才是我方 bug。


## [cursor-review-fixes] - 2026-08-07

对抗审查(codex Architect + kimi Skeptic,两个独立模型)+ 自审的产出。**结论是 REJECT**:
前三条高危两位审查员各自独立找到,不是风格问题。本条记修掉的部分。

### ⭐ 工具回路发散:两个独立病因

现象:grok 在 opencode 里对同一个文件连调 **9 次** `read`,请求体每轮只涨 1.2KB;
另一次是一串 `→Read … [limit=, offset=]`,参数值全空,模型反复自我纠正「参数改为数值类型」。

**病因一:数值参数解成空。** `1.2.2.15.1.2.2` 那层不是我方猜的包装,是标准
`google.protobuf.Value`(`null=1, number=2/double, string=3, bool=4, struct=5, list=6`)。
抓包实物只出现过 field 3,于是解析被写死成"只读字符串档";模型传 `limit: 200` 时值在
**number 档(fixed64 double)**,而 `protobuf::Reader` 那时把 fixed64 的字节**丢掉了**
(只记"有个 fixed64")。结果 `limit=""` → 工具失败 → 模型换个说法再试 → 永不收敛。
现在 Reader 带出 fixed64/fixed32 原字节,`ToolCall.args` 从 `Vec<(String, String)>` 改成
`Vec<(String, serde_json::Value)>`,数字/布尔/嵌套对象/数组全部原型透传;
认不出的档位解成 `null` 而不是空串(空串会被工具当成"给了个空值"而静默做错事)。

**病因二:折叠把问题弄丢了。** 工具回路第二轮,Anthropic 请求的最后一条 user 消息
**只有 tool_result**,`fold_history` 折完之后本轮用户消息是一段裸的 `[工具返回]…`,
**一个问题都没有** —— 原始请求被埋进 `<conversation_history>`。模型看到「工具返回了但
没人问我什么」,最合理的动作就是再调一次工具。现在识别出这种中间轮,把**用户最近一次
真实提问**复述回本轮并附一句「据此继续、不要重复调用已返回结果的工具」。

顺手:`tool_use.id` 去掉换行 —— 上游 call id 是 `call-<uuid>-N\nfc_<uuid>_N` 两段用换行连的,
原样交给客户端会让 id 校验失败,还把日志劈成两半(排查时看着像日志损坏)。

### ⭐ 一次 api2 超时能把健康号永久禁用(自己上一版改出来的)

链条:`GetServerConfig` 失败 → 上一版加的退化「过期旧值 > 空串」→ 空 `config_version`
→ `config.rs` 自己写明的完整性门回 `resource_exhausted` → `trailer_to_error` 判
`QuotaExhausted` → scheduler 的处置是 `disabled_until = None`,**持久、不自愈、要人工 reset**,
连排队等待都不给。api2 连续抖动就是整池挨个阵亡。

那段注释写的是「取不到不该打挂聊天」—— 意图对,后果比它替换掉的 `?` 坏得多。
现在:取不到就返回**可重试错误**(`Run` 还没发出,不算 committed,gw-app 会换号);
过期旧值也不再兜底(同一条软封路径)。另外把「Update Required」签名从 `QuotaExhausted`
里摘出来判 `ServerError` —— 完整性门与真的额度耗尽**共用同一个 code**,而错判方向是
不对称的:把额度耗尽当完整性门只多试一次,反过来是账号死掉。

### 出口代理构造失败改 fail-closed

原先 warn 一声回退默认出口。理由「配置写错不该让号彻底不可用」是反的:账号配了 `proxy`
就是在声明要独占出口,回退等于**把本该隔离的号并到同一 IP**,而这是已实测的封号维度
(同出口 59.5% vs 独立代理 0%)。gw-dario 早先的对抗审查就同一问题已定案 fail-closed,
同一个安全边界不该因 provider 不同换结论。现在拒绝 + `validate_account` 加载期就报出来。

### 会话亲和键(`affinity_key`)

不覆盖它的后果不是「少一层优化」:worker 是 `affinity_key.unwrap_or_default()` 装
`CallCtx.session_id`/`cache_key`,默认 `None` → 两个都是空串 → `conversation_id` 恒为 `""`。
于是上游 `1.5` 发零长度字符串(真客户端是 UUID)、`x-blob-encryption-key`/`x-fs-client-key`
从「每会话一把」退化成「每账号一个常量」——两条都是稳定的可区分指纹;`ConvRegistry`
所有会话挤在同一个键上,`CURSOR_STATEFUL=1` 下是**跨用户串话**。

现在按首条用户消息 + system 派生稳定键(与 gw-kiro 同思路),并在 provider 内过一遍
`conversation_uuid`:保证非空、保证 UUID 形态、把调度用的分组前缀折进哈希(那是 caio
内部命名空间,不该进上游报文)。

### 四道体积闸

`gunzip` 单帧 64MB、帧声明长度 64MB、单附件 12MB、附件合计 24MB、PDF 抽取总量 4MB +
流数 512。此前 `pdf::inflate` 的 16MB 只是**单流**上限:100 个合法流累计 1.6GB,
而这段文本随后还要进 prompt、protobuf、gzip,峰值是它的数倍,且全在首个 await 之前
同步做完 —— **爆炸半径是整个 worker 进程,连带同进程的 kiro 流量一起死**。

### 测试:改掉「测自己镜像」的那几条

kimi 点名:tool_call 相关用例用与解析器**同一套常量**造帧再解,字段号整套写错也照样绿。
现在把 2026-08-07 抓到的两帧实物(外部工具 / 内建工具)存成
`tests/fixtures/*.bin`,`include_bytes!` 直接解;`google.protobuf.Value` 的嵌套
struct/list 用**手写字节**断言。helper 保留但注释写明它只能证明编码解码互逆。

全量 **1143 项测试通过**。

### 文档订正

`PROTOCOL §12.5` 的嫌疑清单里第 2(不发 `1.25`)、第 3(第二轮仍发首轮形态)**都已经做掉了,
做完仍然挂起** —— 已标为排除,只剩 FileSync 一条。旧清单会让下一个人重做已完成的工作
并误以为 `1.25` 还没验。另注明:此前该实验走 `examples/e2e.rs`(传真实 `session_id`),
而生产路径当时 `conversation_id` 恒空,重跑前先对齐。

### 审查里**尚未**处理的(按分级留档)

`is_tool_call` 在正文判断之前 break(同帧正文被静默丢,而 usage 帧特意防了这一手 ——
同一条原则只执行了一半)、`parse_tool_call` 允许空 `id` 放行、`refresh_auth` 不回写
`expires_at`(`has_fresh_token` 对缺失字段视为永鲜 → 从不主动刷 → 每次先吃 403)、
403 一律判 `TokenInvalid`(出口 IP 被拦时坏的是 IP 却禁了号)、`config_fetch_gate`
是 provider 全局锁而缓存按账号分片(N 个冷号串行,单账号卡住阻塞全池)、
`send()` 等响应头无超时(配代理的号无界,会永久占住并发租约)、
SSE `message_start.usage.input_tokens` 恒 0(客户看到输入 0 却按非零输入计费)、
模型归一化在选号之后且不回写身份(执行一个模型/回显另一个/计费第三套命名空间)、
`ConvRegistry` 三处 `lock().unwrap()` 毒化即 provider 下线、
`config.rs` 硬编码 `Asia/Shanghai` 与 `client-type: ide`(账号配别的时区就造出一个
**内部自相矛盾**的指纹,比配错更可疑)、`pricing::price_for` 只认 opus/sonnet/haiku
而 cursor 广播 8 个 id(其中 6 个算出来成本为 0)。


## [cursor-thinking-and-guard] - 2026-08-07

### thinking 透传

`1.4.1` → Anthropic thinking 块,**只在调用方请求了 thinking 时发**。块索引改成顺序分配
(thinking / text / tool_use 都可能缺席,而 Anthropic 要求索引连续不重复)。

### ⭐ 内建工具护栏:「卡住」的真正原因

Cursor 的内建工具(终端/读写文件/网页搜索)是**服务端自带**的 —— 哪怕我们一个工具都不
声明,模型照样会调。实测:问「帮我查今天的新闻」→ 模型说「先确认今天日期,再查最新新闻」
→ 去调 `date '+%Y-%m-%d'` → 我方只能收口。**用户看到的就是一句没头没尾的计划,像卡死了。**

修法是往系统提示追加运行环境约束(有工具/无工具两版)。同一个问题现在变成一段完整回答
(说明无法联网 + 三条替代做法),内建工具收口 0 次。

### Features

- thinking 块(`thinking_delta`),按 `body.thinking.type == "enabled"` 开关。
- `message_start` 抽成函数:思考先到 / 正文先到 / 纯工具调用三条路径都要发它。
- `builtin_tool_guard()`:系统提示层的内建工具护栏。

### Design Rationale

- **没请求 thinking 就不发 thinking 块。** 客户端要么报未知块类型、要么把推理当正文显示。
- **thinking 必须排在 text 之前,且 text 开过就不再回头开 thinking** —— 否则块顺序倒置。
- **护栏放系统提示而不是别处**:它能改行为是实证过的(PINEAPPLE 实验)。

### Notes & Caveats

- 护栏是**缓解不是根治**:提示层的东西,模型仍可能偶发绕过。根治要么代执行内建工具
  (安全上不可接受 —— 等于跑模型选定的 shell 命令),要么找到关掉服务端内建工具的开关
  (`1.2.1.2` 的 `.32..50` 布尔位是候选,未逐位试过)。
- thinking 块**不带 signature**。Anthropic 原生 thinking 块回传时要签名;
  只做展示不回传的客户端没问题,要把 thinking 塞回历史的客户端可能拒。

## [cursor-media] - 2026-08-07

### 图片与 PDF 打通,两者都是内联的

`PROTOCOL` §5 猜的「附件走 FileSync blob 上传」**是错的** —— 带图/带 PDF 的真包里
一次 `FSSyncFile` 都没有,字节就在请求里。

- **图片** → `1.2.1.1.3`(此前记成「恒为空字符串」的那个字段,其实是附件容器)。
  ⚠️ 里面还有一层 `.1` 包装,少了服务端回 `internal`(结构错的信号)。
- **PDF** → `1.2.1.2.20` = `{1: 路径, 2: 内容}`,而**路径是连接键**:必须同时写进
  用户文本和 ProseMirror 的 `mentionNode`,否则模型不知道有附件。

### ⭐ 但服务端根本不读 PDF 内容 —— 这件事决定了实现

真客户端的流程是:模型**调终端工具跑 `pdftotext "<路径>"`**,由客户端在真实磁盘上
执行再回传。**反代不能走这条路** —— 答应内建终端工具调用等于在我们的服务器上执行
模型选定的任意 shell 命令。

所以 gw-cursor 自己抽文本层(新 `pdf.rs`),以 `<document path="…">` 塞进 prompt。

### Features

- `run::ImageAttachment` / `run::DocAttachment` / `run::Media`,Anthropic 的
  `image` / `document` 块 → 对应字段。
- `run::image_dims`:只读 PNG/JPEG 头取宽高(**不解像素**,所以没有解压炸弹面)。
- `pdf.rs`:FlateDecode + `Tj`/`TJ`/`'`/`"` 串抽取,带 **16MB 解压上限**。
- ProseMirror 的 `mentionNode`,以及路径写进用户文本。

### Design Rationale

- **`source.type != "base64"`(如 `url`)一律跳过。** 替调用方下载 URL 是我方主动出网,
  不该悄悄做。
- **抽不到文本时明确告诉模型「无法抽取,请直接告知用户」。** 不说的话它会反复尝试
  调工具读文件,而我们每次只能收口 —— 用户看到的是反复的半句话。
- **宽高解不出来就填 0,不猜。** 猜错的尺寸会让上游的图像分块算错。
- 空附件容器与图片**不能同时发**:`1.2.1.1.3` 出现两次时读侧取到的是空的那个。

### Notes & Caveats

- PDF 抽取**不支持**:扫描件/图片型(需 OCR)、非 Flate 压缩(LZW/DCT)、
  CID 自定义编码字体(可能乱码)、多栏阅读顺序(会串行)。
- 图片走原始字节内联,大图会显著撑大请求体;尚未接 `gw-kiro/src/image.rs` 那套
  四档压缩 + 解压炸弹护栏(那是跨 crate 抽取的活,见统一转换模块规划)。
- 实测:64×64 红/蓝图 → 「红色、蓝色」;探针 PDF → 哨兵串;0.7MB 真论文 → 标题正确。

## [cursor-tool-use] - 2026-08-07

### 工具调用打通,opencode 的 agent 能用了

此前 agent 类任务会在模型想动手时被截断。现在完整回路:声明 → 调用 → 结果 → 续答。

**工具身份是 protobuf 字段号,不是名字。** Cursor 内建工具是闭集枚举
(`1.2.2.1` 终端 / `1.2.2.4` 读文件,不带名字),装不下任意 Anthropic 工具名。
所以调用方的工具必须声明成 "MCP 工具"(`1.2.1.2.7`,5 个字段全必填,命名空间用
`gwtools`),它们的调用才会回到带名字的 `1.2.2.15`。

### Features

- 请求侧:Anthropic `tools` → `1.2.1.2.7`(`run::ToolDef`)。
- 响应侧:`1.2.2.15` → Anthropic `tool_use` 块 + `stop_reason: "tool_use"`(`run::parse_tool_call`)。
- 回路:`tool_use` / `tool_result` 块渲染成文字进折叠历史,模型据此续答。
- 内建工具调用**认得出但不转发** —— 转发会给调用方一个它没有的工具名。

### Design Rationale

- **不用请求侧 `field 2` 中途回传。** 真 IDE 在同一条流里回工具结果,那要求把流挂着
  等调用方执行完;反代是一问一答的,把结果当成下一轮的上下文更贴合。代价是模型看到的是
  "工具返回了这些文字"而不是结构化 tool_result,实测不影响它续答。
- **call_id 原样带回。** 里面嵌着 `\n`(`call-<uuid>-N\nfc_<uuid>_N`),重新生成就对不上;
  JSON 转义后在线上完全合法。
- **内建工具只收口不转发。** 认出来是为了不陪它等满 watchdog;不转发是因为调用方
  没有 `run_terminal_cmd` 这种工具,给了它只会报未知工具名。

### Notes & Caveats

- 参数值目前只支持**字符串**(上游 `1.2.2.15.1.2.2` 只见过 `3`=string 这一个分支)。
  嵌套对象/数组参数的编码分支未知,复杂 schema 的工具可能失真。
- image 块仍不支持(要先摸清 Cursor 侧的图像字段)。
- 实测:`get_weather` 单工具闭环;opencode agent(`ls` + `Read`)16s 完成。

## [cursor-stateful-turns] - 2026-08-07

### 首轮与后续轮是**两种形态**,混用会让服务端无视内联历史

同一会话连抓两轮真包(98858B vs 2121B,相差 47 倍)后才看清:

| | 首轮 | 后续轮 |
|---|---|---|
| `1.1` 环境/预算块 | **空** | 612B(预算表报账) |
| `1.2.1.2` 内联上下文 | **97895B 全量** | 不发 |
| `1.2.17.9` 环境详情 | 无 | 有 |
| 历史 | — | 服务端按 `1.5` 自持 |

`1.5` 是 conversation_id(两轮相同),`1.25` 是轮次 id(每轮变)。

**多轮怎么通的(别记错因果)**:上游**不认** repeated `1.2.1` —— 请求里只要多于一条,
就是与缺 `1.2.1.2` 一模一样的静默挂起。所以历史靠 `fold_history()` 渲染成
`<conversation_history>` 文本塞进当前这一轮。代价是上游看到的是"一条很长的用户消息"
而不是结构化对话,但模型确实读得到(实测多轮答对)。
两形态(首轮 `1.1` 空 / 后续轮带预算表)是另一件事,它修的是"服务端以为历史在它那边"。

### Features

- `run::Phase{Opening,Continuation}`:两形态分别构造;`ConvRegistry` 记「这个会话在这个号上
  已经开过场」,匹配不上(新会话 / 换了号)自动降级回首轮全量形态。
- 工具调用识别:上游发起工具调用(顶层 `field 2` + `1.8.1=17`)时**主动收口**,
  不再陪它等到 watchdog。90s 挂起 → 6s 返回。
- `GetServerConfig` 加 single-flight + 失败不阻断 + TTL 120s→30min。

### Design Rationale

- **工具调用必须主动收口。** `tool_use` 没实现之前,陪上游等的代价是每个「需要动手」的
  请求都挂满 90 秒。有字就交出去、没字就明确报错,都比静默挂着好;报文前缀打进日志,
  攒的是下一步映射 `tool_use` 的规格(已看到 `call-<uuid>` / `fc_<uuid>` / 参数 `ls -la`)。
- **`GetServerConfig` 失败不该打挂聊天。** 它只是个握手下发的版本号,不是凭据,
  而 `api2.cursor.sh` 实测会偶发超时+单次要 5–6 秒。原先一个 `?` 让抖动变成用户可见失败。
- single-flight 用 async 锁:必须跨 await 持有才挡得住并发,std 锁跨 await 会让 future `!Send`。

### Notes & Caveats

- **`tool_use` 仍未实现**,所以 agent 类任务会在模型想动手时被截断(返回半句)。
  这是当前唯一的功能缺口,规格已备。
- 本地测试配置:`config/{accounts,instances,system}.yaml` + opencode 的 `cursor-local` provider。
  账号 `max_concurrency: 6`(opencode 一轮并发发 4 个,默认 2 会撞「并发已满」)。
  ⚠️ 真 IDE 不会同时发这么多,并发数本身是一处可被服务端区分的特征。

## [cursor-run-unblocked] - 2026-08-07

### 起因：请求被接受，却永远不生成

`gw-cursor` 合成的 `agent.v1.AgentService/Run` 能让上游回 **200**、回一帧会话回显
（证明它解析并接受了我们的会话），然后每 **10 秒**发一个 4 字节心跳，**永不产出文本**。
没有错误码、没有超时、没有 trailer —— 只看返回值查不出任何东西。

此前的排查方向全是**加法**（补开场帧、逐字照抄 19 个字段、换模型、half-close 与否），
全部无效。这次换成**减法**：拿当天新抓的一份真请求（27 帧 / 28KB），一刀一刀往下削，
看哪一刀让「出字」变成「只心跳」。

### 结论：请求与响应各错一处，都不报错

**① 请求：`1.2.1.2` 必须在场，哪怕长度为 0。**

| 请求 | 大小 | 结果 |
|---|---|---|
| 会话块含 `1.2.1.2`（空） | **446B** | ✅ 正常生成 |
| 同上，去掉 `1.2.1.2` | **445B** | ❌ 200 + 会话回显 + 永远心跳 |

两个请求**差一个字节**。服务端拿这个字段判定「本轮上下文已声明完毕」，缺了它就一直等。
内容可以完全为空 —— **它是握手信号，不是数据**。

顺带纠正：环境/系统提示那一块的路径是 `1.2.1.2`，不是文档记的 `1.2.17.9`（字段号是对的，
挂错了地方）。`1.2.17` 里只放 blob 哈希，**绝不能发** —— 一发服务端就会去取我们从没通过
FileSyncService 上传过的 blob，直接 `invalid_argument: Failed to resolve request context blobs`。

**② 响应：正文在 `1.1.1`，`1.4.1` 是思考。**

两者结构完全相同（`{1: 文本, 2: 1}`），只差字段号。解错了同样不报错：请求成功、有字出来，
但出来的是模型的推理过程，真实回答一个字都收不到。实测「只回答 PINEAPPLE」那次，
`1.4.1` 是「用户要求…」，`1.1.1` 才是 `P` / `INE` / `APPLE`。

### Features

- 帧0 会话块补上 `1.2.1.2`，挂在**最后一轮**（与真客户端一致，也避免几十 KB 重复）。
- `parse_frame()` 取代 `parse_text_delta()`，正文 / 思考 / 用量分开。
- 认 `1.14` 用量帧收口，并用上游自报的 token 数取代 `chars/4` 粗估。
- 系统提示透传实测有效（`1.2.1.2.25`）；不发时服务端会自己合成一份。

### Design Rationale

- **`1.2.1.2` 不受 `RunShape.context_block` 控制。** 那个开关是给二分实验用的，
  管的是「里面装什么」；字段本身是出不出字的开关，被开关关掉过一次就够了。
- **`1.14` 必须当收尾信号。** BiDi 流不会自己关，上游发完用量就只剩心跳。
  不认它的话每个请求都要挂到客户端超时 —— 表现是「答完了却一直转圈」，
  而在反代里这等于每个请求占着一个连接直到超时。
- **思考绝不能混进正文。** 拼进回答里客户会看到「用户要求我…」这种自言自语。
  目前是丢弃（`RespFrame.thinking` 已解出待用），透传成 Anthropic thinking 块是后续。
- 减法实验的每一步都留成了回归测试：这类 bug 的共同特征是**不报错**，
  只有把「当时的对照实验」钉成断言，重构时才会当场变红。

### Notes & Caveats

- 端到端实测出字并正常 `end_turn`：`grok-4.5` / `composer-2.5` / `default` 三个模型。
  `claude-*` / `gpt-*` 仍是计费额度问题（`ERROR_RATE_LIMITED_CHANGEABLE`），与协议无关。
- 仍未做：tool_use、thinking 透传、图像、文件附件（L2 FileSync）。
- 抓包手法与完整字段表见 `crates/gw-cursor/PROTOCOL-agent-run.md` §11。

## [restock-hunt] - 2026-08-06

### 起因:速刷号变稀缺,货一上架就被别人买走

补货的常规轮询是 **30 秒一轮**。这个间隔在「随时有货」的时代是对的 —— 那时
`min_healthy` 一破,下一轮就能买到。号变稀缺之后同一个间隔变成了纯粹的落后:
上架到售罄可能只有几秒,而我们平均要等 15 秒才去看第一眼。

近 7 天有 **854 轮**「水位已破、闸门全过、就差有货」,其中 2026-08-04 一天连断
**4.7 小时** —— 那期间流量全落到贵号池(¥0.068/积分 vs 自购号中位 ¥0.021)。

### 「缺货」与「不该买」是两种状态,只有前者值得提速

这是本次改动的全部判断依据。原先两者在代码里都只是一句 `reason` 字符串,
所以先给 `Decision` 加了结构化的 `out_of_stock`(**闸门全过、就差有货**),
而不是拿人读的那句话去匹配 —— 改一个字就会让提速悄悄失效,
而失效的表现是「一切正常,只是又没抢到」。

三种情况严格分开:

| 情况 | 处置 | 为什么 |
|---|---|---|
| 一件货都没有 | **提速到 5s** | 每多等一秒都是纯损失 |
| 有货但过不了闸门 | 不提速 | 闸门不会因为多问几次就放行 |
| 询价全部失败 | **不提速**(退回 30s) | 最可能的原因恰恰是对方在限流我们 |

第三条是**自动退避**:对方开始 429 时,抢货模式自己就退出了,不需要另写一套限流处理。

### Features

- **抢货模式**:缺货时轮询间隔切到 `hunt_interval_secs`(默认 **5s**),有货、
  闸门变化或询价失败即退回 `poll_interval_secs`。`hunt_max_secs` 可设时长上限,
  **默认 0 = 一直抢**(断供能连着 4.7 小时,那时最不该歇)。
- **事件回调(webhook)**:`notify_url` 非空时,「连续缺货超过 `notify_after_secs`」
  「抢到号了」「熔断」三类事件回调它。按域名自动适配企业微信 / 钉钉 / 飞书 / Slack,
  其余发通用 JSON(`text` + 结构化字段平铺)。逐事件节流 `notify_min_gap_secs`。
- **`POST /admin/api/restock/wake`**:外部到货监控的回调入口。与 `buy-now` 的唯一
  区别是 **`force = false`** —— 闸门全部照常。
- 面板:抢货实况(已抢多久 / 探测几次 / 是否已通知)进 `GET /restock/state` 的
  `hunt` 字段,页面顶部显示蓝色横幅与「抢货中」徽标。

### Design Rationale

- **抢货期间不写决策流水。** 5 秒一轮 × 4.7 小时 = 三千多条一模一样的
  「所有货源都没有库存」。那不只是占地方,它会把决策流水**这个工具本身**毁掉。
  改成进入/退出各记一条,带次数与时长 —— 两条比三千条更能回答「刚才断了多久」。
- **正因为不写流水,才必须有 `hunt` 实况字段。** 否则面板会连着几小时一动不动,
  与「真的卡住了」无法区分。该字段带心跳,进程被 SIGKILL 留下的残留会被判成
  `null` ——「永远显示正在努力」是最难察觉的一种错。
- **租约 TTL 一律按常规轮询间隔算,绝不用抢货间隔。** 否则 TTL 会缩到 15 秒,
  而退出抢货后要睡 30 秒 —— 租约在睡眠中途过期,另一个 router 顺势接管,
  两边轮流当 leader。提速反而会把互斥搞坏。
- **通知按域名自动认方言,不给下拉框。** 选错的后果是**静默不通知**
  (企业微信收到飞书报文回 200 + `errcode: 93000`),而这个功能的全部意义
  就是「出事时人能收到」。同理:HTTP 200 也要查 `errcode`,只看状态码会让
  「配错地址」表现成「一切正常但从来收不到」。
- **每轮抢货只叫一次人。** 断供期间每 30 分钟响一次的下场是被静音,
  而静音之后真正要紧的熔断通知也一起收不到了。
- **`wake` 不 force。** 一个盯着上架页面的脚本不知道我方的日预算、需求速率、
  熔断状态,让它 force 等于把花钱的判断交给外部。
- 抢货状态机抽成纯逻辑(`restock/hunt.rs`,12 个测试):它的每个分支都要
  几十分钟到几小时才在生产上表现出来,「抢了一整晚但从没通知」和
  「每 5 秒发一条通知」都是真实可能的写法,而两者都要等下一次断供才看得见。

### Notes & Caveats

- **两家货源都没有到货通知能力。** 2026-08-06 核过 drop 前端 bundle:
  API-Key 那套接口只有查询与下单,`/api/v1/*` 只认会话 cookie(会过期,
  不适合无人值守)。所以「对方通知我」这条路走不通,`wake` 是留给
  「别人替我盯着」的。
- **请求密度是 6 倍。** 5s × 2 家 ≈ 每家 12 次/分钟。对方若开始限流,
  询价会失败 → 自动退回 30s;但若表现为静默降速而非报错,需要人工把
  `hunt_interval_secs` 调大。这是本次改动唯一需要盯的指标。
  实际生效间隔取 `min(hunt_interval_secs, poll_interval_secs)` ——
  抢货永远不该比常规轮询更慢,顺带这也是关闭提速的方式(调到 ≥ 轮询间隔)。
- `notify_url` 会在 `GET /restock/params` 里原样回显(机器人 key 在里面)。
  与供应商密钥不同,它必须可见才能编辑,而该端点本就在 admin 鉴权之后。
  错误信息里的 URL 已被剥除,不会进 `/logs`。
- 参数全部走 `restock_params` KV,**没有动 `SystemSettings`**,回滚地板不受影响。

## [settings-effective-echo] - 2026-08-06

### 起因:「我保存了设置很多时候不生效」查不下去

运行参数存 DB 的 `settings` 表,worker 每 30s 轮询后热应用。链路本身是好的
(实测 `cache_floor_ratio` 0.6→0.9 在 **11:51→11:52** 生效,worker 连续运行 18.4 小时
`restarts=0`,全期 0 次解析失败)。真正的问题是**面板上只有应然值,没有实然值**:

- 面板显示的是「库里的 overlay 叠 YAML 基线」算出来的**应该是什么**
- 决定计费与调度的是各 worker 自己轮询、应用之后的**实际是什么**

于是这两件事完全无法区分:①真的没生效;②生效了,但在 30s 轮询窗口内看的、
或看的是保存前的历史日志行(用户这次遇到的正是②——截图是 11:46,生效于 11:52)。

更糟的是链路里有两处**静默**失败,长得和「保存成功」一模一样:

1. overlay 解析失败 → `continue` → 每 30s 跳过一轮、保持旧配置,只有一行 error 日志
2. 本版本不认识的字段 → 被无声忽略(该 worker 镜像比写库的那个旧),表现为
   「别的设置都生效,就这一个不生效」

### Features

- worker 新增 `SettingsSync`(最近一次**成功应用**的时刻 / 错误 / 未知字段 /
  应用后真正在用的值),启动期与 30s 轮询两处写入,`/health` 回显 `settings` 段。
- admin `GET /settings` 扇出问各 worker,把实然值挂在同一份响应的 `workers` 字段。
  **`PUT` 不带** —— 刚写完库时 worker 还没轮询到,回一份必然过时的值只会误导。
- 面板在**改设置的同一页**新增一张卡:逐 worker 显示「多久前同步」「同步是否报错」
  「未知字段」以及**与应然值对不上的字段**(全对时不列)。

### Design Rationale

- **成功才刷新时间戳**:失败时 `applied_at` 停住不动,「现在 − applied_at」就是
  配置僵了多久 —— 这是静默失效唯一的外部特征,必须让它可读。
- **比对 worker 回报的全部字段**,而不是一份手写清单:手写清单漏掉的字段会得出
  「一致」这个错误结论,而漏掉的恰恰是没人想到的那个。
- **新鲜度阈值取 90s**(轮询周期的 3 倍):每轮还要先跑账号同步并抢锁,一轮超 30s 很常见。
  宁可迟报也不能误报 —— 天天变红的健康 worker 会让整张卡失去可信度。

### 对抗审查(kimi)修掉的问题

- **[high] 旧镜像 worker 会被显示成绿色「一致」**,正好是本功能唯一想抓的场景:
  它的 `/health` 没有 `settings` 字段 → 前端拿到 null → 走「无差异」分支渲染绿勾。
  改为服务端单独标 `stale_image`,前端红色单列。
- **非 2xx 响应也可能带可解析的 JSON 错误体**,当成正常回包会把坏掉的 worker 显示成
  健康。加 `status().is_success()` 校验。
- **文案错误**:原写「重启才会恢复」,实际库里 JSON 恢复可解析后下一轮就自愈。
- **`inject_workers` 重建响应体**(序列化→解析→再序列化)有 Content-Length 与
  「body 非 object 就 panic」的边角。改成 `respond_with_workers` 多传一个参数。
- **时钟回拨**会让 `age_secs` 变负并被渲染成「从未成功同步」;区分 `applied_at==0`
  与时钟异常。
- React key 由 `instance` 改为 `group#instance`(instance 配重时会合并渲染)。

判定为误报、已核实的三条:①`unknown` 是 `BTreeMap`,顺序稳定,告警去重不会失效;
②admin 的 http 客户端有 **2s 全局超时**,一个挂住的 worker 拖不垮 `GET /settings`;
③保存后走 `invalidateQueries` 重新 GET,卡片不会消失。

### 第二轮对抗审查(kimi 三视角)修掉的问题

三个视角**独立指向同一组**核心问题,说明不是风格分歧:

- **[medium×3] 回显的是「打算应用的值」而非「真正在用的值」**。两处会让面板报假「一致」:
  ① `if let Ok(sv) = to_value(&full)` 序列化失败时 provider 被跳过,而 scheduler 与
  cache_sim 照常更新、快照照记成功;②**`claude-dario` 的 `apply_hot_settings` 是 no-op**
  —— 它的 provider 级设置(缓存计费)改了永远不生效,但 worker 算得出「有效配置」照样报,
  面板会渲染绿色「一致」,把原本要抓的 bug 原样重演。
  修:`Provider` 新增 `hot_settings_supported()`(默认 `false`,与 `apply_hot_settings`
  的默认 no-op 同进退,只覆盖其一就是撒谎),KiroProvider 覆盖为 `true`;序列化失败写进 `error`。
- **[medium×3] 手写 9 字段清单本身就是新的盲区**,而卡片注释还在批评这个模式。
  改用 `SystemSettings::from_effective` 全量回显(30 项),同时删掉前后端两份手工清单。
  `default_proxy` 传 `None` —— 代理 URL 可能带 `user:pass@`,而这份数据要上浏览器。
- **[medium×3] 扇出复用 `/health` 把设置页和上游耦合了**:该端点会对配额缓存陈旧的账号
  触发 `getUsageLimits`,于是「有人打开设置页」= 「对付费账号打上游」。这个号池对封禁
  很敏感。新增轻量端点 `GET /settings-sync`(只读一把 RwLock),扇出改打它。
- **错误串脱敏**:serde 的类型错误会把出错的**值**嵌进消息,而那个值可能是
  `socks5://user:pass@host` —— 这条串要一路上到浏览器。入库前过 `redact_proxy_url`。
- **保存后的正常收敛窗不再报红**:每次 PUT 后 ≤30s 必有 drift,一律标红等于训练用户
  忽略红色。改成黄色「收敛中」,同步僵住才上红。
- **`SystemSettingsPatch` 排除 `workers`**:它是只读回显,留在可写类型里的话,任何把 GET
  数据摊开进 patch 的写法都会被写侧的未知字段保护用 400 挡掉**整次保存**。
- 「一致」文案在同步僵住 / 有未知字段 / provider 不热应用时一律不显示(信号自相矛盾)。
- `store=None`(库打不开)的降级 worker 现在会说明原因,而不是永久标红却毫无线索。
- 删掉 `warned_unknown` 这份与 `settings_sync.unknown` 重复的手工同步状态。

判定为误报、已核实的三条:①`unknown` 是 `BTreeMap`,顺序稳定,告警去重不会失效;
②admin 的 http 客户端有 **2s 全局超时**,挂住的 worker 拖不垮 `GET /settings`;
③保存后走 `invalidateQueries` 重新 GET,卡片不会消失。

### 上线时实测撞到的第四个问题

首次上线后核对生产数据发现:`caio-worker-dario` 跑着 3 天前的镜像,`/settings-sync`
返回 **404**,而原实现把「非 2xx」一律当成抓取失败 → 面板显示**离线**。但它 `/health`
是 200,进程活得好好的。「连不上」与「连得上但没这个路由」指向完全不同的运维动作
(去看容器 vs 重建镜像),报错方向反了会把人指到错的地方。

抓取结果改成三态 `Fetched::{Unreachable, NoData, Body}`:**答得出话就说明进程活着**,
只是镜像旧。上线后三种状态各自渲染正确:

| worker | 状态 |
|---|---|
| worker0 (G0) | 绿色,30 项全部一致,18 秒前同步 |
| worker1 (DARIO) | 在线 + **镜像过旧**(而不是「离线」) |
| worker2 (EXP) | 离线(确实没在跑) |

### Notes & Caveats

- 本次改动在 **worker** 里,所以 worker0 必须重建重启 —— 会打断在途请求。
  (第二轮修的是 admin 侧映射,只重启 router,不再打断 worker。)
- `caio-worker-dario` 仍在旧镜像上,面板会持续标它「镜像过旧」。这是**准确的**:
  它的 provider 级设置本来就不热生效。等它重建后会转成 `provider_hot: false` 的红字说明。

## [restock-supplier-priority] - 2026-08-05

### 起因:「两家的号价值等价」这个前提被证伪

多供应商上线时选家规则是**纯按单价升序**,写在代码注释里的理由是「实测两家的号
价值等价(都是 KIRO POWER、都按墙上时钟死)」。当天晚些时候的数据推翻了它:

| 货源 | 号数 | `temporarily_suspended`(Kiro 侧封号) |
|---|---|---|
| drop(us-east-1) | 29 | **0** |
| kiroapp/eu | 12 | **12** |

其中 `kiro-apikey-025ed93fd7df` 到手第一个请求就 502、**零次成功**即被封;同一分钟
买的 drop 号至今在正常服务。10 个 EU 号还在 17:47–17:49 两分钟内**同时**停摆,
是整批被封而不是各自寿终。两批号跑在同一台机器、同一出口 IP、同样的并发参数。

结论:便宜 48% 的货架买到的号会被封。**封禁率既观测不到、也编码不进单价**,
纯比价规则必然持续选中它。

### Features

- **货架档位 `priority`**(数值越小越优先,与账号优先级同向)。排序键从
  `(价格, id)` 变成 `(档位, 价格, id)`,即 `Shelf::sort_key` 与 `rank_shelves`。
- **细到货架的覆盖 `shelf_priority`**:出问题的是 `kiroapp/eu` 这**一个货架**,
  `kiroapp/us` 是同一家的另一批号。只能整家降级的话,要么误伤 US、要么放过 EU。
- **档位由引擎按名册回填**(`registry::shelf_priority_of` → `Engine::survey_all`),
  供应商适配器一律留 0:一家货源不该自己声明自己有多重要。
- **快照新增 `next_pick` / `next_pick_why`**:「下一单会买哪个」由 `choose_shelf`
  本人回答并写进快照,前端只渲染。
- 面板:每个货架显示**生效档位**,名册可编辑本家档位与逐货架覆盖。

### Design Rationale

- **档位是软优先,不是硬绑定**。首选档缺货/熔断/超上限/余额不足时自动落到下一档。
  drop 常年 0 库存(近 7 天 854 轮无货),硬绑定等于亲手制造多供应商要消除的断供 ——
  断供的代价(客户报障)高于买到一个可能被封的号。
- **档位是人写的,不自动加权**。「这家的号能不能用」目前只能靠人看封禁数据判断,
  自动加权会把一次抽样噪声放大成长期偏置。
- **缺省 0 = 全部同档 = 退回纯比价**。老名册没有这两个字段,`#[serde(default)]`
  解析成 0,与本次改动前同序;`SupplierCfg` 没挂 `deny_unknown_fields`,所以
  **旧镜像读新名册**也只是忽略未知字段。回滚两个方向都安全。

### 对抗审查(kimi,三视角)修掉的问题

- **PUT 缺省语义不对称会静默回滚整个改动**:`priority` 原本是 `#[serde(default)] i64`,
  而同结构里的 `api_key` 是「缺省保留原值」。部署瞬间浏览器里缓存的**旧版面板 JS**
  保存一次名册(哪怕只想改日上限)就会把所有档位归零 → 排序退回纯比价,只留一条
  warn 日志。改成 `Option<i64>` / `Option<BTreeMap>`,与 `api_key` 同语义。
- **面板自己算「下一单会买谁」会说谎**:排序能复刻,余额/单价上限/日上限/
  `unit_cost_veto` 这些闸门复刻不了。三个审查员独立指出同一条。改为后端给
  `next_pick`,前端删掉自己那套判断;展示排序补齐 `unit_price_cny > 0` 过滤与定序尾键。
- **读路径不去重导致余额张冠李戴**:`choose_shelf` 的余额表按 `supplier_id` 收集,
  名册里两条同 id(只能绕过 admin API 直接改库产生)会让后一家覆盖前一家的余额,
  于是**用 A 的余额批准 B 的购买**。`parse_roster` 加去重。
- **软优先只测了一半**:原先只测「缺货落档」,补上「有货但过不了花钱闸门」——
  生产上后者更常见(首选家余额见底 / 本家日上限到顶)。

判定为误报、已核实的两条:①「可能存在绕过 `survey_all` 的 `choose_shelf` 调用点」
—— 全仓只有一个生产调用点且 surveys 确实来自 `survey_all`;②「旧镜像读新名册会
解析失败」—— `SupplierCfg` 未挂 `deny_unknown_fields`。

### Notes & Caveats

- **代码合入 ≠ 行为改变**。现网名册没有档位字段 → 全 0 → 仍是纯比价。
  必须在部署后配置名册,否则首选依旧是 ¥15.43 的 kiroapp/eu。
- **降级的自我封闭**:eu 降档后几乎不再被买 → 不再产生封禁样本 → 没有机制发现
  它什么时候变好了。目前只能靠人定期手动试一单,没有过期提醒也没有探针单。
- 单价上限要容得下首选档:现网 `max_price_usd=6.0` × `rate_cap=7.2` = ¥43.2,
  drop ¥26.36 过得去。若哪天上限被按 eu 价位调低,首选档会每轮被价格闸门否掉、
  静默落回 eu,线索只在决策流水的 whys 里。

## [restock-multi-supplier] - 2026-08-05

### 起因

补货只认 drop 一家。近 7 天有 **854 轮**「水位已破、闸门全过、就差有货」——
2026-08-04 一天连断 4.7 小时,那期间流量全落到贵号池(¥0.068/积分 vs 自购号中位 ¥0.021)。
接第二家 `kiroapp.io` 的理由是**缺货冗余**,不是价差(两边贴身跟价,且 EU/US 各有涨落)。

面板侧的直接问题:库存/单价/余额三张卡显示的是「drop 一家」,多一家之后这三个数
不再有单一含义,而只显示一家的后果是 —— 那家充足、另一家钱包见底导致买不到号,
面板上却一切正常。

### Features

- **`Supplier` 契约**(`restock/supplier.rs`):各家把库存表达成一组 `Shelf`、把钱表达成 CNY,
  引擎只比 `unit_price_cny`。购买结局是四态 `BuyOutcome`(Ok / Conflict / Fault / Unknown)
  而不是 `Result` —— 「结果未知」是唯一会丢钱的分支,必须能被单独表达。
- **选家只有一条规则**:`engine::choose_shelf`,**能买的里面最便宜的那个**。纯函数,
  11 条单元测试钉死。所有花钱闸门(单价上限、单位成本、余额、全局日上限、单家日上限)
  在这里逐货架跑一遍,过不了就换下一个货架而不是整轮放弃。
- **kiroapp 适配器**:计价单位是 credits,走 `credits → USD → CNY` 三段折算;
  分 `us` / `eu` 两个货架(价格与库存独立);错误体是扁平 `{"error":"中文串"}`;
  竞争失败是 403/404 而非 drop 的 409。
- **每家独立熔断** `restock_breaker:{id}`:一家 key 失效不再连累另一家。
- **每家独立日上限** `daily_cap_cny`(0=不限):限制单家的敞口。
- **订单记货源**:`restock_orders` 增 `supplier`/`shelf`/`region` 三列(`ensure_column` 热升级)。
  在途订单靠 `supplier` 才知道该向谁对账。
- **面板**:新增「货源」卡 —— 逐家余额(含原生单位)、逐货架库存与单价、今日花费与上限进度、
  熔断/询价失败状态,按价格升序排列(**与引擎选家同序,第一行就是下一单会从哪买**);
  名册可在面板增删改,保存后**下一轮生效不用重启**。密钥只写不回显。

### Design Rationale

- **名册存 DB 而不是 `system.yaml`**:后者挂 `deny_unknown_fields`,加字段会让回滚到
  旧镜像变成启动失败。旧镜像读不到这个键就回落「只有 drop 一家」,与本次改动前等价。
- **金额一进引擎就必须是 CNY**,折算只许发生在适配器里。两家用同一个上限汇率,
  所以绝对值偏高(fail-closed)但**比价精确**(倍率对消)。
- **不做同轮 fallback**:一个货架竞争失败后本轮就结束,等下一轮(30s)重选。
  号活 ~50 分钟,30s 的代价可忽略,而少一条分支就少一处花钱的路径。
- **对账进花钱锁**:一轮对账可能跑过 leader 租约(TTL 90s),第二个 router 会对同一批
  pending 再重放一次。两个执行体同时重放一个幂等键,对方幂等只要有缝就是双扣。

### 对抗审查(gpt-5.6-terra × Skeptic/Architect)修掉的问题

1. **drop 对账把真实扣款记成 ¥0**(两个 lens 都判 high)。drop 不下发单笔扣款额,
   对账路径又拿不到买前余额 → `spent_cny = 0`,一笔真钱从日预算消失、当天可多买一单。
   改为 fail-closed 回落**下单时的限价**。
2. **单家日上限形同虚设**:原先只在「已花 ≥ 上限」时屏蔽,没把本单算进去。
   上限 ¥20、已花 ¥19、本单 ¥15 会照买到 ¥34。
3. **名册改动会让在途订单变孤儿**:删/停用/改名一家之后,它的 pending 单永远找不到
   客户端去对账。`PUT /restock/suppliers` 现在直接 409 拒绝这类改动。
4. **查单解析失败被当成「订单不存在」**:200 但响应截断/字段升级时,一笔**已扣款**的
   订单会被判死、释放预算、永不重试。改为 `Unknown`;且「没找到」只有在
   `total <= page_size`(确认看全了)时才算数,否则保持在途。
5. **对账重放 `replayed=false`(已发生二次扣款)只打日志**:现在置 `double_charged`
   并**熔断该货源** —— 幂等失效意味着「重放安全」这个前提没了,而整套对账建立在它上面。
6. **重放丢了 `region`**:kiroapp 不带 region 会回落 US,要么判成另一张订单(二次扣款),
   要么取回一批错区域的 key。`Supplier::reconcile` 现在接 `shelf`。
7. **连败计数只增不减**:一次 401、几百单成功、再一次 401 会被算成「连续 2 次」而熔断
   一家健康货源。购买成功即清零。
8. **实扣超限价 / 成交数 > 请求数 / key 数 < 付款数** 三种账实不符现在都会写进决策流水,
   超限价还会当场熔断该家。

### Notes & Caveats

- ⚠️ **kiroapp 没有任何服务端限价与数量校验**。`max_total_cny` 完全没实现;`count` 超过
  `max` 时它 **clamp 后成交而不是拒绝**(2026-08-05 实测发 `count:99` 被扣走 10 个号的钱,
  150 积分 ≈¥154)。所以这家的敞口上限只有三个:**钱包余额**、引擎的 `max_per_purchase`、
  以及日预算闸。询价与下单之间涨价的那一笔**拦不住**,只能事后熔断止损。
  想要硬上限,唯一可靠的办法是**把钱包余额本身控制在可接受的敞口内**。
- ⚠️ **回滚约束**:一旦存在 `supplier='kiroapp'` 的 pending 订单,**不能裸回滚** ——
  旧镜像的 `reconcile_pending` 会把所有 pending 一律交给 drop 重放,用同一个 order id
  向 drop 下一张全新订单。回滚前必须先把在途订单清空或人工收尾。
- 历史订单的 `supplier` 是空串,不归任何一家(那时还没有多供应商概念,归给谁都是编的),
  但仍进全局日花费;对账时按事实归给 drop。
- 未做:同轮 fallback、余额守恒告警(`days_left` 预警)、ksk_ 占比按小时物化。

## [restock-money-safety] - 2026-08-05

补货的**资金安全**修复。起因是多供应商方案的对抗审查(gpt-5.6-terra,
Skeptic / Architect / Minimalist 三视角)——三个视角独立收敛到同一批问题,
而且它们**都不是新设计引入的,是现在就在生产上跑着的**。多供应商会放大每一条,
所以先修地基。详见 `docs/MULTI_SUPPLIER_PLAN.md` §3。

### 起因(五条,全部经代码核实)

1. `POST /restock/buy-now` **完全不受互斥保护** —— 它不抢任何锁,直接 `run_once(true)`。
   手动点两次、或手动与后台轮询撞上,两个执行体会读到**同一个** `spent`,各自下单扣款。
2. leader 租约 TTL 最短 **30s**(`poll_interval` 下限 10s × 3),而单轮外层超时是 **120s**;
   花钱前不复验持有。第 31 秒另一个 router 可以接管并下单。
3. **网络超时一律标 `failed`**,而对账只扫 `pending` —— 于是「对方已扣款、响应丢了」
   会**永久失去对账机会**,钱和 key 双双成孤儿。
4. 日预算只统计 `purchased`/`imported`,**不统计 `pending`**。120s 超时把购买掐断后,
   下一轮把那单当成没花过钱,换个 `client_order_id` 再买一次 ——
   幂等键防得住「同一个 id 重放」,防不住「换个 id 再来」。
5. `reconciled` 一置 `true` **永不再对账**。只覆盖得到重启前的遗留单,
   运行期产生的在途单只要进程不重启就一直悬着。

### Features

- **新增「花钱锁」`restock_purchase_lock`,与 leader 租约分开**。两把锁回答不同问题:
  租约 = 「谁来跑轮询」(长期持有),花钱锁 = 「此刻谁在花钱」(只在临界区内持有)。
  `SqliteStore::{try_acquire_lock, release_lock, holds_lock}` 是通用实现,
  `try_acquire_restock_lease` 改为委托给它(那段棘手的条件 UPDATE 现在只有一份)。
- 临界区从**读预算**开始(不是从下单开始),由 RAII 守卫 `PurchaseGuard` 归还。
  **`buy-now` 与后台循环都必须过这道门。**
- `DropError::is_indeterminate()`:`status == 0`(网络失败)或 `5xx` = **结果未知**。
  这类订单**停在 `pending`** 等对账重放,不再判死、也不计熔断。
- `restock_spent_since` 把 `pending` 按 `max_total_cny`(限价 = 可能花掉的上限)计入花费。
- `restock_pending_orders(min_age_secs)` 加年龄门槛;对账改为**每轮都跑**。
- 花钱前复验 leader 租约(`holds_lease`),手动购买跳过。
- 面板新增 `in_flight_orders`(在途单),与 `orphan_orders`(孤儿单)分开数。

### Design Rationale

- **为什么花钱锁不能复用 leader 租约**:后台循环长期持有租约,让 buy-now 去抢会永远抢不到,
  变成一个点了没反应的按钮。反过来只有租约、没有花钱锁,buy-now 就完全裸奔。
- **为什么 `pending` 按限价而不是按实扣计入**:在途单的实扣**还不知道**。方向必须
  fail-closed —— 宁可高估当日花费少买一个号,也不能低估到超预算。对账落定后高估自动消失。
- **为什么对账要加年龄门槛(300s)**:改成每轮跑之后,「刚落库、请求正在飞」的订单也是
  `pending`。没有门槛的话,另一个 router 会把**别人正在途中的那一单**拿去重放 ——
  对方若还没记录这个 id,那就是第二次真实扣款。门槛必须大于单轮外层超时(120s)。
- **4xx 与 5xx 的分界**:4xx 是对方在处理前就拒绝了(确定没扣款),可以安全判死;
  5xx 和网络失败则可能发生在扣款之后。实测该站往返 3.4–4.2s 而超时是 20s ——
  真触发超时时,对方多半已经处理完了。

### Notes & Caveats

- ⚠️ **TTL 锁给不了 exactly-once**(没有 fencing token):持有者卡死超过 90s 时锁会过期,
  放第二个执行体进来。这里拿到的是两层保障 —— 常见情况被串行化,而**最坏情况的总花费**
  由日预算兜住(现在把在途单算进去了)。要做到严格一次需要 fencing,不在本期范围。
- 全量测试 **962 通过 0 失败**。三条新用例都做过回退验证:撤掉对应修复即变红
  (回退「花钱锁校验 holder」时连原有的租约用例也一起红,说明重构被现有用例覆盖住了)。

## [restock-region] - 2026-08-05

自动补货上号时,**把号的服务区(region)一路带到建号**。这是接第二家供应商
(`kiroapp.io`,欧洲区)的前置改动;对现有的 drop 货源**零行为差异**。

### 起因

Kiro 的号**绑死服务区**。2026-08-05 实测一个 `eu-central-1` 的 `ksk_` key:

```
management.eu-central-1.kiro.dev/Get-Usage-Limits  → 200  KIRO POWER
management.us-east-1.kiro.dev/Get-Usage-Limits     → 403  {"message":"Invalid token"}
```

而 `gw_kiro::usage_limits::DEFAULT_REGION` 就是 `us-east-1` —— 账号 `extra` 里读不到
`region` 就用它。

`Engine::onboard` 原先把 key 序列化成**裸串数组** `["ksk_a"]`,走
`import::map_api_key(s, None)` 这条**不带 region** 的路径。drop 的号全是 us-east-1,
默认值恰好蒙对,所以这个洞一直没暴露 —— 换一家欧洲区的货源就是**每个请求都 403**。

### Features

- 新增 `restock::drop::BoughtKey { api_key, region, subscription_title }`,
  `PurchaseResult.keys` 从 `Vec<String>` 升级为 `Vec<BoughtKey>`。
- `BoughtKey::parse` 同时收裸串与 `{"key": "ksk_…"}` 对象,对象形态顺带取
  `region` / `subscription_title`;仍然只放行 `ksk_` 前缀。
- 新增纯函数 `engine::import_payload()`:上号载荷改为**对象数组**
  `[{"api_key":"…","region":"…"}]`,这样 import 走 `map_flat` → `map_api_key(key, Some(acc))`,
  region 才会落进 `extra`。
- 新增纯函数 `engine::key_strings()`:订单 `keys_json` 仍只记 key 串本身。

### Design Rationale

- **region 不进 `restock_orders.keys_json`**。一张订单的所有 key 来自同一个货架、
  共享同一个 region,所以它是**订单级**属性,该由订单加一列来记(多供应商方案 §3.5),
  而不是在每个 key 上重复一遍。本期因此**不动 store 层、不需要迁移**。
- **空 region / 空档位不写进导入对象**。import 侧对这两个字段用的是
  `filter(|s| !s.is_empty())`,写空串与不写等效;不写能让排障时一眼看出
  「这家没给区域」而不是「区域是空的」。
- **对象形态与裸串形态的 `account_id` 派生完全相同**(无 email 时都是
  `kiro-apikey-{sha256(key)[..12]}`),所以老号不会因为换了载荷形态而变成新号。
  测试 `无区域时与改动前的裸串路径逐字节等价` 把这条钉死。

### Notes & Caveats

- **drop 目前不发 region**,解析到就带上、解析不到留空,行为与改动前一致。
- 已用真实的 EU 号手工验证过整条链路:对象形态导入后 `extra.region=eu-central-1` 落库,
  worker 据此成功查到配额,30 分钟内承接 90 个 `claude-opus-5` 请求全部 200,
  按每输出 token 比同期 us-east-1 的号还快约 18%。
- 全量测试 959 通过 0 失败;新增的 region 测试做过回退验证(撤掉写入即变红)。

## [tier-hold] - 2026-08-04

调度器新增「**降层前先等**」:高优先层只是被 429 节流那几百毫秒时,等它回来,
而不是把请求送给低优先级兜底池。默认关闭,两个热调参数开启。

### 起因(生产实测)

30 分钟窗口:唯一活着的自购速刷号(`G0@0`,并发 100)吃 1082 个请求,同期三个 PRO+
兜底号(`G0@100`)吃 169 个。这 169 个**全部**由 429 造成,分钟级对照近乎 1:1 ——
零 429 的那 13 分钟里兜底号精确为 0。不是容量问题(在途并发 4–13,上限 100),
纯粹是「不肯等 250ms」。

漏出去的两条路径都不经过排队:

1. 吃了 429 的请求当场换号 —— `RateLimited` 不在 `worth_switching_account()` 的排除
   名单里,重新选号时该号刚被 pace,合格集把它剔了,最高层空了就落到 100 层;
2. pace 窗口内新到的请求直接降层 —— `eligible_ids` 本来就排除 paced 号。

排队(`queue_wait_ms`)管不到这两条:它的进入条件是「一个能选的号都没有」。
`acquire_in_group` 里确实有一段「等 pace 窗口过去」,但同样在那个分支里,
只要兜底号还活着就永远够不到。

### Features

- `SchedulerConfig` 新增 `tier_hold_ms`(单次取号的等待预算)与
  `tier_hold_window_ms`(请求开始后多久内其取号还允许等待)。**两者默认 0 = 关闭**,
  与开关引入前逐字节等价;经 settings 覆盖层热生效,不需要重启。
- `AccountScheduler::tier_hold_wait()`:一次持锁扫出「此刻能选中的最高层」与
  「仅因节流暂不可选的最高层」,只有后者严格更优先时才等,返回最早到期时间。
- `acquire_in_group` 增加 `allow_tier_hold` 参数;worker 按请求级墙上时钟
  (`retry_started.elapsed()` vs 窗口)传入,实现「窗口外照常降层」。
- `QueueStats` 新增 `tier_held_total`(每次内部睡眠 +1)。它只回答「开关有没有在
  工作、强度如何」;**不能**与 `paced_total` 相减推算漏量 —— 一次 429 可以让几十个
  并发请求各自等一轮,两者口径不同。量化漏出请看 `request_logs` 的各优先层占比。

### Design Rationale

- **等待判定放在 `select_id` 之前**。后者会把会话亲和的 primary 当场改钉到低层号上
  (「primary 不可用 → 立即改选并当场转正」),先选后等就晚了,还白烧一次跨层迁移的
  去抖额度。放前面还有个附带好处:`select_id` 一行没动,它现有的调度测试零改动通过。
- **不消耗 `attempts`**。那是给 busy 换号用的预算(`total * 5`),小组会被等待循环
  几轮耗光,把成功变成 `AllBusy`(503)。循环终止只由预算的单调递增保证。
- **只等节流,不等冷却**。禁用/冷却态归 `queue_wait_ms` 管,职责不重叠,否则两套
  预算叠加。
- **不等 permit 满的号**。窗口过去照样租不到,等只是把延迟加给客户 —— 与
  `best_available_higher` 用 `available_permits()` 判「高层饱和就别迁」同源。
- **必须封顶,且用墙上时钟而不是重试轮数**。`switch_cap(RateLimited)` 是全组,配 180s
  请求级总时限,不封的话一个持续撞 429 的请求能在同一个号上来回弹到超时。用时间窗口
  而非 `attempts`:后者对所有失败类别共用(凭证刷新失败、`ModelNotAvailable` 都在消耗
  它),拿它当等待额度会出现「前两轮被无关错误吃掉、真撞上 429 时反而不许等」——正好是
  本开关要治的病。窗口远小于 180s,顺带保证等待不会把响应推到那条硬线附近。
- **预算裁剪是最后一步**。睡眠下限(10ms)同时兼任「是否还值得等」的门槛:剩余预算不足
  10ms 就直接降层。若反过来「先裁到剩余预算、再取 max(下限)」,预算尾部会系统性超发,
  实际等待越过配置声明的上限。
- **没改 `worth_switching_account()`**。重新取号时等 pace 窗口再选中同一个号,效果
  等同于同号重试,还顺带保住 prompt 缓存(换号意味着 cache_read 全部重算)。动错误
  分类表会波及所有 provider 的换号语义,收益重复、风险更大。

### Notes & Caveats

- **预算用尽一律降层兜底,绝不因此把错误抛给客户** —— 有专门的用例锁死这条。
- 等待发生在**响应开始之前**,客户端只是变慢,不会看到半截流。
- 惊群:同一个 paced 号上的多个等待者会同时醒来再撞一片 429。用 `tier_held_total`
  的低位派生 0–48ms 确定性抖动散开(仓库无 `rand` 依赖,不为此引一个)。
- **回滚不需要重新部署**:把 `tier_hold_ms` 设回 0,30s 内热生效。
- ⚠️ 本次改的是 **worker 侧**,上线必须重启 worker 容器。账号运行态
  (`disabled_reason` / `disabled_until` / 亲和表 / LRU)只在内存,重启即清 ——
  已冷却/封禁的号会以「可用」状态回到轮转,被选中后再逐个失败下线。建议先把确认死透的
  号在 DB 里 `disabled=true` 再重启。

## [restock-2] - 2026-08-04

补货上线一天后的复盘与重做。**判活从「问 caio」改成「看请求日志」,触发从「钟点表」
改成「算这单划不划算」。**

### 起因(生产实测,不是推断)

过去 11 小时里 ksk_ 号池有 **8.1 小时零成功请求**,而同期全站流量没停 ——
02:00–09:00 那 7 小时的 3599 个成功请求全由贵号池承担。按小时看 ksk_ 占比:
有活号时 79–95%,断供时 0%。三个原因叠加:

| 原因 | 损失 |
|---|---|
| 高峰窗口闸排在算 health **之前**直接 return,没有「池子空了」的逃生口 | 7.0 小时/晚 |
| 余额保留金默认 ¥40 > 单价 ¥21,¥58 的余额买不了 ¥21 的号 | 25 轮 |
| 判活失真:全池零成功时仍报 `healthy=1` 拒绝下单 | 结构性 |

### Features

- **判活三态**:有成功=活;有尝试零成功=**僵尸**(不计入水位);窗口内无任何尝试=
  不下结论(维持 caio 的判断)。新号有宽限期。面板与横幅直接显示僵尸数。
- **单位成本闸**取代硬时间窗:`预期单位成本 = 单价 ÷ (min(需求, 单号吞吐上限) × 预期寿命)`,
  高于 `max_unit_cost_cny_per_credit`(默认 ¥0.04/积分)就不买。掏钱前用真实报价复核一次。
- 高峰窗口保留但**降级为可选硬禁买时段**,判断移到 health 之后(流水从此始终带水位)。
- 回收补上真正的死法:`temporarily_suspended` + 近 6 小时有尝试零成功。
- 账号清单新增**服务时长**、**¥/积分**、**死法**三列;快照新增需求速率、预期单位成本、
  实测寿命中位数。
- 轮询间隔默认 60 → 30 秒;余额保留金默认 40 → 0。

### Design Rationale

- **判活为什么不能只信 `reason`**:`status_snapshot()` 第一件事是 `heal_cooldowns()`,
  而 `TemporarilySuspended`(实测 0/35 永不恢复)被归为「可冷却自愈」,3600s 一到就被
  复活成 `reason == ""`。**补货每轮拉 `/health` 这个动作本身就会触发那次复活**,
  然后把复活的尸体数进健康号 —— 观测行为改变了被观测量。实测 22 个成功率 0% 的号
  各自每 51 分钟被轮到一次(正是 3600s 周期),11 小时白烧 258 个客户请求。
  这套判据 Python 原型推导正确过(`fails_since_last_success`),搬进 Rust 时整块丢了。
- **判活为什么留「不下结论」这一档**:窗口内零成功有两种成因 —— 打不通,和压根没被
  选中。夜里没流量时所有号都零成功,一律判死会触发连环购买。只有「有尝试」才构成
  反面证据。
- **买号策略的依据:号按墙上时钟死,不按用量死。** 22 个自购号全生命周期实测,
  烧速差 3 倍(676 vs 1990 积分/时)而存活一律 0.7–0.9 小时,烧得最猛的反而活得最久。
  推论:①并发拉满是对的,「省着烧」的部分到点直接蒸发;②**闲置一分钟就是烧钱一分钟**,
  需求不够时买号 = 花 ¥20 买 45 分钟只用掉三成,单位成本反而高过它要替代的贵号池。
  所以策略不是「买什么号」而是「什么时候买」—— 这与补货及时性是同一件事。
- **旋钮用「¥/积分」而不是「积分/时的阈值」**:前者随价格自适应。drop 从 $2.95 降到
  $2.20 时可接受的需求门槛自动跟着降,不用人改配置。
- **不做提前量买号**(参数留着,默认 0):检测 + 上号的空档约占 45 分钟周期的 2%,
  而提前 N 秒下单会等长折掉新号自己的寿命(同样是墙上时钟)—— 拿 6.7% 的产出换 2%
  的连续性,净亏。改用把轮询降到 30 秒,零成本减半空档。
- **不动 `worker/scheduler.rs`**:`heal_cooldowns` 复活冷却号对数据面是对的,
  错的是补货拿它当水位。在补货侧修,不去动正服务着付费客户的调度器。
- **实测寿命中位数只展示不自动生效**:这个值估短了就是每轮提前下单、花费翻倍,
  这种旋钮不该自己转。

### Notes & Caveats

- `Health` 原注释断言「掉出正常态 = 真死,是精确判据」——**这个前提不成立**,
  已改写。另一个未爆的雷:`queue_enabled` + `rate_limit_pace_max_strikes = 0`
  (生产实际值)意味着开了排队的号撞 429 **永远**不下线、永远停在 `reason == ""`。
  当前窗口内 0 个 429 所以没触发;判活改成证据驱动后它也不再能骗到水位。
- 生产需同步调三个参数:`peak_start`/`peak_end` 设为相同值(全天)、
  `daily_cap_cny` 设 800、确认 `min_balance_reserve_cny` 为 0。
- 覆盖时长翻倍意味着花费也涨:预计从 ~¥320/天到 **~¥500–560/天**(单位成本不变,
  买的是覆盖率)。¥800 上限是刹车不是目标。
- 池子里还有 6 个**人工上的**零成功号(power-4..7 / ad11f612dea7 / db58169df946),
  自动回收按设计不碰人工号,需人工 `disabled=true`。

## [restock] - 2026-08-03

把原本独立的 Python 自动补货服务(`/root/kiro-restock`，面板 :38995)**整体重写进 caio**。
单进程、单套配置、单个后台界面。

### Features

- **系统设置 → 自动补货**:总开关、DRY-RUN、补号时间段(高峰窗口)、水位、日上限、
  单价上限、熔断阈值、新号并发/排队模式、预测时长、闲时抑制阈值 —— 全部即时生效无需重启。
- **自动补货页**:实况看板、每小时积分消耗曲线(实测柱 + 斜纹预测柱同轴)、
  按钟点/按星期几两种画像、决策流水、ksk_ 账号清单(带成本与单次成本)。
- **按周预测**:用「星期几 × 钟点」画像预测未来 N 小时的积分消耗,
  样本不足时逐级退化(周画像 → 日画像 → 近期均值)并**如实标注依据**。
- **闲时抑制**(默认关):预测消耗低于历史峰值的设定比例时不补货。
- 决策、订单、自购号回收、启动对账、熔断,全部从 Python 版等价搬迁。

### Design Rationale

- **积分数据源换成 `usage_records`,不是 `request_logs`**。后者是
  `REQUEST_LOG_CAP = 10_000` 的硬环形缓冲(实测只覆盖约 1.5 小时);前者同样带
  `metering_credit` 且**永不裁剪**,线上已有 51 天历史。所以**周画像上线即成熟**,
  不存在 Python 版那个「要攒两周」的冷启动。聚合走 `stats_conn` 只读连接,
  避免百万行 GROUP BY 占住写锁让计费落库排队。
- **用 DB 租约做 leader election,不靠部署纪律**。生产上有**两个以上**
  `--mode router` 进程(kiro 一个、dario 一个,开了 exp 栈还有第三个),共用同一个
  control.db。把 60s 循环直接挂在 router 角色上就是各买各的、**重复扣款**。
  `README.md` 里「Router 单实例」是单通道时代的过时描述。
- **补货参数存自己的 KV,绝不进 `SystemSettings`**。后者有回滚地板
  (2026-07-31 前的镜像仍带 `deny_unknown_fields`,加字段会让回滚变成全量 503,
  正是 `375e1e3` 刚修过的坑)。密钥同理只留在 `SystemConfig`,不经 `/settings` 回显。
- **配置分两半**:启动期参数 + 密钥在 `system.yaml`(`:ro` 挂载且 dockerignore,
  改了 restart 即可);运行时可调项在 DB。
- **时区不引 chrono-tz**,用可配的 UTC 偏移分钟(默认 480)。Asia/Shanghai 自 1991 年
  无夏令时,固定偏移即精确,省一个依赖及其编译时间。
- **图用 CSS 柱不用 SVG**:SVG 文字随 viewBox 缩放,375px 窄屏刻度会缩到 6px 不可读;
  也与本仓既有可视化语言一致(用量表的占比条)。时间轴超 56 根自动并桶;
  周画像 168 格给自己的横滚容器(167 个 2px 间隙就 334px,不滚会被挤成 0 宽)。
- **闸门分两类**:自动化闸门(开关/窗口/水位/闲时)手动可越过;
  **花钱闸门(熔断/日上限/余额/单价/DRY-RUN)任何情况都不许越过**。

### Notes & Caveats

- **三层默认全关**:`system.yaml` 的 `restock.enabled`、DB 里的业务开关、`dry_run`。
  部署完不会有任何扣款,必须逐层手动打开。
- 上线过程中被测试抓到三个真 bug,其中两个会在生产直接出事:
  ① `#[serde(default)]` 与 `Default` 实现互相递归 → **启动即栈溢出崩溃循环**;
  ② `strftime('%s','now')` 返回 TEXT 不是 INTEGER → **租约完全失效 → 重复扣款**。
- 号的配额是 **10,000 积分**(`quota.limit`),不是调用次数;但实测号平均只用掉 **13.6%**
  就被上游 403 `TEMPORARILY_SUSPENDED` 封死,且 **0/35 个号在冷却后恢复过**。
  所以补货的真实成本由「号被扫死的速度」决定,不由配额决定。

## [account-sort] - 2026-08-02

### Features
- 账号列表新增**排序**选择器:「最新在前」(默认) / 「最早在前」/「按组+名称」。
  纯前端排序,不改后端接口。

### Design Rationale
- **默认改成「最新在前」**:后端 `list_accounts` 固定按 `group_name ASC, account_id ASC`
  返回,新上的号会散落在字母序各处。而 Kiro 短命号(`ksk_` API Key 实测寿命只有 14–22 分钟、
  额度约 320 次成功调用,且**成批死亡**)只能按上号时间管理,旧顺序完全帮不上忙。
- **同批号按 `account_id` 兜底排序**:成批导入的号 `created_at` 是同一秒,没有第二排序键的话,
  账号页 15s 运行态轮询每刷新一次同批号就互相跳位,根本点不中。
- **`sortAccounts` 是纯函数、不改入参**:调用方是 `useMemo`,原地 `sort()` 会污染
  react-query 缓存里的数组,让持有同一引用的其它 memo 读到被打乱的顺序。
- **排在筛选之后、分页之前**:保证「第 1 页」永远是当前筛选条件下最新的号。

### Notes & Caveats
- 服务器 Node 是 v12、本机 v14,都跑不了 Vite 6 / React 19。构建走 `oven/bun:1-slim`
  容器(该镜像本地已有,dario-sidecar 在用),**无需在宿主装任何东西**:
  `docker run --rm -v $PWD/admin-ui:/app -w /app oven/bun:1-slim sh -c 'bun install && bun run build'`
- `embed-ui` 是**编译期 feature**,前端改动必须重建整个 Rust 镜像才能生效,没有热更新路径。
- 上线用**新标签** `acctsort-20260802` 构建,保留 `poison-fix` 作回滚锚点;compose 已备份为
  `docker-compose.yml.bak-acctsort-20260802-151425`。只重建了 router(worker 不服务 `/admin`,
  不动它们就不断流量)。**回滚**:把 compose 里的 image 改回 `claude-all-in-one:poison-fix`
  再 `docker compose up -d router`。
- 上线前已核对:`crates/` 下 **0 个**源文件比 `poison-fix` 镜像新,故本次镜像不夹带任何
  未经上线验证的 Rust 改动;两个镜像二进制 md5 不同属预期(前端被 embed 进二进制)。
- 验证:`tsc --noEmit` 0 错误、`vitest` 15/15 通过(其中 6 个新增排序用例)、线上 bundle
  中英文案均命中、SPA 挂载无 console 报错、`/admin/api/{ping,accounts}` 与 `/health` 均 200。


## [queue-wait] - 2026-07-31

### Features
- 新增**逐账号**「排队等冷却」开关 `extra.queue_enabled`(默认关)。开启后 `acquire` 在
  「组内全禁用」时不立刻返回 503,而是等冷却中的号自愈再选,把上游限速在网关内部消化掉。
- 新增 `scheduler.queue_wait_ms`(默认 `0` = 关闭)作为**最长等待时间**,热调
  (`PUT /admin/api/settings`,worker 30s 轮询生效)。
- `PATCH /admin/api/accounts/{id}` 新增 `queue_enabled` 字段(走 `merge_account_extra`,
  绝不碰凭据);`GET /accounts` 回显该字段。

### Design Rationale
- **为什么逐账号而不是全局**:企业号(`ksk_`/IdC)的上游并发是**跨租户共享**的,429 是跟
  别的买家抢同一个池子 —— 等一下真的就有。而社交号的 429 常伴随额度见底,等待只是把
  客户多挂几秒后照样报错。一刀切会把后者的失败延迟化,体验更差。
- **为什么等待发生在响应开始之前**:此时还没发 HTTP 状态码,客户端只是变慢,不会看到
  半截 SSE。若改成"先发 message_start 再抢号",就再也不能返回 503,只能在流里发 error
  事件,客户端处理方式完全不同 —— 那是协议改动,不在本次范围。
- **队列容量按并发之和动态定**:上限 = 本组内已开排队、**且预算内真能服务**的号的
  `max_concurrency` 之和(1×)。额度跑干/config 禁用/1h 封禁的号**不计入** —— 否则一堆
  跑干的号会把容量撑大,等待者远超真实吞吐、全部排到超时(正是本开关要防的堆积)。
  等待者再多也吃不下更多并发,超出的只是排在后面陪跑到超时 —— 客户等更久、结果一样。
  取 1× 让最坏等待≈一次请求的周转时间,而不是好几轮。
- **额度跑干的号不产生等待**:`QuotaExhausted` 的 `disabled_until` 是 `None`,不构成
  "等得到"的理由。池子真干时仍然快速失败,不会把容量问题放大成全站卡死。同理,到期
  时刻在预算之外的(如 1h 的 `TemporarilySuspended`)也不等。
- **排队位用 RAII 守卫**:客户端中途断开会让整个 acquire future 被 drop,手写 decrement
  在那条路径上不会执行,计数只涨不落,几分钟后队列就永久"满"了。

### UI(同版追加)
- 账号表新增**排队**列(开/关),鼠标悬停说明该号冷却时的行为差异。
- 编辑弹窗新增**排队等冷却**开关(Segment 两档),提交时与原值比较,没动就不带该字段
  —— 否则对着不认这个字段的旧后端,只改并发也会让**整个保存**被判 400。
- 账号页顶部新增**排队实况**横幅:`等待数 / 容量`,以及已开排队的号数。容量口径与
  准入一致(只算开了排队**且当前可服务**的号),所以这个比值是真实拥挤度,不会因为库里
  躺着一堆额度跑干的号而虚高。等待数触到容量时标红。
- 排队实况新增两个**累计**计数:`queued_total`(进过排队的请求数)、`paced_total`(被节流吸收的
  429 次数)。理由:`waiting` 是瞬时值且几乎恒为 0(排队只在全组不可用时触发),准确但看不出
  机制有没有在工作;而节流日志是 `debug!` 级、线上 `RUST_LOG=info` 根本看不到 —— 累计值是
  这两个机制唯一的可观测面。上线首 4.5 分钟实测 `paced_total=627`(≈140 次/分钟的 429 被吸收)、
  `queued_total=0`。
- worker `/health` 与 admin `GET /accounts/runtime` 新增 `queue{waiting,capacity,enabled_accounts}`;
  `accounts_status[]` 新增 `queue_enabled`。旧 worker 不返回该字段时 UI **整块不渲染**,
  而不是显示 `0/0` 误导运维以为队列是空的。

### 新增:账号探针 `POST /admin/api/accounts/{id}/probe`
- **钉住指定账号**真发一次最小 chat(`max_tokens=16`、单轮 "hi"、收到首个文本 delta 即断流),
  返回 `{replied, model, elapsed_ms, text, error_kind, error}`。
  `?model=` 缺省 `claude-haiku-4.5`。
- **为什么必须有**:`/quota` 的 `verified:true` 只证明**控制面**凭据活着,`/models` 只证明
  目录里有这个模型 —— 两者都不代表数据面能出词。实测存在「有额度、目录有 opus,一发 chat
  恒 `ModelNotAvailable`」的号。判定停用号能否复活,只有真收到 delta 才算数。
  在此之前 caio **没有**任何"指定账号发一次请求"的能力,调度器自己选号。
- **不上报账号健康**:人工探测失败不计入与真实流量共用的失败池/冷却,否则批量探测会把好号
  探成 `too_many_failures`(与 `/quota` 的取舍一致)。
- ⚠️ **调用方必须串行 + 限速**。短时高频 chat 验号历史上直接导致 `TEMPORARILY_SUSPENDED`
  (22 分钟送走 5 个号)。服务端只做单次最小化,**批量节奏不替调用方兜底**。

### 新增:429 节流(替代二值冷却),只对开了排队的号
- `scheduler.rate_limit_pace_ms`(热调,默认 `0`=关)。开启后,开了 `queue_enabled` 的号
  命中 429 **不再下线**:只在 `pace` 毫秒内不被选中,到点继续抢 —— 即"保持一个频率访问"。
  面板上这类号仍显示**正常**,不再是"限流冷却"。
- `scheduler.rate_limit_pace_max_strikes`(默认 10)= **熔断**:连续这么多次 429 仍未成功,
  放弃节流退回二值冷却。上游真把号限死时不能无限定频硬撞(22 分钟送走 5 个号的教训)。
  `report_success` 一次即清零连击并解除节流闸。
- 请求级新增 **180s 重试总时限**:限流类错误的 `switch_cap` 本就是 `total`(全组),再叠加
  每轮取号各一份 `queue_wait` 预算,一个一直撞 429 的请求理论上能循环好几分钟 —— 而下游
  yapi 是 300s 无 event 即中止,超了客户只会看到更难查的中断。

### Fixes
- **节流窗口的取号空洞**(自查用例抓到):被节流的号 `disabled=false`,于是 `select_id`
  跳过它、错误分类却把它算进 `avail_total`,两边都不认领 → 掉进 `AllDisabled` 分支把 503
  抛给客户。现在单独统计节流中的号,像 busy 一样短睡重试,**与队列开关无关** ——
  节流窗口是我方自己设的几百毫秒,不该变成客户的错误。
- 探针扇出**不能复用 `AdminState.http`**:它是 2s 超时(为的是 worker 离线时快速跳过),
  而探针要真发一次 chat。超时在扇出循环里等价于"该 worker 离线",最终误报成
  「没有 worker 持有该账号」。已改用独立 120s 客户端。

### Notes & Caveats
- 等待预算**必须远小于**下游客户端的空闲判定。yapi 是 300s 无 event 即中止,建议
  `queue_wait_ms` 不超过 20000。
- 等待分支刻意**不消耗** `attempts` 预算(那是给 busy 换号用的),循环边界只由
  `queue_wait_ms` 决定。
- 队列满时返回 `AllBusy`(而非 `AllDisabled`),两者对外都是 503,仅日志可区分。
- **尚未做**:admin 面板 UI 里的账号级开关(当前只能走 API);按账号类型自动开启。


## [thinking-effort-default] - 2026-07-30

默认思考档位从 `max` 降到 `high`(用户反馈"太慢了"),并把它从编译期常量变成**设置面板
可热改**的运行期参数。

### Features

- **`DEFAULT_EFFORT`: `max` → `high`**。唯一事实源移到
  `gw_core::config::DEFAULT_THINKING_EFFORT`,`gw_kiro::anthropic_types::DEFAULT_EFFORT`
  改为它的别名 —— 该常量同时是配置 schema 的默认值,两处各写一份字面量必然漂移。
- **新增 `thinking.default_effort` 配置段 + `default_thinking_effort` overlay 字段**,
  走既有热更新链路:面板改 → DB overlay → worker 30s 轮询 → `apply_hot_settings` →
  `anthropic_types::set_default_effort`。**改档位无需重启 worker**。
- **设置页新增「思维链」区块**:five 档 segmented 选择器,每档给出实测的深度/延迟取舍,
  选中档位的说明实时显示在下方。
- **`PUT /settings` 校验档位合法性**:非法值 400 并列出可选档位,归一成小写后才落库。

### Design Rationale

**为什么降档**:2026-07-28 的剂量反应实测确认 `effort` 是真正在起作用的旋钮
(`low` 122 帧 → `xhigh` 644 → `max` 1100,单调无重叠),当时据此把默认提到顶格。
但深度不是免费的 —— `max` 约 1.7 倍于 `xhigh` 的思考量,就是约 1.7 倍的等待**和输出计费**。
`high` 的签名加密体是 `xhigh` 的 73%、耗时 95s vs 124s,是折中点,且恰好是上游多数模型
schema 自己的 `default`。**唯一例外是 opus-4.7**(schema `default` 是 `xhigh`):对它我们
现在显式发一个比上游默认更低的档,这是本次降档的本意,专门加了一条用例钉住
(`opus_4_7_default_request_lands_on_policy_default_not_upstream_xhigh`)。

**为什么用进程级全局而非依赖注入**:转换层(`thinking_policy` / `converter`)是一组自由函数,
`chat_stream` 与 `render_kiro_payload` 都不持有 provider 句柄,把参数一路穿下去要改到
gw-app 的请求日志路径。这与 `converter/cache_point.rs` 里 `thinking_signature` 等热控开关
同款,不新造机制。

**为什么归一逻辑要拆出纯函数版**:`normalize_effort` 现在读可热改的全局,拿它做断言等于让
用例依赖全局当下的值,并发跑的其它用例一改就互相污染。逻辑全部下沉到
`normalize_effort_with(fallback, raw)`,单测走这个入口;`set_default_effort` 自己的用例
只验校验语义(成功用例故意设成**当前值**,可观察状态不变),**本 crate 的测试从不把运行期
默认档改成别的值**。

**校验为什么做两层**:admin `PUT /settings` 挡住接口路径,`set_default_effort` 挡住手改 DB
绕过接口的路径。这个值会原样进 wire(`additionalModelRequestFields.effort`),脏值换来的是
上游 400,而且要等下一次真实请求才暴露。

### Fixes(对抗审查,3 个 lens 各自独立提出)

- **档位改成枚举 `ThinkingEffort`,非法值在配置装载边界不可表示**(3/3 lens 一致,Skeptic 判 high)。
  原先是无校验的 `String`:`system.yaml` 写 `default_effort: hihg` 能解析成功、`GET /settings`
  照实返回 `hihg`,而数据面消费点拒收并继续用旧值 —— **控制面显示值与实际生效值永久分叉**,
  面板上五个档位一个都不高亮,运维每 30 秒收一次告警却看不出该改哪。改枚举后 serde 在
  配置装载与 `PUT /settings` 两处边界直接拒绝,这个状态不可达。新增用例钉住
  `VALID_EFFORTS` 与 `ThinkingEffort::ALL` 逐项相等(两份档位表只在一处加档 = 面板能选、
  wire 拒收)。
- **`system.yaml` 存在却解析失败时拒绝启动**(Skeptic#1,既有缺陷,本次新增的 `thinking` 段
  给它添了新触发点)。router 与 worker 原先都是 `unwrap_or_default()` / 双 `.ok()`,而各配置段
  都带 `deny_unknown_fields` —— 一个拼错的字段名会让**整个** `SystemConfig` 静默换成默认值,
  上游超时、调度参数、缓存计费、实验开关一起被重置,线上只表现为"行为莫名其妙变了"。
  现在:文件**缺失**仍用默认值(合法形态),**存在却解析不了**当场报错退出。
  上线前已用生产 `config/system.yaml` 验过能解析(`upstream_timeout_secs=720` 等真实值),
  四个容器共用这一份、无人覆盖 `--system`。
- **补齐"热改真的生效"的端到端测试**(3/3 lens 一致)。原先所有新增用例都刻意不改进程级全局
  (避免污染同进程并发跑的 865 个用例),后果是**把 `set_default_effort` 里的赋值删掉、
  或让 `normalize_effort` 回头读编译期常量,单测依然全绿而线上"面板改了不生效"**。
  新增 `crates/gw-kiro/tests/thinking_effort_hot_reload.rs`:集成测试是独立进程,可以放心改
  全局(文件内用 `SERIAL` 互斥锁串行)。覆盖 `apply_hot_settings → 全局 → wire payload` 全链路、
  热改后仍走逐模型夹取(4.6 收到 xhigh 回落 high)、无 schema 的模型一个字段都不发、
  以及客户端显式档位不被默认值压掉。
- **`normalize_effort_with` 收回私有**(Minimalist#1)。它的 `fallback` 不校验合法性,公开出去
  等于给外部一条绕过档位白名单、把任意串送上 wire 的路;crate 内唯一调用方喂的是
  `default_effort()`,恒合法。
- **前端 patch 不再无条件携带该字段**(Architect#1)。`buildPatch` 原先拿补过默认值的表单值与
  **原始**响应比,旧后端不返回该字段时 `'high' !== undefined` 恒成立 —— 用户只改代理也会捎带
  这个字段,被旧后端的 `deny_unknown_fields` 判 400,整个保存失败。现在比较对象与
  `settingsToForm` 同口径补默认值。
- **`high` 的说明从「当前默认」改为「出厂默认」**(Skeptic#4)。它现在是可热改参数,
  管理员存了 `max` 后再看这句会自相矛盾。

不采纳两条:Architect#3 称"多个 worker 各自轮询、旧快照覆盖新快照",但一个进程只跑一个
worker(`--mode worker --instance N`),进程内只有一个轮询者写这个全局,前提不成立;
Minimalist#4 嫌本条目的设计辩护冗长,但项目约定要求 changelog 记 Design Rationale。

### Notes & Caveats

- 热应用时**字段缺失 = 不动当前值**,不回落编译期兜底 —— 轮询响应偶发缺字段不该把面板上
  设的档位悄悄打回出厂值。非法值只告警不生效。
- 4.5 系与 haiku 上游没有 `additionalModelRequestFieldsSchema`,这个设置对它们**不起作用**
  (一个字段都不发),UI 文案里已注明。
- 按 `budget_tokens` 翻译档位的老客户端(opencode 全量、部分 Claude Code)走
  `budget_to_effort`,**最高只到 `xhigh`**,到不了 `max`,也不受本设置影响。
- `OutputConfig::effective_effort()` 仍是仅测试可达的死代码(生产无调用点),本次只让它
  跟随运行期默认值,没有删。

## [admin-ui-memberships] - 2026-07-29

07-28 上线的「账号-分组多对多」重构后端全套改完了,admin-ui 一直停在旧模型上。
这次把运维界面对齐到新调度模型,并收敛上号入口。

### Features

- **`GET /accounts` 返回成员边**:每行新增 `groups: [{name, priority}]`(组名升序)。
  新增 `SqliteStore::list_all_memberships()` 一次查全表聚合,不按 owner 过滤 —— 与
  worker 选号用的 `load_group_memberships(owner)` 是两个口径,后者只回本 owner 名下的边。
- **账号编辑弹窗可改成员边**:每个分组一行「勾选框 + 高/低两档」,一个号可同时属多组、
  每组独立优先级。提交时按差集只发变化的请求(`upsertGroupMember` / `removeGroupMember`),
  单条失败点名是哪个组、不关弹窗。
- **删掉编辑弹窗里那个写 `extra.priority` 的优先级控件** —— 重构后调度根本不读它,
  改了没有任何效果,留着只会继续误导。
- **账号表格改显示成员边**:分组列渲染多个 chip;成员边为空时显示红色「无分组」。
  优先级列改为按组汇总(`高×1 · 低×2`),逐组明细在 title 里。
- **导入弹窗的方式选择改成两栏卡片**,标题由「导入 KiroManager 账号」中性化为「导入账号」。
- **移除「添加账号」入口**,claude-dario 粘贴 `.credentials.json` 的路径挪进 OAuth 弹窗的
  折叠区;后端 `POST /accounts` 端点保留(粘贴路径仍在用)。

### Design Rationale

**为什么必须做**:2026-07-29 踩了一次。3 个新导入的社交 POWER 号只有 `G0@100` 一条成员边,
而 G0 挂的两把 key 24 小时零请求 —— 号在 runtime 里显示「可用、0.0%」,`request_logs` 里
却一行都没有,白放了一天才被发现。旧 UI 既看不出一个号属于哪几个组,也改不了成员边,
这几天所有边都是手工 `curl` 敲的。

**`groups` 只加在列表响应上**,单条增删改的响应仍不带 —— 前端那几个 mutation 只做
invalidate、不回写缓存,加了也没人读。

**组名升序只为展示稳定**(表格里的 chip 不因刷新而跳动);差集比较是按组名做 Map 查找的,
不依赖顺序。测试钉住顺序是为了防表格抖动,不是差集正确性的前提。

**新勾选的组默认「低」档**,与后端 `default_member_priority` 一致。这也是更安全的一档:
新号直接进高优先层会一上来吃掉全部新会话 —— 07-29 五个 ksk_ 号就是 `pri=0 / conc=10`
配置下 22 分钟被上游风控封掉的。

### Fixes(对抗审查发现,三个 lens 跑在 codex 上)

- **[三人共识 high] 改归属可以绕过"一组一 owner"不变量**。`upsert_membership` 守着建边侧,
  但 `update_account` 直接写 `accounts.group_name`、不校验已有成员边 —— 边一条没动,
  却把边另一端的 owner 换了,同一条不变量从另一头被破坏,而且是运维在 UI 上点一下就能走通。
  现在 `update_account` 返回 `UpdateAccountOutcome`,在**同一事务**内先查"这个号参与的每个组里,
  别的成员归属谁",有冲突整单拒绝并回 400 点名是哪个组。**这是重构就带进来的既有缺陷,不是本次引入。**
- **[medium] 两档 UI 会静默改写 0/100 之外的历史优先级**。草稿原本存"高/低"档位,提交时映射回
  0/100,于是一条实际优先级 50 的边会在运维只改并发时被顺手改成 0。改为草稿存**原始数值**,
  只有用户真的点了档位按钮才写 0/100。
- **[medium] 部分失败会让账号掉容量**。成员边原先是加删混在一起并发发出;若新增因 CrossOwner
  失败而删除成功,账号会当场比原来的组还少。改为 `applyMembershipDiff` **先加后删,加失败就完全不删**
  —— 最坏只是多挂几个组,可见可恢复,不掉容量。
- **[medium] 部分失败后重试永远报错**。已成功的删除在重试时会再 DELETE 一次,后端返回 404 →
  又被记成失败。删除现在对 404 宽容(边本来就不在 = 目标状态已达成)。
- **[medium] N 条边 = N×2 次缓存失效**。改为单个 `useSaveMemberships`,全部 settle 后只失效一次
  (用 `onSettled` 而非 `onSuccess`,部分失败时同样要拿到权威新基线)。
- **[medium] `GET /accounts` 不是同一数据库快照**。原先分两次调用、中间释放了锁,导入正好插在
  中间就会返回"账号已建、边还没有"这种从未真实存在过的组合(`create_account` 是原子建号+建边)。
  合并为 `list_accounts_with_memberships()`,一把锁读完两张表。
- **[low] `groups` 缺失被当成"没有分组"**。旧缓存响应缺该字段时表格会误报红色"无分组"。
  现在区分 `undefined`(未知,显示 —)与 `[]`(确实不在任何组,报红)。
- **[low] UI 文案里的 `**markdown**` 原样显示星号**(放进原生 `title` 和普通 `<p>`),已去掉。

**未采纳**:审查建议加一个事务型的批量 replace 端点,让"保存账号"成为原子操作。
先加后删 + 单次失效已经堵掉了会真实伤到客户的那条路径(掉容量),剩下的中间态是
"多挂了几个组"这种可见可恢复的情形。等真有跨 owner 迁移需求时再上端点。

### Notes & Caveats

- `upsert_membership` 的 **CrossOwner 拒绝**(一个组的成员必须同 owner)会以 400 返回,
  前端原样透出错误文案。当前池子里除 DARIO 的 2 个号外全是 owner=G0,实际不会触发。
- 改归属现在要求**先把该号从与别人共享的组里摘出去**。这不是新限制,是原本就该有、
  只是一直没人拦的约束;实践中改归属本来就极少发生。
- admin-ui 经 `rust-embed` 内嵌进二进制(`router/mod.rs`),**改前端必须重建整个镜像**,
  不能只传 dist。
- 顶层 `priority`(=`extra.priority`)字段保留,仍是导入时的默认种子,语义未变。
- 一并清掉了随 `CreateAccountDialog` 删除而孤立的 30 个 i18n 键(zh/en 各 30 条)。

## [effort-max-default] - 2026-07-28

隔离栈交叉实验推翻了「旧 thinking 标签让思考更深」的结论。改用真正有效的那条通道。

### Features

- **默认思考档位 `xhigh` → `max`**(`DEFAULT_EFFORT`)。
- **停发旧 thinking 文本标签**:生产去掉 `KIRO_LEGACY_THINKING_TAGS`,
  `history[0]` 不再含 `<thinking_mode>/<thinking_effort>`,与 1.0.212 客户端一致。

### Design Rationale

先前基于一轮 n=4 的对照得出「关掉旧标签思考深度减半、省 43.5% 积分」。**交叉实验推翻了它**:
把处理在两个账号间对调后差异消失(ON 687 vs OFF 644,范围完全重叠),而第一轮的
2.7 倍差距主要来自**账号本身**(phamdragon 均 900 vs phambac 均 559)。原实验处理与账号共线。

同一批实验给出了真正有用的结论 —— **新字段才是起作用的旋钮**,且剂量反应干净:

| effort(仅新字段,无旧标签) | thinking 帧 |
|---|---|
| `low`   | 123, 121      → 122  |
| `xhigh` | 894, 509, 529 → 644  |
| `max`   | 1345, 855     → 1100 |

单调、无重叠。且带着旧标签时新字段照样说了算(`low` 70 帧 vs `xhigh` 687 帧,9.8 倍),
说明两条通道不互斥、新字段主导;`ON+low`(70)甚至低于 `OFF+low`(122),
旧标签在低档位不但没加深度还略微反向。

所以:**要深度就提 `max`,而不是靠在正文里塞提示词。** `max` 在上游 enum 里、真客户端
也发得出,是合规通道,不增加指纹;旧标签则效应存疑还带着真客户端不会有的文本特征。

`max` 在**所有**带 schema 的模型上都存在(含没有 `xhigh` 的 4.6 系),所以这个默认值
对全系模型都能原样落地,不会触发按模型回落。

### Notes & Caveats

- **积分会涨**。生产实测计费近似 `每请求 ≈ 0.84 + 输出token/1000 × 0.48`
  (输入 17-27 万 token 因 96% 命中缓存只值 0.84,**输出才是驱动项**)。
  thinking token 计入 output,所以档位提高必然涨钱。但真实流量 82.5% 的请求输出 < 600 token,
  底价占大头,涨幅远小于重推理场景的测试值。
- 本次同时移除旧标签 = 改动 `history[0]` = **一次性冲掉全部在途会话的缓存前缀**。
  与档位改动一起部署,这笔代价只付一次。
- 交叉实验里有 1 条请求失败(4s,429),已从统计中剔除;其余 13 条全部 200。


## [kiro-1.0.212-align] - 2026-07-28

同步到 Kiro 1.0.212 客户端的线缆形态。长期不同步 = 可被规则化识别 = 封号,这是唯一动机。
依据是拆包 `extensions/kiro.kiro-agent/dist/extension.js`(22 MB / 536,408 行)+ 真号只读实测。

### Features

- **`effort: "max"` 是合法最高档,不再被降级成 `xhigh`。** 真实 enum 为
  `["low","medium","high","xhigh","max"]`。此前把 `max` 当同义词映射,等于把客户端顶格的
  请求(生产每 300 条约 33 条)静默降一级。
- **thinking 档位改为逐模型夹取**(`clamp_effort_for_model`)。档位表随模型不同:
  `opus-4.6`/`sonnet-4.6` **没有 `xhigh`**;`opus-4.7` 的 schema default 是 `xhigh`
  (全表唯一);`opus-4.5`/`sonnet-4.5`/`haiku-4.5` 压根没有 schema → **一个字段都不发**。
- **`ListAvailableProfiles` 也迁到 `management.{region}.kiro.dev`**(POST 路径不变)。
  它属于 `AmazonCodeWhispererService`(UA 仍是 `api/codewhispererruntime`),但 1.0.212
  构造该 client 时的 endpoint 来自 `cpsConfigs`(`:389260`),值就是 management 域。
  **`q.{region}.amazonaws.com` 在 1.0.212 全树一次都没出现。**
- **`getUsageLimits` 迁到 control-plane**:
  `GET https://management.{region}.kiro.dev/Get-Usage-Limits?origin=AI_EDITOR[&profileArn]`,
  UA 换成 `api/kirocontrolplanebearer#1.0.0`,**不再发 `resourceType`**。
  `KIRO_LEGACY_QUOTA_ENDPOINT=1` 可整套切回旧形态。
- **新增 `ListAvailableModels` 接入**(`models_api.rs`)+ 落库 + 两个 admin 端点:
  `POST /accounts/{id}/models`(用该号拉一次并落库)、`GET /models/catalog`(读快照)。
  取回 `rateMultiplier` 供定价、逐模型档位表供上一条夹取。
- 顺带上线前一批已就绪改动:body 里必发 `agentMode`、无工具时不发空
  `userInputMessageContext`、UA 版本 → 1.0.212、旧 thinking 文本标签默认关。

### Design Rationale

- **为什么不补 `thinking` / `max_tokens`**:上游 schema 确实声明了这两个属性,但客户端的生成
  函数 `qe8`(`extension.js:222579`)整个函数体只有两个 `case`,产出恒为
  `{output_config:{effort}}` —— 补了就是比真客户端**多发**字段,与做这件事的初衷相反。
  (原计划里有这一条,拆包后撤销。)
- **为什么不补 `systemPrompt`**:`extractSystemPrompt` 受 A/B 开关
  `AB_SYSTEM_FIELD_INJECTION` 控制,默认 **false**(`:227082`)。关着时 system 留在
  `history[0]` —— 正是 caio 现有做法。当初为省积分这么设计是对的。
- **为什么档位不可用时回落 `default` 而不是升到 `max`**:对齐客户端 `A7`
  (`:140071-140076`)与设置面板(`:340057`)的行为。宁可少一档,也不擅自升档制造真客户端
  不会出现的形态。
- **为什么老配额端点必须换**(结论不变,但理由要说准):我一度断言
  `AmazonCodeWhispererService.GetUsageLimits`"全树零调用点" —— **那是错的**,`:493283` 的
  `fetchUsageLimitsData` 就在调它、还带 `resourceType`(只在"列 profile 顺带查用量"时走)。
  漏判是因为只搜了控制面的 `new yh(`。真正为零的是**域名**:`q.*.amazonaws.com` 全树不出现,
  连那个"runtime client"也被 `cpsConfigs` 指到了 management 域。换的是域名,不是"操作没人用了"。
  配额是后台**每 20 分钟一轮**的常驻轮询,比偶发请求的信号稳定得多,更该对齐。
- **模型目录复用 `settings` 表**(键 `model_catalog`)而不是新建表:零 schema 变更、零迁移。
  测试钉死了它与 `system` 键互不影响。

### Notes & Caveats

- **端点路径大小写和连字符是有意义的**:Smithy `@http` trait 逐字为 `/Get-Usage-Limits` /
  `/List-Available-Models`,不是 `/getUsageLimits`。
- **UA 虚惊一场**:客户端源码里 `getCustomUserAgent()` 返回空格分隔的
  `KiroIDE {ver} {machineId}`,但 SDK 的 `escapeUserAgent` 会把空格转成 `-`
  (`:356844-356846`),最终线缆上仍是 `KiroIDE-{ver}-{machineId}`。现有拼法是对的,未改。
- **上线前真号只读实测**(不发 chat):新旧配额端点返回 **937 字节、6 个解析字段逐字相同**;
  带不带 `resourceType` 返回**完全一致**(breakdown 恒为 `CREDIT`,上游根本不读该参数)。
  `ListAvailableProfiles` 用一个 **IdC 号**(最依赖这条路径的类型)两端各打一次:
  均 200、468 字节、**profileArn 逐字段相同**。
- **隔离栈端到端实测**(独占 2 个号、同出口、12 个请求全 200 零错误):
  `max` 档位原样上 wire;新配额端点与 `ListAvailableModels` 均真机打通(19 个模型,
  档位表与静态表逐条一致);前缀击穿护栏按设计只在未设 `KIRO_LEGACY_THINKING_TAGS` 的一侧告警。
- **`m/N,E` 这一段是沿用**,未从 bundle 中重新推导。它由运行时 feature 检测生成,
  控制面与 runtime 两个 client 的 bearer 认证路径相同,推测一致但未逐字验证。
- **本批全部前缀安全**,不动 prompt 缓存前缀。移除 `history[0]` 里的 thinking 标签
  **不在本批** —— 那条会让所有在途会话下一轮全 miss,须低峰单独切,并对着基准
  (opus-5:0.9584 积分/请求、83.4% 命中)盯两个数。

## [thinking-budget-floor] - 2026-07-28

客户端可以把按 opus 计费的请求降级成浅思考。给它设个下限。

### Features

- **`enabled` 模式的思考预算下限**:低于 `KIRO_MIN_THINKING_BUDGET`(默认 8192)时抬到下限。
  抬升被 `max_tokens - 1024` 夹住,夹不下就保持客户端原值(绝不挤掉答案的空间)。
- **只动 `enabled`**:`adaptive` 的深度由 effort 决定、budget 字段不参与;`disabled` 一律不碰。
- **`effort: "max"` 作为 `xhigh` 的同义词翻译**,不再当非法值回退。

### Design Rationale

- **为什么要有下限**:生产实测 **opencode 18/18 条请求都发 `budget_tokens=1024`**,签名加密体
  (真 CoT 的载体)只有 5800 字节 = `xhigh` 21984 的 26%,用户体感"秒回,不像在思考"。
  我方按 opus 计费,不该让客户端把它降级。
- **为什么不碰 `disabled`**:抽样 48 条 `disabled` **全是 Claude Code 的内部杂务** ——
  16 条 `max_tokens=64` 的会话标题生成 + 30 条 haiku 的技能路由。给它们开思考是纯烧钱
  + 拖慢 UI,对"用户用上聪明的 Claude"零贡献。
- **这是抬"上限"不是设"目标"**:`max_thinking_length` 是天花板,模型按需取用。上线前后实测
  同一道题:签名 5800 → **10260(+77%)**、耗时 45s → 68s,但输出只从 1621 → 1722 token
  (**+6%**)。成本不是按比例涨的。
- **为什么 `max` 是翻译不是回退**:Kiro 档位到 `xhigh` 为止,`max` 语义上就是顶格。之前当
  非法值处理,30 分钟刷 103 条告警,还掩盖了真正的脏 effort。

### Notes & Caveats

- **可见 thinking 文本长度不能用来判断思考深度**。实测 `high` 的可见摘要只有 1579 字符
  (全场最短),而它的签名加密体 15940 比 `low` 的 10744 大 48%。要看深度就看
  **signature 长度和耗时**,两者随 effort 严格单调。旧注释「high 仅产桩推理」据此推翻。
- **签名是单向的,这是 Kiro 协议决定的,不是缺陷**:线缆结构 `AssistantMessage{content, tool_uses}`
  没有 thinking 字段。客户端回传的 thinking 块会被静默丢弃 —— 实测两轮往返 HTTP 200、
  答案正确、缓存命中 4450 token,**不破坏会话**。副作用反而是正向的:上游收不到历史推理,
  模型每轮都从头重新思考,不存在"上轮没传回去所以这轮变笨"。
- 下限调高需评估成本:8192 实测约等于 `adaptive` + `low` 的深度(签名 10260 vs 10744)。

## [client-error-sanitization] - 2026-07-28

对外错误只说「客户能做什么」,不说「这条渠道背后是谁」。

### Features

- **`UpstreamError` 拆成两份文案**:`message`(内部诊断,含上游原始报文/接口名,只进
  `tracing` 与运维面)与 `client_message()`(对外)。后者是唯一对外出口。
- **`client_detail: Option<String>` 且 fail-closed**:默认 `None` → 按 `UpstreamErrorKind`
  给中性文案。只有我方本地生成、逐条确认过的文案才用 `bad_request_visible` /
  `with_client_detail` 登记(体积超限、报文解析失败、模型不支持、毒报文拦截)。
  新增错误点忘了登记 = 客户少看到一点细节,而不是 = 泄露渠道来源。
- **三条对外出口统一口径**:`upstream_error_response`(非流式/首包前)、
  `sse_error_event`(流内 error 事件)、`AcquireError::client_message()`(选号失败)。
- **运维面单独留一条 `admin_error_response`**:worker 的 `/oauth/exchange`、
  `/accounts/{id}/refresh`、`/accounts/{id}/quota` 仍回全量原文 —— 它们 listen 在
  127.0.0.1,由 admin 面板扇出调用,客户到不了。
- **router 侧同步脱敏**:`分组 'GECO' 无可用 worker` / `worker 不可达` → 统一
  `CLIENT_UNAVAILABLE`。分组名就是价格档,`worker` 就是账号池形态。
- **补日志**:非流式抽干与流中硬错误此前**没有**任何一处记录上游原文(唯一的去处就是
  发给客户端)。脱敏前先把这两处的 `tracing::warn` 加上,否则排查会变瞎。
- **公网 `GET /health` 收成纯存活探针**(对抗评审 high,两个镜头一致):旧实现无鉴权
  返回 65 KB —— `provider: kiro`、分组名(= 价格档)、出口 IP,以及 **220 个账号 ID
  (真实邮箱)** 连同配额/优先级/禁用态。报错文案脱敏得再干净,一条 `curl /health`
  就把渠道来源和整个账号池抖干净。明细在 `GET /admin/api/accounts/runtime`(admin 鉴权)
  里一条不少,面板走的本就是那条。顺带去掉「每次公网探针扇出 N 个内网请求」的放大面。
- **上游自发的 SSE `error` 事件也过闸**(对抗评审 high):provider 把错误当**普通 SSE
  事件**产出时(dario 直透 Anthropic 流即如此)绕开 `UpstreamError`,流式会原样转发、
  非流式经 `fold_sse_to_message` 原样回传。新增 `sanitize_upstream_error_payload`:
  **保留 `error.type`**(客户端按它判重试,动它就是改重试语义)**只换 `message`**,原文落日志。
- **`client_detail` 收成私有 + 删掉公开的 `with_client_detail`**(对抗评审三镜头一致):
  只留 `bad_request_visible` 一个入口。留着「把任意 String 登记为对外文案」的公开 API,
  fail-closed 早晚退化成靠自觉。

### Design Rationale

- **为什么不是逐个改字符串**:上游响应体是厂商控制的文本,今天是
  `USER_REQUEST_RATE_EXCEEDED`,明天可能是别的指纹。关键词黑名单迟早漏。所以对外**默认
  不透传上游任何文字**,要透传的必须显式登记 —— 白名单方向,不是黑名单方向。
- **为什么状态码一个不动**:429/502/529 的重试语义是客户端(Claude Code / SDK / NewAPI)
  的行为契约,改了会连带改变重试与「渠道判死」逻辑。本次只动 message。
- **为什么运维面不脱敏**:导号时「这个号是 invalid_grant 还是网络抖」全靠上游原文,
  一起脱敏等于把运维的眼睛蒙上。按「客户能不能打到这个端点」分界,不按错误类型分界。

### Notes & Caveats

- 客户端可见文案改为中性后,**客服口径要跟着变**:客户报「服务暂时不可用」时,凭
  `request_logs.error_kind` + worker 日志定位,不能再让客户把报错原文贴过来。
- `bad_request_visible` 登记的四处是有意为之(客户可自助):请求体超限 ×2、Anthropic
  报文解析失败、模型不支持 / 消息为空。新增登记前请自问文案里有没有厂商名、接口名、
  上游报文、账号标识。
- **`/health` 的返回体变了**:任何依赖它拿 worker/账号明细的外部脚本会失效(面板不依赖)。
  监控只看 `status == "ok"` 的不受影响。**worker 自己的 `/health` 未动**(内网 127.0.0.1,
  admin 面板与 `accounts/runtime` 都靠它)。
- **对抗评审驳回的两条**:①「按端点是否公网可达来划脱敏边界不够结构化,应拆成两个
  Router / 统一 PublicError renderer」—— 方向对,但那是一次协议层重构,不该搭在本次
  上线路径上;当前边界有注释 + 测试钉住。②「客户看到中性文案后无法与内部日志关联,
  应引入 request_id」—— 真缺口,但属新增特性,单列。

### 待办(评审提出、本次未做)

- 对外错误带一个不泄密的 `request_id`,与 router/worker/SSE 日志贯通,客服据此定位。
- `gw-claude-subprocess` 的本地请求校验文案(缺 messages / 无用户文本)目前退化成中性
  文案;该 provider 未部署,待启用前再逐条登记 `bad_request_visible`。

## [account-group-membership] - 2026-07-27

把 `group` 这一个词承担的**三件事**拆开。这是对同日上线的影子分组(见下一节)的
**替换**,不是叠加 —— 影子组那套整体删除。

### Features

- **新表 `account_groups(account_id, group_name, priority)`**:账号↔分组的 **N:M 成员边**。
  `priority` 挂在**边**上,不再挂在账号上 —— 同一个号因此可以在 A 组当主力(0)、
  在 B 组当兜底(100)。
- **`accounts.group_name` 语义收窄为「归属」**:哪个 worker 进程独占管理该号的运行态。
  列名与基数(N:1)都不变,只是不再兼任权限与排序。
- **调度器改按请求的成员视图**:`TierGuard` → `GroupView{rank}`。`eligible_ids` /
  `tiered_lru` / `best_available_higher` / `select_id` 里读 `e.priority` 的三处全部改读
  `view.rank_of(id)`;分层 LRU、会话亲和、向上迁移去抖(`MIGRATE_UP_DEBOUNCE`)语义不变。
  `acquire_where` 保留为「无视图」的薄封装。
- **自愈按视图收窄**:`heal_too_many_failures` 只复活**本组成员**。低价组一次全灭不再把
  正常组刚合法禁用的号一起复活并清零失败计数。
- **内网头简化**:`x-gw-tier`(档位区间 JSON)→ `x-gw-group`(组名)。worker 用组名从
  本地 membership 快照取视图;快照与账号集在同一把 `sync_lock` 内一起换。
- **router 按成员归属选 worker**:`pick_worker` 由「组名 == account_group」改成
  「owner 覆盖本组成员」(`group_owners()` + 15s TTL 快照)。一个组的成员因此**可以跨多个
  owner**,router 在其中做亲和/负载 —— 旧模型做不到。
- **admin 成员边端点**:`GET/POST /groups/{name}/members`、
  `DELETE /groups/{name}/members/{account_id}`、`POST /groups/{name}/members/bulk`
  (按 owner / subscription_title 批量加,220 个号手工点不现实)。
- **删除影子组全套**:`shadow_of`、`tier_min_priority`、`tier_max_priority`、`TierPolicy`、
  `AcquireError::TierExhausted`(→ 语义更准的 `GroupEmpty`)。

### Design Rationale

- **为什么非拆不可**:归属是物理约束(两个 worker 持有同一个号 → 并发翻倍 + rolling
  refresh_token 互相覆盖 → `invalid_grant` 报废),而权限与排序是策略。把策略焊死在物理
  约束上,导致「一个号进两个组」只能造影子组、「同一个号在两组里排序不同」只能拿全局
  priority 切区间 —— 而单侧区间在数学上表达不了「只要低优先的号」,才有了同日的
  `tier_min_priority` 补丁。**根因是建模,不是缺一个字段。**
- **2026-07-27 事故就是这个建模缺陷的代价**:低价档配成「只用小号」后,13 个小号全部
  上游过载、成功率 71%,却**无法溢出**到主力号(溢出顺序是全局的,改不了),只能硬报错。
  新模型下这就是一条成员边的事:低价组配 `小号@0 + 主力@100`,小号压满自然溢出。
- **回填保证逐条等价**:`INSERT OR IGNORE ... SELECT account_id, group_name,
  COALESCE(extra.priority, 100) FROM accounts` —— 每个号在原组、优先级沿用原值。
  `OR IGNORE` + 复合主键使其**幂等且只补不覆盖**:运维手工调过的组内优先级不会在
  下次重启被账号上的旧值冲掉。
- **查不到组名 → 空视图(503),绝不回落全量池**:回落等于让一个成员边还没同步过来的
  受限分组瞬间拿到全部账号,是静默提权。头**缺席**才回落全量池(未分组 key / 滚动升级
  窗口内的旧 router),这两种情况下的全量池本就是升级前的行为。
- **`delete_group` 护栏收紧**:原先只挡影子组,现在**任何**仍有 key 绑定的组都不许删。
  删组会把 `api_keys.group_name` 清空,而 router 把空组名回落到主组 —— 这些客户当场
  拿到全部账号。下线一个组的正确姿势是清空它的成员边(该组随即 503),或先迁走 key。

### Notes & Caveats

- **验收基线**:`cargo test --workspace` 802 绿,其中 **49 条既有 scheduler 测试零改动
  通过** = G0 行为不变的机械证明。两处变异(非成员回落默认排序、分层比较取反)均被
  多条测试抓红。
- **生产数据副本验证**(隔离栈 `/root/caio-next`,222 个号的 `refresh_token` 全部清空):
  迁移 222 条边、与旧模型**不一致 0 条 / 漏建 0 条**;G0 键 33 请求 100% 落 POWER,
  低价键 30 请求 100% 落 PRO MAX(两组账号集完全不相交);低价组第一层全灭后
  12/12 **溢出**到主力号而非硬报错。
- **`extra.priority` 仍保留**,但只作导入时的默认种子,调度不再读它。改它不影响任何
  已存在的成员边 —— 这是刻意的,避免"改了账号却不知道改到了哪些组"。
- **发布顺序不可颠倒:先 worker 后 router**。旧 worker 不认 `x-gw-group`,会忽略它并用
  全量池。更强的保险是**先上代码、后改成员边**:回填后的成员边与升级前等价,切换那一刻
  行为零变化。
- **未做(需要时再说)**:按组的并发预算 / 额度闸门。组内优先级只能保证"先用小号",
  不能保证"低价最多占主力号 N 个并发槽"——共享账号的并发仍是先到先得。

### 对抗评审(codex,三镜头)修掉的问题

首轮判 **REJECT**,4 条高危有共识。都是设计成立、实现有洞的那类:

- **worker 启动后 30 秒全组 503**:周期同步的首跳被跳过(注释"启动时刚加载过"只对
  账号成立),`group_views` 留空 → 头 30 秒每个带组名的请求都 GroupEmpty。**每次发版
  都会打出一个 30 秒不可用窗口。** 改成启动时同步装载成员边。
- **删组会毁掉同名 owner**:`delete_group` 只看绑在该组的 key,却把归属该组的账号
  `group_name` 清空 → 那些号成为孤儿,而**借用它们的别的组当场全量 503**,删的人还
  看不出因果。新增 `IsOwner` 拒绝;删组不再改动账号归属。
- **跨 owner 组静默降级**:成员跨 owner 时 router 只按会话数选 worker,被选中的 worker
  只看得见自己那部分成员,可能直接用兜底层而另一 owner 的主力号闲着。与其假装支持,
  改为写入侧拒绝(单条与批量都拒,批量**整批**拒不做部分写入)。
- **成员边与账号集撕裂发布**:两次独立查询分别发布,membership 成功而账号失败 → 新视图
  配旧账号;反过来 → **已撤销的成员边继续授权**(提权方向)。改成都读成功才发布、
  任一失败整轮跳过,且**先视图后账号**(撤销立即生效,新号至多短暂不可选)。
- **自愈触发条件被放宽**:原语义是"全池告罄"才自愈;按视图收窄后,一个只含单个坏号的
  小组每个请求都能触发一次自愈 → 该号反复复活,连续失败保护对它失效。加
  `whole_pool_exhausted` 前置闸门(复活范围仍按视图收窄)。
- **两个轮询器错配**:router owner 缓存 15s、worker 成员快照 30s,合法变更会有最长约
  30 秒的错误路由窗口。成员边变更后主动捅 worker `/sync`;owner 缓存查询挪出全局
  mutex(否则控制面抖动变成数据面队头阻塞);冷启动读库失败回落"组名即 owner"而不是
  空映射(空映射会让每个组都 503)。
- **缺少一步下线动作**:错误信息让运维"清空本组成员",却只有单条 DELETE(220 个成员
  要发 220 次,中途失败留半下线态)。新增 `DELETE /groups/{name}/members`。

**已知取舍(刻意保留)**:`last_selected_at` 挂在账号上、全 worker 共享,两个组共享同一
层里的号时会互相扰动**层内**轮转顺序。分层(哪层可见、先用哪层)才是隔离语义,已由成员边
严格保证;层内先用谁不影响可见性、不影响溢出顺序。要让层内 LRU 也按组独立,得给每个组
维护一份 `last_selected_at`,那会让"同一个号被多个组用"的负载彼此看不见,更容易把号打爆。
已用一条测试把该耦合显式钉住,避免以后被当成 bug 误修。

## [shadow-group] - 2026-07-27

### Features

- **影子分组(低价档)**:新增一类分组,它**不持有账号、不绑 worker**,而是复用源组的
  worker,只是**可见的账号更少**。典型用法:`GLOW.shadow_of = G0` + `tier_max_priority = 0`
  → 低价档只看得见 G0 里 priority=0 的主力号,看不见 priority=100 的兜底层。
  - `groups` 表加 `shadow_of` / `tier_max_priority` 两列(`ensure_column` 增量迁移)。
  - `authenticate` 的 SQL 加一次 `LEFT JOIN groups`,零额外查询把策略带到 `AuthenticatedKey.tier`;
    **策略每请求现读** → admin 改完下一个请求即生效(回退无需重启、无需发版)。
  - router 新增 `resolve_route`:影子组映射到源组再 `pick_worker`,并经内网头 `x-gw-tier`
    把守卫下发给 worker。`forward` / `forward_models` / `count_tokens` 三个入口全部走它。
  - scheduler 新增 `TierGuard` + `acquire_tiered`;`acquire_where` 保留为委托 `None` 的薄封装。
- **`AcquireError::TierExhausted`**(→ 503):档位内账号全冷却/占满时的专用错误。
- **删组保护**:`delete_group` 返回 `DeleteGroupOutcome`,对「被影子组引用的源组」和
  「仍有 key 绑定的影子组」返回 409。
- **转组保护**(对抗审查追加):把一个**仍有客户 key 在用**的普通组就地转成影子组会让
  那些客户无声降级(丢掉低优兜底层 + 改路由),`validate_shadow` 一并拒绝。反方向
  (`shadow_of=""`,低价档下线)不受影响 —— 那是解除限制,是标准回退姿势。
- **写入时兜底**(对抗审查追加):`create_group` / `update_group` 把「源组必须存在且本身
  非影子」合并进同一条 SQL 语句。admin 层是"先读快照再写"两次独立加锁,并发的
  `delete_group` 能在窗口里删掉源组 → 写出 `shadow_of` 悬空的行(整组静默 503)。
- **会话亲和按档位分命名空间**:worker 侧亲和键由请求内容派生、不含档位,两档共用一张表。
  正常客户的会话若钉在 priority=100 兜底号上,一个键名碰撞的低价请求会因守卫判定该号不合格,
  触发 `select_id` 的「primary 不可用 → 改选并当场转正、永不迁回」,**永久改写正常客户的
  钉扎**(上游前缀缓存冷启动)。档位头带上组名,worker 据此给亲和表分区。

### Design Rationale

- **为什么影子组不能有自己的 worker**:`instances.validate` 与 router 启动校验都禁止
  一个 account_group 绑多个 worker。两个 worker 加载同一批账号 → 并发上限翻倍 +
  各自刷新 rolling refresh_token 互相覆盖 → 账号 `invalid_grant` 报废。所以"一个号同时
  属于两个组"只能做成**同一 worker 内的可见性视图**:一份 entry、一个信号量、一个刷新写者。
- **守卫为何独立于 `supports` 谓词**:塞进 `supports` 的话,过滤后的空集会掉进
  `!supported_any` 分支 → `NoModelSupport` → **400**「订阅等级不足」。而档位耗尽是运行时
  状态、稍后可恢复,必须是 **503**;客户端(SDK/NewAPI)对 400 不重试,会误判成请求非法。
  错误码映射同时从二分 `if` 改成穷尽 `match`,逼后续新增变体做显式决策。
- **带守卫的请求不触发全灭自愈**:`heal_too_many_failures` 是全局的,会复活所有
  `TooManyFailures` 账号并清零失败计数。若低价请求也走这条路,正常组刚合法禁用的号会被
  反复复活,连续失败保护对所有档位一起失效。
- **守卫判据只收单调量**:`max_priority` 只在 admin 改配置时变,所以会话亲和不会因它反复
  重钉。`available_permits()` 这类抖动量**不能**进守卫——primary 一失格就会"改选并当场
  转正、永不迁回",会让会话反复换号、上游缓存冷启动,反而放大额度消耗。
- **删组为何要拦**:`delete_group` 把成员的 `group_name` 清成 `''`,而 router 的
  `resolve_group` 把 `''` 回落到 `default_group`(主组)。所以裸删影子组 = 低价客户**静默
  提权**成主组不受限访问,无任何告警。正确的下线姿势是 `PATCH {"shadow_of":""}`。

### Notes & Caveats

- **默认完全 inert**:DB 里没有 `shadow_of != ''` 的行时,`tier` 恒 `None` → 恒不发头 →
  `acquire_tiered(.., None)` 与旧 `acquire_where` 逐字节等价(有测试
  `guard_none_matches_acquire_where_exactly` 机械保证)。开关是一行 DB 数据,不是代码分支。
- **发布顺序不可颠倒:先 worker,后 router**。新 router + 旧 worker = 发了头没人认 =
  低价流量无守卫打主力号。更强的保险是代码先上线、影子组后建。
- **schema 双向兼容**:所有 SELECT 都是显式列名,旧二进制读新库正常 → 回滚二进制不需要
  回滚 schema。
- **共享账号 = 共享故障域(本次范围外,需知情)**:低价流量消耗的是主力号同一份月额度;
  它触发的 `QuotaExhausted`(永久禁用)/ `TemporarilyBlocked`(1h 冷却)会同时把号从正常组
  踢掉;**封号风险无法隔离**(账号/machineId/profileArn/出口 IP 都共享)。本次只做分组与
  优先级可见性,未做额度预算、故障归因分层、并发预留。

## [upstream-overload] - 2026-07-25

### Features

- **新增 `UpstreamErrorKind::Overloaded`——上游模型级过载不再当成账号故障。** 三条策略同时成立:
  - **不惩罚账号**(`spares_account_health()`):不计 `failure_count`、不触发 `TooManyFailures` 禁用。
  - **不换号**(`worth_switching_account() = false`):容量是模型级的,换号打的还是同一个模型端点。
  - **同号退避重试**(`chat_with_overload_backoff`):250 / 750 / 2000ms,各叠 0~40% 抖动。
- **模型级过载窗口**:收到显式 `MODEL_TEMPORARILY_UNAVAILABLE` 后 60s 内,该模型的通用 5xx
  (`reason:null`)也按过载处理。窗口按模型隔离,不跨模型泄漏。
- **对外映射 529 `overloaded_error`**(Anthropic 官方过载语义)。状态码收敛到 `upstream_status()`
  单点,响应体与请求日志 `status_code` 同源。
- 测试:gw-core 49 / gw-kiro 447 / gw-app 184 全绿,新增 10 条。

### Design Rationale

- **事故实况(2026-07-25)**:Kiro 的 opus-5 容量抖动被记进账号连续失败计数,**35 秒内禁光 7 个
  健康号**并触发全灭自愈。禁用对**所有模型**生效,于是 opus-4-6 / sonnet-5 一起被连带打挂。
  上游报文两种:`{"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}` 与 `{"reason":null}`。
- **为什么不换号**:实测 177 次上游 500 靠换号只救回 19 次(19%)。换号还有两笔实打实的成本——
  白烧另一个号的配额,以及丢掉会话 cache 亲和(实测一次 opus-5 请求 `cache_read` 达 10.7 万 token,
  换号全部重算)。
- **为什么这不违反 2026-06 防雪崩约束**:同号退避的**爆炸半径 = 1 个号**,比原换号重试(默认 2 个号)
  更小。`max_switch_attempts` 一字未改。
- **为什么通用 5xx 用"窗口"而不是直接全归过载**:重分类必须有上游显式信号背书。实测 176 条通用 5xx
  中 **84.7% 与显式过载落在同一分钟**(35 个有 5xx 的分钟里 19 个两种并存),故以显式信号为真相源;
  窗口外的通用 5xx 一律仍是 `ServerError`(仍换号、仍记账号失败),不靠猜。
- **窗口取 60s**:事故是分钟级成簇的,60s 刚好覆盖一簇。更长会把上游真内部错误也长期误判成过载,
  更短则簇内空隙漏掉、退回逐个禁号。
- **抖动是必需的**:并发请求会在同一波容量抖动里齐刷刷失败,无抖动会同步重撞,把重试变成新尖峰。
  熵取 `uuid` v4 的一个字节(workspace 已依赖 uuid+fast-rng,不为此引入 `rand`)。

### Notes & Caveats

- **`report_failure` 刻意穷举 kind 而非用 `spares_account_health()` 做守卫**:守卫会让新增 kind 悄悄
  落进某个分支,穷举则编译不过、强迫做决策。两者一致性由测试
  `spares_account_health_matches_no_penalty_arms` 锁住。
- **校正只作用于首包前的错误**。事故中观测到的 276 次 5xx 全部 `ttfb=NULL`(都在首包前);流中途
  冒出的 5xx 仍走 `finish_response` 原路径,不做窗口校正。
- **过载窗口仅内存**,重启即清、重新学习(与 `model_unavailable` 同策略)。
- **失败请求的上游报错仍未落库**:`response_payload` 只在 `Outcome::Ok` 时写,失败一律空串,原因只在
  `tracing::warn!` → docker 日志会轮转。所以 admin UI 的"失败"依旧不显示原因,排查仍需
  `docker logs caio-worker0`。这是本次**未**修的独立改进项。
- 事故期间曾把 `max_failures` 从 5 热调到 30 止血(admin settings,30s 生效);本次上线后**已调回 5**。

## [opus-5] - 2026-07-25

### Features

- **接入 Claude Opus 5**(上游 Kiro 已支持,官方 2026-07-24 发布):对外 `claude-opus-5`,上游 `claude-opus-5`。
  - `KIRO_MODELS` 权威表新增一行(1M 窗口、`supports_thinking=true`、`identity_short="Opus 5"`、无 `dated_alias`)。
    `/v1/models` 目录、`map_model` 路由、`get_context_window_size` 窗口、`requested_model_identity` 身份短名
    **全部从该行自动派生**,无需改动其它位置。
  - `map_model_substring` 的 opus 分支新增 `opus-5`/`opus5` 邻接匹配(置于最前),覆盖未列名异名写法
    (如 `claude-opus-5-0`、`openrouter/claude-opus-5-preview`);窗口兜底同步给 1M。
  - 前端 `RequestLogsPage.contextWindow()` 补 `opus-5`。**顺带修复 sonnet-5 的遗漏**——上次接 sonnet-5
    只改了后端,前端这份硬编码窗口表没跟上,导致 admin UI 里 sonnet-5 的上下文% 一直按 200k 计(实际 1M)。
- 端到端实测通过:`claude-opus-5`(非流式)、`claude-opus-5-thinking`(返回 thinking+text 块)、
  异名 `claude-opus-5-0`(子串兜底),对照 `claude-opus-4-8` 无回归。

### Design Rationale

- **5 系命名与 4.x 不同,不能套用短横转点号规则**:4.x 是对外 `claude-opus-4-8` → 上游 `claude-opus-4.8`,
  而 5 系上游 modelId 就是主版本裸名 `claude-opus-5`(无 `x.y` 点号)——与 2026-07-02 接入的 `claude-sonnet-5`
  同规律。历史上 sonnet-4.8 因猜错上游 id 吃过 `INVALID_MODEL_ID`,故本次上线前以真实账号实测确认。
- **子串兜底必须锚定 `opus-5` 邻接串**,不能用裸 `contains("5")`:否则 `claude-opus-4-5`/`4.5` 会被误吞。
  该分支置于 opus 各版本判断最前,因 opus-5 的写法不含 `4-x`,与下方分支天然互斥(测试已钉住这组边界)。
- **不把 `claude-opus-5-0` 加进公告表**:官方 Opus 5 是无日期快照,id 仅 `claude-opus-5`;公告一个不存在的
  id 会误导客户端(且违反"公告面 ⊆ 可服务面"的既有不变量)。该写法经子串兜底仍可正常路由,鱼与熊掌兼得。
- 计价无需改动:`pricing.rs` 按 `contains("opus")` 归入 OPUS 档(5/25/0.5/6.25),与 Opus 5 官方价恰好一致。

### Notes & Caveats

- **前后端各有一份窗口表**,是既有的结构性重复(后端 `model_map.rs` / 前端 `RequestLogsPage.tsx`)。本次两处都改了,
  但下次加模型仍会踩——sonnet-5 就是漏在前端。值得后续让前端改从 `/v1/models` 的 `context_length` 取值,消除重复。
- **线上代码与本地 git 已严重漂移**:线上为严格超集——独有 `crates/gw-dario` 整个 crate(dario/Claude-OAuth 通道)、
  `websearch.rs`、`tool_repair.rs`、`document_name.rs` 等 14 个文件,另有 53 个文件内容领先。
  ⚠️ **任何从本地 rsync/部署到线上的操作都会抹掉 dario 通道**,回流前必须先把线上漂移入库。
- 回滚:`docker tag claude-all-in-one:rollback-20260725 claude-all-in-one:local && docker tag ... :exp && docker compose up -d --no-build`。


## [kiro-api-key-credential] - 2026-07-20

### Feature —— 支持官方 Kiro API Key(ksk_)作为上游账号凭据

**背景**:此前只支持 social/IdC(均需 refresh_token 运行时换 access_token)。Kiro CLI 2.0 headless
模式引入官方 API Key(`ksk_`,app.kiro.dev 生成),长期有效、无刷新。本版把它接成一类新的上游凭据。

- **鉴权(实测确定)**:apikey 号发包带 `Authorization: Bearer ksk_...` + **`TokenType: API_KEY`** 头。
  该头**服务端强制要求**——缺它则 ksk_ 被当普通 OAuth token,报 400「profileArn is required」;带它则
  免 profileArn,服务端按 key 自身账号解析。实测三个端点(runtime.kiro.dev / q / codewhisperer)均放行,
  **caio 现用的 `runtime.{region}.kiro.dev/generateAssistantResponse` 直接可用**,主链路无需改端点。
- **凭据判定**:统一以「`kiro_api_key` 非空」为准(`machine_id::is_api_key_credential`,提为 pub 复用),
  不只看 `auth_method` 标签(防误配)。bearer 由 `headers::bearer_token` 按类型取真值:apikey→`kiro_api_key`,
  social/IdC→`access_token`(故 apikey 号仅需一个字段,不必镜像 access_token)。
- **旁路 OAuth 专属步骤**:apikey 号 `has_fresh_token` 恒真(不触发刷新)、`refresh_auth` 空操作、
  `discover_profile_arn`/`ensure_profile_arn` 短路(不发 ListAvailableProfiles)、`resolve_profile_arn`
  永不注入 profileArn。订阅档位由后台 getUsageLimits 回填(实测订阅 KIRO POWER,不含 FREE → 放行 opus)。
- **导入 / 配置**:accounts.yaml 支持 `auth_method: api_key + kiro_api_key`;JSON 导入识别 `kiro_api_key`/
  `apiKey` 字段及裸 `ksk_` 字符串;新增 admin `POST /accounts/import-apikeys` + 前端「导入 → 官方 API Key」
  粘贴列表批量建号(复用既有建号/去重/逐号只读验活链路)。

### Design Rationale
- **bearer 按类型取真值,而非镜像 access_token**:apikey 号可能只从 YAML/admin 建号、只带 `kiro_api_key`;
  强制镜像一份密钥既冗余又易漏(YAML seed / admin create 各是一条路径)。单一事实来源 = `kiro_api_key`。
- **`TokenType: API_KEY` 是官方 CLI headless 的真实客户端形态**(非 IDE 形态),属合法官方指纹,且服务端强制。

### Notes & Caveats
- apikey 号收 403 会走一次同号刷新兜底(refresh_auth 对 apikey 空操作),retry 同 key 一次后由调度层换号/上报,
  行为有界、无网络放大。
- 端到端已验证:非流式 / 流式 / opus / 配额(KIRO POWER)/ 批量导入端点均实测通过。


## [ingress-body-limit] - 2026-06-17

### Features

- **放开入站请求体上限(修大图片/PDF 在入口被 413 闷死)**:客户端含大量 base64 图片/PDF 的请求体常 >2MB,
  axum 0.8 的 `Bytes`/`Json` 提取器默认上限仅 **2MB**,超了在 handler 执行前就被框架直接 **413**,
  请求根本到不了业务逻辑——**不入库、后台不可见**(线上实测 `status_code=413, bad response status code 413`)。
  - `SystemConfig` 新增 `max_request_body_bytes`(默认 **16MB**,`0`/缺省回落默认),`effective_max_request_body_bytes()` 取有效值。
  - router(`/v1/messages` 等 + admin 全路由)与 worker(`/v1/messages`)**两处入站咽喉**各挂 `DefaultBodyLimit::max(..)`——
    缺一处仍会在另一侧 413。router 的 layer 挂在 `nest("/admin/api")` **之后**,使大 JSON 导入也一并放开。
  - 两进程启动时各打印生效上限,便于核对配置一致(只重启一侧导致漂移时,日志即现形)。

### Design Rationale

- 取 **16MB** = 出站 6.3MB 护栏(`DEFAULT_MAX_BODY_BYTES`,对齐 Kiro 上游 ~7.3MB 硬限)的 ~2.5×:给当前轮 +
  可被 `shed` 裁掉的历史媒体留余量;放开入口只是让大请求**抵达**内容感知护栏(裁剪/压缩/清晰报错)而非被框架裸 413。
- 用有界 `DefaultBodyLimit::max()` 而非 `disable()`:网关 `:38991` 对外、入口提取在鉴权前完成,无界缓冲是 DoS 面;
  16MB 较旧 2MB 已 8× 决定性解除闷死,同时把缓冲面控制在合理范围,需更大可在 `system.yaml` 显式上调。
- 该值是**启动期参数**(axum `DefaultBodyLimit` app 构建期固定),故意**不进** `SystemSettings` 热调 overlay,
  避免「前端改了不重启不生效」的误导;改动需同时重启 router + worker。

### Notes & Caveats

- **PDF 结构性上限**:文档无法像图像那样压缩(base64 原样透传),且 `shed` 只裁历史不裁当前轮 → 单个原始 >~4.5MB 的
  PDF(base64 ~6.16MB + 脚手架顶破 6.3MB)在当前轮仍会被 `gw-kiro` 本地 `BadRequest`,无法自动补救——需产品侧引导拆分/缩小。
- **待硬化(本次未做)**:router 入口缓冲在鉴权前完成,理想应加 鉴权前置 / Content-Length 预检 / 并发护栏;
  当前以「有界默认 + 可配」缓解。`tool_result` 内嵌图(browser 截图)目前不走压缩,满体量撞 6.3MB 墙,可后续纳入压缩。
- 入站放开**不绕过**出站 6.3MB 护栏:二者是上下游两道独立闸门(入站 Anthropic body vs 出站 Kiro body)。

## [anti-ban-retry-cascade + egress-gateways] - 2026-06-17

### Features

- **重试雪崩止血(防大面积封号)**:`messages()` 重试循环改用 `UpstreamErrorKind::worth_switching_account()`
  + 新增硬上限 `max_switch_attempts`(默认 **2**,热调)。一个失败请求最多波及 2 个号、不再走遍全组。
  - `worth_switching_account()`:`BadRequest / EmptyResponse / TemporarilyBlocked` 一律**不换号**。
  - `error_map`:403 拆分——含封禁标记(`is_account_suspended`,"suspend")→ `TemporarilyBlocked`;否则 → `TokenInvalid`。
  - 新增 `DisabledReason::TemporarilySuspended` + `suspended_cooldown_secs`(默认 **3600=1h**,热调):封禁号较长冷却、面板标 `temporarily_suspended`,不每 5min 重戳。
  - `token.rs` 刷新错误也认 403+suspend → `TemporarilyBlocked`,封禁号不再被永久禁死。
- **出口网关(美国多 IP)+ 上号可选**:
  - `SystemSettings.egress_pool`(每行一个代理 URL = 一个网关)+ 设置页可配;新增 `max_switch_attempts`/`suspended_cooldown_secs` 热调项。
  - 导入/新建账号对话框新增「出口网关」下拉:**直连 / 自动均衡(最少使用) / 指定网关**(`EgressPicker` + body `egress` 字段,按索引解析,密码不经前端)。
  - `POST /admin/api/accounts/rebalance-egress`:把现有号按最少使用回填到网关池。

### Design Rationale

- 根因:旧循环上限=账号总数且从不调 `worth_switching_account()`,封号 403 被错分 `TokenInvalid` → 刷新失败 → 换号把同一(被封内容/高频)请求扩散到健康号 → 雪崩封全池。硬上限 + 内容/封禁类不换号双管齐下,把单请求爆破半径从「全池」降到 2。
- `TemporarilyBlocked` 走冷却自愈而非永久禁用:封禁多为临时,1h 后自愈再试、仍封则再冷却,既不扩散也不每 5min 重戳产生异常指纹。
- 出口选择按**索引**回传而非明文 URL:后端响应已掩码代理密码,前端拿不到真值,选索引由后端解析,杜绝密码经接口往返。

### Notes & Caveats

- `is_account_suspended` 仅匹配 "suspend"(覆盖 `TEMPORARILY_SUSPENDED`);上线后需用真实封禁 403 body 复核标记串。即便标记不中,`max_switch_attempts=2` 仍是兜底。
- Vultr 附加 IP(66/140 段)经实测进出均不通(元数据挂着但 Vultr 未路由),3-IP 需面板 re-attach;当前单主 IP 美国出口可用。

## [tool-repair] - 2026-06-16

### Fix —— 工具参数双重编码防御性修复（tool_repair）

**线上取证**：部分上游模型（Kiro 上的 Opus）偶发把本应是 JSON array 的工具参数序列化成 JSON
**字符串**——1027 次 `AskUserQuestion` 调用中 223 次把 `questions` 编码成字符串，客户端按
`input_schema` 校验拒收：`The parameter questions type is expected as array but provided as string`。
反代逐字透传模型产出的 tool input，本身无 bug，但这是唯一可控点。

- **新增 `crates/gw-kiro/src/tool_repair.rs`**：转换请求时从每个工具 `input_schema` 提取「顶层
  type 为 array/object 的字段名集合」建表（`tool_repair_fields: 工具短名 → 字段集合`，键与上游回显的
  wire 短名同源）；收尾组装 tool input 时，若字段当前为字符串且能解析成 array/object 就解包替换。
- **流式（chat.rs）**：命中字段的工具走「缓冲不发 → `close_open_tool` 在 `content_block_stop` 前
  只发一条 `repair_str` 修复后完整 `input_json_delta`」（无双发）；`finish()` 兜未显式 stop 的截断。
  非修复工具逐帧透传。非流式经同一 BlockTracker 后 `fold_sse_to_message` 折叠，天然覆盖。

### Design Rationale

- 安全边界（绝不破坏正常输入）：仅 schema 声明 array/object 的顶层字段、当前值为字符串、且可解析成
  对应类型时才替换；标量字符串、非 JSON 串、已是数组、半截 JSON 一律原样保留（11 项单测锁定）。
- `array_object_fields` 读**原始** `input_schema`（在 normalize/多模态降级之前），故 schema 降级只改
  发往上游的副本、不影响修复表——按客户端真实期望解包。

### Notes & Caveats

- 缓冲对所有含顶层 array/object 字段的工具生效（input 不再增量流式、整段缓冲到 stop；协议正确性无损）。
- 仅解顶层一层；nullable/$ref/anyOf 包裹的 array、嵌套双编码不识别（漏修不误伤）。
- 既有缺陷（与本修复无关、不受影响）：响应侧未消费 tool_name_map，名长 >63B 的工具向客户端泄露短哈希名。

## [agent-continuation-cache-ab] - 2026-06-15

### Fix(实验开关) —— 恢复稳定 agentContinuationId,修复真实缓存全 miss

**背景/根因(线上取证)**:caio 真实 Kiro 前缀缓存命中 **0%**(1985/1985 success),credit **+49% vs
kiro.rs**(opus-4-8 1.37 vs 0.92/req);而 kiro.rs 同上游同期 **~43%** 命中。逐层对比两套源码:caio
发的 wire 看似完美可缓存(稳定 conversationId、history 前缀逐字节相同、reminder 已剥),唯一差异是
2026-06-13 caio **删掉了** `agentContinuationId` + `agentTaskType="vibe"`,理由"kiro.rs/static_flow
都不发"——经核对**证伪**:kiro.rs 生产一直在发稳定值(其代码实测注释:稳定 → metering 降 ~36%),
caio 自身 2026-05-24 命中 A/B 基线也含此字段(6-13 才删)。删除疑为当时 reminder-leak bug 混淆的误判。

- **恢复** `derive_agent_continuation_id`(= `SHA256("agent-continuation:"+conversationId)` 前 16 字节
  排 UUID,**逐字节对齐 kiro.rs**;golden-vector `621628ff-…` 锁死)。
- **新实验开关 `agent_continuation`**(默认**关**):走与 tools_in_prefix/cache_point 同款 RwLock 热控
  (config.rs→cache_point.rs→apply_hot_settings),可经**设置面板/API 热翻**——生产做**可逆 A/B**:
  默认关 = 部署零 wire 变化(`Option` 字段 `skip_serializing_if` 完全省略),开启 = 复刻 kiro.rs proven
  配置(稳定 conversationId + 稳定 agentContinuationId + vibe)。
- 附挂逻辑抽成纯函数 `with_agent_continuation_metadata(state, enabled)`,便于直接测两分支。
- 前端设置面板加该开关 + 中英文案(指向请求日志「真」命中列)。
- **设计取舍**:做成默认关的可逆开关(而非直接改默认)——历史测量多次被混淆,用真实流量 A/B 定论;
  必须配稳定 conversationId(本 crate 已保证),否则每轮新值反而 miss(kiro.rs 实测)。
- **对抗审查(Codex×2)**:修 4 项 medium——补 enabled-path 测试(纯函数双分支 + 序列化边界双向
  断言:关→JSON 省略两 key、开→含两 key 且 = golden vector)、golden-vector 锁死、OnceLock→RwLock
  热控(消除"重启才能翻、不可热回滚")。1 项 high 为误报(glob import,编译已过)。
- **测试**:gw-kiro 357 + workspace 560 全绿;admin-ui tsc+build 通过;clippy 净。

## [request-log-response + modal-layout] - 2026-06-14

### Feature —— 请求日志新增「模型回复」保存 + 详情弹窗布局修复

**背景**:请求日志已存「用户原始报文 / 发 Kiro 前报文」,但缺模型的实际回复,不便复盘"问了什么→
答了什么"。同时详情弹窗内容过长会撑出浏览器可视范围。

- **后端·模型回复入库**:`request_logs` 新增 `response_payload` 列(gzip 入库,旧库 `ensure_column`
  热升级回填空串)。回复 = 把本次响应折叠成的单条 Anthropic Messages JSON(与下发客户端同形):
  - 非流式:**复用**已折叠、即将下发客户端的同一份响应体(`ResponseLog::Folded`),**不二次折叠**——
    入库与客户端实收严格一致;
  - 流式:转发期间按**累计字节**预算(`RESPONSE_LOG_MAX_BYTES`,与入库截断同量级)采集 SSE 事件
    (`ResponseLog::Events`),收尾在 blocking 任务里折叠。复用转发时的 `data.to_string()` 既当字节
    度量又喂下游,避免二次序列化;
  - 折叠/序列化全在 detach 的 blocking 线程池,**绝不碰收入热路径**;失败请求/无 message_start → 空串
    (详情页不展示该区块)。
- **前端·展示**:详情弹窗第三块「模型回复」,沿用 PayloadView 的 formatted/raw 双视图(新增
  `parseAnthropicResponse` 识别单条 Messages 响应)+ 下载按钮;`response_payload` 为空则隐藏。
- **前端·弹窗布局**:共享 `Modal` 卡片限高 `max-h-[88vh]`、标题栏固定、内容区超出自身滚动,
  彻底解决长报文撑出视口。
- **设计取舍 / 对抗审查(Codex×3,1 high + 数 medium 已修)**:
  - 流式封顶从"按条数"改为**按字节**——单个超大 `input_json_delta` 也挡得住(原 high 共识);
  - 回复序列化超限时存**合法**占位 JSON(`_truncated:true`)而非半截非法 JSON,保住 formatted 渲染;
  - 非流式去掉二次折叠,消除入库与客户端响应分歧风险;`ResponseLog` 具名枚举替代裸 `Vec<SseEvent>`
    参数;弹窗滚动条改 `pr-1` 不用负 margin,避免其它弹窗水平溢出/焦点环裁切。
- **测试**:新增 `serialize_response_capped` 2 个(常规直通 / 超限合法占位)+ store 往返断言;
  workspace 全绿(557 测试)、admin-ui tsc+build 通过。

## [thinking-effort-validation] - 2026-06-14

### Feature —— thinking effort 白名单校验 + 非法值回退告警

**背景**:Kiro desktop/CLI 通过 `output_config.effort`(`low/medium/high/xhigh`)控制思考强度,
caio 全程支持客户端传入(`thinking_policy` 客户端值优先,`generate_thinking_prefix` 注入
`<thinking_effort>` 到上游 wire,缺省 xhigh)。但此前 effort 是**裸字符串透传**,无任何校验——
客户端传个 Kiro 不认的串会原样打到上游,可能触发 400,且存在往 wire 前缀注标签的隐患。

- **新增** `anthropic_types::VALID_EFFORTS`(`["low","medium","high","xhigh"]`)+ `normalize_effort()`:
  归一到白名单(大小写不敏感、去前后空白),命中→标准小写形态;非空但非法→回退 `xhigh` 并返回
  `fell_back=true`;`None`/空白→默认 `xhigh`(不算非法、不告警)。`effective_effort()` 改走它。
- **单一 wire 出口做归一 + 告警**:`thinking_policy` 改为保留客户端**原始** effort 串(不预归一),
  合法化与 `tracing::warn!` 统一在 `generate_thinking_prefix`(`<thinking_effort>` 唯一注入点)做,
  避免双重归一/双重告警。顺带堵死往 effort 标签注入尖括号的口子。
- **设计取舍**:白名单为代码常量(非 YAML 热编辑);非法值默认回退最高档 xhigh(对 Opus 无副作用),
  而非 400 拒绝——宁可降级也不打断请求。客户端要降档须传合法档位或用 `thinking.budget_tokens`。
- **测试**:新增 11 个(`normalize_effort` 6 + `generate_thinking_prefix` 端到端 3 + 既有 effort 2);
  gw-kiro 全套 354 passed。已部署 139(router health 200,启动无 panic)。

## [strip-ephemeral-reminders-history] - 2026-06-14

### Fix —— 剥离历史里的 Claude Code 临时提醒块(降 Kiro 积分,缓存命中)

**根因(实测坐实)**:Kiro 按真实 prefix-cache miss 扣积分。用请求日志的完整 kiro_payload
对同一 conversationId 的连续两轮做 diff,发现 Claude Code 把 `<system-reminder>` /
`<internal_reminder>` 注入 user 轮,再随对话推进**从旧轮里删掉**,导致同一条历史消息跨轮
字节抖动 → Kiro 前缀缓存从该轮起全 miss。实测 caio opus-4-8 烧 **1.28 积分/次** vs kiro.rs
**0.90**(+42%),就是这么来的(总上下文 caio 142k < kiro.rs 162k,纯粹命中差)。

- **修复**:在 `converter/history.rs::merge_user_messages`(**只走历史**)调
  `strip_ephemeral_blocks`,剥掉 `<system-reminder>`/`<internal_reminder>` 块,让历史前缀
  跨轮字节恒定。当前轮经另一路径 `merge_current_message_content`,**保留提醒**(模型本轮仍看得到)。
  这是既有「历史 thinking 丢弃」(`convert_assistant_message`)的直接同构。
- **两处漂移源**(首版只修了第一处,线上 log 2099 验证时抓到第二处):① 主 user 文本;
  ② **tool_result 内容**(`toolResults[].content[].text`——CC 把 workflow 提醒追加在工具输出后)。
  第二处经 `strip_ephemeral_from_tool_results` 就地剥(同样只走历史)。长会话(工具密集)主要靠这处。
- 纯手写扫描(无 regex 依赖):边界校验防 `<system-reminderish>` 误匹配、无闭合标签不吞正文、
  确定性 + 幂等;仅当确有剥离才 trim 首尾(无提醒的历史**零改动**,不引入 cold-start)。
- `KIRO_STRIP_HISTORY_TAGS`(逗号分隔)可追加更多漂移标签。conversationId/亲和键不受影响
  (从原始 messages 派生)。
- 数据驱动确认:census 里 `<available_skills>`/`<env>`/`<skill>` 等高频标签**跨轮稳定、不漂移**,
  故**不剥**(删了丢上下文、零收益)。唯一漂移真凶 = 那两个 reminder。

### Notes & Caveats

- 中段提醒只能尽力对齐(实测提醒都在轮尾,主场景已覆盖)。
- 预期:opus-4-8 积分/次从 1.28 往 0.90 甚至更低降;上线后用 prefix-diff(应变 verbatim)+
  metering_credit/次 下降验证。这套修复也可回灌 kiro.rs(它同样没剥)。

## [account-negative-remaining] - 2026-06-14

### Fix —— 账号剩余积分允许负值(显示超额了多少)

Kiro 账号允许超额跑,但原先 `remaining = (limit - used).max(0)` 把超额夹成 0,
看不出超了多少。去掉两处 clamp(后端 `AccountQuota::from_used_limit` 的 `.max(0.0)`
+ 前端 `formatCredits` 的 `Math.max(n,0)`)→ `remaining = limit - used`,可为负。

- 超额账号在账号表显示如 `-93 / 1,000`(实测 used 1092.87 / limit 1000 → remaining -92.87)。
- `isQuotaLow` 加 `remaining < 0 → true`:超额一定标红,即使上限未知也不漏标。
- 不影响调度:账号停用由 `QuotaExhausted` 错误驱动,不读此快照(对抗审查确认)。
- 已部署 139。

## [usage-cost-stats-panel] - 2026-06-14

### Feature —— 积分与成本统计面板(按 Anthropic 标准价折算 · 计费/真实双口径)

`/usage` 页原本只有 token 用量 + 时间筛选,缺积分与 USD 成本。补齐成"累计用量与成本"卡:
总花费 / 总请求 / 总积分消耗 / 每积分成本 + 输入·输出·缓存读 token + 每积分 in/out,
按模型表加 积分 / 每积分in / 花费 列。沿用既有时间筛选(7d/30d/all + 自定义区间 + key)。

- **存储**:`usage_records` 加 `metering_credit`(Kiro 真实积分)+ `real_cache_read_tokens`
  (金标准缓存命中)两列;`ensure_column` 热迁移存量库(历史行回填 0,**不可补**)。
  `record()` 落库带上两值;三个聚合查询 SUM 出来。负 credit 在插入处 `max(0)`,聚合再用
  `SUM(CASE WHEN metering_credit>0 …)` 兜底(单条坏行不污染分母)。
- **计价**:新 `gw-core/pricing.rs`——Anthropic 标准价(opus 5/25/0.5/6.25、sonnet 3/15/0.3/3.75、
  haiku 1/5/0.1/1.25,USD/1M),子串分档。`admin/usage.rs::model_cost` **按模型分别计价后求和**
  (不同模型不同价,不混算)。
- **关键口径**:caio 存的 `input_tokens` 是**总上下文**(含 cache_read 子集),故折算 USD 前
  必须 `input - cache_read` 得未命中输入再计价(否则缓存 token 既按 input 全价又按命中价**重复计费**);
  前端展示同样扣除,与 kiro.rs 成本面板口径一致。
- **双口径**:计费口径用上报 `cache_read`,真实口径用 `real_cache_read`(历史多为 0 → 全价估算,
  卡片加脚注说明真实成本为上限)。

### Design Rationale

- 单一权威成本算在 admin 层而非 store 层:store 只忠实记录,计价/折算是展示策略,放近端点便于改价。
- 子串分档(非精确日期名匹配):同族同价,`claude-opus-4-5-20251101` 与 4.5 同价,够用即可
  (对齐 kiro.rs,符合"计费分类器够用就行")。

### Notes & Caveats

- **历史无积分**:`metering_credit`/`real_cache_read` 是新列,部署前的行恒 0,回填不了 →
  积分/每积分成本/真实口径缓存只对**部署后**新流量有数;USD 花费用历史就有的 token 数算,历史也准。
- `by_key` 暂不出 USD 成本(需按 key×model 分组才准确;面板当前不需要)。已知保留。
- summary 的 token 总数与成本来自两条独立查询(usage_summary + usage_by_model),活跃写入时
  可能差一个亚秒快照——只读统计面板,刷新自愈,非计费权威路径。已知限制。
- 对抗审查(codex×3):核心已验正确(双口径减法、迁移竞态、除零守卫);采纳 3 项廉价加固
  (pricing 文档纠正、负 credit 聚合兜底、真实口径脚注);Minimalist"砍掉积分/真实/每积分列"
  与用户明确指定的目标面板冲突,**驳回**。

## [request-logs-gzip-fulltext] - 2026-06-13

### Fix —— 报文不再截断:gzip 全文存储 + 下载完整报文

**问题**:报文文本按 512KiB 截断,大报文(实测 534KB roleplay)被截尾 → JSON 非法 →
详情页既不能格式化(回退原始)也不完整。任何固定上限迟早截到长对话。

- **gzip 全文压缩存储**(`gw-store`):`client_payload`/`kiro_payload` 入库前 `gzip_text`
  压成 BLOB(文本压 5-10 倍),读时 `value_to_payload` 按动态 `Value` 还原——`Blob`→解压、
  `Text`→旧明文行原样(**无缝兼容本特性前的明文行**,无需迁移)。gzip 失败退原文字节、
  解压失败 lossy,绝不 panic。文本截断上限 512KiB→**16MiB**(纯防御;Kiro 报文硬上限 ~6.3MB,
  真实报文绝不触顶,全文无损)。
- **前端「下载完整报文」按钮**:详情弹窗两份报文各加下载(Blob→`a.download`),超大报文不便
  在弹窗滚动时直接存盘。

### Notes & Caveats

- 旧明文行与新 gzip 行共存:读侧按 SQLite 存储类(Blob/Text)分流,`r.get::<_,Value>` 兼容两者
  (rusqlite `Vec<u8>` FromSql 只认 BLOB,故不能直接读旧 TEXT 行——必须走 Value)。
- 解压无外部输入面:BLOB 只由我方 `gzip_text` 产出,旧行是明文,无解压炸弹风险。
- 格式化视图单 text block >20000 字符仍转纯文本 `<pre>`(防大正文卡死),与本次叠加。

## [request-logs-pagination] - 2026-06-13

### Feature —— 请求日志列表分页

原本一次最多返回 200/2000 条(`total=754` 实测),前端一次渲染太多。加基本分页:

- 后端 `RequestLogFilter` 加 `offset`;`list_request_logs` 加 `OFFSET`;新增 `count_request_logs`(同筛选总数,不受 limit/offset 影响)。`GET /logs` 改返回信封 `{items,total,page,page_size}`;`LogsQuery` 加 `page`/`page_size`(默认 50,钳 [1,2000],兼容旧 `limit`),`offset=(page-1)*page_size`(`saturating_mul` 防溢出)。
- 前端列表消费信封,加上一页/下一页 + 「共 N 条 · 第 X/Y 页」;筛选变化回第一页(`keepPreviousData` 翻页不闪)。`i18n t()` 支持可选 `{name}` 占位参数(向后兼容,原无参调用不变)。

## [request-logs-media-dedup] - 2026-06-13

### Feature —— 请求日志保存图片/文档(去重)+ 前端格式化/Markdown 视图

调试需要看到客户**实际上传的图片/文档**,但同一张图在一个会话里每轮都发,直接存会撑爆库。
方案:**内容寻址去重**存储 + 前端内联渲染。

- **媒体 blob 去重存储**(`gw-core` + `gw-store`):新增 `extract_log_blobs`——按字段把
  Anthropic `image/document.source.data`(需同级 `media_type`/`type:base64`)、Kiro `source.bytes`、
  `images[].source.bytes` 的 base64 抽到 `log_blobs` 表(主键 `hash=sha256(base64)`,`INSERT OR IGNORE`
  自动去重),报文里只留 `blob:<hash>` 短引用。对话正文走 `text` 键、**绝不误抽**(修正旧版按
  字符集猜测会误伤无空格长正文的问题)。`log_blob_refs(log_id,hash)` 记多对多引用。
- **环形 GC**(`insert_request_log` 改事务):日志裁到最新 2000 条时,连带删被裁日志的 blob 引用,
  再清掉已无任何引用的 `log_blobs`(防无限膨胀)。详情按 `log_id` JOIN 取回该条引用的 blob。
- **前端格式化视图**(`PayloadView.tsx`):详情弹窗加「格式化/原始」切换。格式化把 Anthropic/Kiro
  两种报文解析成对话(system/user/assistant + text/thinking/tool_use/tool_result/media 块),正文用
  `react-markdown`+`remark-gfm` 渲染;**图片内联**(data URI)、**文档给下载链接**;解析失败回退原始 JSON。
- 报文**文本**上限从 64KiB 提到 512KiB(媒体已抽走,只剩文本 + blob 引用,≈13 万 token 覆盖绝大多数对话)。

### Notes & Caveats

- 安全:`react-markdown` 默认不渲染原始 HTML、拦 `javascript:` 协议;Markdown 远程图片不加载(占位)。
  blob 媒体的 `media_type` 是**客户可控**值,故 data URI 走**白名单**:仅 png/jpeg/gif/webp/avif/bmp 内联
  渲染、application/pdf 可下载;其余(text/html、svg 等)只占位不生成 data URI——杜绝
  `data:text/html` 下载链接被中键/Ctrl 点开新标签执行任意 JS(对抗审查 high 修复)。
- 健壮性:`PayloadView` 解析器整体 `try/catch` + 数组字段 `Array.isArray` 守卫;单个文本块 >20000 字符
  不跑 Markdown 改纯文本(防不可信超大正文卡死详情页主线程)。
- 详情响应会内联该条日志引用的 blob 完整 base64;一条引用多张大图时响应偏大(debug 单条查看,可接受)。
- 库膨胀:512KiB×2×2000 文本 + 去重后 blob 表;`DELETE` 不自动缩文件(SQLite 不 VACUUM),空间复用但
  文件不缩。去重 + GC 保证不无限涨。

## [wire-parity-isError-multimodal] - 2026-06-13

### Fix —— 两处发往 Kiro 报文与 static_flow 金标准对齐

离线把 caio 完整 wire 与 static_flow/kiro.rs 逐字段对比(`[cache-fix-no-agent-continuation]` 后已无大缓存差异),修两处剩余 parity/正确性差异:

- **`ToolResult.is_error` 永不上 wire**(`kiro_types/tool.rs`):原在出错时发 `"isError":true`,改为 `#[serde(skip_serializing)]` 永不发——真 Kiro 客户端/static_flow 都不发,错误信号只走 `status:"error"`(caio `ToolResult::error()` 与 content.rs 已设 status,删序列化安全)。去掉真客户端没有的字段,收窄检测面、对齐金标准报文。
- **多模态 schema 兼容**(`converter/tools.rs` + `mod.rs`):新增 `apply_multimodal_tool_schema_compatibility`——请求(当前轮或历史)含图片时,把带 `anyOf/oneOf/allOf/$defs` 等 10 个关键字的工具 schema 整体降级为宽松 object schema,避免 Kiro 对"多模态 + 复杂 schema"返回 400 Improperly formed request。逐字搬 static_flow `converter/schema.rs`,在工具放置前 hook(current/history 放置都覆盖)。

### Notes & Caveats

- 两项均**非缓存**改动:`[cache-fix-no-agent-continuation]` 已把真号真实成本降到 kiro.rs 水平(0.0106→0.0100/1k);本次是报文合法性/抗检测对齐。`真=miss` 显示是 Kiro 对 caio 这批号不填 cacheReadInputTokens 的报告口径,与本次无关。
- 多模态降级**仅在含图片时**触发,且只替换真正含不支持关键字的 schema;纯文本请求工具定义零改动(有测试锁双向)。降级会丢失该工具参数约束,是 static_flow 接受的取舍(宁可宽松也别 400)。

## [cache-fix-no-agent-continuation] - 2026-06-13

### Fix —— 真实缓存全 miss 根因:停发自造的 agentContinuationId

**症状**:线上 caio 每条请求真实命中 `真=miss`(Kiro `tokenUsageEvent.cacheReadInputTokens=0`),
同一对话连续轮也全冷,真号真实额度暴烧;同期 kiro.rs 同类负载真实命中 20~37%。

**根因**:caio converter 把自造的稳定 `agentContinuationId`(+ body 里的 `agentTaskType="vibe"`)
写进发往 Kiro 的 ConversationState。Kiro 收到 `agentContinuationId` 会把请求当成「续接一个它从未
签发的 agent 上下文」,**绕过 conversationId 前缀缓存** → 每轮冷 miss。conversationId/亲和/前缀
其实都正常(模拟器都能检测到前缀连续),唯独这个多余字段毒掉了真实命中。

**修复**(`crates/gw-kiro/src/converter/mod.rs`):ConversationState 只发 `chat_trigger_type="MANUAL"`,
**不再发 `agentContinuationId` / `agentTaskType`**——逐字对齐 static_flow 金标准(其测试
`convert_request_does_not_send_random_agent_continuation_metadata_by_default` 断言这俩为 None)
与 kiro.rs 生产(二者均不发)。删除无用的 `derive_agent_continuation_id`,补回归测试
`convert_request_does_not_send_agent_continuation_metadata` 锁死 wire 形状。

**纠正旧误判**:此前注释/记忆称"agentContinuationId 必须稳定否则 3 倍计费"——那个实验对比的是
「稳定发」vs「随机发」两个都错的配置;正解是**根本不发**。本修复正是用刚上线的"真/credit"
请求日志(`[request-logs-real-credit]`)抓到 `真=miss` 才定位的。

## [request-logs-real-credit] - 2026-06-13

### Features —— 请求日志增加「真(真实命中)/ 报(上报缓存)/ credit(Kiro 原生计费)」

让每条请求都能看到**真号真实消耗**信号,供按真实缓存命中持续优化(对齐 kiro.rs 的 真/报/credit 展示):

- **真** = 上游 `tokenUsageEvent.cacheReadInputTokens`——真号在 Kiro 服务端的**真实** prefix cache
  命中(决定真号真实额度消耗速度);**报** = 既有 `cache_read_tokens`(经 floor/cap 模拟后上报
  NewAPI 的计费口径);**credit** = 上游 `meteringEvent.usage`(Kiro 原生计费,真号本次真实积分消耗)。
- **捕获(`gw-kiro/chat.rs`)**:此前 `tokenUsageEvent.cacheReadInputTokens` 算进 report_total 即
  丢弃、`meteringEvent` 整个吞掉;现各留一份。**仅记录,绝不参与上报计费**(`reported_cache_read`/
  `build_usage_json` 逻辑零改动)。热路径仅新增 meteringEvent 一次小 JSON parse(流末尾、不阻塞 TTFB)。
- **透传**:`ChatUsage` +`real_cache_read_tokens`/`metering_credit`(去 `Eq`,因含 f64);
  `RequestLog`/`RequestLogRow` 同步加列;`worker/mod.rs::write_request_log` 透传。
- **store**:`request_logs` 加 `real_cache_read_tokens INTEGER` + `metering_credit REAL` 两列;
  存量库经 `ensure_column` 热迁移(并发 open 竞态下 duplicate-column 视为成功,审查 Skeptic#1)。
- **前端**:请求日志表把单列"缓存读"拆为 **真 / 报 / credit** 三列(真带命中%、credit 4 位小数、
  input 下显示上下文%),详情弹窗补 6 格;contextWindow 归一点号/连字符模型名(审查 Architect#1)。

### Design Rationale

- 两个新值是**已被上游下发、原本丢弃**的数据,捕获近乎零成本;刻意只新增透传字段、不动
  `reported_cache_read`/`build_usage_json`,与"报"(上报计费)严格隔离——优化看"真/credit",计费看"报"。
- credit 直接取 Kiro `meteringEvent.usage`(金标准 metering),不自建价格表;判断 Kiro 服务端有没有
  应用缓存折扣的权威信号。

### Notes & Caveats

- `real_cache_read_tokens=0` → 前端显示 "miss"(无 tokenUsageEvent 真值或真实零命中);
  `metering_credit=0` → 显示 "—"(无 meteringEvent 信号)。非 Kiro provider(subprocess)二者恒 0。
- 上下文% 前端按 model→窗口估算(opus 4.6/4.7/4.8、sonnet 4.6 = 1M,其余 200k);写库的 `input_tokens`
  为 report_total(含缓存总量),故真实命中% = 真 / input_tokens。

## [request-logs] - 2026-06-13

### Features —— 请求日志(调试用:每次 chat 落一条,含完整报文)

补全此前只建了 store 层地基的"全报文请求日志":worker 在每次 chat 收尾环形落库最新 2000 条
(账号/模型/流式/成功失败/状态码/错误类型/耗时/TTFB/input·output·cache_read token + 用户原始
报文 client_payload + 发 Kiro 前报文 kiro_payload),admin 暴露列表(按成功/失败/账号/模型/时间
窗筛选)+ 详情(含两份报文),前端新增"请求日志"页(筛选 Segment + 表格 + 详情弹窗看报文)。

- **捕获(`worker/mod.rs`)**:挂在 #130 已验证的 `StreamCtx::Drop`(流式)与 `collect_response`
  (非流式)收尾;**绝不碰收入热路径**——两份报文序列化 + `kiro_payload` 重渲染 + 同步 SQLite 写
  全部 detach 到 **blocking 线程池**(`spawn_blocking`),handler 入口不再同步序列化 client_payload。
  失败请求也记:首包前终态失败(上游 400 / 重试耗尽)在 chat-error 分支补落一条,故"失败"筛选
  能看到生产 400 类失败。
- **渲染助手(`gw-kiro/chat.rs::render_kiro_payload`)**:纯函数复刻转换 + profileArn 注入,
  不跑 cache_sim、不发送、无副作用,在 blocking 任务里重渲染"发 Kiro 前报文"。
- **store**:复用既有 `request_logs` 表 + `insert_request_log(cap)`(环形)/ `list_request_logs`
  / `get_request_log`;每份报文入库前按 UTF-8 边界截断到 64KiB(最坏 2000×2×64KiB≈256MiB)。
- **admin**:`GET /admin/api/logs`(筛选,limit 钳 ≤2000)+ `GET /admin/api/logs/{id}`(详情)。
- **前端**:`pages/RequestLogsPage.tsx` + `features/logs/*` + 侧栏导航 + i18n(zh/en)。

### Design Rationale

- 复用 #130 的 `StreamCtx::Drop` + `pending_writes` 排空机制,捕获与 usage 落库同生命周期;
  请求日志走 `spawn_blocking`(序列化/写库是阻塞工作),usage 走 `spawn`(sink.record 是 async)。
- 不扩 `ChatUsage`/`StreamItem`/chat 发送链路,故 credit、真实缓存命中(meteringEvent)本期不抓
  (schema 无此列);避免改每请求必过的热路径核心类型。

### Notes & Caveats

- **对抗审查(Codex)修复**:client_payload 移出热路径(blocking 任务内序列化)、render+写库改
  `spawn_blocking`(不占 async worker 线程)、非流式日志改 detach(不持 lease 不延迟响应)、
  补失败请求落库、payload cap 256→64KiB、limit 钳制、上游错误回显 400/502 状态码。
- **未抓**:token 刷新类终态失败(B/C/D 站点)不落库——经账号状态可见,本期略;
  `kiro_payload` 展示的是转换产物,不含体积护栏剔除媒体后的瘦身版(护栏属另一关注点)。
- 部署即生效;capture 仅在 chat 流量上触发,上线时 caio 空闲故 `request_logs` 暂 0 行,
  下一笔真实流量即落库(受"真号禁聊天验证"约束未自造流量验证写路径,端点/鉴权/前端已验)。

## [admin-ui-restyle] - 2026-06-13

### Features —— admin 前端整体换肤（玻璃拟态 → playmate 设计语言）

把 admin 控制面从原"玻璃拟态（glassmorphism）"全面改为 playmate-platform 的设计语言，
统一品牌观感。**纯视觉重构：零业务逻辑/API 契约/i18n key 改动**，所有 hooks、props、
表单校验、两阶段验活、脱敏哨兵、轮询、proxy 三态语义原样保留。

- **设计 token 重写**（`styles/globals.css`）：调色板换成 暖米白纸面（浅色 `#f3f4ef`）/ ink 近黑画布（深色 `#050505`），
  唯一强调色 acid 荧光黄绿 `#d7ff2f`；圆角整体放大（卡片 28px / 容器 32px / 按钮·标签 pill）；
  字体引入压缩超粗大标题（`font-display` + `font-black` + 负字距）+ `.eyebrow` 章节小帽。
  旧玻璃工具类名（`glass-card*` / `gradient-bg-primary` / `page-hero` 等）**保留为主题 API**，
  底层重定义成纸面风格，避免逐处改类名。
- **原子组件换肤**（`components/ui/*`）：Button（黑 pill，hover 翻 acid）/ Card（实色白卡）/
  Badge / Segment（acid 激活药丸）/ Table / Modal（白圆角大卡）/ Select / StatCard / ErrorNote。
- **布局**：侧边栏恒为 ink 黑 + acid 激活块 + CAIO wordmark（`Sidebar`/`AppShell`）；
  登录页为暗色品牌时刻（双径向渐变 + 噪点 + acid CTA）。
- **页面**：6 个页面页头统一为无框纯排版（eyebrow + 压缩大标题）；usage/accounts/keys/groups/settings
  各 feature 组件随原子组件换肤。

### Design Rationale

- 对标 playmate-platform（Next.js + Tailwind）的视觉，但落地仍用本项目既有栈（React 19 + Tailwind v4 + Vite），
  不引入新框架/新依赖，构建与 embed-ui 部署链路零改动。
- 沿用旧玻璃类名作为"主题 API 层"重定义，使 feature 组件改动面最小、回归风险最低。

### Notes & Caveats

- 本次仅改 `admin-ui/` 前端样式文件 + 本 CHANGELOG；未触碰任何 Rust 代码。
- 全量 `bun run build`（tsc + vite）通过；Playwright 实测登录/看板（明+暗）/账号/设置/对话框五处布局无崩溃。
- 工作区另有 5 个文件（`accounts.rs`/`accounts/api.ts`/`hooks.ts`/`settings/types.ts`/`i18n.tsx`）为**更早会话遗留的未提交功能改动**，与本次无关、未触碰。

## [body-guard-poison-memo] - 2026-06-13

### Features —— 出站体积护栏 + 毒报文备忘录（从 kiro.rs v63 移植）

背景:kiro.rs 生产事故——客户会话史带大 PDF/图,序列化后超 Kiro 报文体积硬上限
（实测 ≤6,341,854 字节成功、7,336,893 字节确定性 400 "Improperly formed request"），
整个会话每轮必死。caio 此前**无任何体积防护**,同样场景会复现。

移植 kiro.rs 四联修中 caio 缺失的两项（Fix A 上游 400 透传、Fix B 全 kind 冷却 caio 早已具备，未搬）：

- **出站体积护栏**（`converter/shed.rs` + `chat.rs`）：序列化后若超
  `max_upstream_body_bytes`（默认 6,300,000，留 4096 headroom 吸收 profileArn 注入），
  先从 history **最老媒体按附件粒度**剔除（文档优先，达标即停，追加中性占位
  "[attachment omitted due to size limits]"），**绝不动 currentMessage**；剔后重序列化仍超限、
  或无历史媒体可剔 → 本地 `BadRequest`（→客户端 400，不发上游、不惩罚账号）。
  会话从"超限即毒化"变"自动瘦身续命"。
- **毒报文备忘录**（`poison_memo.rs`）：上游确定性 400（`BadRequest` kind）的请求体
  SHA-256 指纹记入 TTL 600s / 容量 512 的备忘录；同字节级 payload 再来本地 400 拦截，
  兜住无视 400 仍重试的客户端（本次事故 NewAPI 上游即此类）。指纹由 chat 路径持有
  （发包前算一次），避免对 MB 级 body clone；锁中毒 into_inner 恢复，不 panic 主路径。
- **配置**：`KiroProvider.max_body_bytes`（`from_config` 读 `max_upstream_body_bytes` /
  `apply_hot_settings` 30s 热调，0 视为非法忽略）。

### Design Rationale

- caio 的 `classify_chat_error` 已正确把上游 400 分到 `BadRequest`→客户端 400，
  `report_failure` 已全 kind 覆盖冷却，故 kiro.rs 的 Fix A/B 无需移植；只补缺失的 C/D。
- 媒体结构（`KiroImage`/`KiroDocument`/`history`）与 kiro.rs 逐字段一致，`shed_history_media`
  近乎原样移植，仅改 import 路径。
- 护栏插在 `chat.rs` 序列化点之后（唯一接缝），超限返回 `UpstreamError::bad_request`
  复用既有 `BadRequest`→客户端 400 路径，语义正确且 `BadRequest` 天然不惩罚账号。

### 对抗审查修复（Codex Skeptic/Architect/Minimalist 三视角）

- **砍掉死配置管线**（三人共识 medium）：`from_config`/`apply_hot_settings` 读
  `max_upstream_body_bytes` 是死代码——caio 的 `SystemSettings` 没有该字段且
  `deny_unknown_fields`，worker 构造 provider cfg 时也不塞，永远读不到。改为固定常量
  `DEFAULT_MAX_BODY_BYTES`（provider 普通 `usize` 字段，测试用 `with_max_body_bytes`
  注入小值）。热调留作将来配 admin 面板时一并设计正确的 SystemConfig 归属节。
  *附带收益*：砍掉 config 读取后用户传不进过小值，Skeptic 的下限越界风险一并消失。
- **去掉 `BODY_LIMIT_HEADROOM`**（Architect#4）：caio 的 profileArn 在序列化前就注入，
  发包路径只加 header 不改 body，序列化的 body 即最终出站字节，kiro.rs 的 headroom 前提
  在 caio 不成立，留着只是白降 4KB。
- **cache_sim 移到护栏之后**（Architect#2）：否则剔除历史媒体后仍按未剔除口径观测，会
  高估 input/cache_read tokens **落库计费**（不只显示），且把从未发出的前缀写进模拟缓存
  污染后续轮。现观测的是真正发出的 conversation_state。
- **收紧毒备忘录 remember 条件**（Minimalist#1）：只记 body 含 "Improperly formed" 的
  格式/体积类 400，不记所有 `BadRequest`——后者可能含账号/profile 相关 400，换号或修
  profile 后同 body 本可成功，记毒会跨账号误伤 600s。对齐 kiro.rs v63 短语门。
- **护栏抽成 `enforce_body_limit` 独立函数**：与 kiro.rs `enforce_upstream_body_limit`
  对称，补上 3 个单测（透传/剔除/无媒体可剔→BadRequest），不再只靠 HTTP 路径覆盖。

### Notes & Caveats

- 体积上限 6.3MB 固定为常量（真实上限在 (6.34, 7.34]MB 区间）；暂不经 admin 热调，
  需调整改 `DEFAULT_MAX_BODY_BYTES` 重新构建，或后续接入 SystemSettings。
- 毒备忘录每请求算一次 SHA-256（MB 级 body 几 ms），与 kiro.rs 同款取舍：账号保护优先。
- **毒备忘录是单 worker 进程级**（Architect#3，已知限制不修）：caio router+多 worker 进程
  模型下，全局 static 不跨进程共享；但会话亲和已把同会话钉到同 worker，且备忘录是
  纵深第二道（第一道 400 透传已让正常客户端不重试），跨进程共享属过度工程。各 worker
  各自记毒后仍收敛。

## [model-catalog-unified-table] - 2026-06-13

### Features
- **`/v1/models` 公告全量模型名(thinking 变体 + 日期快照)**:原只列 6 个裸名,NewAPI 渠道按日期名(`claude-sonnet-4-5-20250929`)/thinking 名(`claude-opus-4-8-thinking`)选型时拉不到。现从权威表生成 **20 个对外 id**——每个模型展开 plain / `-thinking` / 日期快照 / 日期`-thinking`(4.5系与 haiku 各 4 个,其余各 2 个)。这些名字此前**发 chat 其实已能路由**(map_model 子串匹配 + thinking_policy 处理后缀),本次只是把它们如实公告出来。
- **单一权威表 `KIRO_MODELS`**(`gw-kiro/src/converter/model_map.rs`):对外裸名 ↔ 上游 Kiro 模型 ↔ 身份短名 ↔ 上下文窗口 ↔ thinking 能力 ↔ 日期别名,一处定义。`/v1/models`、chat 路由(`map_model`)、身份规范化(`requested_model_identity`)、窗口推导(`get_context_window_size`)**全部从表派生**,消除历史"四处列表不一致"漂移。新增 `resolve_base`(统一归一 plain/-thinking/日期/日期-thinking 四形态)。

### Bug Fixes
- **修日期快照名导致 Kiro 真实代号(claude-quince)泄漏**:`requested_model_identity` 原对去 `-thinking` 后的名字做**精确匹配**,`claude-sonnet-4-5-20250929` 匹配不上 → 返回 None → system identity 行不规范化 → 上游真实代号泄漏给客户端。改走 `resolve_base`(剥 thinking + 剥日期后匹配),身份行回显**去 thinking 的原请求名**(保留日期名,与甲方请求一致),既不泄漏又名实相符。一旦公告日期名,此泄漏必被触发,故本次同改。

### Design Rationale
- 参考甲方 API_All_in_One 的 ALLinOne `models.yaml` 声明式单一来源思路,但用 **Rust 常量表**落地(代码表先上;暂不做 YAML 热编辑 + admin 模型管理页)。新增/下线模型只改 `KIRO_MODELS` 一处,派生点自动跟随。
- `map_model`/`get_context_window_size` 保留**子串兜底**:权威表未命中时(老客户端异名)仍按版本号子串路由,零回归。
- 日期↔版本静态对应(sonnet-4.5=20250929 / opus-4.5=20251101 / haiku-4.5=20251001)由表维护——Kiro 全链路拿不到带日期快照号,自维护是唯一路径。

### Notes & Caveats
- **不出价格**:NewAPI 价格在其渠道侧手配,`/v1/models` 不返回价格、NewAPI"从上游获取"也只拉 id 列表。
- opus-4.5 之前只被 map 接受、未公告,本次纳入公告 + 身份规范化(甲方列表含 `claude-opus-4-5-20251101`)。若 Kiro 上游对某裸名回 INVALID_MODEL_ID,从 `KIRO_MODELS` 删该行即可。
- cargo test 501 全绿;本地 worker `/v1/models` 实测 20 个 id 正确。

## [import-verify-on-arrival] - 2026-06-13

### Features
- **批量导入「导入即验活」**(对齐 kiro.rs batch-import-dialog 能力):导入对话框升级两阶段——导入(原智能合并不变)后**自动逐账号验活**:刷新 token + getUsageLimits 查配额,**全程只读、绝不发 chat**(no-chat-test-on-real-accounts)。逐行状态(等待/验活中/✓成功含 积分剩余/上限/⚠无配额数据/✗失败含上游原文)+ 进度条 + 汇总计数;验活失败且**本次新建**的账号提供「一键删除」回滚(merged 是已有账号,绝不连带);验活失败的号由 worker `report_failure` 自动标禁用,死号导入即现形。死号再也不用烧一条 chat 才暴露。
- **后端验活通路**:worker 新增 `POST /accounts/{id}/quota`(确保 token 有效→查配额;成功回写 quota_cache 让积分列即时反映,失败也写入节流缓存)+ admin 顺序扇出 `/accounts/{id}/quota`(镜像 refresh:首个非 404 即返、错误原文透传)。`try_fetch_quota` 返回类型改 `Result<_, UpstreamError>`(原 anyhow 包装),让验活端点能透出可分类错误;后台轮询调用方行为不变。
- **导入后立即同步(消 30s 窗口)**:worker 新增 `POST /sync`(与 30s 周期循环**共用** `sync_accounts_from_db` 实现);admin `import_accounts` 落库后 best-effort 并发捅所有 worker `/sync`——导入完立刻可验活/刷新,不再报「没有 worker 持有该账号」。前端仍保留 404=等待同步的重试兜底(4s×8)。

### Design Rationale
- 不照搬 kiro.rs 的"客户端逐条 add + 验活失败自动回滚删除":caio 的服务端一次性导入 + 智能合并更安全(token 不回退/碰撞防护),验活失败改**显式按钮**删除且只删 created——merge 语义下自动删有误删真号风险。
- 新端点仅挂 loopback(与 reset/refresh 同块),信任同机 router;验活失败 report_failure 与 chat/refresh 路径同口径(TokenInvalid→invalid_refresh_token 禁用,transient→计数,救号可清)。

### Notes & Caveats
- 验活通过=「能刷新+能查配额」,**不等于保证 chat 可用**(suspended 号已知 refresh 200 但 chat 403;此类号配额查询通常也失败/无数据,能被识别)。
- 本地隔离烟测 + Playwright 全流程验证(导入假号 2s 内验活命中、invalid_grant 原文透传、自动禁用、一键删除→DB 清除);cargo test 494 全绿。

## [machineid-no-freeze-match-kirors] - 2026-06-12

> **⚠️ 2026-06-13 更正**:本条的依据「kiro.rs 从不冻结、每次按当前 rt 重派生」是**误读**——只看了 `machine_id.rs::generate_from_credentials`,漏看加载层:`kiro.rs token_manager.rs:649-652` 在启动时对无 machineId 的凭据**派生并写回 credentials.json 持久化**(= 首导入即冻结),此后 rt 滚动不重算。即:用户长期不封号的 kiro.rs 实为**冻结**行为;static_flow 则是每次重派生,也不封。→ 冻结/重派生**都不是封号因子**,mrdev3258 等翻车实为 IdC durable 整批预死。本次改动(删冻结)与 static_flow 一致,行为无害,**暂保留**;若要逐字对齐 kiro.rs 可恢复导入时冻结(`freeze_machine_id_if_absent` 仍保留未删)。

### Bug Fixes（防封,重大）
- **撤销 machineId 冻结,改为每次按当前 rt 重新派生(对齐 kiro.rs + 真实客户端)**:`gw-kiro/src/lib.rs::refresh_auth` 原在首次刷新时把派生的 machineId(`sha256("KotlinNativeAPI/"+rt)`)**冻结**为显式 `machine_id` 持久化(理论:防"指纹随 rolling token 漂移")。**该理论错了**:对照 kiro.rs `generate_from_credentials` —— 它**从不冻结**,每次按**当前** refresh_token 重新派生;真实 Kiro 客户端也这么算,rt 滚动时 machineId 随之滚动、始终与上游一致。冻结反而在 rt 滚动后发出**陈旧** machineId(真实客户端不会发的值)→ 风控视作换设备 → 封号(mrdev3258 被反复刷新滚 rt + 陈旧冻结值,秒封)。已移除冻结调用;`generate_from_account` 保持"显式真机值优先,否则每次按当前 rt 派生"。`freeze_machine_id_if_absent` 标 `#[allow(dead_code)]` + 弃用说明防误接线;gw-core MachineIdentity 文档同步纠正。**有真机 machine_id(import 带入)的号不受影响,仍优先用显式值。**

### Notes & Caveats
- 已有库里若残留**之前冻结**的 `machine_id`(派生值),会继续被优先使用(陈旧)→ 建议清掉这些号的 `machine_id` 字段让其重新按当前 rt 派生(本次部署时账号库为空,无残留)。

## [import-camelcase-compat] - 2026-06-12

### Features
- **「添加账号」智能填充 + 类型自动识别**:CreateAccountDialog 顶部新增「智能填充」粘贴框——粘一个账号的 JSON(camelCase / snake_case / KiroManager 嵌套 `credentials` 都认,非 JSON 当纯 refresh_token),**客户端自动抽取** refresh_token/client_id/client_secret/region/machine_id + 从 email/userId 派生 account_id,并**识别类型**:有 client 凭据按 provider 分 `BuilderId✅安全` / `IdC⚠️易封`,无凭据为 `Social`。识别后字段可手动核对/改。同时补齐手动凭据字段(Client ID/Secret 成对校验 / Region / Machine ID)。i18n zh/en。线上合成账号烟测确认 client_id/client_secret/region/machine_id 正确入库(secret 脱敏)。**批量仍走列表页「导入」;此为单号便捷补录。**

### Bug Fixes
- **扁平导入兼容 camelCase(修 BuilderId 号导入丢 client 凭据)**:`gw-kiro/src/import.rs::map_flat` 原用 `top_str` 精确取键、只读 snake_case 的 `client_id`/`client_secret`/`access_token`/`profile_arn`/`auth_method`,而用户在 kiro.rs 用的 **camelCase BuilderId 导出**(`clientId`/`clientSecret`/`refreshToken`/`provider:BuilderId`)会**静默丢失 client 凭据** → 刷新分流(`is_idc` 看 client_id+secret 在不在)判成 social → BuilderId 号刷新失败/行为错。新增 `flat_str`(snake_case 优先、回退 camelCase,对齐 kiro.rs 的 `#[serde(rename_all="camelCase")]`)+ `snake_to_camel`,`map_flat` 全字段改用之;`expires_at` 也认 `expiresAt`。snake_case durable 格式不回归。新增 3 测试(camelCase BuilderId 保留 client 凭据 / camelCase accessToken·profileArn·machineId / snake_to_camel);线上合成账号烟测确认 clientId+clientSecret+provider 正确入库。

### Notes & Caveats
- **账号类型与防封**:BuilderId(个人 AWS Builder ID)的 machineId 本就按 `sha256("KotlinNativeAPI/"+rt)` 派生,我方派生值=真客户端值 → 不封(`has_machine_id:false` 对 BuilderId 是**正常且安全**的)。IAM Identity Center(`auth_method:idc`,企业 SSO)用真机 machineId,派生对不上 + 新出口 IP → 风控锁号(`TEMPORARILY_SUSPENDED`)。**生产用 BuilderId,避开 IdC durable 导出。**

## [x-api-key-auth-and-concurrency-display] - 2026-06-12

### Bug Fixes
- **接受 Anthropic 标准 `x-api-key` 鉴权头(修 NewAPI「从上游获取」500)**:`router::extract_bearer` 原只读 `Authorization` 头,而真 Anthropic API / NewAPI 的 Claude 渠道 / Anthropic SDK / Claude Code 指到本网关都发 **`x-api-key`** → 一律 401,NewAPI 再包装成 500(表现为点「从上游获取模型」报 500)。改为**两种头都认**:优先 `x-api-key`(本网关对外是 Anthropic 线缆),回退 `Authorization: Bearer <key>`(或直接放 key);均 trim、空值跳过。修复后 `/v1/models`(及 `/v1/messages` 等所有代理端点)对 Anthropic 原生客户端可用。错误文案改「缺少 API key(x-api-key 或 Authorization: Bearer)」。新增单测覆盖两种头/优先级/空值回退/缺失。
- **并发列语义翻正(显示在用而非空闲)**:账号页「并发」列原显示 `available_permits/max`(空闲槽/上限),空闲账号反直觉地显示 `2/2`。改为通用约定 **`在用/上限`**(`在用 = 上限 - 空闲`),空闲 → `0/2`;加 tooltip。纯前端 + i18n(zh/en)。

### Notes & Caveats
- 鉴权仍逐请求查库验 key,`x-api-key` 只是多认一个头、不放松校验(无绕过)。
- 排障副产:导入账号配额列显示 `—` 是因导入的 access_token 已过期 + 60s 失败缓存 TTL;系统会自愈(后台轮询/按需刷新会先刷 token 再查),新加的手动刷新按钮可即时解决。

## [manual-token-refresh-and-full-models-catalog] - 2026-06-12

### Features
- **人工「刷新 token」按钮(账号管理页)**:每行新增刷新图标(始终可用,与编辑/删除一致),强制该账号走一次 rt→at 上游交换。后端新增 worker `POST /accounts/{id}/refresh`(仅 loopback 挂载,镜像 reset)+ admin 顺序扇出 `POST /accounts/{id}/refresh`;前端 `useRefreshAccount` + 成功轻量反馈条(账号 + 新 token 有效期,下次操作自动消失)。**只刷新、不发 chat → 不触发风控**(见 no-chat-test-on-real-accounts),就是后台轮询/按需刷新本就在做的 OIDC 交换。
- **补全 `/v1/models` 目录**:`list_models` 原手列 3 个 stub(opus-4-8/sonnet-4-5/haiku-4-5),漏报 chat 实际可服务的 opus-4-6/4-7 与 sonnet-4-6。改为映射 converter 新权威常量 `MODEL_CATALOG`(6 个公告模型,带展示名/thinking 能力),`context_length` 由 `get_context_window_size` 推导(opus-4.6+/sonnet-4.6=1M,余 200k)。

### Design Rationale
- **token 刷新拆两条语义**(对抗审查 Skeptic#2/Architect#3 共识 medium):`force_refresh`【无条件】刷新供人工按钮(必须真打上游验证 rt);chat 403 retry 改走 `refresh_after_rejection`【CAS:仅当 scheduler 现存 access_token 仍是这枚被拒 token 才刷】,避免同账号 N 个并发 403 各无条件重刷一次、放大 token 交换/rolling refresh_token。判据用「token 是否被换掉」而非 expires_at 新鲜度(被拒 token 常仍看似新)。共享尾段抽成 `do_refresh_and_persist`(3 调用点复用,保锁内不变量)。
- **admin refresh 顺序扇出**:与 reset 并发扇出不同——refresh 有副作用(滚动 rt),故顺序问各 worker;**成功立即返回**(成功才滚 token,绝不刷到第二个 worker),失败(未滚 token,安全)记下首错继续问,扫完无成功才透出首错/404(审查 Skeptic#3/Architect#4:重复持有窗口里别让首个瞬时 502 掩盖另一 worker 的成功)。
- **`MODEL_CATALOG` 作公告子集**:`map_model` 子串匹配更宽松(也接受 legacy opus-4-5),目录只列完整支持(含身份规范化)的 6 个稳定模型。

### Bug Fixes
- **opus-4-5 身份泄漏(对抗审查 Architect#1)**:`map_model` 接受 `claude-opus-4-5` 但 `normalize::requested_model_identity` 无对应分支 → 路由到 Kiro 后真实代号(claude-quince)可能从 system identity 行泄漏。补 opus-4-5 分支(映射集合须 ⊇ 身份规范化集合)+ 回归测试 `every_advertised_model_normalizes_identity` / `legacy_opus_4_5_normalizes_identity`。
- **人工刷新失败如实反映(审查 Skeptic#1)**:worker refresh handler 失败时 `report_failure`——rt 永久失效→即标 invalid_refresh_token 禁用(仪表盘即时见死号),transient→计失败数(救号一键清),与 chat 路径一致。
- **前端刷新成功条 stale(审查 3 名 reviewer 共识)**:改本地态(任一操作开始即清),不再派生自 `refreshMutation.isSuccess`(后者会在其它 mutation 成功后残留旧账号);刷新按钮不再 gate `runtime.online`(runtime 降级时仍可手动刷新)。

### Notes & Caveats
- 重复持有窗口(组重分配中两 worker 暂持同账号)下,admin refresh 仍可能刷一个、另一个 30s sync 前用旧 rt;这是 30s 最终一致模型的固有性质(reset 同),非本次回归。
- 刷新成功仅展示 expires_at(不回传 token 明文);worker handler 仅 loopback 挂载(非 loopback 误配下禁用,避免绕过 router 鉴权)。

## [request-log-store-and-experimental-settings] - 2026-06-12

### Features
- **请求日志 store 层(③ 调试功能地基,零风险)**:gw-store 新 `request_logs` 表 + `RequestLog`/`RequestLogRow`/`RequestLogDetail`/`RequestLogFilter` 类型。`insert_request_log(log, cap)` 追加并**环形保留最新 `cap` 条**(`id <= max_id - cap` 裁旧,AUTOINCREMENT 不复用);`list_request_logs(filter, default_limit)` 按 account/model/success/时间窗筛选(不含大 payload);`get_request_log(id)` 取含完整 client+kiro payload 的详情。3 单测(往返/环形裁剪/筛选)。**捕获(worker)+ admin 端点 + 前端日志页仍待建**(留待后续:难点是流式 Drop 收尾穿透原始 body + 渲染后 Kiro body + 计时)。
- **实验性开关进设置面板(热控)**:`tools_in_prefix` / `cache_point` 从 env-only 改为可经设置面板热控。gw-kiro converter 改进程级 `RwLock<ExperimentalFlags>`(env 仍作启动默认,后向兼容),`KiroProvider::apply_hot_settings` 经 settings overlay 热改;gw-core `SystemConfig.experimental` + `SystemSettings` overlay。前端 SettingsPage 新增「实验性」卡(tools_in_prefix/cache_point 带说明)+ 调度卡补 `quota_poll_enabled` 复选框 + i18n(zh/en)。

### Notes & Caveats
- 请求日志 store 方法均为 `pub` 库 API,暂无调用方(捕获未接);非死代码(lib pub item),由单测覆盖。
- 实验开关在 worker **重启后有 ≤30s 窗口**先用 env 默认值,再被 DB overlay(30s settings 轮询)覆盖——与其它热设置一致;默认全 off 故窗口无害。

## [dup-tool-id-fix-and-ambient-quota-poll] - 2026-06-12

### Features
- **跨轮重复 `tool_use_id` 修复(正确性)**:新 `gw-kiro/src/converter/tool_id.rs::rewrite_duplicate_tool_use_ids`(🟢 借鉴 static_flow)。客户端(Claude Code)反复 auto-compact 后,同一 `tool_use_id` 可能在两个【各自已完成】的 assistant 轮里各出现一次;原 `validate_tool_pairing` 用 `HashSet` 静默去重,但 history 残留两个同 id 的 `ToolUseEntry` → Kiro 400 Improperly formed。修复=把跨轮重复 id(及按 FIFO 配对的 tool_result)改写成带 `__caiodup{N}` 后缀的唯一 id。**只作用于发往 Kiro 的 wire 报文**;在 conversationId 派生【之后】调用,身份链(与 worker `affinity_key_from_body` 同源)不漂移。
- **static_flow 式后台配额轮询(防封 ambient 流量)**:`worker` 自身每 240–300s(`jittered_secs` 时间熵抖动,免引 `rand`)兜底刷新一遍 `getUsageLimits`(只读,安全),**不依赖 /health 被打**。复刻真实 Kiro IDE 每 ~5min 一次配额轮询的 ambient 流量(纯反代账号两次聊天间对上游静默是最易被审计的指纹)+ 让配额面板无人看 dashboard 时也新鲜。被 /health(TTL=60s)在 floor(240s)内刷过的账号本轮跳过——不双倍打点;上游并发仍受 `quota_sem(3)` 节流。启停经 `SchedulerConfig.quota_poll_enabled` 热开关(设置面板可即时启停,无需重启)。

### Design Rationale
- **dup-id 改写绝不报错**:畸形/compact 残留(同轮活跃重用、孤儿 result)交既有 `pairing` 阶段兜底,FIFO 队列按出现序配对,绝不把可清理的输入升级成用户可见 400。
- **种子纳入所有现存 id**(tool_use + tool_result):后缀候选避开全部现存 id,防改写后撞上历史孤儿 tool_result 把它"复活"成错误配对。
- **配额轮询单驱动顺序 sweep**:不再 per-account spawn(避免上万账号周期性建上万个挂起 task);跳过 disabled/dead 账号(禁用即不再使用,不周期性打上游/试刷 token)。
- 对抗审查(Codex×3:Skeptic/Architect/Minimalist)逐条处理:A1 孤儿复活(加 used_ids 全 id 种子+专测)、A2 去掉所有报错改 FIFO、A4 身份链不变量加回归测试、B1 单 task 顺序、B2 跳过 disabled、B4 env→SchedulerConfig 热开关、B5 floor inline。

### Notes & Caveats
- **dup-id 跨 compact 漂移(已知,接受)**:改写后缀按"该 id 第几次出现"分配,确定性 given 可见历史。若客户端后续 compact 删掉较早那次重复,同一逻辑调用的序号变 → wire id 随之变(`dup__caiodup2`→`dup`),打断该点之后 Kiro 前缀缓存。只影响【已含重复 id 的罕见降级会话】,代价是一次缓存未命中(非 400),仍优于不改写直接 400。
- **轮询 shutdown 边界**:后台任务是 daemon(同既有 30s sync loop,无独立 shutdown 协调)。安全依据:`refresh_locked` 刷新 token 后**同步落库**(`merge_account_extra`),停机中途被 drop 也不丢 rolling token;sweep 唯一外部副作用是只读 getUsageLimits,被 drop 无损。
- `QUOTA_POLL_MIN_SECS`(240)与 `QUOTA_TTL`(60)耦合:前者作 stale floor 必须 > 后者,否则与 /health 重复打点。调任一常量时检查二者关系。
- **tools_in_prefix 保持默认关闭**:把工具定义 blob 挪进 history[0] 前缀想蹭 Kiro 缓存省积分,是"部分客户端工具调用失效"的根因(Kiro 只可靠地从 currentMessage 提供工具),金标准 static_flow 亦不用。env `KIRO_TOOLS_IN_PREFIX=1` 才开,默认 off,本轮未动。

## [settings-panel-and-egress-proxy] - 2026-06-12

### Features
- **系统设置面板(前端首个设置页)+ DB 持久 + 30s 热生效**:`settings` 单行表(JSON overlay)叠在不可变的 `system.yaml` 基线上;`GET/PUT /admin/api/settings`(GET=有效全量,PUT=部分 patch,`null`/空=删该 overlay 字段回 YAML 默认)。worker 30s 轮询用 `from_effective` 广播全量回 provider(`apply_hot_settings`)+ `scheduler.update_tuning` + cache_sim,**无需重启**。前端 `SettingsPage` 四组卡片(代理/缓存/调度/图像),只提交改动字段。
- **全局默认代理 + 每账号出口代理(全程同出口)**:新 `gw-kiro/src/resolver.rs::EgressResolver` 按账号解析出口 client,优先级 **账号 `extra.proxy` → 全局 `default_proxy` → worker 绑定源 IP**。KiroProvider 的 chat/refresh/quota/profileArn **四处**统一走 `resolver.client_for(account)`——同一账号刷新与发包同 IP(防封铁律)。代理 client 用 `reqwest::Proxy`(不绑 local_address);无代理才用 base(绑源 IP)。
- **每账号代理写入通道**:创建账号 `extra.proxy`;导入 `batch_proxy`(整批);编辑 `PATCH proxy_url`(定点 `merge_account_extra`,空串=清除,**绝不碰凭据**,规避 PATCH extra 整块替换坑)。前端三对话框对应加字段。
- **调度 `Tuning` 热更**:`RwLock<Tuning>` + `update_tuning`;`KiroProvider` 的 cache_billing/image_cfg 改 `RwLock` 承接热调。

### Design Rationale
- 不改 `ProviderFactory` 签名:`default_proxy` 经 `provider_cfg` JSON 注入 + `Provider::apply_hot_settings`(默认 no-op)——改动局限在 Kiro。
- 对抗审查(Codex×3)修复:①**代理写入边界 fail-closed**(`validate_proxy_url`:非法/含掩码占位的代理在 create/import/update/settings-PUT 一律 400,杜绝"配了代理却静默回退裸 IP");②**代理密码脱敏**(`redact_proxy_url`:GET 响应把 `user:pass@` 的密码段掩成 `***`,真值仍存库供 resolver 用);③**`deny_unknown_fields`**(设置 PUT 拼错 key 直接 400,不落库死 overlay);④Provider trait 出口契约文档更新(不再"出口由进程固定")。

### Notes & Caveats
- **默认代理热更的请求内一致性**:账号**无专属代理**、恰好在一次请求的 refresh 与 chat 之间被 admin 改了全局 `default_proxy` 时,这一次可能两步走不同出口(极窄窗口、自愈)。**用每账号专属代理(完全稳定)做严格按号隔离**,不要在有流量时热改全局默认代理。彻底冻结"请求级出口身份"需重构 provider/worker 出口流程,代价大、暂不做。
- `default_proxy` 仅 DB 管理(不读 `system.yaml` 基线);设置 PUT 的读-改-写非单事务(单运营者并发可忽略);前端数字字段暂无"逐字段重置回 YAML 默认"(可手动填默认值)——均为已知后续项。
- 进程拓扑(端口/每 worker 源 IP)仍在 instances.yaml,改动需重启(面板已注明)。

## [scheduler-hardening-and-image-compression] - 2026-06-11

### Features
- **模型能力过滤(opus 防误杀)**:`Provider::account_supports_model`(Kiro 实现 = `subscription_title` 含 FREE 拒 opus,未知放行)+ `AccountScheduler::acquire_where(谓词)` + 新错误 `NoModelSupport`(HTTP 400)。实测:纯 FREE 池打 opus 在选号阶段即拒,**零上游调用**——此前会 403 → 误判 TokenInvalid → 永久禁用健康号。
- **订阅档位数据闭环**:getUsageLimits 的 `subscriptionTitle` 回填 `extra.subscription_title`(内存锁内单字段合并 + DB 持久化);worker 启动/账号 sync 后对缺该字段的账号**预热**配额查询(只读,quota_sem=3 节流)。只导 rt 的老号也能收敛。
- **调度/冷却参数 config 化**(`system.yaml scheduler` 段),默认对齐 kiro.rs 生产:429 冷却 60s→**300s**、empty 冷却 20s→**60s**、empty 窗口 120s→**60s**、亲和 TTL 1800s;`max_failures=5` 保留(本项目 5xx 计数、kiro.rs 不计,语义不同)。
- **救号 reset 贯通**:`scheduler.reset_account`(清运行时禁用/冷却/计数,**配置禁用不动**)→ worker `POST /accounts/{id}/reset`(仅 loopback 挂载)→ admin 扇出端点 → 前端 HeartPulse 按钮(运行时禁用/有失败计数才显示)。
- **图像压缩移植**(🔵 kiro.rs/xkiro):四档阈值缩放 + **解码前 OOM 护栏**(1 亿像素/64MB 上限拦解压炸弹)+ 信号量背压 + spawn_blocking;失败一律回退原图。`system.yaml image` 段可配,接在 KiroProvider::chat 转换前。
- **`POST /v1/messages/count_tokens`**:router 本地估算(对齐 kiro.rs 默认路径),补 NewAPI/客户端探测兼容。
- `max_concurrency` 默认 1→**2**(serde/admin create/导入/SQLite schema/前端表单五处对齐 kiro.rs);DB 存量行不受影响,可 PATCH。

### Design Rationale
- 对抗审查(Codex×3)发现并已修复:①刷新回写「替换→置脏」非原子,30s sync 可在窗口内用 DB 旧值洗掉新 rolling token(新增 `update_account_dirty` 单锁原子化);②`flush_dirty_extras` 用旧快照整块落库+无条件清脏,会回滚并发刷新刚写库的新 token(改为逐账号持 refresh_lock、锁内重读+重查脏位);③刷新基底改用 scheduler 真值而非调用方旧快照(防用已作废 rolling token 刷新、防抹掉 merge 进来的字段);④全灭自愈带模型过滤(opus 请求不复活无关 FREE 失败号);⑤acquire 尝试预算与 max_failures 解耦(max_failures=1 时自愈后仍有机会重选);⑥reset 端点仅 loopback 挂载(非 loopback 误配不暴露无鉴权写操作)。
- 冷却状态仍纯内存(QuotaExhausted 重启复活,撞一次 402 重禁),与 kiro.rs 的差异已知、影响小,暂不持久化。

### Notes & Caveats
- 调度参数改动需重启 worker;热调控制面与 cache 参数热调一起留作后续(跨进程 plumbing)。

## [profile-arn-discovery] - 2026-06-11

### Features
- **动态 profileArn 发现(`ListAvailableProfiles`)** —— 🔵 对齐 static_flow,**kiro.rs 无此能力**(它依赖导入时凭据自带 profileArn)。企业/IdC 号的 chat 与 getUsageLimits 都强制要求 profileArn,凭据常不带;`gw-kiro/src/profiles.rs` 在缺失且无固定兜底(social/builderid)时,运行时 POST `q.{region}.amazonaws.com/ListAvailableProfiles`(跨候选区、翻页、runtime UA)发现并经 `ensure_profile_arn` 持久化进 extra。一次发现、后续短路。
- `Provider::discover_profile_arn`(默认 None)+ KiroProvider 实现 + worker 在 chat/配额前 `ensure_profile_arn`(发现失败不阻断,让上游自然 400 BadRequest,不惩罚账号)。

### Design Rationale
- 发现失败(403「not authorized」= 个人/Builder ID 层无此功能)时静默回退,由固定兜底或显式 profileArn 接管。复用 `persist_extra_field`(refresh_lock 互斥,与 subscription_title 同协议)。

### Notes & Caveats
- **实测验证(真号端到端,IdC Builder ID PRO 账号)**:
  - getUsageLimits → `KIRO PRO` 已用 9456/1000(945% 超额),subscription_title 回填成功;
  - opus 非流式 chat → `OK` + end_turn + usage(cache_read=683/input=6150);
  - opus 流式 chat → 完整 SSE 事件序列(message_start…message_stop);
  - count_tokens 真实内容 → 估算正常;模型过滤 opus 路由正确;BadRequest **不封号**(账号始终 enabled)。
- **发现**:Builder ID 个人号(clientName「Amazon Q Developer for command line」)的 `ListAvailableProfiles` 返回 403「not authorized」,须靠 `kiro_provider=builderid` 走固定 `BUILDER_ID_PROFILE_ARN` 兜底;原始 JSON 凭据(无 kiro_provider)目前需手填该字段。**待办**:对此类 403 自动回退 builder ARN,让裸凭据即插即用(本次保守未做,避免误判真企业号)。
- **运营提醒**:`PATCH /accounts/{id}` 的 `extra` 是**整块替换**(凭据轮换语义),漏字段会清空凭据——admin UI 编辑须回填全字段或改后端为字段级 merge(待办)。

## [thinking-xhigh-and-converter-hardening] - 2026-06-11

### Features

睡前 backlog 的安全增量(thinking 深度 + 缓存配置 + converter 400 兜底):

- **thinking 默认 effort `high`→`xhigh`**:Opus 全系默认 adaptive 思维链时,缺省 effort 之前是 `high`(实测仅产 ~43 字符桩推理),改为 `xhigh`(~3560 字符深推理),对齐 static_flow。`OutputConfig.effort` 改为 `Option<String>` + `effective_effort()` 缺省 xhigh(客户端带 output_config 但不带 effort 时也能正确回退,不再被 serde 默认强写 high)。
- **cache_sim 会话表可配**:`CacheConfig` 加 `sim_ttl_secs`(默认 300)/`max_sessions`(默认 4096),worker 启动时同步到全局 sim store(此前恒用编译期默认)。带 serde 默认,旧 system.yaml 仍可解析。
- **converter 400 兜底两项**(纯正确性,对齐 static_flow):空工具描述兜底为 `Client-provided tool '{name}'`(某些 Kiro 版本拒空描述 400);文档 `source.type="text"`(markdown/html/csv/txt)现 base64 编码后透传(此前静默丢弃)。
- **identity_override + 隐私策略注入**(`converter/history.rs`,逐字对齐 static_flow):每个请求 history[0] 始终注入,强制模型自认 Claude、不自曝 Kiro。⚠️ **实测发现不足**:用真号"你是谁"探针,模型注入后**仍答"我是 Kiro"**——Kiro 上游服务端身份压过客户端注入。单测证实注入确实落线缆;故此为 static_flow 平价(无害该留),**非身份检测银弹**。真实检测向量是结构化输出/desc/thinking 泄漏(见 docs 计划),需 38990 探针重放定位。

### Notes & Caveats

- 实测:gw-kiro 264 测试全绿(含 thinking effort/空描述/text 文档新单测);workspace 全绿。**未对真号发 chat**。
- **未做(故意推迟,见 docs/CONTEXT_LEGITIMACY_AND_TUNING_PLAN.md)**:① identity_override + 隐私策略注入(防身份检测封号)——**检测敏感,需 38990 探针重放验证后再上**,不盲发;② 更大的 converter 防 400 项(tool_use ID 清洗/文档去重限额/多模态工具 schema 兼容/stringified tool_result 解析);③ cache 三参数运行时热调端点(跨进程,需 router↔worker 内网跳);④ 前端补齐(请求日志/调度面板/强制刷新/批量操作)。

## [import-and-quota] - 2026-06-11

### Features

完整导入 KiroManager 账号 + 账号配额(积分)展示:

- **完整导入(防封核心)**:新增 `POST /admin/api/accounts/import` + 前端 `ImportAccountsDialog`,粘贴/上传 KiroManager 导出 JSON 一键导入。`gw-kiro/src/import.rs` 把导出字段映射到账号 extra——**关键是搬运真机 `machineId`**:此前只导 refreshToken,服务器据 rt 重派生一个不同 machineId(`sha256("KotlinNativeAPI/"+rt)`)→ 上游看到"激活设备 A、发包设备 B" = 双指纹 = 封号;完整导入消除这一根因。同时搬 clientId/secret/profileArn/region/kiro_provider。
- **智能合并**:已存在账号只补缺失身份字段;**token 字段(refresh_token/access_token/expires_at)仅创建时写,合并永不碰**(服务器拥有并轮换,导出里是旧值)。`machineId` 与已有不同时不覆盖、标 `machine_id_conflict` 提示。
- **账号配额展示**:`gw-kiro/src/usage_limits.rs` 移植 kiro.rs 的 `getUsageLimits`(只读),Provider 新增 `account_quota`。worker 侧 stale-while-revalidate 缓存(TTL 60s)+ 并发上限信号量,`/health` 带 `quota` 字段,前端账号表加"积分(剩余/上限)"列(吃紧标红)。

### Design Rationale

- **machineId 是防封关键,非端点**:KiroManager 导出 JSON 顶层带激活时的真机 `machineId`,完整导入原样搬入即可让发包指纹与激活一致。
- **token 合并即创建时写**:既兑现"不回退服务器已 roll token",又消除"导入读到旧值→并发刷新写新值→导入覆盖回旧值"的 TOCTOU 竞态(无需把合并塞进 DB 事务)。
- **配额只读 + 不阻塞 /health**:getUsageLimits 是只读查询(用户确认不招封号),后台刷新 + 缓存,/health 立即返回缓存值;信号量挡住"上百账号同时被查看"的 stampede。

### Notes & Caveats

- **配额刷新会 roll token**:后台刷新调用 ensure_credentialed,token 临期时会刷新(roll rt)。对已托管给反代的账号是预期行为;若同时还在用 KiroManager 管同一账号,两边 token 会发散。
- **machineId 必须合法**:导入只接受 64hex/UUID 形态的 machineId(非法形态丢弃,留空靠冻结按 rt 派生并提示),避免"谎报已设置但运行时仍派生"。
- **account_id 由 email 清洗派生**:可读但可能碰撞;智能合并用 user_id/email 稳定身份核对,不同真号撞同一 ID 时跳过(绝不合并两个真号)。
- getUsageLimits 走 `q.amazonaws.com`(同 kiro.rs)、发包走 `runtime.kiro.dev`,不同 host 但 machineId 一致(真实客户端本就跨 host)。
- 实测:workspace 测试全绿(gw-app 80 / gw-kiro 258,含导入映射/智能合并/碰撞/token保留/配额解析);admin-ui tsc+vite 通过;两个真号(POWER/PRO)经实时端点导入验证 machineId 落库后清理。**未对真号发任何 chat**。
- 对抗审查(codex Skeptic+Architect+Minimalist):修 3 个 high(account_id 碰撞合并/非法 machineId 谎报/token 覆盖竞态)+ 配额失败节流 + stampede 信号量 + 去 `backfilled` 泄漏字段名 + json 单一字符串形态;保留 Provider trait 默认方法(与现有 affinity_key 一致)。

## [kiro-wire-align] - 2026-06-10

### Features

gw-kiro 报文标准化 + machineId 防封 —— 逐字节对齐当前生产客户端 **static_flow**(commit 9051d71,已 `git fetch` 更新到最新):

- **主推理端点迁移**:`q.{region}.amazonaws.com` → `runtime.{region}.kiro.dev`(env `KIRO_RUNTIME_UPSTREAM_BASE_URL` / `KIRO_UPSTREAM_BASE_URL` 可覆盖)。kiro.rs 旧实现仍停在 `q.amazonaws.com` 且写死不可配,本项目对齐 static_flow 当前客户端。
- **新模块 `gw-kiro/src/headers.rs`**(报文单一事实源):主推理请求头逐字对齐——`accept: application/vnd.amazon.eventstream`、UA `os/darwin#24.6.0`/`nodejs#22.22.0`、**主 UA 去掉 `m/E`**(此前为残缺指纹)、条件头 `TokenType: EXTERNAL_IDP`(external_idp)/`redirect-for-internal`(internal provider)。chat.rs/token.rs/lib.rs 的 UA 全部收敛至此,消除版本漂移(此前 machine_identity 写死 `aws-sdk-js/1.0.0` 的陷阱)。
- **IdC 刷新 UA 对齐**:x-amz-user-agent 带版本 `KiroIDE-0.12.155`;user-agent 去掉 `api/sso-oidc`、补齐 os/node 版本。
- **machineId 冻结防封**(核心):`machineId = sha256("KotlinNativeAPI/"+refresh_token)`,而 refresh_token 是 **rolling** 的——不冻结则每次刷新 machineId 漂移 = 上游视为"同账号换设备" = 封号。`freeze_machine_id_if_absent` 在 `refresh_auth` **覆盖新 token 之前**用旧 token 派生值钉成显式 `machine_id` 并经 worker delta 持久化,设备指纹此后恒定。
- **账号 schema 扩字段**:暴露 `machine_id`(防封关键,可填真机指纹)、`auth_method`、`client_id`/`client_secret`(IdC 一等公民)、`kiro_api_key`、`kiro_version`,admin 表单可配置三种凭据(Social/IdC/API Key)。`FieldSpec` 加 `with_help` 提示。
- **脱敏补全**(安全):admin GET 脱敏此前只认 `token`/`secret`/`password`,漏掉 `kiro_api_key`(含 `key`)→ 明文泄漏。现加入 `key` 规则,所有 `*_key` 凭据字段一并脱敏(PATCH `***` 哨兵保留逻辑不受影响)。

### Design Rationale

- **为什么是 machineId 而非端点**:经 kiro.rs / static_flow / gw-kiro 三方代码交叉验证,KiroManager 导入(Social 号)易封、JSON 导入(常为 IdC)不易封的根因是 machineId 指纹漂移——Social 号注册绑真机 `vscode.env.machineId`,导出常丢失该值,派生哈希对不上;且 rolling token 让派生值持续漂移。端点 `q.amazonaws.com` 仍可用(static_flow 自身也用于 ListAvailableProfiles),属"不够像当前客户端"的次要指纹,非已证实封号主因。
- **冻结在 provider 的 refresh_auth**:此处持有刷新前的旧 token(派生材料),且返回的 extra 经既有 worker delta 持久化机制落库,无需新增跨层管道。
- **headers.rs 单一事实源 + golden 单测**:把"对齐 static_flow"显式化为逐字断言的单测(`streaming_ua_matches_static_flow_exactly` 等),static_flow 再更新时测试即对照点,防止悄悄漂移。

### 对抗审查加固(codex Skeptic + Architect,CONTESTED → 处置)

- **撤下未实现的 API Key 路径**(high):此前 schema 暴露 `kiro_api_key` 但调用链 OAuth-only(refresh_auth 强制 refresh_token、chat 只读 access_token),会"加载通过、首请求才报错"。现从 schema 移除,`validate_account` 明确要求 refresh_token 并提示 API Key 暂不支持。
- **profileArn 固定兜底**(high):端点迁 runtime.kiro.dev 后,缺 profileArn 可能被拒/命中错误 profile。port static_flow 的 `fixed_profile_arn`:按 `kiro_provider`(github/google→social 共享 ARN;builderid→builder ARN)兜底,显式值优先,企业号仍省略(动态 ListAvailableProfiles 未实现)。
- **IdC 刷新报文逐字对齐**(medium):补 `accept: */*`、头序对齐 static_flow `refresh_idc`,并抽到 `headers::apply_idc_refresh_headers` + golden 测试(此前 golden 不覆盖 IdC 刷新)。
- **machineId 误判修复**(medium):`is_api_key_credential` 改为必须有非空 `kiro_api_key`(仅 `auth_method=api_key` 标签不够),避免误配账号落随机指纹再被冻结固化。
- **provider 撞名修复**(medium):`redirect-for-internal` / profileArn 兜底改读专用键 `kiro_provider`(`extra["provider"]` 会被 serde flatten 吃到 `Account.provider` 顶层字段)。
- 安全:admin 脱敏补 `key` 规则,`kiro_api_key` 等 `*_key` 字段不再明文经 GET 泄漏(Architect 复核已确认修复)。

### Notes & Caveats

- 冻结的局限(如实声明):若账号导入时 refresh_token 已 roll 过(KiroManager 导出的是当前而非原始 token),冻结值仍 ≠ 真机指纹,只是阻止"继续漂移";彻底规避需在账号里填真机 `machine_id`(schema 已支持)。Social 号首次刷新时 info 日志提示。
- `kiro_version` 可 per-account 覆盖,但 OS/Node/SDK 版本写死:改它而不同步会造成现实不存在的指纹组合(schema help 已警示),非必要勿动。
- 端点统一 `runtime.{region}.kiro.dev`:暂未保留 static_flow 对 gov region 的 `q-fips.*` 特殊 host(当前无 gov 账号;需要时经 env 覆盖)。
- "对齐 static_flow" 靠 vendored 常量 + golden 单测(注明源 commit 9051d71),非自动同步:static_flow 升级 client/SDK 版本时需手动比对更新(已 `git fetch` 到最新)。endpoint-family 抽象(MCP/usage/profile 路径)暂缓(当前无 MCP 上游路径)。
- 实测:gw-kiro 测试(含 headers golden 10 + machineId 冻结/收紧 + IdC golden);workspace 387 全绿,零警告。报文未对真实上游发包验证(凭据/风控约束),靠 golden 单测对照 static_flow 源码保证字节一致。

## [rename] - 2026-06-10

### Features

- 项目更名:**kiro-gw → Claude All in One**。二进制 `claude-all-in-one`、admin UI 标题/登录页、文档、示例配置、前端包名全部跟随;localStorage 键前缀 `kiroGw*` → `caio*`(已登录会话需重输一次 admin token)。

### Notes & Caveats

- 内部 crate 名(gw-core/gw-kiro/gw-store/gw-app)是实现细节,未随名;上游 provider "Kiro" 的指称(gw-kiro、账号示例 kiro-01)保留——那是上游名,不是项目名。
- 启动命令换为 `./target/debug/claude-all-in-one --mode ...`;旧名二进制已从 target 删除,防误用旧产物。

## [admin-v1.1] - 2026-06-10

### Features

后端完善四件套 + 停机数据安全:

- **router 负载计数修正**:删掉只增不减的累计计数器,活跃负载改为从亲和表派生(钉在该 worker 上的未过期 session 数)。session 过期负载即回落,空闲 worker 能重新承接新会话;亲和指向已下线 worker(拓扑变更)时丢弃重选。
- **admin `/accounts/runtime` 并行聚合**:逐 worker 拉 `/health` 由串行改 `join_all` 并发,最坏耗时 ≈ 单个 2s 超时,不再随离线 worker 数累加。
- **优雅停机**:router/worker 响应 SIGTERM/Ctrl-C(`with_graceful_shutdown`)——停止接收新连接,在途请求(含流式 SSE)自然跑完;排空不设上限,硬截止由 supervisor 兜底(docker 默认 10s、systemd `TimeoutStopSec`)。
- **worker 停机前脏 extra 落盘**:`flush_dirty_extras` 由 30s sync 循环与停机排空共用——「刷新成功但 DB 回写失败」的 rolling refresh_token 在进程退出前有最后一次落盘机会,不再依赖下轮 30s 重试(进程退出即 drop)。
- **`--features embed-ui` 单二进制部署**:rust-embed 把 `admin-ui/dist` 嵌进二进制;SPA 客户端路由兜底回 index.html;vite 哈希资产 `immutable` 永久缓存、index.html `no-cache` 保发布即生效。feature 默认关闭(fresh clone 无 dist 仍可编译),关闭时维持原 ServeDir 磁盘读取。

对抗审查(Skeptic+Architect)加固:

- **router 故障转移**(Architect high):worker 进程挂掉但仍在配置里时,原实现会让钉住它的 session 502 长达 30 分钟。现 `send()` 连接失败 → 丢弃指向故障实例的亲和、在其余 worker 里重选重发一次(请求未送达,无重复送达风险),亲和重钉到备选。活体实测:打挂掉的 instance 0 → 自动转移 instance 1 拿到正常响应,第二个请求直达不再转移。
- **停机等待在途 usage 落库**(Skeptic medium):SSE 收尾的 usage/quota 落库是 Drop 里 detach 的 spawn 任务,graceful shutdown 只等响应体不等它们。新增 `PendingWrites` RAII 登记,排空后 `wait_idle`(5s 上限)等这批任务收尾,最后一批计费记录不再随 runtime 关闭静默丢失。
- **亲和全表清理节流**(Skeptic medium):O(n) retain 从每请求改为 ≥5s 一次(`cleanup_if_due`);命中路径补 O(1) 精确过期判断(不依赖清理兜底)。几十万 session 时 router 延迟不再随表大小线性放大;代价是负载统计里陈旧条目最多滞留 5s。

### Design Rationale

- **负载从亲和表派生而非独立计数**:单一事实源,过期清理(retain)天然让负载回落,无需配对的 increment/decrement(后者漏一边就永久漂移——正是被替换实现的病根)。代价是「负载」语义为活跃 session 数而非在途请求数,对会话粘性网关是合理代理指标。
- **embed-ui 用 feature 门控而非无条件嵌入**:rust-embed 编译期要求资产目录存在,`dist` 又被 gitignore;无条件嵌入会让 fresh clone 直接编译失败。release 部署构建用 `cargo build --release --features embed-ui`(需先 `bun run build`)。
- **停机落盘只兜「已脏」数据**:正常路径刷新成功即同步落库(admin-v1 已做),脏位只在落库失败时存在;停机 flush 是窄窗口兜底而非主路径。

### Notes & Caveats

- 实测链:负载回落/重选/故障转移/PendingWrites 有单测(注入 now / in-memory store);内嵌单二进制从无 dist 目录起服 curl 验证(index/哈希资产/SPA 兜底/缓存头/admin api);SIGTERM 实测日志+1s 内干净退出;故障转移双 worker 活体实测(挂 0 → 转移 1 → 亲和重钉)。
- 「活跃负载」语义是亲和 session 数,不是在途请求/流数(Architect medium,接受):一个 session 开多条长 SSE 只计 1。换真实在途计数留给下阶段(届时需拆亲和表双职责)。
- 负载均衡对「无 session_id」请求不计负载(无亲和记忆,一发即走)。
- worker 运行态(冷却/封禁)仍是内存态,重启即清——优雅停机不改变这一点(既有设计:持久化冷却反而会在重启后误恢复过期冷却;DB 只存配置)。
- embed-ui 与默认 ServeDir 两条路径的缓存头有漂移(ServeDir 无 Cache-Control;Architect low,接受):ServeDir 仅用于开发迭代,生产走 embed。

## [admin-v1] - 2026-06-10

### Features

admin 控制面完整落地(嵌入 router 进程,`/admin` SPA + `/admin/api/*`,单一 `admin.token` 鉴权,常量时间比较):

- **用量看板/用量页**:总览卡、按模型、按客户 apikey 三维度;时间窗(近 7/30 天/全部/自定义起止)+ 按 key 筛选;未归属流量单列桶。
- **API Keys 管理**:列表(掩码+复制)、新建(服务端 `sk-gw-<uuid4>` 生成或导入自定义 key,字符集 `[A-Za-z0-9._~-]{8,128}`)、备注、启停(逐请求查库即时生效)、删除(usage 历史保留)。
- **账号管理(yaml → SQLite)**:`accounts.yaml` 启动幂等导入(只播种,绝不覆盖已 roll 的 token),此后 DB 是配置事实源;admin CRUD;worker 30s 周期 sync——增删改免重启生效;**token 刷新成功先回写 DB**,rolling refresh_token 重启不丢;运行态(冷却剩余/禁用原因/并发占用)由调度器快照经 worker `/health` 暴露,admin `/accounts/runtime` 聚合;凭据响应一律脱敏保尾 4 位。
- **分组**:groups 表(色板/备注/账号数/key 数);账号与 key 都可归组;删组成员转未分组(事务),不级联删。
- **按客户限额(计费 v1)**:`api_keys.quota_tokens/used_tokens`,UsageSink 落库时锁内累加(口径 input+output,未归属不计);鉴权 SQL 内算 `over_quota`,超额回 429 `rate_limit_error`;admin 可设额/清除/重置已用,即时生效。

### Design Rationale

- **DB=配置事实源,worker 内存=运行态**:配置(账号/key/组/限额)进 SQLite 由 admin 管理;冷却/封禁/并发等运行态留在调度器内存经 HTTP 快照暴露——避免高频运行态写库,也避免重启后误恢复过期冷却。
- **sync 翻转语义**:配置 `disabled` 仅在**翻转**时触碰运行态(→false 视为 admin 显式复活)。同值周期 sync 绝不洗掉风控冷却/封禁,防止 30s 轮询反复"救活"被风控的账号。
- **统计读连接分离**:admin 全历史聚合走独立只读连接(WAL 并发),控制面再慢也压不到数据面的鉴权与计费落库。
- **限额单位用 token(input+output)**:v1 求简单可解释;加权成本(cache 折扣/模型价差)留到计费表达式阶段,届时只需替换累加口径。
- **busy_timeout 必须最先设**:router/worker 并发启动同一 WAL 库,否则抢锁直接 "database is locked" 即死。

### Notes & Caveats

- 明文 key 仍兼任身份/PK/usage 归属键:key 轮换、同名重建会合并历史归属。引入稳定 `key_id` 规划在加权计费阶段一并做。
- keys/accounts 列表无分页,客户量大(千级)后需要加。
- 限额检查在鉴权时读、settlement 时写:并发在途请求可少量超额(误差 ≤ 在途并发量),对人为限额场景可接受。
- admin 改账号生效有 ≤30s 的 sync 延迟(UI 已提示)。
- 部署:`admin-ui/dist` 由 router 运行时读取(改前端需 `bun run build`);单二进制内嵌(rust-embed)待做。
