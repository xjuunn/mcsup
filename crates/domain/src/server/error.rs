use super::download::DownloadStatus;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("invalid download status transition: {from:?} -> {to:?}")]
pub struct InvalidTransition {
    pub from: DownloadStatus,
    pub to: DownloadStatus,
}

/// 服务器名称非法
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid server name: {raw}")]
pub struct InvalidServerName {
    pub raw: String,
}

/// 无法识别的变体名
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown server variant: {raw}")]
pub struct UnknownVariant {
    pub raw: String,
}

/// 无法识别的校验算法
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknow checksum algorithm: {0}")]
pub struct UnknownChecksumAlgorithm(pub String);
