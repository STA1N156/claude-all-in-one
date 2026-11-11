//! gw-node —— 子机(dario 节点)。
//!
//! 子机主动连主机的 [`hub`](公网 TLS 口),握手通过后 hub 在这条连接上当 HTTP/2 客户端、
//! 子机([`agent`])当服务端:主机 worker 发来的请求经 hub 转进子机,子机转给本机的
//! dario 管理器 / dario 进程,流式结果原路返回。号的账号库、调度、计费都留在主机。
//!
//! - [`join`]   加入令牌与指纹的编解码
//! - [`tls`]    hub 自签证书 + 子机侧的指纹校验
//! - [`tunnel`] 握手帧与 HTTP/2 参数
//! - [`hub`]    主机侧
//! - [`agent`]  子机侧

pub mod agent;
pub mod hub;
pub mod join;
pub mod tls;
pub mod tunnel;

use axum::http::HeaderMap;

/// 隧道里 HTTP/2 请求的 `:authority`。只是占位,子机不看它。
pub(crate) const NODE_AUTHORITY: &str = "caio-node";

/// HTTP/2 禁止(或对端会拒收)的逐跳头。
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
];

pub(crate) fn filter_headers(h: &mut HeaderMap) {
    for name in HOP_BY_HOP {
        h.remove(*name);
    }
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
