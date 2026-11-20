//! 插件产物来源：本地目录 / 压缩包 / URL + 摘要校验
//!
//! 安装流水线的第一步与第二步在这里实现：把来源解析成解包后的目录，做
//! 归档层面的安全检查（Zip Slip、符号链接、大小与数量上限），并计算摘要。
//! manifest / descriptor 的语义校验由 `service` 调用 `domain` 完成——错误
//! 顺序必须是「先解包安全，再语义」，否则坏包可能先触发语义报错掩盖逃逸。
//!
//! # 签名
//!
//! 压缩包来源在解包前校验 minisign 签名：本地文件读 `<file>.minisig`，URL
//! 取 `<url>.minisig`（404 视为未签名，其他失败一律拒绝）。目录来源没有稳定
//! 字节流，视为未签名。签名失败不可覆盖；未签名策略由 `service` 决定。
//! `[禁止]` 在没有验证的情况下把 `signed` 置真。
//!
//! `PluginArtifactVerifier` 只复查摘要与元数据：解包后压缩包已不存在，签名
//! 必须在拉取阶段完成。

use crate::plugin_sig::{MAX_SIGNATURE_BYTES, SignatureFile, TrustedKeys};
use domain::plugin::manifest::Manifest;
use domain::server::artifact::{Checksum, ChecksumAlgorithm};
use port::plugin::{ArtifactOrigin, ArtifactSource, ArtifactVerifier, HostError, PluginArtifact};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

/// 压缩包上限 128 MiB
pub const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;

/// 解包后总上限 512 MiB
pub const MAX_EXTRACTED_BYTES: u64 = 512 * 1024 * 1024;

/// 文件数上限
pub const MAX_ARCHIVE_FILES: usize = 5000;

/// 单文件上限 256 MiB
pub const MAX_ARCHIVE_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// 路径深度上限
pub const MAX_ARCHIVE_DEPTH: usize = 8;

/// 解包目录名（与安装临时目录分开，临时目录里可能还有下载的压缩包）
const UNPACK_DIR: &str = "artifact";

/// 插件产物来源
#[derive(Debug, Clone, Default)]
pub struct PluginArtifactSource {
    /// 信任库目录；未配置时任何签名都会因「无可信公钥」被拒
    trusted_keys: Option<PathBuf>,
}

impl PluginArtifactSource {
    /// 新建
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置 minisign 信任库目录
    #[must_use]
    pub fn with_trusted_keys(mut self, dir: impl Into<PathBuf>) -> Self {
        self.trusted_keys = Some(dir.into());
        self
    }
}

impl ArtifactSource for PluginArtifactSource {
    fn fetch(
        &self,
        origin: &ArtifactOrigin,
        dest_root: &Path,
    ) -> Result<PluginArtifact, HostError> {
        let root = dest_root.join(UNPACK_DIR);
        if root.exists() {
            std::fs::remove_dir_all(&root)
                .map_err(|err| HostError::internal(format!("clear unpack dir: {err}")))?;
        }
        std::fs::create_dir_all(&root)
            .map_err(|err| HostError::internal(format!("create unpack dir: {err}")))?;
        let trusted_keys = self.trusted_keys.as_deref();
        let mut signed = false;
        let sha256 = match origin {
            ArtifactOrigin::Local(path) => {
                if path.is_dir() {
                    copy_tree(path, &root)?;
                    digest_tree(&root)?
                } else {
                    let sha = digest_file(path)?;
                    if let Some(signature) = read_local_signature(path)? {
                        verify_signature(trusted_keys, path, &signature)?;
                        signed = true;
                    }
                    extract_archive(path, &root)?;
                    sha
                }
            }
            ArtifactOrigin::Url(url) => {
                let archive = dest_root.join("package.zip");
                download(url, &archive)?;
                let sha = digest_file(&archive)?;
                if let Some(signature) = download_signature(url)? {
                    verify_signature(trusted_keys, &archive, &signature)?;
                    signed = true;
                }
                extract_archive(&archive, &root)?;
                let _ = std::fs::remove_file(&archive);
                sha
            }
            ArtifactOrigin::Git { .. } => {
                return Err(HostError::unsupported(
                    "git plugin sources are not implemented yet; use --from or --url",
                ));
            }
        };
        let root = unwrap_single_dir(root)?;
        let (manifest, descriptor) = read_metadata(&root)?;
        Ok(PluginArtifact {
            root,
            manifest,
            descriptor,
            sha256,
            signed,
            trusted: signed,
        })
    }
}

/// 摘要与元数据复查
///
/// 签名结论由来源在拉取阶段给出；本类型只重算解包目录摘要，并保留
/// `signed` / `trusted`，避免安装记录丢失签名状态。
#[derive(Debug, Clone, Default)]
pub struct PluginArtifactVerifier;

impl PluginArtifactVerifier {
    /// 新建
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// 校验已安装目录（`signed` 来自安装记录）
    ///
    /// # Errors
    ///
    /// 摘要不匹配返回 `invalid_input`；元数据不可读时返回 `invalid_input`。
    pub fn verify_installed(
        &self,
        root: &Path,
        expected_sha256: Option<&Checksum>,
        signed: bool,
    ) -> Result<PluginArtifact, HostError> {
        let sha256 = digest_tree(root)?;
        check_checksum(expected_sha256, &sha256)?;
        let (manifest, descriptor) = read_metadata(root)?;
        Ok(PluginArtifact {
            root: root.to_path_buf(),
            manifest,
            descriptor,
            sha256,
            signed,
            trusted: signed,
        })
    }
}

impl ArtifactVerifier for PluginArtifactVerifier {
    fn verify(
        &self,
        artifact: &PluginArtifact,
        expected_sha256: Option<&Checksum>,
    ) -> Result<PluginArtifact, HostError> {
        // 期望摘要是来源摘要（压缩包字节或目录树），与安装记录里的树摘要
        // 不同：先比对来源摘要，再重算并返回树摘要。
        check_checksum(expected_sha256, &artifact.sha256)?;
        let mut verified = self.verify_installed(&artifact.root, None, artifact.signed)?;
        verified.trusted = artifact.trusted;
        Ok(verified)
    }
}

fn check_checksum(expected: Option<&Checksum>, actual: &str) -> Result<(), HostError> {
    if let Some(expected) = expected
        && (expected.algorithm() != ChecksumAlgorithm::Sha256 || !expected.matches(actual))
    {
        return Err(HostError::invalid_input(format!(
            "sha256 mismatch: expected {}, got {actual}",
            expected.hex()
        )));
    }
    Ok(())
}

fn read_local_signature(archive: &Path) -> Result<Option<SignatureFile>, HostError> {
    let signature_path = signature_path(archive);
    match std::fs::read(&signature_path) {
        Ok(bytes) => {
            if bytes.len() as u64 > MAX_SIGNATURE_BYTES {
                return Err(HostError::limit_exceeded(format!(
                    "signature {} exceeds {MAX_SIGNATURE_BYTES} bytes",
                    signature_path.display()
                )));
            }
            let text = std::str::from_utf8(&bytes).map_err(|_| {
                HostError::denied(format!(
                    "signature {} is not UTF-8",
                    signature_path.display()
                ))
            })?;
            SignatureFile::parse(text).map(Some)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(HostError::internal(format!(
            "read {}: {err}",
            signature_path.display()
        ))),
    }
}

fn download_signature(url: &str) -> Result<Option<SignatureFile>, HostError> {
    let signature_url = signature_url(url)?;
    let client = http_client()?;
    let response = client
        .get(&signature_url)
        .send()
        .map_err(|err| network(format!("download {signature_url}: {err}")))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(network(format!(
            "download {signature_url}: HTTP {}",
            response.status()
        )));
    }
    if let Some(length) = response.content_length()
        && length > MAX_SIGNATURE_BYTES
    {
        return Err(HostError::limit_exceeded(format!(
            "signature is {length} bytes; limit is {MAX_SIGNATURE_BYTES}"
        )));
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_SIGNATURE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| network(format!("read {signature_url}: {err}")))?;
    if bytes.len() as u64 > MAX_SIGNATURE_BYTES {
        return Err(HostError::limit_exceeded(format!(
            "signature exceeds {MAX_SIGNATURE_BYTES} bytes"
        )));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| HostError::denied(format!("signature {signature_url} is not UTF-8")))?;
    SignatureFile::parse(text).map(Some)
}

fn verify_signature(
    trust_dir: Option<&Path>,
    archive: &Path,
    signature: &SignatureFile,
) -> Result<(), HostError> {
    let trust_dir = trust_dir.ok_or_else(|| {
        HostError::denied(
            "artifact is signed but no trusted key directory is configured".to_owned(),
        )
    })?;
    let keys = TrustedKeys::load(trust_dir)?;
    if keys.is_empty() {
        return Err(HostError::denied(format!(
            "artifact is signed but {} contains no public keys",
            trust_dir.display()
        )));
    }
    let message = std::fs::read(archive).map_err(|err| {
        HostError::internal(format!("read {} for signature: {err}", archive.display()))
    })?;
    keys.verify(&message, signature)
}

fn signature_path(archive: &Path) -> PathBuf {
    let mut name = archive.file_name().unwrap_or_default().to_os_string();
    name.push(".minisig");
    archive.with_file_name(name)
}

fn signature_url(url: &str) -> Result<String, HostError> {
    let mut parsed = url::Url::parse(url)
        .map_err(|err| HostError::invalid_input(format!("invalid artifact URL {url}: {err}")))?;
    let path = format!("{}.minisig", parsed.path());
    parsed.set_path(&path);
    Ok(parsed.to_string())
}

fn http_client() -> Result<reqwest::blocking::Client, HostError> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|err| HostError::internal(format!("build http client: {err}")))
}

fn network(message: impl Into<String>) -> HostError {
    HostError::new(port::plugin::ErrorCode::Unavailable, message)
}

fn read_metadata(root: &Path) -> Result<(Manifest, Vec<u8>), HostError> {
    let manifest_path = root.join("manifest.json");
    let manifest_bytes = std::fs::read(&manifest_path).map_err(|err| {
        HostError::invalid_input(format!("read {}: {err}", manifest_path.display()))
    })?;
    let manifest = Manifest::from_json(&manifest_bytes)
        .map_err(|err| HostError::invalid_input(format!("invalid manifest: {err}")))?
        .raw;
    let descriptor_path = root.join("descriptor.json");
    let descriptor = std::fs::read(&descriptor_path).map_err(|err| {
        HostError::invalid_input(format!("read {}: {err}", descriptor_path.display()))
    })?;
    Ok((manifest, descriptor))
}

/// 允许产物多包一层目录（`chat-bridge-0.3.1/manifest.json`）
fn unwrap_single_dir(root: PathBuf) -> Result<PathBuf, HostError> {
    if root.join("manifest.json").is_file() {
        return Ok(root);
    }
    let mut dirs = Vec::new();
    let entries = std::fs::read_dir(&root)
        .map_err(|err| HostError::internal(format!("list artifact root: {err}")))?;
    for entry in entries {
        let entry = entry.map_err(|err| HostError::internal(format!("list artifact: {err}")))?;
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            dirs.push(entry.path());
        }
    }
    match dirs.as_slice() {
        [single] if single.join("manifest.json").is_file() => Ok(single.clone()),
        _ => Err(HostError::invalid_input(
            "artifact root must contain manifest.json (optionally inside one directory)",
        )),
    }
}

fn download(url: &str, dest: &Path) -> Result<(), HostError> {
    let response = http_client()?
        .get(url)
        .send()
        .map_err(|err| network(format!("download {url}: {err}")))?;
    if !response.status().is_success() {
        return Err(network(format!(
            "download {url}: HTTP {}",
            response.status()
        )));
    }
    if let Some(length) = response.content_length()
        && length > MAX_ARCHIVE_BYTES
    {
        return Err(HostError::limit_exceeded(format!(
            "archive is {length} bytes; limit is {MAX_ARCHIVE_BYTES}"
        )));
    }
    let mut reader = response;
    let mut file = std::fs::File::create(dest)
        .map_err(|err| HostError::internal(format!("create {}: {err}", dest.display())))?;
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|err| network(format!("read response: {err}")))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_ARCHIVE_BYTES {
            return Err(HostError::limit_exceeded(format!(
                "archive exceeds {MAX_ARCHIVE_BYTES} bytes"
            )));
        }
        file.write_all(&buffer[..read])
            .map_err(|err| HostError::internal(format!("write archive: {err}")))?;
    }
    Ok(())
}

fn digest_file(path: &Path) -> Result<String, HostError> {
    let mut file = std::fs::File::open(path)
        .map_err(|err| HostError::internal(format!("open {}: {err}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| HostError::internal(format!("read {}: {err}", path.display())))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// 目录摘要：按相对路径排序后依次摘要路径与内容
///
/// 同一棵树在不同文件系统上的遍历顺序不同，排序保证摘要稳定。
fn digest_tree(root: &Path) -> Result<String, HostError> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for relative in files {
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        let path = root.join(&relative);
        let bytes = std::fs::read(&path)
            .map_err(|err| HostError::internal(format!("read {}: {err}", path.display())))?;
        hasher.update(&bytes);
        hasher.update([0]);
    }
    Ok(to_hex(&hasher.finalize()))
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), HostError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|err| HostError::internal(format!("list {}: {err}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|err| HostError::internal(format!("list entry: {err}")))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| HostError::internal(format!("stat {}: {err}", path.display())))?;
        if file_type.is_symlink() {
            return Err(HostError::denied(format!(
                "symbolic link in artifact: {}",
                path.display()
            )));
        }
        if file_type.is_dir() {
            collect_files(root, &path, out)?;
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| HostError::internal("artifact path escaped root"))?;
            out.push(relative.to_path_buf());
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, dest: &Path) -> Result<(), HostError> {
    let mut files = 0_usize;
    let mut total = 0_u64;
    copy_dir(source, dest, source, &mut files, &mut total, 0)
}

fn copy_dir(
    root: &Path,
    dest: &Path,
    dir: &Path,
    files: &mut usize,
    total: &mut u64,
    depth: usize,
) -> Result<(), HostError> {
    if depth > MAX_ARCHIVE_DEPTH {
        return Err(HostError::limit_exceeded(format!(
            "artifact path depth exceeds {MAX_ARCHIVE_DEPTH}"
        )));
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|err| HostError::internal(format!("list {}: {err}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|err| HostError::internal(format!("list entry: {err}")))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| HostError::internal(format!("stat {}: {err}", path.display())))?;
        if file_type.is_symlink() {
            return Err(HostError::denied(format!(
                "symbolic link in artifact: {}",
                path.display()
            )));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| HostError::internal("artifact path escaped root"))?;
        let target = dest.join(relative);
        if file_type.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|err| HostError::internal(format!("create dir: {err}")))?;
            copy_dir(root, dest, &path, files, total, depth + 1)?;
        } else if file_type.is_file() {
            *files += 1;
            if *files > MAX_ARCHIVE_FILES {
                return Err(HostError::limit_exceeded(format!(
                    "artifact has more than {MAX_ARCHIVE_FILES} files"
                )));
            }
            let size = entry
                .metadata()
                .map_err(|err| HostError::internal(format!("stat file: {err}")))?
                .len();
            *total = total.saturating_add(size);
            if size > MAX_ARCHIVE_FILE_BYTES || *total > MAX_EXTRACTED_BYTES {
                return Err(HostError::limit_exceeded(
                    "artifact exceeds extraction size limits",
                ));
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| HostError::internal(format!("create dir: {err}")))?;
            }
            std::fs::copy(&path, &target)
                .map_err(|err| HostError::internal(format!("copy file: {err}")))?;
        }
    }
    Ok(())
}

fn extract_archive(archive: &Path, dest: &Path) -> Result<(), HostError> {
    let size = std::fs::metadata(archive)
        .map_err(|err| HostError::internal(format!("stat archive: {err}")))?
        .len();
    if size > MAX_ARCHIVE_BYTES {
        return Err(HostError::limit_exceeded(format!(
            "archive is {size} bytes; limit is {MAX_ARCHIVE_BYTES}"
        )));
    }
    let file = std::fs::File::open(archive)
        .map_err(|err| HostError::internal(format!("open archive: {err}")))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|err| HostError::invalid_input(format!("not a zip archive: {err}")))?;
    if zip.len() > MAX_ARCHIVE_FILES {
        return Err(HostError::limit_exceeded(format!(
            "archive has {} entries; limit is {MAX_ARCHIVE_FILES}",
            zip.len()
        )));
    }
    let mut total = 0_u64;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|err| HostError::invalid_input(format!("read zip entry: {err}")))?;
        let name = entry
            .name()
            .map_or_else(|_| "<undecodable>".to_owned(), std::borrow::Cow::into_owned);
        let Some(relative) = entry.enclosed_name() else {
            return Err(HostError::denied(format!(
                "archive entry escapes root: {name}"
            )));
        };
        if is_symlink(&entry) {
            return Err(HostError::denied(format!(
                "archive contains a symbolic link: {name}"
            )));
        }
        if check_depth(&relative) > MAX_ARCHIVE_DEPTH {
            return Err(HostError::limit_exceeded(format!(
                "archive path depth exceeds {MAX_ARCHIVE_DEPTH}"
            )));
        }
        for component in relative.components() {
            if let Component::Normal(name) = component
                && is_reserved_name(&name.to_string_lossy())
            {
                return Err(HostError::invalid_input(format!(
                    "archive contains a reserved name: {}",
                    name.to_string_lossy()
                )));
            }
        }
        let target = dest.join(&relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|err| HostError::internal(format!("create dir: {err}")))?;
            continue;
        }
        if entry.size() > MAX_ARCHIVE_FILE_BYTES {
            return Err(HostError::limit_exceeded(format!(
                "archive entry {name} is too large"
            )));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_EXTRACTED_BYTES {
            return Err(HostError::limit_exceeded(format!(
                "archive exceeds {MAX_EXTRACTED_BYTES} bytes extracted"
            )));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| HostError::internal(format!("create dir: {err}")))?;
        }
        let mut output = std::fs::File::create(&target)
            .map_err(|err| HostError::internal(format!("create {}: {err}", target.display())))?;
        std::io::copy(&mut entry, &mut output)
            .map_err(|err| HostError::internal(format!("extract {name}: {err}")))?;
    }
    Ok(())
}

fn is_symlink<R: Read + ?Sized>(entry: &zip::read::ZipFile<'_, R>) -> bool {
    entry
        .unix_mode()
        .is_some_and(|mode| mode & 0o170_000 == 0o120_000)
}

fn check_depth(path: &Path) -> usize {
    path.components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count()
}

/// Windows 保留名与尾随点 / 空格
fn is_reserved_name(name: &str) -> bool {
    let trimmed = name.trim_end_matches([' ', '.']);
    let upper = trimmed.to_ascii_uppercase();
    let stem = upper.split('.').next().unwrap_or(&upper);
    matches!(stem, "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
        || stem
            .strip_prefix("LPT")
            .is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
        || trimmed.len() != name.len()
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use blake2::Digest as _;

    fn manifest_json(id: &str) -> String {
        format!(
            r#"{{"schema":1,"id":"{id}","name":"Demo","version":"0.1.0","api":1,
               "entry":{{"type":"process","cmd":"bin/demo"}},"capabilities":["events"],
               "license":"MIT"}}"#
        )
    }

    fn descriptor_json(id: &str) -> String {
        format!(r#"{{"schema":1,"plugin":"{id}","ops":[]}}"#)
    }

    #[test]
    fn local_directory_is_copied_and_digested() {
        let src = tempfile::tempdir().expect("src");
        std::fs::write(src.path().join("manifest.json"), manifest_json("demo")).expect("manifest");
        std::fs::write(src.path().join("descriptor.json"), descriptor_json("demo"))
            .expect("descriptor");
        let dest = tempfile::tempdir().expect("dest");
        let artifact = PluginArtifactSource::new()
            .fetch(
                &ArtifactOrigin::Local(src.path().to_path_buf()),
                dest.path(),
            )
            .expect("fetch");
        assert_eq!(artifact.manifest.id, "demo");
        assert!(!artifact.signed);
        assert!(artifact.root.join("manifest.json").is_file());
        // 源目录仍在（复制而不是移动）
        assert!(src.path().join("manifest.json").is_file());
        assert_eq!(artifact.sha256.len(), 64);
    }

    #[test]
    fn zip_slip_is_rejected() {
        let dir = tempfile::tempdir().expect("tmp");
        let archive = dir.path().join("bad.zip");
        {
            use std::io::Write as _;
            let file = std::fs::File::create(&archive).expect("create");
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("../escape.txt", zip::write::SimpleFileOptions::default())
                .expect("entry");
            zip.write_all(b"x").expect("write");
            zip.finish().expect("finish");
        }
        let dest = tempfile::tempdir().expect("dest");
        let err = PluginArtifactSource::new()
            .fetch(&ArtifactOrigin::Local(archive), dest.path())
            .expect_err("zip slip");
        assert_eq!(err.code, port::plugin::ErrorCode::Denied);
    }

    #[test]
    fn verifier_preserves_signature_state() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("manifest.json"), manifest_json("demo")).expect("manifest");
        std::fs::write(dir.path().join("descriptor.json"), descriptor_json("demo"))
            .expect("descriptor");
        let verifier = PluginArtifactVerifier;
        let unsigned = verifier
            .verify_installed(dir.path(), None, false)
            .expect("verify");
        assert!(!unsigned.signed);
        assert!(!unsigned.trusted);
        let trusted = verifier
            .verify_installed(dir.path(), None, true)
            .expect("verify");
        assert!(trusted.signed);
        assert!(trusted.trusted);
    }

    #[test]
    fn reserved_names_are_rejected() {
        assert!(is_reserved_name("CON"));
        assert!(is_reserved_name("nul.txt"));
        assert!(is_reserved_name("name. "));
        assert!(!is_reserved_name("console.txt"));
    }

    fn test_artifact_zip(path: &Path) {
        use std::io::Write as _;

        let file = std::fs::File::create(path).expect("create zip");
        let mut zip = zip::ZipWriter::new(file);
        for (name, content) in [
            ("manifest.json", manifest_json("demo")),
            ("descriptor.json", descriptor_json("demo")),
        ] {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .expect("entry");
            zip.write_all(content.as_bytes()).expect("write entry");
        }
        zip.finish().expect("finish");
    }

    const TEST_KEYNUM: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    fn test_signing_key() -> ed25519_dalek::SigningKey {
        use ed25519_dalek::SigningKey;

        SigningKey::from_bytes(&[42_u8; 32])
    }

    fn write_trust_store(dir: &Path, key: &ed25519_dalek::SigningKey) {
        use base64::Engine as _;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .expect("chmod trust dir");
        }
        let mut public = Vec::new();
        public.extend_from_slice(b"Ed");
        public.extend_from_slice(&TEST_KEYNUM);
        public.extend_from_slice(key.verifying_key().as_bytes());
        let key_path = dir.join("demo.pub");
        std::fs::write(
            &key_path,
            format!(
                "untrusted comment: test key\n{}\n",
                base64::engine::general_purpose::STANDARD.encode(public)
            ),
        )
        .expect("write key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod key");
        }
    }

    fn write_signature(archive: &Path, key: &ed25519_dalek::SigningKey) {
        use base64::Engine as _;
        use ed25519_dalek::Signer as _;

        let message = std::fs::read(archive).expect("read archive");
        let mut hasher = blake2::Blake2b512::new();
        hasher.update(&message);
        let digest = hasher.finalize();
        let signature = key.sign(&digest).to_bytes();
        let trusted = "timestamp:1700000000\tfile:demo.zip";
        let mut package = Vec::new();
        package.extend_from_slice(b"ED");
        package.extend_from_slice(&TEST_KEYNUM);
        package.extend_from_slice(&signature);
        let mut global_message = signature.to_vec();
        global_message.extend_from_slice(trusted.as_bytes());
        let global = key.sign(&global_message).to_bytes();
        let mut name = archive.file_name().expect("name").to_os_string();
        name.push(".minisig");
        std::fs::write(
            archive.with_file_name(name),
            format!(
                "untrusted comment: test signature\n{}\ntrusted comment: {trusted}\n{}\n",
                base64::engine::general_purpose::STANDARD.encode(package),
                base64::engine::general_purpose::STANDARD.encode(global)
            ),
        )
        .expect("write signature");
    }

    #[test]
    fn signed_local_archive_is_trusted() {
        let dir = tempfile::tempdir().expect("tmp");
        let archive = dir.path().join("demo.zip");
        test_artifact_zip(&archive);
        let key = test_signing_key();
        write_signature(&archive, &key);
        let trust = dir.path().join("trusted-keys");
        std::fs::create_dir(&trust).expect("trust dir");
        write_trust_store(&trust, &key);

        let dest = tempfile::tempdir().expect("dest");
        let artifact = PluginArtifactSource::new()
            .with_trusted_keys(&trust)
            .fetch(&ArtifactOrigin::Local(archive), dest.path())
            .expect("fetch");
        assert!(artifact.signed);
        assert!(artifact.trusted);
        assert_eq!(artifact.manifest.id, "demo");
    }

    #[test]
    fn tampered_signed_archive_is_rejected() {
        let dir = tempfile::tempdir().expect("tmp");
        let archive = dir.path().join("demo.zip");
        test_artifact_zip(&archive);
        let key = test_signing_key();
        write_signature(&archive, &key);
        let trust = dir.path().join("trusted-keys");
        std::fs::create_dir(&trust).expect("trust dir");
        write_trust_store(&trust, &key);

        // 追加一个字节：签名包不变，内容已变
        let mut bytes = std::fs::read(&archive).expect("read");
        bytes.push(0);
        std::fs::write(&archive, bytes).expect("tamper");

        let dest = tempfile::tempdir().expect("dest");
        let err = PluginArtifactSource::new()
            .with_trusted_keys(&trust)
            .fetch(&ArtifactOrigin::Local(archive), dest.path())
            .expect_err("tampered");
        assert_eq!(err.code, port::plugin::ErrorCode::Denied);
    }

    #[test]
    fn signed_archive_without_trust_store_is_rejected() {
        let dir = tempfile::tempdir().expect("tmp");
        let archive = dir.path().join("demo.zip");
        test_artifact_zip(&archive);
        write_signature(&archive, &test_signing_key());

        let dest = tempfile::tempdir().expect("dest");
        let err = PluginArtifactSource::new()
            .fetch(&ArtifactOrigin::Local(archive), dest.path())
            .expect_err("no trust store");
        assert_eq!(err.code, port::plugin::ErrorCode::Denied);
    }

    #[test]
    fn signature_url_appends_next_to_path_not_query() {
        assert_eq!(
            signature_url("https://example.test/plugins/demo.zip?token=1").expect("url"),
            "https://example.test/plugins/demo.zip.minisig?token=1"
        );
    }

    #[test]
    fn signature_path_appends_suffix() {
        assert_eq!(
            signature_path(Path::new("/tmp/demo.zip")),
            PathBuf::from("/tmp/demo.zip.minisig")
        );
    }
}
