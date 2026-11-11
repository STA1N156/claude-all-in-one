//! hub 的自签证书与子机侧的指纹校验。
//!
//! 不依赖 CA 和域名:hub 首次启动生成一张自签证书并持久化,子机只认令牌(或安装包)里
//! 钉住的 SPKI sha256。证书丢了 = 指纹变了 = 所有子机连不上,所以文件要和库一起备份。

use std::path::Path;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

/// 子机连 hub 时用的 SNI。证书里写的也是它;实际校验只看指纹。
pub const HUB_SERVER_NAME: &str = "caio-hub";
const CERT_FILE: &str = "hub-cert.pem";
const KEY_FILE: &str = "hub-key.pem";

pub struct HubIdentity {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
    pub spki: [u8; 32],
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// 证书 DER → SPKI 的 sha256。
pub fn spki_sha256(cert: &CertificateDer<'_>) -> anyhow::Result<[u8; 32]> {
    let ee = webpki::EndEntityCert::try_from(cert).map_err(|e| anyhow::anyhow!("证书解析失败: {e:?}"))?;
    let spki = ee.subject_public_key_info();
    Ok(Sha256::digest(spki.as_ref()).into())
}

/// 读取或首次生成 hub 证书(目录 0700、私钥 0600)。
pub fn load_or_create(dir: &Path) -> anyhow::Result<HubIdentity> {
    let cert_path = dir.join(CERT_FILE);
    let key_path = dir.join(KEY_FILE);
    if !cert_path.exists() || !key_path.exists() {
        create_private_dir(dir)?;
        let key = rcgen::KeyPair::generate()?;
        let cert = rcgen::CertificateParams::new(vec![HUB_SERVER_NAME.to_string()])?.self_signed(&key)?;
        write_private(&key_path, key.serialize_pem().as_bytes())?;
        write_private(&cert_path, cert.pem().as_bytes())?;
        tracing::info!(dir = %dir.display(), "已生成 hub 自签证书");
    }
    let cert_pem = std::fs::read(&cert_path)?;
    let key_pem = std::fs::read(&key_path)?;
    let cert = pem_block(&cert_pem, "CERTIFICATE")?;
    let cert = CertificateDer::from(cert);
    let key = PrivateKeyDer::try_from(pem_block(&key_pem, "PRIVATE KEY")?)
        .map_err(|e| anyhow::anyhow!("hub 私钥解析失败: {e}"))?;
    let spki = spki_sha256(&cert)?;
    Ok(HubIdentity { cert, key, spki })
}

/// 取 PEM 里第一个指定类型的块(只为读回自己写的文件,不做通用 PEM 解析)。
fn pem_block(pem: &[u8], label: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    let text = std::str::from_utf8(pem)?;
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = text.find(&begin).ok_or_else(|| anyhow::anyhow!("PEM 里没有 {label}"))? + begin.len();
    let stop = text[start..].find(&end).ok_or_else(|| anyhow::anyhow!("PEM {label} 未结束"))? + start;
    let body: String = text[start..stop].chars().filter(|c| !c.is_whitespace()).collect();
    Ok(base64::engine::general_purpose::STANDARD.decode(body)?)
}

fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

pub fn server_config(id: &HubIdentity) -> anyhow::Result<rustls::ServerConfig> {
    let cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![id.cert.clone()], id.key.clone_key())?;
    Ok(cfg)
}

/// 子机侧:只认指纹,不看 CA、不看主机名。
pub fn client_config(pin: [u8; 32]) -> anyhow::Result<rustls::ClientConfig> {
    let p = provider();
    let cfg = rustls::ClientConfig::builder_with_provider(p.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedSpki { pin, provider: p }))
        .with_no_client_auth();
    Ok(cfg)
}

#[derive(Debug)]
struct PinnedSpki {
    pin: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedSpki {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = spki_sha256(end_entity).map_err(|e| rustls::Error::General(e.to_string()))?;
        // 指纹是公开信息,不需要常量时间比较。
        if got == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("hub 证书指纹不符(主机地址错了,或有人在中间)".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_persists_and_spki_is_stable() {
        let dir = std::env::temp_dir().join(format!("gw-node-tls-{}", uuid::Uuid::new_v4()));
        let a = load_or_create(&dir).unwrap();
        let b = load_or_create(&dir).unwrap();
        assert_eq!(a.spki, b.spki, "第二次读回同一张证书");
        assert_eq!(spki_sha256(&a.cert).unwrap(), a.spki);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(KEY_FILE)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(server_config(&a).is_ok());
        assert!(client_config(a.spki).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
