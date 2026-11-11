//! `--mode hub`(主机)与 `--mode node`(子机)的入口,以及 hub 读写主机库的实现。
//!
//! 两个模式的参数都不进 system.yaml(那份文件所有容器共用,老镜像对新字段零容忍):
//! hub 用命令行参数,子机用环境变量(systemd 的 EnvironmentFile)或同名命令行参数。

use std::path::Path;
use std::sync::Arc;

use gw_core::config::SystemConfig;
use gw_node::hub::{NodeAuth, NodeFlags, NodeRegistry, Reject};
use gw_node::tunnel::Hello;
use gw_store::{BindOutcome, SqliteStore};

/// 版本号:构建时可用 `CAIO_BUILD` 注入提交号。
pub fn version() -> String {
    match option_env!("CAIO_BUILD") {
        Some(b) if !b.is_empty() => format!("{}+{b}", env!("CARGO_PKG_VERSION")),
        _ => env!("CARGO_PKG_VERSION").to_string(),
    }
}

pub struct HubArgs<'a> {
    pub system: &'a Path,
    pub db: &'a Path,
    pub listen: String,
    pub local: String,
    pub public_addr: Option<String>,
    pub cert_dir: std::path::PathBuf,
}

pub async fn run_hub(a: HubArgs<'_>) -> anyhow::Result<()> {
    let system = load_system(a.system)?;
    // 库打不开就不启动:hub 的鉴权全靠库里的令牌摘要,不能「没库就放行」。
    let store = Arc::new(
        SqliteStore::open(a.db).map_err(|e| anyhow::anyhow!("打开 {} 失败,hub 拒绝启动: {e}", a.db.display()))?,
    );
    // compose 里 `--public-addr ${CAIO_HUB_PUBLIC_ADDR:-}` 没设时传进来的是空串:
    // 当成「没给」(保留库里上次的值),不是非法值。
    let public_addr = a.public_addr.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(p) = &public_addr {
        gw_node::join::validate_master(p)?;
    }
    let opts = gw_node::hub::HubOptions {
        public_listen: a.listen.parse().map_err(|e| anyhow::anyhow!("--hub-listen 非法: {e}"))?,
        local_listen: a.local.parse().map_err(|e| anyhow::anyhow!("--hub-local 非法: {e}"))?,
        cert_dir: a.cert_dir,
        public_addr,
        local_key: system.dario.api_key.clone(),
        exec_key: system.effective_node_exec_key(),
    };
    if !opts.local_listen.ip().is_loopback() {
        anyhow::bail!("--hub-local 必须是回环地址(它转发到子机,只给本机 worker/admin 用)");
    }
    gw_node::hub::run(opts, Arc::new(SqliteRegistry { store })).await
}

fn load_system(path: &Path) -> anyhow::Result<SystemConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_yaml::from_str(&text).map_err(|e| anyhow::anyhow!("{} 解析失败: {e}", path.display())),
        Err(e) => {
            tracing::info!("{} 读取不到({e}),SystemConfig 全用默认值", path.display());
            Ok(SystemConfig::default())
        }
    }
}

pub struct SqliteRegistry {
    pub store: Arc<SqliteStore>,
}

impl NodeRegistry for SqliteRegistry {
    fn authenticate(&self, hello: &Hello) -> Result<NodeAuth, Reject> {
        let transient = |e: anyhow::Error| Reject { reason: format!("主机暂时无法校验令牌: {e}"), fatal: false };
        let fatal = |m: &str| Reject { reason: m.to_string(), fatal: true };
        if !gw_core::node::valid_node_id(&hello.node_id) {
            return Err(fatal("节点 ID 非法"));
        }
        let install = hello.install_id.trim();
        if install.is_empty() || install.len() > 128 {
            return Err(fatal("子机缺少安装 ID(请用安装包重新安装)"));
        }
        let row = self.store.get_node(&hello.node_id).map_err(transient)?;
        let Some(row) = row else {
            return Err(fatal("节点不存在或已被吊销"));
        };
        let given = gw_node::join::secret_hash(&hello.secret);
        // 优先认加入令牌;对不上再认安装票据的密钥段 —— 一行安装命令(/i/ 脚本)传给
        // 子机的就是它,装好后它即子机的长期密钥,所以**不看过期**(票据过期只封下载口)。
        // 防克隆不受影响:下面 bind_node_install 的 install_id 检查照旧拦第二台机器。
        if !ct_eq(given.as_bytes(), row.token_hash.as_bytes()) {
            let ticket = format!("caiob1.{}.{}", hello.node_id, hello.secret);
            let ok = self
                .store
                .get_kv(&crate::admin::nodes::bundle_ticket_key(&hello.node_id))
                .map_err(transient)?
                .is_some_and(|v| crate::admin::nodes::bundle_ticket_matches(&v, &ticket));
            if !ok {
                return Err(fatal("令牌不对"));
            }
        }
        match self.store.bind_node_install(&hello.node_id, install).map_err(transient)? {
            BindOutcome::Ok => Ok(NodeAuth { name: row.name }),
            BindOutcome::NotFound => Err(fatal("节点不存在或已被吊销")),
            BindOutcome::Mismatch => Err(fatal(
                "这个令牌已经绑定了另一台机器。若确实是重装,请在后台对该子机点「解除绑定」后再连",
            )),
        }
    }

    fn node_flags(&self) -> anyhow::Result<std::collections::HashMap<String, NodeFlags>> {
        Ok(self
            .store
            .list_nodes()?
            .into_iter()
            .map(|r| (r.id, NodeFlags { name: r.name, disabled: r.disabled }))
            .collect())
    }

    fn record_seen(&self, node_id: &str, ip: &str, version: &str, info: Option<&serde_json::Value>) {
        let info = info.map(|v| v.to_string());
        if let Err(e) = self.store.touch_node(node_id, ip, version, info.as_deref()) {
            tracing::warn!(node = %node_id, error = %e, "记录子机状态失败");
        }
    }

    fn tombstones(&self, node_id: &str) -> Vec<String> {
        let all = self.store.list_node_tombstones(node_id).unwrap_or_default();
        // 防「旧待删记录杀掉新凭证」:子机离线时删号 → 记 tombstone;之后同 ID 在同一子机
        // 重新上号(交接不需要子机在线)。不过滤的话,子机重连后的补删会落在交接之后,
        // 把新令牌删掉 —— 而主机侧的 refresh_token 交接时已清空,号救不回来。
        // 所以:库里当前仍挂在这台子机上的号,不补删(它的凭证是新的,不是旧记录要清的那份)。
        let current: std::collections::HashSet<String> = self
            .store
            .list_accounts()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| crate::admin::accounts::row_node(&a.extra).as_deref() == Some(node_id))
            .map(|a| a.account_id)
            .collect();
        all.into_iter().filter(|id| !current.contains(id)).collect()
    }

    fn clear_tombstone(&self, node_id: &str, account_id: &str) {
        if let Err(e) = self.store.remove_node_tombstone(node_id, account_id) {
            tracing::warn!(node = %node_id, account = %account_id, error = %e, "清除待删记录失败");
        }
    }

    fn save_identity(&self, spki_b64: &str, public_addr: Option<&str>) {
        let old = self
            .store
            .get_kv(gw_store::KEY_NODE_HUB_IDENTITY)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .unwrap_or_default();
        // 指纹变了 = 证书被重新生成了(忘挂 ./data 卷、--cert-dir 与 compose 不一致、
        // 清过 data 目录)。后果是**所有子机 TLS 握手失败**,而主机侧只看到「全部离线」、
        // 后台新发的令牌又带着新指纹、一切看起来正常 —— 极易误判成网络问题。
        // 这里必须吼一声:它是唯一能把人指向真因的线索。
        if let Some(prev) = old.get("spki").and_then(|x| x.as_str()) {
            if prev != spki_b64 {
                tracing::error!(
                    prev = %prev, now = %spki_b64,
                    "⚠ hub 证书指纹变了:现有子机会全部连不上(TLS 指纹不符)。\
                     多半是 data/node-hub 没挂上或被清过 —— 要么把原证书放回去,\
                     要么给每台子机重发令牌"
                );
            }
        }
        // 没给对外地址时保留上次的(重启 hub 忘了带参数,不该把后台的安装命令弄坏)。
        let old_addr = old.get("public_addr").and_then(|x| x.as_str()).map(str::to_string);
        let addr = public_addr.map(str::to_string).or(old_addr);
        let v = serde_json::json!({ "spki": spki_b64, "public_addr": addr });
        if let Err(e) = self.store.upsert_kv(gw_store::KEY_NODE_HUB_IDENTITY, &v.to_string()) {
            tracing::warn!(error = %e, "保存 hub 身份失败(后台生成的安装命令将缺主机地址/指纹)");
        }
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 子机参数:命令行优先,其次环境变量。
pub struct NodeArgs {
    pub master: Option<String>,
    pub fp: Option<String>,
    pub token: Option<String>,
    pub install_id: Option<String>,
    pub sup_url: Option<String>,
}

fn pick(cli: Option<String>, env: &str) -> Option<String> {
    cli.or_else(|| std::env::var(env).ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn node_options(a: NodeArgs) -> anyhow::Result<gw_node::agent::AgentOptions> {
    let token_raw = pick(a.token, "CAIO_TOKEN").ok_or_else(|| anyhow::anyhow!("缺少加入令牌(--token 或 CAIO_TOKEN)"))?;
    let token = gw_node::join::JoinToken::parse(&token_raw)?;
    // 完整令牌自带主机与指纹;显式给的 --master/--fp 优先(换主机时用)。
    let master = pick(a.master, "CAIO_MASTER")
        .or(token.master.clone())
        .ok_or_else(|| anyhow::anyhow!("缺少主机地址(--master 或 CAIO_MASTER,或使用完整令牌)"))?;
    gw_node::join::validate_master(&master)?;
    let spki = match pick(a.fp, "CAIO_FP") {
        Some(fp) => gw_node::join::parse_spki(&fp)?,
        None => token.spki.ok_or_else(|| anyhow::anyhow!("缺少主机证书指纹(--fp 或 CAIO_FP,或使用完整令牌)"))?,
    };
    let install_id = pick(a.install_id, "CAIO_INSTALL_ID")
        .ok_or_else(|| anyhow::anyhow!("缺少安装 ID(CAIO_INSTALL_ID,安装脚本会生成)"))?;
    let dario_key = pick(None, "DARIO_API_KEY").ok_or_else(|| anyhow::anyhow!("缺少 DARIO_API_KEY"))?;
    let sup_url = pick(a.sup_url, "CAIO_SUP_URL").unwrap_or_else(|| gw_dario::DEFAULT_SUPERVISOR_URL.to_string());
    Ok(gw_node::agent::AgentOptions {
        master,
        spki,
        node_id: token.node_id,
        secret: token.secret,
        install_id,
        sup_url,
        dario_key,
        version: version(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(id: &str, secret: &str, install: &str) -> Hello {
        Hello { proto: 1, node_id: id.into(), secret: secret.into(), install_id: install.into(), version: "v".into(), hostname: "h".into() }
    }

    #[test]
    fn registry_checks_token_binding_and_revocation() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let secret = gw_node::join::new_secret();
        store.create_node("n-1", "东京", &gw_node::join::secret_hash(&secret)).unwrap();
        let r = SqliteRegistry { store: store.clone() };
        assert_eq!(r.authenticate(&hello("n-1", &secret, "A")).unwrap().name, "东京");
        assert!(r.authenticate(&hello("n-1", &secret, "A")).is_ok(), "同一台机器重连");
        let e = r.authenticate(&hello("n-1", &secret, "B")).unwrap_err();
        assert!(e.fatal && e.reason.contains("另一台机器"));
        assert!(r.authenticate(&hello("n-1", "wrong-wrong-wrong-wrong", "A")).unwrap_err().fatal);
        assert!(r.authenticate(&hello("n-1", &secret, "")).unwrap_err().fatal);
        store.delete_node("n-1").unwrap();
        assert!(r.authenticate(&hello("n-1", &secret, "A")).unwrap_err().fatal, "吊销后拒绝");
    }

    #[test]
    fn registry_accepts_bundle_ticket_as_install_credential() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store.create_node("n-1", "东京", &gw_node::join::secret_hash(&gw_node::join::new_secret())).unwrap();
        let ticket = crate::admin::nodes::issue_bundle_ticket(&store, "n-1").unwrap();
        let tsecret = ticket.rsplit_once('.').unwrap().1.to_string();
        let r = SqliteRegistry { store: store.clone() };
        // 票据密钥可当加入凭证用,install_id 绑定照常;同一台机器重连也认。
        assert!(r.authenticate(&hello("n-1", &tsecret, "A")).is_ok());
        assert!(r.authenticate(&hello("n-1", &tsecret, "A")).is_ok());
        // 第二台机器拿同一票据:被绑定检查拒绝(防克隆不变量不受影响)。
        let e = r.authenticate(&hello("n-1", &tsecret, "B")).unwrap_err();
        assert!(e.fatal && e.reason.contains("另一台机器"));
        // 错密钥依然拒绝。
        assert!(r.authenticate(&hello("n-1", "wrong-wrong-wrong-wrong", "A")).unwrap_err().fatal);
        // 下载口过期后,已装子机的重连不受影响(hub 不看过期 —— 它已是子机的长期密钥)。
        store
            .upsert_kv(
                &crate::admin::nodes::bundle_ticket_key("n-1"),
                &serde_json::json!({"h": gw_node::join::secret_hash(&ticket), "exp": 1}).to_string(),
            )
            .unwrap();
        assert!(r.authenticate(&hello("n-1", &tsecret, "A")).is_ok(), "票据过期只封下载口,不能踢掉已装子机");
    }

    #[test]
    fn identity_keeps_previous_public_addr() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let r = SqliteRegistry { store: store.clone() };
        r.save_identity("SPKI1", Some("203.0.113.1:38993"));
        r.save_identity("SPKI2", None);
        let v: serde_json::Value =
            serde_json::from_str(&store.get_kv(gw_store::KEY_NODE_HUB_IDENTITY).unwrap().unwrap()).unwrap();
        assert_eq!(v["spki"], "SPKI2");
        assert_eq!(v["public_addr"], "203.0.113.1:38993");
    }

    #[test]
    fn node_options_from_full_token() {
        let t = gw_node::join::JoinToken {
            node_id: "n-1".into(),
            secret: gw_node::join::new_secret(),
            master: Some("203.0.113.1:38993".into()),
            spki: Some([5u8; 32]),
        };
        std::env::set_var("DARIO_API_KEY", "k-0123456789abcdef");
        let o = node_options(NodeArgs {
            master: None,
            fp: None,
            token: t.full(),
            install_id: Some("inst".into()),
            sup_url: None,
        })
        .unwrap();
        assert_eq!(o.master, "203.0.113.1:38993");
        assert_eq!(o.spki, [5u8; 32]);
        assert_eq!(o.sup_url, gw_dario::DEFAULT_SUPERVISOR_URL);
        // 短令牌缺主机 → 报错说清楚缺什么。
        let e = node_options(NodeArgs { master: None, fp: None, token: Some(t.short()), install_id: Some("i".into()), sup_url: None });
        assert!(e.is_err());
    }
}
