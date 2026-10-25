//! Fabric 官方 Meta 服务端 JAR 来源
//!
//! 版本口径：一版本即一个 Minecraft 版本（如 `1.21.1`），loader 与安装器由本
//! 来源在该 MC 版本下自动挑选最新稳定版。这样 Fabric 与原版在
//! [`ServerJarSource`] 上的语义完全一致——`artifact(&VersionId)` 仍只吃一个
//! 版本号，`latest` / `latest-snapshot` 别名也照旧由来源解析。
//!
//! 服务端 JAR 地址（官方唯一入口，见 Meta 路由表）：
//!
//! ```text
//! GET /v2/versions/loader/{game}/{loader}/{installer}/server/jar
//! ```
//!
//! 该端点返回完整可运行的 Fabric 服务端核心，无需另行下载原版再装配。
//!
//! # 上游约束
//!
//! - 不存在的 MC 版本返回 `400` + `[]`，不是 `404`；映射为
//!   [`SourceError::NotFound`] 交由调用方判定。
//! - 服务端 JAR 走 chunked 传输，既无 `Content-Length` 也不提供哈希文件，
//!   因此解析结果为 [`Checksum::unverified`]，大小取 `0` 并由下载结果覆盖。

use crate::http::{build_client, get_json};
use async_trait::async_trait;
use domain::server::{
    artifact::Checksum,
    version::{ServerVariant, ServerVersion, VersionId},
};
use port::{ResolvedArtifact, ServerJarSource, SourceError};
use reqwest::Client;
use serde::de::DeserializeOwned;
use std::fmt;
use url::Url;

use super::manifest::{GameVersion, InstallerVersion, LoaderVersion};

/// Fabric 官方 Meta 根地址
pub(crate) const OFFICIAL_META: &str = "https://meta.fabricmc.net";

/// 挑选 loader / 安装器版本的口径
///
/// 允许调用方接管选择策略（固定版本、按实验版、灰度），而不必重写整个来源。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Picks {
    /// 最新稳定版；无稳定版时退回列表首项（最新）
    #[default]
    LatestStable,
    /// 指定版本号，上游不存在时报 [`SourceError::NotFound`]
    Fixed {
        /// loader 版本号
        loader: String,
        /// 安装器版本号
        installer: String,
    },
}

/// Fabric 服务端来源
///
/// 用 [`FabricSource::new`] 得到对接官方的默认实例；第三方镜像通过
/// [`FabricSource::builder`] 改 `base_url`，自定义版本策略改 `picks`。
pub struct FabricSource {
    http: Client,
    /// Meta 根地址（不带尾斜杠），官方或镜像
    base_url: String,
    picks: Picks,
}

impl fmt::Debug for FabricSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FabricSource")
            .field("base_url", &self.base_url)
            .field("picks", &self.picks)
            .finish_non_exhaustive()
    }
}

impl FabricSource {
    /// 对接官方 Meta 的默认实例
    ///
    /// 构造不打网络，只准备 HTTP 客户端。
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败时返回 [`SourceError::Init`]
    pub fn new() -> Result<Self, SourceError> {
        Self::builder().build()
    }

    /// 构造器：改镜像地址或版本策略
    #[must_use]
    pub fn builder() -> FabricSourceBuilder {
        FabricSourceBuilder::default()
    }

    async fn get<T>(&self, url: &str, not_found: Option<VersionId>) -> Result<T, SourceError>
    where
        T: DeserializeOwned,
    {
        get_json(&self.http, url, not_found).await
    }

    /// 服务端 JAR 的下载地址
    fn server_jar_url(&self, game: &str, loader: &str, installer: &str) -> String {
        format!(
            "{}/v2/versions/loader/{game}/{loader}/{installer}/server/jar",
            self.base_url
        )
    }
}

#[async_trait]
impl ServerJarSource for FabricSource {
    fn variant(&self) -> ServerVariant {
        ServerVariant::Fabric
    }

    fn name(&self) -> &'static str {
        "fabric"
    }

    async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError> {
        let url = format!("{}/v2/versions/game", self.base_url);
        let games: Vec<GameVersion> = self.get(&url, None).await?;
        Ok(games
            .into_iter()
            .map(|game| ServerVersion {
                id: VersionId::new(game.version),
                kind: game_kind(game.stable),
                variant: ServerVariant::Fabric,
                // Fabric 只关心 loader 自身的最低 Java 要求，MC 版本要求的
                // Java 大版本由原版元数据给出，这里不做猜测
                java_major: None,
            })
            .collect())
    }

    async fn artifact(&self, version: &VersionId) -> Result<ResolvedArtifact, SourceError> {
        let game = self.resolve_game(version).await?;
        let (loader, installer) = self.pick_versions(&game).await?;
        let location = parse_https_url(&self.server_jar_url(&game, &loader, &installer))?;
        Ok(ResolvedArtifact {
            artifact: domain::server::artifact::JarArtifact {
                version: version.clone(),
                // 上游不给大小；下载完成后以实际字节数覆盖
                size: 0,
                checksum: Checksum::unverified(),
            },
            location,
            java_major: None,
        })
    }
}

impl FabricSource {
    /// 把 `latest` / `latest-snapshot` 别名解析为具体 MC 版本，并校验存在性
    ///
    /// 上游对不存在的版本返回 `400` 而非 `404`，直接拼进下载地址只会得到
    /// 语义模糊的状态码错误；这里先对列表做存在性检查，把「版本不存在」如实
    /// 报成 [`SourceError::NotFound`]，与 [`ServerJarSource`] 契约一致。
    async fn resolve_game(&self, version: &VersionId) -> Result<String, SourceError> {
        let url = format!("{}/v2/versions/game", self.base_url);
        let games: Vec<GameVersion> = self.get(&url, None).await?;
        resolve_game_id(version.as_str(), games)
            .ok_or_else(|| SourceError::NotFound(version.clone()))
    }

    /// 按 [`Picks`] 选出 loader 与安装器版本
    ///
    /// 两种策略都先确认版本在上游列表里：不存在的版本直接拼进下载地址只会
    /// 得到 `400` / `404` 这类语义模糊的状态码错误，提前判定能如实报成
    /// [`SourceError::NotFound`]。
    async fn pick_versions(&self, game: &str) -> Result<(String, String), SourceError> {
        let loaders: Vec<LoaderVersion> = self
            .get(&format!("{}/v2/versions/loader", self.base_url), None)
            .await?;
        let installers: Vec<InstallerVersion> = self
            .get(&format!("{}/v2/versions/installer", self.base_url), None)
            .await?;
        let game_id = VersionId::new(game);
        select_versions(&self.picks, &loaders, &installers).ok_or(SourceError::NotFound(game_id))
    }
}

/// 按 [`Picks`] 选出 loader 与安装器版本，任一缺失返回 `None`
///
/// 纯函数：不碰网络，便于覆盖「固定版本是否存在」这类分支。
fn select_versions(
    picks: &Picks,
    loaders: &[LoaderVersion],
    installers: &[InstallerVersion],
) -> Option<(String, String)> {
    match picks {
        Picks::Fixed { loader, installer } => {
            let has_loader = loaders.iter().any(|v| &v.version == loader);
            let has_installer = installers.iter().any(|v| &v.version == installer);
            (has_loader && has_installer).then(|| (loader.clone(), installer.clone()))
        }
        Picks::LatestStable => {
            let loader = pick_stable(loaders.iter().map(|v| (v.version.as_str(), v.stable)))?;
            let installer = pick_stable(installers.iter().map(|v| (v.version.as_str(), v.stable)))?;
            Some((loader, installer))
        }
    }
}

/// 选版本：优先稳定版，没有稳定版时退回列表首项
///
/// 上游按「新版本在前」返回，首项即最新；稳定版按同一顺序取第一个。
fn pick_stable<'a>(items: impl Iterator<Item = (&'a str, bool)> + Clone) -> Option<String> {
    let first = items.clone().next()?;
    let stable = items.filter(|(_, stable)| *stable).map(|(v, _)| v).next();
    stable.or(Some(first.0)).map(str::to_owned)
}

/// 把版本标识（含 `latest` / `latest-snapshot` 别名）解析为列表里的具体版本
///
/// 列表按「新版本在前」返回，因此 `find` 取到的就是最新的稳定 / 快照版本。
/// 字面版本必须出现在列表里才算命中：上游对不存在的版本返回 `400`，
/// 提前判定能把错误如实报成 `NotFound`。
fn resolve_game_id(raw: &str, games: Vec<GameVersion>) -> Option<String> {
    games
        .into_iter()
        .find(|game| match raw {
            "latest" => game.stable,
            "latest-snapshot" => !game.stable,
            other => game.version == other,
        })
        .map(|game| game.version)
}

/// `stable` 标记 → 领域版本类别
///
/// Fabric Meta 只区分稳定与快照，没有原版的「历史版本」档，因此非稳定版
/// 统一归为快照。
fn game_kind(stable: bool) -> domain::server::version::VersionKind {
    if stable {
        domain::server::version::VersionKind::Release
    } else {
        domain::server::version::VersionKind::Snapshot
    }
}

fn parse_https_url(raw: &str) -> Result<Url, SourceError> {
    let url = raw
        .parse::<Url>()
        .map_err(|e| SourceError::Decode(Box::new(UrlError::Parse(e))))?;
    if url.scheme() != "https" {
        return Err(SourceError::Decode(Box::new(UrlError::Insecure(
            url.scheme().to_owned(),
        ))));
    }
    Ok(url)
}

#[derive(Debug, thiserror::Error)]
enum UrlError {
    #[error("invalid URL: {0}")]
    Parse(#[source] url::ParseError),
    #[error("only https URLs are accepted, got schema {0:?}")]
    Insecure(String),
}

/// [`FabricSource`] 的构造器
///
/// 第三方镜像（如 BMCLAPI 的 `/fabric-meta`）只需换 [`Self::base_url`]；
/// 版本选择策略通过 [`Self::picks`] 接管。
#[derive(Debug, Default)]
pub struct FabricSourceBuilder {
    base_url: Option<String>,
    picks: Option<Picks>,
}

impl FabricSourceBuilder {
    /// 换 Meta 根地址（第三方镜像）
    ///
    /// 地址需与官方 v2 端点同构，例如
    /// `https://bmclapi2.bangbang93.com/fabric-meta`。只接受 `https`，
    /// 尾斜杠会被去掉。
    #[must_use]
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// 接管 loader / 安装器版本选择
    #[must_use]
    pub fn picks(mut self, picks: Picks) -> Self {
        self.picks = Some(picks);
        self
    }

    /// 装配来源
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败，或 `base_url` 非 `https` 时返回 [`SourceError::Init`]
    pub fn build(self) -> Result<FabricSource, SourceError> {
        let base_url = self
            .base_url
            .unwrap_or_else(|| OFFICIAL_META.to_owned())
            .trim_end_matches('/')
            .to_owned();
        let parsed = base_url
            .parse::<Url>()
            .map_err(|e| SourceError::Init(Box::new(UrlError::Parse(e))))?;
        if parsed.scheme() != "https" {
            return Err(SourceError::Init(Box::new(UrlError::Insecure(
                parsed.scheme().to_owned(),
            ))));
        }
        Ok(FabricSource {
            http: build_client()?,
            base_url,
            picks: self.picks.unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fabric::manifest::{GameVersion, InstallerVersion, LoaderVersion};
    use domain::server::version::VersionKind;

    const GAME: &str = include_str!("../../tests/fixtures/fabric_game.json");
    const LOADER: &str = include_str!("../../tests/fixtures/fabric_loader.json");
    const INSTALLER: &str = include_str!("../../tests/fixtures/fabric_installer.json");

    fn games() -> Vec<GameVersion> {
        serde_json::from_str(GAME).expect("fabric_game fixtures")
    }

    fn loaders() -> Vec<LoaderVersion> {
        serde_json::from_str(LOADER).expect("fabric_loader fixtures")
    }

    fn installers() -> Vec<InstallerVersion> {
        serde_json::from_str(INSTALLER).expect("fabric_installer fixtures")
    }

    /// 能力查询：Fabric 来源声明 fabric 变体
    #[test]
    fn declares_fabric_variant() {
        let source = FabricSource::new().expect("fabric source");
        assert_eq!(source.variant(), ServerVariant::Fabric);
        assert_eq!(source.name(), "fabric");
    }

    /// `stable` 标记映射为版本类别：稳定 → 正式版，快照 → 快照
    #[test]
    fn maps_stable_flag_to_version_kind() {
        assert_eq!(game_kind(true), VersionKind::Release);
        assert_eq!(game_kind(false), VersionKind::Snapshot);
    }

    /// 选版本优先稳定版，即使稳定版不在首位
    #[test]
    fn pick_stable_prefers_stable_over_list_head() {
        let versions = loaders();
        let picked = pick_stable(versions.iter().map(|v| (v.version.as_str(), v.stable)))
            .expect("应选出 loader");
        assert_eq!(picked, "0.20.1", "首项就是稳定版");
    }

    /// 列表里没有稳定版时退回首项（最新），而不是空手而归
    #[test]
    fn pick_stable_falls_back_to_head_without_stable() {
        let items = [("0.21.0-beta.1", false), ("0.20.0", false)];
        assert_eq!(
            pick_stable(items.iter().map(|(v, s)| (*v, *s))),
            Some("0.21.0-beta.1".to_owned())
        );
    }

    /// 空列表不选版本，交由上层报 `NotFound`
    #[test]
    fn pick_stable_returns_none_on_empty() {
        let items: [(&str, bool); 0] = [];
        assert_eq!(pick_stable(items.iter().map(|(v, s)| (*v, *s))), None);
    }

    /// 官方 installer 列表只有一项稳定版：必须选中它
    #[test]
    fn picks_only_stable_installer() {
        assert_eq!(
            pick_stable(installers().iter().map(|v| (v.version.as_str(), v.stable))),
            Some("1.1.2".to_owned())
        );
    }

    /// 服务端 JAR 端点拼装：官方唯一入口，三段版本缺一不可
    #[test]
    fn builds_server_jar_endpoint() {
        let source = FabricSource::new().expect("fabric source");
        assert_eq!(
            source.server_jar_url("1.21.1", "0.16.10", "1.0.1"),
            "https://meta.fabricmc.net/v2/versions/loader/1.21.1/0.16.10/1.0.1/server/jar"
        );
    }

    /// 镜像地址去尾斜杠，拼接不出现双斜杠
    #[test]
    fn trims_trailing_slash_on_mirror_base_url() {
        let source = FabricSource::builder()
            .base_url("https://bmclapi2.bangbang93.com/fabric-meta/")
            .build()
            .expect("fabric source");
        assert_eq!(
            source.server_jar_url("1.21.1", "0.16.10", "1.0.1"),
            "https://bmclapi2.bangbang93.com/fabric-meta/v2/versions/loader/1.21.1/0.16.10/1.0.1/server/jar"
        );
    }

    /// 自定义版本策略：固定 loader / installer 时直接用，不回落
    #[test]
    fn fixed_picks_are_exposed_verbatim() {
        let source = FabricSource::builder()
            .picks(Picks::Fixed {
                loader: "0.16.10".to_owned(),
                installer: "1.0.1".to_owned(),
            })
            .build()
            .expect("fabric source");
        assert_eq!(
            source.picks,
            Picks::Fixed {
                loader: "0.16.10".to_owned(),
                installer: "1.0.1".to_owned()
            }
        );
    }

    /// 固定版本存在于列表时如实返回
    #[test]
    fn select_versions_fixed_hits_existing_version() {
        let picks = Picks::Fixed {
            loader: "0.16.10".to_owned(),
            installer: "1.1.2".to_owned(),
        };
        let loaders = loaders();
        let installers = installers();
        assert_eq!(
            select_versions(&picks, &loaders, &installers),
            Some(("0.16.10".to_owned(), "1.1.2".to_owned()))
        );
    }

    /// 固定的 loader 或 installer 不在列表里时返回 `None`（上层报 `NotFound`），
    /// 而不是把不存在的版本拼进下载地址换来 400 / 404
    #[test]
    fn select_versions_fixed_rejects_unknown_version() {
        let bad_loader = Picks::Fixed {
            loader: "9.9.9".to_owned(),
            installer: "1.1.2".to_owned(),
        };
        let bad_installer = Picks::Fixed {
            loader: "0.16.10".to_owned(),
            installer: "9.9.9".to_owned(),
        };
        let loaders = loaders();
        let installers = installers();
        assert_eq!(select_versions(&bad_loader, &loaders, &installers), None);
        assert_eq!(select_versions(&bad_installer, &loaders, &installers), None);
    }

    /// 默认策略选最新稳定 loader + 最新稳定 installer
    #[test]
    fn select_versions_latest_stable_picks_newest_stable() {
        let loaders = loaders();
        let installers = installers();
        assert_eq!(
            select_versions(&Picks::LatestStable, &loaders, &installers),
            Some(("0.20.1".to_owned(), "1.1.2".to_owned()))
        );
    }

    /// 非 https 的镜像地址在构造期就被拒绝，不把明文下载地址带进来
    #[test]
    fn rejects_insecure_base_url() {
        let err = FabricSource::builder()
            .base_url("http://example.com/fabric-meta")
            .build();
        assert!(matches!(err, Err(SourceError::Init(_))));
    }

    /// 解析出的下载地址必须是 https
    #[test]
    fn rejects_non_https_download_locations() {
        assert!(parse_https_url("http://example.com/file.jar").is_err());
        assert!(parse_https_url("not a url").is_err());
        assert!(parse_https_url("https://example.com/jar").is_ok());
    }

    /// `latest` 取列表里第一个稳定版，跳过前面的快照
    #[test]
    fn resolve_latest_skips_snapshots() {
        assert_eq!(
            resolve_game_id("latest", games()).as_deref(),
            Some("1.21.4")
        );
    }

    /// `latest-snapshot` 取第一个快照版本
    #[test]
    fn resolve_latest_snapshot_picks_first_snapshot() {
        assert_eq!(
            resolve_game_id("latest-snapshot", games()).as_deref(),
            Some("26.4-snapshot-5")
        );
    }

    /// 字面版本必须在列表里：命中返回原值，未命中返回 `None`（上层报 `NotFound`）
    #[test]
    fn resolve_literal_version_requires_presence() {
        assert_eq!(
            resolve_game_id("1.21.1", games()).as_deref(),
            Some("1.21.1")
        );
        assert_eq!(resolve_game_id("1.99.999", games()), None);
    }

    /// 版本列表按 `stable` 标注类别，且全部声明 fabric 变体
    #[test]
    fn version_list_marks_kind_and_variant() {
        let mapped: Vec<_> = games()
            .into_iter()
            .map(|game| ServerVersion {
                id: VersionId::new(game.version),
                kind: game_kind(game.stable),
                variant: ServerVariant::Fabric,
                java_major: None,
            })
            .collect();
        assert_eq!(mapped[0].id, VersionId::new("26.4-snapshot-5"));
        assert_eq!(mapped[0].kind, VersionKind::Snapshot);
        assert_eq!(mapped[1].kind, VersionKind::Release);
        assert!(mapped.iter().all(|v| v.variant == ServerVariant::Fabric));
    }
}
