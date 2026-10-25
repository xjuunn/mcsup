//! ``PaperMC`` Fill v3 的响应模型
//!
//! `Paper` / `Folia` / `Velocity` / `Waterfall` 四个项目共用同一套端点结构与
//! 响应结构，差异只在项目 id 与「是不是代理」。因此这里一份模型服务四个变体。
//!
//! 端点（均已实测可达，需带非通用 User-Agent + 联系方式）：
//!
//! ```text
//! GET {fill}/v3/projects/{project}
//! GET {fill}/v3/projects/{project}/versions/{version}/builds?channel=STABLE
//! ```
//!
//! # 下载地址与哈希都在响应里
//!
//! Fill v3 的 `downloads["server:default"]` 同时给出 `url`、`size` 与
//! `checksums.sha256`。**禁止自行拼下载地址**：真实 URL 指向
//! `fill-data.papermc.io/v1/objects/{sha256}/...`，与项目 / 版本号无路径关系，
//! 自行拼接必然 404。
//!
//! # 版本分组
//!
//! `/versions` 的 `versions` 是「MC 主版本 → 该主版本下的具体版本」两级映射
//! （如 `"1.21": ["1.21.11", "1.21.1", ...]`），`BTreeMap` 保持稳定顺序便于
//! 测试与展示。列表按「新在前」，**首项最新**。
//!
//! # 只声明用得到的字段
//!
//! 项目 id / 展示名与下载物文件名都不参与业务判定，只是上游回显；为了让
//! 「响应能不能解析」有可断言的锚点、并在镜像把项目搞混时能报出人话，模型里
//! 保留它们，由 [`Project::id`] / [`Project::display_name`] /
//! [`Download::file_name`] 读取。

use serde::Deserialize;
use std::collections::BTreeMap;

/// `GET /v3/projects/{project}` 的响应
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Project {
    /// 项目标识（`paper` / `folia` / `velocity` / `waterfall`）
    #[serde(default)]
    pub(crate) project: ProjectId,
    /// MC 主版本 → 具体版本列表，新在前
    #[serde(default)]
    pub(crate) versions: BTreeMap<String, Vec<String>>,
}

/// 项目标识块
///
/// 只取 `id`：它用来校验「镜像返回的是不是我要的项目」。`name` 是展示名，
/// 本项目展示名自有词表（见 `ServerVariant::as_str`），上游文案不参与渲染，
/// 因此不声明。
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ProjectId {
    /// 项目 id
    #[serde(default)]
    pub(crate) id: String,
}

impl Project {
    /// 项目 id（上游回显；用于确认镜像没把项目搞混）
    #[must_use]
    pub(crate) fn id(&self) -> &str {
        &self.project.id
    }

    /// 全部具体版本，按「主版本降序、组内原序」展平
    ///
    /// 分组的 key 是 MC 主版本（`1.21`），组内才是可安装的具体版本
    /// （`1.21.11`）。`BTreeMap` 升序遍历，这里反转以得到「新在前」。
    #[must_use]
    pub(crate) fn flat_versions(&self) -> Vec<&str> {
        self.versions
            .values()
            .rev()
            .flat_map(|group| group.iter().map(String::as_str))
            .collect()
    }
}

/// `GET /versions/{version}/builds` 的一条 build
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Build {
    /// build 编号
    #[serde(default)]
    pub(crate) id: u64,
    /// 发布渠道（`STABLE` / `ALPHA` / `BETA` …）
    #[serde(default)]
    pub(crate) channel: String,
    /// 该 build 的可下载物，按用途分键
    #[serde(default)]
    pub(crate) downloads: BTreeMap<String, Download>,
}

/// 一个下载物
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Download {
    /// 文件名（诊断 / 改名用）
    #[serde(default)]
    pub(crate) name: String,
    /// 直接下载地址（`fill-data.papermc.io`，**不要自行拼**）
    #[serde(default)]
    pub(crate) url: String,
    /// 字节数
    #[serde(default)]
    pub(crate) size: u64,
    /// 校验和集合
    #[serde(default)]
    pub(crate) checksums: BTreeMap<String, String>,
}

/// 服务端下载物在 `downloads` 里的键
///
/// Fill v3 用 `{用途}:{渠道}` 形式作键，服务端档位叫 `server:default`。
/// 代理（`Velocity` / `Waterfall`）同样用这个键，不必另立。
pub(crate) const SERVER_DOWNLOAD_KEY: &str = "server:default";

impl Build {
    /// 该 build 的服务端下载物
    #[must_use]
    pub(crate) fn server_download(&self) -> Option<&Download> {
        self.downloads.get(SERVER_DOWNLOAD_KEY)
    }

    /// 是否为稳定渠道
    #[must_use]
    pub(crate) fn is_stable(&self) -> bool {
        self.channel.eq_ignore_ascii_case("STABLE")
    }
}

impl Download {
    /// 内嵌的 sha256
    #[must_use]
    pub(crate) fn sha256(&self) -> Option<&str> {
        self.checksums.get("sha256").map(String::as_str)
    }

    /// 服务端 JAR 的文件名（改名或诊断时用）
    #[must_use]
    pub(crate) fn file_name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = include_str!("../../tests/fixtures/paper_project.json");
    const BUILDS: &str = include_str!("../../tests/fixtures/paper_builds.json");

    fn project() -> Project {
        serde_json::from_str(PROJECT).expect("paper_project fixture")
    }

    fn builds() -> Vec<Build> {
        serde_json::from_str(BUILDS).expect("paper_builds fixture")
    }

    #[test]
    fn project_deserializes_two_level_versions() {
        let project = project();
        assert_eq!(project.id(), "paper");
        assert!(project.versions.contains_key("1.21"));
    }

    /// 展平后新在前：`26.3` 组在 `1.21` 组之前
    #[test]
    fn flat_versions_puts_newer_groups_first() {
        let project = project();
        let flat = project.flat_versions();
        let pos_263 = flat.iter().position(|v| *v == "26.3").expect("26.3");
        let pos_1211 = flat.iter().position(|v| *v == "1.21.11").expect("1.21.11");
        assert!(pos_263 < pos_1211, "{flat:?}");
        // 组内顺序保留（上游已是新在前）
        assert_eq!(flat[0], "26.3");
    }

    #[test]
    fn builds_deserialize_with_embedded_download() {
        let builds = builds();
        assert_eq!(builds[0].id, 133);
        assert!(builds[0].is_stable());
        let download = builds[0].server_download().expect("server:default");
        assert_eq!(download.file_name(), "paper-1.21.1-133.jar");
        assert_eq!(download.size, 49_394_394);
        assert_eq!(
            download.sha256(),
            Some("39bd8c00b9e18de91dcabd3cc3dcfa5328685a53b7187a2f63280c22e2d287b9")
        );
        assert!(download.url.starts_with("https://fill-data.papermc.io/"));
    }

    #[test]
    fn build_without_server_download_is_detected() {
        let build: Build = serde_json::from_str(
            r#"{"id":1,"channel":"STABLE","downloads":{"server:default":{"url":"https://x/a.jar","size":1},"application":{"url":"https://x/b.zip","size":2}}}"#,
        )
        .expect("parse");
        assert!(build.server_download().is_some());
        let build: Build =
            serde_json::from_str(r#"{"id":1,"channel":"STABLE","downloads":{}}"#).expect("parse");
        assert!(build.server_download().is_none());
    }

    #[test]
    fn channel_matching_is_case_insensitive() {
        let build: Build =
            serde_json::from_str(r#"{"id":1,"channel":"stable","downloads":{}}"#).expect("parse");
        assert!(build.is_stable());
    }

    #[test]
    fn missing_checksums_is_empty_not_error() {
        let download: Download =
            serde_json::from_str(r#"{"name":"x.jar","url":"u"}"#).expect("parse");
        assert_eq!(download.size, 0);
        assert!(download.sha256().is_none());
    }

    #[test]
    fn ignores_unknown_fields() {
        let project: Project =
            serde_json::from_str(r#"{"project":{"id":"paper"},"versions":{},"extra":1}"#)
                .expect("parse");
        assert_eq!(project.id(), "paper");
    }

    /// 缺 `project` 块时不 panic，id 为空（由来源决定是否报错）
    #[test]
    fn missing_project_block_yields_empty_id() {
        let project: Project = serde_json::from_str(r#"{"versions":{}}"#).expect("parse");
        assert_eq!(project.id(), "");
    }
}
