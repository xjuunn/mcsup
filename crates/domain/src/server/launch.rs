//! 服务端启动方式
//!
//! 原版 / Fabric / Paper 都是「单个可运行 JAR，`java -jar <jar> nogui`」；
//! `Forge` 与 `NeoForge` 由安装器产出目录，启动命令是一串 `@参数文件`，没有可
//! `-jar` 的入口。两者都必须能被 [`super::meta::ServerMeta`] 记住，否则
//! 重装后只能靠猜。
//!
//! 这里只做「怎么启动」的建模，不构造命令行；命令行由 Runtime 组装。

use std::path::PathBuf;

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
    pub fn primary_file(&self) -> &std::path::Path {
        match self {
            Self::Jar { path } => path,
            Self::Args { files, .. } => files
                .first()
                .map_or_else(|| std::path::Path::new(""), PathBuf::as_path),
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
        assert_eq!(spec.primary_file(), std::path::Path::new("server.jar"));
        assert!(!spec.is_empty());
    }

    #[test]
    fn args_spec_primary_file_is_first_args_file() {
        let spec = LaunchSpec::args(["user_jvm_args.txt", "libraries/x/unix_args.txt"]);
        assert_eq!(
            spec.primary_file(),
            std::path::Path::new("user_jvm_args.txt")
        );
        assert!(!spec.is_empty());
    }

    #[test]
    fn empty_args_spec_is_empty() {
        let spec = LaunchSpec::Args {
            files: Vec::new(),
            args: vec![NOGUI.to_owned()],
        };
        assert!(spec.is_empty());
        assert_eq!(spec.primary_file(), std::path::Path::new(""));
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
}
