//! 撤销列表缓存：读写、权限与原子替换
//!
//! 判定规则（命中、版本范围）在 `domain::plugin::revocation`，这里只负责
//! 「文件长得对不对」与「写下去会不会留半截」。
//!
//! 缓存是安全设施：读不懂不能当成空表放行，所以权限过宽、符号链接、内容
//! 超限、JSON 非法一律返回错误，由调用方按「无法检查」处理。

use crate::private_file;
use domain::plugin::revocation::{MAX_REVOCATION_BYTES, RevocationList};
use port::plugin::HostError;
use std::io::ErrorKind;
use std::path::Path;

/// 撤销列表缓存文件名（位于 `plugins/` 下）
pub const REVOCATION_FILE: &str = "revoked.json";

/// 读取撤销列表缓存
///
/// 文件不存在返回 `Ok(None)`；权限过宽、符号链接、不是普通文件、内容超限
/// 或非法都返回错误——撤销列表是安全设施，读不懂就不能当成空表。
///
/// # Errors
///
/// 见上；错误码为 `denied`（权限 / 链接）或 `invalid_input`（内容）。
pub fn load_revocation_list(path: &Path) -> Result<Option<RevocationList>, HostError> {
    use std::io::Read as _;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(HostError::internal(format!(
                "stat revocation list {}: {err}",
                path.display()
            )));
        }
    };
    if !metadata.is_file() {
        return Err(HostError::denied(format!(
            "revocation list {} must be a regular file",
            path.display()
        )));
    }
    if metadata.len() > MAX_REVOCATION_BYTES {
        return Err(HostError::invalid_input(format!(
            "revocation list {} exceeds {MAX_REVOCATION_BYTES} bytes",
            path.display()
        )));
    }
    private_file::check_permissions(path, &metadata, false);
    let file = std::fs::File::open(path).map_err(|err| {
        HostError::internal(format!("open revocation list {}: {err}", path.display()))
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_REVOCATION_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| HostError::internal(format!("read revocation list: {err}")))?;
    if bytes.len() as u64 > MAX_REVOCATION_BYTES {
        return Err(HostError::invalid_input(format!(
            "revocation list {} exceeds {MAX_REVOCATION_BYTES} bytes",
            path.display()
        )));
    }
    RevocationList::from_json(&bytes)
        .map(Some)
        .map_err(|err| HostError::invalid_input(err.to_string()))
}

/// 写入撤销列表缓存：同目录临时文件 → `fsync` → `rename`，文件权限 `0600`
///
/// # Errors
///
/// 序列化或写盘失败返回 `internal`。
pub fn store_revocation_list(path: &Path, list: &RevocationList) -> Result<(), HostError> {
    let bytes = list
        .to_json()
        .map_err(|err| HostError::invalid_input(err.to_string()))?;
    let parent = path.parent().ok_or_else(|| {
        HostError::invalid_input(format!(
            "revocation list path {} has no parent",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(parent)
        .map_err(|err| HostError::internal(format!("create {}: {err}", parent.display())))?;
    let temp = path.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp)
        .map_err(|err| HostError::internal(format!("create {}: {err}", temp.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|err| HostError::internal(format!("chmod {}: {err}", temp.display())))?;
    }
    {
        use std::io::Write as _;
        file.write_all(&bytes)
            .map_err(|err| HostError::internal(format!("write {}: {err}", temp.display())))?;
        file.sync_all()
            .map_err(|err| HostError::internal(format!("sync {}: {err}", temp.display())))?;
    }
    std::fs::rename(&temp, path)
        .map_err(|err| HostError::internal(format!("replace {}: {err}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(json: &str) -> RevocationList {
        RevocationList::from_json(json.as_bytes()).expect("parse")
    }

    #[test]
    fn missing_cache_is_none() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join(REVOCATION_FILE);
        assert!(load_revocation_list(&path).expect("load").is_none());
    }

    #[test]
    fn store_then_load_round_trip() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join(REVOCATION_FILE);
        let list = list(r#"{"schema":1,"entries":[{"id":"demo","versions":["0.1.0"]}]}"#);
        store_revocation_list(&path, &list).expect("store");
        let loaded = load_revocation_list(&path).expect("load").expect("some");
        assert_eq!(loaded, list);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn symlinked_cache_is_rejected() {
        let dir = tempfile::tempdir().expect("tmp");
        let real = dir.path().join("real.json");
        std::fs::write(&real, br#"{"schema":1,"entries":[]}"#).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).expect("chmod");
            let link = dir.path().join(REVOCATION_FILE);
            std::os::unix::fs::symlink(&real, &link).expect("symlink");
            let err = load_revocation_list(&link).expect_err("symlink");
            assert_eq!(err.code, port::plugin::ErrorCode::Denied);
        }
    }

    #[test]
    fn malformed_cache_is_an_error_not_empty() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join(REVOCATION_FILE);
        std::fs::write(&path, b"not json").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        let err = load_revocation_list(&path).expect_err("malformed");
        assert_eq!(err.code, port::plugin::ErrorCode::InvalidInput);
    }
}
