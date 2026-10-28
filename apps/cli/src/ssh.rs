//! SSH 引导与本地隧道：复用已有 SSH 信任，不新增凭证
//!
//! `remote connect user@host` 的两步都在这里：
//!
//! 1. 经 SSH 执行远端的隐藏入口 `mcsup remote __serve-ssh`，从 stdout 读回
//!    agent 的地址、证书指纹与配对码；
//! 2. 起一条 `ssh -N -L` 隧道把本地回环端口转到远端的 agent 端口，之后的
//!    RPC 全部走 `127.0.0.1:<local>`。
//!
//! 两条子进程绑在一起登记为一条**会话**（[`SshSession`]）：引导进程活着，
//! 远端的 agent 才活着；隧道活着，本地才有可连的地址。任何一步失败，整条
//! 会话作废，不留半截。
//!
//! 会话与 CLI 进程同寿命：命令结束即断开，符合「按需连接、无常驻会话」。
//! 同一条命令里的多处使用（组合仓库、supervisor、备份执行器）复用同一条
//! 会话，不会为每个成员重复起 agent。
//!
//! 为什么每次命令都要重新引导：远端 agent 监听的是**随机端口**，且它随上
//! 一条 SSH 会话一起结束（见 `mcsup::remote::serve_ssh_main` 的 stdin EOF
//! 收尾）。记录里存一个上次的端口没有意义——那条 agent 早就不在了。真正
//! 稳定的是「怎么连过去」，也就是 `user@host`。
//!
//! 为什么用 SSH 而不是在远端开公网端口：SSH 的信任已经存在，用户不需要
//! 额外分发 token、也不需要把 agent 暴露到网络上。证书指纹仍然是 TOFU 的
//! 信任锚——SSH 只负责「能连上」，不能替代「连的是哪张证书」。

use crate::diagnostic::AppDiagnostic;
use crate::locale::Lang;
use anyhow::Result;
use mcsup::remote::SshHandshake;
use mcsup::service::{RemoteRecord, RemoteVia};
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 握手读取上限：远端启动 agent 通常在一秒内；超过这个时间按启动失败处理
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// 隧道建立后等待端口可连的上限
const TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// 一条已建立的 SSH 会话
///
/// 引导进程（远端 agent 的宿主）与隧道进程必须同生共死：只留隧道会连到
/// 已经退出的 agent，只留引导进程则本地没有可用地址。
struct SshSession {
    /// 本地回环地址（`127.0.0.1:<port>`）
    local: String,
    /// 引导时读到的握手信息（配对码只在 `connect` 用得上）
    handshake: SshHandshake,
    /// `ssh user@host mcsup remote __serve-ssh`：远端 agent 的宿主
    bootstrap: Child,
    /// `ssh -N -L ...`：本地端口到远端 agent 端口的转发
    tunnel: Child,
    /// 引导进程 stdout 的接收端：**持有即可，不读**
    ///
    /// 读线程往这里送配对码轮换行；接收端一 drop，线程就停读、管道关闭，
    /// 远端下次写拿到 `EPIPE` 便会收摊。所以它必须和会话同寿命——不是用来
    /// 取数据的字段，而是「让读线程继续读」的凭据。
    _lines: LineReader,
}

impl std::fmt::Debug for SshSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshSession")
            .field("local", &self.local)
            .field("handshake", &self.handshake)
            .finish_non_exhaustive()
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        // 先断隧道再杀引导进程：反过来的话远端 agent 可能已经退出，隧道
        // 还在短暂地接受连接，表现为「连上了但没有响应」
        let _ = self.tunnel.kill();
        let _ = self.tunnel.wait();
        let _ = self.bootstrap.kill();
        let _ = self.bootstrap.wait();
    }
}

/// 会话登记表：同一进程内按 `user@host` 复用
///
/// CLI 是单命令进程，登记表只在一次命令内有效；它的价值是让组合仓库、
/// supervisor、备份执行器三处共用一条会话与一个远端 agent。
fn sessions() -> &'static Mutex<Vec<(String, SshSession)>> {
    static SESSIONS: OnceLock<Mutex<Vec<(String, SshSession)>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 进程退出前收掉全部会话
///
/// 静态变量在进程退出时不会被 drop，`SshSession::drop` 里的 `kill` 也就不会
/// 发生——`ssh -N -L` 子进程会活到系统重启。入口在 `main` 里持有一个
/// [`SessionGuard`]，把「正常退出」这条路补上。
///
/// # Errors
///
/// 会话注册表被 poison 时返回错误（此时已无法可靠清理，只记日志）
pub(crate) fn shutdown() -> Result<()> {
    let mut guard = sessions()
        .lock()
        .map_err(|_| anyhow::anyhow!("ssh session registry is poisoned"))?;
    guard.clear();
    Ok(())
}

/// 会话清理守卫：`main` 持有，退出时清空登记表
///
/// 用独立的守卫而不是在 `main` 末尾显式调用：早退（错误路径提前返回）与
/// panic 展开都走 `Drop`，不必在每个出口各写一遍。
pub(crate) struct SessionGuard;

impl SessionGuard {
    /// 安装守卫
    pub(crate) const fn install() -> Self {
        Self
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Err(err) = shutdown() {
            tracing::warn!(%err, "ssh session cleanup failed");
        }
    }
}

/// SSH 可执行文件（`MCSUP_SSH` 覆盖，测试与自定义客户端用）
fn ssh_bin() -> String {
    std::env::var("MCSUP_SSH").unwrap_or_else(|_| "ssh".to_owned())
}

/// 远端 mcsup 可执行文件（`MCSUP_REMOTE_BIN` 覆盖）
fn remote_bin() -> String {
    std::env::var("MCSUP_REMOTE_BIN").unwrap_or_else(|_| "mcsup".to_owned())
}

/// 解析 `user@host`：返回 `(user, host)`
///
/// # Errors
///
/// 缺少 `@`、user 或 host 为空时返回用法错误
pub(crate) fn parse_user_host(target: &str, lang: Lang) -> Result<(String, String)> {
    let (user, host) = target.split_once('@').ok_or_else(|| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("SSH 目标必须是 user@host: {target}"),
                format!("ssh target must be user@host: {target}"),
            ),
            None,
        )
    })?;
    if user.is_empty() || host.is_empty() || target.starts_with('-') {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("SSH 目标必须是 user@host: {target}"),
                format!("ssh target must be user@host: {target}"),
            ),
            None,
        )
        .into());
    }
    Ok((user.to_owned(), host.to_owned()))
}

/// 经 SSH 执行远端 agent 的隐藏入口，读回握手信息
///
/// 远端进程保持前台运行；stdout 每行一个 JSON 对象，首行是握手，之后是
/// 配对码轮换。读到首行即返回，子进程继续在后台服务（它的 stdin 由本进程
/// 持有，管道一断远端就收摊）。
///
/// 读 stdout 放在独立线程 + 超时接收：`ssh` 可能停在密码提示上永远不说话，
/// 直接 `read_line` 会把整个命令挂死。超时即杀子进程并报错。
///
/// # Errors
///
/// SSH 不可用、远端没有 mcsup、握手超时或首行非法时返回错误
fn bootstrap(target: &str, lang: Lang) -> Result<(SshHandshake, Child, LineReader)> {
    let mut child = Command::new(ssh_bin())
        // `--` 终止选项解析：目标即使以 `-` 开头也不会被 ssh 当成选项
        .arg("--")
        .arg(target)
        .arg(remote_bin())
        .args(["remote", "__serve-ssh"])
        // 管道而不是 null：这是远端 agent 的寿命开关。本地进程一退出管道
        // 就关，远端读到 EOF 即停——SSH 没有「会话结束」通知，只有流关闭。
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            AppDiagnostic::failure(
                crate::i18n::pick(
                    lang,
                    format!("无法执行 ssh: {err}"),
                    format!("failed to run ssh: {err}"),
                ),
                Some(crate::i18n::pick(
                    lang,
                    "确认本机已安装 OpenSSH 且 user@host 可免交互登录",
                    "make sure OpenSSH is installed and user@host can log in non-interactively",
                )),
            )
        })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("ssh child has no stdout"))?;
    let lines = spawn_line_reader(stdout);
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match lines.recv_timeout(remaining) {
            // 远端进程已退出：把 stderr 带回来，用户才能看到「mcsup: not found」
            Ok(None) => {
                let stderr = take_stderr(&mut child);
                let _ = child.wait();
                return Err(AppDiagnostic::failure(
                    crate::i18n::pick(
                        lang,
                        format!("远端未返回握手信息：{}", stderr.trim()),
                        format!("the remote did not return a handshake: {}", stderr.trim()),
                    ),
                    Some(crate::i18n::pick(
                        lang,
                        "确认远端已安装 mcsup 且在 PATH 中（可用 MCSUP_REMOTE_BIN 指定路径）",
                        "make sure mcsup is installed on the remote and on PATH (or set MCSUP_REMOTE_BIN)",
                    )),
                )
                .into());
            }
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(handshake) = serde_json::from_str::<SshHandshake>(trimmed) {
                    // 握手成功后不再读 stderr：必须排空，否则远端日志写满管道
                    // 会阻塞 agent（长会话里很常见）
                    drain_child_stderr(&mut child);
                    return Ok((handshake, child, lines));
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                return Err(AppDiagnostic::failure(
                    crate::i18n::pick(
                        lang,
                        format!("等待远端握手超时（{HANDSHAKE_TIMEOUT:?}）"),
                        format!(
                            "timed out waiting for the remote handshake ({HANDSHAKE_TIMEOUT:?})"
                        ),
                    ),
                    Some(crate::i18n::pick(
                        lang,
                        "确认 user@host 可免交互登录（不要在配对时弹密码提示）",
                        "make sure user@host can log in without an interactive password prompt",
                    )),
                )
                .into());
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                return Err(anyhow::anyhow!(
                    "{}",
                    crate::i18n::pick(
                        lang,
                        "远端握手通道意外关闭",
                        "the remote handshake channel closed unexpectedly"
                    )
                ));
            }
        }
    }
}

/// 读子进程 stdout 的线程：把每一行交给通道
///
/// 握手那行由调用方取走；之后的行（配对码轮换）没人消费，堆在通道里也无所
/// 谓——线程继续读才是目的。**不能**在读走握手后关掉读端：远端下一次写就是
/// `EPIPE`，它会按「会话已断」收摊，表现为「连上几秒后远端自己停了」。
fn spawn_line_reader(stdout: std::process::ChildStdout) -> LineReader {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    // 接收端可能早已不关心，忽略发送失败
                    let _ = tx.send(None);
                    return;
                }
                Ok(_) => {
                    // 接收端 drop 后 send 失败即返回；此时读端关闭，远端会看到
                    // EOF/EPIPE——所以只在整个进程收尾时才会走到这里
                    if tx.send(Some(line.clone())).is_err() {
                        return;
                    }
                }
            }
        }
    });
    LineReader { rx }
}

/// 引导进程 stdout 的行通道
///
/// 存在的意义不是「取数据」，而是**保持读端打开**：会话里持有它，读线程就
/// 一直把远端写来的配对码轮换行读走（并丢弃）。接收端 drop 时读线程退出、
/// 管道关闭，远端随即收到 `EPIPE`。
struct LineReader {
    /// 只持有、不读取：字段本身没有读取点，读端的存在就是它的作用
    #[allow(dead_code)]
    rx: std::sync::mpsc::Receiver<Option<String>>,
}

impl LineReader {
    /// 等到下一行、远端退出（`Ok(None)`）或超时
    fn recv_timeout(&self, timeout: Duration) -> Result<Option<String>, RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }
}

/// 取子进程 stderr（失败信息里要带上远端自己说的话）
fn take_stderr(child: &mut Child) -> String {
    let mut stderr = String::new();
    if let Some(mut err) = child.stderr.take() {
        use std::io::Read as _;
        let _ = err.read_to_string(&mut stderr);
    }
    stderr
}

/// 后台排空子进程 stderr，避免写满管道把对端阻塞
///
/// 只在「进程要继续跑」的路径调用：失败路径仍同步读取 stderr 用于报错。
fn drain_child_stderr(child: &mut Child) {
    let Some(mut stderr) = child.stderr.take() else {
        return;
    };
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut buf = [0u8; 4096];
        while matches!(stderr.read(&mut buf), Ok(read) if read > 0) {}
    });
}

/// 建一条本地隧道到远端 agent 端口
///
/// 本地端口由系统分配：先绑定 `127.0.0.1:0` 拿到端口再释放，交给 `ssh -L`。
/// 中间存在极小的竞态窗口（端口被别人抢走），`ssh` 会启动失败并报出，用户
/// 重试即可——比固定端口冲突时的体验好得多。
///
/// # Errors
///
/// 无法分配本地端口、ssh 启动失败或隧道在超时内不可连时返回错误
fn open_tunnel(target: &str, remote_addr: &str, lang: Lang) -> Result<(String, Child)> {
    let remote_port = remote_addr
        .rsplit_once(':')
        .map(|(_, port)| port.to_owned())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{}",
                crate::i18n::pick(
                    lang,
                    format!("远端地址缺少端口: {remote_addr}"),
                    format!("remote address has no port: {remote_addr}"),
                )
            )
        })?;

    let local_port = {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|err| {
            anyhow::anyhow!(
                "{}",
                crate::i18n::pick(
                    lang,
                    format!("分配本地隧道端口失败: {err}"),
                    format!("failed to allocate a local tunnel port: {err}"),
                )
            )
        })?;
        listener.local_addr()?.port()
    };
    let local = format!("127.0.0.1:{local_port}");
    let forward = format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}");

    let mut child = Command::new(ssh_bin())
        .args(["-N", "-o", "ExitOnForwardFailure=yes", "-L", &forward])
        .arg("--")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            AppDiagnostic::failure(
                crate::i18n::pick(
                    lang,
                    format!("无法建立 SSH 隧道: {err}"),
                    format!("failed to open the ssh tunnel: {err}"),
                ),
                None,
            )
        })?;

    // 隧道进程要长期存活：stderr 必须排空，否则写满管道会阻塞 ssh
    drain_child_stderr(&mut child);
    if let Err(err) = wait_ready(&local, lang) {
        // 就绪失败不能留下孤儿 ssh 进程：先收掉再返回
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }
    Ok((local, child))
}

/// 轮询本地端口直到可连（`ssh -L` 的就绪没有回调）
fn wait_ready(local: &str, lang: Lang) -> Result<()> {
    let deadline = Instant::now() + TUNNEL_READY_TIMEOUT;
    loop {
        if std::net::TcpStream::connect(local).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(AppDiagnostic::failure(
                crate::i18n::pick(
                    lang,
                    format!("SSH 隧道在 {TUNNEL_READY_TIMEOUT:?} 内未就绪: {local}"),
                    format!(
                        "the ssh tunnel was not ready within {TUNNEL_READY_TIMEOUT:?}: {local}"
                    ),
                ),
                None,
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 取得（或建立）到某台 SSH 远端的会话
///
/// 复用按 `user@host` 判定：同一条命令内的组合仓库、supervisor、备份执行器
/// 共用一条会话与一个远端 agent，不会给每处都起一个。
///
/// # Errors
///
/// SSH 不可用、远端缺 mcsup、握手失败或隧道未就绪时返回错误
pub(crate) fn session(target: &str, lang: Lang) -> Result<(String, SshHandshake)> {
    if let Some((local, handshake)) = lookup(target)? {
        return Ok((local, handshake));
    }

    // 不在持锁状态下起子进程：引导最长要等 HANDSHAKE_TIMEOUT，把登记表锁住
    // 会让同一命令里其他远端串行排队。
    let (handshake, bootstrap, lines) = bootstrap(target, lang)?;
    let (local, tunnel) = match open_tunnel(target, &handshake.addr, lang) {
        Ok(pair) => pair,
        Err(err) => {
            // 隧道建不起来，引导进程必须一并收掉，否则远端留一个没人用的 agent
            let mut bootstrap = bootstrap;
            let _ = bootstrap.kill();
            let _ = bootstrap.wait();
            return Err(err);
        }
    };
    let session = SshSession {
        local: local.clone(),
        handshake: handshake.clone(),
        bootstrap,
        tunnel,
        _lines: lines,
    };
    let mut guard = sessions()
        .lock()
        .map_err(|_| anyhow::anyhow!("ssh session registry is poisoned"))?;
    // 建会话期间没有持锁，可能有人先登记了同一个目标：复用它，丢弃本次
    if let Some((existing_local, existing_handshake)) = guard
        .iter()
        .find(|(key, _)| key == target)
        .map(|(_, session)| (session.local.clone(), session.handshake.clone()))
    {
        drop(guard);
        // Drop 会 kill 本次多起的引导进程 / 隧道
        drop(session);
        return Ok((existing_local, existing_handshake));
    }
    guard.push((target.to_owned(), session));
    drop(guard);
    Ok((local, handshake))
}

/// 查登记表；没有就返回 `None`
fn lookup(target: &str) -> Result<Option<(String, SshHandshake)>> {
    let guard = sessions()
        .lock()
        .map_err(|_| anyhow::anyhow!("ssh session registry is poisoned"))?;
    Ok(guard
        .iter()
        .find(|(key, _)| key == target)
        .map(|(_, session)| (session.local.clone(), session.handshake.clone())))
}

/// 一条远端记录的**实际可连地址**
///
/// 直连接入的地址就是记录里的 `target`；SSH 接入要先建会话，地址是本次
/// 隧道的本地回环端口。所有「拿地址去连」的地方都必须经过这里，否则 SSH
/// 记录会拿着 `user@host` 去当 TCP 地址用。
///
/// # Errors
///
/// 记录声明了 SSH 接入但会话建立失败时返回错误
pub(crate) fn endpoint(record: &RemoteRecord, lang: Lang) -> Result<String> {
    match record.via {
        RemoteVia::Direct => Ok(record.target.clone()),
        RemoteVia::Ssh => Ok(session(&record.target, lang)?.0),
    }
}

/// [`endpoint`] 的纯地址口径：供装配层构造延迟解析的连接
///
/// 装配期不建隧道（本地命令不该为远端付连接代价），第一次真正发帧时才调
/// 到这里。装配层没有 `Printer`，错误按英文构造——它最终会包进传输层错误，
/// 那一层本来就是英文。
///
/// # Errors
///
/// 会话建立失败时返回错误
pub(crate) fn endpoint_for(target: &str) -> Result<String> {
    session(target, Lang::En).map(|(local, _)| local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_host_is_required() {
        assert_eq!(
            parse_user_host("me@10.0.0.5", Lang::Zh).expect("ok"),
            ("me".to_owned(), "10.0.0.5".to_owned())
        );
        for bad in ["10.0.0.5", "@host", "user@", "user"] {
            let err = parse_user_host(bad, Lang::Zh).expect_err(bad);
            assert_eq!(
                err.downcast_ref::<AppDiagnostic>().map(|d| d.exit_code),
                Some(2),
                "{bad}"
            );
        }
    }

    #[test]
    fn remote_port_is_extracted_from_the_handshake_address() {
        // 与 open_tunnel 同一套切分逻辑：只取端口，不关心 host
        let addr = "127.0.0.1:54321";
        let (_, port) = addr.rsplit_once(':').expect("split");
        assert_eq!(port, "54321");
    }

    fn record(via: RemoteVia, target: &str) -> RemoteRecord {
        RemoteRecord {
            target: target.to_owned(),
            via,
            read_only: false,
            added_at: 1,
            fingerprint: None,
            credential: None,
        }
    }

    /// 直连记录的可连地址就是记录里的 target，不碰任何子进程
    #[test]
    fn direct_record_endpoint_is_the_record_target() {
        let rec = record(RemoteVia::Direct, "10.0.0.5:7681");
        assert_eq!(
            endpoint(&rec, Lang::Zh).expect("direct endpoint"),
            "10.0.0.5:7681"
        );
    }
}
