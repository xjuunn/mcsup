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
use std::sync::Arc;
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
    /// 系统钥匙串失败（含索引内容损坏）
    #[error("keyring: {0}")]
    Keyring(String),
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
        let Some(text) = read_private(&self.path)? else {
            return Ok(CredentialFile::default());
        };
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
        write_private(&self.path, &text)
    }
}

/// 读一个受保护的文本文件；不存在时返回 `None`
///
/// 「受保护」= Unix 上权限不得比 `0600` 宽。宽了就拒绝读取，而不是静默继续：
/// 静默继续等于假装保护生效。
pub(crate) fn read_private(path: &Path) -> Result<Option<String>, CredentialError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(CredentialError::io(path, err)),
    };
    #[cfg(unix)]
    check_permissions(path)?;
    Ok(Some(text))
}

/// 原子写入并保证 `0600`（同目录临时文件 + fsync + rename）
pub(crate) fn write_private(path: &Path, text: &str) -> Result<(), CredentialError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| CredentialError::io(parent, err))?;
    }
    let tmp = path.with_extension("toml.tmp");
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
    #[cfg(unix)]
    restrict(&tmp)?;
    std::fs::rename(&tmp, path).map_err(|err| CredentialError::io(path, err))
}

/// 校验文件权限不比 0600 宽
///
/// 只在 Unix 上有意义；Windows 的 ACL 模型与 `mode` 不对应，交给系统默认
/// （用户目录本身已受 ACL 保护）。
#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<(), CredentialError> {
    use std::os::unix::fs::PermissionsExt as _;
    let meta = std::fs::metadata(path).map_err(|err| CredentialError::io(path, err))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(CredentialError::InsecurePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

/// 把权限收紧到 0600（Unix）
#[cfg(unix)]
fn restrict(path: &Path) -> Result<(), CredentialError> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|err| CredentialError::io(path, err))
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

/// 钥匙串底层：一条 entry 的读写
///
/// 抽成 trait 是为了让 [`KeyringCredentialStore`] 的核心逻辑（索引维护、JSON
/// 编解码、幂等删除）能用内存实现测试，不必依赖真实系统钥匙串。
pub trait KeyringBackend: Send + Sync {
    /// 写入 / 覆盖一条
    ///
    /// # Errors
    ///
    /// 底层钥匙串报错时返回 [`CredentialError::Keyring`]
    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), CredentialError>;

    /// 读取；不存在返回 `None`
    ///
    /// # Errors
    ///
    /// 底层钥匙串报错时返回 [`CredentialError::Keyring`]
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, CredentialError>;

    /// 删除；不存在视为成功（幂等）
    ///
    /// # Errors
    ///
    /// 底层钥匙串报错时返回 [`CredentialError::Keyring`]
    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError>;
}

/// 系统钥匙串：macOS Keychain / Windows Credential Manager / Linux Secret Service
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemKeyring;

impl SystemKeyring {
    fn entry(service: &str, account: &str) -> Result<keyring::Entry, CredentialError> {
        keyring::Entry::new(service, account)
            .map_err(|err| CredentialError::Keyring(format!("{service}/{account}: {err}")))
    }
}

impl KeyringBackend for SystemKeyring {
    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), CredentialError> {
        Self::entry(service, account)?
            .set_password(secret)
            .map_err(|err| CredentialError::Keyring(format!("{service}/{account}: {err}")))
    }

    fn get(&self, service: &str, account: &str) -> Result<Option<String>, CredentialError> {
        match Self::entry(service, account)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(CredentialError::Keyring(format!(
                "{service}/{account}: {err}"
            ))),
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
        match Self::entry(service, account)?.delete_credential() {
            // 幂等：删不存在的凭证也算成功
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(CredentialError::Keyring(format!(
                "{service}/{account}: {err}"
            ))),
        }
    }
}

/// 索引 entry 的 account 名（与远端名隔离在 `{service}:index` 这个 service 下）
const INDEX_ACCOUNT: &str = "names";

/// 系统钥匙串凭证存储
///
/// 每个远端名对应一条 entry（account = 名字，secret = [`StoredCredential`] 的
/// JSON）。钥匙串不支持枚举，`names()` 由一条独立索引 entry 维护：写入 / 删除
/// 时同步更新。索引放在另一个 service（`{service}:index`），避免与某个恰好
/// 叫 `names` 的远端名冲突。
#[derive(Debug, Clone)]
pub struct KeyringCredentialStore<B: KeyringBackend = SystemKeyring> {
    backend: Arc<B>,
    service: String,
}

impl<B: KeyringBackend> KeyringCredentialStore<B> {
    /// 用给定 service 名与后端构造
    #[must_use]
    pub fn new(service: impl Into<String>, backend: B) -> Self {
        Self {
            backend: Arc::new(backend),
            service: service.into(),
        }
    }

    fn index_service(&self) -> String {
        format!("{}:index", self.service)
    }

    fn load_index(&self) -> Result<Vec<String>, CredentialError> {
        match self.backend.get(&self.index_service(), INDEX_ACCOUNT)? {
            Some(text) => serde_json::from_str(&text).map_err(|err| {
                CredentialError::Keyring(format!("credential index is corrupt: {err}"))
            }),
            None => Ok(Vec::new()),
        }
    }

    fn save_index(&self, names: &[String]) -> Result<(), CredentialError> {
        let text = serde_json::to_string(names)
            .map_err(|err| CredentialError::Keyring(format!("encode credential index: {err}")))?;
        self.backend
            .set(&self.index_service(), INDEX_ACCOUNT, &text)
    }

    fn add_index(&self, name: &str) -> Result<(), CredentialError> {
        let mut names = self.load_index()?;
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_owned());
            names.sort();
            self.save_index(&names)?;
        }
        Ok(())
    }

    fn remove_index(&self, name: &str) -> Result<(), CredentialError> {
        let mut names = self.load_index()?;
        let before = names.len();
        names.retain(|existing| existing != name);
        if names.len() != before {
            self.save_index(&names)?;
        }
        Ok(())
    }
}

impl<B: KeyringBackend> CredentialStore for KeyringCredentialStore<B> {
    fn put(&self, name: &str, credential: &StoredCredential) -> Result<(), CredentialError> {
        let secret = serde_json::to_string(credential)
            .map_err(|err| CredentialError::Keyring(format!("encode credential {name}: {err}")))?;
        self.backend.set(&self.service, name, &secret)?;
        self.add_index(name)
    }

    fn get(&self, name: &str) -> Result<StoredCredential, CredentialError> {
        let Some(secret) = self.backend.get(&self.service, name)? else {
            return Err(CredentialError::NotFound(name.to_owned()));
        };
        serde_json::from_str(&secret)
            .map_err(|err| CredentialError::Keyring(format!("credential {name} is corrupt: {err}")))
    }

    fn remove(&self, name: &str) -> Result<(), CredentialError> {
        self.backend.delete(&self.service, name)?;
        self.remove_index(name)
    }

    fn names(&self) -> Result<Vec<String>, CredentialError> {
        let mut names = self.load_index()?;
        names.sort();
        Ok(names)
    }
}

/// 凭证存储选择环境变量：`auto`（缺省）/ `file` / `keyring`
pub const CREDENTIAL_STORE_ENV: &str = "MCSUP_CREDENTIAL_STORE";

/// 默认钥匙串 service 名
pub const KEYRING_SERVICE: &str = "mcsup";

/// 显式指定的存储后端（`auto` 返回 `None`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreChoice {
    File,
    Keyring,
}

/// 解析环境变量取值；`auto` / 非法值都返回 `None`，交给可用性探测决定
fn parse_choice(raw: Option<&str>) -> Option<StoreChoice> {
    match raw.map(str::trim) {
        Some("file") => Some(StoreChoice::File),
        Some("keyring") => Some(StoreChoice::Keyring),
        _ => None,
    }
}

/// 探针 account：只读探测一个不存在的条目
const PROBE_ACCOUNT: &str = "__mcsup_probe__";

/// 选择后端：显式取值优先，否则按钥匙串是否真的可用决定
fn choose_backend(raw: Option<&str>, keyring_available: bool) -> StoreChoice {
    parse_choice(raw).unwrap_or(if keyring_available {
        StoreChoice::Keyring
    } else {
        StoreChoice::File
    })
}

/// 本机钥匙串此刻是否可用（进程内只探测一次）
///
/// 不看平台 / 环境变量，而是真的读一个不存在的探针条目：
/// `Ok(None)` / `Ok(Some(_))` 说明后端能工作，`Err` 说明没有可用的钥匙串
/// （headless 机器、没有 Secret Service 等），此时退到 0600 文件。
fn keyring_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| SystemKeyring.get(KEYRING_SERVICE, PROBE_ACCOUNT).is_ok())
}

/// 平台凭证存储：优先系统钥匙串，无钥匙串时退到 0600 文件
///
/// 调用方只见 [`CredentialStore`]；后端由 `MCSUP_CREDENTIAL_STORE` 或平台能力
/// 决定，换存储不动业务代码。`describe()` 给出后端描述，写进配对记录便于排查
/// 「凭证到底存在哪」。
#[derive(Debug, Clone)]
pub enum CredentialVault {
    /// 系统钥匙串
    Keyring(KeyringCredentialStore<SystemKeyring>),
    /// 0600 文件
    File(FileCredentialStore),
}

impl CredentialVault {
    /// 按环境变量 / 平台能力选择后端
    #[must_use]
    pub fn open(file_path: impl Into<PathBuf>) -> Self {
        let file_path = file_path.into();
        let choice = choose_backend(
            std::env::var(CREDENTIAL_STORE_ENV).ok().as_deref(),
            keyring_available(),
        );
        match choice {
            StoreChoice::Keyring => {
                Self::Keyring(KeyringCredentialStore::new(KEYRING_SERVICE, SystemKeyring))
            }
            StoreChoice::File => Self::File(FileCredentialStore::new(file_path)),
        }
    }

    /// 后端描述：`keyring` 或 `file:<路径>`
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Keyring(_) => "keyring".to_owned(),
            Self::File(store) => format!("file:{}", store.path().display()),
        }
    }
}

impl CredentialStore for CredentialVault {
    fn put(&self, name: &str, credential: &StoredCredential) -> Result<(), CredentialError> {
        match self {
            Self::Keyring(store) => store.put(name, credential),
            Self::File(store) => store.put(name, credential),
        }
    }

    fn get(&self, name: &str) -> Result<StoredCredential, CredentialError> {
        match self {
            Self::Keyring(store) => store.get(name),
            Self::File(store) => store.get(name),
        }
    }

    fn remove(&self, name: &str) -> Result<(), CredentialError> {
        match self {
            Self::Keyring(store) => store.remove(name),
            Self::File(store) => store.remove(name),
        }
    }

    fn names(&self) -> Result<Vec<String>, CredentialError> {
        match self {
            Self::Keyring(store) => store.names(),
            Self::File(store) => store.names(),
        }
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

    /// 内存钥匙串：用同一套（service, account）键空间模拟系统钥匙串
    #[derive(Debug, Default, Clone)]
    struct MemoryKeyring {
        entries: Arc<std::sync::Mutex<BTreeMap<(String, String), String>>>,
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

    #[test]
    fn keyring_store_roundtrip_index_and_idempotent_remove() {
        let store = KeyringCredentialStore::new("mcsup-test", MemoryKeyring::default());
        assert!(matches!(
            store.get("prod-box"),
            Err(CredentialError::NotFound(_))
        ));
        assert!(store.names().expect("names").is_empty());

        store.put("prod-box", &credential()).expect("put");
        assert_eq!(store.get("prod-box").expect("get"), credential());
        assert_eq!(store.names().expect("names"), vec!["prod-box".to_owned()]);

        // 索引保持升序、put 覆盖不重复登记
        store.put("alpha", &credential()).expect("put alpha");
        store.put("prod-box", &credential()).expect("overwrite");
        assert_eq!(
            store.names().expect("names"),
            vec!["alpha".to_owned(), "prod-box".to_owned()]
        );

        store.remove("prod-box").expect("remove");
        assert!(matches!(
            store.get("prod-box"),
            Err(CredentialError::NotFound(_))
        ));
        assert_eq!(store.names().expect("names"), vec!["alpha".to_owned()]);
        // 幂等：删不存在的不报错
        store.remove("prod-box").expect("remove again");
    }

    #[test]
    fn credential_store_choice_parsing_and_vault_describe() {
        assert_eq!(parse_choice(None), None);
        assert_eq!(parse_choice(Some("auto")), None);
        assert_eq!(parse_choice(Some(" file ")), Some(StoreChoice::File));
        assert_eq!(parse_choice(Some("keyring")), Some(StoreChoice::Keyring));
        // 非法值按 auto 处理，不 panic
        assert_eq!(parse_choice(Some("bogus")), None);

        // auto：按钥匙串真实可用性决定；显式取值优先
        assert_eq!(choose_backend(None, true), StoreChoice::Keyring);
        assert_eq!(choose_backend(None, false), StoreChoice::File);
        assert_eq!(choose_backend(Some("file"), true), StoreChoice::File);
        assert_eq!(choose_backend(Some("keyring"), false), StoreChoice::Keyring);

        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("credentials.toml");
        let vault = CredentialVault::File(FileCredentialStore::new(&path));
        assert!(vault.describe().starts_with("file:"));
        vault.put("box", &credential()).expect("put via vault");
        assert_eq!(vault.get("box").expect("get via vault"), credential());
    }
}
