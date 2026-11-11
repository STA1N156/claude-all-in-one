# dario-supervisor

caio 的 dario 侧车：**每个 Claude OAuth 号一个原版（不打补丁）dario 进程**。

## 为什么
dario 从 v5 起把会话 ID、请求节奏、beta 拒绝记录、超额保护等全部按「池里的号」存放。旧做法是一个进程、每个请求临时注入令牌，号对 dario 是匿名的，几百个号挤在同一个共用位置：
- 所有号共用一个上游会话 ID；
- 所有号共用 500ms 请求节奏；
- 一个号超额，整个进程停 30 分钟；
- 还要长期维护一个 fork。

现在每个号一个进程、一个号的池，这些机制天然按号隔离，dario 直接用上游发布版。

## 令牌归属
- caio 登录或导入拿到凭证 → `PUT /accounts/:id` **交接一次** → 管理器写入该号的 `~/.dario/accounts/seat.json` → **dario 成为唯一刷新者**。
- caio 随后清掉自己保存的 refresh token，不再刷新。两方刷新同一条 refresh token 会触发 Anthropic 的复用检测，令牌一起作废。
- 重新登录要用新的 `rev` 再 PUT 一次：管理器先停进程，再替换文件。`rev` 相同的 PUT 永远不会覆盖令牌。
- alias 固定为 `seat`。不能用 `login`：dario 会在每次启动时从 `~/.dario/credentials.json`、`~/.claude/.credentials.json`、系统钥匙串回填，可能换成别的号的令牌。

## 进程
- 启动命令：`bun <dario>/dist/cli.js proxy --port=<p> --host=127.0.0.1 --no-live-capture --session-per-client --no-ledger [--upstream-proxy=<该号出口>]`。
- 续流（mid-stream continuation）保持 dario 默认开启：上游掐断长流时自动把已产出部分作为上下文、回本号原模型续完并缝进同一条客户端流。**不得**再加 `--no-midstream-continue`（2026-10-12 事故：opus-5.5 长流 ~4 分钟被掐，客户端只见残流，表现为「请求永远发不出去」）。
- `--upstream-proxy` 会替换全局 fetch，所以令牌刷新、心跳、模型目录、对话都从这个号自己的出口 IP 发出。
- 环境变量只给白名单：`HOME`、`PATH`、`DARIO_API_KEY`、`DARIO_IGNORE_CC_CREDENTIALS=1`、`DARIO_NO_LIVE_CAPTURE=1`、`DARIO_MAX_BODY_MB`。
- **按需启动，闲置停止**（默认 30 分钟）。dario 每 5 秒发一次在线心跳且不能关，常驻 24 小时不像真人；停着的号下次启动时会先刷新令牌再开端口。
- 启动即崩溃时按指数退避（1s 到 60s）。注册端口被占用时**拒绝启动**，不换端口，防止同一个号出现两个进程。
- 每 30 秒轮询运行中进程的 `GET /accounts`：出现 `action=regrant`，或令牌过期超过 20 分钟，就判定需要重新登录，停掉进程，之后 `ensure` 返回 409。

## 接口（只监听回环，`x-api-key` 鉴权，`/livez` 免鉴权）
| 方法 | 路径 | 说明 |
|---|---|---|
| PUT | `/accounts/:id` | `{rev, accessToken, refreshToken, expiresAt(ms), deviceId, accountUuid, grantedAt?, proxy?}` → `created` / `replaced` / `unchanged` |
| PATCH | `/accounts/:id` | `{proxy}`：停进程，下次启动生效 |
| POST | `/accounts/:id/ensure` | 返回 `{port}`；404 未登记，409 需要重新登录，503 启动失败或退避中（带 retry-after） |
| DELETE | `/accounts/:id` | 停进程并删除目录 |
| GET | `/accounts[/:id]` | 状态（不含令牌）；`seat` 字段是 dario 自报的额度、过期、action |

## 环境变量
| 变量 | 默认值 | 说明 |
|---|---|---|
| `DARIO_API_KEY` | （必填，≥16 字符） | 管理器鉴权与各 dario 进程共用 |
| `SUPERVISOR_HOST` / `SUPERVISOR_PORT` | `127.0.0.1` / `8788` | |
| `DARIO_CLI` | `/opt/dario/dist/cli.js` | 上游 tag 原样构建的 dist |
| `DATA_DIR` | `/data` | `registry.json` 与各号 HOME；有令牌，权限 0700，需纳入备份 |
| `PORT_MIN` / `PORT_MAX` | `18800` / `18999` | |
| `IDLE_STOP_MS` | 30 分钟 | |
| `START_TIMEOUT_MS` | 15s | |
| `HEALTH_POLL_MS` | 30s | |
| `STOP_TIMEOUT_MS` | 95s | 容器的 `stop_grace_period` 要大于它 |

## 测试
```sh
bun test                                           # 用假 dario，覆盖交接 / 生命周期 / 死号判定
REAL_DARIO_CLI=<上游 dist>/cli.js bun test         # 另跑一遍真实 dario 联调（不访问 Anthropic）
```

## 升级 dario
在 dario-caio 仓库检出上游新 tag → `npm ci && npm run build` → 把 dist 同步到服务器的 `/root/dario-upstream` → 重启侧车容器。不需要合并任何 fork。
