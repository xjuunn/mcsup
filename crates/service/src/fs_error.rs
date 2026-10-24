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

/// 目录删除失败的共用提示：Windows 上目录被占用时 `remove_dir_all` 报
/// `ERROR_SHARING_VIOLATION`（os error 32）
///
/// 常见于有终端把该目录当工作目录、资源管理器开着它。补一句可操作提示，
/// 否则用户只看到一个 os error 32，不知道该干什么。
/// registry / workspace / java 共用，禁止再各写一份。
pub(crate) fn deletion_hint(err: std::io::Error) -> std::io::Error {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    if cfg!(windows) && err.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
        return std::io::Error::other(format!(
            "{err}（目录正被占用：常见于终端的工作目录就在其中，或资源管理器/其他程序打开着；退出占用后重试）"
        ));
    }
    err
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_windows_and_other_errors_pass_through() {
        let err = std::io::Error::other("boom");
        assert_eq!(deletion_hint(err).to_string(), "boom");
    }
}
