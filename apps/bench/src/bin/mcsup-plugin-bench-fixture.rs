//! 握手基准用的最小插件进程（`BENCH-001` / `BENCH-002`）
//!
//! 只走握手与一次回显调用：连端点 → `hello` → 等 `welcome` → 回应 `ping`、
//! `config` 与 `call` → 收到 `shutdown` 退出。
//!
//! 与 `crates/plugin/tests/fixture` 的区别是它不实现契约、流与失败分支：
//! 基准只量握手，不需要那些路径。合在这里而不是复用测试 fixture，是因为
//! `CARGO_BIN_EXE_*` 只对**同一个包**的 bin 生效，而 `apps/bench` 只允许依赖
//! 门面 `mcsup`；让 bench 包自带一个只依赖门面的 fixture，比给 bench 加一条
//! 指向实现层的依赖更符合依赖方向。
//!
//! 帧类型全部来自 `mcsup::protocol`，协议漂移在编译期暴露。

use mcsup::domain::plugin::PLUGIN_API_VERSION;
use mcsup::plugin::process::platform_key;
use mcsup::protocol::plugin::{Call, Frame, Hello, ResultFrame, read_frame, write_frame};
use serde_json::json;
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

fn connect(address: &str) -> io::Result<Stream> {
    if let Some(rest) = address.strip_prefix("tcp://") {
        return Ok(Stream::Tcp(TcpStream::connect(rest)?));
    }
    #[cfg(unix)]
    {
        Ok(Stream::Unix(UnixStream::connect(address)?))
    }
    #[cfg(not(unix))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("unsupported endpoint: {address}"),
        ))
    }
}

fn frame_io(err: &mcsup::protocol::plugin::FrameError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err.to_string())
}

fn main() -> io::Result<()> {
    let endpoint = arg("--endpoint")?;
    let token = std::env::var("MCSUP_PLUGIN_TOKEN").unwrap_or_default();
    let plugin_id = std::env::var("MCSUP_PLUGIN_ID").unwrap_or_else(|_| "bench".to_owned());
    let stream = connect(&endpoint)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let hello = Hello {
        token,
        api: PLUGIN_API_VERSION,
        plugin_id,
        // 与安装记录的 `version` 必须一致，宿主在握手时逐字比较。
        plugin_version: env!("CARGO_PKG_VERSION").to_owned(),
        platform: platform_key(),
        pid: std::process::id(),
        requested: Vec::new(),
        subscriptions: Vec::new(),
    };
    write_frame(&mut writer, &Frame::Hello(hello)).map_err(|err| frame_io(&err))?;

    loop {
        let frame = match read_frame(&mut reader) {
            Ok(frame) => frame,
            Err(mcsup::protocol::plugin::FrameError::UnexpectedEof) => return Ok(()),
            Err(err) => return Err(frame_io(&err)),
        };
        match frame {
            Frame::Call(call) => {
                respond(&mut writer, &call)?;
            }
            Frame::Ping(seq) => {
                write_frame(&mut writer, &Frame::Pong(seq)).map_err(|err| frame_io(&err))?;
            }
            Frame::Config(config) => {
                let result = ResultFrame::ok(config.id, json!({ "revision": config.revision }));
                write_frame(&mut writer, &Frame::Result(result)).map_err(|err| frame_io(&err))?;
            }
            Frame::Shutdown { .. } | Frame::Reject(_) => return Ok(()),
            Frame::Welcome(_)
            | Frame::Unknown { .. }
            | Frame::Hello(_)
            | Frame::Event(_)
            | Frame::Result(_)
            | Frame::Stream(_)
            | Frame::Cancel(_)
            | Frame::Pong(_) => {}
        }
    }
}

fn respond(writer: &mut Stream, call: &Call) -> io::Result<()> {
    let result = ResultFrame::ok(
        call.id,
        json!({
            "op": call.op,
            "input": call.input,
            "pid": std::process::id(),
        }),
    );
    write_frame(writer, &Frame::Result(result)).map_err(|err| frame_io(&err))
}

fn arg(name: &str) -> io::Result<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("{name} needs a value"))
            });
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("missing required argument {name}"),
    ))
}
