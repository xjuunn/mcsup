//! 机器密钥：本机敏感数据（插件密钥）的对称加密密钥
//!
//! 密钥只存系统钥匙串，不在磁盘上再落一份：把密钥和密文放在同一个目录，等于
//! 拿到目录副本的人同时拿到密钥，加密只剩形式。钥匙串不可用时（headless
//! Linux、没有 Secret Service 的容器）[`MachineKey::load_or_create`] 返回
//! `None`，调用方退化为文件权限并给出明确警告，而不是假装加密生效。
//!
//! 加密用 AES-256-GCM：一条数据一个随机 nonce，调用方传入的 AAD 把密文绑定到
//! 它所属的命名空间（插件密钥用插件 id），密文挪到别的插件目录就解不开。

use crate::credential::{CredentialError, KeyringBackend, SystemKeyring};
use base64::Engine as _;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use thiserror::Error;

/// 钥匙串服务名（与 token 凭证共用）
pub const MACHINE_KEY_SERVICE: &str = "mcsup";

/// 机器密钥在钥匙串里的账户名
pub const MACHINE_KEY_ACCOUNT: &str = "machine-key";

/// 密钥长度（AES-256）
pub const KEY_LEN: usize = 32;

/// GCM nonce 长度
pub const NONCE_LEN: usize = 12;

/// 机器密钥错误
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MachineKeyError {
    /// 钥匙串读写失败
    #[error("machine key store: {0}")]
    Store(#[from] CredentialError),
    /// 密钥长度不是 [`KEY_LEN`]
    #[error("machine key must be {KEY_LEN} bytes, got {0}")]
    InvalidLength(usize),
    /// 密钥内容不是合法 base64
    #[error("machine key is not valid base64")]
    InvalidEncoding,
    /// 随机源失败
    #[error("machine key random source failed")]
    Random,
    /// 密文损坏，或用另一把机器密钥加密
    #[error("sealed data is corrupt or was encrypted with another machine key")]
    Corrupt,
}

/// 对称加密用的机器密钥
///
/// `Debug` 不输出内容，`Drop` 清零：密钥不该出现在日志里，也不该在内存里
/// 多留一刻。
#[derive(Clone)]
pub struct MachineKey {
    bytes: [u8; KEY_LEN],
}

impl std::fmt::Debug for MachineKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MachineKey(***)")
    }
}

impl Drop for MachineKey {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

/// 一段密封数据：随机 nonce 与密文（含 GCM tag）
#[derive(Debug, Clone)]
pub struct Sealed {
    /// 随机 nonce，每次密封都换
    pub nonce: [u8; NONCE_LEN],
    /// 密文 + 认证标签
    pub ciphertext: Vec<u8>,
}

impl MachineKey {
    /// 从原始字节构造
    ///
    /// # Errors
    ///
    /// 长度不是 [`KEY_LEN`] 时返回 [`MachineKeyError::InvalidLength`]
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MachineKeyError> {
        let bytes: [u8; KEY_LEN] = bytes
            .try_into()
            .map_err(|_| MachineKeyError::InvalidLength(bytes.len()))?;
        Ok(Self { bytes })
    }

    /// 生成一把新密钥
    ///
    /// # Errors
    ///
    /// 随机源不可用时返回 [`MachineKeyError::Random`]
    pub fn generate() -> Result<Self, MachineKeyError> {
        let mut bytes = [0_u8; KEY_LEN];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| MachineKeyError::Random)?;
        Ok(Self { bytes })
    }

    /// base64 文本形式（存钥匙串用）
    #[must_use]
    pub fn encode(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.bytes)
    }

    /// 从钥匙串读取；没有则生成并写入
    ///
    /// 返回 `None` 只表示**钥匙串不可用**，调用方应退化为文件权限。读写错误
    /// （权限被拒、钥匙串被锁）同样按不可用处理：拿不到密钥就等于没有密钥，
    /// 这时继续下去只能把明文写到磁盘上。
    #[must_use]
    pub fn load_or_create(service: &str, account: &str) -> Option<Self> {
        if !crate::credential::system_keyring_available() {
            return None;
        }
        Self::load_or_create_with(&SystemKeyring, service, account).ok()
    }

    /// 用指定后端读写（测试用内存实现）
    ///
    /// # Errors
    ///
    /// 钥匙串报错、已有内容不是合法密钥时返回 [`MachineKeyError`]
    pub fn load_or_create_with<B: KeyringBackend>(
        backend: &B,
        service: &str,
        account: &str,
    ) -> Result<Self, MachineKeyError> {
        if let Some(encoded) = backend.get(service, account)? {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map_err(|_| MachineKeyError::InvalidEncoding)?;
            return Self::from_bytes(&raw);
        }
        let key = Self::generate()?;
        backend.set(service, account, &key.encode())?;
        Ok(key)
    }

    /// 加密
    ///
    /// # Errors
    ///
    /// 随机源或 AEAD 初始化失败时返回 [`MachineKeyError`]
    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<Sealed, MachineKeyError> {
        let mut nonce = [0_u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| MachineKeyError::Random)?;
        let key = self.aead_key()?;
        let mut ciphertext = plaintext.to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut ciphertext,
        )
        .map_err(|_| MachineKeyError::Corrupt)?;
        Ok(Sealed { nonce, ciphertext })
    }

    /// 解密
    ///
    /// # Errors
    ///
    /// 密钥不匹配、`aad` 不匹配或密文被改动时返回 [`MachineKeyError::Corrupt`]
    pub fn open(&self, aad: &[u8], sealed: &Sealed) -> Result<Vec<u8>, MachineKeyError> {
        let key = self.aead_key()?;
        let mut plaintext = sealed.ciphertext.clone();
        let opened = key
            .open_in_place(
                Nonce::assume_unique_for_key(sealed.nonce),
                Aad::from(aad),
                &mut plaintext,
            )
            .map_err(|_| MachineKeyError::Corrupt)?;
        Ok(opened.to_vec())
    }

    fn aead_key(&self) -> Result<LessSafeKey, MachineKeyError> {
        UnboundKey::new(&AES_256_GCM, &self.bytes)
            .map(LessSafeKey::new)
            .map_err(|_| MachineKeyError::Corrupt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryKeyring {
        entries: Mutex<BTreeMap<(String, String), String>>,
    }

    impl KeyringBackend for MemoryKeyring {
        fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), CredentialError> {
            self.entries
                .lock()
                .expect("lock")
                .insert((service.to_owned(), account.to_owned()), secret.to_owned());
            Ok(())
        }

        fn get(&self, service: &str, account: &str) -> Result<Option<String>, CredentialError> {
            Ok(self
                .entries
                .lock()
                .expect("lock")
                .get(&(service.to_owned(), account.to_owned()))
                .cloned())
        }

        fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
            self.entries
                .lock()
                .expect("lock")
                .remove(&(service.to_owned(), account.to_owned()));
            Ok(())
        }
    }

    fn key() -> MachineKey {
        MachineKey::from_bytes(&[7_u8; KEY_LEN]).expect("key")
    }

    #[test]
    fn seal_and_open_roundtrip() {
        let key = key();
        let sealed = key.seal(b"demo", b"hunter2").expect("seal");
        assert_ne!(sealed.ciphertext, b"hunter2");
        assert_eq!(key.open(b"demo", &sealed).expect("open"), b"hunter2");
    }

    #[test]
    fn nonce_differs_between_seals() {
        let key = key();
        let first = key.seal(b"demo", b"same").expect("seal");
        let second = key.seal(b"demo", b"same").expect("seal");
        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.ciphertext, second.ciphertext);
    }

    #[test]
    fn aad_mismatch_is_rejected() {
        let key = key();
        let sealed = key.seal(b"demo", b"hunter2").expect("seal");
        assert!(matches!(
            key.open(b"other", &sealed),
            Err(MachineKeyError::Corrupt)
        ));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let key = key();
        let mut sealed = key.seal(b"demo", b"hunter2").expect("seal");
        sealed.ciphertext[0] ^= 0xff;
        assert!(matches!(
            key.open(b"demo", &sealed),
            Err(MachineKeyError::Corrupt)
        ));
    }

    #[test]
    fn another_key_cannot_open() {
        let sealed = key().seal(b"demo", b"hunter2").expect("seal");
        let other = MachineKey::from_bytes(&[9_u8; KEY_LEN]).expect("key");
        assert!(matches!(
            other.open(b"demo", &sealed),
            Err(MachineKeyError::Corrupt)
        ));
    }

    #[test]
    fn key_length_is_enforced() {
        assert!(matches!(
            MachineKey::from_bytes(&[0_u8; 31]),
            Err(MachineKeyError::InvalidLength(31))
        ));
    }

    #[test]
    fn keyring_key_is_created_once_and_reused() {
        let backend = MemoryKeyring::default();
        let first =
            MachineKey::load_or_create_with(&backend, "service", "account").expect("create");
        let second = MachineKey::load_or_create_with(&backend, "service", "account").expect("load");
        assert_eq!(first.encode(), second.encode());

        let sealed = first.seal(b"aad", b"payload").expect("seal");
        assert_eq!(second.open(b"aad", &sealed).expect("open"), b"payload");
    }

    #[test]
    fn corrupt_keyring_value_is_reported() {
        let backend = MemoryKeyring::default();
        backend
            .set("service", "account", "not base64 !!")
            .expect("set");
        assert!(matches!(
            MachineKey::load_or_create_with(&backend, "service", "account"),
            Err(MachineKeyError::InvalidEncoding)
        ));
        backend
            .set(
                "service",
                "account",
                &base64::engine::general_purpose::STANDARD.encode([0_u8; 8]),
            )
            .expect("set");
        assert!(matches!(
            MachineKey::load_or_create_with(&backend, "service", "account"),
            Err(MachineKeyError::InvalidLength(8))
        ));
    }

    /// 真实系统钥匙串上的往返。CI 与无钥匙串环境不跑（`#[ignore]`），本地
    /// 排查「为什么密钥没进钥匙串」时手工执行：
    /// `cargo test -p mcsup-auth --lib -- --ignored machine_key`
    #[test]
    #[ignore = "需要真实系统钥匙串"]
    fn system_keyring_roundtrip() {
        let account = format!("machine-key-test-{}", std::process::id());
        let created = MachineKey::load_or_create(MACHINE_KEY_SERVICE, &account).expect("keyring");
        let sealed = created.seal(b"aad", b"payload").expect("seal");
        let loaded = MachineKey::load_or_create(MACHINE_KEY_SERVICE, &account).expect("keyring");
        assert_eq!(loaded.open(b"aad", &sealed).expect("open"), b"payload");
        SystemKeyring
            .delete(MACHINE_KEY_SERVICE, &account)
            .expect("cleanup");
    }

    #[test]
    fn debug_and_drop_do_not_expose_the_key() {
        let key = key();
        assert_eq!(format!("{key:?}"), "MachineKey(***)");
        assert_eq!(
            key.encode(),
            base64::engine::general_purpose::STANDARD.encode([7_u8; KEY_LEN])
        );
    }
}
