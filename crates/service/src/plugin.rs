//! 插件业务编排：安装记录、授权、启停与宿主能力实现
//!
//! 本模块只做编排：状态迁移、能力交集与 manifest / descriptor 校验都在
//! `domain::plugin`；进程与帧在 `crates/plugin`；文件后端在 `storage`。
//!
//! # 方向
//!
//! [`HostApiImpl`] 把宿主需要的业务能力实现成 [`port::plugin`] 的窄 trait。
//! 依赖全部是 `Arc<dyn ...>` 或同层 service，不反向依赖宿主运行时。

use crate::fs_error::FsError;
use crate::lifecycle::ServerLifecycleService;
use crate::props::ServerPropsService;
use crate::registry::ServerRegistry;
use domain::plugin::capability::{self, Capability};
use domain::plugin::descriptor::Descriptor;
use domain::plugin::manifest::Manifest;
use domain::plugin::state::{PluginEvent, PluginState};
use domain::plugin::{PluginId, PluginScope};
use domain::server::artifact::Checksum;
use domain::server::layout::console_log_path;
use domain::server::meta::ServerName;
use domain::server::status::ServerStatus;
use port::plugin::{
    ArtifactOrigin, ArtifactSource, ArtifactVerifier, AuditEvent, AuditResult, AuditSink,
    BackupOps, ConfigOps, ConsoleIo, ConsoleLine, ConsolePage, ContentOps, FileOps, HostError,
    JavaOps, KvStore, MAX_CONSOLE_LINE_BYTES, MAX_CONSOLE_LINES, NetPolicy, PluginConfigStore,
    PluginRecord, PluginRuntimeStatus, PluginStore, PluginSupervisor, SecretStore, ServerLifecycle,
    ServerRead, ServerStatusView, ServerSummary, SessionInfo, StoredConfig,
};
use port::{ErrorCategory, RepositoryError};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// 清理安装临时目录时使用的固定前缀
const TEMP_PREFIX: &str = ".install-";

/// 插件业务失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PluginError {
    /// 记录不存在
    #[error("plugin not found: {0}")]
    NotFound(String),
    /// 记录或产物已经存在
    #[error("plugin already exists: {0}")]
    AlreadyExists(String),
    /// 当前状态不允许该操作
    #[error("{0}")]
    Conflict(String),
    /// 输入不合法（能力名、来源、版本等）
    #[error("{0}")]
    Usage(String),
    /// 能力未授予
    #[error("{0}")]
    Denied(String),
    /// 宿主未运行 / 崩溃
    #[error("{0}")]
    Host(String),
    /// 存储失败
    #[error(transparent)]
    Repo(#[from] RepositoryError),
    /// 文件系统失败（无具体路径的创建 / 删除）
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// 文件系统失败
    #[error(transparent)]
    Fs(#[from] FsError),
    /// 宿主能力返回的错误
    #[error(transparent)]
    HostError(#[from] HostError),
}

impl PluginError {
    /// 失败等级
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::NotFound(_) => ErrorCategory::NotFound,
            Self::AlreadyExists(_) | Self::Conflict(_) => ErrorCategory::Conflict,
            Self::Usage(_) => ErrorCategory::Usage,
            Self::Denied(_) => ErrorCategory::Unauthorized,
            Self::Host(_) | Self::Io(_) => ErrorCategory::Backend,
            Self::HostError(err) => err.category,
            Self::Repo(err) => err.category(),
            Self::Fs(err) => err.category(),
        }
    }

    fn from_repo(err: RepositoryError, id: &PluginId) -> Self {
        match err {
            RepositoryError::NotFound(_) => Self::NotFound(id.to_string()),
            other => Self::Repo(other),
        }
    }
}

/// 安装结果
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    /// 写入的安装记录
    pub record: PluginRecord,
    /// 安装后需要用户确认的能力
    pub pending: BTreeSet<Capability>,
}

/// 启停后的状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// 会话已建立
    Started(SessionInfo),
    /// 宿主未连接，仅更新了期望状态
    Offline,
}

/// 插件业务服务
pub struct PluginService {
    store: Arc<dyn PluginStore>,
    supervisor: Option<Arc<dyn PluginSupervisor>>,
    source: Option<Arc<dyn ArtifactSource>>,
    verifier: Option<Arc<dyn ArtifactVerifier>>,
    audit: Arc<dyn AuditSink>,
    /// 配置读写（与宿主控制通道共用同一份实现）
    config: Arc<PluginConfig>,
    /// `{data_dir}/mcsup/plugins`：不可变产物根
    plugins_root: PathBuf,
    /// `{data_dir}/mcsup/plugin-data`：插件可写目录
    plugin_data: PathBuf,
    /// `{data_dir}/mcsup/logs/plugins`
    log_root: PathBuf,
}

impl std::fmt::Debug for PluginService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginService")
            .field("plugins_root", &self.plugins_root)
            .field("supervisor", &self.supervisor.is_some())
            .finish_non_exhaustive()
    }
}

impl PluginService {
    /// 构造服务
    ///
    /// 参数就是装配清单：存储、宿主、来源、校验、审计、三类根目录。拆成多个
    /// 结构体只会把同一份清单换个地方摆放。
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        store: Arc<dyn PluginStore>,
        config_store: Arc<dyn PluginConfigStore>,
        supervisor: Option<Arc<dyn PluginSupervisor>>,
        source: Option<Arc<dyn ArtifactSource>>,
        verifier: Option<Arc<dyn ArtifactVerifier>>,
        audit: Arc<dyn AuditSink>,
        plugins_root: PathBuf,
        plugin_data: PathBuf,
        log_root: PathBuf,
    ) -> Self {
        let config = Arc::new(PluginConfig::new(
            Arc::clone(&store),
            config_store,
            plugins_root.clone(),
            supervisor.clone(),
            Arc::clone(&audit),
        ));
        Self {
            store,
            supervisor,
            source,
            verifier,
            audit,
            config,
            plugins_root,
            plugin_data,
            log_root,
        }
    }

    /// 配置读写句柄（宿主控制通道与 CLI 共用）
    #[must_use]
    pub fn config(&self) -> Arc<dyn ConfigOps> {
        self.config.clone()
    }

    /// 宿主是否可达
    #[must_use]
    pub fn host_available(&self) -> bool {
        self.supervisor.is_some()
    }

    /// 已安装版本的 descriptor（`ops` 命令与调用的参数校验共用）
    ///
    /// # Errors
    ///
    /// 记录不存在或 `descriptor.json` 非法时返回 [`PluginError`]
    pub fn descriptor(
        &self,
        id: &PluginId,
    ) -> Result<domain::plugin::descriptor::ValidatedDescriptor, PluginError> {
        let record = self.record(id)?;
        let path = record.artifact_dir.join("descriptor.json");
        let bytes = std::fs::read(&path).map_err(|err| PluginError::Fs(FsError::io(&path, err)))?;
        Descriptor::from_json(&bytes, &record.id)
            .map_err(|err| PluginError::Usage(format!("invalid descriptor: {err}")))
    }

    /// 读取插件日志
    ///
    /// # Errors
    ///
    /// 宿主未连接时返回 [`PluginError::Host`]
    pub fn logs(
        &self,
        id: &PluginId,
        offset: Option<u64>,
        max: usize,
    ) -> Result<Value, PluginError> {
        Ok(self.supervisor()?.logs(id, offset, max)?)
    }

    /// 安装记录根
    #[must_use]
    pub fn plugins_root(&self) -> &Path {
        &self.plugins_root
    }

    /// 插件数据根
    #[must_use]
    pub fn plugin_data(&self) -> &Path {
        &self.plugin_data
    }

    /// 全部记录，按 id 升序
    ///
    /// # Errors
    ///
    /// 记录读取失败时返回 [`PluginError`]
    pub fn list(&self) -> Result<Vec<PluginRecord>, PluginError> {
        Ok(self.store.list()?)
    }

    /// 单条记录
    ///
    /// # Errors
    ///
    /// 不存在返回 [`PluginError::NotFound`]
    pub fn record(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        self.store
            .get(id)
            .map_err(|err| PluginError::from_repo(err, id))
    }

    /// 实时状态；宿主未连接时回退到记录里的离线状态
    ///
    /// # Errors
    ///
    /// 记录不存在时返回 [`PluginError::NotFound`]
    pub fn status(&self, id: &PluginId) -> Result<PluginRuntimeStatus, PluginError> {
        if let Some(supervisor) = &self.supervisor {
            match supervisor.status(id) {
                Ok(status) => return Ok(status),
                // 宿主未运行：回退到记录，不把「宿主不在」当成「插件不存在」
                Err(err) if err.code == port::plugin::ErrorCode::Unavailable => {}
                Err(err) => return Err(err.into()),
            }
        }
        let record = self.record(id)?;
        Ok(PluginRuntimeStatus {
            id: id.clone(),
            state: offline_state(&record),
            session: None,
            last_error: record.last_error,
        })
    }

    /// 读取配置（密钥字段打码）
    ///
    /// # Errors
    ///
    /// 记录不存在或 descriptor 非法时返回 [`PluginError`]
    pub fn config_get(&self, id: &PluginId) -> Result<Value, PluginError> {
        Ok(self.config.get(id)?)
    }

    /// 写入配置：顶层合并、整体校验、落盘并下发
    ///
    /// # Errors
    ///
    /// schema 校验失败返回 [`PluginError::Usage`]；插件应用失败时保留上一版本
    pub fn config_set(&self, id: &PluginId, values: &Value) -> Result<Value, PluginError> {
        Ok(self.config.set(id, values)?)
    }

    /// 本机拉黑：立即停用且不再自动启动
    ///
    /// # Errors
    ///
    /// 记录不存在时返回 [`PluginError::NotFound`]
    pub fn block(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        self.set_flag(id, Flag::Blocked, true)
    }

    /// 解除拉黑；不自动启用
    ///
    /// # Errors
    ///
    /// 记录不存在时返回 [`PluginError::NotFound`]
    pub fn unblock(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        self.set_flag(id, Flag::Blocked, false)
    }

    /// 隔离：保留产物但禁止运行
    ///
    /// # Errors
    ///
    /// 记录不存在时返回 [`PluginError::NotFound`]
    pub fn quarantine(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        self.set_flag(id, Flag::Quarantined, true)
    }

    /// 解除隔离；不自动启用
    ///
    /// # Errors
    ///
    /// 记录不存在时返回 [`PluginError::NotFound`]
    pub fn release(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        self.set_flag(id, Flag::Quarantined, false)
    }

    fn set_flag(&self, id: &PluginId, flag: Flag, on: bool) -> Result<PluginRecord, PluginError> {
        let mut record = self.record(id)?;
        let already = match flag {
            Flag::Blocked => record.blocked,
            Flag::Quarantined => record.quarantined,
        };
        if already != on {
            if on {
                // 拉黑 / 隔离立即生效：先停进程，再改记录。
                if let Some(supervisor) = &self.supervisor {
                    match supervisor.status(id) {
                        Ok(status) if status.session.is_some() => {
                            supervisor.stop(id, Duration::from_secs(10))?;
                        }
                        Ok(_) => {}
                        Err(err) if err.code == port::plugin::ErrorCode::Unavailable => {}
                        Err(err) => return Err(err.into()),
                    }
                }
                record.enabled = false;
                record.last_state = PluginState::Disabled;
            }
            match flag {
                Flag::Blocked => record.blocked = on,
                Flag::Quarantined => record.quarantined = on,
            }
            record.updated_at = now_secs();
            self.store.put(&record)?;
            self.audit.record(&AuditEvent {
                at: now_secs(),
                plugin: id.to_string(),
                action: match (flag, on) {
                    (Flag::Blocked, true) => "security:block",
                    (Flag::Blocked, false) => "security:unblock",
                    (Flag::Quarantined, true) => "security:quarantine",
                    (Flag::Quarantined, false) => "security:release",
                }
                .to_owned(),
                capability: None,
                result: AuditResult::Allowed,
                detail: None,
            });
        }
        Ok(record)
    }

    /// 启用：只改期望状态，不拉起进程
    ///
    /// # Errors
    ///
    /// 已启用、仍有待确认能力、被拉黑或隔离时返回 [`PluginError::Conflict`]
    pub fn enable(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        let mut record = self.record(id)?;
        if record.blocked {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is blocked; run `mcsup plugin unblock {id}` first"
            )));
        }
        if record.quarantined {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is quarantined; run `mcsup plugin release {id}` first"
            )));
        }
        if record.enabled {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is already enabled"
            )));
        }
        if record.needs_approval() {
            let pending = pending_capabilities(&record);
            return Err(PluginError::Conflict(format!(
                "plugin {id} still needs approval: {pending}; run `mcsup plugin grant {id} <capability>`"
            )));
        }
        // 待确认状态先走 `Approved`，其余从离线状态直接 `Enabled`。
        let from = record.last_state;
        let next = if from == PluginState::PendingApproval {
            PluginState::PendingApproval
                .transition(PluginEvent::Approved)
                .and_then(|state| state.transition(PluginEvent::Enabled))
        } else {
            from.transition(PluginEvent::Enabled)
        }
        .map_err(|err| PluginError::Conflict(err.to_string()))?;
        record.enabled = true;
        record.last_state = next;
        record.last_error = None;
        record.updated_at = now_secs();
        self.store.put(&record)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "lifecycle:enable".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: None,
        });
        Ok(record)
    }

    /// 停用：先停进程，再改期望状态
    ///
    /// # Errors
    ///
    /// 记录不存在或停止失败时返回 [`PluginError`]
    pub fn disable(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        let mut record = self.record(id)?;
        if let Some(supervisor) = &self.supervisor {
            match supervisor.status(id) {
                Ok(status) if status.session.is_some() => {
                    supervisor.stop(id, Duration::from_secs(10))?;
                }
                Ok(_) => {}
                Err(err) if err.code == port::plugin::ErrorCode::Unavailable => {}
                Err(err) => return Err(err.into()),
            }
        }
        record.enabled = false;
        record.last_state = record
            .last_state
            .transition(PluginEvent::Disabled)
            .unwrap_or(PluginState::Disabled);
        record.updated_at = now_secs();
        self.store.put(&record)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "lifecycle:disable".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: None,
        });
        Ok(record)
    }

    /// 追加授权
    ///
    /// # Errors
    ///
    /// 能力不在 manifest 内返回 [`PluginError::Usage`]
    pub fn grant(
        &self,
        id: &PluginId,
        capability: Capability,
    ) -> Result<PluginRecord, PluginError> {
        let mut record = self.record(id)?;
        let declared = record.declared_capabilities();
        if !declared.contains(&capability) {
            return Err(PluginError::Usage(format!(
                "capability {capability} is not declared by plugin {id}"
            )));
        }
        if !capability.is_open() {
            return Err(PluginError::Usage(format!(
                "capability {capability} is not available in this version"
            )));
        }
        record.approved.insert(capability);
        if record.needs_approval() {
            // 仍有待确认项
        } else if record.last_state == PluginState::PendingApproval {
            record.last_state = PluginState::Disabled;
        }
        record.updated_at = now_secs();
        self.store.put(&record)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "capability:grant".to_owned(),
            capability: Some(capability),
            result: AuditResult::Allowed,
            detail: None,
        });
        Ok(record)
    }

    /// 撤销授权：立即生效，不重启进程
    ///
    /// # Errors
    ///
    /// 能力尚未授予返回 [`PluginError::NotFound`]
    pub fn revoke(
        &self,
        id: &PluginId,
        capability: Capability,
    ) -> Result<PluginRecord, PluginError> {
        let mut record = self.record(id)?;
        if !record.approved.remove(&capability) {
            return Err(PluginError::NotFound(format!(
                "capability {capability} is not granted to plugin {id}"
            )));
        }
        if let Some(supervisor) = &self.supervisor {
            // 会话内的授予集合同步收缩；宿主不在时忽略（下次握手自然按记录计算）。
            match supervisor.revoke(id, capability) {
                Ok(()) => {}
                Err(err) if err.code == port::plugin::ErrorCode::Unavailable => {}
                Err(err) => return Err(err.into()),
            }
        }
        // 撤销后若重新出现待确认能力，状态回到 `pending_approval`。
        if record.needs_approval() {
            record.last_state = PluginState::PendingApproval;
        }
        record.updated_at = now_secs();
        self.store.put(&record)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "capability:revoke".to_owned(),
            capability: Some(capability),
            result: AuditResult::Denied,
            detail: None,
        });
        Ok(record)
    }

    /// 拉起插件；宿主未连接时返回 [`PluginError::Host`]
    ///
    /// # Errors
    ///
    /// 未启用、握手失败时返回 [`PluginError`]
    pub fn start(&self, id: &PluginId) -> Result<SessionInfo, PluginError> {
        let record = self.record(id)?;
        if record.blocked {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is blocked; run `mcsup plugin unblock {id}` first"
            )));
        }
        if record.quarantined {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is quarantined; run `mcsup plugin release {id}` first"
            )));
        }
        if !record.enabled {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is not enabled; run `mcsup plugin enable {id}`"
            )));
        }
        let supervisor = self.supervisor()?;
        Ok(supervisor.start(id)?)
    }

    /// 停止插件
    ///
    /// # Errors
    ///
    /// 未运行或宿主未连接时返回 [`PluginError`]
    pub fn stop(&self, id: &PluginId, grace: Duration) -> Result<(), PluginError> {
        Ok(self.supervisor()?.stop(id, grace)?)
    }

    /// 重启插件
    ///
    /// # Errors
    ///
    /// 同 [`Self::start`]
    pub fn restart(&self, id: &PluginId, grace: Duration) -> Result<SessionInfo, PluginError> {
        if let Some(supervisor) = &self.supervisor
            && let Ok(status) = supervisor.status(id)
            && status.session.is_some()
        {
            supervisor.stop(id, grace)?;
        }
        self.start(id)
    }

    /// 调用插件 op
    ///
    /// # Errors
    ///
    /// 能力未授予、超时、宿主未连接时返回 [`PluginError`]
    pub fn call(&self, id: &PluginId, op: &str, input: Value) -> Result<Value, PluginError> {
        Ok(self.supervisor()?.call(id, op, input)?)
    }

    /// 订阅插件 stream
    ///
    /// # Errors
    ///
    /// 同 [`Self::call`]
    pub fn stream(
        &self,
        id: &PluginId,
        op: &str,
        input: Value,
        sink: &mut dyn port::plugin::StreamSink,
    ) -> Result<(), PluginError> {
        Ok(self.supervisor()?.stream(id, op, input, sink)?)
    }

    /// 安装插件
    ///
    /// 流水线：拉取 → 校验 → 解包（由来源实现）→ manifest / descriptor →
    /// 原子移动到 `plugins/<id>/<version>` → 写记录。安装期间不启动进程。
    ///
    /// # Errors
    ///
    /// 来源不可用、校验失败、manifest / descriptor 非法、同版本已存在时返回
    /// [`PluginError`]
    pub fn install(
        &self,
        origin: &ArtifactOrigin,
        expected_sha256: Option<&Checksum>,
        allow_unsigned: bool,
    ) -> Result<InstallOutcome, PluginError> {
        let source = self.source.as_ref().ok_or_else(|| {
            PluginError::Usage("no plugin artifact source is configured".to_owned())
        })?;
        let source = Arc::clone(source);
        std::fs::create_dir_all(&self.plugins_root)?;
        let temp = self.plugins_root.join(format!(
            "{TEMP_PREFIX}{}-{}",
            std::process::id(),
            now_secs()
        ));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp)?;
        let result = self.install_into(origin, expected_sha256, allow_unsigned, &source, &temp);
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&temp);
        }
        result
    }

    fn install_into(
        &self,
        origin: &ArtifactOrigin,
        expected_sha256: Option<&Checksum>,
        allow_unsigned: bool,
        source: &Arc<dyn ArtifactSource>,
        temp: &Path,
    ) -> Result<InstallOutcome, PluginError> {
        let fetched = source.fetch(origin, temp)?;
        let artifact = if let Some(verifier) = &self.verifier {
            verifier.verify(&fetched.root, expected_sha256, allow_unsigned)?
        } else {
            if !fetched.trusted && !allow_unsigned {
                return Err(PluginError::Denied(
                    "artifact is not signed; pass --allow-unsigned to install unsigned local builds"
                        .to_owned(),
                ));
            }
            fetched
        };
        let manifest = Manifest::from_json(
            &serde_json::to_vec(&artifact.manifest)
                .map_err(|err| PluginError::Usage(format!("serialize manifest: {err}")))?,
        )
        .map_err(|err| PluginError::Usage(format!("invalid manifest: {err}")))?;
        if artifact.manifest.id != manifest.id.as_str() {
            return Err(PluginError::Usage(format!(
                "manifest id {} does not match artifact",
                artifact.manifest.id
            )));
        }
        Descriptor::from_json(&artifact.descriptor, &manifest.id)
            .map_err(|err| PluginError::Usage(format!("invalid descriptor: {err}")))?;
        let id = manifest.id.clone();
        let version = manifest.raw.version.clone();
        let target = self.plugins_root.join(id.as_str()).join(&version);
        if target.exists() {
            return Err(PluginError::AlreadyExists(format!(
                "{id} {version} is already installed"
            )));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if manifest.scope == PluginScope::Server {
            return Err(PluginError::Usage(
                "server-scoped plugins require `--server <name>`".to_owned(),
            ));
        }
        // 同文件系统内 rename 才是原子的：临时目录建在 `plugins_root` 下。
        std::fs::rename(&artifact.root, &target)
            .map_err(|err| PluginError::Fs(FsError::io(&artifact.root, err)))?;
        let _ = std::fs::remove_dir_all(temp);
        // 升级：只保留新版本仍声明、且此前已确认的能力；低风险照常自动通过。
        let previous = self.store.get(&id).ok();
        let mut approved: BTreeSet<Capability> = manifest
            .capabilities
            .iter()
            .copied()
            .filter(|cap| cap.default_policy() == capability::DefaultPolicy::Allow)
            .collect();
        if let Some(old) = &previous {
            approved.extend(
                old.approved
                    .iter()
                    .copied()
                    .filter(|cap| manifest.capabilities.contains(cap)),
            );
        }
        let pending: BTreeSet<Capability> = manifest
            .capabilities
            .iter()
            .copied()
            .filter(|cap| !approved.contains(cap))
            .collect();
        let now = now_secs();
        let record = PluginRecord {
            id: id.clone(),
            version: version.clone(),
            api: manifest.raw.api,
            scope: manifest.scope,
            server: None,
            enabled: false,
            approved,
            manifest: manifest.raw.clone(),
            artifact_dir: target,
            installed_at: now,
            updated_at: now,
            last_state: PluginState::after_install(!pending.is_empty()),
            last_error: None,
            previous_version: previous.as_ref().map(|record| record.version.clone()),
            source: Some(origin_text(origin)),
            sha256: Some(artifact.sha256),
            signed: artifact.signed,
            blocked: false,
            quarantined: false,
        };
        self.store.put(&record)?;
        self.audit.record(&AuditEvent {
            at: now,
            plugin: id.to_string(),
            action: "install".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: Some(format!("version {version}")),
        });
        Ok(InstallOutcome { record, pending })
    }

    /// 回滚到上一版本
    ///
    /// # Errors
    ///
    /// 没有上一版本、上一版本产物缺失时返回 [`PluginError`]
    pub fn rollback(&self, id: &PluginId) -> Result<PluginRecord, PluginError> {
        let mut record = self.record(id)?;
        let previous = record
            .previous_version
            .clone()
            .ok_or_else(|| PluginError::Conflict(format!("plugin {id} has no previous version")))?;
        let artifact = self.plugins_root.join(id.as_str()).join(&previous);
        if !artifact.is_dir() {
            return Err(PluginError::Conflict(format!(
                "artifact for {id} {previous} is missing"
            )));
        }
        let was_running = self
            .supervisor
            .as_ref()
            .and_then(|supervisor| supervisor.status(id).ok())
            .is_some_and(|status| status.session.is_some());
        if was_running && let Some(supervisor) = &self.supervisor {
            supervisor.stop(id, Duration::from_secs(10))?;
        }
        let current = record.version.clone();
        record.previous_version = Some(current);
        record.version = previous;
        record.artifact_dir = artifact;
        record.last_state = PluginState::Stopped;
        record.last_error = None;
        record.updated_at = now_secs();
        self.store.put(&record)?;
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "rollback".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: Some(format!("active version {}", record.version)),
        });
        if was_running && record.enabled {
            self.start(id)?;
        }
        Ok(record)
    }

    /// 卸载：要求已停用且进程退出
    ///
    /// `purge` 同时删除 `plugin-data/<id>` 与日志；默认保留数据。
    ///
    /// # Errors
    ///
    /// 仍启用或进程仍在运行时返回 [`PluginError::Conflict`]
    pub fn remove(&self, id: &PluginId, purge: bool) -> Result<(), PluginError> {
        let record = self.record(id)?;
        if let Some(supervisor) = &self.supervisor
            && let Ok(status) = supervisor.status(id)
            && status.session.is_some()
        {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is running; run `mcsup plugin disable {id}` first"
            )));
        }
        if record.enabled {
            return Err(PluginError::Conflict(format!(
                "plugin {id} is enabled; run `mcsup plugin disable {id}` first"
            )));
        }
        self.store.remove(id)?;
        let artifact = self.plugins_root.join(id.as_str());
        if artifact.exists() {
            std::fs::remove_dir_all(&artifact)?;
        }
        if purge {
            let data = self.plugin_data.join(id.as_str());
            if data.exists() {
                std::fs::remove_dir_all(&data)?;
            }
            let log = self.log_root.join(format!("{id}.log"));
            if log.exists() {
                std::fs::remove_file(&log)?;
            }
        }
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "remove".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            detail: Some(format!("purge={purge}")),
        });
        Ok(())
    }

    fn supervisor(&self) -> Result<&Arc<dyn PluginSupervisor>, PluginError> {
        self.supervisor.as_ref().ok_or_else(|| {
            PluginError::Host(
                "plugin host is not connected; run `mcsup plugin start <id>` on the host machine"
                    .to_owned(),
            )
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flag {
    Blocked,
    Quarantined,
}

/// 配置读写：校验、合并、落盘、下发
///
/// 入口（CLI）与宿主控制通道共用这一份实现，避免「CLI 校验一遍、宿主再
/// 校验一遍」后两边规则分叉。
pub(crate) struct PluginConfig {
    store: Arc<dyn PluginStore>,
    config_store: Arc<dyn PluginConfigStore>,
    plugins_root: PathBuf,
    supervisor: Option<Arc<dyn PluginSupervisor>>,
    audit: Arc<dyn AuditSink>,
}

impl std::fmt::Debug for PluginConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginConfig")
            .field("plugins_root", &self.plugins_root)
            .finish_non_exhaustive()
    }
}

impl PluginConfig {
    /// 构造
    #[must_use]
    pub(crate) fn new(
        store: Arc<dyn PluginStore>,
        config_store: Arc<dyn PluginConfigStore>,
        plugins_root: PathBuf,
        supervisor: Option<Arc<dyn PluginSupervisor>>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            store,
            config_store,
            plugins_root,
            supervisor,
            audit,
        }
    }

    fn record(&self, id: &PluginId) -> Result<PluginRecord, HostError> {
        self.store.get(id).map_err(|err| match err {
            RepositoryError::NotFound(_) => HostError::not_found(format!("plugin {id} not found")),
            other => HostError::internal(format!("read plugin {id}: {other}")),
        })
    }

    /// 已安装版本的配置 schema
    fn schema(record: &PluginRecord) -> Result<Option<Value>, HostError> {
        let path = record.artifact_dir.join("descriptor.json");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            // artifact 被手工删掉时按「无配置」处理；调用方会在写配置时拒绝。
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(HostError::internal(format!(
                    "read descriptor {}: {err}",
                    path.display()
                )));
            }
        };
        Descriptor::from_json(&bytes, &record.id)
            .map(|descriptor| descriptor.config)
            .map_err(|err| HostError::internal(format!("invalid descriptor: {err}")))
    }

    /// 当前值：schema 默认值打底，叠加已保存的值
    fn current(&self, id: &PluginId, schema: &Value) -> Result<StoredConfig, HostError> {
        let stored = self
            .config_store
            .get(id)
            .map_err(|err| HostError::internal(format!("read plugin config: {err}")))?
            .unwrap_or_default();
        let mut values = domain::plugin::schema::defaults(schema);
        if let (Some(base), Some(saved)) = (values.as_object_mut(), stored.values.as_object()) {
            for (key, value) in saved {
                base.insert(key.clone(), value.clone());
            }
        }
        Ok(StoredConfig {
            revision: stored.revision,
            values,
        })
    }
}

impl ConfigOps for PluginConfig {
    fn get(&self, id: &PluginId) -> Result<Value, HostError> {
        let record = self.record(id)?;
        let Some(schema) = Self::schema(&record)? else {
            return Ok(serde_json::json!({ "revision": 0, "values": {} }));
        };
        let current = self.current(id, &schema)?;
        Ok(serde_json::json!({
            "revision": current.revision,
            "values": domain::plugin::schema::mask_secrets(&schema, &current.values),
        }))
    }

    fn set(&self, id: &PluginId, values: &Value) -> Result<Value, HostError> {
        let record = self.record(id)?;
        let schema = Self::schema(&record)?.ok_or_else(|| {
            HostError::unsupported(format!("plugin {id} does not declare a config schema"))
        })?;
        let patch = values
            .as_object()
            .ok_or_else(|| HostError::invalid_input("config values must be an object"))?;
        let previous = self.current(id, &schema)?;
        let mut merged = previous.values.clone();
        let Some(object) = merged.as_object_mut() else {
            return Err(HostError::internal("stored config is not an object"));
        };
        for (key, value) in patch {
            object.insert(key.clone(), value.clone());
        }
        domain::plugin::schema::validate_instance(&schema, &merged, "config")
            .map_err(|err| HostError::invalid_input(err.to_string()))?;
        let next = StoredConfig {
            revision: previous.revision + 1,
            values: merged.clone(),
        };
        self.config_store
            .put(id, &next)
            .map_err(|err| HostError::internal(format!("write plugin config: {err}")))?;
        if let Some(supervisor) = &self.supervisor {
            match supervisor.apply_config(id, next.revision, &merged) {
                Ok(()) => {}
                // 宿主不在：配置已落盘，下次握手经 `welcome.config` 下发。
                Err(err) if err.code == port::plugin::ErrorCode::Unavailable => {}
                Err(err) => {
                    self.rollback(id, &previous);
                    self.mark_degraded(id, &err);
                    return Err(err);
                }
            }
        }
        let changed = patch.keys().cloned().collect::<Vec<_>>().join(", ");
        self.audit.record(&AuditEvent {
            at: now_secs(),
            plugin: id.to_string(),
            action: "config:set".to_owned(),
            capability: None,
            result: AuditResult::Allowed,
            // 审计只记键名，绝不记值。
            detail: Some(format!("revision {}, keys: {changed}", next.revision)),
        });
        Ok(serde_json::json!({
            "revision": next.revision,
            "values": domain::plugin::schema::mask_secrets(&schema, &next.values),
        }))
    }
}

impl PluginConfig {
    fn rollback(&self, id: &PluginId, previous: &StoredConfig) {
        // 写回失败只影响「保留上一版本」的强度，不能再把原始错误覆盖掉。
        let _ = self.config_store.put(id, previous);
    }

    fn mark_degraded(&self, id: &PluginId, err: &HostError) {
        let Ok(mut record) = self.store.get(id) else {
            return;
        };
        record.last_state = PluginState::Degraded;
        record.last_error = Some(format!("config apply failed: {}", err.message));
        record.updated_at = now_secs();
        let _ = self.store.put(&record);
    }
}

/// 安装记录里的离线状态
fn offline_state(record: &PluginRecord) -> PluginState {
    if !record.enabled {
        return match record.last_state {
            PluginState::PendingApproval | PluginState::Failed => record.last_state,
            _ => PluginState::Disabled,
        };
    }
    match record.last_state {
        PluginState::Running
        | PluginState::Starting
        | PluginState::Stopping
        | PluginState::Degraded => PluginState::Stopped,
        other => other,
    }
}

fn pending_capabilities(record: &PluginRecord) -> String {
    record
        .declared_capabilities()
        .into_iter()
        .filter(|cap| cap.risk().needs_approval() && !record.approved.contains(cap))
        .map(|cap| cap.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn origin_text(origin: &ArtifactOrigin) -> String {
    match origin {
        ArtifactOrigin::Local(path) => format!("local:{}", path.display()),
        ArtifactOrigin::Url(url) => format!("url:{url}"),
        ArtifactOrigin::Git { url, rev } => match rev {
            Some(rev) => format!("git:{url}@{rev}"),
            None => format!("git:{url}"),
        },
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

// ---------------------------------------------------------------- 宿主能力

/// `server:read` / `console:read` 的宿主实现
///
/// 只读能力直接复用注册表、生命周期与属性服务；控制台写、备份、文件等能力
/// 在后续阶段按同一形状追加，未装配时宿主返回 `unsupported`。
pub struct HostApiImpl {
    registry: Arc<ServerRegistry>,
    lifecycle: Arc<ServerLifecycleService>,
    props: ServerPropsService,
    kv: Arc<dyn KvStore>,
    secrets: Arc<dyn SecretStore>,
    store: Arc<dyn PluginStore>,
}

impl std::fmt::Debug for HostApiImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostApiImpl").finish_non_exhaustive()
    }
}

impl HostApiImpl {
    /// 构造宿主能力实现
    #[must_use]
    pub fn new(
        registry: Arc<ServerRegistry>,
        lifecycle: Arc<ServerLifecycleService>,
        kv: Arc<dyn KvStore>,
        secrets: Arc<dyn SecretStore>,
        store: Arc<dyn PluginStore>,
    ) -> Self {
        Self {
            props: ServerPropsService::new(Arc::clone(&registry)),
            registry,
            lifecycle,
            kv,
            secrets,
            store,
        }
    }

    /// 键值实现（装配到 `HostServices::kv`）
    #[must_use]
    pub fn kv(&self) -> Arc<dyn KvStore> {
        Arc::clone(&self.kv)
    }

    /// 密钥实现（装配到 `HostServices::secrets`）
    #[must_use]
    pub fn secrets(&self) -> Arc<dyn SecretStore> {
        Arc::clone(&self.secrets)
    }
}

impl NetPolicy for HostApiImpl {
    fn allowed_hosts(&self, plugin: &PluginId) -> Result<Vec<String>, HostError> {
        let record = self.store.get(plugin).map_err(|err| match err {
            RepositoryError::NotFound(message) => {
                HostError::not_found(format!("plugin {plugin} not found: {message}"))
            }
            other => HostError::internal(format!("read plugin {plugin}: {other}")),
        })?;
        Ok(record
            .manifest
            .net
            .as_ref()
            .map(|net| net.allow.clone())
            .unwrap_or_default())
    }
}

impl ServerRead for HostApiImpl {
    fn list(&self) -> Result<Vec<ServerSummary>, HostError> {
        let servers = self
            .registry
            .list()
            .map_err(|err| HostError::internal(format!("list servers: {err}")))?;
        let statuses = self
            .lifecycle
            .statuses()
            .map_err(|err| HostError::internal(format!("server statuses: {err}")))?;
        Ok(servers
            .into_iter()
            .map(|server| {
                let status = statuses
                    .get(server.meta.name.as_str())
                    .and_then(|result| result.as_ref().ok())
                    .and_then(Option::as_ref)
                    .map_or(ServerStatus::Stopped, |status| status.status);
                ServerSummary {
                    name: server.meta.name.clone(),
                    status,
                    version: Some(server.meta.version.clone()),
                }
            })
            .collect())
    }

    fn status(&self, server: &ServerName) -> Result<ServerStatusView, HostError> {
        let registered = self
            .registry
            .get(server.as_str())
            .map_err(|err| match err {
                crate::registry::ServerRegistryError::NotFound(_) => {
                    HostError::not_found(format!("server {server} not found"))
                }
                other => HostError::internal(format!("read server {server}: {other}")),
            })?;
        let status = self
            .lifecycle
            .status(server.as_str())
            .map_err(|err| HostError::internal(format!("server {server} status: {err}")))?
            .map_or(ServerStatus::Stopped, |status| status.status);
        Ok(ServerStatusView {
            status,
            version: Some(registered.meta.version.clone()),
            players: None,
            uptime: None,
        })
    }

    fn version(
        &self,
        server: &ServerName,
    ) -> Result<Option<domain::server::version::VersionId>, HostError> {
        let registered = self
            .registry
            .get(server.as_str())
            .map_err(|err| match err {
                crate::registry::ServerRegistryError::NotFound(_) => {
                    HostError::not_found(format!("server {server} not found"))
                }
                other => HostError::internal(format!("read server {server}: {other}")),
            })?;
        Ok(Some(registered.meta.version.clone()))
    }

    fn properties(
        &self,
        server: &ServerName,
    ) -> Result<std::collections::BTreeMap<String, String>, HostError> {
        let report = self
            .props
            .read(server.as_str())
            .map_err(|err| HostError::internal(format!("read properties: {err}")))?;
        Ok(report
            .properties
            .entries()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect())
    }
}

impl ConsoleIo for HostApiImpl {
    fn tail(
        &self,
        server: &ServerName,
        cursor: Option<u64>,
        max_lines: usize,
    ) -> Result<ConsolePage, HostError> {
        if max_lines == 0 || max_lines > MAX_CONSOLE_LINES {
            return Err(HostError::limit_exceeded(format!(
                "max_lines must be 1..={MAX_CONSOLE_LINES}"
            )));
        }
        let registered = self
            .registry
            .get(server.as_str())
            .map_err(|err| match err {
                crate::registry::ServerRegistryError::NotFound(_) => {
                    HostError::not_found(format!("server {server} not found"))
                }
                other => HostError::internal(format!("read server {server}: {other}")),
            })?;
        let path = console_log_path(&registered.dir);
        let text = match std::fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(HostError::internal(format!("read console log: {err}"))),
        };
        // 游标是字节偏移：单调递增、可跨进程复用，且无需额外状态文件。
        let start = cursor.unwrap_or(u64::MAX);
        let mut lines = Vec::new();
        let mut offset = 0_u64;
        for line in text.split_inclusive('\n') {
            let end = offset + line.len() as u64;
            if start == u64::MAX {
                // `None`：从尾部开始，保留最后 `max_lines` 行
                lines.push(ConsoleLine {
                    cursor: end,
                    text: line.trim_end_matches(['\r', '\n']).to_owned(),
                });
                if lines.len() > max_lines {
                    lines.remove(0);
                }
            } else if end > start {
                lines.push(ConsoleLine {
                    cursor: end,
                    text: line.trim_end_matches(['\r', '\n']).to_owned(),
                });
                if lines.len() >= max_lines {
                    break;
                }
            }
            offset = end;
        }
        let next = lines.last().map_or(start.min(offset), |line| line.cursor);
        Ok(ConsolePage { lines, next })
    }

    fn send_line(&self, server: &ServerName, line: &str) -> Result<(), HostError> {
        if line.len() > MAX_CONSOLE_LINE_BYTES {
            return Err(HostError::limit_exceeded(format!(
                "console line is {} bytes; limit is {MAX_CONSOLE_LINE_BYTES}",
                line.len()
            )));
        }
        self.lifecycle
            .send_line(server.as_str(), line)
            .map_err(|err| HostError::conflict(format!("inject console line: {err}")))
    }
}

/// 后续阶段的能力实现占位：装配到的能力若阶段未开放，宿主返回 `unsupported`
///
/// 用空实现而不是 `None` 是为了让装配方一眼看出「已显式关闭」，不是漏配。
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableLifecycle;

impl ServerLifecycle for UnavailableLifecycle {}

/// 备份能力占位
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableBackup;

impl BackupOps for UnavailableBackup {}

/// 内容能力占位
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableContent;

impl ContentOps for UnavailableContent {}

/// Java 能力占位
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableJava;

impl JavaOps for UnavailableJava {}

/// 文件能力占位
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableFiles;

impl FileOps for UnavailableFiles {}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::plugin::PluginScope;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryStore {
        records: Mutex<BTreeMap<String, PluginRecord>>,
    }

    /// 测试用配置存储：内存
    #[derive(Default)]
    struct MemoryConfigStore {
        configs: Mutex<BTreeMap<String, StoredConfig>>,
    }

    impl PluginConfigStore for MemoryConfigStore {
        fn get(&self, id: &PluginId) -> Result<Option<StoredConfig>, RepositoryError> {
            Ok(self.configs.lock().expect("lock").get(id.as_str()).cloned())
        }

        fn put(&self, id: &PluginId, config: &StoredConfig) -> Result<(), RepositoryError> {
            self.configs
                .lock()
                .expect("lock")
                .insert(id.to_string(), config.clone());
            Ok(())
        }
    }

    impl PluginStore for MemoryStore {
        fn list(&self) -> Result<Vec<PluginRecord>, RepositoryError> {
            Ok(self
                .records
                .lock()
                .expect("lock")
                .values()
                .cloned()
                .collect())
        }

        fn get(&self, id: &PluginId) -> Result<PluginRecord, RepositoryError> {
            self.records
                .lock()
                .expect("lock")
                .get(id.as_str())
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(id.to_string()))
        }

        fn put(&self, record: &PluginRecord) -> Result<(), RepositoryError> {
            self.records
                .lock()
                .expect("lock")
                .insert(record.id.to_string(), record.clone());
            Ok(())
        }

        fn remove(&self, id: &PluginId) -> Result<(), RepositoryError> {
            self.records
                .lock()
                .expect("lock")
                .remove(id.as_str())
                .map(|_| ())
                .ok_or_else(|| RepositoryError::NotFound(id.to_string()))
        }
    }

    fn record(id: &str, enabled: bool, approved: &[Capability]) -> PluginRecord {
        let manifest = Manifest::from_json(
            format!(
                r#"{{"schema":1,"id":"{id}","name":"Demo","version":"0.1.0","api":1,
                    "entry":{{"type":"process","cmd":"bin/demo"}},
                    "capabilities":["events","storage:kv","server:read","console:read"],
                    "license":"MIT"}}"#
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
            enabled,
            approved: approved.iter().copied().collect(),
            manifest: manifest.raw,
            artifact_dir: PathBuf::from("/tmp/demo"),
            installed_at: 0,
            updated_at: 0,
            last_state: PluginState::Disabled,
            last_error: None,
            previous_version: None,
            source: None,
            sha256: None,
            signed: false,
            blocked: false,
            quarantined: false,
        }
    }

    fn service(store: Arc<MemoryStore>) -> PluginService {
        PluginService::new(
            store,
            Arc::new(MemoryConfigStore::default()),
            None,
            None,
            None,
            Arc::new(port::plugin::NoopAuditSink),
            PathBuf::from("/tmp/plugins"),
            PathBuf::from("/tmp/plugin-data"),
            PathBuf::from("/tmp/logs"),
        )
    }

    #[test]
    fn enable_requires_approval_for_medium_risk() {
        let store = Arc::new(MemoryStore::default());
        let mut pending = record("demo", false, &[]);
        pending.last_state = PluginState::PendingApproval;
        // console:read 是中风险，未授权时启用必须被拒
        pending.approved.insert(Capability::ServerRead);
        store.put(&pending).expect("put");
        let service = service(store.clone());
        let id = PluginId::parse("demo").expect("id");
        let err = service.enable(&id).expect_err("pending approval");
        assert_eq!(err.category(), ErrorCategory::Conflict);
        service
            .grant(&id, Capability::Events)
            .expect("grant events");
        service.grant(&id, Capability::StorageKv).expect("grant kv");
        service
            .grant(&id, Capability::ConsoleRead)
            .expect("grant console");
        let enabled = service.enable(&id).expect("enable");
        assert!(enabled.enabled);
        assert_eq!(enabled.last_state, PluginState::Stopped);
    }

    #[test]
    fn revoke_returns_to_pending_approval() {
        let store = Arc::new(MemoryStore::default());
        let mut ready = record(
            "demo",
            true,
            &[
                Capability::Events,
                Capability::StorageKv,
                Capability::ServerRead,
                Capability::ConsoleRead,
            ],
        );
        ready.last_state = PluginState::Stopped;
        store.put(&ready).expect("put");
        let service = service(store);
        let id = PluginId::parse("demo").expect("id");
        let record = service
            .revoke(&id, Capability::ConsoleRead)
            .expect("revoke");
        assert_eq!(record.last_state, PluginState::PendingApproval);
        assert!(record.needs_approval());
    }

    #[test]
    fn grant_rejects_undeclared_capability() {
        let store = Arc::new(MemoryStore::default());
        store.put(&record("demo", false, &[])).expect("put");
        let service = service(store);
        let id = PluginId::parse("demo").expect("id");
        let err = service
            .grant(&id, Capability::Java)
            .expect_err("undeclared");
        assert_eq!(err.category(), ErrorCategory::Usage);
    }

    #[test]
    fn remove_requires_disabled() {
        let store = Arc::new(MemoryStore::default());
        store.put(&record("demo", true, &[])).expect("put");
        let service = service(store);
        let id = PluginId::parse("demo").expect("id");
        assert_eq!(
            service.remove(&id, false).expect_err("enabled").category(),
            ErrorCategory::Conflict
        );
    }

    fn config_service(dir: &Path) -> (PluginService, PluginId) {
        let id = PluginId::parse("demo").expect("id");
        std::fs::write(
            dir.join("descriptor.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema": 1,
                "plugin_id": "demo",
                "api": 1,
                "config": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "interval_secs": { "type": "integer", "minimum": 5, "default": 60 },
                        "token": { "type": "string", "x-mcsup-secret": true }
                    }
                }
            }))
            .expect("descriptor json"),
        )
        .expect("write descriptor");
        let store: Arc<dyn PluginStore> = Arc::new(MemoryStore::default());
        let mut stored = record("demo", false, &[]);
        stored.artifact_dir = dir.to_path_buf();
        store.put(&stored).expect("put");
        let service = PluginService::new(
            Arc::clone(&store),
            Arc::new(MemoryConfigStore::default()),
            None,
            None,
            None,
            Arc::new(port::plugin::NoopAuditSink),
            dir.join("plugins"),
            dir.join("plugin-data"),
            dir.join("logs"),
        );
        (service, id)
    }

    #[test]
    fn config_get_starts_from_schema_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (service, id) = config_service(dir.path());
        let config = service.config_get(&id).expect("config");
        assert_eq!(config["revision"], 0);
        assert_eq!(config["values"]["interval_secs"], 60);
    }

    #[test]
    fn config_set_merges_validates_and_bumps_revision() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (service, id) = config_service(dir.path());
        let config = service
            .config_set(&id, &json!({ "interval_secs": 30 }))
            .expect("set");
        assert_eq!(config["revision"], 1);
        assert_eq!(config["values"]["interval_secs"], 30);
        assert_eq!(
            service.config_get(&id).expect("get")["values"]["interval_secs"],
            30
        );
    }

    #[test]
    fn invalid_config_is_rejected_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (service, id) = config_service(dir.path());
        let err = service
            .config_set(&id, &json!({ "interval_secs": 1 }))
            .expect_err("below minimum");
        assert_eq!(err.category(), ErrorCategory::Usage);
        assert_eq!(service.config_get(&id).expect("get")["revision"], 0);
    }

    #[test]
    fn secret_config_values_are_masked_on_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (service, id) = config_service(dir.path());
        service
            .config_set(&id, &json!({ "token": "s3cret" }))
            .expect("set secret");
        let config = service.config_get(&id).expect("get");
        assert_eq!(config["values"]["token"], "***");
    }

    #[test]
    fn blocked_plugin_cannot_be_enabled() {
        let store = Arc::new(MemoryStore::default());
        store
            .put(&record("demo", false, &all_capabilities()))
            .expect("put");
        let service = service(store);
        let id = PluginId::parse("demo").expect("id");
        let blocked = service.block(&id).expect("block");
        assert!(blocked.blocked);
        assert_eq!(
            service.enable(&id).expect_err("blocked").category(),
            ErrorCategory::Conflict
        );
        let released = service.unblock(&id).expect("unblock");
        assert!(!released.blocked);
        service.enable(&id).expect("enable after unblock");
    }

    #[test]
    fn quarantined_plugin_cannot_be_enabled() {
        let store = Arc::new(MemoryStore::default());
        store
            .put(&record("demo", false, &all_capabilities()))
            .expect("put");
        let service = service(store);
        let id = PluginId::parse("demo").expect("id");
        service.quarantine(&id).expect("quarantine");
        assert_eq!(
            service.enable(&id).expect_err("quarantined").category(),
            ErrorCategory::Conflict
        );
        service.release(&id).expect("release");
        service.enable(&id).expect("enable after release");
    }

    fn all_capabilities() -> [Capability; 4] {
        [
            Capability::Events,
            Capability::StorageKv,
            Capability::ServerRead,
            Capability::ConsoleRead,
        ]
    }
}
