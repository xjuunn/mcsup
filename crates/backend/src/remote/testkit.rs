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
