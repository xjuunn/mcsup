//! 插件错误的稳定词表
//!
//! 11 个错误码是协议、CLI、Web 与审计共用的词汇。分类（usage / unauthorized /
//! backend / ...）由 `port` 映射成 `port::ErrorCategory`，本模块只负责
//! 「码 → 是否可重试」与「码 → 含义」这两件不依赖入口的事。
//!
//! `[禁止]` 插件把未授予的能力调用降级成成功：没发出就必须回报 `denied`。

use std::fmt;

/// 插件错误码（线上 `error.code`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PluginErrorCode {
    /// 输入不符合 descriptor
    InvalidInput,
    /// 服务器 / 资源不存在
    NotFound,
    /// 能力未授予或越权
    Denied,
    /// 状态不允许（已在运行等）
    Conflict,
    /// 超时
    Timeout,
    /// 被取消
    Cancelled,
    /// 插件未运行 / 正在重启
    Unavailable,
    /// 插件进程崩溃
    Crashed,
    /// 超过帧 / 流 / 队列限额
    LimitExceeded,
    /// 当前平台或版本不支持
    Unsupported,
    /// 插件内部错误
    Internal,
}

impl PluginErrorCode {
    /// 全部 11 个错误码
    pub const ALL: [Self; 11] = [
        Self::InvalidInput,
        Self::NotFound,
        Self::Denied,
        Self::Conflict,
        Self::Timeout,
        Self::Cancelled,
        Self::Unavailable,
        Self::Crashed,
        Self::LimitExceeded,
        Self::Unsupported,
        Self::Internal,
    ];

    /// 线上文本
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::NotFound => "not_found",
            Self::Denied => "denied",
            Self::Conflict => "conflict",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Unavailable => "unavailable",
            Self::Crashed => "crashed",
            Self::LimitExceeded => "limit_exceeded",
            Self::Unsupported => "unsupported",
            Self::Internal => "internal",
        }
    }

    /// 解析线上文本
    ///
    /// # Errors
    ///
    /// 未知码返回 [`UnknownErrorCode`]；调用方按 `internal` 处理并记录原文，
    /// 不得静默当成成功
    pub fn parse(raw: &str) -> Result<Self, UnknownErrorCode> {
        Self::ALL
            .into_iter()
            .find(|code| code.as_str() == raw)
            .ok_or_else(|| UnknownErrorCode {
                raw: raw.to_owned(),
            })
    }

    /// 调用方是否可以原样重试
    ///
    /// `conflict` 视情况：状态修好后可重试，但当前请求本身不能立刻重发，
    /// 因此按 `false` 返回，由调用方在修正状态后重新发起新请求。
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(self, Self::Timeout | Self::Unavailable | Self::Crashed)
    }

    /// 人类可读的默认说明（仅在插件未给出 message 时使用）
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::InvalidInput => "input does not match the descriptor",
            Self::NotFound => "resource not found",
            Self::Denied => "capability not granted",
            Self::Conflict => "state does not allow this operation",
            Self::Timeout => "operation timed out",
            Self::Cancelled => "operation was cancelled",
            Self::Unavailable => "plugin is not running",
            Self::Crashed => "plugin process crashed",
            Self::LimitExceeded => "limit exceeded",
            Self::Unsupported => "unsupported on this platform or version",
            Self::Internal => "plugin internal error",
        }
    }
}

impl fmt::Display for PluginErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl serde::Serialize for PluginErrorCode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for PluginErrorCode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// 未知错误码
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown plugin error code: {raw}")]
pub struct UnknownErrorCode {
    /// 原始输入
    pub raw: String,
}

/// 一个插件错误（线上 `error` 对象的内容）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginError {
    /// 稳定错误码
    pub code: PluginErrorCode,
    /// 人类可读消息；不得包含密钥
    pub message: String,
}

impl PluginError {
    /// 构造
    #[must_use]
    pub fn new(code: PluginErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 用错误码的默认说明构造
    #[must_use]
    pub fn from_code(code: PluginErrorCode) -> Self {
        Self {
            code,
            message: code.description().to_owned(),
        }
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PluginError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn eleven_codes_are_unique_and_parseable() {
        assert_eq!(PluginErrorCode::ALL.len(), 11);
        let names: BTreeSet<&str> = PluginErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(names.len(), 11);
        for code in PluginErrorCode::ALL {
            assert_eq!(PluginErrorCode::parse(code.as_str()), Ok(code));
        }
        assert!(PluginErrorCode::parse("sandbox_error").is_err());
    }

    #[test]
    fn retryability_matches_contract() {
        for code in PluginErrorCode::ALL {
            let expected = matches!(
                code,
                PluginErrorCode::Timeout | PluginErrorCode::Unavailable | PluginErrorCode::Crashed
            );
            assert_eq!(code.retryable(), expected, "{code}");
        }
    }

    #[test]
    fn error_display_includes_code() {
        let err = PluginError::new(PluginErrorCode::Denied, "console:write 未授予");
        assert_eq!(err.to_string(), "denied: console:write 未授予");
        assert_eq!(
            PluginError::from_code(PluginErrorCode::Timeout).message,
            "operation timed out"
        );
    }
}
