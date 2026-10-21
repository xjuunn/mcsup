//! 子进程创建约定：后台常驻进程不继承终端、不新开控制台窗口

use std::process::Command;

/// 脱离终端：Unix 新进程组，Windows 分离进程 + 新进程组
///
/// 保证 CLI 退出 / 终端关闭不带走子进程；子进程随之脱离前台信号组。
/// Windows 上父进程没有控制台时，未加标志的控制台程序（`java.exe`、
/// `cmd.exe`）会自行新建控制台并弹出窗口，必须显式分离。
#[cfg(unix)]
pub(crate) fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
pub(crate) fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn detach(_command: &mut Command) {}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::process::Stdio;

    /// 分离只改进程组 / 控制台归属：显式管道 IO 必须照常工作
    #[test]
    fn detached_spawn_keeps_pipes() {
        let mut command = Command::new("cmd");
        command.args(["/c", "echo", "ok"]);
        command.stdout(Stdio::piped());
        detach(&mut command);

        let mut child = command.spawn().expect("分离进程应能启动");
        let mut out = String::new();
        child
            .stdout
            .take()
            .expect("stdout 已管道化")
            .read_to_string(&mut out)
            .expect("读取子进程输出");
        let status = child.wait().expect("等待子进程");
        assert!(status.success());
        assert_eq!(out.trim(), "ok");
    }
}
