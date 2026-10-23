//! 服务器生命周期编排：start / stop / restart / status / send / logs
//!
//! 判定在此收敛为一条路径（eula、jar、Java 选择、幂等停机），三个入口
//! （CLI / API / MCP）不再各自重写；supervisor 的权威状态判定仍在
//! `port` / `domain`，本层只做组装与前置检查。

use crate::java::JavaService;
use crate::{ServerRegistry, ServerRegistryError};
use domain::java::java_bin_under;
use domain::server::eula::{EULA_FILE, eula_accepted, render_eula};
use domain::server::layout::console_log_path;
use port::{
    ScannedJava, StopOutcome, StopSpec, SupervisorError, SupervisorPort, SupervisorStartOutcome,
    SupervisorStartRequest, SupervisorStatus,
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
}

/// 生命周期操作失败
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
    #[error("服务器 {name} 未安装核心，可执行 mcsup server install {name}")]
    NoJar {
        /// 服务器名
        name: String,
    },
    /// EULA 未同意且未带 `--accept-eula`
    #[error("EULA 未同意（需 eula=true），可加 --accept-eula 重试")]
    EulaRequired,
    /// 本机没有可用 Java 运行时
    #[error("未发现可用 Java 运行时，可执行 mcsup java install 安装")]
    NoJava,
    /// 绑定的大版本没有任何已注册运行时
    #[error(
        "服务器 {name} 绑定的 Java 大版本 {major} 无已注册运行时，可执行 mcsup java install {major} 安装"
    )]
    UnknownJavaMajor {
        /// 服务器名
        name: String,
        /// 未注册的大版本
        major: domain::java::JavaMajor,
    },
    /// 服务器已在运行
    #[error("服务器已在运行")]
    AlreadyRunning,
    /// 服务器未在运行
    #[error("服务器未在运行")]
    NotRunning,
    /// 宽限到期仍未退出（未加 `--force`）
    #[error("停机超时（已等待 {waited:?}），可加 --force 强杀")]
    StopTimedOut {
        /// 已等待的时长
        waited: Duration,
    },
    /// 文件读写失败
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// 服务器生命周期编排
pub struct ServerLifecycleService {
    registry: Arc<ServerRegistry>,
    java: JavaService,
    supervisor: Arc<dyn SupervisorPort>,
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
    /// 组合依赖
    #[must_use]
    pub fn new(
        registry: Arc<ServerRegistry>,
        java: JavaService,
        supervisor: Arc<dyn SupervisorPort>,
    ) -> Self {
        Self {
            registry,
            java,
            supervisor,
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
        if let Some(status) = self.supervisor.status(&server.dir)?
            && status.status.is_alive()
        {
            return Err(LifecycleError::AlreadyRunning);
        }

        let jar_rel = server
            .meta
            .jar
            .as_ref()
            .map(|jar| jar.path.clone())
            .ok_or_else(|| LifecycleError::NoJar {
                name: name.to_owned(),
            })?;
        let jar_path = server.dir.join(&jar_rel);
        if !jar_path.is_file() {
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
            jar_path,
            // 优先级：显式选项 > 服务属性（server set ram）> 平台默认
            ram_mib: Some(opts.ram_mib.or(settings.ram_mib).unwrap_or(DEFAULT_RAM_MIB)),
            supervisor_exe: opts.supervisor_exe.clone(),
        };
        self.supervisor.start(request).map_err(|err| match err {
            SupervisorError::AlreadyRunning => LifecycleError::AlreadyRunning,
            other => LifecycleError::Supervisor(other),
        })
    }

    /// 停机：不在运行视为幂等成功；宽限到期映射为 [`LifecycleError::StopTimedOut`]
    ///
    /// # Errors
    ///
    /// supervisor 无响应、协议失败或宽限超时
    pub fn stop(&self, name: &str, spec: StopSpec) -> Result<StopOutcome, LifecycleError> {
        let server = self.registry.get(name)?;
        if self.supervisor.status(&server.dir)?.is_none() {
            return Ok(StopOutcome::AlreadyStopped);
        }
        match self.supervisor.stop(&server.dir, spec)? {
            StopOutcome::TimedOut { waited } => Err(LifecycleError::StopTimedOut { waited }),
            other => Ok(other),
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

    /// 实时状态；`Ok(None)` = 未在运行
    ///
    /// # Errors
    ///
    /// 注册表或 supervisor 协议失败
    pub fn status(&self, name: &str) -> Result<Option<SupervisorStatus>, LifecycleError> {
        let server = self.registry.get(name)?;
        Ok(self.supervisor.status(&server.dir)?)
    }

    /// 全量服务器的实时状态（`server list` 状态列用）
    ///
    /// 逐台探测；连不上 = `None`（未在运行）。按键取值，顺序不保证。
    ///
    /// # Errors
    ///
    /// 注册表失败，或任一 supervisor 协议失败
    pub fn statuses(&self) -> Result<HashMap<String, Option<SupervisorStatus>>, LifecycleError> {
        let servers = self.registry.list()?;
        let mut out = HashMap::with_capacity(servers.len());
        for srv in servers {
            out.insert(
                srv.meta.name.as_str().to_owned(),
                self.supervisor.status(&srv.dir)?,
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
        if self.supervisor.status(&server.dir)?.is_none() {
            return Err(LifecycleError::NotRunning);
        }
        match self.supervisor.send_line(&server.dir, line) {
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
            status: self.supervisor.status(&server.dir)?,
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
    use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
    use port::{
        ArchiveExtractor, ArtifactDownloader, CancellationToken, DownloadError, DownloadRequest,
        DownloadedArtifact, ExtractError, JavaPackageRequest, JavaRuntimeRepository, JavaScanner,
        JavaSourceRegistryPort, ProgressSink, RepositoryError, ServerRepository, SourceError,
        WorkspaceRepository,
    };
    use std::io::Write as _;
    use std::sync::Mutex;

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
            meta.settings = *settings;
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
        ServerLifecycleService::new(Arc::clone(&fixture.registry), java, supervisor)
    }

    fn opts(accept_eula: bool) -> StartOptions {
        StartOptions {
            accept_eula,
            ram_mib: None,
            supervisor_exe: PathBuf::from("/usr/local/bin/mcsup"),
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
        assert_eq!(request.jar_path, fixture.server_dir.join("server.jar"));
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
        let view = map.get("alpha").expect("alpha").as_ref().expect("view");
        assert!(view.status.is_alive());

        let idle = service(
            &fixture,
            java_service(MemJavaRepo::new(vec![], true), vec![]),
            Arc::new(FakeSupervisor::default()),
        );
        let map = idle.statuses().expect("statuses");
        assert!(map.get("alpha").expect("alpha").is_none());
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
}
