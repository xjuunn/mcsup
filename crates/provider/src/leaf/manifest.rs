//! Leaf 官方 API 的响应模型
//!
//! Leaf 是 Paper 的下游分支，但同样**不复用 `PaperMC` Fill v3**：它自有
//! `api.leafmc.one/v2` 接口（结构与 `PaperMC` v2 相近，与 Fill v3 不同）。
//!
//! 端点（均已实测可达）：
//!
//! ```text
//! GET {base}/projects/leaf                                    → 版本列表
//! GET {base}/projects/leaf/versions/{version}/builds          → 该版本的 build 列表
//! GET {base}/projects/leaf/versions/{version}/builds/{b}      → 单个 build 详情
//! ```
//!
//! 下载地址由「build 列表里给出的文件名」拼成
//! `{base}/projects/leaf/versions/{v}/builds/{b}/downloads/{name}`——文件名来自
//! 响应，不自行拼 `leaf-{v}-{b}.jar`。
//!
//! # 顺序与稳定标记
//!
//! `builds` 数组按「旧在前」，**末项最新**；`versions` 是平铺数组（不是 `PaperMC`
//! Fill 的两级 map），同样「旧在前」。与 `PaperMC` 不同，Leaf **不提供 stable
//! 标记**（`promoted` 实测多为 `false`，`channel` 有 `default` / `experimental`），
//! 因此不能按渠道筛「稳定版」，只能取最新。
//!
//! # 哈希
//!
//! `downloads.primary.sha256` 内嵌在 build 响应里，可直接强校验。

use serde::Deserialize;

/// `GET /projects/leaf` 的响应
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Project {
    /// 项目 id（上游回显；恒为 `leaf`）
    #[serde(default)]
    pub(crate) project_id: String,
    /// 全部 MC 版本，旧在前
    #[serde(default)]
    pub(crate) versions: Vec<String>,
}

impl Project {
    /// 项目 id 是否确实是 leaf（空串表示上游未回显，不做判定）
    #[must_use]
    pub(crate) fn is_leaf(&self) -> bool {
        self.project_id.is_empty() || self.project_id == "leaf"
    }
}

/// `GET /versions/{version}/builds` 的响应
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BuildList {
    /// 版本号（上游回显）
    #[serde(default)]
    pub(crate) version: String,
    /// build 列表，旧在前
    #[serde(default)]
    pub(crate) builds: Vec<Build>,
}

impl BuildList {
    /// 上游回显的版本号是否与请求一致（空串表示未回显，不做判定）
    #[must_use]
    pub(crate) fn matches_version(&self, requested: &str) -> bool {
        self.version.is_empty() || self.version == requested
    }
}

/// 一条 build
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Build {
    /// build 编号
    ///
    /// 上游字段名就是 `build`，保留原名以免多一处 serde rename；结构体已叫
    /// `Build`，`build.build` 读起来确实重复，但它是最贴近上游的名字。
    #[serde(default)]
    #[allow(clippy::struct_field_names)]
    pub(crate) build: u64,
    /// 发布渠道（`default` / `experimental` …）
    ///
    /// Leaf 的 `channel` **没有稳定语义**（`promoted` 也多为 `false`），不能用它
    /// 筛「推荐版」，本来源不按它筛选。字段保留 + 由 [`Build::is_experimental`]
    /// 读取，是为了上游将来给出稳定语义时不必改模型。
    #[serde(default)]
    pub(crate) channel: String,
    /// 下载物，按用途分键
    #[serde(default)]
    pub(crate) downloads: Downloads,
}
/// build 的下载物集合
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Downloads {
    /// 主下载物（服务端 JAR）
    pub(crate) primary: Option<Primary>,
}

/// 主下载物
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Primary {
    /// 文件名（`leaf-1.21.11-179.jar`），用于拼下载地址
    #[serde(default)]
    pub(crate) name: String,
    /// 内嵌 sha256
    #[serde(default)]
    pub(crate) sha256: String,
}

impl Build {
    /// 是否为实验渠道
    ///
    /// Leaf 没有「稳定版」概念，只有 `default` 与 `experimental` 的区分；本来源
    /// 不按它筛（见模块文档），此访问器供诊断 / 将来需要时使用。
    #[must_use]
    #[allow(
        dead_code,
        reason = "Leaf 的 channel 无稳定语义，本来源不按它筛选；保留判定供诊断与上游语义变化"
    )]
    pub(crate) fn is_experimental(&self) -> bool {
        self.channel.eq_ignore_ascii_case("experimental")
    }

    /// 该 build 的主下载物；文件名缺失时视为不可下载
    ///
    /// `name` 会拼进 URL 路径，空串或含路径分隔符的输入一律拒绝——它是不可信
    /// 的上游数据，不能让 `..%2f` 之类的东西进到请求地址里。
    #[must_use]
    pub(crate) fn primary(&self) -> Option<&Primary> {
        let primary = self.downloads.primary.as_ref()?;
        if primary.name.is_empty() || primary.name.contains('/') || primary.name.contains('\\') {
            return None;
        }
        Some(primary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = include_str!("../../tests/fixtures/leaf_project.json");
    const BUILDS: &str = include_str!("../../tests/fixtures/leaf_builds.json");

    fn project() -> Project {
        serde_json::from_str(PROJECT).expect("leaf_project fixture")
    }

    fn builds() -> BuildList {
        serde_json::from_str(BUILDS).expect("leaf_builds fixture")
    }

    #[test]
    fn project_lists_versions_oldest_first() {
        let project = project();
        assert_eq!(project.project_id, "leaf");
        assert!(project.is_leaf());
        assert!(project.versions.iter().any(|v| v == "1.21.11"));
    }

    #[test]
    fn build_list_deserializes_primary_download() {
        let list = builds();
        assert_eq!(list.version, "1.21.11");
        assert!(list.matches_version("1.21.11"));
        assert!(!list.matches_version("1.21.4"));
        let newest = list.builds.last().expect("非空");
        let primary = newest.primary().expect("primary");
        assert_eq!(primary.name, "leaf-1.21.11-179.jar");
        assert_eq!(primary.sha256.len(), 64);
    }

    /// 文件名缺失 / 含路径分隔符时拒绝（防止路径穿越进 URL）
    #[test]
    fn primary_rejects_unsafe_names() {
        let raw = r#"{"build":1,"downloads":{"primary":{"name":"","sha256":"x"}}}"#;
        let build: Build = serde_json::from_str(raw).expect("parse");
        assert!(build.primary().is_none());
        let raw = r#"{"build":1,"downloads":{"primary":{"name":"../../x.jar","sha256":"x"}}}"#;
        let build: Build = serde_json::from_str(raw).expect("parse");
        assert!(build.primary().is_none());
        let raw = r#"{"build":1,"downloads":{"primary":{"name":"a\\b.jar","sha256":"x"}}}"#;
        let build: Build = serde_json::from_str(raw).expect("parse");
        assert!(build.primary().is_none());
    }

    #[test]
    fn build_without_downloads_has_no_primary() {
        let build: Build =
            serde_json::from_str(r#"{"build":1,"channel":"default"}"#).expect("parse");
        assert!(build.primary().is_none());
        assert!(!build.is_experimental());
        let build: Build =
            serde_json::from_str(r#"{"build":1,"channel":"experimental"}"#).expect("parse");
        assert!(build.is_experimental());
    }

    #[test]
    fn missing_fields_are_defaulted() {
        let list: BuildList = serde_json::from_str("{}").expect("parse");
        assert!(list.builds.is_empty());
        assert!(list.matches_version("1.21.11"));
        let project: Project = serde_json::from_str("{}").expect("parse");
        assert!(project.is_leaf());
    }
}
