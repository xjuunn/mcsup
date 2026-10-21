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

/// JVM 参数（堆上限），jar 与 nogui 由 [`build_command`] 追加
#[must_use]
pub(crate) fn jvm_args(ram_mib: Option<u32>) -> Vec<String> {
    match ram_mib {
        Some(mib) => vec![format!("-Xmx{mib}M")],
        None => Vec::new(),
    }
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
    fn jvm_args_only_encode_ram_limit() {
        assert_eq!(jvm_args(Some(2048)), vec!["-Xmx2048M".to_owned()]);
        assert!(jvm_args(None).is_empty());
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
