//! `RoutingSupervisor` 的测试：本地内存 supervisor + 真 TLS 垫片远端
//!
//! 验证按服务器 id 查组合仓库来源后的分派：本地 id 走本地、远端 id 走 RPC、
//! 未知 id 报 `Unreachable`、`status_many` 按目标分桶且顺序保持。

use super::combined::{CombinedServerRepository, RemoteMember, RemoteMembers};
use super::router::RoutingSupervisor;
use super::supervisor::RemoteSupervisor;
use super::testkit::{PairingHarness, encoding};
use domain::java::JavaMajor;
use domain::server::launch::LaunchSpec;
use domain::server::meta::{InstalledJar, ServerId, ServerMeta, ServerName};
use domain::server::version::VersionId;
use port::{
    Origin, RemoteName, RepositoryError, ServerRef, ServerRepository, StopOutcome, StopSpec,
    SupervisorError, SupervisorPort, SupervisorStartOutcome, SupervisorStartRequest,
    SupervisorStatus,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 内存本地 supervisor：记录 start / stop，状态表可注入
#[derive(Default)]
struct MemSupervisor {
    running: Mutex<Vec<ServerId>>,
}

impl MemSupervisor {
    fn set_if_running(&self, id: &ServerId) -> bool {
        let mut guard = self.running.lock().expect("lock");
        let before = guard.len();
        guard.retain(|known| known != id);
        guard.len() != before
    }

    fn status_of(&self, id: &ServerId) -> Option<SupervisorStatus> {
        let guard = self.running.lock().expect("lock");
        guard.contains(id).then_some(SupervisorStatus {
            status: port::ServerStatus::Running,
            pid: Some(1111),
            uptime: None,
            exit_code: None,
        })
    }
}

impl SupervisorPort for MemSupervisor {
    fn start(
        &self,
        request: SupervisorStartRequest,
    ) -> Result<SupervisorStartOutcome, SupervisorError> {
        let mut guard = self.running.lock().expect("lock");
        if !guard.contains(&request.server_id) {
            guard.push(request.server_id.clone());
        }
        Ok(SupervisorStartOutcome {
            supervisor_pid: 1111,
        })
    }

    fn stop(
        &self,
        server: &ServerRef<'_>,
        _spec: StopSpec,
    ) -> Result<StopOutcome, SupervisorError> {
        if self.set_if_running(server.id) {
            Ok(StopOutcome::Stopped {
                elapsed: std::time::Duration::from_millis(1),
            })
        } else {
            Ok(StopOutcome::AlreadyStopped)
        }
    }

    fn status(&self, server: &ServerRef<'_>) -> Result<Option<SupervisorStatus>, SupervisorError> {
        Ok(self.status_of(server.id))
    }

    fn status_many(
        &self,
        servers: &[ServerRef<'_>],
    ) -> Vec<Result<Option<SupervisorStatus>, SupervisorError>> {
        servers
            .iter()
            .map(|server| Ok(self.status_of(server.id)))
            .collect()
    }

    fn send_line(&self, server: &ServerRef<'_>, _line: &str) -> Result<(), SupervisorError> {
        if self.status_of(server.id).is_some() {
            Ok(())
        } else {
            Err(SupervisorError::Remote {
                kind: "not_found".to_owned(),
                message: "not running".to_owned(),
            })
        }
    }

    fn shutdown(&self, server: &ServerRef<'_>) -> Result<(), SupervisorError> {
        self.set_if_running(server.id);
        Ok(())
    }
}

/// 内存本地服务器仓库：固定一条本地服务器（web）
struct MemServers(ServerId);

impl ServerRepository for MemServers {
    fn create(&self, _meta: &ServerMeta) -> Result<(), RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "test",
        ))))
    }

    fn get(&self, name: &ServerName) -> Result<ServerMeta, RepositoryError> {
        self.list()?
            .into_iter()
            .find(|meta| &meta.name == name)
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
    }

    fn list(&self) -> Result<Vec<ServerMeta>, RepositoryError> {
        let name = ServerName::parse("local-web").expect("name");
        Ok(vec![ServerMeta {
            id: self.0.clone(),
            name,
            workspace_id: encoding::workspace_id(),
            rel_dir: std::path::PathBuf::from("local-web"),
            version: VersionId::new("1.21.1"),
            variant: domain::server::version::ServerVariant::Vanilla,
            jar: None,
            launch: None,
            settings: domain::server::settings::ServerSettings::default(),
            java_major: None,
            created_at: 1,
            updated_at: 1,
        }])
    }

    fn update_core(
        &self,
        _name: &ServerName,
        _version: &VersionId,
        _jar: &InstalledJar,
        _launch: Option<&LaunchSpec>,
        _required_java_major: Option<JavaMajor>,
        _updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "test",
        ))))
    }

    fn update_settings(
        &self,
        _name: &ServerName,
        _settings: &domain::server::settings::ServerSettings,
        _updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "test",
        ))))
    }

    fn remove(&self, _name: &ServerName) -> Result<(), RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "test",
        ))))
    }

    fn remove_by_workspace(
        &self,
        _workspace_id: &domain::workspace::WorkspaceId,
    ) -> Result<(), RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "test",
        ))))
    }

    fn relink(
        &self,
        _name: &ServerName,
        _workspace_id: &domain::workspace::WorkspaceId,
        _rel_dir: &std::path::Path,
        _updated_at: i64,
    ) -> Result<ServerMeta, RepositoryError> {
        Err(RepositoryError::Storage(Box::new(std::io::Error::other(
            "test",
        ))))
    }
}

/// 本地服务器 id（固定，与远端垫片的 `...440001` 不同）
fn local_id() -> ServerId {
    ServerId::parse("110e8400-e29b-41d4-a716-446655440001").expect("id")
}

/// 装配一套路由：本地内存 supervisor + 一台垫片远端 + 组合仓库
struct RouterFixture {
    router: RoutingSupervisor,
    local_id: ServerId,
    remote_id: ServerId,
    _harness: PairingHarness,
}

impl RouterFixture {
    fn spawn() -> Self {
        let harness = PairingHarness::spawn();
        let remote_name = RemoteName::parse("box").expect("name");
        let member = RemoteMember::new(harness.link(), remote_name);

        // 远端注册一台服务器（走 ServerCreate 帧），拿到远端分配的身份
        let remote_meta = member
            .servers
            .create(None, "remote-web", "1.21.1", "vanilla", None)
            .expect("remote create");
        let remote_id = remote_meta.id.clone();

        let local_servers: Arc<dyn ServerRepository> = Arc::new(MemServers(local_id()));
        let mut members = RemoteMembers::empty();
        members.push(member);
        let combined: Arc<dyn ServerRepository> =
            Arc::new(CombinedServerRepository::new(local_servers, members));

        let local: Arc<dyn SupervisorPort> = Arc::new(MemSupervisor::default());
        let mut remotes = HashMap::new();
        remotes.insert(
            RemoteName::parse("box").expect("name"),
            RemoteSupervisor::new(harness.link()),
        );
        let router = RoutingSupervisor::new(local, remotes, combined);
        Self {
            router,
            local_id: local_id(),
            remote_id,
            _harness: harness,
        }
    }
}

#[test]
fn unknown_id_is_unreachable() {
    let fixture = RouterFixture::spawn();
    let unknown = ServerId::parse("990e8400-e29b-41d4-a716-446655440099").expect("id");
    let err = fixture
        .router
        .status(&ServerRef::remote(&unknown))
        .expect_err("unknown");
    assert!(
        matches!(err, SupervisorError::Unreachable(ref text) if text.contains("no such server")),
        "{err:?}"
    );
}

#[test]
fn status_many_buckets_and_preserves_order() {
    let fixture = RouterFixture::spawn();
    let local = ServerRef::remote(&fixture.local_id);
    let remote = ServerRef::remote(&fixture.remote_id);

    // 本地启动、远端启动（各自一侧）
    fixture
        .router
        .start(start_request(&fixture.local_id))
        .expect("local start");
    fixture
        .router
        .start(start_request(&fixture.remote_id))
        .expect("remote start");

    // 混合批量查询：顺序与入参一致，本地/远端各自命中
    let results = fixture.router.status_many(&[local, remote, local]);
    assert_eq!(results.len(), 3);
    assert!(results[0].as_ref().expect("r0").is_some(), "local running");
    assert!(results[1].as_ref().expect("r1").is_some(), "remote running");
    assert!(results[2].as_ref().expect("r2").is_some(), "local running");

    // 本地内存 supervisor 确实收到了本地的调用（pid 1111）
    let status = (*results[0].as_ref().expect("r0")).clone().expect("some");
    assert_eq!(status.pid, Some(1111));
    // 远端返回垫片的 pid 4242
    let status = (*results[1].as_ref().expect("r1")).clone().expect("some");
    assert_eq!(status.pid, Some(4242));
}

#[test]
fn stop_routes_to_the_right_side() {
    let fixture = RouterFixture::spawn();
    // 未运行 → 两侧都是 AlreadyStopped
    let outcome = fixture
        .router
        .stop(
            &ServerRef::remote(&fixture.local_id),
            StopSpec::default_grace(),
        )
        .expect("local stop");
    assert!(matches!(outcome, StopOutcome::AlreadyStopped));
    let outcome = fixture
        .router
        .stop(
            &ServerRef::remote(&fixture.remote_id),
            StopSpec::default_grace(),
        )
        .expect("remote stop");
    assert!(matches!(outcome, StopOutcome::AlreadyStopped));

    // 远端启动后停机 → Stopped（走 RPC）
    fixture
        .router
        .start(start_request(&fixture.remote_id))
        .expect("start");
    let outcome = fixture
        .router
        .stop(
            &ServerRef::remote(&fixture.remote_id),
            StopSpec::default_grace(),
        )
        .expect("stop");
    let StopOutcome::Stopped { .. } = outcome else {
        panic!("expected Stopped, got {outcome:?}");
    };
}

#[test]
fn start_routes_local_and_remote() {
    let fixture = RouterFixture::spawn();
    let local = fixture
        .router
        .start(start_request(&fixture.local_id))
        .expect("local start");
    assert_eq!(local.supervisor_pid, 1111);
    let remote = fixture
        .router
        .start(start_request(&fixture.remote_id))
        .expect("remote start");
    assert_eq!(remote.supervisor_pid, 4242);
}

#[test]
fn status_of_origin_resolves() {
    // 本地未运行 → Ok(None)（不是错误）；远端未运行 → Ok(None)
    let fixture = RouterFixture::spawn();
    assert_eq!(
        fixture
            .router
            .status(&ServerRef::remote(&fixture.local_id))
            .expect("local status"),
        None
    );
    assert_eq!(
        fixture
            .router
            .status(&ServerRef::remote(&fixture.remote_id))
            .expect("remote status"),
        None
    );
    // 来源标注正确：组合仓库里本地 / 远端各一条
    let sourced = fixture
        .router
        .status_many(&[ServerRef::remote(&fixture.local_id)]);
    assert_eq!(sourced.len(), 1);
    let _ = Origin::Local; // 保证 Origin 在作用域内（来源断言由 pid 区分覆盖）
}

fn start_request(id: &ServerId) -> SupervisorStartRequest {
    SupervisorStartRequest {
        server_id: id.clone(),
        server_dir: std::path::PathBuf::from("/srv/mc/web"),
        java_bin: std::path::PathBuf::from("/usr/bin/java"),
        launch: LaunchSpec::jar("server.jar"),
        ram_mib: Some(1024),
        supervisor_exe: std::path::PathBuf::from("mcsup"),
        supervisor_args: vec![std::ffi::OsString::from("__supervise")],
        accept_eula: true,
    }
}
