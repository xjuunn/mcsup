//! supervisor 主循环：监护服务端进程 + 提供 IPC
//!
//! 单线程主循环负责收尸与 accept（非阻塞轮询），每个连接在独立线程应答；
//! 优雅停机靠 stdin 注入 `stop`，宽限到期且 `force` 才强杀。
use crate::client;
use crate::endpoint;
use crate::launch::{self, MinecraftLaunch};
use domain::server::layout::{console_log_path, run_dir};
use domain::server::status::ServerStatus;
use interprocess::local_socket::ListenerNonblockingMode;
use interprocess::local_socket::prelude::*;
use protocol::{self, Request, Response, StopResult};
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::process::{Child, ChildStdin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// 服务端启动完成的就绪标志（Minecraft 系列统一的 Done 行）
///
/// 属于运行时就绪判定（读 stdout 流），不是 IPC 契约的一部分，故留在
/// runtime 而非 `protocol` crate。
const READY_MARKER: &str = "Done (";

/// 主循环轮询间隔
const TICK: Duration = Duration::from_millis(100);
/// 子进程退出后继续留守应答状态查询的时间
const LINGER: Duration = Duration::from_secs(5);
/// 强杀后等待退出的时间
const KILL_WAIT: Duration = Duration::from_secs(5);

/// 运行视图（所有字段只在锁内读写）
struct View {
    status: ServerStatus,
    spawned_at: Instant,
    exit_code: Option<i32>,
}

/// 跨线程共享状态
struct Shared {
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    view: Mutex<View>,
    pid: u32,
    log: Arc<Mutex<BufWriter<std::fs::File>>>,
}

/// 锁中毒不丢数据：本进程内锁内容总是自洽的
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 启动失败 / 致命错误写到 stderr（→ `run/supervisor.log`）
///
/// 不用 `eprintln!`：workspace 的 `print_stderr` lint 面向入口输出。
fn supervisor_note(message: &str) {
    let mut stderr = io::stderr();
    let _ = writeln!(stderr, "mcsup supervisor: {message}");
}

/// 忽略 SIGHUP：终端关闭 / 会话挂断不带走 supervisor
///
/// 注册一个只置标志位的安全动作即改写默认终止行为；子进程经
/// `process_group` 脱离前台组，不会收到终端信号。
#[cfg(unix)]
fn ignore_hup() {
    let flag = Arc::new(AtomicBool::new(false));
    if let Err(err) = signal_hook::flag::register(signal_hook::consts::signal::SIGHUP, flag) {
        supervisor_note(&format!("ignore SIGHUP failed: {err}"));
    }
}

/// 子进程退出后由主循环调用：按领域状态机落终态
///
/// 停机请求下的任何退出（含被强杀）都算正常停机。
fn record_exit(view: &mut View, code: i32) {
    view.exit_code = Some(code);
    let next = match view.status {
        ServerStatus::Stopping => ServerStatus::Stopped,
        status if status.is_alive() && code == 0 => ServerStatus::Stopped,
        status if status.is_alive() => ServerStatus::Crashed,
        status => status, // 已终态：保持
    };
    if let Ok(transitioned) = view.status.transition(next) {
        view.status = transitioned;
    }
}

/// 收集子进程输出：逐行写 `console.log` 并做就绪判定
///
/// 日志按**原始字节**落盘，不做解码：服务端在 Windows 下按系统代码页（GBK）
/// 输出中文，用 `from_utf8_lossy` 先解码会把非 UTF-8 字节替换成 U+FFFD，
/// 写进日志的中文就永久损坏了。编码解释留给读取 / 展示端。
///
/// 就绪判定用 lossy 字符串匹配 ASCII 标记（[`READY_MARKER`]），编码无关。
fn pump<R: BufRead + Send + 'static>(mut reader: R, shared: &Shared) {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break, // EOF 或读取失败：放弃该流，进程监护不受影响
            Ok(_) => {
                {
                    // 原样写出收到的字节，仅保证以换行结尾
                    let mut log = lock(&shared.log);
                    let _ = log.write_all(strip_line_ending(&buf));
                    let _ = log.write_all(b"\n");
                    let _ = log.flush();
                }
                let decoded = String::from_utf8_lossy(&buf);
                let line = decoded.trim_end_matches(['\n', '\r']);
                if line.contains(READY_MARKER) {
                    let mut view = lock(&shared.view);
                    if view.status == ServerStatus::Starting
                        && let Ok(next) = view.status.transition(ServerStatus::Running)
                    {
                        view.status = next;
                    }
                }
            }
        }
    }
}

/// 去掉行尾的 `\n` / `\r\n`，返回不含行尾的字节切片
///
/// 写入端统一补一个 `\n`，避免不同平台的子进程输出混入多余空行。
fn strip_line_ending(bytes: &[u8]) -> &[u8] {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

/// 向服务端 stdin 注入单行（剥掉换行，保持单行语义）
fn write_console_line(shared: &Shared, line: &str) -> io::Result<()> {
    let clean: String = line
        .chars()
        .filter(|ch| !matches!(ch, '\r' | '\n'))
        .collect();
    let mut guard = lock(&shared.stdin);
    match guard.as_mut() {
        Some(stdin) => {
            stdin.write_all(clean.as_bytes())?;
            stdin.write_all(b"\n")?;
            stdin.flush()
        }
        None => Err(io::Error::other("console pipe closed")),
    }
}

/// 停机流程；返回线格式结果与耗时
fn perform_stop(shared: &Shared, grace: Duration, force: bool) -> (&'static str, Duration) {
    let started = Instant::now();

    // 进入 Stopping（幂等：已在 Stopping 则直接进入等待）
    {
        let mut view = lock(&shared.view);
        if view.status.is_terminal() {
            return (StopResult::ALREADY_STOPPED, Duration::ZERO);
        }
        if view.status != ServerStatus::Stopping {
            match view.status.transition(ServerStatus::Stopping) {
                Ok(next) => view.status = next,
                Err(_) => return (StopResult::ALREADY_STOPPED, Duration::ZERO),
            }
        }
    }

    // 优雅路径：stdin 注入 stop（失败交给宽限 / 强杀兜底）
    let _ = write_console_line(shared, "stop");

    // 等到终态或宽限到期
    loop {
        if lock(&shared.view).status.is_terminal() {
            return (StopResult::STOPPED, started.elapsed());
        }
        if started.elapsed() >= grace {
            if !force {
                return (StopResult::TIMED_OUT, started.elapsed());
            }
            break;
        }
        thread::sleep(TICK);
    }

    // 强杀
    {
        let mut guard = lock(&shared.child);
        if let Some(child) = guard.as_mut() {
            let _ = child.kill();
        }
    }
    loop {
        if lock(&shared.view).status.is_terminal() {
            return (StopResult::STOPPED, started.elapsed());
        }
        if started.elapsed() >= grace.saturating_add(KILL_WAIT) {
            return (StopResult::TIMED_OUT, started.elapsed());
        }
        thread::sleep(TICK);
    }
}

/// 应答单个连接：一问一答后关闭
fn handle_connection(
    mut stream: interprocess::local_socket::Stream,
    shared: &Shared,
    shutdown: &AtomicBool,
) {
    let response = {
        let mut reader = BufReader::new(&mut stream);
        match protocol::read_frame::<_, Request>(&mut reader) {
            Ok(request) => fulfill(request, shared, shutdown),
            Err(err) => Response::Error {
                kind: "protocol".to_owned(),
                message: err.to_string(),
            },
        }
    };
    let _ = protocol::write_frame(&mut stream, &response);
}

/// 请求 → 响应
fn fulfill(request: Request, shared: &Shared, shutdown: &AtomicBool) -> Response {
    match request {
        Request::Status => {
            let view = lock(&shared.view);
            let alive = view.status.is_alive();
            Response::Status {
                status: view.status.as_str().to_owned(),
                pid: alive.then_some(shared.pid),
                uptime_secs: alive.then(|| view.spawned_at.elapsed().as_secs()),
                exit_code: view.exit_code,
            }
        }
        Request::Stop { grace_secs, force } => {
            // 请求来自本机 IPC，同样按领域上限夹紧：宽限过大时等待循环
            // 永远不会醒来，等于把 supervisor 线程锁死
            let grace = Duration::from_secs(grace_secs).min(domain::limits::MAX_STOP_GRACE);
            let (result, elapsed) = perform_stop(shared, grace, force);
            Response::Stop {
                result: result.to_owned(),
                elapsed_secs: elapsed.as_secs(),
            }
        }
        Request::Send { line } => {
            if lock(&shared.view).status.is_terminal() {
                return Response::Error {
                    kind: "not_running".to_owned(),
                    message: "server process has exited".to_owned(),
                };
            }
            match write_console_line(shared, &line) {
                Ok(()) => Response::Ok,
                Err(err) => Response::Error {
                    kind: "io".to_owned(),
                    message: err.to_string(),
                },
            }
        }
        Request::Shutdown => {
            if !lock(&shared.view).status.is_terminal() {
                return Response::Error {
                    kind: "busy".to_owned(),
                    message: "server is still running".to_owned(),
                };
            }
            shutdown.store(true, Ordering::Relaxed);
            Response::Ok
        }
    }
}

/// 终态 → supervisor 退出码
fn supervisor_exit_code(status: ServerStatus) -> i32 {
    match status {
        ServerStatus::Crashed => 1,
        _ => 0,
    }
}

/// 环境准备：run 目录 / 清场 / console.log / IPC 监听 / 子进程 / 共享状态
///
/// **先绑 IPC 端点，再启动子进程**：`Child` 在 drop 时不会杀进程，若先 spawn
/// 再 bind，任何 bind / `set_nonblocking` 失败都会留下一个仍在写 world 的
/// Minecraft 进程，而且调用方已经收到启动失败。反过来，bind 失败时子进程
/// 尚未存在，直接返回即可；spawn 失败时 listener 随作用域 drop 并清掉端点。
///
/// # Errors
///
/// 任一准备步骤失败返回退出码 `1`（错误已记入 supervisor 日志）
fn prepare<F>(
    server_dir: &Path,
    spawn: F,
) -> Result<(Arc<Shared>, interprocess::local_socket::Listener), i32>
where
    F: FnOnce() -> io::Result<Child>,
{
    if let Err(err) = std::fs::create_dir_all(run_dir(server_dir)) {
        supervisor_note(&format!("create run dir failed: {err}"));
        return Err(1);
    }
    endpoint::cleanup_stale(server_dir);

    let log_file = match OpenOptions::new()
        .create(true)
        .append(true)
        .open(console_log_path(server_dir))
    {
        Ok(file) => file,
        Err(err) => {
            supervisor_note(&format!("open console.log failed: {err}"));
            return Err(1);
        }
    };
    let log = Arc::new(Mutex::new(BufWriter::new(log_file)));

    let name = endpoint::endpoint_name(server_dir).map_err(|err| {
        supervisor_note(&format!("endpoint name failed: {err}"));
        1
    })?;
    let listener = interprocess::local_socket::ListenerOptions::new()
        .name(name)
        .create_sync()
        .map_err(|err| {
            supervisor_note(&format!("bind ipc endpoint failed: {err}"));
            1
        })?;
    listener
        .set_nonblocking(ListenerNonblockingMode::Accept)
        .map_err(|err| {
            supervisor_note(&format!("set nonblocking failed: {err}"));
            1
        })?;

    let mut child = match spawn() {
        Ok(child) => child,
        Err(err) => {
            supervisor_note(&format!("spawn server process failed: {err}"));
            return Err(1);
        }
    };
    let pid = child.id();
    if let Err(err) = std::fs::write(endpoint::pid_path(server_dir), pid.to_string()) {
        supervisor_note(&format!("write pid file failed: {err}"));
    }

    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let status = ServerStatus::Stopped
        .transition(ServerStatus::Starting)
        .unwrap_or(ServerStatus::Starting);
    let shared = Arc::new(Shared {
        child: Mutex::new(Some(child)),
        stdin: Mutex::new(stdin),
        view: Mutex::new(View {
            status,
            spawned_at: Instant::now(),
            exit_code: None,
        }),
        pid,
        log,
    });

    if let Some(stdout) = stdout {
        let shared = Arc::clone(&shared);
        thread::spawn(move || pump(BufReader::new(stdout), &shared));
    }
    if let Some(stderr) = stderr {
        let shared = Arc::clone(&shared);
        thread::spawn(move || pump(BufReader::new(stderr), &shared));
    }

    Ok((shared, listener))
}

/// 收一次尸（只在本线程 `try_wait`）；返回本次观察到的退出码
fn reap(shared: &Shared) -> Option<i32> {
    let mut guard = lock(&shared.child);
    match guard.as_mut() {
        Some(child) => match child.try_wait() {
            Ok(Some(exit)) => {
                let code = exit.code().unwrap_or(-1);
                *guard = None;
                Some(code)
            }
            Ok(None) => None,
            Err(err) => {
                supervisor_note(&format!("wait server process failed: {err}"));
                *guard = None;
                Some(-1)
            }
        },
        None => None,
    }
}

/// 终态后的退出判定：shutdown 即走，否则留守 [`LINGER`] 应答查询
fn exit_decision(view: &View, shutdown: &AtomicBool, exited_at: Option<Instant>) -> Option<i32> {
    if !view.status.is_terminal() {
        return None;
    }
    if shutdown.load(Ordering::Relaxed) {
        return Some(supervisor_exit_code(view.status));
    }
    if exited_at.is_some_and(|at| at.elapsed() >= LINGER) {
        return Some(supervisor_exit_code(view.status));
    }
    None
}

/// supervisor 主循环（测试可注入子进程构造）
///
/// # 错误
///
/// 通过返回码表达：`0` 正常停机，`1` 致命错误或服务端崩溃退出，
/// `2` 端点已被另一个活着的 supervisor 占用（拒绝成为第二个）
pub(crate) fn run_with<F>(server_dir: &Path, spawn: F) -> i32
where
    F: FnOnce() -> io::Result<Child>,
{
    #[cfg(unix)]
    ignore_hup();

    // 双进程竞态闸：CLI 侧的 probe→spawn 有窗口，两个并发 start 会各自
    // 通过「不在运行」检查。此处再探一次：端点上已有活的 supervisor 时
    // 拒绝启动（返回 2），绝不清理别人的端点——清了会让两个 Minecraft
    // 写同一个 world。探测本身失败按「无主」处理，bind 冲突仍会拦下
    if let Ok(Some(status)) = client::probe_status(server_dir)
        && status.status.is_alive()
    {
        supervisor_note("another live supervisor owns the endpoint; refusing to start");
        return 2;
    }

    let (shared, listener) = match prepare(server_dir, spawn) {
        Ok(prepared) => prepared,
        Err(code) => return code,
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut exited_at: Option<Instant> = None;

    let exit_code = loop {
        if let Some(code) = reap(&shared) {
            record_exit(&mut lock(&shared.view), code);
            exited_at.get_or_insert_with(Instant::now);
        }
        {
            let view = lock(&shared.view);
            if let Some(code) = exit_decision(&view, &shutdown, exited_at) {
                break code;
            }
        }

        // 接受连接（非阻塞轮询，保证收尸不被阻塞）
        match listener.accept() {
            Ok(stream) => {
                let shared = Arc::clone(&shared);
                let shutdown = Arc::clone(&shutdown);
                thread::spawn(move || handle_connection(stream, &shared, &shutdown));
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
            Err(err) => {
                // 瞬时错误（如已断开的连接）不致命，继续轮询
                tracing::debug!(error = %err, "accept transient error");
            }
        }

        thread::sleep(TICK);
    };

    // 清理：PID 文件；listener drop 回收 socket 名（Unix）
    drop(listener);
    let _ = std::fs::remove_file(endpoint::pid_path(server_dir));
    {
        let mut guard = lock(&shared.child);
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    exit_code
}

/// supervisor 生产入口：解析后的启动参数 → 拉起 Minecraft 并进入主循环
///
/// # 参数
///
/// 由 `mcsup __supervise` 调用；返回进程退出码。
#[must_use]
pub fn run_supervisor(launch: &MinecraftLaunch) -> i32 {
    let launch = launch.clone();
    let server_dir = launch.server_dir.clone();
    run_with(&server_dir, move || launch::spawn_minecraft(&launch))
}

#[cfg(test)]
mod tests {
    use super::strip_line_ending;
    #[cfg(unix)]
    use super::{prepare, run_dir};

    /// bind 失败时不得先启动服务端进程
    ///
    /// `Child` 在 drop 时不会杀进程：若先 spawn 再 bind，bind 失败会留下仍在
    /// 写 world 的孤儿 JVM，而调用方已经收到启动失败。
    ///
    /// 用目录占住 Unix socket 路径：`cleanup_stale` 只能删文件，删不掉目录，
    /// 后续 bind 必然失败。Windows 端点走命名管道，不适用这条构造。
    #[cfg(unix)]
    #[test]
    fn bind_failure_does_not_spawn_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = run_dir(dir.path());
        std::fs::create_dir_all(&run).expect("run dir");
        std::fs::create_dir(run.join("supervisor.sock")).expect("block socket path");

        let mut spawned = false;
        let result = prepare(dir.path(), || {
            spawned = true;
            std::process::Command::new("sh")
                .arg("-c")
                .arg("true")
                .spawn()
        });

        assert!(result.is_err(), "占用 socket 路径时 bind 应失败");
        assert!(!spawned, "bind 失败时不得启动服务端进程");
    }

    /// 行尾统一剥成不含换行的字节，供写入端补单个 `\n`
    #[test]
    fn strip_line_ending_removes_trailing_newline() {
        assert_eq!(strip_line_ending(b"abc\n"), b"abc");
        assert_eq!(strip_line_ending(b"abc\r\n"), b"abc");
        assert_eq!(strip_line_ending(b"abc"), b"abc");
        assert_eq!(strip_line_ending(b"\n"), b"");
        assert_eq!(strip_line_ending(b""), b"");
    }

    /// 非 UTF-8 字节（如 GBK 中文）必须原样保留，不能被解码替换
    #[test]
    fn strip_line_ending_keeps_non_utf8_bytes() {
        // GBK 的「正在」（0xD5 0xFD 0xD4 0xDA），不是合法 UTF-8
        let gbk = b"\xD5\xFD\xD4\xDA\n";
        assert_eq!(strip_line_ending(gbk), b"\xD5\xFD\xD4\xDA");
    }
}
