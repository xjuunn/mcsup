//! `server` 生命周期命令：start / stop / restart / status / console / send / logs
//!
//! 只做参数解析、进度回显与结果渲染；判定与编排在 `service::ServerLifecycleService`，
//! supervisor 行为在 `runtime`（经 `port::SupervisorPort` 注入）。

use crate::bootstrap;
use crate::diagnostic::AppDiagnostic;
use crate::input;
use crate::keymap::Action;
use crate::output::{self, Printer};
use crate::progress::TaskSpinner;
use crate::prompts;
use anyhow::{Context, Result};
use comfy_table::Table;
use mcsup::port::{StopOutcome, StopSpec, SupervisorStartOutcome};
use mcsup::protocol::cli::{LogLineJson, LogsTailJson, SendJson, StartJson, StatusJson, StopJson};
use mcsup::service::{LifecycleError, ServerLifecycleService, StartOptions};
use std::sync::Arc;
use std::time::Duration;

/// 装配生命周期服务：经库侧组合根（`mcsup::bootstrap`）统一装配，
/// CLI 不再手工拼注册表 + Java + supervisor
pub(crate) fn lifecycle_service() -> Result<Arc<ServerLifecycleService>> {
    let mcsup = bootstrap::open_mcsup_with_remotes()?;
    Ok(Arc::new(mcsup.lifecycle.clone()))
}

/// 单阶段回显：进度在结果返回前收掉，错误路径不留悬空 spinner
fn with_spinner<T>(
    printer: Printer,
    message: String,
    run: impl FnOnce(&TaskSpinner) -> Result<T>,
) -> Result<T> {
    let spinner = TaskSpinner::new(message, printer.is_human());
    let result = run(&spinner);
    spinner.finish();
    result
}

/// 共享的启动流程（start 与 restart 后半段）：EULA 非交互报错，交互下询问后重试
/// Java 缺失时交互提示安装，安装后重试
async fn run_start(
    service: &ServerLifecycleService,
    name: &str,
    mut accept_eula: bool,
    supervisor_exe: &std::path::Path,
    printer: Printer,
) -> Result<SupervisorStartOutcome> {
    let opts = |accept: bool| StartOptions {
        accept_eula: accept,
        ram_mib: None,
        supervisor_exe: supervisor_exe.to_path_buf(),
        supervisor_args: vec![std::ffi::OsString::from("__supervise")],
    };
    let attempt = |accept: bool| {
        with_spinner(
            printer,
            crate::i18n::pick(
                printer.lang(),
                format!("正在启动 {name}…"),
                format!("starting {name}…"),
            ),
            |spinner| {
                service
                    .start(
                        name,
                        &opts(accept),
                        &mut || {
                            spinner.set_message(crate::i18n::pick(
                                printer.lang(),
                                "正在扫描本机 Java…",
                                "scanning the host for Java…",
                            ));
                        },
                        &mut |_| {},
                    )
                    .map_err(anyhow::Error::from)
            },
        )
    };

    loop {
        match attempt(accept_eula) {
            Ok(outcome) => return Ok(outcome),
            Err(err) => {
                let needs_eula = err
                    .downcast_ref::<LifecycleError>()
                    .is_some_and(|e| matches!(e, LifecycleError::EulaRequired));
                if needs_eula && printer.is_interactive() {
                    if !prompts::confirm_or_yes(
                        printer,
                        "需要同意 Mojang EULA（eula=true），是否同意？",
                        true,
                    )? {
                        return Err(anyhow::anyhow!("未同意 EULA，已中止"));
                    }
                    accept_eula = true;
                    continue;
                }

                // 交互式修复循环：缺哪个 Java 装哪个，装完重试
                // NoJava：本机一个运行时都没有（清单为空，说不出缺哪个版本）→
                // 引导用户从已发布大版本中选一个装；
                // UnknownJavaMajor：服务器绑定 / 版本要求的 Java 大版本缺失 →
                // 明确点名该版本（class file 69.0 这类报错的根因是选低了）
                let missing = err.downcast_ref::<LifecycleError>().and_then(|e| match e {
                    LifecycleError::NoJava => Some(None),
                    LifecycleError::UnknownJavaMajor { major, .. } => Some(Some(*major)),
                    _ => None,
                });

                if let Some(missing_major) = missing {
                    if !printer.is_interactive() {
                        return Err(err);
                    }
                    let prompt_text = match missing_major {
                        Some(major) => format!("本机缺少 Java {major}，是否现在安装？"),
                        None => "本机未发现任何 Java 运行时，是否现在安装？".to_owned(),
                    };
                    if !prompts::confirm_or_yes(printer, &prompt_text, true)? {
                        return Err(anyhow::anyhow!("缺少所需 Java 版本，已中止启动"));
                    }
                    // 安装缺的版本；NoJava 无从点名，装当前 LTS 21 作为起步
                    let major_str = missing_major.map_or("21".to_string(), |m| m.to_string());
                    crate::commands::java::install_with_outcome(
                        Some(&major_str),
                        false,
                        None,
                        printer,
                    )
                    .await?;
                    continue;
                }

                return Err(err);
            }
        }
    }
}

/// `server start`：启动并报告 supervisor PID
pub(crate) async fn start(name: &str, accept_eula: bool, printer: Printer) -> Result<()> {
    let service = lifecycle_service()?;
    let supervisor_exe = std::env::current_exe().context("定位 mcsup 可执行文件")?;
    let outcome = run_start(&service, name, accept_eula, &supervisor_exe, printer).await?;
    let payload = StartJson {
        name,
        supervisor_pid: outcome.supervisor_pid,
    };
    printer.print_outcome(
        &format!(
            "已启动 {name}（supervisor PID {}），运行 mcsup server status {name} 查看启动进度",
            payload.supervisor_pid
        ),
        &payload,
    )
}

/// `server stop`：优雅停机（`--grace` 到期且 `--force` 时强杀）
pub(crate) fn stop(name: &str, grace: u64, force: bool, printer: Printer) -> Result<()> {
    let service = lifecycle_service()?;
    let spec = StopSpec {
        grace: Duration::from_secs(grace),
        force,
    };
    let outcome = with_spinner(
        printer,
        crate::i18n::pick(
            printer.lang(),
            format!("正在停止 {name}…（宽限 {grace}s）"),
            format!("stopping {name}… (grace {grace}s)"),
        ),
        |_| service.stop(name, spec).map_err(anyhow::Error::from),
    )?;

    let (human, payload) = match outcome {
        StopOutcome::Stopped { elapsed } => (
            crate::i18n::pick(
                printer.lang(),
                format!("已停止 {name}（耗时 {}s）", elapsed.as_secs()),
                format!("stopped {name} (took {}s)", elapsed.as_secs()),
            ),
            StopJson {
                name,
                outcome: "stopped",
                elapsed_secs: Some(elapsed.as_secs()),
            },
        ),
        StopOutcome::AlreadyStopped => (
            crate::i18n::pick(
                printer.lang(),
                format!("{name} 未在运行"),
                format!("{name} is not running"),
            ),
            StopJson {
                name,
                outcome: "already_stopped",
                elapsed_secs: None,
            },
        ),
        StopOutcome::TimedOut { waited } => {
            return Err(anyhow::anyhow!(crate::i18n::pick(
                printer.lang(),
                format!(
                    "停机超时（已等待 {}s），可加 --force 强杀",
                    waited.as_secs()
                ),
                format!(
                    "shutdown timed out after {}s; add --force to kill it",
                    waited.as_secs()
                ),
            )));
        }
    };
    printer.print_outcome(&human, &payload)
}

/// `server restart`：先停（超时中止），再走与 start 相同的启动路径
pub(crate) async fn restart(
    name: &str,
    accept_eula: bool,
    grace: u64,
    force: bool,
    printer: Printer,
) -> Result<()> {
    let service = lifecycle_service()?;
    let supervisor_exe = std::env::current_exe().context("定位 mcsup 可执行文件")?;

    let spec = StopSpec {
        grace: Duration::from_secs(grace),
        force,
    };
    let stop_outcome = with_spinner(
        printer,
        crate::i18n::pick(
            printer.lang(),
            format!("正在停止 {name}…（宽限 {grace}s）"),
            format!("stopping {name}… (grace {grace}s)"),
        ),
        |_| service.stop(name, spec).map_err(anyhow::Error::from),
    )?;
    match stop_outcome {
        StopOutcome::Stopped { .. } | StopOutcome::AlreadyStopped => {}
        StopOutcome::TimedOut { waited } => {
            return Err(anyhow::anyhow!(crate::i18n::pick(
                printer.lang(),
                format!(
                    "停机超时（已等待 {}s），重启中止；可加 --force 强杀",
                    waited.as_secs()
                ),
                format!(
                    "shutdown timed out after {}s; restart aborted; add --force to kill it",
                    waited.as_secs()
                ),
            )));
        }
    }

    let outcome = run_start(&service, name, accept_eula, &supervisor_exe, printer).await?;
    let payload = StartJson {
        name,
        supervisor_pid: outcome.supervisor_pid,
    };
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("已重启 {name}（supervisor PID {}）", payload.supervisor_pid),
            format!(
                "restarted {name} (supervisor PID {})",
                payload.supervisor_pid
            ),
        ),
        &payload,
    )
}

/// `server status`：注册信息 + 实时状态
pub(crate) fn status(name: &str, printer: Printer) -> Result<()> {
    let service = lifecycle_service()?;
    let report = service.describe(name)?;
    let view = report.status.as_ref();
    let running = view.is_some_and(|s| s.status.is_alive());
    let payload = StatusJson {
        name: &report.name,
        version: &report.version,
        variant: &report.variant,
        running,
        status: view.map_or("stopped", |s| s.status.as_str()),
        pid: view.and_then(|s| s.pid),
        uptime_secs: view.and_then(|s| s.uptime.map(|d| d.as_secs())),
        exit_code: view.and_then(|s| s.exit_code),
    };

    if printer.is_human() {
        let l = printer.lang();
        let mut table = Table::new();
        table.set_content_arrangement(comfy_table::ContentArrangement::Dynamic);
        table.set_header([
            crate::i18n::text(l, "项", "field"),
            crate::i18n::text(l, "值", "value"),
        ]);
        table.add_row([crate::i18n::text(l, "名称", "name"), payload.name]);
        table.add_row([crate::i18n::text(l, "版本", "version"), payload.version]);
        table.add_row([crate::i18n::text(l, "变体", "variant"), payload.variant]);
        table.add_row([crate::i18n::text(l, "状态", "status"), payload.status]);
        table.add_row([
            comfy_table::Cell::new(crate::i18n::text(l, "进程", "pid")),
            comfy_table::Cell::new(
                payload
                    .pid
                    .map_or_else(|| "-".to_owned(), |p| p.to_string()),
            ),
        ]);
        table.add_row([
            comfy_table::Cell::new(crate::i18n::text(l, "运行时长", "uptime")),
            comfy_table::Cell::new(fmt_uptime(payload.uptime_secs)),
        ]);
        table.add_row([
            comfy_table::Cell::new(crate::i18n::text(l, "退出码", "exit code")),
            comfy_table::Cell::new(
                payload
                    .exit_code
                    .map_or_else(|| "-".to_owned(), |c| c.to_string()),
            ),
        ]);
        output::print_table(&table);
        Ok(())
    } else {
        let line = serde_json::to_string(&payload)?;
        output::print_json_line(&line);
        Ok(())
    }
}

/// `server console`：stdin 注入 + 实时输出，快捷键动作（当前为 Ctrl+D/Ctrl+Z 分离）
///
/// 行读取本身只负责取一行；控制字符 → 动作在 [`crate::input`] + [`crate::keymap`] 完成。
/// 注入失败回显走 stderr：stdout 只留服务器输出。
#[allow(clippy::print_stderr)]
pub(crate) fn console(name: &str, printer: Printer) -> Result<()> {
    if !printer.is_interactive() {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                printer.lang(),
                "server console 需要交互终端",
                "server console needs an interactive terminal",
            ),
            Some(crate::i18n::pick(
                printer.lang(),
                "非交互下用 server send 注入单条命令、server logs 查看输出",
                "non-interactive: use server send for one command and server logs for output",
            )),
        )
        .into());
    }
    let service = lifecycle_service()?;
    let report = service.describe(name)?;
    if !report.status.as_ref().is_some_and(|s| s.status.is_alive()) {
        return Err(LifecycleError::NotRunning.into());
    }
    // 先取历史 + 增量读取器（同一快照为界，不重不漏），再回放，避免附加瞬间丢行
    let (history, mut watch) = service.log_watch_tail(name, crate::cli::LOG_TAIL_DEFAULT)?;

    // Windows 无 EOF 键：Ctrl+D 随整行到达，必须回车才生效
    let detach_hint = if cfg!(windows) {
        crate::i18n::pick(
            printer.lang(),
            "Ctrl+D 后回车分离",
            "Ctrl+D then enter to detach",
        )
    } else {
        crate::i18n::pick(printer.lang(), "Ctrl+D 分离", "Ctrl+D to detach")
    };
    printer.print_section(&crate::i18n::pick(
        printer.lang(),
        format!("已附加 {name} 控制台（{detach_hint}，服务器继续运行）"),
        format!("attached to {name} console ({detach_hint}; the server keeps running)"),
    ));
    for line in &history {
        print_raw_line(line);
    }
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let done = Arc::clone(&done);
        let service = Arc::clone(&service);
        let name = name.to_owned();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut line = String::new();
            loop {
                line.clear();
                match stdin.read_line(&mut line) {
                    // EOF（Unix 行首 Ctrl+D、Windows Ctrl+Z 回车）或读失败：只分离，不带走进程
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let (text, action) = input::split_action(&line);
                        let trimmed = text.trim_end();
                        if !trimmed.is_empty()
                            && let Err(err) = service.send_line(&name, trimmed)
                        {
                            eprintln!(
                                "{}",
                                crate::i18n::pick(
                                    crate::locale::current(),
                                    format!("注入失败: {err}"),
                                    format!("injection failed: {err}")
                                )
                            );
                            if matches!(err, LifecycleError::NotRunning) {
                                break;
                            }
                        }
                        // 动作判定收敛在 keymap：目前只有「分离」
                        if matches!(action, Some(Action::Detach)) {
                            break;
                        }
                    }
                }
            }
            done.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }

    let exited = wait_console_exit(&service, name, &mut watch, &done)?;
    report_console_exit(name, exited, printer);
    Ok(())
}

/// 控制台附加期间的主循环：等分离或服务器退出，返回是否为「服务器已停」
fn wait_console_exit(
    service: &ServerLifecycleService,
    name: &str,
    watch: &mut mcsup::service::LogWatch,
    done: &std::sync::atomic::AtomicBool,
) -> Result<bool> {
    // 服务器自行退出（例如注入 `stop`）也要结束附加：状态判定收敛在 service，
    // 这里只消费结果，避免控制台停在「服务器已停、console 仍未退出」的状态
    let mut exited = false;
    let mut ticks = 0u32;
    while !done.load(std::sync::atomic::Ordering::Relaxed) {
        for line in watch.read_new()? {
            print_raw_line(&line);
        }
        // 探活每 5 个轮询（约 500ms）一次：status 走 IPC，不必跟读日志同频
        ticks += 1;
        if ticks.is_multiple_of(5) && !server_alive(service, name)? {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // 收尾：线程退出前最后一段输出
    for line in watch.read_new()? {
        print_raw_line(&line);
    }
    Ok(exited)
}

/// 控制台收尾提示：服务器停了就说明原因，否则是用户主动分离
fn report_console_exit(name: &str, exited: bool, printer: Printer) {
    if exited {
        printer.print_section(&crate::i18n::pick(
            printer.lang(),
            format!("{name} 已停止，控制台已退出"),
            format!("{name} stopped; leaving the console"),
        ));
    } else {
        printer.print_section(&crate::i18n::pick(
            printer.lang(),
            "已分离（服务器继续运行）",
            "detached (the server keeps running)",
        ));
    }
}

/// 控制台附加期间探活：服务器已消失或进入终态即视为退出
fn server_alive(service: &ServerLifecycleService, name: &str) -> Result<bool> {
    Ok(service
        .describe(name)?
        .status
        .is_some_and(|s| !s.status.is_terminal()))
}

/// `server send`：单条命令注入 stdin
pub(crate) fn send(name: &str, command: &str, printer: Printer) -> Result<()> {
    let service = lifecycle_service()?;
    service.send_line(name, command)?;
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("已注入 {command}"),
            format!("sent {command}"),
        ),
        &SendJson { name, command },
    )
}

/// `server logs`：末尾若干行，`--follow` 持续跟踪（JSON 模式 JSONL）
pub(crate) fn logs(name: &str, tail: usize, follow: bool, printer: Printer) -> Result<()> {
    let service = lifecycle_service()?;
    let lines = service.log_tail(name, tail)?;
    if printer.is_human() {
        for line in &lines {
            print_raw_line(line);
        }
    } else {
        let payload = LogsTailJson {
            name,
            lines: lines.clone(),
        };
        let line = serde_json::to_string(&payload)?;
        output::print_json_line(&line);
    }
    if !follow {
        return Ok(());
    }

    let mut watch = service.log_watch(name)?;
    loop {
        for line in watch.read_new()? {
            print_stream_line(printer, &line)?;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 流式单行：human 原样输出；JSON 一行一个对象（`--follow` 的 JSONL 契约）
fn print_stream_line(printer: Printer, line: &str) -> Result<()> {
    if printer.is_human() {
        print_raw_line(line);
    } else {
        let payload = LogLineJson { line };
        let text = serde_json::to_string(&payload)?;
        output::print_json_line(&text);
    }
    Ok(())
}

#[allow(clippy::print_stdout)]
fn print_raw_line(line: &str) {
    println!("{line}");
}

/// 运行时长：秒 → `12s` / `3m20s` / `1h2m`
fn fmt_uptime(secs: Option<u64>) -> String {
    match secs {
        None => "-".to_owned(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m{}s", s / 60, s % 60),
        Some(s) => format!("{}h{}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::fmt_uptime;

    #[test]
    fn uptime_formats_by_scale() {
        assert_eq!(fmt_uptime(None), "-");
        assert_eq!(fmt_uptime(Some(0)), "0s");
        assert_eq!(fmt_uptime(Some(59)), "59s");
        assert_eq!(fmt_uptime(Some(60)), "1m0s");
        assert_eq!(fmt_uptime(Some(200)), "3m20s");
        assert_eq!(fmt_uptime(Some(3600)), "1h0m");
        assert_eq!(fmt_uptime(Some(3725)), "1h2m");
    }
}
