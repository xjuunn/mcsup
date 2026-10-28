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
                    // 目标可能在远端：缺 Java 时要装到「跑服务器的那台」，装在本机没用
                    let remote = super::origin::remote_target(name, printer.lang())?;
                    let host = remote
                        .as_ref()
                        .map_or_else(|| "本机".to_owned(), |t| t.member.name.to_string());
                    let prompt_text = match missing_major {
                        Some(major) => format!("{host} 缺少 Java {major}，是否现在安装？"),
                        None => format!("{host} 未发现任何 Java 运行时，是否现在安装？"),
                    };
                    if !prompts::confirm_or_yes(printer, &prompt_text, true)? {
                        return Err(anyhow::anyhow!("缺少所需 Java 版本，已中止启动"));
                    }
                    // 安装缺的版本；NoJava 无从点名，装当前 LTS 21 作为起步
                    let major_str = missing_major.map_or("21".to_string(), |m| m.to_string());
                    match &remote {
                        Some(target) => {
                            crate::commands::java::install_on(
                                &mcsup::port::Origin::Remote(target.member.name.clone()),
                                Some(&major_str),
                                false,
                                None,
                                printer,
                            )?;
                        }
                        None => {
                            crate::commands::java::install_with_outcome(
                                Some(&major_str),
                                false,
                                None,
                                printer,
                            )
                            .await?;
                        }
                    }
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

/// 构造停机规格：`--grace` 超过领域上限时按用法错误（2）拒绝
///
/// 上限在 [`mcsup::domain::limits::MAX_STOP_GRACE`]；入口只负责映射错误。
/// 静默夹紧会让用户以为「宽限 100 年」真的生效了。
fn stop_spec(grace: u64, force: bool, lang: crate::locale::Lang) -> Result<StopSpec> {
    let max = mcsup::domain::limits::MAX_STOP_GRACE.as_secs();
    if grace > max {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("--grace 最大 {max}s，收到 {grace}"),
                format!("--grace must be at most {max}s, got {grace}"),
            ),
            Some(crate::i18n::pick(lang, "默认 30s", "the default is 30s")),
        )
        .into());
    }
    Ok(StopSpec {
        grace: Duration::from_secs(grace),
        force,
    })
}

/// `server stop`：优雅停机（`--grace` 到期且 `--force` 时强杀）
pub(crate) fn stop(name: &str, grace: u64, force: bool, printer: Printer) -> Result<()> {
    let service = lifecycle_service()?;
    let spec = stop_spec(grace, force, printer.lang())?;
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

    let spec = stop_spec(grace, force, printer.lang())?;
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
#[allow(clippy::print_stderr, clippy::too_many_lines)]
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
    let lang = printer.lang();
    // 远端服务器的 `console.log` 在对端：日志走长轮询帧（本地 `LogWatch`
    // 读的永远是本机文件），注入与探活也按对端 **id** 走帧——名字在本地与
    // 远端重名时，按名字回落到 service 会悄悄操作到本地那台。
    let remote = super::origin::remote_target(name, lang)?;
    let remote_client = remote
        .as_ref()
        .map(|target| transport_client(&target.member).map(Arc::new))
        .transpose()?;
    let alive = match (&remote, &remote_client) {
        (Some(target), Some(client)) => remote_alive(client, &target.server_id, lang)?,
        _ => service
            .describe(name)?
            .status
            .as_ref()
            .is_some_and(|s| s.status.is_alive()),
    };
    if !alive {
        return Err(LifecycleError::NotRunning.into());
    }

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

    // 本地先取历史 + 增量读取器（同一快照为界，不重不漏），再回放，避免附加瞬间丢行
    let mut watch = None;
    if remote.is_none() {
        let (history, opened) = service.log_watch_tail(name, crate::cli::LOG_TAIL_DEFAULT)?;
        for line in &history {
            print_raw_line(line);
        }
        watch = Some(opened);
    }

    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let injector = match (&remote, &remote_client) {
        (Some(target), Some(client)) => Inject::Remote {
            client: Arc::clone(client),
            server_id: target.server_id.clone(),
        },
        _ => Inject::Local {
            service: Arc::clone(&service),
            name: name.to_owned(),
        },
    };
    {
        let done = Arc::clone(&done);
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
                            && let Some((message, keep_going)) = injector.send(trimmed)
                        {
                            eprintln!("{message}");
                            if !keep_going {
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

    let exited = if let (Some(target), Some(client)) = (&remote, &remote_client) {
        wait_console_exit_remote(client, &target.server_id, name, &done, lang)?
    } else if let Some(mut watcher) = watch {
        wait_console_exit(&service, name, &mut watcher, &done)?
    } else {
        // 本地分支在附加前必定打开 watcher；走到这里说明分叉有偏差，如实报错
        return Err(anyhow::anyhow!(
            "{}",
            crate::i18n::pick(
                lang,
                "控制台日志通道未打开",
                "console log watcher was not opened"
            )
        ));
    };
    report_console_exit(name, exited, printer);
    Ok(())
}

/// 控制台注入通道
///
/// 远端按服务器 **id** 注入而非名字：同名本地服务器不会被误注入。
enum Inject {
    Local {
        service: Arc<ServerLifecycleService>,
        name: String,
    },
    Remote {
        client: Arc<mcsup::transport::Client>,
        server_id: String,
    },
}

impl Inject {
    /// 注入一行；成功返回 `None`，失败返回（文案，是否还能继续）
    ///
    /// 远端不在这里判断「服务器是否已退出」：主循环每两轮探活一次，由它
    /// 统一收尾；传输抖动只提示、不结束附加。
    fn send(&self, line: &str) -> Option<(String, bool)> {
        let lang = crate::locale::current();
        match self {
            Self::Local { service, name } => service.send_line(name, line).err().map(|err| {
                let keep_going = !matches!(err, LifecycleError::NotRunning);
                (
                    crate::i18n::pick(
                        lang,
                        format!("注入失败: {err}"),
                        format!("injection failed: {err}"),
                    ),
                    keep_going,
                )
            }),
            Self::Remote { client, server_id } => client
                .call(&mcsup::protocol::remote::Request::SendLine {
                    server_id: server_id.clone(),
                    line: line.to_owned(),
                })
                .err()
                .map(|err| {
                    (
                        crate::i18n::pick(
                            lang,
                            format!("在远端注入失败: {err}"),
                            format!("remote injection failed: {err}"),
                        ),
                        true,
                    )
                }),
        }
    }
}

/// 按 id 查对端服务器是否还在运行
fn remote_alive(
    client: &mcsup::transport::Client,
    server_id: &str,
    lang: crate::locale::Lang,
) -> Result<bool> {
    let response = client
        .call(&mcsup::protocol::remote::Request::StatusMany {
            ids: vec![server_id.to_owned()],
        })
        .with_context(|| {
            crate::i18n::pick(
                lang,
                "查询远端服务器状态失败",
                "failed to query the remote server status",
            )
        })?;
    let mcsup::protocol::remote::Response::Statuses { items } = response else {
        return Err(anyhow::anyhow!(
            "{}",
            crate::i18n::pick(
                lang,
                "对端返回了非预期帧",
                "the peer returned an unexpected frame"
            )
        ));
    };
    Ok(items
        .first()
        .and_then(|entry| entry.status.clone())
        .and_then(|wire| mcsup::port::SupervisorStatus::try_from(wire).ok())
        .is_some_and(|status| status.status.is_alive()))
}

/// 远端控制台主循环：长轮询取增量 + 周期性探活
///
/// 首帧带 `tail` 回放历史，之后只取增量；探活按 id 发 `StatusMany`，
/// 服务器自行退出时结束附加。
fn wait_console_exit_remote(
    client: &mcsup::transport::Client,
    server_id: &str,
    name: &str,
    done: &std::sync::atomic::AtomicBool,
    lang: crate::locale::Lang,
) -> Result<bool> {
    let mut lines = crate::cli::LOG_TAIL_DEFAULT;
    let mut exited = false;
    let mut ticks = 0u32;
    while !done.load(std::sync::atomic::Ordering::Relaxed) {
        let response = client
            .call(&mcsup::protocol::remote::Request::LogsFollow {
                server_id: server_id.to_owned(),
                lines,
                timeout_ms: FOLLOW_WAIT_MS,
            })
            .with_context(|| {
                crate::i18n::pick(
                    lang,
                    format!("跟随 {name} 的日志失败"),
                    format!("failed to follow the logs of {name}"),
                )
            })?;
        lines = 0;
        let mcsup::protocol::remote::Response::LogLines { lines: batch } = response else {
            return Err(anyhow::anyhow!(
                "{}",
                crate::i18n::pick(
                    lang,
                    "对端返回了非预期帧",
                    "the peer returned an unexpected frame"
                )
            ));
        };
        for line in &batch {
            print_raw_line(line);
        }
        // 长轮询每轮最多等 1.5s，探活每两轮一次：不必跟读日志同频
        ticks += 1;
        if ticks.is_multiple_of(2) && !remote_alive(client, server_id, lang)? {
            exited = true;
            break;
        }
    }
    Ok(exited)
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
    // 远端日志：历史 + 增量都走长轮询帧，本地路径不感知
    if let Some(outcome) = logs_remote(name, tail, follow, printer)? {
        return Ok(outcome);
    }
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

/// 远端日志：历史一次性拉回；`--follow` 用长轮询循环取增量
///
/// 返回 `Ok(Some(()))` 表示目标在远端且已处理完毕；`Ok(None)` 表示本地目标，
/// 调用方继续走本地路径。轮询间隔由对端的等待时长决定（无新行最多等
/// [`FOLLOW_WAIT_MS`]），断线即失败——用户 Ctrl+C 或 agent 重启都会结束跟随。
///
/// # Errors
///
/// 服务器不存在（3）、远端不可达（5）或未授权（6）时返回错误
fn logs_remote(name: &str, tail: usize, follow: bool, printer: Printer) -> Result<Option<()>> {
    let lang = printer.lang();
    let Some(target) = super::origin::remote_target(name, lang)? else {
        return Ok(None);
    };
    let server_id = target.server_id.clone();

    // 历史：一次 LogsFollow 带 tail；增量：循环带 0
    let client = transport_client(&target.member)?;
    let mut request = mcsup::protocol::remote::Request::LogsFollow {
        server_id: server_id.clone(),
        lines: tail,
        timeout_ms: FOLLOW_WAIT_MS,
    };
    let mut printed_history = false;
    loop {
        let response = client.call(&request).with_context(|| {
            crate::i18n::pick(
                lang,
                format!("跟随 {name} 的日志失败"),
                format!("failed to follow the logs of {name}"),
            )
        })?;
        let mcsup::protocol::remote::Response::LogLines { lines } = response else {
            return Err(anyhow::anyhow!(
                "{}",
                crate::i18n::pick(
                    lang,
                    "对端返回了非预期帧",
                    "the peer returned an unexpected frame"
                )
            ));
        };
        if !printer.is_human() && !printed_history {
            // JSONL 契约：--follow 的每一行都是一个对象；历史也逐行输出
            printed_history = true;
        }
        for line in &lines {
            print_stream_line(printer, line)?;
        }
        if !follow {
            return Ok(Some(()));
        }
        request = mcsup::protocol::remote::Request::LogsFollow {
            server_id: server_id.clone(),
            lines: 0,
            timeout_ms: FOLLOW_WAIT_MS,
        };
    }
}

/// 远端长轮询的单次等待上限（毫秒）：太短会高频打帧，太长 Ctrl+C 延迟高
const FOLLOW_WAIT_MS: u64 = 1_500;

/// 用一条远端成员构造已认证的客户端
fn transport_client(
    member: &mcsup::backend::remote::RemoteMember,
) -> Result<mcsup::transport::Client> {
    let cfg = crate::config::ConfigFile::locate(None).load()?;
    let record = cfg
        .remote(&member.name)
        .ok_or_else(|| anyhow::anyhow!("remote {} is not configured", member.name))?;
    let fingerprint = mcsup::transport::Fingerprint::parse(
        record
            .fingerprint
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("remote {} has no fingerprint", member.name))?,
    )?;
    let credential = {
        use mcsup::auth::CredentialStore as _;
        bootstrap::open_credential_store()
            .get(member.name.as_str())
            .map_err(|err| anyhow::anyhow!("read credential for {}: {err}", member.name))?
    };
    Ok(
        mcsup::transport::Client::new(member.servers_link().target(), fingerprint)
            .with_token(credential.token),
    )
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
    use super::{fmt_uptime, stop_spec};
    use crate::locale::Lang;
    use std::time::Duration;

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

    #[test]
    fn stop_spec_rejects_grace_above_domain_max() {
        let too_long = mcsup::domain::limits::MAX_STOP_GRACE.as_secs() + 1;
        let err = stop_spec(too_long, true, Lang::En).expect_err("out of range");
        let diagnostic = err
            .downcast_ref::<crate::diagnostic::AppDiagnostic>()
            .expect("usage diagnostic");
        assert_eq!(diagnostic.exit_code, 2, "超过上限是用法错误: {err}");
    }

    #[test]
    fn stop_spec_keeps_in_range_grace_and_force() {
        let spec = stop_spec(30, true, Lang::Zh).expect("in range");
        assert_eq!(spec.grace, Duration::from_secs(30));
        assert!(spec.force);
    }
}
