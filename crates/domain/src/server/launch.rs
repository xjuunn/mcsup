//! 服务端启动方式
//!
//! 原版 / Fabric / Paper 都是「单个可运行 JAR，`java -jar <jar> nogui`」；
//! `Forge` 与 `NeoForge` 由安装器产出目录，启动命令是一串 `@参数文件`，没有可
//! `-jar` 的入口。两者都必须能被 [`super::meta::ServerMeta`] 记住，否则
//! 重装后只能靠猜。
//!
//! 这里只做「怎么启动」的建模，不构造命令行；命令行由 Runtime 组装。

use super::version::ServerVariant;
use std::path::{Path, PathBuf};

/// 安装器产出的首个启动参数文件（[`LaunchSpec::Args`] 的第一项）
pub const USER_ARGS: &str = "user_jvm_args.txt";

/// `DirProbe::list` 返回的一项
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// 子项完整路径
    pub path: PathBuf,
    /// 是否为目录
    pub is_dir: bool,
}

/// 目录探测能力：布局判定属于领域，读盘属于宿主
///
/// [`installer_args_files`] 需要「这个文件在不在」「这个目录里有什么」两个
/// 事实才能下判定。领域层不直接调 `std::fs`——那样它就无法脱离真实磁盘测试，
/// 也无法在非本地场景复用。把这两个能力声明成 trait，由调用方（`service`）
/// 注入实现，判定逻辑本身仍完整留在领域层。
pub trait DirProbe {
    /// `path` 是否为已存在的普通文件
    fn is_file(&self, path: &Path) -> bool;

    /// 列出 `dir` 的直接子项；读不到时返回 `None`
    fn list(&self, dir: &Path) -> Option<Vec<DirEntry>>;
}

/// 安装器类变体的启动参数文件（相对服务器目录，按展开顺序）
///
/// `Forge` 与 `NeoForge` 的安装器都会写出 `user_jvm_args.txt` 与平台专属的
/// `win_args.txt` / `unix_args.txt`，但 Maven 坐标不同：
///
/// - `Forge`：`libraries/net/minecraftforge/forge/{build}`
/// - `NeoForge`：`libraries/net/neoforged/neoforge/{build}`
///
/// 只返回磁盘上**确实存在**的那些，缺第一个（用户参数文件）视为安装失败。
/// 平台参数文件路径由变体对应的坐标推出；不存在时再扫一遍 `libraries` 兜底，
/// 容忍上游路径变化。
///
/// 兜底**只认所在目录名等于本 build 的**：一个目录里可能同时存在多个版本的
/// `win_args.txt`（升级后旧版本目录不会自动清理），随便取第一个会拉起到旧版本。
///
/// 属于领域判定：这是「该变体该如何启动」，`service` 编排与 `runtime` 组装命令行
/// 都要用同一份，放在任一实现层都会让另一层要么复制、要么反向依赖。
///
/// 需要看磁盘的部分经 `probe` 注入，领域层自己不做 IO。
#[must_use]
pub fn installer_args_files<P: DirProbe + ?Sized>(
    probe: &P,
    variant: &ServerVariant,
    build: &str,
    dir: &Path,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !probe.is_file(&dir.join(USER_ARGS)) {
        return files;
    }
    files.push(PathBuf::from(USER_ARGS));

    let platform = platform_args_name();
    if let Some(base) = installer_maven_base(variant) {
        let relative = format!("{base}/{build}/{platform}");
        if probe.is_file(&dir.join(&relative)) {
            files.push(PathBuf::from(relative));
            return files;
        }
    }
    // 兜底：上游若改了坐标或版本目录命名，仍要求所在目录就是本 build
    if let Some(rel) = find_args_file(probe, &dir.join("libraries"), platform, build) {
        files.push(rel);
    }
    files
}

/// 平台参数文件名：Windows 与其余平台由安装器分别产出
#[must_use]
pub fn platform_args_name() -> &'static str {
    if cfg!(windows) {
        "win_args.txt"
    } else {
        "unix_args.txt"
    }
}

/// 安装器类变体的 Maven 坐标前缀（相对服务器目录，不含 build 段）
///
/// 只有走 `@参数文件` 启动的变体才有值；其余（含未来新增的）返回 `None`，
/// 由 [`installer_args_files`] 的兜底扫描决定能否找到。
#[must_use]
pub fn installer_maven_base(variant: &ServerVariant) -> Option<&'static str> {
    match variant {
        ServerVariant::Forge => Some("libraries/net/minecraftforge/forge"),
        ServerVariant::NeoForge => Some("libraries/net/neoforged/neoforge"),
        _ => None,
    }
}

/// 在 `root` 下递归找本 build 的平台参数文件，返回相对服务器目录的路径
///
/// 命中条件是「文件名匹配 **且** 所在目录名等于 `build`」——宁可找不到（由上层
/// 报错）也不要拉起到另一个版本。深度受限，避免扫遍整个 libraries。
fn find_args_file<P: DirProbe + ?Sized>(
    probe: &P,
    root: &Path,
    name: &str,
    build: &str,
) -> Option<PathBuf> {
    fn walk<P: DirProbe + ?Sized>(
        probe: &P,
        dir: &Path,
        name: &str,
        build: &str,
        depth: usize,
    ) -> Option<PathBuf> {
        if depth == 0 {
            return None;
        }
        let entries = probe.list(dir)?;
        for entry in entries {
            let path = entry.path;
            if entry.is_dir {
                if let Some(found) = walk(probe, &path, name, build, depth - 1) {
                    return Some(found);
                }
            } else if path.file_name().and_then(|n| n.to_str()) == Some(name)
                && path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    == Some(build)
            {
                return Some(path);
            }
        }
        None
    }
    let found = walk(probe, root, name, build, 8)?;
    // 转成相对服务器目录：root 是 `{dir}/libraries`，取其父
    let server_dir = root.parent()?;
    found.strip_prefix(server_dir).ok().map(PathBuf::from)
}

/// 服务端统一的程序参数：不尝试开图形界面
const NOGUI: &str = "nogui";

/// 服务端启动方式
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LaunchSpec {
    /// 单个可运行 JAR：`java -jar <path> nogui`
    ///
    /// `path` 相对服务器目录（如 `server.jar`）。
    Jar {
        /// 相对服务器目录的核心 JAR 路径
        path: PathBuf,
    },
    /// 无 `-jar` 入口，靠一串 `@文件` 拼出命令行（`Forge` / `NeoForge`）
    ///
    /// 实际命令形如 `java @user_jvm_args.txt @libraries/.../unix_args.txt nogui`：
    /// 前段是调用方自己的 JVM 参数文件，后段是安装器产出的参数文件，
    /// 末尾的 `nogui` 由 [`Self::program_args`] 钉死。路径全部相对服务器目录。
    Args {
        /// 依次展开的参数文件，相对服务器目录
        files: Vec<PathBuf>,
        /// 追加给服务端的程序参数（参数文件之后、以独立 token 传入）
        args: Vec<String>,
    },
}

impl LaunchSpec {
    /// 单个 JAR 启动
    #[must_use]
    pub fn jar(path: impl Into<PathBuf>) -> Self {
        Self::Jar { path: path.into() }
    }

    /// 参数文件启动
    #[must_use]
    pub fn args(files: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self::Args {
            files: files.into_iter().map(Into::into).collect(),
            args: vec![NOGUI.to_owned()],
        }
    }

    /// 追加给服务端的程序参数
    ///
    /// `Jar` 与 `Args` 都是 `nogui`：服务端不该尝试开图形界面。以后要加
    /// `--universe` 一类参数时，两边都从这里取，避免逐处漏改。
    #[must_use]
    pub fn program_args(&self) -> Vec<String> {
        match self {
            Self::Jar { .. } => vec![NOGUI.to_owned()],
            Self::Args { args, .. } => args.clone(),
        }
    }

    /// 替换程序参数
    ///
    /// `Args` 形态直接落 `args`；`Jar` 形态的服务端参数只有 `nogui`，没有
    /// 第二份存储位置，因此非空的 `args` 只在不等于默认值时无法表达——届时
    /// 保持默认（`Jar` 启动的真实调用只传 `nogui`）。空 `args` 保留默认值，
    /// 不把「没传」误当成「不要 nogui」。
    #[must_use]
    pub fn with_program_args(self, args: Vec<String>) -> Self {
        if args.is_empty() {
            return self;
        }
        match self {
            Self::Jar { .. } => self,
            Self::Args { files, .. } => Self::Args { files, args },
        }
    }

    /// 需要存在的主文件（相对服务器目录）
    ///
    /// 启动前据此确认核心已装成：`Jar` 是那个 JAR，`Args` 是第一个参数文件
    /// （安装器总会产出它，是「装过」的最早可观测证据）。
    #[must_use]
    pub fn primary_file(&self) -> &Path {
        match self {
            Self::Jar { path } => path,
            Self::Args { files, .. } => files
                .first()
                .map_or_else(|| Path::new(""), PathBuf::as_path),
        }
    }

    /// 是否为空规格（`Args` 且没有任何文件）
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Jar { .. } => false,
            Self::Args { files, .. } => files.is_empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jar_spec_primary_file_is_the_jar() {
        let spec = LaunchSpec::jar("server.jar");
        assert_eq!(spec.primary_file(), Path::new("server.jar"));
        assert!(!spec.is_empty());
    }

    #[test]
    fn args_spec_primary_file_is_first_args_file() {
        let spec = LaunchSpec::args(["user_jvm_args.txt", "libraries/x/unix_args.txt"]);
        assert_eq!(spec.primary_file(), Path::new("user_jvm_args.txt"));
        assert!(!spec.is_empty());
    }

    #[test]
    fn empty_args_spec_is_empty() {
        let spec = LaunchSpec::Args {
            files: Vec::new(),
            args: vec![NOGUI.to_owned()],
        };
        assert!(spec.is_empty());
        assert_eq!(spec.primary_file(), Path::new(""));
    }

    #[test]
    fn args_spec_preserves_order() {
        let spec = LaunchSpec::args(["a.txt", "b.txt", "c.txt"]);
        let LaunchSpec::Args { files, args } = spec else {
            panic!("应为 Args");
        };
        assert_eq!(
            files,
            vec![
                PathBuf::from("a.txt"),
                PathBuf::from("b.txt"),
                PathBuf::from("c.txt")
            ]
        );
        assert_eq!(args, vec![NOGUI.to_owned()]);
    }

    /// 两种形态都以 `nogui` 收尾：服务端不该尝试开图形界面
    #[test]
    fn program_args_end_with_nogui_for_both_forms() {
        assert_eq!(LaunchSpec::jar("server.jar").program_args(), vec!["nogui"]);
        assert_eq!(
            LaunchSpec::args(["user_jvm_args.txt"]).program_args(),
            vec!["nogui"]
        );
    }

    // ---- 安装器产物的磁盘布局 ----
    //
    // 这些用例全部走内存 `FakeProbe`：领域层的判定不该依赖真实磁盘，否则
    // 既跑不快，也没法表达「读不到目录」这类事实。

    /// 内存目录树：`is_file` 判定集合里的路径，`list` 按父目录分组
    #[derive(Default)]
    struct FakeProbe {
        files: Vec<PathBuf>,
        dirs: Vec<PathBuf>,
    }

    impl FakeProbe {
        fn file(mut self, path: impl Into<PathBuf>) -> Self {
            self.files.push(path.into());
            self
        }

        fn dir(mut self, path: impl Into<PathBuf>) -> Self {
            self.dirs.push(path.into());
            self
        }
    }

    impl DirProbe for FakeProbe {
        fn is_file(&self, path: &Path) -> bool {
            self.files.iter().any(|f| f == path)
        }

        fn list(&self, dir: &Path) -> Option<Vec<DirEntry>> {
            if !self.dirs.iter().any(|d| d == dir) {
                return None;
            }
            let mut out = Vec::new();
            for d in &self.dirs {
                if d != dir && d.parent() == Some(dir) {
                    out.push(DirEntry {
                        path: d.clone(),
                        is_dir: true,
                    });
                }
            }
            for f in &self.files {
                if f.parent() == Some(dir) {
                    out.push(DirEntry {
                        path: f.clone(),
                        is_dir: false,
                    });
                }
            }
            Some(out)
        }
    }

    /// 坐标命中：用户参数 + `libraries/net/minecraftforge/forge/<build>/{platform}_args.txt`
    #[test]
    fn finds_installer_args_by_maven_coordinate() {
        let dir = Path::new("/srv/mc");
        let rel = format!(
            "libraries/net/minecraftforge/forge/26.3-66.1.0/{}",
            platform_args_name()
        );
        let probe = FakeProbe::default()
            .file(dir.join(USER_ARGS))
            .dir(dir.join("libraries"))
            .file(dir.join(&rel));

        let files = installer_args_files(&probe, &ServerVariant::Forge, "26.3-66.1.0", dir);
        assert_eq!(files, vec![PathBuf::from(USER_ARGS), PathBuf::from(rel)]);
    }

    /// 缺用户参数文件 = 安装失败（返回空，由上层报错）
    #[test]
    fn missing_user_args_yields_nothing() {
        let dir = Path::new("/srv/mc");
        let probe = FakeProbe::default();
        assert!(
            installer_args_files(&probe, &ServerVariant::Forge, "26.3", dir).is_empty(),
            "缺 user_jvm_args.txt 时必须返回空，交由上层报安装失败"
        );
    }

    /// 兜底扫描只认本 build 的目录，不会拉起到旧版本
    #[test]
    fn fallback_only_accepts_matching_build() {
        let dir = Path::new("/srv/mc");
        let stale_dir = dir.join("libraries/net/minecraftforge/forge/26.3-OLD");
        let probe = FakeProbe::default()
            .file(dir.join(USER_ARGS))
            .dir(dir.join("libraries"))
            .dir(dir.join("libraries/net/minecraftforge"))
            .dir(dir.join("libraries/net/minecraftforge/forge"))
            .dir(stale_dir.clone())
            .file(stale_dir.join(platform_args_name()));

        // build 不匹配 → 只剩用户参数文件
        let files = installer_args_files(&probe, &ServerVariant::Forge, "26.3", dir);
        assert_eq!(
            files,
            vec![PathBuf::from(USER_ARGS)],
            "兜底扫描不得命中其他 build 的目录"
        );
    }

    /// 兜底能得到正确结果时，返回相对服务器目录的路径
    #[test]
    fn fallback_returns_path_relative_to_server_dir() {
        let dir = Path::new("/srv/mc");
        // 坐标前缀不匹配（forge 之外的路径），只能走兜底
        let build_dir = dir.join("libraries/renamed/forge/26.3");
        let probe = FakeProbe::default()
            .file(dir.join(USER_ARGS))
            .dir(dir.join("libraries"))
            .dir(dir.join("libraries/renamed"))
            .dir(dir.join("libraries/renamed/forge"))
            .dir(build_dir.clone())
            .file(build_dir.join(platform_args_name()));

        let files = installer_args_files(&probe, &ServerVariant::Forge, "26.3", dir);
        assert_eq!(
            files,
            vec![
                PathBuf::from(USER_ARGS),
                PathBuf::from(format!(
                    "libraries/renamed/forge/26.3/{}",
                    platform_args_name()
                ))
            ]
        );
    }

    /// 读不到目录时退化为「找不到」，不 panic
    #[test]
    fn unreadable_libraries_dir_falls_back_to_user_args_only() {
        let dir = Path::new("/srv/mc");
        let probe = FakeProbe::default().file(dir.join(USER_ARGS));

        let files = installer_args_files(&probe, &ServerVariant::Forge, "26.3", dir);
        assert_eq!(files, vec![PathBuf::from(USER_ARGS)]);
    }

    /// 非安装器类变体没有坐标前缀
    #[test]
    fn non_installer_variants_have_no_maven_base() {
        assert!(installer_maven_base(&ServerVariant::Vanilla).is_none());
        assert!(installer_maven_base(&ServerVariant::Forge).is_some());
        assert!(installer_maven_base(&ServerVariant::NeoForge).is_some());
    }
}
