use domain::server::launch::LaunchSpec;

/// Quilt 安装器产出的入口 JAR（无参数文件）
const QUILT_LAUNCH_JAR: &str = "quilt-server-launch.jar";
use domain::server::meta::{InstalledJar, ServerId, ServerMeta, ServerName};
use domain::server::settings::ServerSettings;
use domain::server::version::{ServerVariant, VersionId};
use domain::server::world::{DataVersion, ensure_world_readable};
use domain::workspace::{Workspace, WorkspaceId};
use port::{
    CancellationToken, CoreInspector, RepositoryError, ServerRepository, Sourced, TaskObserver,
    TaskStage, WorkspaceRepository,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

use crate::fs_error::{FsError, deletion_hint};
use crate::workspace::WorkspaceError;
use crate::{InstallError, InstalledDownload, ServerInstaller};

const SERVER_MARKER: &str = "mcsup.json";
const SERVER_MARKER_SCHEMA: u32 = 1;
/// 核心固定文件名（[`InstalledJar::path`] 的相对路径口径）
const SERVER_JAR: &str = "server.jar";
/// 安装暂存文件：先下载到这里，校验通过后再原子替换 `server.jar`
const STAGING_JAR: &str = ".server.jar.installing";
/// 安装器暂存文件名：安装器下载到这里，运行完即删除（不留在服务器目录）
const STAGING_INSTALLER: &str = ".forge-installer.jar";

/// 服务器注册操作失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServerRegistryError {
    /// 工作目录相关失败
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// 名称冲突（全局或目录已存在）
    #[error("server already exists: {0}")]
    AlreadyExists(String),
    /// 服务器不存在
    #[error("server not found: {0}")]
    NotFound(String),
    /// 文件系统 / marker 失败（共用载荷见 [`FsError`]）
    #[error(transparent)]
    Fs(#[from] FsError),
    /// 注册表失败
    #[error(transparent)]
    Repo(#[from] RepositoryError),
    /// 名称非法
    #[error(transparent)]
    InvalidName(#[from] domain::server::error::InvalidServerName),
    /// 目标位于某个工作区的 servers/ 子树内，禁止 link
    #[error("target is inside a workspace servers directory: {0}")]
    TargetInsideWorkspace(PathBuf),
    /// 就地注册时 `--workspace` 指向了别的工作区（目录物理归属不可改）
    #[error("target belongs to workspace `{owner}` but --workspace resolved to `{selected}`")]
    WorkspaceMismatch { owner: String, selected: String },
}

impl ServerRegistryError {
    /// 面向用户展示的分类；结构化错误不带文案，入口按此渲染提示
    ///
    /// 包装类变体（[`Self::Workspace`] / [`Self::Fs`] / [`Self::Repo`] /
    /// [`Self::InvalidName`]）各自有自己的 `Display` 或 `kind`，此处返回 `None`
    /// 让入口继续沿错误链找。
    #[must_use]
    pub fn kind(&self) -> Option<ServerRegistryErrorKind<'_>> {
        let kind = match self {
            Self::Workspace(_) | Self::Fs(_) | Self::Repo(_) | Self::InvalidName(_) => return None,
            Self::AlreadyExists(name) => ServerRegistryErrorKind::AlreadyExists { name },
            Self::NotFound(name) => ServerRegistryErrorKind::NotFound { name },
            Self::TargetInsideWorkspace(path) => {
                ServerRegistryErrorKind::TargetInsideWorkspace { path }
            }
            Self::WorkspaceMismatch { owner, selected } => {
                ServerRegistryErrorKind::WorkspaceMismatch { owner, selected }
            }
        };
        Some(kind)
    }
}

/// [`ServerRegistryError::kind`] 的结果：只带判定与参数，文案由入口渲染
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum ServerRegistryErrorKind<'a> {
    /// 名称冲突或目标目录已存在
    AlreadyExists {
        /// 服务器名
        name: &'a str,
    },
    /// 服务器不存在
    NotFound {
        /// 服务器名
        name: &'a str,
    },
    /// 目标位于某个工作区的 servers/ 内，禁止 link
    TargetInsideWorkspace {
        /// 目标路径
        path: &'a Path,
    },
    /// 目标属于另一个工作区
    WorkspaceMismatch {
        /// 目标所属工作区
        owner: &'a str,
        /// `--workspace` 解析出的工作区
        selected: &'a str,
    },
}

/// 服务端核心安装失败（注册表定位 + 下载编排）
///
/// [`Self::Inspect`] 与 [`Self::Downgrade`] 会被入口直接展示，文案由入口按
/// [`InstallJarError::kind`] 渲染；本层 `Display` 只用于日志与调试。
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum InstallJarError {
    /// 注册表失败（含服务器不存在）
    #[error(transparent)]
    Registry(#[from] ServerRegistryError),
    /// 来源、下载或校验失败
    #[error(transparent)]
    Install(#[from] InstallError),
    /// 世界或目标核心的元数据无法读取
    #[error("failed to inspect world or core metadata")]
    Inspect(#[from] port::InspectError),
    /// 目标核心低于世界数据版本：Minecraft 不支持降级世界
    #[error(
        "world data version {world} is newer than target core data version {target}; downgrading a world is not supported"
    )]
    Downgrade {
        /// 世界数据版本
        world: DataVersion,
        /// 目标核心数据版本
        target: DataVersion,
        /// 世界对应的版本名（缺失时为 `DataVersion N`）
        world_version: String,
        /// 用户选择的目标版本
        target_version: String,
    },
}

impl InstallJarError {
    /// 失败等级
    ///
    /// 包装变体**委托**；世界降级被拒是状态冲突（目标核心与既有世界矛盾，
    /// 换目标版本才能过），元数据读不出来是后端问题。
    #[must_use]
    pub fn category(&self) -> port::ErrorCategory {
        match self {
            Self::Registry(err) => err.category(),
            Self::Install(err) => err.category(),
            Self::Downgrade { .. } => port::ErrorCategory::Conflict,
            Self::Inspect(_) => port::ErrorCategory::Backend,
        }
    }

    /// 面向用户展示的分类；结构化错误不带文案，入口按此渲染提示
    #[must_use]
    pub fn kind(&self) -> Option<InstallJarErrorKind<'_>> {
        let kind = match self {
            Self::Registry(_) | Self::Install(_) => return None,
            Self::Inspect(_) => InstallJarErrorKind::Inspect,
            Self::Downgrade {
                world,
                target,
                world_version,
                target_version,
            } => InstallJarErrorKind::Downgrade {
                world: *world,
                target: *target,
                world_version,
                target_version,
            },
        };
        Some(kind)
    }
}

/// [`InstallJarError::kind`] 的结果：只带判定与参数，文案由入口渲染
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum InstallJarErrorKind<'a> {
    /// 世界或目标核心元数据无法读取
    Inspect,
    /// 目标核心低于世界数据版本
    Downgrade {
        /// 世界数据版本
        world: DataVersion,
        /// 目标核心数据版本
        target: DataVersion,
        /// 世界对应的版本名
        world_version: &'a str,
        /// 用户选择的目标版本
        target_version: &'a str,
    },
}

impl ServerRegistryError {
    /// 失败等级
    ///
    /// 包装变体**委托**给内层；名字非法与「目标在工作区 servers/ 内」是调用方
    /// 输入问题，名称冲突是状态冲突。
    #[must_use]
    pub fn category(&self) -> port::ErrorCategory {
        match self {
            Self::Workspace(err) => err.category(),
            Self::Fs(err) => err.category(),
            Self::Repo(err) => err.category(),
            Self::AlreadyExists(_) => port::ErrorCategory::Conflict,
            Self::NotFound(_) => port::ErrorCategory::NotFound,
            Self::InvalidName(_)
            | Self::TargetInsideWorkspace(_)
            | Self::WorkspaceMismatch { .. } => port::ErrorCategory::Usage,
        }
    }

    fn io(path: &Path, source: std::io::Error) -> Self {
        FsError::io(path, source).into()
    }

    fn marker(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        FsError::marker(path, message).into()
    }
}

/// `mcsup.json` 载荷
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerMarkerJson {
    schema: u32,
    id: String,
    name: String,
}

/// 已注册服务器 + 绝对目录（路径 = workspace.path / servers / `rel_dir`）
#[derive(Debug, Clone)]
pub struct RegisteredServer {
    pub meta: ServerMeta,
    pub dir: PathBuf,
    /// 目录是符号链接（`server link` 引入的外部服务器）
    pub linked: bool,
}

impl RegisteredServer {
    /// 定位这台服务器的身份键：身份 + 已解析目录
    ///
    /// supervisor 类 port 一律按身份键寻址，调用点统一走这里，避免各处手拼
    /// `ServerRef` 时漏填目录或拿错 id。
    #[must_use]
    pub fn server_ref(&self) -> port::ServerRef<'_> {
        port::ServerRef::local(&self.meta.id, &self.dir)
    }

    /// 是否为远端镜像条目（按**目录是否为空**推断）
    ///
    /// 只对 [`ServerRegistry::list_sourced`] 产出的条目成立：那里对非本地来源
    /// 一律把 `dir` 置空。
    ///
    /// **不要用它判断 [`ServerRegistry::get`] 的结果**：`get` 用工作区 `path`
    /// 拼目录（`absolute_dir`），而远端工作区
    /// 的 `path` 是对端的真实路径（如 `/root/ws`），在客户端上并不存在却是
    /// 非空字符串——据此判断会把远端条目当成本地条目。要判定来源请用
    /// [`ServerRegistry::get_sourced`] 的 `origin`。
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.dir.as_os_str().is_empty()
    }

    /// 远端实现用的身份引用（只带 id，不带目录）
    ///
    /// 对本地条目调用也安全，只是结果不含目录：本方法**不**校验条目来源，
    /// 需要区分时先看 [`Self::is_remote`]。
    #[must_use]
    pub fn remote_ref(&self) -> port::ServerRef<'_> {
        port::ServerRef::remote(&self.meta.id)
    }
}

/// `server remove` 结果
#[derive(Debug, Clone)]
pub struct RemoveOutcome {
    /// 被注销的服务器
    pub server: RegisteredServer,
    /// 目录是否被本次调用实际删除
    ///
    /// `false` 表示本次没有删除动作：未带 `purge`、链接服务器（只删链接），
    /// 或普通目录本就不存在。入口据此区分「删成功」与「无事可做」。
    pub disk_purged: bool,
}

impl RemoveOutcome {
    /// 目录是否被本次调用实际删除
    #[must_use]
    pub fn disk_purged(&self) -> bool {
        self.disk_purged
    }
}

/// `server link` 结果
#[derive(Debug, Clone)]
pub struct LinkedServer {
    /// 注册后的服务器
    pub server: RegisteredServer,
    /// 目标真实路径（canonicalize 后）
    pub target: PathBuf,
}

/// 核心安装成功的结果
#[derive(Debug, Clone)]
pub struct InstalledServerJar {
    /// 回写核心元数据后的服务器
    pub server: RegisteredServer,
    /// 写入的元数据
    pub jar: InstalledJar,
    /// 核心绝对路径（单 JAR 启动时的 JAR；参数文件启动时为第一个参数文件）
    pub destination: PathBuf,
    /// 本次安装确定的启动方式
    pub launch: LaunchSpec,
}

/// 服务器注册表编排：归属工作目录、目录创建、marker、注册
pub struct ServerRegistry {
    servers: Arc<dyn ServerRepository>,
    workspaces: Arc<dyn WorkspaceRepository>,
}

impl std::fmt::Debug for ServerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerRegistry").finish_non_exhaustive()
    }
}

impl ServerRegistry {
    /// 组合两个注册表
    #[must_use]
    pub fn new(
        servers: Arc<dyn ServerRepository>,
        workspaces: Arc<dyn WorkspaceRepository>,
    ) -> Self {
        Self {
            servers,
            workspaces,
        }
    }

    /// 在所选工作目录下创建服务器：目录 + `mcsup.json` + 入库
    ///
    /// # Errors
    ///
    /// 名称全局冲突、工作目录不存在、IO / marker / 注册表失败时返回
    /// [`ServerRegistryError`]
    pub fn create(
        &self,
        workspace_selector: Option<&str>,
        name: &str,
        version: &VersionId,
        variant: ServerVariant,
        java_major: Option<domain::java::JavaMajor>,
    ) -> Result<RegisteredServer, ServerRegistryError> {
        let now = crate::atomic::unix_now();
        let name = ServerName::parse(name)?;
        let ws = self.resolve_workspace(workspace_selector)?;

        if self.servers.get(&name).is_ok() {
            return Err(ServerRegistryError::AlreadyExists(name.as_str().to_owned()));
        }

        let dir = ws.path.join("servers").join(name.as_str());
        // symlink_metadata：悬空符号链接在 exists() 下返回 false，
        // 用 exists() 会把悬空链接当成「目录不存在」然后覆盖写入
        if dir.symlink_metadata().is_ok() {
            return Err(ServerRegistryError::AlreadyExists(name.as_str().to_owned()));
        }
        crate::fs::create_dir_all(&dir).map_err(|err| ServerRegistryError::io(&dir, err))?;

        let id = ServerId::parse(&uuid::Uuid::new_v4().to_string())
            .map_err(|err| ServerRegistryError::marker(dir.join(SERVER_MARKER), err.to_string()))?;
        write_server_marker(
            &dir,
            &ServerMarkerJson {
                schema: SERVER_MARKER_SCHEMA,
                id: id.as_str().to_owned(),
                name: name.as_str().to_owned(),
            },
        )?;

        let rel_dir = PathBuf::from(name.as_str());
        let meta = ServerMeta {
            id,
            name,
            workspace_id: ws.id.clone(),
            rel_dir,
            version: version.clone(),
            variant,
            jar: None,
            launch: None,
            settings: ServerSettings::default(),
            java_major,
            created_at: now,
            updated_at: now,
        };
        self.servers.create(&meta)?;
        Ok(RegisteredServer {
            meta,
            dir,
            linked: false,
        })
    }

    /// 为新目录写入一份全新身份（`backup restore --to` 用）
    ///
    /// 恢复只复制文件，不复制身份：快照里的 `mcsup.json` 属于源服务器，
    /// 沿用会让新目录与源服务器撞 id，`server link` 必然失败。这里生成新
    /// id 并写进目标目录，之后用户照常 `server link <dir>` 即可。
    ///
    /// 名称已注册时拒绝，避免写出一个永远注册不上的目录。
    ///
    /// # Errors
    ///
    /// 名称已注册、marker 写入失败时返回 [`ServerRegistryError`]
    pub fn assign_new_identity(
        &self,
        dir: &Path,
        name: &ServerName,
    ) -> Result<ServerId, ServerRegistryError> {
        if self.servers.get(name).is_ok() {
            return Err(ServerRegistryError::AlreadyExists(name.as_str().to_owned()));
        }
        let id = ServerId::parse(&uuid::Uuid::new_v4().to_string())
            .map_err(|err| ServerRegistryError::marker(dir.join(SERVER_MARKER), err.to_string()))?;
        write_server_marker(
            dir,
            &ServerMarkerJson {
                schema: SERVER_MARKER_SCHEMA,
                id: id.as_str().to_owned(),
                name: name.as_str().to_owned(),
            },
        )?;
        Ok(id)
    }

    /// 把服务器目录注册进工作区：外部目录建符号链接，工作区内目录就地注册
    ///
    /// 外部目标在 `<ws>/servers/<name>` 建指向 `target` 的符号链接，
    /// `target` 不能位于任何工作区的 `servers/` 子树内（防环）。目标是某工作区
    /// `servers/` 的直接子目录时不建链接、直接就地注册——恢复「目录还在、
    /// 注册已删」的服务器；名称一律以目录名为准，与 `--name` 或 marker 名
    /// 不一致时直接拒绝，不做猜测。更深的子树仍拒绝（`rel_dir` 只有单层）。
    /// 目标目录带合法 `mcsup.json` 时复用其 id，否则写入新 id。符号链接
    /// 路径上注册表写失败时回滚链接。
    ///
    /// # Errors
    ///
    /// 名称冲突、目标在工作区内、`--workspace` 与目录归属不一致、marker 损坏 /
    /// id 已注册、IO / 注册表失败时返回 [`ServerRegistryError`]
    #[allow(clippy::too_many_lines)]
    pub fn link(
        &self,
        target: &Path,
        name: Option<&str>,
        workspace_selector: Option<&str>,
    ) -> Result<LinkedServer, ServerRegistryError> {
        let now = crate::atomic::unix_now();
        let target =
            crate::fs::canonicalize(target).map_err(|err| ServerRegistryError::io(target, err))?;
        if !target.is_dir() {
            return Err(ServerRegistryError::Fs(FsError::marker(
                target.clone(),
                "target is not a directory",
            )));
        }
        // 工作区内 servers/ 的直接子目录：就地注册（不建符号链接）
        if let Some(owner) = self.in_place_owner(&target)? {
            return self.register_in_place(target, name, workspace_selector, &owner, now);
        }
        self.reject_target_inside_workspace_servers(&target)?;

        let ws = self.resolve_workspace(workspace_selector)?;
        let marker = read_server_marker(&target)?;
        let name = match (name, marker.as_ref()) {
            (Some(raw), _) => ServerName::parse(raw)?,
            (None, Some(m)) => ServerName::parse(&m.name)?,
            (None, None) => {
                let base = target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .ok_or_else(|| {
                        ServerRegistryError::Fs(FsError::marker(
                            target.clone(),
                            "cannot derive server name from path",
                        ))
                    })?;
                ServerName::parse(&base)?
            }
        };

        if self.servers.get(&name).is_ok() {
            return Err(ServerRegistryError::AlreadyExists(name.as_str().to_owned()));
        }
        if let Some(m) = marker.as_ref() {
            let id = ServerId::parse(&m.id).map_err(|err| {
                ServerRegistryError::marker(target.join(SERVER_MARKER), err.to_string())
            })?;
            if self
                .servers
                .list()?
                .iter()
                .any(|existing| existing.id == id)
            {
                return Err(ServerRegistryError::AlreadyExists(name.as_str().to_owned()));
            }
        }

        let link_path = ws.path.join("servers").join(name.as_str());
        if link_path.symlink_metadata().is_ok() {
            return Err(ServerRegistryError::AlreadyExists(name.as_str().to_owned()));
        }
        create_dir_all_parent(&link_path)?;
        symlink_dir(&target, &link_path)?;

        let (id, marker_name) = if let Some(m) = marker {
            (
                ServerId::parse(&m.id).map_err(|err| {
                    ServerRegistryError::marker(target.join(SERVER_MARKER), err.to_string())
                })?,
                m.name,
            )
        } else {
            let id = ServerId::parse(&uuid::Uuid::new_v4().to_string()).map_err(|err| {
                ServerRegistryError::marker(target.join(SERVER_MARKER), err.to_string())
            })?;
            write_server_marker(
                &target,
                &ServerMarkerJson {
                    schema: SERVER_MARKER_SCHEMA,
                    id: id.as_str().to_owned(),
                    name: name.as_str().to_owned(),
                },
            )?;
            (id, name.as_str().to_owned())
        };
        // marker 名与 --name 不一致时以 marker 为准入库，避免一个 id 两个名字
        let meta_name = ServerName::parse(&marker_name)?;
        let rel_dir = PathBuf::from(meta_name.as_str());
        let meta = ServerMeta {
            id,
            name: meta_name,
            workspace_id: ws.id.clone(),
            rel_dir,
            version: VersionId::new("unknown"),
            variant: ServerVariant::Unknown,
            jar: None,
            launch: None,
            settings: ServerSettings::default(),
            java_major: None,
            created_at: now,
            updated_at: now,
        };
        if let Err(err) = self.servers.create(&meta) {
            // 注册表失败：回滚符号链接，避免磁盘有链接、库里没记录
            let _ = remove_link_or_dir(&link_path);
            return Err(err.into());
        }
        Ok(LinkedServer {
            server: RegisteredServer {
                meta,
                dir: link_path,
                linked: true,
            },
            target,
        })
    }

    /// 注销服务器
    ///
    /// 符号链接（`server link` 引入）：仅删除链接与登记，绝不触碰目标目录。
    /// `purge` 对链接服务器直接拒绝——目标在工作区外，删了无法恢复。
    /// 普通目录：默认仅注销保留磁盘；`purge` 时先删目录再注销。
    ///
    /// `cwd` 是调用方进程的工作目录，位于待删目录内时拒绝
    /// （[`WorkspaceError::CwdInsideTarget`]，与 `workspace remove --purge` 同一
    /// 判定）：Windows 上子进程改不了父进程的工作目录，删掉后调用方终端会停在
    /// 已不存在的路径上。
    ///
    /// # Errors
    ///
    /// 不存在、链接服务器带 `purge`、`cwd` 在待删目录内、IO 或注册表失败时
    /// 返回 [`ServerRegistryError`]
    pub fn remove(
        &self,
        name: &str,
        purge: bool,
        cwd: &Path,
    ) -> Result<RemoveOutcome, ServerRegistryError> {
        let srv = self.get(name)?;
        let mut disk_purged = false;
        if srv.linked {
            if purge {
                return Err(ServerRegistryError::Fs(FsError::marker(
                    srv.dir.clone(),
                    "refusing to purge a linked server; delete the target manually \
                     and run `server remove` without --purge",
                )));
            }
            // 链接不是目录：删掉的是链接本身，目标目录仍在磁盘上
            remove_symlink(&srv.dir).map_err(|err| ServerRegistryError::io(&srv.dir, err))?;
        } else if purge {
            // 前置闸：目录本就不存在时放行（没有可删的对象，注销登记即可，幂等）。
            // 不能报成删成功，也不能因此拒绝——记录才是要清理的东西。
            // 目录由 `ws.path/servers/<rel_dir>` 拼出，记录被手改后可能指向工作区
            // 外，删除前必须拿到文件系统证据（`mcsup.json` 与该记录同 id）。
            self.ensure_purgeable(name, cwd)?;
            if srv.dir.exists() {
                crate::fs::remove_dir_all(&srv.dir)
                    .map_err(|err| ServerRegistryError::io(&srv.dir, deletion_hint(err)))?;
                disk_purged = true;
            }
        }
        let name = srv.meta.name.clone();
        self.servers.remove(&name)?;
        Ok(RemoveOutcome {
            server: srv,
            disk_purged,
        })
    }

    /// 删盘前的前置校验：该服务器现在能否执行 `remove` 的删盘部分
    ///
    /// 只做判定、不改任何状态，供入口在弹「删除磁盘数据?」之前调用：交互入口
    /// 先问再拒，用户会在确认框与手输名称之后才看到拒绝原因，白走一遍。
    /// 判定与 [`ServerRegistry::remove`] 的删盘分支一致：
    ///
    /// - 链接服务器带 `purge` → 拒绝（目标在工作区外）
    /// - 目录不存在 → 放行（`remove` 此时只清登记）
    /// - `cwd` 在目录内 → [`WorkspaceError::CwdInsideTarget`]
    /// - 目录里的 `mcsup.json` 不属于该记录 → 拒绝
    ///
    /// # Errors
    ///
    /// 不存在，或上述任一条件不满足时返回 [`ServerRegistryError`]
    pub fn ensure_purgeable(&self, name: &str, cwd: &Path) -> Result<(), ServerRegistryError> {
        let srv = self.get(name)?;
        if srv.linked {
            return Err(ServerRegistryError::Fs(FsError::marker(
                srv.dir.clone(),
                "refusing to purge a linked server; delete the target manually \
                 and run `server remove` without --purge",
            )));
        }
        if !srv.dir.exists() {
            return Ok(());
        }
        crate::workspace::ensure_outside(cwd, &srv.dir)?;
        if !server_dir_matches(&srv.dir, &srv.meta.id) {
            return Err(ServerRegistryError::Fs(FsError::marker(
                srv.dir.clone(),
                format!(
                    "refusing to purge: {} does not hold a {SERVER_MARKER} \
                     belonging to server {}; fix the record or delete it manually",
                    srv.dir.display(),
                    srv.meta.name.as_str()
                ),
            )));
        }
        Ok(())
    }

    /// 全部服务器，按名称升序，附绝对目录
    ///
    /// # Errors
    ///
    /// 注册表失败或路径无法拼接时返回 [`ServerRegistryError`]
    pub fn list(&self) -> Result<Vec<RegisteredServer>, ServerRegistryError> {
        let mut out = Vec::new();
        for meta in self.servers.list()? {
            let dir = self.absolute_dir(&meta)?;
            let linked = is_linked(&dir);
            out.push(RegisteredServer { meta, dir, linked });
        }
        Ok(out)
    }

    /// 列出全部服务并标注来源
    ///
    /// 组合仓库（本地 + 远端）下每条都带真实来源；纯本地实现全部是
    /// [`port::Origin::Local`]。`linked` 是本地文件系统探测，远端条目恒为 `false`
    /// ——符号链接是客户端侧的展示细节，对端无从得知。
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`ServerRegistryError`]
    pub fn list_sourced(&self) -> Result<Vec<Sourced<RegisteredServer>>, ServerRegistryError> {
        let mut out = Vec::new();
        for sourced in self.servers.list_sourced()? {
            let origin = sourced.origin;
            let meta = sourced.value;
            let (dir, linked) = if origin.is_local() {
                let dir = self.absolute_dir(&meta)?;
                let linked = is_linked(&dir);
                (dir, linked)
            } else {
                (PathBuf::new(), false)
            };
            out.push(Sourced::new(origin, RegisteredServer { meta, dir, linked }));
        }
        Ok(out)
    }

    /// 按名解析服务器（附来源）
    ///
    /// # Errors
    ///
    /// 不存在时返回 [`ServerRegistryError`]
    pub fn get_sourced(
        &self,
        name: &str,
    ) -> Result<Sourced<RegisteredServer>, ServerRegistryError> {
        // 限定名（origin/name）在跨来源解析层统一处理；裸名保持「精确命中、
        // 唯一前缀」的既有语义之外，这里只做精确匹配（列出与歧义错误由
        // `--origin` 过滤与 resolve_index 承担）
        let selector = crate::origin::Selector::parse(name);
        let want = ServerName::parse(selector.name())
            .map_err(|_| ServerRegistryError::NotFound(name.to_owned()))?;
        // `local/名称` 是合法限定（`workspace use local/ws` 已有先例），不能
        // 因为「本地本来就不歧义」就拒绝——限定名在两类来源间必须对称，
        // 否则用户写 `local/foo` 拿到的是「没找到」而不是那条本地记录。
        let filter = match selector.origin() {
            Some(port::Origin::Remote(remote_name)) => {
                crate::origin::OriginFilter::Named(remote_name.clone())
            }
            Some(port::Origin::Local) => crate::origin::OriginFilter::LocalOnly,
            None => crate::origin::OriginFilter::All,
        };
        for sourced in self.list_sourced()? {
            if sourced.value.meta.name != want || !filter.matches(&sourced.origin) {
                continue;
            }
            return Ok(sourced);
        }
        Err(ServerRegistryError::NotFound(name.to_owned()))
    }

    /// 按名解析服务器
    ///
    /// 接受裸名，也接受 `远端/名称`（`local/名称`）限定名：带来源时交给
    /// [`Self::get_sourced`]。限定名的**唯一**作用就是消掉跨来源同名歧义，
    /// 只认名字会让它形同虚设——本地有同名时永远命中本地那条。
    ///
    /// # Errors
    ///
    /// 不存在、来源未配对或工作目录缺失时返回 [`ServerRegistryError`]
    pub fn get(&self, name: &str) -> Result<RegisteredServer, ServerRegistryError> {
        if crate::origin::Selector::parse(name).origin().is_some() {
            return self.get_sourced(name).map(|sourced| sourced.value);
        }
        let name =
            ServerName::parse(name).map_err(|_| ServerRegistryError::NotFound(name.to_owned()))?;
        let meta = self
            .servers
            .get(&name)
            .map_err(|_| ServerRegistryError::NotFound(name.as_str().to_owned()))?;
        let dir = self.absolute_dir(&meta)?;
        let linked = is_linked(&dir);
        Ok(RegisteredServer { meta, dir, linked })
    }

    /// 回写核心元数据（版本 + jar + 该版本要求的 Java 大版本），安装完成后调用
    ///
    /// # Errors
    ///
    /// 不存在或注册表失败时返回 [`ServerRegistryError`]
    pub fn update_core(
        &self,
        name: &str,
        version: &VersionId,
        jar: &InstalledJar,
        launch: Option<&LaunchSpec>,
        required_java_major: Option<domain::java::JavaMajor>,
        updated_at: i64,
    ) -> Result<RegisteredServer, ServerRegistryError> {
        let name =
            ServerName::parse(name).map_err(|_| ServerRegistryError::NotFound(name.to_owned()))?;
        let meta = self.servers.update_core(
            &name,
            version,
            jar,
            launch,
            required_java_major,
            updated_at,
        )?;
        let dir = self.absolute_dir(&meta)?;
        let linked = is_linked(&dir);
        Ok(RegisteredServer { meta, dir, linked })
    }

    /// 回写服务属性（`server set` 调用）
    ///
    /// # Errors
    ///
    /// 不存在或注册表失败时返回 [`ServerRegistryError`]
    pub fn update_settings(
        &self,
        name: &str,
        settings: &ServerSettings,
    ) -> Result<RegisteredServer, ServerRegistryError> {
        let name =
            ServerName::parse(name).map_err(|_| ServerRegistryError::NotFound(name.to_owned()))?;
        let meta = self
            .servers
            .update_settings(&name, settings, crate::atomic::unix_now())?;
        let dir = self.absolute_dir(&meta)?;
        let linked = is_linked(&dir);
        Ok(RegisteredServer { meta, dir, linked })
    }

    /// 给既有服务器安装核心
    ///
    /// 两条路径按来源声明的 [`port::InstallKind`] 分流：
    ///
    /// - **直接可运行**（原版 / Fabric / Paper）：下载到暂存文件 → 校验数据版本
    ///   → 原子替换 `server.jar` → 回写元数据。先下到暂存文件是为了在覆盖旧核心
    ///   前完成降级检查：校验不过时旧核心原样保留。
    /// - **安装器**（Forge / NeoForge）：把安装器下到服务器目录并就地运行，产出
    ///   `libraries/` 与参数文件；启动方式记为 [`LaunchSpec::Args`]。安装器完成后
    ///   即删除，不留在服务器目录。
    ///
    /// 目标目录由注册表回答，入口不再拿当前工作目录当目标。
    ///
    /// # Errors
    ///
    /// 服务器不存在返回 [`InstallJarError::Registry`]；来源、下载、校验或安装器
    /// 执行失败返回 [`InstallJarError::Install`]；元数据读取失败返回
    /// [`InstallJarError::Inspect`]；目标版本低于世界数据版本返回
    /// [`InstallJarError::Downgrade`]
    #[allow(clippy::too_many_arguments)]
    pub async fn install_jar(
        &self,
        name: &str,
        version: &VersionId,
        installer: &ServerInstaller,
        inspector: &dyn CoreInspector,
        installer_ctx: Option<&crate::InstallerContext>,
        observer: Option<&dyn TaskObserver>,
        cancel: &CancellationToken,
    ) -> Result<InstalledServerJar, InstallJarError> {
        let srv = self.get(name)?;

        // 安装器路径：下载物是安装器而非服务端。先试这条路，拿不到产物
        // （来源其实是直接可运行的 jar）时清掉临时文件，回落到下方直接路径。
        if let Some(ctx) = installer_ctx
            && let Some(installed) = self
                .install_via_installer(
                    name,
                    version,
                    srv.meta.variant,
                    installer,
                    ctx,
                    &srv.dir,
                    observer,
                    cancel,
                )
                .await?
        {
            return Ok(installed);
        }

        // —— 直接可运行的服务端：下载到暂存文件 → 降级检查 → 原子替换 ——
        let destination = srv.dir.join(SERVER_JAR);
        let staging = srv.dir.join(STAGING_JAR);
        let _ = crate::fs::remove_file(&staging);

        let world = inspector.world_info(&srv.dir)?;
        let out: InstalledDownload = installer
            .install(version, staging.clone(), None, observer, cancel)
            .await?;
        let target = match inspector.jar_data_version(&staging) {
            Ok(target) => target,
            Err(err) => {
                let _ = crate::fs::remove_file(&staging);
                return Err(err.into());
            }
        };
        if let Some(target) = target
            && let Some(world) = world.as_ref()
            && let Some(world_version) = world.data_version
            && let Err(downgrade) = ensure_world_readable(world_version, target)
        {
            let _ = crate::fs::remove_file(&staging);
            return Err(InstallJarError::Downgrade {
                world: downgrade.world,
                target: downgrade.target,
                world_version: world
                    .version_name
                    .clone()
                    .unwrap_or_else(|| format!("DataVersion {}", downgrade.world)),
                target_version: version.to_string(),
            });
        }
        if let Err(err) = crate::fs::rename(&staging, &destination) {
            let _ = crate::fs::remove_file(&staging);
            return Err(ServerRegistryError::io(&destination, err).into());
        }

        let jar = InstalledJar {
            path: PathBuf::from(SERVER_JAR),
            size: out.artifact.size,
            checksum: out.checksum,
        };
        let launch = LaunchSpec::jar(SERVER_JAR);
        let srv = self.update_core(
            name,
            version,
            &jar,
            Some(&launch),
            out.java_major,
            crate::atomic::unix_now(),
        )?;
        Ok(InstalledServerJar {
            server: srv,
            jar,
            destination,
            launch,
        })
    }

    /// 安装器路径：下载安装器到服务器目录 → 就地运行 → 记录参数文件启动方式
    ///
    /// 返回 `Ok(None)` 表示本次来源不是安装器（调用方继续走直接下载路径）；
    /// `Ok(Some(..))` 表示安装器安装已完成。
    // 参数同样是编排必需的输入（目标、版本、下载器、安装器上下文、目录、进度、
    // 取消），与 [`Self::install_jar`] 保持同一套平铺签名
    #[allow(clippy::too_many_arguments)]
    async fn install_via_installer(
        &self,
        name: &str,
        version: &VersionId,
        variant: ServerVariant,
        installer: &ServerInstaller,
        ctx: &crate::InstallerContext,
        dir: &Path,
        observer: Option<&dyn TaskObserver>,
        cancel: &CancellationToken,
    ) -> Result<Option<InstalledServerJar>, InstallJarError> {
        let installer_path = dir.join(STAGING_INSTALLER);
        let _ = crate::fs::remove_file(&installer_path);
        // 无论成功失败都清掉安装器 JAR：它不是服务端的一部分，留在目录里会
        // 污染备份，也会让下次安装误以为「已经下过」。失败路径同样要清——
        // 否则一次网络中断就在服务器目录留下一个 9MB 的死文件。
        let out = installer
            .install(version, installer_path.clone(), Some(ctx), observer, cancel)
            .await;
        let _ = crate::fs::remove_file(&installer_path);
        let out = out?;
        if !out.install.runs_installer() {
            // 不是安装器：临时下载已清，交回调用方走直接路径
            return Ok(None);
        }

        if let Some(observer) = observer {
            observer.on_stage(TaskStage::Finalizing);
        }
        // 参数文件所在目录名是安装器产物的完整 build 串（`26.3-66.1.0`），
        // 不是 `VersionId` 里的 MC 版本（`26.3`）——拿 MC 版本拼路径必然落空，
        // 只能找到 `user_jvm_args.txt`，启动时就少了平台参数文件。
        let build = out.build.as_deref().unwrap_or_else(|| version.as_str());

        // Quilt 的产物形态不同：入口是单个 `quilt-server-launch.jar`，没有
        // `user_jvm_args.txt`，也没有 `*_args.txt` 参数文件。用 Forge 的判据查
        // 它必然落空，把「装好了」报成 Layout 不全。
        if variant == ServerVariant::Quilt {
            let entry = PathBuf::from(QUILT_LAUNCH_JAR);
            if !dir.join(&entry).is_file() {
                return Err(InstallError::Layout(format!(
                    "安装器完成后在 {} 找不到 {QUILT_LAUNCH_JAR}",
                    dir.display()
                ))
                .into());
            }
            let jar = InstalledJar {
                path: entry.clone(),
                size: crate::fs::metadata(dir.join(&entry)).map_or(0, |m| m.len()),
                checksum: out.checksum.clone(),
            };
            let launch = LaunchSpec::jar(entry.clone());
            let srv = self.update_core(
                name,
                version,
                &jar,
                Some(&launch),
                out.java_major,
                crate::atomic::unix_now(),
            )?;
            return Ok(Some(InstalledServerJar {
                destination: dir.join(&entry),
                server: srv,
                jar,
                launch,
            }));
        }

        let files = loader_args_files(&variant, build, dir);
        if files.is_empty() {
            return Err(InstallError::Layout(format!(
                "安装器完成后在 {} 找不到启动参数文件",
                dir.display()
            ))
            .into());
        }
        let launch = LaunchSpec::args(files.iter().cloned());
        // 参数文件启动没有单一核心 JAR：以首个参数文件作「核心」记录，
        // 大小与 SHA1 记安装器下载值（安装产物本身由 libraries/ 表达）
        let jar = InstalledJar {
            path: files[0].clone(),
            size: out.artifact.size,
            checksum: out.checksum,
        };
        let srv = self.update_core(
            name,
            version,
            &jar,
            Some(&launch),
            out.java_major,
            crate::atomic::unix_now(),
        )?;
        Ok(Some(InstalledServerJar {
            destination: dir.join(&files[0]),
            server: srv,
            jar,
            launch,
        }))
    }
}

/// 安装器产出的启动参数文件（相对服务器目录，按展开顺序）
///
/// 布局判定在 `domain::server::launch`：那是「该变体怎么启动」，`runtime` 也要
/// 用同一份。`service` 只做委托，不内置一份「Forge 目录长什么样」的副本。
fn loader_args_files(variant: &ServerVariant, build: &str, dir: &Path) -> Vec<PathBuf> {
    domain::server::launch::installer_args_files(&crate::fs_probe::FsDirProbe, variant, build, dir)
}

impl ServerRegistry {
    /// 某工作目录下的服务器数量（注册表口径，与 `server list` 同源）
    ///
    /// `workspace list` 的「服务器数」用这个，不要在入口数磁盘目录——
    /// 残留目录与漏入库会让两边数字分叉。
    ///
    /// # Errors
    ///
    /// 注册表失败时返回 [`ServerRegistryError`]
    pub fn count_in(&self, workspace_id: &WorkspaceId) -> Result<usize, ServerRegistryError> {
        Ok(self
            .servers
            .list()?
            .iter()
            .filter(|meta| &meta.workspace_id == workspace_id)
            .count())
    }

    /// 一次读取服务器列表，按工作区统计数量
    ///
    /// 列表页需要对每个工作区显示服务器数；逐行调用 [`Self::count_in`] 会把
    /// 同一份服务器列表（远端还要走 RPC）重复拉 M 次，离线远端上退化成
    /// M×N 次建连等待。这里只读一次，调用方按 id 查表。
    ///
    /// # Errors
    ///
    /// 注册表失败时返回 [`ServerRegistryError`]
    pub fn counts_by_workspace(
        &self,
    ) -> Result<std::collections::HashMap<WorkspaceId, usize>, ServerRegistryError> {
        let mut counts: std::collections::HashMap<WorkspaceId, usize> =
            std::collections::HashMap::new();
        for meta in self.servers.list()? {
            *counts.entry(meta.workspace_id).or_insert(0) += 1;
        }
        Ok(counts)
    }

    fn resolve_workspace(&self, selector: Option<&str>) -> Result<Workspace, ServerRegistryError> {
        match selector {
            Some(sel) => {
                let all = self.workspaces.list()?;
                super::workspace::resolve_selector(&all, sel)
                    .map_err(ServerRegistryError::Workspace)
            }
            None => self
                .workspaces
                .current()?
                .ok_or_else(|| ServerRegistryError::Workspace(WorkspaceError::NoCurrent)),
        }
    }

    fn absolute_dir(&self, meta: &ServerMeta) -> Result<PathBuf, ServerRegistryError> {
        let ws = self.workspaces.get(&meta.workspace_id).map_err(|_| {
            ServerRegistryError::Workspace(WorkspaceError::NotFound(
                meta.workspace_id.as_str().to_owned(),
            ))
        })?;
        Ok(ws.path.join("servers").join(&meta.rel_dir))
    }
    /// 目标在任一工作区的 `servers/` 子树内时拒绝：通过 servers/ 再链回
    /// servers/ 会成环，盘点与删除都会失控
    fn reject_target_inside_workspace_servers(
        &self,
        target: &Path,
    ) -> Result<(), ServerRegistryError> {
        for ws in self.workspaces.list()? {
            let servers_dir = ws.path.join("servers");
            // 与 `workspace remove --purge` 同一份包含判定：各写一份就会出现
            // 一处解析符号链接、另一处不解析，`/var` 与 `/private/var` 被当成
            // 两个目录，防环检查形同虚设
            if crate::workspace::is_within(target, &servers_dir) {
                return Err(ServerRegistryError::TargetInsideWorkspace(servers_dir));
            }
        }
        Ok(())
    }

    /// 目标是某个工作区 `servers/` 的直接子目录时返回该工作区
    ///
    /// 只认直接子目录：更深的子树无法用单层 `rel_dir` 表达，仍交给防环拒绝。
    fn in_place_owner(&self, target: &Path) -> Result<Option<Workspace>, ServerRegistryError> {
        for ws in self.workspaces.list()? {
            let servers_dir = ws.path.join("servers");
            // target 已 canonicalize；servers_dir 同口径比较，避免前缀 / 大小写误判
            let servers_dir = crate::fs::canonicalize(&servers_dir).unwrap_or(servers_dir);
            if target.parent() == Some(servers_dir.as_path()) {
                return Ok(Some(ws));
            }
        }
        Ok(None)
    }

    /// 就地注册工作区内目录：目录存在但注册缺失时经 `server link` 恢复
    ///
    /// 不建符号链接、不搬动目录，只补一条注册行。名称一律以目录名为准——
    /// `rel_dir` 必须与磁盘目录名一致，否则 doctor 盘点与目录推导都会错位；
    /// `--name` 与目录名不一致、marker 名与目录名不一致时拒绝，不猜测意图。
    /// marker 的 id 已被占用时同样拒绝，防止一个 id 两个目录。
    fn register_in_place(
        &self,
        target: PathBuf,
        name: Option<&str>,
        workspace_selector: Option<&str>,
        owner: &Workspace,
        now: i64,
    ) -> Result<LinkedServer, ServerRegistryError> {
        // 目录物理归属不可改：--workspace 只能与归属一致
        if let Some(sel) = workspace_selector {
            let selected = self.resolve_workspace(Some(sel))?;
            if selected.id != owner.id {
                return Err(ServerRegistryError::WorkspaceMismatch {
                    owner: owner.name.as_str().to_owned(),
                    selected: selected.name.as_str().to_owned(),
                });
            }
        }
        let dir_name = in_place_dir_name(&target, name)?;
        if self.servers.get(&dir_name).is_ok() {
            return Err(ServerRegistryError::AlreadyExists(
                dir_name.as_str().to_owned(),
            ));
        }
        let marker = read_server_marker(&target)?;
        let id = self.in_place_id(&target, &dir_name, marker.as_ref())?;
        let meta = ServerMeta {
            id,
            name: dir_name.clone(),
            workspace_id: owner.id.clone(),
            rel_dir: PathBuf::from(dir_name.as_str()),
            version: VersionId::new("unknown"),
            variant: ServerVariant::Unknown,
            jar: None,
            launch: None,
            settings: ServerSettings::default(),
            java_major: None,
            created_at: now,
            updated_at: now,
        };
        self.servers.create(&meta)?;
        Ok(LinkedServer {
            server: RegisteredServer {
                meta,
                dir: target.clone(),
                linked: false,
            },
            target,
        })
    }

    /// marker 的 id 复用与冲突检查：无 marker 写入新 id；marker 名与目录名
    /// 不一致、id 已被占用时拒绝，防止一个 id 两个目录
    fn in_place_id(
        &self,
        target: &Path,
        dir_name: &ServerName,
        marker: Option<&ServerMarkerJson>,
    ) -> Result<ServerId, ServerRegistryError> {
        let Some(m) = marker else {
            let id = ServerId::parse(&uuid::Uuid::new_v4().to_string()).map_err(|err| {
                ServerRegistryError::marker(target.join(SERVER_MARKER), err.to_string())
            })?;
            write_server_marker(
                target,
                &ServerMarkerJson {
                    schema: SERVER_MARKER_SCHEMA,
                    id: id.as_str().to_owned(),
                    name: dir_name.as_str().to_owned(),
                },
            )?;
            return Ok(id);
        };
        let id = ServerId::parse(&m.id).map_err(|err| {
            ServerRegistryError::marker(target.join(SERVER_MARKER), err.to_string())
        })?;
        if self
            .servers
            .list()?
            .iter()
            .any(|existing| existing.id == id)
        {
            return Err(ServerRegistryError::AlreadyExists(
                dir_name.as_str().to_owned(),
            ));
        }
        let marker_name = ServerName::parse(&m.name)?;
        if marker_name != *dir_name {
            return Err(ServerRegistryError::marker(
                target.join(SERVER_MARKER),
                format!(
                    "marker name `{}` does not match directory name `{}`; \
                     fix {} or rename the directory first",
                    marker_name.as_str(),
                    dir_name.as_str(),
                    SERVER_MARKER
                ),
            ));
        }
        Ok(id)
    }
}

fn write_server_marker(dir: &Path, marker: &ServerMarkerJson) -> Result<(), ServerRegistryError> {
    let path = dir.join(SERVER_MARKER);
    let text = serde_json::to_string_pretty(marker)
        .map_err(|err| ServerRegistryError::marker(path.clone(), err.to_string()))?;
    // 原子替换：marker 被截断等于服务器身份丢失，`server link` 再也接不回来
    crate::atomic::write_atomic(&path, &text)?;
    Ok(())
}

/// 就地注册的名称口径：以目录名为准，`--name` 不一致时拒绝
///
/// `rel_dir` 必须与磁盘目录名一致，否则 doctor 盘点与目录推导都会错位。
fn in_place_dir_name(target: &Path, name: Option<&str>) -> Result<ServerName, ServerRegistryError> {
    let dir_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| {
            ServerRegistryError::Fs(FsError::marker(
                target.to_path_buf(),
                "cannot derive server name from path",
            ))
        })?;
    let dir_name = ServerName::parse(&dir_name)?;
    if let Some(raw) = name {
        let requested = ServerName::parse(raw)?;
        if requested != dir_name {
            return Err(ServerRegistryError::Fs(FsError::marker(
                target.to_path_buf(),
                format!(
                    "--name `{}` does not match directory name `{}`; \
                     in-place registration must use the directory name",
                    requested.as_str(),
                    dir_name.as_str()
                ),
            )));
        }
    }
    Ok(dir_name)
}

/// 目录里是否有属于该 id 的 `mcsup.json`（`--purge` 前置闸）
///
/// `server link` 就地注册的目录本来就有 marker；由 `server create` 建的目录
/// 也有。missing / 损坏 / id 不符一律不放行——记录指向别处时不能拿整个目录
/// 赌一次，让用户确认后手工处理。
fn server_dir_matches(dir: &Path, id: &ServerId) -> bool {
    match read_server_marker(dir) {
        Ok(Some(marker)) => marker.id == id.as_str(),
        Ok(None) | Err(_) => false,
    }
}

/// 读取 `mcsup.json`；不存在返回 `Ok(None)`，损坏返回错误
fn read_server_marker(dir: &Path) -> Result<Option<ServerMarkerJson>, ServerRegistryError> {
    let path = dir.join(SERVER_MARKER);
    let Ok(text) = crate::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let marker: ServerMarkerJson = serde_json::from_str(&text)
        .map_err(|err| ServerRegistryError::marker(path.clone(), err.to_string()))?;
    if marker.schema != SERVER_MARKER_SCHEMA {
        return Err(ServerRegistryError::marker(
            path,
            format!("unsupported schema {}", marker.schema),
        ));
    }
    Ok(Some(marker))
}

fn create_dir_all_parent(link_path: &Path) -> Result<(), ServerRegistryError> {
    let parent = link_path.parent().ok_or_else(|| {
        ServerRegistryError::Fs(FsError::marker(
            link_path.to_path_buf(),
            "link path has no parent",
        ))
    })?;
    crate::fs::create_dir_all(parent).map_err(|err| ServerRegistryError::io(parent, err))
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> Result<(), ServerRegistryError> {
    std::os::unix::fs::symlink(target, link).map_err(|err| ServerRegistryError::io(link, err))
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> Result<(), ServerRegistryError> {
    std::os::windows::fs::symlink_dir(target, link).map_err(|err| {
        ServerRegistryError::Fs(FsError::marker(
            link.to_path_buf(),
            format!("{err}（Windows 创建目录符号链接需要开发者模式或管理员权限）"),
        ))
    })
}

#[cfg(not(any(unix, windows)))]
fn symlink_dir(target: &Path, link: &Path) -> Result<(), ServerRegistryError> {
    let _ = (target, link);
    Err(ServerRegistryError::Fs(FsError::marker(
        link.to_path_buf(),
        "symlink is not supported on this platform",
    )))
}

/// 目录当前是否为链接（`linked` 标记的唯一来源）
///
/// 只问「这个路径指向别处吗」，与 [`is_link_like`] 同口径。判定分散在多处
/// 各写一遍 `is_symlink()` 的后果是：某处改了、别处没改，于是「删的时候
/// 认为是链接、列表的时候认为不是」。
fn is_linked(dir: &Path) -> bool {
    dir.symlink_metadata().is_ok_and(|meta| is_link_like(&meta))
}

/// 条目是否为符号链接（`linked` 标记与删除共用的唯一口径）
///
/// 只认 `is_symlink()`，**不**按「重解析点」判定：Windows 上
/// `FILE_ATTRIBUTE_REPARSE_POINT` 不只标记符号链接与 junction，OneDrive 这类
/// 云占位目录也带这个属性。按它判定会把一个真实的服务器目录当成链接，随后
/// `remove` 走删链接的路径把它摘掉——为覆盖一个 `server link` 本来就不会
/// 创建的形态（junction），换来一个删数据的可能，不划算。
///
/// junction 因此落入「普通目录」分支：只注销登记、不碰磁盘。保守但安全。
fn is_link_like(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

/// 删除符号链接本身，绝不触碰目标
///
/// Windows 上「目录符号链接」是目录重解析点：`DeleteFileW`（`remove_file`）
/// 对目录一律失败，必须走 `RemoveDirectoryW`（`remove_dir`）；Unix 与
/// 「文件符号链接」则只有 `remove_file` 可用，`remove_dir` 一定报
/// `NotADirectory`。因此不能按平台硬编，先试文件再试目录：两条分支互斥，
/// 失败时保留第一次的错误（那才是调用方想看到的「为什么删不掉」）。
///
/// 先判类型再删是必须的，不能省：登记的 `linked` 只是元数据里的一句话，
/// 链接可能已被换成真实目录，此时 `remove_dir` 会把一个空目录删掉。删链接
/// 的方法绝不允许变成删数据的路径。
///
/// 悬空链接在这里同样成立：`symlink_metadata` 拿得到、目标不存在。
fn remove_symlink(path: &Path) -> std::io::Result<()> {
    let meta = match crate::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        // 链接已不在（并发清理、手工删除）：目的已达成，注销要能继续
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if !is_link_like(&meta) {
        return Err(std::io::Error::other(format!(
            "{} 不是符号链接（可能已被替换成真实目录），拒绝删除以免丢数据",
            path.display()
        )));
    }
    match crate::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(file_err) if file_err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(file_err) => match crate::fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            // 报第一次的错误：`remove_file` 的失败原因才是调用方关心的那个
            Err(_) => Err(file_err),
        },
    }
}

fn remove_link_or_dir(path: &Path) -> std::io::Result<()> {
    let Ok(meta) = crate::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if is_link_like(&meta) {
        remove_symlink(path)
    } else {
        crate::fs::remove_dir_all(path)
    }
}

// Windows 上目录被占用时 `remove_dir_all` 报 `ERROR_SHARING_VIOLATION`（os error 32）。
// 提示统一在 `fs_error::deletion_hint`。

#[cfg(test)]
mod tests;
