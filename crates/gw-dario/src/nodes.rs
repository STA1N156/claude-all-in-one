//! 子机号的路由:按 `extra.dario_node` 选子机上的管理器,并维护各子机的在线快照。
//!
//! 快照每 2 秒从 hub 本地口 `/status` 拉一次,worker 选号时据此把离线/排空子机上的号
//! 直接排除([`NodeRouter::routable`])。拉不到或太旧时**按不可用处理**(fail-closed):
//! hub 都不通,子机号的请求反正也发不出去。

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use gw_core::error::{UpstreamError, UpstreamErrorKind};

use crate::supervisor::Supervisor;

const POLL_EVERY: Duration = Duration::from_secs(2);
/// 快照超过这个年龄就不再信它。
const STALE_AFTER: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeState {
    online: bool,
    disabled: bool,
    /// 子机上的 dario 管理器是否明确报错(hub 的 `/info` 拉回来的)。
    ///
    /// 「隧道活着」不等于「这台机器能干活」:`caio-node` 在跑而 `caio-dario` 崩了/端口段
    /// 耗尽/磁盘写不下凭证时,号照样被选中,然后每条请求白等一个 ensure 超时才失败。
    /// 真机上「机器在、服务不在」比「整机离线」常见得多。
    /// 只在**明确报错**时才算不可用;还没拉到 info(刚连上)按可用,不给冷启动开洞。
    supervisor_down: bool,
}

#[derive(Default)]
struct Snapshot {
    nodes: HashMap<String, NodeState>,
    fetched_at: Option<Instant>,
    /// 上一次拉取是否成功(只用于日志去重)。
    last_ok: Option<bool>,
}

pub(crate) struct NodeRouter {
    hub_url: String,
    api_key: String,
    client: reqwest::Client,
    sups: Mutex<HashMap<String, Arc<Supervisor>>>,
    snapshot: Arc<RwLock<Snapshot>>,
    poller_started: AtomicBool,
}

impl NodeRouter {
    pub fn new(hub_url: &str, api_key: &str, client: reqwest::Client) -> Self {
        Self {
            hub_url: hub_url.trim().trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            client,
            sups: Mutex::new(HashMap::new()),
            snapshot: Arc::new(RwLock::new(Snapshot::default())),
            poller_started: AtomicBool::new(false),
        }
    }

    /// 该子机的管理器客户端(按需建,之后复用:交接缓存与串行锁跟着它)。
    pub fn supervisor(&self, node_id: &str) -> Result<Arc<Supervisor>, UpstreamError> {
        let mut m = self.sups.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(s) = m.get(node_id) {
            return Ok(s.clone());
        }
        let s = Supervisor::for_node(&self.hub_url, node_id, &self.api_key, self.client.clone())
            .map_err(|e| UpstreamError::new(UpstreamErrorKind::BadRequest, format!("dario 子机配置错误: {e}")))?;
        let s = Arc::new(s);
        m.insert(node_id.to_string(), s.clone());
        Ok(s)
    }

    /// 选号过滤:子机在线、没被排空、且它上面的管理器没报错才可用。
    /// 快照缺失或过旧 → 不可用(fail-closed)。
    pub fn routable(&self, node_id: &str) -> bool {
        self.ensure_poller();
        let snap = self.snapshot.read().unwrap_or_else(|p| p.into_inner());
        if !snap.fetched_at.is_some_and(|t| t.elapsed() < STALE_AFTER) {
            return false;
        }
        snap.nodes
            .get(node_id)
            .is_some_and(|s| s.online && !s.disabled && !s.supervisor_down)
    }

    pub fn drain_updates(&self) -> Vec<(String, BTreeMap<String, serde_json::Value>)> {
        let sups: Vec<Arc<Supervisor>> = self.sups.lock().unwrap_or_else(|p| p.into_inner()).values().cloned().collect();
        sups.iter().flat_map(|s| s.drain_updates()).collect()
    }

    /// 有 tokio 运行时才起(单元测试里构造 provider 时没有)。
    pub fn ensure_poller(&self) {
        if self.poller_started.load(Ordering::Relaxed) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        if self.poller_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(&self.snapshot);
        let url = format!("{}/status", self.hub_url);
        let (client, key) = (self.client.clone(), self.api_key.clone());
        handle.spawn(poll_loop(weak, client, url, key));
    }
}

async fn poll_loop(snapshot: Weak<RwLock<Snapshot>>, client: reqwest::Client, url: String, key: String) {
    loop {
        let result = fetch(&client, &url, &key).await;
        let Some(snap) = snapshot.upgrade() else { return };
        apply(&snap, result);
        drop(snap);
        tokio::time::sleep(POLL_EVERY).await;
    }
}

async fn fetch(client: &reqwest::Client, url: &str, key: &str) -> anyhow::Result<HashMap<String, NodeState>> {
    let v: serde_json::Value = client
        .get(url)
        .header("x-api-key", key)
        .timeout(Duration::from_secs(3))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(parse_status(&v))
}

fn parse_status(v: &serde_json::Value) -> HashMap<String, NodeState> {
    v.get("nodes")
        .and_then(|n| n.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|n| {
                    let id = n.get("id")?.as_str()?.to_string();
                    let online = n.get("online").and_then(|x| x.as_bool()).unwrap_or(false);
                    let disabled = n.get("disabled").and_then(|x| x.as_bool()).unwrap_or(false);
                    // info 缺失(刚连上、还没拉过)≠ 管理器坏了,所以只认显式的 error 字段。
                    let supervisor_down = n
                        .pointer("/info/supervisor/error")
                        .is_some_and(|e| !e.is_null());
                    Some((id, NodeState { online, disabled, supervisor_down }))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn apply(snap: &RwLock<Snapshot>, result: anyhow::Result<HashMap<String, NodeState>>) {
    let mut s = snap.write().unwrap_or_else(|p| p.into_inner());
    match result {
        Ok(nodes) => {
            let usable = |x: &NodeState| x.online && !x.disabled && !x.supervisor_down;
            for (id, st) in &nodes {
                let before = s.nodes.get(id).copied();
                if before.as_ref().map(usable) != Some(usable(st)) {
                    tracing::info!(
                        node = %id, online = st.online, disabled = st.disabled,
                        supervisor_down = st.supervisor_down, "dario 子机状态变化"
                    );
                }
            }
            if s.last_ok == Some(false) {
                tracing::info!("hub 已恢复可达");
            }
            s.nodes = nodes;
            s.fetched_at = Some(Instant::now());
            s.last_ok = Some(true);
        }
        Err(e) => {
            // 只在转为失败时记一次;没部署 hub 的环境不刷屏。
            if s.last_ok != Some(false) {
                tracing::warn!(error = %e, "拉取子机状态失败(子机上的号暂不参与选号)");
            }
            s.last_ok = Some(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routable_needs_fresh_online_enabled_node() {
        let r = NodeRouter::new("http://127.0.0.1:1", "k", reqwest::Client::new());
        assert!(!r.routable("n-1"), "还没拉到快照:不可用");
        let v = serde_json::json!({"nodes": [
            {"id": "n-1", "online": true, "disabled": false},
            {"id": "n-2", "online": false, "disabled": false},
            {"id": "n-3", "online": true, "disabled": true},
            // 隧道活着但子机上的管理器崩了:不该再往它派号。
            {"id": "n-4", "online": true, "disabled": false,
             "info": {"supervisor": {"error": "connection refused"}}},
            // 管理器正常(有 accounts、没有 error):可用。
            {"id": "n-5", "online": true, "disabled": false,
             "info": {"supervisor": {"accounts": []}}},
        ]});
        apply(&r.snapshot, Ok(parse_status(&v)));
        assert!(r.routable("n-1"), "还没拉到 info 不算坏");
        assert!(!r.routable("n-2"));
        assert!(!r.routable("n-3"));
        assert!(!r.routable("n-4"), "管理器报错 → 不参与选号");
        assert!(r.routable("n-5"));
        assert!(!r.routable("n-9"));
        // 拉取失败不清空旧快照,但旧快照过期后一律不可用。
        apply(&r.snapshot, Err(anyhow::anyhow!("down")));
        assert!(r.routable("n-1"));
        r.snapshot.write().unwrap().fetched_at = Some(Instant::now() - STALE_AFTER - Duration::from_secs(1));
        assert!(!r.routable("n-1"));
    }

    #[test]
    fn supervisor_is_cached_and_id_checked() {
        let r = NodeRouter::new("http://127.0.0.1:8790/", "k", reqwest::Client::new());
        let a = r.supervisor("n-1").unwrap();
        let b = r.supervisor("n-1").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.node(), Some("n-1"));
        assert_eq!(a.node_chat_url("a@b.com", "/v1/messages"), "http://127.0.0.1:8790/n/n-1/a/a@b.com/v1/messages");
        assert_eq!(a.node_chat_url("a b/c", "/v1/messages"), "http://127.0.0.1:8790/n/n-1/a/a%20b%2Fc/v1/messages");
        assert!(r.supervisor("../x").is_err());
    }
}
