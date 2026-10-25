//! 默认组合根：把「开箱即用」的装配从 `apps/cli` 下沉到库侧
//!
//! 第三方 Rust 宿主不必自己拼 `JsonStore` + 各实现——这里给出与 CLI
//! 完全相同的一套默认装配；要接管某项能力时，用 `McsupBuilder` 注入
//! 自己的实现覆盖对应字段（port trait 全部公开、不 sealed）。
//!
//! 契约（与 `docs/embedding.md` 公开 API 约束一致）：
//! - 无全局状态、无 lazy_static / once_cell 单例；实例由调用方持有
//! - 库不读配置文件、不读环境变量；路径等参数由调用方传入
//! - 构造不打网络：provider 只准备客户端，探测 / 排序延迟到安装阶段

use file::{CommandExtractor, FileDownloader};
use java::HostJavaScanner;
use port::ServerJarSource;
use provider::fabric::FabricSource;
use provider::forge::ForgeSource;
use provider::http::HttpByteSource;
use provider::java::JavaSourceRegistry;
use provider::mojang::MojangSource;
use service::{JavaService, ServerLifecycleService, ServerRegistry, WorkspaceService};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use storage::JsonStore;
use thiserror::Error;

/// 组合根装配失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BootstrapError {
    /// 元数据仓库打开失败
    #[error("open store at {path}: {source}")]
    Store {
        /// 仓库根目录
        path: PathBuf,
        /// 底层错误
        #[source]
        source: port::RepositoryError,
    },
    /// HTTP 传输初始化失败
    #[error("init http transport: {0}")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// 服务端来源初始化失败
    #[error("init server jar source: {0}")]
    Source(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// 目录创建失败
    #[error("create directory {path}: {source}")]
    Dir {
        /// 目录路径
        path: PathBuf,
        /// 底层错误
        #[source]
        source: std::io::Error,
    },
}

/// 默认组合根：一次装配全部服务
///
/// 持有它即持有整套 MCSUP；各字段按需取用。注入自定义能力见
/// [`McsupBuilder`]。
///
/// 注意：默认 supervisor（`LocalSupervisor`）通过拉起 `supervisor_exe`
/// 子进程工作，`server start` 时必须传入带 `__supervise` 隐藏子命令的
/// mcsup CLI 路径；宿主启动器通常应注入自己的
/// [`port::SupervisorPort`] 接管进程生命周期，而不是复用默认实现。
pub struct Mcsup {
    /// 工作区服务
    pub workspaces: WorkspaceService,
    /// 服务器注册表
    pub registry: Arc<ServerRegistry>,
    /// Java 检测与安装
    pub java: JavaService,
    /// 生命周期编排（start / stop / status / send / logs）
    pub lifecycle: ServerLifecycleService,
    /// 备份编排（create / list / restore / delete / verify / prune）
    pub backup: service::BackupService,
    /// 已注册的服务端 JAR 来源（能力查询数据源）
    pub jar_sources: Vec<Arc<dyn ServerJarSource>>,
    /// 元数据仓库句柄（Workspace / Server / Java 三个仓库共用）
    ///
    /// 具体存储类型是实现细节，对外只经 [`McsupBuilder::server_repository`]
    /// / [`McsupBuilder::workspace_repository`] 的 trait 注入口替换。
    store: Arc<JsonStore>,
}

impl std::fmt::Debug for Mcsup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mcsup").finish_non_exhaustive()
    }
}

impl Mcsup {
    /// 按平台数据目录装配：仓库根目录在 `<data_dir>/db`，其中 `workspaces/`、`servers/`、`java/` 各自一条记录一个文件
    ///
    /// `data_dir` 由调用方决定（库不读环境变量）。
    ///
    /// # Errors
    ///
    /// 仓库打开、目录创建或来源初始化失败时返回 [`BootstrapError`]
    pub fn open(data_dir: &Path) -> Result<Self, BootstrapError> {
        McsupBuilder::new().build(data_dir)
    }

    /// 元数据仓库句柄（仅测试与诊断用；业务访问经各 service）
    #[must_use]
    pub fn store(&self) -> &Arc<JsonStore> {
        &self.store
    }

    /// Java 安装根目录约定：`<data_dir>/java`
    ///
    /// # Errors
    ///
    /// 目录创建失败时返回 [`BootstrapError`]
    pub fn java_root(data_dir: &Path) -> Result<PathBuf, BootstrapError> {
        let root = data_dir.join("java");
        std::fs::create_dir_all(&root).map_err(|source| BootstrapError::Dir {
            path: root.clone(),
            source,
        })?;
        Ok(root)
    }
}

/// 组合根构造器：每个字段都是 `Option`，不注入就用内置默认实现
///
/// ```no_run
/// use std::path::Path;
/// use mcsup::bootstrap::McsupBuilder;
///
/// let mcsup = McsupBuilder::new()
///     // .supervisor(my_supervisor)   // 启动器通常注入此项接管进程
///     .build(Path::new("/data/mcsup"))
///     .expect("bootstrap");
/// ```
#[derive(Default)]
pub struct McsupBuilder {
    store: Option<Arc<JsonStore>>,
    servers_repo: Option<Arc<dyn port::ServerRepository>>,
    workspaces_repo: Option<Arc<dyn port::WorkspaceRepository>>,
    java_repo: Option<Arc<dyn port::JavaRuntimeRepository>>,
    downloader: Option<Arc<dyn port::ArtifactDownloader>>,
    scanner: Option<Arc<dyn port::JavaScanner>>,
    java_registry: Option<Arc<dyn port::JavaSourceRegistryPort>>,
    extractor: Option<Arc<dyn port::ArchiveExtractor>>,
    supervisor: Option<Arc<dyn port::SupervisorPort>>,
    jar_sources: Option<Vec<Arc<dyn ServerJarSource>>>,
}

impl std::fmt::Debug for McsupBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McsupBuilder")
            .field("store", &self.store.is_some())
            .field("servers_repo", &self.servers_repo.is_some())
            .field("workspaces_repo", &self.workspaces_repo.is_some())
            .field("java_repo", &self.java_repo.is_some())
            .field("downloader", &"Option<dyn …>")
            .field("scanner", &"Option<dyn …>")
            .field("java_registry", &"Option<dyn …>")
            .field("extractor", &"Option<dyn …>")
            .field("supervisor", &"Option<dyn …>")
            .field(
                "jar_sources",
                &self.jar_sources.as_ref().map_or(0, Vec::len),
            )
            .finish()
    }
}

impl McsupBuilder {
    /// 空构造器：全部走默认实现
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入自定义仓库（如内存实现、未来网络实现）
    ///
    /// 只换连接实现；Workspace / Server 两个仓库仍由该库派生。要换掉
    /// 仓库实现本身（自定义持久化），用 [`Self::server_repository`] /
    /// [`Self::workspace_repository`]。
    #[must_use]
    pub fn store(mut self, store: Arc<JsonStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// 注入自定义服务器仓库（按 trait，覆盖默认 JSON 实现）
    ///
    /// 传入后 [`Mcsup::registry`] 的工作区关联检查仍用默认实现，
    /// 因此通常与 [`Self::workspace_repository`] 成对注入。
    #[must_use]
    pub fn server_repository(mut self, repo: Arc<dyn port::ServerRepository>) -> Self {
        self.servers_repo = Some(repo);
        self
    }

    /// 注入自定义工作区仓库（按 trait，覆盖默认 JSON 实现）
    #[must_use]
    pub fn workspace_repository(mut self, repo: Arc<dyn port::WorkspaceRepository>) -> Self {
        self.workspaces_repo = Some(repo);
        self
    }

    /// 注入自定义 Java 运行时仓库（按 trait，覆盖默认 JSON 实现）
    #[must_use]
    pub fn java_repository(mut self, repo: Arc<dyn port::JavaRuntimeRepository>) -> Self {
        self.java_repo = Some(repo);
        self
    }

    /// 注入自定义下载器（接管传输 / 断点续传 / 校验策略）
    #[must_use]
    pub fn downloader(mut self, downloader: Arc<dyn port::ArtifactDownloader>) -> Self {
        self.downloader = Some(downloader);
        self
    }

    /// 注入自定义 Java 扫描器（接管宿主已有 JVM 的发现）
    #[must_use]
    pub fn scanner(mut self, scanner: Arc<dyn port::JavaScanner>) -> Self {
        self.scanner = Some(scanner);
        self
    }

    /// 注入自定义 Java 源聚合（接管发行包来源）
    #[must_use]
    pub fn java_registry(mut self, registry: Arc<dyn port::JavaSourceRegistryPort>) -> Self {
        self.java_registry = Some(registry);
        self
    }

    /// 注入自定义归档解压器
    #[must_use]
    pub fn extractor(mut self, extractor: Arc<dyn port::ArchiveExtractor>) -> Self {
        self.extractor = Some(extractor);
        self
    }

    /// 注入自定义 supervisor（接管进程生命周期，宿主启动器常见诉求）
    #[must_use]
    pub fn supervisor(mut self, supervisor: Arc<dyn port::SupervisorPort>) -> Self {
        self.supervisor = Some(supervisor);
        self
    }

    /// 注入自定义服务端来源集合（默认 Mojang vanilla + Fabric）
    #[must_use]
    pub fn jar_sources(mut self, sources: Vec<Arc<dyn ServerJarSource>>) -> Self {
        self.jar_sources = Some(sources);
        self
    }

    /// 装配；`data_dir` 为平台数据目录
    ///
    /// # Errors
    ///
    /// 见 [`BootstrapError`]
    pub fn build(self, data_dir: &Path) -> Result<Mcsup, BootstrapError> {
        let db_root = data_dir.join("db");
        let store = match self.store {
            Some(store) => store,
            None => {
                Arc::new(
                    JsonStore::open(&db_root).map_err(|source| BootstrapError::Store {
                        path: db_root.clone(),
                        source,
                    })?,
                )
            }
        };

        let bytes = Arc::new(
            HttpByteSource::new().map_err(|err| BootstrapError::Transport(Box::new(err)))?,
        );
        let downloader = self.downloader.unwrap_or_else(|| {
            Arc::new(FileDownloader::new(bytes.clone()).with_range_source(bytes))
        });

        let java_root = Mcsup::java_root(data_dir)?;
        let java_repo: Arc<dyn port::JavaRuntimeRepository> =
            self.java_repo.unwrap_or_else(|| store.clone());
        let java = JavaService::new(
            java_repo,
            self.scanner
                .unwrap_or_else(|| Arc::new(HostJavaScanner::new())),
            self.java_registry
                .unwrap_or_else(|| Arc::new(JavaSourceRegistry::all_sources())),
            downloader.clone(),
            self.extractor
                .unwrap_or_else(|| Arc::new(CommandExtractor::new())),
            java_root,
        );

        let servers_repo: Arc<dyn port::ServerRepository> =
            self.servers_repo.unwrap_or_else(|| store.clone());
        let workspaces_repo: Arc<dyn port::WorkspaceRepository> =
            self.workspaces_repo.unwrap_or_else(|| store.clone());
        let registry = Arc::new(ServerRegistry::new(
            Arc::clone(&servers_repo),
            Arc::clone(&workspaces_repo),
        ));
        let workspaces = WorkspaceService::new(workspaces_repo, servers_repo);
        let supervisor: Arc<dyn port::SupervisorPort> = self
            .supervisor
            .unwrap_or_else(|| Arc::new(runtime::LocalSupervisor));
        let lifecycle = ServerLifecycleService::new(
            Arc::clone(&registry),
            java.clone(),
            Arc::clone(&supervisor),
        );
        let backup = service::BackupService::new(Arc::clone(&registry), supervisor);

        let jar_sources: Vec<Arc<dyn ServerJarSource>> = match self.jar_sources {
            Some(sources) => sources,
            None => vec![
                Arc::new(MojangSource::new().map_err(|err| BootstrapError::Source(Box::new(err)))?),
                Arc::new(FabricSource::new().map_err(|err| BootstrapError::Source(Box::new(err)))?),
                Arc::new(ForgeSource::new().map_err(|err| BootstrapError::Source(Box::new(err)))?),
            ],
        };

        Ok(Mcsup {
            workspaces,
            registry,
            java,
            lifecycle,
            backup,
            jar_sources,
            store,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内存库 + 全默认装配必须能跑通：第三方「零拼装」的验收线
    #[test]
    fn builds_with_in_memory_store() {
        let store = Arc::new(JsonStore::open_in_memory().expect("memory store"));
        let dir = std::env::temp_dir().join(format!("mcsup-bootstrap-{}", std::process::id()));
        let mcsup = McsupBuilder::new()
            .store(store)
            .build(&dir)
            .expect("bootstrap");
        // 三个仓库共用同一 store：装配后立即可查询
        assert!(mcsup.workspaces.list().expect("list").is_empty());
        assert!(mcsup.registry.list().expect("list").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 变体能力来自注入的 `jar_sources`，默认覆盖 vanilla 与 fabric
    #[test]
    fn default_jar_sources_cover_vanilla_and_fabric() {
        let store = Arc::new(JsonStore::open_in_memory().expect("memory store"));
        let dir = std::env::temp_dir().join(format!("mcsup-bootstrap-cap-{}", std::process::id()));
        let mcsup = McsupBuilder::new()
            .store(store)
            .build(&dir)
            .expect("bootstrap");
        let variants: Vec<_> = mcsup.jar_sources.iter().map(|s| s.variant()).collect();
        assert!(
            variants.contains(&domain::server::version::ServerVariant::Vanilla),
            "{variants:?}"
        );
        assert!(
            variants.contains(&domain::server::version::ServerVariant::Fabric),
            "{variants:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
