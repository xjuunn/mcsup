//! Fabric Meta v2 的响应模型
//!
//! 只声明本项目用到的字段；上游新增字段不影响解析。端点清单见
//! <https://meta.fabricmc.net/>，本模块覆盖其中三处：
//!
//! - `GET /v2/versions/game`：Minecraft 版本列表（`stable` 标记）
//! - `GET /v2/versions/loader`：loader 版本列表（`stable` 标记）
//! - `GET /v2/versions/installer`：安装器版本列表（`stable` 标记）
//!
//! 三处响应都按「新版本在前」返回，选取逻辑不得依赖该顺序，一律显式比对
//! `stable` 与版本号。

use serde::Deserialize;

/// `GET /v2/versions/game` 的一项
#[derive(Debug, Deserialize)]
pub(crate) struct GameVersion {
    /// Minecraft 版本号，如 `1.21.1`
    pub(crate) version: String,
    /// 是否为稳定版；快照为 `false`
    pub(crate) stable: bool,
}

/// `GET /v2/versions/loader` 的一项
#[derive(Debug, Deserialize)]
pub(crate) struct LoaderVersion {
    /// loader 版本号，如 `0.16.10`
    pub(crate) version: String,
    /// 是否为稳定版；预发布为 `false`
    pub(crate) stable: bool,
}

/// `GET /v2/versions/installer` 的一项
#[derive(Debug, Deserialize)]
pub(crate) struct InstallerVersion {
    /// 安装器版本号，如 `1.0.1`
    pub(crate) version: String,
    /// 是否为稳定版
    pub(crate) stable: bool,
}
