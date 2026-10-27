//! 远端帧的线格式：与领域对象一一对应的可序列化形态
//!
//! `domain` 刻意不依赖 `serde`（领域模型不该被某种传输格式绑架），而
//! `storage` 的记录结构是本机落盘口径、不能复用。因此线格式在这里单独定义：
//! 字段名即 JSON 键，转换只在本模块内发生一次。
//!
//! 转换方向成对出现（`From<&T>` / `TryFrom<Wire>`），由测试锁住往返一致——
//! 单写一侧必然出现「发得出去、收不回来」。

use domain::backup::BackupScope;
use domain::java::{JavaMajor, JavaRuntime, JavaVendor};
use domain::server::artifact::{Checksum, ChecksumAlgorithm};
use domain::server::launch::LaunchSpec;
use domain::server::meta::{InstalledJar, ServerId, ServerMeta, ServerName};
use domain::server::settings::ServerSettings;
use domain::server::version::{ServerVariant, VersionId};
use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
use port::{RepositoryError, StopOutcome, SupervisorStartOutcome, SupervisorStatus};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// 线格式解码失败：统一成 `RepositoryError::Corrupt`
///
/// 对端发来的记录不合法与本地记录损坏是同一类问题（「这条数据读不出来」），
/// 因此复用同一个错误变体，入口的提示不必分两套。
pub fn corrupt(
    label: &str,
    value: impl std::fmt::Display,
    err: impl std::fmt::Display,
) -> RepositoryError {
    RepositoryError::Corrupt(format!("{label} {value}: {err}"))
}

/// 已配对客户端的线格式
///
/// 刻意不含 `digest`：客户端列表是给管理员看的，摘要对展示无用，多传一次就
/// 多一处泄漏面（摘要足以在本地做离线比对，不需要跨网络流动）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireClient {
    /// 服务端分配的身份
    pub client_id: String,
    /// 客户端自称
    pub name: String,
    /// 角色
    pub role: auth::Role,
    /// 配对时间（Unix 秒）
    pub paired_at: i64,
}

impl From<&auth::ClientRecord> for WireClient {
    fn from(record: &auth::ClientRecord) -> Self {
        Self {
            client_id: record.client_id.as_str().to_owned(),
            name: record.name.clone(),
            role: record.role,
            paired_at: record.paired_at,
        }
    }
}

impl WireClient {
    /// 客户端身份
    ///
    /// # Errors
    ///
    /// 对端发来的 id 不是合法 UUID 时返回 [`RepositoryError::Corrupt`]
    pub fn client_id(&self) -> Result<auth::ClientId, RepositoryError> {
        auth::ClientId::parse(&self.client_id)
            .map_err(|err| corrupt("client id", &self.client_id, err))
    }
}

/// 工作目录的线格式
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireWorkspace {
    /// 身份（UUID 文本）
    pub id: String,
    /// 名称
    pub name: String,
    /// 对端绝对路径
    pub path: String,
    /// 是否为对端的当前工作区
    pub is_current: bool,
    /// Unix 秒
    pub created_at: i64,
    /// Unix 秒
    pub updated_at: i64,
}

impl From<&Workspace> for WireWorkspace {
    fn from(ws: &Workspace) -> Self {
        Self {
            id: ws.id.as_str().to_owned(),
            name: ws.name.as_str().to_owned(),
            path: ws.path.to_string_lossy().into_owned(),
            is_current: ws.is_current,
            created_at: ws.created_at,
            updated_at: ws.updated_at,
        }
    }
}

impl TryFrom<WireWorkspace> for Workspace {
    type Error = RepositoryError;

    fn try_from(wire: WireWorkspace) -> Result<Self, Self::Error> {
        Ok(Self {
            id: WorkspaceId::parse(&wire.id)
                .map_err(|err| corrupt("workspace id", &wire.id, err))?,
            name: WorkspaceName::parse(&wire.name)
                .map_err(|err| corrupt("workspace name", &wire.name, err))?,
            path: PathBuf::from(wire.path),
            is_current: wire.is_current,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
        })
    }
}

/// 已安装核心的线格式
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireJar {
    /// 相对服务器目录的核心路径
    pub path: String,
    /// 字节数
    pub size: u64,
    /// 校验算法（`sha1` / `sha256`）
    pub algorithm: String,
    /// 十六进制哈希；未校验时为空串
    pub hex: String,
    /// 上游是否提供了可校验的哈希
    pub verified: bool,
}

impl From<&InstalledJar> for WireJar {
    fn from(jar: &InstalledJar) -> Self {
        Self {
            path: jar.path.to_string_lossy().into_owned(),
            size: jar.size,
            algorithm: jar.checksum.algorithm().as_str().to_owned(),
            hex: jar.checksum.hex().to_owned(),
            verified: jar.checksum.is_verified(),
        }
    }
}

impl TryFrom<WireJar> for InstalledJar {
    type Error = RepositoryError;

    fn try_from(wire: WireJar) -> Result<Self, Self::Error> {
        let algorithm = ChecksumAlgorithm::parse(&wire.algorithm)
            .map_err(|err| corrupt("checksum algorithm", &wire.algorithm, err))?;
        let checksum = if wire.verified {
            Checksum::new(algorithm, wire.hex)
        } else {
            Checksum::unverified()
        };
        Ok(Self {
            path: PathBuf::from(wire.path),
            size: wire.size,
            checksum,
        })
    }
}

/// 启动方式的线格式
///
/// 用带标签枚举而不是「`kind` + 两个可空字段」：非法组合在解码时就消失，
/// 不必在读取侧补一堆「`kind = jar` 却带了 `files`」的判断。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireLaunch {
    /// 单个可运行 JAR
    Jar {
        /// 相对服务器目录的路径
        path: String,
    },
    /// 参数文件串（`Forge` / `NeoForge`）
    Args {
        /// 参数文件（相对服务器目录，按展开顺序）
        files: Vec<String>,
        /// 追加给服务端的程序参数
        args: Vec<String>,
    },
}

impl TryFrom<&LaunchSpec> for WireLaunch {
    type Error = RepositoryError;

    /// `LaunchSpec` 是 `non_exhaustive`：领域层将来新增启动方式时，本协议版本
    /// 表达不出来。这里必须报错而不是猜一个近似值——把 `Forge` 的参数文件
    /// 当成单 JAR 发过去，对端会拉起一个必然失败的命令行。
    fn try_from(spec: &LaunchSpec) -> Result<Self, Self::Error> {
        match spec {
            LaunchSpec::Jar { path } => Ok(Self::Jar {
                path: path.to_string_lossy().into_owned(),
            }),
            LaunchSpec::Args { files, args } => Ok(Self::Args {
                files: files
                    .iter()
                    .map(|f| f.to_string_lossy().into_owned())
                    .collect(),
                args: args.clone(),
            }),
            other => Err(corrupt(
                "launch",
                format_args!("{other:?}"),
                "not representable in this remote protocol version",
            )),
        }
    }
}

impl From<WireLaunch> for LaunchSpec {
    fn from(wire: WireLaunch) -> Self {
        match wire {
            WireLaunch::Jar { path } => Self::Jar {
                path: PathBuf::from(path),
            },
            WireLaunch::Args { files, args } => Self::Args {
                files: files.into_iter().map(PathBuf::from).collect(),
                args,
            },
        }
    }
}

/// 服务属性的线格式
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSettings {
    /// 绑定的 Java 大版本
    pub java_major: Option<i64>,
    /// 堆上限（MiB）
    pub ram_mib: Option<u32>,
    /// 机器启动时自动拉起
    pub autostart: bool,
    /// 备份范围的顶层项；空 = 整个服务器目录
    pub backup_scope: Vec<String>,
}

impl From<&ServerSettings> for WireSettings {
    fn from(settings: &ServerSettings) -> Self {
        Self {
            java_major: settings.java_major.map(JavaMajor::as_i64),
            ram_mib: settings.ram_mib,
            autostart: settings.autostart,
            backup_scope: settings.backup_scope.entries().to_vec(),
        }
    }
}

impl TryFrom<WireSettings> for ServerSettings {
    type Error = RepositoryError;

    fn try_from(wire: WireSettings) -> Result<Self, Self::Error> {
        let java_major = match wire.java_major {
            Some(raw) => {
                Some(JavaMajor::parse(raw).map_err(|err| corrupt("java major", raw, err))?)
            }
            None => None,
        };
        let backup_scope = BackupScope::from_entries(wire.backup_scope)
            .map_err(|err| RepositoryError::Corrupt(format!("backup scope: {err}")))?;
        Ok(Self {
            java_major,
            ram_mib: wire.ram_mib,
            autostart: wire.autostart,
            backup_scope,
        })
    }
}

/// 一次 `server.properties` 写入的一项
///
/// 用结构体而不是二元组：帧是 JSON，`[["motd","hi"]]` 读起来要靠位置记忆，
/// 字段名让审计日志与抓包都能直接看懂。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WirePropUpdate {
    /// 键
    pub key: String,
    /// 新值
    pub value: String,
}

impl From<&(String, String)> for WirePropUpdate {
    fn from(update: &(String, String)) -> Self {
        Self {
            key: update.0.clone(),
            value: update.1.clone(),
        }
    }
}

/// 服务元数据的线格式
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireServer {
    /// 身份（UUID 文本）
    pub id: String,
    /// 名称
    pub name: String,
    /// 所属工作区身份
    pub workspace_id: String,
    /// 相对工作目录 `servers/` 下的目录名
    pub rel_dir: String,
    /// 服务端版本
    pub version: String,
    /// 变体标识
    pub variant: String,
    /// 已安装核心
    pub jar: Option<WireJar>,
    /// 启动方式
    pub launch: Option<WireLaunch>,
    /// 服务属性
    pub settings: WireSettings,
    /// 该版本要求的 Java 大版本
    pub java_major: Option<i64>,
    /// Unix 秒
    pub created_at: i64,
    /// Unix 秒
    pub updated_at: i64,
}

impl TryFrom<&ServerMeta> for WireServer {
    type Error = RepositoryError;

    fn try_from(meta: &ServerMeta) -> Result<Self, Self::Error> {
        Ok(Self {
            id: meta.id.as_str().to_owned(),
            name: meta.name.as_str().to_owned(),
            workspace_id: meta.workspace_id.as_str().to_owned(),
            rel_dir: meta.rel_dir.to_string_lossy().into_owned(),
            version: meta.version.as_str().to_owned(),
            variant: meta.variant.as_str().to_owned(),
            jar: meta.jar.as_ref().map(WireJar::from),
            launch: meta.launch.as_ref().map(WireLaunch::try_from).transpose()?,
            settings: WireSettings::from(&meta.settings),
            java_major: meta.java_major.map(JavaMajor::as_i64),
            created_at: meta.created_at,
            updated_at: meta.updated_at,
        })
    }
}

impl TryFrom<WireServer> for ServerMeta {
    type Error = RepositoryError;

    fn try_from(wire: WireServer) -> Result<Self, Self::Error> {
        let id = ServerId::parse(&wire.id).map_err(|err| corrupt("server id", &wire.id, err))?;
        let name = ServerName::parse(&wire.name).map_err(|err| corrupt("name", &wire.name, err))?;
        let workspace_id = WorkspaceId::parse(&wire.workspace_id)
            .map_err(|err| corrupt("workspace id", &wire.workspace_id, err))?;
        let variant = ServerVariant::from_id(wire.variant.clone())
            .map_err(|err| corrupt("variant", &wire.variant, err))?;
        let java_major = match wire.java_major {
            Some(raw) => {
                Some(JavaMajor::parse(raw).map_err(|err| corrupt("java major", raw, err))?)
            }
            None => None,
        };
        Ok(Self {
            id,
            name,
            workspace_id,
            rel_dir: PathBuf::from(wire.rel_dir),
            version: VersionId::new(wire.version),
            variant,
            jar: wire.jar.map(InstalledJar::try_from).transpose()?,
            launch: wire.launch.map(LaunchSpec::from),
            settings: ServerSettings::try_from(wire.settings)?,
            java_major,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
        })
    }
}

/// Java 运行时的线格式
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireJava {
    /// 稳定身份
    pub id: String,
    /// JVM home 或 `java` 所在目录（对端绝对路径）
    pub path: String,
    /// 大版本
    pub major: i64,
    /// 完整版本串
    pub version: String,
    /// 厂商
    pub vendor: String,
    /// 是否由 mcsup 安装
    pub managed: bool,
    /// Unix 秒
    pub created_at: i64,
    /// Unix 秒
    pub updated_at: i64,
}

impl From<&JavaRuntime> for WireJava {
    fn from(rt: &JavaRuntime) -> Self {
        Self {
            id: rt.id.clone(),
            path: rt.path.to_string_lossy().into_owned(),
            major: rt.major.as_i64(),
            version: rt.version.clone(),
            vendor: rt.vendor.as_str().to_owned(),
            managed: rt.managed,
            created_at: rt.created_at,
            updated_at: rt.updated_at,
        }
    }
}

impl TryFrom<WireJava> for JavaRuntime {
    type Error = RepositoryError;

    fn try_from(wire: WireJava) -> Result<Self, Self::Error> {
        let major =
            JavaMajor::parse(wire.major).map_err(|err| corrupt("java major", wire.major, err))?;
        Ok(Self {
            id: wire.id,
            path: PathBuf::from(wire.path),
            major,
            version: wire.version,
            vendor: JavaVendor::parse_fuzzy(&wire.vendor),
            managed: wire.managed,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
        })
    }
}

/// supervisor 实时状态的线格式
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireStatus {
    /// 状态机口径文本（`stopped` / `running` …）
    pub status: String,
    /// 服务端进程 PID
    pub pid: Option<u32>,
    /// 已运行秒数
    pub uptime_secs: Option<u64>,
    /// 退出码
    pub exit_code: Option<i32>,
}

impl From<&SupervisorStatus> for WireStatus {
    fn from(status: &SupervisorStatus) -> Self {
        Self {
            status: status.status.as_str().to_owned(),
            pid: status.pid,
            uptime_secs: status.uptime.map(|d| d.as_secs()),
            exit_code: status.exit_code,
        }
    }
}

impl TryFrom<WireStatus> for SupervisorStatus {
    type Error = String;

    fn try_from(wire: WireStatus) -> Result<Self, Self::Error> {
        let status = crate::status_from_wire(&wire.status)
            .ok_or_else(|| format!("unknown server status: {}", wire.status))?;
        Ok(Self {
            status,
            pid: wire.pid,
            uptime: wire.uptime_secs.map(Duration::from_secs),
            exit_code: wire.exit_code,
        })
    }
}

/// 停机结果的线格式
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WireStop {
    /// 宽限内正常退出
    Stopped {
        /// 请求停机到退出的耗时（毫秒）
        elapsed_ms: u64,
    },
    /// 本来就没在跑
    AlreadyStopped,
    /// 宽限到期仍在运行
    TimedOut {
        /// 已等待的时长（毫秒）
        waited_ms: u64,
    },
}

impl From<&StopOutcome> for WireStop {
    fn from(outcome: &StopOutcome) -> Self {
        match outcome {
            StopOutcome::Stopped { elapsed } => Self::Stopped {
                elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            },
            StopOutcome::AlreadyStopped => Self::AlreadyStopped,
            StopOutcome::TimedOut { waited } => Self::TimedOut {
                waited_ms: u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
            },
        }
    }
}

impl From<WireStop> for StopOutcome {
    fn from(wire: WireStop) -> Self {
        match wire {
            WireStop::Stopped { elapsed_ms } => Self::Stopped {
                elapsed: Duration::from_millis(elapsed_ms),
            },
            WireStop::AlreadyStopped => Self::AlreadyStopped,
            WireStop::TimedOut { waited_ms } => Self::TimedOut {
                waited: Duration::from_millis(waited_ms),
            },
        }
    }
}

/// supervisor 拉起结果的线格式
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireStartOutcome {
    /// supervisor 进程 PID
    pub supervisor_pid: u32,
}

impl From<&SupervisorStartOutcome> for WireStartOutcome {
    fn from(outcome: &SupervisorStartOutcome) -> Self {
        Self {
            supervisor_pid: outcome.supervisor_pid,
        }
    }
}

impl From<WireStartOutcome> for SupervisorStartOutcome {
    fn from(wire: WireStartOutcome) -> Self {
        Self {
            supervisor_pid: wire.supervisor_pid,
        }
    }
}

/// 批量状态里的一项
///
/// 用「两个可空字段」而不是 `Result`：`serde` 对 `Result` 的外部标签会把
/// 线格式变成 `{"Ok":…}` / `{"Err":…}`，既难读也难与「本来就是 `null`」区分。
/// 不变量（恰好一个为 `Some`）在解码时由 [`Self::into_result`] 断言。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireStatusEntry {
    /// 该台的实时状态；`None` = 没有 supervisor（未运行）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<WireStatus>,
    /// 单台查询失败的原因；`None` = 查询成功
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WireStatusEntry {
    /// 查询成功（可能没在跑）
    #[must_use]
    pub fn ok(status: Option<&SupervisorStatus>) -> Self {
        Self {
            status: status.map(WireStatus::from),
            error: None,
        }
    }

    /// 单台查询失败
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            status: None,
            error: Some(message.into()),
        }
    }

    /// 解码成契约类型
    ///
    /// # Errors
    ///
    /// 两个字段同时为 `Some`（非法组合）或状态文本无法识别时返回说明
    pub fn into_result(self) -> Result<Option<SupervisorStatus>, String> {
        match (self.status, self.error) {
            (Some(_), Some(_)) => Err("status entry carries both status and error".to_owned()),
            (None, Some(error)) => Err(error),
            (None, None) => Ok(None),
            (Some(status), None) => SupervisorStatus::try_from(status).map(Some),
        }
    }
}

/// 快照的线格式
///
/// `entries` / `deleted` / `stamps` 是快照内容的三份清单：路径一律 `/` 分隔，
/// 哈希是 64 位小写十六进制。只传元数据——CAS 对象本身留在对端。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSnapshot {
    /// 快照 id
    pub id: String,
    /// 所属工作区标识
    pub workspace_id: String,
    /// 所属服务器名
    pub server: String,
    /// 用户标签
    pub label: Option<String>,
    /// 父快照 id（增量基线）
    pub parent: Option<String>,
    /// 备份范围（顶层项）；空 = 整个目录
    pub scope: Vec<String>,
    /// 状态文本（`in_progress` / `complete` / `failed`）
    pub status: String,
    /// 相对路径 → 内容哈希
    pub entries: Vec<(String, String)>,
    /// 删除墓碑（相对父快照被删掉的路径）
    pub deleted: Vec<String>,
    /// 相对路径 → (字节数, 修改时间)
    pub stamps: Vec<(String, u64, i64)>,
    /// 本次新建的 CAS 对象数
    pub added: usize,
    /// 复用已存在对象的文件数
    pub reused: usize,
    /// 逻辑总字节数
    pub total_bytes: u64,
    /// 本次净增字节数
    pub added_bytes: u64,
    /// 备份时服务器是否在运行
    pub server_online: bool,
    /// Unix 秒
    pub created_at: i64,
}

impl TryFrom<&domain::backup::Snapshot> for WireSnapshot {
    type Error = RepositoryError;

    fn try_from(snapshot: &domain::backup::Snapshot) -> Result<Self, Self::Error> {
        let mut entries = Vec::with_capacity(snapshot.content.len());
        for (path, hash) in snapshot.content.entries() {
            entries.push((path.as_str().to_owned(), hash.as_str().to_owned()));
        }
        let stamps = snapshot
            .content
            .stamps()
            .iter()
            .map(|(path, size, mtime)| (path.as_str().to_owned(), *size, *mtime))
            .collect();
        Ok(Self {
            id: snapshot.id.as_str().to_owned(),
            workspace_id: snapshot.workspace_id.clone(),
            server: snapshot.server.clone(),
            label: snapshot.label.clone(),
            parent: snapshot.parent.as_ref().map(|id| id.as_str().to_owned()),
            scope: snapshot.scope.entries().to_vec(),
            status: snapshot.status.as_str().to_owned(),
            entries,
            deleted: snapshot
                .content
                .deleted()
                .iter()
                .map(|path| path.as_str().to_owned())
                .collect(),
            stamps,
            added: snapshot.added,
            reused: snapshot.reused,
            total_bytes: snapshot.total_bytes,
            added_bytes: snapshot.added_bytes,
            server_online: snapshot.server_online,
            created_at: snapshot.created_at,
        })
    }
}

impl TryFrom<WireSnapshot> for domain::backup::Snapshot {
    type Error = RepositoryError;

    fn try_from(wire: WireSnapshot) -> Result<Self, Self::Error> {
        let mut entries = Vec::with_capacity(wire.entries.len());
        for (path, hash) in wire.entries {
            entries.push((
                domain::backup::RelPath::parse(&path)
                    .map_err(|err| corrupt("snapshot path", &path, err))?,
                domain::backup::ContentHash::parse(&hash)
                    .map_err(|err| corrupt("snapshot hash", &hash, err))?,
            ));
        }
        let mut deleted = Vec::with_capacity(wire.deleted.len());
        for path in wire.deleted {
            deleted.push(
                domain::backup::RelPath::parse(&path)
                    .map_err(|err| corrupt("snapshot tombstone", &path, err))?,
            );
        }
        let stamps = wire
            .stamps
            .into_iter()
            .map(|(path, size, mtime)| {
                domain::backup::RelPath::parse(&path)
                    .map(|path| (path, size, mtime))
                    .map_err(|err| corrupt("snapshot stamp", &path, err))
            })
            .collect::<Result<Vec<_>, RepositoryError>>()?;
        let content = domain::backup::SnapshotContent::new(entries, deleted)
            .map_err(|err| corrupt("snapshot content", "", err))?
            .with_stamps(stamps);
        Ok(Self {
            id: domain::backup::SnapshotId::parse(&wire.id)
                .map_err(|err| corrupt("snapshot id", &wire.id, err))?,
            workspace_id: wire.workspace_id,
            server: wire.server,
            label: wire.label,
            parent: wire
                .parent
                .map(|raw| {
                    domain::backup::SnapshotId::parse(&raw)
                        .map_err(|err| corrupt("snapshot parent", &raw, err))
                })
                .transpose()?,
            scope: BackupScope::from_entries(wire.scope)
                .map_err(|err| corrupt("snapshot scope", "", err))?,
            status: domain::backup::SnapshotStatus::parse(&wire.status)
                .map_err(|err| corrupt("snapshot status", &wire.status, err))?,
            content,
            added: wire.added,
            reused: wire.reused,
            total_bytes: wire.total_bytes,
            added_bytes: wire.added_bytes,
            server_online: wire.server_online,
            created_at: wire.created_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::server::status::ServerStatus;

    fn workspace() -> Workspace {
        Workspace {
            id: WorkspaceId::parse("550e8400-e29b-41d4-a716-446655440000").expect("id"),
            name: WorkspaceName::parse("survival").expect("name"),
            path: PathBuf::from("/srv/mc/survival"),
            is_current: true,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_100,
        }
    }

    fn server() -> ServerMeta {
        ServerMeta {
            id: ServerId::parse("550e8400-e29b-41d4-a716-446655440001").expect("id"),
            name: ServerName::parse("web").expect("name"),
            workspace_id: WorkspaceId::parse("550e8400-e29b-41d4-a716-446655440000").expect("ws"),
            rel_dir: PathBuf::from("web"),
            version: VersionId::new("1.21.1"),
            variant: ServerVariant::Paper,
            jar: Some(InstalledJar {
                path: PathBuf::from("server.jar"),
                size: 42,
                checksum: Checksum::new(ChecksumAlgorithm::Sha256, "ab".repeat(32)),
            }),
            launch: Some(LaunchSpec::Args {
                files: vec![PathBuf::from("user_jvm_args.txt")],
                args: vec!["--nogui".to_owned()],
            }),
            settings: ServerSettings {
                java_major: Some(JavaMajor::parse(21).expect("major")),
                ram_mib: Some(2048),
                autostart: true,
                backup_scope: BackupScope::from_entries(["world"]).expect("scope"),
            },
            java_major: Some(JavaMajor::parse(21).expect("major")),
            created_at: 1,
            updated_at: 2,
        }
    }

    fn java() -> JavaRuntime {
        JavaRuntime {
            id: "rt-1".to_owned(),
            path: PathBuf::from("/opt/java/temurin-21"),
            major: JavaMajor::parse(21).expect("major"),
            version: "21.0.1+12".to_owned(),
            vendor: JavaVendor::Temurin,
            managed: true,
            created_at: 1,
            updated_at: 2,
        }
    }

    fn json_roundtrip<T>(value: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let text = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&text).expect("deserialize")
    }

    #[test]
    fn workspace_roundtrips() {
        let original = workspace();
        let wire = json_roundtrip(&WireWorkspace::from(&original));
        assert_eq!(Workspace::try_from(wire).expect("decode"), original);
    }

    #[test]
    fn server_roundtrips_including_launch_and_checksum() {
        let original = server();
        let wire = json_roundtrip(&WireServer::try_from(&original).expect("encode"));
        assert_eq!(ServerMeta::try_from(wire).expect("decode"), original);
    }

    /// 未校验的校验和必须原样还原成「未校验」，不能变成「哈希是空串的有效值」
    #[test]
    fn unverified_checksum_survives_the_wire() {
        let mut original = server();
        original.jar = Some(InstalledJar {
            path: PathBuf::from("server.jar"),
            size: 1,
            checksum: Checksum::unverified(),
        });
        let wire = json_roundtrip(&WireServer::try_from(&original).expect("encode"));
        let decoded = ServerMeta::try_from(wire).expect("decode");
        let jar = decoded.jar.expect("jar");
        assert!(!jar.checksum.is_verified());
        assert_eq!(jar.checksum.hex(), "");
    }

    #[test]
    fn java_roundtrips() {
        let original = java();
        let wire = json_roundtrip(&WireJava::from(&original));
        assert_eq!(JavaRuntime::try_from(wire).expect("decode"), original);
    }

    #[test]
    fn status_and_stop_roundtrip() {
        let status = SupervisorStatus {
            status: ServerStatus::Running,
            pid: Some(7),
            uptime: Some(Duration::from_secs(3)),
            exit_code: None,
        };
        let wire = json_roundtrip(&WireStatus::from(&status));
        assert_eq!(SupervisorStatus::try_from(wire).expect("decode"), status);

        for outcome in [
            StopOutcome::Stopped {
                elapsed: Duration::from_millis(1200),
            },
            StopOutcome::AlreadyStopped,
            StopOutcome::TimedOut {
                waited: Duration::from_secs(30),
            },
        ] {
            let wire = json_roundtrip(&WireStop::from(&outcome));
            assert_eq!(StopOutcome::from(wire), outcome);
        }
    }

    /// 非法字段必须在解码时报错，而不是静默变成默认值
    #[test]
    fn malformed_wire_records_are_rejected() {
        let mut wire = WireServer::try_from(&server()).expect("encode");
        wire.id = "not-a-uuid".to_owned();
        assert!(ServerMeta::try_from(wire).is_err());

        let mut wire = WireServer::try_from(&server()).expect("encode");
        wire.variant = "  ".to_owned();
        assert!(ServerMeta::try_from(wire).is_err());

        let bad = WireStatus {
            status: "banana".to_owned(),
            pid: None,
            uptime_secs: None,
            exit_code: None,
        };
        assert!(SupervisorStatus::try_from(bad).is_err());
    }

    /// 批量状态项的不变量：恰好一个字段为 `Some`
    #[test]
    fn status_entry_requires_exactly_one_of_status_and_error() {
        let running = SupervisorStatus {
            status: ServerStatus::Running,
            pid: Some(7),
            uptime: Some(Duration::from_secs(3)),
            exit_code: None,
        };
        assert_eq!(
            WireStatusEntry::ok(Some(&running))
                .into_result()
                .expect("ok"),
            Some(running)
        );
        assert_eq!(WireStatusEntry::ok(None).into_result().expect("none"), None);
        assert_eq!(
            WireStatusEntry::failed("boom").into_result(),
            Err("boom".to_owned())
        );

        let both = WireStatusEntry {
            status: Some(WireStatus {
                status: "running".to_owned(),
                pid: None,
                uptime_secs: None,
                exit_code: None,
            }),
            error: Some("boom".to_owned()),
        };
        assert!(both.into_result().is_err());

        // 两个都缺省 = 查询成功且未运行（线格式上省略字段是合法的）
        let empty: WireStatusEntry = serde_json::from_str("{}").expect("decode");
        assert_eq!(empty.into_result().expect("none"), None);
    }

    /// 两种已知启动方式都能编码（`TryFrom` 的正常分支）
    #[test]
    fn known_launch_kinds_encode() {
        for spec in [
            LaunchSpec::Jar {
                path: PathBuf::from("server.jar"),
            },
            LaunchSpec::Args {
                files: vec![PathBuf::from("a.txt")],
                args: vec![],
            },
        ] {
            assert!(WireLaunch::try_from(&spec).is_ok());
        }
    }
}
