//! `remote serve` 的后台常驻：单实例、日志、pid 与就绪探测
//!
//! 三条约束驱动这里的每个决定：
//!
//! - **只有一个实例**：监听端口和 `grant.toml` 都是独占资源。用**内核文件锁**
//!   判定，不是「pid 文件存在即占位」——后者在持有者崩溃后要靠 pid 存活探测
//!   回收，而「探测到死亡 → 删文件 → 重建」这段窗口里会放进第二个实例。
//! - **不继承终端**：`serve start` 拉起子进程后立刻返回，终端关掉也要继续跑。
//!   复用 `runtime::process::detach`（Unix 新进程组 / Windows 分离进程）。
//! - **失败要说清**：父进程等不到就绪时，把日志尾部连同路径一起报出来，而不是
//!   只留一句「启动失败」。
//!
//! 这里不碰网络与配对码：它只回答「怎么把一个进程变成一个可管理的后台服务」。

use anyhow::{Context, Result};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 单实例锁文件名（`{data_dir}/remote/agent.lock`）
///
/// **永不删除**：删掉会让等待者在新旧 inode 上各持一把锁。内容只作排障线索。
const LOCK_FILE: &str = "agent.lock";

/// 日志文件名（`{data_dir}/remote/agent.log`）
const LOG_FILE: &str = "agent.log";

/// 日志滚动阈值：超过就改名为 `.1`（只保留一代）
///
/// 常驻进程的日志没人看管，不滚动会把磁盘写满。一代足够：出问题时用户要的是
/// 「上一次跑留下了什么」，不是完整历史。
const LOG_ROLL_BYTES: u64 = 5 * 1024 * 1024;

/// 等子进程就绪的超时
///
/// 装配 `Mcsup`（要打开/迁移元数据仓库）比单纯 bind 慢，5 秒在慢盘上不够，
/// 给到 15 秒；再久就是真出问题了。
const READY_WAIT: Duration = Duration::from_secs(15);

/// `{data_dir}/remote/agent.log`
#[must_use]
pub(crate) fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("remote").join(LOG_FILE)
}

/// 已持有的单实例锁
///
/// 句柄关闭（含进程崩溃）即释放。**必须活到进程退出**：`serve` 的入口要把它
/// 绑定到一个贯穿 `run()` 的局部变量上，中途 drop 就等于自动放弃单实例保证。
#[derive(Debug)]
pub(crate) struct InstanceLock {
    /// 只用于保活
    _file: std::fs::File,
}

impl InstanceLock {
    /// 取锁；已有实例持锁时返回 `Ok(None)`
    ///
    /// 用 `try_lock` 而不是阻塞等待：两个 `serve start` 同时跑，第二个应该
    /// 立刻告诉用户「已经在跑了」，而不是排队 30 秒再失败。
    ///
    /// # Errors
    ///
    /// 目录创建或锁文件打开失败时返回错误
    pub(crate) fn acquire(data_dir: &Path) -> Result<Option<Self>> {
        let path = data_dir.join("remote").join(LOCK_FILE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录 {}", parent.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("打开实例锁 {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {
                // 内容只是排障线索，不参与互斥判定
                let _ = file.set_len(0);
                let _ = writeln!(&file, "{}", std::process::id());
                Ok(Some(Self { _file: file }))
            }
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(err)) => {
                Err(anyhow::Error::from(err)).with_context(|| format!("锁定 {}", path.display()))
            }
        }
    }

    /// 别的实例是否正在跑（不取锁，只探测）
    ///
    /// # Errors
    ///
    /// 打不开锁文件时返回错误
    pub(crate) fn is_held(data_dir: &Path) -> Result<bool> {
        let path = data_dir.join("remote").join(LOCK_FILE);
        let file = match std::fs::OpenOptions::new().read(true).open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => {
                return Err(anyhow::Error::from(err))
                    .with_context(|| format!("打开实例锁 {}", path.display()));
            }
        };
        match file.try_lock() {
            // 能拿到 = 没人持锁；立刻释放
            Ok(()) => Ok(false),
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(std::fs::TryLockError::Error(err)) => {
                Err(anyhow::Error::from(err)).with_context(|| format!("探测 {}", path.display()))
            }
        }
    }
}

/// 滚动日志：超过阈值就把现有内容挪到 `.1`
///
/// 在**启动时**做而不是运行时轮询：常驻进程的日志量在启动后基本恒定，启动时
/// 检查一次就不会无限增长，且不必为此养一个后台线程。
///
/// # Errors
///
/// 目录创建或改名失败时返回错误
pub(crate) fn rotate_log(data_dir: &Path) -> Result<()> {
    let path = log_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录 {}", parent.display()))?;
    }
    let Ok(meta) = std::fs::metadata(&path) else {
        return Ok(());
    };
    if meta.len() < LOG_ROLL_BYTES {
        return Ok(());
    }
    let rotated = path.with_extension("log.1");
    // 旧 `.1` 直接覆盖：只保留一代
    let _ = std::fs::remove_file(&rotated);
    std::fs::rename(&path, &rotated)
        .with_context(|| format!("滚动日志 {} -> {}", path.display(), rotated.display()))
}

/// 读日志尾部（失败排查用）
///
/// 只取最后 `lines` 行：完整日志可能几 MB，终端里没有意义。
#[must_use]
pub(crate) fn log_tail(data_dir: &Path, lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(log_path(data_dir)) else {
        return String::new();
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// 后台拉起 `serve start --foreground` 并等它就绪
///
/// 返回子进程 pid。父进程**不等待**子进程退出：`start` 返回后 CLI 即将结束，
/// 子进程由 init 收养。
///
/// # Errors
///
/// 拉起失败、或 `READY_WAIT` 内没就绪时返回错误；后者附带日志尾部
pub(crate) fn spawn_background(
    exe: &Path,
    data_dir: &Path,
    listen: &str,
    name: Option<&str>,
    ready: impl Fn(&Path) -> bool,
) -> Result<u32> {
    rotate_log(data_dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(data_dir))
        .with_context(|| format!("打开日志 {}", log_path(data_dir).display()))?;

    let mut command = Command::new(exe);
    command
        .arg("remote")
        .arg("serve")
        .arg("start")
        .arg("--foreground")
        .arg("--listen")
        .arg(listen)
        .arg("--data-dir")
        .arg(data_dir);
    if let Some(name) = name {
        command.arg("--name").arg(name);
    }
    // 子进程的 stdout/stderr 全进日志：终端关掉后没人能读管道，写了会 EPIPE
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    detach(&mut command);

    let child = command
        .spawn()
        .with_context(|| format!("拉起 {}", exe.display()))?;
    let pid = child.id();
    drop(child);

    let deadline = Instant::now() + READY_WAIT;
    while Instant::now() < deadline {
        if ready(data_dir) {
            return Ok(pid);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let tail = log_tail(data_dir, 20);
    Err(anyhow::anyhow!(
        "agent 在 {READY_WAIT:?} 内没有就绪（pid {pid}）\n日志 {}\n{tail}",
        log_path(data_dir).display()
    ))
}

/// 脱离终端：Unix 新进程组，Windows 分离进程 + 新进程组
///
/// 与 `runtime::process::detach` 同一个口径。`runtime` 那个是 `pub(crate)`，
/// `mcsup` 门面没有导出，而这里只差几行 FFI 常量，为此把一个内部函数提成
/// 公开 API 不划算。
#[cfg(unix)]
pub(crate) fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

#[cfg(windows)]
pub(crate) fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn detach(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_instance_cannot_take_the_lock() {
        let dir = tempfile::tempdir().expect("tmp");
        let first = InstanceLock::acquire(dir.path()).expect("acquire");
        assert!(first.is_some(), "首次应当拿到锁");
        assert!(
            InstanceLock::acquire(dir.path())
                .expect("acquire")
                .is_none(),
            "同一进程内第二次也拿不到：内核锁由句柄持有"
        );
        assert!(InstanceLock::is_held(dir.path()).expect("probe"));
        drop(first);
        assert!(!InstanceLock::is_held(dir.path()).expect("probe"));
    }

    #[test]
    fn lock_file_is_never_removed() {
        let dir = tempfile::tempdir().expect("tmp");
        drop(InstanceLock::acquire(dir.path()).expect("acquire"));
        assert!(
            dir.path().join("remote").join(LOCK_FILE).exists(),
            "锁文件必须保留：删掉会让等待者在新旧 inode 上各持一把锁"
        );
    }

    #[test]
    fn probe_on_missing_dir_reports_free() {
        let dir = tempfile::tempdir().expect("tmp");
        assert!(!InstanceLock::is_held(dir.path()).expect("probe"));
    }

    #[test]
    fn log_rotates_only_past_threshold() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join("remote")).expect("mkdir");
        std::fs::write(log_path(dir.path()), b"small").expect("write");
        rotate_log(dir.path()).expect("rotate");
        assert!(log_path(dir.path()).exists(), "未达阈值不滚动");

        // 用写句柄灌到阈值 +1 字节：不依赖 u64 -> usize 的转换
        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(log_path(dir.path())).expect("big log");
            let chunk = [b'x'; 8192];
            let mut left = LOG_ROLL_BYTES + 1;
            while left > 0 {
                let take = usize::try_from(left.min(chunk.len() as u64)).expect("chunk");
                file.write_all(&chunk[..take]).expect("write big");
                left -= take as u64;
            }
        }
        rotate_log(dir.path()).expect("rotate");
        assert!(!log_path(dir.path()).exists(), "超过阈值应被改名");
        assert!(log_path(dir.path()).with_extension("log.1").exists());
    }

    #[test]
    fn log_tail_returns_last_lines() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join("remote")).expect("mkdir");
        std::fs::write(log_path(dir.path()), "a\nb\nc\nd\n").expect("write");
        assert_eq!(log_tail(dir.path(), 2), "c\nd");
        assert_eq!(log_tail(dir.path(), 99), "a\nb\nc\nd");
    }

    #[test]
    fn missing_log_tail_is_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("tmp");
        assert_eq!(log_tail(dir.path(), 5), "");
    }
}
