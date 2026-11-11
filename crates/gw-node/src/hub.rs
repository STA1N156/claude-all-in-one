//! 主机侧 hub:等子机来连,把 worker / admin 的请求转进对应子机。
//!
//! - **公网口**(TLS):子机主动连上来,握手通过后 hub 在这条连接上当 HTTP/2 客户端;
//! - **本地口**(只绑 127.0.0.1,要 key):`/n/<节点>/<路径>` 转进该子机;`/status` 给
//!   worker 选号过滤和后台展示用。
//!
//! 子机离线、被禁用时立即回 503 并带 [`HDR_NODE`] 标记,调用方据此知道请求没发出去。
//! 节点的令牌、绑定、禁用状态、待删清单都在主机库里,经 [`NodeRegistry`] 读写。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use gw_core::node::{valid_node_id, NodeMark, HDR_NODE};
use http_body_util::BodyExt;
use hyper::client::conn::http2::SendRequest;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use crate::tunnel::{self, Hello, Welcome};
use crate::{filter_headers, now_unix, NODE_AUTHORITY};

/// `/sup` 控制面请求的截止时间(含排队):管理器在子机本机,正常几十毫秒。
/// 隧道半死时不让交接把 worker 的整段重试预算耗光。
const SUP_DEADLINE: Duration = Duration::from_secs(20);
const OAUTH_DEADLINE: Duration = Duration::from_secs(40);
const INFO_DEADLINE: Duration = Duration::from_secs(10);
/// 对话请求只限「等到响应头」的时间(子机冷启动 dario 最多约 15s);响应体是流,不限。
const CHAT_HEAD_DEADLINE: Duration = Duration::from_secs(150);
/// 远端 bash:子机侧上限 120s,这里多留 10s 给传输与排队。
const EXEC_DEADLINE: Duration = Duration::from_secs(130);
const MAINTAIN_EVERY: Duration = Duration::from_secs(15);
/// 「同一令牌两台机器」判定前,先探测旧连接是否还活着(超时就当死了,让位给新连接)。
/// 不按连接年龄判断:旧连接活得久不代表它是合法持有者。
const CLONE_PROBE_TIMEOUT: Duration = Duration::from_secs(4);
const FLAGS_EVERY: Duration = Duration::from_secs(5);
const MAX_INFO_BYTES: usize = 256 * 1024;
/// 公网口同时进行的握手(accept 之后、HTTP/2 建立之前)上限:防未鉴权连接洪峰。
const MAX_CONCURRENT_HANDSHAKES: usize = 64;

/// 认证通过后的节点信息。
#[derive(Debug, Clone)]
pub struct NodeAuth {
    pub name: String,
}

/// 拒绝原因。`fatal` = 重试也没用(子机会拉长退避并报错)。
#[derive(Debug, Clone)]
pub struct Reject {
    pub reason: String,
    pub fatal: bool,
}

/// 库里节点的当前标志(5 秒刷新一次)。不在表里 = 已吊销。
#[derive(Debug, Clone, Default)]
pub struct NodeFlags {
    pub name: String,
    pub disabled: bool,
}

/// hub 与主机库之间的接口(实现在 gw-app,用 SQLite)。调用都很快,直接在异步上下文里调。
pub trait NodeRegistry: Send + Sync + 'static {
    /// 核对令牌;首次连接时把 `install_id` 绑定到节点。
    fn authenticate(&self, hello: &Hello) -> Result<NodeAuth, Reject>;
    fn node_flags(&self) -> anyhow::Result<HashMap<String, NodeFlags>>;
    fn record_seen(&self, node_id: &str, ip: &str, version: &str, info: Option<&serde_json::Value>);
    /// 子机离线时被删掉的号,等它重连后补删。
    fn tombstones(&self, node_id: &str) -> Vec<String>;
    fn clear_tombstone(&self, node_id: &str, account_id: &str);
    /// 证书指纹与对外地址,后台生成安装命令时用。
    fn save_identity(&self, spki_b64: &str, public_addr: Option<&str>);
}

struct Live {
    conn_id: u64,
    sender: SendRequest<Body>,
    peer: SocketAddr,
    connected_at: i64,
    version: String,
    hostname: String,
    driver: tokio::task::AbortHandle,
    info: Mutex<Option<serde_json::Value>>,
    rtt_ms: AtomicU64,
}

pub struct Hub {
    registry: Arc<dyn NodeRegistry>,
    local_key: String,
    /// 远端 bash 的独立钥匙(None = exec 完全关闭)。与 local_key 分开:dario.api_key
    /// 的持有面太广(管理器、worker、每个 dario 进程),不能让它等于「全舰队的 shell」。
    exec_key: Option<String>,
    spki_b64: String,
    live: RwLock<HashMap<String, Arc<Live>>>,
    flags: RwLock<HashMap<String, NodeFlags>>,
    next_conn: AtomicU64,
    /// 公网口握手并发上限(见 serve_public)。
    handshake_permits: Arc<tokio::sync::Semaphore>,
}

pub struct HubOptions {
    pub public_listen: SocketAddr,
    pub local_listen: SocketAddr,
    pub cert_dir: std::path::PathBuf,
    /// 子机连主机用的地址(`host:port`),写进后台生成的完整令牌。
    pub public_addr: Option<String>,
    /// 本地口的 key(与 `dario.api_key` 相同)。
    pub local_key: String,
    /// `/exec` 的独立钥匙(system.yaml `node_exec_key`);None/空 = exec 关闭。
    pub exec_key: Option<String>,
}

/// 起 hub:两个监听 + 标志刷新。正常不返回。
pub async fn run(opts: HubOptions, registry: Arc<dyn NodeRegistry>) -> anyhow::Result<()> {
    if opts.local_key.trim().len() < 16 {
        anyhow::bail!("hub 本地口需要 key(dario.api_key,至少 16 位);为空时拒绝启动");
    }
    let id = crate::tls::load_or_create(&opts.cert_dir)?;
    let spki_b64 = crate::join::spki_b64(&id.spki);
    registry.save_identity(&spki_b64, opts.public_addr.as_deref());
    let acceptor = TlsAcceptor::from(Arc::new(crate::tls::server_config(&id)?));
    let hub = Arc::new(Hub::new(registry, opts.local_key, opts.exec_key, spki_b64.clone()));
    let public = TcpListener::bind(opts.public_listen).await?;
    let local = TcpListener::bind(opts.local_listen).await?;
    tracing::info!(public = %opts.public_listen, local = %opts.local_listen, spki = %spki_b64, "子机 hub 已启动");
    hub.refresh_flags();
    tokio::spawn(hub.clone().flags_loop());
    let app = hub.clone().local_router();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(local, app).await {
            tracing::error!(error = %e, "hub 本地口退出");
        }
    });
    hub.serve_public(public, acceptor).await
}

impl Hub {
    pub fn new(registry: Arc<dyn NodeRegistry>, local_key: String, exec_key: Option<String>, spki_b64: String) -> Self {
        Self {
            registry,
            local_key,
            exec_key: exec_key.filter(|k| k.len() >= 16),
            spki_b64,
            live: RwLock::new(HashMap::new()),
            flags: RwLock::new(HashMap::new()),
            next_conn: AtomicU64::new(1),
            handshake_permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HANDSHAKES)),
        }
    }

    pub async fn serve_public(self: Arc<Self>, listener: TcpListener, acceptor: TlsAcceptor) -> anyhow::Result<()> {
        loop {
            let (tcp, peer) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(error = %e, "hub 接受连接失败");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            };
            // 握手前的连接不占名额不行:公网口裸 TCP 连接零成本,不封顶会被洪峰耗死。
            let Ok(permit) = self.handshake_permits.clone().try_acquire_owned() else {
                tracing::warn!(peer = %peer, "hub 握手并发已满,丢弃连接");
                drop(tcp);
                continue;
            };
            let hub = self.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(e) = hub.accept_node(tcp, peer, acceptor).await {
                    tracing::info!(peer = %peer, error = %e, "子机握手未完成");
                }
            });
        }
    }

    async fn accept_node(self: Arc<Self>, tcp: TcpStream, peer: SocketAddr, acceptor: TlsAcceptor) -> anyhow::Result<()> {
        let _ = tcp.set_nodelay(true);
        let (mut tls, hello) = tokio::time::timeout(tunnel::HANDSHAKE_TIMEOUT, async {
            let mut tls = acceptor.accept(tcp).await?;
            let hello: Hello = tunnel::read_frame(&mut tls).await?;
            anyhow::Ok((tls, hello))
        })
        .await
        .map_err(|_| anyhow::anyhow!("握手超时"))??;
        if hello.proto != tunnel::PROTO {
            let msg = format!("协议版本不符(主机 {},子机 {}),请升级子机", tunnel::PROTO, hello.proto);
            // fatal=false:版本不符多半是升级过程中的**暂态**(主机先上、子机还没重装,
            // 或主机回滚了)。判 fatal 会让子机退避 5 分钟,主机侧一修好也要干等;
            // 按可重试处理,子机 1~30 秒退避重连,主机一就位它自己回来。
            tunnel::write_frame(&mut tls, &Welcome::reject(&msg, false)).await?;
            anyhow::bail!(msg);
        }
        let auth = match self.registry.authenticate(&hello) {
            Ok(a) => a,
            Err(r) => {
                tunnel::write_frame(&mut tls, &Welcome::reject(&r.reason, r.fatal)).await?;
                tracing::warn!(peer = %peer, node = %hello.node_id, reason = %r.reason, "拒绝子机");
                return Ok(());
            }
        };

        // 克隆防护:**先探活,再定去留**(在写 accept 之前,好让被拒的一方拿到明确原因)。
        //
        // `install_id` 存在 /etc/caio/node.env 里,整盘克隆或拷配置会把它一起带走,两台机器
        // 都能通过认证。那是最坏的情况:两边的 dario 各自用同一条 refresh token 刷新 → 上游
        // 复用检测 → 这台子机上所有号一起作废,且 caio 侧的 refresh_token 交接时已清空,
        // 救不回来。
        //
        // 旧实现按「连接年龄 < 120s」判定,有两个洞:老连接活过 120s 后克隆机可以把它顶掉
        // (两台机器从此轮流持有同一批号);同 NAT 同主机名的克隆根本不会触发。现在改成:
        // 同一 node_id 已有在线连接、且主机名或来源 IP 不同 → 先给旧连接发 /info 探活;
        // 活着就拒新保旧,死了(超时/出错)才让新连接接管。同一台机器重连(主机名与 IP
        // 都不变)直接替换,不付探活的 4 秒。
        if let Some(old) = self.current(&hello.node_id) {
            let same_machine = old.hostname == hello.hostname && old.peer.ip() == peer.ip();
            if !same_machine {
                if self.probe_alive(&old).await {
                    let msg = "这个令牌的另一台机器仍在线(可能是克隆了 node.env / 整盘镜像),已保留旧连接。确认要换机器:先在后台「解除机器绑定」";
                    tracing::error!(
                        node = %hello.node_id, old_peer = %old.peer, new_peer = %peer,
                        old_host = %old.hostname, new_host = %hello.hostname, "{msg}"
                    );
                    tunnel::write_frame(&mut tls, &Welcome::reject(msg, false)).await?;
                    return Ok(());
                }
                tracing::warn!(
                    node = %hello.node_id, old_peer = %old.peer, new_peer = %peer,
                    "同一令牌的旧连接探测无响应,判定已离线,允许新连接接管"
                );
            }
        }

        tunnel::write_frame(&mut tls, &Welcome::accept(&auth.name)).await?;

        let (sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .timer(TokioTimer::new())
            .keep_alive_interval(tunnel::KEEPALIVE_INTERVAL)
            .keep_alive_timeout(tunnel::KEEPALIVE_TIMEOUT)
            .keep_alive_while_idle(true)
            .initial_stream_window_size(tunnel::HUB_STREAM_WINDOW)
            .initial_connection_window_size(tunnel::HUB_CONN_WINDOW)
            .handshake::<_, Body>(TokioIo::new(tls))
            .await?;
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let node_id = hello.node_id.clone();
        let hub = self.clone();
        let driver = tokio::spawn({
            let node_id = node_id.clone();
            async move {
                if let Err(e) = conn.await {
                    tracing::debug!(node = %node_id, error = %e, "子机连接结束");
                }
                hub.on_disconnect(&node_id, conn_id);
            }
        });
        let driver = driver.abort_handle();
        let live = Arc::new(Live {
            conn_id,
            sender,
            peer,
            connected_at: now_unix(),
            version: hello.version.clone(),
            hostname: hello.hostname.clone(),
            driver: driver.clone(),
            info: Mutex::new(None),
            rtt_ms: AtomicU64::new(0),
        });
        let old = self.live.write().unwrap_or_else(|p| p.into_inner()).insert(node_id.clone(), live);
        if let Some(old) = old {
            old.driver.abort();
        }
        self.registry.record_seen(&node_id, &peer.ip().to_string(), &hello.version, None);
        tracing::info!(node = %node_id, name = %auth.name, peer = %peer, version = %hello.version, "子机已连上");
        tokio::spawn(self.clone().maintain(node_id, conn_id));
        Ok(())
    }

    fn on_disconnect(&self, node_id: &str, conn_id: u64) {
        let mut live = self.live.write().unwrap_or_else(|p| p.into_inner());
        if live.get(node_id).is_some_and(|l| l.conn_id == conn_id) {
            live.remove(node_id);
            tracing::warn!(node = %node_id, "子机已断开");
        }
    }

    fn current(&self, node_id: &str) -> Option<Arc<Live>> {
        self.live.read().unwrap_or_else(|p| p.into_inner()).get(node_id).cloned()
    }

    /// 每条连接一个维护任务:立即补删一次待删号,之后每 15 秒拉一次子机状态。
    async fn maintain(self: Arc<Self>, node_id: String, conn_id: u64) {
        let mut tick: u64 = 0;
        loop {
            let Some(live) = self.current(&node_id).filter(|l| l.conn_id == conn_id) else {
                return;
            };
            // 连上立即补删一次,之后每 4 轮(约 1 分钟)再看一次。
            if tick % 4 == 0 {
                self.apply_tombstones(&node_id, &live).await;
            }
            tick += 1;
            self.poll_info(&node_id, &live).await;
            drop(live);
            tokio::time::sleep(MAINTAIN_EVERY).await;
        }
    }

    /// 探活一条已建立的子机连接:发 /info,只等到响应头。活着 = true。
    /// 用在克隆防护上(新连接进来前确认旧连接是死是活),不影响 poll_info 的周期任务。
    async fn probe_alive(&self, live: &Live) -> bool {
        let req = match hyper::Request::get(format!("http://{NODE_AUTHORITY}/info")).body(Body::empty()) {
            Ok(r) => r,
            Err(_) => return false,
        };
        let mut sender = live.sender.clone();
        tokio::time::timeout(CLONE_PROBE_TIMEOUT, async {
            sender.ready().await?;
            sender.send_request(req).await
        })
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
    }

    async fn poll_info(&self, node_id: &str, live: &Live) {        let started = Instant::now();
        let req = match hyper::Request::get(format!("http://{NODE_AUTHORITY}/info")).body(Body::empty()) {
            Ok(r) => r,
            Err(_) => return,
        };
        let mut sender = live.sender.clone();
        let res = tokio::time::timeout(INFO_DEADLINE, async {
            sender.ready().await?;
            let resp = sender.send_request(req).await?;
            let bytes = http_body_util::Limited::new(resp.into_body(), MAX_INFO_BYTES)
                .collect()
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .to_bytes();
            anyhow::Ok(serde_json::from_slice::<serde_json::Value>(&bytes)?)
        })
        .await;
        match res {
            Ok(Ok(info)) => {
                live.rtt_ms.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                self.registry.record_seen(node_id, &live.peer.ip().to_string(), &live.version, Some(&info));
                *live.info.lock().unwrap_or_else(|p| p.into_inner()) = Some(info);
            }
            Ok(Err(e)) => tracing::debug!(node = %node_id, error = %e, "拉子机状态失败"),
            Err(_) => tracing::debug!(node = %node_id, "拉子机状态超时"),
        }
    }

    /// 补删:子机离线期间在后台删掉的号,让子机上的管理器停进程、删凭证。
    async fn apply_tombstones(&self, node_id: &str, live: &Live) {
        for account_id in self.registry.tombstones(node_id) {
            let mut url = reqwest::Url::parse(&format!("http://{NODE_AUTHORITY}/sup/accounts")).expect("常量 URL");
            url.path_segments_mut().expect("http URL").push(&account_id);
            let Ok(req) = hyper::Request::delete(url.as_str()).body(Body::empty()) else { continue };
            let mut sender = live.sender.clone();
            let res = tokio::time::timeout(SUP_DEADLINE, async {
                sender.ready().await?;
                anyhow::Ok(sender.send_request(req).await?.status().as_u16())
            })
            .await;
            match res {
                Ok(Ok(200 | 404)) => {
                    self.registry.clear_tombstone(node_id, &account_id);
                    tracing::info!(node = %node_id, account = %account_id, "已在子机上补删号");
                }
                other => tracing::warn!(node = %node_id, account = %account_id, result = ?other.map(|r| r.map_err(|e| e.to_string())), "子机补删失败(稍后重试)"),
            }
        }
    }

    pub fn refresh_flags(&self) {
        match self.registry.node_flags() {
            Ok(flags) => {
                let revoked: Vec<String> = self
                    .live
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .keys()
                    .filter(|id| !flags.contains_key(*id))
                    .cloned()
                    .collect();
                *self.flags.write().unwrap_or_else(|p| p.into_inner()) = flags;
                for id in revoked {
                    if let Some(l) = self.live.write().unwrap_or_else(|p| p.into_inner()).remove(&id) {
                        l.driver.abort();
                        tracing::warn!(node = %id, "子机已被吊销,断开连接");
                    }
                }
            }
            // 读库失败时保留上一份标志:不能因为一次读失败就把所有子机断开。
            Err(e) => tracing::warn!(error = %e, "读取子机列表失败(沿用上次结果)"),
        }
    }

    async fn flags_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(FLAGS_EVERY).await;
            self.refresh_flags();
        }
    }

    pub fn local_router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/status", get(status))
            .fallback(forward)
            .layer(middleware::from_fn_with_state(self.clone(), require_key))
            .with_state(self)
    }

    fn flag(&self, node_id: &str) -> Option<NodeFlags> {
        self.flags.read().unwrap_or_else(|p| p.into_inner()).get(node_id).cloned()
    }
}

async fn require_key(State(hub): State<Arc<Hub>>, req: Request, next: Next) -> Response {
    let h = req.headers();
    let given = h
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .or_else(|| h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")))
        .unwrap_or("");
    if !ct_eq(given.as_bytes(), hub.local_key.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "unauthorized"}))).into_response();
    }
    next.run(req).await
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn status(State(hub): State<Arc<Hub>>) -> Json<serde_json::Value> {
    let flags = hub.flags.read().unwrap_or_else(|p| p.into_inner()).clone();
    let live = hub.live.read().unwrap_or_else(|p| p.into_inner()).clone();
    let mut nodes: Vec<serde_json::Value> = flags
        .iter()
        .map(|(id, f)| {
            let l = live.get(id);
            serde_json::json!({
                "id": id,
                "name": f.name,
                "disabled": f.disabled,
                "online": l.is_some(),
                "peer_ip": l.map(|l| l.peer.ip().to_string()),
                "connected_at": l.map(|l| l.connected_at),
                "version": l.map(|l| l.version.clone()),
                "hostname": l.map(|l| l.hostname.clone()),
                "rtt_ms": l.map(|l| l.rtt_ms.load(Ordering::Relaxed)).filter(|v| *v > 0),
                "info": l.and_then(|l| l.info.lock().unwrap_or_else(|p| p.into_inner()).clone()),
            })
        })
        .collect();
    nodes.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Json(serde_json::json!({ "nodes": nodes, "spki": hub.spki_b64, "now": now_unix() }))
}

fn marked(status: StatusCode, mark: NodeMark, msg: &str) -> Response {
    let mut resp = (
        status,
        Json(serde_json::json!({"type": "error", "error": {"type": "node_unavailable", "message": msg}})),
    )
        .into_response();
    resp.headers_mut().insert(HDR_NODE, HeaderValue::from_static(mark.as_str()));
    resp
}

/// `/n/<节点>/<路径>` → 子机。路径按原始字节切分、原样转发(账号 ID 是百分号编码)。
async fn forward(State(hub): State<Arc<Hub>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let Some((node_id, tail)) = path.strip_prefix("/n/").and_then(|r| r.split_once('/')) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    if !valid_node_id(node_id) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let tail = format!("/{tail}");
    // exec 要独立钥匙(在本地口 key 之外再加一道):dario.api_key 的持有面太广,
    // 不能让它等于「全舰队的 shell」。没配 exec_key 时 exec 完全关闭(fail-closed)。
    if tail == "/exec" {
        let Some(want) = &hub.exec_key else {
            return (StatusCode::FORBIDDEN, "远端 bash 未启用:主机没配 node_exec_key").into_response();
        };
        let given = req.headers().get(gw_core::node::HDR_EXEC_KEY).and_then(|v| v.to_str().ok()).unwrap_or("");
        if !ct_eq(given.as_bytes(), want.as_bytes()) {
            return (StatusCode::FORBIDDEN, "远端 bash 钥匙不对").into_response();
        }
    }
    let Some(flag) = hub.flag(node_id) else {
        return marked(StatusCode::SERVICE_UNAVAILABLE, NodeMark::Offline, "子机不存在或已吊销");
    };
    // 排空只挡新对话;删号、换码、状态查询照常放行。
    if flag.disabled && tail.starts_with("/a/") {
        return marked(StatusCode::SERVICE_UNAVAILABLE, NodeMark::Disabled, "子机已被禁用(排空中)");
    }
    let Some(live) = hub.current(node_id) else {
        return marked(StatusCode::SERVICE_UNAVAILABLE, NodeMark::Offline, "子机当前离线");
    };
    let deadline = if tail.starts_with("/sup/") {
        SUP_DEADLINE
    } else if tail.starts_with("/oauth/") {
        OAUTH_DEADLINE
    } else if tail == "/info" {
        INFO_DEADLINE
    } else if tail == "/exec" {
        // 子机侧 exec 上限 120s:留 10s 给传输与排队,保证「超时已杀」的回复回得来。
        EXEC_DEADLINE
    } else {
        CHAT_HEAD_DEADLINE
    };
    let target = match req.uri().query() {
        Some(q) => format!("http://{NODE_AUTHORITY}{tail}?{q}"),
        None => format!("http://{NODE_AUTHORITY}{tail}"),
    };
    let (parts, body) = req.into_parts();
    let mut builder = hyper::Request::builder().method(parts.method).uri(target);
    if let Some(h) = builder.headers_mut() {
        *h = forwardable(&parts.headers);
    }
    let Ok(out) = builder.body(body) else {
        return (StatusCode::BAD_REQUEST, "bad request").into_response();
    };
    let mut sender = live.sender.clone();
    drop(live);
    // ready() 也要在 deadline 里:子机作为 h2 服务端设了 max_concurrent_streams(256),
    // 流占满时 ready() 会**无限期**挂住 —— 那是整条热路径上唯一一处无界等待,挂住的
    // 同时 worker 这条请求还占着账号租约。
    match tokio::time::timeout(deadline, async {
        sender.ready().await?;
        sender.send_request(out).await
    })
    .await
    {
        Ok(Ok(resp)) => {
            let (mut parts, body) = resp.into_parts();
            filter_headers(&mut parts.headers);
            // `x-caio-node` 是 hub 的**独占**控制通道:调用方据此判定「请求一个字节都没发
            // 出去,可以放心换号重发」。若让子机(或子机上的 dario、被改过的子机)也能写它,
            // 一个已经执行过的请求就能伪称「没发出去」→ 主机换号重发 → 同一次对话烧两份
            // 额度。所以回程无条件剥掉。
            parts.headers.remove(HDR_NODE);
            Response::from_parts(parts, Body::new(body))
        }
        Ok(Err(e)) => {
            tracing::warn!(node = %node_id, error = %e, "转发到子机途中失败");
            marked(StatusCode::BAD_GATEWAY, NodeMark::Lost, "与子机的连接中断")
        }
        Err(_) => {
            tracing::warn!(node = %node_id, path = %tail, "子机响应超时");
            marked(StatusCode::GATEWAY_TIMEOUT, NodeMark::Lost, "子机响应超时")
        }
    }
}

/// 转给子机的请求头:去掉逐跳头与主机侧的鉴权(子机注入自己的 key)。
fn forwardable(h: &HeaderMap) -> HeaderMap {
    let mut out = h.clone();
    filter_headers(&mut out);
    out.remove("x-api-key");
    out.remove("authorization");
    out.remove("host");
    out
}
