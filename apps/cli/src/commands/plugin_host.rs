//! 插件宿主的探测与拉起
//!
//! `PluginService` 只经控制通道与宿主对话，宿主本身是独立进程
//! （`mcsup __plugin-host`）。管理命令与服务器启停都要先确保它在，统一放在
//! 这里，避免每处各写一份等待逻辑。

use crate::config;
use crate::diagnostic::AppDiagnostic;
use anyhow::{Context, Result};
use std::time::Duration;

/// 宿主等待就绪的上限：拉起后最多等这么久
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// 轮询间隔
const READY_POLL: Duration = Duration::from_millis(100);

/// 宿主控制通道是否已可用
#[must_use]
pub(crate) fn available() -> bool {
    mcsup::plugin::ControlClient::new(&config::platform_data_dir()).is_available()
}

/// 宿主不在时拉起 `__plugin-host` 并等待可连接
///
/// 已在运行时直接返回，不重复拉起。
///
/// # Errors
///
/// 定位或启动子进程失败、超过 [`READY_TIMEOUT`] 仍未就绪时返回错误
pub(crate) fn ensure() -> Result<()> {
    let data_dir = config::platform_data_dir();
    let client = mcsup::plugin::ControlClient::new(&data_dir);
    if client.is_available() {
        return Ok(());
    }
    let exe = std::env::current_exe().context("定位 mcsup 可执行文件")?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("__plugin-host")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // 宿主是常驻进程，不能挂在拉起它的这个 CLI 进程的终端上。终端销毁
    //（包括无窗口的伪控制台）后，宿主手里会留一个死掉的控制台句柄，之后
    // 它再拉起 `cmd.exe`（`.cmd` 入口）就会在控制台初始化阶段失败，退出码
    // `0xC0000142`（`STATUS_DLL_INIT_FAILED`），且一行输出都没有。与远端
    // agent 同一口径：脱离终端 + 独立进程组。
    crate::daemon::detach(&mut command);
    command.spawn().context("拉起插件宿主")?;
    let attempts = READY_TIMEOUT.as_millis() / READY_POLL.as_millis();
    for _ in 0..attempts {
        if client.is_available() {
            return Ok(());
        }
        std::thread::sleep(READY_POLL);
    }
    Err(AppDiagnostic::failure(
        format!("插件宿主未能在 {} 秒内就绪", READY_TIMEOUT.as_secs()),
        Some("查看 `mcsup plugin doctor` 或宿主日志".to_owned()),
    )
    .into())
}
