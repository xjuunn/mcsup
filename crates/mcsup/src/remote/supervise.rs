//! agent 侧的 supervisor 入口：`mcsup-agent __supervise …`
//!
//! 远端 `server start` 在 agent 机器上拉起 supervisor 子进程（`docs/remote.md`
//! 「远端 supervisor 形态」）。子进程必须是一个**能自己跑下去**的进程：它持有
//! JVM、控制台与 IPC 端点，agent 重启不影响已在跑的服务端。
//!
//! 这里只做参数解析 + 交给 `runtime::run_supervisor`，与 `mcsup __supervise`
//! 走同一条运行时路径：`LocalSupervisor` 拼出来的参数形状是固定的
//! （`--dir` / `--java` / `--jar` 或 `--arg-file` / `--server-arg` / `--ram`），
//! 两个入口解析同一份形状，才不会出现「本地能起、远端起不来」。
//!
//! 为什么不用 clap：`mcsup-agent` 是常驻进程，只有这一条隐藏入口需要子命令
//! 解析；为它引入整套 clap 依赖（以及随之而来的帮助文案、错误格式）不划算。
//! 参数形状由 `LocalSupervisor` 单方面产生，手写解析足够，且非法组合一律
//! 拒绝而不是猜。

use domain::server::launch::LaunchSpec;
use std::io::Write;
use std::path::PathBuf;

/// 隐藏入口的名字
pub const SUPERVISE_COMMAND: &str = "__supervise";

/// 一次 supervisor 启动的参数
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuperviseSpec {
    /// 服务器目录（作为 JVM 工作目录）
    pub dir: PathBuf,
    /// `java` 可执行文件
    pub java: PathBuf,
    /// 启动方式（相对服务器目录）
    pub launch: LaunchSpec,
    /// 堆上限（MiB）
    pub ram: Option<u32>,
}

/// 参数解析失败
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SuperviseError {
    /// 缺少必需参数
    #[error("missing required argument: {0}")]
    Missing(&'static str),
    /// 参数值非法
    #[error("invalid value for {flag}: {value}")]
    Invalid {
        /// 参数名
        flag: &'static str,
        /// 原始取值
        value: String,
    },
    /// `--jar` 与 `--arg-file` 同时出现或都没出现
    #[error("__supervise requires exactly one of --jar or --arg-file")]
    AmbiguousLaunch,
}

impl SuperviseSpec {
    /// 从参数迭代器解析（不含可执行文件与子命令名）
    ///
    /// # Errors
    ///
    /// 缺参数、值非法或启动方式不唯一时返回 [`SuperviseError`]
    pub fn parse<I, S>(args: I) -> Result<Self, SuperviseError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut dir: Option<PathBuf> = None;
        let mut java: Option<PathBuf> = None;
        let mut jar: Option<PathBuf> = None;
        let mut arg_files: Vec<PathBuf> = Vec::new();
        let mut server_args: Vec<String> = Vec::new();
        let mut ram: Option<u32> = None;

        let mut iter = args.into_iter().map(Into::into);
        while let Some(flag) = iter.next() {
            match flag.as_str() {
                "--dir" => dir = Some(PathBuf::from(next_value(&mut iter, "--dir")?)),
                "--java" => java = Some(PathBuf::from(next_value(&mut iter, "--java")?)),
                "--jar" => jar = Some(PathBuf::from(next_value(&mut iter, "--jar")?)),
                "--arg-file" => arg_files.push(PathBuf::from(next_value(&mut iter, "--arg-file")?)),
                "--server-arg" => server_args.push(next_value(&mut iter, "--server-arg")?),
                "--ram" => {
                    let raw = next_value(&mut iter, "--ram")?;
                    ram = Some(raw.parse().map_err(|_| SuperviseError::Invalid {
                        flag: "--ram",
                        value: raw,
                    })?);
                }
                other => {
                    return Err(SuperviseError::Invalid {
                        flag: "argument",
                        value: other.to_owned(),
                    });
                }
            }
        }

        // 两种启动方式二选一：都给了说明调用方在自相矛盾，都不给则无从启动
        let launch = match (jar, arg_files.is_empty()) {
            (Some(jar), true) => LaunchSpec::jar(jar).with_program_args(server_args),
            (None, false) => LaunchSpec::args(arg_files).with_program_args(server_args),
            _ => return Err(SuperviseError::AmbiguousLaunch),
        };
        Ok(Self {
            dir: dir.ok_or(SuperviseError::Missing("--dir"))?,
            java: java.ok_or(SuperviseError::Missing("--java"))?,
            launch,
            ram,
        })
    }

    /// 转成运行时的启动参数
    #[must_use]
    pub fn into_launch(self) -> runtime::MinecraftLaunch {
        runtime::MinecraftLaunch {
            server_dir: self.dir,
            java_bin: self.java,
            launch: self.launch,
            ram_mib: self.ram,
        }
    }
}

/// 取下一个参数值
fn next_value(
    iter: &mut impl Iterator<Item = String>,
    flag: &'static str,
) -> Result<String, SuperviseError> {
    iter.next().ok_or(SuperviseError::Missing(flag))
}

/// 是否是 supervisor 入口调用（`mcsup-agent __supervise …`）
#[must_use]
pub fn is_supervise_invocation<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .nth(1)
        .is_some_and(|arg| arg.as_ref() == SUPERVISE_COMMAND)
}

/// 入口调用的完整处理：解析 `std::env::args().skip(2)` 并阻塞跑 supervisor
///
/// 返回进程退出码（`0` 正常，非 `0` 表示启动或运行失败）。失败原因写 stderr：
/// 它由 `LocalSupervisor` 重定向进 `run/supervisor.log`，用户据此排查。
#[must_use]
pub fn supervise_main() -> i32 {
    let args = std::env::args().skip(2);
    match SuperviseSpec::parse(args) {
        Ok(spec) => runtime::run_supervisor(&spec.into_launch()),
        Err(err) => {
            // 不用 `eprintln!`：workspace 的 `print_stderr` lint 面向入口输出，
            // 库侧统一走显式 stderr 句柄（与 `runtime::supervisor` 同口径）
            let mut stderr = std::io::stderr();
            let _ = writeln!(stderr, "mcsup-agent {SUPERVISE_COMMAND}: {err}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jar_launch_like_the_local_supervisor_builds_it() {
        let spec = SuperviseSpec::parse([
            "--dir",
            "/srv/mc/web",
            "--java",
            "/jdk/bin/java",
            "--jar",
            "server.jar",
            "--server-arg",
            "nogui",
            "--ram",
            "2048",
        ])
        .expect("parse");
        assert_eq!(spec.dir, PathBuf::from("/srv/mc/web"));
        assert_eq!(spec.java, PathBuf::from("/jdk/bin/java"));
        assert_eq!(spec.launch, LaunchSpec::jar("server.jar"));
        assert_eq!(spec.launch.program_args(), vec!["nogui"]);
        assert_eq!(spec.ram, Some(2048));
    }

    #[test]
    fn parses_args_launch_and_keeps_file_order() {
        let spec = SuperviseSpec::parse([
            "--dir",
            "/srv/mc/forge",
            "--java",
            "/jdk/bin/java",
            "--arg-file",
            "user_jvm_args.txt",
            "--arg-file",
            "libraries/net/minecraftforge/forge/1.21.1/unix_args.txt",
        ])
        .expect("parse");
        let LaunchSpec::Args { files, .. } = &spec.launch else {
            panic!("应为 Args");
        };
        assert_eq!(
            files,
            &vec![
                PathBuf::from("user_jvm_args.txt"),
                PathBuf::from("libraries/net/minecraftforge/forge/1.21.1/unix_args.txt"),
            ]
        );
        assert_eq!(spec.launch.program_args(), vec!["nogui"], "缺省仍是 nogui");
    }

    /// 两种启动方式都给或都不给都必须报错：猜一个等于让 supervisor 跑错命令
    #[test]
    fn ambiguous_or_empty_launch_is_rejected() {
        assert_eq!(
            SuperviseSpec::parse(["--dir", "/a", "--java", "/j"]),
            Err(SuperviseError::AmbiguousLaunch)
        );
        assert_eq!(
            SuperviseSpec::parse([
                "--dir",
                "/a",
                "--java",
                "/j",
                "--jar",
                "s.jar",
                "--arg-file",
                "x.txt"
            ]),
            Err(SuperviseError::AmbiguousLaunch)
        );
    }

    #[test]
    fn missing_and_invalid_arguments_are_named() {
        assert_eq!(
            SuperviseSpec::parse(["--java", "/j", "--jar", "s.jar"]),
            Err(SuperviseError::Missing("--dir"))
        );
        assert_eq!(
            SuperviseSpec::parse(["--dir", "/a", "--jar", "s.jar"]),
            Err(SuperviseError::Missing("--java"))
        );
        assert_eq!(
            SuperviseSpec::parse([
                "--dir", "/a", "--java", "/j", "--jar", "s.jar", "--ram", "lots"
            ]),
            Err(SuperviseError::Invalid {
                flag: "--ram",
                value: "lots".to_owned()
            })
        );
        assert_eq!(
            SuperviseSpec::parse(["--dir"]),
            Err(SuperviseError::Missing("--dir"))
        );
    }

    /// 只有第二个位置是 `__supervise` 才算入口调用：别把普通参数误当子命令
    #[test]
    fn invocation_detection_looks_at_the_first_argument_only() {
        assert!(is_supervise_invocation([
            "mcsup-agent",
            "__supervise",
            "--dir",
            "/a"
        ]));
        assert!(!is_supervise_invocation(["mcsup-agent"]));
        assert!(!is_supervise_invocation(["mcsup-agent", "--serve"]));
        assert!(!is_supervise_invocation([
            "mcsup-agent",
            "--serve",
            "__supervise"
        ]));
    }
}
