//! 集成测试用的最小插件进程
//!
//! 只看协议，不碰平台数据：完成 `hello` / `welcome`，回应 `call`、`ping`，
//! 收到 `shutdown` 或 `reject` 后退出。宿主侧的 `SpawnPlan` 会把令牌、id 与
//! 作用域放到环境变量里，把端点放到 `--endpoint`。
//!
//! 契约测试用两个 op：
//!
//! - `call_contract`：由测试发起，本进程向宿主发送一次契约调用并等待结果；
//! - `contract.call`：宿主投递契约调用，本进程按载荷决定是否嵌套调用。
//!
//! 载荷里的 `chain` 数组让提供方逐层嵌套调用，用来覆盖循环、深度与 target。
//!
//! 这个 bin 只在 `fixture` feature 下构建；集成测试通过
//! `CARGO_BIN_EXE_plugin-fixture` 找到它，复制进伪产物目录后拉起。

use domain::plugin::error::PluginErrorCode;
use protocol::plugin::{
    Call, Frame, Hello, ResultFrame, StreamFrame, WireError, read_frame, write_frame,
};
use serde_json::{Value, json};
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// 本进程 → 宿主方向的调用 id
static NEXT_CALL_ID: AtomicU64 = AtomicU64::new(1);

type Outcome = Result<Value, WireError>;

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
                stream_seq =
                    handle_call(&mut reader, &mut writer, &call, stream_seq, config_revision)?;
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
    reader: &mut BufReader<Stream>,
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
        // 测试发起：本进程作为调用方，向宿主发一次契约调用。
        "call_contract" => {
            let contract = string_field(&call.input, "contract")?;
            let target = call.input.get("target").and_then(Value::as_str);
            let trace = call.input.get("trace").and_then(Value::as_str);
            let input = call.input.get("input").cloned().unwrap_or(Value::Null);
            let outcome = request_contract(reader, writer, contract, target, trace, input)?;
            respond(writer, call.id, outcome)?;
            Ok(stream_seq)
        }
        // 宿主投递：本进程作为提供方，按载荷决定是否继续嵌套调用。
        CONTRACT_OP => {
            let outcome = provider(reader, writer, &call.input)?;
            respond(writer, call.id, outcome)?;
            Ok(stream_seq)
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

/// 宿主投递的契约调用名，与 `plugin::contract::CONTRACT_OP` 对齐
const CONTRACT_OP: &str = "contract.call";

fn provider(
    reader: &mut BufReader<Stream>,
    writer: &mut Stream,
    input: &Value,
) -> io::Result<Outcome> {
    let contract = input.get("contract").and_then(Value::as_str).unwrap_or("");
    let caller = input.get("caller").and_then(Value::as_str).unwrap_or("");
    let trace = input.get("trace").and_then(Value::as_str);
    let payload = input.get("input").cloned().unwrap_or(Value::Null);
    let me = env_or("MCSUP_PLUGIN_ID", "fixture");
    if payload
        .get("fail_for")
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(me.as_str())))
    {
        return Ok(Err(WireError::from_code(
            PluginErrorCode::Denied,
            format!("fixture {me} refuses this contract"),
        )));
    }
    let chain = payload.get("chain").and_then(Value::as_array).cloned();
    let Some(mut chain) = chain.filter(|chain| !chain.is_empty()) else {
        return Ok(Ok(json!({
            "provider": me,
            "contract": contract,
            "caller": caller,
            "trace": trace,
            "echo": payload,
        })));
    };
    let next = chain.remove(0);
    let nested_contract = next.get("contract").and_then(Value::as_str).unwrap_or("");
    let nested_target = next.get("target").and_then(Value::as_str);
    let mut nested_input = next.get("input").cloned().unwrap_or_else(|| json!({}));
    if let Some(object) = nested_input.as_object_mut() {
        object.insert("chain".to_owned(), Value::Array(chain));
    }
    let outcome = request_contract(
        reader,
        writer,
        nested_contract,
        nested_target,
        trace,
        nested_input,
    )?;
    Ok(match outcome {
        Ok(nested) => Ok(json!({
            "provider": me,
            "contract": contract,
            "caller": caller,
            "trace": trace,
            "echo": payload,
            "nested": nested,
        })),
        Err(err) => Err(err),
    })
}

fn request_contract(
    reader: &mut BufReader<Stream>,
    writer: &mut Stream,
    contract: &str,
    target: Option<&str>,
    trace: Option<&str>,
    input: Value,
) -> io::Result<Outcome> {
    let id = NEXT_CALL_ID.fetch_add(1, Ordering::SeqCst);
    let call = Call::contract(
        id,
        contract,
        target.map(str::to_owned),
        trace.map(str::to_owned),
        input,
    );
    write_frame(writer, &Frame::Call(call)).map_err(frame_io)?;
    read_result(reader, writer, id)
}

fn read_result(
    reader: &mut BufReader<Stream>,
    writer: &mut Stream,
    id: u64,
) -> io::Result<Outcome> {
    loop {
        let frame = read_frame(reader).map_err(frame_io)?;
        match frame {
            Frame::Result(result) if result.id == id => {
                return Ok(if result.ok {
                    Ok(result.output.unwrap_or(Value::Null))
                } else {
                    Err(result.error.unwrap_or_else(|| {
                        WireError::from_code(PluginErrorCode::Internal, "missing error payload")
                    }))
                });
            }
            Frame::Ping(seq) => write_frame(writer, &Frame::Pong(seq)).map_err(frame_io)?,
            Frame::Config(config) => {
                let revision = config.revision;
                let result = ResultFrame::ok(config.id, json!({ "revision": revision }));
                write_frame(writer, &Frame::Result(result)).map_err(frame_io)?;
            }
            // 宿主在等待本进程的外部结果时仍可能投递新的契约调用（循环 /
            // 深度用例）。同一连接上同步处理，避免连接两侧互相等待。
            Frame::Call(call) => {
                let _ = handle_call(reader, writer, &call, 0, 0)?;
            }
            Frame::Shutdown { .. } => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "host shut down while waiting for a contract result",
                ));
            }
            _ => {}
        }
    }
}

fn respond(writer: &mut Stream, id: u64, outcome: Outcome) -> io::Result<()> {
    let frame = match outcome {
        Ok(output) => ResultFrame::ok(id, output),
        Err(error) => ResultFrame::error(id, error),
    };
    write_frame(writer, &Frame::Result(frame)).map_err(frame_io)
}

fn string_field<'a>(input: &'a Value, key: &str) -> Result<&'a str, io::Error> {
    input.get(key).and_then(Value::as_str).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("field {key} must be a string"),
        )
    })
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
