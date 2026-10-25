//! Purpur 官方 API 服务端来源
//!
//! Purpur 是 Paper 的下游分支（性能与可配置性增强），但**发布渠道独立**：它不
//! 进 `PaperMC` Fill，而是自有 `api.purpurmc.org/v2`。因此这里单列一个来源，
//! 而不是复用 [`crate::papermc`]——两者的响应结构、版本顺序、哈希能力都不同，
//! 强行合并只会让 Paper 来源里塞满 `if purpur` 分支。
//!
//! # 版本口径
//!
//! 一个版本就是一个 Minecraft 版本（如 `1.21.1`），该 MC 版本下用哪个 build
//! 由本来源挑选（`builds.latest`，上游自己判定的最新）。`latest` 取版本列表
//! 末项（上游「旧在前」）。
//!
//! # 上游约束（实测）
//!
//! - `GET {base}/purpur` → `{"project":"purpur","versions":[...]}`，**旧在前**。
//! - `GET {base}/purpur/{version}` → `{"version":"1.21.1","builds":{"latest":"2329","all":[...]}}`。
//! - `GET {base}/purpur/{version}/{build}/download` 直接返回 JAR（`Content-Length`
//!   约 53 MB），响应头 `Content-Disposition` 给出文件名
//!   `purpur-1.21.1-2329.jar`。
//! - **无校验和**：build 详情无哈希字段，`?hash=md5` 返回 `405`。只能
//!   [`Checksum::unverified`]——见 [`PurpurSource::new`] 的说明。
//!
//! # 为什么不自行拼下载文件名
//!
//! 下载走 `/{version}/{build}/download` 这个端点，文件名由上游 `Content-Disposition`
//! 决定。本来源只给端点地址，不拼 `purpur-{v}-{b}.jar`——上游改命名时拼出来的
//! 地址就 404 了。

use crate::http::{build_client, get_json};
use crate::shared::{
    Downloads, VersionIntent, normalize_base, parse_https_url, pick_by_version_aliases,
};
use async_trait::async_trait;
use domain::server::version::{ServerVariant, ServerVersion, VersionId, VersionKind};
use port::{ServerJarSource, SourceError};
use reqwest::Client;
use serde::de::DeserializeOwned;
use std::fmt;

use super::manifest::{BuildIndex, VersionIndex};

/// Purpur 官方 API 根地址
pub(crate) const OFFICIAL_API: &str = "https://api.purpurmc.org/v2";

/// 挑选 build 的口径
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Picks {
    /// 该版本的最新 build（`builds.latest`）
    #[default]
    Latest,
    /// 指定 build 号
    Fixed {
        /// build 号（字符串形式，与上游一致）
        build: String,
    },
}

/// Purpur 服务端来源
///
/// [`PurpurSource::new`] 对接官方；镜像通过 [`PurpurSource::builder`] 改
/// `api_base`。
pub struct PurpurSource {
    http: Client,
    /// API 根地址（不带尾斜杠），官方或镜像
    api_base: String,
    picks: Picks,
}

impl fmt::Debug for PurpurSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PurpurSource")
            .field("api_base", &self.api_base)
            .field("picks", &self.picks)
            .finish_non_exhaustive()
    }
}

impl PurpurSource {
    /// 对接官方的默认实例
    ///
    /// 构造不打网络，只准备 HTTP 客户端。
    ///
    /// # 校验和能力
    ///
    /// Purpur 不发布任何校验和（build 详情无哈希，`?hash=md5` 返回 `405`），
    /// 因此本来源产出的 [`ResolvedArtifact`](port::ResolvedArtifact) 恒为
    /// [`Checksum::unverified`](domain::server::artifact::Checksum::unverified)，
    /// `size` 为 `0` 由下载结果覆盖。
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败时返回 [`SourceError::Init`]
    pub fn new() -> Result<Self, SourceError> {
        Self::builder().build()
    }

    /// 构造器：改镜像地址或选版策略
    #[must_use]
    pub fn builder() -> PurpurSourceBuilder {
        PurpurSourceBuilder::default()
    }

    async fn get<T>(&self, url: &str, not_found: Option<VersionId>) -> Result<T, SourceError>
    where
        T: DeserializeOwned,
    {
        get_json(&self.http, url, not_found).await
    }

    /// 版本列表地址
    fn versions_url(&self) -> String {
        format!("{}/purpur", self.api_base)
    }

    /// 某版本的 build 列表地址
    fn builds_url(&self, version: &str) -> String {
        format!("{}/purpur/{version}", self.api_base)
    }

    /// 某 build 的下载地址
    fn download_url(&self, version: &str, build: &str) -> String {
        format!("{}/purpur/{version}/{build}/download", self.api_base)
    }

    /// 拉取版本索引
    ///
    /// 镜像把项目路径答错时这里报错，而不是把别的项目的版本当成 purpur 的。
    async fn index(&self) -> Result<VersionIndex, SourceError> {
        let index: VersionIndex = self.get(&self.versions_url(), None).await?;
        if !index.is_purpur() {
            return Err(SourceError::Decode(Box::new(std::io::Error::other(
                format!(
                    "purpur api returned project `{}` for requested `purpur`",
                    index.project
                ),
            ))));
        }
        Ok(index)
    }

    /// 版本标识 → 具体版本（含 `latest` / `latest-snapshot` 别名）
    ///
    /// Purpur 只发布正式版本，没有快照标记：`latest-snapshot` 与 `latest` 同口径
    /// （取末项）。
    async fn resolve_version(&self, version: &VersionId) -> Result<String, SourceError> {
        let index = self.index().await?;
        let versions: Vec<&str> = index.versions.iter().map(String::as_str).collect();
        pick_by_version_aliases(VersionIntent::of(version), versions, |_| false, |_| false)
            .or_else(|| match VersionIntent::of(version) {
                VersionIntent::Literal(raw) => index
                    .versions
                    .iter()
                    .find(|candidate| candidate.as_str() == raw)
                    .cloned(),
                VersionIntent::Latest | VersionIntent::LatestSnapshot => {
                    index.versions.last().cloned()
                }
            })
            .ok_or_else(|| SourceError::NotFound(version.clone()))
    }

    /// 在某版本下选 build 号
    async fn pick_build(&self, version: &str) -> Result<String, SourceError> {
        if let Picks::Fixed { build } = &self.picks {
            return Ok(build.clone());
        }
        let index: BuildIndex = self
            .get(&self.builds_url(version), Some(VersionId::new(version)))
            .await?;
        if !index.matches_version(version) {
            return Err(SourceError::Decode(Box::new(std::io::Error::other(
                format!(
                    "purpur api returned version `{}` for requested `{version}`",
                    index.version
                ),
            ))));
        }
        index
            .builds
            .latest()
            .map(str::to_owned)
            .ok_or_else(|| SourceError::NotFound(VersionId::new(version)))
    }
}

#[async_trait]
impl ServerJarSource for PurpurSource {
    fn variant(&self) -> ServerVariant {
        ServerVariant::Purpur
    }

    fn name(&self) -> &'static str {
        "purpur"
    }

    async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError> {
        Ok(self
            .index()
            .await?
            .versions
            .into_iter()
            .map(into_server_version)
            .collect())
    }

    async fn artifact(&self, version: &VersionId) -> Result<port::ResolvedArtifact, SourceError> {
        let resolved = self.resolve_version(version).await?;
        let build = self.pick_build(&resolved).await?;
        if !is_valid_build(&build) {
            return Err(SourceError::Decode(Box::new(std::io::Error::other(
                format!("invalid purpur build `{build}`"),
            ))));
        }
        let location = parse_https_url(&self.download_url(&resolved, &build))?;
        // 上游无哈希：`Downloads::direct` 的默认就是 unverified + size 0
        Ok(Downloads::direct(location).resolve(version))
    }
}

/// build 号是否为合法形状：纯 ASCII 数字
///
/// build 号会直接进 URL 路径，必须挡住空串与含 `/`、`..` 的输入（固定 build
/// 由宿主注入，不可信）。
fn is_valid_build(build: &str) -> bool {
    !build.is_empty() && build.chars().all(|c| c.is_ascii_digit())
}

/// 版本号 → 领域版本
///
/// Purpur 只覆盖正式 MC 版本，统一标 `Release`；Java 大版本由原版元数据给出。
fn into_server_version(id: String) -> ServerVersion {
    ServerVersion {
        id: VersionId::new(id),
        kind: VersionKind::Release,
        variant: ServerVariant::Purpur,
        java_major: None,
    }
}

/// [`PurpurSource`] 的构造器
#[derive(Debug, Default)]
pub struct PurpurSourceBuilder {
    api_base: Option<String>,
    picks: Option<Picks>,
}

impl PurpurSourceBuilder {
    /// 换 API 根地址（镜像）
    ///
    /// 地址需与官方同构（`/purpur/...`）。只接受 `https`，尾斜杠会被去掉。
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
    pub fn build(self) -> Result<PurpurSource, SourceError> {
        Ok(PurpurSource {
            http: build_client()?,
            api_base: normalize_base(self.api_base, OFFICIAL_API)?,
            picks: self.picks.unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERSIONS: &str = include_str!("../../tests/fixtures/purpur_versions.json");
    const BUILDS: &str = include_str!("../../tests/fixtures/purpur_builds.json");

    fn index() -> VersionIndex {
        serde_json::from_str(VERSIONS).expect("purpur_versions fixture")
    }

    fn build_index() -> BuildIndex {
        serde_json::from_str(BUILDS).expect("purpur_builds fixture")
    }

    #[test]
    fn declares_purpur_variant() {
        let source = PurpurSource::new().expect("purpur source");
        assert_eq!(source.variant(), ServerVariant::Purpur);
        assert_eq!(source.name(), "purpur");
    }

    #[test]
    fn builds_endpoints() {
        let source = PurpurSource::new().expect("purpur source");
        assert_eq!(source.versions_url(), "https://api.purpurmc.org/v2/purpur");
        assert_eq!(
            source.builds_url("1.21.1"),
            "https://api.purpurmc.org/v2/purpur/1.21.1"
        );
        assert_eq!(
            source.download_url("1.21.1", "2329"),
            "https://api.purpurmc.org/v2/purpur/1.21.1/2329/download"
        );
    }

    #[test]
    fn trims_trailing_slash_on_mirror_base() {
        let source = PurpurSource::builder()
            .api_base("https://mirror.example/v2/")
            .build()
            .expect("purpur source");
        assert_eq!(
            source.download_url("1.21.1", "2329"),
            "https://mirror.example/v2/purpur/1.21.1/2329/download"
        );
    }

    #[test]
    fn rejects_insecure_base() {
        let err = PurpurSource::builder()
            .api_base("http://mirror.example/v2")
            .build();
        assert!(matches!(err, Err(SourceError::Init(_))));
    }

    /// 最新 build 取上游 `builds.latest`
    #[test]
    fn latest_build_comes_from_upstream_field() {
        assert_eq!(build_index().builds.latest(), Some("2329"));
    }

    /// `latest` 版本取版本列表末项（上游旧在前）
    #[test]
    fn latest_version_takes_last_entry() {
        let index = index();
        assert_eq!(index.versions.last().map(String::as_str), Some("26.3"));
    }

    #[test]
    fn version_list_marks_purpur_release() {
        let mapped: Vec<_> = index()
            .versions
            .into_iter()
            .map(into_server_version)
            .collect();
        assert!(mapped.iter().all(|v| v.variant == ServerVariant::Purpur));
        assert!(mapped.iter().all(|v| v.kind == VersionKind::Release));
        assert!(mapped.iter().all(|v| v.java_major.is_none()));
        assert!(mapped.iter().any(|v| v.id.as_str() == "1.21.1"));
    }

    /// build 号必须是纯数字（会进 URL 路径）
    #[test]
    fn build_number_must_be_numeric() {
        assert!(is_valid_build("2329"));
        assert!(!is_valid_build(""));
        assert!(!is_valid_build("../etc/passwd"));
        assert!(!is_valid_build("2329a"));
    }

    /// 固定 build 直接透传，不经上游选版
    #[test]
    fn fixed_picks_are_exposed_verbatim() {
        let source = PurpurSource::builder()
            .picks(Picks::Fixed {
                build: "2300".to_owned(),
            })
            .build()
            .expect("purpur source");
        assert_eq!(
            source.picks,
            Picks::Fixed {
                build: "2300".to_owned()
            }
        );
    }
}
