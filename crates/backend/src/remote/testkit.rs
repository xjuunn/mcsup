//! 远端镜像测试的共享设施：真 TLS agent + 已配对客户端 + 领域对象样例
//!
//! 与 `crates/mcsup/tests/agent_roundtrip.rs` 同一口径：不 mock 传输，直接
//! 起一个真 agent（真 TLS、真名册、真 `service`），镜像仓库对着它跑完整
//! 请求-响应。镜像测的是「客户端解帧 + 组装」，没有真对端就测不出两端
//! 线格式的对齐。

use transport::Client;

/// 一个已配对的测试环境：起了 agent、完成了配对，[`Self::link`] 直接可用
pub(crate) struct PairingHarness {
    /// agent 数据目录（对端的「本机」）
    pub agent_data: std::path::PathBuf,
    /// agent 垫片句柄（Drop 时停掉监听线程）
    pub agent: mcsup_agent_shim::AgentHandle,
}

impl PairingHarness {
    /// 起 agent 并配对出一个 operator 客户端
    pub(crate) fn spawn() -> Self {
        let dir = tempfile::tempdir().expect("tmp");
        // tempdir 离开作用域会删目录，但身份与运行时状态都在内存里已就绪；
        // 垫片不依赖目录存续（身份只load 一次），这里刻意泄漏目录句柄，
        // 让测试进程退出时统一回收
        let path = dir.path().to_path_buf();
        let handle = mcsup_agent_shim::spawn(&path, "test-box");
        let _token = handle.pair();
        std::mem::forget(dir);
        Self {
            agent_data: path,
            agent: handle,
        }
    }

    /// 已认证的连接网关
    pub(crate) fn link(&self) -> crate::remote::RemoteLink {
        crate::remote::RemoteLink::new(
            self.agent.target(),
            self.agent.fingerprint.clone(),
            &self.agent.token,
        )
    }
}

/// agent 垫片：`backend` 不能依赖 `mcsup` 门面（门面依赖 backend），测试里
/// 需要一个最小 agent。真实现是 `crates/mcsup/src/remote`，但那是门面的
/// 模块——垫片用 `transport::Handler` 的内存实现顶替：帧集与分发逻辑相同
/// （同一份 `protocol::remote`），业务数据用内存表。
///
/// 这样镜像测试验证的是「两端协议对齐」；「agent 分发正确性」由
/// `crates/mcsup` 的集成测试负责，两边不重复。
pub(crate) mod mcsup_agent_shim {
    use super::*;
    use auth::{ClientId, Role};
    use port::ServerStatus;
    use protocol::remote::wire::WireStatusEntry;
    use protocol::remote::{Capabilities, Request, Response};
    use std::sync::Mutex;
    use transport::ServerIdentity;
    use transport::handler::{Handler, RemoteFailure, SessionInfo};
    use transport::server::Server;

    /// 已绑定并在后台线程运行的 agent，配对码固定
    pub(crate) struct AgentHandle {
        pub target: String,
        pub fingerprint: transport::Fingerprint,
        pub token: String,
        stop: transport::StopHandle,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl AgentHandle {
        /// 用固定配对码完成一次配对，返回 token
        pub(crate) fn pair(&self) -> String {
            let client = Client::new(self.target.clone(), self.fingerprint.clone());
            let enrolled = client
                .enroll("1234-5678", "mirror-test", false)
                .expect("enroll");
            enrolled.token
        }

        pub(crate) fn target(&self) -> String {
            self.target.clone()
        }
    }

    impl Drop for AgentHandle {
        fn drop(&mut self) {
            self.stop.request_stop();
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    /// 在 `dir` 下生成身份、绑定 `127.0.0.1:0` 并在后台运行
    pub(crate) fn spawn(dir: &std::path::Path, _name: &str) -> AgentHandle {
        let identity_dir = dir.join("agent-identity");
        std::fs::create_dir_all(&identity_dir).expect("mkdir");
        let identity = ServerIdentity::load_or_create(&identity_dir).expect("identity");
        let fingerprint = identity.fingerprint().clone();
        let handler: std::sync::Arc<dyn Handler> = std::sync::Arc::new(MemAgent::default());
        let server = Server::bind("127.0.0.1:0", &identity, handler).expect("bind");
        let target = server.local_addr().expect("addr").to_string();
        let stop = server.stop_handle().expect("stop handle");
        let join = std::thread::spawn(move || {
            let _ = server.run();
        });
        AgentHandle {
            target,
            fingerprint,
            token: "a".repeat(64),
            stop,
            join: Some(join),
        }
    }

    /// 内存版 agent：固定 token，接受所有已认证业务帧并返回最小合法载荷
    #[derive(Default)]
    struct MemAgent {
        workspaces: Mutex<Vec<protocol::remote::wire::WireWorkspace>>,
        servers: Mutex<Vec<protocol::remote::wire::WireServer>>,
        /// 服务器身份 → 运行状态（supervisor 垫片）
        running: Mutex<Vec<RunningServer>>,
        /// 备份垫片的快照索引
        snapshots: Mutex<Vec<protocol::remote::wire::WireSnapshot>>,
        /// `server.properties` 垫片：键值表（保持写入顺序）
        props: Mutex<Vec<(String, String)>>,
    }

    /// 垫片里的 `server.properties` 路径
    const PROPS_PATH: &str = "/srv/mc/server.properties";

    impl MemAgent {
        /// 一条固定内容的快照（id 随机后缀避免同秒撞名）
        fn snapshot(server: &str) -> protocol::remote::wire::WireSnapshot {
            protocol::remote::wire::WireSnapshot {
                id: format!("20990101t000000-{:06x}", fastrand()),
                workspace_id: "survival".to_owned(),
                server: server.to_owned(),
                label: None,
                parent: None,
                scope: Vec::new(),
                status: "complete".to_owned(),
                entries: vec![("world/level.dat".to_owned(), "a".repeat(64))],
                deleted: Vec::new(),
                stamps: vec![("world/level.dat".to_owned(), 100, 1)],
                added: 1,
                reused: 0,
                total_bytes: 100,
                added_bytes: 100,
                server_online: false,
                created_at: 1,
            }
        }
        /// `server.properties` 垫片的原文（每行 `key=value`）
        fn props_text(&self) -> String {
            let guard = self.props.lock().expect("lock");
            let mut text = String::new();
            for (key, value) in guard.iter() {
                text.push_str(key);
                text.push('=');
                text.push_str(value);
                text.push('\n');
            }
            text
        }
    }

    /// supervisor 垫片里的一台「运行中」服务器
    #[derive(Clone)]
    #[allow(dead_code)] // started_at 留给日志/uptime 断言扩展用
    pub(crate) struct RunningServer {
        pub id: String,
        pub status: protocol::remote::wire::WireStatus,
        pub started_at: std::time::Instant,
    }

    impl Handler for MemAgent {
        fn greeting(&self) -> transport::Greeting {
            transport::Greeting {
                agent_version: "test".to_owned(),
                server_name: "test-box".to_owned(),
                capabilities: Capabilities::all(),
                fingerprint: transport::Fingerprint::of_der(b"unused"),
                has_clients: true,
            }
        }

        fn enroll(
            &self,
            code: &str,
            _client_name: &str,
            _read_only: bool,
        ) -> Result<transport::Enrolled, RemoteFailure> {
            if code != "1234-5678" {
                return Err(RemoteFailure::unauthorized("bad pairing code"));
            }
            Ok(transport::Enrolled {
                token: "a".repeat(64),
                client_id: ClientId::generate(),
                role: Role::Operator,
            })
        }

        fn authenticate(&self, token: &str) -> Result<SessionInfo, RemoteFailure> {
            if token.chars().all(|c| c.is_ascii_hexdigit()) && token.len() == 64 {
                Ok(SessionInfo {
                    client_id: ClientId::generate(),
                    role: Role::Operator,
                })
            } else {
                Err(RemoteFailure::unauthorized("bad token"))
            }
        }

        #[allow(clippy::too_many_lines)] // 垫片按帧族展开，比拆分更可读
        fn handle(&self, _session: &SessionInfo, request: &Request) -> Response {
            match request {
                Request::WorkspaceList => Response::Workspaces {
                    items: self.workspaces.lock().expect("lock").clone(),
                },
                Request::WorkspaceInit { dir, name } => {
                    let mut guard = self.workspaces.lock().expect("lock");
                    let wire = protocol::remote::wire::WireWorkspace {
                        id: uuid_v4(),
                        name: name.clone().unwrap_or_else(|| "ws".to_owned()),
                        path: dir.clone(),
                        is_current: guard.is_empty(),
                        created_at: 1,
                        updated_at: 1,
                    };
                    guard.push(wire.clone());
                    Response::Workspaces { items: vec![wire] }
                }
                Request::WorkspaceUse { .. } | Request::ServerList => {
                    if matches!(request, Request::ServerList) {
                        Response::Servers {
                            items: self.servers.lock().expect("lock").clone(),
                        }
                    } else {
                        let mut guard = self.workspaces.lock().expect("lock");
                        let Some(first) = guard.first_mut() else {
                            return RemoteFailure::new(
                                protocol::remote::ErrorKind::NOT_FOUND,
                                "no workspace",
                            )
                            .into_response();
                        };
                        first.is_current = true;
                        Response::Workspaces {
                            items: vec![first.clone()],
                        }
                    }
                }
                Request::ServerCreate {
                    workspace: _,
                    name,
                    version,
                    variant,
                    java_major,
                } => {
                    let mut guard = self.servers.lock().expect("lock");
                    let wire = encoding::wire_server(name, version, variant, *java_major);
                    guard.push(wire.clone());
                    Response::Servers { items: vec![wire] }
                }
                Request::ServerRemove { name, .. } => {
                    let mut guard = self.servers.lock().expect("lock");
                    let before = guard.len();
                    guard.retain(|srv| srv.name != *name);
                    if guard.len() == before {
                        return RemoteFailure::new(
                            protocol::remote::ErrorKind::NOT_FOUND,
                            format!("no such server: {name}"),
                        )
                        .into_response();
                    }
                    Response::Removed { disk_purged: false }
                }
                Request::JavaList | Request::JavaScan => Response::Javas { items: Vec::new() },
                // ---- 写入类帧：内存垫片按最小语义回帧 ----
                Request::ServerInstall { name, version } => {
                    let mut guard = self.servers.lock().expect("lock");
                    let Some(srv) = guard.iter_mut().find(|srv| srv.name == *name) else {
                        return RemoteFailure::new(
                            protocol::remote::ErrorKind::NOT_FOUND,
                            format!("no such server: {name}"),
                        )
                        .into_response();
                    };
                    srv.version = version.clone();
                    srv.updated_at = 2;
                    let wire = srv.clone();
                    drop(guard);
                    Response::Servers { items: vec![wire] }
                }
                Request::ServerCreateModpack {
                    name,
                    project,
                    version,
                    ..
                } => {
                    let wire = encoding::wire_server(name, "1.21.1", "fabric", None);
                    self.servers.lock().expect("lock").push(wire.clone());
                    Response::ModpackApplied {
                        server: wire,
                        project_id: project.clone(),
                        version_id: version.clone().unwrap_or_else(|| "v1".to_owned()),
                        version_number: "1.0.0".to_owned(),
                        minecraft: "1.21.1".to_owned(),
                        loader: Some("fabric 0.16.9".to_owned()),
                        files: 3,
                        downloaded_bytes: 1024,
                        reused: 1,
                        skipped_client_only: 0,
                    }
                }
                Request::PropsGet { .. } => Response::Props {
                    path: PROPS_PATH.to_owned(),
                    text: self.props_text(),
                },
                Request::PropsSet { updates, .. } => {
                    let mut guard = self.props.lock().expect("lock");
                    for update in updates {
                        match guard.iter_mut().find(|(key, _)| *key == update.key) {
                            Some(slot) => slot.1.clone_from(&update.value),
                            None => guard.push((update.key.clone(), update.value.clone())),
                        }
                    }
                    drop(guard);
                    Response::Props {
                        path: PROPS_PATH.to_owned(),
                        text: self.props_text(),
                    }
                }
                Request::SettingsSet { value, .. } => Response::SettingsChanged {
                    value: value.clone(),
                    settings: protocol::remote::wire::WireSettings::from(
                        &domain::server::settings::ServerSettings::default(),
                    ),
                },
                Request::JavaInstall { major, .. } => Response::Javas {
                    items: vec![protocol::remote::wire::WireJava {
                        id: format!("rt-{major}"),
                        path: format!("/opt/java/{major}"),
                        major: *major,
                        version: format!("{major}.0.1"),
                        vendor: "temurin".to_owned(),
                        managed: true,
                        created_at: 1,
                        updated_at: 1,
                    }],
                },
                Request::JavaUninstall { path } => Response::JavaUninstalled {
                    path: path.clone(),
                    managed: true,
                },
                // ---- supervisor 垫片：按 RunningServer 表回帧 ----
                Request::StatusMany { ids } => {
                    let guard = self.running.lock().expect("lock");
                    Response::Statuses {
                        items: ids
                            .iter()
                            .map(|id| match guard.iter().find(|srv| srv.id == *id) {
                                Some(srv) => WireStatusEntry {
                                    status: Some(srv.status.clone()),
                                    error: None,
                                },
                                None => WireStatusEntry {
                                    status: None,
                                    error: None,
                                },
                            })
                            .collect(),
                    }
                }
                Request::Start { server_id, .. } => {
                    let mut guard = self.running.lock().expect("lock");
                    if guard.iter().any(|srv| srv.id == *server_id) {
                        return RemoteFailure::new(
                            protocol::remote::ErrorKind::BUSY,
                            "already running",
                        )
                        .into_response();
                    }
                    let status = WireStatusEntry::ok(Some(&port::SupervisorStatus {
                        status: ServerStatus::Running,
                        pid: Some(4242),
                        uptime: None,
                        exit_code: None,
                    }))
                    .status
                    .expect("wire status");
                    guard.push(RunningServer {
                        id: server_id.clone(),
                        status,
                        started_at: std::time::Instant::now(),
                    });
                    Response::Started {
                        outcome: protocol::remote::wire::WireStartOutcome {
                            supervisor_pid: 4242,
                        },
                    }
                }
                Request::Stop { server_id, .. } => {
                    let mut guard = self.running.lock().expect("lock");
                    let before = guard.len();
                    guard.retain(|srv| srv.id != *server_id);
                    if guard.len() == before {
                        return Response::Stopped {
                            outcome: protocol::remote::wire::WireStop::AlreadyStopped,
                        };
                    }
                    Response::Stopped {
                        outcome: protocol::remote::wire::WireStop::Stopped { elapsed_ms: 5 },
                    }
                }
                Request::SendLine { server_id, .. } => {
                    let guard = self.running.lock().expect("lock");
                    if guard.iter().any(|srv| srv.id == *server_id) {
                        Response::Ok
                    } else {
                        RemoteFailure::new(
                            protocol::remote::ErrorKind::NOT_FOUND,
                            format!("not running: {server_id}"),
                        )
                        .into_response()
                    }
                }
                Request::LogsTail { lines, .. } => Response::LogLines {
                    lines: (0..*lines).map(|i| format!("log line {i}")).collect(),
                },
                // 备份垫片：一台固定快照的内存索引（create 追加，delete/prune 移除）
                Request::BackupCreate { server_id, .. } => {
                    let snapshot = MemAgent::snapshot(server_id);
                    let mut guard = self.snapshots.lock().expect("lock");
                    guard.push(snapshot.clone());
                    Response::SnapshotCreated {
                        snapshot,
                        skipped: 0,
                    }
                }
                Request::BackupList {
                    page, page_size, ..
                } => {
                    let guard = self.snapshots.lock().expect("lock");
                    let total = guard.len();
                    let skip = page.saturating_sub(1) * page_size;
                    Response::Snapshots {
                        items: guard.iter().skip(skip).take(*page_size).cloned().collect(),
                        total,
                        page: *page,
                        page_size: *page_size,
                    }
                }
                Request::BackupRestore {
                    server_id,
                    snapshot,
                    to_new,
                } => {
                    let guard = self.snapshots.lock().expect("lock");
                    match guard.iter().find(|snap| snap.id == *snapshot) {
                        Some(found) => Response::Restored {
                            snapshot: found.clone(),
                            destination: format!("/srv/mc/{server_id}"),
                            files: 1,
                            bytes: 100,
                            safety_snapshot: if to_new.is_none() {
                                Some("20990101t000000-safety".to_owned())
                            } else {
                                None
                            },
                            deleted: 0,
                        },
                        None => RemoteFailure::new(
                            protocol::remote::ErrorKind::NOT_FOUND,
                            format!("no such snapshot: {snapshot}"),
                        )
                        .into_response(),
                    }
                }
                Request::BackupDelete { snapshot, .. } => {
                    let mut guard = self.snapshots.lock().expect("lock");
                    let index = guard.iter().position(|snap| snap.id == *snapshot);
                    match index {
                        Some(at) => {
                            let deleted = guard.remove(at);
                            Response::SnapshotDeleted {
                                snapshot: deleted,
                                orphaned: 1,
                            }
                        }
                        None => RemoteFailure::new(
                            protocol::remote::ErrorKind::NOT_FOUND,
                            format!("no such snapshot: {snapshot}"),
                        )
                        .into_response(),
                    }
                }
                Request::BackupVerify { snapshot, .. } => {
                    let guard = self.snapshots.lock().expect("lock");
                    match guard.iter().find(|snap| snap.id == *snapshot) {
                        Some(found) => Response::SnapshotVerified {
                            snapshot: found.clone(),
                            objects: 1,
                            bytes: 100,
                            broken: Vec::new(),
                        },
                        None => RemoteFailure::new(
                            protocol::remote::ErrorKind::NOT_FOUND,
                            format!("no such snapshot: {snapshot}"),
                        )
                        .into_response(),
                    }
                }
                Request::BackupPrune { .. } => {
                    let mut guard = self.snapshots.lock().expect("lock");
                    let removed: Vec<_> = guard.drain(..).collect();
                    Response::Pruned {
                        removed,
                        objects_reclaimed: 0,
                        bytes_reclaimed: 0,
                        dry_run: false,
                    }
                }
                other => RemoteFailure::invalid(format!("unsupported: {}", other.operation()))
                    .into_response(),
            }
        }
    }

    fn uuid_v4() -> String {
        // 内存垫片不引 uuid 依赖：合法的 v4 形状（版本位 4、变体位 a）+ 递增序号
        let n = fastrand() % 1000;
        format!("550e8400-e29b-41d4-a716-44665544{n:04}")
    }

    fn fastrand() -> u32 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(1);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }
}

/// 领域对象与线格式的样例工厂（镜像测试共用）
pub(crate) mod encoding {
    use domain::java::{JavaMajor, JavaRuntime, JavaVendor};
    use domain::server::meta::{ServerId, ServerMeta, ServerName};
    use domain::server::settings::ServerSettings;
    use domain::server::version::{ServerVariant, VersionId};
    use domain::workspace::WorkspaceId;
    use protocol::remote::wire::{WireJava, WireServer};

    /// 一个合法的 `WorkspaceId` 样例
    pub(crate) fn workspace_id() -> WorkspaceId {
        WorkspaceId::parse("550e8400-e29b-41d4-a716-446655440000").expect("ws id")
    }

    /// 一条最小合法的 `ServerMeta`（不含 jar / launch）
    pub(crate) fn server_meta(name: &ServerName, ws: WorkspaceId) -> ServerMeta {
        ServerMeta {
            id: ServerId::parse("550e8400-e29b-41d4-a716-446655440001").expect("id"),
            name: name.clone(),
            workspace_id: ws,
            rel_dir: std::path::PathBuf::from(name.as_str()),
            version: VersionId::new("1.21.1"),
            variant: ServerVariant::Vanilla,
            jar: None,
            launch: None,
            settings: ServerSettings::default(),
            java_major: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    /// 一条最小合法的 `WireServer`
    pub(crate) fn wire_server(
        name: &str,
        version: &str,
        variant: &str,
        java_major: Option<i64>,
    ) -> WireServer {
        let meta = server_meta(&ServerName::parse(name).expect("name"), workspace_id());
        let mut wire = WireServer::try_from(&meta).expect("encode");
        wire.name = name.to_owned();
        wire.version = version.to_owned();
        wire.variant = variant.to_owned();
        wire.java_major = java_major;
        wire
    }

    /// 一条最小合法的 `JavaRuntime`
    pub(crate) fn java_runtime() -> JavaRuntime {
        JavaRuntime {
            id: "rt-1".to_owned(),
            path: std::path::PathBuf::from("/opt/java/21"),
            major: JavaMajor::parse(21).expect("major"),
            version: "21.0.1".to_owned(),
            vendor: JavaVendor::Temurin,
            managed: true,
            created_at: 1,
            updated_at: 1,
        }
    }

    /// `JavaRuntime` → `WireJava`（镜像侧解帧的对照样例）
    #[allow(dead_code)]
    pub(crate) fn wire_java(rt: &JavaRuntime) -> WireJava {
        WireJava::from(rt)
    }
}
