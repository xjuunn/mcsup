//! Modrinth Labrinth v2 客户端
//!
//! 对接 <https://docs.modrinth.com/api/>。覆盖本项目下载 Mod / Modpack /
//! `ResourcePack` / Datapack / Shader / Plugin 所需的查询能力：搜索、项目详情、
//! 版本枚举与详情、按文件哈希反查、依赖图、以及标签（加载器 / 游戏版本 /
//! 分类 / 项目类型）。真正的字节下载不在这里——由 provider 的下载器按返回的
//! [`VersionFile`] 地址与哈希执行，本模块只回答「去哪里拿、拿的是什么」。
//!
//! # 端点
//!
//! ```text
//! GET  /search
//! GET  /project/{id|slug}
//! GET  /projects?ids=[...]             # 批量项目
//! GET  /project/{id|slug}/version
//! GET  /version/{id}
//! GET  /versions?ids=[...]
//! GET  /version_file/{hash}?algorithm=sha512
//! GET  /project/{id|slug}/dependencies
//! GET  /tag/loader
//! GET  /tag/game_version
//! GET  /tag/category
//! GET  /tag/project_type
//! ```
//!
//! # 上游约束
//!
//! - **必须**带可辨识的 `User-Agent`，否则可能被封。本 crate 的
//!   [`crate::http::build_client`] 统一带 `mcsup/{版本}`，这里用构造器可覆盖
//!   成带联系方式的完整 UA（Modrinth 官方推荐）。
//! - 限流按 IP，300 请求 / 分钟。超限返回 `429`，归类为可重试。
//! - 找不到资源返回 `404`；`facets` 非法返回 `400` + `{error, description}`。
//! - 批量端点的 `ids` 是**JSON 数组字符串**（如 `["A","B"]`），不是逗号分隔，
//!   必须按 JSON 编码后再作为 query / body 传。
//!
//! # 换源与自建
//!
//! 通过 [`ModrinthSource::builder`] 的 [`base_url`](ModrinthSourceBuilder::base_url)
//! 指向任意与官方 v2 同构的实例（自建 Labrinth、镜像）。只接受 `https`，
//! 与其余来源保持一致，避免明文降级。

use crate::http::{build_client, get_json, post_json};
use crate::modrinth::facets::FacetTree;
use crate::modrinth::manifest::{
    CategoryTag, DependencyGraph, GameVersionTag, LoaderTag, Project, SearchHit, SearchResults,
    TeamMember, Version, VersionFile,
};
use reqwest::Client;
use serde::de::DeserializeOwned;
use std::fmt;
use url::Url;

/// Modrinth 官方 API 根地址
pub(crate) const OFFICIAL_API: &str = "https://api.modrinth.com/v2";

/// 项目类型（下载资源分类的唯一口径）
///
/// 覆盖本项目要下载的全部资源类型。上游未来可能新增类型，[`Self::Other`]
/// 承载未知值，不拒绝解析——判定逻辑按需 `match`，未知值落到兜底分支。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectType {
    /// 模组
    Mod,
    /// 整合包
    Modpack,
    /// 资源包
    ResourcePack,
    /// 数据包
    DataPack,
    /// 光影
    Shader,
    /// 插件（服务端）
    Plugin,
    /// 上游新增 / 未识别的类型（保留原值）
    Other(String),
}

impl ProjectType {
    /// 由上游字符串构造，已知值归一为内置项
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "mod" => Self::Mod,
            "modpack" => Self::Modpack,
            "resourcepack" => Self::ResourcePack,
            "datapack" => Self::DataPack,
            "shader" => Self::Shader,
            "plugin" => Self::Plugin,
            other => Self::Other(other.to_owned()),
        }
    }

    /// 上游机器名（写进 facet / 比较用的稳定值）
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Mod => "mod",
            Self::Modpack => "modpack",
            Self::ResourcePack => "resourcepack",
            Self::DataPack => "datapack",
            Self::Shader => "shader",
            Self::Plugin => "plugin",
            Self::Other(raw) => raw,
        }
    }
}

impl fmt::Display for ProjectType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 版本发布渠道
///
/// 上游可能新增渠道，[`Self::Other`] 承载未知值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionChannel {
    /// 正式版
    Release,
    /// 测试版
    Beta,
    /// 内测版
    Alpha,
    /// 未识别渠道（保留原值）
    Other(String),
}

impl VersionChannel {
    /// 由上游字符串构造
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "release" => Self::Release,
            "beta" => Self::Beta,
            "alpha" => Self::Alpha,
            other => Self::Other(other.to_owned()),
        }
    }

    /// 上游机器名
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Release => "release",
            Self::Beta => "beta",
            Self::Alpha => "alpha",
            Self::Other(raw) => raw,
        }
    }
}

/// 一次搜索的条件
///
/// 所有字段可选，缺省即不加对应过滤。`facets` 由 [`FacetTree`] 承载，构造语义
/// 见 [`crate::modrinth::facets`]。`index` 决定排序口径。
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    /// 搜索关键词；空串表示按条件浏览
    pub query: Option<String>,
    /// facet 过滤条件（AND / OR）
    pub facets: FacetTree,
    /// 排序口径
    pub index: SearchIndex,
    /// 跳过的结果数
    pub offset: Option<u32>,
    /// 返回的结果数（上游上限 100）
    pub limit: Option<u32>,
}

impl SearchQuery {
    /// 空条件（按默认排序浏览）
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置关键词
    #[must_use]
    pub fn query(mut self, query: impl Into<String>) -> Self {
        self.query = Some(query.into());
        self
    }

    /// 设置 facet 条件
    #[must_use]
    pub fn facets(mut self, facets: FacetTree) -> Self {
        self.facets = facets;
        self
    }

    /// 设置排序口径
    #[must_use]
    pub fn index(mut self, index: SearchIndex) -> Self {
        self.index = index;
        self
    }

    /// 设置分页
    #[must_use]
    pub fn page(mut self, offset: u32, limit: u32) -> Self {
        self.offset = Some(offset);
        self.limit = Some(limit);
        self
    }
}

/// 搜索排序口径
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchIndex {
    /// 相关度（默认）
    #[default]
    Relevance,
    /// 下载量
    Downloads,
    /// 关注数
    Follows,
    /// 最新发布
    Newest,
    /// 最近更新
    Updated,
}

impl SearchIndex {
    /// 上游排序名
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Relevance => "relevance",
            Self::Downloads => "downloads",
            Self::Follows => "follows",
            Self::Newest => "newest",
            Self::Updated => "updated",
        }
    }
}

/// 版本列表的过滤条件
///
/// 对应 `GET /project/{id}/version` 的 query 参数。`loaders` / `game_versions`
/// 是「任一匹配即可」（OR），由上游按此语义处理。
#[derive(Debug, Clone, Default)]
pub struct VersionFilter {
    /// 加载器过滤（空表示不限）
    pub loaders: Vec<String>,
    /// 游戏版本过滤（空表示不限）
    pub game_versions: Vec<String>,
    /// 仅要精选版本 / 仅要非精选版本；`None` 表示不限
    pub featured: Option<bool>,
    /// 是否包含 changelog（上游默认 `true`，量大时建议关掉）
    pub include_changelog: bool,
}

impl VersionFilter {
    /// 默认过滤：包含 changelog，不加任何条件
    #[must_use]
    pub fn new() -> Self {
        Self {
            include_changelog: true,
            ..Self::default()
        }
    }

    /// 指定加载器
    #[must_use]
    pub fn loaders(mut self, loaders: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.loaders = loaders.into_iter().map(Into::into).collect();
        self
    }

    /// 指定游戏版本
    #[must_use]
    pub fn game_versions(mut self, versions: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.game_versions = versions.into_iter().map(Into::into).collect();
        self
    }

    /// 是否排除 changelog（省流量）
    #[must_use]
    pub fn without_changelog(mut self) -> Self {
        self.include_changelog = false;
        self
    }
}

/// 按哈希反查版本时使用的哈希算法
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm {
    /// SHA-1（上游默认）
    Sha1,
    /// SHA-512
    Sha512,
}

impl HashAlgorithm {
    /// 上游算法名
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha512 => "sha512",
        }
    }
}

/// Modrinth 资源来源
///
/// 用 [`ModrinthSource::new`] 得到对接官方的默认实例；自建 / 镜像通过
/// [`ModrinthSource::builder`] 改 `base_url`。
pub struct ModrinthSource {
    http: Client,
    /// API 根地址（不带尾斜杠），官方或镜像
    base_url: String,
}

impl fmt::Debug for ModrinthSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModrinthSource")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

/// URL 处理失败
#[derive(Debug, thiserror::Error)]
pub(crate) enum UrlError {
    /// 地址无法解析
    #[error("invalid URL: {0}")]
    Parse(#[source] url::ParseError),
    /// 只接受 `https`
    #[error("only https URLs are accepted, got schema {0:?}")]
    Insecure(String),
}

impl ModrinthSource {
    /// 对接官方 API 的默认实例
    ///
    /// 构造不打网络，只准备 HTTP 客户端。
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败时返回 [`port::SourceError::Init`]
    pub fn new() -> Result<Self, port::SourceError> {
        Self::builder().build()
    }

    /// 构造器：改 API 根地址（自建 / 镜像）
    #[must_use]
    pub fn builder() -> ModrinthSourceBuilder {
        ModrinthSourceBuilder::default()
    }

    /// API 根地址（不含尾斜杠）
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 测试专用：接受任意 scheme 的根地址
    ///
    /// 生产入口只接受 `https`（见 [`ModrinthSourceBuilder::build`]）。端点契约
    /// 测试要打本机 `http://127.0.0.1:<port>` 的 mock 服务，因此单开一个
    /// 不限 scheme 的构造入口。`#[doc(hidden)]`：它不在公开契约里，但跨 crate
    /// 的集成测试需要能调用。
    ///
    /// # Errors
    ///
    /// 地址无法解析或 HTTP 客户端构建失败时返回 [`port::SourceError::Init`]
    #[doc(hidden)]
    pub fn for_test(base_url: &str) -> Result<Self, port::SourceError> {
        let base_url = base_url.trim_end_matches('/').to_owned();
        base_url
            .parse::<Url>()
            .map_err(|e| port::SourceError::Init(Box::new(UrlError::Parse(e))))?;
        Ok(Self {
            http: build_client()?,
            base_url,
        })
    }

    /// 拼接根地址下的路径
    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// 通用 GET
    async fn get<T>(&self, url: &str) -> Result<T, port::SourceError>
    where
        T: DeserializeOwned,
    {
        get_json(&self.http, url, None).await
    }

    /// 搜索项目
    ///
    /// # Errors
    ///
    /// 上游不可达、状态码异常、facets 非法（400）或响应无法解析时返回
    /// [`port::SourceError`]
    pub async fn search(&self, query: &SearchQuery) -> Result<SearchResults, port::SourceError> {
        let mut params: Vec<String> = Vec::new();
        if let Some(q) = &query.query
            && !q.is_empty()
        {
            params.push(format!("query={}", urlencode(q)));
        }
        if !query.facets.is_empty() {
            params.push(format!("facets={}", urlencode(&query.facets.encode())));
        }
        params.push(format!("index={}", urlencode(query.index.as_str())));
        if let Some(offset) = query.offset {
            params.push(format!("offset={offset}"));
        }
        if let Some(limit) = query.limit {
            // 上游上限 100：超出会给 400，这里夹一旦保调用方拿到结果
            params.push(format!("limit={}", limit.min(100)));
        }
        let url = self.endpoint(&format!("/search?{}", params.join("&")));
        self.get(&url).await
    }

    /// 读取单个项目详情（按 id 或 slug）
    ///
    /// # Errors
    ///
    /// 项目不存在返回 [`port::SourceError::Status`]（404）；其余同上
    pub async fn project(&self, id_or_slug: &str) -> Result<Project, port::SourceError> {
        let url = self.endpoint(&format!("/project/{}", urlencode_path(id_or_slug)));
        self.get(&url).await
    }

    /// 批量读取项目详情（`GET /projects?ids=[...]`）
    ///
    /// `ids` 必须是 JSON 数组字符串（上游要求），不是逗号分隔。上游按请求的
    /// id 顺序不易保证，也不拒绝部分缺失；返回的列表可能短于请求列表，调用方
    /// 应按 `id` 自行索引。
    ///
    /// # Errors
    ///
    /// 上游不可达、状态码异常或响应无法解析时返回 [`port::SourceError`]
    pub async fn projects(&self, ids: &[String]) -> Result<Vec<Project>, port::SourceError> {
        let url = self.endpoint(&format!("/projects?ids={}", urlencode(&json_array(ids))));
        self.get(&url).await
    }

    /// 列出某项目的版本
    ///
    /// # Errors
    ///
    /// 项目不存在（404）或响应无法解析时返回 [`port::SourceError`]
    pub async fn project_versions(
        &self,
        id_or_slug: &str,
        filter: &VersionFilter,
    ) -> Result<Vec<Version>, port::SourceError> {
        let mut params: Vec<String> = Vec::new();
        if !filter.loaders.is_empty() {
            params.push(format!(
                "loaders={}",
                urlencode(&json_array(&filter.loaders))
            ));
        }
        if !filter.game_versions.is_empty() {
            params.push(format!(
                "game_versions={}",
                urlencode(&json_array(&filter.game_versions))
            ));
        }
        if let Some(featured) = filter.featured {
            params.push(format!("featured={featured}"));
        }
        params.push(format!("include_changelog={}", filter.include_changelog));
        let url = self.endpoint(&format!(
            "/project/{}/version?{}",
            urlencode_path(id_or_slug),
            params.join("&")
        ));
        self.get(&url).await
    }

    /// 读取单个版本详情（按版本 id）
    ///
    /// # Errors
    ///
    /// 版本不存在（404）或响应无法解析时返回 [`port::SourceError`]
    pub async fn version(&self, version_id: &str) -> Result<Version, port::SourceError> {
        let url = self.endpoint(&format!("/version/{}", urlencode_path(version_id)));
        self.get(&url).await
    }

    /// 批量读取版本详情（`GET /versions?ids=`）
    ///
    /// # Errors
    ///
    /// 上游不可达、状态码异常或响应无法解析时返回 [`port::SourceError`]
    pub async fn versions(&self, ids: &[String]) -> Result<Vec<Version>, port::SourceError> {
        let url = self.endpoint(&format!("/versions?ids={}", urlencode(&json_array(ids))));
        self.get(&url).await
    }

    /// 按文件哈希反查版本（`GET /version_file/{hash}`）
    ///
    /// 用于「本地已有 jar，判断它属于哪个版本」。`multiple=false`（默认）时
    /// 上游返回单个 [`Version`]；本方法保留单对象语义。
    ///
    /// # Errors
    ///
    /// 哈希未命中（404）或响应无法解析时返回 [`port::SourceError`]
    pub async fn version_from_hash(
        &self,
        hash: &str,
        algorithm: HashAlgorithm,
    ) -> Result<Version, port::SourceError> {
        let url = self.endpoint(&format!(
            "/version_file/{}?algorithm={}",
            urlencode_path(hash),
            algorithm.as_str()
        ));
        self.get(&url).await
    }

    /// 批量按文件哈希反查版本（`POST /version_files`）
    ///
    /// 用于「扫描 `mods/` 目录，判断每个 jar 分别属于哪个版本」。上游按请求里
    /// 的哈希逐个返回，未命中的哈希不出现在结果中，因此返回的 map 可能比请求
    /// 短——调用方按哈希自行查表。
    ///
    /// # Errors
    ///
    /// 上游不可达、状态码异常或响应无法解析时返回 [`port::SourceError`]
    pub async fn versions_from_hashes(
        &self,
        hashes: &[String],
        algorithm: HashAlgorithm,
    ) -> Result<std::collections::HashMap<String, Version>, port::SourceError> {
        let body = serde_json::json!({
            "hashes": hashes,
            "algorithm": algorithm.as_str(),
        });
        let body = serde_json::to_string(&body).map_err(|e| {
            port::SourceError::Decode(Box::new(std::io::Error::other(format!(
                "encode version hashes: {e}"
            ))))
        })?;
        let url = self.endpoint("/version_files");
        post_json(&self.http, &url, &body).await
    }

    /// 读取项目的依赖图（`GET /project/{id}/dependencies`）
    ///
    /// # Errors
    ///
    /// 项目不存在（404）或响应无法解析时返回 [`port::SourceError`]
    pub async fn project_dependencies(
        &self,
        id_or_slug: &str,
    ) -> Result<DependencyGraph, port::SourceError> {
        let url = self.endpoint(&format!(
            "/project/{}/dependencies",
            urlencode_path(id_or_slug)
        ));
        self.get(&url).await
    }

    /// 读取团队成员（`GET /team/{id}/members`）
    ///
    /// 项目详情只给 `team` id；作者昵称要再查成员列表。上游没有团队单体
    /// 路由，只有成员列表——这是唯一的作者信息来源。
    ///
    /// # Errors
    ///
    /// 团队不存在（404）或响应无法解析时返回 [`port::SourceError`]
    pub async fn team_members(&self, team_id: &str) -> Result<Vec<TeamMember>, port::SourceError> {
        let url = self.endpoint(&format!("/team/{}/members", urlencode_path(team_id)));
        self.get(&url).await
    }

    /// 加载器标签列表
    ///
    /// # Errors
    ///
    /// 上游不可达或响应无法解析时返回 [`port::SourceError`]
    pub async fn loaders(&self) -> Result<Vec<LoaderTag>, port::SourceError> {
        self.get(&self.endpoint("/tag/loader")).await
    }

    /// 游戏版本标签列表
    ///
    /// # Errors
    ///
    /// 上游不可达或响应无法解析时返回 [`port::SourceError`]
    pub async fn game_versions(&self) -> Result<Vec<GameVersionTag>, port::SourceError> {
        self.get(&self.endpoint("/tag/game_version")).await
    }

    /// 分类标签列表
    ///
    /// # Errors
    ///
    /// 上游不可达或响应无法解析时返回 [`port::SourceError`]
    pub async fn categories(&self) -> Result<Vec<CategoryTag>, port::SourceError> {
        self.get(&self.endpoint("/tag/category")).await
    }

    /// 项目类型列表（`GET /tag/project_type`，上游返回纯字符串数组）
    ///
    /// # Errors
    ///
    /// 上游不可达或响应无法解析时返回 [`port::SourceError`]
    pub async fn project_types(&self) -> Result<Vec<String>, port::SourceError> {
        self.get(&self.endpoint("/tag/project_type")).await
    }
}

/// [`ModrinthSource`] 的构造器
///
/// 自建 Labrinth / 镜像只需换 [`Self::base_url`]。与其余来源一致，只接受
/// `https`，尾斜杠会被去掉。
#[derive(Debug, Default)]
pub struct ModrinthSourceBuilder {
    base_url: Option<String>,
}

impl ModrinthSourceBuilder {
    /// 换 API 根地址（自建 / 镜像）
    ///
    /// 地址需与官方 v2 同构，例如 `https://labrinth.example.com/v2`。
    /// 只接受 `https`，尾斜杠会被去掉。
    #[must_use]
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// 装配来源
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败，或 `base_url` 非 `https` 时返回
    /// [`port::SourceError::Init`]
    pub fn build(self) -> Result<ModrinthSource, port::SourceError> {
        let base_url = self
            .base_url
            .unwrap_or_else(|| OFFICIAL_API.to_owned())
            .trim_end_matches('/')
            .to_owned();
        let parsed = base_url
            .parse::<Url>()
            .map_err(|e| port::SourceError::Init(Box::new(UrlError::Parse(e))))?;
        if parsed.scheme() != "https" {
            return Err(port::SourceError::Init(Box::new(UrlError::Insecure(
                parsed.scheme().to_owned(),
            ))));
        }
        Ok(ModrinthSource {
            http: build_client()?,
            base_url,
        })
    }
}

/// 把字符串 URL 编码为 query 参数值
///
/// 直接拼 query 时必须对值做百分号编码：facets 里含 `[` `]` `"` 空格，版本号
/// 里含 `+`，不转义会让上游把参数截断或误解析。
fn urlencode(raw: &str) -> String {
    url::form_urlencoded::byte_serialize(raw.as_bytes()).collect()
}

/// 把路径段编码为单个路径片段
///
/// 路径里的 id / slug 只允许 base62 与少数符号；`+`（某些 slug 可能含）在
/// 路径中不特殊，但空格等仍需转义。这里用 path 段序列化，保留 `-` `.` `_` `~`。
fn urlencode_path(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// 把字符串列表序列化为 JSON 数组字符串（如 `["a","b"]`）
///
/// 上游的批量 id 与版本过滤参数要的是 JSON 数组字符串，不是逗号分隔。
fn json_array(items: &[String]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_owned())
}

/// 便于测试断言：把 [`VersionFile`] 的主文件哈希取为 [`domain::server::artifact::Checksum`]
///
/// Modrinth 每个文件都有 sha1 与 sha512；本项目下载器只认 sha1 / sha256，
/// 这里优先取 sha1。只有 sha512 可用时返回 `None`——下载侧据此跳过校验，
/// 而不是拿 sha512 冒充 sha1。
#[must_use]
pub fn file_checksum(file: &VersionFile) -> Option<domain::server::artifact::Checksum> {
    let sha1 = file.hashes.sha1.as_ref()?;
    if sha1.len() != 40 || !sha1.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(domain::server::artifact::Checksum::new(
        domain::server::artifact::ChecksumAlgorithm::Sha1,
        sha1.clone(),
    ))
}

/// 从版本中选主文件
///
/// 上游约定：每个版本最多一个 `primary=true`；若没有主文件，则「第一个文件
/// 即主文件」。本函数如实实现该约定，`None` 表示该版本没有任何文件。
#[must_use]
pub fn primary_file(version: &Version) -> Option<&VersionFile> {
    version
        .files
        .iter()
        .find(|f| f.primary)
        .or_else(|| version.files.first())
}

/// 从版本中选服务端归档
///
/// Modrinth 允许同一版本同时上传客户端与服务端文件，服务端文件通常不是
/// `primary`，只在文件名里带标记（官方常见写法 `[server]`）。服务端安装
/// 必须优先取它：客户端包会把仅客户端模组标成 `server: required`，直接
/// 装到服务端会在启动时崩。没有服务端标记时退回 [`primary_file`]。
#[must_use]
pub fn server_file(version: &Version) -> Option<&VersionFile> {
    server_archive(version).or_else(|| primary_file(version))
}

/// 版本是否提供了服务端归档（只认带标记的 `.mrpack`）
#[must_use]
pub fn server_archive(version: &Version) -> Option<&VersionFile> {
    version
        .files
        .iter()
        .find(|file| is_server_archive(&file.filename))
}

/// 文件名是否标记为服务端归档
///
/// 只认 `.mrpack`：服务端 `.zip` 包没有 `modrinth.index.json`，本项目的
/// 安装流程无法消费，退回主文件并交由上层按清单处理。
fn is_server_archive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".mrpack")
        && (lower.contains("[server]")
            || lower.contains("(server)")
            || lower.contains("-server")
            || lower.contains("_server")
            || lower.contains(" server"))
}

/// 供 `SearchHit` 便捷取项目类型
impl SearchHit {
    /// 解析 `project_type`
    #[must_use]
    pub fn project_type(&self) -> ProjectType {
        ProjectType::parse(&self.project_type)
    }
}

/// 供 `Project` 便捷取项目类型
impl Project {
    /// 解析 `project_type`
    #[must_use]
    pub fn project_type(&self) -> ProjectType {
        ProjectType::parse(&self.project_type)
    }
}

/// 供 `Version` 便捷取发布渠道
impl Version {
    /// 解析 `version_type`
    #[must_use]
    pub fn channel(&self) -> VersionChannel {
        VersionChannel::parse(&self.version_type)
    }
}

// `async_trait` 在上面用于其他来源；此处保留导入以免触发未使用告警
#[allow(unused_imports, reason = "保持与 http 模块一致的 trait 引入风格")]
use async_trait as _async_trait_marker;

/// 仅测试用的内部单测：纯函数（URL 编码 / JSON 数组 / 主文件 / 校验和）
#[cfg(test)]
mod tests {
    use super::*;
    use crate::modrinth::manifest::FileHashes;

    #[test]
    fn project_type_round_trips_and_keeps_unknown() {
        for (raw, expected) in [
            ("mod", ProjectType::Mod),
            ("modpack", ProjectType::Modpack),
            ("resourcepack", ProjectType::ResourcePack),
            ("datapack", ProjectType::DataPack),
            ("shader", ProjectType::Shader),
            ("plugin", ProjectType::Plugin),
        ] {
            assert_eq!(ProjectType::parse(raw), expected);
            assert_eq!(ProjectType::parse(raw).as_str(), raw);
        }
        // 未知值保留原样，不丢弃
        let unknown = ProjectType::parse("brand-new-type");
        assert_eq!(unknown, ProjectType::Other("brand-new-type".to_owned()));
        assert_eq!(unknown.as_str(), "brand-new-type");
        assert_eq!(unknown.to_string(), "brand-new-type");
    }

    #[test]
    fn version_channel_round_trips() {
        for raw in ["release", "beta", "alpha"] {
            assert_eq!(VersionChannel::parse(raw).as_str(), raw);
        }
        assert_eq!(
            VersionChannel::parse("nightly"),
            VersionChannel::Other("nightly".to_owned())
        );
    }

    #[test]
    fn search_index_names() {
        assert_eq!(SearchIndex::default(), SearchIndex::Relevance);
        assert_eq!(SearchIndex::Relevance.as_str(), "relevance");
        assert_eq!(SearchIndex::Downloads.as_str(), "downloads");
        assert_eq!(SearchIndex::Follows.as_str(), "follows");
        assert_eq!(SearchIndex::Newest.as_str(), "newest");
        assert_eq!(SearchIndex::Updated.as_str(), "updated");
    }

    #[test]
    fn hash_algorithm_names() {
        assert_eq!(HashAlgorithm::Sha1.as_str(), "sha1");
        assert_eq!(HashAlgorithm::Sha512.as_str(), "sha512");
    }

    #[test]
    fn urlencode_escapes_reserved_chars() {
        // 空格、方括号、引号都必须编码
        let out = urlencode(r#"[["project_type=mod"]]"#);
        assert!(!out.contains('['), "{out}");
        assert!(!out.contains('"'), "{out}");
        assert!(out.contains("%20") || !out.contains(' '), "{out}");
        // 版本号里的 `+` 必须编码
        assert_eq!(urlencode("1.20+"), "1.20%2B");
    }

    #[test]
    fn urlencode_path_keeps_safe_chars() {
        assert_eq!(urlencode_path("sodium-1.0_test.v2"), "sodium-1.0_test.v2");
        assert_eq!(urlencode_path("a b"), "a%20b");
        assert_eq!(urlencode_path("a/b"), "a%2Fb");
    }

    #[test]
    fn json_array_encodes_strings() {
        let items = vec!["fabric".to_owned(), "forge".to_owned()];
        assert_eq!(json_array(&items), r#"["fabric","forge"]"#);
        assert_eq!(json_array(&[]), "[]");
    }

    #[test]
    fn endpoint_joins_without_double_slash() {
        let source = ModrinthSource::new().expect("source");
        assert_eq!(
            source.endpoint("/search"),
            "https://api.modrinth.com/v2/search"
        );
    }

    #[test]
    fn mirror_base_trims_trailing_slash() {
        let source = ModrinthSource::builder()
            .base_url("https://labrinth.example.com/v2/")
            .build()
            .expect("source");
        assert_eq!(source.base_url(), "https://labrinth.example.com/v2");
        assert_eq!(
            source.endpoint("/project/x"),
            "https://labrinth.example.com/v2/project/x"
        );
    }

    #[test]
    fn rejects_insecure_base_url() {
        let err = ModrinthSource::builder()
            .base_url("http://example.com/v2")
            .build();
        assert!(matches!(err, Err(port::SourceError::Init(_))));
    }

    fn version_with(files: Vec<VersionFile>) -> Version {
        Version {
            name: "v".to_owned(),
            version_number: "1.0.0".to_owned(),
            changelog: None,
            dependencies: Vec::new(),
            game_versions: vec!["1.21.1".to_owned()],
            version_type: "release".to_owned(),
            loaders: vec!["fabric".to_owned()],
            featured: false,
            status: "listed".to_owned(),
            id: "V".to_owned(),
            project_id: "P".to_owned(),
            author_id: "A".to_owned(),
            date_published: "2024-01-01T00:00:00Z".to_owned(),
            downloads: 0,
            environment: None,
            files,
        }
    }

    fn file(name: &str, primary: bool, sha1: Option<&str>) -> VersionFile {
        VersionFile {
            hashes: FileHashes {
                sha1: sha1.map(str::to_owned),
                sha512: None,
            },
            url: format!("https://cdn.modrinth.com/{name}"),
            filename: name.to_owned(),
            primary,
            size: 1,
            file_type: None,
        }
    }

    /// 有主文件时选主文件，无主文件时退回首文件
    #[test]
    fn primary_file_selection() {
        let with_primary =
            version_with(vec![file("a.jar", false, None), file("b.jar", true, None)]);
        assert_eq!(
            primary_file(&with_primary).map(|f| f.filename.as_str()),
            Some("b.jar")
        );

        let no_primary = version_with(vec![file("a.jar", false, None), file("b.jar", false, None)]);
        assert_eq!(
            primary_file(&no_primary).map(|f| f.filename.as_str()),
            Some("a.jar")
        );

        let empty = version_with(Vec::new());
        assert!(primary_file(&empty).is_none());
    }

    /// 服务端安装优先取文件名带 server 标记的归档
    #[test]
    fn server_file_prefers_server_archive() {
        let version = version_with(vec![
            file("client.mrpack", true, None),
            file("pack [server].mrpack", false, None),
        ]);
        assert_eq!(
            server_file(&version).map(|f| f.filename.as_str()),
            Some("pack [server].mrpack")
        );

        let dashed = version_with(vec![
            file("client.mrpack", true, None),
            file("pack-server.MRPACK", false, None),
        ]);
        assert_eq!(
            server_file(&dashed).map(|f| f.filename.as_str()),
            Some("pack-server.MRPACK")
        );

        // 没有服务端归档时退回主文件
        let client_only = version_with(vec![file("client.mrpack", true, None)]);
        assert_eq!(
            server_file(&client_only).map(|f| f.filename.as_str()),
            Some("client.mrpack")
        );

        // `.zip` 服务端包没有清单，不能选
        let zipped = version_with(vec![
            file("client.mrpack", true, None),
            file("pack [server].zip", false, None),
        ]);
        assert_eq!(
            server_file(&zipped).map(|f| f.filename.as_str()),
            Some("client.mrpack")
        );
    }

    /// sha1 合法时返回校验和；缺失或格式错时返回 None
    #[test]
    fn file_checksum_uses_valid_sha1_only() {
        let good = file("a.jar", true, Some(&"a".repeat(40)));
        assert!(file_checksum(&good).is_some());

        let missing = file("a.jar", true, None);
        assert!(file_checksum(&missing).is_none());

        let bad = file("a.jar", true, Some("xyz"));
        assert!(file_checksum(&bad).is_none());
    }

    /// `SearchQuery` / `VersionFilter` 构造器累积字段
    #[test]
    fn query_builders_accumulate() {
        let q = SearchQuery::new()
            .query("sodium")
            .index(SearchIndex::Downloads)
            .page(20, 500);
        assert_eq!(q.query.as_deref(), Some("sodium"));
        assert_eq!(q.index, SearchIndex::Downloads);
        assert_eq!(q.offset, Some(20));
        // limit 由 search() 夹到 100，这里保留原始值
        assert_eq!(q.limit, Some(500));

        let f = VersionFilter::new()
            .loaders(["fabric"])
            .game_versions(["1.21.1"])
            .without_changelog();
        assert_eq!(f.loaders, vec!["fabric".to_owned()]);
        assert_eq!(f.game_versions, vec!["1.21.1".to_owned()]);
        assert!(!f.include_changelog);
    }
}
