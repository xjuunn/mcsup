//! 服务端内容资源：Mod / 资源包 / 数据包 / 光影 / 插件 / 整合包
//!
//! 本模块只承载**领域口径**：资源分几类、每类落到服务端目录的哪里、候选版本
//! 如何在给定加载器与游戏版本下挑选。搜索与下载属于 Provider / Service，
//! 这里不做任何网络或 IO。
//!
//! # 为什么类型与目录口径放在领域层
//!
//! CLI、API、MCP 三个入口都要回答「这个项目是什么资源」「装到哪」「选哪个
//! 版本」。判定一旦写在入口就会分叉——这是本项目最常见的架构腐化方式。因此
//! 分类与挑选规则只在这里实现一次，入口只负责展示与参数适配。

use std::fmt;
use std::path::PathBuf;

use thiserror::Error;

/// 内容资源类别
///
/// 覆盖本项目要下载的全部资源。Modrinth 的 `project_type` 只有 `mod` /
/// `modpack` / `resourcepack` / `shader` 四类（`datapack` 与 `plugin` 是同一
/// 个 mod 项目的额外用途，见 [`ProviderHint`]），因此这里的分类是**面向安装
/// 落点**的口径，而不是照抄上游字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ContentKind {
    /// 模组（`mods/`）
    Mod,
    /// 插件（服务端，`plugins/`）
    Plugin,
    /// 资源包（`resourcepacks/`）
    ResourcePack,
    /// 数据包（存档外的 `datapacks/`）
    DataPack,
    /// 光影（客户端，`shaderpacks/`）
    Shader,
    /// 整合包（安装后展开为整个服务端）
    Modpack,
}

impl ContentKind {
    /// 全部类别（按安装落点排序）
    pub const ALL: [Self; 6] = [
        Self::Mod,
        Self::Plugin,
        Self::ResourcePack,
        Self::DataPack,
        Self::Shader,
        Self::Modpack,
    ];

    /// 稳定机器名（用于 CLI 参数、facet 取值与展示）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mod => "mod",
            Self::Plugin => "plugin",
            Self::ResourcePack => "resourcepack",
            Self::DataPack => "datapack",
            Self::Shader => "shader",
            Self::Modpack => "modpack",
        }
    }

    /// 由机器名解析
    ///
    /// 只接受 [`Self::ALL`] 中的名字；大小写不敏感，便于 CLI 输入。
    ///
    /// # Errors
    ///
    /// 名字不在已知集合时返回 [`UnknownContentKind`]
    pub fn parse(raw: &str) -> Result<Self, UnknownContentKind> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str().eq_ignore_ascii_case(raw))
            .ok_or_else(|| UnknownContentKind(raw.to_owned()))
    }

    /// 安装落点：相对**服务端目录**的路径
    ///
    /// 只回答「相对哪里」，不判断目录是否存在，也不创建目录——IO 属于
    /// Service / File。
    #[must_use]
    pub fn install_dir(self) -> PathBuf {
        PathBuf::from(match self {
            Self::Mod | Self::Modpack => "mods",
            Self::Plugin => "plugins",
            Self::ResourcePack => "resourcepacks",
            Self::DataPack => "datapacks",
            Self::Shader => "shaderpacks",
        })
    }

    /// 是否为服务端运行时可加载的内容
    ///
    /// 光影是纯客户端资源，装在服务端目录没有效果；安装入口据此拒绝，而不是
    /// 默默放进去一个永远不会被读取的文件。
    #[must_use]
    pub const fn is_server_usable(self) -> bool {
        !matches!(self, Self::Shader)
    }

    /// 该类内容所需的加载器口径
    ///
    /// 数据包不依赖模组加载器（原版即可），因此返回 [`LoaderScope::Any`] 表示
    /// 「不按加载器过滤」；其余类别由服务端变体给出对应的加载器名。
    #[must_use]
    pub fn loader_scope(self) -> LoaderScope {
        match self {
            Self::DataPack => LoaderScope::Any,
            Self::Mod | Self::Modpack | Self::Plugin | Self::ResourcePack | Self::Shader => {
                LoaderScope::Variant
            }
        }
    }
}

impl fmt::Display for ContentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 按加载器过滤的口径
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderScope {
    /// 不按加载器过滤（数据包、以及原版服务端的资源包 / 光影）
    Any,
    /// 用服务端变体对应的加载器名字过滤
    Variant,
}

/// 无法解析的内容类别
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown content kind: {0}")]
pub struct UnknownContentKind(pub String);

impl ProviderHint {
    /// 由上游 `project_type` 与 `all_project_types` 推断安装类别
    ///
    /// 上游没有独立的 `datapack` / `plugin` 项目类型：同一份 jar 可以既是
    /// mod 又可当插件（`all_project_types` 里会同时出现）。因此推断按「安装
    /// 时想把它放到哪」决定，默认取 `project_type` 本身；调用方明确要求时用
    /// [`ContentKind`] 覆盖。
    #[must_use]
    pub fn guess_kind(project_type: &str) -> Option<ContentKind> {
        ContentKind::parse(project_type).ok()
    }
}

/// 占位：`ProviderHint` 只在文档与未来扩展中使用
///
/// 目前分类直接走 [`ContentKind::parse`]；保留这个类型让「上游字段 → 类别」
/// 的映射有一个明确的落点，后续接入 `CurseForge` 等来源时在此扩展。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderHint;

impl fmt::Display for LoaderScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Any => "any",
            Self::Variant => "variant",
        })
    }
}

/// 下载候选：一次「按条件挑版本」的描述
///
/// 由 Service 从服务端元数据构造，交给 Provider 解析成具体的版本与文件。
/// 这里放的是**挑选条件**，不是上游版本对象——领域层不依赖 Provider 的模型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentRequest {
    /// 要装的内容类别
    pub kind: ContentKind,
    /// 服务端加载器（`fabric` / `forge` / `paper` …）
    ///
    /// `None` 表示无加载器概念（如数据包），或调用方未提供。
    pub loader: Option<String>,
    /// 服务端 Minecraft 版本（如 `1.21.1`）
    pub game_version: Option<String>,
    /// 只要正式版
    pub release_only: bool,
}

impl ContentRequest {
    /// 构造一个仅带类别的请求
    #[must_use]
    pub fn new(kind: ContentKind) -> Self {
        Self {
            kind,
            loader: None,
            game_version: None,
            release_only: true,
        }
    }

    /// 指定加载器
    #[must_use]
    pub fn loader(mut self, loader: impl Into<String>) -> Self {
        self.loader = Some(loader.into());
        self
    }

    /// 指定游戏版本
    #[must_use]
    pub fn game_version(mut self, version: impl Into<String>) -> Self {
        self.game_version = Some(version.into());
        self
    }

    /// 允许测试版 / 内测版
    #[must_use]
    pub fn allow_prerelease(mut self) -> Self {
        self.release_only = false;
        self
    }

    /// 实际参与过滤的加载器名
    ///
    /// 数据包不看加载器；其余类别在 `loader` 为空时也不过滤（原版服务端装
    /// 资源包 / 光影时没有加载器）。
    #[must_use]
    pub fn loader_filter(&self) -> Option<&str> {
        match self.kind.loader_scope() {
            LoaderScope::Any => None,
            LoaderScope::Variant => self.loader.as_deref().filter(|l| !l.is_empty()),
        }
    }
}

/// 一个可下载候选项（已按适配度打分，供 Service 选择）
///
/// 不含字节，只含「选谁、为什么」：Provider 把上游版本映射成它，领域规则对它
/// 排序，Service 据此下载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentCandidate {
    /// 上游版本 ID（下载与幂等判定用）
    pub version_id: String,
    /// 版本号（展示用）
    pub version_number: String,
    /// 发布渠道（`release` / `beta` / `alpha`）
    pub channel: String,
    /// 是否精选
    pub featured: bool,
    /// 该版本支持的加载器
    pub loaders: Vec<String>,
    /// 该版本支持的游戏版本
    pub game_versions: Vec<String>,
    /// 发布时间（ISO-8601）
    pub date_published: String,
    /// 与请求的适配度（由 [`rank_candidates`] 填充）
    pub score: i32,
}

/// 候选适配度打分
///
/// 分值越高越合适。判定口径（与 Modrinth 网页的默认行为一致）：
///
/// 1. 加载器匹配 `+100`，不匹配但候选声明了加载器 `-50`（硬性不合，仅作兜底）
/// 2. 游戏版本精确匹配 `+50`
/// 3. 正式版 `+20`，测试版 `+5`，内测版 `0`
/// 4. 精选 `+2`
///
/// 只打分不筛选：把「为什么选它」留给调用方展示，也让 `release_only` 这类
/// 硬条件集中在 [`rank_candidates`] 里一次处理。
#[must_use]
pub fn score_candidate(candidate: &ContentCandidate, request: &ContentRequest) -> i32 {
    let mut score = 0;
    if let Some(loader) = request.loader_filter() {
        if candidate.loaders.iter().any(|l| l == loader) {
            score += 100;
        } else if !candidate.loaders.is_empty() {
            score -= 50;
        }
    }
    if let Some(version) = request.game_version.as_deref()
        && candidate.game_versions.iter().any(|v| v == version)
    {
        score += 50;
    }
    score += match candidate.channel.as_str() {
        "release" => 20,
        "beta" => 5,
        _ => 0,
    };
    if candidate.featured {
        score += 2;
    }
    score
}

/// 按请求给候选打分并排序
///
/// `release_only` 时候选会被过滤成只剩正式版——这是硬条件，不是加分项，
/// 因此不放在 [`score_candidate`] 里。
///
/// 排序：分数降序；同分时发布时间晚者在前（ISO-8601 字符串可直接比较）；
/// 再同则保持输入顺序（稳定排序），便于测试复现。
#[must_use]
pub fn rank_candidates(
    candidates: impl IntoIterator<Item = ContentCandidate>,
    request: &ContentRequest,
) -> Vec<ContentCandidate> {
    let mut scored: Vec<ContentCandidate> = candidates
        .into_iter()
        .filter(|candidate| !request.release_only || candidate.channel == "release")
        .map(|mut candidate| {
            candidate.score = score_candidate(&candidate, request);
            candidate
        })
        .collect();
    scored.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.date_published.cmp(&left.date_published))
    });
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, channel: &str, loaders: &[&str], games: &[&str]) -> ContentCandidate {
        ContentCandidate {
            version_id: id.to_owned(),
            version_number: id.to_owned(),
            channel: channel.to_owned(),
            featured: false,
            loaders: loaders.iter().map(|s| (*s).to_owned()).collect(),
            game_versions: games.iter().map(|s| (*s).to_owned()).collect(),
            date_published: format!("2024-01-0{}T00:00:00Z", id.len()),
            score: 0,
        }
    }

    #[test]
    fn content_kind_round_trips() {
        for kind in ContentKind::ALL {
            assert_eq!(ContentKind::parse(kind.as_str()), Ok(kind));
            assert_eq!(kind.to_string(), kind.as_str());
        }
    }

    #[test]
    fn content_kind_parse_is_case_insensitive_and_rejects_unknown() {
        assert_eq!(ContentKind::parse("MOD"), Ok(ContentKind::Mod));
        assert_eq!(
            ContentKind::parse("TexturePack"),
            Err(UnknownContentKind("TexturePack".to_owned()))
        );
    }

    #[test]
    fn install_dirs_are_server_relative() {
        assert_eq!(ContentKind::Mod.install_dir(), PathBuf::from("mods"));
        assert_eq!(ContentKind::Plugin.install_dir(), PathBuf::from("plugins"));
        assert_eq!(
            ContentKind::ResourcePack.install_dir(),
            PathBuf::from("resourcepacks")
        );
        assert_eq!(
            ContentKind::DataPack.install_dir(),
            PathBuf::from("datapacks")
        );
        assert_eq!(
            ContentKind::Shader.install_dir(),
            PathBuf::from("shaderpacks")
        );
        // 整合包安装的是整个服务端：文件先落到 mods/ 再展开由 Service 负责
        assert_eq!(ContentKind::Modpack.install_dir(), PathBuf::from("mods"));
    }

    #[test]
    fn shader_is_not_server_usable() {
        assert!(!ContentKind::Shader.is_server_usable());
        for kind in [
            ContentKind::Mod,
            ContentKind::Plugin,
            ContentKind::ResourcePack,
            ContentKind::DataPack,
            ContentKind::Modpack,
        ] {
            assert!(kind.is_server_usable(), "{kind}");
        }
    }

    #[test]
    fn datapack_ignores_loader() {
        let request = ContentRequest::new(ContentKind::DataPack).loader("fabric");
        assert_eq!(request.loader_filter(), None);
        let request = ContentRequest::new(ContentKind::Mod).loader("fabric");
        assert_eq!(request.loader_filter(), Some("fabric"));
        // 空加载器不等于「匹配空串」：不做过滤
        let request = ContentRequest::new(ContentKind::Mod).loader("");
        assert_eq!(request.loader_filter(), None);
    }

    #[test]
    fn scoring_prefers_loader_match() {
        let request = ContentRequest::new(ContentKind::Mod)
            .loader("fabric")
            .game_version("1.21.1");
        let fabric = candidate("fabric", "release", &["fabric"], &["1.21.1"]);
        let forge = candidate("forge", "release", &["forge"], &["1.21.1"]);
        assert!(score_candidate(&fabric, &request) > score_candidate(&forge, &request));
    }

    #[test]
    fn scoring_prefers_game_version_match() {
        let request = ContentRequest::new(ContentKind::Mod)
            .loader("fabric")
            .game_version("1.21.1");
        let matching = candidate("a", "release", &["fabric"], &["1.21.1"]);
        let other = candidate("b", "release", &["fabric"], &["1.20.1"]);
        assert!(score_candidate(&matching, &request) > score_candidate(&other, &request));
    }

    #[test]
    fn release_only_filters_prerelease() {
        let request = ContentRequest::new(ContentKind::Mod);
        let ranked = rank_candidates(
            [
                candidate("r", "release", &["fabric"], &["1.21.1"]),
                candidate("b", "beta", &["fabric"], &["1.21.1"]),
            ],
            &request,
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].version_id, "r");
    }

    #[test]
    fn allow_prerelease_keeps_them_and_release_still_wins() {
        let request = ContentRequest::new(ContentKind::Mod).allow_prerelease();
        let ranked = rank_candidates(
            [
                candidate("b", "beta", &["fabric"], &["1.21.1"]),
                candidate("r", "release", &["fabric"], &["1.21.1"]),
            ],
            &request,
        );
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].version_id, "r");
    }

    #[test]
    fn ties_break_by_publish_date_desc() {
        let request = ContentRequest::new(ContentKind::Mod);
        let mut older = candidate("a", "release", &["fabric"], &["1.21.1"]);
        older.date_published = "2023-01-01T00:00:00Z".to_owned();
        let mut newer = candidate("b", "release", &["fabric"], &["1.21.1"]);
        newer.date_published = "2024-01-01T00:00:00Z".to_owned();
        // 版本号长度不同会改变构造出的日期，这里用显式日期覆盖
        let ranked = rank_candidates([older, newer], &request);
        assert_eq!(ranked[0].version_id, "b");
    }

    #[test]
    fn empty_candidates_rank_to_empty() {
        let request = ContentRequest::new(ContentKind::Mod);
        assert!(rank_candidates(Vec::new(), &request).is_empty());
    }

    #[test]
    fn guess_kind_maps_upstream_project_type() {
        assert_eq!(ProviderHint::guess_kind("mod"), Some(ContentKind::Mod));
        assert_eq!(
            ProviderHint::guess_kind("modpack"),
            Some(ContentKind::Modpack)
        );
        assert_eq!(
            ProviderHint::guess_kind("shader"),
            Some(ContentKind::Shader)
        );
        assert_eq!(ProviderHint::guess_kind("weird"), None);
    }
}
