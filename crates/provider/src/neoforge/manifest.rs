//! `NeoForge` 官方 Maven 版本索引模型
//!
//! 端点（已实测可达）：
//!
//! ```text
//! GET {maven}/api/maven/versions/releases/net/neoforged/neoforge
//! ```
//!
//! 响应形如：
//!
//! ```json
//! { "isSnapshot": false, "versions": ["20.6.115", ..., "21.1.265"] }
//! ```
//!
//! 只声明本项目用到的字段；上游新增字段不影响解析。
//!
//! # 顺序陷阱
//!
//! `versions` 按「旧版本在前」排列，**末项才是最新 build**，与 Fabric Meta 的
//! 「新版本在前」相反。任何选取逻辑都必须显式取末项。
//!
//! # `isSnapshot` 为什么保留
//!
//! 索引按「release / snapshot 两条 Maven 线」分开发布，本模块只打 release 线
//! （URL 里已写死 `releases`），因此响应恒为 `false`。字段仍声明出来，是为了
//! 在镜像把两条线混在同一个地址时能用 [`NeoForgeVersions::is_snapshot`] 判定，
//! 而不是默默按正式版处理。

use serde::Deserialize;

/// `/api/maven/versions/releases/...` 的响应
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct NeoForgeVersions {
    /// 该前缀下是否全是快照；正式发布索引为 `false`
    #[serde(rename = "isSnapshot", default)]
    pub(crate) is_snapshot: bool,
    /// 全部 `NeoForge` 版本串，旧在前
    #[serde(default)]
    pub(crate) versions: Vec<String>,
}

impl NeoForgeVersions {
    /// 索引是否来自「正式发布」线
    ///
    /// release 线恒为 `true`；镜像把 snapshot 线混进来时返回 `false`，调用方
    /// 应据此拒绝，而不是把快照 build 当正式版装给用户。
    #[must_use]
    pub(crate) fn is_release_line(&self) -> bool {
        !self.is_snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_versions_index() {
        let raw = r#"{"isSnapshot":false,"versions":["21.0.167","21.1.265"]}"#;
        let index: NeoForgeVersions = serde_json::from_str(raw).expect("parse");
        assert!(!index.is_snapshot);
        assert!(index.is_release_line());
        assert_eq!(index.versions, vec!["21.0.167", "21.1.265"]);
    }

    #[test]
    fn snapshot_line_is_detected() {
        let raw = r#"{"isSnapshot":true,"versions":["0.1.0-beta"]}"#;
        let index: NeoForgeVersions = serde_json::from_str(raw).expect("parse");
        assert!(!index.is_release_line());
    }

    #[test]
    fn missing_versions_field_is_empty_not_error() {
        let index: NeoForgeVersions =
            serde_json::from_str(r#"{"isSnapshot":false}"#).expect("parse");
        assert!(index.versions.is_empty());
    }

    #[test]
    fn ignores_unknown_fields() {
        let raw = r#"{"isSnapshot":false,"versions":["21.1.1"],"somethingNew":42}"#;
        let index: NeoForgeVersions = serde_json::from_str(raw).expect("parse");
        assert_eq!(index.versions, vec!["21.1.1"]);
    }
}
