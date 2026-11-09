//! 服务器生命周期编排：start / stop / restart / status / send / logs
//!
//! 判定在此收敛为一条路径（eula、jar、Java 选择、幂等停机），三个入口
//! （CLI / API / MCP）不再各自重写；supervisor 的权威状态判定仍在
//! `port` / `domain`，本层只做组装与前置检查。

use crate::java::JavaService;
use crate::{
    InstallJarError, ServerRegistry, ServerRegistryError,
    mod_inspect::{ModBlocked, ModBlocker, ModInspectService},
};
use domain::java::java_bin_under;
use domain::server::eula::{EULA_FILE, eula_accepted, render_eula};
use domain::server::layout::console_log_path;
use domain::server::version::VersionId;
use port::mod_::ModInspector;
use port::{
    Backend, CancellationToken, CoreInspector, EventSink, NoopEventSink, ScannedJava, StopOutcome,
    StopSpec, SupervisorError, SupervisorStartOutcome, SupervisorStartRequest, SupervisorStatus,
    TaskObserver,
};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// 默认堆上限（MiB）；`server set ram` 落地前的全局默认
pub const DEFAULT_RAM_MIB: u32 = 2048;

/// 反向 tail 的读取粒度
const TAIL_CHUNK: u64 = 4096;

/// 状态报告：注册信息 + supervisor 实时视图（`server status` 用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    /// 服务器名
    pub name: String,
    /// 服务端版本（注册元数据）
    pub version: String,
    /// 变体标识
    pub variant: String,
    /// 实时视图；`None` = 没有 supervisor（未在运行）
    pub status: Option<SupervisorStatus>,
}

/// `server start` 选项
#[derive(Debug, Clone)]
pub struct StartOptions {
    /// 同意 EULA（缺失 / `eula=false` 时写 `eula=true`）
    pub accept_eula: bool,
    /// 跳过 mod 预检
    ///
    /// 预检拦的是「加载器一定会在解析阶段拒绝」的问题（缺必需依赖 / 仅客户端
    /// / 版本不匹配）。确认要硬闯时跳过，与服务端自己决定是否启动的口径一致。
    pub skip_mod_check: bool,
    /// 堆上限；`None` 用 [`DEFAULT_RAM_MIB`]
    pub ram_mib: Option<u32>,
    /// 拉起 supervisor 的可执行文件（`mcsup` 本体）
    pub supervisor_exe: PathBuf,
    /// supervisor 可执行文件的前置子命令（如 `["__supervise"]`）
    ///
    /// 由入口填写，service 只透传给 [`SupervisorStartRequest`]，不解释其含义。
    pub supervisor_args: Vec<std::ffi::OsString>,
}

/// 生命周期操作失败
///
/// 变体若被入口直接展示，文案由入口按 `kind()` 渲染：本层的 `Display` 只用于
/// 日志与调试，不是面向用户的最终口径（见 `docs/reliability-security.md`）。
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LifecycleError {
    /// 注册表解析失败（服务器 / 工作目录不存在等）
    #[error(transparent)]
    Registry(#[from] ServerRegistryError),
    /// Java 选择（列出 / 扫描）失败
    #[error(transparent)]
    Java(#[from] crate::JavaError),
    /// supervisor 交互失败
    #[error(transparent)]
    Supervisor(#[from] SupervisorError),
    /// 未安装核心，或核心文件已不在磁盘
    #[error("server {name} has no core jar")]
    NoJar {
        /// 服务器名
        name: String,
    },
    /// EULA 未同意且未带 `--accept-eula`
    #[error("Mojang EULA is not accepted")]
    EulaRequired,
    /// 本机没有可用 Java 运行时
    #[error("no usable Java runtime found")]
    NoJava,
    /// 绑定的大版本没有任何已注册运行时
    #[error("server {name} requires Java {major}, which is not registered")]
    UnknownJavaMajor {
        /// 服务器名
        name: String,
        /// 未注册的大版本
        major: domain::java::JavaMajor,
    },
    /// 服务器已在运行
    #[error("server is already running")]
    AlreadyRunning,
    /// 服务器未在运行
    #[error("server is not running")]
    NotRunning,
    /// 有会让服务端起不来的 mod（缺必需依赖 / 仅客户端 / 与服务端不匹配）
    ///
    /// 预检在拉起 supervisor **之前**执行：这类问题加载器一定会在解析阶段
    /// 终止启动，与其让用户去服务端日志里翻英文报错，不如启动前就给出中文
    /// 结论与逐条原因。`--skip-mod-check` 可跳过。
    #[error("server {name} has mods that will stop it from starting")]
    ModsBlocked {
        /// 服务器名
        name: String,
        /// 逐条问题（文件名 + 原因）
        issues: Vec<String>,
    },
    /// 目标操作与运行状态冲突：删除 / 换核心不允许在运行中进行
    #[error("server {name} is running, refusing to {op:?}")]
    Busy {
        /// 服务器名
        name: String,
        /// 被拒绝的操作
        op: BusyOp,
    },
    /// 安装 / 更换核心失败（来源、下载、校验、世界降级）
    #[error(transparent)]
    InstallJar(#[from] InstallJarError),
    /// 宽限到期仍未退出（未加 `--force`）
    #[error("stop timed out after {waited:?}")]
    StopTimedOut {
        /// 已等待的时长
        waited: Duration,
    },
    /// 文件读写失败
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// 远端镜像条目没有增量日志流（对端只支持一次性 tail）
    #[error("server {0} is remote: log streaming is not supported")]
    RemoteLogStream(String),
    /// `status_many` 返回的条数与入参不符（实现违约，会导致状态错位）
    #[error("status_many returned {got} result(s) for {expected} server(s)")]
    StatusCountMismatch {
        /// 入参条数
        expected: usize,
        /// 实际返回条数
        got: usize,
    },
}

/// 因服务器正在运行而被拒绝的操作
///
/// 判定结果的一部分，不带文案；入口据此决定提示措辞。
/// 工作区级拦截时 `server` 是区内第一台活着的服务器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BusyOp {
    /// 删除
    Remove,
    /// 更换核心
    InstallJar,
    /// 注销整个工作区
    WorkspaceRemove,
}

impl LifecycleError {
    /// 失败等级
    ///
    /// 包装变体**委托**给内层；其余按「改参数能不能解决」判：缺资源 → 未找到，
    /// 运行中被拒 → 状态冲突，EULA 未同意 → 用法错误。
    #[must_use]
    pub fn category(&self) -> port::ErrorCategory {
        match self {
            Self::Registry(err) => err.category(),
            Self::Java(err) => err.category(),
            Self::Supervisor(err) => err.category(),
            Self::InstallJar(err) => err.category(),
            // 未装核心 / 本机没 Java / 绑定的大版本未注册：都是「要的资源不在」
            Self::NoJar { .. } | Self::NoJava | Self::UnknownJavaMajor { .. } => {
                port::ErrorCategory::NotFound
            }
            // 缺 `--accept-eula` 是调用方可直接补的参数
            Self::EulaRequired => port::ErrorCategory::Usage,
            // 已在运行 / 未在运行 / 运行中被拒：状态冲突。停机超时说明服务端
            // 仍在运行，同样是状态没到可继续的地步。mod 预检拦截同理：去掉
            // 或修好 mods/ 里的文件后状态就变了
            Self::AlreadyRunning
            | Self::NotRunning
            | Self::Busy { .. }
            | Self::StopTimedOut { .. }
            | Self::ModsBlocked { .. } => port::ErrorCategory::Conflict,
            // 远端镜像拿不到增量日志流是对端能力缺失，不是本机错误；
            // IO 与实现违约同样是后端不可用
            Self::RemoteLogStream(_) | Self::Io(_) | Self::StatusCountMismatch { .. } => {
                port::ErrorCategory::Backend
            }
        }
    }

    /// supervisor 注入失败 → 领域错误
    ///
    /// 「对端明确说没在运行」是幂等语义，归 [`Self::NotRunning`]；其余原样
    /// 包装。`backup` 的控制台注入与 `lifecycle` 的 `send_line` 共用这一份
    /// 判定，避免两处各写一个 `kind` 比较。
    #[must_use]
    pub(crate) fn from_supervisor_send(err: SupervisorError) -> Self {
        match err {
            SupervisorError::Remote {
                kind: port::RemoteFailureKind::NotRunning,
                ..
            } => Self::NotRunning,
            other => Self::Supervisor(other),
        }
    }

    /// 面向用户展示的分类；结构化错误不带文案，入口按此渲染提示
    #[must_use]
    pub fn kind(&self) -> Option<LifecycleErrorKind<'_>> {
        let kind = match self {
            Self::Registry(_)
            | Self::Java(_)
            | Self::Supervisor(_)
            | Self::InstallJar(_)
            | Self::Io(_)
            | Self::StatusCountMismatch { .. } => return None,
            Self::NoJar { name } => LifecycleErrorKind::NoJar { name },
            Self::EulaRequired => LifecycleErrorKind::EulaRequired,
            Self::NoJava => LifecycleErrorKind::NoJava,
            Self::UnknownJavaMajor { name, major } => LifecycleErrorKind::UnknownJavaMajor {
                name,
                major: *major,
            },
            Self::AlreadyRunning => LifecycleErrorKind::AlreadyRunning,
            Self::NotRunning => LifecycleErrorKind::NotRunning,
            Self::ModsBlocked { name, issues } => LifecycleErrorKind::ModsBlocked {
                name,
                issues: issues.as_slice(),
            },
            Self::Busy { name, op } => LifecycleErrorKind::Busy { name, op: *op },
            Self::StopTimedOut { waited } => LifecycleErrorKind::StopTimedOut { waited: *waited },
            Self::RemoteLogStream(name) => LifecycleErrorKind::RemoteLogStream { name },
        };
        Some(kind)
    }
}

/// [`LifecycleError::kind`] 的结果：只带判定与参数，文案由入口渲染
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum LifecycleErrorKind<'a> {
    /// 未安装核心
    NoJar { name: &'a str },
    /// EULA 未同意
    EulaRequired,
    /// 本机没有可用 Java
    NoJava,
    /// 绑定的大版本未注册
    UnknownJavaMajor {
        name: &'a str,
        major: domain::java::JavaMajor,
    },
    /// 已在运行
    AlreadyRunning,
    /// 未在运行
    NotRunning,
    /// 有会让服务端起不来的 mod
    ModsBlocked {
        /// 服务器名
        name: &'a str,
        /// 逐条问题（文件名 + 原因）
        issues: &'a [String],
    },
    /// 运行中被拒
    Busy { name: &'a str, op: BusyOp },
    /// 停机超时
    StopTimedOut { waited: Duration },
    /// 远端条目无增量日志
    RemoteLogStream { name: &'a str },
}

/// 服务器生命周期编排
///
/// 各组件均为 `Arc` / 可共享句柄，克隆即共享同一套装配（组合根把同一
/// 实例交给 CLI 命令与库侧调用方）。
#[derive(Clone)]
pub struct ServerLifecycleService {
    registry: Arc<ServerRegistry>,
    java: JavaService,
    backend: Arc<dyn Backend>,
    events: Arc<dyn EventSink>,
    /// mod 预检用的读取器；`None` = 不做预检（保持旧行为）
    ///
    /// 组合根默认给 zip 解析版。预检是可选能力的理由：宿主启动器可能完全
    /// 接管 mod 管理，让它被迫装配读取器没有意义。
    mod_inspector: Option<Arc<dyn ModInspector>>,
}

impl std::fmt::Debug for ServerLifecycleService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerLifecycleService")
            .field("registry", &"…")
            .field("java", &"…")
            .field("supervisor", &"…")
            .finish_non_exhaustive()
    }
}

impl ServerLifecycleService {
    /// 组合依赖（无事件消费者）
    #[must_use]
    pub fn new(
        registry: Arc<ServerRegistry>,
        java: JavaService,
        backend: Arc<dyn Backend>,
    ) -> Self {
        Self::with_events(registry, java, backend, Arc::new(NoopEventSink))
    }

    /// 组合依赖并注入领域事件出口
    #[must_use]
    pub fn with_events(
        registry: Arc<ServerRegistry>,
        java: JavaService,
        backend: Arc<dyn Backend>,
        events: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            registry,
            java,
            backend,
            events,
            mod_inspector: None,
        }
    }

    /// 注入 mod 预检的读取器（启动前检查 `mods/` 里会让服务端起不来的文件）
    #[must_use]
    pub fn with_mod_inspector(mut self, inspector: Arc<dyn ModInspector>) -> Self {
        self.mod_inspector = Some(inspector);
        self
    }

    /// 启动服务器：注册表 → 提前拒绝已在运行 → jar → EULA → Java 选择 → 拉起
    ///
    /// Java 首次扫描的回调与 `java list` 同一套（入口用于打印扫描进度）。
    /// 「已在运行」在 port 侧是权威判定，此处提前拒绝只为省去扫描等重活。
    ///
    /// # Errors
    ///
    /// 见 [`LifecycleError`]：已在运行 / 无核心 / EULA 未同意 / 无 Java /
    /// supervisor 交互失败
    pub fn start(
        &self,
        name: &str,
        opts: &StartOptions,
        on_first_scan: &mut dyn FnMut(),
        on_found: &mut dyn FnMut(&ScannedJava),
    ) -> Result<SupervisorStartOutcome, LifecycleError> {
        // 按**来源**取条目，不能用 `get`：后者经 `absolute_dir` 用工作区路径拼
        // 出目录，而远端工作区的 `path` 是**对端**的真实路径（如 `/root/ws`），
        // 在客户端上并不存在。据此做的「核心是否存在」检查必然失败，会把好端端
        // 的远端服务器误报成 `NoJar`。`get_sourced` 走 `list_sourced`，那里对
        // 远端条目一律把 `dir` 置空，来源也可判定。
        let sourced = self.registry.get_sourced(name)?;
        let remote = !sourced.origin.is_local();
        let server = sourced.value;
        // 远端条目只带身份：`server_ref()` 会填 `ServerRef::local(dir)`，而远端
        // 的 `dir` 是空的（来源判定见上）。让引用如实反映来源，避免下游误读空目录。
        let server_ref = if remote {
            server.remote_ref()
        } else {
            server.server_ref()
        };
        if let Some(status) = self.backend.supervisor().status(&server_ref)?
            && status.status.is_alive()
        {
            return Err(LifecycleError::AlreadyRunning);
        }

        // 启动方式：优先用安装时记录的 `LaunchSpec`；旧库记录（无 launch）
        // 回退到历史口径——`server.jar`。
        let launch = server
            .meta
            .launch
            .clone()
            .unwrap_or_else(|| domain::server::launch::LaunchSpec::jar("server.jar"));

        // 远端条目：磁盘、Java 运行时、EULA 全在对端，本地无从预检。核心是否
        // 存在、用哪个 Java、EULA 写没写，都由对端在收到 `Start` 后按同一段
        // `lifecycle.start` 重新判定——判定不复制到客户端，两端各写一套必然分叉。
        let settings = server.meta.settings.clone();
        // 优先级：显式选项 > 服务属性（server set ram）> 平台默认
        let ram_mib = opts.ram_mib.or(settings.ram_mib).unwrap_or(DEFAULT_RAM_MIB);

        let request = if remote {
            SupervisorStartRequest {
                server_id: server.meta.id.clone(),
                // 对端用自己的注册表重新算出这两个字段，这里明确留空表示「本端不知道」
                server_dir: PathBuf::new(),
                java_bin: PathBuf::new(),
                launch,
                ram_mib: Some(ram_mib),
                supervisor_exe: opts.supervisor_exe.clone(),
                supervisor_args: opts.supervisor_args.clone(),
                accept_eula: opts.accept_eula,
            }
        } else {
            // 核心文件是否存在按 spec 的主文件判定：`Jar` 是那个 JAR，
            // `Args` 是第一个参数文件（安装器总会产出它）。
            let primary = server.dir.join(launch.primary_file());
            if launch.is_empty() || !primary.is_file() {
                return Err(LifecycleError::NoJar {
                    name: name.to_owned(),
                });
            }
            ensure_eula(&server.dir, opts.accept_eula)?;
            self.check_mods(&server, opts.skip_mod_check)?;
            let runtimes = self.java.list_or_scan(on_first_scan, on_found)?;
            let runtime = pick_runtime(
                &runtimes,
                settings.java_major,
                meta_java_major(server.meta.java_major, server.meta.version.as_str()),
                name,
            )?;
            SupervisorStartRequest {
                server_id: server.meta.id.clone(),
                server_dir: server.dir.clone(),
                java_bin: java_bin_under(&runtime.path),
                launch,
                ram_mib: Some(ram_mib),
                supervisor_exe: opts.supervisor_exe.clone(),
                supervisor_args: opts.supervisor_args.clone(),
                // EULA 已在上方 ensure_eula 写入；带上只是让远端实现拿到一致意图
                accept_eula: opts.accept_eula,
            }
        };

        let outcome = self
            .backend
            .supervisor()
            .start(request)
            .map_err(|err| match err {
                SupervisorError::AlreadyRunning => LifecycleError::AlreadyRunning,
                other => LifecycleError::Supervisor(other),
            })?;
        self.events
            .publish(domain::server::event::DomainEvent::ServerStarting {
                name: name.to_owned(),
            });
        Ok(outcome)
    }

    /// 启动前的 mod 预检：找出会让加载器拒绝启动的文件
    ///
    /// 只拦「必然失败」：缺必需依赖（加载器解析阶段直接终止）、仅客户端
    /// （加载器拒绝装载）、与服务端明显不匹配（声明区间不含这台的版本）。
    /// 「待确认」（判不出客/服）不拦——那种启动也许能成，拦了就没人能启动
    /// 任何整合包了。远端服务器不做：`mods/` 在对端，本地读不到。
    ///
    /// 没装配读取器时静默跳过：预检是可选能力，不是必须项。
    fn check_mods(
        &self,
        server: &crate::registry::RegisteredServer,
        skip: bool,
    ) -> Result<(), LifecycleError> {
        if skip {
            return Ok(());
        }
        let Some(inspector) = &self.mod_inspector else {
            return Ok(());
        };
        let service = ModInspectService::local(Arc::clone(inspector));
        let target = crate::mod_::server_target(server);
        // 预检不联网：启动是本地决策，上游挂了不该拦着人开服；检查本身
        // 是同步的（`start` 会被异步调用方调，不能起运行时也不能 `block_on`）
        let blocked = service
            .blockers_in_dir(&server.dir, &target)
            .map_err(|err| LifecycleError::Io(std::io::Error::other(err.to_string())))?;
        let issues: Vec<String> = blocked.iter().map(blocking_issue).collect();
        if issues.is_empty() {
            return Ok(());
        }
        Err(LifecycleError::ModsBlocked {
            name: server.meta.name.as_str().to_owned(),
            issues,
        })
    }

    /// 停机：不在运行视为幂等成功；宽限到期映射为 [`LifecycleError::StopTimedOut`]
    ///
    /// 宽限先按 [`domain::limits::MAX_STOP_GRACE`] 夹紧：这个值可能来自远端
    /// 帧，`u64::MAX` 会让 supervisor 的等待循环永远不结束。
    ///
    /// # Errors
    ///
    /// supervisor 无响应、协议失败或宽限超时
    pub fn stop(&self, name: &str, mut spec: StopSpec) -> Result<StopOutcome, LifecycleError> {
        spec.grace = spec.grace.min(domain::limits::MAX_STOP_GRACE);
        let server = self.registry.get(name)?;
        if self
            .backend
            .supervisor()
            .status(&server.server_ref())?
            .is_none()
        {
            return Ok(StopOutcome::AlreadyStopped);
        }
        match self.backend.supervisor().stop(&server.server_ref(), spec)? {
            StopOutcome::TimedOut { waited } => Err(LifecycleError::StopTimedOut { waited }),
            other => {
                self.events
                    .publish(domain::server::event::DomainEvent::ServerStopped {
                        name: name.to_owned(),
                    });
                Ok(other)
            }
        }
    }

    /// 重启：先停（超时中止，不再拉起），再走同一条 start 路径
    ///
    /// # Errors
    ///
    /// 停机失败 / 超时，或 start 失败（见 [`Self::start`]）
    pub fn restart(
        &self,
        name: &str,
        start_opts: &StartOptions,
        stop_spec: StopSpec,
        on_first_scan: &mut dyn FnMut(),
        on_found: &mut dyn FnMut(&ScannedJava),
    ) -> Result<SupervisorStartOutcome, LifecycleError> {
        self.stop(name, stop_spec)?;
        self.start(name, start_opts, on_first_scan, on_found)
    }

    /// 删除服务器的统一入口：运行中拒绝，再转 [`ServerRegistry::remove`]
    ///
    /// 运行中删除在 Linux 上会让 JVM 拿着已删目录的句柄继续写盘（存档
    /// 「蒸发」、进程变幽灵），Windows 上因占用中途失败——平台行为分叉，
    /// 必须在 service 层统一拦下，禁止入口各自补检查。
    ///
    /// `cwd` 是调用方进程的工作目录，位于待删目录内时由
    /// [`ServerRegistry::remove`] 拒绝（同 `workspace remove --purge`）。
    ///
    /// # Errors
    ///
    /// 运行中返回 [`LifecycleError::Busy`]；其余见
    /// [`ServerRegistry::remove`]（经 [`LifecycleError::Registry`]）
    pub fn remove_server(
        &self,
        name: &str,
        purge: bool,
        cwd: &Path,
    ) -> Result<crate::registry::RemoveOutcome, LifecycleError> {
        self.ensure_stopped(name, BusyOp::Remove)?;
        Ok(self.registry.remove(name, purge, cwd)?)
    }

    /// 删盘前置闸：运行中或其余删盘条件不满足时立刻拒绝
    ///
    /// 给入口在弹「删除磁盘数据?」之前用：交互入口先问再拒，用户会在确认框
    /// 与手输名称之后才看到拒绝原因。判定与 [`Self::remove_server`] 同源
    /// （运行状态 + [`ServerRegistry::ensure_purgeable`]），不改任何状态。
    ///
    /// # Errors
    ///
    /// 运行中返回 [`LifecycleError::Busy`]；其余见
    /// [`ServerRegistry::ensure_purgeable`]
    pub fn ensure_server_purgeable(&self, name: &str, cwd: &Path) -> Result<(), LifecycleError> {
        self.ensure_stopped(name, BusyOp::Remove)?;
        Ok(self.registry.ensure_purgeable(name, cwd)?)
    }

    /// 安装 / 更换服务端核心的统一入口：运行中拒绝，再转
    /// [`ServerRegistry::install_jar`]
    ///
    /// 运行中换核心在 Linux 上 rename 成功但 JVM 继续用旧 inode（用户以为
    /// 升级了、实际没有，重启后行为不一致），Windows 上 rename 因占用直接
    /// 失败——与删除同理必须在 service 层统一拦截。
    ///
    /// # Errors
    ///
    /// 运行中返回 [`LifecycleError::Busy`]；其余见
    /// [`crate::InstallJarError`]（经 [`LifecycleError::Registry`]
    /// 的扩展转换，见 `From` 实现）
    // 参数都是编排必需的输入（目标、版本、来源、校验器、安装器上下文、观察者、
    // 取消），拆结构体只会把调用方搞得更绕，保持平铺
    #[allow(clippy::too_many_arguments)]
    pub async fn install_jar_checked(
        &self,
        name: &str,
        version: &VersionId,
        installer: &crate::ServerInstaller,
        inspector: &dyn CoreInspector,
        installer_ctx: Option<&crate::InstallerContext>,
        observer: Option<&dyn TaskObserver>,
        cancel: &CancellationToken,
    ) -> Result<crate::InstalledServerJar, LifecycleError> {
        self.ensure_stopped(name, BusyOp::InstallJar)?;
        Ok(self
            .registry
            .install_jar(
                name,
                version,
                installer,
                inspector,
                installer_ctx,
                observer,
                cancel,
            )
            .await?)
    }

    /// 为服务端安装器挑一个 `java`：口径与 [`Self::start`] 完全一致
    ///
    /// 只有安装器类来源（Forge / NeoForge）需要：它得先用一个 JVM 跑
    /// `--installServer`。挑选依据仍是「服务属性 > 版本要求 > 最新」——与
    /// 启动同一套挑选口径（`pick_runtime`），避免安装与启动用不同 Java 导致行为分叉。
    ///
    /// # Errors
    ///
    /// 服务器不存在、无可用 Java 或注册表失败时返回 [`LifecycleError`]
    pub fn installer_java_bin(
        &self,
        name: &str,
        on_first_scan: &mut dyn FnMut(),
        on_found: &mut dyn FnMut(&ScannedJava),
    ) -> Result<PathBuf, LifecycleError> {
        let server = self.registry.get(name)?;
        let settings = server.meta.settings;
        let runtimes = self.java.list_or_scan(on_first_scan, on_found)?;
        let runtime = pick_runtime(
            &runtimes,
            settings.java_major,
            meta_java_major(server.meta.java_major, server.meta.version.as_str()),
            name,
        )?;
        Ok(java_bin_under(&runtime.path))
    }

    /// 运行状态闸：任一服务器存活则拒绝（Busy），供生命周期敏感操作共用
    fn ensure_stopped(&self, name: &str, op: BusyOp) -> Result<(), LifecycleError> {
        let server = self.registry.get(name)?;
        if self
            .backend
            .supervisor()
            .status(&server.server_ref())?
            .is_some_and(|s| s.status.is_alive())
        {
            return Err(LifecycleError::Busy {
                name: name.to_owned(),
                op,
            });
        }
        Ok(())
    }

    /// 工作区级运行状态闸：区内任一服务器存活则拒绝（`workspace remove --purge` 用）
    ///
    /// 错误点名第一台活着的服务器，让用户知道停谁。
    ///
    /// # Errors
    ///
    /// 区内有服务器在运行返回 [`LifecycleError::Busy`]；注册表失败时返回
    /// [`LifecycleError::Registry`]
    pub fn ensure_workspace_stopped(
        &self,
        workspace_id: &domain::workspace::WorkspaceId,
    ) -> Result<(), LifecycleError> {
        for srv in self.registry.list()? {
            if srv.meta.workspace_id != *workspace_id {
                continue;
            }
            if self
                .backend
                .supervisor()
                .status(&srv.server_ref())?
                .is_some_and(|s| s.status.is_alive())
            {
                return Err(LifecycleError::Busy {
                    name: srv.meta.name.as_str().to_owned(),
                    op: BusyOp::WorkspaceRemove,
                });
            }
        }
        Ok(())
    }

    /// 实时状态；`Ok(None)` = 未在运行
    ///
    /// # Errors
    ///
    /// 注册表或 supervisor 协议失败
    pub fn status(&self, name: &str) -> Result<Option<SupervisorStatus>, LifecycleError> {
        let server = self.registry.get(name)?;
        Ok(self.backend.supervisor().status(&server.server_ref())?)
    }

    /// 全量服务器的实时状态（`server list` 状态列用）
    ///
    /// 一次批量探测：本地实现逐台，远端实现一次往返——列表页不该是 N+1 串行
    /// RPC。单台协议失败不拖死整个列表：失败记为该键的 `Err`，由调用方决定
    /// 渲染为 `unknown` 还是上抛。`Ok(None)` = 未在运行。按键取值，顺序不保证。
    ///
    /// # Errors
    ///
    /// 注册表失败、或 `status_many` 条数与入参不符时返回错误；
    /// supervisor 单台失败在值里
    pub fn statuses(
        &self,
    ) -> Result<HashMap<String, Result<Option<SupervisorStatus>, SupervisorError>>, LifecycleError>
    {
        let servers = self.registry.list()?;
        let refs: Vec<_> = servers.iter().map(|srv| srv.server_ref()).collect();
        let results = self.backend.supervisor().status_many(&refs);
        // 长度不符说明实现违反了 `status_many` 的契约。release 下 `zip` 会
        // 静默截断，让尾部服务器的状态凭空消失且不留日志——宁可报错。
        if results.len() != servers.len() {
            return Err(LifecycleError::StatusCountMismatch {
                expected: servers.len(),
                got: results.len(),
            });
        }
        let mut out = HashMap::with_capacity(servers.len());
        for (srv, result) in servers.iter().zip(results) {
            out.insert(srv.meta.name.as_str().to_owned(), result);
        }
        Ok(out)
    }

    /// 只探测指定名字的服务器（`server list` 用）
    ///
    /// 与 [`Self::statuses`] 的差别只在取样范围：远端条目的状态可以从客户端的
    /// 缓存里读，只有本地条目必须实时问 supervisor。用 [`Self::statuses`] 会把
    /// 每台远端也探一遍，离线远端上正好把「列表秒出」又还回去。
    ///
    /// 不在注册表里的名字被忽略（调用方给集合，不是单台，缺了不必报错）。
    ///
    /// # Errors
    ///
    /// 注册表失败、或 `status_many` 条数与入参不符时返回错误；
    /// supervisor 单台失败在值里
    pub fn statuses_for(
        &self,
        names: &std::collections::HashSet<String>,
    ) -> Result<HashMap<String, Result<Option<SupervisorStatus>, SupervisorError>>, LifecycleError>
    {
        let servers: Vec<_> = self
            .registry
            .list()?
            .into_iter()
            .filter(|srv| names.contains(srv.meta.name.as_str()))
            .collect();
        if servers.is_empty() {
            return Ok(HashMap::new());
        }
        let refs: Vec<_> = servers.iter().map(|srv| srv.server_ref()).collect();
        let results = self.backend.supervisor().status_many(&refs);
        // 长度不符说明实现违反了 `status_many` 的契约。release 下 `zip` 会
        // 静默截断，让尾部服务器的状态凭空消失且不留日志——宁可报错。
        if results.len() != servers.len() {
            return Err(LifecycleError::StatusCountMismatch {
                expected: servers.len(),
                got: results.len(),
            });
        }
        let mut out = HashMap::with_capacity(servers.len());
        for (srv, result) in servers.iter().zip(results) {
            out.insert(srv.meta.name.as_str().to_owned(), result);
        }
        Ok(out)
    }

    /// 向服务端 stdin 注入一行
    ///
    /// # Errors
    ///
    /// 未在运行返回 [`LifecycleError::NotRunning`]；其余见 [`LifecycleError`]
    pub fn send_line(&self, name: &str, line: &str) -> Result<(), LifecycleError> {
        let server = self.registry.get(name)?;
        if self
            .backend
            .supervisor()
            .status(&server.server_ref())?
            .is_none()
        {
            return Err(LifecycleError::NotRunning);
        }
        match self
            .backend
            .supervisor()
            .send_line(&server.server_ref(), line)
        {
            Err(err) => Err(LifecycleError::from_supervisor_send(err)),
            Ok(()) => Ok(()),
        }
    }

    /// 读 `console.log` 末尾若干行；文件不存在返回空
    ///
    /// 远端服务器：走 `LogsTail` RPC（对端读自己的 `console.log`）。
    ///
    /// # Errors
    ///
    /// 读取失败（非「不存在」）
    pub fn log_tail(&self, name: &str, lines: usize) -> Result<Vec<String>, LifecycleError> {
        let server = self.registry.get(name)?;
        if server.is_remote() {
            return self
                .backend
                .supervisor()
                .logs_tail(&server.remote_ref(), lines)
                .map_err(LifecycleError::from);
        }
        Ok(tail_lines(&console_log_path(&server.dir), lines)?)
    }

    /// 打开 `console.log` 的增量读取器（定位到当前末尾）
    ///
    /// 远端条目没有增量日志能力（对端只提供 `LogsTail` 一次性读取），这里
    /// **明确拒绝**而不是照本地路径打开：远端条目的 `dir` 是空的，按它拼路径
    /// 会得到一个相对 CWD 的 `console.log`，静默读错文件比报错更糟。
    ///
    /// # Errors
    ///
    /// 注册表失败、条目是远端镜像，或日志打开失败
    pub fn log_watch(&self, name: &str) -> Result<LogWatch, LifecycleError> {
        let server = self.registry.get(name)?;
        if server.is_remote() {
            return Err(LifecycleError::RemoteLogStream(name.to_owned()));
        }
        Ok(LogWatch::open(console_log_path(&server.dir)))
    }

    /// 打开 `console.log` 的增量读取器，同时返回末尾若干行历史（`server console` 附加时用）
    ///
    /// 历史与增量以同一文件长度快照为界：快照之前的内容进历史，
    /// 快照之后的追加由 [`LogWatch::read_new`] 返回，不重不漏。
    ///
    /// # Errors
    ///
    /// 注册表失败、条目是远端镜像，或日志读取失败
    pub fn log_watch_tail(
        &self,
        name: &str,
        lines: usize,
    ) -> Result<(Vec<String>, LogWatch), LifecycleError> {
        let server = self.registry.get(name)?;
        if server.is_remote() {
            return Err(LifecycleError::RemoteLogStream(name.to_owned()));
        }
        let (watch, history) = LogWatch::open_tail(console_log_path(&server.dir), lines)?;
        Ok((history, watch))
    }

    /// 注册元数据 + 实时状态的合并报告（`server status` 用）
    ///
    /// # Errors
    ///
    /// 注册表或 supervisor 协议失败
    pub fn describe(&self, name: &str) -> Result<StatusReport, LifecycleError> {
        let server = self.registry.get(name)?;
        Ok(StatusReport {
            name: server.meta.name.as_str().to_owned(),
            version: server.meta.version.to_string(),
            variant: server.meta.variant.as_str().to_owned(),
            status: self.backend.supervisor().status(&server.server_ref())?,
        })
    }
}

/// 服务器记录缺少 Java 要求时，按 MC 版本推算补上
///
/// `Fabric` / `Quilt` / `NeoForge` 的元数据不声明 Java 要求，旧记录里
/// `java_major` 可能是 `None`。不补的话会落到「本机最高 JDK」，用 25 跑
/// 要求 21 的模组会直接崩。推算口径与来源安装时写入的一致。
fn meta_java_major(
    recorded: Option<domain::java::JavaMajor>,
    mc: &str,
) -> Option<domain::java::JavaMajor> {
    recorded.or_else(|| domain::server::version::required_java_major(mc))
}

/// 一个 mod 会让启动失败的理由；没有则 `None`
/// 把结构化原因渲染成一行中文（`LifecycleError` 只带文本，不再细分类型）
fn blocking_issue(entry: &ModBlocked) -> String {
    let file = Path::new(&entry.file).file_name().map_or_else(
        || entry.file.clone(),
        |name| name.to_string_lossy().into_owned(),
    );
    match &entry.blocker {
        ModBlocker::MissingDeps { mods } => format!(
            "{file}: 缺少必需依赖 {}（可执行 mcsup server mod fix 自动补装）",
            mods.join("、")
        ),
        ModBlocker::ClientOnly => {
            format!("{file}: 清单或上游判定仅客户端，服务端无法装载（可停用该文件）")
        }
        ModBlocker::Mismatch { detail } => {
            format!("{file}: {detail}（与目标服务端的版本区间不符，可停用或更换适配版本）")
        }
    }
}

/// Java 选择：`server set java-version` 绑定大版本时取该版本的运行时，
/// 未绑定且服务器元数据记录了版本要求时按该要求取运行时，
/// 否则取已注册运行时中的最高大版本
fn pick_runtime<'a>(
    runtimes: &'a [domain::java::JavaRuntime],
    bound: Option<domain::java::JavaMajor>,
    meta_java_major: Option<domain::java::JavaMajor>,
    name: &str,
) -> Result<&'a domain::java::JavaRuntime, LifecycleError> {
    // 一个运行时都没有：报「缺 Java」比报「缺 Java 21」更准确，交互入口
    // 据此装当前 LTS
    if runtimes.is_empty() {
        return Err(LifecycleError::NoJava);
    }
    // 优先级：显式绑定 > 服务器元数据记录的 java_major > 最高已注册版本
    let target_major = bound.or(meta_java_major);
    match target_major {
        Some(major) => {
            runtimes
                .iter()
                .find(|rt| rt.major == major)
                .ok_or(LifecycleError::UnknownJavaMajor {
                    name: name.to_owned(),
                    major,
                })
        }
        // 无任何依据时不猜具体版本：用户装什么由交互入口决定
        None => runtimes
            .iter()
            .max_by_key(|rt| rt.major.as_i16())
            .ok_or(LifecycleError::NoJava),
    }
}

/// EULA 判定与写入（收敛在 service，入口只负责 `--accept-eula` 传参）
fn ensure_eula(server_dir: &Path, accept_eula: bool) -> Result<(), LifecycleError> {
    let eula_path = server_dir.join(EULA_FILE);
    let accepted = match crate::fs::read_to_string(&eula_path) {
        Ok(text) => eula_accepted(&text),
        Err(err) if err.kind() == ErrorKind::NotFound => false,
        Err(err) => return Err(LifecycleError::Io(err)),
    };
    if accepted {
        return Ok(());
    }
    if !accept_eula {
        return Err(LifecycleError::EulaRequired);
    }
    crate::fs::write(&eula_path, render_eula(true))?;
    Ok(())
}

/// 反向读取文件末尾 `max_lines` 行，并返回读取时的文件长度快照；文件不存在返回空 + 0
///
/// 从尾部按块回退收集，够 `max_lines + 1` 行（首行可能是半截）或到文件头
/// 为止。控制台日志可能非 UTF-8，按 lossy 解码。快照长度用于增量读取的
/// 起始偏移：只读 `[0, len)`，追加写入不受影响。
fn tail_lines_upto(path: &Path, max_lines: usize) -> std::io::Result<(Vec<String>, u64)> {
    let mut file = match crate::fs::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(err) => return Err(err),
    };
    let len = file.metadata()?.len();
    let mut pos = len;
    let mut buf: Vec<u8> = Vec::new();
    while pos > 0 && buf.split(|&b| b == b'\n').count().saturating_sub(1) <= max_lines {
        let take = TAIL_CHUNK.min(pos);
        pos -= take;
        file.seek(SeekFrom::Start(pos))?;
        let mut chunk = Vec::new();
        (&mut file).take(take).read_to_end(&mut chunk)?;
        chunk.append(&mut buf);
        buf = chunk;
    }

    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if pos > 0 && !lines.is_empty() {
        lines.remove(0); // 开头是被切断的半行
    }
    if lines.len() > max_lines {
        lines.drain(..lines.len() - max_lines);
    }
    let lines = lines
        .into_iter()
        .map(|line| line.trim_end_matches('\r').to_owned())
        .collect();
    Ok((lines, len))
}

/// 反向读取文件末尾 `max_lines` 行；文件不存在返回空
///
/// 见 [`tail_lines_upto`]。
fn tail_lines(path: &Path, max_lines: usize) -> std::io::Result<Vec<String>> {
    tail_lines_upto(path, max_lines).map(|(lines, _)| lines)
}

/// 增量读取日志文件（`server logs --follow` 用）：按路径 + 偏移轮询
///
/// 文件被删除重建（偏移大于文件长度）时自动回到文件头重读。
#[derive(Debug)]
pub struct LogWatch {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
}

impl LogWatch {
    /// 打开并定位到当前文件末尾：只输出此后新增的内容
    #[must_use]
    pub fn open(path: PathBuf) -> Self {
        let offset = crate::fs::metadata(&path).map_or(0, |m| m.len());
        Self {
            path,
            offset,
            partial: Vec::new(),
        }
    }

    /// 打开并返回末尾 `max_lines` 行历史，同时定位到读取时的文件长度快照
    ///
    /// 快照之前的内容进历史，之后的追加由 [`Self::read_new`] 返回，不重不漏。
    /// 文件不存在按空历史 + 从文件头跟踪处理。
    ///
    /// # Errors
    ///
    /// 读取失败（「不存在」不报错）
    pub fn open_tail(path: PathBuf, max_lines: usize) -> std::io::Result<(Self, Vec<String>)> {
        let (history, offset) = tail_lines_upto(&path, max_lines)?;
        Ok((
            Self {
                path,
                offset,
                partial: Vec::new(),
            },
            history,
        ))
    }

    /// 读取新增的完整行；尾部半行暂存到下次调用
    ///
    /// # Errors
    ///
    /// 读取失败（文件不存在按「暂无内容」处理）
    pub fn read_new(&mut self) -> Result<Vec<String>, LifecycleError> {
        let len = match crate::fs::metadata(&self.path) {
            Ok(meta) => meta.len(),
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(LifecycleError::Io(err)),
        };
        if len < self.offset {
            // 文件被重建：偏移失效，回到文件头
            self.offset = 0;
            self.partial.clear();
        }
        if len == self.offset {
            return Ok(Vec::new());
        }

        let mut file = match crate::fs::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(LifecycleError::Io(err)),
        };
        file.seek(SeekFrom::Start(self.offset))?;
        let mut chunk = Vec::new();
        file.read_to_end(&mut chunk)?;
        self.offset += chunk.len() as u64;
        self.partial.extend(chunk);

        let mut out = Vec::new();
        while let Some(pos) = self.partial.iter().position(|&b| b == b'\n') {
            let mut line = self.partial.drain(..=pos).collect::<Vec<u8>>();
            line.pop(); // 去掉 '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            out.push(String::from_utf8_lossy(&line).into_owned());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
