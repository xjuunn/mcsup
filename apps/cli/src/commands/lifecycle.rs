//! `server` 生命周期命令：start / stop / restart / status / console / send / logs
//!
//! 只做参数解析、进度回显与结果渲染；判定与编排在 `service::ServerLifecycleService`，
//! supervisor 行为在 `runtime`（经 `port::SupervisorPort` 注入）。

use crate::bootstrap;
use crate::diagnostic::AppDiagnostic;
use crate::output::{self, Printer};
use crate::progress::TaskSpinner;
use crate::prompts;
use anyhow::{Context, Result};
use comfy_table::Table;
use mcsup::port::{StopOutcome, StopSpec, SupervisorStartOutcome};
use mcsup::runtime::LocalSupervisor;
use mcsup::service::{LifecycleError, ServerLifecycleService, StartOptions};
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;

/// `server start` / `server restart` 结果（JSON 模式序列化用）
#[derive(Serialize)]
struct StartJson<'a> {
    name: &'a str,
    supervisor_pid: u32,
}

/// `server stop` 结果（JSON 模式序列化用）
#[derive(Serialize)]
struct StopJson<'a> {
    name: &'a str,
    outcome: &'a str,
    elapsed_secs: Option<u64>,
}

/// `server status` 结果（JSON 模式序列化用）
#[derive(Serialize)]
struct StatusJson<'a> {
    name: &'a str,
    version: &'a str,
    variant: &'a str,
    running: bool,
    status: &'a str,
    pid: Option<u32>,
    uptime_secs: Option<u64>,
    exit_code: Option<i32>,
}

/// `server send` 结果（JSON 模式序列化用）
#[derive(Serialize)]
struct SendJson<'a> {
    name: &'a str,
    command: &'a str,
}

/// `server logs --tail` 结果（JSON 模式序列化用）
#[derive(Serialize)]
struct LogsTailJson<'a> {
    name: &'a str,
    lines: Vec<String>,
}

/// 装配生命周期服务：注册表 + Java + 本机 supervisor
fn lifecycle_service() -> Result<Arc<ServerLifecycleService>> {
    let registry = bootstrap::open_registry()?;
    let java = bootstrap::open_java_service()?;
    Ok(Arc::new(ServerLifecycleService::new(
        registry,
        java,
        Arc::new(LocalSupervisor),
    )))
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
fn run_start(
    service: &ServerLifecycleService,
    name: &str,
    accept_eula: bool,
    supervisor_exe: &std::path::Path,
    printer: Printer,
) -> Result<SupervisorStartOutcome> {
    let opts = |accept: bool| StartOptions {
        accept_eula: accept,
        ram_mib: None,
        supervisor_exe: supervisor_exe.to_path_buf(),
    };
    let attempt = |accept: bool| {
        with_spinner(printer, format!("正在启动 {name}…"), |spinner| {
            service
                .start(
                    name,
                    &opts(accept),
                    &mut || spinner.set_message("正在扫描本机 Java…"),
                    &mut |_| {},
                )
                .map_err(anyhow::Error::from)
        })
    };

    match attempt(accept_eula) {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            let needs_eula = err
                .downcast_ref::<LifecycleError>()
                .is_some_and(|e| matches!(e, LifecycleError::EulaRequired));
            if !needs_eula || !printer.is_interactive() {
                return Err(err);
            }
            if !prompts::confirm_yes("需要同意 Mojang EULA（eula=true），是否同意？", true)?
            {
                return Err(anyhow::anyhow!("未同意 EULA，已中止"));
            }
            attempt(true)
        }
    }
}

/// `server start`：启动并报告 supervisor PID
pub(crate) fn start(name: &str, accept_eula: bool, printer: Printer) -> Result<()> {
    let service = lifecycle_service()?;
    let supervisor_exe = std::env::current_exe().context("定位 mcsup 可执行文件")?;
    let outcome = run_start(&service, name, accept_eula, &supervisor_exe, printer)?;
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
        format!("正在停止 {name}…（宽限 {grace}s）"),
        |_| service.stop(name, spec).map_err(anyhow::Error::from),
    )?;

    let (human, payload) = match outcome {
        StopOutcome::Stopped { elapsed } => (
            format!("已停止 {name}（耗时 {}s）", elapsed.as_secs()),
            StopJson {
                name,
                outcome: "stopped",
                elapsed_secs: Some(elapsed.as_secs()),
            },
        ),
        StopOutcome::AlreadyStopped => (
            format!("{name} 未在运行"),
            StopJson {
                name,
                outcome: "already_stopped",
                elapsed_secs: None,
            },
        ),
        StopOutcome::TimedOut { waited } => {
            return Err(anyhow::anyhow!(
                "停机超时（已等待 {}s），可加 --force 强杀",
                waited.as_secs()
            ));
        }
    };
    printer.print_outcome(&human, &payload)
}

/// `server restart`：先停（超时中止），再走与 start 相同的启动路径
pub(crate) fn restart(
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
        format!("正在停止 {name}…（宽限 {grace}s）"),
        |_| service.stop(name, spec).map_err(anyhow::Error::from),
    )?;
    match stop_outcome {
        StopOutcome::Stopped { .. } | StopOutcome::AlreadyStopped => {}
        StopOutcome::TimedOut { waited } => {
            return Err(anyhow::anyhow!(
                "停机超时（已等待 {}s），重启中止；可加 --force 强杀",
                waited.as_secs()
            ));
        }
    }

    let outcome = run_start(&service, name, accept_eula, &supervisor_exe, printer)?;
    let payload = StartJson {
        name,
        supervisor_pid: outcome.supervisor_pid,
    };
    printer.print_outcome(
        &format!("已重启 {name}（supervisor PID {}）", payload.supervisor_pid),
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
        let mut table = Table::new();
        table.set_content_arrangement(comfy_table::ContentArrangement::Dynamic);
        table.set_header(["项", "值"]);
        table.add_row(["名称", payload.name]);
        table.add_row(["版本", payload.version]);
        table.add_row(["变体", payload.variant]);
        table.add_row(["状态", payload.status]);
        table.add_row([
            comfy_table::Cell::new("进程"),
            comfy_table::Cell::new(
                payload
                    .pid
                    .map_or_else(|| "-".to_owned(), |p| p.to_string()),
            ),
        ]);
        table.add_row([
            comfy_table::Cell::new("运行时长"),
            comfy_table::Cell::new(fmt_uptime(payload.uptime_secs)),
        ]);
        table.add_row([
            comfy_table::Cell::new("退出码"),
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

/// `server console`：stdin 注入 + 实时输出，Ctrl+D 分离（服务器继续运行）
///
/// 注入失败回显走 stderr：stdout 只留服务器输出。
#[allow(clippy::print_stderr)]
pub(crate) fn console(name: &str, printer: Printer) -> Result<()> {
    if !printer.is_interactive() {
        return Err(AppDiagnostic::usage(
            "server console 需要交互终端",
            Some("非交互下用 server send 注入单条命令、server logs 查看输出".to_owned()),
        )
        .into());
    }
    let service = lifecycle_service()?;
    let report = service.describe(name)?;
    if !report.status.as_ref().is_some_and(|s| s.status.is_alive()) {
        return Err(LifecycleError::NotRunning.into());
    }

    printer.print_section(&format!(
        "已附加 {name} 控制台（Ctrl+D 分离，服务器继续运行）"
    ));
    let mut watch = service.log_watch(name)?;
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
                    Ok(0) | Err(_) => break, // Ctrl+D 或读失败：只分离，不带走进程
                    Ok(_) => {
                        let trimmed = line.trim_end();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Err(err) = service.send_line(&name, trimmed) {
                            eprintln!("注入失败: {err}");
                            if matches!(err, LifecycleError::NotRunning) {
                                break;
                            }
                        }
                    }
                }
            }
            done.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }

    while !done.load(std::sync::atomic::Ordering::Relaxed) {
        for line in watch.read_new()? {
            print_raw_line(&line);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // 收尾：线程退出前最后一段输出
    for line in watch.read_new()? {
        print_raw_line(&line);
    }
    printer.print_section("已分离（服务器继续运行）");
    Ok(())
}

/// `server send`：单条命令注入 stdin
pub(crate) fn send(name: &str, command: &str, printer: Printer) -> Result<()> {
    let service = lifecycle_service()?;
    service.send_line(name, command)?;
    printer.print_outcome(&format!("已注入 {command}"), &SendJson { name, command })
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
        let payload = serde_json::json!({ "line": line });
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
