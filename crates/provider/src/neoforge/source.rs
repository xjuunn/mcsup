//! `NeoForge` 官方 Maven 服务端来源
//!
//! 版本口径与 Forge / Fabric 一致：一个版本就是一个 Minecraft 版本（如
//! `1.21.1`），该 MC 版本下用哪个 `NeoForge` build 由本来源挑选（最新稳定版）。
//! `artifact(&VersionId)` 只吃一个 MC 版本号，`latest` / `latest-snapshot` 别名
//! 照旧由来源解析。
//!
//! # 与 Forge 的关系
//!
//! 两者都是「安装器 + 参数文件启动」形态，但版本号规则不同：
//!
//! | | Forge | `NeoForge` |
//! |---|---|---|
//! | 版本串 | `1.21.1-52.0.1`（`{mc}-{forge}`） | `21.1.200`（`{mc 主版本}.{mc 次版本}.{build}`） |
//! | 元数据 | `files.minecraftforge.net/...maven-metadata.json` | `maven.neoforged.net/api/maven/versions/releases/...` |
//! | 推荐表 | `promotions_slim.json` | 无（取最新 build） |
//!
//! `NeoForge` 的 build 串编码了 MC 版本：`21.1.200` 对应 MC `1.21.1`，即
//! 「`1.` 去掉、前两段作为 MC 次版本、第三段是 build 号」。反向映射
//! （MC `1.21.1` → `21.1.*`）是纯字符串变换，见 [`mc_to_neoforge_prefix`]。
//!
//! 少数版本是快照 MC（如 `0.25w14craftmine.3-beta`），前缀为 `0.` 开头，
//! 无法映射回正式 MC 版本号，因此**不进入版本列表**（原版快照才需要它们，
//! 这里只服务正式 MC 版本的服务器）。
//!
//! # 上游约束
//!
//! - `GET {maven}/api/maven/versions/releases/net/neoforged/neoforge`
//!   返回 `{"isSnapshot":false,"versions":[...]}`，**顺序为旧在前**，末项最新。
//! - installer 位于 `{maven}/releases/net/neoforged/neoforge/{ver}/neoforge-{ver}-installer.jar`，
//!   同名 `.sha1` 侧车是纯 40 位十六进制（实测与文件一致，可强校验）。

use crate::http::{build_client, get_json, get_text};
use crate::shared::{
    Downloads, VersionIntent, normalize_base, parse_https_url, parse_sha1_sidecar,
    pick_by_version_aliases,
};
use async_trait::async_trait;
use domain::server::version::{ServerVariant, ServerVersion, VersionId, VersionKind};
use port::{ServerJarSource, SourceError};
use reqwest::Client;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::fmt;

use super::manifest::NeoForgeVersions;

/// `NeoForge` 官方 Maven 根地址（`/api/maven` 与 `/releases` 共同的父路径）
pub(crate) const OFFICIAL_MAVEN: &str = "https://maven.neoforged.net";

/// 挑选 build 的口径
///
/// 给宿主留的扩展点：[`Latest`](Self::Latest) 是默认口径（该 MC 线最新 build），
/// [`Fixed`](Self::Fixed) 只经 [`NeoForgeSource::builder`] 注入。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Picks {
    /// 该 MC 线最新 build（列表末项）
    #[default]
    Latest,
    /// 指定完整 build 串（如 `21.1.200`）
    Fixed {
        /// 完整 `NeoForge` 版本串
        build: String,
    },
}

/// `NeoForge` 服务端来源
///
/// [`NeoForgeSource::new`] 是对接官方的唯一出厂路径；镜像通过
/// [`NeoForgeSource::builder`] 改 `maven_base`。
pub struct NeoForgeSource {
    http: Client,
    /// Maven 根地址（不带尾斜杠），官方或镜像
    maven_base: String,
    picks: Picks,
}

impl fmt::Debug for NeoForgeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NeoForgeSource")
            .field("maven_base", &self.maven_base)
            .field("picks", &self.picks)
            .finish_non_exhaustive()
    }
}

impl NeoForgeSource {
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
    pub fn builder() -> NeoForgeSourceBuilder {
        NeoForgeSourceBuilder::default()
    }

    async fn get<T>(&self, url: &str, not_found: Option<VersionId>) -> Result<T, SourceError>
    where
        T: DeserializeOwned,
    {
        get_json(&self.http, url, not_found).await
    }

    /// 版本索引地址（所有已发布 `NeoForge` 版本，含快照）
    fn versions_url(&self) -> String {
        format!(
            "{}/api/maven/versions/releases/net/neoforged/neoforge",
            self.maven_base
        )
    }

    /// installer 的 Maven 相对目录：`releases/net/neoforged/neoforge/{ver}`
    fn maven_dir(&self, build: &str) -> String {
        format!(
            "{}/releases/net/neoforged/neoforge/{build}",
            self.maven_base
        )
    }

    /// installer JAR 地址
    fn installer_url(&self, build: &str) -> String {
        format!("{}/neoforge-{build}-installer.jar", self.maven_dir(build))
    }

    /// `.sha1` 侧车地址
    fn sha1_url(&self, build: &str) -> String {
        format!(
            "{}/neoforge-{build}-installer.jar.sha1",
            self.maven_dir(build)
        )
    }

    /// 拉取版本索引
    ///
    /// 只接受 release 线：镜像若把 snapshot 混进来，这里直接报错，避免把快照
    /// build 当正式版装给用户。
    async fn index(&self) -> Result<NeoForgeVersions, SourceError> {
        let index: NeoForgeVersions = self.get(&self.versions_url(), None).await?;
        if !index.is_release_line() && !index.versions.is_empty() {
            return Err(SourceError::Decode(Box::new(std::io::Error::other(
                "neoforge version index is a snapshot line, expected release",
            ))));
        }
        Ok(index)
    }

    /// 把版本标识解析为具体 MC 版本
    ///
    /// 列表由 [`builds_by_mc`] 从完整版本索引里挑出「可映射到正式 MC」的 build，
    /// 再按 MC 版本分组。`latest` 取字典序最后的 MC 版本（`BTreeMap` 稳定序）。
    async fn resolve_mc(&self, version: &VersionId) -> Result<String, SourceError> {
        let grouped = builds_by_mc(&self.index().await?);
        resolve_mc_id(version, &grouped).ok_or_else(|| SourceError::NotFound(version.clone()))
    }

    /// 在某 MC 版本下选 build 串
    async fn pick_build(&self, mc: &str) -> Result<String, SourceError> {
        if let Picks::Fixed { build } = &self.picks {
            return Ok(build.clone());
        }
        let grouped = builds_by_mc(&self.index().await?);
        select_build(mc, &grouped).ok_or_else(|| SourceError::NotFound(VersionId::new(mc)))
    }

    /// 读取 installer 的 SHA1
    async fn fetch_checksum(
        &self,
        build: &str,
    ) -> Result<domain::server::artifact::Checksum, SourceError> {
        let url = self.sha1_url(build);
        let text = get_text(&self.http, &url, None).await?;
        parse_sha1_sidecar(&text).ok_or_else(|| {
            SourceError::Decode(Box::new(std::io::Error::other(format!(
                "invalid sha1 sidecar at {url}"
            ))))
        })
    }
}

#[async_trait]
impl ServerJarSource for NeoForgeSource {
    fn variant(&self) -> ServerVariant {
        ServerVariant::NeoForge
    }

    fn name(&self) -> &'static str {
        "neoforge"
    }

    async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError> {
        Ok(builds_by_mc(&self.index().await?)
            .into_keys()
            .map(into_server_version)
            .collect())
    }

    async fn artifact(&self, version: &VersionId) -> Result<port::ResolvedArtifact, SourceError> {
        let mc = self.resolve_mc(version).await?;
        let build = self.pick_build(&mc).await?;
        if !is_valid_build(&build) {
            return Err(SourceError::Decode(Box::new(std::io::Error::other(
                format!("invalid neoforge build `{build}`"),
            ))));
        }
        let location = parse_https_url(&self.installer_url(&build))?;
        let checksum = self.fetch_checksum(&build).await?;
        Ok(Downloads::installer(location)
            .checksum(checksum)
            .resolve(version))
    }
}

/// MC 版本 → 该 MC 下的 `NeoForge` build 列表（旧在前，与上游索引一致）
pub(crate) type BuildsByMc = BTreeMap<String, Vec<String>>;

/// 从完整版本索引挑出可映射的 build，按 MC 版本分组
///
/// 无法映射到正式 MC 版本的项（快照 MC，前缀 `0.`）被丢弃：它们对应的服务器
/// 是快照服务端，与「按正式 MC 版本安装」的口径不符。
fn builds_by_mc(versions: &NeoForgeVersions) -> BuildsByMc {
    let mut grouped: BuildsByMc = BTreeMap::new();
    for build in &versions.versions {
        if let Some(mc) = neoforge_to_mc(build) {
            grouped.entry(mc).or_default().push(build.clone());
        }
    }
    grouped
}

/// `NeoForge` build 串 → 对应的 MC 版本（`21.1.200` → `1.21.1`）
///
/// 规则：三段数字 `{major}.{minor}.{build}`，其中前两段是 MC 版本去掉 `1.` 前缀
/// 后的结果。`20.6.x` 是例外——它对应 MC `1.20.6`（`NeoForge` 从 20.2 起改用
/// 该编码）。带后缀（`-beta`）或首段不是纯数字的串不映射，返回 `None`。
#[must_use]
pub(crate) fn neoforge_to_mc(build: &str) -> Option<String> {
    let mut parts = build.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    let rest = parts.next()?;
    // build 段必须是纯数字；带 `-beta` 之类的预发布串无法确定 MC 版本
    if !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !major.chars().all(|c| c.is_ascii_digit()) || !minor.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // `0.x` 是快照 MC（如 `0.25w14craftmine.3-beta` 的变体），不映射
    if major == "0" {
        return None;
    }
    Some(format!("1.{major}.{minor}"))
}

/// MC 版本 → `NeoForge` build 前缀（`1.21.1` → `21.1`）
///
/// [`neoforge_to_mc`] 的逆变换：`21.1.200` 的 MC 版本 `1.21.1` 反过来给出
/// `21.1`，取该前缀的 build 就是这条 MC 线的全部候选。非 `1.x.y` 形状的
/// MC 版本（快照名）返回 `None`。
///
/// 当前 `versions()` / `artifact()` 都直接用 [`neoforge_to_mc`] 分组（索引条目
/// 有限，一次遍历比「先算前缀再筛」更省），本函数是同一变换的对外口径，供需要
/// 反向查找的来源 / 诊断代码使用。
///
/// # 为什么标 `allow(dead_code)`
///
/// 它是 [`neoforge_to_mc`] 的逆变换，两者必须保持同步：改了一侧不改另一侧，
/// 按前缀筛 build 的代码就会静默筛错。保留实现并让它进测试，比删掉后等某个
/// 调用方各自手写一遍 `strip_prefix("1.")` 更安全。
#[allow(
    dead_code,
    reason = "neoforge_to_mc 的逆变换，供按前缀筛 build 的调用方与测试复用"
)]
#[must_use]
pub(crate) fn mc_to_neoforge_prefix(mc: &str) -> Option<String> {
    let (major, minor) = mc.strip_prefix("1.")?.split_once('.')?;
    Some(format!("{major}.{minor}"))
}

/// build 串是否为合法形状：三段、前两段数字、第三段数字
fn is_valid_build(build: &str) -> bool {
    neoforge_to_mc(build).is_some()
}

/// 版本标识 → 列表里的具体 MC 版本
fn resolve_mc_id(intent: &VersionId, grouped: &BuildsByMc) -> Option<String> {
    let keys: Vec<&str> = grouped.keys().map(String::as_str).collect();
    // `NeoForge` 索引没有 stable 标记，`latest` 与 `latest-snapshot` 同口径：
    // 取字典序最后的 MC 版本（`BuildsByMc` 是 `BTreeMap`，末项即最大）
    pick_by_version_aliases(
        VersionIntent::of(intent),
        keys.iter().copied(),
        |_| false,
        |_| false,
    )
    .or_else(|| match VersionIntent::of(intent) {
        VersionIntent::Literal(raw) => grouped.contains_key(raw).then(|| raw.to_owned()),
        VersionIntent::Latest | VersionIntent::LatestSnapshot => {
            grouped.keys().next_back().cloned()
        }
    })
}

/// 在某 MC 版本下选 build：取列表末项（上游「旧在前」）
fn select_build(mc: &str, grouped: &BuildsByMc) -> Option<String> {
    grouped.get(mc)?.last().cloned()
}

/// MC 版本 → 领域版本
///
/// `NeoForge` 只覆盖正式 MC 版本，统一标 `Release`；Java 大版本由原版元数据
/// 给出，这里不做猜测。
fn into_server_version(mc: String) -> ServerVersion {
    ServerVersion {
        id: VersionId::new(mc),
        kind: VersionKind::Release,
        variant: ServerVariant::NeoForge,
        java_major: None,
    }
}

/// [`NeoForgeSource`] 的构造器
///
/// 第三方镜像换 [`Self::maven_base`]；版本策略通过 [`Self::picks`] 接管。
#[derive(Debug, Default)]
pub struct NeoForgeSourceBuilder {
    maven_base: Option<String>,
    picks: Option<Picks>,
}

impl NeoForgeSourceBuilder {
    /// 换 Maven 根地址
    ///
    /// 地址需与官方同构（`/api/maven/versions/releases/...` 与 `/releases/...`）。
    /// 只接受 `https`，尾斜杠会被去掉。
    #[must_use]
    pub fn maven_base(mut self, base: impl Into<String>) -> Self {
        self.maven_base = Some(base.into());
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
    /// HTTP 客户端构建失败，或 `maven_base` 非 `https` 时返回 [`SourceError::Init`]
    pub fn build(self) -> Result<NeoForgeSource, SourceError> {
        Ok(NeoForgeSource {
            http: build_client()?,
            maven_base: normalize_base(self.maven_base, OFFICIAL_MAVEN)?,
            picks: self.picks.unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = include_str!("../../tests/fixtures/neoforge_versions.json");

    fn index() -> NeoForgeVersions {
        serde_json::from_str(INDEX).expect("neoforge_versions fixture")
    }

    /// 能力查询：声明 neoforge 变体
    #[test]
    fn declares_neoforge_variant() {
        let source = NeoForgeSource::new().expect("neoforge source");
        assert_eq!(source.variant(), ServerVariant::NeoForge);
        assert_eq!(source.name(), "neoforge");
    }

    /// build 串 ↔ MC 版本双向映射
    #[test]
    fn build_string_maps_to_mc_version() {
        assert_eq!(neoforge_to_mc("21.1.200").as_deref(), Some("1.21.1"));
        assert_eq!(neoforge_to_mc("20.6.144").as_deref(), Some("1.20.6"));
        assert_eq!(neoforge_to_mc("21.0.167").as_deref(), Some("1.21.0"));
        // 快照 MC / 预发布 build 不映射
        assert_eq!(neoforge_to_mc("0.25w14craftmine.3-beta"), None);
        assert_eq!(neoforge_to_mc("21.1.200-beta"), None);
        assert_eq!(neoforge_to_mc("nonsense"), None);
        assert_eq!(neoforge_to_mc("21"), None);
    }

    /// 前缀变换是映射的逆运算
    #[test]
    fn mc_prefix_round_trips_through_build() {
        assert_eq!(mc_to_neoforge_prefix("1.21.1").as_deref(), Some("21.1"));
        let prefix = mc_to_neoforge_prefix("1.21.1").expect("prefix");
        let mc = neoforge_to_mc(&format!("{prefix}.200")).expect("mc");
        assert_eq!(mc, "1.21.1");
        // 非 1.x.y 形状不映射
        assert_eq!(mc_to_neoforge_prefix("26.3"), None);
        assert_eq!(mc_to_neoforge_prefix("1.21"), None);
    }

    /// 索引按 MC 分组：快照被剔除，组内保持旧在前的顺序
    #[test]
    fn groups_builds_by_mc_and_skips_snapshots() {
        let grouped = builds_by_mc(&index());
        assert!(grouped.contains_key("1.21.1"));
        assert!(grouped.contains_key("1.20.6"));
        assert_eq!(
            grouped.get("1.21.1").expect("1.21.1"),
            &vec![
                "21.1.1".to_owned(),
                "21.1.2".to_owned(),
                "21.1.264".to_owned(),
                "21.1.265".to_owned()
            ]
        );
        assert!(grouped.keys().all(|mc| mc.starts_with("1.")));
    }

    /// 某 MC 下选末项（上游「旧在前」，末项最新）
    #[test]
    fn select_build_picks_last_entry() {
        let grouped = builds_by_mc(&index());
        assert_eq!(
            select_build("1.21.1", &grouped).as_deref(),
            Some("21.1.265")
        );
        assert_eq!(select_build("1.99.9", &grouped), None);
    }

    /// `latest` 取字典序最后的 MC 版本
    #[test]
    fn resolve_latest_picks_highest_mc() {
        let grouped = builds_by_mc(&index());
        let latest = resolve_mc_id(&VersionId::new("latest"), &grouped).expect("latest");
        assert_eq!(latest, grouped.keys().next_back().expect("非空").clone());
        assert_eq!(latest, "1.21.1");
    }

    /// 字面版本必须在列表里
    #[test]
    fn resolve_literal_requires_presence() {
        let grouped = builds_by_mc(&index());
        assert_eq!(
            resolve_mc_id(&VersionId::new("1.20.6"), &grouped).as_deref(),
            Some("1.20.6")
        );
        assert_eq!(resolve_mc_id(&VersionId::new("1.99.999"), &grouped), None);
    }

    /// 版本列表全部标 neoforge、正式版、无 Java 要求
    #[test]
    fn version_list_marks_variant_and_kind() {
        let mapped: Vec<_> = builds_by_mc(&index())
            .into_keys()
            .map(into_server_version)
            .collect();
        assert!(mapped.iter().all(|v| v.variant == ServerVariant::NeoForge));
        assert!(mapped.iter().all(|v| v.kind == VersionKind::Release));
        assert!(mapped.iter().all(|v| v.java_major.is_none()));
    }

    /// installer 与 `.sha1` 地址拼装
    #[test]
    fn builds_installer_endpoints() {
        let source = NeoForgeSource::new().expect("neoforge source");
        assert_eq!(
            source.installer_url("21.1.200"),
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.200/neoforge-21.1.200-installer.jar"
        );
        assert_eq!(
            source.sha1_url("21.1.200"),
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.200/neoforge-21.1.200-installer.jar.sha1"
        );
        assert_eq!(
            source.versions_url(),
            "https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/neoforge"
        );
    }

    /// 镜像去尾斜杠，拼接不出现双斜杠
    #[test]
    fn trims_trailing_slash_on_mirror_base() {
        let source = NeoForgeSource::builder()
            .maven_base("https://mirror.example/maven/")
            .build()
            .expect("neoforge source");
        assert_eq!(
            source.installer_url("21.1.200"),
            "https://mirror.example/maven/releases/net/neoforged/neoforge/21.1.200/neoforge-21.1.200-installer.jar"
        );
    }

    /// 非 https 的镜像根在构造期被拒绝
    #[test]
    fn rejects_insecure_base() {
        let err = NeoForgeSource::builder()
            .maven_base("http://example.com/maven")
            .build();
        assert!(matches!(err, Err(SourceError::Init(_))));
    }

    /// 固定 build 直接透传，不依赖索引选版
    #[test]
    fn fixed_picks_are_exposed_verbatim() {
        let source = NeoForgeSource::builder()
            .picks(Picks::Fixed {
                build: "21.1.200".to_owned(),
            })
            .build()
            .expect("neoforge source");
        assert_eq!(
            source.picks,
            Picks::Fixed {
                build: "21.1.200".to_owned()
            }
        );
    }
}
