//! 实现层错误：文件系统与扫描

use domain::backup::RelPath;
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

/// 目录扫描失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ScanError {
    /// 目录无法读取
    #[error("cannot read directory {path}: {source}")]
    Unreadable {
        /// 出错目录
        path: PathBuf,
        /// 底层错误
        #[source]
        source: std::io::Error,
    },
    /// 路径不是合法的相对路径（超出服务器目录 / 非法字符）
    #[error(transparent)]
    InvalidPath(#[from] domain::backup::InvalidRelPath),
    /// 文件读取失败
    #[error("cannot read file {path}: {source}")]
    UnreadableFile {
        /// 出错文件
        path: PathBuf,
        /// 底层错误
        #[source]
        source: std::io::Error,
    },
    /// 写入 CAS 失败
    #[error(transparent)]
    Cas(#[from] port::CasError),
    /// 扫描途中的文件系统失败
    #[error(transparent)]
    Io(#[from] BackupIoError),
}

/// 扫描期跳过的条目原因（用于把「为什么少了文件」告诉用户）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkipReason {
    /// 符号链接：不跟随，避免备份目录外内容
    Symlink,
    /// 非常规文件（管道 / 设备 / socket）
    NotRegularFile,
    /// 运行期文件（`run/` 等），备份无意义且会与运行中的进程争用
    Runtime,
    /// 不在备份范围内
    OutOfScope,
}

impl SkipReason {
    /// 稳定文本（诊断输出用）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symlink => "symlink",
            Self::NotRegularFile => "not-a-regular-file",
            Self::Runtime => "runtime",
            Self::OutOfScope => "out-of-scope",
        }
    }
}

/// 被跳过的条目
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// 相对路径
    pub path: RelPath,
    /// 跳过原因
    pub reason: SkipReason,
}
