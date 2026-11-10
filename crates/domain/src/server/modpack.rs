//! Modrinth 整合包（`.mrpack`）清单模型与判定
//!
//! `.mrpack` 是一个 zip，根目录的 `modrinth.index.json` 描述这份整合包：
//! 它依赖哪个 Minecraft 版本、哪个加载器版本，以及每个要落地的文件从哪下载、
//! 是不是服务端需要。本模块只解析这份清单并做**服务端视角**的判定，不做
//! 任何 IO 或网络。
//!
//! 规范见 <https://docs.modrinth.com/modpacks/format>。
//!
//! # 为什么清单解析在领域层
//!
//! 「哪些文件要装到服务端」「用哪个加载器」「这份整合包能不能开服」三个判定
//! 都在这里。入口与 Provider 只负责把清单字节搬过来；判定一旦分散，CLI 与
//! 未来的 API / MCP 就会各写一份，进而分叉。
//!
//! # 与服务端目录的关系
//!
//! 清单里的 `path` 是**相对整合包根**的路径（如 `mods/a.jar`、
//! `config/x.toml`）。服务端目录与整合包根同构，因此这里只做路径安全校验，
//! 不重新映射目录。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use thiserror::Error;

/// 清单文件名（`.mrpack` 根目录）
pub const INDEX_FILE: &str = "modrinth.index.json";

/// 清单格式版本（本项目只支持 1）
const SUPPORTED_FORMAT: u32 = 1;

/// 被禁用文件的统一后缀
///
/// Forge / `NeoForge` / Fabric 都按后缀跳过：改名的 `x.jar.disabled` 与
/// `x.jar.txt` 一样不会被加载，装了等于没装。
const DISABLED_SUFFIX: &str = ".disabled";

/// 服务端进程不读取的目录（小写比较）
///
/// 资源包与光影由客户端加载，服务端没有对应代码路径。清单里经常把它们标成
/// `server: required`（作者用客户端视角填的），按声明装会白下几十到几百 MiB。
const SERVER_IRRELEVANT_DIRS: [&str; 2] = ["resourcepacks/", "shaderpacks/"];

/// 一个文件的环境标记（`required` / `optional` / `unsupported`）
///
/// 反序列化在 `provider::modrinth::mrpack`（领域层不引入 serde，见模块文档）。
/// 刻意不加 `#[non_exhaustive]`：`Unsupported` 与其余两档处置相反，未知档
/// 不能静默归入任一侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvSupport {
    /// 该侧必须安装
    Required,
    /// 该侧可选（本项目按「装」处理：整合包作者通常希望前后端一致）
    Optional,
    /// 该侧不支持
    Unsupported,
}

/// 文件在两个侧上的支持情况
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileEnv {
    /// 客户端
    pub client: EnvSupport,
    /// 服务端
    pub server: EnvSupport,
}

impl FileEnv {
    /// 服务端是否需要安装这个文件
    ///
    /// `Required` 与 `Optional` 都装：`Optional` 表示「服务端可以不装」，但
    /// 整合包作者把它列进清单通常是因为它对服务端有益（如优化 mod），装上是
    /// 更符合预期的默认；`Unsupported` 明确表示服务端不该有它，必须跳过。
    #[must_use]
    pub const fn installs_on_server(self) -> bool {
        !matches!(self.server, EnvSupport::Unsupported)
    }
}

/// 清单里的一个待落地文件
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModpackFile {
    /// 相对整合包根的路径（如 `mods/a.jar`）
    pub path: String,
    /// 下载地址（按顺序尝试；空表示该文件由 overrides/ 提供，不联网）
    pub downloads: Vec<String>,
    /// 字节数
    pub size: u64,
    /// 可用校验和（sha1 / sha512）
    pub hashes: ModpackHashes,
    /// 环境标记
    pub env: Option<FileEnv>,
    /// 上游项目 ID（能从下载地址解析时）
    ///
    /// 清单不直接给项目 / 版本 ID，但 Modrinth 的下载地址是
    /// `https://cdn.modrinth.com/data/{project}/versions/{version}/{file}`，
    /// 两个 ID 都在里面。解析出来存下来，后续补依赖、查更新都能直接按 ID
    /// 定位，不必拿 modid 去搜索——搜索是猜，会挑错项目（实测 `satin`
    /// 搜出来首位是 `satin-free-wakes`）。
    pub project_id: Option<String>,
    /// 上游版本 ID（同上，从下载地址解析）
    pub version_id: Option<String>,
}

impl ModpackFile {
    /// 按清单条目构造，顺带解析下载地址里的项目 / 版本 ID
    ///
    /// ID 一律由这里推导，调用方不必各写一遍：解析规则只有一份，才不会出现
    /// 「某条路径忘了填 ID，后续拿 modid 去搜索猜错项目」。
    #[must_use]
    pub fn new(
        path: String,
        downloads: Vec<String>,
        size: u64,
        hashes: ModpackHashes,
        env: Option<FileEnv>,
    ) -> Self {
        let ids = downloads.iter().find_map(|url| modrinth_ids_from_url(url));
        let (project_id, version_id) = match ids {
            Some((project, version)) => (Some(project), Some(version)),
            None => (None, None),
        };
        Self {
            path,
            downloads,
            size,
            hashes,
            env,
            project_id,
            version_id,
        }
    }

    /// 是否落在 `mods/`（或 `plugins/`）下的 jar
    ///
    /// 只有这类文件能提供 modid，也只有它们值得按 modid 索引。
    #[must_use]
    pub fn is_plugin_jar(&self) -> bool {
        let lowered = self.path.to_ascii_lowercase();
        (lowered.starts_with("mods/") || lowered.starts_with("plugins/"))
            && Path::new(&self.path)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
            && !self.path.ends_with(DISABLED_SUFFIX)
    }

    /// 服务端是否需要这个文件
    #[must_use]
    pub fn installs_on_server(&self) -> bool {
        self.env.is_none_or(FileEnv::installs_on_server)
    }

    /// 服务端进程是否真的会读取这个文件
    ///
    /// 与 [`Self::installs_on_server`] 是两个不同的问题：后者问「清单声明的环境
    /// 要不要装到服务端」，这里问「服务端跑起来会不会读它」。清单只声明
    /// `client` / `server` 两个轴的**支持情况**，并不描述文件在服务端有没有用处，
    /// 因此仅客户端素材常被整合包作者一并标成 `server: required`（实测某整合包
    /// 113 个文件全部声明 `required`，其中 80% 的字节服务端永远用不到）。
    ///
    /// 以下两类装了也永远不生效：
    ///
    /// - `resourcepacks/`、`shaderpacks/`：资源包与光影由**客户端**加载，
    ///   服务端进程没有任何读取它们的代码路径
    /// - `*.disabled`：加载器按后缀跳过，等同于没装
    ///
    /// 只做这两条**确定性**判定，不猜「这个 mod 是不是仅客户端」——那需要上游
    /// 元数据（见 `ModpackSource::client_only_hashes`），猜错会装出一个起不来的
    /// 服务端，或删掉本该在的文件。
    #[must_use]
    pub fn is_server_loadable(&self) -> bool {
        if self.path.ends_with(DISABLED_SUFFIX) {
            return false;
        }
        let lowered = self.path.to_ascii_lowercase();
        !SERVER_IRRELEVANT_DIRS
            .iter()
            .any(|dir| lowered.starts_with(dir))
    }

    /// 是否有可下载地址
    #[must_use]
    pub fn has_download(&self) -> bool {
        !self.downloads.is_empty()
    }
}

/// 从 Modrinth 下载地址里解析 `(项目 ID, 版本 ID)`
///
/// 地址形如 `https://cdn.modrinth.com/data/{project}/versions/{version}/{file}`。
/// 不是这个形状（第三方站、自有镜像、overrides 里的相对路径）一律返回
/// `None`——解析不出来就是不知道，不编造。
#[must_use]
pub fn modrinth_ids_from_url(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("https://cdn.modrinth.com/data/")
        .or_else(|| url.strip_prefix("http://cdn.modrinth.com/data/"))?;
    let mut parts = rest.split('/');
    let project = parts.next().filter(|part| !part.is_empty())?;
    if parts.next()? != "versions" {
        return None;
    }
    let version = parts.next().filter(|part| !part.is_empty())?;
    Some((project.to_owned(), version.to_owned()))
}

/// 清单里的哈希表
///
/// Modrinth 每个文件都同时给 sha1 与 sha512。本项目下载器只认 sha1 / sha256，
/// 因此只保留 sha1；sha512 留着供后续按哈希反查使用。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModpackHashes {
    /// sha1（十六进制）
    pub sha1: Option<String>,
    /// sha512（十六进制）
    pub sha512: Option<String>,
}

impl ModpackHashes {
    /// 取可用于下载校验的 sha1
    ///
    /// 只有格式合法（40 位十六进制）才算数：上游给空串或残缺值时应当跳过
    /// 校验，而不是拿一个永远对不上的值把下载判失败。
    #[must_use]
    pub fn sha1_checksum(&self) -> Option<crate::server::artifact::Checksum> {
        let raw = self.sha1.as_deref()?;
        if raw.len() != 40 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some(crate::server::artifact::Checksum::new(
            crate::server::artifact::ChecksumAlgorithm::Sha1,
            raw,
        ))
    }
}

/// 整合包依赖的软件（`dependencies` 字段）
///
/// 上游用 `{name: version}` 的扁平对象表示，键是 `minecraft` /
/// `fabric-loader` / `forge` / `neoforge` / `quilt-loader`。这里按已知键归一
/// 成枚举，未知键保留原样（未来格式新增依赖不影响开服，只是不参与选 loader）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModpackDependencies {
    /// 原始依赖表（保留未知键）
    pub raw: BTreeMap<String, String>,
}

impl ModpackDependencies {
    /// Minecraft 版本
    #[must_use]
    pub fn minecraft(&self) -> Option<&str> {
        self.raw.get("minecraft").map(String::as_str)
    }

    /// 加载器与它的版本：`(加载器名, 版本)`
    ///
    /// 返回的名字与 `Modrinth` 的 loader facet 同名（`fabric` / `forge` /
    /// `neoforge` / `quilt`），可直接用于按版本过滤。
    #[must_use]
    pub fn loader(&self) -> Option<(&'static str, &str)> {
        const KEYS: [(&str, &str); 4] = [
            ("fabric-loader", "fabric"),
            ("neoforge", "neoforge"),
            ("forge", "forge"),
            ("quilt-loader", "quilt"),
        ];
        for (key, name) in KEYS {
            if let Some(version) = self.raw.get(key) {
                return Some((name, version.as_str()));
            }
        }
        None
    }
}

/// 解析后的整合包清单
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModpackIndex {
    /// 格式版本
    pub format_version: u32,
    /// 游戏（本项目只支持 `minecraft`）
    pub game: String,
    /// 整合包版本号（作者口径，如 `14.1.1`）
    pub version_id: String,
    /// 整合包名
    pub name: String,
    /// 依赖的软件版本
    pub dependencies: ModpackDependencies,
    /// 待落地文件
    pub files: Vec<ModpackFile>,
}

impl ModpackIndex {
    /// 服务端需要安装的文件（已过滤 `server: unsupported`）
    #[must_use]
    pub fn server_files(&self) -> Vec<&ModpackFile> {
        self.files
            .iter()
            .filter(|file| file.installs_on_server())
            .collect()
    }

    /// 服务端需要、且有下载地址的文件
    #[must_use]
    pub fn downloadable_server_files(&self) -> Vec<&ModpackFile> {
        self.server_files()
            .into_iter()
            .filter(|file| file.has_download())
            .collect()
    }

    /// 服务端需要、有下载地址、且服务端进程真的会读取的文件
    ///
    /// 下载只看这一步：判定见 [`ModpackFile::is_server_loadable`]。装到服务端
    /// 目录的文件集合仍是 [`Self::server_files`]，这里只负责不白下字节。
    #[must_use]
    pub fn fetchable_server_files(&self) -> Vec<&ModpackFile> {
        self.downloadable_server_files()
            .into_iter()
            .filter(|file| file.is_server_loadable())
            .collect()
    }

    /// 该整合包要求的 Minecraft 版本
    ///
    /// # Errors
    ///
    /// 清单没有声明 `minecraft` 依赖时返回 [`ModpackError::MissingMinecraft`]
    pub fn minecraft_version(&self) -> Result<&str, ModpackError> {
        self.dependencies
            .minecraft()
            .ok_or(ModpackError::MissingMinecraft)
    }

    /// 该整合包要求的主加载器
    ///
    /// 返回 `None` 表示这是原版整合包（只有数据包 / 资源包），应由原版核心承载。
    #[must_use]
    pub fn loader(&self) -> Option<(&'static str, &str)> {
        self.dependencies.loader()
    }
}

/// 整合包成员的依赖关系图
///
/// 用来回答一个问题：**服务端到底要装哪些成员**。输入是三份上游事实
/// （谁属于哪个项目、谁声明了哪些必需依赖、哪个项目被标成仅客户端），
/// 输出是「真正会装」的成员集合。
///
/// # 为什么需要不动点
///
/// 只做一次「谁被依赖」的统计是错的，因为**被跳过的成员不该为别人提供
/// 依赖保护**。反例来自真实整合包：`sodium` 被 `indium` / `iris` /
/// `reeses` 依赖，而这三个自己都是仅客户端、都会被跳过。一次统计会把
/// `sodium` 判成「有人依赖」而留下，服务端照样起不来。跳出这一步的
/// 依赖关系不复存在，必须先删掉跳过者，再看剩下的人需要什么。
///
/// 反过来，留下的成员需要的库即使自己被标成仅客户端也必须装（`satin`
/// 一类提供 API 的项目常被标 `unsupported`，却是别的 mod 的硬依赖）。
/// 因此从「服务端可用」的成员出发，沿必需依赖反复扩张到不再有新成员，
/// 就是最终要装的集合。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackMemberGraph {
    /// 文件哈希 → 该文件所属的项目 ID
    pub project_of: BTreeMap<String, String>,
    /// 文件哈希 → 该文件声明为必需依赖的项目 ID
    pub requires: BTreeMap<String, Vec<String>>,
    /// 上游把整个项目标成仅客户端的文件哈希
    pub client_only: BTreeSet<String>,
}

impl PackMemberGraph {
    /// 服务端真正会安装的成员（文件哈希）
    ///
    /// 起点是「自己不是仅客户端」的成员——它们无论如何都要装；然后反复
    /// 把「被已装成员必需依赖、且自己能提供该项目」的仅客户端成员并进来，
    /// 直到集合不再变大。两者都装之后，其余仅客户端成员才是真的该跳过。
    #[must_use]
    pub fn retained(&self) -> BTreeSet<String> {
        let mut kept: BTreeSet<String> = self
            .project_of
            .keys()
            .filter(|hash| !self.client_only.contains(*hash))
            .cloned()
            .collect();
        loop {
            // 只有「会装」的成员声明的必需依赖才提供保护
            let mut needed: BTreeSet<&str> = BTreeSet::new();
            for hash in &kept {
                if let Some(requires) = self.requires.get(hash) {
                    needed.extend(requires.iter().map(String::as_str));
                }
            }
            let mut grew = false;
            for hash in &self.client_only {
                if kept.contains(hash) {
                    continue;
                }
                let Some(project) = self.project_of.get(hash) else {
                    continue;
                };
                if needed.contains(project.as_str()) {
                    kept.insert(hash.clone());
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        kept
    }

    /// 仅客户端、但被留下的成员需要、因此不能跳过的文件哈希
    ///
    /// 判定口径与 [`Self::retained`] 一致：`retained` 里那些上游标成仅客户端的
    /// 条目。
    #[must_use]
    pub fn depended_on(&self) -> BTreeSet<String> {
        self.retained()
            .into_iter()
            .filter(|hash| self.client_only.contains(hash))
            .collect()
    }
}

/// 整合包清单解析 / 校验失败
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ModpackError {
    /// 清单不是合法 JSON 或字段类型不符
    #[error("invalid modpack index: {0}")]
    Parse(String),
    /// 不支持的文件名 / 路径（绝对路径、`..` 穿越、空路径）
    #[error("unsafe file path in modpack index: {0:?}")]
    UnsafePath(String),
    /// 格式版本不受支持
    #[error("unsupported modpack format version {0}")]
    UnsupportedFormat(u32),
    /// `game` 不是 `minecraft`
    #[error("unsupported modpack game: {0}")]
    UnsupportedGame(String),
    /// 清单没有声明 Minecraft 版本
    #[error("modpack index does not declare a minecraft dependency")]
    MissingMinecraft,
}

impl ModpackIndex {
    /// 组装并校验清单
    ///
    /// 反序列化在 Provider 侧（`provider::modrinth::mrpack`），这里只做**领域
    /// 校验**：格式版本、游戏、每个文件的路径。路径校验是必须的——清单来自
    /// 远端，`path` 直接参与本地落盘，`../` 会写到服务端目录外。
    ///
    /// # Errors
    ///
    /// 格式版本不支持、游戏不是 minecraft、或任一文件路径不安全时返回
    /// [`ModpackError`]
    pub fn new(
        format_version: u32,
        game: impl Into<String>,
        version_id: impl Into<String>,
        name: impl Into<String>,
        dependencies: ModpackDependencies,
        files: Vec<ModpackFile>,
    ) -> Result<Self, ModpackError> {
        if format_version != SUPPORTED_FORMAT {
            return Err(ModpackError::UnsupportedFormat(format_version));
        }
        let game = game.into();
        if game != "minecraft" {
            return Err(ModpackError::UnsupportedGame(game));
        }
        for file in &files {
            validate_relative_path(&file.path)?;
        }
        Ok(Self {
            format_version,
            game,
            version_id: version_id.into(),
            name: name.into(),
            dependencies,
            files,
        })
    }
}

impl fmt::Display for ModpackIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.version_id)
    }
}

/// 校验归档内相对路径可以安全落地
///
/// 拒绝：绝对路径、盘符（含 `:`）、`..` 段、空路径、只剩分隔符的路径。允许子目录
/// （`config/x.toml`）——整合包本来就要铺一层配置。
///
/// 清单里的路径与 `overrides/` 里的成员名来自外部归档，解压适配层落盘前也要
/// 用它：安全检查必须是同一套规则。
///
/// # Errors
///
/// 路径不安全时返回 [`ModpackError::UnsafePath`]
pub fn validate_relative_path(raw: &str) -> Result<(), ModpackError> {
    if raw.is_empty() {
        return Err(ModpackError::UnsafePath(raw.to_owned()));
    }
    // 盘符（`C:/x`）在 Unix 上不会被识别成 `Component::Prefix`，而它是绝对
    // 路径的等价物：直接按字符拒绝，跨平台口径一致
    if raw.contains(':') {
        return Err(ModpackError::UnsafePath(raw.to_owned()));
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(ModpackError::UnsafePath(raw.to_owned()));
    }
    let mut seen = false;
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::Normal(_) => seen = true,
            // `.` 可以忽略，`..` 与根 / 前缀一律拒绝
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ModpackError::UnsafePath(raw.to_owned()));
            }
        }
    }
    if !seen {
        return Err(ModpackError::UnsafePath(raw.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps(pairs: &[(&str, &str)]) -> ModpackDependencies {
        ModpackDependencies {
            raw: pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        }
    }

    fn file(path: &str, downloads: &[&str], env: Option<FileEnv>) -> ModpackFile {
        ModpackFile::new(
            path.to_owned(),
            downloads.iter().map(|url| (*url).to_owned()).collect(),
            1,
            ModpackHashes::default(),
            env,
        )
    }

    fn index(files: Vec<ModpackFile>, dependencies: ModpackDependencies) -> ModpackIndex {
        ModpackIndex::new(1, "minecraft", "1.0.0", "Pack", dependencies, files).expect("校验通过")
    }

    #[test]
    fn builds_index_and_reads_dependencies() {
        let index = index(
            Vec::new(),
            deps(&[("minecraft", "1.21.1"), ("fabric-loader", "0.16.0")]),
        );
        assert_eq!(index.format_version, 1);
        assert_eq!(index.game, "minecraft");
        assert_eq!(index.version_id, "1.0.0");
        assert_eq!(index.name, "Pack");
        assert_eq!(index.minecraft_version(), Ok("1.21.1"));
        assert_eq!(index.loader(), Some(("fabric", "0.16.0")));
        assert_eq!(index.to_string(), "Pack 1.0.0");
    }

    #[test]
    fn loader_detection_prefers_known_keys_and_keeps_unknown() {
        for (key, expected) in [
            ("fabric-loader", "fabric"),
            ("quilt-loader", "quilt"),
            ("forge", "forge"),
            ("neoforge", "neoforge"),
        ] {
            let index = index(Vec::new(), deps(&[("minecraft", "1.21.1"), (key, "9.9")]));
            assert_eq!(index.loader(), Some((expected, "9.9")), "{key}");
        }
        // 只有未知依赖：没有加载器（原版整合包），但原始键仍保留
        let index = index(
            Vec::new(),
            deps(&[("minecraft", "1.21.1"), ("some-future-loader", "1")]),
        );
        assert_eq!(index.loader(), None);
        assert_eq!(
            index
                .dependencies
                .raw
                .get("some-future-loader")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn missing_minecraft_is_reported() {
        let index = index(Vec::new(), deps(&[("fabric-loader", "0.16.0")]));
        assert_eq!(
            index.minecraft_version(),
            Err(ModpackError::MissingMinecraft)
        );
    }

    #[test]
    fn rejects_unsupported_format_and_game() {
        assert_eq!(
            ModpackIndex::new(2, "minecraft", "1", "P", deps(&[]), Vec::new()),
            Err(ModpackError::UnsupportedFormat(2))
        );
        assert_eq!(
            ModpackIndex::new(1, "other", "1", "P", deps(&[]), Vec::new()),
            Err(ModpackError::UnsupportedGame("other".to_owned()))
        );
    }

    #[test]
    fn rejects_unsafe_paths() {
        for path in ["../evil.jar", "/etc/passwd", "a/../../b", ".", "", "C:/x"] {
            let result = ModpackIndex::new(
                1,
                "minecraft",
                "1",
                "P",
                deps(&[]),
                vec![file(path, &["https://x/y.jar"], None)],
            );
            assert!(
                matches!(result, Err(ModpackError::UnsafePath(_))),
                "{path:?} 应被拒绝: {result:?}"
            );
        }
    }

    #[test]
    fn allows_curdir_and_nested_relative_paths() {
        for path in ["a/./b.jar", "config/sub/x.toml", "mods/a.jar"] {
            let result = ModpackIndex::new(
                1,
                "minecraft",
                "1",
                "P",
                deps(&[]),
                vec![file(path, &["https://x/y"], None)],
            );
            assert!(result.is_ok(), "{path:?} 应允许: {result:?}");
        }
    }

    #[test]
    fn server_file_filtering_respects_env_and_downloads() {
        let required = Some(FileEnv {
            client: EnvSupport::Required,
            server: EnvSupport::Required,
        });
        let server_unsupported = Some(FileEnv {
            client: EnvSupport::Required,
            server: EnvSupport::Unsupported,
        });
        let server_optional = Some(FileEnv {
            client: EnvSupport::Unsupported,
            server: EnvSupport::Optional,
        });
        let index = index(
            vec![
                file("mods/a.jar", &["https://x/a.jar"], required),
                file("mods/b.jar", &["https://x/b.jar"], server_unsupported),
                file("mods/c.jar", &["https://x/c.jar"], server_optional),
                file("mods/d.jar", &[], required),
                file("mods/e.jar", &["https://x/e.jar"], None),
            ],
            deps(&[("minecraft", "1.21.1")]),
        );
        // 服务端要装：a（required）、c（optional）、d（required 但无下载）、e（无 env 默认装）
        let server: Vec<&str> = index
            .server_files()
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(
            server,
            vec!["mods/a.jar", "mods/c.jar", "mods/d.jar", "mods/e.jar"]
        );
        // 可下载的只有三份（d 由 overrides/ 提供，不联网）
        let downloadable: Vec<&str> = index
            .downloadable_server_files()
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(downloadable, vec!["mods/a.jar", "mods/c.jar", "mods/e.jar"]);
    }

    #[test]
    fn env_filtering_without_env_installs() {
        let file = file("mods/x.jar", &["https://x"], None);
        assert!(file.installs_on_server());
    }

    /// 服务端永远不读的文件不下载，但**仍然算要装的**——
    /// 装的集合与下载的集合是两个问题，混起来会让「清单里有这个文件」
    /// 与「下过这个文件」互相污染。
    #[test]
    fn server_irrelevant_files_are_not_downloaded() {
        let required = Some(FileEnv {
            client: EnvSupport::Required,
            server: EnvSupport::Required,
        });
        let index = index(
            vec![
                file("mods/a.jar", &["https://x/a.jar"], required),
                file("resourcepacks/pack.zip", &["https://x/p.zip"], required),
                file("shaderpacks/s.zip", &["https://x/s.zip"], required),
                file("mods/off.jar.disabled", &["https://x/o.jar"], required),
            ],
            deps(&[("minecraft", "1.21.1")]),
        );
        // 四份都要装进服务端目录
        assert_eq!(index.server_files().len(), 4);
        assert_eq!(index.downloadable_server_files().len(), 4);
        // 只有 mods/a.jar 值得下
        let fetch: Vec<&str> = index
            .fetchable_server_files()
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(fetch, vec!["mods/a.jar"]);
    }

    /// 目录名大小写不敏感：清单来自外部，`ResourcePacks/` 同样不该下
    #[test]
    fn server_irrelevant_dirs_match_case_insensitively() {
        for path in [
            "resourcepacks/a.zip",
            "ResourcePacks/a.zip",
            "SHADERPACKS/a.zip",
            "mods/a.jar.disabled",
        ] {
            let file = file(path, &["https://x"], None);
            assert!(!file.is_server_loadable(), "{path:?} 不该下载");
        }
        // 名字里含关键词但不在该目录下的正常 mod 不能被误伤
        for path in [
            "mods/resourcepacks.jar",
            "config/resourcepacks.toml",
            "mods/a.jar",
        ] {
            let file = file(path, &["https://x"], None);
            assert!(file.is_server_loadable(), "{path:?} 应该下载");
        }
    }

    #[test]
    fn sha1_checksum_requires_valid_hex() {
        let good = ModpackHashes {
            sha1: Some("a".repeat(40)),
            sha512: None,
        };
        assert!(good.sha1_checksum().is_some());

        for bad in [None, Some(String::new()), Some("xyz".to_owned())] {
            let hashes = ModpackHashes {
                sha1: bad,
                sha512: None,
            };
            assert!(hashes.sha1_checksum().is_none());
        }
    }

    /// 建一张成员依赖图：`(哈希, 项目ID, 必需依赖的项目ID, 是否仅客户端)`
    fn graph(members: &[(&str, &str, &[&str], bool)]) -> PackMemberGraph {
        let mut out = PackMemberGraph::default();
        for (hash, project, requires, client_only) in members {
            out.project_of
                .insert((*hash).to_owned(), (*project).to_owned());
            out.requires.insert(
                (*hash).to_owned(),
                requires.iter().map(|id| (*id).to_owned()).collect(),
            );
            if *client_only {
                out.client_only.insert((*hash).to_owned());
            }
        }
        out
    }

    fn names<'a>(set: impl IntoIterator<Item = &'a String>) -> Vec<&'a str> {
        set.into_iter().map(String::as_str).collect()
    }

    #[test]
    fn only_client_member_without_dependents_is_skipped() {
        let g = graph(&[
            ("h-api", "fabric-api", &[], false),
            ("h-etf", "etf", &[], true),
        ]);
        assert!(g.depended_on().is_empty());
        assert_eq!(names(&g.retained()), vec!["h-api"]);
    }

    #[test]
    fn library_needed_by_server_member_is_kept() {
        // satin 一类：自己被标仅客户端，却是 environmentz 的硬依赖
        let g = graph(&[
            ("h-env", "environmentz", &["satin"], false),
            ("h-satin", "satin", &[], true),
        ]);
        assert_eq!(names(&g.depended_on()), vec!["h-satin"]);
        assert_eq!(names(&g.retained()), vec!["h-env", "h-satin"]);
    }

    #[test]
    fn dependency_of_a_skipped_member_protects_nothing() {
        // 真实反例：indium 依赖 sodium，而 indium 自己被跳过，
        // 于是 sodium 也不该留下
        let g = graph(&[
            ("h-fabric-api", "fabric-api", &[], false),
            ("h-indium", "indium", &["sodium"], true),
            ("h-sodium", "sodium", &["fabric-api"], true),
        ]);
        assert!(g.depended_on().is_empty());
        assert_eq!(names(&g.retained()), vec!["h-fabric-api"]);
    }

    #[test]
    fn closure_follows_chained_dependencies_to_a_fixed_point() {
        // a（服务端可用）→ b（仅客户端）→ c（仅客户端）
        let g = graph(&[
            ("h-a", "a", &["b"], false),
            ("h-b", "b", &["c"], true),
            ("h-c", "c", &[], true),
            ("h-orphan", "orphan", &[], true),
        ]);
        assert_eq!(names(&g.retained()), vec!["h-a", "h-b", "h-c"]);
        assert_eq!(names(&g.depended_on()), vec!["h-b", "h-c"]);
    }

    #[test]
    fn dependency_on_a_project_outside_the_pack_changes_nothing() {
        // 整合包里没有 fabric-api（由加载器或用户另行提供），不该凭空留下东西
        let g = graph(&[("h-x", "x", &["fabric-api"], true)]);
        assert!(g.retained().is_empty());
    }
}
