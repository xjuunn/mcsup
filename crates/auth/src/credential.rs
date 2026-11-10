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

/// 凭证文件跨进程锁的等待上限
const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 同一进程内并发写时区分临时文件名
static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    /// 跨进程锁在超时内没拿到
    #[error("credential file {path} is locked by another process (waited {waited:?})")]
    LockBusy {
        /// 锁文件路径
        path: PathBuf,
        /// 等待时长
        waited: std::time::Duration,
    },
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
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct StoredCredential {
    /// 明文 token
    pub token: String,
    /// 服务端分配的客户端身份
    pub client_id: String,
    /// 角色（`viewer` / `operator` / `admin`）
    pub role: String,
}

/// 手写 [`Debug`]：`token` 必须脱敏
///
/// 与 [`crate::token::Token`] 同一口径。派生 `Debug` 会让 `tracing::debug!(?cred)`
/// 或任何 `{:?}` 把明文 token 写进日志——一次误用即泄漏，且在评审时看不出来。
impl std::fmt::Debug for StoredCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredCredential")
            .field("token", &"***")
            .field("client_id", &self.client_id)
            .field("role", &self.role)
            .finish()
    }
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
    ///
    /// 调用方必须已持有 [`FileLock`]：这里是读-改-写的后半段，单独调用会让
    /// 并发写者的更新互相覆盖。
    fn save(&self, file: &CredentialFile) -> Result<(), CredentialError> {
        let text = toml::to_string_pretty(file).map_err(|err| CredentialError::Corrupt {
            path: self.path.clone(),
            reason: err.to_string(),
        })?;
        write_private(&self.path, &text)
    }

    /// 读-改-写：整段持锁，避免并发写者丢失更新
    fn mutate<F>(&self, change: F) -> Result<(), CredentialError>
    where
        F: FnOnce(&mut CredentialFile),
    {
        let _lock = self.lock()?;
        let mut file = self.load()?;
        change(&mut file);
        self.save(&file)
    }

    /// 取跨进程锁；锁文件与数据文件同目录
    ///
    /// 与 [`crate::grant`] 同一口径（`try_lock` + 超时 + 退避）。凭证文件是
    /// 「整个文件一个 map」的形态，任何读-改-写都必须独占，否则两个
    /// `mcsup remote connect` 并发时会丢掉一条凭证。
    fn lock(&self) -> Result<FileLock, CredentialError> {
        let lock_path = self.path.with_extension("lock");
        FileLock::acquire(&lock_path)
    }
}

/// 跨进程文件锁句柄：关闭（含进程崩溃）即释放
struct FileLock {
    /// 只用于保活；drop 时操作系统自动解锁
    _file: std::fs::File,
}

impl FileLock {
    /// 在 `path` 上取排他锁，最多等 [`LOCK_TIMEOUT`]
    ///
    /// 锁文件永不删除：删掉会让等待者在新旧 inode 上各持一把锁。
    fn acquire(path: &Path) -> Result<Self, CredentialError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| CredentialError::io(parent, err))?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|err| CredentialError::io(path, err))?;
        let deadline = std::time::Instant::now() + LOCK_TIMEOUT;
        let mut delay = std::time::Duration::from_millis(2);
        loop {
            match file.try_lock() {
                Ok(()) => {
                    let _ = file.set_len(0);
                    let mut handle = &file;
                    let _ = writeln!(handle, "{}", std::process::id());
                    return Ok(Self { _file: file });
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(CredentialError::LockBusy {
                            path: path.to_path_buf(),
                            waited: LOCK_TIMEOUT,
                        });
                    }
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(std::time::Duration::from_millis(50));
                }
                Err(std::fs::TryLockError::Error(err)) => {
                    return Err(CredentialError::io(path, err));
                }
            }
        }
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
    // 临时文件名带 pid + 序号：即使调用方漏了锁，两个写者也不会共用同一个
    // 临时文件而把内容交错写坏
    let tmp = path.with_extension(format!(
        "toml.{}.{}.tmp",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
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
        let owned = credential.clone();
        self.mutate(move |file| {
            file.0.insert(name.to_owned(), owned);
        })
    }

    fn get(&self, name: &str) -> Result<StoredCredential, CredentialError> {
        self.load()?
            .0
            .get(name)
            .cloned()
            .ok_or_else(|| CredentialError::NotFound(name.to_owned()))
    }

    fn remove(&self, name: &str) -> Result<(), CredentialError> {
        // 删除是不存在的名字时也持锁：读-改-写本身就是临界区，跳过它就等于
        // 允许一个并发 put 与本删除交错
        self.mutate(|file| {
            file.0.remove(name);
        })
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
#[non_exhaustive]
pub enum CredentialVault {
    /// 系统钥匙串
    Keyring(KeyringCredentialStore<SystemKeyring>),
    /// 0600 文件
    File(FileCredentialStore),
}

impl CredentialVault {
    /// 按平台能力选择后端（不读环境变量）
    ///
    /// 库不读环境：宿主进程里的一个变量就能静默改变密钥存放位置，而调用方
    /// 无从得知。需要显式指定后端时用 [`Self::open_with`]，取值由 app 层决定。
    #[must_use]
    pub fn open(file_path: impl Into<PathBuf>) -> Self {
        Self::open_with(file_path, None)
    }

    /// 按显式指定 + 平台能力选择后端
    ///
    /// `choice` 是 `Some("keyring")` / `Some("file")` 时强制该后端，`None` /
    /// `Some("auto")` / 无法识别的取值按「钥匙串可用则用它，否则退文件」。
    #[must_use]
    pub fn open_with(file_path: impl Into<PathBuf>, choice: Option<&str>) -> Self {
        let file_path = file_path.into();
        let choice = choose_backend(choice, keyring_available());
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

    /// 明文 token 绝不能出现在 `Debug` 输出里
    ///
    /// `Token` 通过不实现 `Debug` 保证这一点；本结构体是落盘形态，必须实现
    /// 才能被测试与日志用，因此改为手写脱敏。
    #[test]
    fn debug_redacts_token() {
        let cred = credential();
        let rendered = format!("{cred:?}");
        assert!(
            !rendered.contains(&cred.token),
            "Debug 泄漏了明文 token: {rendered}"
        );
        assert!(rendered.contains("***"), "{rendered}");
        assert!(
            rendered.contains("operator"),
            "非敏感字段应保留: {rendered}"
        );
    }

    /// 读-改-写持锁：并发 put 不得丢失更新
    #[test]
    fn concurrent_puts_do_not_lose_updates() {
        let (_dir, store) = store();
        let store = Arc::new(store);
        let mut handles = Vec::new();
        for i in 0..8 {
            let store = Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                store
                    .put(&format!("remote-{i}"), &credential())
                    .expect("put");
            }));
        }
        for handle in handles {
            handle.join().expect("join");
        }
        let names = store.names().expect("names");
        assert_eq!(names.len(), 8, "并发写入丢了更新: {names:?}");
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
        // 临时文件名带 pid + 序号，用前缀扫目录确认没有残留
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .flatten()
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件应已 rename: {leftovers:?}");
        // 目录里只应有凭证文件与锁文件（锁文件永不删除，见 FileLock::acquire）
        let mut entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            ["credentials.lock", "credentials.toml"],
            "{entries:?}"
        );
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
