//! Windows 上的 supervisor IPC：真实命名管道 + 真实子进程
//!
//! Unix 那套端到端用例靠 `/bin/sh` 与路径型 socket，`e2e_tests` 整体
//! `cfg(unix)`，于是 Windows 的命名管道链路一直没人走过。这里补上最小闭环：
//! 只要能连上并拿到状态帧，就说明客户端与服务端两侧的管道用法都成立。
//!
//! 回归背景：Windows 命名管道不支持 `set_recv_timeout`，客户端曾把
//! `Unsupported` 当成连接失败，导致**所有** supervisor 调用报 unreachable，
//! `server start` 永远起不来。
#![cfg(windows)]

use crate::client;
use crate::supervisor::run_with;
use protocol::Request;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

/// 长跑但什么都不做的子进程：`ping` 在 Windows 上一定有
///
/// 不用 `-n 60`：`cmd` 被杀掉后 `ping` 仍占着继承来的 stdout，supervisor
/// 的收尾会一直等到它自己结束——这属于「用 cmd 包一层」的固有代价，与被测
/// 的 IPC 无关，把次数压到 10 次即可覆盖。
fn spawn_idle() -> std::io::Result<Child> {
    Command::new("cmd")
        .args(["/C", "ping -n 10 127.0.0.1 > nul"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

#[test]
fn supervisor_answers_over_named_pipe() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();

    let handle = {
        let dir = dir.clone();
        thread::spawn(move || run_with(&dir, spawn_idle))
    };

    // 连上并拿到状态帧：这一步在修复前必定失败
    let ready = client::wait_up(&dir, Duration::from_secs(20));
    let status = client::probe_status(&dir);
    let shutdown = client::rpc(&dir, Request::Shutdown, Duration::from_secs(5));

    assert!(
        ready.is_ok(),
        "supervisor 必须能在命名管道上应答: {ready:?}"
    );
    let status = status
        .expect("probe 不该报错")
        .expect("supervisor 应当存在");
    assert!(status.status.is_alive(), "{status:?}");
    shutdown.expect("shutdown 必须被受理");

    let code = handle.join().expect("supervisor thread");
    assert_eq!(code, 0, "shutdown 后 supervisor 正常退出");
}

/// 端点无人监听时是「没有 supervisor」，不是错误
///
/// `probe_status` 把这个区别当成公开语义（未运行 vs 协议故障），命名管道
/// 上同样要成立：连不上时返回 `Ok(None)`。
#[test]
fn missing_supervisor_is_reported_as_not_running() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir: PathBuf = tmp.path().to_path_buf();
    let status = client::probe_status(&dir).expect("连不上不是错误");
    assert!(status.is_none(), "{status:?}");
}
