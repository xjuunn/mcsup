//! 传输层失败
//!
//! 与 `protocol` 的错误分开：这里是「连接 / 身份 / TLS」的问题，那里是
//! 「帧内容」的问题。入口的退出码不同（连不上是网络问题，帧非法是对端问题），
//! 混成一个枚举会让调用方只能靠字符串猜。

use std::path::PathBuf;
use thiserror::Error;

/// 传输层失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TransportError {
    /// TCP / TLS 读写失败
    #[error("{context}: {source}")]
    Io {
        /// 正在做什么（`connect 10.0.0.5:7681` / `read frame` …）
        context: String,
        /// 底层错误
        #[source]
        source: std::io::Error,
    },

    /// TLS 握手或配置失败
    #[error("tls: {0}")]
    Tls(String),

    /// 服务端证书指纹与固定值不符（可能是中间人，也可能是对端换了证书）
    #[error("certificate fingerprint mismatch: expected {expected}, got {actual}")]
    FingerprintMismatch {
        /// 本地记录的指纹
        expected: String,
        /// 对端出示的指纹
        actual: String,
    },

    /// 对端未按协议说话（版本不匹配、帧非法、顺序错误）
    #[error("remote protocol: {0}")]
    Protocol(String),

    /// 对端拒绝了本次操作
    #[error("{kind}: {message}")]
    Remote {
        /// 错误种类（`protocol::remote::ErrorKind` 的取值）
        kind: String,
        /// 人类可读消息
        message: String,
    },

    /// 身份文件读写失败
    #[error("{path}: {source}")]
    Identity {
        /// 身份文件路径
        path: PathBuf,
        /// 底层错误
        #[source]
        source: std::io::Error,
    },

    /// 身份文件内容非法或证书生成失败
    #[error("remote identity: {0}")]
    IdentityCorrupt(String),

    /// 连接建立后认证被拒
    #[error("unauthorized: {0}")]
    Unauthorized(String),
}

impl TransportError {
    /// 构造带上下文的 IO 失败
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// 该失败是否值得重连一次再试
    ///
    /// 只有传输层问题值得重试：协议错误与权限拒绝重试多少次都一样，反而
    /// 会把「版本不匹配」变成一串超时。
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Io { .. })
    }

    /// 错误种类文本（入口映射退出码用）
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Io { .. } => "io",
            Self::Tls(_) => "tls",
            Self::FingerprintMismatch { .. } => "fingerprint",
            Self::Protocol(_) => "protocol",
            Self::Remote { .. } => "remote",
            Self::Identity { .. } | Self::IdentityCorrupt(_) => "identity",
            Self::Unauthorized(_) => "unauthorized",
        }
    }
}

impl From<rustls::Error> for TransportError {
    fn from(err: rustls::Error) -> Self {
        Self::Tls(err.to_string())
    }
}
