//! Quilt 官方 Meta 服务端来源
//!
//! 版本口径与 Fabric 一致：一个版本就是一个 Minecraft 版本（如 `1.21.1`），该 MC
//! 版本下用哪个 loader 由本来源挑选（最新稳定版）。`artifact(&VersionId)` 只吃
//! 一个 MC 版本号，`latest` / `latest-snapshot` 别名照旧由来源解析。
//!
//! # 与 Fabric 的关系
//!
//! Quilt 的 Meta 服务是从 Fabric 的实现 fork 出来的，路径与字段高度相似，但：
//!
//! - Quilt 的 `/v3/versions/loader/{game}/{loader}/{installer}/server/jar`
//!   实测不稳定，本项目改用官方安装器。
//! - Fabric 的 loader 列表带独立嵌套，Quilt 的 `/v3/versions/loader` 是一维数组。
//!
//! 共享的是更底层的 [`crate::shared`] 脚手架，而非 Fabric 来源本身。
//!
//! # 安装器
//!
//! ```text
//! GET https://quiltmc.org/api/v1/download-latest-installer/java-universal
//! ```
//!
//! 这是「最新安装器」的**通用地址**：不带版本号，也没有侧车哈希。因此：
//!
//! - `InstallKind::Installer`——由 service 执行 `java -jar {installer} install server .`。
//! - [`Checksum::unverified`](domain::server::artifact::Checksum::unverified)，
//!   `size` 为 `0` 由下载结果覆盖。
//! - 同一次解析里不同 MC 版本拿到的是**同一个安装器**；差异由安装器参数（MC 版本
//!   + loader 版本）表达，这也正是安装器形态的语义。
//!
//! # 上游约束（实测）
//!
//! - `GET {meta}/v3/versions/loader` → `[{"version":"0.31.0-beta.4",...}]`，新在前。
//! - `GET {meta}/v3/versions/game` → `[{"version":"26.3","stable":true}]`，新在前。
//! - 两处都**无**分页参数；不存在的路径返回 `404`。

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

use super::manifest::{GameVersion, LoaderVersion};

/// Quilt 官方 Meta 根地址
pub(crate) const OFFICIAL_META: &str = "https://meta.quiltmc.org";

/// Quilt 官方安装器的「最新」地址（不带版本号）
pub(crate) const OFFICIAL_INSTALLER: &str =
    "https://quiltmc.org/api/v1/download-latest-installer/java-universal";

/// 挑选 loader 的口径
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Picks {
    /// 最新稳定 loader（无 `-` 后缀），没有稳定版时取列表首项
    #[default]
    LatestStable,
    /// 指定 loader 版本号
    Fixed {
        /// loader 版本号（如 `0.31.0-beta.4`）
        loader: String,
    },
}

/// Quilt 服务端来源
///
/// [`QuiltSource::new`] 对接官方；镜像通过 [`QuiltSource::builder`] 改
/// `meta_base` / `installer_url`。
pub struct QuiltSource {
    http: Client,
    /// Meta 根地址（不带尾斜杠），官方或镜像
    meta_base: String,
    /// 安装器下载地址（官方或镜像）
    installer_url: String,
    picks: Picks,
}

impl fmt::Debug for QuiltSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuiltSource")
            .field("meta_base", &self.meta_base)
            .field("installer_url", &self.installer_url)
            .field("picks", &self.picks)
            .finish_non_exhaustive()
    }
}

impl QuiltSource {
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
    pub fn builder() -> QuiltSourceBuilder {
        QuiltSourceBuilder::default()
    }

    async fn get<T>(&self, url: &str, not_found: Option<VersionId>) -> Result<T, SourceError>
    where
        T: DeserializeOwned,
    {
        get_json(&self.http, url, not_found).await
    }

    /// 全部 loader 版本地址
    fn loaders_url(&self) -> String {
        format!("{}/v3/versions/loader", self.meta_base)
    }

    /// 全部 MC 版本地址
    fn games_url(&self) -> String {
        format!("{}/v3/versions/game", self.meta_base)
    }

    async fn games(&self) -> Result<Vec<GameVersion>, SourceError> {
        self.get(&self.games_url(), None).await
    }

    async fn loaders(&self) -> Result<Vec<LoaderVersion>, SourceError> {
        self.get(&self.loaders_url(), None).await
    }

    /// 版本标识 → 具体 MC 版本（含 `latest` / `latest-snapshot` 别名）
    async fn resolve_mc(&self, version: &VersionId) -> Result<String, SourceError> {
        let games = self.games().await?;
        let candidates: Vec<&str> = games.iter().map(|game| game.version.as_str()).collect();
        let intent = VersionIntent::of(version);
        // `latest` 取首个 stable；`latest-snapshot` 取首个非 stable
        pick_by_version_aliases(
            intent,
            candidates.iter().copied(),
            |candidate| {
                games
                    .iter()
                    .any(|game| game.version == candidate && game.stable)
            },
            |candidate| {
                games
                    .iter()
                    .any(|game| game.version == candidate && !game.stable)
            },
        )
        .ok_or_else(|| SourceError::NotFound(version.clone()))
    }

    /// 在某 MC 版本下选 loader 版本
    ///
    /// `Picks::Fixed` 只校验「形状非空」，不校验是否在上游列表里：指定 loader 的
    /// 场景往往是上游刚发布了列表还没同步的版本。
    async fn pick_loader(&self) -> Result<String, SourceError> {
        if let Picks::Fixed { loader } = &self.picks {
            if loader.is_empty() {
                return Err(SourceError::Decode(Box::new(std::io::Error::other(
                    "quilt loader version must not be empty",
                ))));
            }
            return Ok(loader.clone());
        }
        let loaders = self.loaders().await?;
        select_loader(&loaders).ok_or_else(|| {
            SourceError::Decode(Box::new(std::io::Error::other(
                "quilt returned no loader versions",
            )))
        })
    }
}

#[async_trait]
impl ServerJarSource for QuiltSource {
    fn variant(&self) -> ServerVariant {
        ServerVariant::Quilt
    }

    fn name(&self) -> &'static str {
        "quilt"
    }

    async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError> {
        Ok(self
            .games()
            .await?
            .into_iter()
            .map(into_server_version)
            .collect())
    }

    async fn artifact(&self, version: &VersionId) -> Result<port::ResolvedArtifact, SourceError> {
        // 先解析 MC 版本：不存在的版本应在打安装器之前报 `NotFound`，
        // 而不是给出一个「装哪儿都行」的安装器地址让下游失败
        let mc = self.resolve_mc(version).await?;
        let _loader = self.pick_loader().await?;
        let location = parse_https_url(&self.installer_url)?;
        // Quilt 元数据不声明 Java 要求，按 MC 版本推算（与 Forge 同口径）
        let java_major = domain::server::version::required_java_major(&mc);
        Ok(Downloads::installer(location)
            .java_major(java_major)
            .resolve(version, None))
    }
}

/// 从 loader 列表里选一个：首个稳定版，没有则取首项（列表新在前）
///
/// 「没有稳定版就取首项」而不是报错：Quilt 长期只有 `-beta` loader，报错等于
/// 整个变体不可用。
fn select_loader(loaders: &[LoaderVersion]) -> Option<String> {
    loaders
        .iter()
        .find(|loader| loader.is_stable())
        .or_else(|| loaders.first())
        .map(|loader| loader.version.clone())
}

/// MC 版本 → 领域版本
///
/// `stable` 映射为 [`VersionKind::Release`]，否则 [`VersionKind::Snapshot`]。
fn into_server_version(game: GameVersion) -> ServerVersion {
    ServerVersion {
        id: VersionId::new(game.version),
        kind: if game.stable {
            VersionKind::Release
        } else {
            VersionKind::Snapshot
        },
        variant: ServerVariant::Quilt,
        java_major: None,
    }
}

/// [`QuiltSource`] 的构造器
#[derive(Debug, Default)]
pub struct QuiltSourceBuilder {
    meta_base: Option<String>,
    installer_url: Option<String>,
    picks: Option<Picks>,
}

impl QuiltSourceBuilder {
    /// 换 Meta 根地址（镜像）
    ///
    /// 地址需与官方同构（`/v3/versions/...`）。只接受 `https`。
    #[must_use]
    pub fn meta_base(mut self, base: impl Into<String>) -> Self {
        self.meta_base = Some(base.into());
        self
    }

    /// 换安装器下载地址（镜像）
    ///
    /// 镜像需提供同构的「最新安装器」端点。只接受 `https`。
    #[must_use]
    pub fn installer_url(mut self, url: impl Into<String>) -> Self {
        self.installer_url = Some(url.into());
        self
    }

    /// 接管 loader 选择策略
    #[must_use]
    pub fn picks(mut self, picks: Picks) -> Self {
        self.picks = Some(picks);
        self
    }

    /// 装配来源
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败，或地址非 `https` 时返回 [`SourceError::Init`]
    pub fn build(self) -> Result<QuiltSource, SourceError> {
        let installer_url = self
            .installer_url
            .unwrap_or_else(|| OFFICIAL_INSTALLER.to_owned());
        // 构造期就校验安装器地址：它是「不带版本号」的固定端点，配错只会在
        // 下载时暴露，届时已经解析完版本、浪费时间
        parse_https_url(&installer_url).map_err(|err| match err {
            SourceError::Decode(inner) => SourceError::Init(inner),
            other => other,
        })?;
        Ok(QuiltSource {
            http: build_client()?,
            meta_base: normalize_base(self.meta_base, OFFICIAL_META)?,
            installer_url,
            picks: self.picks.unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOADER: &str = include_str!("../../tests/fixtures/quilt_loader.json");
    const GAME: &str = include_str!("../../tests/fixtures/quilt_game.json");

    fn loaders() -> Vec<LoaderVersion> {
        serde_json::from_str(LOADER).expect("quilt_loader fixture")
    }

    fn games() -> Vec<GameVersion> {
        serde_json::from_str(GAME).expect("quilt_game fixture")
    }

    #[test]
    fn declares_quilt_variant() {
        let source = QuiltSource::new().expect("quilt source");
        assert_eq!(source.variant(), ServerVariant::Quilt);
        assert_eq!(source.name(), "quilt");
    }

    #[test]
    fn builds_endpoints() {
        let source = QuiltSource::new().expect("quilt source");
        assert_eq!(
            source.loaders_url(),
            "https://meta.quiltmc.org/v3/versions/loader"
        );
        assert_eq!(
            source.games_url(),
            "https://meta.quiltmc.org/v3/versions/game"
        );
        assert_eq!(
            source.installer_url,
            "https://quiltmc.org/api/v1/download-latest-installer/java-universal"
        );
    }

    #[test]
    fn trims_trailing_slash_on_mirror_base() {
        let source = QuiltSource::builder()
            .meta_base("https://mirror.example/meta/")
            .installer_url("https://mirror.example/quilt-installer.jar")
            .build()
            .expect("quilt source");
        assert_eq!(
            source.games_url(),
            "https://mirror.example/meta/v3/versions/game"
        );
        assert_eq!(
            source.installer_url,
            "https://mirror.example/quilt-installer.jar"
        );
    }

    #[test]
    fn rejects_insecure_base() {
        let err = QuiltSource::builder()
            .meta_base("http://mirror.example/meta")
            .build();
        assert!(matches!(err, Err(SourceError::Init(_))));
    }

    /// 安装器地址在构造期校验
    #[test]
    fn rejects_insecure_installer() {
        let err = QuiltSource::builder()
            .installer_url("http://mirror.example/quilt-installer.jar")
            .build();
        assert!(matches!(err, Err(SourceError::Init(_))), "{err:?}");
    }

    /// 选 loader：稳定版优先
    #[test]
    fn select_loader_prefers_stable() {
        assert_eq!(select_loader(&loaders()).as_deref(), Some("0.30.1"));
    }

    /// 没有稳定版时取列表首项
    #[test]
    fn select_loader_falls_back_to_first() {
        let only_beta = vec![
            LoaderVersion {
                version: "0.31.0-beta.4".to_owned(),
            },
            LoaderVersion {
                version: "0.31.0-beta.3".to_owned(),
            },
        ];
        assert_eq!(select_loader(&only_beta).as_deref(), Some("0.31.0-beta.4"));
        assert_eq!(select_loader(&[]), None);
    }

    /// 版本列表：stable → Release，其余 → Snapshot
    #[test]
    fn version_list_maps_stable_flag_to_kind() {
        let mapped: Vec<_> = games().into_iter().map(into_server_version).collect();
        assert!(mapped.iter().all(|v| v.variant == ServerVariant::Quilt));
        let stable = mapped
            .iter()
            .find(|v| v.id.as_str() == "26.3")
            .expect("26.3");
        assert_eq!(stable.kind, VersionKind::Release);
        let snapshot = mapped
            .iter()
            .find(|v| v.id.as_str() == "26.3-rc-3")
            .expect("26.3-rc-3");
        assert_eq!(snapshot.kind, VersionKind::Snapshot);
    }

    /// 固定 loader 直接透传，空串被拒
    #[test]
    fn fixed_loader_is_exposed_verbatim() {
        let source = QuiltSource::builder()
            .picks(Picks::Fixed {
                loader: "0.31.0-beta.4".to_owned(),
            })
            .build()
            .expect("quilt source");
        assert_eq!(
            source.picks,
            Picks::Fixed {
                loader: "0.31.0-beta.4".to_owned()
            }
        );
    }
}
