//! 凭证存储：把 token 存到 0600 文件
//!
//! 口径来自 `docs/remote.md`「安全」：token 优先存操作系统钥匙串，无钥匙串时
//! 退到权限 `0600` 的文件。本模块只实现后者，并把前者留成 [`CredentialStore`]
//! 的另一个实现——调用方只见 trait，换存储不动业务代码。
//!
//! 落盘格式刻意简单（一行一条 `name = token` 的 TOML）：用户可能在
//! `~/.local/share/mcsup/credentials.toml` 里手查、手删，二进制或加密格式会让
//! 「我该怎么清掉这个凭证」变成需要文档才能回答的问题。
//!
//! 但简单不等于可以放松权限：写文件时用 `0600` 创建，读之前先校验权限，
//! 权限比 `0600` 宽时**拒绝读取**而不是静默继续——静默继续等于假装保护生效。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// 凭证存储失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CredentialError {
    /// 读取 / 写入失败
    #[error("credential file {path}: {source}")]
    Io {
        /// 文件路径
        path: PathBuf,
        /// 底层错误
        #[source]
        source: std::io::Error,
    },
    /// 文件内容不是合法 TOML 或字段不符
    #[error("credential file {path} is corrupt: {reason}")]
    Corrupt {
        /// 文件路径
        path: PathBuf,
        /// 具体原因
        reason: String,
    },
    /// 文件权限比 `0600` 宽（Unix）
    #[error("credential file {path} is readable by others (mode {mode:o}); run `chmod 600 {path}`")]
    InsecurePermissions {
        /// 文件路径
        path: PathBuf,
        /// 当前权限位
        mode: u32,
    },
    /// 该名字没有凭证
    #[error("no credential stored for {0}")]
    NotFound(String),
}

impl CredentialError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// 一条已存凭证
///
/// 只放「够用就好」的字段：token 本体、客户端身份、角色。远端地址与 target
/// 在 `config.toml` 的配对记录里，不重复存——两处都存就会分叉。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct StoredCredential {
    /// 明文 token
    pub token: String,
    /// 服务端分配的客户端身份
    pub client_id: String,
    /// 角色（`viewer` / `operator` / `admin`）
    pub role: String,
}

/// 凭证存储：按名字读写 token
///
/// `name` 是远端名。实现可以是文件、钥匙串、内存——业务代码只见这个 trait。
pub trait CredentialStore: Send + Sync {
    /// 写入（覆盖）一条凭证
    ///
    /// # Errors
    ///
    /// 目录创建或写入失败时返回 [`CredentialError`]
    fn put(&self, name: &str, credential: &StoredCredential) -> Result<(), CredentialError>;

    /// 读取一条凭证
    ///
    /// # Errors
    ///
    /// 不存在返回 [`CredentialError::NotFound`]；文件损坏或权限过宽时返回其余
    fn get(&self, name: &str) -> Result<StoredCredential, CredentialError>;

    /// 删除一条凭证；本来就没有视为成功（幂等）
    ///
    /// # Errors
    ///
    /// 写入失败时返回 [`CredentialError`]
    fn remove(&self, name: &str) -> Result<(), CredentialError>;

    /// 全部已存凭证的名字，升序
    ///
    /// # Errors
    ///
    /// 读取失败时返回 [`CredentialError`]
    fn names(&self) -> Result<Vec<String>, CredentialError>;
}

/// 文件落盘形态：`name = { token, client-id, role }`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
struct CredentialFile(BTreeMap<String, StoredCredential>);

/// 0600 文件凭证存储
#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    /// 以给定文件路径构造（不创建文件）
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 文件路径
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读整份文件；不存在时为空表
    fn load(&self) -> Result<CredentialFile, CredentialError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CredentialFile::default());
            }
            Err(err) => return Err(CredentialError::io(&self.path, err)),
        };
        self.check_permissions()?;
        toml::from_str(&text).map_err(|err| CredentialError::Corrupt {
            path: self.path.clone(),
            reason: err.to_string(),
        })
    }

    /// 原子写回并保证 0600
    fn save(&self, file: &CredentialFile) -> Result<(), CredentialError> {
        let text = toml::to_string_pretty(file).map_err(|err| CredentialError::Corrupt {
            path: self.path.clone(),
            reason: err.to_string(),
        })?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| CredentialError::io(parent, err))?;
        }
        let tmp = self.path.with_extension("toml.tmp");
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            // 创建时就限定 0600：先创建再 chmod 会有一个窗口期文件是可读的
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options
                .open(&tmp)
                .map_err(|err| CredentialError::io(&tmp, err))?;
            file.write_all(text.as_bytes())
                .map_err(|err| CredentialError::io(&tmp, err))?;
            file.sync_all()
                .map_err(|err| CredentialError::io(&tmp, err))?;
        }
        // 已存在的旧文件可能权限更宽，rename 前先收紧临时文件
        restrict(&tmp)?;
        std::fs::rename(&tmp, &self.path).map_err(|err| CredentialError::io(&self.path, err))
    }

    /// 校验文件权限不比 0600 宽
    ///
    /// 只在 Unix 上有意义；Windows 的 ACL 模型与 `mode` 不对应，交给系统默认
    /// （用户目录本身已受 ACL 保护）。
    fn check_permissions(&self) -> Result<(), CredentialError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let meta = std::fs::metadata(&self.path)
                .map_err(|err| CredentialError::io(&self.path, err))?;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(CredentialError::InsecurePermissions {
                    path: self.path.clone(),
                    mode,
                });
            }
        }
        Ok(())
    }
}

/// 把权限收紧到 0600（Unix）
fn restrict(path: &Path) -> Result<(), CredentialError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|err| CredentialError::io(path, err))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

impl CredentialStore for FileCredentialStore {
    fn put(&self, name: &str, credential: &StoredCredential) -> Result<(), CredentialError> {
        let mut file = self.load()?;
        file.0.insert(name.to_owned(), credential.clone());
        self.save(&file)
    }

    fn get(&self, name: &str) -> Result<StoredCredential, CredentialError> {
        self.load()?
            .0
            .get(name)
            .cloned()
            .ok_or_else(|| CredentialError::NotFound(name.to_owned()))
    }

    fn remove(&self, name: &str) -> Result<(), CredentialError> {
        let mut file = self.load()?;
        if file.0.remove(name).is_some() {
            self.save(&file)?;
        }
        Ok(())
    }

    fn names(&self) -> Result<Vec<String>, CredentialError> {
        Ok(self.load()?.0.into_keys().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential() -> StoredCredential {
        StoredCredential {
            token: "a".repeat(64),
            client_id: "6f1a1d3e-0b7c-4a51-9c8e-2b0f3a4d5e60".to_owned(),
            role: "operator".to_owned(),
        }
    }

    fn store() -> (tempfile::TempDir, FileCredentialStore) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let store = FileCredentialStore::new(dir.path().join("credentials.toml"));
        (dir, store)
    }

    #[test]
    fn put_get_remove_roundtrip() {
        let (_dir, store) = store();
        assert!(matches!(
            store.get("prod-box"),
            Err(CredentialError::NotFound(_))
        ));
        store.put("prod-box", &credential()).expect("put");
        assert_eq!(store.get("prod-box").expect("get"), credential());
        assert_eq!(store.names().expect("names"), vec!["prod-box".to_owned()]);
        store.remove("prod-box").expect("remove");
        assert!(matches!(
            store.get("prod-box"),
            Err(CredentialError::NotFound(_))
        ));
        // 幂等：删不存在的不报错
        store.remove("prod-box").expect("remove again");
    }

    #[test]
    fn names_are_sorted_and_put_overwrites() {
        let (_dir, store) = store();
        store.put("zeta", &credential()).expect("put");
        store.put("alpha", &credential()).expect("put");
        assert_eq!(
            store.names().expect("names"),
            vec!["alpha".to_owned(), "zeta".to_owned()]
        );
        let mut updated = credential();
        updated.role = "viewer".to_owned();
        store.put("alpha", &updated).expect("overwrite");
        assert_eq!(store.get("alpha").expect("get").role, "viewer");
    }

    /// 落盘必须是 0600，且不留临时文件
    #[cfg(unix)]
    #[test]
    fn file_is_created_with_restrictive_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let (dir, store) = store();
        store.put("prod-box", &credential()).expect("put");
        let mode = std::fs::metadata(store.path())
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "凭证文件必须是 0600");
        let tmp = store.path().with_extension("toml.tmp");
        assert!(!tmp.exists(), "临时文件应已 rename");
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
    }

    /// 权限被放宽时必须拒绝读取，而不是假装保护生效
    #[cfg(unix)]
    #[test]
    fn wide_permissions_are_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, store) = store();
        store.put("prod-box", &credential()).expect("put");
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644))
            .expect("chmod");
        let err = store.get("prod-box").expect_err("必须拒绝");
        assert!(
            matches!(
                err,
                CredentialError::InsecurePermissions { mode: 0o644, .. }
            ),
            "{err}"
        );
        assert!(err.to_string().contains("chmod 600"), "{err}");
    }

    #[test]
    fn corrupt_file_is_reported_with_path() {
        let (dir, store) = store();
        std::fs::write(store.path(), "not toml at all").expect("write");
        let err = store.names().expect_err("corrupt");
        assert!(err.to_string().contains("credentials.toml"), "{err}");
        assert!(dir.path().exists());
    }
}
