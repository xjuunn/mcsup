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
//! `NeoForge` 的 build 串编码了 MC 版本，分两代：
//!
//! - 旧编码（MC `1.x`）：`21.1.200` 对应 MC `1.21.1`，即「`1.` 去掉、前两段
//!   作为 MC 次版本、第三段是 build 号」。
//! - 新编码（MC `26.x` 起，Mojang 从 `1.21.11` 之后改用年份制版本号）：
//!   `26.3.0.99` 对应 MC `26.3`，即「前段就是 MC 版本、末段是 build 号」。
//!
//! 反向映射（MC `1.21.1` → `21.1.*`，MC `26.3` → `26.3.0.*`）是纯字符串变换，
//! 见 [`mc_to_neoforge_prefix`]。
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
            .resolve(version, Some(build)))
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
/// 分两代编码，由首段数字区分：
///
/// - **旧编码（MC `1.x`）**：`{mc 次版本}.{build}`，如 `21.1.200` → `1.21.1`，
///   `20.6.144` → `1.20.6`。首段是 MC 次版本，映射时补回 `1.` 前缀。
/// - **新编码（MC `26.x` 起）**：Mojang 从 `1.21.11` 之后改用年份制版本号
///   （`26.1` / `26.1.2` / `26.3`），不再有 `1.` 前缀可去。`NeoForge` 因此改成
///   `{mc 主版本}.{mc 次版本}.{mc 补丁}.{build}` 四段，如 `26.3.0.99` → MC
///   `26.3`、`26.1.2.124` → MC `26.1.2`。首段即 MC 主版本，原样使用。
///
/// 带后缀（`-beta`）或段内非纯数字的串不映射，返回 `None`。
#[must_use]
pub(crate) fn neoforge_to_mc(build: &str) -> Option<String> {
    let parts: Vec<&str> = build.split('.').collect();
    // 每段都必须非空且纯数字；`-beta`、`+snapshot` 等预发布串据此被排除
    if !parts
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    match parts.as_slice() {
        // 旧编码：`{mc minor}.{mc patch}.{build}` → MC `1.{minor}.{patch}`
        [major, minor, _build] if *major != "0" => Some(format!("1.{major}.{minor}")),
        // 新编码：`{mc major}.{mc minor}.{mc patch}.{build}` → MC `{major}.{minor}.{patch}`
        [major, minor, patch, _build] if *major != "0" => Some(format!("{major}.{minor}.{patch}")),
        _ => None,
    }
}

/// MC 版本 → `NeoForge` build 前缀（`1.21.1` → `21.1`，`26.3` → `26.3.0`）
///
/// [`neoforge_to_mc`] 的逆变换，两代编码各自对应：
///
/// - MC `1.x.y`（旧编码）→ 去掉 `1.`，得 `x.y`，匹配三段的 `x.y.*`。
/// - MC `26.x` / `26.x.y`（新编码）→ 主版本在前，末段是 build 号所以 MC 的
///   次版本之后补 `0`：`26.3` → `26.3.0`，`26.1.2` → `26.1.2`，匹配四段的
///   `26.3.0.*` / `26.1.2.*`。
///
/// 其它形状（快照名等）返回 `None`。
#[allow(
    dead_code,
    reason = "neoforge_to_mc 的逆变换，供按前缀筛 build 的调用方与测试复用"
)]
#[must_use]
pub(crate) fn mc_to_neoforge_prefix(mc: &str) -> Option<String> {
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if let Some(rest) = mc.strip_prefix("1.") {
        // 旧编码：`1.{minor}.{patch}` → `{minor}.{patch}`
        let (minor, patch) = rest.split_once('.')?;
        return (digits(minor) && digits(patch)).then(|| format!("{minor}.{patch}"));
    }
    // 新编码：`{major}.{minor}` → `{major}.{minor}.0`；`{major}.{minor}.{patch}` → 原样
    let parts: Vec<&str> = mc.split('.').collect();
    if !parts.iter().all(|p| digits(p)) {
        return None;
    }
    match parts.as_slice() {
        [major, minor] => Some(format!("{major}.{minor}.0")),
        [major, minor, patch] => Some(format!("{major}.{minor}.{patch}")),
        _ => None,
    }
}

/// build 串是否为合法形状：能映射到 MC 版本即为合法
fn is_valid_build(build: &str) -> bool {
    neoforge_to_mc(build).is_some()
}

/// MC 版本比较键，用于 `latest` 取「真实最新」而非字典序最大
///
/// `BTreeMap` 的字符串字典序会把 `26.1` 排在 `1.21.11` 之前（`'2' < '1'` 不成立，
/// 实际是 `'1'` 小于 `'2'`，所以 `1.x` 反而排最后），`latest` 会错取到旧线。
/// 这里把版本拆成数字元组比较：旧编码 `1.x.y` 与新年份制 `26.x` 统一成
/// `(major, minor, patch)`，按数值取最大。
fn mc_sort_key(mc: &str) -> Option<(u64, u64, u64)> {
    let nums: Vec<u64> = mc
        .split('.')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    match nums.as_slice() {
        [major, minor, patch] => Some((*major, *minor, *patch)),
        [major, minor] => Some((*major, *minor, 0)),
        _ => None,
    }
}

/// 取数值最大的 MC 版本（快照 / 无法解析的名不参与）
fn latest_mc(grouped: &BuildsByMc) -> Option<String> {
    grouped
        .keys()
        .filter_map(|mc| mc_sort_key(mc).map(|key| (key, mc)))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, mc)| mc.clone())
}

/// 版本标识 → 列表里的具体 MC 版本
fn resolve_mc_id(intent: &VersionId, grouped: &BuildsByMc) -> Option<String> {
    match VersionIntent::of(intent) {
        VersionIntent::Literal(raw) => grouped.contains_key(raw).then(|| raw.to_owned()),
        // `NeoForge` 索引没有 stable 标记，`latest` 与 `latest-snapshot` 同口径：
        // 取数值最大的 MC 版本（不能按字符串字典序，见 [`mc_sort_key`]）
        VersionIntent::Latest | VersionIntent::LatestSnapshot => latest_mc(grouped),
    }
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
        // 旧编码（MC 1.x）：三段，首段是 MC 次版本
        assert_eq!(neoforge_to_mc("21.1.200").as_deref(), Some("1.21.1"));
        assert_eq!(neoforge_to_mc("20.6.144").as_deref(), Some("1.20.6"));
        assert_eq!(neoforge_to_mc("21.0.167").as_deref(), Some("1.21.0"));
        // 新编码（MC 26.x 起）：四段，前缀就是 MC 版本本身
        assert_eq!(neoforge_to_mc("26.3.0.99").as_deref(), Some("26.3.0"));
        assert_eq!(neoforge_to_mc("26.1.2.124").as_deref(), Some("26.1.2"));
        // 快照 MC / 预发布 build 不映射
        assert_eq!(neoforge_to_mc("0.25w14craftmine.3-beta"), None);
        assert_eq!(neoforge_to_mc("21.1.200-beta"), None);
        assert_eq!(neoforge_to_mc("26.3.0.1-beta"), None);
        assert_eq!(neoforge_to_mc("26.1.0.0-alpha.1+snapshot-1"), None);
        assert_eq!(neoforge_to_mc("nonsense"), None);
        assert_eq!(neoforge_to_mc("21"), None);
        // 段数既不是 3 也不是 4
        assert_eq!(neoforge_to_mc("26.3.0.1.2"), None);
    }

    /// 前缀变换是映射的逆运算
    #[test]
    fn mc_prefix_round_trips_through_build() {
        assert_eq!(mc_to_neoforge_prefix("1.21.1").as_deref(), Some("21.1"));
        let prefix = mc_to_neoforge_prefix("1.21.1").expect("prefix");
        let mc = neoforge_to_mc(&format!("{prefix}.200")).expect("mc");
        assert_eq!(mc, "1.21.1");
        // 新编码：`26.3` 的 build 前缀是 `26.3.0`，末段才是 build 号
        assert_eq!(mc_to_neoforge_prefix("26.3").as_deref(), Some("26.3.0"));
        let prefix = mc_to_neoforge_prefix("26.3").expect("prefix");
        assert_eq!(
            neoforge_to_mc(&format!("{prefix}.99")).as_deref(),
            Some("26.3.0")
        );
        // MC 自身的补丁版本原样保留
        assert_eq!(mc_to_neoforge_prefix("26.1.2").as_deref(), Some("26.1.2"));
        assert_eq!(neoforge_to_mc("26.1.2.124").as_deref(), Some("26.1.2"));
        // 非 MC 版本形状不映射
        assert_eq!(mc_to_neoforge_prefix("1.21"), None);
        assert_eq!(mc_to_neoforge_prefix("25w14craftmine"), None);
    }

    /// 索引按 MC 分组：快照被剔除，组内保持旧在前的顺序（新旧两代编码并存）
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
        // 新编码：`26.3.0.99` 归到 MC `26.3.0`，`26.1.2.124` 归到 `26.1.2`
        assert_eq!(
            grouped.get("26.3.0").expect("26.3.0"),
            &vec!["26.3.0.98".to_owned(), "26.3.0.99".to_owned()]
        );
        assert_eq!(
            grouped.get("26.1.2").expect("26.1.2"),
            &vec![
                "26.1.2.122".to_owned(),
                "26.1.2.123".to_owned(),
                "26.1.2.124".to_owned()
            ]
        );
        // 快照 / 预发布 build 不产生分组
        assert!(!grouped.contains_key("1.26.3"));
        assert!(!grouped.keys().any(|mc| mc.starts_with("0.")));
    }

    /// 某 MC 下选末项（上游「旧在前」，末项最新）
    #[test]
    fn select_build_picks_last_entry() {
        let grouped = builds_by_mc(&index());
        assert_eq!(
            select_build("1.21.1", &grouped).as_deref(),
            Some("21.1.265")
        );
        assert_eq!(
            select_build("26.3.0", &grouped).as_deref(),
            Some("26.3.0.99")
        );
        assert_eq!(select_build("1.99.9", &grouped), None);
    }

    /// `latest` 取数值最大的 MC 版本，而不是字符串字典序
    #[test]
    fn resolve_latest_picks_highest_mc() {
        let grouped = builds_by_mc(&index());
        let latest = resolve_mc_id(&VersionId::new("latest"), &grouped).expect("latest");
        // `1.21.x` 的字典序最大项是 `1.21.1`，但它不是最新；新年份制 `26.3.0` 才是
        assert_eq!(latest, "26.3.0");
        assert_eq!(
            resolve_mc_id(&VersionId::new("latest-snapshot"), &grouped).as_deref(),
            Some("26.3.0")
        );
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
