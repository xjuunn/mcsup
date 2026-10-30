//! 客户端侧 IPC：一问一答 + 就绪轮询
use crate::endpoint;
use interprocess::local_socket::prelude::*;
use port::{SupervisorError, SupervisorStatus};
use protocol::{self, Request, Response};
use std::io::BufReader;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// 发一个请求并等待响应；整体受 `timeout` 约束（在独立线程里收发）
///
/// # Errors
///
/// 连接失败 / 超时返回 [`SupervisorError::Unreachable`]，帧或版本错误返回
/// [`SupervisorError::Protocol`]
pub(crate) fn rpc(
    server_dir: &Path,
    request: Request,
    timeout: Duration,
) -> Result<Response, SupervisorError> {
    let dir = server_dir.to_path_buf();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| -> Result<Response, SupervisorError> {
            let mut stream =
                interprocess::local_socket::Stream::connect(endpoint::endpoint_name(&dir)?)
                    .map_err(|err| SupervisorError::Unreachable(err.to_string()))?;
            protocol::write_frame(&mut stream, &request).map_err(|err| transport(&err))?;
            let response: Response = protocol::read_frame(&mut BufReader::new(stream))
                .map_err(|err| frame_error(&err))?;
            Ok(response)
        })();
        let _ = tx.send(result);
    });
    rx.recv_timeout(timeout)
        .map_err(|_| SupervisorError::Unreachable("request timed out".to_owned()))?
}

/// 传输层 IO 错误
fn transport(err: &std::io::Error) -> SupervisorError {
    SupervisorError::Unreachable(err.to_string())
}

/// 帧非法 = 协议问题，其余 = 连接问题
fn frame_error(err: &std::io::Error) -> SupervisorError {
    if err.kind() == std::io::ErrorKind::InvalidData {
        SupervisorError::Protocol(err.to_string())
    } else {
        transport(err)
    }
}

/// 状态查询：连不上视为「没有 supervisor」（未运行），协议问题才报错
///
/// # Errors
///
/// 协议失败时返回错误
pub(crate) fn probe_status(server_dir: &Path) -> Result<Option<SupervisorStatus>, SupervisorError> {
    let response = match rpc(server_dir, Request::Status, Duration::from_secs(3)) {
        Ok(response) => response,
        Err(SupervisorError::Unreachable(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    match response {
        Response::Status {
            status,
            pid,
            uptime_secs,
            exit_code,
        } => {
            let status = protocol::status_from_wire(&status).ok_or_else(|| {
                SupervisorError::Protocol(format!("unknown status text: {status}"))
            })?;
            Ok(Some(SupervisorStatus {
                status,
                pid,
                uptime: uptime_secs.map(Duration::from_secs),
                exit_code,
            }))
        }
        Response::Error { kind, message } => Err(SupervisorError::Remote {
            kind: port::RemoteFailureKind::from_wire(&kind),
            message,
        }),
        other => Err(SupervisorError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// 轮询直到 supervisor 可应答（拉起后的握手）
///
/// # Errors
///
/// 截止时间内始终不可达时返回 [`SupervisorError::Unreachable`]
pub(crate) fn wait_up(server_dir: &Path, total: Duration) -> Result<(), SupervisorError> {
    let deadline = std::time::Instant::now() + total;
    loop {
        match rpc(server_dir, Request::Status, Duration::from_secs(2)) {
            Ok(_) => return Ok(()),
            Err(err) => {
                if std::time::Instant::now() >= deadline {
                    return Err(err);
                }
                thread::sleep(Duration::from_millis(150));
            }
        }
    }
}

/// 等待空载 supervisor 释放端点（重启前的清场）
pub(crate) fn wait_gone(server_dir: &Path, total: Duration) {
    let deadline = std::time::Instant::now() + total;
    while std::time::Instant::now() < deadline {
        match probe_status(server_dir) {
            Ok(None) => return,
            Ok(Some(status)) if status.status.is_terminal() => {
                // 还在留守窗口：通知退出并继续等
                let _ = rpc(server_dir, Request::Shutdown, Duration::from_secs(1));
            }
            _ => {}
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// 把 [`Response`] 里的错误信封展开成 `Err`（`Ok` 变体直接返回）
pub(crate) fn unwrap_ok(response: Response) -> Result<Response, SupervisorError> {
    match response {
        Response::Error { kind, message } => Err(SupervisorError::Remote {
            kind: port::RemoteFailureKind::from_wire(&kind),
            message,
        }),
        other => Ok(other),
    }
}
