//! Local 实现：以子进程拉起 supervisor，经本地 socket 交互
use crate::client;
use crate::endpoint;
use domain::server::layout::run_dir;
use port::{StopOutcome, StopSpec, SupervisorError, SupervisorPort};
use port::{SupervisorStartOutcome, SupervisorStartRequest, SupervisorStatus};
use protocol::{Request, Response, StopResult};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// 拉起到 IPC 就绪的等待上限
const READY_WAIT: Duration = Duration::from_secs(5);
/// 清场时等待旧 supervisor 退出的上限
const GONE_WAIT: Duration = Duration::from_secs(1);

/// 本机 supervisor 适配：只回答「在哪里执行」，判定在 domain / service
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalSupervisor;

impl LocalSupervisor {
    /// 组装 `mcsup __supervise ...` 启动命令
    fn supervisor_command(request: &SupervisorStartRequest) -> Command {
        let mut command = Command::new(&request.supervisor_exe);
        command
            .arg("__supervise")
            .arg("--dir")
            .arg(&request.server_dir)
            .arg("--java")
            .arg(&request.java_bin)
            .arg("--jar")
            .arg(&request.jar_path);
        if let Some(ram_mib) = request.ram_mib {
            command.arg("--ram").arg(ram_mib.to_string());
        }
        command
    }
}

/// 脱离终端：Unix 新进程组，Windows 分离进程 + 新进程组
///
/// 保证 CLI 退出 / 终端关闭不带走 supervisor；子进程随之脱离前台信号组。
#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut Command) {}

impl SupervisorPort for LocalSupervisor {
    fn start(
        &self,
        request: SupervisorStartRequest,
    ) -> Result<SupervisorStartOutcome, SupervisorError> {
        let dir = request.server_dir.as_path();

        // 清场：活着 → 拒绝重复启动；空载留守 → 通知退出并等它释放端点
        if let Some(status) = client::probe_status(dir)? {
            if status.status.is_alive() {
                return Err(SupervisorError::AlreadyRunning);
            }
            let _ = client::rpc(dir, Request::Shutdown, Duration::from_secs(1));
            client::wait_gone(dir, GONE_WAIT);
        }

        std::fs::create_dir_all(run_dir(dir))?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(endpoint::supervisor_log_path(dir))?;

        let mut command = Self::supervisor_command(&request);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));
        detach(&mut command);

        let child = command.spawn().map_err(SupervisorError::Spawn)?;
        let supervisor_pid = child.id();
        // 不等待：start 返回后 CLI 即将退出，supervisor 由 init 收养
        drop(child);

        client::wait_up(dir, READY_WAIT).map_err(|err| {
            SupervisorError::Unreachable(format!(
                "{err}（详见 {}）",
                endpoint::supervisor_log_path(dir).display()
            ))
        })?;
        Ok(SupervisorStartOutcome { supervisor_pid })
    }

    fn stop(&self, server_dir: &Path, spec: StopSpec) -> Result<StopOutcome, SupervisorError> {
        let timeout = spec.grace + Duration::from_secs(10);
        let response = client::rpc(
            server_dir,
            Request::Stop {
                grace_secs: spec.grace.as_secs(),
                force: spec.force,
            },
            timeout,
        )?;
        match client::unwrap_ok(response)? {
            Response::Stop {
                result,
                elapsed_secs,
            } => {
                let elapsed = Duration::from_secs(elapsed_secs);
                match result.as_str() {
                    StopResult::STOPPED => Ok(StopOutcome::Stopped { elapsed }),
                    StopResult::ALREADY_STOPPED => Ok(StopOutcome::AlreadyStopped),
                    StopResult::TIMED_OUT => Ok(StopOutcome::TimedOut { waited: elapsed }),
                    other => Err(SupervisorError::Protocol(format!(
                        "unknown stop result: {other}"
                    ))),
                }
            }
            other => Err(SupervisorError::Protocol(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    fn status(&self, server_dir: &Path) -> Result<Option<SupervisorStatus>, SupervisorError> {
        client::probe_status(server_dir)
    }

    fn send_line(&self, server_dir: &Path, line: &str) -> Result<(), SupervisorError> {
        let response = client::rpc(
            server_dir,
            Request::Send {
                line: line.to_owned(),
            },
            Duration::from_secs(5),
        )?;
        client::unwrap_ok(response)?;
        Ok(())
    }

    fn shutdown(&self, server_dir: &Path) -> Result<(), SupervisorError> {
        let response = client::rpc(server_dir, Request::Shutdown, Duration::from_secs(3))?;
        client::unwrap_ok(response)?;
        Ok(())
    }
}
