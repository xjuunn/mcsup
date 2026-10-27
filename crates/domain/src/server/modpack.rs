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

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use thiserror::Error;

/// 清单文件名（`.mrpack` 根目录）
pub const INDEX_FILE: &str = "modrinth.index.json";

/// 清单格式版本（本项目只支持 1）
const SUPPORTED_FORMAT: u32 = 1;

/// 一个文件的环境标记（`required` / `optional` / `unsupported`）
///
/// 反序列化在 `provider::modrinth::mrpack`（领域层不引入 serde，见模块文档）。
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
}

impl ModpackFile {
    /// 服务端是否需要这个文件
    #[must_use]
    pub fn installs_on_server(&self) -> bool {
        self.env.is_none_or(FileEnv::installs_on_server)
    }

    /// 是否有可下载地址
    #[must_use]
    pub fn has_download(&self) -> bool {
        !self.downloads.is_empty()
    }
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
        ModpackFile {
            path: path.to_owned(),
            downloads: downloads.iter().map(|url| (*url).to_owned()).collect(),
            size: 1,
            hashes: ModpackHashes::default(),
            env,
        }
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
}
