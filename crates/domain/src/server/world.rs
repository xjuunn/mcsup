//! 世界数据版本：判断目标核心能否打开既有世界
//!
//! Minecraft 的世界数据带单调递增的 `DataVersion`，只支持升级、不支持降级。
//! 目标核心的 `DataVersion` 低于世界时，启动会因无法解析世界数据而失败，
//! 因此在安装阶段就要拦住。

use std::fmt;

use thiserror::Error;

/// 世界数据版本（`level.dat` 的 `DataVersion` 与核心 `version.json` 的
/// `world_version` 同一口径）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DataVersion(i32);

impl DataVersion {
    #[must_use]
    pub const fn new(raw: i32) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

impl fmt::Display for DataVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 既有世界的版本信息
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorldInfo {
    /// `level.dat` 的 `DataVersion`；极老或损坏的世界可能缺失
    pub data_version: Option<DataVersion>,
    /// `level.dat` 的 `Version.Name`（如 `26.1`），仅用于错误提示
    pub version_name: Option<String>,
}

/// 目标核心低于世界数据版本：Minecraft 不支持降级世界
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("world DataVersion {world} is newer than core DataVersion {target}")]
pub struct WorldDowngrade {
    /// 世界的数据版本
    pub world: DataVersion,
    /// 目标核心的数据版本
    pub target: DataVersion,
}

/// 目标核心必须能读这个世界：`target >= world`
///
/// # Errors
///
/// 目标低于世界时返回 [`WorldDowngrade`]
pub fn ensure_world_readable(
    world: DataVersion,
    target: DataVersion,
) -> Result<(), WorldDowngrade> {
    if target < world {
        Err(WorldDowngrade { world, target })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_or_newer_core_is_allowed() {
        let world = DataVersion::new(4786);
        assert!(ensure_world_readable(world, DataVersion::new(4786)).is_ok());
        assert!(ensure_world_readable(world, DataVersion::new(5000)).is_ok());
    }

    #[test]
    fn older_core_is_rejected_with_both_versions() {
        let err = ensure_world_readable(DataVersion::new(4786), DataVersion::new(4554))
            .expect_err("downgrade");
        assert_eq!(err.world, DataVersion::new(4786));
        assert_eq!(err.target, DataVersion::new(4554));
    }
}
