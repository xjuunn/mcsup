//! 集成测试用的最小插件进程
//!
//! 只看协议，不碰平台数据：完成 `hello` / `welcome`，回应 `call`、`ping`，
//! 收到 `shutdown` 或 `reject` 后退出。宿主侧的 `SpawnPlan` 会把令牌、id 与
//! 作用域放到环境变量里，把端点放到 `--endpoint`。
//!
//! 这个 bin 只在 `fixture` feature 下构建；集成测试通过
//! `CARGO_BIN_EXE_plugin-fixture` 找到它，复制进伪产物目录后拉起。

use domain::plugin::error::PluginErrorCode;
use protocol::plugin::{
    Call, Frame, Hello, ResultFrame, StreamFrame, WireError, read_frame, write_frame,
};
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(unix)]
enum Stream {
    Tcp(TcpStream),
    Unix(UnixStream),
}

#[cfg(not(unix))]
enum Stream {
    Tcp(TcpStream),
}

impl Stream {
    fn connect(address: &str) -> io::Result<Self> {
        if let Some(rest) = address.strip_prefix("tcp://") {
            return Ok(Self::Tcp(TcpStream::connect(rest)?));
        }
        #[cfg(unix)]
        {
            Ok(Self::Unix(UnixStream::connect(address)?))
        }
        #[cfg(not(unix))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported endpoint: {address}"),
            ))
        }
    }

    fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Tcp(stream) => Ok(Self::Tcp(stream.try_clone()?)),
            #[cfg(unix)]
            Self::Unix(stream) => Ok(Self::Unix(stream.try_clone()?)),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.flush(),
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
        }
    }
}

// 测试 fixture：stderr 由宿主采集进插件日志，是唯一可观测输出通道
#[allow(clippy::print_stderr)]
fn main() -> io::Result<()> {
    let endpoint = parse_endpoint()?;
    let token = env_or("MCSUP_PLUGIN_TOKEN", "");
    let plugin_id = env_or("MCSUP_PLUGIN_ID", "fixture");
    let scope = env_or("MCSUP_PLUGIN_SCOPE", "machine");
    let stream = Stream::connect(&endpoint)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let hello = Hello {
        token,
        api: domain::plugin::PLUGIN_API_VERSION,
        plugin_id,
        plugin_version: env!("CARGO_PKG_VERSION").to_owned(),
        platform: plugin::process::platform_key(),
        pid: std::process::id(),
        requested: Vec::new(),
        subscriptions: Vec::new(),
    };
    let _ = scope;
    write_frame(&mut writer, &Frame::Hello(hello)).map_err(frame_io)?;
    eprintln!(
        "fixture ready: {}@{}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    let mut stream_seq = 0_u64;
    let mut config_revision = 0_u64;
    loop {
        let frame = match read_frame(&mut reader) {
            Ok(frame) => frame,
            Err(protocol::plugin::FrameError::UnexpectedEof) => return Ok(()),
            Err(err) => return Err(frame_io(err)),
        };
        match frame {
            Frame::Call(call) => {
                stream_seq = handle_call(&mut writer, &call, stream_seq, config_revision)?;
            }
            Frame::Ping(seq) => write_frame(&mut writer, &Frame::Pong(seq)).map_err(frame_io)?,
            Frame::Shutdown { .. } | Frame::Reject(_) => return Ok(()),
            Frame::Welcome(welcome) => config_revision = welcome.config.revision,
            Frame::Config(config) => {
                config_revision = config.revision;
                let result = ResultFrame::ok(
                    config.id,
                    serde_json::json!({ "revision": config.revision }),
                );
                write_frame(&mut writer, &Frame::Result(result)).map_err(frame_io)?;
            }
            Frame::Unknown { .. }
            | Frame::Hello(_)
            | Frame::Event(_)
            | Frame::Result(_)
            | Frame::Stream(_)
            | Frame::Cancel(_)
            | Frame::Pong(_) => {}
        }
    }
}

fn handle_call(
    writer: &mut Stream,
    call: &Call,
    stream_seq: u64,
    config_revision: u64,
) -> io::Result<u64> {
    let op = call.op.as_deref().unwrap_or_default();
    match op {
        "fail" => {
            let error =
                WireError::from_code(PluginErrorCode::InvalidInput, "fixture refused the op");
            let result = ResultFrame::error(call.id, error);
            write_frame(writer, &Frame::Result(result)).map_err(frame_io)?;
            Ok(stream_seq)
        }
        "stream" => {
            let mut seq = stream_seq;
            for index in 0..3_u64 {
                let frame = StreamFrame {
                    id: call.id,
                    seq,
                    chunk: serde_json::json!({ "index": index }),
                    done: index == 2,
                };
                write_frame(writer, &Frame::Stream(frame)).map_err(frame_io)?;
                seq += 1;
            }
            Ok(seq)
        }
        _ => {
            let result = ResultFrame::ok(
                call.id,
                serde_json::json!({
                    "op": op,
                    "input": call.input,
                    "pid": std::process::id(),
                    "config_revision": config_revision,
                }),
            );
            write_frame(writer, &Frame::Result(result)).map_err(frame_io)?;
            Ok(stream_seq)
        }
    }
}

fn parse_endpoint() -> io::Result<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--endpoint" {
            return args.next().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "--endpoint needs a value")
            });
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "--endpoint is required",
    ))
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_owned())
}

fn frame_io(err: protocol::plugin::FrameError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}
