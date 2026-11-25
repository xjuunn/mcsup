//! `CurseForge` v1 API 客户端
//!
//! 默认对接社区镜像 `https://mod.mcimirror.top/curseforge/v1`：官方
//! `api.curseforge.com` 不提供匿名访问，必须有 API key。镜像与官方 v1 同构，
//! 因此配置 `CURSEFORGE_API_KEY` 后可以切到官方地址，代码不变。
//!
//! # 检索语义（实测，2026-11）
//!
//! 上游全文检索按**分隔符切词**，不是子串匹配，这决定了检索策略：
//!
//! - `searchFilter=twilightforest` → 0~4 条无关 fork（`The Twilight Forest`
//!   的索引词是 `the` / `twilight` / `forest`）
//! - `searchFilter=twilight forest` → 首位就是本体
//! - `slug=the-twilight-forest&classId=6` → 精确命中；`slug` 必须带 `classId`，
//!   否则同名整合包（`classId=4471`）会抢在前面
//! - 排序必须给 `sortField=2&sortOrder=desc`（热度）：同样的检索词不给排序时
//!   首位可能是毫不相干的低下载项目
//!
//! 因此检索顺序固定为：精确 `slug` → 原词 → 把连写词切成两段再试。切分只在
//! 「原词没有命中」时发生，且最多试 [`SPLIT_ATTEMPTS`] 个位置，避免一次检索
//! 打十几个请求。

use crate::curseforge::manifest::{Envelope, File, ListEnvelope, Mod};
use crate::http::{build_client, get_json_with, map_transport};
use domain::server::content::ContentKind;
use port::SourceError;
use reqwest::Client;
use serde::de::DeserializeOwned;
use url::Url;

/// `CurseForge` 官方 API 根地址（需要 API key）
pub const OFFICIAL_API: &str = "https://api.curseforge.com/v1";
/// 社区镜像（匿名可用，与官方 v1 同构）
pub const MCIM_API: &str = "https://mod.mcimirror.top/curseforge/v1";

/// `CurseForge` 里 Minecraft 的 `gameId`
pub const GAME_ID_MINECRAFT: u32 = 432;

/// 内容类别 → 上游 `classId`
///
/// 只映射本项目要装的类别。上游的 `classId` 是「项目在网页上的分类」，
/// 与安装落点不是一一对应（`datapack` / `plugin` 也可能是一份 mod），因此
/// 这里按**安装落点**取最贴近的那一类。
#[must_use]
pub fn class_id(kind: ContentKind) -> u32 {
    match kind {
        ContentKind::Plugin => 5,
        ContentKind::ResourcePack => 12,
        ContentKind::DataPack => 6945,
        ContentKind::Shader => 6552,
        ContentKind::Modpack => 4471,
        // `ContentKind` 是 `#[non_exhaustive]`：新增类别时按 mod 兜底，
        // 不让检索因为上游分类没跟上而整条失败
        _ => 6,
    }
}

/// 上游 `classId` → 内容类别
///
/// 认不出的类别按 mod 兜底：宁可多展示一个项目，也不要因为上游新增分类而
/// 把整条搜索结果丢掉。
#[must_use]
pub fn kind_of_class(class_id: u32) -> ContentKind {
    match class_id {
        5 => ContentKind::Plugin,
        12 => ContentKind::ResourcePack,
        6945 => ContentKind::DataPack,
        6552 => ContentKind::Shader,
        4471 => ContentKind::Modpack,
        _ => ContentKind::Mod,
    }
}

/// 领域加载器名 → 上游 `modLoaderType`
///
/// 取值来自官方枚举：1 = Forge，4 = Fabric，5 = Quilt，6 = `NeoForge`。
/// 认不出的加载器返回 `None`（不加此过滤），而不是猜一个。
#[must_use]
pub fn loader_id(name: &str) -> Option<u32> {
    match name.to_ascii_lowercase().as_str() {
        "forge" => Some(1),
        "fabric" => Some(4),
        "quilt" => Some(5),
        "neoforge" => Some(6),
        _ => None,
    }
}

/// 上游 `modLoaderType` → 领域加载器名
#[must_use]
pub fn loader_name(id: u32) -> Option<&'static str> {
    match id {
        1 => Some("forge"),
        4 => Some("fabric"),
        5 => Some("quilt"),
        6 => Some("neoforge"),
        _ => None,
    }
}

/// 一次检索的参数
#[derive(Debug, Clone, Default)]
pub struct ModSearch {
    /// 类别
    pub class_id: Option<u32>,
    /// 关键词（全文检索）
    pub keyword: Option<String>,
    /// 精确 slug
    pub slug: Option<String>,
    /// 游戏版本
    pub game_version: Option<String>,
    /// 加载器
    pub loader: Option<u32>,
    /// 跳过的结果数
    pub offset: u32,
    /// 返回条数
    pub page_size: u32,
}

/// 切词尝试的上限：连写 modid 可能很长，逐个位置切开打十几个请求不值得
const SPLIT_ATTEMPTS: usize = 6;
/// 切出来的两段各自至少多长
const SPLIT_MIN_LEN: usize = 3;
/// 上游分页上限
pub(super) const PAGE_SIZE_MAX: u32 = 50;

/// 把一个连写关键词的候选切法列出来
///
/// 只对 ASCII 生效：非 ASCII 无法按字节安全切分，直接返回空表。切分位置从右侧
/// 开始（`twilightforest` 的实际词界在 `twilight|forest`，靠近中间偏右），
/// 命中一个就够，调用方按顺序试。
#[must_use]
pub fn split_terms(keyword: &str) -> Vec<String> {
    if !keyword.is_ascii() || keyword.contains([' ', '-', '_']) {
        return Vec::new();
    }
    let len = keyword.len();
    let mut out = Vec::new();
    // 从右侧往左：位置 i 表示「前 i 个字符 + 空格 + 剩余」
    for i in (SPLIT_MIN_LEN..=len - SPLIT_MIN_LEN).rev() {
        if out.len() >= SPLIT_ATTEMPTS {
            break;
        }
        out.push(format!("{} {}", &keyword[..i], &keyword[i..]));
    }
    out
}

/// [`CurseForgeSource`] 的构造器
#[derive(Debug, Default)]
pub struct CurseForgeSourceBuilder {
    base_url: Option<String>,
    api_key: Option<String>,
}

impl CurseForgeSourceBuilder {
    /// 换 API 根地址（官方或自建镜像）
    ///
    /// 只接受 `https`，尾斜杠会被去掉。
    #[must_use]
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// 设置 API key
    ///
    /// 只对官方地址有意义；镜像忽略它也不报错，但这里仍然会照发（镜像不校验
    /// 未知头）。
    #[must_use]
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// 装配来源
    ///
    /// # Errors
    ///
    /// 地址非法、非 `https` 或 HTTP 客户端构建失败时返回 [`SourceError::Init`]
    pub fn build(self) -> Result<CurseForgeSource, SourceError> {
        let base_url = self
            .base_url
            .unwrap_or_else(|| MCIM_API.to_owned())
            .trim_end_matches('/')
            .to_owned();
        let parsed = base_url
            .parse::<Url>()
            .map_err(|err| SourceError::Init(Box::new(std::io::Error::other(err.to_string()))))?;
        if parsed.scheme() != "https" {
            return Err(SourceError::Init(Box::new(std::io::Error::other(format!(
                "only https URLs are accepted, got schema {:?}",
                parsed.scheme()
            )))));
        }
        Ok(CurseForgeSource {
            http: build_client()?,
            base_url,
            api_key: self.api_key.filter(|key| !key.trim().is_empty()),
        })
    }
}

/// `CurseForge` 内容来源
#[derive(Debug)]
pub struct CurseForgeSource {
    http: Client,
    base_url: String,
    api_key: Option<String>,
}

impl CurseForgeSource {
    /// 默认实例：社区镜像、匿名
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败时返回 [`SourceError::Init`]
    pub fn new() -> Result<Self, SourceError> {
        Self::builder().build()
    }

    /// 构造器
    #[must_use]
    pub fn builder() -> CurseForgeSourceBuilder {
        CurseForgeSourceBuilder::default()
    }

    /// 测试专用：接受任意 scheme 的根地址（端点契约测试要打本机 mock）
    ///
    /// `#[doc(hidden)]`：不在公开契约里，跨 crate 的集成测试需要能调用。
    ///
    /// # Errors
    ///
    /// 地址无法解析或 HTTP 客户端构建失败时返回 [`SourceError::Init`]
    #[doc(hidden)]
    pub fn for_test(base_url: &str) -> Result<Self, SourceError> {
        let base_url = base_url.trim_end_matches('/').to_owned();
        base_url
            .parse::<Url>()
            .map_err(|err| SourceError::Init(Box::new(std::io::Error::other(err.to_string()))))?;
        Ok(Self {
            http: build_client()?,
            base_url,
            api_key: None,
        })
    }

    /// API 根地址
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn get<T>(&self, url: &str) -> Result<T, SourceError>
    where
        T: DeserializeOwned,
    {
        get_json_with(&self.http, url, None, self.api_key.as_deref()).await
    }

    /// 搜索项目
    ///
    /// # Errors
    ///
    /// 上游不可达、状态码异常或响应无法解析时返回 [`SourceError`]
    pub async fn search(&self, search: &ModSearch) -> Result<ListEnvelope<Mod>, SourceError> {
        let mut params: Vec<String> = vec![format!("gameId={GAME_ID_MINECRAFT}")];
        if let Some(class_id) = search.class_id {
            params.push(format!("classId={class_id}"));
        }
        if let Some(slug) = search.slug.as_deref()
            && !slug.is_empty()
        {
            params.push(format!("slug={}", encode(slug)));
        }
        if let Some(keyword) = search.keyword.as_deref()
            && !keyword.trim().is_empty()
        {
            params.push(format!("searchFilter={}", encode(keyword.trim())));
        }
        if let Some(version) = search.game_version.as_deref()
            && !version.is_empty()
        {
            params.push(format!("gameVersion={}", encode(version)));
        }
        if let Some(loader) = search.loader {
            params.push(format!("modLoaderType={loader}"));
        }
        // 热度降序：不给排序时上游按自己的索引顺序返回，首位常常是无关项目
        params.push("sortField=2".to_owned());
        params.push("sortOrder=desc".to_owned());
        params.push(format!("index={}", search.offset));
        params.push(format!(
            "pageSize={}",
            search.page_size.clamp(1, PAGE_SIZE_MAX)
        ));
        let url = self.endpoint(&format!("/mods/search?{}", params.join("&")));
        self.get(&url).await
    }

    /// 读取项目详情
    ///
    /// # Errors
    ///
    /// 项目不存在、上游不可达或响应无法解析时返回 [`SourceError`]
    pub async fn project(&self, id: u64) -> Result<Mod, SourceError> {
        let url = self.endpoint(&format!("/mods/{id}"));
        let envelope: Envelope<Mod> = self.get(&url).await?;
        Ok(envelope.data)
    }

    /// 列出项目的文件
    ///
    /// # Errors
    ///
    /// 项目不存在、上游不可达或响应无法解析时返回 [`SourceError`]
    pub async fn files(
        &self,
        project: u64,
        game_version: Option<&str>,
        loader: Option<u32>,
        page_size: u32,
    ) -> Result<Vec<File>, SourceError> {
        let mut params: Vec<String> = Vec::new();
        if let Some(version) = game_version
            && !version.is_empty()
        {
            params.push(format!("gameVersion={}", encode(version)));
        }
        if let Some(loader) = loader {
            params.push(format!("modLoaderType={loader}"));
        }
        params.push(format!("pageSize={}", page_size.clamp(1, PAGE_SIZE_MAX)));
        let url = self.endpoint(&format!("/mods/{project}/files?{}", params.join("&")));
        let envelope: ListEnvelope<File> = self.get(&url).await?;
        Ok(envelope.data)
    }

    /// 读取单个文件
    ///
    /// # Errors
    ///
    /// 文件不存在、上游不可达或响应无法解析时返回 [`SourceError`]
    pub async fn file(&self, project: u64, file_id: u64) -> Result<File, SourceError> {
        let url = self.endpoint(&format!("/mods/{project}/files/{file_id}"));
        let envelope: Envelope<File> = self.get(&url).await?;
        Ok(envelope.data)
    }

    /// 探测一个文件能否下载（仅状态码）
    ///
    /// 用于诊断「CDN 地址是否可用」，不读 body。
    ///
    /// # Errors
    ///
    /// 传输失败或非成功状态码时返回 [`SourceError`]
    pub async fn probe_file(&self, url: &str) -> Result<(), SourceError> {
        let parsed = url
            .parse::<Url>()
            .map_err(|err| SourceError::Decode(Box::new(err)))?;
        let response = self.http.head(parsed).send().await.map_err(map_transport)?;
        let status = response.status();
        if status.is_success() || status == reqwest::StatusCode::PARTIAL_CONTENT {
            Ok(())
        } else {
            Err(SourceError::Status {
                status: status.as_u16(),
            })
        }
    }
}

/// 把字符串编码为 query 参数值
fn encode(raw: &str) -> String {
    url::form_urlencoded::byte_serialize(raw.as_bytes()).collect()
}

/// 把文件名编码为单个路径段（保留 `-` `.` `_` `~`）
#[must_use]
fn encode_path_segment(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// 文件 ID → CDN 路径
///
/// 官方 CDN 的路径是 `files/<fileId / 1000>/<fileId % 1000>/<文件名>`。上游
/// 在作者未授权分发时把 `downloadUrl` 置为 `null`，这条换算就是唯一来源；
/// 实测 `mediafilez.forgecdn.net` 与 `edge.forgecdn.net` 都可用。
#[must_use]
pub fn cdn_url(file_id: u64, file_name: &str) -> String {
    format!(
        "https://mediafilez.forgecdn.net/files/{}/{}/{}",
        file_id / 1000,
        file_id % 1000,
        encode_path_segment(file_name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_ids_round_trip() {
        for kind in ContentKind::ALL {
            assert_eq!(kind_of_class(class_id(kind)), kind, "{kind}");
        }
        // 未知分类按 mod 兜底，不丢项目
        assert_eq!(kind_of_class(999_999), ContentKind::Mod);
    }

    #[test]
    fn loader_ids_match_official_enum() {
        assert_eq!(loader_id("forge"), Some(1));
        assert_eq!(loader_id("NeoForge"), Some(6));
        assert_eq!(loader_id("fabric"), Some(4));
        assert_eq!(loader_id("quilt"), Some(5));
        assert_eq!(loader_id("paper"), None);
        assert_eq!(loader_name(1), Some("forge"));
        assert_eq!(loader_name(99), None);
    }

    #[test]
    fn split_terms_reaches_the_real_word_boundary() {
        let terms = split_terms("twilightforest");
        assert!(terms.contains(&"twilight forest".to_owned()), "{terms:?}");
        // 只有空格切分，不再细分
        assert!(split_terms("twilight forest").is_empty());
        // 非 ASCII 不切
        assert!(split_terms("暮色森林").is_empty());
        // 太短不切
        assert!(split_terms("jei").is_empty());
    }

    #[test]
    fn split_attempts_are_bounded() {
        let terms = split_terms("averylongconcatenatedmodid");
        assert!(terms.len() <= SPLIT_ATTEMPTS, "{terms:?}");
    }

    #[test]
    fn cdn_url_follows_the_file_id_split() {
        assert_eq!(
            cdn_url(5_468_648, "twilightforest-1.20.1-4.3.2508-universal.jar"),
            "https://mediafilez.forgecdn.net/files/5468/648/twilightforest-1.20.1-4.3.2508-universal.jar"
        );
        // `+` 必须编码，否则 CDN 把加号当空格
        assert_eq!(
            cdn_url(7_594_389, "Connector-1.0.0-beta.48+1.20.1.jar"),
            "https://mediafilez.forgecdn.net/files/7594/389/Connector-1.0.0-beta.48%2B1.20.1.jar"
        );
    }

    #[test]
    fn builder_rejects_plain_http() {
        assert!(
            CurseForgeSource::builder()
                .base_url("http://example.invalid/v1")
                .build()
                .is_err()
        );
    }
}
