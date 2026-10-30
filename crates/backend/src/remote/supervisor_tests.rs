//! `RemoteSupervisor` 的测试：真 TLS + supervisor 垫片
//!
//! 垫片（`testkit::mcsup_agent_shim`）维护一张「运行表」：`Start` 登记、
//! `Stop` 注销、`StatusMany` 按表回帧。镜像测试只验证客户端解帧与组装；
//! agent 真实分发由 `crates/mcsup` 集成测试负责。

use super::supervisor::RemoteSupervisor;
use crate::remote::testkit::PairingHarness;
use domain::server::meta::ServerId;
use port::{
    ServerRef, StopOutcome, StopSpec, SupervisorError, SupervisorPort, SupervisorStartRequest,
    SupervisorStatus,
};
use std::time::Duration;

fn server_id(text: &str) -> ServerId {
    ServerId::parse(text).expect("id")
}

fn server_ref(id: &ServerId) -> ServerRef<'_> {
    ServerRef::remote(id)
}

fn start_request(id: &ServerId) -> SupervisorStartRequest {
    SupervisorStartRequest {
        server_id: id.clone(),
        // 远端实现忽略这些本地执行细节；给合法值仅为满足构造
        server_dir: std::path::PathBuf::from("/srv/mc/web"),
        java_bin: std::path::PathBuf::from("/usr/bin/java"),
        launch: domain::server::launch::LaunchSpec::jar("server.jar"),
        ram_mib: Some(2048),
        supervisor_exe: std::path::PathBuf::from("mcsup"),
        supervisor_args: vec![std::ffi::OsString::from("__supervise")],
        accept_eula: true,
    }
}

#[test]
fn start_registers_and_status_reports_running() {
    let harness = PairingHarness::spawn();
    let supervisor = RemoteSupervisor::new(harness.link());
    let id = server_id("550e8400-e29b-41d4-a716-446655440001");

    // 未启动：Ok(None)
    let status = supervisor.status(&server_ref(&id)).expect("status");
    assert_eq!(status, None);

    let outcome = supervisor.start(start_request(&id)).expect("start");
    assert_eq!(outcome.supervisor_pid, 4242);

    let status = supervisor.status(&server_ref(&id)).expect("status");
    let status = status.expect("running");
    assert_eq!(status.status, port::ServerStatus::Running);
    assert_eq!(status.pid, Some(4242));
}

#[test]
fn start_twice_is_rejected_as_busy() {
    let harness = PairingHarness::spawn();
    let supervisor = RemoteSupervisor::new(harness.link());
    let id = server_id("550e8400-e29b-41d4-a716-446655440001");
    supervisor.start(start_request(&id)).expect("first start");
    let err = supervisor.start(start_request(&id)).expect_err("second");
    assert!(matches!(err, SupervisorError::Remote { .. }), "{err:?}");
}

#[test]
fn status_many_is_one_roundtrip_and_order_preserved() {
    let harness = PairingHarness::spawn();
    let supervisor = RemoteSupervisor::new(harness.link());
    let a = server_id("550e8400-e29b-41d4-a716-446655440001");
    let b = server_id("550e8400-e29b-41d4-a716-446655440002");
    supervisor.start(start_request(&a)).expect("start a");

    let results = supervisor.status_many(&[server_ref(&a), server_ref(&b)]);
    assert_eq!(results.len(), 2);
    let a_status = results[0].as_ref().expect("a ok");
    assert!(a_status.is_some(), "a is running");
    let b_status = results[1].as_ref().expect("b ok");
    assert_eq!(*b_status, None, "b is not running");

    // 空输入 → 空输出
    assert!(supervisor.status_many(&[]).is_empty());
}

#[test]
fn stop_is_idempotent() {
    let harness = PairingHarness::spawn();
    let supervisor = RemoteSupervisor::new(harness.link());
    let id = server_id("550e8400-e29b-41d4-a716-446655440001");
    let spec = StopSpec {
        grace: Duration::from_secs(5),
        force: false,
    };

    // 未运行：AlreadyStopped（幂等）
    let outcome = supervisor
        .stop(&server_ref(&id), spec.clone())
        .expect("stop");
    assert!(matches!(outcome, StopOutcome::AlreadyStopped));

    supervisor.start(start_request(&id)).expect("start");
    let outcome = supervisor.stop(&server_ref(&id), spec).expect("stop");
    let StopOutcome::Stopped { elapsed } = outcome else {
        panic!("expected Stopped, got {outcome:?}");
    };
    assert!(elapsed <= Duration::from_secs(5));

    // 停后再查：不在运行
    let status = supervisor.status(&server_ref(&id)).expect("status");
    assert_eq!(status, None);
}

#[test]
fn send_line_fails_when_not_running() {
    let harness = PairingHarness::spawn();
    let supervisor = RemoteSupervisor::new(harness.link());
    let id = server_id("550e8400-e29b-41d4-a716-446655440001");
    let err = supervisor
        .send_line(&server_ref(&id), "say hi")
        .expect_err("not running");
    assert!(
        matches!(
            err,
            SupervisorError::Remote {
                kind: port::RemoteFailureKind::NotFound,
                ..
            }
        ),
        "{err:?}"
    );

    supervisor.start(start_request(&id)).expect("start");
    supervisor
        .send_line(&server_ref(&id), "say hi")
        .expect("send after start");
}

#[test]
fn logs_tail_returns_lines() {
    let harness = PairingHarness::spawn();
    let supervisor = RemoteSupervisor::new(harness.link());
    let id = server_id("550e8400-e29b-41d4-a716-446655440001");
    supervisor.start(start_request(&id)).expect("start");
    // 垫片不校验运行态，直接回固定行
    let lines = harness_tail(&supervisor, &id, 3);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "log line 0");
}

fn harness_tail(supervisor: &RemoteSupervisor, id: &ServerId, lines: usize) -> Vec<String> {
    use protocol::remote::{Request, Response};
    let response = supervisor
        .call_for_test(&Request::LogsTail {
            server_id: id.as_str().to_owned(),
            lines,
        })
        .expect("logs");
    let Response::LogLines { lines } = response else {
        panic!("expected LogLines");
    };
    lines
}

/// 领域状态在往返后保持不变（wire 层锁过的形状，这里锁 client 侧组装）
#[test]
fn status_roundtrip_preserves_exit_code() {
    let harness = PairingHarness::spawn();
    let supervisor = RemoteSupervisor::new(harness.link());
    let id = server_id("550e8400-e29b-41d4-a716-446655440001");
    supervisor.start(start_request(&id)).expect("start");
    let status = supervisor.status(&server_ref(&id)).expect("some");
    let SupervisorStatus {
        status,
        pid,
        exit_code,
        ..
    } = status.expect("status");
    assert_eq!(status, port::ServerStatus::Running);
    assert_eq!(pid, Some(4242));
    assert_eq!(exit_code, None);
}
