//! 服务器生命周期编排：start / stop / restart / status / send / logs
//!
//! 判定在此收敛为一条路径（eula、jar、Java 选择、幂等停机），三个入口
//! （CLI / API / MCP）不再各自重写；supervisor 的权威状态判定仍在
//! `port` / `domain`，本层只做组装与前置检查。

use crate::java::JavaService;
use crate::{InstallJarError, ServerRegistry, ServerRegistryError};
use domain::java::java_bin_under;
use domain::server::eula::{EULA_FILE, eula_accepted, render_eula};
use domain::server::layout::console_log_path;
use domain::server::version::VersionId;
use port::{
    Backend, CancellationToken, CoreInspector, EventSink, NoopEventSink, ProgressSink, ScannedJava,
    StopOutcome, StopSpec, SupervisorError, SupervisorStartOutcome, SupervisorStartRequest,
    SupervisorStatus,
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
    /// 面向用户展示的分类；结构化错误不带文案，入口按此渲染提示
    #[must_use]
    pub fn kind(&self) -> Option<LifecycleErrorKind<'_>> {
        let kind = match self {
            Self::Registry(_)
            | Self::Java(_)
            | Self::Supervisor(_)
            | Self::InstallJar(_)
            | Self::Io(_) => return None,
            Self::NoJar { name } => LifecycleErrorKind::NoJar { name },
            Self::EulaRequired => LifecycleErrorKind::EulaRequired,
            Self::NoJava => LifecycleErrorKind::NoJava,
            Self::UnknownJavaMajor { name, major } => LifecycleErrorKind::UnknownJavaMajor {
                name,
                major: *major,
            },
            Self::AlreadyRunning => LifecycleErrorKind::AlreadyRunning,
            Self::NotRunning => LifecycleErrorKind::NotRunning,
            Self::Busy { name, op } => LifecycleErrorKind::Busy { name, op: *op },
            Self::StopTimedOut { waited } => LifecycleErrorKind::StopTimedOut { waited: *waited },
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
    /// 运行中被拒
    Busy { name: &'a str, op: BusyOp },
    /// 停机超时
    StopTimedOut { waited: Duration },
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
        }
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
        let server = self.registry.get(name)?;
        if let Some(status) = self.backend.supervisor().status(&server.dir)?
            && status.status.is_alive()
        {
            return Err(LifecycleError::AlreadyRunning);
        }

        // 启动方式：优先用安装时记录的 `LaunchSpec`；旧库记录（无 launch）
        // 回退到历史口径——`server.jar`。核心文件是否存在按 spec 的主文件判定：
        // `Jar` 是那个 JAR，`Args` 是第一个参数文件（安装器总会产出它）。
        let launch = server
            .meta
            .launch
            .clone()
            .unwrap_or_else(|| domain::server::launch::LaunchSpec::jar("server.jar"));
        let primary = server.dir.join(launch.primary_file());
        if launch.is_empty() || !primary.is_file() {
            return Err(LifecycleError::NoJar {
                name: name.to_owned(),
            });
        }

        ensure_eula(&server.dir, opts.accept_eula)?;

        let settings = server.meta.settings;
        let runtimes = self.java.list_or_scan(on_first_scan, on_found)?;
        let runtime = pick_runtime(&runtimes, settings.java_major, server.meta.java_major, name)?;

        let request = SupervisorStartRequest {
            server_dir: server.dir.clone(),
            java_bin: java_bin_under(&runtime.path),
            launch,
            // 优先级：显式选项 > 服务属性（server set ram）> 平台默认
            ram_mib: Some(opts.ram_mib.or(settings.ram_mib).unwrap_or(DEFAULT_RAM_MIB)),
            supervisor_exe: opts.supervisor_exe.clone(),
            supervisor_args: opts.supervisor_args.clone(),
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

    /// 停机：不在运行视为幂等成功；宽限到期映射为 [`LifecycleError::StopTimedOut`]
    ///
    /// # Errors
    ///
    /// supervisor 无响应、协议失败或宽限超时
    pub fn stop(&self, name: &str, spec: StopSpec) -> Result<StopOutcome, LifecycleError> {
        let server = self.registry.get(name)?;
        if self.backend.supervisor().status(&server.dir)?.is_none() {
            return Ok(StopOutcome::AlreadyStopped);
        }
        match self.backend.supervisor().stop(&server.dir, spec)? {
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
    // 参数都是编排必需的输入（目标、版本、来源、校验器、安装器上下文、进度、
    // 取消），拆结构体只会把调用方搞得更绕，保持平铺
    #[allow(clippy::too_many_arguments)]
    pub async fn install_jar_checked(
        &self,
        name: &str,
        version: &VersionId,
        installer: &crate::ServerInstaller,
        inspector: &dyn CoreInspector,
        installer_ctx: Option<&crate::InstallerContext>,
        progress: Option<&dyn ProgressSink>,
        stages: Option<&dyn crate::StageSink>,
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
                progress,
                stages,
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
        let runtime = pick_runtime(&runtimes, settings.java_major, server.meta.java_major, name)?;
        Ok(java_bin_under(&runtime.path))
    }

    /// 运行状态闸：任一服务器存活则拒绝（Busy），供生命周期敏感操作共用
    fn ensure_stopped(&self, name: &str, op: BusyOp) -> Result<(), LifecycleError> {
        let server = self.registry.get(name)?;
        if self
            .backend
            .supervisor()
            .status(&server.dir)?
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
                .status(&srv.dir)?
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
        Ok(self.backend.supervisor().status(&server.dir)?)
    }

    /// 全量服务器的实时状态（`server list` 状态列用）
    ///
    /// 逐台探测，单台协议失败不拖死整个列表：失败记为该键的 `Err`，
    /// 由调用方决定渲染为 `unknown` 还是上抛。`Ok(None)` = 未在运行。
    /// 按键取值，顺序不保证。
    ///
    /// # Errors
    ///
    /// 仅注册表失败时返回错误；supervisor 单台失败在值里
    pub fn statuses(
        &self,
    ) -> Result<HashMap<String, Result<Option<SupervisorStatus>, SupervisorError>>, LifecycleError>
    {
        let servers = self.registry.list()?;
        let mut out = HashMap::with_capacity(servers.len());
        for srv in servers {
            out.insert(
                srv.meta.name.as_str().to_owned(),
                self.backend.supervisor().status(&srv.dir),
            );
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
        if self.backend.supervisor().status(&server.dir)?.is_none() {
            return Err(LifecycleError::NotRunning);
        }
        match self.backend.supervisor().send_line(&server.dir, line) {
            Err(SupervisorError::Remote { kind, .. }) if kind == "not_running" => {
                Err(LifecycleError::NotRunning)
            }
            Err(other) => Err(LifecycleError::Supervisor(other)),
            Ok(()) => Ok(()),
        }
    }

    /// 读 `console.log` 末尾若干行；文件不存在返回空
    ///
    /// # Errors
    ///
    /// 读取失败（非「不存在」）
    pub fn log_tail(&self, name: &str, lines: usize) -> Result<Vec<String>, LifecycleError> {
        let server = self.registry.get(name)?;
        Ok(tail_lines(&console_log_path(&server.dir), lines)?)
    }

    /// 打开 `console.log` 的增量读取器（定位到当前末尾）
    ///
    /// # Errors
    ///
    /// 注册表失败
    pub fn log_watch(&self, name: &str) -> Result<LogWatch, LifecycleError> {
        let server = self.registry.get(name)?;
        Ok(LogWatch::open(console_log_path(&server.dir)))
    }

    /// 打开 `console.log` 的增量读取器，同时返回末尾若干行历史（`server console` 附加时用）
    ///
    /// 历史与增量以同一文件长度快照为界：快照之前的内容进历史，
    /// 快照之后的追加由 [`LogWatch::read_new`] 返回，不重不漏。
    ///
    /// # Errors
    ///
    /// 注册表失败或日志读取失败
    pub fn log_watch_tail(
        &self,
        name: &str,
        lines: usize,
    ) -> Result<(Vec<String>, LogWatch), LifecycleError> {
        let server = self.registry.get(name)?;
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
            status: self.backend.supervisor().status(&server.dir)?,
        })
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
    // 优先级：显式绑定 > 服务器元数据记录的 java_major > 最高已注册版本
    let target_major = bound.or(meta_java_major);
    match target_major {
        Some(major) => runtimes.iter().find(|rt| rt.major == major).ok_or_else(|| {
            LifecycleError::UnknownJavaMajor {
                name: name.to_owned(),
                major,
            }
        }),
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
    let accepted = match std::fs::read_to_string(&eula_path) {
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
    std::fs::write(&eula_path, render_eula(true))?;
    Ok(())
}

/// 反向读取文件末尾 `max_lines` 行，并返回读取时的文件长度快照；文件不存在返回空 + 0
///
/// 从尾部按块回退收集，够 `max_lines + 1` 行（首行可能是半截）或到文件头
/// 为止。控制台日志可能非 UTF-8，按 lossy 解码。快照长度用于增量读取的
/// 起始偏移：只读 `[0, len)`，追加写入不受影响。
fn tail_lines_upto(path: &Path, max_lines: usize) -> std::io::Result<(Vec<String>, u64)> {
    let mut file = match std::fs::File::open(path) {
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
        let offset = std::fs::metadata(&path).map_or(0, |m| m.len());
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
        let len = match std::fs::metadata(&self.path) {
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

        let mut file = match std::fs::File::open(&self.path) {
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
mod tests {
    use super::*;
    use domain::java::{JavaMajor, JavaRuntime, JavaVendor, java_bin_under};
    use domain::server::artifact::{Checksum, ChecksumAlgorithm};
    use domain::server::meta::InstalledJar;
    use domain::server::version::{ServerVariant, VersionId};
    use domain::server::world::{DataVersion, WorldInfo};
    use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
    use port::{
        ArchiveExtractor, ArtifactDownloader, Backend, CancellationToken, CoreInspector,
        DownloadError, DownloadRequest, DownloadedArtifact, EventSink, ExtractError, InspectError,
        InstallerRunner, JavaPackageRequest, JavaRuntimeRepository, JavaScanner,
        JavaSourceRegistryPort, ProgressSink, RepositoryError, ServerJarSource, ServerRepository,
        SourceError, SupervisorPort, WorkspaceRepository,
    };
    use std::io::Write as _;
    use std::sync::Mutex;

    /// 单 JAR 启动规格（相对服务器目录）
    fn mcsup_launch_jar(path: &str) -> domain::server::launch::LaunchSpec {
        domain::server::launch::LaunchSpec::jar(path)
    }

    // ---- 注册表 / Java 测试替身 ----

    struct MemServers {
        items: Mutex<Vec<domain::server::meta::ServerMeta>>,
    }

    impl MemServers {
        fn new() -> Self {
            Self {
                items: Mutex::new(Vec::new()),
            }
        }
    }

    impl ServerRepository for MemServers {
        fn create(&self, meta: &domain::server::meta::ServerMeta) -> Result<(), RepositoryError> {
            self.items.lock().expect("lock").push(meta.clone());
            Ok(())
        }
        fn get(
            &self,
            name: &domain::server::meta::ServerName,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            self.items
                .lock()
                .expect("lock")
                .iter()
                .find(|m| m.name == *name)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
        }
        fn list(&self) -> Result<Vec<domain::server::meta::ServerMeta>, RepositoryError> {
            Ok(self.items.lock().expect("lock").clone())
        }
        fn update_core(
            &self,
            name: &domain::server::meta::ServerName,
            version: &VersionId,
            jar: &InstalledJar,
            _launch: Option<&domain::server::launch::LaunchSpec>,
            required_java_major: Option<JavaMajor>,
            updated_at: i64,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            let meta = items
                .iter_mut()
                .find(|m| m.name == *name)
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
            meta.version = version.clone();
            meta.jar = Some(jar.clone());
            meta.java_major = required_java_major;
            meta.updated_at = updated_at;
            Ok(meta.clone())
        }
        fn update_settings(
            &self,
            name: &domain::server::meta::ServerName,
            settings: &domain::server::settings::ServerSettings,
            updated_at: i64,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            let meta = items
                .iter_mut()
                .find(|m| m.name == *name)
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
            meta.settings = settings.clone();
            meta.updated_at = updated_at;
            Ok(meta.clone())
        }
        fn remove(&self, name: &domain::server::meta::ServerName) -> Result<(), RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            let before = items.len();
            items.retain(|m| m.name != *name);
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
            _name: &domain::server::meta::ServerName,
            _workspace_id: &WorkspaceId,
            _rel_dir: &Path,
            _updated_at: i64,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            unreachable!("not used in tests")
        }
    }

    struct MemWorkspaces {
        items: Mutex<Vec<Workspace>>,
    }

    impl MemWorkspaces {
        fn new(ws: Workspace) -> Self {
            Self {
                items: Mutex::new(vec![ws]),
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
                .find(|w| w.id == *id)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(id.as_str().to_owned()))
        }
        fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError> {
            self.items
                .lock()
                .expect("lock")
                .iter()
                .find(|w| w.name == *name)
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

    struct MemJavaRepo {
        runtimes: Mutex<Vec<JavaRuntime>>,
        scanned: Mutex<Option<i64>>,
    }

    impl MemJavaRepo {
        fn new(runtimes: Vec<JavaRuntime>, scanned: bool) -> Self {
            Self {
                runtimes: Mutex::new(runtimes),
                scanned: Mutex::new(if scanned { Some(1) } else { None }),
            }
        }
    }

    impl JavaRuntimeRepository for MemJavaRepo {
        fn upsert(&self, runtime: &JavaRuntime) -> Result<(), RepositoryError> {
            let mut list = self.runtimes.lock().expect("lock");
            list.retain(|r| r.path != runtime.path);
            list.push(runtime.clone());
            Ok(())
        }
        fn list(&self) -> Result<Vec<JavaRuntime>, RepositoryError> {
            Ok(self.runtimes.lock().expect("lock").clone())
        }
        fn get_by_path(&self, path: &Path) -> Result<JavaRuntime, RepositoryError> {
            self.runtimes
                .lock()
                .expect("lock")
                .iter()
                .find(|r| r.path == path)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(path.display().to_string()))
        }
        fn remove(&self, path: &Path) -> Result<(), RepositoryError> {
            let mut list = self.runtimes.lock().expect("lock");
            let before = list.len();
            list.retain(|r| r.path != path);
            if list.len() == before {
                return Err(RepositoryError::NotFound(path.display().to_string()));
            }
            Ok(())
        }
        fn scan_state(&self) -> Result<Option<i64>, RepositoryError> {
            Ok(*self.scanned.lock().expect("lock"))
        }
        fn mark_scanned(&self, at: i64) -> Result<(), RepositoryError> {
            *self.scanned.lock().expect("lock") = Some(at);
            Ok(())
        }
    }

    /// 首次扫描上报固定候选项（用于验证 start 触发扫描）
    struct FixedScanner {
        homes: Vec<PathBuf>,
    }

    impl JavaScanner for FixedScanner {
        fn scan(
            &self,
            on_found: &mut dyn FnMut(ScannedJava),
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            for home in &self.homes {
                on_found(ScannedJava {
                    path: home.clone(),
                    major: JavaMajor::parse(21).expect("21"),
                    version: "21.0.5".to_owned(),
                    vendor: JavaVendor::Temurin,
                });
            }
            Ok(())
        }
    }

    struct NoopJavaRegistry;

    #[async_trait::async_trait]
    impl JavaSourceRegistryPort for NoopJavaRegistry {
        fn source_names(&self) -> Vec<&'static str> {
            vec!["noop"]
        }
        async fn ranked(&self) -> Arc<dyn JavaSourceRegistryPort> {
            Arc::new(NoopJavaRegistry)
        }
        async fn list_packages(
            &self,
            _request: &JavaPackageRequest,
        ) -> Result<Vec<port::JavaPackage>, SourceError> {
            Ok(Vec::new())
        }
        async fn resolve_binary(
            &self,
            _package: &port::JavaPackage,
        ) -> Result<port::ResolvedJava, SourceError> {
            Err(SourceError::NotFound(VersionId::new("none")))
        }
    }

    struct NoopDownloader;

    #[async_trait::async_trait]
    impl ArtifactDownloader for NoopDownloader {
        async fn download(
            &self,
            _request: &DownloadRequest,
            _progress: Option<&dyn ProgressSink>,
            _cancel: &CancellationToken,
        ) -> Result<DownloadedArtifact, DownloadError> {
            Err(DownloadError::Cancelled)
        }
    }

    struct NoopExtractor;

    impl ArchiveExtractor for NoopExtractor {
        fn is_extractable(&self, _archive: &Path) -> bool {
            false
        }
        fn extract(&self, _archive: &Path, _dest: &Path) -> Result<(), ExtractError> {
            Ok(())
        }
    }

    /// 服务端来源替身：拒之门外即可（Busy 测试不会走到它）
    struct NoopJarSource;

    #[async_trait::async_trait]
    impl ServerJarSource for NoopJarSource {
        fn variant(&self) -> ServerVariant {
            ServerVariant::Vanilla
        }
        fn name(&self) -> &'static str {
            "noop"
        }
        async fn versions(
            &self,
        ) -> Result<Vec<domain::server::version::ServerVersion>, SourceError> {
            Ok(Vec::new())
        }
        async fn artifact(
            &self,
            _version: &VersionId,
        ) -> Result<port::ResolvedArtifact, SourceError> {
            Err(SourceError::NotFound(VersionId::new("none")))
        }
    }

    /// 核心检查替身：无世界、无数据版本
    struct NoopInspector;

    impl CoreInspector for NoopInspector {
        fn world_info(&self, _server_dir: &Path) -> Result<Option<WorldInfo>, InspectError> {
            Ok(None)
        }
        fn jar_data_version(&self, _jar: &Path) -> Result<Option<DataVersion>, InspectError> {
            Ok(None)
        }
    }

    /// 只承载 supervisor 的测试 `Backend`；installer 在生命周期用例中不用
    struct SupBackend {
        supervisor: Arc<dyn SupervisorPort>,
    }

    struct NoopInstaller;

    impl InstallerRunner for NoopInstaller {
        fn run_installer(
            &self,
            _java_bin: &Path,
            _installer: &Path,
            _dir: &Path,
            _cancel: &CancellationToken,
        ) -> Result<(), port::InstallerError> {
            unreachable!("not used")
        }
    }

    impl Backend for SupBackend {
        fn supervisor(&self) -> &dyn SupervisorPort {
            self.supervisor.as_ref()
        }
        fn installer(&self) -> &dyn InstallerRunner {
            &NoopInstaller
        }
    }

    /// 可编排的 supervisor 替身
    #[derive(Default)]
    struct FakeSupervisor {
        status: Mutex<Option<SupervisorStatus>>,
        start_result: Mutex<Option<SupervisorError>>,
        stop_result: Mutex<Option<StopOutcome>>,
        send_error: Mutex<Option<SupervisorError>>,
        start_calls: Mutex<Vec<SupervisorStartRequest>>,
        stop_calls: Mutex<usize>,
        send_calls: Mutex<Vec<String>>,
    }

    impl FakeSupervisor {
        fn with_status(status: Option<SupervisorStatus>) -> Self {
            Self {
                status: Mutex::new(status),
                ..Self::default()
            }
        }

        fn running() -> Self {
            Self::with_status(Some(SupervisorStatus {
                status: domain::server::status::ServerStatus::Running,
                pid: Some(4242),
                uptime: Some(Duration::from_secs(10)),
                exit_code: None,
            }))
        }

        fn starts(&self) -> Vec<SupervisorStartRequest> {
            self.start_calls.lock().expect("lock").clone()
        }
    }

    impl SupervisorPort for FakeSupervisor {
        fn start(
            &self,
            request: SupervisorStartRequest,
        ) -> Result<SupervisorStartOutcome, SupervisorError> {
            if let Some(err) = self.start_result.lock().expect("lock").take() {
                return Err(err);
            }
            self.start_calls.lock().expect("lock").push(request);
            *self.status.lock().expect("lock") = Some(SupervisorStatus {
                status: domain::server::status::ServerStatus::Starting,
                pid: Some(1000),
                uptime: None,
                exit_code: None,
            });
            Ok(SupervisorStartOutcome {
                supervisor_pid: 777,
            })
        }

        fn stop(
            &self,
            _server_dir: &Path,
            _spec: StopSpec,
        ) -> Result<StopOutcome, SupervisorError> {
            *self.stop_calls.lock().expect("lock") += 1;
            let outcome =
                self.stop_result
                    .lock()
                    .expect("lock")
                    .clone()
                    .unwrap_or(StopOutcome::Stopped {
                        elapsed: Duration::from_secs(1),
                    });
            if matches!(outcome, StopOutcome::Stopped { .. }) {
                *self.status.lock().expect("lock") = None;
            }
            Ok(outcome)
        }

        fn status(&self, _server_dir: &Path) -> Result<Option<SupervisorStatus>, SupervisorError> {
            Ok(self.status.lock().expect("lock").clone())
        }

        fn send_line(&self, _server_dir: &Path, line: &str) -> Result<(), SupervisorError> {
            if let Some(err) = self.send_error.lock().expect("lock").take() {
                return Err(err);
            }
            self.send_calls.lock().expect("lock").push(line.to_owned());
            Ok(())
        }

        fn shutdown(&self, _server_dir: &Path) -> Result<(), SupervisorError> {
            Ok(())
        }
    }

    // ---- 夹具 ----

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

    fn runtime(path: &Path, major: i64) -> JavaRuntime {
        JavaRuntime {
            id: format!("id-{major}"),
            path: path.to_path_buf(),
            major: JavaMajor::parse(major).expect("major"),
            version: format!("{major}.0.0"),
            vendor: JavaVendor::Temurin,
            managed: false,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    /// 建好 workspace + 服务器 + （可选）jar 与 eula 的完整夹具
    struct Fixture {
        registry: Arc<ServerRegistry>,
        server_dir: PathBuf,
        name: String,
        _tmp: tempfile::TempDir,
    }

    impl Fixture {
        fn new(with_jar: bool) -> Self {
            let tmp = tempfile::tempdir().expect("tmp");
            let ws = ws_fixture(tmp.path());
            let registry = Arc::new(ServerRegistry::new(
                Arc::new(MemServers::new()),
                Arc::new(MemWorkspaces::new(ws)),
            ));
            let server = registry
                .create(
                    None,
                    "alpha",
                    &VersionId::new("1.21.1"),
                    ServerVariant::Vanilla,
                    None,
                )
                .expect("create");
            if with_jar {
                std::fs::write(server.dir.join("server.jar"), b"jar").expect("jar");
                registry
                    .update_core(
                        "alpha",
                        &VersionId::new("1.21.1"),
                        &InstalledJar {
                            path: PathBuf::from("server.jar"),
                            size: 3,
                            checksum: Checksum::new(ChecksumAlgorithm::Sha1, "0".repeat(40)),
                        },
                        None,
                        None,
                        1_700_000_000,
                    )
                    .expect("jar");
            }
            let name = server.meta.name.as_str().to_owned();
            Self {
                registry,
                server_dir: server.dir,
                name,
                _tmp: tmp,
            }
        }
    }

    fn java_service(repo: MemJavaRepo, homes: Vec<PathBuf>) -> JavaService {
        JavaService::new(
            Arc::new(repo),
            Arc::new(FixedScanner { homes }),
            Arc::new(NoopJavaRegistry),
            Arc::new(NoopDownloader),
            Arc::new(NoopExtractor),
            PathBuf::from("/nonexistent-java-root"),
        )
    }

    fn service(
        fixture: &Fixture,
        java: JavaService,
        supervisor: Arc<FakeSupervisor>,
    ) -> ServerLifecycleService {
        ServerLifecycleService::new(
            Arc::clone(&fixture.registry),
            java,
            Arc::new(SupBackend {
                supervisor: supervisor as Arc<dyn SupervisorPort>,
            }),
        )
    }

    fn opts(accept_eula: bool) -> StartOptions {
        StartOptions {
            accept_eula,
            ram_mib: None,
            supervisor_exe: PathBuf::from("/usr/local/bin/mcsup"),
            supervisor_args: vec![std::ffi::OsString::from("__supervise")],
        }
    }

    fn start(
        service: &ServerLifecycleService,
        name: &str,
        accept: bool,
    ) -> Result<SupervisorStartOutcome, LifecycleError> {
        service.start(name, &opts(accept), &mut || {}, &mut |_| {})
    }

    // ---- start ----

    #[test]
    fn start_rejects_running_before_anything_else() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::running());
        // 未扫描的 Java 仓库：若走到扫描说明提前拒绝失效
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(Vec::new(), false), vec![]),
            Arc::clone(&supervisor),
        );
        let err = start(&svc, &fixture.name, true).expect_err("must reject");
        assert!(matches!(err, LifecycleError::AlreadyRunning), "{err:?}");
        assert!(supervisor.starts().is_empty());
    }

    #[test]
    fn start_requires_jar_before_eula() {
        let fixture = Fixture::new(false);
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::new(FakeSupervisor::default()),
        );
        let err = start(&svc, &fixture.name, true).expect_err("must fail");
        assert!(matches!(err, LifecycleError::NoJar { .. }), "{err:?}");
        assert!(!fixture.server_dir.join(EULA_FILE).exists());
    }

    #[test]
    fn start_rejects_missing_jar_file() {
        let fixture = Fixture::new(true);
        std::fs::remove_file(fixture.server_dir.join("server.jar")).expect("rm");
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::new(FakeSupervisor::default()),
        );
        let err = start(&svc, &fixture.name, true).expect_err("must fail");
        assert!(matches!(err, LifecycleError::NoJar { .. }), "{err:?}");
    }

    #[test]
    fn start_without_accept_eula_fails_and_leaves_file_untouched() {
        let fixture = Fixture::new(true);
        std::fs::write(fixture.server_dir.join(EULA_FILE), render_eula(false)).expect("eula");
        let supervisor = Arc::new(FakeSupervisor::default());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::clone(&supervisor),
        );
        let err = start(&svc, &fixture.name, false).expect_err("must fail");
        assert!(matches!(err, LifecycleError::EulaRequired), "{err:?}");
        let text = std::fs::read_to_string(fixture.server_dir.join(EULA_FILE)).expect("read");
        assert_eq!(text, render_eula(false));
        assert!(supervisor.starts().is_empty());
    }

    #[test]
    fn start_with_accept_eula_writes_true_and_picks_highest_java() {
        let fixture = Fixture::new(true);
        let runtimes = vec![
            runtime(Path::new("/opt/java17"), 17),
            runtime(Path::new("/opt/java21"), 21),
        ];
        let supervisor = Arc::new(FakeSupervisor::default());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(runtimes, true), vec![]),
            Arc::clone(&supervisor),
        );

        let outcome = start(&svc, &fixture.name, true).expect("start");
        assert_eq!(outcome.supervisor_pid, 777);

        let eula = std::fs::read_to_string(fixture.server_dir.join(EULA_FILE)).expect("eula");
        assert_eq!(eula, render_eula(true));

        let requests = supervisor.starts();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.server_dir, fixture.server_dir);
        assert_eq!(
            request.launch,
            mcsup_launch_jar("server.jar"),
            "默认启动方式为相对服务器目录的 server.jar"
        );
        assert_eq!(request.java_bin, java_bin_under(Path::new("/opt/java21")));
        assert_eq!(request.ram_mib, Some(DEFAULT_RAM_MIB));
        assert_eq!(
            request.supervisor_exe,
            PathBuf::from("/usr/local/bin/mcsup")
        );
    }

    /// `server set java-version`：绑定大版本时只在该版本内选运行时
    #[test]
    fn start_honors_bound_java_major() {
        let fixture = Fixture::new(true);
        let runtimes = vec![
            runtime(Path::new("/opt/java17"), 17),
            runtime(Path::new("/opt/java21"), 21),
        ];
        fixture
            .registry
            .update_settings(
                &fixture.name,
                &domain::server::settings::ServerSettings {
                    java_major: Some(JavaMajor::parse(17).expect("17")),
                    ram_mib: None,
                    autostart: false,
                    backup_scope: domain::backup::BackupScope::all(),
                },
            )
            .expect("settings");
        let supervisor = Arc::new(FakeSupervisor::default());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(runtimes, true), vec![]),
            Arc::clone(&supervisor),
        );

        start(&svc, &fixture.name, true).expect("start");

        let requests = supervisor.starts();
        assert_eq!(
            requests[0].java_bin,
            java_bin_under(Path::new("/opt/java17"))
        );
    }

    /// 版本自带 Java 要求（`required_java_major`）：未绑定时按版本要求选运行时，
    /// 而不是盲目取最高大版本——class file 69.0 需要新版 JDK，选低了会
    /// `UnsupportedClassVersionError`
    #[test]
    fn start_honors_version_required_java_major() {
        let fixture = Fixture::new(true);
        let runtimes = vec![
            runtime(Path::new("/opt/java17"), 17),
            runtime(Path::new("/opt/java21"), 21),
        ];
        fixture
            .registry
            .update_core(
                &fixture.name,
                &VersionId::new("1.21.1"),
                &InstalledJar {
                    path: PathBuf::from("server.jar"),
                    size: 3,
                    checksum: Checksum::new(ChecksumAlgorithm::Sha1, "0".repeat(40)),
                },
                None,
                Some(JavaMajor::parse(21).expect("21")),
                1_700_000_000,
            )
            .expect("core");
        let supervisor = Arc::new(FakeSupervisor::default());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(runtimes, true), vec![]),
            Arc::clone(&supervisor),
        );

        start(&svc, &fixture.name, true).expect("start");

        let requests = supervisor.starts();
        assert_eq!(
            requests[0].java_bin,
            java_bin_under(Path::new("/opt/java21")),
            "版本要求的 Java 大版本必须被采纳"
        );
    }

    /// 版本要求的 Java 大版本缺失运行时：报 UnknownJavaMajor，交互入口可据此
    /// 提示安装（class file 69.0 即 Java 25 编译产物，JDK 17 跑不起来）
    #[test]
    fn start_reports_unknown_when_version_required_major_missing() {
        let fixture = Fixture::new(true);
        fixture
            .registry
            .update_core(
                &fixture.name,
                &VersionId::new("26.1"),
                &InstalledJar {
                    path: PathBuf::from("server.jar"),
                    size: 3,
                    checksum: Checksum::new(ChecksumAlgorithm::Sha1, "0".repeat(40)),
                },
                None,
                Some(JavaMajor::parse(25).expect("25")),
                1_700_000_000,
            )
            .expect("core");
        let supervisor = Arc::new(FakeSupervisor::default());
        let svc = service(
            &fixture,
            java_service(
                MemJavaRepo::new(vec![runtime(Path::new("/opt/java17"), 17)], true),
                vec![],
            ),
            Arc::clone(&supervisor),
        );

        let err = start(&svc, &fixture.name, true).expect_err("must fail");
        assert!(
            matches!(err, LifecycleError::UnknownJavaMajor { major, .. } if major.as_i64() == 25),
            "{err:?}"
        );
        assert!(supervisor.starts().is_empty());
    }

    /// 绑定的大版本没有运行时：start 报 UnknownJavaMajor（而不是静默降级）
    #[test]
    fn start_reports_unknown_java_major_when_bound_runtime_missing() {
        let fixture = Fixture::new(true);
        fixture
            .registry
            .update_settings(
                &fixture.name,
                &domain::server::settings::ServerSettings {
                    java_major: Some(JavaMajor::parse(17).expect("17")),
                    ram_mib: None,
                    autostart: false,
                    backup_scope: domain::backup::BackupScope::all(),
                },
            )
            .expect("settings");
        let supervisor = Arc::new(FakeSupervisor::default());
        let svc = service(
            &fixture,
            java_service(
                MemJavaRepo::new(vec![runtime(Path::new("/opt/java21"), 21)], true),
                vec![],
            ),
            Arc::clone(&supervisor),
        );

        let err = start(&svc, &fixture.name, true).expect_err("must fail");
        assert!(
            matches!(err, LifecycleError::UnknownJavaMajor { .. }),
            "{err:?}"
        );
        assert!(supervisor.starts().is_empty());
    }

    /// `server set ram`：服务属性定堆上限；显式选项优先于属性
    #[test]
    fn start_honors_ram_setting_and_opts_override_it() {
        let fixture = Fixture::new(true);
        fixture
            .registry
            .update_settings(
                &fixture.name,
                &domain::server::settings::ServerSettings {
                    java_major: None,
                    ram_mib: Some(4096),
                    autostart: false,
                    backup_scope: domain::backup::BackupScope::all(),
                },
            )
            .expect("settings");
        let supervisor = Arc::new(FakeSupervisor::default());
        let svc = service(
            &fixture,
            java_service(
                MemJavaRepo::new(vec![runtime(Path::new("/opt/java21"), 21)], true),
                vec![],
            ),
            Arc::clone(&supervisor),
        );

        start(&svc, &fixture.name, true).expect("start");
        assert_eq!(supervisor.starts()[0].ram_mib, Some(4096));

        // 显式 StartOptions 覆盖属性（首个 start 已把假监督器置为运行中，
        // 换一套监督器验证覆盖关系）
        let explicit = StartOptions {
            accept_eula: true,
            ram_mib: Some(512),
            supervisor_exe: PathBuf::from("/usr/local/bin/mcsup"),
            supervisor_args: vec![std::ffi::OsString::from("__supervise")],
        };
        let explicit_supervisor = Arc::new(FakeSupervisor::default());
        let explicit_svc = service(
            &fixture,
            java_service(
                MemJavaRepo::new(vec![runtime(Path::new("/opt/java21"), 21)], true),
                vec![],
            ),
            Arc::clone(&explicit_supervisor),
        );
        explicit_svc
            .start(&fixture.name, &explicit, &mut || {}, &mut |_| {})
            .expect("start");
        assert_eq!(explicit_supervisor.starts()[0].ram_mib, Some(512));
    }

    #[test]
    fn start_without_java_reports_no_java() {
        let fixture = Fixture::new(true);
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::new(FakeSupervisor::default()),
        );
        let err = start(&svc, &fixture.name, true).expect_err("must fail");
        assert!(matches!(err, LifecycleError::NoJava), "{err:?}");
    }

    #[test]
    fn start_scans_when_never_scanned_and_uses_found_runtime() {
        let fixture = Fixture::new(true);
        let home = fixture.server_dir.join("fake-jvm");
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], false), vec![home.clone()]),
            Arc::new(FakeSupervisor::default()),
        );
        let mut first_scan = false;
        let outcome = svc
            .start(
                &fixture.name,
                &opts(true),
                &mut || first_scan = true,
                &mut |_| {},
            )
            .expect("start");
        assert_eq!(outcome.supervisor_pid, 777);
        assert!(first_scan, "first scan must be announced");
    }

    #[test]
    fn start_maps_port_already_running() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::default());
        *supervisor.start_result.lock().expect("lock") = Some(SupervisorError::AlreadyRunning);
        let svc = service(
            &fixture,
            java_service(
                MemJavaRepo::new(vec![runtime(Path::new("/opt/java21"), 21)], true),
                vec![],
            ),
            Arc::clone(&supervisor),
        );
        let err = start(&svc, &fixture.name, true).expect_err("must fail");
        assert!(matches!(err, LifecycleError::AlreadyRunning), "{err:?}");
    }

    // ---- stop / restart ----

    #[test]
    fn stop_without_supervisor_is_idempotent() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::default());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::clone(&supervisor),
        );
        let outcome = svc
            .stop(&fixture.name, StopSpec::default_grace())
            .expect("stop");
        assert_eq!(outcome, StopOutcome::AlreadyStopped);
        assert_eq!(*supervisor.stop_calls.lock().expect("lock"), 0);
    }

    #[test]
    fn stop_success_passes_outcome_through() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::with_status(Some(SupervisorStatus {
            status: domain::server::status::ServerStatus::Running,
            pid: Some(4242),
            uptime: Some(Duration::from_secs(10)),
            exit_code: None,
        })));
        *supervisor.stop_result.lock().expect("lock") = Some(StopOutcome::Stopped {
            elapsed: Duration::from_secs(3),
        });
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::clone(&supervisor),
        );
        let outcome = svc
            .stop(&fixture.name, StopSpec::default_grace())
            .expect("stop");
        assert_eq!(
            outcome,
            StopOutcome::Stopped {
                elapsed: Duration::from_secs(3)
            }
        );
        assert_eq!(*supervisor.stop_calls.lock().expect("lock"), 1);
    }

    #[test]
    fn stop_timeout_maps_to_error() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::with_status(Some(SupervisorStatus {
            status: domain::server::status::ServerStatus::Running,
            pid: Some(4242),
            uptime: Some(Duration::from_secs(10)),
            exit_code: None,
        })));
        *supervisor.stop_result.lock().expect("lock") = Some(StopOutcome::TimedOut {
            waited: Duration::from_secs(30),
        });
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::clone(&supervisor),
        );
        let err = svc
            .stop(&fixture.name, StopSpec::default_grace())
            .expect_err("must fail");
        assert!(
            matches!(err, LifecycleError::StopTimedOut { waited } if waited == Duration::from_secs(30)),
            "{err:?}"
        );
    }

    #[test]
    fn restart_stops_then_starts() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::running());
        let svc = service(
            &fixture,
            java_service(
                MemJavaRepo::new(vec![runtime(Path::new("/opt/java21"), 21)], true),
                vec![],
            ),
            Arc::clone(&supervisor),
        );
        let outcome = svc
            .restart(
                &fixture.name,
                &opts(true),
                StopSpec::default_grace(),
                &mut || {},
                &mut |_| {},
            )
            .expect("restart");
        assert_eq!(outcome.supervisor_pid, 777);
        assert_eq!(*supervisor.stop_calls.lock().expect("lock"), 1);
        assert_eq!(supervisor.starts().len(), 1);
    }

    #[test]
    fn restart_aborts_on_stop_timeout_without_start() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::running());
        *supervisor.stop_result.lock().expect("lock") = Some(StopOutcome::TimedOut {
            waited: Duration::from_secs(30),
        });
        let svc = service(
            &fixture,
            java_service(
                MemJavaRepo::new(vec![runtime(Path::new("/opt/java21"), 21)], true),
                vec![],
            ),
            Arc::clone(&supervisor),
        );
        let err = svc
            .restart(
                &fixture.name,
                &opts(true),
                StopSpec::default_grace(),
                &mut || {},
                &mut |_| {},
            )
            .expect_err("must fail");
        assert!(
            matches!(err, LifecycleError::StopTimedOut { .. }),
            "{err:?}"
        );
        assert!(supervisor.starts().is_empty());
    }

    // ---- 生命周期敏感编排：remove / install ----

    /// 运行中删除必须被拒（Linux 上会蒸发存档），判定收敛在 service
    #[test]
    fn remove_server_rejects_running() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::running());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(Vec::new(), false), vec![]),
            Arc::clone(&supervisor),
        );
        let err = svc
            .remove_server("alpha", false, &fixture.server_dir.join(".."))
            .expect_err("must fail");
        assert!(
            matches!(
                err,
                LifecycleError::Busy {
                    op: BusyOp::Remove,
                    ..
                }
            ),
            "{err:?}"
        );
        // 目录与登记都保留
        assert!(fixture.server_dir.exists());
    }

    /// 未运行时删除放行：注销登记（默认不删盘）
    #[test]
    fn remove_server_allows_when_stopped() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::default());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(Vec::new(), false), vec![]),
            Arc::clone(&supervisor),
        );
        let removed = svc
            .remove_server("alpha", false, &fixture.server_dir.join(".."))
            .expect("remove");
        assert_eq!(removed.server.meta.name.as_str(), "alpha");
        assert!(fixture.registry.get("alpha").is_err());
    }

    /// 运行中换核心必须被拒（Linux 上 rename 成功但 JVM 用旧 inode）
    #[tokio::test]
    async fn install_jar_checked_rejects_running() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::running());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(Vec::new(), false), vec![]),
            Arc::clone(&supervisor),
        );
        let installer =
            crate::ServerInstaller::new(Arc::new(NoopJarSource), Arc::new(NoopDownloader));
        let err = svc
            .install_jar_checked(
                "alpha",
                &VersionId::new("1.21.2"),
                &installer,
                &NoopInspector,
                None,
                None,
                None,
                &CancellationToken::new(),
            )
            .await
            .expect_err("must fail");
        assert!(
            matches!(
                err,
                LifecycleError::Busy {
                    op: BusyOp::InstallJar,
                    ..
                }
            ),
            "{err:?}"
        );
        // 旧核心原样保留
        assert!(fixture.server_dir.join("server.jar").is_file());
    }

    // ---- status / send / logs ----

    #[test]
    fn status_and_send_map_not_running() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::default());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::clone(&supervisor),
        );
        assert!(svc.status(&fixture.name).expect("status").is_none());
        let err = svc.send_line(&fixture.name, "list").expect_err("must fail");
        assert!(matches!(err, LifecycleError::NotRunning), "{err:?}");
    }

    #[test]
    fn send_maps_remote_not_running_error() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::running());
        *supervisor.send_error.lock().expect("lock") = Some(SupervisorError::Remote {
            kind: "not_running".to_owned(),
            message: "not running".to_owned(),
        });
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::clone(&supervisor),
        );
        let err = svc.send_line(&fixture.name, "list").expect_err("must fail");
        assert!(matches!(err, LifecycleError::NotRunning), "{err:?}");
    }

    #[test]
    fn send_reaches_supervisor() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::running());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::clone(&supervisor),
        );
        svc.send_line(&fixture.name, "list").expect("send");
        assert_eq!(
            supervisor.send_calls.lock().expect("lock").clone(),
            vec!["list".to_owned()]
        );
    }

    #[test]
    fn describe_merges_meta_and_supervisor_view() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::running());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::clone(&supervisor),
        );
        let report = svc.describe(&fixture.name).expect("describe");
        assert_eq!(report.name, "alpha");
        assert_eq!(report.version, "1.21.1");
        assert_eq!(report.variant, "vanilla");
        let status = report.status.expect("running");
        assert_eq!(status.pid, Some(4242));

        // 无 supervisor：视图为 None，注册信息仍在
        let idle = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::new(FakeSupervisor::default()),
        );
        let report = idle.describe(&fixture.name).expect("describe");
        assert!(report.status.is_none());
        assert_eq!(report.version, "1.21.1");
    }

    #[test]
    fn statuses_probes_all_registered_servers() {
        let fixture = Fixture::new(true);
        let running = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::new(FakeSupervisor::running()),
        );
        let map = running.statuses().expect("statuses");
        assert_eq!(map.len(), 1);
        let probe = map.get("alpha").expect("alpha");
        let view = probe.as_ref().expect("probe ok").as_ref().expect("view");
        assert!(view.status.is_alive());

        let idle = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::new(FakeSupervisor::default()),
        );
        let map = idle.statuses().expect("statuses");
        assert!(
            map.get("alpha")
                .expect("alpha")
                .as_ref()
                .expect("probe ok")
                .is_none()
        );
    }

    /// 单台探测失败记 Err，不拖死整个列表
    #[test]
    fn statuses_degrade_single_probe_failure() {
        let fixture = Fixture::new(true);
        let svc = ServerLifecycleService::new(
            Arc::clone(&fixture.registry),
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::new(SupBackend {
                supervisor: Arc::new(ProbeFailSupervisor),
            }),
        );
        let map = svc.statuses().expect("statuses");
        assert_eq!(map.len(), 1);
        assert!(map.get("alpha").expect("alpha").is_err());
    }

    /// 状态探测恒失败的替身
    struct ProbeFailSupervisor;

    impl SupervisorPort for ProbeFailSupervisor {
        fn start(
            &self,
            _request: SupervisorStartRequest,
        ) -> Result<SupervisorStartOutcome, SupervisorError> {
            Err(SupervisorError::Unreachable("probe fail".to_owned()))
        }
        fn stop(
            &self,
            _server_dir: &Path,
            _spec: StopSpec,
        ) -> Result<StopOutcome, SupervisorError> {
            Err(SupervisorError::Unreachable("probe fail".to_owned()))
        }
        fn status(&self, _server_dir: &Path) -> Result<Option<SupervisorStatus>, SupervisorError> {
            Err(SupervisorError::Protocol("version mismatch".to_owned()))
        }
        fn send_line(&self, _server_dir: &Path, _line: &str) -> Result<(), SupervisorError> {
            Err(SupervisorError::Unreachable("probe fail".to_owned()))
        }
        fn shutdown(&self, _server_dir: &Path) -> Result<(), SupervisorError> {
            Ok(())
        }
    }

    #[test]
    fn log_watch_opens_at_tail_of_existing_file() {
        let fixture = Fixture::new(true);
        let log = console_log_path(&fixture.server_dir);
        std::fs::write(&log, "old\n").expect("write");
        let supervisor = Arc::new(FakeSupervisor::default());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::clone(&supervisor),
        );
        let mut watch = svc.log_watch(&fixture.name).expect("watch");
        assert!(watch.read_new().expect("at tail").is_empty());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .expect("open");
        writeln!(file, "new").expect("append");
        assert_eq!(watch.read_new().expect("new"), vec!["new".to_owned()]);
    }

    #[test]
    fn log_tail_returns_last_lines_and_empty_when_missing() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::default());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::clone(&supervisor),
        );
        assert!(svc.log_tail(&fixture.name, 10).expect("missing").is_empty());

        let log = console_log_path(&fixture.server_dir);
        std::fs::write(&log, "l1\nl2\r\nl3\n").expect("write");
        assert_eq!(
            svc.log_tail(&fixture.name, 2).expect("tail"),
            vec!["l2".to_owned(), "l3".to_owned()]
        );
        assert_eq!(
            svc.log_tail(&fixture.name, 99).expect("tail"),
            vec!["l1".to_owned(), "l2".to_owned(), "l3".to_owned()]
        );
    }

    #[test]
    fn log_watch_reads_new_lines_and_holds_partial() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("console.log");
        std::fs::write(&path, "before\n").expect("write");
        let mut watch = LogWatch::open(path.clone());
        assert!(watch.read_new().expect("empty").is_empty());

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        writeln!(file, "after").expect("append");
        assert_eq!(watch.read_new().expect("new"), vec!["after".to_owned()]);

        write!(file, "half").expect("append");
        assert!(watch.read_new().expect("partial").is_empty());
        writeln!(file, "-line").expect("append");
        assert_eq!(
            watch.read_new().expect("completed"),
            vec!["half-line".to_owned()]
        );
    }

    #[test]
    fn log_watch_tail_replays_history_then_only_new_lines() {
        let fixture = Fixture::new(true);
        let log = console_log_path(&fixture.server_dir);
        std::fs::write(&log, "l1\nl2\nl3\nl4\n").expect("write");
        let supervisor = Arc::new(FakeSupervisor::default());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::clone(&supervisor),
        );
        let (history, mut watch) = svc.log_watch_tail(&fixture.name, 2).expect("watch tail");
        assert_eq!(history, vec!["l3".to_owned(), "l4".to_owned()]);
        // 历史与增量不重：快照之后才追加
        assert!(watch.read_new().expect("no dup").is_empty());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .expect("open");
        writeln!(file, "l5").expect("append");
        assert_eq!(watch.read_new().expect("new"), vec!["l5".to_owned()]);
    }

    #[test]
    fn log_watch_tail_missing_file_starts_empty_then_reads_from_start() {
        let fixture = Fixture::new(true);
        let supervisor = Arc::new(FakeSupervisor::default());
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::clone(&supervisor),
        );
        let (history, mut watch) = svc.log_watch_tail(&fixture.name, 30).expect("watch tail");
        assert!(history.is_empty());
        // 文件此时尚未创建：快照长度为 0，创建后从头跟踪
        let log = console_log_path(&fixture.server_dir);
        std::fs::write(&log, "boot\nready\n").expect("write");
        assert_eq!(
            watch.read_new().expect("from start"),
            vec!["boot".to_owned(), "ready".to_owned()]
        );
    }

    #[test]
    fn log_watch_open_tail_caps_history_at_max_lines() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("console.log");
        let content = (1..=100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, format!("{content}\n")).expect("write");
        let (mut watch, history) = LogWatch::open_tail(path.clone(), 5).expect("open tail");
        assert_eq!(
            history,
            vec!["line96", "line97", "line98", "line99", "line100"]
        );
        assert!(watch.read_new().expect("no dup").is_empty());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        writeln!(file, "line101").expect("append");
        assert_eq!(watch.read_new().expect("new"), vec!["line101".to_owned()]);
    }

    #[test]
    fn log_watch_rereads_after_recreate() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("console.log");
        std::fs::write(&path, "x".repeat(100)).expect("write");
        let mut watch = LogWatch::open(path.clone());
        assert!(watch.read_new().expect("empty").is_empty());

        // 重建更短的文件：偏移失效，回到文件头重读
        std::fs::write(&path, "fresh\n").expect("rewrite");
        assert_eq!(watch.read_new().expect("reread"), vec!["fresh".to_owned()]);
    }

    #[test]
    fn eula_error_surfaces_as_io_not_eula_required() {
        let fixture = Fixture::new(true);
        // eula.txt 是目录：读失败（非 NotFound）
        std::fs::create_dir(fixture.server_dir.join(EULA_FILE)).expect("dir");
        let svc = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::new(FakeSupervisor::default()),
        );
        let err = start(&svc, &fixture.name, true).expect_err("must fail");
        assert!(matches!(err, LifecycleError::Io(_)), "{err:?}");
    }

    /// 捕获型事件出口：验证 service 在成功路径上确实发布了事实
    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<domain::server::event::DomainEvent>>,
    }

    impl EventSink for RecordingSink {
        fn publish(&self, event: domain::server::event::DomainEvent) {
            self.events.lock().expect("lock").push(event);
        }
    }

    #[test]
    fn stop_publishes_server_stopped_event() {
        let fixture = Fixture::new(true);
        let sink = Arc::new(RecordingSink::default());
        let svc = ServerLifecycleService::with_events(
            Arc::clone(&fixture.registry),
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::new(SupBackend {
                supervisor: Arc::new(FakeSupervisor::running()),
            }),
            sink.clone(),
        );
        svc.stop(&fixture.name, StopSpec::default_grace())
            .expect("stop");
        let events = sink.events.lock().expect("lock").clone();
        assert!(
            events.iter().any(|event| matches!(
                event,
                domain::server::event::DomainEvent::ServerStopped { name } if name == &fixture.name
            )),
            "{events:?}"
        );
    }
}
