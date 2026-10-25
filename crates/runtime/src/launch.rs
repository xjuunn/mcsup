//! 服务端启动参数与 JVM 命令构造
///
/// 「怎么启动 Minecraft」是 Runtime 的职责：JVM 参数、jar、工作目录。
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// 一次服务端启动的完整参数（已由调用方解析成绝对路径）
#[derive(Debug, Clone)]
pub struct MinecraftLaunch {
    /// 服务器目录（作为 JVM 工作目录）
    pub server_dir: PathBuf,
    /// `java` 可执行文件
    pub java_bin: PathBuf,
    /// 服务端核心 JAR 绝对路径
    pub jar_path: PathBuf,
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

/// JVM 参数（堆上限 + UTF-8 编码），jar 与 nogui 由 [`build_command`] 追加
#[must_use]
pub(crate) fn jvm_args(ram_mib: Option<u32>) -> Vec<String> {
    let mut args: Vec<String> = ENCODING_ARGS.iter().map(|arg| (*arg).to_owned()).collect();
    if let Some(mib) = ram_mib {
        args.push(format!("-Xmx{mib}M"));
    }
    args
}

/// 组装启动命令（不 spawn）
pub(crate) fn build_command(launch: &MinecraftLaunch) -> Command {
    let mut command = Command::new(&launch.java_bin);
    command.args(jvm_args(launch.ram_mib));
    command.arg("-jar").arg(&launch.jar_path).arg("nogui");
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

    fn launch(ram: Option<u32>) -> MinecraftLaunch {
        MinecraftLaunch {
            server_dir: PathBuf::from("/srv/mc/a"),
            java_bin: PathBuf::from("/jdk/bin/java"),
            jar_path: PathBuf::from("/srv/mc/a/server.jar"),
            ram_mib: ram,
        }
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
    fn command_runs_in_server_dir_with_nogui() {
        let cmd = build_command(&launch(Some(1024)));
        assert_eq!(cmd.get_program(), PathBuf::from("/jdk/bin/java"));
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "-Dfile.encoding=UTF-8".to_owned(),
                "-Dstdout.encoding=UTF-8".to_owned(),
                "-Dstderr.encoding=UTF-8".to_owned(),
                "-Dsun.stdout.encoding=UTF-8".to_owned(),
                "-Dsun.stderr.encoding=UTF-8".to_owned(),
                "-Xmx1024M".to_owned(),
                "-jar".to_owned(),
                "/srv/mc/a/server.jar".to_owned(),
                "nogui".to_owned(),
            ]
        );
        assert_eq!(
            cmd.get_current_dir(),
            Some(std::path::Path::new("/srv/mc/a"))
        );
    }
}
