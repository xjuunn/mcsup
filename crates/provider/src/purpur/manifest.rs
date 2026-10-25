//! Purpur 官方 API 的响应模型
//!
//! Purpur 是 Paper 的下游分支，但**不复用 `PaperMC` Fill v3**：它自有
//! `api.purpurmc.org/v2` 接口，响应结构与 Fill v3 完全不同。
//!
//! 端点（均已实测可达）：
//!
//! ```text
//! GET {base}/purpur                              → 版本列表
//! GET {base}/purpur/{version}                    → 该版本的 build 列表
//! GET {base}/purpur/{version}/{build}/download    → 服务端 JAR（流式）
//! ```
//!
//! # 没有哈希
//!
//! Purpur **不提供任何校验和**：build 详情里没有哈希字段，
//! `?hash=md5` 查询参数在 GET / POST 下都返回 `405 Method Not Allowed`。
//! 因此本来源只能给出 [`Checksum::unverified`]，下载后无法强校验——这是上游
//! 能力的上限，不是本项目的取舍。
//!
//! # 顺序
//!
//! `versions` 数组按「旧在前」排列，**末项最新**，与 `PaperMC` Fill v3 的
//! 「新在前」相反。`builds.all` 同样是「旧在前」，`builds.latest` 单独给出最新
//! build 号（string，不是数字）。

use serde::Deserialize;

/// `GET /purpur` 的响应
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct VersionIndex {
    /// 项目名（恒为 `purpur`）
    #[serde(default)]
    pub(crate) project: String,
    /// 全部 MC 版本，旧在前
    #[serde(default)]
    pub(crate) versions: Vec<String>,
}

/// `GET /purpur/{version}` 的响应
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BuildIndex {
    /// 版本号（上游回显）
    #[serde(default)]
    pub(crate) version: String,
    /// build 列表
    #[serde(default)]
    pub(crate) builds: Builds,
}

impl BuildIndex {
    /// 上游回显的版本号是否与请求一致
    ///
    /// 镜像若把版本路径答错（如 `1.21.1` 回成 `1.21`），下载的就是别的版本的
    /// 核心。空串表示上游未回显，不做判定。
    #[must_use]
    pub(crate) fn matches_version(&self, requested: &str) -> bool {
        self.version.is_empty() || self.version == requested
    }
}

/// 某版本下的 build 集合
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Builds {
    /// 最新 build 号（**字符串**）
    #[serde(default)]
    pub(crate) latest: String,
    /// 全部 build 号，旧在前
    #[serde(default)]
    pub(crate) all: Vec<String>,
}

impl Builds {
    /// 最新 build 号
    ///
    /// 优先用上游的 `latest`：它是 Purpur 自己判定的「当前 build」，可能包含
    /// 不在 `all` 里的补发构建。`latest` 缺失时退回 `all` 的末项。
    #[must_use]
    pub(crate) fn latest(&self) -> Option<&str> {
        if !self.latest.is_empty() {
            return Some(&self.latest);
        }
        self.all.last().map(String::as_str)
    }
}

impl VersionIndex {
    /// 项目名是否确实是 purpur（空串表示上游未回显，不做判定）
    #[must_use]
    pub(crate) fn is_purpur(&self) -> bool {
        self.project.is_empty() || self.project == "purpur"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERSIONS: &str = include_str!("../../tests/fixtures/purpur_versions.json");
    const BUILDS: &str = include_str!("../../tests/fixtures/purpur_builds.json");

    #[test]
    fn version_index_lists_versions_oldest_first() {
        let index: VersionIndex = serde_json::from_str(VERSIONS).expect("purpur_versions fixture");
        assert_eq!(index.project, "purpur");
        assert_eq!(index.versions.first().map(String::as_str), Some("1.14.1"));
        assert_eq!(index.versions.last().map(String::as_str), Some("26.3"));
        assert!(index.versions.iter().any(|v| v == "1.21.1"));
    }

    #[test]
    fn build_index_carries_latest_and_all() {
        let index: BuildIndex = serde_json::from_str(BUILDS).expect("purpur_builds fixture");
        assert_eq!(index.version, "1.21.1");
        assert_eq!(index.builds.latest(), Some("2329"));
        assert_eq!(index.builds.all, vec!["2327", "2328", "2329"]);
    }

    /// `latest` 缺失时退回 `all` 末项
    #[test]
    fn latest_falls_back_to_last_of_all() {
        let builds: Builds = serde_json::from_str(r#"{"all":["1","2"]}"#).expect("parse");
        assert_eq!(builds.latest(), Some("2"));
        let empty: Builds = serde_json::from_str(r#"{"latest":"","all":[]}"#).expect("parse");
        assert_eq!(empty.latest(), None);
    }

    /// `latest` 优先于 `all` 末项
    #[test]
    fn latest_prefers_upstream_field() {
        let builds: Builds =
            serde_json::from_str(r#"{"latest":"9","all":["1","2"]}"#).expect("parse");
        assert_eq!(builds.latest(), Some("9"));
    }

    #[test]
    fn missing_fields_are_defaulted() {
        let index: BuildIndex = serde_json::from_str("{}").expect("parse");
        assert!(index.builds.all.is_empty());
        assert_eq!(index.builds.latest(), None);
        // 未回显时不做判定
        assert!(index.matches_version("1.21.1"));
        let empty: VersionIndex = serde_json::from_str("{}").expect("parse");
        assert!(empty.is_purpur());
    }

    /// 回显与请求不一致时判定为否（镜像答错版本 / 项目）
    #[test]
    fn echo_mismatch_is_detected() {
        let index: BuildIndex = serde_json::from_str(BUILDS).expect("fixture");
        assert!(index.matches_version("1.21.1"));
        assert!(!index.matches_version("1.21"));
        let versions: VersionIndex = serde_json::from_str(VERSIONS).expect("fixture");
        assert!(versions.is_purpur());
        let other: VersionIndex = serde_json::from_str(r#"{"project":"paper"}"#).expect("parse");
        assert!(!other.is_purpur());
    }
}
