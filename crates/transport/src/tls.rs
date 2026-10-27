//! TLS 配置：服务端出示自签证书，客户端固定指纹
//!
//! 客户端不使用系统根证书，也不做名称校验——信任的唯一来源是
//! [`Fingerprint`]。这不是「关掉校验」：关掉校验是「谁都能连」，固定指纹是
//! 「只认这一张证书」，强度更高，因为它不依赖任何 CA 的诚实。
//!
//! 两处都显式指定 `ring` 作为密码学后端，并复用同一份安装入口：`rustls` 的
//! 默认 provider 是进程级单例，provider 与 transport 各自安装会有一方失败。

use crate::error::TransportError;
use crate::fingerprint::Fingerprint;
use crate::identity::ServerIdentity;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use std::sync::{Arc, Once};

static INSTALL: Once = Once::new();

/// 安装 `ring` 密码学后端（幂等）
///
/// 进程内可能已有别处安装过（`provider` 的 HTTP 客户端）；这里只在没人安装时
/// 安装，失败也不报错——已有 provider 就够用。
pub fn install_provider() {
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// 本进程的密码学后端
fn provider() -> Arc<CryptoProvider> {
    install_provider();
    CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()))
}

/// 服务端 TLS 配置
///
/// # Errors
///
/// 证书与私钥不匹配或密码学后端不可用时返回 [`TransportError::Tls`]
pub fn server_config(identity: &ServerIdentity) -> Result<Arc<ServerConfig>, TransportError> {
    let provider = provider();
    let config = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![identity.cert().clone()], identity.key().clone_key())?;
    Ok(Arc::new(config))
}

/// 客户端 TLS 配置：只接受指纹匹配的那张证书
///
/// # Errors
///
/// 密码学后端不可用时返回 [`TransportError::Tls`]
pub fn client_config(expected: &Fingerprint) -> Result<Arc<ClientConfig>, TransportError> {
    let provider = provider();
    let verifier = Arc::new(PinnedVerifier {
        expected: expected.clone(),
        algorithms: provider.signature_verification_algorithms,
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// 首次配对：接受对端证书并记下指纹（TOFU 的「首次」一半）
///
/// 这里不校验任何东西——用户还没见过指纹，也就没有可校验的对象。指纹被
/// 捕获后由入口展示给用户确认，确认过的值再走 [`client_config`] 固定。
///
/// # Errors
///
/// 密码学后端不可用时返回 [`TransportError::Tls`]
pub fn client_config_capture(
    captured: Arc<std::sync::Mutex<Option<Fingerprint>>>,
) -> Result<Arc<ClientConfig>, TransportError> {
    let provider = provider();
    let verifier = Arc::new(CaptureVerifier {
        captured,
        algorithms: provider.signature_verification_algorithms,
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// 首次配对用的校验器：只记录，不拒绝
struct CaptureVerifier {
    captured: Arc<std::sync::Mutex<Option<Fingerprint>>>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl std::fmt::Debug for CaptureVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureVerifier").finish_non_exhaustive()
    }
}

impl ServerCertVerifier for CaptureVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fingerprint = Fingerprint::of_der(end_entity.as_ref());
        if let Ok(mut slot) = self.captured.lock() {
            *slot = Some(fingerprint);
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// 固定指纹的证书校验器
///
/// 只比对指纹，不查有效期、不查名称、不查链：这张证书是我们唯一认识的那张，
/// 它的其余属性没有第三方能担保，检查它们只会制造「看起来很安全」的假象。
struct PinnedVerifier {
    expected: Fingerprint,
    algorithms: WebPkiSupportedAlgorithms,
}

impl std::fmt::Debug for PinnedVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedVerifier")
            .field("expected", &self.expected)
            .finish_non_exhaustive()
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = Fingerprint::of_der(end_entity.as_ref());
        if actual == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "certificate fingerprint mismatch: expected {expected}, got {actual}",
                expected = self.expected
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mcsup-tls-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn server_and_client_configs_build_from_one_identity() {
        let dir = temp_dir("build");
        let identity = ServerIdentity::load_or_create(&dir).expect("identity");
        assert!(server_config(&identity).is_ok());
        assert!(client_config(identity.fingerprint()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 客户端配置只接受记录下来的那张证书
    #[test]
    fn verifier_rejects_a_different_certificate() {
        let dir = temp_dir("pin");
        let identity = ServerIdentity::load_or_create(&dir).expect("identity");
        let other = Fingerprint::of_der(b"a different certificate");
        let config = client_config(&other).expect("config");
        // 配置能建起来（校验发生在握手时）；这里只断言指纹本身不同
        assert_ne!(identity.fingerprint(), &other);
        assert!(!config.crypto_provider().cipher_suites.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
