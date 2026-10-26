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
use port::content::{ContentSource, ContentSourceRegistry};
use port::modpack::{ModpackExtractor, ModpackSource, ModpackSourceRegistry};
use provider::fabric::FabricSource;
use provider::forge::ForgeSource;
use provider::http::HttpByteSource;
use provider::java::JavaSourceRegistry;
use provider::leaf::LeafSource;
use provider::modrinth::{ModrinthModpackSource, ModrinthSource, MrpackExtractor};
use provider::mojang::MojangSource;
use provider::neoforge::NeoForgeSource;
use provider::papermc::{PaperProject, PaperSource};
use provider::purpur::PurpurSource;
use provider::quilt::QuiltSource;
use provider::sponge::SpongeSource;
use service::{
    ContentService, JavaService, ModpackService, ServerLifecycleService, ServerRegistry,
    WorkspaceService,
};
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
    /// 内容资源安装（Mod / 资源包 / 数据包 …）
    pub content: ContentService,
    /// 整合包开服编排（取回清单 → 装加载器 → 铺 overrides → 装文件）
    pub modpack: ModpackService,
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
    backend: Option<Arc<dyn port::Backend>>,
    backup_store: Option<Arc<dyn port::BackupStore>>,
    events: Option<Arc<dyn port::EventSink>>,
    jar_sources: Option<Vec<Arc<dyn ServerJarSource>>>,
    content_sources: Option<Vec<Arc<dyn ContentSource>>>,
    modpack_sources: Option<Vec<Arc<dyn ModpackSource>>>,
    modpack_extractor: Option<Arc<dyn ModpackExtractor>>,
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
            .field("backend", &"Option<dyn …>")
            .field("backup_store", &"Option<dyn …>")
            .field("events", &"Option<dyn …>")
            .field(
                "jar_sources",
                &self.jar_sources.as_ref().map_or(0, Vec::len),
            )
            .field(
                "content_sources",
                &self.content_sources.as_ref().map_or(0, Vec::len),
            )
            .field(
                "modpack_sources",
                &self.modpack_sources.as_ref().map_or(0, Vec::len),
            )
            .field("modpack_extractor", &self.modpack_extractor.is_some())
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
    ///
    /// 与 [`Self::backend`] 二选一：给定 `backend` 时优先级更高，`supervisor`
    /// 只影响默认装配出的 `LocalBackend`。
    #[must_use]
    pub fn supervisor(mut self, supervisor: Arc<dyn port::SupervisorPort>) -> Self {
        self.supervisor = Some(supervisor);
        self
    }

    /// 注入自定义执行位置（Local / Remote），整体接管进程、存储与备份
    ///
    /// 给定后不再按默认 `LocalBackend` 装配；`supervisor` 忽略。
    #[must_use]
    pub fn backend(mut self, backend: Arc<dyn port::Backend>) -> Self {
        self.backend = Some(backend);
        self
    }

    /// 注入自定义备份数据能力（扫描 / CAS / 快照；默认本地文件系统）
    #[must_use]
    pub fn backup_store(mut self, store: Arc<dyn port::BackupStore>) -> Self {
        self.backup_store = Some(store);
        self
    }

    /// 注入领域事件出口（默认丢弃）
    ///
    /// CLI / API / MCP / Audit / Notification 通过它订阅 `ServerStarted`、
    /// `BackupCompleted` 等事实；事件是旁路，发不出去不影响业务。
    #[must_use]
    pub fn events(mut self, events: Arc<dyn port::EventSink>) -> Self {
        self.events = Some(events);
        self
    }

    /// 注入自定义服务端来源集合（默认 Mojang vanilla + Fabric）
    #[must_use]
    pub fn jar_sources(mut self, sources: Vec<Arc<dyn ServerJarSource>>) -> Self {
        self.jar_sources = Some(sources);
        self
    }

    /// 注入自定义内容来源集合（默认 Modrinth 官方）
    ///
    /// 换源（自建 Labrinth / 镜像 / 其他站点）在这里替换，不改 Service。顺序
    /// 有意义：不指定来源名时用第一个。
    #[must_use]
    pub fn content_sources(mut self, sources: Vec<Arc<dyn ContentSource>>) -> Self {
        self.content_sources = Some(sources);
        self
    }

    /// 注入整合包来源集合（默认 Modrinth）
    ///
    /// 换源与内容来源同理：Service 只依赖 trait，装配层决定用哪个实现。
    #[must_use]
    pub fn modpack_sources(mut self, sources: Vec<Arc<dyn ModpackSource>>) -> Self {
        self.modpack_sources = Some(sources);
        self
    }

    /// 注入整合包归档读取实现（默认 `unzip` 版）
    #[must_use]
    pub fn modpack_extractor(mut self, extractor: Arc<dyn ModpackExtractor>) -> Self {
        self.modpack_extractor = Some(extractor);
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
        let supervisor: Arc<dyn port::SupervisorPort> = self
            .supervisor
            .unwrap_or_else(|| Arc::new(runtime::LocalSupervisor));
        // 执行位置：只聚合进程生命周期与安装器；存储/备份是另一个能力轴
        let backend: Arc<dyn port::Backend> = match self.backend {
            Some(backend) => backend,
            None => Arc::new(backend::LocalBackend::new(
                Arc::clone(&supervisor),
                Arc::new(runtime::LocalInstallerRunner::new()),
            )),
        };
        let backup_store: Arc<dyn port::BackupStore> = match self.backup_store {
            Some(store) => store,
            None => Arc::new(backend::LocalBackupStore),
        };
        let workspaces =
            WorkspaceService::new(Arc::clone(&workspaces_repo), Arc::clone(&servers_repo));
        let events: Arc<dyn port::EventSink> =
            self.events.unwrap_or_else(|| Arc::new(port::NoopEventSink));
        let lifecycle = ServerLifecycleService::with_events(
            Arc::clone(&registry),
            java.clone(),
            Arc::clone(&backend),
            Arc::clone(&events),
        );
        let backup =
            service::BackupService::new(Arc::clone(&registry), Arc::clone(&backend), backup_store)
                .with_events(events);

        let jar_sources: Vec<Arc<dyn ServerJarSource>> = match self.jar_sources {
            Some(sources) => sources,
            None => default_jar_sources()?,
        };

        let content_sources = match self.content_sources {
            Some(sources) => sources,
            None => default_content_sources()?,
        };
        let content = ContentService::new(
            Arc::new(ContentSourceRegistry::new(content_sources)),
            Arc::clone(&downloader),
        );

        let modpack_sources = match self.modpack_sources {
            Some(sources) => sources,
            None => default_modpack_sources()?,
        };
        let modpack = ModpackService::new(
            Arc::new(ModpackSourceRegistry::new(modpack_sources)),
            self.modpack_extractor
                .unwrap_or_else(|| Arc::new(MrpackExtractor::new())),
            Arc::clone(&downloader),
        );

        Ok(Mcsup {
            workspaces,
            registry,
            java,
            lifecycle,
            backup,
            jar_sources,
            content,
            modpack,
            store,
        })
    }
}

/// 默认的内容来源集合：Modrinth 官方
///
/// 只准备 HTTP 客户端，不打网络。换源由 [`McsupBuilder::content_sources`]
/// 覆盖；自建 / 镜像用 `ModrinthSource::builder().base_url(..)` 构造。
///
/// # Errors
///
/// 来源构造失败时返回 [`BootstrapError::Source`]
pub fn default_content_sources() -> Result<Vec<Arc<dyn ContentSource>>, BootstrapError> {
    let source = ModrinthSource::new().map_err(|err| BootstrapError::Source(Box::new(err)))?;
    Ok(vec![Arc::new(source) as Arc<dyn ContentSource>])
}

/// 默认的整合包来源集合：Modrinth 官方
///
/// # Errors
///
/// 来源构造失败时返回 [`BootstrapError::Source`]
pub fn default_modpack_sources() -> Result<Vec<Arc<dyn ModpackSource>>, BootstrapError> {
    let source =
        ModrinthModpackSource::new().map_err(|err| BootstrapError::Source(Box::new(err)))?;
    Ok(vec![Arc::new(source) as Arc<dyn ModpackSource>])
}

/// 默认的来源集合（与 CLI 完全一致）
///
/// 这是「开箱即用能装哪些变体」的唯一口径。顺序有意义：同一变体若被注册多次，
/// [`port::ServerJarSource`] 的查找按列表顺序取第一个，因此这里每种变体只出现
/// 一次。Paper 系四个变体共用一次 [`PaperSource`] 实现，按 [`PaperProject`]
/// 参数化——这就是「不同启动方式复用、不每变体各写一套」的落地形态。
///
/// 构造只准备 HTTP 客户端，不打网络；任一来源构造失败（如 rustls provider
/// 装不上）即整体失败，不静默降级成「少几个变体」。
///
/// # Errors
///
/// 任一来源构造失败时返回 [`BootstrapError::Source`]
pub fn default_jar_sources() -> Result<Vec<Arc<dyn ServerJarSource>>, BootstrapError> {
    fn wrap<E>(
        result: Result<E, port::SourceError>,
    ) -> Result<Arc<dyn ServerJarSource>, BootstrapError>
    where
        E: ServerJarSource + 'static,
    {
        result
            .map(|source| Arc::new(source) as Arc<dyn ServerJarSource>)
            .map_err(|err| BootstrapError::Source(Box::new(err)))
    }

    let mut sources: Vec<Arc<dyn ServerJarSource>> = vec![
        wrap(MojangSource::new())?,
        wrap(FabricSource::new())?,
        wrap(ForgeSource::new())?,
        wrap(NeoForgeSource::new())?,
        wrap(PurpurSource::new())?,
        wrap(LeafSource::new())?,
        wrap(QuiltSource::new())?,
        wrap(SpongeSource::new())?,
    ];
    // Paper 系：一个实现，按项目参数化
    for project in [
        PaperProject::Paper,
        PaperProject::Folia,
        PaperProject::Velocity,
        PaperProject::Waterfall,
    ] {
        sources.push(wrap(PaperSource::new(project))?);
    }
    Ok(sources)
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

    /// 默认来源覆盖**全部**内置变体（除 `Unknown`）
    ///
    /// 这是「注册表不漏项」的断言：新增内置变体却忘了在
    /// [`default_jar_sources`] 里装配来源时，这里会直接失败。
    #[test]
    fn default_jar_sources_cover_every_builtin_variant() {
        use domain::server::variant::ServerVariant;

        let sources = default_jar_sources().expect("默认来源");
        let registered: Vec<ServerVariant> = sources.iter().map(|s| s.variant()).collect();

        // 两个有意的例外：
        // - `Unknown` 是历史数据兜底项，不是可安装变体
        // - `ModrinthModpack` 由整合包来源承载（`default_modpack_sources`），
        //   没有 `ServerJarSource`——加载器由清单决定，不由变体决定
        let non_jar = [ServerVariant::Unknown, ServerVariant::ModrinthModpack];
        let mut missing: Vec<&str> = Vec::new();
        for variant in ServerVariant::builtin() {
            if !non_jar.contains(variant) && !registered.contains(variant) {
                missing.push(variant.as_str());
            }
        }
        assert!(missing.is_empty(), "缺少来源的变体：{missing:?}");

        // 整合包变体必须恰好由整合包来源覆盖
        let modpacks = default_modpack_sources().expect("默认整合包来源");
        assert!(
            !modpacks.is_empty(),
            "整合包变体没有对应来源，server create 会给出一个无法安装的选项"
        );
        assert!(
            sources
                .iter()
                .all(|source| source.variant() != ServerVariant::ModrinthModpack),
            "整合包变体不应注册 JAR 来源"
        );

        // 每种变体只注册一次：重复注册时 `pick_source` 只认第一个，
        // 第二个会变成永远走不到的死代码
        let mut seen: Vec<ServerVariant> = Vec::new();
        for variant in &registered {
            assert!(
                !seen.contains(variant),
                "变体 {} 被重复注册",
                variant.as_str()
            );
            seen.push(variant.clone());
        }
    }

    /// 来源名与变体标识的关系是**有意**分层的
    ///
    /// `name()` 是「数据来源」，`variant().as_str()` 是「服务端软件」。多数来源
    /// 二者同名（paper 来自 PaperMC），但原版的来源是 Mojang 的官网，因此
    /// `name()` 为 `mojang` 而变体为 `vanilla`。这里把这个例外钉死：不要为了让
    /// 两者一致去改任何一方，它们本来就是两个概念。
    #[test]
    fn source_names_follow_the_upstream_not_the_variant() {
        let sources = default_jar_sources().expect("默认来源");
        for source in &sources {
            let variant = source.variant();
            let name = source.name();
            if name == "mojang" {
                assert_eq!(variant, domain::server::variant::ServerVariant::Vanilla);
                continue;
            }
            assert_eq!(name, variant.as_str(), "来源名与变体标识不一致");
        }
    }
}
