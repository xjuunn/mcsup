//! 被连接端的身份：自签证书与私钥
//!
//! 首次 `remote serve` 时生成一次，之后一直复用：证书换了就等于换了身份，
//! 已配对的客户端会因指纹不符而拒绝连接（这正是我们要的——否则任何人都能
//! 冒充这台机器）。
//!
//! 落盘两个文件，都在 `{data_dir}/remote/identity/`：
//!
//! - `cert.pem`：自签证书，可以公开
//! - `key.pem`：私钥，Unix 上 `0600`
//!
//! 用 PEM 而不是 DER：用户可以 `openssl x509 -text -in cert.pem` 自查，
//! 「这是什么证书」不该需要本项目才能回答。

use crate::error::TransportError;
use crate::fingerprint::Fingerprint;
use crate::tls;
use rcgen::generate_simple_self_signed;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// 证书文件名
pub const CERT_FILE: &str = "cert.pem";
/// 私钥文件名
pub const KEY_FILE: &str = "key.pem";

/// 证书里的 DNS 名
///
/// 客户端按指纹而不是按名字校验，因此这个名字只影响「证书看起来是否正常」。
/// 用固定名而不是本机主机名：主机名会随 DHCP / 改名变化，而证书不该跟着变。
const SUBJECT_ALT_NAME: &str = "mcsup.local";

/// 本机远端身份
pub struct ServerIdentity {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    fingerprint: Fingerprint,
}

impl std::fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 私钥不得进日志
        f.debug_struct("ServerIdentity")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl ServerIdentity {
    /// 读取已有身份；不存在时生成并落盘
    ///
    /// # Errors
    ///
    /// 目录创建、生成或读写失败时返回 [`TransportError`]
    pub fn load_or_create(dir: &Path) -> Result<Self, TransportError> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        match (cert_path.exists(), key_path.exists()) {
            (true, true) => Self::load(dir),
            // 只有一半：宁可报错也不重新生成。重新生成会让所有已配对客户端
            // 静默失效，而用户看到的只是「突然连不上」
            (true, false) => Err(TransportError::IdentityCorrupt(format!(
                "{} 存在但 {} 缺失，无法恢复身份",
                cert_path.display(),
                key_path.display()
            ))),
            (false, true) => Err(TransportError::IdentityCorrupt(format!(
                "{} 存在但 {} 缺失，无法恢复身份",
                key_path.display(),
                cert_path.display()
            ))),
            (false, false) => Self::generate(dir),
        }
    }

    /// 读取目录下已有的身份
    ///
    /// # Errors
    ///
    /// 文件缺失、内容非法或读失败时返回 [`TransportError`]
    pub fn load(dir: &Path) -> Result<Self, TransportError> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        let cert_text = std::fs::read(&cert_path).map_err(|source| TransportError::Identity {
            path: cert_path.clone(),
            source,
        })?;
        let key_text = std::fs::read(&key_path).map_err(|source| TransportError::Identity {
            path: key_path.clone(),
            source,
        })?;

        let cert_pem = pem::parse(&cert_text).map_err(|err| {
            TransportError::IdentityCorrupt(format!("{}: {err}", cert_path.display()))
        })?;
        let key_pem = pem::parse(&key_text).map_err(|err| {
            TransportError::IdentityCorrupt(format!("{}: {err}", key_path.display()))
        })?;

        let cert = CertificateDer::from(cert_pem.into_contents());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pem.into_contents()));
        let fingerprint = Fingerprint::of_der(cert.as_ref());
        Ok(Self {
            cert,
            key,
            fingerprint,
        })
    }

    /// 生成新身份并落盘
    ///
    /// # Errors
    ///
    /// 生成失败或写文件失败时返回 [`TransportError`]
    pub fn generate(dir: &Path) -> Result<Self, TransportError> {
        tls::install_provider();
        let generated = generate_simple_self_signed([SUBJECT_ALT_NAME.to_owned()])
            .map_err(|err| TransportError::IdentityCorrupt(format!("generate cert: {err}")))?;
        let cert_der = generated.cert.der().clone();
        let key_der = generated.signing_key.serialize_der();
        let key_pem = generated.signing_key.serialize_pem();

        std::fs::create_dir_all(dir).map_err(|source| TransportError::Identity {
            path: dir.to_path_buf(),
            source,
        })?;
        write_private(&dir.join(KEY_FILE), key_pem.as_bytes())?;
        // 证书不是秘密，但也没有理由让全世界可写
        std::fs::write(
            dir.join(CERT_FILE),
            pem::encode(&pem::Pem::new("CERTIFICATE", cert_der.as_ref().to_vec())),
        )
        .map_err(|source| TransportError::Identity {
            path: dir.join(CERT_FILE),
            source,
        })?;

        let fingerprint = Fingerprint::of_der(cert_der.as_ref());
        Ok(Self {
            cert: cert_der,
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
            fingerprint,
        })
    }

    /// 证书指纹
    #[must_use]
    pub fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }

    /// 证书（DER）
    #[must_use]
    pub fn cert(&self) -> &CertificateDer<'static> {
        &self.cert
    }

    /// 私钥（DER）
    #[must_use]
    pub fn key(&self) -> &PrivateKeyDer<'static> {
        &self.key
    }
}

/// 写私钥：先创建再限权，且创建时就带 `0600`
fn write_private(path: &PathBuf, bytes: &[u8]) -> Result<(), TransportError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|source| TransportError::Identity {
            path: path.clone(),
            source,
        })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| TransportError::Identity {
            path: path.clone(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mcsup-identity-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn generates_then_reloads_the_same_identity() {
        let dir = temp_dir("reload");
        let created = ServerIdentity::load_or_create(&dir).expect("generate");
        assert!(dir.join(CERT_FILE).exists());
        assert!(dir.join(KEY_FILE).exists());

        let reloaded = ServerIdentity::load_or_create(&dir).expect("load");
        assert_eq!(created.fingerprint(), reloaded.fingerprint());
        assert_eq!(created.cert(), reloaded.cert());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 半套身份必须报错：静默重新生成会让所有已配对客户端失效
    #[test]
    fn half_identity_is_rejected() {
        let dir = temp_dir("half");
        ServerIdentity::load_or_create(&dir).expect("generate");
        std::fs::remove_file(dir.join(KEY_FILE)).expect("rm key");
        let err = ServerIdentity::load_or_create(&dir).expect_err("missing key");
        assert!(matches!(err, TransportError::IdentityCorrupt(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_cert_is_rejected() {
        let dir = temp_dir("corrupt");
        ServerIdentity::load_or_create(&dir).expect("generate");
        std::fs::write(dir.join(CERT_FILE), b"not pem").expect("write");
        let err = ServerIdentity::load(&dir).expect_err("corrupt");
        assert!(matches!(err, TransportError::IdentityCorrupt(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 指纹必须由证书本身算出来，不能是随机串
    #[test]
    fn fingerprint_matches_certificate() {
        let dir = temp_dir("fp");
        let identity = ServerIdentity::load_or_create(&dir).expect("generate");
        assert!(identity.fingerprint().matches_der(identity.cert().as_ref()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
