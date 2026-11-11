//! claude-all-in-one 二进制入口。
//!
//! 单二进制多角色:`--mode router` / `--mode worker --instance N`;
//! 子机相关:`--mode hub`(主机上等子机来连)/ `--mode node`(子机)。
//! 见 docs/ARCHITECTURE.md §1。

mod admin;
mod egress;
mod node_modes;
mod registry;
mod restock;
mod router;
mod websearch;
mod worker;

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// 本二进制构建自哪个 git 提交(构建时由 `GW_BUILD_COMMIT` 注入,见 Dockerfile 与
/// `scripts/build-image.sh`)。router / worker 的 `/health` 都回显它,用来核对「线上跑的
/// 就是这个提交」—— 10-23 起有三次回归都是从不受 git 管的服务器源码目录构建造成的。
/// 本地 `cargo build` 不注入时为 `unknown`。
pub const BUILD_COMMIT: &str = match option_env!("GW_BUILD_COMMIT") {
    Some(c) => c,
    None => "unknown",
};

/// 内网头:router 鉴权后把客户 key 透传给 worker,worker 据此把 usage 归属到该客户。
/// 仅在 router→worker 的 localhost 内网跳使用(对外 Authorization 不透传给 worker)。
pub const CLIENT_KEY_HEADER: &str = "x-gw-client-key";

/// 内网头:本次请求所属的**分组名**。worker 据此取该组的成员视图(哪些号可见 +
/// 组内优先级),不在成员集里的号对本次请求不存在。
///
/// **只由 router 依据客户 key 的分组生成**;`send_messages_to_worker` 是白名单转发,
/// 客户端自带的同名头到不了 worker,因此无法伪造/绕过分组。
/// 头缺席 = 未分组请求(worker 用全量池,与本次重构之前逐字节相同)。
pub const GROUP_HEADER: &str = "x-gw-group";

/// worker 明确未处理该协议入口；只由本地入口拒绝响应设置，router 不向客户透传。
pub(crate) const ENDPOINT_UNAVAILABLE_HEADER: &str = "x-gw-endpoint-unavailable";

/// 优雅停机信号:SIGTERM(docker stop / systemd)或 Ctrl-C。
/// 触发后 axum 停止接收新连接,在途请求(含流式 SSE)自然跑完;不设排空上限——
/// 硬截止由 supervisor 兜底(docker 默认 10s 后 SIGKILL,systemd TimeoutStopSec)。
pub(crate) async fn shutdown_signal(role: &'static str) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!("注册 SIGTERM 监听失败,仅响应 Ctrl-C: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("{role} 收到停机信号:停止接收新连接,等待在途请求完成");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// 前置路由:鉴权 + session→worker 亲和 + 转发。
    Router,
    /// 实际反代:绑定固定出口 + 管理一组账号。
    Worker,
    /// 主机侧子机 hub:公网 TLS 口等子机来连,本地口把 worker/admin 的请求转进子机。
    Hub,
    /// 子机:主动连主机 hub,把主机的请求转给本机的 dario 管理器 / dario 进程。
    Node,
}

#[derive(Debug, Parser)]
#[command(name = "claude-all-in-one", about = "Claude All in One —— 多账号多进程反代")]
struct Args {
    /// 进程角色。
    #[arg(long, value_enum)]
    mode: Mode,

    /// worker 实例号(--mode worker 必填,对应 instances.yaml 的 instance)。
    #[arg(long)]
    instance: Option<u32>,



    /// instances.yaml 路径。
    #[arg(long, default_value = "config/instances.yaml")]
    instances: PathBuf,

    /// accounts.yaml 路径。
    #[arg(long, default_value = "config/accounts.yaml")]
    accounts: PathBuf,

    /// system.yaml 路径。
    #[arg(long, default_value = "config/system.yaml")]
    system: PathBuf,

    /// SQLite 控制面路径。
    #[arg(long, default_value = "data/control.db")]
    db: PathBuf,

    /// hub:子机连进来的公网监听地址。
    #[arg(long, default_value = "0.0.0.0:38993")]
    hub_listen: String,

    /// hub:给本机 worker/admin 用的回环监听地址。
    #[arg(long, default_value = "127.0.0.1:8790")]
    hub_local: String,

    /// hub:子机连主机用的地址(`host:port`),写进后台生成的完整令牌。
    #[arg(long)]
    public_addr: Option<String>,

    /// hub:自签证书目录(要和库一起备份:证书变了所有子机都连不上)。
    #[arg(long, default_value = "data/node-hub")]
    cert_dir: PathBuf,

    /// 子机:主机 hub 地址 `host:port`(也可用 CAIO_MASTER;完整令牌自带)。
    #[arg(long)]
    master: Option<String>,

    /// 子机:主机证书指纹(也可用 CAIO_FP;完整令牌自带)。
    #[arg(long)]
    fp: Option<String>,

    /// 子机:加入令牌(也可用 CAIO_TOKEN)。
    #[arg(long)]
    token: Option<String>,

    /// 子机:安装 ID(也可用 CAIO_INSTALL_ID)。
    #[arg(long)]
    install_id: Option<String>,

    /// 子机:本机 dario 管理器地址(也可用 CAIO_SUP_URL,默认 http://127.0.0.1:8788)。
    #[arg(long)]
    sup_url: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    match args.mode {
        Mode::Router => {
            tracing::info!("启动 router 角色");
            router::run(&args.instances, &args.db, &args.system).await
        }
        Mode::Worker => {
            let instance = args.instance.ok_or_else(|| {
                anyhow::anyhow!("--mode worker 需要 --instance N")
            })?;
            tracing::info!(instance, "启动 worker 角色");
            worker::run(
                instance,
                &args.instances,
                &args.accounts,
                &args.system,
                &args.db,
            )
            .await
        }
        Mode::Hub => {
            tracing::info!(version = %node_modes::version(), "启动子机 hub 角色");
            node_modes::run_hub(node_modes::HubArgs {
                system: &args.system,
                db: &args.db,
                listen: args.hub_listen,
                local: args.hub_local,
                public_addr: args.public_addr,
                cert_dir: args.cert_dir,
            })
            .await
        }
        Mode::Node => {
            let opts = node_modes::node_options(node_modes::NodeArgs {
                master: args.master,
                fp: args.fp,
                token: args.token,
                install_id: args.install_id,
                sup_url: args.sup_url,
            })?;
            tracing::info!(version = %opts.version, node = %opts.node_id, master = %opts.master, "启动子机角色");
            gw_node::agent::run(opts).await
        }
    }
}
