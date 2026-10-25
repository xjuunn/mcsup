//! `SpongeVanilla` 官方 Maven 服务端来源
//!
//! `SpongeVanilla` 是 Sponge 混合服务端的「官方安装器版」（`SpongeForge` /
//! `SpongeNeo` 才是模组形态）。它发布在 Sponge 自己的 Nexus Maven 仓库，元数据
//! 是 **XML**。
//!
//! # 版本口径（与其他来源不同）
//!
//! 其他变体都是「一个 MC 版本 → 该版本下挑 build」，`VersionId` 里放 MC 版本号。
//! `SpongeVanilla` 的上游版本串本身就是 `{mc}-{sponge}`（如
//! `26.3-21.0.0-RC2755`），且**同一个 MC 版本有多个 Sponge 实现版本**。因此这里
//! 直接把上游版本串当作 `VersionId`：
//!
//! - `versions()` 返回全部上游版本串；
//! - `latest` 取 `<release>`（没有则 `<latest>`）；
//! - `latest-snapshot` 取 `<latest>`；
//! - 字面量必须是上游列出的版本串。
//!
//! 这样「按 MC 版本还是按 Sponge 版本安装」的差异留给调用方：`artifact` 只认
//! 上游版本串，`series` 里想要 `26.3` 的某一版就传完整串。用 MC 版本号当
//! `VersionId` 会把「同一个 MC 有多个 Sponge 实现」压扁成一个，丢掉用户的选择权。
//!
//! # 上游约束（实测）
//!
//! - 元数据：`{repo}/org/spongepowered/spongevanilla/maven-metadata.xml`（XML，
//!   3000+ 个版本，**不保证顺序**）。
//! - JAR：`{repo}/org/spongepowered/spongevanilla/{ver}/spongevanilla-{ver}.jar`
//!   （`HEAD` 200，`Content-Type: application/java-archive`）。
//! - `.sha1` 侧车：`同路径 + .sha1`，纯 40 位十六进制（实测
//!   `422eb9ab34e0b56e1b7dc2879c18e84686ab0c2d`），可强校验。
//! - JAR 是**直接可运行**的（`InstallKind::Direct`），不需要安装器。
//!
//! # 只有 vanilla 一种
//!
//! `spongeforge` / `spongeneo` 是模组形态（要配合 `Forge` / `NeoForge`），
//! 启动方式不同，本来源只负责 `spongevanilla`。

use crate::http::{build_client, get_text};
use crate::shared::{Downloads, normalize_base, parse_https_url, parse_sha1_sidecar};
use async_trait::async_trait;
use domain::server::version::{ServerVariant, ServerVersion, VersionId, VersionKind};
use port::{ServerJarSource, SourceError};
use reqwest::Client;
use std::fmt;

use super::manifest::{Metadata, mc_of};

/// Sponge 官方 Maven 仓库根地址
pub(crate) const OFFICIAL_REPO: &str = "https://repo.spongepowered.org/repository/maven-public";

/// `spongevanilla` 在仓库里的路径
const ARTIFACT_PATH: &str = "org/spongepowered/spongevanilla";

/// `SpongeVanilla` 服务端来源
///
/// [`SpongeSource::new`] 对接官方；镜像通过 [`SpongeSource::builder`] 改 `repo_base`。
pub struct SpongeSource {
    http: Client,
    /// Maven 仓库根地址（不带尾斜杠），官方或镜像
    repo_base: String,
}

impl fmt::Debug for SpongeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpongeSource")
            .field("repo_base", &self.repo_base)
            .finish_non_exhaustive()
    }
}

impl SpongeSource {
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

    /// 构造器：改镜像仓库地址
    #[must_use]
    pub fn builder() -> SpongeSourceBuilder {
        SpongeSourceBuilder::default()
    }

    /// `maven-metadata.xml` 地址
    fn metadata_url(&self) -> String {
        format!("{}/{ARTIFACT_PATH}/maven-metadata.xml", self.repo_base)
    }

    /// JAR 地址
    fn jar_url(&self, version: &str) -> String {
        format!(
            "{}/{ARTIFACT_PATH}/{version}/spongevanilla-{version}.jar",
            self.repo_base
        )
    }

    /// `.sha1` 侧车地址
    fn sha1_url(&self, version: &str) -> String {
        format!("{}.sha1", self.jar_url(version))
    }

    /// 拉取并解析元数据
    async fn metadata(&self) -> Result<Metadata, SourceError> {
        let raw = get_text(&self.http, &self.metadata_url(), None).await?;
        Ok(Metadata::parse(&raw))
    }

    /// 读取 JAR 的 SHA1 侧车
    ///
    /// 侧车缺失或格式不符时**降级为无校验**而不是失败：Sponge 的发布流水线
    /// 偶尔漏传 `.sha1`，为此拒绝安装一个存在的 JAR 并不合理。
    async fn fetch_checksum(
        &self,
        version: &str,
    ) -> Result<domain::server::artifact::Checksum, SourceError> {
        let url = self.sha1_url(version);
        match get_text(&self.http, &url, None).await {
            Ok(text) => Ok(parse_sha1_sidecar(&text)
                .unwrap_or_else(domain::server::artifact::Checksum::unverified)),
            Err(SourceError::NotFound(_)) => Ok(domain::server::artifact::Checksum::unverified()),
            Err(err) => Err(err),
        }
    }

    /// 版本标识 → 具体上游版本串
    async fn resolve_version(&self, version: &VersionId) -> Result<String, SourceError> {
        let meta = self.metadata().await?;
        let raw = version.as_str();
        let resolved = match raw {
            "latest" => meta.release.clone().or_else(|| meta.latest.clone()),
            "latest-snapshot" => meta.latest.clone(),
            literal => meta
                .versions
                .iter()
                .find(|candidate| candidate.as_str() == literal)
                .cloned(),
        };
        resolved.ok_or_else(|| SourceError::NotFound(version.clone()))
    }
}

#[async_trait]
impl ServerJarSource for SpongeSource {
    fn variant(&self) -> ServerVariant {
        ServerVariant::SpongeVanilla
    }

    fn name(&self) -> &'static str {
        "spongevanilla"
    }

    async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError> {
        Ok(self
            .metadata()
            .await?
            .versions
            .into_iter()
            .map(into_server_version)
            .collect())
    }

    async fn artifact(&self, version: &VersionId) -> Result<port::ResolvedArtifact, SourceError> {
        let resolved = self.resolve_version(version).await?;
        let location = parse_https_url(&self.jar_url(&resolved))?;
        let checksum = self.fetch_checksum(&resolved).await?;
        Ok(Downloads::direct(location)
            .checksum(checksum)
            .resolve(version))
    }
}

/// 上游版本串 → 领域版本
///
/// `26.3-21.0.0-RC2755` 这类带 `RC` / `beta` 的串标 [`VersionKind::Snapshot`]，
/// 其余（`1.21.1-14.0.0`、`26.3-21.0.0`）标 [`VersionKind::Release`]。
/// 判定只看 Sponge 实现版本段的后缀——MC 版本段本身不会是预发布名。
fn into_server_version(version: String) -> ServerVersion {
    let kind = if is_prerelease(&version) {
        VersionKind::Snapshot
    } else {
        VersionKind::Release
    };
    ServerVersion {
        id: VersionId::new(version),
        kind,
        variant: ServerVariant::SpongeVanilla,
        java_major: None,
    }
}

/// 版本串是否为预发布（含 `-RC` / `-beta` / `-alpha` / `-pre`）
///
/// 只看第一个 `-` 之后的部分：MC 版本段（`26.3` / `1.21.1`）不含这些字样。
fn is_prerelease(version: &str) -> bool {
    let Some((_, sponge)) = version.split_once('-') else {
        return false;
    };
    let lower = sponge.to_ascii_lowercase();
    lower.contains("-rc")
        || lower.contains("-beta")
        || lower.contains("-alpha")
        || lower.contains("-pre")
}

/// `SpongeVanilla` 的 MC 版本轴（供上层按 MC 版本分组展示）
///
/// 上游一个 `VersionId` 是 `{mc}-{sponge}`，展示时通常只想看 MC 版本。本函数
/// 取第一个 `-` 之前的部分，公开出来供 CLI / 诊断层使用。
#[must_use]
pub fn mc_version_of(version: &str) -> &str {
    mc_of(version)
}

/// [`SpongeSource`] 的构造器
#[derive(Debug, Default)]
pub struct SpongeSourceBuilder {
    repo_base: Option<String>,
}

impl SpongeSourceBuilder {
    /// 换 Maven 仓库根地址（镜像）
    ///
    /// 地址需与官方同构（`/org/spongepowered/spongevanilla/...`）。只接受 `https`。
    #[must_use]
    pub fn repo_base(mut self, base: impl Into<String>) -> Self {
        self.repo_base = Some(base.into());
        self
    }

    /// 装配来源
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败，或 `repo_base` 非 `https` 时返回 [`SourceError::Init`]
    pub fn build(self) -> Result<SpongeSource, SourceError> {
        Ok(SpongeSource {
            http: build_client()?,
            repo_base: normalize_base(self.repo_base, OFFICIAL_REPO)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/sponge_metadata.xml");

    #[test]
    fn declares_sponge_variant() {
        let source = SpongeSource::new().expect("sponge source");
        assert_eq!(source.variant(), ServerVariant::SpongeVanilla);
        assert_eq!(source.name(), "spongevanilla");
    }

    #[test]
    fn builds_endpoints() {
        let source = SpongeSource::new().expect("sponge source");
        let base = "https://repo.spongepowered.org/repository/maven-public/org/spongepowered/spongevanilla";
        assert_eq!(source.metadata_url(), format!("{base}/maven-metadata.xml"));
        assert_eq!(
            source.jar_url("26.3-21.0.0-RC2755"),
            format!("{base}/26.3-21.0.0-RC2755/spongevanilla-26.3-21.0.0-RC2755.jar")
        );
        assert_eq!(
            source.sha1_url("26.3-21.0.0-RC2755"),
            format!("{base}/26.3-21.0.0-RC2755/spongevanilla-26.3-21.0.0-RC2755.jar.sha1")
        );
    }

    #[test]
    fn trims_trailing_slash_on_mirror_base() {
        let source = SpongeSource::builder()
            .repo_base("https://mirror.example/maven/")
            .build()
            .expect("sponge source");
        assert_eq!(
            source.jar_url("1.21.1-14.0.0"),
            "https://mirror.example/maven/org/spongepowered/spongevanilla/1.21.1-14.0.0/spongevanilla-1.21.1-14.0.0.jar"
        );
    }

    #[test]
    fn rejects_insecure_base() {
        let err = SpongeSource::builder()
            .repo_base("http://mirror.example/maven")
            .build();
        assert!(matches!(err, Err(SourceError::Init(_))));
    }

    /// 元数据解析：版本列表原样保留
    #[test]
    fn metadata_yields_all_versions() {
        let meta = Metadata::parse(FIXTURE);
        assert_eq!(meta.versions.len(), 5);
    }

    /// 预发布判定只认 Sponge 段后缀
    #[test]
    fn prerelease_detection_looks_at_sponge_segment() {
        assert!(is_prerelease("26.3-21.0.0-RC2755"));
        assert!(is_prerelease("1.21.1-14.0.0-beta"));
        assert!(!is_prerelease("1.21.1-14.0.0"));
        assert!(!is_prerelease("26.3-21.0.0"));
        // 无 `-` 的极老串按正式版处理
        assert!(!is_prerelease("1.21.1"));
    }

    /// 版本列表：预发布 → Snapshot，其余 → Release
    #[test]
    fn version_list_maps_prerelease_to_snapshot() {
        let mapped: Vec<_> = Metadata::parse(FIXTURE)
            .versions
            .into_iter()
            .map(into_server_version)
            .collect();
        assert!(
            mapped
                .iter()
                .all(|v| v.variant == ServerVariant::SpongeVanilla)
        );
        // fixture 全是 RC：都应标 Snapshot
        assert!(mapped.iter().all(|v| v.kind == VersionKind::Snapshot));
        assert!(mapped.iter().any(|v| v.id.as_str() == "26.3-21.0.0-RC2755"));
    }

    /// MC 版本轴：第一个 `-` 之前
    #[test]
    fn mc_axis_is_prefix_before_first_dash() {
        assert_eq!(mc_version_of("26.3-21.0.0-RC2755"), "26.3");
        assert_eq!(mc_version_of("1.21.1"), "1.21.1");
    }
}
