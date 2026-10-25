//! `PaperMC` Fill v3 服务端来源（`Paper` / `Folia` / `Velocity` / `Waterfall`）
//!
//! 四个项目共用 Fill v3 的同一套端点与响应结构，差异只有「项目 id」与「变体
//! 标识」。因此这里**一个来源实现**由 [`PaperProject`] 参数化，而不是四个各自
//! 复制一份解析逻辑：新增一个 Paper 系分支只需往 [`PaperProject`] 加一个枚举项。
//!
//! # 端点
//!
//! ```text
//! GET {fill}/v3/projects/{project}
//! GET {fill}/v3/projects/{project}/versions/{version}/builds?channel=STABLE
//! ```
//!
//! # 上游约束（实测）
//!
//! - **必须**带非通用 `User-Agent`（含联系方式），否则可能被拒；本 crate 的
//!   [`crate::http::build_client`] 统一带 `mcsup/{版本}`。
//! - **v2 已日落**：`api.papermc.io/v2` 自 2026-07-01 起对一切请求返回
//!   `410 Gone`。这里只用 Fill v3。
//! - 下载地址与 sha256 **内嵌在 build 响应里**（键 `server:default`）。
//!   `fill-data.papermc.io/v1/objects/{sha256}/...` 与项目名无关，禁止自行拼。
//! - 不存在的项目 / 版本返回 `404`。
//!
//! # 版本口径
//!
//! 一个版本就是一个 Minecraft 版本（如 `1.21.1`），该 MC 版本下用哪个 build
//! 由本来源挑选（稳定渠道优先，其次取首条）。`latest` 取列表首项（上游「新在前」）。
//!
//! # 代理与服务端
//!
//! [`Velocity`](PaperProject::Velocity) 与 [`Waterfall`](PaperProject::Waterfall)
//! 是代理，启动语义与服务端不同，但**下载形态相同**（都是单个可运行 JAR），
//! 因此这里不做区分：`InstallKind::Direct`、`LaunchSpec::Jar` 都成立，差异留给
//! 使用方按变体决定（如是否要求 `server.properties`）。

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

use super::manifest::{Build, Project};

/// Fill v3 根地址
pub(crate) const OFFICIAL_FILL: &str = "https://fill.papermc.io";

/// 一个 Paper 系项目
///
/// 「项目 id / 变体 / 中文名」三件事的唯一口径。新增 Paper 系分支（上游再出
/// 一个 fork）只需在这里加一项，来源实现不必改。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaperProject {
    /// Paper：高性能服务端
    Paper,
    /// Folia：区域化多线程服务端
    Folia,
    /// Velocity：现代代理
    Velocity,
    /// Waterfall：BungeeCord 分支代理（已停止新构建，仅历史版本）
    Waterfall,
}

impl PaperProject {
    /// Fill v3 的项目 id
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Paper => "paper",
            Self::Folia => "folia",
            Self::Velocity => "velocity",
            Self::Waterfall => "waterfall",
        }
    }

    /// 对应的服务端变体
    #[must_use]
    pub fn variant(self) -> ServerVariant {
        match self {
            Self::Paper => ServerVariant::Paper,
            Self::Folia => ServerVariant::Folia,
            Self::Velocity => ServerVariant::Velocity,
            Self::Waterfall => ServerVariant::Waterfall,
        }
    }

    /// 是否为代理（`Velocity` / `Waterfall`）
    ///
    /// 下载形态与服务端一致（单个可运行 JAR），但语义不同：代理没有世界、
    /// 不读 `server.properties`。使用方按此决定后续处理。
    #[must_use]
    pub fn is_proxy(self) -> bool {
        matches!(self, Self::Velocity | Self::Waterfall)
    }
}

impl fmt::Display for PaperProject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// 挑选 build 的口径
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Picks {
    /// 稳定渠道（`STABLE`）最新 build；没有稳定版时退回该版本首条
    #[default]
    LatestStable,
    /// 指定 build 编号
    Fixed {
        /// build 编号
        build: u64,
    },
}

/// `PaperMC` Fill v3 服务端来源
///
/// [`PaperSource::new`] 对接官方；一个实例只服务一个 [`PaperProject`]。
pub struct PaperSource {
    http: Client,
    /// Fill v3 根地址（不带尾斜杠），官方或镜像
    fill_base: String,
    project: PaperProject,
    picks: Picks,
}

impl fmt::Debug for PaperSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaperSource")
            .field("fill_base", &self.fill_base)
            .field("project", &self.project)
            .field("picks", &self.picks)
            .finish_non_exhaustive()
    }
}

impl PaperSource {
    /// 对接官方 Fill v3 的默认实例
    ///
    /// 构造不打网络，只准备 HTTP 客户端。
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败时返回 [`SourceError::Init`]
    pub fn new(project: PaperProject) -> Result<Self, SourceError> {
        Self::builder(project).build()
    }

    /// 构造器：改镜像地址或选版策略
    #[must_use]
    pub fn builder(project: PaperProject) -> PaperSourceBuilder {
        PaperSourceBuilder {
            project,
            fill_base: None,
            picks: None,
        }
    }

    async fn get<T>(&self, url: &str, not_found: Option<VersionId>) -> Result<T, SourceError>
    where
        T: DeserializeOwned,
    {
        get_json(&self.http, url, not_found).await
    }

    /// 项目元数据地址（版本列表）
    fn project_url(&self) -> String {
        format!("{}/v3/projects/{}", self.fill_base, self.project.id())
    }

    /// 某版本的 build 列表地址
    ///
    /// **不带 `channel` 过滤**：上游把 `channel=STABLE` 应用到「该版本暂无稳定
    /// build」时返回空数组（Folia 的 `26.3` 实测如此），于是「版本存在但没有
    /// 稳定版」与「版本不存在」变得无法区分。这里拉全量，由 [`select_build`]
    /// 在客户端挑稳定渠道、没有稳定版时退回首个可下载 build。
    fn builds_url(&self, version: &str) -> String {
        format!(
            "{}/v3/projects/{}/versions/{version}/builds",
            self.fill_base,
            self.project.id()
        )
    }

    /// 拉取项目元数据
    ///
    /// 镜像若把项目路径搞混（回显的 id 与请求的不一致），这里报错而不是把别的
    /// 项目的版本当成自己的。
    async fn project(&self) -> Result<Project, SourceError> {
        let project: Project = self.get(&self.project_url(), None).await?;
        if !project.id().is_empty() && project.id() != self.project.id() {
            return Err(SourceError::Decode(Box::new(std::io::Error::other(
                format!(
                    "fill v3 returned project `{}` for requested `{}`",
                    project.id(),
                    self.project.id()
                ),
            ))));
        }
        Ok(project)
    }

    /// 把版本标识解析为具体版本（含 `latest` / `latest-snapshot` 别名）
    ///
    /// Fill v3 只列正式版本，没有快照标记：`latest-snapshot` 只能与 `latest`
    /// 同口径（取首项），这与上游实际提供的版本集合一致。
    ///
    /// `latest` 取得的首项可能**没有可下载 build**（如 Folia 的 `26.3` 刚开线
    /// 时只有空 build 列表）。因此这里逐个往下试，直到某个版本能选出 build
    /// 为止——否则用户装 `latest` 会撞上一个「版本存在但装不了」的死胡同。
    async fn resolve_version(
        &self,
        version: &VersionId,
    ) -> Result<(String, Vec<Build>), SourceError> {
        let project = self.project().await?;
        let versions = project.flat_versions();
        let intent = VersionIntent::of(version);
        let ordered: Vec<String> = match intent {
            // 字面版本不做回退：指定的版本没有 build 就如实报错
            VersionIntent::Literal(raw) => {
                pick_by_version_aliases(intent, versions, |_| true, |_| true)
                    .into_iter()
                    .filter(|candidate| candidate == raw)
                    .collect()
            }
            VersionIntent::Latest | VersionIntent::LatestSnapshot => {
                versions.into_iter().map(str::to_owned).collect()
            }
        };
        for candidate in ordered {
            // 两个「该版本不可用」的形态都跳过，继续试下一个版本：
            // 空列表（版本刚开线、还没出 build）与 404（上游不认这个版本）
            match self.fetch_builds(&candidate).await {
                Ok(builds) if !builds.is_empty() => return Ok((candidate, builds)),
                Ok(_) | Err(SourceError::NotFound(_)) => {}
                Err(err) => return Err(err),
            }
        }
        Err(SourceError::NotFound(version.clone()))
    }

    /// 在某版本下选 build
    ///
    /// 复用已经拉到的列表，不再重复请求（`resolve_version` 为了挑出可安装的
    /// `latest` 已经付过一次代价）。
    fn pick_build_with(&self, version: &str, builds: Vec<Build>) -> Result<Build, SourceError> {
        if let Picks::Fixed { build } = self.picks {
            return builds
                .into_iter()
                .find(|candidate| candidate.id == build)
                .ok_or_else(|| SourceError::NotFound(VersionId::new(version)));
        }
        select_build(&builds, version)
    }

    async fn fetch_builds(&self, version: &str) -> Result<Vec<Build>, SourceError> {
        self.get(&self.builds_url(version), Some(VersionId::new(version)))
            .await
    }
}

#[async_trait]
impl ServerJarSource for PaperSource {
    fn variant(&self) -> ServerVariant {
        self.project.variant()
    }

    fn name(&self) -> &'static str {
        self.project.id()
    }

    async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError> {
        let project = self.project().await?;
        let variant = self.project.variant();
        Ok(project
            .flat_versions()
            .into_iter()
            .map(|id| into_server_version(id, variant.clone()))
            .collect())
    }

    async fn artifact(&self, version: &VersionId) -> Result<port::ResolvedArtifact, SourceError> {
        let (resolved_version, builds) = self.resolve_version(version).await?;
        let build = self.pick_build_with(&resolved_version, builds)?;
        let download = build.server_download().ok_or_else(|| {
            SourceError::Decode(Box::new(std::io::Error::other(format!(
                "build {} ({}) of {} {} has no server download",
                build.id,
                build.server_download().map_or("", |d| d.file_name()),
                self.project.id(),
                resolved_version
            ))))
        })?;
        let location = parse_https_url(&download.url)?;
        let mut resolved = Downloads::direct(location).size(download.size);
        if let Some(checksum) = download.sha256().and_then(parse_sha256_hex) {
            resolved = resolved.checksum(checksum);
        }
        Ok(resolved.resolve(version))
    }
}

/// 从 build 列表里选一条
///
/// 列表已去掉服务端 `channel` 过滤（见 [`PaperSource::builds_url`] 的说明），
/// 因此这里**必须**显式挑「第一条稳定渠道」；没有稳定版时才退回首条可下载
/// build，而不是让安装失败（Folia 等项目的早期版本常常只有 `ALPHA`）。
fn select_build(builds: &[Build], version: &str) -> Result<Build, SourceError> {
    builds
        .iter()
        .find(|build| build.is_stable() && build.server_download().is_some())
        .or_else(|| {
            builds
                .iter()
                .find(|build| build.server_download().is_some())
        })
        .cloned()
        .ok_or_else(|| SourceError::NotFound(VersionId::new(version)))
}

/// 具体版本 → 领域版本
///
/// Fill v3 只提供正式版本，统一标 `Release`；Java 大版本由原版元数据给出。
fn into_server_version(id: &str, variant: ServerVariant) -> ServerVersion {
    ServerVersion {
        id: VersionId::new(id),
        kind: VersionKind::Release,
        variant,
        java_major: None,
    }
}

/// [`PaperSource`] 的构造器
///
/// 第三方镜像换 [`Self::fill_base`]；版本策略通过 [`Self::picks`] 接管。
#[derive(Debug)]
pub struct PaperSourceBuilder {
    project: PaperProject,
    fill_base: Option<String>,
    picks: Option<Picks>,
}

impl PaperSourceBuilder {
    /// 换 Fill v3 根地址（镜像）
    ///
    /// 地址需与官方同构（`/v3/projects/{项目}/...`）。只接受 `https`。
    #[must_use]
    pub fn fill_base(mut self, base: impl Into<String>) -> Self {
        self.fill_base = Some(base.into());
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
    /// HTTP 客户端构建失败，或 `fill_base` 非 `https` 时返回 [`SourceError::Init`]
    pub fn build(self) -> Result<PaperSource, SourceError> {
        Ok(PaperSource {
            http: build_client()?,
            fill_base: normalize_base(self.fill_base, OFFICIAL_FILL)?,
            project: self.project,
            picks: self.picks.unwrap_or_default(),
        })
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

    /// 四个项目各自声明对应变体
    #[test]
    fn each_project_declares_its_variant() {
        let cases = [
            (PaperProject::Paper, "paper", ServerVariant::Paper),
            (PaperProject::Folia, "folia", ServerVariant::Folia),
            (PaperProject::Velocity, "velocity", ServerVariant::Velocity),
            (
                PaperProject::Waterfall,
                "waterfall",
                ServerVariant::Waterfall,
            ),
        ];
        for (project, id, variant) in cases {
            let source = PaperSource::new(project).expect("paper source");
            assert_eq!(source.variant(), variant);
            assert_eq!(source.name(), id);
            assert_eq!(project.id(), id);
        }
    }

    /// 代理标记只对 Velocity / Waterfall 为真
    #[test]
    fn proxies_are_flagged() {
        assert!(PaperProject::Velocity.is_proxy());
        assert!(PaperProject::Waterfall.is_proxy());
        assert!(!PaperProject::Paper.is_proxy());
        assert!(!PaperProject::Folia.is_proxy());
    }

    /// 内置项目清单覆盖全部四个变体
    #[test]
    fn all_builtin_projects_are_covered() {
        let projects = [
            PaperProject::Paper,
            PaperProject::Folia,
            PaperProject::Velocity,
            PaperProject::Waterfall,
        ];
        let ids: Vec<_> = projects.iter().map(|p| p.id()).collect();
        assert_eq!(ids, ["paper", "folia", "velocity", "waterfall"]);
        let variants: Vec<_> = projects.iter().map(|p| p.variant()).collect();
        assert_eq!(
            variants,
            [
                ServerVariant::Paper,
                ServerVariant::Folia,
                ServerVariant::Velocity,
                ServerVariant::Waterfall
            ]
        );
    }

    /// 项目元数据回显 id（镜像搞混项目时靠它报错）
    #[test]
    fn project_echoes_id() {
        let project = project();
        assert_eq!(project.id(), "paper");
    }

    /// 端点拼装
    #[test]
    fn builds_endpoints() {
        let source = PaperSource::new(PaperProject::Paper).expect("paper source");
        assert_eq!(
            source.project_url(),
            "https://fill.papermc.io/v3/projects/paper"
        );
        assert_eq!(
            source.builds_url("1.21.1"),
            "https://fill.papermc.io/v3/projects/paper/versions/1.21.1/builds"
        );
    }

    /// 镜像去尾斜杠
    #[test]
    fn trims_trailing_slash_on_mirror_base() {
        let source = PaperSource::builder(PaperProject::Folia)
            .fill_base("https://mirror.example/fill/")
            .build()
            .expect("folio source");
        assert_eq!(
            source.project_url(),
            "https://mirror.example/fill/v3/projects/folia"
        );
    }

    /// 非 https 镜像根被拒绝
    #[test]
    fn rejects_insecure_base() {
        let err = PaperSource::builder(PaperProject::Paper)
            .fill_base("http://mirror.example/fill")
            .build();
        assert!(matches!(err, Err(SourceError::Init(_))));
    }

    /// 选 build：稳定渠道优先
    #[test]
    fn select_build_prefers_stable_channel() {
        let picked = select_build(&builds(), "1.21.1").expect("build");
        assert_eq!(picked.id, 133);
        assert!(picked.is_stable());
    }

    /// 镜像忽略 channel 参数时，仍挑稳定渠道而不是首条
    #[test]
    fn select_build_skips_unstable_channels() {
        let raw = r#"[
            {"id":99,"channel":"ALPHA","downloads":{"server:default":{"url":"https://x/a.jar","size":1}}},
            {"id":98,"channel":"STABLE","downloads":{"server:default":{"url":"https://x/b.jar","size":1}}}
        ]"#;
        let builds: Vec<Build> = serde_json::from_str(raw).expect("parse");
        let picked = select_build(&builds, "1.21.1").expect("build");
        assert_eq!(picked.id, 98);
    }

    /// 没有稳定渠道时退回首条可下载 build，而不是失败
    #[test]
    fn select_build_falls_back_to_first_downloadable() {
        let raw = r#"[
            {"id":99,"channel":"ALPHA","downloads":{"server:default":{"url":"https://x/a.jar","size":1}}}
        ]"#;
        let builds: Vec<Build> = serde_json::from_str(raw).expect("parse");
        let picked = select_build(&builds, "1.21.1").expect("build");
        assert_eq!(picked.id, 99);
    }

    /// 空列表 / 无可下载物时报 `NotFound`
    #[test]
    fn select_build_empty_is_not_found() {
        let err = select_build(&[], "1.21.1").expect_err("应报 NotFound");
        assert!(matches!(err, SourceError::NotFound(_)), "{err:?}");
        let raw = r#"[{"id":1,"channel":"STABLE","downloads":{}}]"#;
        let builds: Vec<Build> = serde_json::from_str(raw).expect("parse");
        let err = select_build(&builds, "1.21.1").expect_err("应报 NotFound");
        assert!(matches!(err, SourceError::NotFound(_)), "{err:?}");
    }

    /// 版本列表展平后标对应变体
    #[test]
    fn version_list_marks_variant() {
        let mapped: Vec<_> = project()
            .flat_versions()
            .into_iter()
            .map(|id| into_server_version(id, ServerVariant::Paper))
            .collect();
        assert!(mapped.iter().all(|v| v.variant == ServerVariant::Paper));
        assert!(mapped.iter().all(|v| v.kind == VersionKind::Release));
        assert!(mapped.iter().any(|v| v.id.as_str() == "1.21.1"));
    }

    /// 固定 build：只认指定编号
    #[test]
    fn fixed_picks_select_exact_build() {
        let source = PaperSource::builder(PaperProject::Paper)
            .picks(Picks::Fixed { build: 132 })
            .build()
            .expect("paper source");
        assert_eq!(source.picks, Picks::Fixed { build: 132 });
        let picked = builds()
            .into_iter()
            .find(|candidate| candidate.id == 132)
            .expect("132");
        assert_eq!(picked.id, 132);
    }
}
