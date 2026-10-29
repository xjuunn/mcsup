//! 按需配对码：**启动时无码**，只有 `remote code` 才发一枚
//!
//! 这是 `docs/remote.md`「安全」里那条约束的验收测试：后台常驻的端口长期
//! 开放，若屏幕/进程里总有一枚待兑换的码，攻击者只需要猜中一次。按需形态下
//! 进程启动后没有任何可用码，猜码连目标都不存在。
//!
//! 测的是真实链路：真 agent（`PairingPolicy::OnDemand`）、真 TLS、真名册，
//! 码由 `PairingGrant` 落盘，agent 在兑换时读同一个数据目录。

// 没开 `remote` feature 时整份文件不编译（feature powerset 会单独开每个 feature）
#![cfg(feature = "remote")]
// 集成测试的辅助函数不在 `#[test]` 内，clippy 的 in-tests 白名单识别不到
#![allow(clippy::expect_used)]

use mcsup::auth::{self, PairingGrant, PairingWindow, Role};
use mcsup::protocol::remote::{Request, Response};
use mcsup::remote::{Agent, PairingPolicy};
use mcsup::transport::{self, Client, Fingerprint, TransportError};
use std::sync::Arc;

/// 一个按需发码的 agent
struct Harness {
    agent: Arc<Agent>,
    addr: String,
    fingerprint: Fingerprint,
    grant: PairingGrant,
    stop: transport::StopHandle,
    join: Option<std::thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent =
            Arc::new(Agent::open_with(dir.path(), "box", PairingPolicy::OnDemand).expect("agent"));
        let grant = PairingGrant::new(dir.path(), PairingWindow::default());
        let server = agent.bind("127.0.0.1:0").expect("bind");
        let addr = server.local_addr().expect("addr").to_string();
        let stop = server.stop_handle().expect("stop");
        let fingerprint = Fingerprint::parse(agent.fingerprint().as_str()).expect("fingerprint");
        let join = std::thread::spawn(move || {
            let _ = server.run();
        });
        Self {
            agent,
            addr,
            fingerprint,
            grant,
            stop,
            join: Some(join),
            _dir: dir,
        }
    }

    fn client(&self) -> Client {
        Client::new(self.addr.clone(), self.fingerprint.clone())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.request_stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// 启动后没有任何码：这是本次设计的核心目的
#[test]
fn a_freshly_started_agent_has_no_pairing_code() {
    let harness = Harness::start();
    assert_eq!(
        harness.agent.pairing_status().expect("status"),
        None,
        "按需形态启动即无码"
    );
    assert_eq!(
        harness.agent.current_pairing_code().expect("current"),
        None,
        "按需形态不持有明文码"
    );
}

/// 无码时配对必被拒，且拒得与「码不对」不可区分
#[test]
fn enroll_without_a_minted_code_is_rejected() {
    let harness = Harness::start();
    for guess in ["0000-0000", "1234-5678"] {
        let err = harness
            .client()
            .enroll(guess, "intruder", false)
            .expect_err("no code to redeem");
        // 对外只报「配对失败」，不区分「没有码」和「码不对」
        assert!(matches!(err, TransportError::Remote { .. }), "{err:?}");
    }
    assert!(
        harness.agent.clients().is_empty().expect("registry"),
        "失败的 Enroll 不得写入名册"
    );
}

/// 发码 → 配对 → 码被销毁；第二台机器必须重新发码
#[test]
fn a_minted_code_pairs_exactly_one_client() {
    let harness = Harness::start();
    let code = harness
        .grant
        .issue(mcsup::remote::now_secs(), 300)
        .expect("issue");
    assert!(
        harness.agent.pairing_status().expect("status").is_some(),
        "发码后 agent 应当看到有码"
    );

    let enrolled = harness
        .client()
        .enroll(&code.display_form(), "laptop", false)
        .expect("enroll");
    assert_eq!(enrolled.role, Role::Operator);

    // 成功即销毁：不留重放窗口
    assert_eq!(harness.agent.pairing_status().expect("status"), None);
    let replay = harness
        .client()
        .enroll(&code.display_form(), "replay", false)
        .expect_err("code is single use");
    assert!(
        matches!(replay, TransportError::Remote { .. }),
        "{replay:?}"
    );

    // 已配对客户端照常工作
    let client = harness.client().with_token(enrolled.token);
    assert!(matches!(
        client.call(&Request::Ping).expect("ping"),
        Response::Pong { .. }
    ));
}

/// 过期码不可兑换：TTL 由发码时的 `--ttl` 决定
#[test]
fn an_expired_code_cannot_pair() {
    let harness = Harness::start();
    let ttl = auth::MIN_TTL_SECS;
    // 把签发时刻放在「已经过期」的位置：判定是纯函数，不必 sleep
    let issued_at = mcsup::remote::now_secs() - ttl - 1;
    let code = harness.grant.issue(issued_at, ttl).expect("issue");

    assert_eq!(
        harness
            .grant
            .status(mcsup::remote::now_secs())
            .expect("status"),
        None,
        "过期即无码"
    );
    assert_eq!(harness.agent.pairing_status().expect("agent status"), None);
    let err = harness
        .client()
        .enroll(&code.display_form(), "late", false)
        .expect_err("expired");
    assert!(matches!(err, TransportError::Remote { .. }), "{err:?}");
}

/// 连续形态（SSH 引导 / 前台调试）不受影响：仍然启动即有码、可兑换
#[test]
fn continuous_policy_still_issues_on_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let agent = Agent::open(dir.path(), "box").expect("agent");
    assert_eq!(agent.pairing_policy(), PairingPolicy::Continuous);
    let code = agent.issue_pairing_code().expect("issue");
    assert!(agent.pairing_status().expect("status").is_some());
    // 码在内存里，不是落盘文件
    assert!(
        !PairingGrant::new(dir.path(), PairingWindow::default())
            .path()
            .exists(),
        "连续形态不写 grant.toml"
    );
    assert_eq!(
        agent.current_pairing_code().expect("current"),
        Some(code),
        "连续形态能读出明文码供展示"
    );
}

/// 按需形态下 agent 不自动补码：过期后仍是「无码」，不会悄悄生成新的
#[test]
fn on_demand_never_reseeds_after_expiry() {
    let harness = Harness::start();
    harness
        .grant
        .issue(mcsup::remote::now_secs(), auth::MIN_TTL_SECS)
        .expect("issue");

    // 模拟轮换线程：按需策略必须什么都不做
    assert_eq!(
        harness.agent.rotate_pairing_code().expect("rotate"),
        None,
        "按需形态不得自动补码"
    );
}
