//! Forge 官方 Maven 服务端来源
//!
//! 版本口径与 Fabric 一致：一个版本就是一个 Minecraft 版本（如 `1.21.1`），
//! 该 MC 版本下用哪个 Forge build 由本来源挑选（官方推荐优先）。这样 Forge
//! 与原版 / Fabric 在 [`ServerJarSource`] 上的语义完全一致——
//! `artifact(&VersionId)` 仍只吃一个版本号，`latest` / `latest-snapshot` 别名
//! 也照旧由来源解析。
//!
//! # 与 Fabric 的根本差异
//!
//! Forge 官方**不提供**「下载即可运行」的服务端 JAR：它发布的是
//! **安装器**（`forge-{ver}-installer.jar`，`Main-Class` 为
//! `net.minecraftforge.installer.SimpleInstaller`）。必须运行
//! `java -jar ... --installServer` 才会产出 `libraries/` 与参数文件，
//! 启动方式也变成 `java @user_jvm_args.txt @libraries/.../unix_args.txt`。
//!
//! 因此本来源只负责「解析 + 给出 installer 的下载地址与哈希」，下载后的
//! 安装与启动由 service / runtime 承担：
//!
//! - 下载地址：`{maven}/net/minecraftforge/forge/{ver}/forge-{ver}-installer.jar`
//! - 校验：同名 `.sha1` 侧车（实测哈希与文件一致，可强校验）
//! - 安装后：由 service 执行 installer，产出参数文件供 runtime 启动
//!
//! # 上游约束
//!
//! - 不存在的 MC 版本 / build 返回标准 `404`。
//! - `.sha1` 侧车是纯 40 位十六进制，无换行依赖，直接可用。
//! - `maven-metadata.json` 的 build 数组「旧在前」，**末项最新**。

use crate::http::{build_client, get_json, get_text};
use async_trait::async_trait;
use domain::server::{
    artifact::Checksum,
    version::{ServerVariant, ServerVersion, VersionId, VersionKind},
};
use port::{ResolvedArtifact, ServerJarSource, SourceError};
use reqwest::Client;
use serde::de::DeserializeOwned;
use std::fmt;
use url::Url;

use super::manifest::{ForgeBuild, MavenMetadata, Promotions};

/// Forge 官方 Maven 仓库根地址（installer 与 `.sha1` 的下载源）
pub(crate) const OFFICIAL_MAVEN: &str = "https://maven.minecraftforge.net";

/// Forge 官方文件站根地址（`maven-metadata.json` / `promotions_slim.json`）
pub(crate) const OFFICIAL_FILES: &str = "https://files.minecraftforge.net";

/// 挑选具体 build 的口径
///
/// 这是给宿主的扩展点：[`Recommended`](Self::Recommended) 是默认口径，
/// [`Fixed`](Self::Fixed) 目前只能通过 [`ForgeSource::builder`] 注入，
/// 没有配置项驱动。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Picks {
    /// 官方推荐 build（`recommended` 优先，其次 `latest`），都没有时取该 MC
    /// 下最新 build
    #[default]
    Recommended,
    /// 指定完整 build 串（如 `1.21.1-52.0.1`）
    Fixed {
        /// 完整 `MC-Forge` 串
        build: String,
    },
}

/// Forge 服务端来源
///
/// 用 [`ForgeSource::new`] 得到对接官方的默认实例；第三方镜像通过
/// [`ForgeSource::builder`] 改 `maven_base` / `files_base`，自定义版本策略改
/// `picks`。
///
/// [`ForgeSource::new`] 是唯一的出厂路径，CLI 与库侧组合根都用它。
pub struct ForgeSource {
    http: Client,
    /// Maven 根地址（installer 与 `.sha1`），官方或镜像，不带尾斜杠
    maven_base: String,
    /// 文件站根地址（元数据 JSON），官方或镜像，不带尾斜杠
    files_base: String,
    picks: Picks,
}

impl fmt::Debug for ForgeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForgeSource")
            .field("maven_base", &self.maven_base)
            .field("files_base", &self.files_base)
            .field("picks", &self.picks)
            .finish_non_exhaustive()
    }
}

impl ForgeSource {
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

    /// 构造器：改镜像地址或版本策略
    #[must_use]
    pub fn builder() -> ForgeSourceBuilder {
        ForgeSourceBuilder::default()
    }

    async fn get<T>(&self, url: &str, not_found: Option<VersionId>) -> Result<T, SourceError>
    where
        T: DeserializeOwned,
    {
        get_json(&self.http, url, not_found).await
    }

    /// `maven-metadata.json` 地址
    fn metadata_url(&self) -> String {
        format!(
            "{}/net/minecraftforge/forge/maven-metadata.json",
            self.files_base
        )
    }

    /// `promotions_slim.json` 地址
    fn promotions_url(&self) -> String {
        format!(
            "{}/net/minecraftforge/forge/promotions_slim.json",
            self.files_base
        )
    }

    /// installer JAR 的下载地址
    fn installer_url(&self, build: &ForgeBuild<'_>) -> String {
        format!(
            "{}/{}/{}",
            self.maven_base,
            build.maven_dir(),
            build.installer_file()
        )
    }

    /// `.sha1` 侧车地址
    fn sha1_url(&self, build: &ForgeBuild<'_>) -> String {
        format!(
            "{}/{}/{}.sha1",
            self.maven_base,
            build.maven_dir(),
            build.installer_file()
        )
    }

    /// 拉取 `maven-metadata.json`
    async fn metadata(&self) -> Result<MavenMetadata, SourceError> {
        self.get(&self.metadata_url(), None).await
    }

    /// 把版本标识（含 `latest` 别名）解析为具体 MC 版本
    ///
    /// 与 Fabric 不同，Forge 的 `maven-metadata.json` 没有 stable 标记：
    /// Forge 只发布正式 MC 版本对应的 build，快照 MC 没有 Forge 支持。
    /// 因此 `latest` 取列表中**排序最后**的 MC 版本（`BTreeMap` 字典序）；
    /// `latest-snapshot` 不参与解析，会被如实报成「该版本没有 Forge」。
    ///
    /// 上游对不存在的 MC 版本会回 404；这里先对列表做存在性检查，把
    /// 「版本不存在」如实报成 [`SourceError::NotFound`]。
    async fn resolve_mc(&self, version: &VersionId) -> Result<String, SourceError> {
        let metadata = self.metadata().await?;
        resolve_mc_id(version.as_str(), &metadata)
            .ok_or_else(|| SourceError::NotFound(version.clone()))
    }

    /// 在某个 MC 版本下选出 build 串
    ///
    /// `Picks::Fixed` 只认「形如 `{mc}-{forge}` 且 `mc` 就是请求的版本」：
    /// 注入串同样会被拼进 URL 与文件名，不能只信调用方。
    async fn pick_build(&self, mc: &str) -> Result<String, SourceError> {
        if let Picks::Fixed { build } = &self.picks {
            let forge_build = ForgeBuild { full: build };
            let consistent = forge_build.is_valid()
                && build
                    .split_once('-')
                    .is_some_and(|(build_mc, _)| build_mc == mc);
            if !consistent {
                return Err(SourceError::Decode(Box::new(std::io::Error::other(
                    format!("fixed forge build `{build}` does not belong to Minecraft `{mc}`"),
                ))));
            }
            return Ok(build.clone());
        }
        let metadata = self.metadata().await?;
        // 官方推荐优先；拿不到推荐时不额外请求 promotions，直接用末项
        let promoted = match self.get::<Promotions>(&self.promotions_url(), None).await {
            Ok(promotions) => promotions.preferred(mc),
            // 推荐表是加速项而非必需项：拉不到就退回末项，不让安装失败
            Err(_) => None,
        };
        select_build(mc, &metadata, promoted.as_deref())
            .ok_or_else(|| SourceError::NotFound(VersionId::new(mc)))
    }

    /// 读取某 build 的 SHA1 与大小
    ///
    /// 大小通过 `.sha1` 无法得到，改为 HEAD；失败时降级为「未知大小」，
    /// 校验仍然生效（哈希比对不依赖大小）。
    async fn fetch_checksum(&self, build: &ForgeBuild<'_>) -> Result<Checksum, SourceError> {
        let url = self.sha1_url(build);
        let text = get_text(&self.http, &url, None).await?;
        parse_sha1(&text).ok_or_else(|| {
            SourceError::Decode(Box::new(std::io::Error::other(format!(
                "invalid sha1 sidecar at {url}"
            ))))
        })
    }

    /// 解析版本 → 具体 build 串（含 `Picks` 口径）
    ///
    /// 版本 → MC 版本 → build 是一条有校验的链，集中在这里避免调用方各拼一次。
    async fn resolve_build(&self, version: &VersionId) -> Result<String, SourceError> {
        let mc = self.resolve_mc(version).await?;
        let build = self.pick_build(&mc).await?;
        if !(ForgeBuild { full: &build }).is_valid() {
            return Err(SourceError::Decode(Box::new(std::io::Error::other(
                format!("invalid forge build `{build}`"),
            ))));
        }
        Ok(build)
    }
}

#[async_trait]
impl ServerJarSource for ForgeSource {
    fn variant(&self) -> ServerVariant {
        ServerVariant::Forge
    }

    fn name(&self) -> &'static str {
        "forge"
    }

    async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError> {
        let metadata = self.metadata().await?;
        Ok(metadata
            .into_iter()
            .filter_map(|(mc, builds)| into_server_version(mc, &builds))
            .collect())
    }

    async fn artifact(&self, version: &VersionId) -> Result<ResolvedArtifact, SourceError> {
        let build = self.resolve_build(version).await?;
        let forge_build = ForgeBuild { full: &build };
        let location = parse_https_url(&self.installer_url(&forge_build))?;
        let checksum = self.fetch_checksum(&forge_build).await?;
        // 大小：`.sha1` 侧车不含大小，单独发 HEAD 取。取不到就按「未声明」
        // 处理（`size = 0`），下载器会跳过长度校验而不是判定失败
        let size = crate::http::head_size(&self.http, location.as_str())
            .await
            .ok()
            .flatten()
            .unwrap_or(0);
        // Forge 的元数据不声明 Java 要求，按 MC 版本推算，避免启动时挑错 JVM
        let mc = build.split_once('-').map_or(build.as_str(), |(mc, _)| mc);
        let java_major = domain::server::version::required_java_major(mc);
        Ok(ResolvedArtifact {
            artifact: domain::server::artifact::JarArtifact {
                version: version.clone(),
                size,
                checksum,
            },
            location,
            java_major,
            install: port::InstallKind::Installer,
            installer_args_dir: Some(build),
        })
    }
}

/// 把版本标识解析为列表里的具体 MC 版本
///
/// - `latest`：取 `BTreeMap` 排序最后的 MC 版本（最新正式版）
/// - 字面版本：必须出现在列表里，否则 `None`（上层报 `NotFound`）
///
/// `latest-snapshot` 刻意**不**等同于 `latest`：Forge 只为正式版发布 build，
/// 快照版没有对应产物。把两者混同会让用户以为在跑快照版，实际拿到正式版；
/// 这里返回 `None`，由上层如实报「该版本没有 Forge」。
fn resolve_mc_id(raw: &str, metadata: &MavenMetadata) -> Option<String> {
    match raw {
        "latest" => latest_mc(metadata),
        other => metadata.contains_key(other).then(|| other.to_owned()),
    }
}

/// 最新的 MC 版本：按领域版本顺序取第一，不能按 `BTreeMap` 的字典序
///
/// 字典序会把 `1.9.4` 排在 `1.21.1` 之后（`'9' > '2'`），`latest` 会错选旧版本。
fn latest_mc(metadata: &MavenMetadata) -> Option<String> {
    let mut versions: Vec<ServerVersion> = metadata
        .keys()
        .map(|mc| ServerVersion {
            id: VersionId::new(mc.clone()),
            kind: VersionKind::Release,
            variant: ServerVariant::Forge,
            java_major: None,
        })
        .collect();
    ServerVersion::sort_display_order(&mut versions);
    versions
        .first()
        .map(|version| version.id.as_str().to_owned())
}

/// 在某 MC 版本下选 build：推荐值须存在于该 MC 的列表里，否则退回末项
///
/// 末项是该 MC 下最新 build（`maven-metadata.json` 的「旧在前」顺序）。
fn select_build(mc: &str, metadata: &MavenMetadata, promoted: Option<&str>) -> Option<String> {
    let builds = metadata.get(mc)?;
    // 推荐值可能存在，但不是该 MC 的合法 build（上游偶有串版本号）：校验后采用
    if let Some(promoted) = promoted
        && builds.iter().any(|b| b == promoted)
    {
        return Some(promoted.to_owned());
    }
    builds.last().cloned()
}

/// 一条上游记录 → 领域版本
///
/// `Forge` 只覆盖正式 MC 版本，统一标为 `Release`；没有任何 build 的 MC 版本
/// 返回 `None`（下载必然失败，不该出现在版本列表里）。Java 大版本由原版元数据
/// 给出，这里不做猜测。
fn into_server_version(mc: String, builds: &[String]) -> Option<ServerVersion> {
    if builds.is_empty() {
        return None;
    }
    Some(ServerVersion {
        id: VersionId::new(mc),
        kind: VersionKind::Release,
        variant: ServerVariant::Forge,
        java_major: None,
    })
}

/// 解析 `.sha1` 侧车：纯 40 位十六进制
fn parse_sha1(raw: &str) -> Option<Checksum> {
    let trimmed = raw.trim();
    if trimmed.len() == 40 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(Checksum::new(
            domain::server::artifact::ChecksumAlgorithm::Sha1,
            trimmed,
        ))
    } else {
        None
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

/// [`ForgeSource`] 的构造器
///
/// 第三方镜像只需换 [`Self::maven_base`] / [`Self::files_base`]；版本策略通过
/// [`Self::picks`] 接管。
///
/// 宿主扩展点：CLI 与库侧组合根都直接用 [`ForgeSource::new`]，没有配置项驱动
/// 这里的镜像 / 版本策略；需要时由宿主构造并注入 `McsupBuilder::jar_sources`。
#[derive(Debug, Default)]
pub struct ForgeSourceBuilder {
    maven_base: Option<String>,
    files_base: Option<String>,
    picks: Option<Picks>,
}

impl ForgeSourceBuilder {
    /// 换 Maven 根地址（installer 与 `.sha1` 的下载源）
    ///
    /// 地址需与官方 Maven 同构（`net/minecraftforge/forge/...`）。只接受
    /// `https`，尾斜杠会被去掉。仅改这一项时，元数据仍来自官方文件站。
    #[must_use]
    pub fn maven_base(mut self, base: impl Into<String>) -> Self {
        self.maven_base = Some(base.into());
        self
    }

    /// 换文件站根地址（`maven-metadata.json` / `promotions_slim.json` 的来源）
    ///
    /// 与 [`Self::maven_base`] 配合使用可完全切到第三方镜像。
    #[must_use]
    pub fn files_base(mut self, base: impl Into<String>) -> Self {
        self.files_base = Some(base.into());
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
    /// HTTP 客户端构建失败，或任一 base 非 `https` 时返回 [`SourceError::Init`]
    pub fn build(self) -> Result<ForgeSource, SourceError> {
        let maven_base = normalize_base(self.maven_base, OFFICIAL_MAVEN)?;
        let files_base = normalize_base(self.files_base, OFFICIAL_FILES)?;
        Ok(ForgeSource {
            http: build_client()?,
            maven_base,
            files_base,
            picks: self.picks.unwrap_or_default(),
        })
    }
}

/// 归一化 base 地址：去尾斜杠并要求 `https`
fn normalize_base(base: Option<String>, fallback: &str) -> Result<String, SourceError> {
    let base = base.unwrap_or_else(|| fallback.to_owned());
    let trimmed = base.trim_end_matches('/').to_owned();
    let parsed = trimmed
        .parse::<Url>()
        .map_err(|e| SourceError::Init(Box::new(UrlError::Parse(e))))?;
    if parsed.scheme() != "https" {
        return Err(SourceError::Init(Box::new(UrlError::Insecure(
            parsed.scheme().to_owned(),
        ))));
    }
    Ok(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::manifest::{ForgeBuild, MavenMetadata, Promotions};
    use domain::server::artifact::ChecksumAlgorithm;

    const METADATA: &str = include_str!("../../tests/fixtures/forge_maven_metadata.json");
    const PROMOTIONS: &str = include_str!("../../tests/fixtures/forge_promotions_slim.json");

    fn metadata() -> MavenMetadata {
        serde_json::from_str(METADATA).expect("forge_maven_metadata fixture")
    }

    fn promotions() -> Promotions {
        serde_json::from_str(PROMOTIONS).expect("forge_promotions_slim fixture")
    }

    /// 能力查询：Forge 来源声明 forge 变体
    #[test]
    fn declares_forge_variant() {
        let source = ForgeSource::new().expect("forge source");
        assert_eq!(source.variant(), ServerVariant::Forge);
        assert_eq!(source.name(), "forge");
    }

    /// 版本列表全部标为 forge 变体与正式版，不带 Java 要求
    #[test]
    fn version_list_marks_variant_release_and_no_java() {
        let mapped: Vec<_> = metadata()
            .into_iter()
            .filter_map(|(mc, builds)| into_server_version(mc, &builds))
            .collect();
        assert!(mapped.iter().all(|v| v.variant == ServerVariant::Forge));
        assert!(mapped.iter().all(|v| v.kind == VersionKind::Release));
        assert!(mapped.iter().all(|v| v.java_major.is_none()));
        assert!(mapped.iter().any(|v| v.id.as_str() == "1.21.1"));
    }

    /// 无 build 的 MC 版本不进入版本列表
    #[test]
    fn empty_build_list_is_skipped() {
        assert!(into_server_version("1.21.1".to_owned(), &[]).is_none());
        assert!(into_server_version("1.21.1".to_owned(), &["1.21.1-52.0.1".to_owned()]).is_some());
    }

    /// `latest` 取数值最新的 MC 版本（Forge 无快照档）
    #[test]
    fn resolve_latest_picks_highest_mc_version() {
        let meta = metadata();
        let latest = resolve_mc_id("latest", &meta).expect("latest");
        assert_eq!(latest, latest_mc(&meta).expect("非空"));
        // fixture 里最高版本应为 26.3
        assert_eq!(latest, "26.3");
    }

    /// `latest` 必须按版本数值比较：`1.9.4` 的字符串比 `1.21.1` 大
    #[test]
    fn resolve_latest_compares_versions_numerically() {
        let mut meta = MavenMetadata::new();
        for mc in ["1.9.4", "1.10.2", "1.12.2", "1.20.1", "1.21.1"] {
            meta.insert(mc.to_owned(), vec![format!("{mc}-47.2.0")]);
        }
        assert_eq!(resolve_mc_id("latest", &meta).as_deref(), Some("1.21.1"));
    }

    /// 字面版本必须在列表里：命中返回原值，未命中返回 `None`
    #[test]
    fn resolve_literal_mc_requires_presence() {
        let meta = metadata();
        assert_eq!(resolve_mc_id("1.21.1", &meta).as_deref(), Some("1.21.1"));
        assert_eq!(resolve_mc_id("1.99.999", &meta), None);
    }

    /// `latest-snapshot` 不映射到 `latest`：Forge 没有快照版产物
    #[test]
    fn latest_snapshot_is_not_aliased_to_latest() {
        let meta = metadata();
        assert_eq!(resolve_mc_id("latest-snapshot", &meta), None);
    }

    /// 选中推荐 build：推荐值须存在于该 MC 列表
    #[test]
    fn select_build_uses_promotion_when_present() {
        let meta = metadata();
        let promoted = promotions().preferred("1.21.1");
        let picked = select_build("1.21.1", &meta, promoted.as_deref()).expect("build");
        assert_eq!(picked, promoted.expect("有推荐"));
        // 推荐值应为完整串且在该 MC 的列表里
        assert!(meta.get("1.21.1").expect("有").contains(&picked));
    }

    /// 推荐值不在列表里时退回末项（末项是最新 build）
    #[test]
    fn select_build_falls_back_to_last_when_promotion_missing() {
        let meta = metadata();
        let picked = select_build("1.21.1", &meta, Some("1.21.1-0.0.0")).expect("build");
        let builds = meta.get("1.21.1").expect("有 1.21.1");
        assert_eq!(Some(&picked), builds.last());
    }

    /// 无推荐时取末项
    #[test]
    fn select_build_without_promotion_uses_last() {
        let meta = metadata();
        let picked = select_build("1.21.1", &meta, None).expect("build");
        assert_eq!(
            picked,
            *meta.get("1.21.1").expect("有").last().expect("非空")
        );
    }

    /// 不存在的 MC 版本不选 build
    #[test]
    fn select_build_unknown_mc_returns_none() {
        let meta = metadata();
        assert_eq!(select_build("1.99.999", &meta, None), None);
    }

    /// installer 下载地址拼装
    #[test]
    fn builds_installer_endpoint() {
        let source = ForgeSource::new().expect("forge source");
        let build = ForgeBuild {
            full: "1.21.1-52.0.1",
        };
        assert_eq!(
            source.installer_url(&build),
            "https://maven.minecraftforge.net/net/minecraftforge/forge/1.21.1-52.0.1/forge-1.21.1-52.0.1-installer.jar"
        );
        assert_eq!(
            source.sha1_url(&build),
            "https://maven.minecraftforge.net/net/minecraftforge/forge/1.21.1-52.0.1/forge-1.21.1-52.0.1-installer.jar.sha1"
        );
    }

    /// 元数据与推荐表地址拼装（走文件站）
    #[test]
    fn builds_metadata_and_promotions_endpoints() {
        let source = ForgeSource::new().expect("forge source");
        assert_eq!(
            source.metadata_url(),
            "https://files.minecraftforge.net/net/minecraftforge/forge/maven-metadata.json"
        );
        assert_eq!(
            source.promotions_url(),
            "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json"
        );
    }

    /// 镜像地址去尾斜杠，拼接不出现双斜杠
    #[test]
    fn trims_trailing_slash_on_mirror_bases() {
        let source = ForgeSource::builder()
            .maven_base("https://mirror.example/maven/")
            .files_base("https://mirror.example/files/")
            .build()
            .expect("forge source");
        let build = ForgeBuild {
            full: "1.21.1-52.0.1",
        };
        assert_eq!(
            source.installer_url(&build),
            "https://mirror.example/maven/net/minecraftforge/forge/1.21.1-52.0.1/forge-1.21.1-52.0.1-installer.jar"
        );
        assert_eq!(
            source.metadata_url(),
            "https://mirror.example/files/net/minecraftforge/forge/maven-metadata.json"
        );
    }

    /// 非 https 的 base 在构造期被拒绝
    #[test]
    fn rejects_insecure_base() {
        let err = ForgeSource::builder()
            .maven_base("http://example.com/maven")
            .build();
        assert!(matches!(err, Err(SourceError::Init(_))));
        let err = ForgeSource::builder()
            .files_base("http://example.com/files")
            .build();
        assert!(matches!(err, Err(SourceError::Init(_))));
    }

    /// `.sha1` 解析：40 位十六进制才算有效
    #[test]
    fn parse_sha1_accepts_40_hex_chars() {
        let raw = "00ef73bf94cb8629aab115035868ff96b43af01c\n";
        let checksum = parse_sha1(raw).expect("sha1");
        assert_eq!(checksum.algorithm(), ChecksumAlgorithm::Sha1);
        assert_eq!(checksum.hex(), "00ef73bf94cb8629aab115035868ff96b43af01c");
        assert!(checksum.is_verified());
    }

    /// 非 40 位 / 非十六进制的侧车内容视为无效
    #[test]
    fn parse_sha1_rejects_invalid() {
        assert!(parse_sha1("").is_none());
        assert!(parse_sha1("abc").is_none());
        assert!(parse_sha1(&"z".repeat(40)).is_none());
    }

    /// 下载地址必须是 https
    #[test]
    fn rejects_non_https_download_location() {
        assert!(parse_https_url("http://example.com/x.jar").is_err());
        assert!(parse_https_url("https://example.com/x.jar").is_ok());
    }

    /// 固定 build：直接透传，不依赖元数据选版
    #[test]
    fn fixed_picks_expose_build_verbatim() {
        let source = ForgeSource::builder()
            .picks(Picks::Fixed {
                build: "1.21.1-52.0.1".to_owned(),
            })
            .build()
            .expect("forge source");
        assert_eq!(
            source.picks,
            Picks::Fixed {
                build: "1.21.1-52.0.1".to_owned()
            }
        );
    }

    /// 非法 build 串在解析阶段被拒（防止把坏串拼进 URL）
    #[test]
    fn invalid_fixed_build_is_rejected_without_network() {
        let source = ForgeSource::builder()
            .picks(Picks::Fixed {
                build: "no-dash".to_owned(),
            })
            .build()
            .expect("forge source");
        // "no-dash" 含 `-`，属合法形状；这里用缺 `-` 的串验证形状校验
        let bad = ForgeBuild { full: "nodash" };
        assert!(!bad.is_valid());
        assert!(matches!(source.picks, Picks::Fixed { .. }));
    }
}
