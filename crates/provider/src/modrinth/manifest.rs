//! Modrinth Labrinth v2 的响应模型
//!
//! 只声明本项目用到的字段；上游新增字段不影响解析（serde 忽略未知字段是本
//! 模块的既定约定，与 `mojang` / `fabric` 等来源一致）。
//!
//! 端点清单见 <https://docs.modrinth.com/api/>。本模块覆盖：
//!
//! - `GET /search`：搜索（[`SearchResults`] / [`SearchHit`]）
//! - `GET /project/{id|slug}`：项目详情（[`Project`]）
//! - `POST /projects`：批量项目详情（同一 [`Project`] 列表）
//! - `GET /project/{id|slug}/version`：版本列表（[`Version`]）
//! - `GET /version/{id}`：单版本（[`Version`]）
//! - `GET /versions?ids=`：批量版本（[`Version`] 列表）
//! - `GET /version_file/{hash}`：按文件哈希反查版本（[`Version`]）
//! - `GET /project/{id|slug}/dependencies`：依赖图（[`DependencyGraph`]）
//! - `GET /tag/{category|loader|game_version}`：标签（`project_type` 是字符串数组）
//! - `GET /team/{id}/members`：团队成员
//!
//! # 为什么用 `String` 而不是领域枚举存放 `project_type` / `version_type`
//!
//! 这里刻意保留上游原值：Modrinth 会不断新增类型与渠道（`datapack` /
//! `plugin` 都是后加的）。用枚举硬固化会让任何上游新增值直接变成解析失败，
//! 把「未知但不影响使用」降级成「整个响应报废」。上层的分类判定放到
//! [`ProjectType`](super::ProjectType) / [`VersionChannel`](super::VersionChannel)
//! 这些带 `Other` 兜底的辅助类型里，既保留可读语义，又不拒绝未知值。

use serde::Deserialize;

/// 搜索结果页
#[derive(Debug, Clone, Deserialize)]
pub struct SearchResults {
    /// 命中的项目摘要
    pub hits: Vec<SearchHit>,
    /// 本次查询跳过的结果数
    pub offset: i64,
    /// 本次返回的结果数
    pub limit: i64,
    /// 匹配查询的结果总数
    pub total_hits: i64,
}

/// 一条搜索结果（比完整 [`Project`] 更轻量）
#[derive(Debug, Clone, Deserialize)]
pub struct SearchHit {
    /// 项目 ID（base62）
    pub project_id: String,
    /// 项目类型（`mod` / `modpack` / `resourcepack` / `shader` …）
    pub project_type: String,
    /// 该项目所有版本覆盖到的全部类型（可能含 `datapack` / `plugin`）
    #[serde(default)]
    pub all_project_types: Vec<String>,
    /// 项目标题
    pub title: String,
    /// 一句话简介
    pub description: String,
    /// 作者用户名
    pub author: String,
    /// 精选分类（搜索把 loader 也算作分类）
    #[serde(default)]
    pub categories: Vec<String>,
    /// 展示用分类
    #[serde(default)]
    pub display_categories: Vec<String>,
    /// 支持的 Minecraft 版本
    #[serde(default)]
    pub versions: Vec<String>,
    /// 总下载量
    pub downloads: i64,
    /// 关注人数
    pub follows: i64,
    /// 图标地址
    pub icon_url: Option<String>,
    /// 创建时间（ISO-8601）
    pub date_created: String,
    /// 最近更新时间（ISO-8601）
    pub date_modified: String,
    /// 最新版本 ID
    #[serde(default)]
    pub latest_version: Option<String>,
    /// SPDX 许可证 id
    pub license: String,
    /// 支持的环境（`client_and_server` 等）
    #[serde(default)]
    pub environment: Vec<String>,
    /// 客户端侧支持情况（`required` / `optional` / `unsupported`）
    #[serde(default)]
    pub client_side: Option<String>,
    /// 服务端侧支持情况（`required` / `optional` / `unsupported`）
    #[serde(default)]
    pub server_side: Option<String>,
    /// 展示的披露类型
    #[serde(default)]
    pub disclosure_types: Vec<String>,
    /// 画廊图片地址
    #[serde(default)]
    pub gallery: Vec<String>,
    /// 项目 slug（可作为 id 使用；可能缺失）
    pub slug: Option<String>,
    /// 作者 ID（可能缺失）
    pub author_id: Option<String>,
    /// 组织名（可能缺失）
    pub organization: Option<String>,
    /// 组织 ID（可能缺失）
    pub organization_id: Option<String>,
    /// 精选画廊图（可能缺失）
    pub featured_gallery: Option<String>,
    /// 主题色 RGB（可能缺失）
    pub color: Option<i64>,
}

/// 完整项目详情
#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    /// 项目 ID（base62）
    pub id: String,
    /// 所属团队 ID
    pub team: String,
    /// 标题
    pub title: String,
    /// 一句话简介
    pub description: String,
    /// 长描述（Markdown）
    pub body: String,
    /// 项目状态（`approved` / `archived` / `draft` …）
    pub status: String,
    /// 项目类型
    pub project_type: String,
    /// 精选分类
    #[serde(default)]
    pub categories: Vec<String>,
    /// 补充分类
    #[serde(default)]
    pub additional_categories: Vec<String>,
    /// 支持的环境
    #[serde(default)]
    pub environment: Vec<String>,
    /// 服务端侧支持情况（`required` / `optional` / `unsupported`）
    #[serde(default)]
    pub server_side: Option<String>,
    /// 客户端侧支持情况（`required` / `optional` / `unsupported`）
    #[serde(default)]
    pub client_side: Option<String>,
    /// 支持的全部 MC 版本
    #[serde(default)]
    pub game_versions: Vec<String>,
    /// 支持的加载器
    #[serde(default)]
    pub loaders: Vec<String>,
    /// 该项目的版本 ID 列表
    #[serde(default)]
    pub versions: Vec<String>,
    /// 许可证信息
    pub license: License,
    /// 创建时间
    pub published: String,
    /// 最近更新时间
    pub updated: String,
    /// 总下载量
    pub downloads: i64,
    /// 关注人数
    pub followers: i64,
    /// 画廊图片
    #[serde(default)]
    pub gallery: Vec<GalleryImage>,
    /// slug（可作为 id 使用）
    pub slug: Option<String>,
    /// 图标地址
    pub icon_url: Option<String>,
    /// 原图地址（未做 CDN 变换）
    pub raw_icon_url: Option<String>,
    /// 主题色
    pub color: Option<i64>,
    /// Issues 链接
    pub issues_url: Option<String>,
    /// 源码链接
    pub source_url: Option<String>,
    /// Wiki 链接
    pub wiki_url: Option<String>,
    /// Discord 邀请链接
    pub discord_url: Option<String>,
    /// 捐赠链接
    pub donation_urls: Option<Vec<DonationUrl>>,
}

/// 许可证信息
#[derive(Debug, Clone, Deserialize)]
pub struct License {
    /// SPDX 许可证 id
    pub id: String,
    /// 许可证全名
    #[serde(default)]
    pub name: Option<String>,
    /// 许可证链接
    #[serde(default)]
    pub url: Option<String>,
}

/// 画廊图片
#[derive(Debug, Clone, Deserialize)]
pub struct GalleryImage {
    /// 图片地址
    pub url: String,
    /// 是否精选
    #[serde(default)]
    pub featured: bool,
    /// 标题
    pub title: Option<String>,
    /// 描述
    pub description: Option<String>,
    /// 创建时间
    #[serde(default)]
    pub created: Option<String>,
}

/// 捐赠链接
#[derive(Debug, Clone, Deserialize)]
pub struct DonationUrl {
    /// 平台 id
    pub id: String,
    /// 平台展示名
    #[serde(default)]
    pub platform: Option<String>,
    /// 链接地址
    pub url: String,
}

/// 一个项目的某个版本
#[derive(Debug, Clone, Deserialize)]
pub struct Version {
    /// 版本名称（可读）
    #[serde(default)]
    pub name: String,
    /// 版本号（一般遵循 semver）
    pub version_number: String,
    /// 更新日志（`include_changelog=false` 时可能缺失）
    pub changelog: Option<String>,
    /// 该版本声明的依赖
    #[serde(default)]
    pub dependencies: Vec<VersionDependency>,
    /// 支持的 MC 版本
    #[serde(default)]
    pub game_versions: Vec<String>,
    /// 发布渠道（`release` / `beta` / `alpha`）
    #[serde(default)]
    pub version_type: String,
    /// 支持的加载器；资源包等用 `minecraft`
    #[serde(default)]
    pub loaders: Vec<String>,
    /// 是否精选
    #[serde(default)]
    pub featured: bool,
    /// 版本状态（`listed` / `archived` …）
    #[serde(default)]
    pub status: String,
    /// 版本 ID（base62）
    pub id: String,
    /// 所属项目 ID
    pub project_id: String,
    /// 发布者 ID
    pub author_id: String,
    /// 发布时间
    pub date_published: String,
    /// 版本下载量
    pub downloads: i64,
    /// 支持的环境
    #[serde(default)]
    pub environment: Option<String>,
    /// 可下载文件列表
    pub files: Vec<VersionFile>,
}

/// 版本依赖声明
///
/// 与 [`Version`] 内的 `dependencies` 数组对应。`project_id` 与 `version_id`
/// 均可为空：`file_name` 用于描述整合包里的外部依赖（不在 Modrinth 上）。
#[derive(Debug, Clone, Deserialize)]
pub struct VersionDependency {
    /// 依赖项目的版本 ID（可能为空）
    pub version_id: Option<String>,
    /// 依赖项目的项目 ID（可能为空）
    pub project_id: Option<String>,
    /// 依赖文件名（主要用于整合包的外部依赖）
    pub file_name: Option<String>,
    /// 依赖类型（`required` / `optional` / `incompatible` / `embedded`）
    pub dependency_type: String,
}

/// 版本内的一个可下载文件
#[derive(Debug, Clone, Deserialize)]
pub struct VersionFile {
    /// 文件哈希表（键为算法，值为十六进制串）
    pub hashes: FileHashes,
    /// 直链下载地址
    pub url: String,
    /// 文件名
    pub filename: String,
    /// 是否为该版本的主文件（每个版本最多一个）
    #[serde(default)]
    pub primary: bool,
    /// 文件大小（字节）
    pub size: u64,
    /// 文件类型（`required-resource-pack` 等；可能缺失）
    pub file_type: Option<String>,
}

/// 文件哈希表
///
/// Modrinth 每个文件都同时给出 `sha1` 与 `sha512`。本项目下载器只认
/// `sha1` / `sha256`，因此这里保留两个上游字段，由下载侧按需取用；
/// `sha512` 用于按哈希反查版本（[`crate::modrinth::MODRINTH` 的哈希查询]）。
#[derive(Debug, Clone, Deserialize)]
pub struct FileHashes {
    /// sha1（十六进制）
    #[serde(default)]
    pub sha1: Option<String>,
    /// sha512（十六进制）
    #[serde(default)]
    pub sha512: Option<String>,
}

/// 项目依赖图
#[derive(Debug, Clone, Deserialize)]
pub struct DependencyGraph {
    /// 被依赖的项目
    #[serde(default)]
    pub projects: Vec<Project>,
    /// 被依赖的具体版本
    #[serde(default)]
    pub versions: Vec<Version>,
}

/// 标签：加载器
#[derive(Debug, Clone, Deserialize)]
pub struct LoaderTag {
    /// 加载器名称（`fabric` / `forge` …）
    pub name: String,
    /// SVG 图标
    #[serde(default)]
    pub icon: String,
    /// 该加载器适用的项目类型
    #[serde(default)]
    pub supported_project_types: Vec<String>,
}

/// 标签：游戏版本
#[derive(Debug, Clone, Deserialize)]
pub struct GameVersionTag {
    /// MC 版本号
    pub version: String,
    /// 版本类型（`release` / `snapshot` …）
    #[serde(default)]
    pub version_type: String,
    /// 发布日期
    #[serde(default)]
    pub date: String,
    /// 是否为主要版本
    #[serde(default)]
    pub major: bool,
}

/// 标签：分类
#[derive(Debug, Clone, Deserialize)]
pub struct CategoryTag {
    /// 分类名（`adventure` / `technology` …）
    pub name: String,
    /// 适用的项目类型
    #[serde(default)]
    pub project_type: String,
    /// SVG 图标
    #[serde(default)]
    pub icon: String,
}

/// 团队成员（`GET /team/{id}/members` 的数组元素）
///
/// 上游**没有** `GET /team/{id}` 单体路由，只能查成员列表。成员里的
/// `user` 是完整用户对象，这里只取展示需要的字段。
#[derive(Debug, Clone, Deserialize)]
pub struct TeamMember {
    /// 所属团队 ID
    #[serde(default)]
    pub team_id: Option<String>,
    /// 用户
    pub user: TeamUser,
    /// 团队角色（`Owner` / `Member` 等；上游首字母大写）
    pub role: String,
    /// 用户是否已接受邀请
    #[serde(default)]
    pub accepted: bool,
}

/// 团队成员对应的用户
#[derive(Debug, Clone, Deserialize)]
pub struct TeamUser {
    /// 用户 ID
    pub id: String,
    /// 用户名（slug 形态，可作为用户 id 使用）
    #[serde(default)]
    pub username: String,
    /// 展示名（可能缺失）
    #[serde(default)]
    pub name: Option<String>,
    /// 头像地址
    #[serde(default)]
    pub avatar_url: Option<String>,
}

// 上游的 `GET /tag/project_type` 返回的是**纯字符串数组**
// （`["mod","modpack",…]`），不是对象数组，因此没有 `ProjectTypeTag` 模型：
// 客户端直接用 `Vec<String>` 接收。

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH: &str = include_str!("../../tests/fixtures/modrinth_search.json");
    const PROJECT: &str = include_str!("../../tests/fixtures/modrinth_project.json");
    const VERSIONS: &str = include_str!("../../tests/fixtures/modrinth_versions.json");
    const DEPENDENCIES: &str = include_str!("../../tests/fixtures/modrinth_dependencies.json");
    const LOADERS: &str = include_str!("../../tests/fixtures/modrinth_loaders.json");
    const GAME_VERSIONS: &str = include_str!("../../tests/fixtures/modrinth_game_versions.json");
    const CATEGORIES: &str = include_str!("../../tests/fixtures/modrinth_categories.json");
    const PROJECT_TYPES: &str = include_str!("../../tests/fixtures/modrinth_project_types.json");
    const TEAM_MEMBERS: &str = include_str!("../../tests/fixtures/modrinth_team_members.json");

    /// 搜索响应完整解析：命中数与摘要字段
    #[test]
    fn parses_search_results() {
        let results: SearchResults = serde_json::from_str(SEARCH).expect("search 应解析");
        assert_eq!(results.total_hits, 2);
        assert_eq!(results.hits.len(), 2);
        let first = &results.hits[0];
        assert_eq!(first.project_id, "AABBCCDD");
        assert_eq!(first.project_type, "mod");
        assert!(first.all_project_types.contains(&"datapack".to_owned()));
        assert_eq!(first.slug.as_deref(), Some("sodium"));
        // 缺省字段必须能容忍：缺失的可选字段解析为 None / 空
        assert!(first.featured_gallery.is_none());
    }

    /// 项目详情完整解析：许可证、画廊、可选链接
    #[test]
    fn parses_project() {
        let project: Project = serde_json::from_str(PROJECT).expect("project 应解析");
        assert_eq!(project.id, "AABBCCDD");
        assert_eq!(project.project_type, "modpack");
        assert_eq!(project.license.id, "LGPL-3.0-or-later");
        assert_eq!(project.gallery.len(), 1);
        assert!(project.gallery[0].featured);
        assert_eq!(project.versions.len(), 2);
        assert!(project.donation_urls.is_none());
    }

    /// 版本列表完整解析：文件哈希、依赖、文件列表
    #[test]
    fn parses_versions() {
        let versions: Vec<Version> = serde_json::from_str(VERSIONS).expect("versions 应解析");
        assert_eq!(versions.len(), 2);
        let v = &versions[0];
        assert_eq!(v.id, "IIJJKKLL");
        assert_eq!(v.version_type, "release");
        assert_eq!(v.files.len(), 1);
        let file = &v.files[0];
        assert!(file.primary);
        assert_eq!(file.size, 1_097_270);
        assert_eq!(file.hashes.sha1.as_deref().map(str::len), Some(40));
        assert_eq!(file.hashes.sha512.as_deref().map(str::len), Some(128));
        assert_eq!(v.dependencies.len(), 1);
        assert_eq!(v.dependencies[0].dependency_type, "required");
    }

    /// `changelog` 可为 null（`include_changelog=false`）
    #[test]
    fn changelog_is_optional() {
        let raw = r#"[{
            "version_number": "1.0.0",
            "id": "X", "project_id": "P", "author_id": "A",
            "date_published": "2024-01-01T00:00:00Z", "downloads": 0,
            "files": []
        }]"#;
        let versions: Vec<Version> = serde_json::from_str(raw).expect("应解析");
        assert!(versions[0].changelog.is_none());
        assert!(versions[0].name.is_empty());
        assert!(versions[0].dependencies.is_empty());
    }

    /// 依赖图完整解析：projects 与 versions 两段
    #[test]
    fn parses_dependency_graph() {
        let graph: DependencyGraph = serde_json::from_str(DEPENDENCIES).expect("deps 应解析");
        assert_eq!(graph.projects.len(), 1);
        assert_eq!(graph.versions.len(), 1);
        assert_eq!(graph.projects[0].id, "QQRRSSTT");
        assert_eq!(graph.versions[0].id, "IIJJKKLL");
    }

    /// 加载器标签：名称与适用类型
    #[test]
    fn parses_loader_tags() {
        let loaders: Vec<LoaderTag> = serde_json::from_str(LOADERS).expect("loaders 应解析");
        assert_eq!(loaders.len(), 2);
        assert_eq!(loaders[0].name, "fabric");
        assert!(
            loaders[0]
                .supported_project_types
                .contains(&"mod".to_owned())
        );
    }

    /// 游戏版本标签：类型与 major 标记
    #[test]
    fn parses_game_version_tags() {
        let games: Vec<GameVersionTag> =
            serde_json::from_str(GAME_VERSIONS).expect("game versions 应解析");
        assert_eq!(games[0].version, "1.21.1");
        assert_eq!(games[0].version_type, "release");
        assert!(games[0].major);
    }

    /// 分类标签
    #[test]
    fn parses_category_tags() {
        let cats: Vec<CategoryTag> = serde_json::from_str(CATEGORIES).expect("categories 应解析");
        assert_eq!(cats[0].name, "adventure");
    }

    /// 项目类型是纯字符串数组
    #[test]
    fn parses_project_types_as_strings() {
        let types: Vec<String> = serde_json::from_str(PROJECT_TYPES).expect("project types 应解析");
        assert!(types.iter().any(|t| t == "modpack"));
        assert!(types.iter().any(|t| t == "datapack"));
    }

    /// 团队成员：嵌套 user、角色、接受状态
    #[test]
    fn parses_team_members() {
        let members: Vec<TeamMember> =
            serde_json::from_str(TEAM_MEMBERS).expect("team members 应解析");
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].team_id.as_deref(), Some("MMNNOOPP"));
        assert_eq!(members[0].role, "Owner");
        assert!(members[0].accepted);
        assert_eq!(members[0].user.username, "robotkoer");
        assert_eq!(members[0].user.name.as_deref(), Some("Robotkoer"));
        // 缺 name / avatar_url 的成员也能解析
        assert!(members[1].user.name.is_none());
        assert!(!members[1].accepted);
    }

    /// 未知字段被忽略，不导致解析失败
    #[test]
    fn unknown_fields_are_ignored() {
        let raw = r#"{
            "id": "X", "team": "T", "title": "t", "description": "d", "body": "b",
            "status": "approved", "project_type": "mod", "license": {"id": "MIT"},
            "published": "2024-01-01T00:00:00Z", "updated": "2024-01-01T00:00:00Z",
            "downloads": 0, "followers": 0, "brand_new_field": {"nested": true}
        }"#;
        let project: Project = serde_json::from_str(raw).expect("未知字段应被忽略");
        assert_eq!(project.id, "X");
    }
}
