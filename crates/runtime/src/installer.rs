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

use port::{CancellationToken, InstallerError, InstallerOutputSink, InstallerRunner};
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

/// 安装器成功后应产出的文件（任一即算「装过」）
///
/// 只看 `user_jvm_args.txt` / `run.sh` / `run.bat` 不够：安装器**先**写这几个
/// 文件，**后**才下载 `libraries/`。下载中途失败（网络中断、401、磁盘满）时
/// 退出码非零，但若安装器把这几份文件留在了目录里，只按「存在」判定就会把
/// 一次半途而废的安装记成成功——服务器随后必然启动失败（`Missing required
/// library`）。因此判据是「平台参数文件存在且它 `-jar` 指向的入口 JAR 也在」：
/// 参数文件是安装器在依赖齐备后才写的，入口 JAR 与它同级，是最晚一批产物。
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
}

impl InstallerRunner for LocalInstallerRunner {
    fn run_installer(
        &self,
        java_bin: &Path,
        installer: &Path,
        dir: &Path,
        output: Option<&dyn InstallerOutputSink>,
        cancel: &CancellationToken,
    ) -> Result<(), InstallerError> {
        // 已取消就不必拉起进程
        if cancel.is_cancelled() {
            return Err(InstallerError::Cancelled);
        }

        let command = build_installer_command(java_bin, installer, dir);
        let (status, stdout, stderr) = run_and_capture(command, self.timeout, cancel, output)?;

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

/// 等待读线程收尾的上限
///
/// 进程退出后管道未必 EOF：安装器（`Forge` / `NeoForge`）会再拉起子 JVM，
/// Windows 上子孙进程继承同一对句柄，只要它们还活着，`read` 就不会返回 0。
/// 无限 `join` 会让整个安装既等不到结果、也不响应 Ctrl+C——进度条停在 100%
/// 之后永久冻结。超过这个时间就放弃等待：已经收到的输出足够拼失败原因。
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// 运行命令并收集输出，期间响应取消与超时
///
/// 关键点是**不能**用 `Command::output()`：它会一直阻塞到进程退出，安装器联网
/// 卡住时用户既看不到进展也取消不了。这里用 `try_wait` 轮询，由调用方给上限。
///
/// `output` 为 `Some` 时把 stdout/stderr 的每个完整行实时交出（去行尾换行），
/// 让漫长的联网下载阶段有可见进展；原始字节仍完整累积，供失败时拼尾部。
///
/// 返回 `(退出状态, stdout, stderr)`；被取消或超时时子进程已被 kill 并 wait。
fn run_and_capture(
    mut command: Command,
    timeout: Duration,
    cancel: &CancellationToken,
    output: Option<&dyn InstallerOutputSink>,
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
    // 尚未成行的尾巴：跨分块拼到换行再交给 sink，避免把半行当行发出
    let mut pending = LineBuffer::default();
    let status = loop {
        drain(&rx, &mut stdout, &mut stderr, output, &mut pending);

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

    // 进程已退出：正常情况下读线程会因管道 EOF 结束，join 后取全尾部输出。
    // 但管道可能被子 JVM 持有（见 [`READER_JOIN_TIMEOUT`]），所以只等有限时间，
    // 超时就带着已收到的输出返回，不把用户吊死在这里。
    join_readers(readers, &rx, &mut stdout, &mut stderr, output, &mut pending);
    Ok((status, stdout, stderr))
}

/// 限时等待读线程，把通道里剩余的分块收进输出缓冲
///
/// 以「每个读线程结束」为完成条件，整体不超过 [`READER_JOIN_TIMEOUT`]。
/// 超时后不再 join（读线程会被遗弃；管道的所有权已在其中，不会泄漏到主线程），
/// 已到达的数据全部保留。
fn join_readers(
    readers: Vec<std::thread::JoinHandle<()>>,
    rx: &mpsc::Receiver<(bool, Vec<u8>)>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    output: Option<&dyn InstallerOutputSink>,
    pending: &mut LineBuffer,
) {
    let deadline = Instant::now() + READER_JOIN_TIMEOUT;
    let mut pending_readers = readers;
    while !pending_readers.is_empty() {
        pending_readers.retain(|reader| !reader.is_finished());
        drain(rx, stdout, stderr, output, pending);
        if pending_readers.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            tracing::debug!(
                remaining = pending_readers.len(),
                "installer output pipes did not close, \
                 likely held by a grandchild JVM; stop waiting"
            );
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    // 读线程已结束（或确定不再等）：把通道里剩下的分块收干净，并吐出未完行的尾巴
    drain(rx, stdout, stderr, output, pending);
    pending.flush(output);
}

/// 把通道里已到达的分块全部收下
///
/// 原始字节照旧累积（失败时要拼尾部），同时把完整行实时交给 `output`。
fn drain(
    rx: &mpsc::Receiver<(bool, Vec<u8>)>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    output: Option<&dyn InstallerOutputSink>,
    pending: &mut LineBuffer,
) {
    while let Ok((is_stdout, chunk)) = rx.try_recv() {
        if is_stdout {
            stdout.extend_from_slice(&chunk);
        } else {
            stderr.extend_from_slice(&chunk);
        }
        pending.push(&chunk, output);
    }
}

/// 把分块字节切成完整行：跨分块的半行暂存，凑齐换行再交付
///
/// stdout 与 stderr 共用同一个缓冲：安装器两者都是纯文本进展，合流显示更
/// 自然；万一交错拼接出问题，也只是显示层面，不影响失败时的原始尾部。
#[derive(Default)]
struct LineBuffer {
    /// 尚未遇到换行的尾巴
    partial: Vec<u8>,
}

impl LineBuffer {
    /// 追加分块，把其中的完整行逐条交给 `output`
    fn push(&mut self, chunk: &[u8], output: Option<&dyn InstallerOutputSink>) {
        let Some(sink) = output else {
            return;
        };
        self.partial.extend_from_slice(chunk);
        // 逐行消费：找到换行就把前面的内容作为一行交出，剩余留作下一轮
        while let Some(index) = self.partial.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.partial.drain(..=index).collect();
            // 去掉行尾 `\n` 与 `\r`（Windows 换行）
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            if !line.is_empty() {
                sink.on_output(&String::from_utf8_lossy(&line));
            }
        }
    }

    /// 进程结束后吐出最后一条没有换行结尾的输出
    fn flush(&mut self, output: Option<&dyn InstallerOutputSink>) {
        let Some(sink) = output else {
            self.partial.clear();
            return;
        };
        if self.partial.is_empty() {
            return;
        }
        let mut line = std::mem::take(&mut self.partial);
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if !line.is_empty() {
            sink.on_output(&String::from_utf8_lossy(&line));
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
///
/// 见 [`EXPECTED_MARKERS`]：存在参数文件只说明安装器跑过前半段。真正的判据是
/// **入口 JAR 也在**——参数文件里 `-jar` 指向的那个文件存在，才说明这一版
/// 安装已经下载完依赖。
fn has_expected_marker(dir: &Path) -> bool {
    if !EXPECTED_MARKERS.iter().any(|name| dir.join(name).is_file()) {
        return false;
    }
    // 参数文件存在但入口 JAR 缺失 = 安装中途失败；扫一遍 libraries 里的参数文件
    // （安装器在下载依赖前就写了它们），全部通过才算成功
    for args_file in args_files_in(dir) {
        let Ok(content) = std::fs::read_to_string(&args_file) else {
            // 读不出来时无法确认入口，按失败处理，让调用方重试
            return false;
        };
        let origin = args_file.parent().unwrap_or(dir);
        if let Some(entry) = launch_entry(&content, origin)
            && !entry.is_file()
        {
            return false;
        }
    }
    true
}

/// 目录下安装器写出的参数文件（`user_jvm_args.txt` 除外：它不含入口）
fn args_files_in(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(args_files_in(&path));
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with("_args.txt") && name != "user_jvm_args.txt" {
            files.push(path);
        }
    }
    files
}

/// 从 Java 参数文件里找出 `-jar` 指向的入口 JAR（相对 `origin` 解析）
///
/// 参数文件是空白分隔的一行参数；`-jar` 的值就是入口。找不到 `-jar` 时返回
/// `None`（如 `user_jvm_args.txt` 只有 JVM 参数），调用方不据此判失败。
fn launch_entry(content: &str, origin: &Path) -> Option<PathBuf> {
    let tokens: Vec<&str> = content.split_whitespace().collect();
    let index = tokens.iter().position(|token| *token == "-jar")?;
    let jar = tokens.get(index + 1)?;
    // 参数文件里可能写 Windows 或 Unix 分隔符，统一按平台约定拼相对路径
    let normalized = jar.replace('\\', std::path::MAIN_SEPARATOR_STR);
    let path = Path::new(&normalized);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        origin.join(path)
    })
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

    /// 从参数文件里找不到 `-jar` 时视为入口未知（`None`），不据此判失败
    #[test]
    fn launch_entry_reads_jar_after_flag() {
        let origin = Path::new("/srv/mc/a");
        assert_eq!(
            launch_entry("-Xmx4G -jar forge-26.3-66.1.0-shim.jar\n", origin),
            Some(origin.join("forge-26.3-66.1.0-shim.jar"))
        );
        // 反斜杠按平台分隔符归一
        assert_eq!(
            launch_entry("-jar libraries\\forge\\server.jar", origin),
            Some(origin.join("libraries").join("forge").join("server.jar"))
        );
        assert_eq!(launch_entry("# -Xmx4G\n", origin), None);
        assert_eq!(launch_entry("-jar", origin), None);
    }

    /// 参数文件存在但入口 JAR 缺失时不算装好
    #[test]
    fn marker_requires_launch_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("user_jvm_args.txt"), b"# args").expect("write");
        let nested = dir
            .path()
            .join("libraries")
            .join("net")
            .join("minecraftforge")
            .join("forge")
            .join("26.3-66.1.0");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::write(
            nested.join("win_args.txt"),
            b"-Djava.net.preferIPv6Addresses=system -jar forge-26.3-66.1.0-shim.jar",
        )
        .expect("write");
        assert!(
            !has_expected_marker(dir.path()),
            "参数文件在、入口 JAR 不在：安装是中途中止的"
        );
        std::fs::write(nested.join("forge-26.3-66.1.0-shim.jar"), b"jar").expect("write");
        assert!(has_expected_marker(dir.path()));
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
                None,
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
        let err = run_and_capture(
            cmd,
            Duration::from_millis(300),
            &CancellationToken::new(),
            None,
        )
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
        let err = run_and_capture(cmd, DEFAULT_INSTALLER_TIMEOUT, &cancel, None)
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
        let (status, stdout, stderr) = run_and_capture(
            cmd,
            DEFAULT_INSTALLER_TIMEOUT,
            &CancellationToken::new(),
            None,
        )
        .expect("should run");
        assert_eq!(status.code(), Some(3));
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        assert!(combined.contains("boom"), "输出: {combined:?}");
    }

    /// 安装器输出逐行实时交出：多行、stderr 合流、末尾无换行都要覆盖
    #[test]
    fn streams_each_output_line() {
        struct Collector(std::sync::Mutex<Vec<String>>);
        impl InstallerOutputSink for Collector {
            fn on_output(&self, line: &str) {
                self.0.lock().expect("lock").push(line.to_owned());
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let mut cmd = if cfg!(windows) {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", "echo one && echo two 1>&2 && echo three"]);
            cmd
        } else {
            shell_command("echo one; echo two 1>&2; echo three")
        };
        cmd.current_dir(dir.path());
        let sink = Collector(std::sync::Mutex::new(Vec::new()));
        run_and_capture(
            cmd,
            DEFAULT_INSTALLER_TIMEOUT,
            &CancellationToken::new(),
            Some(&sink),
        )
        .expect("should run");
        let lines = sink.0.into_inner().expect("lock");
        assert!(lines.iter().any(|l| l == "one"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "two"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "three"), "{lines:?}");
    }

    /// 半行跨分块必须拼成一行；进程结束仍未换行的尾巴也要吐出
    #[test]
    fn line_buffer_joins_split_lines() {
        struct Collector(std::sync::Mutex<Vec<String>>);
        impl InstallerOutputSink for Collector {
            fn on_output(&self, line: &str) {
                self.0.lock().expect("lock").push(line.to_owned());
            }
        }

        let sink = Collector(std::sync::Mutex::new(Vec::new()));
        let mut buffer = LineBuffer::default();
        buffer.push(b"downlo", Some(&sink));
        assert!(sink.0.lock().expect("lock").is_empty(), "半行不能先发");
        buffer.push(b"ading\r\nsecond", Some(&sink));
        buffer.flush(Some(&sink));
        assert_eq!(
            sink.0.into_inner().expect("lock"),
            vec!["downloading".to_owned(), "second".to_owned()]
        );
    }

    /// 退出码 0 但没有产物 → NoOutput（退出码 0 不等于装好了）
    #[test]
    fn zero_exit_without_marker_is_no_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 用必然成功的命令充当 java：只 exit 0，不写任何产物
        let mut cmd = shell_command("exit 0");
        cmd.current_dir(dir.path());
        let (status, _, _) = run_and_capture(
            cmd,
            DEFAULT_INSTALLER_TIMEOUT,
            &CancellationToken::new(),
            None,
        )
        .expect("should run");
        assert!(status.success());
        assert!(
            !has_expected_marker(dir.path()),
            "测试前提：没有任何预期产物"
        );
    }

    /// 父进程退出后管道被子进程持有：不能卡在 join 上无限等待
    ///
    /// 复刻 `Forge` 安装器拉起的子 JVM 继承 stdout/stderr 句柄的情形：父进程
    /// 打印一行就退出，但子进程仍持有同一对管道。修复前这里会一直等到子进程
    /// 结束（也既不响应取消），表现为 `server create` 在进度条走完后卡死。
    #[test]
    fn exits_even_when_grandchild_holds_pipes() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 父进程把 stdout 重定向进文件（于是**只有**那个后台子进程持有管道），
        // 随后立即退出；子进程睡一阵才退出。
        let mut cmd = if cfg!(windows) {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", "start /b ping -n 60 127.0.0.1 >nul && exit 0"]);
            cmd
        } else {
            // 后台 `sleep` 继承 stdout/stderr，父 shell 立刻退出
            shell_command("sleep 60 & echo boom; exit 0")
        };
        cmd.current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let started = Instant::now();
        run_and_capture(
            cmd,
            DEFAULT_INSTALLER_TIMEOUT,
            &CancellationToken::new(),
            None,
        )
        .expect("should not hang");
        let elapsed = started.elapsed();

        // 父进程先退出，最坏也要在 join 上限内返回，而不是等子进程的 60 秒
        assert!(
            elapsed < Duration::from_secs(20),
            "管道被子进程持有时应限时返回，实际 {elapsed:?}"
        );
    }
}
