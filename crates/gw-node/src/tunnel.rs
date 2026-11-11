//! 隧道的握手帧与 HTTP/2 参数。
//!
//! 子机连上 hub、TLS 建好后,先交换一对定长头 + JSON 的握手帧(子机发 [`Hello`],hub 回
//! [`Welcome`]),然后**角色反转**:hub 在这条连接上当 HTTP/2 客户端,子机当服务端。
//! 帧长度前置、用 `read_exact` 读,保证不会多读走后面 HTTP/2 的前导字节。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 协议版本。hub 拒绝不认识的版本,子机据此提示升级。
pub const PROTO: u32 = 1;
const MAGIC: &[u8; 4] = b"CAIO";
const MAX_FRAME: u32 = 64 * 1024;
/// 握手整体时限(TLS + 两帧)。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// 两端的 HTTP/2 ping 间隔与超时:约 40s 内发现半死的连接。
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
pub const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);
/// 子机作为 HTTP/2 服务端的接收窗口:请求体(最大约 24MB)从主机上传过来,
/// 高 RTT 下默认 1MB 窗口会把上传拖到数秒并挡住同连接的其它请求。
pub const NODE_STREAM_WINDOW: u32 = 8 * 1024 * 1024;
pub const NODE_CONN_WINDOW: u32 = 32 * 1024 * 1024;
/// hub 作为客户端的接收窗口(下行是 SSE,量小)。
pub const HUB_STREAM_WINDOW: u32 = 4 * 1024 * 1024;
pub const HUB_CONN_WINDOW: u32 = 16 * 1024 * 1024;

/// 子机 → hub 的第一帧。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u32,
    pub node_id: String,
    pub secret: String,
    /// 安装时生成、写在子机本地的随机 ID。首次连接时与节点绑定,防一个令牌装两台机器。
    pub install_id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub hostname: String,
}

/// hub → 子机的回帧。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Welcome {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    /// 拒绝是否是「重试也没用」的那种(令牌错、被吊销、绑定了别的机器)。
    #[serde(default)]
    pub fatal: bool,
    #[serde(default)]
    pub node_name: Option<String>,
}

impl Welcome {
    pub fn accept(name: &str) -> Self {
        Self { ok: true, error: None, fatal: false, node_name: Some(name.to_string()) }
    }
    pub fn reject(error: impl Into<String>, fatal: bool) -> Self {
        Self { ok: false, error: Some(error.into()), fatal, node_name: None }
    }
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, v: &T) -> anyhow::Result<()> {
    let body = serde_json::to_vec(v)?;
    let len = u32::try_from(body.len()).map_err(|_| anyhow::anyhow!("握手帧过大"))?;
    if len > MAX_FRAME {
        anyhow::bail!("握手帧过大");
    }
    let mut buf = Vec::with_capacity(8 + body.len());
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(r: &mut R) -> anyhow::Result<T> {
    let mut head = [0u8; 8];
    r.read_exact(&mut head).await?;
    if &head[..4] != MAGIC {
        anyhow::bail!("不是 caio 子机协议");
    }
    let len = u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
    if len > MAX_FRAME {
        anyhow::bail!("握手帧过大");
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_roundtrip_without_overreading() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let hello = Hello {
            proto: PROTO,
            node_id: "n-1".into(),
            secret: "s".into(),
            install_id: "i".into(),
            version: "v".into(),
            hostname: "h".into(),
        };
        write_frame(&mut a, &hello).await.unwrap();
        // 紧跟在帧后面的字节(相当于 HTTP/2 前导)必须原样留在流里。
        a.write_all(b"PRI * HTTP/2.0").await.unwrap();
        let got: Hello = read_frame(&mut b).await.unwrap();
        assert_eq!(got.node_id, "n-1");
        let mut rest = [0u8; 14];
        b.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"PRI * HTTP/2.0");
    }

    #[tokio::test]
    async fn rejects_garbage_and_oversize() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        assert!(read_frame::<_, Hello>(&mut b).await.is_err());
        let (mut a, mut b) = tokio::io::duplex(1024);
        let mut head = Vec::from(&MAGIC[..]);
        head.extend_from_slice(&(MAX_FRAME + 1).to_be_bytes());
        a.write_all(&head).await.unwrap();
        assert!(read_frame::<_, Hello>(&mut b).await.is_err());
    }
}
