//! 子机侧:主动连主机 hub,在这条连接上当 HTTP/2 服务端,把主机的请求转给本机。
//!
//! 只放行五类路径,其余一律 403:
//! - `/sup/accounts[/<id>[/ensure]]` → 本机 dario 管理器(交接、改状态、删号),注入本机 key;
//! - `/a/<id>/v1/...` → 先在本机 `ensure` 拿端口,再流式转给该号的 dario 进程(省一个往返,
//!   不把端口暴露给主机);`ensure` 失败时原样回传管理器的状态码并带 [`HDR_STAGE`];
//! - `/oauth/exchange` → 用**本机 IP** 直连换码(铸 token IP = 将来发包 IP);
//! - `/info` → 版本、负载、管理器里各号的状态;
//! - `/exec` → 远端 bash(后台「子机」页的终端)。以 caio 用户跑,限时限量,环境变量清空——
//!   本进程的环境里有 CAIO_TOKEN / DARIO_API_KEY,不能泄进命令输出。

use std::convert::Infallible;
use std::os::unix::process::CommandExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use gw_core::node::{HDR_STAGE, STAGE_ENSURE};
use http_body_util::{BodyDataStream, BodyExt};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::tunnel::{self, Hello, Welcome};
use crate::{filter_headers, now_unix};

/// dario 管理器给每号进程分配的端口段(与 `sidecars/dario-supervisor` 的默认值一致)。
pub const PORT_MIN: u16 = 18800;
pub const PORT_MAX: u16 = 18999;
const SUP_TIMEOUT: Duration = Duration::from_secs(120);
const ENSURE_TIMEOUT: Duration = Duration::from_secs(60);
/// 本机 dario 进程**回响应头**的上限(响应体是流,不限)。
///
/// ensure 已经确认进程在跑,所以这之后再不回头就是它卡住了。不设的话只能等 hub 那边的
/// 150s(`CHAT_HEAD_DEADLINE`),而 hub 超时给的是 `Lost` —— 请求可能已执行、不能换号重发,
/// 等于白等 150 秒再报废一条请求。本机先失败则是 502,主机能按「没发出去」换号。
const CHAT_HEAD_TIMEOUT: Duration = Duration::from_secs(100);
const MAX_SUP_BODY: usize = 1024 * 1024;
const MAX_OAUTH_BODY: usize = 64 * 1024;
/// exec:命令体上限(JSON 整体)。要比命令文本上限大:32K 字符全是 3 字节 UTF-8 时约 96KB,
/// 再留 JSON 外壳的余量 —— 不然上限内的命令会在 JSON 层先被 413。
const MAX_EXEC_BODY: usize = 192 * 1024;
/// exec:stdout / stderr 各自保留的上限;超出的部分丢弃但继续读(不然子进程写满管道会卡死)。
const MAX_EXEC_OUTPUT: usize = 256 * 1024;
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// 被拒(令牌错、已吊销、绑定了别的机器)时的退避:重试没用,但也不彻底退出,
/// 好让运维在后台修好后它自己恢复。
const BACKOFF_FATAL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// 主机 hub 的 `host:port`。
    pub master: String,
    pub spki: [u8; 32],
    pub node_id: String,
    pub secret: String,
    pub install_id: String,
    /// 本机 dario 管理器地址。
    pub sup_url: String,
    /// 本机 dario 管理器与各 dario 进程的 key(安装时生成,只在子机上)。
    pub dario_key: String,
    pub version: String,
}

enum ConnectError {
    Fatal(String),
    Retry(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for ConnectError {
    fn from(e: E) -> Self {
        ConnectError::Retry(e.into())
    }
}

/// 一直运行:断了就退避重连。
pub async fn run(opts: AgentOptions) -> anyhow::Result<()> {
    let ctx = Arc::new(AgentCtx::new(&opts)?);
    let connector = TlsConnector::from(Arc::new(crate::tls::client_config(opts.spki)?));
    let mut backoff = BACKOFF_MIN;
    loop {
        let started = Instant::now();
        let wait = match connect_and_serve(&opts, &connector, ctx.clone()).await {
            Ok(()) => {
                tracing::warn!(master = %opts.master, "与主机的连接已结束,准备重连");
                backoff
            }
            Err(ConnectError::Fatal(msg)) => {
                tracing::error!(master = %opts.master, reason = %msg, "主机拒绝了本子机(重试也没用,请在后台检查令牌/绑定)");
                BACKOFF_FATAL
            }
            Err(ConnectError::Retry(e)) => {
                tracing::warn!(master = %opts.master, error = %e, "连接主机失败");
                backoff
            }
        };
        // 稳定连过一阵子再断的,下次从最短退避开始。
        backoff = if started.elapsed() > Duration::from_secs(60) { BACKOFF_MIN } else { (backoff * 2).min(BACKOFF_MAX) };
        tokio::time::sleep(wait + jitter(wait)).await;
    }
}

fn jitter(base: Duration) -> Duration {
    let r = uuid::Uuid::new_v4().as_bytes()[0] as u64;
    Duration::from_millis(base.as_millis() as u64 * r / 1024)
}

async fn connect_and_serve(opts: &AgentOptions, connector: &TlsConnector, ctx: Arc<AgentCtx>) -> Result<(), ConnectError> {
    let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&opts.master))
        .await
        .map_err(|_| anyhow::anyhow!("连接超时"))??;
    let _ = tcp.set_nodelay(true);
    let name = rustls::pki_types::ServerName::try_from(crate::tls::HUB_SERVER_NAME)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (tls, welcome) = tokio::time::timeout(tunnel::HANDSHAKE_TIMEOUT, async {
        let mut tls = connector.connect(name, tcp).await?;
        let hello = Hello {
            proto: tunnel::PROTO,
            node_id: opts.node_id.clone(),
            secret: opts.secret.clone(),
            install_id: opts.install_id.clone(),
            version: opts.version.clone(),
            hostname: hostname(),
        };
        tunnel::write_frame(&mut tls, &hello).await?;
        let w: Welcome = tunnel::read_frame(&mut tls).await?;
        anyhow::Ok((tls, w))
    })
    .await
    .map_err(|_| anyhow::anyhow!("握手超时"))??;
    if !welcome.ok {
        let msg = welcome.error.unwrap_or_else(|| "主机拒绝".into());
        return Err(if welcome.fatal { ConnectError::Fatal(msg) } else { ConnectError::Retry(anyhow::anyhow!(msg)) });
    }
    tracing::info!(master = %opts.master, name = ?welcome.node_name, "已连上主机");
    let svc = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
        let ctx = ctx.clone();
        async move { Ok::<_, Infallible>(ctx.handle(req).await) }
    });
    hyper::server::conn::http2::Builder::new(TokioExecutor::new())
        .timer(TokioTimer::new())
        .keep_alive_interval(tunnel::KEEPALIVE_INTERVAL)
        .keep_alive_timeout(tunnel::KEEPALIVE_TIMEOUT)
        .initial_stream_window_size(tunnel::NODE_STREAM_WINDOW)
        .initial_connection_window_size(tunnel::NODE_CONN_WINDOW)
        .max_concurrent_streams(256)
        .serve_connection(TokioIo::new(tls), svc)
        .await?;
    Ok(())
}

pub struct AgentCtx {
    sup_url: String,
    dario_key: String,
    /// 审计日志用(exec 记录是哪台子机执行的)。
    node_id: String,
    version: String,
    /// 子机侧的 exec 总开关:`CAIO_EXEC=0` 关掉(机器的主人可以决定不开放远端 bash)。
    exec_enabled: bool,
    started: Instant,
    /// 本机回环(管理器 / dario 进程):不走任何系统代理。
    local: reqwest::Client,
    /// 换码:**本机 IP 直连**。workspace 开了 system-proxy,必须显式 `no_proxy`,
    /// 否则环境里的代理变量会让铸 token 的 IP 与 dario 发包的 IP 不一致。
    oauth: reqwest::Client,
}

impl AgentCtx {
    pub fn new(opts: &AgentOptions) -> anyhow::Result<Self> {
        // 本机回环(管理器 / dario 进程):不走任何系统代理。
        // 不留空闲连接池:管理器(bun)对 keep-alive 空闲连接关闭得很凶,复用到死连接
        // 会让 POST 0ms 瞬间失败且难以排查(2026-10-12)。回环建连亚毫秒,不值得池化。
        let local = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(0)
            .build()?;
        let oauth = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            sup_url: opts.sup_url.trim_end_matches('/').to_string(),
            dario_key: opts.dario_key.clone(),
            node_id: opts.node_id.clone(),
            version: opts.version.clone(),
            exec_enabled: std::env::var("CAIO_EXEC").map(|v| v.trim() != "0").unwrap_or(true),
            started: Instant::now(),
            local,
            oauth,
        })
    }

    pub async fn handle(&self, req: hyper::Request<Incoming>) -> Response {
        let path = req.uri().path().to_string();
        let query = req.uri().query().map(str::to_string);
        if path == "/info" && req.method() == Method::GET {
            return self.info().await;
        }
        if path == "/exec" && req.method() == Method::POST {
            return self.exec(req).await;
        }
        if path == "/oauth/exchange" && req.method() == Method::POST {
            return self.oauth_exchange(req).await;
        }
        if let Some(rest) = path.strip_prefix("/sup/") {
            if sup_path_allowed(rest) {
                return self.sup_forward(req, rest, query.as_deref()).await;
            }
        }
        if let Some(rest) = path.strip_prefix("/a/") {
            if let Some((id, tail)) = rest.split_once('/') {
                if safe_segment(id) && tail.starts_with("v1/") && tail.split('/').all(safe_segment) {
                    return self.chat(req, id, tail, query.as_deref()).await;
                }
            }
        }
        (StatusCode::FORBIDDEN, "forbidden").into_response()
    }

    async fn sup_forward(&self, req: hyper::Request<Incoming>, rest: &str, query: Option<&str>) -> Response {
        let method = req.method().clone();
        let ctype = req.headers().get("content-type").cloned();
        let body = match http_body_util::Limited::new(req.into_body(), MAX_SUP_BODY).collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
        };
        let url = match query {
            Some(q) => format!("{}/{rest}?{q}", self.sup_url),
            None => format!("{}/{rest}", self.sup_url),
        };
        let mut rb = self
            .local
            .request(method, url)
            .header("x-api-key", &self.dario_key)
            .timeout(SUP_TIMEOUT)
            .body(body);
        if let Some(ct) = ctype {
            rb = rb.header("content-type", ct);
        }
        match rb.send().await {
            Ok(resp) => small_response(resp).await,
            Err(e) => (StatusCode::BAD_GATEWAY, format!("子机上的 dario 管理器不可达: {e}")).into_response(),
        }
    }

    async fn chat(&self, req: hyper::Request<Incoming>, id: &str, tail: &str, query: Option<&str>) -> Response {
        let port = match self.ensure(id).await {
            Ok(p) => p,
            Err(resp) => return resp,
        };
        let url = match query {
            Some(q) => format!("http://127.0.0.1:{port}/{tail}?{q}"),
            None => format!("http://127.0.0.1:{port}/{tail}"),
        };
        let (parts, body) = req.into_parts();
        let mut headers = parts.headers;
        filter_headers(&mut headers);
        headers.remove("host");
        headers.remove("authorization");
        headers.remove("x-api-key");
        let stream = BodyDataStream::new(body);
        let resp = self
            .local
            .request(parts.method, url)
            .headers(headers)
            .header("x-api-key", &self.dario_key)
            .body(reqwest::Body::wrap_stream(stream))
            .send();
        match tokio::time::timeout(CHAT_HEAD_TIMEOUT, resp).await {
            Ok(Ok(resp)) => {
                let mut out = Response::builder().status(resp.status().as_u16());
                if let Some(h) = out.headers_mut() {
                    *h = resp.headers().clone();
                    filter_headers(h);
                    // `x-caio-node` / `x-caio-stage` 只能由 hub / agent 自己写:dario 的回包里若
                    // 带上它们,主机会误判「请求没发出去」换号重发(烧两份额度),或把 dario 的
                    // 错误当成「子机基础设施故障」而不计入账号健康。
                    h.remove(gw_core::node::HDR_NODE);
                    h.remove(gw_core::node::HDR_STAGE);
                }
                out.body(Body::from_stream(resp.bytes_stream()))
                    .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
            }
            Ok(Err(e)) => {
                // 连接失败也好、半截 TCP 也好,稳妥起见一律按「可能已发出」处理:
                // 少换一次号,好过多烧一份额度。
                let _ = e;
                stage_dario(StatusCode::BAD_GATEWAY, "子机上的 dario 进程连接失败".to_string())
            }
            Err(_) => stage_dario(
                // 响应头 100s 没回来:请求体已经发给 dario,可能已执行。
                StatusCode::GATEWAY_TIMEOUT,
                format!("子机上的 dario 进程 {CHAT_HEAD_TIMEOUT:?} 没有回响应头"),
            ),
        }
    }

    /// 本机 ensure;失败时回传管理器的原状态码与正文,并标明阶段。
    async fn ensure(&self, id: &str) -> Result<u16, Response> {
        let url = format!("{}/accounts/{id}/ensure", self.sup_url);
        let resp = self
            .local
            .post(url)
            .header("x-api-key", &self.dario_key)
            .timeout(ENSURE_TIMEOUT)
            .send()
            .await
            .map_err(|e| stage_ensure(StatusCode::BAD_GATEWAY, format!("子机上的 dario 管理器不可达: {e}")))?;
        let code = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if code != reqwest::StatusCode::OK {
            let status = StatusCode::from_u16(code.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            return Err(stage_ensure(status, text));
        }
        let port = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("port").and_then(|p| p.as_u64()))
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| (PORT_MIN..=PORT_MAX).contains(p));
        port.ok_or_else(|| stage_ensure(StatusCode::BAD_GATEWAY, format!("dario 管理器 ensure 返回无效端口: {text}")))
    }

    async fn oauth_exchange(&self, req: hyper::Request<Incoming>) -> Response {
        #[derive(serde::Deserialize)]
        struct Body {
            code: String,
            verifier: String,
            // 上游换码报文要求带 state(与官方客户端对齐);老主机不带时为空串。
            #[serde(default)]
            state: String,
        }
        let bytes = match http_body_util::Limited::new(req.into_body(), MAX_OAUTH_BODY).collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
        };
        let Ok(b) = serde_json::from_slice::<Body>(&bytes) else {
            return (StatusCode::BAD_REQUEST, "bad body").into_response();
        };
        match gw_dario::oauth::exchange(&self.oauth, &b.code, &b.verifier, &b.state).await {
            Ok(fields) => Json(serde_json::Value::Object(fields)).into_response(),
            Err(e) => {
                let status = e.status_code.and_then(|c| StatusCode::from_u16(c).ok()).unwrap_or(StatusCode::BAD_GATEWAY);
                (status, Json(serde_json::json!({"type": "error", "error": {"message": e.to_string()}}))).into_response()
            }
        }
    }

    /// 远端 bash:后台「子机」页的终端。命令以本进程的用户(caio)执行,限时、限量。
    ///
    /// 安全链:admin(管理密钥)→ hub 本地口(key)→ 隧道(TLS + 加入令牌)→ 这里。
    /// 这里不再做鉴权 —— 能走到这儿的请求已经过完全部三道门。
    async fn exec(&self, req: hyper::Request<Incoming>) -> Response {
        #[derive(serde::Deserialize)]
        struct ExecBody {
            command: String,
            timeout_secs: Option<u64>,
        }
        if !self.exec_enabled {
            return (StatusCode::FORBIDDEN, "这台子机关闭了远端 bash(子机侧 CAIO_EXEC=0)").into_response();
        }
        let bytes = match http_body_util::Limited::new(req.into_body(), MAX_EXEC_BODY).collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
        };
        let Ok(b) = serde_json::from_slice::<ExecBody>(&bytes) else {
            return (StatusCode::BAD_REQUEST, "bad body").into_response();
        };
        if b.command.trim().is_empty() || b.command.chars().count() > gw_core::node::EXEC_CMD_MAX_CHARS {
            return (StatusCode::BAD_REQUEST, format!("command 为空或超过 {} 字符", gw_core::node::EXEC_CMD_MAX_CHARS)).into_response();
        }
        let timeout = Duration::from_secs(
            b.timeout_secs.unwrap_or(gw_core::node::EXEC_TIMEOUT_DEFAULT).clamp(1, gw_core::node::EXEC_TIMEOUT_MAX),
        );
        // 审计:谁在子机上跑了什么。输出不进日志(可能含敏感内容),命令本身要留痕。
        let preview: String = b.command.chars().take(200).collect();
        tracing::info!(node = %self.node_id, command = %preview, timeout = ?timeout, "远端 bash 执行");
        let out = run_capped(&b.command, timeout).await;
        let status = if out.spawn_failed { StatusCode::INTERNAL_SERVER_ERROR } else { StatusCode::OK };
        tracing::info!(
            node = %self.node_id, command = %preview,
            exit = ?out.body.exit_code, timed_out = out.body.timed_out, ms = out.body.duration_ms,
            "远端 bash 结束"
        );
        (status, Json(out.body)).into_response()
    }

    async fn info(&self) -> Response {
        let supervisor = match self
            .local
            .get(format!("{}/accounts", self.sup_url))
            .header("x-api-key", &self.dario_key)
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r.json::<serde_json::Value>().await.unwrap_or(serde_json::Value::Null),
            Ok(r) => serde_json::json!({"error": format!("HTTP {}", r.status().as_u16())}),
            Err(e) => serde_json::json!({"error": e.to_string()}),
        };
        let (mem_total_kb, mem_available_kb) = meminfo();
        Json(serde_json::json!({
            "version": self.version,
            "hostname": hostname(),
            // install_id 是克隆防护的判定材料之一,不外报(少一样攻击者要凑的东西)。
            "uptime_secs": self.started.elapsed().as_secs(),
            "now": now_unix(),
            "exec_enabled": self.exec_enabled,
            "loadavg": std::fs::read_to_string("/proc/loadavg").ok().map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" ")),
            "mem_total_kb": mem_total_kb,
            "mem_available_kb": mem_available_kb,
            "supervisor": supervisor,
        }))
        .into_response()
    }
}

fn stage_ensure(status: StatusCode, body: String) -> Response {
    let mut resp = (status, body).into_response();
    resp.headers_mut().insert(HDR_STAGE, HeaderValue::from_static(STAGE_ENSURE));
    resp
}

/// agent 自己产的 502/504(转给 dario 失败):打 `stage: dario` 标记。
/// 不带标记时主机会按 ServerError + Untracked 分类 —— 既换号重发(可能双烧额度),
/// 又累加账号失败计数(远端抖动会把完好的号永久禁用)。
fn stage_dario(status: StatusCode, body: String) -> Response {
    let mut resp = (status, body).into_response();
    resp.headers_mut().insert(HDR_STAGE, HeaderValue::from_static(gw_core::node::STAGE_DARIO));
    resp
}

async fn small_response(resp: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let ctype = resp.headers().get("content-type").cloned();
    let body = resp.bytes().await.unwrap_or_default();
    let mut out = (status, body).into_response();
    if let Some(ct) = ctype {
        out.headers_mut().insert("content-type", ct);
    }
    out
}

/// 一个百分号编码的路径段能否原样拼进本机 URL:非空,且不是 `.`/`..`(含编码形态,
/// WHATWG URL 会把 `%2e%2e` 也当成上一级)。反斜杠也要拒:http 允许它出现在路径里,
/// 而 url crate 解析时会把 `\` 当成 `/` 并消解 `..`,能绕过白名单。
fn safe_segment(seg: &str) -> bool {
    if seg.is_empty() || seg.contains('\\') || seg.contains("%5c") || seg.contains("%5C") {
        return false;
    }
    let lower = seg.to_ascii_lowercase().replace("%2e", ".");
    lower != "." && lower != ".."
}

/// `/sup/` 之后只允许 `accounts`、`accounts/<id>`、`accounts/<id>/ensure`。
fn sup_path_allowed(rest: &str) -> bool {
    let segs: Vec<&str> = rest.split('/').collect();
    match segs.as_slice() {
        ["accounts"] => true,
        ["accounts", id] => safe_segment(id),
        ["accounts", id, "ensure"] => safe_segment(id),
        _ => false,
    }
}

// ── 远端 bash(/exec) ─────────────────────────────────────────────────────────

/// exec 的返回体。输出按 cap 截断(丢弃尾部但会继续读,见 [`run_capped`])。
#[derive(Debug, serde::Serialize)]
struct ExecResult {
    /// None = 被信号杀掉(含超时后被我们 kill)。
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
    /// 收割宽限到期、读者被强制中止:输出可能不完整(区别于 truncated,那是「完整但只留头部」)。
    output_incomplete: bool,
    duration_ms: u128,
}

struct ExecOutcome {
    body: ExecResult,
    /// bash 都没能启动(子机上没有 bash?)。对外回 500 而不是 200,免得后台把启动失败当成命令输出。
    spawn_failed: bool,
}

/// (已保留的前 cap 字节, 实际总字节)。总字节决定 truncated 标记。
type Sink = Arc<std::sync::Mutex<(Vec<u8>, usize)>>;

/// 读流到 EOF,只保留前 `cap` 字节。**必须读到 EOF**:提前停读会让写端管道装满,
/// 子进程卡在 write 上,超时前永不退出。
fn spawn_reader<R>(mut r: R, cap: usize, sink: Sink) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match tokio::io::AsyncReadExt::read(&mut r, &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut g = sink.lock().unwrap_or_else(|p| p.into_inner());
                    g.1 += n;
                    if g.0.len() < cap {
                        let room = cap - g.0.len();
                        g.0.extend_from_slice(&buf[..n.min(room)]);
                    }
                }
            }
        }
    })
}

/// 杀 bash 所在的整个进程组(bash 经 `process_group(0)` 自立为组长)。
///
/// 只杀 bash 是不够的:`bash -c 'sleep 200; echo'`、任何管道命令都会 fork 子进程,
/// bash 一死它们成孤儿继续跑、还握着管道。进程组也兜不住自己 `setsid` 逃跑的进程
/// (已知限制,那需要 cgroup)。
fn kill_group(pg: u32) {
    // SAFETY:killpg 对任意 pgid 都是安全调用;进程组不存在时返回 ESRCH,忽略。
    unsafe { libc::killpg(pg as libc::pid_t, libc::SIGKILL) };
}

async fn run_capped(command: &str, timeout: Duration) -> ExecOutcome {
    let started = Instant::now();
    let fail = |stderr: String| ExecOutcome {
        spawn_failed: true,
        body: ExecResult {
            exit_code: None,
            timed_out: false,
            stdout: String::new(),
            stderr,
            stdout_truncated: false,
            stderr_truncated: false,
            output_incomplete: false,
            duration_ms: started.elapsed().as_millis(),
        },
    };
    // HOME/USER 跟随运行用户:系统级是 caio,用户态是登录用户(读进程自身环境,它在
    // env_clear 之外 —— 清的是子进程,不是我们自己)。
    let home = std::env::var("HOME").unwrap_or_else(|_| "/var/lib/caio".to_string());
    let user = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| "caio".to_string());
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // 本进程环境里有 CAIO_TOKEN / DARIO_API_KEY:子进程环境清空重建。
        // 注意它挡不住刻意的读取:node.env 与 dario 数据目录对运行用户本来就可读
        // —— exec 的权限上限就是运行用户本身。
        .env_clear()
        .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
        .env("HOME", &home)
        .env("USER", &user)
        .env("LANG", "C.UTF-8")
        // bash 自立进程组,退出/超时/被 drop 时整组 SIGKILL,不留孙子进程。
        .process_group(0);
    // 用户态节点上 systemctl --user 需要 user bus:把 XDG_RUNTIME_DIR/DBUS 带上(存在才带)。
    // SAFETY:geteuid 无风险调用。
    let euid = unsafe { libc::geteuid() };
    let xdg = format!("/run/user/{euid}");
    if std::path::Path::new(&xdg).is_dir() {
        cmd.env("XDG_RUNTIME_DIR", &xdg)
            .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={xdg}/bus"));
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail(format!("启动 bash 失败: {e}")),
    };
    let pgid = child.id().unwrap_or(0);
    // 进程组的生命周期钉在这个作用域上:正常结束、超时、以及「调用方消失(hub 断流 →
    // hyper 直接 drop 本 future)」三条路都会走 Drop —— 不留任何路径能漏掉子孙进程。
    struct GroupGuard(u32);
    impl Drop for GroupGuard {
        fn drop(&mut self) {
            kill_group(self.0);
        }
    }
    let _group_guard = GroupGuard(pgid);
    let out_sink: Sink = Default::default();
    let err_sink: Sink = Default::default();
    let mut out_task = child.stdout.take().map(|s| spawn_reader(s, MAX_EXEC_OUTPUT, out_sink.clone()));
    let mut err_task = child.stderr.take().map(|s| spawn_reader(s, MAX_EXEC_OUTPUT, err_sink.clone()));
    let (exit_code, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(st)) => (st.code(), false),
        Ok(Err(e)) => {
            kill_group(pgid);
            return fail(format!("等待命令结束失败: {e}"));
        }
        Err(_) => (None, true),
    };
    // 不论正常结束还是超时,都杀一遍进程组:bash 退出不代表它 fork 出去的子孙也退了
    // (`bash -c '长任务 &'` 的孙子会握着管道不放,读者永远到不了 EOF)。
    kill_group(pgid);
    if timed_out {
        let _ = child.wait().await;
    }
    // 收割输出。setsid 逃出去的进程还会握着管道:给 3 秒宽限,过了就中止读者任务。
    // 注意不能 take() 进 async 块 —— timeout 触发时 JoinHandle 被 drop 只是 detach,
    // 不是 abort,任务会留下继续满速读管道。
    let mut incomplete = false;
    let grace = tokio::time::timeout(Duration::from_secs(3), async {
        if let Some(t) = out_task.as_mut() {
            let _ = t.await;
        }
        if let Some(t) = err_task.as_mut() {
            let _ = t.await;
        }
    })
    .await;
    if grace.is_err() {
        incomplete = true;
        if let Some(t) = &out_task {
            t.abort();
        }
        if let Some(t) = &err_task {
            t.abort();
        }
    }
    let (so, so_total) = std::mem::take(&mut *out_sink.lock().unwrap_or_else(|p| p.into_inner()));
    let (se, se_total) = std::mem::take(&mut *err_sink.lock().unwrap_or_else(|p| p.into_inner()));
    ExecOutcome {
        spawn_failed: false,
        body: ExecResult {
            exit_code,
            timed_out,
            stdout: String::from_utf8_lossy(&so).into_owned(),
            stderr: String::from_utf8_lossy(&se).into_owned(),
            stdout_truncated: so_total > MAX_EXEC_OUTPUT,
            stderr_truncated: se_total > MAX_EXEC_OUTPUT,
            output_incomplete: incomplete,
            duration_ms: started.elapsed().as_millis(),
        },
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn meminfo() -> (Option<u64>, Option<u64>) {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else { return (None, None) };
    let field = |name: &str| {
        text.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn segments_reject_dot_forms() {
        for bad in ["", ".", "..", "%2e", "%2E%2e", ".%2e", "%2e."] {
            assert!(!safe_segment(bad), "{bad:?}");
        }
        for ok in ["a%40b.com", "a%20b%2Fc%40d", "...", "a.b"] {
            assert!(safe_segment(ok), "{ok:?}");
        }
    }

    #[test]
    fn sup_allowlist() {
        assert!(sup_path_allowed("accounts"));
        assert!(sup_path_allowed("accounts/a%40b.com"));
        assert!(sup_path_allowed("accounts/a%40b.com/ensure"));
        assert!(!sup_path_allowed("accounts/../x"));
        assert!(!sup_path_allowed("accounts/a/b"));
        assert!(!sup_path_allowed("livez"));
        assert!(!sup_path_allowed("accounts/%2e%2e/ensure"));
    }
}
