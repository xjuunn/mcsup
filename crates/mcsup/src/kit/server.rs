//! 服务器任务：建服（含整合包）、装 / 换核心、起停查状态、删服
//!
//! 这里只有「按什么顺序调 service」和「默认值从哪来」。状态判定、EULA 写入、
//! 核心可行性、运行中拒改磁盘都在 `service` / `domain`，kit 不复制任何一条。

use super::Kit;
use super::config::KitConfig;
use super::error::KitError;
use super::options::RunOptions;
use domain::java::JavaMajor;
use domain::server::variant::ServerVariant;
use domain::server::version::VersionId;
use domain::workspace::Workspace;
use port::{ScannedJava, StopOutcome, SupervisorStartOutcome, SupervisorStatus};
use service::{
    InitOutcome, InstalledServerJar, ModpackInstallOptions, ModpackPrepared, ServerRemoveOutcome,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Java 首次扫描开始时的回调
type FirstScanListener = Arc<dyn Fn() + Send + Sync>;
/// 发现一个 Java 运行时的回调
type JavaFoundListener = Arc<dyn Fn(&ScannedJava) + Send + Sync>;

/// 建服任务的单次覆盖
///
/// 只写这次要改的字段，其余由 [`KitConfig`] 补齐。`name` 是唯一的必填项：
/// [`CreateServer::default`] 不提供名字，直接调用会得到
/// [`KitError::MissingName`]。
///
/// 两种建法共用同一个入口：
///
/// - 普通核心：`variant` 决定加载器，`version` 是 MC 版本
/// - 整合包：给 [`Self::modpack`]，此时 `version` 是**整合包版本 ID**，
///   MC 版本与加载器由包内清单决定
///
/// ```no_run
/// use mcsup::domain::server::variant::ServerVariant;
/// use mcsup::kit::CreateServer;
///
/// // 名字必填，其余取默认
/// let plain = CreateServer::new("survival");
/// // 个别覆盖：只改版本与变体
/// let tuned = CreateServer::default()
///     .name("creative")
///     .version("1.21.4")
///     .variant(ServerVariant::Paper);
/// // 按整合包建服：加载器与 MC 版本取自清单
/// let packed = CreateServer::new("atm").modpack("all-the-mods-10");
/// ```
#[derive(Clone, Debug, Default)]
pub struct CreateServer {
    name: Option<String>,
    version: Option<String>,
    variant: Option<ServerVariant>,
    workspace: Option<String>,
    workspace_dir: Option<PathBuf>,
    java_major: Option<JavaMajor>,
    start: Option<bool>,
    accept_eula: Option<bool>,
    ram_mib: Option<u32>,
    modpack: Option<String>,
    modpack_source: Option<String>,
    run: RunOptions,
}

impl CreateServer {
    /// 以服务器名称开始构造
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            ..Self::default()
        }
    }

    /// 服务器名称（必填）
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// 版本；缺省用 [`KitConfig::default_version`]（默认 `latest`）
    ///
    /// 整合包模式下这里给的是**整合包版本 ID**，不是 MC 版本。
    #[must_use]
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// 按整合包建服：`project` 是来源里的项目 ID / slug
    ///
    /// 给本项后流程变为「取回清单 → 按清单落库 → 装加载器核心 → 铺 overrides
    /// → 下载清单文件」。显式 `variant` 必须与整合包模式一致，否则拒绝——
    /// 静默忽略调用方的变体只会让人以为装的是自己指定的加载器。
    #[must_use]
    pub fn modpack(mut self, project: impl Into<String>) -> Self {
        self.modpack = Some(project.into());
        self
    }

    /// 整合包来源名（如 `modrinth`）；缺省用注册表默认来源
    #[must_use]
    pub fn modpack_source(mut self, source: impl Into<String>) -> Self {
        self.modpack_source = Some(source.into());
        self
    }

    /// 服务端变体；缺省用 [`KitConfig::variant`]
    #[must_use]
    pub fn variant(mut self, variant: ServerVariant) -> Self {
        self.variant = Some(variant);
        self
    }

    /// 已存在的工作目录选择器（名称或路径）
    ///
    /// 与 [`Self::workspace_dir`] 二选一；同时给出时本字段被忽略。
    #[must_use]
    pub fn workspace(mut self, selector: impl Into<String>) -> Self {
        self.workspace = Some(selector.into());
        self
    }

    /// 直接指定一个目录，不存在就按工作目录初始化后再建服
    ///
    /// 判空、marker 与注册都走 `WorkspaceService::init`，与本仓库其他入口
    /// 同一套恢复逻辑：目录已登记只是缺 marker 时会补写，而不是报错。
    #[must_use]
    pub fn workspace_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.workspace_dir = Some(dir.into());
        self
    }

    /// 记录 Java 大版本要求（旧版本服务端用；安装成功后由核心元数据覆盖）
    #[must_use]
    pub fn java_major(mut self, major: JavaMajor) -> Self {
        self.java_major = Some(major);
        self
    }

    /// 建完是否起服；缺省用 [`KitConfig::start_after_create`]
    #[must_use]
    pub fn start(mut self, start: bool) -> Self {
        self.start = Some(start);
        self
    }

    /// 是否接受 EULA；缺省用 [`KitConfig::accept_eula`]
    #[must_use]
    pub fn accept_eula(mut self, accept: bool) -> Self {
        self.accept_eula = Some(accept);
        self
    }

    /// 起服时的堆上限（MiB）
    #[must_use]
    pub fn ram_mib(mut self, ram_mib: u32) -> Self {
        self.ram_mib = Some(ram_mib);
        self
    }

    /// 进度观察者与取消令牌
    #[must_use]
    pub fn run(mut self, run: RunOptions) -> Self {
        self.run = run;
        self
    }

    /// 与默认策略合并，得到本次任务的最终参数（纯计算，不碰 IO）
    fn resolve(self, config: &KitConfig) -> Result<ResolvedCreate, KitError> {
        let name = self.name.ok_or(KitError::MissingName)?;
        let accept_eula = self.accept_eula.unwrap_or(config.accept_eula);
        let start = self.start.unwrap_or(config.start_after_create);
        // 新目录里不可能已有 `eula.txt`，起服必然失败。在建任何东西之前拒绝，
        // 避免留下「建好了但起不来」的半成品。
        if start && !accept_eula {
            return Err(KitError::EulaNotAccepted);
        }
        // 整合包模式下加载器由清单决定；显式给了别的变体是调用方搞错了对象
        if let Some(variant) = &self.variant
            && self.modpack.is_some()
            && !service::is_modpack_variant(variant)
        {
            return Err(KitError::ConflictingVariant {
                variant: variant.as_str().to_owned(),
            });
        }
        let explicit_version = self.version;
        let modpack = self.modpack.map(|project| ModpackRequest {
            project,
            // 没给版本时交给来源挑最新正式版；不能拿配置里的 `latest`
            // 当包版本 ID，那是一个不存在的版本
            version: explicit_version.clone(),
            source: self.modpack_source,
        });
        Ok(ResolvedCreate {
            name,
            version: explicit_version.unwrap_or_else(|| config.default_version.clone()),
            variant: self.variant.unwrap_or_else(|| config.variant.clone()),
            workspace: self.workspace.or_else(|| config.workspace.clone()),
            workspace_dir: self.workspace_dir,
            java_major: self.java_major,
            accept_eula,
            start,
            ram_mib: self.ram_mib.or(config.ram_mib),
            modpack,
            run: self.run,
        })
    }
}

/// 整合包建服的输入
#[derive(Debug, Clone)]
struct ModpackRequest {
    project: String,
    /// 包版本 ID；`None` 由来源挑最新正式版
    version: Option<String>,
    source: Option<String>,
}

/// 合并后的建服参数
#[derive(Debug)]
struct ResolvedCreate {
    name: String,
    /// 普通建法的 MC 版本（整合包模式不使用，见 `modpack`）
    version: String,
    variant: ServerVariant,
    workspace: Option<String>,
    workspace_dir: Option<PathBuf>,
    java_major: Option<JavaMajor>,
    accept_eula: bool,
    start: bool,
    ram_mib: Option<u32>,
    modpack: Option<ModpackRequest>,
    run: RunOptions,
}

/// 建服前取回的整合包暂存目录：离开作用域即删除
///
/// 只删自己建的目录。归档可能由调用方从别处传入，按 `archive.parent()` 推断
/// 会连带删掉用户文件——这条与 CLI 的处置一致。
struct StagingGuard(PathBuf);

impl Drop for StagingGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 建服结果
#[derive(Debug)]
#[non_exhaustive]
pub struct CreatedServer {
    /// 已登记并装好核心的服务器
    pub core: InstalledServerJar,
    /// 请求了起服时的拉起结果；未起服为 `None`
    pub started: Option<SupervisorStartOutcome>,
}

/// 起服任务的单次覆盖
#[derive(Clone, Default)]
pub struct StartServer {
    accept_eula: Option<bool>,
    skip_mod_check: Option<bool>,
    ram_mib: Option<u32>,
    supervisor_exe: Option<PathBuf>,
    supervisor_args: Option<Vec<OsString>>,
    on_first_scan: Option<FirstScanListener>,
    on_found: Option<JavaFoundListener>,
    run: RunOptions,
}

impl StartServer {
    /// 是否接受 EULA；缺省用 [`KitConfig::accept_eula`]
    ///
    /// 服务器目录里已有 `eula=true` 时不会被覆盖，本项只决定「缺失时写不写」。
    #[must_use]
    pub fn accept_eula(mut self, accept: bool) -> Self {
        self.accept_eula = Some(accept);
        self
    }

    /// 是否跳过 mod 预检
    #[must_use]
    pub fn skip_mod_check(mut self, skip: bool) -> Self {
        self.skip_mod_check = Some(skip);
        self
    }

    /// 堆上限（MiB）；缺省用服务属性，其次 [`KitConfig::ram_mib`]
    #[must_use]
    pub fn ram_mib(mut self, ram_mib: u32) -> Self {
        self.ram_mib = Some(ram_mib);
        self
    }

    /// supervisor 可执行文件，覆盖 [`KitConfig::supervisor_exe`]
    #[must_use]
    pub fn supervisor_exe(mut self, exe: impl Into<PathBuf>) -> Self {
        self.supervisor_exe = Some(exe.into());
        self
    }

    /// supervisor 前置子命令，覆盖 [`KitConfig::supervisor_args`]
    #[must_use]
    pub fn supervisor_args(mut self, args: Vec<OsString>) -> Self {
        self.supervisor_args = Some(args);
        self
    }

    /// Java 首次扫描开始时的回调（扫描可能耗时数秒）
    #[must_use]
    pub fn on_first_scan(mut self, listener: FirstScanListener) -> Self {
        self.on_first_scan = Some(listener);
        self
    }

    /// 每发现一个 Java 运行时的回调
    #[must_use]
    pub fn on_found(mut self, listener: JavaFoundListener) -> Self {
        self.on_found = Some(listener);
        self
    }

    /// 取消令牌与观察者（起服本身不产生字节进度，观察者仅透传）
    #[must_use]
    pub fn run(mut self, run: RunOptions) -> Self {
        self.run = run;
        self
    }

    /// 与默认策略合并（纯计算，不碰 IO）
    fn resolve(
        &self,
        config: &KitConfig,
        external_supervisor: bool,
    ) -> Result<service::StartOptions, KitError> {
        let supervisor_exe = match self
            .supervisor_exe
            .clone()
            .or_else(|| config.supervisor_exe.clone())
        {
            Some(exe) => exe,
            // 宿主注入自己的 supervisor 时，该路径由宿主解释，库不猜
            None if external_supervisor => PathBuf::new(),
            None => return Err(KitError::MissingSupervisorExe),
        };
        Ok(service::StartOptions {
            accept_eula: self.accept_eula.unwrap_or(config.accept_eula),
            skip_mod_check: self.skip_mod_check.unwrap_or(config.skip_mod_check),
            ram_mib: self.ram_mib.or(config.ram_mib),
            supervisor_exe,
            supervisor_args: self
                .supervisor_args
                .clone()
                .unwrap_or_else(|| config.supervisor_args.clone()),
        })
    }
}

impl std::fmt::Debug for StartServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartServer")
            .field("accept_eula", &self.accept_eula)
            .field("skip_mod_check", &self.skip_mod_check)
            .field("ram_mib", &self.ram_mib)
            .field("supervisor_exe", &self.supervisor_exe)
            .field("supervisor_args", &self.supervisor_args)
            .field("on_first_scan", &self.on_first_scan.is_some())
            .field("on_found", &self.on_found.is_some())
            .field("run", &self.run)
            .finish()
    }
}

/// 停服任务的单次覆盖
#[derive(Clone, Copy, Debug, Default)]
pub struct StopServer {
    grace: Option<Duration>,
    force: Option<bool>,
}

impl StopServer {
    /// 优雅停机宽限时长；缺省 30 秒
    #[must_use]
    pub fn grace(mut self, grace: Duration) -> Self {
        self.grace = Some(grace);
        self
    }

    /// 宽限到期后是否强杀；缺省不强杀
    #[must_use]
    pub fn force(mut self, force: bool) -> Self {
        self.force = Some(force);
        self
    }

    fn resolve(&self) -> port::StopSpec {
        let mut spec = port::StopSpec::default_grace();
        if let Some(grace) = self.grace {
            spec.grace = grace;
        }
        if let Some(force) = self.force {
            spec.force = force;
        }
        spec
    }
}

/// 装 / 换核心的单次覆盖
///
/// 三项缺省都沿用注册表里的记录：`InstallCore::default()` 等于「按原样重装」。
#[derive(Clone, Debug, Default)]
pub struct InstallCore {
    version: Option<String>,
    variant: Option<ServerVariant>,
    java_major: Option<JavaMajor>,
    run: RunOptions,
}

impl InstallCore {
    /// 目标 MC 版本（也是来源解析的版本参数）；缺省用注册表记录
    #[must_use]
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// 换一个服务端变体（如从 vanilla 换到 paper）；缺省用注册表记录
    #[must_use]
    pub fn variant(mut self, variant: ServerVariant) -> Self {
        self.variant = Some(variant);
        self
    }

    /// 回写核心元数据时记录的 Java 大版本；缺省用注册表记录
    #[must_use]
    pub fn java_major(mut self, major: JavaMajor) -> Self {
        self.java_major = Some(major);
        self
    }

    /// 进度观察者与取消令牌
    #[must_use]
    pub fn run(mut self, run: RunOptions) -> Self {
        self.run = run;
        self
    }

    /// 与注册表记录合并（纯计算）
    fn resolve(self, server: &service::RegisteredServer) -> ResolvedCore {
        ResolvedCore {
            version: VersionId::new(
                self.version
                    .unwrap_or_else(|| server.meta.version.as_str().to_owned()),
            ),
            variant: self.variant.unwrap_or_else(|| server.meta.variant.clone()),
            run: self.run,
        }
    }
}

/// 合并后的核心安装参数
struct ResolvedCore {
    version: VersionId,
    variant: ServerVariant,
    run: RunOptions,
}

/// 删除服务器的单次覆盖
///
/// 删除只注销登记；`purge` 才删目录。运行中一律拒绝（由 `service` 判定）。
#[derive(Clone, Debug, Default)]
pub struct RemoveServer {
    purge: Option<bool>,
    cwd: Option<PathBuf>,
}

impl RemoveServer {
    /// 同时删除服务器目录；缺省只注销登记
    ///
    /// 链接服务器不能带 `purge`（目标在工作区外），由 `service` 拒绝。
    #[must_use]
    pub fn purge(mut self, purge: bool) -> Self {
        self.purge = Some(purge);
        self
    }

    /// 调用方进程的工作目录，用于「cwd 在待删目录内」的拒绝判定
    ///
    /// 缺省 `"."`，由 `service` 解析成真实工作目录；嵌在别的进程里的宿主
    /// 应当传入自己的目录。
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    fn resolve(self) -> (bool, PathBuf) {
        (
            self.purge.unwrap_or(false),
            self.cwd.unwrap_or_else(|| PathBuf::from(".")),
        )
    }
}

impl Kit<'_> {
    /// 一行建服：登记目录 → 装核心 →（可选）起服
    ///
    /// 每一步都走与 CLI 相同的 service：目录与 marker 由注册表写，核心由
    /// `ServerInstaller` 下载校验，安装器类变体按起服同一口径挑 Java，起服
    /// 判定由生命周期服务给出。kit 只决定顺序与默认值。
    ///
    /// **部分失败**：登记成功、装核心失败时服务器记录与目录会保留（与 CLI
    /// 一致），重试 `install` 即可，不需要重建；本函数不会替你删目录。
    ///
    /// # Errors
    ///
    /// 缺名称 / 变体没有来源 / 未接受 EULA 而要求起服（用法类），以及工作目录、
    /// 注册表、下载、校验、安装器、起服失败时返回 [`KitError`]
    pub async fn create_server(&self, options: CreateServer) -> Result<CreatedServer, KitError> {
        let resolved = options.resolve(self.config())?;
        let mcsup = self.core();

        // 起服请求先解析：缺 supervisor 之类的用法错误必须在建目录之前报，
        // 否则会留下「建好了、核心也装了，但起不来」的半成品
        let start_request = resolved.start.then(|| StartServer {
            accept_eula: Some(resolved.accept_eula),
            skip_mod_check: None,
            ram_mib: resolved.ram_mib,
            supervisor_exe: None,
            supervisor_args: None,
            on_first_scan: None,
            on_found: None,
            run: resolved.run.clone(),
        });
        if let Some(request) = &start_request {
            request.resolve(self.config(), mcsup.has_external_supervisor())?;
        }

        // 整合包先取回清单：MC 版本与加载器要写进注册表，且取回失败时磁盘上
        // 不应该留下任何东西。暂存目录用 guard 保证任何退出路径都被清掉。
        let mut staging = None;
        let mut prepared: Option<ModpackPrepared> = None;
        let mut variant = resolved.variant.clone();
        let mut core_version = VersionId::new(resolved.version.as_str());
        let token = resolved.run.token();
        if let Some(request) = &resolved.modpack {
            let dir = staging_dir(&resolved.name);
            std::fs::create_dir_all(&dir).map_err(|source| KitError::Io {
                path: dir.clone(),
                source,
            })?;
            staging = Some(StagingGuard(dir.clone()));
            let mut pack_options = ModpackInstallOptions::new(&dir);
            if let Some(version) = &request.version {
                pack_options = pack_options.version(version);
            }
            if let Some(source) = &request.source {
                pack_options = pack_options.source(source);
            }
            if let Some(observer) = resolved.run.observer_ref() {
                pack_options = pack_options.progress(observer);
            }
            let pack = mcsup
                .modpack
                .prepare(&request.project, pack_options, &token)
                .await?;
            variant = pack.require_loader()?;
            core_version = VersionId::new(pack.minecraft_version()?);
            prepared = Some(pack);
        }

        // 来源先查：未知变体不该留下一个装不了核心的空目录
        let source = self.core_source(&variant)?;

        let workspace = match &resolved.workspace_dir {
            Some(dir) => Some(self.ensure_workspace(dir, None)?.name.as_str().to_owned()),
            None => resolved.workspace.clone(),
        };

        let server = mcsup.registry.create(
            workspace.as_deref(),
            &resolved.name,
            &core_version,
            variant.clone(),
            resolved.java_major,
        )?;

        let core = self
            .install_core_inner(
                &resolved.name,
                source,
                &core_version,
                variant.needs_installer(),
                &resolved.run,
            )
            .await?;

        // 整合包：核心就位后铺 overrides、按清单下载文件。失败时服务器与
        // 核心都保留（重试入口是 `install_modpack` 语义的 `server install`）
        if let Some(pack) = &prepared {
            let progress = resolved
                .run
                .observer_ref()
                .map(|observer| observer as &dyn port::ProgressSink);
            mcsup
                .modpack
                .apply(pack, &server.dir, progress, &token)
                .await?;
        }
        drop(staging);

        let started = match start_request {
            Some(request) => Some(self.start_server(&resolved.name, request)?),
            None => None,
        };

        Ok(CreatedServer { core, started })
    }

    /// 一行装 / 换核心（已有服务器）
    ///
    /// 运行中换核心会被拒绝（Linux 上 rename 成功但 JVM 仍用旧 inode），判定在
    /// `service`。版本、变体与 Java 大版本缺省沿用注册表记录，因此
    /// `kit.install_core("survival", InstallCore::default())` 就是「按原样重装」。
    ///
    /// # Errors
    ///
    /// 服务器不存在、变体没有来源、运行中、下载 / 校验 / 安装器失败时返回
    /// [`KitError`]
    pub async fn install_core(
        &self,
        name: &str,
        options: InstallCore,
    ) -> Result<InstalledServerJar, KitError> {
        let server = self.core().registry.get(name)?;
        let resolved = options.resolve(&server);
        let source = self.core_source(&resolved.variant)?;
        self.install_core_inner(
            name,
            source,
            &resolved.version,
            resolved.variant.needs_installer(),
            &resolved.run,
        )
        .await
    }

    /// 查来源 + 装核心的共同尾巴
    ///
    /// 安装器类变体（Forge / `NeoForge` / Quilt）要先运行安装器：挑 Java 的口径
    /// 与起服一致，因此复用 lifecycle 的选法，不另写一套。
    async fn install_core_inner(
        &self,
        name: &str,
        source: Arc<dyn port::ServerJarSource>,
        version: &VersionId,
        needs_installer: bool,
        run: &RunOptions,
    ) -> Result<InstalledServerJar, KitError> {
        let mcsup = self.core();
        let installer_ctx = if needs_installer {
            let java_bin = mcsup
                .lifecycle
                .installer_java_bin(name, &mut || {}, &mut |_| {})?;
            Some(service::InstallerContext {
                java_bin,
                runner: Arc::new(runtime::LocalInstallerRunner::new()),
            })
        } else {
            None
        };
        let installer = service::ServerInstaller::new(source, Arc::clone(&mcsup.downloader));
        let token = run.token();
        Ok(mcsup
            .lifecycle
            .install_jar_checked(
                name,
                version,
                &installer,
                &file::FileCoreInspector::new(),
                installer_ctx.as_ref(),
                run.observer_ref(),
                &token,
            )
            .await?)
    }

    /// 按变体找核心来源；本实例没有承载该变体时给出用法错误
    fn core_source(
        &self,
        variant: &ServerVariant,
    ) -> Result<Arc<dyn port::ServerJarSource>, KitError> {
        self.core()
            .jar_sources
            .iter()
            .find(|source| source.variant() == *variant)
            .cloned()
            .ok_or_else(|| KitError::UnsupportedVariant {
                variant: variant.as_str().to_owned(),
            })
    }

    /// 一行起服
    ///
    /// # Errors
    ///
    /// 缺 supervisor 可执行文件（默认 supervisor 且未配置），或注册表 / Java /
    /// EULA / mod 预检 / supervisor 交互失败时返回 [`KitError`]
    pub fn start_server(
        &self,
        name: &str,
        options: StartServer,
    ) -> Result<SupervisorStartOutcome, KitError> {
        let start = options.resolve(self.config(), self.core().has_external_supervisor())?;
        let first = options.on_first_scan;
        let found = options.on_found;
        let mut on_first = || {
            if let Some(listener) = &first {
                listener();
            }
        };
        let mut on_found = |java: &ScannedJava| {
            if let Some(listener) = &found {
                listener(java);
            }
        };
        Ok(self
            .core()
            .lifecycle
            .start(name, &start, &mut on_first, &mut on_found)?)
    }

    /// 一行停服（幂等：本就没在跑时返回 `AlreadyStopped`）
    ///
    /// # Errors
    ///
    /// 服务器不存在、supervisor 交互失败或宽限到期仍在运行（未加 `force`）时
    /// 返回 [`KitError`]
    pub fn stop_server(&self, name: &str, options: StopServer) -> Result<StopOutcome, KitError> {
        Ok(self.core().lifecycle.stop(name, options.resolve())?)
    }

    /// 一行重启：先停（超时中止，不再拉起），再走同一条起服路径
    ///
    /// 停服失败时不继续拉起：在「不知道停没停」的状态上再起一台，只会得到
    /// 两个进程抢同一份世界文件。
    ///
    /// # Errors
    ///
    /// 停服失败 / 宽限超时，或起服失败（见 [`Self::start_server`]）时返回
    /// [`KitError`]
    pub fn restart_server(
        &self,
        name: &str,
        start: StartServer,
        stop: StopServer,
    ) -> Result<SupervisorStartOutcome, KitError> {
        self.stop_server(name, stop)?;
        self.start_server(name, start)
    }

    /// 一行删服：注销登记，（可选）删磁盘目录
    ///
    /// 运行中拒绝；`cwd` 在待删目录内也拒绝（否则删掉的正是进程脚下的目录）。
    /// 两条判定都在 `service`，kit 只合并默认值。
    ///
    /// # Errors
    ///
    /// 服务器不存在、运行中、链接服务器带 `purge`、`cwd` 在目录内、目录
    /// `mcsup.json` 不属于该记录，或删除失败时返回 [`KitError`]
    pub fn remove_server(
        &self,
        name: &str,
        options: RemoveServer,
    ) -> Result<ServerRemoveOutcome, KitError> {
        let (purge, cwd) = options.resolve();
        Ok(self.core().lifecycle.remove_server(name, purge, &cwd)?)
    }

    /// 服务器实时状态；`None` = 未在运行
    ///
    /// # Errors
    ///
    /// 服务器不存在或 supervisor 交互失败时返回 [`KitError`]
    pub fn server_status(&self, name: &str) -> Result<Option<SupervisorStatus>, KitError> {
        Ok(self.core().lifecycle.status(name)?)
    }

    /// 确保 `dir` 是一个已登记的工作目录，返回它
    ///
    /// 目录已有合法 marker 或已登记时不会重复创建（`init` 自带恢复分支），
    /// 因此可以安全地重复调用。
    ///
    /// # Errors
    ///
    /// 目录非空且未登记、名称冲突、marker / 注册表写入失败时返回 [`KitError`]
    pub fn ensure_workspace(&self, dir: &Path, name: Option<&str>) -> Result<Workspace, KitError> {
        let outcome = self.core().workspaces.init(dir, name)?;
        Ok(match outcome {
            InitOutcome::Created(workspace)
            | InitOutcome::Recovered(workspace)
            | InitOutcome::AlreadyRegistered(workspace)
            | InitOutcome::MarkerRestored(workspace) => workspace,
        })
    }
}

/// 整合包暂存目录：进程 + 服务器名隔离，避免并发建服互相覆盖
///
/// 只放取回的 `.mrpack` 与解出的临时文件；建成后由 [`StagingGuard`] 删除。
fn staging_dir(name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    std::env::temp_dir().join(format!("mcsup-kit-modpack-{}-{safe}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> KitConfig {
        KitConfig {
            variant: ServerVariant::Paper,
            default_version: "1.21.4".to_owned(),
            accept_eula: true,
            ram_mib: Some(4096),
            start_after_create: true,
            supervisor_exe: Some(PathBuf::from("/usr/bin/mcsup")),
            ..KitConfig::default()
        }
    }

    /// 未设置的字段由 `KitConfig` 补齐，设置过的优先
    #[test]
    fn create_merges_call_over_config() {
        let resolved = CreateServer::default()
            .name("survival")
            .version("1.20.1")
            .resolve(&config())
            .expect("resolve");
        assert_eq!(resolved.name, "survival");
        assert_eq!(resolved.version.as_str(), "1.20.1");
        // 变体 / 自动起服 / 堆上限都来自配置
        assert_eq!(resolved.variant, ServerVariant::Paper);
        assert!(resolved.start);
        assert_eq!(resolved.ram_mib, Some(4096));
    }

    /// `Default` 不给名字：缺必填项必须是用法错误，不能猜一个名字
    #[test]
    fn create_without_name_is_usage_error() {
        let err = CreateServer::default()
            .resolve(&KitConfig::default())
            .expect_err("缺名字");
        assert!(matches!(err, KitError::MissingName));
        assert_eq!(err.category(), port::ErrorCategory::Usage);
    }

    /// 要起服但没接受 EULA：在建目录之前就拒绝
    #[test]
    fn create_with_start_rejects_unaccepted_eula() {
        let err = CreateServer::new("survival")
            .start(true)
            .resolve(&KitConfig::default())
            .expect_err("EULA");
        assert!(matches!(err, KitError::EulaNotAccepted));
    }

    /// 没有 supervisor 可执行文件、也没注入 supervisor → 指名错误
    #[test]
    fn start_requires_supervisor_exe_without_injection() {
        let err = StartServer::default()
            .resolve(&KitConfig::default(), false)
            .expect_err("缺 exe");
        assert!(matches!(err, KitError::MissingSupervisorExe));
    }

    /// 注入过 supervisor 时不需要 exe：路径由宿主解释
    #[test]
    fn start_allows_injected_supervisor() {
        let start = StartServer::default()
            .resolve(
                &KitConfig {
                    ram_mib: Some(1024),
                    ..KitConfig::default()
                },
                true,
            )
            .expect("resolve");
        assert_eq!(start.ram_mib, Some(1024));
        assert!(start.supervisor_exe.as_os_str().is_empty());
        assert!(!start.accept_eula, "默认不代接受 EULA");
    }

    /// 单次设置覆盖配置里的同名项
    #[test]
    fn start_call_overrides_config() {
        let start = StartServer::default()
            .ram_mib(2048)
            .accept_eula(false)
            .resolve(&config(), false)
            .expect("resolve");
        assert_eq!(start.ram_mib, Some(2048));
        assert!(!start.accept_eula);
        assert_eq!(start.supervisor_exe, PathBuf::from("/usr/bin/mcsup"));
    }

    /// 内容类别默认值与配置默认值一致（防止两处各写一份）
    #[test]
    fn config_content_kind_default_is_mod() {
        assert_eq!(
            KitConfig::default().content_kind,
            domain::server::content::ContentKind::Mod
        );
    }
}
