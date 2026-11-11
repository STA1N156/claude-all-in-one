//! 子机(dario 节点)协议常量与小工具:hub、node、gw-dario、admin 共用。
//!
//! 子机主动连主机的 hub,hub 在这条长连接上把 worker 发来的请求转进子机;子机上跑
//! dario 管理器和每号 dario 进程。号归哪台子机记在 `extra.dario_node`,为空 = 本机管理器。

use crate::account::Account;

/// 号所属子机的 extra 键。只在上号时写一次,之后不可改(令牌只能有一个持有者)。
pub const NODE_EXTRA_KEY: &str = "dario_node";

/// hub 自己给出的失败标记(子机离线 / 被禁用 / 请求途中隧道断了)。
/// 带这个头的响应不是 Anthropic 或 dario 的回答,不能按上游状态码分类。
pub const HDR_NODE: &str = "x-caio-node";
/// 远端 bash 的独立钥匙头(admin → hub;与本地口 key 分开)。
pub const HDR_EXEC_KEY: &str = "x-caio-exec-key";
/// 子机自己给出的「阶段」标记(本机 ensure 失败等):带它的响应不是 dario 的回答。
/// agent 生成;回程会被 agent 从 dario 的响应里剥掉(与 HDR_NODE 同口径)。
pub const HDR_STAGE: &str = "x-caio-stage";
pub const STAGE_ENSURE: &str = "ensure";
/// agent 自己产的 502(连不上 dario / dario 太久没回响应头)。见 agent::chat。
pub const STAGE_DARIO: &str = "dario";

/// 远端 bash(/exec)的共享上限:admin 与子机两侧校验用同一份,免得改一边漏一边。
pub const EXEC_CMD_MAX_CHARS: usize = 32 * 1024;
pub const EXEC_TIMEOUT_DEFAULT: u64 = 30;
/// 必须小于 hub 的 exec deadline(130s),否则「超时已杀」的回复发不回去。
pub const EXEC_TIMEOUT_MAX: u64 = 120;

/// hub 本地口的默认地址(worker / admin 经它访问子机)。
pub const DEFAULT_HUB_URL: &str = "http://127.0.0.1:8790";

/// 子机安装包(.run)在主机上的路径:环境变量 `CAIO_NODE_BUNDLE` 优先,
/// 否则 `data/node-bundle/caio-node.run`(docker 里 /app/data 随库一起挂卷)。
/// 由 `scripts/build-node-bundle.sh` 产出后 scp 上去;router 的 `/node-bundle` 按令牌分发。
pub fn bundle_path() -> std::path::PathBuf {
    match std::env::var("CAIO_NODE_BUNDLE") {
        Ok(p) if !p.trim().is_empty() => std::path::PathBuf::from(p),
        _ => std::path::PathBuf::from("data/node-bundle/caio-node.run"),
    }
}

/// hub 的失败标记。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeMark {
    /// 子机当前没有连上来。请求一个字节都没发出去。
    Offline,
    /// 子机被后台禁用(排空)。请求没有发出去。
    Disabled,
    /// 请求已交给隧道,途中连接断了。子机可能已经执行。
    Lost,
}

impl NodeMark {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeMark::Offline => "offline",
            NodeMark::Disabled => "disabled",
            NodeMark::Lost => "lost",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "offline" => Some(NodeMark::Offline),
            "disabled" => Some(NodeMark::Disabled),
            "lost" => Some(NodeMark::Lost),
            _ => None,
        }
    }

    /// 请求是否肯定没有到达子机(可放心换号)。
    pub fn not_submitted(self) -> bool {
        !matches!(self, NodeMark::Lost)
    }
}

/// 子机 ID 的合法形态:小写字母、数字、连字符,1~32 位。
/// 它会被拼进 URL 路径和 systemd 环境文件,所以收得很紧。
pub fn valid_node_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// 号所属子机;空串或缺失 = 本机。
pub fn account_node(account: &Account) -> Option<&str> {
    account
        .extra_str(NODE_EXTRA_KEY)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn node_id_charset_is_strict() {
        assert!(valid_node_id("n-1a2b3c4d"));
        assert!(!valid_node_id(""));
        assert!(!valid_node_id("N-1"));
        assert!(!valid_node_id("a/b"));
        assert!(!valid_node_id("a.b"));
        assert!(!valid_node_id(&"a".repeat(33)));
    }

    #[test]
    fn mark_roundtrip_and_delivery() {
        for m in [NodeMark::Offline, NodeMark::Disabled, NodeMark::Lost] {
            assert_eq!(NodeMark::parse(m.as_str()), Some(m));
        }
        assert!(NodeMark::Offline.not_submitted());
        assert!(!NodeMark::Lost.not_submitted());
        assert_eq!(NodeMark::parse("x"), None);
    }

    #[test]
    fn account_node_ignores_blank() {
        let mut extra = BTreeMap::new();
        extra.insert(NODE_EXTRA_KEY.to_string(), serde_json::json!("  "));
        let mut a = Account {
            account_id: "a".into(),
            provider: "claude-dario".into(),
            max_concurrency: 1,
            disabled: false,
            created_at: 0,
            extra,
        };
        assert_eq!(account_node(&a), None);
        a.extra.insert(NODE_EXTRA_KEY.to_string(), serde_json::json!("n-1"));
        assert_eq!(account_node(&a), Some("n-1"));
    }
}
