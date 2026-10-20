use std::path::{Path, PathBuf};
use thiserror::Error;

/// service 层文件系统失败的共用载荷
///
/// workspace / registry / java 三个错误面共用同一份形状：
/// - [`FsError::Io`] 消息只带路径，原因走 source 链——`{:#}` 渲染时才不会把
///   原因打两遍
/// - [`FsError::Marker`] 带路径与解析 / 序列化原因
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FsError {
    /// 文件系统失败
    #[error("io error on {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// marker 解析 / 序列化失败
    #[error("marker {path}: {message}")]
    Marker { path: PathBuf, message: String },
}

impl FsError {
    /// IO 失败：路径随消息带出，原因走 source 链
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    /// marker 解析 / 序列化失败
    pub(crate) fn marker(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Marker {
            path: path.into(),
            message: message.into(),
        }
    }
}
