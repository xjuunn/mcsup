use domain::server::meta::{InstalledJar, ServerId, ServerMeta, ServerName};
use domain::server::settings::ServerSettings;
use domain::server::version::{ServerVariant, VersionId};
use domain::server::world::{DataVersion, ensure_world_readable};
use domain::workspace::{Workspace, WorkspaceId};
use port::{
    CancellationToken, CoreInspector, ProgressSink, RepositoryError, ServerRepository,
    WorkspaceRepository,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

use crate::fs_error::FsError;
use crate::workspace::{DoctorFinding, DoctorLevel, WorkspaceError};
use crate::{InstallError, InstalledDownload, ServerInstaller};

const SERVER_MARKER: &str = "mcsup.json";
const SERVER_MARKER_SCHEMA: u32 = 1;
/// 核心固定文件名（[`InstalledJar::path`] 的相对路径口径）
const SERVER_JAR: &str = "server.jar";
/// 安装暂存文件：先下载到这里，校验通过后再原子替换 `server.jar`
const STAGING_JAR: &str = ".server.jar.installing";

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

/// 服务端核心安装失败（注册表定位 + 下载编排）
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
    #[error("无法检查世界或目标核心的兼容性")]
    Inspect(#[from] port::InspectError),
    /// 目标核心低于世界数据版本：Minecraft 不支持降级世界
    #[error(
        "世界由 {world_version}（DataVersion {world}）创建，目标版本 {target_version}（DataVersion {target}）更低；Minecraft 不支持降级世界。请改用 {world_version} 或更高版本，或备份并移走世界目录后重试"
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

impl ServerRegistryError {
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
    /// 核心绝对路径
    pub destination: PathBuf,
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
        let now = crate::util::unix_now();
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
        std::fs::create_dir_all(&dir).map_err(|err| ServerRegistryError::io(&dir, err))?;

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
        let now = crate::util::unix_now();
        let target =
            std::fs::canonicalize(target).map_err(|err| ServerRegistryError::io(target, err))?;
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
    /// # Errors
    ///
    /// 不存在、链接服务器带 `purge`、IO 或注册表失败时返回 [`ServerRegistryError`]
    pub fn remove(&self, name: &str, purge: bool) -> Result<RegisteredServer, ServerRegistryError> {
        let srv = self.get(name)?;
        if srv.linked {
            if purge {
                return Err(ServerRegistryError::Fs(FsError::marker(
                    srv.dir.clone(),
                    "refusing to purge a linked server; delete the target manually \
                     and run `server remove` without --purge",
                )));
            }
            std::fs::remove_file(&srv.dir).map_err(|err| ServerRegistryError::io(&srv.dir, err))?;
        } else if purge {
            std::fs::remove_dir_all(&srv.dir)
                .map_err(|err| ServerRegistryError::io(&srv.dir, deletion_hint(err)))?;
        }
        let name = srv.meta.name.clone();
        self.servers.remove(&name)?;
        Ok(srv)
    }

    /// `workspace doctor`：检查注册表与磁盘的一致性
    ///
    /// 检查项：无当前工作区（warning）、工作目录缺失、`servers/` 下悬空符号
    /// 链接、磁盘有目录 / 链接但未入库、登记的服务器目录不存在。
    ///
    /// # Errors
    ///
    /// 注册表失败时返回 [`ServerRegistryError`]
    pub fn doctor(&self) -> Result<Vec<DoctorFinding>, ServerRegistryError> {
        let mut out = Vec::new();
        let workspaces = self.workspaces.list()?;
        if self.workspaces.current()?.is_none() {
            out.push(DoctorFinding {
                level: DoctorLevel::Warning,
                workspace: None,
                message: "no current workspace; run `mcsup workspace use <name>` to set one"
                    .to_owned(),
            });
        }
        let registered: Vec<ServerMeta> = self.servers.list()?;
        for ws in &workspaces {
            let ws_name = ws.name.as_str().to_owned();
            if !ws.path.is_dir() {
                out.push(DoctorFinding {
                    level: DoctorLevel::Error,
                    workspace: Some(ws_name.clone()),
                    message: format!("workspace directory missing: {}", ws.path.display()),
                });
                continue;
            }
            let servers_dir = ws.path.join("servers");
            let mut seen_on_disk = std::collections::BTreeSet::new();
            if let Ok(entries) = std::fs::read_dir(&servers_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let fname = entry.file_name().to_string_lossy().into_owned();
                    seen_on_disk.insert(fname.clone());
                    let Ok(ft) = entry.file_type() else { continue };
                    if ft.is_symlink() && std::fs::metadata(&path).is_err() {
                        out.push(DoctorFinding {
                            level: DoctorLevel::Error,
                            workspace: Some(ws_name.clone()),
                            message: format!("dangling symlink: {}", path.display()),
                        });
                        continue;
                    }
                    if !registered
                        .iter()
                        .any(|m| m.workspace_id == ws.id && m.rel_dir == Path::new(&fname))
                    {
                        out.push(DoctorFinding {
                            level: DoctorLevel::Warning,
                            workspace: Some(ws_name.clone()),
                            message: format!(
                                "on disk but not registered: {} \
                                 (run `server link {}` to re-register it, or remove it)",
                                path.display(),
                                path.display()
                            ),
                        });
                    }
                }
            }
            for meta in registered.iter().filter(|m| m.workspace_id == ws.id) {
                let dir = servers_dir.join(&meta.rel_dir);
                if dir.symlink_metadata().is_err() {
                    out.push(DoctorFinding {
                        level: DoctorLevel::Error,
                        workspace: Some(ws_name.clone()),
                        message: format!(
                            "registered but missing on disk: {} (run `server remove {}`)",
                            dir.display(),
                            meta.name.as_str()
                        ),
                    });
                }
            }
        }
        Ok(out)
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
            let linked = dir
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink());
            out.push(RegisteredServer { meta, dir, linked });
        }
        Ok(out)
    }

    /// 按名解析服务器
    ///
    /// # Errors
    ///
    /// 不存在或工作目录缺失时返回 [`ServerRegistryError`]
    pub fn get(&self, name: &str) -> Result<RegisteredServer, ServerRegistryError> {
        let name =
            ServerName::parse(name).map_err(|_| ServerRegistryError::NotFound(name.to_owned()))?;
        let meta = self
            .servers
            .get(&name)
            .map_err(|_| ServerRegistryError::NotFound(name.as_str().to_owned()))?;
        let dir = self.absolute_dir(&meta)?;
        let linked = dir
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink());
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
        required_java_major: Option<domain::java::JavaMajor>,
        updated_at: i64,
    ) -> Result<RegisteredServer, ServerRegistryError> {
        let name =
            ServerName::parse(name).map_err(|_| ServerRegistryError::NotFound(name.to_owned()))?;
        let meta =
            self.servers
                .update_core(&name, version, jar, required_java_major, updated_at)?;
        let dir = self.absolute_dir(&meta)?;
        let linked = dir
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink());
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
            .update_settings(&name, settings, crate::util::unix_now())?;
        let dir = self.absolute_dir(&meta)?;
        let linked = dir
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink());
        Ok(RegisteredServer { meta, dir, linked })
    }

    /// 给既有服务器安装核心：定位目录 → 下载到暂存文件 → 校验数据版本 →
    /// 原子替换 `server.jar` → 回写元数据
    ///
    /// 目标目录由注册表回答，入口不再拿当前工作目录当目标。先下到暂存文件是
    /// 为了在覆盖旧核心前完成降级检查：校验不过时旧核心原样保留。
    ///
    /// # Errors
    ///
    /// 服务器不存在返回 [`InstallJarError::Registry`]；来源、下载或校验失败返回
    /// [`InstallJarError::Install`]；元数据读取失败返回 [`InstallJarError::Inspect`]；
    /// 目标版本低于世界数据版本返回 [`InstallJarError::Downgrade`]
    pub async fn install_jar(
        &self,
        name: &str,
        version: &VersionId,
        installer: &ServerInstaller,
        inspector: &dyn CoreInspector,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<InstalledServerJar, InstallJarError> {
        let srv = self.get(name)?;
        let destination = srv.dir.join(SERVER_JAR);
        let staging = srv.dir.join(STAGING_JAR);
        let _ = std::fs::remove_file(&staging);

        let world = inspector.world_info(&srv.dir)?;
        let out: InstalledDownload = installer
            .install(version, staging.clone(), progress, cancel)
            .await?;
        let target = match inspector.jar_data_version(&staging) {
            Ok(target) => target,
            Err(err) => {
                let _ = std::fs::remove_file(&staging);
                return Err(err.into());
            }
        };
        if let Some(target) = target
            && let Some(world) = world.as_ref()
            && let Some(world_version) = world.data_version
            && let Err(downgrade) = ensure_world_readable(world_version, target)
        {
            let _ = std::fs::remove_file(&staging);
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
        if let Err(err) = std::fs::rename(&staging, &destination) {
            let _ = std::fs::remove_file(&staging);
            return Err(ServerRegistryError::io(&destination, err).into());
        }

        let jar = InstalledJar {
            path: PathBuf::from(SERVER_JAR),
            size: out.artifact.size,
            checksum: out.checksum,
        };
        let srv = self.update_core(name, version, &jar, out.java_major, crate::util::unix_now())?;
        Ok(InstalledServerJar {
            server: srv,
            jar,
            destination,
        })
    }

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
            if let Ok(servers_dir) = std::fs::canonicalize(&servers_dir) {
                if target.starts_with(&servers_dir) {
                    return Err(ServerRegistryError::TargetInsideWorkspace(servers_dir));
                }
            } else if target.starts_with(&servers_dir) {
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
            let servers_dir = std::fs::canonicalize(&servers_dir).unwrap_or(servers_dir);
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
    std::fs::write(&path, text).map_err(|err| ServerRegistryError::io(&path, err))
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

/// 读取 `mcsup.json`；不存在返回 `Ok(None)`，损坏返回错误
fn read_server_marker(dir: &Path) -> Result<Option<ServerMarkerJson>, ServerRegistryError> {
    let path = dir.join(SERVER_MARKER);
    let Ok(text) = std::fs::read_to_string(&path) else {
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
    std::fs::create_dir_all(parent).map_err(|err| ServerRegistryError::io(parent, err))
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

fn remove_link_or_dir(path: &Path) -> std::io::Result<()> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if meta.file_type().is_symlink() {
        std::fs::remove_file(path)
    } else {
        std::fs::remove_dir_all(path)
    }
}

/// Windows 上目录被占用时 `remove_dir_all` 报 `ERROR_SHARING_VIOLATION`（os error 32）
#[cfg(windows)]
fn deletion_hint(err: std::io::Error) -> std::io::Error {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    if err.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
        return std::io::Error::other(format!("{err}（目录正被占用：退出占用后重试）"));
    }
    err
}

#[cfg(not(windows))]
fn deletion_hint(err: std::io::Error) -> std::io::Error {
    err
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
    use port::{ServerRepository, WorkspaceRepository};
    use std::path::Path;

    struct MemServers {
        items: std::sync::Mutex<Vec<ServerMeta>>,
    }

    impl MemServers {
        fn new() -> Self {
            Self {
                items: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl ServerRepository for MemServers {
        fn create(&self, meta: &ServerMeta) -> Result<(), RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            if items.iter().any(|m| m.name == meta.name) {
                return Err(RepositoryError::AlreadyExists(
                    meta.name.as_str().to_owned(),
                ));
            }
            items.push(meta.clone());
            Ok(())
        }
        fn get(&self, name: &ServerName) -> Result<ServerMeta, RepositoryError> {
            self.items
                .lock()
                .expect("lock")
                .iter()
                .find(|m| &m.name == name)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
        }
        fn list(&self) -> Result<Vec<ServerMeta>, RepositoryError> {
            let mut items = self.items.lock().expect("lock").clone();
            items.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(items)
        }
        fn update_core(
            &self,
            name: &ServerName,
            version: &VersionId,
            jar: &InstalledJar,
            required_java_major: Option<domain::java::JavaMajor>,
            updated_at: i64,
        ) -> Result<ServerMeta, RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            let meta = items
                .iter_mut()
                .find(|m| &m.name == name)
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
            meta.version = version.clone();
            meta.jar = Some(jar.clone());
            meta.java_major = required_java_major;
            meta.updated_at = updated_at;
            Ok(meta.clone())
        }
        fn update_settings(
            &self,
            name: &ServerName,
            settings: &ServerSettings,
            updated_at: i64,
        ) -> Result<ServerMeta, RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            let meta = items
                .iter_mut()
                .find(|m| &m.name == name)
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
            meta.settings = *settings;
            meta.updated_at = updated_at;
            Ok(meta.clone())
        }
        fn remove(&self, name: &ServerName) -> Result<(), RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            let before = items.len();
            items.retain(|m| &m.name != name);
            if items.len() == before {
                return Err(RepositoryError::NotFound(name.as_str().to_owned()));
            }
            Ok(())
        }
        fn remove_by_workspace(&self, _workspace_id: &WorkspaceId) -> Result<(), RepositoryError> {
            Ok(())
        }
        fn relink(
            &self,
            _name: &ServerName,
            _workspace_id: &WorkspaceId,
            _rel_dir: &Path,
            _updated_at: i64,
        ) -> Result<ServerMeta, RepositoryError> {
            unreachable!("not used in tests")
        }
    }

    struct MemWorkspaces {
        items: std::sync::Mutex<Vec<Workspace>>,
    }

    impl MemWorkspaces {
        fn new(ws: Workspace) -> Self {
            Self {
                items: std::sync::Mutex::new(vec![ws]),
            }
        }
    }

    impl WorkspaceRepository for MemWorkspaces {
        fn create(&self, ws: &Workspace) -> Result<(), RepositoryError> {
            self.items.lock().expect("lock").push(ws.clone());
            Ok(())
        }
        fn get(&self, id: &WorkspaceId) -> Result<Workspace, RepositoryError> {
            self.items
                .lock()
                .expect("lock")
                .iter()
                .find(|w| &w.id == id)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(id.as_str().to_owned()))
        }
        fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError> {
            self.items
                .lock()
                .expect("lock")
                .iter()
                .find(|w| &w.name == name)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
        }
        fn list(&self) -> Result<Vec<Workspace>, RepositoryError> {
            Ok(self.items.lock().expect("lock").clone())
        }
        fn current(&self) -> Result<Option<Workspace>, RepositoryError> {
            Ok(self
                .items
                .lock()
                .expect("lock")
                .iter()
                .find(|w| w.is_current)
                .cloned())
        }
        fn set_current(&self, _id: &WorkspaceId) -> Result<(), RepositoryError> {
            Ok(())
        }
        fn remove(&self, _id: &WorkspaceId) -> Result<(), RepositoryError> {
            Ok(())
        }
        fn relink_path(
            &self,
            _id: &WorkspaceId,
            _path: &Path,
            _updated_at: i64,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    fn ws_fixture(root: &Path) -> Workspace {
        let path = root.join("ws");
        std::fs::create_dir_all(path.join("servers")).expect("mkdir");
        Workspace {
            id: WorkspaceId::parse("550e8400-e29b-41d4-a716-446655440001").expect("uuid"),
            name: WorkspaceName::parse("ws01").expect("valid"),
            path,
            is_current: true,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    #[test]
    fn create_writes_marker_and_registers() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = ws_fixture(tmp.path());
        let reg = ServerRegistry::new(
            Arc::new(MemServers::new()),
            Arc::new(MemWorkspaces::new(ws)),
        );
        let out = reg
            .create(
                None,
                "alpha",
                &VersionId::new("1.21.1"),
                ServerVariant::Vanilla,
                None,
            )
            .expect("create");
        assert_eq!(out.meta.name.as_str(), "alpha");
        assert!(out.dir.join("mcsup.json").is_file());
        let got = reg.get("alpha").expect("get");
        assert_eq!(got.meta.id, out.meta.id);
    }

    #[test]
    fn create_duplicate_name_rejected() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = ws_fixture(tmp.path());
        let reg = ServerRegistry::new(
            Arc::new(MemServers::new()),
            Arc::new(MemWorkspaces::new(ws)),
        );
        reg.create(
            None,
            "alpha",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect("first");
        let err = reg
            .create(
                None,
                "alpha",
                &VersionId::new("1.21.1"),
                ServerVariant::Vanilla,
                None,
            )
            .expect_err("dup");
        assert!(matches!(err, ServerRegistryError::AlreadyExists(_)));
    }

    #[test]
    fn get_missing_is_not_found() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = ws_fixture(tmp.path());
        let reg = ServerRegistry::new(
            Arc::new(MemServers::new()),
            Arc::new(MemWorkspaces::new(ws)),
        );
        let err = reg.get("ghost").expect_err("missing");
        assert!(matches!(err, ServerRegistryError::NotFound(_)));
    }

    #[test]
    fn list_orders_by_name() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = ws_fixture(tmp.path());
        let reg = ServerRegistry::new(
            Arc::new(MemServers::new()),
            Arc::new(MemWorkspaces::new(ws)),
        );
        reg.create(
            None,
            "beta",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect("b");
        reg.create(
            None,
            "alpha",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect("a");
        let names: Vec<_> = reg
            .list()
            .expect("list")
            .into_iter()
            .map(|r| r.meta.name.as_str().to_owned())
            .collect();
        assert_eq!(names, ["alpha", "beta"]);
    }

    /// 工作区注册表 fixture：当前工作区 `ws01`，路径 `<root>/ws`
    fn registry_with_ws(root: &Path) -> ServerRegistry {
        ServerRegistry::new(
            Arc::new(MemServers::new()),
            Arc::new(MemWorkspaces::new(ws_fixture(root))),
        )
    }

    /// 第二个工作区 fixture：非当前，路径 `<root>/ws2`
    fn ws2_fixture(root: &Path) -> Workspace {
        let path = root.join("ws2");
        std::fs::create_dir_all(path.join("servers")).expect("mkdir");
        Workspace {
            id: WorkspaceId::parse("550e8400-e29b-41d4-a716-446655440002").expect("uuid"),
            name: WorkspaceName::parse("ws02").expect("valid"),
            path,
            is_current: false,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    #[cfg(unix)]
    #[test]
    fn link_creates_symlink_and_registers() {
        let tmp = tempfile::tempdir().expect("tmp");
        let reg = registry_with_ws(tmp.path());
        let external = tmp.path().join("external");
        std::fs::create_dir_all(&external).expect("mkdir");

        let out = reg.link(&external, Some("ext"), None).expect("link");

        assert!(out.server.linked);
        assert!(
            out.server
                .dir
                .symlink_metadata()
                .expect("link")
                .file_type()
                .is_symlink()
        );
        assert_eq!(out.server.meta.variant, ServerVariant::Unknown);
        assert_eq!(out.server.meta.version.as_str(), "unknown");
        assert!(out.server.dir.join("mcsup.json").is_file());
    }

    /// 直接子目录：就地注册，不建符号链接
    #[test]
    fn link_registers_in_place_for_direct_child() {
        let tmp = tempfile::tempdir().expect("tmp");
        let reg = registry_with_ws(tmp.path());
        let dir = tmp.path().join("ws/servers/nested");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let out = reg.link(&dir, None, None).expect("in-place");

        assert_eq!(out.server.meta.name.as_str(), "nested");
        assert_eq!(out.server.meta.rel_dir, Path::new("nested"));
        assert!(!out.server.linked);
        assert_eq!(out.target, std::fs::canonicalize(&dir).expect("canonical"));
        assert!(dir.join("mcsup.json").is_file());
        assert!(
            !dir.symlink_metadata()
                .expect("meta")
                .file_type()
                .is_symlink()
        );
    }

    /// 恢复场景：create → 注销（磁盘保留）→ link 补回注册并复用 marker 身份
    #[test]
    fn link_in_place_reuses_id_after_unregister() {
        let tmp = tempfile::tempdir().expect("tmp");
        let reg = registry_with_ws(tmp.path());
        let created = reg
            .create(
                None,
                "test",
                &VersionId::new("1.21.1"),
                ServerVariant::Vanilla,
                None,
            )
            .expect("create");
        reg.remove("test", false).expect("unregister");

        let out = reg.link(&created.dir, None, None).expect("recover");

        assert_eq!(out.server.meta.id, created.meta.id);
        assert_eq!(out.server.meta.rel_dir, Path::new("test"));
        assert_eq!(out.server.meta.version.as_str(), "unknown");
        assert!(!out.server.linked);
        assert!(created.dir.is_dir());
    }

    /// 重复就地注册：名称已入库即冲突
    #[test]
    fn link_in_place_twice_is_already_exists() {
        let tmp = tempfile::tempdir().expect("tmp");
        let reg = registry_with_ws(tmp.path());
        let dir = tmp.path().join("ws/servers/nested");
        std::fs::create_dir_all(&dir).expect("mkdir");
        reg.link(&dir, None, None).expect("first");

        let err = reg.link(&dir, None, None).expect_err("reject");
        assert!(matches!(err, ServerRegistryError::AlreadyExists(_)));
    }

    /// 目录归属不可改：--workspace 指向别的工作区时拒绝
    #[test]
    fn link_in_place_rejects_foreign_workspace_selector() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws01 = ws_fixture(tmp.path());
        let dir = ws01.path.join("servers").join("nested");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let store = Arc::new(MemWorkspaces::new(ws01));
        store.create(&ws2_fixture(tmp.path())).expect("second ws");
        let reg = ServerRegistry::new(Arc::new(MemServers::new()), store);

        let err = reg.link(&dir, None, Some("ws02")).expect_err("reject");
        assert!(matches!(err, ServerRegistryError::WorkspaceMismatch { .. }));
    }

    /// `--name` 与目录名不一致时拒绝：`rel_dir` 必须与磁盘一致
    #[test]
    fn link_in_place_rejects_name_not_matching_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let reg = registry_with_ws(tmp.path());
        let dir = tmp.path().join("ws/servers/nested");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let err = reg.link(&dir, Some("other"), None).expect_err("reject");
        assert!(matches!(err, ServerRegistryError::Fs(_)));
        assert!(reg.list().expect("list").is_empty());
    }

    /// marker 名与目录名不一致时拒绝：不猜哪个是真实身份
    #[test]
    fn link_in_place_rejects_marker_name_mismatch() {
        let tmp = tempfile::tempdir().expect("tmp");
        let reg = registry_with_ws(tmp.path());
        let dir = tmp.path().join("ws/servers/nested");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("mcsup.json"),
            r#"{"schema":1,"id":"550e8400-e29b-41d4-a716-446655440009","name":"oldname"}"#,
        )
        .expect("marker");

        let err = reg.link(&dir, None, None).expect_err("reject");
        assert!(matches!(err, ServerRegistryError::Fs(_)));
        assert!(reg.list().expect("list").is_empty());
    }

    /// marker id 已被别的服务器占用时拒绝：一个 id 不能对应两个目录
    #[test]
    fn link_in_place_rejects_registered_id() {
        let tmp = tempfile::tempdir().expect("tmp");
        let reg = registry_with_ws(tmp.path());
        let taken = reg
            .create(
                None,
                "taken",
                &VersionId::new("1.21.1"),
                ServerVariant::Vanilla,
                None,
            )
            .expect("create");
        let dir = tmp.path().join("ws/servers/nested");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("mcsup.json"),
            format!(
                r#"{{"schema":1,"id":"{}","name":"nested"}}"#,
                taken.meta.id.as_str()
            ),
        )
        .expect("marker");

        let err = reg.link(&dir, None, None).expect_err("reject");
        assert!(matches!(err, ServerRegistryError::AlreadyExists(_)));
    }

    /// `servers/` 更深的子树仍拒绝：单层 `rel_dir` 表达不了，链接回去会成环
    #[test]
    fn link_rejects_target_below_servers_child() {
        let tmp = tempfile::tempdir().expect("tmp");
        let reg = registry_with_ws(tmp.path());
        let deep = tmp.path().join("ws/servers/nested/deep");
        std::fs::create_dir_all(&deep).expect("mkdir");

        let err = reg.link(&deep, None, None).expect_err("reject");
        assert!(matches!(err, ServerRegistryError::TargetInsideWorkspace(_)));
        assert!(reg.list().expect("list").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn remove_purge_on_linked_refuses_and_keeps_target() {
        let tmp = tempfile::tempdir().expect("tmp");
        let reg = registry_with_ws(tmp.path());
        let external = tmp.path().join("external");
        std::fs::create_dir_all(&external).expect("mkdir");
        std::fs::write(external.join("keep.txt"), "data").expect("file");
        reg.link(&external, Some("ext"), None).expect("link");

        let err = reg.remove("ext", true).expect_err("refuse");
        assert!(err.to_string().contains("linked"));
        assert!(external.join("keep.txt").is_file());
        assert!(reg.get("ext").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn remove_linked_drops_link_but_keeps_target() {
        let tmp = tempfile::tempdir().expect("tmp");
        let reg = registry_with_ws(tmp.path());
        let external = tmp.path().join("external");
        std::fs::create_dir_all(&external).expect("mkdir");
        std::fs::write(external.join("keep.txt"), "data").expect("file");
        reg.link(&external, Some("ext"), None).expect("link");

        reg.remove("ext", false).expect("remove");

        assert!(reg.get("ext").is_err());
        assert!(external.join("keep.txt").is_file());
        assert!(
            tmp.path()
                .join("ws/servers/ext")
                .symlink_metadata()
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_refuses_dangling_symlink_path() {
        let tmp = tempfile::tempdir().expect("tmp");
        let reg = registry_with_ws(tmp.path());
        let ghost = tmp.path().join("ghost-target");
        std::os::unix::fs::symlink(&ghost, tmp.path().join("ws/servers/stale")).expect("dangling");

        let err = reg
            .create(
                None,
                "stale",
                &VersionId::new("1.21.1"),
                ServerVariant::Vanilla,
                None,
            )
            .expect_err("refuse");
        assert!(matches!(err, ServerRegistryError::AlreadyExists(_)));
    }

    #[cfg(unix)]
    #[test]
    fn doctor_reports_drift() {
        let tmp = tempfile::tempdir().expect("tmp");
        let reg = registry_with_ws(tmp.path());
        reg.create(
            None,
            "alpha",
            &VersionId::new("1.21.1"),
            ServerVariant::Vanilla,
            None,
        )
        .expect("create");
        // 磁盘有目录但未入库
        std::fs::create_dir_all(tmp.path().join("ws/servers/orphan")).expect("mkdir");
        // 悬空符号链接
        std::os::unix::fs::symlink(
            tmp.path().join("no-such-target"),
            tmp.path().join("ws/servers/dangling"),
        )
        .expect("dangling");
        // 登记了但磁盘缺失
        std::fs::remove_dir_all(tmp.path().join("ws/servers/alpha")).expect("rmdir");

        let findings = reg.doctor().expect("doctor");
        let msgs: Vec<&str> = findings.iter().map(|f| f.message.as_str()).collect();
        assert!(msgs.iter().any(|m| m.contains("dangling symlink")));
        assert!(msgs.iter().any(|m| m.contains("not registered")));
        assert!(msgs.iter().any(|m| m.contains("missing on disk")));
    }

    struct FakeSource;

    #[async_trait::async_trait]
    impl port::ServerJarSource for FakeSource {
        fn variant(&self) -> ServerVariant {
            ServerVariant::Vanilla
        }

        async fn versions(
            &self,
        ) -> Result<Vec<domain::server::version::ServerVersion>, port::SourceError> {
            Ok(Vec::new())
        }

        async fn artifact(
            &self,
            version: &VersionId,
        ) -> Result<port::ResolvedArtifact, port::SourceError> {
            Ok(port::ResolvedArtifact {
                artifact: domain::server::artifact::JarArtifact {
                    version: version.clone(),
                    size: 3,
                    checksum: domain::server::artifact::Checksum::new(
                        domain::server::artifact::ChecksumAlgorithm::Sha1,
                        "0".repeat(40),
                    ),
                },
                location: url::Url::parse("https://example.invalid/server.jar").expect("url"),
                java_major: None,
            })
        }

        fn name(&self) -> &'static str {
            "fake"
        }
    }

    struct FakeDownloader;

    #[async_trait::async_trait]
    impl port::ArtifactDownloader for FakeDownloader {
        async fn download(
            &self,
            request: &port::DownloadRequest,
            _progress: Option<&dyn ProgressSink>,
            _cancel: &CancellationToken,
        ) -> Result<port::DownloadedArtifact, port::DownloadError> {
            std::fs::write(&request.destination, b"new").expect("write");
            Ok(port::DownloadedArtifact {
                path: request.destination.clone(),
                size: 3,
            })
        }
    }

    struct FakeInspector {
        world: Option<domain::server::world::WorldInfo>,
        target: Option<DataVersion>,
    }

    impl CoreInspector for FakeInspector {
        fn world_info(
            &self,
            _server_dir: &Path,
        ) -> Result<Option<domain::server::world::WorldInfo>, port::InspectError> {
            Ok(self.world.clone())
        }

        fn jar_data_version(&self, _jar: &Path) -> Result<Option<DataVersion>, port::InspectError> {
            Ok(self.target)
        }
    }

    fn world_info(data_version: i32, name: &str) -> domain::server::world::WorldInfo {
        domain::server::world::WorldInfo {
            data_version: Some(DataVersion::new(data_version)),
            version_name: Some(name.to_owned()),
        }
    }

    /// 世界比目标新：拒绝替换，旧 `server.jar` 原样保留，暂存文件清理
    #[tokio::test]
    async fn downgrade_is_rejected_and_old_jar_kept() {
        let tmp = tempfile::tempdir().expect("tmp");
        let reg = registry_with_ws(tmp.path());
        let created = reg
            .create(
                None,
                "alpha",
                &VersionId::new("26.1"),
                ServerVariant::Vanilla,
                None,
            )
            .expect("create");
        std::fs::write(created.dir.join(SERVER_JAR), b"old").expect("old jar");

        let installer = ServerInstaller::new(Arc::new(FakeSource), Arc::new(FakeDownloader));
        let inspector = FakeInspector {
            world: Some(world_info(4786, "26.1")),
            target: Some(DataVersion::new(4554)),
        };
        let err = reg
            .install_jar(
                "alpha",
                &VersionId::new("1.21.9"),
                &installer,
                &inspector,
                None,
                &CancellationToken::new(),
            )
            .await
            .expect_err("downgrade must fail");
        assert!(matches!(err, InstallJarError::Downgrade { .. }));
        assert_eq!(
            std::fs::read(created.dir.join(SERVER_JAR)).expect("read"),
            b"old"
        );
        assert!(!created.dir.join(STAGING_JAR).exists(), "暂存文件要清理");
    }

    /// 目标不低：替换成功并回写版本
    #[tokio::test]
    async fn compatible_core_replaces_jar_and_version() {
        let tmp = tempfile::tempdir().expect("tmp");
        let reg = registry_with_ws(tmp.path());
        let created = reg
            .create(
                None,
                "alpha",
                &VersionId::new("26.1"),
                ServerVariant::Vanilla,
                None,
            )
            .expect("create");
        std::fs::write(created.dir.join(SERVER_JAR), b"old").expect("old jar");

        let installer = ServerInstaller::new(Arc::new(FakeSource), Arc::new(FakeDownloader));
        let inspector = FakeInspector {
            world: Some(world_info(4786, "26.1")),
            target: Some(DataVersion::new(4786)),
        };
        let out = reg
            .install_jar(
                "alpha",
                &VersionId::new("26.1"),
                &installer,
                &inspector,
                None,
                &CancellationToken::new(),
            )
            .await
            .expect("install");
        assert_eq!(out.server.meta.version.as_str(), "26.1");
        assert_eq!(
            std::fs::read(created.dir.join(SERVER_JAR)).expect("read"),
            b"new"
        );
        assert!(!created.dir.join(STAGING_JAR).exists());
    }
}
