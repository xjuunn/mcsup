//! 可控的 `SupervisorPort`：记录每一行控制台命令，可注入发送失败
//!
//! 备份最要紧的不变量是「`save-off` 之后必须发 `save-on`」，且**任何**退出
//! 路径都要发。要断言它，实现必须能做到两件事：
//!
//! 1. 记录命令的完整顺序（而不只是「发过没有」）
//! 2. 让某一次 `send_line` 失败（扫描失败、记录写入失败之外的第三种故障）
//!
//! 默认不认为服务器在运行：跑没跑由 [`CommandLog::running`] /
//! [`CommandLog::stopped`] 显式声明，测试里不允许出现「隐含的默认状态」。

use domain::server::status::ServerStatus;
use port::{
    ServerRef, StopOutcome, StopSpec, SupervisorError, SupervisorPort, SupervisorStartOutcome,
    SupervisorStartRequest, SupervisorStatus,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 一次调用记录到的服务器身份
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerCall {
    /// 服务器身份（跨机器键）
    pub id: String,
    /// 本地目录；远端场景为 `None`
    pub dir: Option<PathBuf>,
}

impl ServerCall {
    fn of(server: &ServerRef<'_>) -> Self {
        Self {
            id: server.id.as_str().to_owned(),
            dir: server.dir.map(Path::to_path_buf),
        }
    }
}

/// 命令记录器：与 [`FakeSupervisor`] 共享，测试通过它读记录
#[derive(Debug, Clone, Default)]
pub struct CommandLog {
    inner: Arc<Mutex<LogInner>>,
}

#[derive(Debug, Default)]
struct LogInner {
    /// 按发送顺序记录（含失败的那次：失败也发生在「尝试发送」之后）
    sent: Vec<String>,
    /// supervisor 是否报告服务器存活
    alive: bool,
    /// 第 N 次（1 基）`send_line` 失败；`None` = 从不失败
    fail_on: Option<usize>,
    send_attempts: usize,
    /// 每次调用给出的服务器身份，便于断言「命令发给了对的服务器」
    dirs: Vec<ServerCall>,
    /// 第 N 次（1 基）`status` 失败；`None` = 从不失败
    status_fail_on: Option<usize>,
    status_attempts: usize,
}

impl CommandLog {
    /// 服务器不运行
    #[must_use]
    pub fn stopped() -> Self {
        Self::default()
    }

    /// 服务器运行中
    #[must_use]
    pub fn running() -> Self {
        let log = Self::default();
        log.inner.lock().expect("lock").alive = true;
        log
    }

    /// 第 `attempt` 次（1 基）`send_line` 失败
    #[must_use]
    pub fn failing_send_on(self, attempt: usize) -> Self {
        self.inner.lock().expect("lock").fail_on = Some(attempt);
        self
    }

    /// 第 `attempt` 次（1 基）`status` 失败
    #[must_use]
    pub fn failing_status_on(self, attempt: usize) -> Self {
        self.inner.lock().expect("lock").status_fail_on = Some(attempt);
        self
    }

    /// 全部尝试发送的命令行，按顺序（失败的也在内）
    #[must_use]
    pub fn sent(&self) -> Vec<String> {
        self.inner.lock().expect("lock").sent.clone()
    }

    /// 实际成功送达的行（失败的尝试不算）
    #[must_use]
    pub fn delivered(&self) -> Vec<String> {
        let inner = self.inner.lock().expect("lock");
        inner
            .sent
            .iter()
            .enumerate()
            .filter(|(index, _)| inner.fail_on != Some(index + 1))
            .map(|(_, line)| line.clone())
            .collect()
    }

    /// 每次调用给出的服务器身份
    #[must_use]
    pub fn calls(&self) -> Vec<ServerCall> {
        self.inner.lock().expect("lock").dirs.clone()
    }

    /// 清空记录（用于「分阶段断言」）
    pub fn clear(&self) {
        let mut inner = self.inner.lock().expect("lock");
        inner.sent.clear();
        inner.dirs.clear();
    }
}

/// 实现 [`SupervisorPort`] 的测试用 supervisor
#[derive(Debug, Clone)]
pub struct FakeSupervisor {
    log: CommandLog,
}

impl FakeSupervisor {
    /// 以给定记录器构造
    #[must_use]
    pub fn new(log: CommandLog) -> Self {
        Self { log }
    }

    /// 服务器不运行
    #[must_use]
    pub fn stopped() -> Self {
        Self::new(CommandLog::stopped())
    }

    /// 服务器运行中
    #[must_use]
    pub fn running() -> Self {
        Self::new(CommandLog::running())
    }

    /// 记录器句柄
    #[must_use]
    pub fn log(&self) -> &CommandLog {
        &self.log
    }
}

fn unreachable(what: &str) -> SupervisorError {
    SupervisorError::Unreachable(format!("fake supervisor: {what} not supported in test"))
}

impl SupervisorPort for FakeSupervisor {
    fn start(
        &self,
        _request: SupervisorStartRequest,
    ) -> Result<SupervisorStartOutcome, SupervisorError> {
        // 测试里不允许真拉进程：需要「运行中」就构造 CommandLog::running()
        Err(unreachable("start"))
    }

    fn stop(
        &self,
        _server: &ServerRef<'_>,
        _spec: StopSpec,
    ) -> Result<StopOutcome, SupervisorError> {
        Err(unreachable("stop"))
    }

    fn status(&self, server: &ServerRef<'_>) -> Result<Option<SupervisorStatus>, SupervisorError> {
        let mut inner = self.log.inner.lock().expect("lock");
        inner.dirs.push(ServerCall::of(server));
        inner.status_attempts += 1;
        let attempt = inner.status_attempts;
        let fail_on = inner.status_fail_on;
        let alive = inner.alive;
        drop(inner);

        if fail_on == Some(attempt) {
            return Err(unreachable("status injected failure"));
        }
        if !alive {
            return Ok(None);
        }
        Ok(Some(SupervisorStatus {
            status: ServerStatus::Running,
            pid: Some(u32::MAX),
            uptime: Some(std::time::Duration::from_secs(0)),
            exit_code: None,
        }))
    }

    fn send_line(&self, server: &ServerRef<'_>, line: &str) -> Result<(), SupervisorError> {
        let mut inner = self.log.inner.lock().expect("lock");
        inner.dirs.push(ServerCall::of(server));
        inner.send_attempts += 1;
        let attempt = inner.send_attempts;
        inner.sent.push(line.to_owned());
        let fail_on = inner.fail_on;
        drop(inner);
        if fail_on == Some(attempt) {
            return Err(SupervisorError::Remote {
                kind: "injected".to_owned(),
                message: format!("send_line #{attempt} failed on purpose"),
            });
        }
        Ok(())
    }

    fn shutdown(&self, _server: &ServerRef<'_>) -> Result<(), SupervisorError> {
        Err(unreachable("shutdown"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assert::{Failure, assert_invariant};

    /// 测试用的本地服务器引用：身份用固定 UUID，目录即给定路径
    fn local(dir: &Path) -> (domain::server::meta::ServerId, &Path) {
        (
            domain::server::meta::ServerId::parse("6f1a1d3e-0b7c-4a51-9c8e-2b0f3a4d5e60")
                .expect("id"),
            dir,
        )
    }

    #[test]
    fn stopped_reports_none_and_running_reports_status() {
        let dir = Path::new("/srv/alpha");
        let (id, dir) = local(dir);
        let sref = ServerRef::local(&id, dir);
        let stopped = FakeSupervisor::stopped();
        assert_invariant(stopped.status(&sref).expect("status").is_none(), || {
            Failure::invariant("stopped supervisor 必须报 None")
        });
        let running = FakeSupervisor::running();
        assert_invariant(
            running
                .status(&sref)
                .expect("status")
                .is_some_and(|status| status.status.is_alive()),
            || Failure::invariant("running supervisor 必须报存活"),
        );
    }

    #[test]
    fn sent_preserves_order() {
        let sup = FakeSupervisor::running();
        let (id, dir) = local(Path::new("/srv/alpha"));
        let sref = ServerRef::local(&id, dir);
        sup.send_line(&sref, "save-off").expect("send");
        sup.send_line(&sref, "save-all flush").expect("send");
        sup.send_line(&sref, "save-on").expect("send");
        assert_invariant(
            sup.log().sent() == ["save-off", "save-all flush", "save-on"],
            || Failure::invariant("命令顺序必须原样保留").actual(format!("{:?}", sup.log().sent())),
        );
    }

    /// 注入失败的那一行仍然留痕，但不算成功送达
    #[test]
    fn injected_failure_is_recorded_but_not_delivered() {
        let sup = FakeSupervisor::new(CommandLog::running().failing_send_on(2));
        let (id, dir) = local(Path::new("/srv/alpha"));
        let sref = ServerRef::local(&id, dir);
        sup.send_line(&sref, "save-off").expect("first ok");
        assert_invariant(sup.send_line(&sref, "save-all flush").is_err(), || {
            Failure::invariant("第 2 次 send_line 必须失败")
        });
        assert_invariant(sup.log().sent() == ["save-off", "save-all flush"], || {
            Failure::invariant("失败的尝试也要留痕").actual(format!("{:?}", sup.log().sent()))
        });
        assert_invariant(sup.log().delivered() == ["save-off"], || {
            Failure::invariant("失败行不算送达").actual(format!("{:?}", sup.log().delivered()))
        });
    }

    #[test]
    fn status_failure_is_injectable() {
        let sup = FakeSupervisor::new(CommandLog::stopped().failing_status_on(1));
        let (id, dir) = local(Path::new("/srv/a"));
        let sref = ServerRef::local(&id, dir);
        assert_invariant(sup.status(&sref).is_err(), || {
            Failure::invariant("第 1 次 status 必须失败")
        });
        assert_invariant(sup.status(&sref).is_ok(), || {
            Failure::invariant("注入只作用于指定次数")
        });
    }

    /// 调用记录按身份键留存：远端引用（无目录）也能被断言到
    #[test]
    fn calls_are_recorded_by_identity() {
        let sup = FakeSupervisor::running();
        let (id, _) = local(Path::new("/srv/a"));
        let remote = ServerRef::remote(&id);
        sup.status(&remote).expect("status");
        let calls = sup.log().calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, id.as_str());
        assert!(calls[0].dir.is_none(), "远端引用不该带目录");
    }
}
