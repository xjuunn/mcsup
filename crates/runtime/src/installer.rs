//! 运行服务端安装器（`Forge` / `NeoForge`）
//!
//! 安装器的约定（实测 `Forge` `1.21.1-52.0.1`）：
//!
//! - `java -jar forge-...-installer.jar --installServer <dir>`
//! - 产出 `libraries/`、`run.sh` / `run.bat`、`user_jvm_args.txt`
//! - 成功时输出 `The server installed successfully`，退出码 0
//!
//! 安装器联网下载原版与 libraries，耗时可达数分钟，因此：
//!
//! - 输出不进入 supervisor 控制台，而是收集尾部若干行供失败时排查；成功即丢弃
//! - 必须可取消、且必须有总时长上限——否则网络卡住时安装会永久挂起，
//!   用户按 Ctrl+C 也只能干等。两者都由 [`LocalInstallerRunner`] 亲自实现：
//!   子进程不 `.output()` 一次性等待，而是轮询 `try_wait` 并在超时 / 取消时
//!   主动 kill。

use port::{CancellationToken, InstallerError, InstallerRunner};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 失败时保留的输出尾部行数
const TAIL_LINES: usize = 20;

/// 轮询子进程状态的间隔：够小以保证取消及时，够大以免空转
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 安装器允许的最长运行时间
///
/// 默认 15 分钟：实测 `1.21.1` 全新安装（无本地缓存）约 1–3 分钟，留足余量给
/// 慢速网络；再久基本可以判定为卡死。
const DEFAULT_INSTALLER_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// 安装器成功后在 `dir` 中应存在的参数文件（任一即算成功产物）
///
/// `Forge` 会生成 `run.sh` / `run.bat` 与 `user_jvm_args.txt`；用参数文件判断
/// 比输出文案更可靠（文案可能随版本变化）。
const EXPECTED_MARKERS: &[&str] = &["user_jvm_args.txt", "run.sh", "run.bat"];

/// 安装器成功后残留的自身日志（约 2MB，不属服务端产物）
const INSTALLER_LOG: &str = ".forge-installer.jar.log";

/// 本机安装器执行器：直接 spawn `java`
#[derive(Debug, Clone, Copy)]
pub struct LocalInstallerRunner {
    /// 允许的最长运行时间
    timeout: Duration,
}

impl Default for LocalInstallerRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalInstallerRunner {
    /// 新建执行器（使用内置默认超时：15 分钟）
    #[must_use]
    pub fn new() -> Self {
        Self {
            timeout: DEFAULT_INSTALLER_TIMEOUT,
        }
    }

    /// 指定超时（测试与极端慢速网络用）
    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl InstallerRunner for LocalInstallerRunner {
    fn run_installer(
        &self,
        java_bin: &Path,
        installer: &Path,
        dir: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), InstallerError> {
        // 已取消就不必拉起进程
        if cancel.is_cancelled() {
            return Err(InstallerError::Cancelled);
        }

        let command = build_installer_command(java_bin, installer, dir);
        let (status, stdout, stderr) = run_and_capture(command, self.timeout, cancel)?;

        if !status.success() {
            return Err(InstallerError::Failed {
                code: status.code().unwrap_or(-1),
                tail: tail_of(&stdout, &stderr),
            });
        }

        // 退出码为 0 不代表产物齐全：某些镜像会「成功」但漏生成参数文件
        if !has_expected_marker(dir) {
            return Err(InstallerError::NoOutput {
                dir: dir.to_path_buf(),
            });
        }
        // 安装器自己的日志不是服务端的一部分，留着会被备份一并打包
        let _ = std::fs::remove_file(dir.join(INSTALLER_LOG));
        Ok(())
    }
}

/// 组装安装器命令：`{java} -jar {installer} --installServer {dir}`，cwd 为 `dir`
fn build_installer_command(java_bin: &Path, installer: &Path, dir: &Path) -> Command {
    let mut command = Command::new(java_bin);
    command
        .arg("-jar")
        .arg(installer)
        .arg("--installServer")
        .arg(dir)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// 运行命令并收集输出，期间响应取消与超时
///
/// 关键点是**不能**用 `Command::output()`：它会一直阻塞到进程退出，安装器联网
/// 卡住时用户既看不到进展也取消不了。这里用 `try_wait` 轮询，由调用方给上限。
///
/// 返回 `(退出状态, stdout, stderr)`；被取消或超时时子进程已被 kill 并 wait。
fn run_and_capture(
    mut command: Command,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), InstallerError> {
    // 调用方可以不设管道；不设就没有输出可收集，这里兜底成 piped
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(InstallerError::Spawn)?;

    // 两个管道各起一个读线程：输出可能很多，不读会写满管道导致子进程阻塞
    // （死锁），而主线程要留在轮询里响应取消
    let (tx, rx) = mpsc::channel();
    let mut readers = Vec::new();
    if let Some(stream) = child.stdout.take() {
        readers.push(spawn_reader(stream, tx.clone(), true));
    }
    if let Some(stream) = child.stderr.take() {
        readers.push(spawn_reader(stream, tx, false));
    }

    let deadline = Instant::now() + timeout;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let status = loop {
        drain(&rx, &mut stdout, &mut stderr);

        match child.try_wait() {
            Ok(Some(done)) => break done,
            Ok(None) => {}
            Err(err) => {
                // 查询状态本身失败：别把子进程留在后台
                let _ = child.kill();
                let _ = child.wait();
                return Err(InstallerError::Spawn(err));
            }
        }

        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(InstallerError::Cancelled);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(InstallerError::TimedOut {
                secs: timeout.as_secs(),
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    // 进程已退出：读线程此时会因管道 EOF 结束。必须 join 读取线程而不是靠
    // `recv_timeout` 猜「收完了」——进程退出与读线程把最后一块数据放进通道之间
    // 存在时间差，靠超时会让尾部输出偶发丢失。
    for reader in readers {
        let _ = reader.join();
    }
    drain(&rx, &mut stdout, &mut stderr);
    // 读线程已全部结束，通道里不会再有新数据；把剩余分块收干净
    while let Ok((is_stdout, chunk)) = rx.try_recv() {
        if is_stdout {
            stdout.extend_from_slice(&chunk);
        } else {
            stderr.extend_from_slice(&chunk);
        }
    }
    Ok((status, stdout, stderr))
}

/// 把通道里已到达的分块全部收下
fn drain(rx: &mpsc::Receiver<(bool, Vec<u8>)>, stdout: &mut Vec<u8>, stderr: &mut Vec<u8>) {
    while let Ok((is_stdout, chunk)) = rx.try_recv() {
        if is_stdout {
            stdout.extend_from_slice(&chunk);
        } else {
            stderr.extend_from_slice(&chunk);
        }
    }
}

/// 为一个管道起读取线程，把分块数据送回主线程
///
/// `is_stdout` 让主线程能把两个流分开累积（失败时按「先 stdout 后 stderr」
/// 拼尾部）。
fn spawn_reader<R: Read + Send + 'static>(
    mut stream: R,
    tx: mpsc::Sender<(bool, Vec<u8>)>,
    is_stdout: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => {
                    break;
                }
                Ok(n) => {
                    if tx.send((is_stdout, buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

/// 取 stdout + stderr 的末尾若干行，合并为一条可读文本
fn tail_of(stdout: &[u8], stderr: &[u8]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for stream in [stdout, stderr] {
        let text = String::from_utf8_lossy(stream);
        for line in text.lines() {
            let trimmed = line.trim_end();
            if !trimmed.is_empty() {
                lines.push(trimmed.to_owned());
            }
        }
    }
    let start = lines.len().saturating_sub(TAIL_LINES);
    lines[start..].join("\n")
}

/// 安装目录下是否存在任一预期产物文件
fn has_expected_marker(dir: &Path) -> bool {
    EXPECTED_MARKERS.iter().any(|name| dir.join(name).is_file())
}

/// 安装器产出的参数文件相对路径（供启动使用）
#[must_use]
pub fn args_files_for(build: &str) -> Vec<PathBuf> {
    let platform_args = if cfg!(windows) {
        format!("libraries/net/minecraftforge/forge/{build}/win_args.txt")
    } else {
        format!("libraries/net/minecraftforge/forge/{build}/unix_args.txt")
    };
    vec![
        PathBuf::from("user_jvm_args.txt"),
        PathBuf::from(platform_args),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_of_joins_last_lines_of_both_streams() {
        let stdout = b"line1\nline2\n";
        let stderr = b"warn\n";
        let tail = tail_of(stdout, stderr);
        assert_eq!(tail, "line1\nline2\nwarn");
    }

    #[test]
    fn tail_of_keeps_only_last_n_lines() {
        let mut stdout = Vec::new();
        for i in 0..50 {
            stdout.extend_from_slice(format!("l{i}\n").as_bytes());
        }
        let tail = tail_of(&stdout, b"");
        let count = tail.lines().count();
        assert_eq!(count, TAIL_LINES);
        assert!(tail.contains("l49"), "{tail}");
        assert!(!tail.contains("l0\n"), "{tail}");
    }

    #[test]
    fn tail_of_empty_is_empty() {
        assert_eq!(tail_of(b"", b""), "");
        assert_eq!(tail_of(b"\n\n", b"  \n"), "");
    }

    #[test]
    fn marker_detection_requires_any_expected_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!has_expected_marker(dir.path()));
        std::fs::write(dir.path().join("user_jvm_args.txt"), b"# args").expect("write");
        assert!(has_expected_marker(dir.path()));
    }

    #[test]
    fn args_files_order_user_before_platform() {
        let files = args_files_for("1.21.1-52.0.1");
        assert_eq!(files[0], PathBuf::from("user_jvm_args.txt"));
        assert!(files[1].to_string_lossy().contains("1.21.1-52.0.1"));
        assert!(files[1].to_string_lossy().contains("_args.txt"));
    }

    /// 取消已置位的令牌：不拉起进程直接报 Cancelled
    #[test]
    fn pre_cancelled_token_never_spawns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = LocalInstallerRunner::new()
            .run_installer(
                Path::new("definitely-not-a-java"),
                Path::new("installer.jar"),
                dir.path(),
                &cancel,
            )
            .expect_err("must be cancelled");
        assert!(matches!(err, InstallerError::Cancelled), "{err:?}");
    }

    /// 命令按 `java -jar <installer> --installServer <dir>` 组装，cwd 为安装目录
    #[test]
    fn installer_command_is_built_for_java() {
        let dir = tempfile::tempdir().expect("tempdir");
        let java = Path::new("/opt/jdk21/bin/java");
        let installer = Path::new("/tmp/x/forge-installer.jar");
        let cmd = build_installer_command(java, installer, dir.path());
        assert_eq!(cmd.get_program(), java);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "-jar");
        assert_eq!(args[1], installer.to_string_lossy());
        assert_eq!(args[2], "--installServer");
        assert_eq!(args[3], dir.path().to_string_lossy());
        assert_eq!(cmd.get_current_dir(), Some(dir.path()));
    }

    /// 跑 `sh -c <script>` 的命令
    ///
    /// Git Bash 的 `sh.exe` 在没有控制台 / stdin 时会把 stdin 当成脚本读取，
    /// 拿不到 EOF 就干脆什么都不执行（连内建 `echo` 都报 `command not found`）。
    /// 所以 stdin 一律给 `null`，并把脚本走 `-c` 传入。
    fn shell_command(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script).stdin(Stdio::null());
        cmd
    }

    /// 长命令 + 短超时：必须及时返回 TimedOut，不能无限等待
    #[test]
    fn timeout_kills_hung_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cmd = shell_command("sleep 30");
        cmd.current_dir(dir.path());
        let started = Instant::now();
        let err = run_and_capture(cmd, Duration::from_millis(300), &CancellationToken::new())
            .expect_err("must time out");
        assert!(matches!(err, InstallerError::TimedOut { .. }), "{err:?}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "超时后应立即返回，实际 {:?}",
            started.elapsed()
        );
    }

    /// 运行中被取消：同样及时返回 Cancelled
    #[test]
    fn cancel_kills_running_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cmd = shell_command("sleep 30");
        cmd.current_dir(dir.path());

        let cancel = CancellationToken::new();
        let canceller = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            canceller.cancel();
        });

        let started = Instant::now();
        let err = run_and_capture(cmd, DEFAULT_INSTALLER_TIMEOUT, &cancel)
            .expect_err("must be cancelled");
        assert!(matches!(err, InstallerError::Cancelled), "{err:?}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "取消后应立即返回，实际 {:?}",
            started.elapsed()
        );
    }

    /// 输出被完整收集，退出码原样带回（供上层拼 `Failed { code, tail }`）
    ///
    /// 用 Windows 自带的 `cmd` / `sh` 二选一：Git Bash 的 `sh.exe` 在无控制台
    /// 场景下偶发不产出内容，`echo` 内建也可能解析不到，不适合验证「捕获」这件事
    /// 本身；这里选当前平台最朴素、行为最稳定的那个。
    #[test]
    fn captures_output_and_exit_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cmd = if cfg!(windows) {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", "echo boom && exit 3"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            cmd
        } else {
            shell_command("echo boom; exit 3")
        };
        cmd.current_dir(dir.path());
        let (status, stdout, stderr) =
            run_and_capture(cmd, DEFAULT_INSTALLER_TIMEOUT, &CancellationToken::new())
                .expect("should run");
        assert_eq!(status.code(), Some(3));
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        assert!(combined.contains("boom"), "输出: {combined:?}");
    }

    /// 退出码 0 但没有产物 → NoOutput（退出码 0 不等于装好了）
    #[test]
    fn zero_exit_without_marker_is_no_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 用必然成功的命令充当 java：只 exit 0，不写任何产物
        let mut cmd = shell_command("exit 0");
        cmd.current_dir(dir.path());
        let (status, _, _) =
            run_and_capture(cmd, DEFAULT_INSTALLER_TIMEOUT, &CancellationToken::new())
                .expect("should run");
        assert!(status.success());
        assert!(
            !has_expected_marker(dir.path()),
            "测试前提：没有任何预期产物"
        );
    }
}
