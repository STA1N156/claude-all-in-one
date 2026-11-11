# Cursor MCP 工具调用帧:field 15 → field 44(2026-10-08 实测钉死)

> 背景:caio 的 cursor wire 路径(`agent.v1.AgentService/Run`)带工具请求 10-03 起
> 44 次调用 0 次交付,上游先发帧、网关当「内建工具收口」截断。本篇是静态分析 +
> 139 上两次实弹探针的结论。**未改动任何 Rust 代码**(`run.rs`/`chat.rs` 原样未动),
> 本文件本身也不提交 git。

## 结论先行

`run.rs::parse_tool_call` 认的外部/MCP 工具身份字段号 `TC_EXTERNAL = 15`
(`1.2.2.15`)已经**不是**上游现在发的字段号。2026-10-08 用 `cargo` 交叉编译
(`x86_64-unknown-linux-musl`,静态二进制)把 `examples/probe_wire_v2.rs` 的 `mcp`
模式(广告 `get_weather`,强制单轮调用)原样搬到 139 上,**经账号 `测试号` 自己的
`extra.proxy`**(未直连)打了 2 次真实请求,两次拿到的工具调用帧字节逐位相同:

```
payload.1                 (RESP_MESSAGE)
payload.1.2                 (RESP_TOOL_CHANNEL,字段号不变,仍是 2)
payload.1.2.1                 (TC_CALL_ID,不变,仍是 1) = "call-<uuid>-0\nfc_<uuid>_0"
payload.1.2.2                 (TC_DETAIL,不变,仍是 2)
payload.1.2.2.44                 ← ⭐ 新身份字段,旧代码认的是 .15
payload.1.2.2.44.1                  内层包装(等于旧 .15.1)
payload.1.2.2.44.1.1                   = "claude-local"   ← 命名空间(旧结构里这是 .4/.9)
payload.1.2.2.44.1.2                   = "get_weather"    ← 裸名(旧结构里这是 .5)
payload.1.2.2.44.1.4                   = call_id 字符串(与 1.2.1 同值)
payload.1.2.2.57                 = call_id(重复,**字段号不变**)
payload.1.2.2.59                 = varint 毫秒时间戳(**字段号不变**)
payload.1.2.3                 = 另一个 id(**字段号不变**,形如 <uuid>-0-<4字符>)
payload.1.25                 = varint 毫秒时间戳(**字段号不变**)
```

两次探针(不同 conversation_id,`default` 模型)字节结构 100% 一致,帧长均为
349B。与 10-08 生产 forensics(`model=grok-4.5`,帧长 351/362B,preview 里能看到
同样的 `claude-local` + `read_file`/空 字样)高度吻合——**production 日志里被当
「内建工具收口」的那一帧,就是这一帧**,不是别的新帧种。

## 这意味着什么

1. **外层包装(`1.2.1` call_id / `1.2.2` detail / `.57` / `.59` / 顶层 `.25`)全部
   没变**,只有身份字段号从 `15` 挪到了 `44`。
2. **内层字段语义也变了,不只是外层编号**:旧文档记的 `.15.1 = {1:全名,
   2:[参数], 3:call_id, 4/9:命名空间, 5:裸名}`;实测 `.44.1` 里:
   - `1` = 命名空间(旧的 4/9)
   - `2` = 裸名(旧的 5)
   - `4` = call_id(旧的 3,且与外层 `1.2.1`/`.57` 三处同值)
   - **没见到参数字段**(两次探针模型都是「先探 `get_weather` 有什么参数、还没
     填 `city` 就先调了一次空参调用」,这是 grok/default 模型这次的行为选择,
     不是协议限制——探针没有回执 `tool_result`,模型没有继续给出带参的第二次
     调用。参数大概率仍在某个字段号上,只是这两次没触发,**没有实证**,
     不要直接假定是 `3`)。
3. `TC_BARE_NAME` 的取值位置和旧代码假设的字段号(`5`)也不对了——旧代码在
   `.44` 这个新身份下完全没有匹配分支,所以 `parse_tool_call` 直接返回
   `None`;接着落到 `parse_builtin_call`,它的 match 分支是 `1/4/12/37`,`44`
   也不在其中,于是落到 `chat.rs` 最后的「Cursor 内建工具(我们无法执行)」
   警告分支——与生产现象完全对得上。

## 一个次要观察(不是本次根因,但值得记一句)

探针里还见过一类**顶层直接是 `field 2`(不经 `field 1` 包一层)**的小帧
(81~102B),内容是 `request_context`(`.10`=conversation_id、`.19`=
span_context、`.55`=0)或一个只带 `claude-local` 命名空间、没有裸名的变体
(`.1`=varint 1、`.36.1`="claude-local")。这类帧结构上也会让
`run::is_tool_call()` 判真(它对顶层 `field 2` 单独判真),但 `parse_tool_call`
/`parse_builtin_call` 两者都**要求先剥一层顶层 `field 1`**,对这类帧天生拿不到
东西。从帧长(81~102B)和生产 forensics 的帧长(351/362B)对比看,**production
日志里截断的那一帧不是这类帧**,是上面 `.44` 那一类——所以这条是陪驾,不是
主凶,但如果将来按下面的修法把 `.44` 接上之后生产日志仍偶发同类警告,这是
第二个该查的地方(`is_tool_call` 判太宽,把 session_notice/exec 回声也算了
进来,`is_session_notice` 本来就是为这类帧写的,但 `parse_tool_call` 的调用点
在 `chat.rs` 里有没有先过一道 `is_session_notice` 需要再核对)。

## 2026-10-08 补测:参数字段号(3 次实测,结论不完整——找到的是错误通道,不是参数本身)

团队要求补一次「模型确实带参数调用」的实测,钉死 `.44.1` 参数字段号。用了 3 个请求
(`测试号` 自己的 `extra.proxy`,未直连):

1. `mcpargs` turn1:工具 `get_weather(city: string 必填, days: integer 必填)`,
   prompt 强制「立刻调用,city="Tokyo"、days=3,不要先探参数」。
2. `mcpargs` turn2:同会话第二轮,给模型「演示过一次成功调用」的伪造上下文,
   逼它模仿同样模式换参数再调一次(city="Shanghai"、days=5)。
3. `mcpargs1`(单独一次新会话):同 turn1 prompt,额外对 `inner_fields` 含 `5`
   或 `7` 的帧(此前从未深挖过的号)也做递归 dump。

**三次结果一致**:最终交付的 `.44.1` 识别帧(`inner=[2,25]`)**参数始终为空**——
只有 `1`(命名空间)/`2`(裸名)/`4`(call_id)三个字段,没有任何第 5 个字段。
**没有钉死参数字段号**,但钉死了一件更关键的事:

### 发现一条此前完全没见过的「校验拒绝」通道:`inner=7`

第 3 次实测(`mcpargs1`)在工具调用帧(`.44`,`inner=2`)**之前**,先收到一个
295B、`inner=[7,25]`(即顶层 `1.7`)的帧,结构是:

```
payload.1.7.1                    = call_id 字符串(与最终 .44 帧的 call_id 同值)
payload.1.7.2                       ← 包装形态与 .44 完全同构(同样走 TC_DETAIL→.44)
payload.1.7.2.44.1.4                 = 字符串 "unknown-tool-call-id"   ← 不是真 call_id,是占位值
payload.1.7.2.44.2.2.1                = 字符串 "Invalid arguments"     ← 人类可读错误信息
payload.1.7.2.57                    = call_id(重复,真实值)
payload.1.7.2.59                    = varint 毫秒时间戳
payload.1.7.4                     = 另一个 id(形如 <uuid>-0-<4字符>,同 .2.3)
```

即:**服务端自己校验过模型提议的参数,判定「Invalid arguments」,然后把这次尝试的
参数清空,只把裸的工具名+call_id 转交给我们**(紧随其后的 `.44` 帧里参数确实是空
的,且与这个错误帧共享同一个真实 call_id)。这解释了为什么 3 次独立尝试、2 种不同
的强制措辞,参数都是空——**不是协议里没有参数字段,是服务端在我们这次注册的工具
schema 下,始终判定模型给的参数非法,校验失败后清空参数再转发**,我们看到的「空
`.44.1`」是校验失败的**后果**,不是原始形态。

另外还见到一个 15B、`inner=[5,25]`(顶层 `1.5`)的小帧,结构是 `1.5.1 = varint 4673`
——数值像某种状态码/重试计数,语义未钉死,这次没有继续深挖(预算已用完)。

### 没弄清楚、下一步该做什么(需要你拍板是否值得再烧请求)

1. **校验失败的根因未定位**:可能是 (a) 我们这次用的 `google.protobuf.Value`
   包 `Struct` 的 JSON Schema 编码本身没问题,但模型给的 `days` 类型(很可能是
   数字 `3` 的某种 wire 表达,或模型干脆给了字符串 `"3"`)没过服务端的严格类型
   校验;(b) 我们注册 schema 时某个必填字段/类型写法服务端不认(可以换成全
   `string` 类型的最简 schema 再试一次,排除类型校验);(c) 两者都不是,是 `44`
   这个新身份号段下服务端对"带参数的外部工具"本身就有更严格的平台侧收紧
   (呼应 `docs-cursor-protocol-re-2026-08-23.md` §附2 记录的 InferenceService
   面"带 parameters 必拒"现象,但那是另一个 RPC——如果 AgentService/Run 也在收紧,
   这是个更大的结构性变化,值得专门起一次实测)。
2. **验证方法**(都没做,各占 1 个请求):
   - 全 `string` 类型最简 schema(两个参数都是 `string`)再测一次,看校验是否通过、
     参数会不会出现在某个新字段号上。
   - 不强制「立刻调用」,放宽让模型自己多轮探参数(类似最早 `mcp` 模式),看是否
     也会撞见同一条 `Invalid arguments` 通道,还是只有"强制立刻带参"才触发。
3. 在没有拿到一次**校验通过、参数非空**的真实帧之前,**不要**假定参数字段号是
   `3`/`5`/`6` 中的某个值并据此改 `run.rs`——猜错的后果是参数被张冠李戴地喂给
   调用方工具,比现在的"空参收口"更隐蔽也更危险。

## 对 `run.rs` / `chat.rs` 的改法建议(未应用,等你定)

`run.rs`:
- `TC_EXTERNAL` 从硬编码 `15` 改成同时接受 `15` 与 `44`(两者都试,`44` 优先,
  `15` 当历史兼容/其他模型可能仍用旧号的后备分支——这次只实测了
  `default`/`grok-4.5` 两个模型,没有把全部模型矩阵都测过,不能排除有模型还在
  走 `15`)。
- `.44.1`(或统一后叫 `TC_EXTERNAL_INNER`)内部字段按实测改:
  `1→命名空间`、`2→裸名`、`4→call_id`;**参数字段号仍是 TODO,且比预想复杂**——
  2026-10-08 补测 3 次(见上面新增小节)只拿到「服务端判模型参数 Invalid
  arguments → 清空参数再转发」的证据,没拿到一次校验通过、参数非空的真实帧。
  接 `.44` 之前不要直接猜一个数字填参数字段——猜错的后果比收口还差(参数值被
  张冠李戴地喂给调用方工具);先按上面小节的「验证方法」用全 `string` 类型
  schema 再测一次,拿到非空参数帧后再定字段号。
- `parse_builtin_call` 不用改:它的身份枚举(`1/4/12/37`)是 Cursor **内建**
  工具,和 MCP 外部工具是两套独立的身份空间,`44` 不会和它们冲突,但也不该
  塞进它的 match 里。

`chat.rs`:
- 3505~3600 行那段 `match run::parse_tool_call(&payload)` 的结构不用大改,
  只要 `run.rs` 把 `44` 接上,现有的「成功→转 `tool_use`」分支会自动走通,
  不需要在 `chat.rs` 侧加判断。
- 建议在 `parse_tool_call` 识别成功路径上加一条一次性 `tracing::info!`,记一下
  这次命中的是 `15` 还是 `44`(或者做成一个枚举返回值),方便上线后用日志
  确认「新号段真的在生效,不是旧号段继续凑巧没撞上」。

## 复现方法(供你验证结论,不建议再烧额度)

- 探针源码已挪出仓库,存在 `/tmp/claude-1000/-home-iiap----self-work/73630d27-493c-476f-b30d-8f0a672611bd/scratchpad/re/probe-src/probe_322_toolcall.rs`
  (基于 `examples/probe_wire_v2.rs` 复制,只改了两处:`read_token()` 改读环境变量
  `CURSOR_PROBE_TOKEN`、`build_client()` 支持环境变量 `CURSOR_PROBE_PROXY` 走代理;
  另加了一个递归 `deep_dump()` 诊断函数)。仓库里的 `crates/gw-cursor/examples/`
  目录已还原,没有残留改动。
- 交叉编译:`docker run --rm -v $PWD:/work -w /work rust:1.95-alpine sh -c
  'apk add musl-dev; cargo build --release --target x86_64-unknown-linux-musl
  -p gw-cursor --example probe_322_toolcall'`(139 没有 cargo,本机 glibc 比
  139 新,必须 musl 静态链接才能直接 scp 过去跑)。
- 跑法:`CURSOR_PROBE_TOKEN=<测试号 的 access_token> CURSOR_PROBE_PROXY=<测试号
  的 proxy url> ./probe_322_toolcall <mode>`,`mode` 用过 `mcp`(1 请求)、
  `mcpargs`(2 请求,强制带参 + 演示上下文再调一次)、`mcpargs1`(1 请求,
  同 mcpargs turn1 但额外深挖 `inner_fields` 含 5/7 的帧)。
- 三轮实测用的凭据文件(`服务器上的临时凭据文件`,600 权限)、二进制
  (`/root/probe_322_toolcall`)、stdout/stderr 日志(`/root/.probe*.log`)
  已在 139 上 `shred -u` 清理,没有留存。

## 本次没做、但如果要继续攻坚该做的事

1. ~~补一次「模型确实带参数调用」的实测~~ ✅ 已做(见上面小节),但结论是
   「服务端校验拒绝、参数被清空」,**没有拿到参数字段号本身**——真要钉死它,
   下一步是上面小节"验证方法"里的全 `string` schema 实测,还没做。
2. 把 `.44` 这个号段在 `claude-opus-5` / `claude-sonnet-5` / `composer-2.5` 上
   各测一次,确认不是某个模型专属的路由差异。
3. 核对 `chat.rs` 里 `is_tool_call()` 判真之后、落进 `parse_tool_call` 之前,
   有没有先过滤掉 `is_session_notice()` 形态的帧——「次要观察」那条如果以后
   复发,从这里查。

## 关于 Cursor IDE 3.22 .deb 的一句交代

团队交接时让我顺手比对一下下载好的 `cursor_3.22_amd64.deb`(IDE 桌面版)。
查了一下发现这条线基本是岔路:**生产 `clidrv` 驱动的 CLI 二进制版本是
`2026.08.11-e8db854`**(139 上 `/opt/cursor-agent/cursor-agent --version` 实测),
与「IDE 3.22」是两个独立发行、独立版本号的产物,10-03 到 10-08 期间没有任何
升级记录。既然 CLI 二进制没变而行为变了,根因只能是**服务端**协议变更
(`agentn.*.cursor.sh` 后端换了帧号),和本机装的是 IDE 3.14 还是 3.22 无关——
本篇的结论也确实是直接从「打真实后端」拿到的,不依赖 .deb 里的任何代码。
deb 已解包在 `/tmp/.../scratchpad/re/cursor322/`(`extensions/cursor-agent-host/
dist/agent-host-daemon/dist/bin/daemon.cjs` 里能查到 `McpTool*`/`ToolCall*` 一
大堆符号名,但没找到这次用到的 protobuf 字段号定义,这条线索价值有限,
没有继续深挖)。
