//! Leaf 官方 API 服务端来源
//!
//! Leaf 是 Paper 的下游分支（性能与特性增强），发布渠道同样独立：它走自有
//! `api.leafmc.one/v2`，不进 `PaperMC` Fill。因此这里单列来源，与
//! [`crate::purpur`] 同理。
//!
//! # 版本口径
//!
//! 一个版本就是一个 Minecraft 版本（如 `1.21.11`），该 MC 版本下用哪个 build
//! 由本来源挑选（列表末项，上游「旧在前」）。`latest` 取版本列表末项——Leaf
//! **没有** stable 标记，所以 `latest` 与 `latest-snapshot` 同口径。
//!
//! # 上游约束（实测）
//!
//! - `GET {base}/projects/leaf` → `{"project_id":"leaf","version_groups":[…],"versions":[…]}`
//! - `GET {base}/projects/leaf/versions/{v}/builds` →
//!   `{"version":"1.21.11","builds":[{...,"downloads":{"primary":{"name":…,"sha256":…}}}]}`
//! - 下载地址：`{base}/projects/leaf/versions/{v}/builds/{b}/downloads/{name}`，
//!   其中 `{name}` 来自响应（形如 `leaf-1.21.11-179.jar`），**不自行拼**。
//! - `channel` 有 `default` / `experimental`，但**无稳定的「推荐版本」概念**；
//!   因此不按渠道过滤，只取最新。
//!
//! # 哈希
//!
//! `downloads.primary.sha256` 内嵌在响应里，可强校验（比 Purpur 强）。

use crate::http::{build_client, get_json};
use crate::shared::{
    Downloads, VersionIntent, normalize_base, parse_https_url, parse_sha256_hex,
    pick_by_version_aliases,
};
use async_trait::async_trait;
use domain::server::version::{ServerVariant, ServerVersion, VersionId, VersionKind};
use port::{ServerJarSource, SourceError};
use reqwest::Client;
use serde::de::DeserializeOwned;
use std::fmt;

use super::manifest::{Build, BuildList, Project};

/// Leaf 官方 API 根地址
pub(crate) const OFFICIAL_API: &str = "https://api.leafmc.one/v2";

/// 挑选 build 的口径
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Picks {
    /// 该版本最新 build（列表末项）
    #[default]
    Latest,
    /// 指定 build 编号
    Fixed {
        /// build 编号
        build: u64,
    },
}

/// Leaf 服务端来源
///
/// [`LeafSource::new`] 对接官方；镜像通过 [`LeafSource::builder`] 改 `api_base`。
pub struct LeafSource {
    http: Client,
    /// API 根地址（不带尾斜杠），官方或镜像
    api_base: String,
    picks: Picks,
}

impl fmt::Debug for LeafSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeafSource")
            .field("api_base", &self.api_base)
            .field("picks", &self.picks)
            .finish_non_exhaustive()
    }
}

impl LeafSource {
    /// 对接官方的默认实例
    ///
    /// 构造不打网络，只准备 HTTP 客户端。
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败时返回 [`SourceError::Init`]
    pub fn new() -> Result<Self, SourceError> {
        Self::builder().build()
    }

    /// 构造器：改镜像地址或选版策略
    #[must_use]
    pub fn builder() -> LeafSourceBuilder {
        LeafSourceBuilder::default()
    }

    async fn get<T>(&self, url: &str, not_found: Option<VersionId>) -> Result<T, SourceError>
    where
        T: DeserializeOwned,
    {
        get_json(&self.http, url, not_found).await
    }

    /// 版本列表地址
    fn project_url(&self) -> String {
        format!("{}/projects/leaf", self.api_base)
    }

    /// 某版本的 build 列表地址
    fn builds_url(&self, version: &str) -> String {
        format!("{}/projects/leaf/versions/{version}/builds", self.api_base)
    }

    /// 某 build 的主下载物地址
    fn download_url(&self, version: &str, build: u64, name: &str) -> String {
        format!(
            "{}/projects/leaf/versions/{version}/builds/{build}/downloads/{name}",
            self.api_base
        )
    }

    /// 拉取项目元数据（镜像答错项目时报错）
    async fn project(&self) -> Result<Project, SourceError> {
        let project: Project = self.get(&self.project_url(), None).await?;
        if !project.is_leaf() {
            return Err(SourceError::Decode(Box::new(std::io::Error::other(
                format!(
                    "leaf api returned project `{}` for requested `leaf`",
                    project.project_id
                ),
            ))));
        }
        Ok(project)
    }

    /// 版本标识 → 具体版本（含 `latest` / `latest-snapshot` 别名）
    ///
    /// Leaf 无 stable 标记：两个别名都取末项。
    async fn resolve_version(&self, version: &VersionId) -> Result<String, SourceError> {
        let project = self.project().await?;
        let versions: Vec<&str> = project.versions.iter().map(String::as_str).collect();
        pick_by_version_aliases(VersionIntent::of(version), versions, |_| false, |_| false)
            .or_else(|| match VersionIntent::of(version) {
                VersionIntent::Literal(raw) => project
                    .versions
                    .iter()
                    .find(|candidate| candidate.as_str() == raw)
                    .cloned(),
                VersionIntent::Latest | VersionIntent::LatestSnapshot => {
                    project.versions.last().cloned()
                }
            })
            .ok_or_else(|| SourceError::NotFound(version.clone()))
    }

    /// 在某版本下拉 build 列表（顺带校验上游回显的版本号）
    async fn fetch_builds(&self, version: &str) -> Result<Vec<Build>, SourceError> {
        let list: BuildList = self
            .get(&self.builds_url(version), Some(VersionId::new(version)))
            .await?;
        if !list.matches_version(version) {
            return Err(SourceError::Decode(Box::new(std::io::Error::other(
                format!(
                    "leaf api returned version `{}` for requested `{version}`",
                    list.version
                ),
            ))));
        }
        Ok(list.builds)
    }

    /// 在某版本下选 build
    async fn pick_build(&self, version: &str) -> Result<Build, SourceError> {
        let builds = self.fetch_builds(version).await?;
        if let Picks::Fixed { build } = self.picks {
            return builds
                .into_iter()
                .find(|candidate| candidate.build == build)
                .ok_or_else(|| SourceError::NotFound(VersionId::new(version)));
        }
        select_build(&builds, version)
    }
}

#[async_trait]
impl ServerJarSource for LeafSource {
    fn variant(&self) -> ServerVariant {
        ServerVariant::Leaf
    }

    fn name(&self) -> &'static str {
        "leaf"
    }

    async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError> {
        Ok(self
            .project()
            .await?
            .versions
            .into_iter()
            .map(into_server_version)
            .collect())
    }

    async fn artifact(&self, version: &VersionId) -> Result<port::ResolvedArtifact, SourceError> {
        let resolved = self.resolve_version(version).await?;
        let build = self.pick_build(&resolved).await?;
        let primary = build.primary().ok_or_else(|| {
            SourceError::Decode(Box::new(std::io::Error::other(format!(
                "leaf build {} of {resolved} has no downloadable primary artifact",
                build.build
            ))))
        })?;
        let location = parse_https_url(&self.download_url(&resolved, build.build, &primary.name))?;
        let mut downloads = Downloads::direct(location);
        if let Some(checksum) = parse_sha256_hex(&primary.sha256) {
            downloads = downloads.checksum(checksum);
        }
        Ok(downloads.resolve(version, None))
    }
}

/// 从 build 列表里选一条：取末项（上游「旧在前」）
///
/// 末项没有可下载主文件时向前回溯，取最靠后的可下载 build——比直接失败更宽，
/// 且不会挑到更旧的「看起来更稳」的版本（Leaf 的 `channel` 没有稳定语义）。
fn select_build(builds: &[Build], version: &str) -> Result<Build, SourceError> {
    builds
        .iter()
        .rev()
        .find(|build| build.primary().is_some())
        .cloned()
        .ok_or_else(|| SourceError::NotFound(VersionId::new(version)))
}

/// 版本号 → 领域版本
///
/// Leaf 只覆盖正式 MC 版本，统一标 `Release`；Java 大版本由原版元数据给出。
fn into_server_version(id: String) -> ServerVersion {
    ServerVersion {
        id: VersionId::new(id),
        kind: VersionKind::Release,
        variant: ServerVariant::Leaf,
        java_major: None,
    }
}

/// [`LeafSource`] 的构造器
#[derive(Debug, Default)]
pub struct LeafSourceBuilder {
    api_base: Option<String>,
    picks: Option<Picks>,
}

impl LeafSourceBuilder {
    /// 换 API 根地址（镜像）
    ///
    /// 地址需与官方同构（`/projects/leaf/...`）。只接受 `https`。
    #[must_use]
    pub fn api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = Some(base.into());
        self
    }

    /// 接管 build 选择策略
    #[must_use]
    pub fn picks(mut self, picks: Picks) -> Self {
        self.picks = Some(picks);
        self
    }

    /// 装配来源
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败，或 `api_base` 非 `https` 时返回 [`SourceError::Init`]
    pub fn build(self) -> Result<LeafSource, SourceError> {
        Ok(LeafSource {
            http: build_client()?,
            api_base: normalize_base(self.api_base, OFFICIAL_API)?,
            picks: self.picks.unwrap_or_default(),
        })
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

    fn builds() -> Vec<Build> {
        serde_json::from_str::<BuildList>(BUILDS)
            .expect("leaf_builds fixture")
            .builds
    }

    #[test]
    fn declares_leaf_variant() {
        let source = LeafSource::new().expect("leaf source");
        assert_eq!(source.variant(), ServerVariant::Leaf);
        assert_eq!(source.name(), "leaf");
    }

    #[test]
    fn builds_endpoints() {
        let source = LeafSource::new().expect("leaf source");
        assert_eq!(
            source.project_url(),
            "https://api.leafmc.one/v2/projects/leaf"
        );
        assert_eq!(
            source.builds_url("1.21.11"),
            "https://api.leafmc.one/v2/projects/leaf/versions/1.21.11/builds"
        );
        assert_eq!(
            source.download_url("1.21.11", 179, "leaf-1.21.11-179.jar"),
            "https://api.leafmc.one/v2/projects/leaf/versions/1.21.11/builds/179/downloads/leaf-1.21.11-179.jar"
        );
    }

    #[test]
    fn trims_trailing_slash_on_mirror_base() {
        let source = LeafSource::builder()
            .api_base("https://mirror.example/v2/")
            .build()
            .expect("leaf source");
        assert_eq!(
            source.download_url("1.21.11", 1, "a.jar"),
            "https://mirror.example/v2/projects/leaf/versions/1.21.11/builds/1/downloads/a.jar"
        );
    }

    #[test]
    fn rejects_insecure_base() {
        let err = LeafSource::builder()
            .api_base("http://mirror.example/v2")
            .build();
        assert!(matches!(err, Err(SourceError::Init(_))));
    }

    /// 选 build：取末项（上游「旧在前」）
    #[test]
    fn select_build_takes_last_entry() {
        let picked = select_build(&builds(), "1.21.11").expect("build");
        assert_eq!(picked.build, 179);
    }

    /// 末项不可下载时向前回溯到最靠后的可下载 build
    #[test]
    fn select_build_skips_undownloadable_tail() {
        let raw = r#"{
            "version": "1.21.11",
            "builds": [
                {"build": 1, "downloads": {"primary": {"name": "a.jar", "sha256": ""}}},
                {"build": 2, "downloads": {}},
                {"build": 3, "downloads": {}}
            ]
        }"#;
        let builds: Vec<Build> = serde_json::from_str::<BuildList>(raw)
            .expect("parse")
            .builds;
        let picked = select_build(&builds, "1.21.11").expect("build");
        assert_eq!(picked.build, 1);
    }

    /// 全部不可下载时报 `NotFound`
    #[test]
    fn select_build_empty_is_not_found() {
        let err = select_build(&[], "1.21.11").expect_err("应报 NotFound");
        assert!(matches!(err, SourceError::NotFound(_)), "{err:?}");
        let raw = r#"{"version":"1.21.11","builds":[{"build":1,"downloads":{}}]}"#;
        let builds: Vec<Build> = serde_json::from_str::<BuildList>(raw)
            .expect("parse")
            .builds;
        let err = select_build(&builds, "1.21.11").expect_err("应报 NotFound");
        assert!(matches!(err, SourceError::NotFound(_)), "{err:?}");
    }

    #[test]
    fn version_list_marks_leaf_release() {
        let mapped: Vec<_> = project()
            .versions
            .into_iter()
            .map(into_server_version)
            .collect();
        assert!(mapped.iter().all(|v| v.variant == ServerVariant::Leaf));
        assert!(mapped.iter().all(|v| v.kind == VersionKind::Release));
        assert!(mapped.iter().any(|v| v.id.as_str() == "1.21.11"));
    }

    /// `latest` 取版本列表末项
    #[test]
    fn latest_version_takes_last_entry() {
        let project = project();
        assert_eq!(project.versions.last().map(String::as_str), Some("26.3"));
    }

    #[test]
    fn fixed_picks_are_exposed_verbatim() {
        let source = LeafSource::builder()
            .picks(Picks::Fixed { build: 178 })
            .build()
            .expect("leaf source");
        assert_eq!(source.picks, Picks::Fixed { build: 178 });
    }
}
