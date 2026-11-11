# Claude All in One

多进程 Claude 网关:对外暴露 Anthropic Messages API,对内把请求调度到一组上游账号(当前 Provider 为 Kiro/CodeWhisperer,架构上可插其他 Provider)。三个核心目标:

- **缓存命中** —— 会话两级亲和(session→worker→账号),同一会话始终落在同一出口、同一账号,最大化上游 prompt cache;
- **防关联** —— 每个 worker 进程绑定独立出口(本机多 IP / 代理)+ 账号 machineId 冻结 + 报文逐字对齐官方客户端;
- **可运营** —— 内嵌 admin 控制面(账号/分组/key/用量),双口径计费统计(真实 vs 模拟缓存)。

## 总体架构

单二进制 `claude-all-in-one`,按 `--mode` 分两种角色,多进程部署:

```mermaid
flowchart TB
    CC["客户端(Claude Code / Anthropic SDK)<br/>x-api-key = client key"]

    subgraph ROUTER["Router 进程 :8990(单实例)"]
        AUTH["client key 鉴权 + 限额"]
        SID["session_id 派生<br/>metadata.user_id → 内容 hash → 随机兜底"]
        PIN["会话 → worker 亲和<br/>同会话钉同 worker"]
        ADMIN["Admin API + 内嵌 UI<br/>accounts / groups / keys / usage / import"]
    end

    subgraph WORKER["Worker 进程 ×N :9000+i —— 每个 = 一个账号组 + 一个固定出口"]
        SCHED["AccountScheduler<br/>组内账号亲和(v52)+ 冷却 + LRU<br/>+ 模型能力过滤(FREE 不接 opus)"]
        PROV["KiroProvider(Provider trait)<br/>报文转换 / machineId 冻结 / token 刷新<br/>profileArn 自动发现 / 图像压缩 / cache_sim 计费"]
        EGRESS["Egress 固定出口<br/>direct / 本机 IP 绑定 / proxy"]
    end

    DB[("SQLite(gw-store)<br/>accounts / groups / keys / usage")]

    subgraph UP["Kiro / AWS 上游"]
        RT["runtime.*.kiro.dev<br/>chat(generateAssistantResponse)"]
        Q["q.*.amazonaws.com<br/>配额 / profile 发现"]
        OIDC["oidc.*.amazonaws.com(IdC 刷新)<br/>prod.*.auth.desktop.kiro.dev(social 刷新)"]
    end

    CC -->|"POST /v1/messages"| AUTH --> SID --> PIN
    PIN -->|"内网转发"| SCHED --> PROV --> EGRESS
    EGRESS --> RT
    EGRESS --> Q
    EGRESS --> OIDC
    ADMIN --- DB
    WORKER ---|"30s 账号同步 / usage 落库"| DB
```

**为什么 router/worker 分进程**:出口 IP、账号组、上游连接池都按 worker 隔离 —— 一个 worker 内的账号永远从同一出口发包,不同账号组之间互不关联;router 只做鉴权与会话粘连,无上游状态,可独立重启。

## 一次请求的生命周期

```mermaid
sequenceDiagram
    participant C as 客户端
    participant R as Router :8990
    participant W as Worker :900x
    participant K as Kiro 上游

    C->>R: POST /v1/messages(client key)
    R->>R: 鉴权 + 派生 session_id / cache_key
    R->>W: 按会话亲和转发到固定 worker
    W->>W: scheduler.acquire_where(账号亲和 + 模型能力过滤)
    W->>W: token 刷新(dirty 协议)+ profileArn 首次自动发现
    W->>W: Anthropic→Kiro 报文转换 + 图像压缩
    W->>K: generateAssistantResponse(固定出口 + 冻结 machineId)
    K-->>W: 上游事件流
    W->>W: 解析为 Anthropic SSE(stop_reason 状态机 / thinking 签名)<br/>cache_sim 计费 + usage 落库
    W-->>R: SSE 透传(非流式则折叠为单 JSON)
    R-->>C: 响应
```

## Crate 结构

```mermaid
flowchart BT
    core["gw-core<br/>Provider trait / Account / 配置<br/>路由派生 / 非流式折叠 / Store trait"]
    kiro["gw-kiro<br/>Kiro Provider:converter / chat / headers<br/>token / profiles / image / cache_sim / usage"]
    sub["gw-claude-subprocess<br/>Claude CLI 子进程 Provider(预留)"]
    store["gw-store<br/>SQLite(WAL)持久化"]
    app["gw-app<br/>二进制:router + worker + admin + egress"]

    kiro --> core
    sub --> core
    store --> core
    app --> kiro
    app --> store
    app --> sub
    app --> core
```

| Crate | 职责 |
|---|---|
| `gw-core` | 与 Provider 无关的地基:`Provider` trait、账号模型、`SchedulerConfig` 等配置、session/cache key 派生、流式→非流式折叠。写一个 Provider 实现即可插一个 worker。 |
| `gw-kiro` | Kiro 上游全量实现:Anthropic↔Kiro 报文转换、防封报文头(machineId 冻结)、IdC/social 双轨 token 刷新、`ListAvailableProfiles` 动态 profileArn 发现、KiroManager 完整导入、图像压缩(OOM 护栏)、模拟缓存计费。 |
| `gw-store` | SQLite 持久化:账号/分组/client key/双口径用量统计。 |
| `gw-app` | 可执行文件:`--mode router`(鉴权 + 会话粘连 + admin)/ `--mode worker --instance N`(调度 + 上游)。admin UI 构建产物内嵌进二进制。 |
| `gw-claude-subprocess` | 拉起官方 Claude CLI 子进程的 Provider(NDJSON 事件流),预留。 |

## API 面

| 端点 | 角色 | 说明 |
|---|---|---|
| `POST /v1/messages` | router→worker | Anthropic Messages,流式/非流式 |
| `POST /v1/messages/count_tokens` | router | 本地估算,不打上游 |
| `GET /v1/models` | router→worker | 模型列表 |
| `/admin/api/*` | router | accounts / groups / keys / usage / import / reset(admin token 鉴权) |
| `GET /health` | both | worker 版含各账号运行态 + 配额缓存 |

## Zeabur 一键部署

1. Zeabur 新建项目,选择 **GitHub → 部署此仓库**。
2. 保持默认设置,Zeabur 会自动使用根目录 `Dockerfile`,只需把 HTTP 端口设为 `8080`。
3. 部署完成后打开 `https://你的域名/admin`。默认密码是 `123456`。
4. 登录后先添加 Kiro 账号,再创建客户端密钥即可使用 `/v1/messages`。

如需修改管理密码,在 Zeabur 添加环境变量 `ADMIN_TOKEN` 并重新部署。建议给服务挂载一个持久化卷到 `/app/data`,用于保留账号、密钥和统计;不挂载也能运行,但重新部署后数据会清空。

## 思考模式

普通模型名默认不主动开启思考，包括 Opus。加 `-thinking` 或 `-reasoning` 后缀可开启，默认强度固定为 `high`。客户端显式的 `thinking`、`reasoning_effort` 或 `reasoning.effort` 优先于后缀，预算不会被自动抬高。关闭时不发送上游强度字段；5.5 等自带思考的模型按上游默认行为处理。

设置页的「本地前缀缓存」只需配置开关、有效期（默认 5 分钟，1–10080 分钟）和最高命中率（默认 90%，0–100%，支持小数）。关闭后使用上游缓存数据，旧版积分反推、缓存倍率与保底比例不再影响本地命中结果。

## 演进记录

版本级变更与设计取舍见 [CHANGELOG.md](CHANGELOG.md)。
