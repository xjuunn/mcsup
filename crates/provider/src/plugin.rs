//! 插件产物来源：本地目录 / 压缩包 / URL / GitHub 仓库归档 + 摘要校验
//!
//! 安装流水线的第一步与第二步在这里实现：把来源解析成解包后的目录，做
//! 归档层面的安全检查（Zip Slip、符号链接、大小与数量上限），并计算摘要。
//! manifest / descriptor 的语义校验由 `service` 调用 `domain` 完成——错误
//! 顺序必须是「先解包安全，再语义」，否则坏包可能先触发语义报错掩盖逃逸。
//!
//! # 完整性
//!
//! 当前只做摘要校验（`--sha256` 可选固定，安装记录保存解包目录摘要），不做
//! 签名校验。摘要能发现下载损坏与传输截断，不提供身份保证——任何人构建的
//! 插件都可以安装。`PluginArtifactVerifier` 负责重算摘要并复查元数据。

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
pub struct PluginArtifactSource;

impl PluginArtifactSource {
    /// 新建
    #[must_use]
    pub fn new() -> Self {
        Self
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
        // 只有 GitHub 来源能给出比请求更精确的定位（这里存的是请求本身，
        // 见下方 `Github` 分支的注释）。
        let mut resolved_source = None;
        let sha256 = match origin {
            ArtifactOrigin::Local(path) => {
                if path.is_dir() {
                    copy_tree(path, &root)?;
                    digest_tree(&root)?
                } else {
                    let sha = digest_file(path)?;
                    extract_archive(path, &root)?;
                    sha
                }
            }
            ArtifactOrigin::Url(url) => {
                let archive = dest_root.join("package.zip");
                download(url, &archive)?;
                let sha = digest_file(&archive)?;
                extract_archive(&archive, &root)?;
                let _ = std::fs::remove_file(&archive);
                sha
            }
            ArtifactOrigin::Github { owner, repo, rev } => {
                let archive_source = crate::plugin_github::resolve(owner, repo, rev.as_deref())?;
                let archive = dest_root.join("package.zip");
                download(&archive_source.url, &archive)?;
                let sha = digest_file(&archive)?;
                extract_archive(&archive, &root)?;
                let _ = std::fs::remove_file(&archive);
                // 记录存用户请求的引用（分支 / tag），不存解析出来的 commit：
                // 存 commit 会丢掉分支名，作者把产物放 `dist` 分支时 `update`
                // 就跟不动了。要钉死版本应当用 `--ref <tag|sha>`。
                resolved_source = Some(
                    domain::plugin::source::PluginSource::Github {
                        owner: owner.clone(),
                        repo: repo.clone(),
                        rev: rev.clone(),
                    }
                    .to_string(),
                );
                sha
            }
        };
        let root = unwrap_single_dir(root)?;
        let (manifest, descriptor) = read_metadata(&root)?;
        Ok(PluginArtifact {
            root,
            manifest,
            descriptor,
            sha256,
            resolved_source,
        })
    }
}

/// 摘要与元数据复查
///
/// 重算解包目录摘要，并重新读取 manifest / descriptor。
#[derive(Debug, Clone, Default)]
pub struct PluginArtifactVerifier;

impl PluginArtifactVerifier {
    /// 新建
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ArtifactVerifier for PluginArtifactVerifier {
    /// 校验已安装目录
    ///
    /// # Errors
    ///
    /// 摘要不匹配返回 `invalid_input`；元数据不可读时返回 `invalid_input`。
    fn verify_installed(
        &self,
        root: &Path,
        expected_sha256: Option<&Checksum>,
    ) -> Result<PluginArtifact, HostError> {
        let sha256 = digest_tree(root)?;
        check_checksum(expected_sha256, &sha256)?;
        let (manifest, descriptor) = read_metadata(root)?;
        Ok(PluginArtifact {
            root: root.to_path_buf(),
            manifest,
            descriptor,
            sha256,
            resolved_source: None,
        })
    }

    fn verify(
        &self,
        artifact: &PluginArtifact,
        expected_sha256: Option<&Checksum>,
    ) -> Result<PluginArtifact, HostError> {
        // 期望摘要是来源摘要（压缩包字节或目录树），与安装记录里的树摘要
        // 不同：先比对来源摘要，再重算并返回树摘要。
        check_checksum(expected_sha256, &artifact.sha256)?;
        // 只重算摘要与元数据；来源定位是来源解析的产物，重算节点拿不到，
        // 必须原样保留，否则 git 来源会退化成可移动的 tag。
        let mut verified = self.verify_installed(&artifact.root, None)?;
        verified
            .resolved_source
            .clone_from(&artifact.resolved_source);
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

fn http_client() -> Result<reqwest::blocking::Client, HostError> {
    // `rustls-no-provider` 下必须先装 crypto provider，否则 reqwest 内部
    // 线程直接 panic（`--url` / GitHub 归档两条路都走这里）。
    crate::http::install_crypto_provider();
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
        restore_mode(&target, entry.unix_mode())?;
    }
    Ok(())
}

/// 恢复归档记录的文件权限位
///
/// `File::create` 按当前 umask 建房，会把 zip 里的 Unix 模式位一起丢掉：
/// 产物中的 `bin/run.sh` 解包后变成 `0o644`，入口校验随即报
/// `is not executable`。这里只取低 9 位，文件类型、setuid / setgid /
/// sticky 一律忽略。
fn restore_mode(path: &Path, unix_mode: Option<u32>) -> Result<(), HostError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let Some(mode) = unix_mode else {
            return Ok(());
        };
        // 由 Windows 工具生成的归档可能只带 DOS 属性，低 9 位可能全为 0；
        // 此时不覆盖新建文件的默认权限。
        if mode.trailing_zeros() >= 9 {
            return Ok(());
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o777))
            .map_err(|err| HostError::internal(format!("set mode {}: {err}", path.display())))?;
    }
    #[cfg(not(unix))]
    let _ = (path, unix_mode);
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

    /// zip 里的 Unix 模式位必须在解包后保留
    ///
    /// 产物入口通常是 `bin/run.sh`，宿主按 `0o111` 校验可执行；`File::create`
    /// 建房会丢掉归档里的模式位，导致安装时报 `is not executable`。
    #[cfg(unix)]
    #[test]
    fn archive_preserves_executable_bit() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tmp");
        let archive = dir.path().join("plugin.zip");
        {
            let file = std::fs::File::create(&archive).expect("create");
            let mut zip = zip::ZipWriter::new(file);
            let plain = zip::write::SimpleFileOptions::default();
            for (name, body) in [
                ("manifest.json", manifest_json("demo")),
                ("descriptor.json", descriptor_json("demo")),
            ] {
                zip.start_file(name, plain).expect("entry");
                zip.write_all(body.as_bytes()).expect("write");
            }
            zip.start_file("bin/run.sh", plain.unix_permissions(0o755))
                .expect("entry");
            zip.write_all(b"#!/bin/sh\nexec node entry.mjs\n")
                .expect("write");
            zip.finish().expect("finish");
        }
        let dest = tempfile::tempdir().expect("dest");
        let artifact = PluginArtifactSource::new()
            .fetch(&ArtifactOrigin::Local(archive), dest.path())
            .expect("fetch");
        let entry = artifact.root.join("bin/run.sh");
        let mode = std::fs::metadata(&entry)
            .expect("stat entry")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "entry mode {mode:o}");
    }

    #[test]
    fn verifier_recomputes_digest_and_rejects_mismatch() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("manifest.json"), manifest_json("demo")).expect("manifest");
        std::fs::write(dir.path().join("descriptor.json"), descriptor_json("demo"))
            .expect("descriptor");
        let verifier = PluginArtifactVerifier;
        let artifact = verifier.verify_installed(dir.path(), None).expect("verify");
        assert_eq!(artifact.sha256.len(), 64);

        let wrong = Checksum::new(ChecksumAlgorithm::Sha256, "0".repeat(64));
        let err = verifier
            .verify_installed(dir.path(), Some(&wrong))
            .expect_err("digest mismatch");
        assert_eq!(err.code, port::plugin::ErrorCode::InvalidInput);
    }

    #[test]
    fn reserved_names_are_rejected() {
        assert!(is_reserved_name("CON"));
        assert!(is_reserved_name("nul.txt"));
        assert!(is_reserved_name("name. "));
        assert!(!is_reserved_name("console.txt"));
    }
}
