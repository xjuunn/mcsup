//! minisign / ed25519 签名校验与信任库
//!
//! 只针对压缩包产物：签名文件与被签文件同名并追加 `.minisig`。目录来源没有
//! 稳定字节流，视为未签名。公钥放在 `plugins/trusted-keys/*.pub`，加载时按
//! 最小权限检查，避免同机其他用户替换信任根。
//!
//! 支持两种签名算法：
//!
//! - `ED`：签名对象是文件的 BLAKE2b-512 摘要（当前 minisign 默认）
//! - `Ed`：签名对象是文件原始字节（旧格式，仍可校验）
//!
//! 公钥头固定为 `Ed`，不能要求与签名头相等。签名文件第四行是对
//! 「签名包 + trusted comment 文本」的全局签名，同样必须校验：只验文件签名
//! 会让攻击者随意改写 trusted comment。

use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use blake2::{Blake2b512, Digest as _};
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use port::plugin::HostError;
use std::io::ErrorKind;
use std::path::Path;

/// 信任库目录名（位于 `plugins/` 下）
pub const TRUSTED_KEYS_DIR: &str = "trusted-keys";

/// 公钥二进制长度：`"Ed"` + 8 字节 key id + 32 字节公钥
const PUBLIC_KEY_BYTES: usize = 42;

/// 签名包长度：算法 2 字节 + 8 字节 key id + 64 字节签名
const SIGNATURE_BYTES: usize = 74;

/// 全局签名长度（ed25519）
const GLOBAL_SIGNATURE_BYTES: usize = 64;

/// 单个公钥文件上限
const MAX_KEY_FILE_BYTES: u64 = 16 * 1024;

/// 签名文件上限：四行文本，远小于此值
pub const MAX_SIGNATURE_BYTES: u64 = 64 * 1024;

/// 公钥 / 签名都允许无填充，容忍不同工具的编码差异
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// 一个可信公钥
#[derive(Debug, Clone)]
struct PublicKey {
    keynum: [u8; 8],
    key: VerifyingKey,
}

/// 信任库
///
/// 目录不存在时为空库：没有签名就不需要公钥；一旦产物带签名，空库会导致
/// 拒绝而不是放行。
#[derive(Debug, Clone, Default)]
pub struct TrustedKeys {
    keys: Vec<PublicKey>,
}

impl TrustedKeys {
    /// 从目录加载全部 `*.pub`
    ///
    /// # Errors
    ///
    /// 目录或文件权限过宽、文件不是普通文件、公钥格式非法时返回 `denied`；
    /// 读取失败返回 `internal`。
    pub fn load(dir: &Path) -> Result<Self, HostError> {
        let metadata = match std::fs::metadata(dir) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(HostError::internal(format!(
                    "read trust store {}: {err}",
                    dir.display()
                )));
            }
        };
        if !metadata.is_dir() {
            return Err(HostError::denied(format!(
                "trust store {} is not a directory",
                dir.display()
            )));
        }
        check_permissions(dir, &metadata, true)?;

        let entries = std::fs::read_dir(dir).map_err(|err| {
            HostError::internal(format!("list trust store {}: {err}", dir.display()))
        })?;
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|err| HostError::internal(format!("list trust store entry: {err}")))?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "pub") {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|err| HostError::internal(format!("stat {}: {err}", path.display())))?;
            if file_type.is_symlink() || !file_type.is_file() {
                return Err(HostError::denied(format!(
                    "trust store entry {} must be a regular file",
                    path.display()
                )));
            }
            paths.push(path);
        }
        paths.sort();

        let mut keys = Vec::with_capacity(paths.len());
        for path in paths {
            keys.push(load_public_key(&path)?);
        }
        Ok(Self { keys })
    }

    /// 信任库是否为空
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// 校验文件签名与 trusted comment 全局签名
    ///
    /// # Errors
    ///
    /// key id 不在信任库、签名不匹配、trusted comment 被改写时返回 `denied`。
    pub fn verify(&self, message: &[u8], signature: &SignatureFile) -> Result<(), HostError> {
        let key = self
            .keys
            .iter()
            .find(|key| key.keynum == signature.keynum)
            .ok_or_else(|| {
                HostError::denied(format!(
                    "signature key id {:016x} is not in the trust store",
                    u64::from_le_bytes(signature.keynum)
                ))
            })?;

        let payload = if signature.prehashed {
            blake2b_512(message)
        } else {
            message.to_vec()
        };
        let file_signature = Signature::from_bytes(&signature.signature);
        key.key
            .verify(&payload, &file_signature)
            .map_err(|_| HostError::denied("minisign artifact signature does not match"))?;

        let mut global_message =
            Vec::with_capacity(SIGNATURE_BYTES + signature.trusted_comment.len());
        global_message.extend_from_slice(&signature.signature);
        global_message.extend_from_slice(signature.trusted_comment.as_bytes());
        let global_signature = Signature::from_bytes(&signature.global_signature);
        key.key
            .verify(&global_message, &global_signature)
            .map_err(|_| HostError::denied("minisign trusted comment signature does not match"))?;
        Ok(())
    }
}

/// 解析后的签名文件
#[derive(Debug, Clone)]
pub struct SignatureFile {
    untrusted_comment: String,
    keynum: [u8; 8],
    prehashed: bool,
    signature: [u8; 64],
    trusted_comment: String,
    global_signature: [u8; 64],
}

impl SignatureFile {
    /// 解析签名文件文本
    ///
    /// # Errors
    ///
    /// 行数不足、前缀不符、base64 非法或长度不对时返回 `denied`。
    pub fn parse(text: &str) -> Result<Self, HostError> {
        parse_signature_file(text).map_err(HostError::denied)
    }

    /// 未签名注释（只用于展示，不参与任何判定）
    #[must_use]
    pub fn untrusted_comment(&self) -> &str {
        &self.untrusted_comment
    }

    /// 签名头声明的 key id
    #[must_use]
    pub fn key_id(&self) -> u64 {
        u64::from_le_bytes(self.keynum)
    }

    /// 是否对内容摘要签名
    #[must_use]
    pub fn is_prehashed(&self) -> bool {
        self.prehashed
    }

    /// trusted comment 文本
    #[must_use]
    pub fn trusted_comment(&self) -> &str {
        &self.trusted_comment
    }
}

fn parse_signature_file(text: &str) -> Result<SignatureFile, String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 4 {
        return Err("signature file must contain four lines".to_owned());
    }
    let untrusted_comment = lines[0]
        .strip_prefix("untrusted comment: ")
        .ok_or_else(|| "signature file has no untrusted comment".to_owned())?
        .trim_end()
        .to_owned();
    let signature = decode(lines[1].trim())
        .map_err(|err| format!("signature line is not valid base64: {err}"))?;
    if signature.len() != SIGNATURE_BYTES {
        return Err(format!(
            "signature must be {SIGNATURE_BYTES} bytes, got {}",
            signature.len()
        ));
    }
    let trusted_comment = lines[2]
        .strip_prefix("trusted comment: ")
        .ok_or_else(|| "signature file has no trusted comment".to_owned())?
        .trim_end()
        .to_owned();
    let global_signature = decode(lines[3].trim())
        .map_err(|err| format!("global signature line is not valid base64: {err}"))?;
    if global_signature.len() != GLOBAL_SIGNATURE_BYTES {
        return Err(format!(
            "global signature must be {GLOBAL_SIGNATURE_BYTES} bytes, got {}",
            global_signature.len()
        ));
    }

    let prehashed = match &signature[..2] {
        b"ED" => true,
        b"Ed" => false,
        other => {
            return Err(format!(
                "unsupported signature algorithm {:?}",
                String::from_utf8_lossy(other)
            ));
        }
    };
    let keynum = signature
        .get(2..10)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| "signature has no key id".to_owned())?;
    let signature_bytes = signature
        .get(10..74)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| "signature body is truncated".to_owned())?;
    let global_bytes = global_signature
        .get(..64)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| "global signature is truncated".to_owned())?;
    Ok(SignatureFile {
        untrusted_comment,
        keynum,
        prehashed,
        signature: signature_bytes,
        trusted_comment,
        global_signature: global_bytes,
    })
}

fn load_public_key(path: &Path) -> Result<PublicKey, HostError> {
    let metadata = std::fs::metadata(path)
        .map_err(|err| HostError::internal(format!("stat {}: {err}", path.display())))?;
    if metadata.len() > MAX_KEY_FILE_BYTES {
        return Err(HostError::denied(format!(
            "public key {} exceeds {MAX_KEY_FILE_BYTES} bytes",
            path.display()
        )));
    }
    check_permissions(path, &metadata, false)?;
    let text = std::fs::read_to_string(path)
        .map_err(|err| HostError::internal(format!("read {}: {err}", path.display())))?;
    parse_public_key_file(&text)
        .map_err(|err| HostError::denied(format!("invalid public key {}: {err}", path.display())))
}

fn parse_public_key_file(text: &str) -> Result<PublicKey, String> {
    let mut lines = text.lines();
    let _comment = lines
        .next()
        .ok_or_else(|| "public key file is empty".to_owned())?;
    let encoded = lines
        .next()
        .ok_or_else(|| "public key file has no key line".to_owned())?;
    let bytes =
        decode(encoded.trim()).map_err(|err| format!("key line is not valid base64: {err}"))?;
    if bytes.len() != PUBLIC_KEY_BYTES {
        return Err(format!(
            "public key must be {PUBLIC_KEY_BYTES} bytes, got {}",
            bytes.len()
        ));
    }
    if &bytes[..2] != b"Ed" {
        return Err("public key algorithm must be Ed".to_owned());
    }
    let keynum = bytes
        .get(2..10)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| "public key has no key id".to_owned())?;
    let key_bytes = bytes
        .get(10..42)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| "public key body is truncated".to_owned())?;
    let key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|err| format!("public key is not a valid ed25519 point: {err}"))?;
    Ok(PublicKey { keynum, key })
}

/// minisign 预签名的消息：文件内容的 BLAKE2b-512 摘要
fn blake2b_512(message: &[u8]) -> Vec<u8> {
    let mut hasher = Blake2b512::new();
    hasher.update(message);
    hasher.finalize().to_vec()
}

fn decode(raw: &str) -> Result<Vec<u8>, String> {
    BASE64.decode(raw).map_err(|err| format!("base64: {err}"))
}

#[cfg(unix)]
fn check_permissions(
    path: &Path,
    metadata: &std::fs::Metadata,
    is_dir: bool,
) -> Result<(), HostError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    let too_open = if is_dir {
        mode & 0o077 != 0
    } else {
        mode & 0o177 != 0
    };
    if too_open {
        return Err(HostError::denied(format!(
            "trust store {} has permissions {mode:04o}; expected {}",
            path.display(),
            if is_dir { "0700" } else { "0600" }
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(
    _path: &Path,
    _metadata: &std::fs::Metadata,
    _is_dir: bool,
) -> Result<(), HostError> {
    Ok(())
}

/// 确保信任库目录存在；新建时收紧到 `0700`
///
/// 已存在的目录不静默改权限：权限过宽由 [`TrustedKeys::load`] 在实际校验时
/// 拒绝，避免启动阶段因为一个未使用的信任库而失败。
///
/// # Errors
///
/// 目录创建或设置权限失败时返回底层错误。
pub fn ensure_trusted_keys_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// 公钥文件路径是否位于信任库目录内（诊断用）
#[must_use]
pub fn is_trusted_key_path(path: &Path, trust_dir: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(trust_dir) else {
        return false;
    };
    relative.components().count() == 1 && relative.extension().is_some_and(|ext| ext == "pub")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};

    const KEYNUM: [u8; 8] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];

    fn chmod_dir(path: &Path, mode: u32) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .expect("chmod dir");
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
        }
    }

    fn chmod_file(path: &Path, mode: u32) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .expect("chmod file");
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
        }
    }

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn public_key_text(key: &SigningKey) -> String {
        let mut bytes = Vec::with_capacity(PUBLIC_KEY_BYTES);
        bytes.extend_from_slice(b"Ed");
        bytes.extend_from_slice(&KEYNUM);
        bytes.extend_from_slice(key.verifying_key().as_bytes());
        format!("untrusted comment: test key\n{}\n", BASE64.encode(bytes))
    }

    fn signature_text(message: &[u8], prehashed: bool, trusted: &str) -> String {
        let key = signing_key(7);
        let payload = if prehashed {
            blake2b_512(message)
        } else {
            message.to_vec()
        };
        let signature = key.sign(&payload).to_bytes();
        let mut package = Vec::with_capacity(SIGNATURE_BYTES);
        package.extend_from_slice(if prehashed { b"ED" } else { b"Ed" });
        package.extend_from_slice(&KEYNUM);
        package.extend_from_slice(&signature);
        let mut global_message = signature.to_vec();
        global_message.extend_from_slice(trusted.as_bytes());
        let global = key.sign(&global_message).to_bytes();
        format!(
            "untrusted comment: test signature\n{}\ntrusted comment: {trusted}\n{}\n",
            BASE64.encode(&package),
            BASE64.encode(global)
        )
    }

    fn trust_store() -> (tempfile::TempDir, TrustedKeys) {
        let dir = tempfile::tempdir().expect("tempdir");
        chmod_dir(dir.path(), 0o700);
        ensure_trusted_keys_dir(dir.path()).expect("trust dir");
        let path = dir.path().join("test.pub");
        std::fs::write(&path, public_key_text(&signing_key(7))).expect("write key");
        chmod_file(&path, 0o600);
        let keys = TrustedKeys::load(dir.path()).expect("load keys");
        (dir, keys)
    }

    #[test]
    fn prehashed_signature_verifies() {
        let (_dir, keys) = trust_store();
        let message = b"artifact bytes";
        let signature =
            SignatureFile::parse(&signature_text(message, true, "timestamp:1")).expect("parse");
        keys.verify(message, &signature).expect("verify");
        assert!(signature.is_prehashed());
    }

    #[test]
    fn legacy_raw_signature_verifies() {
        let (_dir, keys) = trust_store();
        let message = b"artifact bytes";
        let signature =
            SignatureFile::parse(&signature_text(message, false, "timestamp:1")).expect("parse");
        keys.verify(message, &signature).expect("verify");
        assert!(!signature.is_prehashed());
    }

    #[test]
    fn tampered_content_is_rejected() {
        let (_dir, keys) = trust_store();
        let signature =
            SignatureFile::parse(&signature_text(b"artifact bytes", true, "timestamp:1"))
                .expect("parse");
        let err = keys
            .verify(b"tampered bytes", &signature)
            .expect_err("tampered");
        assert_eq!(err.code, port::plugin::ErrorCode::Denied);
    }

    #[test]
    fn tampered_trusted_comment_is_rejected() {
        let (_dir, keys) = trust_store();
        let text = signature_text(b"artifact bytes", true, "timestamp:1");
        let tampered = text.replace("timestamp:1", "timestamp:2");
        let signature = SignatureFile::parse(&tampered).expect("parse");
        let err = keys
            .verify(b"artifact bytes", &signature)
            .expect_err("tampered");
        assert_eq!(err.code, port::plugin::ErrorCode::Denied);
    }

    #[test]
    fn unknown_key_id_is_rejected() {
        let (_dir, keys) = trust_store();
        let message = b"artifact bytes";
        let text = signature_text(message, true, "timestamp:1");
        // 把签名包里的 key id 改掉：其余字节保持不变，必须因「未知 key」被拒
        let lines: Vec<&str> = text.lines().collect();
        let mut package = decode(lines[1]).expect("decode");
        package[2..10].copy_from_slice(&[0xaa; 8]);
        let text = format!(
            "{}\n{}\n{}\n{}\n",
            lines[0],
            BASE64.encode(package),
            lines[2],
            lines[3]
        );
        let signature = SignatureFile::parse(&text).expect("parse");
        let err = keys.verify(message, &signature).expect_err("unknown key");
        assert_eq!(err.code, port::plugin::ErrorCode::Denied);
    }

    #[test]
    fn malformed_signature_is_rejected() {
        assert!(SignatureFile::parse("only one line").is_err());
        assert!(SignatureFile::parse("a\nb\nc\nd").is_err());
        assert!(
            SignatureFile::parse(
                "untrusted comment: x\nnot base64\ntrusted comment: y\nnot base64"
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn loose_key_permissions_are_rejected() {
        let (dir, _keys) = trust_store();
        chmod_file(&dir.path().join("test.pub"), 0o644);
        let err = TrustedKeys::load(dir.path()).expect_err("loose permissions");
        assert_eq!(err.code, port::plugin::ErrorCode::Denied);
    }

    #[cfg(unix)]
    #[test]
    fn loose_directory_permissions_are_rejected() {
        let (dir, _keys) = trust_store();
        chmod_dir(dir.path(), 0o755);
        let err = TrustedKeys::load(dir.path()).expect_err("loose permissions");
        assert_eq!(err.code, port::plugin::ErrorCode::Denied);
    }

    #[test]
    fn missing_trust_store_is_empty() {
        let keys = TrustedKeys::load(Path::new("/nonexistent/mcsup/trusted-keys"))
            .expect("missing directory");
        assert!(keys.is_empty());
    }

    #[test]
    fn key_path_filter_only_accepts_direct_pub_files() {
        let dir = Path::new("/tmp/trusted-keys");
        assert!(is_trusted_key_path(&dir.join("a.pub"), dir));
        assert!(!is_trusted_key_path(&dir.join("a.txt"), dir));
        assert!(!is_trusted_key_path(&dir.join("nested/a.pub"), dir));
        assert!(!is_trusted_key_path(Path::new("/tmp/other/a.pub"), dir));
    }

    #[test]
    fn public_key_parser_rejects_wrong_algorithm() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ED");
        bytes.extend_from_slice(&KEYNUM);
        bytes.extend_from_slice(signing_key(7).verifying_key().as_bytes());
        let text = format!("untrusted comment: x\n{}\n", BASE64.encode(bytes));
        assert!(parse_public_key_file(&text).is_err());
    }
}
