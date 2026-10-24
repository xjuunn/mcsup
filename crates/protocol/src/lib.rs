//! supervisor IPC 与未来 RPC 消息的契约类型：版本化 JSON-line 帧
//!
//! 这是「命令 + 结果」的唯一真相来源：本地 supervisor 的 socket IPC、
//! 未来的 REST / WebSocket / Remote transport 都从这里取类型，禁止各入口
//! 自行定义 DTO。破坏性变更走新增字段 + [`PROTOCOL_VERSION`] 递增，不
//! 猜测兼容。
//!
//! 不做自定义打包（interprocess / HTTP 传的就是裸字节），行分帧由本模块
//! 负责。
use domain::server::status::ServerStatus;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::{self, BufRead, Write};

/// CLI `--output json` 的行类型（唯一真相来源，禁止各入口自行定义 DTO）
pub mod cli;

/// IPC 协议版本；不匹配直接报错，禁止猜测兼容
pub const PROTOCOL_VERSION: u32 = 1;

/// 请求：一次连接一个请求，一问一答后关闭
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// 查询实时状态
    Status,
    /// 请求停机
    Stop {
        /// 优雅停机宽限秒数
        grace_secs: u64,
        /// 宽限到期是否强杀
        force: bool,
    },
    /// 控制台注入一行
    Send { line: String },
    /// 通知空载 supervisor 退出
    Shutdown,
}

/// 响应
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// 状态查询结果（`status` 为 [`ServerStatus::as_str`] 口径）
    Status {
        /// 生命周期状态（小写文本）
        status: String,
        /// 服务端进程 PID（未运行时 `None`）
        pid: Option<u32>,
        /// 已运行秒数（未运行时 `None`）
        uptime_secs: Option<u64>,
        /// 退出码（已退出时 `Some`）
        exit_code: Option<i32>,
    },
    /// 停机结果（`result` 见 [`StopResult`] 的线格式）
    Stop {
        /// 停机结果线格式
        result: String,
        /// 停机耗时秒数
        elapsed_secs: u64,
    },
    /// 通用成功
    Ok,
    /// 业务 / 协议错误
    Error {
        /// 错误种类（如 `protocol` / `io` / `busy` / `not_running`）
        kind: String,
        /// 人类可读消息
        message: String,
    },
}

/// [`Response::Stop::result`] 线格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopResult;

impl StopResult {
    /// 优雅停机成功
    pub const STOPPED: &'static str = "stopped";
    /// 本就未在运行（幂等）
    pub const ALREADY_STOPPED: &'static str = "already_stopped";
    /// 宽限到期仍未退出
    pub const TIMED_OUT: &'static str = "timed_out";
}

/// 带版本字段的帧：`{"v":1,"type":...,...}`
#[derive(Debug, Serialize, serde::Deserialize)]
pub struct Frame<T> {
    /// 协议版本
    pub v: u32,
    /// 帧体（请求或响应）
    #[serde(flatten)]
    pub body: T,
}

/// 写出一帧（JSON + 换行 + flush）
///
/// # Errors
///
/// 序列化或写入失败时返回底层 IO 错误
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, body: &T) -> io::Result<()> {
    let frame = Frame {
        v: PROTOCOL_VERSION,
        body,
    };
    serde_json::to_writer(&mut *w, &frame)?;
    w.write_all(b"\n")?;
    w.flush()
}

/// 读入一帧；版本不匹配或 JSON 非法按 [`io::ErrorKind::InvalidData`] 返回
///
/// # Errors
///
/// 读取失败、帧非法或协议版本不匹配时返回错误
pub fn read_frame<R: BufRead, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut line = String::new();
    let read = r.read_line(&mut line)?;
    if read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed before a frame arrived",
        ));
    }
    let frame: Frame<T> = serde_json::from_str(line.trim_end())
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if frame.v != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "protocol version mismatch: peer {peer}, local {local}",
                peer = frame.v,
                local = PROTOCOL_VERSION
            ),
        ));
    }
    Ok(frame.body)
}

/// 线格式 → 领域状态
#[must_use]
pub fn status_from_wire(text: &str) -> Option<ServerStatus> {
    match text {
        "stopped" => Some(ServerStatus::Stopped),
        "starting" => Some(ServerStatus::Starting),
        "running" => Some(ServerStatus::Running),
        "stopping" => Some(ServerStatus::Stopping),
        "crashed" => Some(ServerStatus::Crashed),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn request_frames_roundtrip_with_version() {
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &Request::Stop {
                grace_secs: 7,
                force: true,
            },
        )
        .expect("write");
        let mut cursor = Cursor::new(buf);
        let req: Request = read_frame(&mut cursor).expect("read");
        assert!(matches!(
            req,
            Request::Stop {
                grace_secs: 7,
                force: true
            }
        ));
    }

    #[test]
    fn response_frames_roundtrip() {
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &Response::Status {
                status: "running".to_owned(),
                pid: Some(42),
                uptime_secs: Some(9),
                exit_code: None,
            },
        )
        .expect("write");
        let mut cursor = Cursor::new(buf);
        let resp: Response = read_frame(&mut cursor).expect("read");
        match resp {
            Response::Status {
                status,
                pid,
                uptime_secs,
                exit_code,
            } => {
                assert_eq!(status, "running");
                assert_eq!(pid, Some(42));
                assert_eq!(uptime_secs, Some(9));
                assert_eq!(exit_code, None);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn version_mismatch_and_garbage_are_invalid_data() {
        let mut cursor = Cursor::new(b"{\"v\":99,\"type\":\"ok\"}\n".to_vec());
        let err = read_frame::<_, Response>(&mut cursor).expect_err("v99");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let mut cursor = Cursor::new(b"not json\n".to_vec());
        let err = read_frame::<_, Response>(&mut cursor).expect_err("garbage");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let mut cursor = Cursor::new(Vec::new());
        let err = read_frame::<_, Response>(&mut cursor).expect_err("eof");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn wire_status_maps_both_directions() {
        for status in [
            ServerStatus::Stopped,
            ServerStatus::Starting,
            ServerStatus::Running,
            ServerStatus::Stopping,
            ServerStatus::Crashed,
        ] {
            assert_eq!(status_from_wire(status.as_str()), Some(status));
        }
        assert_eq!(status_from_wire("banana"), None);
    }
}
