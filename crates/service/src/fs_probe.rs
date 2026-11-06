//! `domain::server::launch::DirProbe` 的真实磁盘实现
//!
//! 领域层只声明「要问磁盘什么」，真正的 `std::fs` 调用落在实现层。放在
//! `service` 是因为调用点（`registry::loader_args_files`）在这里；等 `service`
//! 的其余 `std::fs` 一并下沉到 `port` 时，本结构随之迁移，判定逻辑不用动。

use domain::server::launch::{DirEntry, DirProbe};
use std::path::Path;

/// 直接读真实文件系统的探测实现
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FsDirProbe;

impl DirProbe for FsDirProbe {
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn list(&self, dir: &Path) -> Option<Vec<DirEntry>> {
        let entries = std::fs::read_dir(dir).ok()?;
        Some(
            entries
                .flatten()
                .map(|entry| {
                    let path = entry.path();
                    // 与原先 `path.is_dir()` 同口径：跟随符号链接
                    let is_dir = path.is_dir();
                    DirEntry { path, is_dir }
                })
                .collect(),
        )
    }
}
