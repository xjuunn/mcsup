//! 插件宿主的探测、拉起与自愈
//!
//! `PluginService` 只经控制通道与宿主对话，宿主本身是独立进程
//! （`mcsup __plugin-host`）。管理命令与服务器启停都要先确保它在，统一放在
//! 这里，避免每处各写一份等待逻辑。
//!
//! 宿主是常驻进程，二进制被替换（重新编译、`cargo install`、升级安装包）不会
//! 换掉已经在跑的那一个：它继续执行启动时那份镜像里的代码。端口连得上只说明
//! 「有人监听」，不说明监听者是不是当前构建——所以连上之后还要核对构建标识，
//! 不一致就把旧宿主换掉并恢复它原本在跑的插件（见 [`adopt_current_build`]）。
//! 没有这一步，升级之后的所有插件操作都会静默地落在旧代码上。

use crate::commands::plugin_bindings;
use crate::config;
use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use anyhow::{Context, Result};
use mcsup::domain::plugin::PluginId;
use mcsup::domain::plugin::state::PluginState;
use mcsup::plugin::{BuildFingerprint, ControlClient, HostStaleness};
use mcsup::port::plugin::ErrorCode;
use serde_json::Value;
use std::time::Duration;

/// 宿主等待就绪的上限：拉起后最多等这么久
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// 轮询间隔
const READY_POLL: Duration = Duration::from_millis(100);

/// 宿主控制通道是否已可用
#[must_use]
pub(crate) fn available() -> bool {
    ControlClient::new(&config::platform_data_dir()).is_available()
}

/// 确保一个**当前构建**的宿主在跑
///
/// 宿主不在时拉起；在跑但构建不是当前这个可执行文件时换掉它，并按绑定门禁
/// 恢复它原本在跑的插件。
///
/// # Errors
///
/// 定位或启动子进程失败、超过 [`READY_TIMEOUT`] 仍未就绪、旧宿主未按时退出时
/// 返回错误
pub(crate) fn ensure(printer: Printer) -> Result<()> {
    let data_dir = config::platform_data_dir();
    let client = ControlClient::new(&data_dir);
    if !client.is_available() {
        return spawn(&client);
    }
    adopt_current_build(&client, printer)
}

/// 连上的宿主是否是当前构建；不是就换成当前的
///
/// 三种不一致的成因不同，提示要能说清是「版本变了」「换了个可执行文件」还是
/// 「同一个路径上的文件被重新编译了」——用户据此判断是不是自己刚做的事。
fn adopt_current_build(client: &ControlClient, printer: Printer) -> Result<()> {
    let local = BuildFingerprint::current();
    match client.identify() {
        Ok(host) => {
            let staleness = host.staleness(&local);
            if !staleness.is_stale() {
                return Ok(());
            }
            replace(client, &describe(&staleness), printer)
        }
        // 认不出 `identity` 的宿主一定早于自愈机制本身：它能应答，但不知道
        // 自己在跑哪个构建，只能换掉
        Err(err) if err.code == ErrorCode::InvalidInput => {
            replace(client, "插件宿主是旧构建（不识别构建握手）", printer)
        }
        // 端点刚好消失之类的瞬时故障：调用方照常重连，下一次判定再处理
        Err(_) => Ok(()),
    }
}

/// 顶掉旧宿主，拉起当前构建，并恢复旧宿主上的插件
fn replace(client: &ControlClient, reason: &str, printer: Printer) -> Result<()> {
    // 先记下「谁在跑」：换宿主之后就没有人去问旧进程了
    let running = running_plugins(client);
    printer.print_note(&format!("{reason}，正在重启插件宿主"));
    client.shutdown().context("请求旧插件宿主退出")?;
    wait_until_down(client)?;
    spawn(client)?;
    printer.print_note(&crate::i18n::pick(
        printer.lang(),
        "插件宿主已换成当前构建",
        "plugin host now runs the current build",
    ));
    plugin_bindings::restore_after_restart(&running, printer);
    Ok(())
}

/// 等旧宿主真正放开端点
///
/// `shutdown` 只是请求：宿主还要收掉插件进程才会退出。端点仍在就拉新的只会
/// 撞上单实例锁，所以要等到连不上为止。
fn wait_until_down(client: &ControlClient) -> Result<()> {
    let attempts = READY_TIMEOUT.as_millis() / READY_POLL.as_millis();
    for _ in 0..attempts {
        if !client.is_available() {
            return Ok(());
        }
        std::thread::sleep(READY_POLL);
    }
    Err(AppDiagnostic::failure(
        format!("旧插件宿主未能在 {} 秒内退出", READY_TIMEOUT.as_secs()),
        Some("结束该进程后重试，或查看 `mcsup plugin doctor`".to_owned()),
    )
    .into())
}

/// 拉起 `__plugin-host` 并等待可连接
fn spawn(client: &ControlClient) -> Result<()> {
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

/// 旧宿主上正在跑的插件
///
/// 读不出来就当没有：换宿主比恢复插件重要，不能因为一次列表失败把旧进程留着。
fn running_plugins(client: &ControlClient) -> Vec<PluginId> {
    let Ok(list) = client.list() else {
        return Vec::new();
    };
    list.get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    item.get("state")
                        .and_then(|state| serde_json::from_value::<PluginState>(state.clone()).ok())
                        .is_some_and(PluginState::is_alive)
                })
                .filter_map(|item| {
                    item.get("id")
                        .and_then(Value::as_str)
                        .and_then(|raw| PluginId::parse(raw).ok())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 不一致的成因，取一句能对得上用户刚做过的事的话
fn describe(staleness: &HostStaleness) -> String {
    match staleness {
        HostStaleness::VersionMismatch { host, local } => {
            format!("插件宿主仍是 {host} 构建，当前为 {local}")
        }
        HostStaleness::DifferentExe { host, .. } => {
            format!("插件宿主来自 {}，不是当前可执行文件", host.display())
        }
        HostStaleness::BinaryReplaced { path } => {
            format!("插件宿主是 {} 被替换之前启动的旧进程", path.display())
        }
        _ => "插件宿主不是当前构建".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "测试夹具，失败直接 panic 才有用")]

    use super::*;
    use std::path::PathBuf;

    #[test]
    fn describe_names_the_actual_cause() {
        let version = describe(&HostStaleness::VersionMismatch {
            host: "0.1.0".to_owned(),
            local: "0.2.0".to_owned(),
        });
        assert!(
            version.contains("0.1.0") && version.contains("0.2.0"),
            "{version}"
        );

        let other = describe(&HostStaleness::DifferentExe {
            host: PathBuf::from("/opt/old/mcsup"),
            local: PathBuf::from("/opt/new/mcsup"),
        });
        assert!(other.contains("/opt/old/mcsup"), "{other}");

        let replaced = describe(&HostStaleness::BinaryReplaced {
            path: PathBuf::from("/home/u/.cargo/bin/mcsup"),
        });
        assert!(replaced.contains("/home/u/.cargo/bin/mcsup"), "{replaced}");
    }
}
