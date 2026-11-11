//! 进程内集成测试:真 hub + 真 agent + 假 dario 管理器 + 假 dario 进程,全在 127.0.0.1。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use tokio::net::TcpListener;

use crate::hub::{Hub, NodeAuth, NodeFlags, NodeRegistry, Reject};
use crate::join::secret_hash;
use crate::tunnel::{self, Hello, Welcome};

const HUB_KEY: &str = "hub-local-key-0123456789";
const EXEC_KEY: &str = "hub-exec-key-0123456789ab";
const NODE_KEY: &str = "node-dario-key-0123456789";
const NODE: &str = "n-test01";
const SECRET: &str = "secret-secret-secret-0001";

// ── 假的主机库 ──────────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeRegistry {
    bound: Mutex<Option<String>>,
    disabled: AtomicBool,
    revoked: AtomicBool,
    tombstones: Mutex<Vec<String>>,
}

impl NodeRegistry for FakeRegistry {
    fn authenticate(&self, h: &Hello) -> Result<NodeAuth, Reject> {
        if h.node_id != NODE || self.revoked.load(Ordering::SeqCst) || secret_hash(&h.secret) != secret_hash(SECRET) {
            return Err(Reject { reason: "令牌无效".into(), fatal: true });
        }
        let mut b = self.bound.lock().unwrap();
        match b.as_deref() {
            None => *b = Some(h.install_id.clone()),
            Some(id) if id == h.install_id => {}
            Some(_) => return Err(Reject { reason: "已绑定别的机器".into(), fatal: true }),
        }
        Ok(NodeAuth { name: "测试子机".into() })
    }
    fn node_flags(&self) -> anyhow::Result<HashMap<String, NodeFlags>> {
        let mut m = HashMap::new();
        if !self.revoked.load(Ordering::SeqCst) {
            m.insert(NODE.into(), NodeFlags { name: "测试子机".into(), disabled: self.disabled.load(Ordering::SeqCst) });
        }
        Ok(m)
    }
    fn record_seen(&self, _: &str, _: &str, _: &str, _: Option<&serde_json::Value>) {}
    fn tombstones(&self, _: &str) -> Vec<String> {
        self.tombstones.lock().unwrap().clone()
    }
    fn clear_tombstone(&self, _: &str, account_id: &str) {
        self.tombstones.lock().unwrap().retain(|a| a != account_id);
    }
    fn save_identity(&self, _: &str, _: Option<&str>) {}
}

// ── 假的 dario 管理器 + dario 进程 ─────────────────────────────────────────

#[derive(Default)]
struct Fake {
    dario_port: Mutex<u16>,
    calls: Mutex<Vec<String>>,
    keys: Mutex<Vec<String>>,
    session_ids: Mutex<Vec<String>>,
    /// 流被丢弃(客户端断开)时置位。
    stream_dropped: AtomicBool,
    streams_started: AtomicUsize,
}
type F = Arc<Fake>;

fn note_key(f: &F, h: &HeaderMap) {
    f.keys.lock().unwrap().push(h.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or("").to_string());
}

async fn sup_list(State(f): State<F>, h: HeaderMap) -> impl IntoResponse {
    note_key(&f, &h);
    f.calls.lock().unwrap().push("LIST".into());
    Json(serde_json::json!([{"id": "acc@x", "running": true}]))
}

async fn sup_ensure(State(f): State<F>, h: HeaderMap, Path(id): Path<String>) -> axum::response::Response {
    note_key(&f, &h);
    f.calls.lock().unwrap().push(format!("ENSURE {id}"));
    if id == "dead" {
        return (StatusCode::CONFLICT, Json(serde_json::json!({"error": "needs relogin"}))).into_response();
    }
    Json(serde_json::json!({"port": *f.dario_port.lock().unwrap()})).into_response()
}

async fn sup_delete(State(f): State<F>, h: HeaderMap, Path(id): Path<String>) -> impl IntoResponse {
    note_key(&f, &h);
    f.calls.lock().unwrap().push(format!("DELETE {id}"));
    Json(serde_json::json!({"deleted": true}))
}

struct DropFlag(F);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.stream_dropped.store(true, Ordering::SeqCst);
    }
}

async fn dario_messages(State(f): State<F>, h: HeaderMap, body: Body) -> axum::response::Response {
    note_key(&f, &h);
    if let Some(s) = h.get("x-session-id").and_then(|v| v.to_str().ok()) {
        f.session_ids.lock().unwrap().push(s.to_string());
    }
    if h.get("x-test").and_then(|v| v.to_str().ok()) == Some("len") {
        let mut n = 0usize;
        let mut s = body.into_data_stream();
        while let Some(chunk) = s.next().await {
            n += chunk.unwrap().len();
        }
        return Json(serde_json::json!({"len": n})).into_response();
    }
    f.streams_started.fetch_add(1, Ordering::SeqCst);
    let guard = DropFlag(f.clone());
    let slow = h.get("x-test").and_then(|v| v.to_str().ok()) == Some("slow");
    let stream = async_stream(guard, slow);
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn async_stream(guard: DropFlag, slow: bool) -> impl futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    let n = if slow { 600 } else { 3 };
    futures::stream::unfold((0usize, guard), move |(i, g)| async move {
        if i >= n {
            return None;
        }
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        Some((Ok(bytes::Bytes::from(format!("data: chunk{i}\n\n"))), (i + 1, g)))
    })
}

/// dario 进程端口必须落在 18800–18999(agent 会校验)。
async fn bind_dario_port() -> TcpListener {
    for p in crate::agent::PORT_MIN..=crate::agent::PORT_MAX {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", p)).await {
            return l;
        }
    }
    panic!("18800-18999 没有空闲端口");
}

async fn spawn_fakes() -> (F, String) {
    let f: F = Arc::new(Fake::default());
    let dl = bind_dario_port().await;
    *f.dario_port.lock().unwrap() = dl.local_addr().unwrap().port();
    let dario = Router::new().route("/v1/messages", post(dario_messages)).with_state(f.clone());
    tokio::spawn(async move { axum::serve(dl, dario).await.unwrap() });
    let sup = Router::new()
        .route("/accounts", get(sup_list))
        .route("/accounts/{id}", axum::routing::delete(sup_delete))
        .route("/accounts/{id}/ensure", post(sup_ensure))
        .with_state(f.clone());
    let sl = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sup_url = format!("http://{}", sl.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(sl, sup).await.unwrap() });
    (f, sup_url)
}

// ── 组装 ───────────────────────────────────────────────────────────────────

struct Env {
    fake: F,
    reg: Arc<FakeRegistry>,
    hub: Arc<Hub>,
    local: String,
    master: String,
    spki: [u8; 32],
    sup_url: String,
    http: reqwest::Client,
    agent: tokio::task::JoinHandle<anyhow::Result<()>>,
}

fn agent_opts(master: &str, spki: [u8; 32], sup_url: &str, install_id: &str) -> crate::agent::AgentOptions {
    crate::agent::AgentOptions {
        master: master.into(),
        spki,
        node_id: NODE.into(),
        secret: SECRET.into(),
        install_id: install_id.into(),
        sup_url: sup_url.into(),
        dario_key: NODE_KEY.into(),
        version: "test".into(),
    }
}

async fn setup() -> Env {
    let (fake, sup_url) = spawn_fakes().await;
    let reg = Arc::new(FakeRegistry::default());
    let dir = std::env::temp_dir().join(format!("gw-node-it-{}", uuid::Uuid::new_v4()));
    let id = crate::tls::load_or_create(&dir).unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(crate::tls::server_config(&id).unwrap()));
    let hub = Arc::new(Hub::new(reg.clone(), HUB_KEY.into(), Some(EXEC_KEY.into()), crate::join::spki_b64(&id.spki)));
    hub.refresh_flags();
    let pl = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let master = pl.local_addr().unwrap().to_string();
    tokio::spawn(hub.clone().serve_public(pl, acceptor));
    let ll = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = format!("http://{}", ll.local_addr().unwrap());
    let app = hub.clone().local_router();
    tokio::spawn(async move { axum::serve(ll, app).await.unwrap() });
    let agent = tokio::spawn(crate::agent::run(agent_opts(&master, id.spki, &sup_url, "install-A")));
    let env = Env { fake, reg, hub, local, master, spki: id.spki, sup_url, http: reqwest::Client::builder().no_proxy().build().unwrap(), agent };
    wait_online(&env, true).await;
    env
}

async fn node_online(env: &Env) -> bool {
    let v: serde_json::Value = env
        .http
        .get(format!("{}/status", env.local))
        .header("x-api-key", HUB_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["nodes"].as_array().unwrap().iter().any(|n| n["id"] == NODE && n["online"] == true)
}

async fn wait_online(env: &Env, want: bool) {
    for _ in 0..100 {
        if node_online(env).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("子机在线状态没有变成 {want}");
}

fn chat_url(env: &Env, id: &str) -> String {
    format!("{}/n/{NODE}/a/{id}/v1/messages", env.local)
}

// ── 测试 ───────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_streams_incrementally_with_node_key() {
    let env = setup().await;
    let started = std::time::Instant::now();
    let resp = env
        .http
        .post(chat_url(&env, "acc%40x"))
        .header("x-api-key", HUB_KEY)
        .header("x-session-id", "sess-1")
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mut s = resp.bytes_stream();
    let first = s.next().await.unwrap().unwrap();
    let first_at = started.elapsed();
    assert!(String::from_utf8_lossy(&first).contains("chunk0"));
    let mut rest = Vec::new();
    while let Some(c) = s.next().await {
        rest.extend_from_slice(&c.unwrap());
    }
    let total = started.elapsed();
    assert!(String::from_utf8_lossy(&rest).contains("chunk2"));
    assert!(total - first_at >= Duration::from_millis(250), "首块要先于后续块到达(不被缓冲)");
    // 子机注入自己的 key,主机的 key 不出主机。
    let keys = env.fake.keys.lock().unwrap().clone();
    assert!(keys.iter().all(|k| k == NODE_KEY), "{keys:?}");
    assert_eq!(*env.fake.session_ids.lock().unwrap(), vec!["sess-1"]);
    assert!(env.fake.calls.lock().unwrap().contains(&"ENSURE acc%40x".to_string()) || env.fake.calls.lock().unwrap().contains(&"ENSURE acc@x".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_disconnect_cancels_node_stream() {
    let env = setup().await;
    let resp = env
        .http
        .post(chat_url(&env, "acc"))
        .header("x-api-key", HUB_KEY)
        .header("x-test", "slow")
        .body("{}")
        .send()
        .await
        .unwrap();
    let mut s = resp.bytes_stream();
    s.next().await.unwrap().unwrap();
    assert!(!env.fake.stream_dropped.load(Ordering::SeqCst));
    drop(s);
    for _ in 0..50 {
        if env.fake.stream_dropped.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("主机侧断开后,子机上的 dario 流没有被取消");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_body_passes() {
    let env = setup().await;
    let body = vec![b'x'; 24 * 1024 * 1024];
    let v: serde_json::Value = env
        .http
        .post(chat_url(&env, "acc"))
        .header("x-api-key", HUB_KEY)
        .header("x-test", "len")
        .body(body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["len"], 24 * 1024 * 1024);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ensure_failure_is_staged_and_paths_are_guarded() {
    let env = setup().await;
    let resp = env.http.post(chat_url(&env, "dead")).header("x-api-key", HUB_KEY).body("{}").send().await.unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(resp.headers().get(gw_core::node::HDR_STAGE).unwrap(), "ensure");
    // 管理器控制面经隧道可达,且用的是子机的 key。
    let resp = env.http.get(format!("{}/n/{NODE}/sup/accounts", env.local)).header("x-api-key", HUB_KEY).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    // 白名单外的路径被子机拒绝(GET /exec 也算:它只认 POST)。
    for bad in ["/sup/livez", "/a/acc/admin", "/a/%2e%2e/v1/messages", "/etc/passwd", "/exec"] {
        let resp = env.http.get(format!("{}/n/{NODE}{bad}", env.local)).header("x-api-key", HUB_KEY).send().await.unwrap();
        assert_eq!(resp.status(), 403, "{bad}");
    }
    // 本地口要 key。
    let resp = env.http.get(format!("{}/status", env.local)).send().await.unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_disabled_and_revoked_are_marked() {
    let env = setup().await;
    let resp = env.http.post(format!("{}/n/n-other/a/acc/v1/messages", env.local)).header("x-api-key", HUB_KEY).send().await.unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers().get(gw_core::node::HDR_NODE).unwrap(), "offline");

    env.reg.disabled.store(true, Ordering::SeqCst);
    env.hub.refresh_flags();
    let resp = env.http.post(chat_url(&env, "acc")).header("x-api-key", HUB_KEY).body("{}").send().await.unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers().get(gw_core::node::HDR_NODE).unwrap(), "disabled");
    // 排空只挡对话,控制面照常。
    let resp = env.http.get(format!("{}/n/{NODE}/sup/accounts", env.local)).header("x-api-key", HUB_KEY).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    env.reg.revoked.store(true, Ordering::SeqCst);
    env.hub.refresh_flags();
    wait_online(&env, false).await;
    let resp = env.http.post(chat_url(&env, "acc")).header("x-api-key", HUB_KEY).body("{}").send().await.unwrap();
    assert_eq!(resp.headers().get(gw_core::node::HDR_NODE).unwrap(), "offline");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnects_after_agent_restart_and_applies_tombstones() {
    let env = setup().await;
    env.agent.abort();
    wait_online(&env, false).await;
    env.reg.tombstones.lock().unwrap().push("gone@x".into());
    let agent = tokio::spawn(crate::agent::run(agent_opts(&env.master, env.spki, &env.sup_url, "install-A")));
    wait_online(&env, true).await;
    for _ in 0..50 {
        if env.reg.tombstones.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(env.reg.tombstones.lock().unwrap().is_empty(), "重连后应补删");
    assert!(env.fake.calls.lock().unwrap().iter().any(|c| c.starts_with("DELETE gone")));
    agent.abort();
}

/// 直接握手:错误密钥、换了机器(install_id 不同)都被拒;指纹不符连 TLS 都过不去。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handshake_rejections() {
    let env = setup().await;
    let w = raw_hello(&env, env.spki, "wrong-secret-0000000000", "install-A", "h").await.unwrap();
    assert!(!w.ok && w.fatal);
    let w = raw_hello(&env, env.spki, SECRET, "install-B", "h").await.unwrap();
    assert!(!w.ok && w.fatal, "同一令牌换机器必须被拒");
    assert!(
        raw_hello(&env, [0u8; 32], SECRET, "install-A", "h").await.is_err(),
        "指纹不符必须握手失败"
    );
    assert!(node_online(&env).await, "被拒的连接不影响已连上的子机");
}

async fn raw_hello(
    env: &Env,
    pin: [u8; 32],
    secret: &str,
    install: &str,
    hostname: &str,
) -> anyhow::Result<Welcome> {
    let tcp = tokio::net::TcpStream::connect(&env.master).await?;
    let conn = tokio_rustls::TlsConnector::from(Arc::new(crate::tls::client_config(pin)?));
    let name = rustls::pki_types::ServerName::try_from(crate::tls::HUB_SERVER_NAME)?;
    let mut tls = conn.connect(name, tcp).await?;
    let h = Hello {
        proto: tunnel::PROTO,
        node_id: NODE.into(),
        secret: secret.into(),
        install_id: install.into(),
        version: "t".into(),
        hostname: hostname.into(),
    };
    tunnel::write_frame(&mut tls, &h).await?;
    tunnel::read_frame(&mut tls).await
}

/// 远端 bash:经隧道到子机执行,回显/非零退出码/超时杀进程/输出截断/环境清令牌,逐项锁住。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_echo_timeout_truncation_and_clean_env() {
    let env = setup().await;
    let url = format!("{}/n/{NODE}/exec", env.local);
    let post = |body: serde_json::Value| {
        env.http
            .post(&url)
            .header("x-api-key", HUB_KEY)
            .header(gw_core::node::HDR_EXEC_KEY, EXEC_KEY)
            .json(&body)
            .send()
    };

    // 回显 + 非零退出码原样回来。
    let v: serde_json::Value = post(serde_json::json!({"command": "echo hello; echo oops >&2; exit 3"}))
        .await.unwrap().json().await.unwrap();
    assert_eq!(v["exit_code"], 3);
    assert_eq!(v["stdout"], "hello\n");
    assert_eq!(v["stderr"], "oops\n");
    assert_eq!(v["timed_out"], false);

    // 子进程环境必须干净:节点进程自己带着 CAIO_TOKEN / DARIO_API_KEY,不能泄进命令输出。
    let v: serde_json::Value = post(serde_json::json!({"command": "env | grep -cE 'CAIO_TOKEN|DARIO_API_KEY' || true"}))
        .await.unwrap().json().await.unwrap();
    assert_eq!(v["stdout"], "0\n", "令牌不得进子进程环境");

    // 超时:被杀掉,timed_out=true,exit_code 为 null(信号终止没有退出码)。
    let started = std::time::Instant::now();
    let v: serde_json::Value = post(serde_json::json!({"command": "sleep 30", "timeout_secs": 1}))
        .await.unwrap().json().await.unwrap();
    assert_eq!(v["timed_out"], true);
    assert!(v["exit_code"].is_null());
    assert!(started.elapsed() < Duration::from_secs(10), "超时后应迅速返回");

    // 截断:300KB 输出只留 256KB,但命令能跑完(读者会一直读到 EOF,不堵管道)。
    let v: serde_json::Value = post(serde_json::json!({"command": "head -c 300000 /dev/zero | tr '\\0' x"}))
        .await.unwrap().json().await.unwrap();
    assert_eq!(v["stdout"].as_str().unwrap().len(), 256 * 1024);
    assert_eq!(v["stdout_truncated"], true);
    assert_eq!(v["exit_code"], 0);

    // 排空中也能用(运维要靠它排查;hub 只挡 /a/ 对话)。
    env.reg.disabled.store(true, Ordering::SeqCst);
    env.hub.refresh_flags();
    let v: serde_json::Value = post(serde_json::json!({"command": "true"})).await.unwrap().json().await.unwrap();
    assert_eq!(v["exit_code"], 0);
    env.reg.disabled.store(false, Ordering::SeqCst);
    env.hub.refresh_flags();

    // 离线:hub 直接 503 + offline 标记,命令肯定没发出去。
    env.agent.abort();
    wait_online(&env, false).await;
    let resp = post(serde_json::json!({"command": "echo hi"})).await.unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers().get(gw_core::node::HDR_NODE).unwrap(), "offline");
}

/// exec 钥匙是独立的第二道:本地口 key 对了、exec 钥匙不对/缺失,一样 403。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_requires_its_own_key() {
    let env = setup().await;
    let url = format!("{}/n/{NODE}/exec", env.local);
    // 没带头 / 错钥匙:403,命令根本没进子机。
    for hdr in [None, Some("wrong-exec-key")] {
        let mut rb = env.http.post(&url).header("x-api-key", HUB_KEY);
        if let Some(k) = hdr {
            rb = rb.header(gw_core::node::HDR_EXEC_KEY, k);
        }
        let resp = rb.json(&serde_json::json!({"command": "echo hi"})).send().await.unwrap();
        assert_eq!(resp.status(), 403);
    }
    // 主机没配 exec_key 时整个功能 fail-closed。
    let reg = Arc::new(FakeRegistry::default());
    let hub = Arc::new(Hub::new(reg, HUB_KEY.into(), None, "x".into()));
    hub.refresh_flags();
    let ll = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = format!("http://{}", ll.local_addr().unwrap());
    let app = hub.clone().local_router();
    tokio::spawn(async move { axum::serve(ll, app).await.unwrap() });
    let resp = env
        .http
        .post(format!("{local}/n/{NODE}/exec"))
        .header("x-api-key", HUB_KEY)
        .header(gw_core::node::HDR_EXEC_KEY, EXEC_KEY)
        .json(&serde_json::json!({"command": "echo hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "没配 exec_key 时必须 fail-closed");
}

/// 超时要杀掉的是整个进程组,不只是 bash:`sleep N &` 留下的后台进程不能活过 exec 结束。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_timeout_kills_whole_process_group() {
    let env = setup().await;
    let url = format!("{}/n/{NODE}/exec", env.local);
    // 挑一个古怪时长,pgrep 不会撞上别的进程。
    let v: serde_json::Value = env
        .http
        .post(&url)
        .header("x-api-key", HUB_KEY)
        .header(gw_core::node::HDR_EXEC_KEY, EXEC_KEY)
        .json(&serde_json::json!({"command": "sleep 217 & echo started", "timeout_secs": 5}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["stdout"], "started\n");
    assert_eq!(v["exit_code"], 0);
    // 命令是正常退出的(bash 先退、孙子还在),但进程组杀必须把它带走。
    let out = std::process::Command::new("pgrep").args(["-f", "sleep 217"]).output().unwrap();
    assert!(out.stdout.is_empty(), "后台孙子进程不应存活: {}", String::from_utf8_lossy(&out.stdout));
}

/// 克隆了 `/etc/caio/node.env`(或整盘镜像)的第二台机器:令牌与 install_id 都对,
/// 所以认证过得去 —— 必须靠「旧连接探活」挡住:旧连接活着就拒新保旧,**没有年龄窗口**。
///
/// 为什么非挡不可:两台机器都在线时,它们的 dario 会各自用**同一条** refresh token 刷新,
/// 上游的复用检测会把这个号整族作废,而 caio 侧的 refresh_token 在交接时已清空 —— 救不回来。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clone_with_same_install_id_is_refused_and_original_kept() {
    let env = setup().await;
    assert!(node_online(&env).await);
    // 主机名不同 → 触发探活;旧连接活着,新连接拿到明确拒绝(可重试,不是 fatal)。
    let w = raw_hello(&env, env.spki, SECRET, "install-A", "另一台机器").await.unwrap();
    assert!(!w.ok, "旧连接在线时,另一台机器必须被拒");
    assert!(!w.fatal, "拒绝要可重试(旧机器下线后它该能接管)");
    assert!(w.error.unwrap_or_default().contains("仍在线"));
    // 原来那台必须还在:宁可新机器连不上(运维看得见、可以去后台解除绑定),
    // 也不能让两台机器轮流持有同一个号。
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(node_online(&env).await, "克隆机器不得把在线的那台顶掉");
    // 在线的那台仍然能正常服务。
    let resp = env
        .http
        .get(format!("{}/n/{NODE}/info", env.local))
        .header("x-api-key", HUB_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// 旧连接已死(子机进程被杀)时,同一令牌从别的机器重连要放行的唯一场景:
/// 探活失败 → 判定旧的下线 → 新连接接管。这正是「换机器」的正常路径。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_from_new_machine_succeeds_once_old_is_dead() {
    let env = setup().await;
    assert!(node_online(&env).await);
    env.agent.abort();
    wait_online(&env, false).await;
    // 主机名不同也没关系:旧连接已死,探活失败,新连接接管。
    let mut opts = agent_opts(&env.master, env.spki, &env.sup_url, "install-A");
    let agent = tokio::spawn(crate::agent::run({
        opts.version = "test-2".into();
        opts
    }));
    wait_online(&env, true).await;
    agent.abort();
}
