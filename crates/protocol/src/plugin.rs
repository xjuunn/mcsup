//! 插件帧协议：握手、事件、调用、流、配置、心跳与控制通道
//!
//! 与 supervisor IPC 共用「一行一帧 + 版本字段」的纪律，但版本号独立
//! （`PLUGIN_PROTOCOL_VERSION`）。解码前先做两项硬检查：单行 ≤ 1 MiB、
//! JSON 嵌套深度 ≤ 32。超出即按协议错误终止会话，不进入反序列化——JSON
//! 解析器对深嵌套与超长行没有天然的防线。
//!
//! 未知 `type` 与未知字段都必须被忽略（连接保持），版本不匹配必须断开。
//! 这是「向前兼容字段、向后拒绝版本」的唯一组合。

use domain::plugin::error::PluginErrorCode;
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};

/// 插件协议版本
pub const PLUGIN_PROTOCOL_VERSION: u32 = 1;

/// 控制通道版本（与插件协议独立递增）
pub const PLUGIN_CONTROL_VERSION: u32 = 1;

/// 单帧字节上限
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// 单帧 JSON 嵌套深度上限
pub const MAX_JSON_DEPTH: usize = 32;

/// 出站 / 入站并发调用默认上限
pub const DEFAULT_INFLIGHT: u32 = 8;

/// 事件队列默认长度
pub const DEFAULT_EVENT_QUEUE: usize = 1024;

/// 插件日志默认限速（行/秒）
pub const DEFAULT_LOG_LINES_PER_SEC: u32 = 200;

/// 握手
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// 一次性令牌
    pub token: String,
    /// 插件 API 主版本
    pub api: u32,
    /// 插件 id
    pub plugin_id: String,
    /// 插件版本
    pub plugin_version: String,
    /// 平台标识，如 `darwin-arm64`
    pub platform: String,
    /// 进程 PID
    #[serde(default)]
    pub pid: u32,
    /// 申请能力
    #[serde(default)]
    pub requested: Vec<String>,
    /// 申请订阅
    #[serde(default)]
    pub subscriptions: Vec<String>,
}

/// 宿主授予结果
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// 会话 id
    pub session_id: String,
    /// 宿主版本
    pub host_version: String,
    /// 实际授予的能力
    pub granted: Vec<String>,
    /// 限额
    pub limits: WelcomeLimits,
    /// 配置
    pub config: ConfigValues,
    /// 作用域
    pub scope: WelcomeScope,
}

/// `welcome.limits`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeLimits {
    /// 单方向 inflight 上限
    pub inflight: u32,
    /// 单帧字节上限
    pub frame_bytes: u32,
    /// 事件队列长度
    pub event_queue: u32,
    /// 日志限速（行/秒）
    pub log_lines_per_sec: u32,
}

impl Default for WelcomeLimits {
    fn default() -> Self {
        Self {
            inflight: DEFAULT_INFLIGHT,
            frame_bytes: u32::try_from(MAX_FRAME_BYTES).unwrap_or(u32::MAX),
            event_queue: u32::try_from(DEFAULT_EVENT_QUEUE).unwrap_or(u32::MAX),
            log_lines_per_sec: DEFAULT_LOG_LINES_PER_SEC,
        }
    }
}

/// 配置快照
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigValues {
    /// 单调递增的修订号
    pub revision: u64,
    /// 配置值；密钥字段回显为 `***`
    #[serde(default)]
    pub values: serde_json::Value,
}

impl Default for ConfigValues {
    fn default() -> Self {
        Self {
            revision: 0,
            values: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

/// 作用域（`welcome.scope`）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeScope {
    /// `machine` 或 `server`
    pub kind: String,
    /// `server` 作用域下的服务器名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

/// 握手拒绝原因
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RejectCode {
    /// 令牌缺失或不匹配
    Unauthorized,
    /// 插件 id / 版本与记录不符
    IdentityMismatch,
    /// API 版本不支持
    ApiMismatch,
    /// manifest / descriptor schema 不支持
    SchemaMismatch,
    /// 作用域不匹配
    ScopeMismatch,
    /// 插件未启用或未获批准
    NotApproved,
    /// 同实例已有会话
    AlreadyRunning,
    /// 其它宿主侧拒绝
    Rejected,
}

/// 拒绝帧
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reject {
    /// 原因码
    pub code: RejectCode,
    /// 人类可读说明
    pub message: String,
    /// 诊断细节（不含密钥）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// 事件帧
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventFrame {
    /// 会话内单调递增序号
    pub seq: u64,
    /// 订阅名
    pub kind: String,
    /// RFC3339 UTC
    pub at: String,
    /// 来源服务器；`server` 作用域下恒为自身
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// 载荷
    pub data: serde_json::Value,
}

/// 调用帧
///
/// 四种用法共用 `type:"call"`：
/// - `op`：宿主调用插件的 op；
/// - `capability` + `method`：插件调用宿主能力；
/// - `method`：CLI / server / agent 经控制通道调用宿主方法；
/// - `contract`：插件经宿主调用另一个插件提供的契约。
///
/// 契约调用可以带 `target`（仅单播契约允许）与 `trace`（宿主在根调用时
/// 分配，提供方再次发起契约调用必须原样带回，宿主据此做循环、深度与扇出
/// 判定）。`trace` 对插件是不透明令牌，解析规则只在宿主。
///
/// 解码后必须调用 [`Call::route`] 校验字段组合，不能只看 `op` 是否为空。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    /// 方向内唯一的调用 id
    pub id: u64,
    /// 插件 op
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    /// 宿主能力
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    /// 方法
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// 契约标识（如 `chat.publish@1`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<String>,
    /// 单播契约的目标插件
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// 宿主分配的调用链令牌
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
    /// 输入
    #[serde(default)]
    pub input: serde_json::Value,
}

/// 调用路由
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallRoute<'a> {
    /// 宿主 → 插件的 op 调用
    Op(&'a str),
    /// 插件 → 宿主的宿主能力调用
    Host {
        /// 能力名
        capability: &'a str,
        /// 方法名
        method: &'a str,
    },
    /// 控制通道方法
    Control(&'a str),
    /// 插件 → 宿主的契约调用
    Contract {
        /// 契约标识（含版本）
        contract: &'a str,
        /// 单播目标
        target: Option<&'a str>,
        /// 调用链令牌
        trace: Option<&'a str>,
    },
}

impl Call {
    /// 构造 op 调用
    #[must_use]
    pub fn op(id: u64, op: impl Into<String>, input: serde_json::Value) -> Self {
        Self {
            id,
            op: Some(op.into()),
            capability: None,
            method: None,
            contract: None,
            target: None,
            trace: None,
            input,
        }
    }

    /// 构造宿主能力调用
    #[must_use]
    pub fn host(
        id: u64,
        capability: impl Into<String>,
        method: impl Into<String>,
        input: serde_json::Value,
    ) -> Self {
        Self {
            id,
            op: None,
            capability: Some(capability.into()),
            method: Some(method.into()),
            contract: None,
            target: None,
            trace: None,
            input,
        }
    }

    /// 构造控制通道调用
    #[must_use]
    pub fn control(id: u64, method: impl Into<String>, input: serde_json::Value) -> Self {
        Self {
            id,
            op: None,
            capability: None,
            method: Some(method.into()),
            contract: None,
            target: None,
            trace: None,
            input,
        }
    }

    /// 构造契约调用
    #[must_use]
    pub fn contract(
        id: u64,
        contract: impl Into<String>,
        target: Option<String>,
        trace: Option<String>,
        input: serde_json::Value,
    ) -> Self {
        Self {
            id,
            op: None,
            capability: None,
            method: None,
            contract: Some(contract.into()),
            target,
            trace,
            input,
        }
    }

    /// 校验字段组合并返回路由
    ///
    /// # Errors
    ///
    /// 同时出现 `op` 与 `capability`、缺少 `method`、全部为空、`target` /
    /// `trace` 脱离契约调用等组合返回 [`FrameError::InvalidCall`]
    pub fn route(&self) -> Result<CallRoute<'_>, FrameError> {
        if let Some(contract) = &self.contract {
            if contract.is_empty()
                || self.op.is_some()
                || self.capability.is_some()
                || self.method.is_some()
                || self.target.as_deref() == Some("")
                || self.trace.as_deref() == Some("")
            {
                return Err(FrameError::InvalidCall {
                    reason: "contract call must use contract only, with optional target/trace"
                        .to_owned(),
                });
            }
            return Ok(CallRoute::Contract {
                contract,
                target: self.target.as_deref(),
                trace: self.trace.as_deref(),
            });
        }
        if self.target.is_some() || self.trace.is_some() {
            return Err(FrameError::InvalidCall {
                reason: "target/trace are only valid on contract calls".to_owned(),
            });
        }
        match (&self.op, &self.capability, &self.method) {
            (Some(op), None, None) if !op.is_empty() => Ok(CallRoute::Op(op)),
            (None, Some(capability), Some(method))
                if !capability.is_empty() && !method.is_empty() =>
            {
                Ok(CallRoute::Host { capability, method })
            }
            (None, None, Some(method)) if !method.is_empty() => Ok(CallRoute::Control(method)),
            _ => Err(FrameError::InvalidCall {
                reason: "call must be exactly one of op, capability+method, method, or contract"
                    .to_owned(),
            }),
        }
    }
}

/// 错误载荷（线上 `error` 对象）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    /// 稳定错误码
    pub code: PluginErrorCode,
    /// 分类文本（`usage` / `unauthorized` / `backend` / ...）
    pub category: String,
    /// 是否可重试
    pub retryable: bool,
    /// 人类可读消息
    pub message: String,
}

impl WireError {
    /// 由错误码构造，分类按协议词表映射
    #[must_use]
    pub fn from_code(code: PluginErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            category: category_of(code).to_owned(),
            retryable: code.retryable(),
            message: message.into(),
        }
    }
}

/// 错误码 → 协议分类文本
///
/// 与 `port::ErrorCategory` 的取值一一对应；映射只在这里写一次。
#[must_use]
pub const fn category_of(code: PluginErrorCode) -> &'static str {
    use PluginErrorCode as C;
    match code {
        C::InvalidInput | C::LimitExceeded | C::Unsupported => "usage",
        C::NotFound => "not_found",
        C::Denied => "unauthorized",
        C::Conflict => "conflict",
        C::Cancelled => "cancelled",
        C::Timeout | C::Unavailable | C::Crashed => "backend",
        // 非穷尽枚举：`internal` 与宿主版本更新带来的未知码都按 failure 处理。
        _ => "failure",
    }
}

/// 结果帧
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultFrame {
    /// 对应调用的 id
    pub id: u64,
    /// 是否成功
    pub ok: bool,
    /// 成功输出
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    /// 失败错误
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

impl ResultFrame {
    /// 成功结果
    #[must_use]
    pub fn ok(id: u64, output: serde_json::Value) -> Self {
        Self {
            id,
            ok: true,
            output: Some(output),
            error: None,
        }
    }

    /// 失败结果
    #[must_use]
    pub fn error(id: u64, error: WireError) -> Self {
        Self {
            id,
            ok: false,
            output: None,
            error: Some(error),
        }
    }
}

/// 流式分块
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamFrame {
    /// 调用 id
    pub id: u64,
    /// 从 0 连续递增
    pub seq: u64,
    /// 分块载荷
    pub chunk: serde_json::Value,
    /// 是否为最后一块
    #[serde(default)]
    pub done: bool,
}

/// 取消
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cancel {
    /// 调用或流 id
    pub id: u64,
    /// 取消原因（`timeout` / `client` / ...）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// 配置变更
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigFrame {
    /// 需要回复 `result` 的调用 id
    pub id: u64,
    /// 单调递增修订号
    pub revision: u64,
    /// 新配置值
    pub values: serde_json::Value,
}

/// 一帧
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// 插件 → 宿主：握手
    Hello(Hello),
    /// 宿主 → 插件：授予
    Welcome(Welcome),
    /// 宿主 → 插件：拒绝
    Reject(Reject),
    /// 宿主 → 插件：事件
    Event(EventFrame),
    /// 双向：调用
    Call(Call),
    /// 双向：结果
    Result(ResultFrame),
    /// 插件 → 宿主：流
    Stream(StreamFrame),
    /// 双向：取消
    Cancel(Cancel),
    /// 宿主 → 插件：配置
    Config(ConfigFrame),
    /// 宿主 → 插件：心跳
    Ping(u64),
    /// 插件 → 宿主：心跳回应
    Pong(u64),
    /// 宿主 → 插件：优雅退出
    Shutdown {
        /// 宽限毫秒
        grace_ms: u64,
    },
    /// 未知类型：忽略并记一行日志，连接保持
    Unknown {
        /// 原始 `type`
        kind: String,
    },
}

impl Frame {
    /// 线上 `type` 文本
    #[must_use]
    pub fn kind(&self) -> &str {
        match self {
            Self::Hello(_) => "hello",
            Self::Welcome(_) => "welcome",
            Self::Reject(_) => "reject",
            Self::Event(_) => "event",
            Self::Call(_) => "call",
            Self::Result(_) => "result",
            Self::Stream(_) => "stream",
            Self::Cancel(_) => "cancel",
            Self::Config(_) => "config",
            Self::Ping(_) => "ping",
            Self::Pong(_) => "pong",
            Self::Shutdown { .. } => "shutdown",
            Self::Unknown { kind } => kind,
        }
    }

    /// 该帧是否只允许插件发送
    #[must_use]
    pub const fn plugin_to_host(&self) -> bool {
        matches!(self, Self::Hello(_) | Self::Stream(_) | Self::Pong(_))
    }
}

/// 帧错误
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FrameError {
    /// 非法 JSON / 结构
    #[error("invalid frame: {reason}")]
    Invalid {
        /// 原因
        reason: String,
    },
    /// 单帧超限
    #[error("frame exceeds {limit} bytes")]
    TooLarge {
        /// 上限
        limit: usize,
    },
    /// JSON 嵌套过深
    #[error("frame nesting exceeds depth {limit}")]
    TooDeep {
        /// 上限
        limit: usize,
    },
    /// 版本不匹配
    #[error("plugin protocol version {peer} is not supported (host: {host})")]
    UnsupportedVersion {
        /// 对端版本
        peer: u32,
        /// 本端版本
        host: u32,
    },
    /// 调用字段组合非法
    #[error("invalid call: {reason}")]
    InvalidCall {
        /// 原因
        reason: String,
    },
    /// 读到 EOF
    #[error("connection closed before a frame arrived")]
    UnexpectedEof,
    /// 底层 IO
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// 写出一帧（JSON + 换行 + flush）
///
/// # Errors
///
/// 序列化或写入失败时返回 [`FrameError`]
pub fn write_frame<W: Write>(writer: &mut W, frame: &Frame) -> Result<(), FrameError> {
    let value = frame_to_value(frame)?;
    serde_json::to_writer(&mut *writer, &value).map_err(|err| FrameError::Invalid {
        reason: err.to_string(),
    })?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// 读入一帧并做行宽 / 深度检查
///
/// # Errors
///
/// 行超限、深度超限、版本不匹配、非法 JSON 或 EOF 时返回 [`FrameError`]
pub fn read_frame<R: BufRead>(reader: &mut R) -> Result<Frame, FrameError> {
    let line = read_line_limited(reader, MAX_FRAME_BYTES)?;
    let line = strip_cr(line);
    parse_line(&line)
}

/// 解析一行（不含行尾换行）
///
/// # Errors
///
/// 同 [`read_frame`]（除 IO / EOF 之外）
pub fn parse_line(line: &[u8]) -> Result<Frame, FrameError> {
    let text = std::str::from_utf8(line).map_err(|err| FrameError::Invalid {
        reason: format!("invalid UTF-8: {err}"),
    })?;
    if text.trim().is_empty() {
        return Err(FrameError::Invalid {
            reason: "empty frame".to_owned(),
        });
    }
    let depth = max_depth(text.as_bytes());
    if depth > MAX_JSON_DEPTH {
        return Err(FrameError::TooDeep {
            limit: MAX_JSON_DEPTH,
        });
    }
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|err| FrameError::Invalid {
            reason: err.to_string(),
        })?;
    frame_from_value(&value)
}

/// 把帧序列化成带版本信封的值
///
/// # Errors
///
/// 帧无法序列化时返回 [`FrameError`]
pub fn frame_to_value(frame: &Frame) -> Result<serde_json::Value, FrameError> {
    // `Frame` 是手写路由的联合体，不是 serde 的 tagged enum：每类帧的
    // 载荷字段不同（`ping` 用 `n`，`shutdown` 用 `grace_ms`），这里按类型
    // 组装成对象并补上信封字段，避免给 `Frame` 造一套与线上格式不一致的
    // derive 表示。
    let (kind, mut object) = match frame {
        Frame::Hello(hello) => ("hello", body_object(hello)?),
        Frame::Welcome(welcome) => ("welcome", body_object(welcome)?),
        Frame::Reject(reject) => ("reject", body_object(reject)?),
        Frame::Event(event) => ("event", body_object(event)?),
        Frame::Call(call) => ("call", body_object(call)?),
        Frame::Result(result) => ("result", body_object(result)?),
        Frame::Stream(stream) => ("stream", body_object(stream)?),
        Frame::Cancel(cancel) => ("cancel", body_object(cancel)?),
        Frame::Config(config) => ("config", body_object(config)?),
        Frame::Ping(n) => ("ping", number_object(*n)),
        Frame::Pong(n) => ("pong", number_object(*n)),
        Frame::Shutdown { grace_ms } => {
            let mut object = serde_json::Map::new();
            object.insert("grace_ms".to_owned(), serde_json::Value::from(*grace_ms));
            ("shutdown", object)
        }
        Frame::Unknown { .. } => {
            return Err(FrameError::Invalid {
                reason: "unknown frames are receive-only".to_owned(),
            });
        }
    };
    object.insert(
        "v".to_owned(),
        serde_json::Value::from(PLUGIN_PROTOCOL_VERSION),
    );
    object.insert("type".to_owned(), serde_json::Value::from(kind));
    Ok(serde_json::Value::Object(object))
}

/// 把载荷结构体序列化成对象；非对象即协议实现错误
fn body_object<T: Serialize>(
    body: &T,
) -> Result<serde_json::Map<String, serde_json::Value>, FrameError> {
    match serde_json::to_value(body).map_err(|err| FrameError::Invalid {
        reason: err.to_string(),
    })? {
        serde_json::Value::Object(map) => Ok(map),
        _ => Err(FrameError::Invalid {
            reason: "frame body must serialize to an object".to_owned(),
        }),
    }
}

/// 心跳帧的载荷：`{ "n": n }`
fn number_object(n: u64) -> serde_json::Map<String, serde_json::Value> {
    let mut object = serde_json::Map::new();
    object.insert("n".to_owned(), serde_json::Value::from(n));
    object
}

fn frame_from_value(value: &serde_json::Value) -> Result<Frame, FrameError> {
    let object = value.as_object().ok_or_else(|| FrameError::Invalid {
        reason: "frame must be a JSON object".to_owned(),
    })?;
    let version = object
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| FrameError::Invalid {
            reason: "missing v".to_owned(),
        })?;
    if version != u64::from(PLUGIN_PROTOCOL_VERSION) {
        return Err(FrameError::UnsupportedVersion {
            peer: u32::try_from(version).unwrap_or(u32::MAX),
            host: PLUGIN_PROTOCOL_VERSION,
        });
    }
    let kind = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| FrameError::Invalid {
            reason: "missing type".to_owned(),
        })?;
    let body = value.clone();
    let frame = match kind {
        "hello" => Frame::Hello(decode(&body)?),
        "welcome" => Frame::Welcome(decode(&body)?),
        "reject" => Frame::Reject(decode(&body)?),
        "event" => Frame::Event(decode(&body)?),
        "call" => Frame::Call(decode(&body)?),
        "result" => Frame::Result(decode_result(&body)?),
        "stream" => Frame::Stream(decode(&body)?),
        "cancel" => Frame::Cancel(decode(&body)?),
        "config" => Frame::Config(decode(&body)?),
        "ping" => Frame::Ping(decode_number(&body)?),
        "pong" => Frame::Pong(decode_number(&body)?),
        "shutdown" => Frame::Shutdown {
            grace_ms: object
                .get("grace_ms")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| FrameError::Invalid {
                    reason: "shutdown requires grace_ms".to_owned(),
                })?,
        },
        other => Frame::Unknown {
            kind: other.to_owned(),
        },
    };
    Ok(frame)
}

fn decode<T: for<'de> Deserialize<'de>>(value: &serde_json::Value) -> Result<T, FrameError> {
    serde_json::from_value(value.clone()).map_err(|err| FrameError::Invalid {
        reason: err.to_string(),
    })
}

fn decode_result(value: &serde_json::Value) -> Result<ResultFrame, FrameError> {
    let frame: ResultFrame = decode(value)?;
    match (frame.ok, frame.error.is_some()) {
        (true, true) => Err(FrameError::Invalid {
            reason: "ok result must not carry error".to_owned(),
        }),
        (false, false) => Err(FrameError::Invalid {
            reason: "failed result requires error".to_owned(),
        }),
        _ => Ok(frame),
    }
}

fn decode_number(value: &serde_json::Value) -> Result<u64, FrameError> {
    let object = value.as_object().ok_or_else(|| FrameError::Invalid {
        reason: "frame must be an object".to_owned(),
    })?;
    object
        .get("n")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| FrameError::Invalid {
            reason: "ping/pong requires n".to_owned(),
        })
}

fn read_line_limited<R: BufRead>(reader: &mut R, limit: usize) -> Result<Vec<u8>, FrameError> {
    let mut out = Vec::new();
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return if out.is_empty() {
                Err(FrameError::UnexpectedEof)
            } else {
                Ok(out)
            };
        }
        if let Some(pos) = buf.iter().position(|byte| *byte == b'\n') {
            if out.len() + pos > limit {
                return Err(FrameError::TooLarge { limit });
            }
            out.extend_from_slice(&buf[..pos]);
            reader.consume(pos + 1);
            return Ok(out);
        }
        if out.len() + buf.len() > limit {
            return Err(FrameError::TooLarge { limit });
        }
        out.extend_from_slice(buf);
        let consumed = buf.len();
        reader.consume(consumed);
    }
}

fn strip_cr(mut line: Vec<u8>) -> Vec<u8> {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    line
}

/// 计算 JSON 嵌套深度（忽略字符串里的括号）
fn max_depth(bytes: &[u8]) -> usize {
    let mut depth = 0_usize;
    let mut max = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                max = max.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample_hello() -> Frame {
        Frame::Hello(Hello {
            token: "<token>".to_owned(),
            api: 1,
            plugin_id: "demo".to_owned(),
            plugin_version: "0.1.0".to_owned(),
            platform: "darwin-arm64".to_owned(),
            pid: 42,
            requested: vec!["events".to_owned()],
            subscriptions: vec!["chat:message".to_owned()],
        })
    }

    #[allow(clippy::needless_pass_by_value)] // 测试直接把手上的帧交进来
    fn roundtrip(frame: Frame) -> Frame {
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).expect("write");
        let mut cursor = Cursor::new(buf);
        read_frame(&mut cursor).expect("read")
    }

    #[test]
    fn hello_roundtrips_with_version_and_type() {
        let frame = roundtrip(sample_hello());
        assert_eq!(frame, sample_hello());
        let mut buf = Vec::new();
        write_frame(&mut buf, &sample_hello()).expect("write");
        let line = String::from_utf8(buf).expect("utf8");
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.contains("\"v\":1"));
        assert!(line.contains("\"type\":\"hello\""));
    }

    #[test]
    fn call_routes_are_validated() {
        let op = Call::op(1, "echo", serde_json::json!({}));
        assert!(matches!(op.route(), Ok(CallRoute::Op("echo"))));
        let host = Call::host(2, "console:write", "send_line", serde_json::json!({}));
        assert!(matches!(
            host.route(),
            Ok(CallRoute::Host {
                capability: "console:write",
                method: "send_line"
            })
        ));
        let control = Call::control(3, "status", serde_json::json!({}));
        assert!(matches!(control.route(), Ok(CallRoute::Control("status"))));
        let contract = Call::contract(
            4,
            "chat.publish@1",
            None,
            Some("t-1".to_owned()),
            serde_json::json!({ "text": "hi" }),
        );
        assert!(matches!(
            contract.route(),
            Ok(CallRoute::Contract {
                contract: "chat.publish@1",
                target: None,
                trace: Some("t-1"),
            })
        ));
        let invalid = Call {
            id: 5,
            op: Some("echo".to_owned()),
            capability: Some("events".to_owned()),
            method: None,
            contract: None,
            target: None,
            trace: None,
            input: serde_json::json!({}),
        };
        assert!(invalid.route().is_err());
        let mut stray_target = Call::op(6, "echo", serde_json::json!({}));
        stray_target.target = Some("other".to_owned());
        assert!(stray_target.route().is_err());
    }

    #[test]
    fn result_requires_consistent_ok_and_error() {
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &Frame::Result(ResultFrame {
                id: 1,
                ok: true,
                output: Some(serde_json::json!({})),
                error: Some(WireError::from_code(PluginErrorCode::Internal, "boom")),
            }),
        )
        .expect("write");
        let err = read_frame(&mut Cursor::new(buf)).expect_err("must reject");
        assert!(matches!(err, FrameError::Invalid { .. }));

        let good = Frame::Result(ResultFrame::error(
            2,
            WireError::from_code(PluginErrorCode::Denied, "nope"),
        ));
        assert_eq!(roundtrip(good.clone()), good);
    }

    #[test]
    fn unsupported_version_breaks_the_session() {
        let mut cursor = Cursor::new(b"{\"v\":2,\"type\":\"ping\",\"n\":1}\n".to_vec());
        assert!(matches!(
            read_frame(&mut cursor),
            Err(FrameError::UnsupportedVersion { peer: 2, host: 1 })
        ));
    }

    #[test]
    fn unknown_type_is_ignored_but_keeps_kind() {
        let frame = parse_line(br#"{"v":1,"type":"telemetry","x":1}"#).expect("parse");
        assert_eq!(
            frame,
            Frame::Unknown {
                kind: "telemetry".to_owned()
            }
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let frame =
            parse_line(br#"{"v":1,"type":"ping","n":7,"extra":{"nested":true}}"#).expect("parse");
        assert_eq!(frame, Frame::Ping(7));
    }

    #[test]
    fn oversize_frame_is_rejected_before_parsing() {
        let mut line = vec![b'x'; MAX_FRAME_BYTES + 1];
        line.push(b'\n');
        let err = read_frame(&mut Cursor::new(line)).expect_err("oversize");
        assert!(matches!(err, FrameError::TooLarge { .. }));
    }

    #[test]
    fn deep_json_is_rejected_before_deserialization() {
        let payload = format!(
            "{{\"v\":1,\"type\":\"event\",\"seq\":0,\"kind\":\"x\",\"at\":\"t\",\"data\":{}{}}}",
            "[".repeat(40),
            "]".repeat(40)
        );
        let err = parse_line(payload.as_bytes()).expect_err("deep");
        assert!(matches!(err, FrameError::TooDeep { .. }));
    }

    #[test]
    fn garbage_and_empty_lines_are_invalid() {
        assert!(parse_line(b"not json").is_err());
        assert!(parse_line(b"").is_err());
        assert!(parse_line(b"[]").is_err());
        assert!(parse_line(b"{\"v\":1}").is_err());
    }

    #[test]
    fn crlf_is_accepted() {
        let mut cursor = Cursor::new(b"{\"v\":1,\"type\":\"ping\",\"n\":1}\r\n".to_vec());
        assert_eq!(read_frame(&mut cursor).expect("read"), Frame::Ping(1));
    }

    #[test]
    fn eof_is_reported() {
        let mut cursor = Cursor::new(Vec::new());
        assert!(matches!(
            read_frame(&mut cursor),
            Err(FrameError::UnexpectedEof)
        ));
    }

    #[test]
    fn writes_flush_immediately() {
        struct FlushSpy {
            buf: Vec<u8>,
            flushed: bool,
        }
        impl Write for FlushSpy {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                self.buf.extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.flushed = true;
                Ok(())
            }
        }
        let mut spy = FlushSpy {
            buf: Vec::new(),
            flushed: false,
        };
        write_frame(&mut spy, &Frame::Ping(1)).expect("write");
        assert!(spy.flushed);
    }

    #[test]
    fn error_categories_match_codes() {
        assert_eq!(category_of(PluginErrorCode::Denied), "unauthorized");
        assert_eq!(category_of(PluginErrorCode::Timeout), "backend");
        assert_eq!(category_of(PluginErrorCode::InvalidInput), "usage");
        assert_eq!(category_of(PluginErrorCode::Internal), "failure");
    }

    #[test]
    fn welcome_defaults_are_stable() {
        let limits = WelcomeLimits::default();
        assert_eq!(limits.frame_bytes as usize, MAX_FRAME_BYTES);
        assert_eq!(limits.inflight, DEFAULT_INFLIGHT);
        assert_eq!(ConfigValues::default().revision, 0);
    }
}
