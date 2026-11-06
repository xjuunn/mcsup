//! agent 运行态文件与进程：pid / 监听地址 / 上次监听 / 指纹

use super::serve::AGENT_PID;
use super::{Context, Path, PathBuf, Result};

/// pid 文件路径
pub(super) fn pid_path(data_dir: &Path) -> PathBuf {
    data_dir.join("remote").join(AGENT_PID)
}

pub(super) fn write_pid(data_dir: &Path, pid: u32) -> Result<()> {
    let path = pid_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录 {}", parent.display()))?;
    }
    std::fs::write(&path, pid.to_string())
        .with_context(|| format!("写入 pid 文件 {}", path.display()))
}

/// 监听地址文件名（`{data_dir}/remote/agent.addr`）
///
/// 只用于 `serve status` / `serve start` 回显：真实地址由 bind 决定（缺省端口
/// 固定，所以不是必需，但 `--listen :0` 时没有它就查不到）。
pub(super) const AGENT_ADDR: &str = "agent.addr";

pub(super) fn write_listen_addr(data_dir: &Path, addr: &str) {
    let path = data_dir.join("remote").join(AGENT_ADDR);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 只作展示：写不进去不影响服务本身
    let _ = std::fs::write(path, addr);
}

pub(super) fn remove_listen_addr(data_dir: &Path) {
    let _ = std::fs::remove_file(data_dir.join("remote").join(AGENT_ADDR));
}

pub(super) fn read_listen_addr(data_dir: &Path) -> Option<String> {
    std::fs::read_to_string(data_dir.join("remote").join(AGENT_ADDR))
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// 上次成功监听过的地址（`{data_dir}/remote/agent.last-listen`）
///
/// 与 [`AGENT_ADDR`] 分开：那个是**本次**运行的就绪信号与回显源，`start` 会
/// 在拉起子进程前删掉它；这个跨启动保留，用来让不带 `--listen` 的 `start`
/// 沿用管理员上次显式指定的地址。
pub(super) const AGENT_LAST_LISTEN: &str = "agent.last-listen";

/// 记住本次使用的监听地址（只对「显式指定」生效，见 [`resolve_listen`]）
pub(super) fn remember_listen(data_dir: &Path, listen: &str) {
    let path = data_dir.join("remote").join(AGENT_LAST_LISTEN);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 只影响「下次不带参数时的默认值」：写不进去不该让本次启动失败
    let _ = std::fs::write(path, listen);
}

pub(super) fn read_last_listen(data_dir: &Path) -> Option<String> {
    std::fs::read_to_string(data_dir.join("remote").join(AGENT_LAST_LISTEN))
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// 没给 `--listen` 时的地址：沿用上次显式指定的，否则用安全默认
///
/// 优先级是「显式 > 上次显式 > 回环」。只在用户**显式**指定过的时候才记住，
/// 避免把内部的默认值当成用户意图固化下来。
pub(super) fn resolve_listen(data_dir: &Path, requested: Option<String>) -> String {
    requested
        .or_else(|| read_last_listen(data_dir))
        .unwrap_or_else(|| DEFAULT_LISTEN.to_owned())
}

/// 安全默认：只监听回环
pub(super) const DEFAULT_LISTEN: &str = "127.0.0.1:7681";

/// 本机证书指纹（读身份文件；没有身份时返回 `None`）
pub(super) fn read_fingerprint(data_dir: &Path) -> Option<String> {
    let dir = mcsup::remote::identity_dir(data_dir);
    mcsup::transport::ServerIdentity::load(&dir)
        .ok()
        .map(|identity| identity.fingerprint().to_string())
}

pub(super) fn read_pid(data_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_path(data_dir))
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

pub(super) fn remove_pid(data_dir: &Path) {
    let _ = std::fs::remove_file(pid_path(data_dir));
}

/// 进程是否存活（`kill -0` / `tasklist`，不引平台库）
pub(super) fn is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // `output()` 而非 `status()`：探测不该往调用方的 stdout 写任何东西
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
            .is_ok_and(|output| output.status.success())
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// 进程的镜像名 / 命令行；取不到返回 `None`
pub(super) fn process_command(pid: u32) -> Option<String> {
    #[cfg(unix)]
    {
        // `command=` 是完整命令行；`comm=` 在 Linux 上会被截到 15 字符
        let out = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }
    #[cfg(windows)]
    {
        // CSV 首列是镜像名：`"mcsup.exe","1234",...`
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines()
            .next()
            .and_then(|line| line.split(',').next())
            .map(|image| image.trim_matches('"').to_owned())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        None
    }
}

/// pid 对应的进程看起来是不是 mcsup
///
/// pid 文件与进程生命周期没有绑定：agent 崩溃后 pid 会被系统复用，
/// 照 pid 直接杀会打死无关进程。终止前必须核对镜像名 / 命令行，
/// 这与 `is_alive` 是两件事：活着不代表还是当初那个进程。
pub(super) fn is_mcsup_process(pid: u32) -> bool {
    process_command(pid).is_some_and(|command| command.to_ascii_lowercase().contains("mcsup"))
}

/// 终止进程
///
/// Unix 默认发 `SIGTERM`，`force` 才 `SIGKILL`。
///
/// Windows **一律** `/F`：不带 `/F` 的 `taskkill` 只是向窗口投递关闭请求，
/// 而 agent 是用 `DETACHED_PROCESS` 拉起的，既无窗口也无控制台，没有可投递的
/// 对象，`taskkill` 会直接失败并报「只能强制终止」。也就是说 Windows 上不存在
/// 与 `SIGTERM` 对应的「可捕获的礼貌退出」，`force` 在这里没有第二条路可选。
///
/// 这不损失可收尾性：agent 进程不注册任何信号处理器（见 `apps/agent`），Unix
/// 上 `SIGTERM` 走的也是默认终止，与 `SIGKILL` 一样不给收尾机会。真正的
/// 「是否真的停了」由调用方等实例锁释放来判定——那是内核锁，进程无论怎么
/// 死都会释放。
pub(super) fn terminate(pid: u32, force: bool) -> Result<()> {
    // 一律 `output()` 而不是 `status()`：`status()` 让子进程继承本进程的
    // stdout/stderr，而 `taskkill` 成功时也会打印
    // `SUCCESS: The process with PID 7624 has been terminated.`——它会直接混进
    // `--output json` 的 stdout，把「纯数据」契约打破（CI 上正是这么红的：
    // 测试拿到的第一行不是 JSON）。`output()` 把两路都收进管道，既不污染
    // 调用方的流，又能在失败时把 stderr 带进错误信息，比只有退出码更有用。
    #[cfg(unix)]
    let output = std::process::Command::new("kill")
        .arg(if force { "-9" } else { "-15" })
        .arg(pid.to_string())
        .output();
    #[cfg(windows)]
    let output = {
        // `force` 在这里没有第二条路可走（见上）：显式吞掉以免 Windows 上
        // 报 unused variable，而 `-D warnings` 会让它变成编译失败
        let _ = force;
        let mut command = std::process::Command::new("taskkill");
        command.args(["/PID", &pid.to_string(), "/F"]);
        command.output()
    };
    #[cfg(not(any(unix, windows)))]
    let output: std::io::Result<std::process::Output> = Err(std::io::Error::other(
        "terminating a process is not supported on this platform",
    ));
    let output = output?;
    if output.status.success() {
        return Ok(());
    }
    let status = output.status;
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        Err(anyhow::anyhow!("terminate {pid} exited with {status}"))
    } else {
        Err(anyhow::anyhow!(
            "terminate {pid} exited with {status}: {detail}"
        ))
    }
}
