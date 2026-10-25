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
    ///
    /// 启动方式按 [`domain::server::launch::LaunchSpec`] 分流：`Jar` 传
    /// `--jar <相对路径>`，`Args` 依次传 `--arg-file <相对路径>`。路径保持
    /// **相对服务器目录**（supervisor 以 `--dir` 为 cwd，由它拼绝对），与
    /// `MinecraftLaunch` 的约定一致。
    fn supervisor_command(request: &SupervisorStartRequest) -> Command {
        let mut command = Command::new(&request.supervisor_exe);
        command
            .arg("__supervise")
            .arg("--dir")
            .arg(&request.server_dir)
            .arg("--java")
            .arg(&request.java_bin);
        match &request.launch {
            domain::server::launch::LaunchSpec::Jar { path } => {
                command.arg("--jar").arg(path);
            }
            domain::server::launch::LaunchSpec::Args { files } => {
                for file in files {
                    command.arg("--arg-file").arg(file);
                }
            }
            // `LaunchSpec` 是 `#[non_exhaustive]`：未知形态不传启动参数，
            // supervisor 会以「无启动入口」失败——不静默跑错命令
            _ => {}
        }
        if let Some(ram_mib) = request.ram_mib {
            command.arg("--ram").arg(ram_mib.to_string());
        }
        command
    }
}

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

        // 默认 supervisor 经 `__supervise` 子命令拉起：exe 缺失或不是
        // mcsup CLI 时，把失败提前到 spawn 前并指名原因，而不是让用户
        // 5 秒超时后看到一句难懂的 Unreachable
        if !request.supervisor_exe.is_file() {
            return Err(SupervisorError::Spawn(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "supervisor exe not found: {}（默认 LocalSupervisor 需要传入 \
                     mcsup CLI 的路径；宿主启动器应注入自己的 SupervisorPort）",
                    request.supervisor_exe.display()
                ),
            )));
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
        crate::process::detach(&mut command);

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
