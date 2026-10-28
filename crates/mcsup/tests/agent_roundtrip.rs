//! agent 的真实链路：真 TLS、真名册、真 service
//!
//! 这里不打桩：`Agent` 装的是 `Mcsup::open` 的默认组合根，客户端走
//! `transport::Client` 的真实握手。测的是「配对 → 认证 → 业务帧 → 撤销」
//! 整条链，任何一环接错都会在这里断，而不是等到用户跨机器试。
//!
//! 数据目录用临时目录：身份、名册、元数据仓库都落在里面，测试之间互不影响。

// feature powerset 会单独开每个 feature：没开 `remote` 时这个测试文件整体不编译
#![cfg(feature = "remote")]
// 集成测试不在 `#[test]` 函数体内，clippy 的 `expect_used` 豁免只覆盖 cfg(test)
#![allow(clippy::expect_used)]

use mcsup::auth::Role;
use mcsup::protocol::remote::{Capability, ErrorKind, Request, Response};
use mcsup::remote::{Agent, AgentError};
use mcsup::transport::{Client, Fingerprint, StopHandle, TransportError};
use std::sync::Arc;

/// 一个已绑定的 agent，连同它的临时数据目录与停止句柄
struct Harness {
    agent: Arc<Agent>,
    addr: std::net::SocketAddr,
    fingerprint: String,
    stop: StopHandle,
    join: Option<std::thread::JoinHandle<Result<(), AgentError>>>,
    dir: tempfile::TempDir,
}

impl Harness {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = Arc::new(Agent::open(dir.path(), "test-box").expect("agent"));
        let server = agent.bind("127.0.0.1:0").expect("bind");
        let addr = server.local_addr().expect("addr");
        let stop = server.stop_handle().expect("stop handle");
        let fingerprint = agent.fingerprint().to_string();
        let join = std::thread::spawn(move || server.run());
        Self {
            agent,
            addr,
            fingerprint,
            stop,
            join: Some(join),
            dir,
        }
    }

    /// 未配对的客户端（还没有 token）
    fn client(&self) -> Client {
        Client::new(
            self.addr.to_string(),
            Fingerprint::parse(&self.fingerprint).expect("fingerprint"),
        )
    }

    /// 用当前有效码配对并返回带 token 的客户端
    fn paired(&self, name: &str, read_only: bool) -> Client {
        let code = self.agent.issue_pairing_code().expect("issue");
        let enrolled = self
            .client()
            .enroll(&code.display_form(), name, read_only)
            .expect("enroll");
        self.client().with_token(enrolled.token)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // `request_stop` 会自连一次踢醒阻塞的 accept；run 随后看到标志退出
        self.stop.request_stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[test]
fn greeting_reports_identity_and_no_clients() {
    let harness = Harness::start();
    let greeting = harness.client().hello().expect("hello");
    assert_eq!(greeting.server_name, "test-box");
    assert_eq!(greeting.fingerprint.to_string(), harness.fingerprint);
    assert!(!greeting.has_clients, "刚装配时名册为空");
    // 能力必须覆盖已实现的帧族
    assert!(greeting.capabilities.has(Capability::Java));
    assert!(greeting.capabilities.has(Capability::Installer));
}

/// `Client::probe`：首次配对前捕获的指纹必须来自 TLS 证书本身
///
/// 这是 TOFU 的第一步：用户看到的指纹就是之后固定的那个；对端在 Hello 里
/// 自报的指纹若与证书不一致，probe 直接拒绝（中间人）。
#[test]
fn probe_captures_the_certificate_fingerprint() {
    let harness = Harness::start();
    let probe = Client::probe(&harness.addr.to_string()).expect("probe");
    assert_eq!(probe.fingerprint.to_string(), harness.fingerprint);
    assert_eq!(probe.greeting.server_name, "test-box");
    assert!(!probe.greeting.has_clients);
    // probe 不消耗配对码，也不登记客户端
    assert!(harness.agent.clients().is_empty().expect("empty"));
}

/// probe 之后仍可用捕获的指纹完成配对：两步之间没有状态丢失
#[test]
fn probe_then_enroll_uses_the_same_identity() {
    let harness = Harness::start();
    let probe = Client::probe(&harness.addr.to_string()).expect("probe");
    let code = harness.agent.issue_pairing_code().expect("issue");
    let enrolled = Client::new(harness.addr.to_string(), probe.fingerprint)
        .enroll(&code.display_form(), "laptop", false)
        .expect("enroll");
    let client = harness.client().with_token(enrolled.token);
    assert!(matches!(
        client.call(&Request::Ping).expect("ping"),
        Response::Pong { .. }
    ));
}

/// 错码被拒且不消耗客户端名额
#[test]
fn wrong_pairing_code_is_rejected() {
    let harness = Harness::start();
    let err = harness
        .client()
        .enroll("0000-0000", "laptop", false)
        .expect_err("bad code");
    assert!(matches!(err, TransportError::Remote { .. }), "{err:?}");
    assert!(!harness.client().hello().expect("hello").has_clients);
}

/// 配对一次即可反复认证；token 只以摘要落盘
#[test]
fn enroll_then_authenticate_and_reuse() {
    let harness = Harness::start();
    let client = harness.paired("laptop", false);

    let info = client.authenticate().expect("authenticate");
    assert_eq!(info.role, Role::Operator);

    // 名册里存的是摘要，不是 token
    let records = harness.agent.clients().load().expect("load");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, "laptop");

    // 业务帧：Ping
    let response = client.call(&Request::Ping).expect("ping");
    assert!(matches!(response, Response::Pong { .. }), "{response:?}");
}

/// 只读配对发 viewer：能读、不能写
#[test]
fn read_only_pairing_yields_viewer() {
    let harness = Harness::start();
    let client = harness.paired("observer", true);
    assert_eq!(client.authenticate().expect("auth").role, Role::Viewer);

    assert!(matches!(
        client.call(&Request::ServerList).expect("list"),
        Response::Servers { .. }
    ));
    let response = client
        .call(&Request::WorkspaceInit {
            dir: "/tmp/never".to_owned(),
            name: None,
        })
        .expect("call");
    match response {
        Response::Error { kind, .. } => assert_eq!(kind, ErrorKind::UNAUTHORIZED),
        other => panic!("viewer 不该能 init：{other:?}"),
    }
}

/// 名册帧要 Manage：operator 看不到、也撤不掉别人
#[test]
fn client_management_requires_admin() {
    let harness = Harness::start();
    let client = harness.paired("operator", false);
    for request in [Request::ClientList, Request::Shutdown] {
        match client.call(&request).expect("call") {
            Response::Error { kind, .. } => assert_eq!(kind, ErrorKind::UNAUTHORIZED),
            other => panic!("{} 需要 Manage：{other:?}", request.operation()),
        }
    }
    // 被拒的操作不能让 agent 退出：服务仍然可用
    assert!(matches!(
        client.call(&Request::Ping).expect("ping"),
        Response::Pong { .. }
    ));
}

/// 工作目录与服务器的写路径真的落在 agent 的 service 上
#[test]
fn workspace_and_server_ops_run_on_the_agent() {
    let harness = Harness::start();
    let client = harness.paired("admin-ish", false);
    let dir = harness.dir.path().join("ws");

    let created = client
        .call(&Request::WorkspaceInit {
            dir: dir.to_string_lossy().into_owned(),
            name: Some("ws01".to_owned()),
        })
        .expect("init");
    let Response::Workspaces { items } = created else {
        panic!("init 应返回工作目录：{created:?}");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "ws01");
    // 目录确实建在 agent 这台机器上
    assert!(dir.join("servers").is_dir(), "{}", dir.display());

    let listed = client.call(&Request::WorkspaceList).expect("list");
    let Response::Workspaces { items } = listed else {
        panic!("list 应返回工作目录：{listed:?}");
    };
    assert_eq!(items.len(), 1);
    assert!(items[0].is_current, "首个工作目录自动成为当前");

    let created = client
        .call(&Request::ServerCreate {
            workspace: Some("ws01".to_owned()),
            name: "web".to_owned(),
            version: "1.21.1".to_owned(),
            variant: "vanilla".to_owned(),
            java_major: Some(21),
        })
        .expect("create");
    let Response::Servers { items } = created else {
        panic!("create 应返回服务器：{created:?}");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "web");

    // 名字冲突必须是 already_exists，而不是笼统的 io
    match client
        .call(&Request::ServerCreate {
            workspace: Some("ws01".to_owned()),
            name: "web".to_owned(),
            version: "1.21.1".to_owned(),
            variant: "vanilla".to_owned(),
            java_major: None,
        })
        .expect("call")
    {
        Response::Error { kind, .. } => assert_eq!(kind, ErrorKind::ALREADY_EXISTS),
        other => panic!("重名应被拒：{other:?}"),
    }

    // 批量状态：一台不存在时只坏那一格
    let response = client
        .call(&Request::StatusMany {
            ids: vec!["no-such-id".to_owned()],
        })
        .expect("status");
    let Response::Statuses { items } = response else {
        panic!("status 应返回批量结果：{response:?}");
    };
    assert_eq!(items.len(), 1);
    assert!(
        items[0].clone().into_result().is_err(),
        "不存在的服务器要报错"
    );
}

/// `server link` / `server variants` / `server versions` 在真实 agent 上执行
#[test]
fn server_link_variants_and_versions_run_on_the_agent() {
    let harness = Harness::start();
    let client = harness.paired("admin-ish", false);

    // link 需要一个工作区作为 servers/ 的落点
    let ws_dir = harness.dir.path().join("ws");
    let created = client
        .call(&Request::WorkspaceInit {
            dir: ws_dir.to_string_lossy().into_owned(),
            name: Some("ws01".to_owned()),
        })
        .expect("init ws");
    assert!(
        matches!(created, Response::Workspaces { .. }),
        "{created:?}"
    );

    // 对端上一个已存在的目录：link 在对端建符号链接并入库
    let external = harness.dir.path().join("external");
    std::fs::create_dir_all(&external).expect("mkdir external");
    let response = client
        .call(&Request::ServerLink {
            target: external.to_string_lossy().into_owned(),
            name: Some("linked".to_owned()),
            workspace: Some("ws01".to_owned()),
        })
        .expect("link");
    let Response::Linked {
        server,
        target,
        linked,
    } = response
    else {
        panic!("link 应返回 Linked：{response:?}");
    };
    assert_eq!(server.name, "linked");
    assert!(linked, "外部目录应建立符号链接");
    assert!(target.ends_with("external"), "{target}");

    // 变体列表来自 agent 的默认来源，至少含 vanilla
    let response = client.call(&Request::ServerVariants).expect("variants");
    let Response::Variants { items } = response else {
        panic!("variants 应返回列表：{response:?}");
    };
    assert!(
        items.iter().any(|variant| variant == "vanilla"),
        "{items:?}"
    );

    // 版本列表走真实来源：拿到列表就校验字段；外网不可用时对端回错误帧，
    // 链路本身仍算正常（不把测试绑定到上游数据）
    let response = client
        .call(&Request::ServerVersions {
            variant: "vanilla".to_owned(),
        })
        .expect("versions");
    match response {
        Response::Versions { items } => {
            assert!(
                items.iter().all(|version| !version.id.is_empty()),
                "{items:?}"
            );
        }
        Response::Error { .. } => {}
        other => panic!("versions 应返回列表或错误帧：{other:?}"),
    }
}

/// 撤销立即失效：同一个 token 再连必须被拒
///
/// 撤销需要 `Manage`（operator 没有），所以先在本机把管理员升到 `admin`：
/// 角色提升只能发生在 agent 这台机器上，正是「远程不能自己给自己提权」的体现。
#[test]
fn revoking_a_client_invalidates_its_token() {
    let harness = Harness::start();
    let victim = harness.paired("laptop", false);
    let admin = harness.paired("desk", false);
    let client_id = victim.authenticate().expect("auth").client_id;
    let admin_id = admin.authenticate().expect("auth").client_id;
    harness
        .agent
        .clients()
        .set_role(&admin_id, Role::Admin)
        .expect("promote admin");

    let response = admin
        .call(&Request::ClientRevoke {
            selector: client_id.as_str().to_owned(),
        })
        .expect("revoke");
    assert!(matches!(response, Response::Ok), "{response:?}");

    let err = victim.authenticate().expect_err("revoked");
    assert!(matches!(err, TransportError::Remote { .. }), "{err:?}");
    // 名册里没有「已撤销」的残留
    assert_eq!(harness.agent.clients().load().expect("load").len(), 1);
}

/// 握手帧在已认证连接上重放属于协议错误，而不是被当成业务帧执行
#[test]
fn handshake_frames_are_rejected_after_authentication() {
    let harness = Harness::start();
    let client = harness.paired("laptop", false);
    match client
        .call(&Request::Authenticate {
            token: "0".repeat(64),
        })
        .expect("call")
    {
        Response::Error { kind, .. } => assert_eq!(kind, ErrorKind::PROTOCOL),
        other => panic!("重放握手帧应报协议错误：{other:?}"),
    }
}
