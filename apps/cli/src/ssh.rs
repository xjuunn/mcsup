//! SSH 引导与本地隧道：复用已有 SSH 信任，不新增凭证
//!
//! `remote connect user@host` 的两步都在这里：
//!
//! 1. 经 SSH 执行远端的隐藏入口 `mcsup remote __serve-ssh`，从 stdout 读回
//!    agent 的地址、证书指纹与配对码；
//! 2. 起一条 `ssh -N -L` 隧道把本地回环端口转到远端的 agent 端口，之后的
//!    RPC 全部走 `127.0.0.1:<local>`。
//!
//! 隧道进程与 CLI 进程同寿命：命令结束即断开，符合「按需连接、无常驻会话」。
//! 长期运行的命令（`server logs --follow`）在同一进程内复用隧道，进程退出
//! 时由操作系统回收子进程。
//!
//! 为什么用 SSH 而不是在远端开公网端口：SSH 的信任已经存在，用户不需要
//! 额外分发 token、也不需要把 agent 暴露到网络上。证书指纹仍然是 TOFU 的
//! 信任锚——SSH 只负责「能连上」，不能替代「连的是哪张证书」。

use crate::diagnostic::AppDiagnostic;
use crate::locale::Lang;
use anyhow::{Context, Result};
use mcsup::remote::SshHandshake;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 握手读取上限：远端启动 agent 通常在一秒内；超过这个时间按启动失败处理
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// 隧道建立后等待端口可连的上限
const TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// 一条已建立的 SSH 隧道
pub(crate) struct Tunnel {
    /// 本地回环地址（`127.0.0.1:<port>`）
    pub local: String,
    child: Child,
}

impl std::fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tunnel")
            .field("local", &self.local)
            .finish_non_exhaustive()
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 隧道登记表：同一进程内按 `user@host` 复用，避免每条命令重复建连
#[allow(dead_code)] // 隧道复用待 reconnect / list 接入后启用
fn tunnels() -> &'static Mutex<Vec<(String, Tunnel)>> {
    static TUNNELS: OnceLock<Mutex<Vec<(String, Tunnel)>>> = OnceLock::new();
    TUNNELS.get_or_init(|| Mutex::new(Vec::new()))
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
    if user.is_empty() || host.is_empty() {
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
/// 配对码轮换。读到首行即返回，子进程继续在后台服务（隧道建立后由 SSH
/// 会话承载）。
///
/// # Errors
///
/// SSH 不可用、远端没有 mcsup、握手超时或首行非法时返回错误
pub(crate) fn bootstrap(target: &str, lang: Lang) -> Result<(SshHandshake, Child)> {
    let mut child = Command::new(ssh_bin())
        .arg(target)
        .arg(remote_bin())
        .args(["remote", "__serve-ssh"])
        .stdin(Stdio::null())
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
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        line.clear();
        let read = reader.read_line(&mut line).with_context(|| {
            crate::i18n::pick(
                lang,
                "读取远端握手失败",
                "failed to read the remote handshake",
            )
        })?;
        if read == 0 {
            // 远端进程已退出：把 stderr 带回来，用户才能看到「mcsup: not found」
            let mut stderr = String::new();
            if let Some(mut err) = child.stderr.take() {
                use std::io::Read as _;
                let _ = err.read_to_string(&mut stderr);
            }
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
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(handshake) = serde_json::from_str::<SshHandshake>(trimmed) {
            return Ok((handshake, child));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err(AppDiagnostic::failure(
                crate::i18n::pick(
                    lang,
                    "等待远端握手超时",
                    "timed out waiting for the remote handshake",
                ),
                None,
            )
            .into());
        }
    }
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
pub(crate) fn open_tunnel(target: &str, remote_addr: &str, lang: Lang) -> Result<Tunnel> {
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

    let child = Command::new(ssh_bin())
        .args(["-N", "-o", "ExitOnForwardFailure=yes", "-L", &forward])
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

    let tunnel = Tunnel { local, child };
    wait_ready(&tunnel.local, lang)?;
    Ok(tunnel)
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

/// 取得（或复用）到某台 SSH 远端的本地隧道地址
///
/// 装配层（`bootstrap::build_member`）在 `via = ssh` 时调用：隧道按
/// `user@host` 复用，同一进程内的多条命令共享一条。
///
/// # Errors
///
/// 隧道建立失败时返回错误
#[allow(dead_code)] // 装配层（via=ssh）接入后启用
pub(crate) fn tunnel_for(target: &str, remote_addr: &str, lang: Lang) -> Result<String> {
    let mut guard = tunnels()
        .lock()
        .map_err(|_| anyhow::anyhow!("ssh tunnel registry is poisoned"))?;
    if let Some((_, tunnel)) = guard.iter().find(|(key, _)| key == target) {
        return Ok(tunnel.local.clone());
    }
    let tunnel = open_tunnel(target, remote_addr, lang)?;
    let local = tunnel.local.clone();
    guard.push((target.to_owned(), tunnel));
    Ok(local)
}

/// 写一行到子进程 stdin（当前未用；保留给未来的交互式 SSH 会话）
#[allow(dead_code)]
fn send_line(child: &mut Child, line: &str) -> Result<()> {
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(line.as_bytes())?;
        stdin.flush()?;
    }
    Ok(())
}

/// 本地可执行文件的绝对路径（测试与提示用）
#[allow(dead_code)]
pub(crate) fn local_bin() -> Result<PathBuf> {
    std::env::current_exe().context("resolve the current executable")
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
}
