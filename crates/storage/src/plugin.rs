//! 插件数据存储：安装记录、键值、密钥与审计
//!
//! 目录口径见 `docs/plugins.md` 第 17 节：
//!
//! ```text
//! {db}/plugins/<id>.json                 一条安装记录
//! {db}/audit/plugins/<YYYY-MM>.jsonl     追加写审计
//! {plugin-data}/<id>/kv/<hash>.json      一个键一个文件
//! {plugin-data}/<id>/secrets.json        凭据（0600；有机器密钥时是密文）
//! ```
//!
//! # 为什么审计用 JSONL
//!
//! 记录不可变、写入高频、按时间追加。一条记录一个 JSON 需要「先读后改」，
//! 并发写会丢事件；JSONL 的追加写与按月轮转是这里的正确形状。这是对既有
//! 「一条记录一个 JSON」约定的有意例外。
//!
//! # 权限
//!
//! 密钥文件创建即 `0600`；读取前校验权限，比 `0600` 宽时**拒绝读取**而不是
//! 静默继续。有机器密钥时内容再经 AES-256-GCM 加密，密钥只在系统钥匙串；
//! 钥匙串不可用时退化为 `0600` 明文，由 [`SecretFileStore::protection`] 报告，
//! 入口据此提示用户。键值文件与安装记录不含凭据，按默认权限写入。

use base64::Engine as _;
use domain::plugin::capability::Capability;
use domain::plugin::manifest::Manifest;
use domain::plugin::state::PluginState;
use domain::plugin::{PluginId, PluginScope};
use port::RepositoryError;
use port::plugin::{
    AuditEvent, AuditSink, HostError, KvStore, MAX_KV_KEY_BYTES, MAX_KV_LIST, MAX_KV_TOTAL_BYTES,
    MAX_KV_VALUE_BYTES, MAX_SECRET_KEY_BYTES, MAX_SECRET_VALUE_BYTES, PluginConfigStore,
    PluginRecord, PluginStore, SecretProtection, SecretStore, SecretValue, StoredConfig,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// 安装记录文件格式版本
const RECORD_SCHEMA: u32 = 1;

/// 同进程并发写时区分临时文件名
static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 一条安装记录
///
/// 字段口径与 `docs/plugins.md` 17.2 一致：时间用 RFC3339 UTC，当前版本
/// 只有 `active_version` 一处真相。`manifest` 原样保存，读回时重新走领域
/// 校验——磁盘上的东西不可信，宁可报损坏也不把未知能力带进运行时。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // 与 PluginRecord 一致：独立开关，不是状态机
struct FileRecord {
    schema: u32,
    id: String,
    active_version: String,
    api: u32,
    scope: PluginScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    server: Option<String>,
    enabled: bool,
    #[serde(default)]
    approved: Vec<String>,
    manifest: Manifest,
    installed_at: String,
    updated_at: String,
    last_state: PluginState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(default)]
    blocked: bool,
    #[serde(default)]
    quarantined: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artifact_dir: Option<String>,
}

impl FileRecord {
    fn from_record(record: &PluginRecord) -> Self {
        Self {
            schema: RECORD_SCHEMA,
            id: record.id.to_string(),
            active_version: record.version.clone(),
            api: record.api,
            scope: record.scope,
            server: record.server.as_ref().map(ToString::to_string),
            enabled: record.enabled,
            approved: record
                .approved
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            manifest: record.manifest.clone(),
            installed_at: format_rfc3339(record.installed_at),
            updated_at: format_rfc3339(record.updated_at),
            last_state: record.last_state,
            last_error: record.last_error.clone(),
            previous_version: record.previous_version.clone(),
            source: record.source.clone(),
            sha256: record.sha256.clone(),
            blocked: record.blocked,
            quarantined: record.quarantined,
            artifact_dir: Some(record.artifact_dir.to_string_lossy().into_owned()),
        }
    }

    fn into_record(self, root: &Path) -> Result<PluginRecord, RepositoryError> {
        if self.schema != RECORD_SCHEMA {
            return Err(RepositoryError::Corrupt(format!(
                "plugin {}: unsupported record schema {}",
                self.id, self.schema
            )));
        }
        let id = PluginId::parse(&self.id)
            .map_err(|err| RepositoryError::Corrupt(format!("{}: {err}", self.id)))?;
        let mut approved = std::collections::BTreeSet::new();
        for raw in &self.approved {
            let capability = Capability::parse(raw).map_err(|err| {
                RepositoryError::Corrupt(format!("plugin {id}: approved {raw}: {err}"))
            })?;
            approved.insert(capability);
        }
        let manifest_bytes = serde_json::to_vec(&self.manifest).map_err(|err| {
            RepositoryError::Corrupt(format!("plugin {id}: serialize manifest: {err}"))
        })?;
        let manifest = Manifest::from_json(&manifest_bytes)
            .map_err(|err| RepositoryError::Corrupt(format!("plugin {id}: manifest: {err}")))?;
        if manifest.id != id {
            return Err(RepositoryError::Corrupt(format!(
                "plugin {id}: manifest id {} does not match record",
                manifest.id
            )));
        }
        let server = match self.server.as_deref() {
            Some(raw) => Some(domain::server::meta::ServerName::parse(raw).map_err(|err| {
                RepositoryError::Corrupt(format!("plugin {id}: server {raw}: {err}"))
            })?),
            None => None,
        };
        if self.scope == PluginScope::Server && server.is_none() {
            return Err(RepositoryError::Corrupt(format!(
                "plugin {id}: server scope requires a server"
            )));
        }
        let artifact_dir = self.artifact_dir.map_or_else(
            || root.join(&self.id).join(&self.active_version),
            PathBuf::from,
        );
        Ok(PluginRecord {
            id,
            version: self.active_version,
            api: self.api,
            scope: self.scope,
            server,
            enabled: self.enabled,
            approved,
            manifest: manifest.raw,
            artifact_dir,
            installed_at: parse_rfc3339(&self.installed_at).unwrap_or(0),
            updated_at: parse_rfc3339(&self.updated_at).unwrap_or(0),
            last_state: self.last_state,
            last_error: self.last_error,
            previous_version: self.previous_version,
            source: self.source,
            sha256: self.sha256,
            blocked: self.blocked,
            quarantined: self.quarantined,
        })
    }
}

/// 安装记录存储
pub struct PluginFileStore {
    root: PathBuf,
    lock: Mutex<()>,
}

impl std::fmt::Debug for PluginFileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginFileStore")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl PluginFileStore {
    /// 打开（并创建）记录目录
    ///
    /// # Errors
    ///
    /// 目录创建失败时返回 [`RepositoryError::Storage`]
    pub fn open(root: &Path) -> Result<Self, RepositoryError> {
        std::fs::create_dir_all(root).map_err(super::storage_err)?;
        Ok(Self {
            root: root.to_path_buf(),
            lock: Mutex::new(()),
        })
    }

    /// 记录根目录
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_of(&self, id: &PluginId) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    fn lock(&self) -> Result<WriteGuard<'_>, RepositoryError> {
        let guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        // 与其它元数据共用 `{db}/.lock`，避免两套跨进程锁互相看不见。
        let lock_path = self
            .root
            .parent()
            .map_or_else(|| self.root.join(".lock"), |parent| parent.join(".lock"));
        let dir = super::DirLock::acquire(&lock_path)?;
        Ok(WriteGuard {
            _thread: guard,
            _dir: dir,
        })
    }
}

/// 同时持有进程内锁与跨进程锁；drop 时按反序释放
struct WriteGuard<'a> {
    _thread: MutexGuard<'a, ()>,
    _dir: super::DirLock,
}

impl PluginStore for PluginFileStore {
    fn list(&self) -> Result<Vec<PluginRecord>, RepositoryError> {
        let mut records = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(records),
            Err(err) => return Err(super::storage_err(err)),
        };
        for entry in entries {
            let entry = entry.map_err(super::storage_err)?;
            let path = entry.path();
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
                continue;
            };
            if stem == "state" {
                continue;
            }
            records.push(self.read(&path)?);
        }
        records.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(records)
    }

    fn get(&self, id: &PluginId) -> Result<PluginRecord, RepositoryError> {
        let path = self.path_of(id);
        match self.read(&path) {
            Ok(record) => Ok(record),
            Err(RepositoryError::NotFound(_)) => Err(RepositoryError::NotFound(id.to_string())),
            Err(err) => Err(err),
        }
    }

    fn put(&self, record: &PluginRecord) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        super::write_atomically(&self.path_of(&record.id), &FileRecord::from_record(record))
    }

    fn remove(&self, id: &PluginId) -> Result<(), RepositoryError> {
        let _guard = self.lock()?;
        match std::fs::remove_file(self.path_of(id)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(RepositoryError::NotFound(id.to_string()))
            }
            Err(err) => Err(super::storage_err(err)),
        }
    }
}

impl PluginFileStore {
    fn read(&self, path: &Path) -> Result<PluginRecord, RepositoryError> {
        let bytes = std::fs::read(path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                RepositoryError::NotFound(path.display().to_string())
            } else {
                super::storage_err(err)
            }
        })?;
        let file: FileRecord = serde_json::from_slice(&bytes)
            .map_err(|err| RepositoryError::Corrupt(format!("{}: {err}", path.display())))?;
        let stem = path
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default();
        if file.id != stem {
            return Err(RepositoryError::Corrupt(format!(
                "{}: record id {} does not match file name",
                path.display(),
                file.id
            )));
        }
        file.into_record(&self.root)
    }
}

/// 配置存储：`{plugin-data}/<id>/config.json`（0600）
///
/// 配置可能包含 `x-mcsup-secret` 字段，因此与密钥文件同等对待：创建即
/// `0600`，读取前校验权限。普通字段与密钥字段放在同一个文件里，避免
/// 「哪些键算密钥」在读写两侧各判一次。
pub struct PluginConfigFileStore {
    root: PathBuf,
}

impl std::fmt::Debug for PluginConfigFileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginConfigFileStore")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl PluginConfigFileStore {
    /// `root` 是 `plugin-data` 目录
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path(&self, plugin: &PluginId) -> PathBuf {
        self.root.join(plugin.as_str()).join("config.json")
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    revision: u64,
    values: serde_json::Value,
}

impl PluginConfigStore for PluginConfigFileStore {
    fn get(&self, plugin: &PluginId) -> Result<Option<StoredConfig>, RepositoryError> {
        let path = self.path(plugin);
        check_secret_permissions(&path)
            .map_err(|err| RepositoryError::Corrupt(err.message.clone()))?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(super::storage_err(err)),
        };
        let file: ConfigFile = serde_json::from_slice(&bytes)
            .map_err(|err| RepositoryError::Corrupt(format!("{}: {err}", path.display())))?;
        Ok(Some(StoredConfig {
            revision: file.revision,
            values: file.values,
        }))
    }

    fn put(&self, plugin: &PluginId, config: &StoredConfig) -> Result<(), RepositoryError> {
        let path = self.path(plugin);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(super::storage_err)?;
        }
        let bytes = serde_json::to_vec_pretty(&ConfigFile {
            revision: config.revision,
            values: config.values.clone(),
        })
        .map_err(|err| RepositoryError::Corrupt(format!("serialize plugin config: {err}")))?;
        write_atomic_bytes(&path, &bytes, 0o600).map_err(super::storage_err)
    }
}

/// 键值存储：一个键一个文件
pub struct KvFileStore {
    root: PathBuf,
}

impl std::fmt::Debug for KvFileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvFileStore")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl KvFileStore {
    /// `root` 是 `plugin-data` 目录；每个插件的键值在其 `<id>/kv/` 下
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn dir(&self, plugin: &PluginId) -> PathBuf {
        self.root.join(plugin.as_str()).join("kv")
    }

    fn path(&self, plugin: &PluginId, key: &str) -> PathBuf {
        // 文件名用 SHA-256，避免键里的任意字节撞上路径分隔符或超长文件名。
        let digest = Sha256::digest(key.as_bytes());
        self.dir(plugin).join(format!("{}.json", hex(&digest)))
    }

    fn total_bytes(&self, plugin: &PluginId) -> u64 {
        let mut total = 0;
        if let Ok(entries) = std::fs::read_dir(self.dir(plugin)) {
            for entry in entries.flatten() {
                if let Ok(metadata) = entry.metadata() {
                    total += metadata.len();
                }
            }
        }
        total
    }
}

impl KvStore for KvFileStore {
    fn get(&self, plugin: &PluginId, key: &str) -> Result<Option<Vec<u8>>, HostError> {
        validate_kv_key(key)?;
        let path = self.path(plugin, key);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(HostError::internal(format!("read kv: {err}"))),
        };
        let value: KvValue = serde_json::from_slice(&bytes)
            .map_err(|err| HostError::internal(format!("corrupt kv file: {err}")))?;
        if value.key != key {
            return Err(HostError::internal("kv file key mismatch"));
        }
        decode(&value.value)
            .map(Some)
            .map_err(|err| HostError::internal(format!("corrupt kv value: {err}")))
    }

    fn set(&self, plugin: &PluginId, key: &str, value: &[u8]) -> Result<(), HostError> {
        validate_kv_key(key)?;
        if value.len() > MAX_KV_VALUE_BYTES {
            return Err(HostError::limit_exceeded(format!(
                "kv value is {} bytes; limit is {MAX_KV_VALUE_BYTES}",
                value.len()
            )));
        }
        let encoded = encode(value);
        let path = self.path(plugin, key);
        let existing = std::fs::metadata(&path).map_or(0, |meta| meta.len());
        // 覆盖已有键时先扣掉旧文件；`saturating_sub` 防「统计与实际不一致」时下溢。
        let projected = self
            .total_bytes(plugin)
            .saturating_sub(existing)
            .saturating_add(encoded.len() as u64)
            .saturating_add(64);
        if projected > MAX_KV_TOTAL_BYTES as u64 {
            return Err(HostError::limit_exceeded(format!(
                "kv total would exceed {MAX_KV_TOTAL_BYTES} bytes"
            )));
        }
        std::fs::create_dir_all(self.dir(plugin))
            .map_err(|err| HostError::internal(format!("create kv dir: {err}")))?;
        let record = KvValue {
            key: key.to_owned(),
            value: encoded,
        };
        let bytes = serde_json::to_vec(&record)
            .map_err(|err| HostError::internal(format!("serialize kv: {err}")))?;
        write_atomic_bytes(&path, &bytes, 0o644)
            .map_err(|err| HostError::internal(format!("write kv: {err}")))
    }

    fn delete(&self, plugin: &PluginId, key: &str) -> Result<bool, HostError> {
        validate_kv_key(key)?;
        match std::fs::remove_file(self.path(plugin, key)) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(HostError::internal(format!("delete kv: {err}"))),
        }
    }

    fn list(
        &self,
        plugin: &PluginId,
        prefix: &str,
        after: Option<&str>,
    ) -> Result<Vec<String>, HostError> {
        if prefix.len() > MAX_KV_KEY_BYTES {
            return Err(HostError::invalid_input("kv prefix is too long"));
        }
        let mut keys = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.dir(plugin)) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                let Ok(value) = serde_json::from_slice::<KvValue>(&bytes) else {
                    continue;
                };
                if value.key.starts_with(prefix)
                    && after.is_none_or(|after| value.key.as_str() > after)
                {
                    keys.push(value.key);
                }
            }
        }
        keys.sort();
        keys.truncate(MAX_KV_LIST);
        Ok(keys)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct KvValue {
    key: String,
    value: String,
}

fn validate_kv_key(key: &str) -> Result<(), HostError> {
    if key.is_empty() {
        return Err(HostError::invalid_input("kv key is empty"));
    }
    if key.len() > MAX_KV_KEY_BYTES {
        return Err(HostError::limit_exceeded(format!(
            "kv key is {} bytes; limit is {MAX_KV_KEY_BYTES}",
            key.len()
        )));
    }
    if key.contains('\0') {
        return Err(HostError::invalid_input("kv key contains NUL"));
    }
    Ok(())
}

/// 密钥文件格式版本
const SECRETS_VERSION: u8 = 1;

/// 加密后的密钥文件：`v` / `nonce` / `data` 三个字段都是密文信封的一部分
///
/// 信封与旧版明文映射共用 JSON 对象形态，因此按三个字段的类型判定，
/// 不能只看键名（明文里可能有合法的 `v` 密钥）。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedSecrets {
    v: u8,
    /// base64(AES-256-GCM nonce)
    nonce: String,
    /// base64(密文 + tag)
    data: String,
}

impl SealedSecrets {
    /// `value` 是否是密文信封
    fn matches(value: &serde_json::Value) -> bool {
        value.get("v").is_some_and(serde_json::Value::is_u64)
            && value.get("nonce").is_some_and(serde_json::Value::is_string)
            && value.get("data").is_some_and(serde_json::Value::is_string)
    }
}

/// 机器密钥来源
///
/// 惰性加载：每次 CLI 命令都会装配存储，但绝大多数命令不碰插件密钥，不该为
/// 它们付一次钥匙串访问。
#[derive(Debug)]
enum SecretKeySource {
    /// 不加密，仅 `0600`
    None,
    /// 显式注入的密钥（测试与嵌入式装配）
    Fixed(auth::machine_key::MachineKey),
    /// 首次使用时从系统钥匙串读取；不可用则退化为文件权限
    SystemKeyring,
}

/// 密钥存储：整个插件一个 JSON 文件，权限 `0600`
///
/// 有机器密钥时文件内容是密文信封（插件 id 作为 AAD），否则退化为明文映射并
/// 由 [`SecretFileStore::protection`] 如实报告。明文只在钥匙串不可用时出现，
/// 属于「至少不比以前更弱」的退化，不是默认路径。
pub struct SecretFileStore {
    root: PathBuf,
    source: SecretKeySource,
    key: std::sync::OnceLock<Option<auth::machine_key::MachineKey>>,
}

impl std::fmt::Debug for SecretFileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = match &self.source {
            SecretKeySource::None => "none",
            SecretKeySource::Fixed(_) => "fixed",
            SecretKeySource::SystemKeyring => "keyring",
        };
        f.debug_struct("SecretFileStore")
            .field("root", &self.root)
            .field("source", &source)
            .finish_non_exhaustive()
    }
}

impl SecretFileStore {
    /// `root` 是 `plugin-data` 目录；不加密（测试与无钥匙串环境的显式装配）
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self::with_source(root, SecretKeySource::None)
    }

    /// 用给定机器密钥加密
    #[must_use]
    pub fn with_machine_key(root: PathBuf, key: auth::machine_key::MachineKey) -> Self {
        Self::with_source(root, SecretKeySource::Fixed(key))
    }

    /// 首次读写时从系统钥匙串取机器密钥；钥匙串不可用时退化为 `0600` 明文
    #[must_use]
    pub fn with_system_keyring(root: PathBuf) -> Self {
        Self::with_source(root, SecretKeySource::SystemKeyring)
    }

    fn with_source(root: PathBuf, source: SecretKeySource) -> Self {
        Self {
            root,
            source,
            key: std::sync::OnceLock::new(),
        }
    }

    fn dir(&self, plugin: &PluginId) -> PathBuf {
        self.root.join(plugin.as_str())
    }

    fn path(&self, plugin: &PluginId) -> PathBuf {
        self.dir(plugin).join("secrets.json")
    }

    /// 机器密钥（首次调用时解析并缓存）
    fn key(&self) -> Option<&auth::machine_key::MachineKey> {
        self.key
            .get_or_init(|| match &self.source {
                SecretKeySource::None => None,
                SecretKeySource::Fixed(key) => Some(key.clone()),
                SecretKeySource::SystemKeyring => auth::machine_key::MachineKey::load_or_create(
                    auth::machine_key::MACHINE_KEY_SERVICE,
                    auth::machine_key::MACHINE_KEY_ACCOUNT,
                ),
            })
            .as_ref()
    }

    /// 是否会有机器密钥；不创建密钥，只探测
    fn has_machine_key(&self) -> bool {
        match self.key.get() {
            Some(key) => key.is_some(),
            None => match &self.source {
                SecretKeySource::None => false,
                SecretKeySource::Fixed(_) => true,
                SecretKeySource::SystemKeyring => auth::system_keyring_available(),
            },
        }
    }

    fn load(&self, plugin: &PluginId) -> Result<BTreeMap<String, String>, HostError> {
        let path = self.path(plugin);
        check_secret_permissions(&path)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(err) => return Err(HostError::internal(format!("read secrets: {err}"))),
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|err| HostError::internal(format!("corrupt secrets file: {err}")))?;
        // 信封按结构判定，不能只看有没有 `v`：明文文件本身就是一个键值映射，
        // 插件合法使用名为 `v` 的密钥时会被误当成加密信封整份读不出来。
        if SealedSecrets::matches(&value) {
            return self.open_sealed(plugin, value);
        }
        serde_json::from_value(value)
            .map_err(|err| HostError::internal(format!("corrupt secrets file: {err}")))
    }

    fn open_sealed(
        &self,
        plugin: &PluginId,
        value: serde_json::Value,
    ) -> Result<BTreeMap<String, String>, HostError> {
        let envelope: SealedSecrets = serde_json::from_value(value)
            .map_err(|err| HostError::internal(format!("corrupt secrets file: {err}")))?;
        if envelope.v != SECRETS_VERSION {
            return Err(HostError::internal(format!(
                "unsupported secrets format version {}",
                envelope.v
            )));
        }
        let Some(key) = self.key() else {
            return Err(HostError::denied(
                "secrets are encrypted with the machine key, but the system keyring is unavailable; \
                 restore keyring access to read them",
            ));
        };
        let nonce = base64::engine::general_purpose::STANDARD
            .decode(&envelope.nonce)
            .ok()
            .and_then(|raw| <[u8; auth::machine_key::NONCE_LEN]>::try_from(raw).ok())
            .ok_or_else(|| HostError::internal("corrupt secrets file: bad nonce"))?;
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(&envelope.data)
            .map_err(|_| HostError::internal("corrupt secrets file: bad ciphertext"))?;
        let plaintext = key
            .open(
                plugin.as_str().as_bytes(),
                &auth::machine_key::Sealed { nonce, ciphertext },
            )
            .map_err(|err| HostError::internal(format!("decrypt secrets: {err}")))?;
        serde_json::from_slice(&plaintext)
            .map_err(|err| HostError::internal(format!("corrupt secrets file: {err}")))
    }

    fn save(&self, plugin: &PluginId, values: &BTreeMap<String, String>) -> Result<(), HostError> {
        std::fs::create_dir_all(self.dir(plugin))
            .map_err(|err| HostError::internal(format!("create secret dir: {err}")))?;
        let bytes = match self.key() {
            Some(key) => {
                let plaintext = serde_json::to_vec(values)
                    .map_err(|err| HostError::internal(format!("serialize secrets: {err}")))?;
                let sealed = key
                    .seal(plugin.as_str().as_bytes(), &plaintext)
                    .map_err(|err| HostError::internal(format!("encrypt secrets: {err}")))?;
                serde_json::to_vec_pretty(&SealedSecrets {
                    v: SECRETS_VERSION,
                    nonce: base64::engine::general_purpose::STANDARD.encode(sealed.nonce),
                    data: base64::engine::general_purpose::STANDARD.encode(&sealed.ciphertext),
                })
                .map_err(|err| HostError::internal(format!("serialize secrets: {err}")))?
            }
            None => serde_json::to_vec_pretty(values)
                .map_err(|err| HostError::internal(format!("serialize secrets: {err}")))?,
        };
        write_atomic_bytes(&self.path(plugin), &bytes, 0o600)
            .map_err(|err| HostError::internal(format!("write secrets: {err}")))
    }
}

impl SecretStore for SecretFileStore {
    fn get(&self, plugin: &PluginId, key: &str) -> Result<Option<SecretValue>, HostError> {
        validate_secret_key(key)?;
        match self.load(plugin)?.get(key) {
            Some(encoded) => decode(encoded)
                .map(SecretValue::from_bytes)
                .map(Some)
                .map_err(|err| HostError::internal(format!("corrupt secret: {err}"))),
            None => Ok(None),
        }
    }

    fn set(&self, plugin: &PluginId, key: &str, value: &SecretValue) -> Result<(), HostError> {
        validate_secret_key(key)?;
        if value.len() > MAX_SECRET_VALUE_BYTES {
            return Err(HostError::limit_exceeded(format!(
                "secret value is {} bytes; limit is {MAX_SECRET_VALUE_BYTES}",
                value.len()
            )));
        }
        let mut values = self.load(plugin)?;
        values.insert(key.to_owned(), encode(value.expose()));
        self.save(plugin, &values)
    }

    fn delete(&self, plugin: &PluginId, key: &str) -> Result<bool, HostError> {
        validate_secret_key(key)?;
        let mut values = self.load(plugin)?;
        let removed = values.remove(key).is_some();
        if removed {
            self.save(plugin, &values)?;
        }
        Ok(removed)
    }

    fn values(&self, plugin: &PluginId) -> Result<Vec<SecretValue>, HostError> {
        self.load(plugin)?
            .values()
            .map(|encoded| {
                decode(encoded)
                    .map(SecretValue::from_bytes)
                    .map_err(|err| HostError::internal(format!("corrupt secret: {err}")))
            })
            .collect()
    }

    fn protection(&self) -> SecretProtection {
        if self.has_machine_key() {
            SecretProtection::MachineKey
        } else {
            SecretProtection::FilePermissions
        }
    }
}

fn validate_secret_key(key: &str) -> Result<(), HostError> {
    if key.is_empty() {
        return Err(HostError::invalid_input("secret key is empty"));
    }
    if key.len() > MAX_SECRET_KEY_BYTES {
        return Err(HostError::limit_exceeded(format!(
            "secret key is {} bytes; limit is {MAX_SECRET_KEY_BYTES}",
            key.len()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn check_secret_permissions(path: &Path) -> Result<(), HostError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(HostError::internal(format!("stat secrets: {err}"))),
    };
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(HostError::denied(format!(
            "secrets file {} is readable by others (mode {mode:o}); run `chmod 600`",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // 与 unix 版本保持同一签名
fn check_secret_permissions(_path: &Path) -> Result<(), HostError> {
    Ok(())
}

/// 审计出口：按月 JSONL 追加
pub struct JsonlAuditSink {
    root: PathBuf,
    lock: Mutex<()>,
}

impl std::fmt::Debug for JsonlAuditSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlAuditSink")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl JsonlAuditSink {
    /// `root` 是 `db/audit/plugins`
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            lock: Mutex::new(()),
        }
    }
}

impl AuditSink for JsonlAuditSink {
    fn record(&self, event: &AuditEvent) {
        // 审计失败不能影响业务；写不进去只留一条 stderr 线索。
        let Ok(_guard) = self.lock.lock() else {
            return;
        };
        if std::fs::create_dir_all(&self.root).is_err() {
            return;
        }
        let path = self.root.join(format!("{}.jsonl", month_key(event.at)));
        let Ok(line) = serde_json::to_vec(event) else {
            return;
        };
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = file.write_all(&line);
            let _ = file.write_all(b"\n");
            let _ = file.sync_data();
        }
    }
}

// ---------------------------------------------------------------- 编解码

fn encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(ALPHABET[(triple >> 18) as usize & 0x3f]));
        out.push(char::from(ALPHABET[(triple >> 12) as usize & 0x3f]));
        out.push(if chunk.len() > 1 {
            char::from(ALPHABET[(triple >> 6) as usize & 0x3f])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(ALPHABET[triple as usize & 0x3f])
        } else {
            '='
        });
    }
    out
}

// 每段 6 位，右移后仍在 u8 范围内；显式掩码加注释比逐字节 try_from 更直白
#[allow(clippy::cast_possible_truncation)]
fn decode(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err("base64 length must be a multiple of 4".to_owned());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut index = 0;
    while index < bytes.len() {
        let chunk = &bytes[index..index + 4];
        let mut values = [0_u32; 4];
        let mut padding = 0;
        for (slot, byte) in chunk.iter().enumerate() {
            values[slot] = if *byte == b'=' {
                if slot < 2 {
                    return Err("base64 padding is only allowed at the end".to_owned());
                }
                padding += 1;
                0
            } else {
                if padding > 0 {
                    return Err("base64 data after padding".to_owned());
                }
                base64_value(*byte)?
            };
        }
        if padding > 0 && index + 4 < bytes.len() {
            return Err("base64 data after padding".to_owned());
        }
        let triple = (values[0] << 18) | (values[1] << 12) | (values[2] << 6) | values[3];
        out.push(((triple >> 16) & 0xFF) as u8);
        if padding < 2 {
            out.push(((triple >> 8) & 0xFF) as u8);
        }
        if padding == 0 {
            out.push((triple & 0xFF) as u8);
        }
        index += 4;
    }
    Ok(out)
}

fn base64_value(byte: u8) -> Result<u32, String> {
    match byte {
        b'A'..=b'Z' => Ok(u32::from(byte - b'A')),
        b'a'..=b'z' => Ok(u32::from(byte - b'a') + 26),
        b'0'..=b'9' => Ok(u32::from(byte - b'0') + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        other => Err(format!("invalid base64 byte {other}")),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// 原子写：临时文件 → fsync → rename；权限在创建临时文件时即设定
fn write_atomic_bytes(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    // 临时名带 PID 与序号：键值/密钥写入不持跨进程锁，两个进程写同一路径时
    // 固定 `.tmp` 名会互相截断。rename 仍是原子的，先到者胜。
    let tmp = path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------- 时间

fn format_rfc3339(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let rest = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

fn parse_rfc3339(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = text.get(range)?;
        if slice.bytes().all(|byte| byte.is_ascii_digit()) {
            slice.parse().ok()
        } else {
            None
        }
    };
    let year = digits(0..4)?;
    let month = digits(5..7)?;
    let day = digits(8..10)?;
    let hour = digits(11..13)?;
    let minute = digits(14..16)?;
    let second = digits(17..19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn month_key(unix: i64) -> String {
    let (year, month, _) = civil_from_days(unix.div_euclid(86_400));
    format!("{year:04}-{month:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::plugin::error::PluginErrorCode;

    fn sample(id: &str) -> PluginRecord {
        let manifest = Manifest::from_json(
            format!(
                r#"{{"schema":1,"id":"{id}","name":"Demo","version":"0.1.0","api":1,
                   "entry":{{"type":"process","cmd":"bin/demo"}},
                   "capabilities":["events","storage:kv"],"license":"MIT"}}"#
            )
            .as_bytes(),
        )
        .expect("manifest");
        PluginRecord {
            id: PluginId::parse(id).expect("id"),
            version: "0.1.0".to_owned(),
            api: 1,
            scope: PluginScope::Machine,
            server: None,
            enabled: true,
            approved: manifest.capabilities.clone(),
            manifest: manifest.raw,
            artifact_dir: PathBuf::from("/tmp/plugins").join(id).join("0.1.0"),
            installed_at: 1_700_000_000,
            updated_at: 1_700_000_100,
            last_state: PluginState::Stopped,
            last_error: None,
            previous_version: None,
            source: None,
            sha256: None,
            blocked: false,
            quarantined: false,
        }
    }

    #[test]
    fn record_roundtrip_keeps_fields_and_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PluginFileStore::open(dir.path()).expect("open");
        store.put(&sample("beta")).expect("put beta");
        store.put(&sample("alpha")).expect("put alpha");
        let records = store.list().expect("list");
        assert_eq!(
            records
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        let alpha = store
            .get(&PluginId::parse("alpha").expect("id"))
            .expect("get");
        assert_eq!(alpha.version, "0.1.0");
        assert!(alpha.enabled);
        assert!(alpha.approved.contains(&Capability::StorageKv));
    }

    #[test]
    fn corrupt_record_is_reported_not_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PluginFileStore::open(dir.path()).expect("open");
        store.put(&sample("demo")).expect("put");
        std::fs::write(dir.path().join("demo.json"), b"{not json").expect("write");
        let err = store
            .get(&PluginId::parse("demo").expect("id"))
            .expect_err("corrupt");
        assert!(matches!(err, RepositoryError::Corrupt(_)));
    }

    #[test]
    fn kv_roundtrip_and_limits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = KvFileStore::new(dir.path().to_path_buf());
        let plugin = PluginId::parse("demo").expect("id");
        assert!(store.get(&plugin, "k").expect("get").is_none());
        store.set(&plugin, "k", b"value").expect("set");
        assert_eq!(
            store.get(&plugin, "k").expect("get"),
            Some(b"value".to_vec())
        );
        store.set(&plugin, "k2", b"other").expect("set");
        assert_eq!(
            store.list(&plugin, "k", None).expect("list"),
            ["k".to_owned(), "k2".to_owned()]
        );
        // `after` 是上一页最后一个键（不含），用于续页
        assert_eq!(
            store.list(&plugin, "k", Some("k")).expect("list after"),
            ["k2".to_owned()]
        );
        assert!(store.delete(&plugin, "k").expect("delete"));
        assert!(!store.delete(&plugin, "k").expect("delete"));
        let too_long = vec![0_u8; MAX_KV_VALUE_BYTES + 1];
        assert!(store.set(&plugin, "big", &too_long).is_err());
    }

    #[test]
    fn secrets_roundtrip_and_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SecretFileStore::new(dir.path().to_path_buf());
        let plugin = PluginId::parse("demo").expect("id");
        store
            .set(&plugin, "token", &SecretValue::from_text("hunter2"))
            .expect("set");
        assert_eq!(
            store
                .get(&plugin, "token")
                .expect("get")
                .expect("value")
                .expose(),
            b"hunter2"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("demo/secrets.json"))
                .expect("stat")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(store.delete(&plugin, "token").expect("delete"));
        assert_eq!(store.protection(), SecretProtection::FilePermissions);
    }

    #[test]
    fn plaintext_secret_named_v_is_not_mistaken_for_an_envelope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SecretFileStore::new(dir.path().to_path_buf());
        let plugin = PluginId::parse("demo").expect("id");
        for (key, value) in [("v", "plain"), ("nonce", "n"), ("data", "d")] {
            store
                .set(&plugin, key, &SecretValue::from_text(value))
                .expect("set");
        }
        for (key, value) in [("v", "plain"), ("nonce", "n"), ("data", "d")] {
            assert_eq!(
                store
                    .get(&plugin, key)
                    .expect("get")
                    .expect("value")
                    .expose(),
                value.as_bytes()
            );
        }
    }

    #[test]
    fn encrypted_secrets_do_not_contain_plaintext() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SecretFileStore::with_machine_key(dir.path().to_path_buf(), test_key());
        let plugin = PluginId::parse("demo").expect("id");
        store
            .set(&plugin, "token", &SecretValue::from_text("hunter2"))
            .expect("set");
        assert_eq!(
            store
                .get(&plugin, "token")
                .expect("get")
                .expect("value")
                .expose(),
            b"hunter2"
        );
        assert_eq!(store.protection(), SecretProtection::MachineKey);
        let raw = std::fs::read(dir.path().join("demo/secrets.json")).expect("read raw");
        let text = String::from_utf8_lossy(&raw);
        assert!(!text.contains("hunter2"), "plaintext leaked: {text}");
        let value: serde_json::Value = serde_json::from_slice(&raw).expect("json");
        assert_eq!(value["v"], 1);
        assert!(value["nonce"].is_string());
        assert!(value["data"].is_string());
    }

    #[test]
    fn secret_values_lists_every_value_for_redaction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin = PluginId::parse("demo").expect("id");
        let store = SecretFileStore::with_machine_key(dir.path().to_path_buf(), test_key());
        store
            .set(&plugin, "a", &SecretValue::from_text("first-secret"))
            .expect("set");
        store
            .set(&plugin, "b", &SecretValue::from_text("second-secret"))
            .expect("set");
        let mut values: Vec<String> = store
            .values(&plugin)
            .expect("values")
            .into_iter()
            .map(|value| String::from_utf8(value.expose().to_vec()).expect("utf8"))
            .collect();
        values.sort();
        assert_eq!(values, ["first-secret", "second-secret"]);
    }

    #[test]
    fn encrypted_store_reads_existing_plaintext_and_migrates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin = PluginId::parse("demo").expect("id");
        let legacy = SecretFileStore::new(dir.path().to_path_buf());
        legacy
            .set(&plugin, "old", &SecretValue::from_text("legacy"))
            .expect("set");

        let store = SecretFileStore::with_machine_key(dir.path().to_path_buf(), test_key());
        assert_eq!(
            store
                .get(&plugin, "old")
                .expect("get")
                .expect("value")
                .expose(),
            b"legacy"
        );
        store
            .set(&plugin, "new", &SecretValue::from_text("fresh"))
            .expect("set");
        let text =
            String::from_utf8(std::fs::read(dir.path().join("demo/secrets.json")).expect("read"))
                .expect("utf8");
        assert!(!text.contains("legacy"), "migration kept plaintext: {text}");
        assert_eq!(
            store
                .get(&plugin, "old")
                .expect("get")
                .expect("value")
                .expose(),
            b"legacy"
        );
    }

    #[test]
    fn encrypted_secrets_need_the_machine_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin = PluginId::parse("demo").expect("id");
        SecretFileStore::with_machine_key(dir.path().to_path_buf(), test_key())
            .set(&plugin, "token", &SecretValue::from_text("hunter2"))
            .expect("set");
        let err = SecretFileStore::new(dir.path().to_path_buf())
            .get(&plugin, "token")
            .expect_err("must not silently degrade");
        assert_eq!(err.code, PluginErrorCode::Denied);
    }

    #[test]
    fn tampered_secrets_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin = PluginId::parse("demo").expect("id");
        let store = SecretFileStore::with_machine_key(dir.path().to_path_buf(), test_key());
        store
            .set(&plugin, "token", &SecretValue::from_text("hunter2"))
            .expect("set");
        let path = dir.path().join("demo/secrets.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
        let data = value["data"].as_str().expect("data").to_owned();
        let mut raw = base64::engine::general_purpose::STANDARD
            .decode(data)
            .expect("decode");
        raw[0] ^= 0xff;
        value["data"] =
            serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(raw));
        std::fs::write(&path, serde_json::to_vec(&value).expect("json")).expect("write");
        let err = store.get(&plugin, "token").expect_err("tamper must fail");
        assert_eq!(err.code, PluginErrorCode::Internal);
    }

    #[test]
    fn encrypted_secrets_are_bound_to_the_plugin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let demo = PluginId::parse("demo").expect("id");
        let other = PluginId::parse("other").expect("id");
        let store = SecretFileStore::with_machine_key(dir.path().to_path_buf(), test_key());
        store
            .set(&demo, "token", &SecretValue::from_text("hunter2"))
            .expect("set");
        std::fs::create_dir_all(dir.path().join("other")).expect("mkdir");
        std::fs::copy(
            dir.path().join("demo/secrets.json"),
            dir.path().join("other/secrets.json"),
        )
        .expect("copy");
        let err = store
            .get(&other, "token")
            .expect_err("aad must bind to plugin");
        assert_eq!(err.code, PluginErrorCode::Internal);
    }

    fn test_key() -> auth::machine_key::MachineKey {
        auth::machine_key::MachineKey::from_bytes(&[3_u8; auth::machine_key::KEY_LEN]).expect("key")
    }

    #[test]
    fn plugin_config_roundtrip_is_private() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PluginConfigFileStore::new(dir.path().to_path_buf());
        let plugin = PluginId::parse("demo").expect("id");
        assert!(store.get(&plugin).expect("get").is_none());
        let config = StoredConfig {
            revision: 3,
            values: serde_json::json!({ "interval_secs": 60, "token": "s3cret" }),
        };
        store.put(&plugin, &config).expect("put");
        let stored = store.get(&plugin).expect("get").expect("stored");
        assert_eq!(stored.revision, 3);
        assert_eq!(stored.values, config.values);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("demo/config.json"))
                .expect("stat")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn audit_appends_one_line_per_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = JsonlAuditSink::new(dir.path().to_path_buf());
        let event = AuditEvent {
            at: 1_700_000_000,
            plugin: "demo".to_owned(),
            action: "capability:events".to_owned(),
            capability: Some(Capability::Events),
            result: port::plugin::AuditResult::Allowed,
            detail: None,
        };
        sink.record(&event);
        sink.record(&event);
        let path = dir.path().join("2023-11.jsonl");
        let text = std::fs::read_to_string(path).expect("read");
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("\"plugin\":\"demo\""));
    }

    #[test]
    fn rfc3339_roundtrips_known_instants() {
        for instant in [0, 951_782_400, 1_700_000_000, 1_800_000_000] {
            assert_eq!(parse_rfc3339(&format_rfc3339(instant)), Some(instant));
        }
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
    }
}
