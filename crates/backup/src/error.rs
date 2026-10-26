//! 实现层错误：文件系统

use std::path::PathBuf;
use thiserror::Error;

/// 文件系统操作失败（带路径上下文）
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BackupIoError {
    /// 读 / 写 / 建目录失败
    #[error("io error at {path}: {source}")]
    Io {
        /// 出错路径
        path: PathBuf,
        /// 底层错误
        #[source]
        source: std::io::Error,
    },
    /// 目标路径越界（符号链接逃逸等）
    #[error("refusing to touch {path}: escapes the target directory")]
    Escape {
        /// 被拒绝的路径
        path: PathBuf,
    },
}

impl BackupIoError {
    /// 构造 IO 失败
    #[must_use]
    pub fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// CAS 错误是文件系统错误的超集：实现层只构造一种，向上仍分两类呈现
impl From<BackupIoError> for port::CasError {
    fn from(err: BackupIoError) -> Self {
        match err {
            BackupIoError::Io { path, source } => Self::Io { path, source },
            BackupIoError::Escape { path } => Self::Io {
                path: path.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing to touch {}: escapes the target directory",
                        path.display()
                    ),
                ),
            },
        }
    }
}

/// 快照索引同样把文件系统错误归入存储错误
impl From<BackupIoError> for port::SnapshotStoreError {
    fn from(err: BackupIoError) -> Self {
        Self::Storage(Box::new(err))
    }
}
