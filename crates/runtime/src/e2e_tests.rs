//! supervisor 端到端：真实子进程 + 真实本地 socket
//!
//! 只在 Unix 上跑（依赖 `/bin/sh` 与路径型 socket）。每个用例一个临时
//! 服务器目录，走 `run_with` + 真实 IPC 客户端，覆盖就绪判定、优雅 / 强制
//! 停机、崩溃映射、控制台注入与 shutdown 语义。
#![cfg(unix)]

use crate::client;
use crate::supervisor::run_with;
use domain::server::layout::console_log_path;
use port::SupervisorStatus;
use protocol::{Request, Response, StopResult};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// 轮询直到状态满足条件；超时返回 `None`
fn poll_status(
    dir: &Path,
    until: impl Fn(&SupervisorStatus) -> bool,
    secs: u64,
) -> Option<SupervisorStatus> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Ok(Some(status)) = client::probe_status(dir)
            && until(&status)
        {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// 启动一个用 `/bin/sh` 脚本充当服务端的 supervisor
fn start(dir: &Path, script: &str) -> thread::JoinHandle<i32> {
    let script = script.to_owned();
    let dir = dir.to_path_buf();
    thread::spawn(move || {
        run_with(&dir, move || {
            Command::new("/bin/sh")
                .args(["-c", &script])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        })
    })
}

/// 等 supervisor 就绪
fn wait_ready(dir: &Path) {
    client::wait_up(dir, Duration::from_secs(5)).expect("supervisor ready");
}

/// 等进入终态
fn wait_terminal(dir: &Path) -> SupervisorStatus {
    poll_status(dir, |status| status.status.is_terminal(), 5).expect("terminal status")
}

/// 请求 shutdown 并等 supervisor 退出
fn shutdown_and_join(dir: &Path, handle: thread::JoinHandle<i32>) -> i32 {
    client::rpc(dir, Request::Shutdown, Duration::from_secs(3)).expect("shutdown rpc");
    handle.join().expect("supervisor thread")
}

/// 读 console.log 全文
fn read_log(dir: &Path) -> String {
    std::fs::read_to_string(console_log_path(dir)).unwrap_or_default()
}

#[test]
fn running_then_graceful_stop_via_stdin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let handle = start(dir, "echo 'Done (0.1s)!'; read line");
    wait_ready(dir);

    let status = poll_status(dir, |s| s.status == port::ServerStatus::Running, 5)
        .expect("running after Done line");
    assert!(status.pid.is_some());
    assert!(status.uptime.is_some());

    let response = client::rpc(
        dir,
        Request::Stop {
            grace_secs: 5,
            force: false,
        },
        Duration::from_secs(10),
    )
    .expect("stop rpc");
    match response {
        Response::Stop {
            result,
            elapsed_secs,
        } => {
            assert_eq!(result, StopResult::STOPPED);
            assert!(
                elapsed_secs < 5,
                "stdin stop should be fast, took {elapsed_secs}s"
            );
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let status = wait_terminal(dir);
    assert_eq!(status.status, port::ServerStatus::Stopped);
    assert_eq!(status.exit_code, Some(0));
    assert_eq!(shutdown_and_join(dir, handle), 0);
}

#[test]
fn timeout_without_force_then_force_stops() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let handle = start(dir, "echo 'Done (0.1s)!'; sleep 60");
    wait_ready(dir);
    poll_status(dir, |s| s.status == port::ServerStatus::Running, 5).expect("running");

    // 不强杀：宽限到期回 timed_out，状态保持 Stopping
    let response = client::rpc(
        dir,
        Request::Stop {
            grace_secs: 1,
            force: false,
        },
        Duration::from_secs(10),
    )
    .expect("stop rpc");
    assert!(
        matches!(
            response,
            Response::Stop { ref result, .. } if result == StopResult::TIMED_OUT
        ),
        "expected timed_out, got {response:?}"
    );
    let status =
        poll_status(dir, |s| s.status == port::ServerStatus::Stopping, 2).expect("still stopping");
    assert!(status.status.is_alive());

    // 强杀：宽限到期 kill → Stopped
    let response = client::rpc(
        dir,
        Request::Stop {
            grace_secs: 1,
            force: true,
        },
        Duration::from_secs(15),
    )
    .expect("force stop rpc");
    assert!(
        matches!(
            response,
            Response::Stop { ref result, .. } if result == StopResult::STOPPED
        ),
        "expected stopped, got {response:?}"
    );
    let status = wait_terminal(dir);
    assert_eq!(status.status, port::ServerStatus::Stopped);
    assert_eq!(shutdown_and_join(dir, handle), 0);
}

#[test]
fn nonzero_exit_maps_to_crashed_and_exit_code_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let handle = start(dir, "echo 'Done (0.1s)!'; exit 3");
    wait_ready(dir);

    let status = wait_terminal(dir);
    assert_eq!(status.status, port::ServerStatus::Crashed);
    assert_eq!(status.exit_code, Some(3));
    assert_eq!(shutdown_and_join(dir, handle), 1);
}

#[test]
fn start_before_done_reports_starting() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let handle = start(dir, "sleep 60");
    wait_ready(dir);

    let status = poll_status(dir, |s| s.status == port::ServerStatus::Starting, 5)
        .expect("starting without Done line");
    assert!(status.pid.is_some());

    // 清理：强杀 + shutdown
    client::rpc(
        dir,
        Request::Stop {
            grace_secs: 1,
            force: true,
        },
        Duration::from_secs(15),
    )
    .expect("force stop");
    wait_terminal(dir);
    assert_eq!(shutdown_and_join(dir, handle), 0);
}

#[test]
fn send_line_reaches_child_stdout_in_console_log() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let handle = start(dir, "echo 'Done (0.1s)!'; read line; echo \"GOT:$line\"");
    wait_ready(dir);
    poll_status(dir, |s| s.status == port::ServerStatus::Running, 5).expect("running");

    client::rpc(
        dir,
        Request::Send {
            line: "hello".to_owned(),
        },
        Duration::from_secs(5),
    )
    .expect("send rpc");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !read_log(dir).contains("GOT:hello") {
        assert!(Instant::now() < deadline, "console.log never saw the echo");
        thread::sleep(Duration::from_millis(50));
    }

    wait_terminal(dir);
    assert_eq!(shutdown_and_join(dir, handle), 0);
}

/// 非 UTF-8 输出必须按原始字节落盘
///
/// 服务端在 Windows 下按系统代码页（GBK）输出中文；若 `pump` 先用
/// `from_utf8_lossy` 解码，GBK 字节会被替换成 U+FFFD 写入 `console.log`，
/// 日志永久损坏。这里让子进程吐 GBK 字节，断言日志里是原字节、没有替换符。
#[test]
fn console_log_keeps_non_utf8_bytes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    // GBK 的「正在」= 0xD5 0xFD 0xD4 0xDA，printf 的 \x 转义按字节输出
    let handle = start(
        dir,
        "echo 'Done (0.1s)!'; printf '\\325\\375\\324\\332\\n'; read line",
    );
    wait_ready(dir);
    poll_status(dir, |s| s.status == port::ServerStatus::Running, 5).expect("running");

    let deadline = Instant::now() + Duration::from_secs(5);
    let gbk = [0xD5u8, 0xFD, 0xD4, 0xDA];
    loop {
        let bytes = std::fs::read(console_log_path(dir)).unwrap_or_default();
        if bytes.windows(gbk.len()).any(|w| w == gbk) {
            break;
        }
        assert!(Instant::now() < deadline, "console.log 未保留原始 GBK 字节");
        thread::sleep(Duration::from_millis(50));
    }

    let bytes = std::fs::read(console_log_path(dir)).expect("read console.log");
    assert!(
        !bytes.windows(3).any(|w| w == [0xEF, 0xBB, 0xBF]),
        "日志不应出现 U+FFFD 替换符"
    );

    // 脚本的 `read line` 会一直等 stdin，进程不会自己退出：显式停机后
    // 才能进终态。少了这一步 `wait_terminal` 必然超时。
    client::rpc(
        dir,
        Request::Stop {
            grace_secs: 5,
            force: false,
        },
        Duration::from_secs(10),
    )
    .expect("stop rpc");
    wait_terminal(dir);
    assert_eq!(shutdown_and_join(dir, handle), 0);
}

#[test]
fn send_after_exit_reports_not_running() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let handle = start(dir, "echo 'Done (0.1s)!'; exit 0");
    wait_ready(dir);
    wait_terminal(dir);

    let response = client::rpc(
        dir,
        Request::Send {
            line: "nope".to_owned(),
        },
        Duration::from_secs(5),
    )
    .expect("send rpc");
    match response {
        Response::Error { kind, .. } => assert_eq!(kind, "not_running"),
        other => panic!("expected error, got {other:?}"),
    }
    assert_eq!(shutdown_and_join(dir, handle), 0);
}

#[test]
fn shutdown_refused_while_running_then_allowed_after_stop() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let handle = start(dir, "echo 'Done (0.1s)!'; sleep 60");
    wait_ready(dir);
    poll_status(dir, |s| s.status == port::ServerStatus::Running, 5).expect("running");

    let response =
        client::rpc(dir, Request::Shutdown, Duration::from_secs(3)).expect("shutdown rpc");
    assert!(
        matches!(response, Response::Error { ref kind, .. } if kind == "busy"),
        "expected busy, got {response:?}"
    );

    client::rpc(
        dir,
        Request::Stop {
            grace_secs: 1,
            force: true,
        },
        Duration::from_secs(15),
    )
    .expect("force stop");
    wait_terminal(dir);
    assert_eq!(shutdown_and_join(dir, handle), 0);
}

#[test]
fn protocol_version_mismatch_gets_protocol_error_response() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let handle = start(dir, "echo 'Done (0.1s)!'; sleep 60");
    wait_ready(dir);

    // 手工发一个版本错误的帧
    {
        use interprocess::local_socket::prelude::*;
        use std::io::BufRead as _;
        use std::io::Write as _;
        let mut stream = interprocess::local_socket::Stream::connect(
            crate::endpoint::endpoint_name(dir).expect("endpoint"),
        )
        .expect("connect");
        stream
            .write_all(b"{\"v\":99,\"type\":\"status\"}\n")
            .expect("write");
        let mut reader = std::io::BufReader::new(&mut stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let json: serde_json::Value = serde_json::from_str(line.trim_end()).expect("json");
        assert_eq!(json["type"], "error");
        assert_eq!(json["kind"], "protocol");
    }

    client::rpc(
        dir,
        Request::Stop {
            grace_secs: 1,
            force: true,
        },
        Duration::from_secs(15),
    )
    .expect("force stop");
    wait_terminal(dir);
    assert_eq!(shutdown_and_join(dir, handle), 0);
}
