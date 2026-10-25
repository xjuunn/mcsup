//! 服务端启动参数与 JVM 命令构造
///
/// 「怎么启动 Minecraft」是 Runtime 的职责：JVM 参数、jar / 参数文件、工作目录。
///
/// 两种启动形态由 [`domain::server::launch::LaunchSpec`] 表达：
///
/// - `Jar`：原版 / Fabric / Paper，`java -Xmx… -jar <jar> nogui`
/// - `Args`：`Forge` / `NeoForge`，`java -Xmx… @user_jvm_args.txt @…_args.txt nogui`
///
/// `LaunchSpec` 里是**相对服务器目录**的路径，这里统一拼成绝对路径再进命令行，
/// 避免依赖 JVM 的 `user.dir` 解析（supervisor spawn 时的 cwd 未必等于服务器目录）。
use domain::server::launch::LaunchSpec;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// 一次服务端启动的完整参数（已由调用方解析成绝对路径）
#[derive(Debug, Clone)]
pub struct MinecraftLaunch {
    /// 服务器目录（作为 JVM 工作目录）
    pub server_dir: PathBuf,
    /// `java` 可执行文件
    pub java_bin: PathBuf,
    /// 启动方式（相对服务器目录的路径，`build_command` 负责拼绝对）
    pub launch: LaunchSpec,
    /// 堆内存上限（MiB）；`None` 用 JVM 默认
    pub ram_mib: Option<u32>,
}

/// 强制 JVM 以 UTF-8 写标准输出 / 错误与文件
///
/// 服务端在 Windows 下默认按系统代码页（GBK）输出，中文写进 `console.log`
/// 后无法与 UTF-8 区分，回放必然乱码。这里统一钉成 UTF-8：
///
/// - `file.encoding`：默认字符集。JDK 18（JEP 400）起已是 UTF-8，但 17 及以下
///   仍随系统，显式设置以覆盖旧版本。
/// - `stdout.encoding` / `stderr.encoding`：`System.out/err` 的编码。
///   **JEP 400 明确不改这两条流的编码**（跟随控制台代码页），必须单独指定。
///   JDK 19 起是公开属性。
/// - `sun.stdout.encoding` / `sun.stderr.encoding`：JDK 17 及以下的等价私有
///   属性。新版会忽略，写上以兼容旧运行时。
///
/// 只影响新启动的进程。已写入磁盘的旧日志不会因此改变。
const ENCODING_ARGS: &[&str] = &[
    "-Dfile.encoding=UTF-8",
    "-Dstdout.encoding=UTF-8",
    "-Dstderr.encoding=UTF-8",
    "-Dsun.stdout.encoding=UTF-8",
    "-Dsun.stderr.encoding=UTF-8",
];

/// JVM 参数（堆上限 + UTF-8 编码），其余启动参数由 [`build_command`] 追加
#[must_use]
pub(crate) fn jvm_args(ram_mib: Option<u32>) -> Vec<String> {
    let mut args: Vec<String> = ENCODING_ARGS.iter().map(|arg| (*arg).to_owned()).collect();
    if let Some(mib) = ram_mib {
        args.push(format!("-Xmx{mib}M"));
    }
    args
}

/// 组装启动命令（不 spawn）
///
/// `Jar` 追加 `-jar <abs> nogui`；`Args` 依次追加 `@<abs>` 与 `nogui`。两条
/// 分支都以 `nogui` 收尾：服务端不应尝试开图形界面。
pub(crate) fn build_command(launch: &MinecraftLaunch) -> Command {
    let mut command = Command::new(&launch.java_bin);
    command.args(jvm_args(launch.ram_mib));
    match &launch.launch {
        LaunchSpec::Jar { path } => {
            command
                .arg("-jar")
                .arg(launch.server_dir.join(path))
                .arg("nogui");
        }
        LaunchSpec::Args { files } => {
            for file in files {
                command.arg(format!("@{}", launch.server_dir.join(file).display()));
            }
            command.arg("nogui");
        }
        // `LaunchSpec` 是 `#[non_exhaustive]`：未知形态按 JAR 启动兜底不会更强，
        // 直接只跑 JVM 参数——真实调用不会走到这里（上层已校验）
        _ => {}
    }
    command.current_dir(&launch.server_dir);
    command
}

/// 拉起服务端进程：stdin 管控制台，stdout / stderr 管道交给 supervisor 收集
///
/// 进程脱离终端运行（[`crate::process::detach`]）：服务端完全后台，Windows 上
/// 不为 JVM 新建控制台窗口，终端关闭也不带走服务端。
///
/// # Errors
///
/// 进程创建失败（java 不存在、权限不足等）时返回底层错误
pub(crate) fn spawn_minecraft(launch: &MinecraftLaunch) -> std::io::Result<Child> {
    let mut command = build_command(launch);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process::detach(&mut command);
    command.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jar_launch(ram: Option<u32>) -> MinecraftLaunch {
        MinecraftLaunch {
            server_dir: PathBuf::from("/srv/mc/a"),
            java_bin: PathBuf::from("/jdk/bin/java"),
            launch: LaunchSpec::jar("server.jar"),
            ram_mib: ram,
        }
    }

    fn args_launch(files: &[&str]) -> MinecraftLaunch {
        MinecraftLaunch {
            server_dir: PathBuf::from("/srv/mc/a"),
            java_bin: PathBuf::from("/jdk/bin/java"),
            launch: LaunchSpec::args(files.iter().copied()),
            ram_mib: Some(2048),
        }
    }

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn jvm_args_pins_utf8_and_ram_limit() {
        assert_eq!(
            jvm_args(Some(2048)),
            vec![
                "-Dfile.encoding=UTF-8".to_owned(),
                "-Dstdout.encoding=UTF-8".to_owned(),
                "-Dstderr.encoding=UTF-8".to_owned(),
                "-Dsun.stdout.encoding=UTF-8".to_owned(),
                "-Dsun.stderr.encoding=UTF-8".to_owned(),
                "-Xmx2048M".to_owned(),
            ]
        );
        assert_eq!(jvm_args(None), ENCODING_ARGS);
    }

    #[test]
    fn jar_command_runs_in_server_dir_with_nogui() {
        let cmd = build_command(&jar_launch(Some(1024)));
        assert_eq!(cmd.get_program(), PathBuf::from("/jdk/bin/java"));
        let expected_jar = PathBuf::from("/srv/mc/a").join("server.jar");
        assert_eq!(
            args_of(&cmd),
            vec![
                "-Dfile.encoding=UTF-8".to_owned(),
                "-Dstdout.encoding=UTF-8".to_owned(),
                "-Dstderr.encoding=UTF-8".to_owned(),
                "-Dsun.stdout.encoding=UTF-8".to_owned(),
                "-Dsun.stderr.encoding=UTF-8".to_owned(),
                "-Xmx1024M".to_owned(),
                "-jar".to_owned(),
                expected_jar.to_string_lossy().into_owned(),
                "nogui".to_owned(),
            ]
        );
        assert_eq!(
            cmd.get_current_dir(),
            Some(std::path::Path::new("/srv/mc/a"))
        );
    }

    /// Args 启动：`@绝对路径` 依次展开，以 nogui 收尾，无 `-jar`
    #[test]
    fn args_command_expands_parameter_files() {
        let cmd = build_command(&args_launch(&[
            "user_jvm_args.txt",
            "libraries/net/minecraftforge/forge/1.21.1-52.0.1/unix_args.txt",
        ]));
        let args = args_of(&cmd);
        let dir = std::path::Path::new("/srv/mc/a");
        let expected_user = format!("@{}", dir.join("user_jvm_args.txt").display());
        let expected_platform = format!(
            "@{}",
            dir.join("libraries/net/minecraftforge/forge/1.21.1-52.0.1/unix_args.txt")
                .display()
        );
        let mut expected: Vec<String> = ENCODING_ARGS.iter().map(|a| (*a).to_owned()).collect();
        expected.push("-Xmx2048M".to_owned());
        expected.push(expected_user);
        expected.push(expected_platform);
        expected.push("nogui".to_owned());
        assert_eq!(args, expected);
        assert!(!args.iter().any(|a| a == "-jar"), "Args 启动不应带 -jar");
    }

    /// Args 里没有文件时只留下 JVM 参数与 nogui（不会 panic）
    #[test]
    fn empty_args_launch_still_runs_jvm_with_nogui() {
        let cmd = build_command(&args_launch(&[]));
        let mut expected: Vec<String> = ENCODING_ARGS.iter().map(|a| (*a).to_owned()).collect();
        expected.push("-Xmx2048M".to_owned());
        expected.push("nogui".to_owned());
        assert_eq!(args_of(&cmd), expected);
    }
}
