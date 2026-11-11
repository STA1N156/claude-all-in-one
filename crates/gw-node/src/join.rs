//! 加入令牌:子机用它向 hub 证明身份,并(完整形态下)告诉子机主机在哪、证书指纹是什么。
//!
//! - 短令牌 `<node_id>.<secret>`:主机地址与指纹取安装包里打包时注入的默认值;
//! - 完整令牌 `caio1.<b64url(master)>.<b64url(spki)>.<node_id>.<secret>`:自带主机与指纹,
//!   换主机时用。
//!
//! secret 在库里只存 sha256(见 [`secret_hash`]),令牌只在后台新建时显示一次。

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use sha2::{Digest, Sha256};

const FULL_PREFIX: &str = "caio1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinToken {
    pub node_id: String,
    pub secret: String,
    /// `host:port`;短令牌没有。
    pub master: Option<String>,
    /// hub 证书 SPKI 的 sha256;短令牌没有。
    pub spki: Option<[u8; 32]>,
}

impl JoinToken {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        let parts: Vec<&str> = raw.trim().split('.').collect();
        let (master, spki, node_id, secret) = match parts.as_slice() {
            [id, secret] => (None, None, *id, *secret),
            [FULL_PREFIX, m, p, id, secret] => {
                let master = String::from_utf8(URL_SAFE_NO_PAD.decode(m)?)
                    .map_err(|_| anyhow::anyhow!("令牌里的主机地址不是 UTF-8"))?;
                (Some(master), Some(decode_spki_b64url(p)?), *id, *secret)
            }
            _ => anyhow::bail!("令牌格式不对:应为 <节点>.<密钥> 或 caio1.<主机>.<指纹>.<节点>.<密钥>"),
        };
        if !gw_core::node::valid_node_id(node_id) {
            anyhow::bail!("令牌里的节点 ID 非法");
        }
        if secret.len() < 16 || !secret.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            anyhow::bail!("令牌里的密钥非法");
        }
        if let Some(m) = &master {
            validate_master(m)?;
        }
        Ok(Self { node_id: node_id.to_string(), secret: secret.to_string(), master, spki })
    }

    pub fn short(&self) -> String {
        format!("{}.{}", self.node_id, self.secret)
    }

    /// 完整令牌;缺主机或指纹时没有。
    pub fn full(&self) -> Option<String> {
        let (m, p) = (self.master.as_ref()?, self.spki.as_ref()?);
        Some(format!(
            "{FULL_PREFIX}.{}.{}.{}.{}",
            URL_SAFE_NO_PAD.encode(m.as_bytes()),
            URL_SAFE_NO_PAD.encode(p),
            self.node_id,
            self.secret
        ))
    }
}

/// 新密钥:32 字节随机,base64url。
pub fn new_secret() -> String {
    let mut buf = [0u8; 32];
    buf[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    buf[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(buf)
}

/// 下载票据的密钥段:12 字节随机,base64url 16 字符。
/// 比加入令牌(32 字节)短 —— 它要出现在手敲的一行命令里;96bit 随机 + 24h 过期兜底。
pub fn new_ticket_secret() -> String {
    URL_SAFE_NO_PAD.encode(&uuid::Uuid::new_v4().as_bytes()[..12])
}

/// 新节点 ID:`n-` + 8 位十六进制。
pub fn new_node_id() -> String {
    let u = uuid::Uuid::new_v4();
    let hex: String = u.as_bytes().iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("n-{hex}")
}

/// 库里存的密钥摘要(sha256 十六进制)。
pub fn secret_hash(secret: &str) -> String {
    Sha256::digest(secret.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// SPKI 指纹的展示形态:标准 base64,与 `curl --pinnedpubkey sha256//<值>` 同口径。
pub fn spki_b64(spki: &[u8; 32]) -> String {
    STANDARD.encode(spki)
}

/// 解析指纹:接受标准 base64(含或不含 `sha256//` 前缀)与 base64url。
pub fn parse_spki(raw: &str) -> anyhow::Result<[u8; 32]> {
    let s = raw.trim().trim_start_matches("sha256//");
    let bytes = STANDARD
        .decode(s)
        .or_else(|_| URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')))
        .map_err(|_| anyhow::anyhow!("证书指纹不是 base64"))?;
    bytes.try_into().map_err(|_| anyhow::anyhow!("证书指纹长度不对(应为 sha256)"))
}

fn decode_spki_b64url(s: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = URL_SAFE_NO_PAD.decode(s).map_err(|_| anyhow::anyhow!("令牌里的指纹不是 base64url"))?;
    bytes.try_into().map_err(|_| anyhow::anyhow!("令牌里的指纹长度不对"))
}

/// 主机地址必须是 `host:port`。
pub fn validate_master(m: &str) -> anyhow::Result<()> {
    let (host, port) = m
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("主机地址应为 host:port"))?;
    if host.is_empty() || port.parse::<u16>().map(|p| p == 0).unwrap_or(true) {
        anyhow::bail!("主机地址应为 host:port");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_and_full_roundtrip() {
        let t = JoinToken {
            node_id: new_node_id(),
            secret: new_secret(),
            master: Some("203.0.113.7:38993".into()),
            spki: Some([7u8; 32]),
        };
        let full = t.full().unwrap();
        assert!(full.starts_with("caio1."));
        assert_eq!(JoinToken::parse(&full).unwrap(), t);
        let short = JoinToken::parse(&t.short()).unwrap();
        assert_eq!(short.node_id, t.node_id);
        assert_eq!(short.master, None);
        assert_eq!(short.full(), None);
    }

    #[test]
    fn rejects_bad_tokens() {
        assert!(JoinToken::parse("").is_err());
        assert!(JoinToken::parse("n-1.short").is_err(), "密钥太短");
        assert!(JoinToken::parse("N-1.aaaaaaaaaaaaaaaaaaaa").is_err(), "节点 ID 大写");
        assert!(JoinToken::parse("a.b.c").is_err());
        let bad_master = format!("caio1.{}.{}.n-1.{}", URL_SAFE_NO_PAD.encode("nohost"), URL_SAFE_NO_PAD.encode([1u8; 32]), new_secret());
        assert!(JoinToken::parse(&bad_master).is_err());
    }

    #[test]
    fn spki_formats() {
        let p = [9u8; 32];
        assert_eq!(parse_spki(&spki_b64(&p)).unwrap(), p);
        assert_eq!(parse_spki(&format!("sha256//{}", spki_b64(&p))).unwrap(), p);
        assert_eq!(parse_spki(&URL_SAFE_NO_PAD.encode(p)).unwrap(), p);
        assert!(parse_spki("abc").is_err());
    }

    #[test]
    fn ids_and_hashes() {
        let id = new_node_id();
        assert!(gw_core::node::valid_node_id(&id) && id.len() == 10);
        assert_ne!(new_secret(), new_secret());
        assert_eq!(secret_hash("x").len(), 64);
        assert_ne!(secret_hash("x"), secret_hash("y"));
        let t = new_ticket_secret();
        assert_eq!(t.len(), 16, "票据密钥要能进手敲的一行命令");
        assert!(t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'), "base64url,不含 '.'");
    }
}
