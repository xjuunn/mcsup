//! 远端只读镜像仓库：`port` 仓库 trait 的远端实现
//!
//! 三个实现都持有 [`RemoteLink`]，读路径直接把对端的 `list` 结果解回领域对象。
//! 客户端不维护第二套注册表——「本地缓存 + TTL 刷新」留给装配层（组合仓库）
//! 做，镜像本身永远说真话：能读到的是对端此刻的状态，读不到就报错。
//!
//! # 写操作为什么在镜像上而不在 service
//!
//! 客户端**不实现** `create` / `update_core` 这类 trait 方法去伪装本地仓库：
//! `WorkspaceService::init` 会在本机建目录、写 marker，这些动作必须在**对端**
//! 发生。镜像因此只暴露与帧一一对应的显式方法（`create` / `remove`），
//! 调用方（组合仓库的路由层）按来源决定「本地走 service、
//! 远端走这些方法」，不存在「假装有仓库」后仍在本机做文件系统动作的歧义。
//!
//! # `list_sourced` 与镜像的来源
//!
//! 单来源实现默认全部标 [`Origin::Local`]，镜像覆写成 [`Origin::Remote`]——
//! 这是「组合仓库给合并列表标注来源」机制下，远端一方的真实标注。

use super::link::RemoteLink;
use domain::java::JavaRuntime;
use domain::server::launch::LaunchSpec;
use domain::server::meta::{InstalledJar, ServerMeta, ServerName};
use domain::server::settings::ServerSettings;
use domain::server::version::{ServerVersion, VersionId};
use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
use port::{Origin, RepositoryError, Sourced};
use protocol::remote::wire::WirePropUpdate;
use protocol::remote::{Request, Response};
use std::path::Path;

/// 远端「按整合包创建」的结果
///
/// 与 `ModpackApplied` 帧一一对应；`meta` 是已经装好加载器核心的对端记录，
/// 客户端据此渲染创建行与后续提示。
#[derive(Debug, Clone)]
pub struct RemoteModpackResult {
    /// 对端上的服务器记录
    pub meta: ServerMeta,
    /// 整合包项目 ID
    pub project_id: String,
    /// 版本 ID
    pub version_id: String,
    /// 版本号
    pub version_number: String,
    /// 清单声明的 Minecraft 版本
    pub minecraft: String,
    /// 加载器展示文本
    pub loader: Option<String>,
    /// 实际安装的服务端文件数
    pub files: usize,
    /// 本次下载的字节数
    pub downloaded_bytes: u64,
    /// 已存在而复用的文件数
    pub reused: usize,
    /// 仅客户端、已跳过的文件数
    pub skipped_client_only: usize,
}

/// `server link --origin` 的结果
///
/// 与 `Linked` 帧一一对应：`meta` 是对端注册后的记录，`target` 是对端上
/// canonicalize 后的真实路径，`linked` 表示是否建立了符号链接（工作区内就地
/// 注册时为 `false`）。
#[derive(Debug, Clone)]
pub struct RemoteLinkResult {
    /// 对端上的服务器记录
    pub meta: ServerMeta,
    /// 对端上的真实目标路径
    pub target: String,
    /// 是否建立了符号链接
    pub linked: bool,
}

/// 工作目录镜像
#[derive(Debug, Clone)]
pub struct RemoteWorkspaceRepository {
    link: RemoteLink,
    origin: Origin,
}

impl RemoteWorkspaceRepository {
    /// 构造；`origin` 是这台远端的来源标注
    #[must_use]
    pub fn new(link: RemoteLink, origin: Origin) -> Self {
        Self { link, origin }
    }

    /// 在对端创建工作目录（`dir` 是**对端**机器上的路径）
    ///
    /// 返回对端创建 / 恢复出的工作目录记录。
    ///
    /// # Errors
    ///
    /// 对端拒绝（目录非空、名称冲突…）或传输失败时返回 [`RepositoryError`]
    pub fn init(&self, dir: &Path, name: Option<&str>) -> Result<Workspace, RepositoryError> {
        let response = self.link.expect(
            &Request::WorkspaceInit {
                dir: dir.to_string_lossy().into_owned(),
                name: name.map(str::to_owned),
            },
            "workspaces",
        )?;
        match response {
            Response::Workspaces { mut items } => {
                let wire = items.pop().ok_or_else(|| {
                    RepositoryError::Corrupt("workspace.init returned no record".to_owned())
                })?;
                Workspace::try_from(wire)
            }
            other => Err(unexpected(other, "workspace.init")),
        }
    }

    /// 切换对端的当前工作区
    ///
    /// 「当前工作区」是对端的状态：客户端 `workspace use prod-box/w1` 时，
    /// 对端下一次缺省落点就在 `w1`。
    ///
    /// # Errors
    ///
    /// 对端拒绝或传输失败时返回 [`RepositoryError`]
    pub fn set_current(&self, selector: &str) -> Result<Workspace, RepositoryError> {
        let response = self.link.expect(
            &Request::WorkspaceUse {
                selector: selector.to_owned(),
            },
            "workspaces",
        )?;
        match response {
            Response::Workspaces { mut items } => {
                let wire = items.pop().ok_or_else(|| {
                    RepositoryError::Corrupt("workspace.use returned no record".to_owned())
                })?;
                Workspace::try_from(wire)
            }
            other => Err(unexpected(other, "workspace.use")),
        }
    }

    /// 注销对端的工作目录；`purge` 时对端一并删盘
    ///
    /// # Errors
    ///
    /// 对端拒绝或传输失败时返回 [`RepositoryError`]
    pub fn remove(&self, selector: &str, purge: bool) -> Result<bool, RepositoryError> {
        let response = self.link.expect(
            &Request::WorkspaceRemove {
                selector: selector.to_owned(),
                purge,
            },
            "removed",
        )?;
        match response {
            Response::Removed { disk_purged } => Ok(disk_purged),
            other => Err(unexpected(other, "workspace.remove")),
        }
    }
}

impl port::WorkspaceRepository for RemoteWorkspaceRepository {
    fn create(&self, _ws: &Workspace) -> Result<(), RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "creating a remote workspace goes through RemoteWorkspaceRepository::init",
        ))))
    }

    fn get(&self, id: &WorkspaceId) -> Result<Workspace, RepositoryError> {
        self.list()?
            .into_iter()
            .find(|ws| &ws.id == id)
            .ok_or_else(|| RepositoryError::NotFound(id.as_str().to_owned()))
    }

    fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError> {
        self.list()?
            .into_iter()
            .find(|ws| &ws.name == name)
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
    }

    fn list(&self) -> Result<Vec<Workspace>, RepositoryError> {
        let response = self.link.expect(&Request::WorkspaceList, "workspaces")?;
        match response {
            Response::Workspaces { items } => {
                let mut out = Vec::with_capacity(items.len());
                decode_list("workspace", items, Workspace::try_from, &mut out)?;
                Ok(out)
            }
            other => Err(unexpected(other, "workspace.list")),
        }
    }

    fn current(&self) -> Result<Option<Workspace>, RepositoryError> {
        Ok(self.list()?.into_iter().find(|ws| ws.is_current))
    }

    fn set_current(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
        let ws = self.get(id)?;
        self.set_current(ws.name.as_str())?;
        Ok(())
    }

    fn remove(&self, id: &WorkspaceId) -> Result<(), RepositoryError> {
        let ws = self.get(id)?;
        self.remove(ws.name.as_str(), false)?;
        Ok(())
    }

    fn relink_path(
        &self,
        _id: &WorkspaceId,
        _path: &Path,
        _updated_at: i64,
    ) -> Result<(), RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "relinking a remote workspace is a local-only operation",
        ))))
    }

    fn list_sourced(&self) -> Result<Vec<Sourced<Workspace>>, RepositoryError> {
        Ok(self
            .list()?
            .into_iter()
            .map(|ws| Sourced::new(self.origin.clone(), ws))
            .collect())
    }
}

/// 服务器镜像
#[derive(Debug, Clone)]
pub struct RemoteServerRepository {
    link: RemoteLink,
    origin: Origin,
}

impl RemoteServerRepository {
    /// 连接网关（supervisor 路由复用同一连接）
    #[must_use]
    pub fn link(&self) -> RemoteLink {
        self.link.clone()
    }

    /// 构造；`origin` 是这台远端的来源标注
    #[must_use]
    pub fn new(link: RemoteLink, origin: Origin) -> Self {
        Self { link, origin }
    }

    /// 在对端创建服务器登记（不下载核心；核心安装是对端的事）
    ///
    /// # Errors
    ///
    /// 对端拒绝（名称冲突、工作区不存在…）或传输失败时返回 [`RepositoryError`]
    pub fn create(
        &self,
        workspace: Option<&str>,
        name: &str,
        version: &str,
        variant: &str,
        java_major: Option<i64>,
    ) -> Result<ServerMeta, RepositoryError> {
        let response = self.link.expect(
            &Request::ServerCreate {
                workspace: workspace.map(str::to_owned),
                name: name.to_owned(),
                version: version.to_owned(),
                variant: variant.to_owned(),
                java_major,
            },
            "servers",
        )?;
        match response {
            Response::Servers { mut items } => {
                let wire = items.pop().ok_or_else(|| {
                    RepositoryError::Corrupt("server.create returned no record".to_owned())
                })?;
                ServerMeta::try_from(wire)
            }
            other => Err(unexpected(other, "server.create")),
        }
    }

    /// 在对端安装 / 更换服务端核心
    ///
    /// 下载、校验、（安装器类变体的）安装器执行都在对端完成；返回对端回写的
    /// 最新注册记录（含 jar 与启动方式）。
    ///
    /// # Errors
    ///
    /// 对端拒绝（来源不可达、版本不存在…）或传输失败时返回 [`RepositoryError`]
    pub fn install(&self, name: &str, version: &str) -> Result<ServerMeta, RepositoryError> {
        let response = self.link.expect(
            &Request::ServerInstall {
                name: name.to_owned(),
                version: version.to_owned(),
            },
            "servers",
        )?;
        match response {
            Response::Servers { mut items } => {
                let wire = items.pop().ok_or_else(|| {
                    RepositoryError::Corrupt("server.install returned no record".to_owned())
                })?;
                ServerMeta::try_from(wire)
            }
            other => Err(unexpected(other, "server.install")),
        }
    }

    /// 在对端按整合包创建服务器并完成布置
    ///
    /// 清单要在对端读：MC 版本与加载器变体由对端从整合包元数据里取，客户端
    /// 只给项目与（可选）版本。一次调用即完成登记 + 装加载器 + 铺文件。
    ///
    /// # Errors
    ///
    /// 对端拒绝（项目 / 版本不存在、名称冲突…）或传输失败时返回 [`RepositoryError`]
    pub fn create_modpack(
        &self,
        workspace: Option<&str>,
        name: &str,
        project: &str,
        version: Option<&str>,
    ) -> Result<RemoteModpackResult, RepositoryError> {
        let response = self.link.expect(
            &Request::ServerCreateModpack {
                workspace: workspace.map(str::to_owned),
                name: name.to_owned(),
                project: project.to_owned(),
                version: version.map(str::to_owned),
            },
            "modpack_applied",
        )?;
        match response {
            Response::ModpackApplied {
                server,
                project_id,
                version_id,
                version_number,
                minecraft,
                loader,
                files,
                downloaded_bytes,
                reused,
                skipped_client_only,
            } => Ok(RemoteModpackResult {
                meta: ServerMeta::try_from(server)?,
                project_id,
                version_id,
                version_number,
                minecraft,
                loader,
                files,
                downloaded_bytes,
                reused,
                skipped_client_only,
            }),
            other => Err(unexpected(other, "server.create_modpack")),
        }
    }

    /// 在对端注册一个已存在的服务器目录（`server link --origin`）
    ///
    /// 符号链接、marker 与入库都在对端完成；`target` 是**对端机器**上的路径。
    ///
    /// # Errors
    ///
    /// 对端拒绝（目标不存在 / 名称冲突 / 目标在工作区内…）或传输失败时返回
    /// [`RepositoryError`]
    pub fn link_directory(
        &self,
        target: &str,
        name: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<RemoteLinkResult, RepositoryError> {
        let response = self.link.expect(
            &Request::ServerLink {
                target: target.to_owned(),
                name: name.map(str::to_owned),
                workspace: workspace.map(str::to_owned),
            },
            "linked",
        )?;
        match response {
            Response::Linked {
                server,
                target,
                linked,
            } => Ok(RemoteLinkResult {
                meta: ServerMeta::try_from(server)?,
                target,
                linked,
            }),
            other => Err(unexpected(other, "server.link")),
        }
    }

    /// 对端可用的服务端变体标识（`server create --origin` 交互向导用）
    ///
    /// # Errors
    ///
    /// 传输失败或对端返回非预期载荷时返回 [`RepositoryError`]
    pub fn variants(&self) -> Result<Vec<String>, RepositoryError> {
        let response = self.link.expect(&Request::ServerVariants, "variants")?;
        match response {
            Response::Variants { items } => Ok(items),
            other => Err(unexpected(other, "server.variants")),
        }
    }

    /// 对端某个变体的版本列表（已按展示顺序排序）
    ///
    /// # Errors
    ///
    /// 传输失败或对端返回非预期载荷时返回 [`RepositoryError`]
    pub fn versions(&self, variant: &str) -> Result<Vec<ServerVersion>, RepositoryError> {
        let response = self.link.expect(
            &Request::ServerVersions {
                variant: variant.to_owned(),
            },
            "versions",
        )?;
        match response {
            Response::Versions { items } => items
                .into_iter()
                .map(ServerVersion::try_from)
                .collect::<Result<Vec<_>, _>>(),
            other => Err(unexpected(other, "server.versions")),
        }
    }

    /// 读取对端的 `server.properties` 原文
    ///
    /// # Errors
    ///
    /// 对端拒绝或传输失败时返回 [`RepositoryError`]
    pub fn props(&self, server_id: &str) -> Result<(String, String), RepositoryError> {
        let response = self.link.expect(
            &Request::PropsGet {
                server_id: server_id.to_owned(),
            },
            "props",
        )?;
        match response {
            Response::Props { path, text } => Ok((path, text)),
            other => Err(unexpected(other, "server.props.get")),
        }
    }

    /// 写入对端的 `server.properties`，返回写入后的路径与原文
    ///
    /// # Errors
    ///
    /// 对端拒绝（键 / 值非法、写入失败…）或传输失败时返回 [`RepositoryError`]
    pub fn set_props(
        &self,
        server_id: &str,
        updates: &[(String, String)],
    ) -> Result<(String, String), RepositoryError> {
        let response = self.link.expect(
            &Request::PropsSet {
                server_id: server_id.to_owned(),
                updates: updates.iter().map(WirePropUpdate::from).collect(),
            },
            "props",
        )?;
        match response {
            Response::Props { path, text } => Ok((path, text)),
            other => Err(unexpected(other, "server.props.set")),
        }
    }

    /// 设置对端服务器的一项服务属性，返回规范化后的值与完整属性集
    ///
    /// # Errors
    ///
    /// 对端拒绝（值非法、大版本未注册…）或传输失败时返回 [`RepositoryError`]
    pub fn set_setting(
        &self,
        server_id: &str,
        key: &str,
        value: &str,
    ) -> Result<(ServerSettings, String), RepositoryError> {
        let response = self.link.expect(
            &Request::SettingsSet {
                server_id: server_id.to_owned(),
                key: key.to_owned(),
                value: value.to_owned(),
            },
            "settings_changed",
        )?;
        match response {
            Response::SettingsChanged { value, settings } => {
                Ok((ServerSettings::try_from(settings)?, value))
            }
            other => Err(unexpected(other, "server.set")),
        }
    }

    /// 注销对端的服务器；`purge` 时对端一并删盘
    ///
    /// # Errors
    ///
    /// 对端拒绝或传输失败时返回 [`RepositoryError`]
    pub fn remove(&self, name: &str, purge: bool) -> Result<bool, RepositoryError> {
        let response = self.link.expect(
            &Request::ServerRemove {
                name: name.to_owned(),
                purge,
            },
            "removed",
        )?;
        match response {
            Response::Removed { disk_purged } => Ok(disk_purged),
            other => Err(unexpected(other, "server.remove")),
        }
    }
}

impl port::ServerRepository for RemoteServerRepository {
    fn create(&self, _meta: &ServerMeta) -> Result<(), RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "creating a remote server goes through RemoteServerRepository::create",
        ))))
    }

    fn get(&self, name: &ServerName) -> Result<ServerMeta, RepositoryError> {
        self.list()?
            .into_iter()
            .find(|meta| &meta.name == name)
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
    }

    fn list(&self) -> Result<Vec<ServerMeta>, RepositoryError> {
        let response = self.link.expect(&Request::ServerList, "servers")?;
        match response {
            Response::Servers { items } => {
                let mut out = Vec::with_capacity(items.len());
                decode_list("server", items, ServerMeta::try_from, &mut out)?;
                Ok(out)
            }
            other => Err(unexpected(other, "server.list")),
        }
    }

    fn update_core(
        &self,
        _name: &ServerName,
        _version: &VersionId,
        _jar: &InstalledJar,
        _launch: Option<&LaunchSpec>,
        _required_java_major: Option<domain::java::JavaMajor>,
        _updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "installing a core on a remote server is a remote-side operation",
        ))))
    }

    fn update_settings(
        &self,
        _name: &ServerName,
        _settings: &ServerSettings,
        _updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "updating remote server settings is not in this protocol version",
        ))))
    }

    fn remove(&self, name: &ServerName) -> Result<(), RepositoryError> {
        self.remove(name.as_str(), false)?;
        Ok(())
    }

    fn remove_by_workspace(&self, _workspace_id: &WorkspaceId) -> Result<(), RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "removing remote servers by workspace is not exposed; remove individually",
        ))))
    }

    fn relink(
        &self,
        _name: &ServerName,
        _workspace_id: &WorkspaceId,
        _rel_dir: &Path,
        _updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "relinking a remote server is a remote-side operation",
        ))))
    }

    fn list_sourced(&self) -> Result<Vec<Sourced<ServerMeta>>, RepositoryError> {
        Ok(self
            .list()?
            .into_iter()
            .map(|meta| Sourced::new(self.origin.clone(), meta))
            .collect())
    }
}

/// Java 运行时镜像
///
/// 扫描是对端的事（[`Self::scan`]），镜像侧的 `upsert` / `remove` /
/// `mark_scanned` 没有意义——那些动作只改对端本机的注册表。
#[derive(Debug, Clone)]
pub struct RemoteJavaRepository {
    link: RemoteLink,
    origin: Origin,
}

impl RemoteJavaRepository {
    /// 构造；`origin` 是这台远端的来源标注
    #[must_use]
    pub fn new(link: RemoteLink, origin: Origin) -> Self {
        Self { link, origin }
    }

    /// 让对端扫一遍本机 JVM 并登记（`force = true`）
    ///
    /// # Errors
    ///
    /// 对端扫描失败或传输失败时返回 [`RepositoryError`]
    pub fn scan(&self) -> Result<Vec<JavaRuntime>, RepositoryError> {
        let response = self.link.expect(&Request::JavaScan, "javas")?;
        match response {
            Response::Javas { items } => {
                let mut out = Vec::with_capacity(items.len());
                decode_list("java", items, JavaRuntime::try_from, &mut out)?;
                Ok(out)
            }
            other => Err(unexpected(other, "java.scan")),
        }
    }

    /// 在对端安装 Java；返回对端登记的全部运行时（新增项也在其中）
    ///
    /// # Errors
    ///
    /// 来源不可达、下载 / 解压失败、对端拒绝或传输失败时返回 [`RepositoryError`]
    pub fn install(
        &self,
        major: i64,
        jre: bool,
        vendor: Option<&str>,
    ) -> Result<Vec<JavaRuntime>, RepositoryError> {
        let response = self.link.expect(
            &Request::JavaInstall {
                major,
                jre,
                vendor: vendor.map(str::to_owned),
            },
            "javas",
        )?;
        match response {
            Response::Javas { items } => {
                let mut out = Vec::with_capacity(items.len());
                decode_list("java", items, JavaRuntime::try_from, &mut out)?;
                Ok(out)
            }
            other => Err(unexpected(other, "java.install")),
        }
    }

    /// 在对端卸载 `path` 指向的运行时；返回（路径，是否删除了磁盘文件）
    ///
    /// # Errors
    ///
    /// 对端拒绝或传输失败时返回 [`RepositoryError`]
    pub fn uninstall(&self, path: &Path) -> Result<(std::path::PathBuf, bool), RepositoryError> {
        let response = self.link.expect(
            &Request::JavaUninstall {
                path: path.to_string_lossy().into_owned(),
            },
            "java_uninstalled",
        )?;
        match response {
            Response::JavaUninstalled { path, managed } => {
                Ok((std::path::PathBuf::from(path), managed))
            }
            other => Err(unexpected(other, "java.uninstall")),
        }
    }
}

impl port::JavaRuntimeRepository for RemoteJavaRepository {
    fn upsert(&self, _runtime: &JavaRuntime) -> Result<(), RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "registering a remote JVM is a remote-side operation",
        ))))
    }

    fn list(&self) -> Result<Vec<JavaRuntime>, RepositoryError> {
        let response = self.link.expect(&Request::JavaList, "javas")?;
        match response {
            Response::Javas { items } => {
                let mut out = Vec::with_capacity(items.len());
                decode_list("java", items, JavaRuntime::try_from, &mut out)?;
                Ok(out)
            }
            other => Err(unexpected(other, "java.list")),
        }
    }

    fn get_by_path(&self, path: &Path) -> Result<JavaRuntime, RepositoryError> {
        self.list()?
            .into_iter()
            .find(|rt| rt.path == path)
            .ok_or_else(|| RepositoryError::NotFound(path.display().to_string()))
    }

    fn remove(&self, _path: &Path) -> Result<(), RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "unregistering a remote JVM is a remote-side operation",
        ))))
    }

    fn scan_state(&self) -> Result<Option<i64>, RepositoryError> {
        // 扫描状态是对端本机口径，镜像不缓存它；对端在 list 时自己补扫
        Ok(None)
    }

    fn mark_scanned(&self, _at: i64) -> Result<(), RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "scan state is remote-side state",
        ))))
    }

    fn list_sourced(&self) -> Result<Vec<Sourced<JavaRuntime>>, RepositoryError> {
        Ok(self
            .list()?
            .into_iter()
            .map(|rt| Sourced::new(self.origin.clone(), rt))
            .collect())
    }
}

/// 把载荷列表解成领域对象；任何一条解不出来都按记录损坏上报
fn decode_list<T, U>(
    what: &'static str,
    items: Vec<T>,
    mut convert: impl FnMut(T) -> Result<U, RepositoryError>,
    out: &mut Vec<U>,
) -> Result<(), RepositoryError> {
    for item in items {
        match convert(item) {
            Ok(value) => out.push(value),
            Err(err) => return Err(RepositoryError::Corrupt(format!("{what}: {err}"))),
        }
    }
    Ok(())
}

/// 分派表之外的响应形态：说明两端协议版本对不上或有分叉，按损坏上报
///
/// 按值接收：调用点都是 `match` 的 `other` 绑定，把所有权交给这里最直接。
#[allow(clippy::needless_pass_by_value)]
fn unexpected(response: Response, op: &'static str) -> RepositoryError {
    RepositoryError::Corrupt(format!("{op} returned an unexpected frame: {response:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::testkit::{PairingHarness, encoding};
    use domain::server::version::ServerVariant;
    use port::JavaRuntimeRepository as _;
    use port::ServerRepository as _;
    use port::WorkspaceRepository as _;
    use std::path::PathBuf;

    fn origin() -> Origin {
        Origin::Remote(port::RemoteName::parse("test-box").expect("name"))
    }

    #[test]
    fn workspace_list_and_get_roundtrip_over_real_tls() {
        let harness = PairingHarness::spawn();
        let link = harness.link();
        let repo = RemoteWorkspaceRepository::new(link, origin());
        let workspaces = repo.list().expect("list");
        // agent 的空数据目录：还没有工作区
        assert!(workspaces.is_empty());
        assert!(repo.current().expect("current").is_none());
    }

    #[test]
    fn workspace_init_creates_on_the_agent_side() {
        let harness = PairingHarness::spawn();
        let agent_dir = harness.agent_data.join("srv");
        let repo = RemoteWorkspaceRepository::new(harness.link(), origin());
        let ws = repo.init(&agent_dir, Some("w1")).expect("init");
        assert_eq!(ws.name.as_str(), "w1");
        // 垫片不落盘：目录是否真被创建由 `crates/mcsup` 的 agent 集成测试负责
        assert_eq!(ws.path.to_string_lossy(), agent_dir.to_string_lossy());

        let list = repo.list().expect("list");
        assert_eq!(list.len(), 1);

        let got = repo.get(&ws.id).expect("get");
        assert_eq!(got.id, ws.id);
        let by_name = repo.get_by_name(&ws.name).expect("get_by_name");
        assert_eq!(by_name.name.as_str(), "w1");

        let sourced = repo.list_sourced().expect("sourced");
        assert!(matches!(&sourced[0].origin, Origin::Remote(_)));
        assert!(sourced[0].value.is_current, "第一个工作区自动成为当前");
    }

    #[test]
    fn server_list_and_create_roundtrip_over_real_tls() {
        let harness = PairingHarness::spawn();
        let agent_dir = harness.agent_data.join("srv");
        let workspaces = RemoteWorkspaceRepository::new(harness.link(), origin());
        workspaces.init(&agent_dir, Some("w1")).expect("init ws");

        let servers = RemoteServerRepository::new(harness.link(), origin());
        let list = servers.list().expect("list");
        assert!(list.is_empty());

        let created = servers
            .create(Some("w1"), "web", "1.21.1", "vanilla", Some(21))
            .expect("create");
        assert_eq!(created.name.as_str(), "web");

        let after = servers.list().expect("list");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, created.id);

        let got = servers.get(&created.name).expect("get");
        assert_eq!(got.version.as_str(), "1.21.1");
        assert!(matches!(got.variant, ServerVariant::Vanilla));

        let sourced = servers.list_sourced().expect("sourced");
        assert!(matches!(&sourced[0].origin, Origin::Remote(_)));

        // 重复创建：垫片不查重，这里只验证「创建 → 列表可见 → 注销」闭环；
        // 重复拒绝由对端 service 保证（agent 集成测试已覆盖）
        assert!(!servers.remove("web", false).expect("remove"));
        assert!(servers.list().expect("list").is_empty());
        let err = servers.get(&created.name).expect_err("gone");
        assert!(matches!(err, RepositoryError::NotFound(_)), "{err:?}");
    }

    #[test]
    fn java_list_over_real_tls() {
        let harness = PairingHarness::spawn();
        let java = RemoteJavaRepository::new(harness.link(), origin());
        // agent 数据目录没有 JVM，也没有 java 注册记录；list 触发对端扫描后为空
        let list = java.list().expect("list");
        assert!(list.is_empty());
        let sourced = java.list_sourced().expect("sourced");
        assert!(sourced.is_empty());

        let path = PathBuf::from("/nowhere/java");
        let err = java.get_by_path(&path).expect_err("not found");
        assert!(matches!(err, RepositoryError::NotFound(_)), "{err:?}");

        // 扫描是对端动作：对端登记 0 条 JVM 时正常返回空列表（不报错）
        let scanned = java.scan().expect("scan");
        assert!(scanned.is_empty());
    }

    #[test]
    fn java_install_and_uninstall_roundtrip_over_real_tls() {
        let harness = PairingHarness::spawn();
        let java = RemoteJavaRepository::new(harness.link(), origin());
        let runtimes = java.install(21, false, None).expect("install");
        assert_eq!(runtimes.len(), 1);
        assert_eq!(runtimes[0].major.as_i64(), 21);
        assert!(runtimes[0].managed);

        let (path, managed) = java.uninstall(&runtimes[0].path).expect("uninstall");
        assert_eq!(path, PathBuf::from("/opt/java/21"));
        assert!(managed, "垫片按 managed 删除回帧");
    }

    #[test]
    fn server_install_updates_the_peer_record() {
        let harness = PairingHarness::spawn();
        let servers = RemoteServerRepository::new(harness.link(), origin());
        let _ = servers
            .create(None, "web", "1.21.1", "vanilla", None)
            .expect("create");
        let updated = servers.install("web", "1.21.4").expect("install");
        assert_eq!(updated.version.as_str(), "1.21.4");
        assert_eq!(updated.updated_at, 2);
    }

    #[test]
    fn server_create_modpack_decodes_the_applied_frame() {
        let harness = PairingHarness::spawn();
        let servers = RemoteServerRepository::new(harness.link(), origin());
        let result = servers
            .create_modpack(None, "atm", "all-the-mods", None)
            .expect("create modpack");
        assert_eq!(result.project_id, "all-the-mods");
        assert_eq!(result.version_id, "v1", "缺省版本由对端决定");
        assert_eq!(result.minecraft, "1.21.1");
        assert_eq!(result.loader.as_deref(), Some("fabric 0.16.9"));
        assert_eq!(result.files, 3);
        assert_eq!(result.downloaded_bytes, 1024);
        // 记录已落在对端，客户端的镜像立即可见
        assert_eq!(servers.list().expect("list").len(), 1);
    }

    #[test]
    fn props_roundtrip_keeps_the_raw_text() {
        let harness = PairingHarness::spawn();
        let servers = RemoteServerRepository::new(harness.link(), origin());
        let (path, text) = servers.props("any-id").expect("props");
        assert_eq!(path, "/srv/mc/server.properties");
        assert!(text.is_empty(), "还没有写入过");

        let updates = [
            ("motd".to_owned(), "hello".to_owned()),
            ("max-players".to_owned(), "20".to_owned()),
        ];
        let (path, text) = servers.set_props("any-id", &updates).expect("set props");
        assert_eq!(path, "/srv/mc/server.properties");
        assert_eq!(text, "motd=hello\nmax-players=20\n");
    }

    #[test]
    fn settings_set_decodes_the_normalized_value() {
        let harness = PairingHarness::spawn();
        let servers = RemoteServerRepository::new(harness.link(), origin());
        let (settings, value) = servers
            .set_setting("any-id", "ram", "2G")
            .expect("set setting");
        assert_eq!(value, "2G");
        assert_eq!(settings, ServerSettings::default());
    }

    #[test]
    fn server_link_registers_on_the_peer() {
        let harness = PairingHarness::spawn();
        let servers = RemoteServerRepository::new(harness.link(), origin());
        let result = servers
            .link_directory("/srv/mc/external", Some("ext"), None)
            .expect("link");
        assert_eq!(result.meta.name.as_str(), "ext");
        assert_eq!(result.target, "/srv/mc/external");
        assert!(result.linked, "垫片按「建立符号链接」回帧");
        // 注册即在对端可见
        assert_eq!(servers.list().expect("list").len(), 1);
    }

    #[test]
    fn server_variants_and_versions_decode_from_the_peer() {
        let harness = PairingHarness::spawn();
        let servers = RemoteServerRepository::new(harness.link(), origin());
        let variants = servers.variants().expect("variants");
        assert_eq!(variants, vec!["vanilla".to_owned(), "paper".to_owned()]);

        let versions = servers.versions("paper").expect("versions");
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].id.as_str(), "1.21.1");
        assert!(matches!(versions[0].variant, ServerVariant::Paper));
    }

    #[test]
    fn trait_write_methods_are_explicitly_unsupported() {
        // 编译期验证：这些方法存在且返回错误，而不是 panic
        let harness = PairingHarness::spawn();
        let servers = RemoteServerRepository::new(harness.link(), origin());
        let name = ServerName::parse("web").expect("name");
        let meta = encoding::server_meta(&name, encoding::workspace_id());
        let err = port::ServerRepository::create(&servers, &meta).expect_err("unsupported");
        assert!(matches!(err, RepositoryError::Storage(_)), "{err:?}");

        let java = RemoteJavaRepository::new(harness.link(), origin());
        let err = port::JavaRuntimeRepository::upsert(&java, &encoding::java_runtime())
            .expect_err("unsupported");
        assert!(matches!(err, RepositoryError::Storage(_)), "{err:?}");
    }
}
