//! `remote serve start / status / stop` 的真实进程生命周期
//!
//! 起的是**真进程**：`serve start` 用当前可执行文件拉起一个后台 agent，父进程
//! 立即返回。测的是这条链上只有真进程才能暴露的东西——单实例、就绪探测、锁
//! 释放、pid 文件清理、以及「停掉之后端口真的不可达」。
//!
//! 每个测试用独立临时 HOME，互不干扰，也不会碰到用户真实的数据目录。

// 集成测试的辅助函数不在 `#[test]` 内，clippy 的 in-tests 白名单识别不到
#![allow(clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

/// 隔离用临时 HOME（与 `cli_remote` 同口径）
fn temp_home() -> TempDir {
    let home = TempDir::new().expect("tempdir");
    if cfg!(windows) {
        std::fs::create_dir_all(home.path().join("AppData").join("Roaming"))
            .expect("创建 AppData\\Roaming");
    }
    home
}

/// 跑一次 mcsup（非 TTY、隔离 HOME）
fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mcsup"))
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_DATA_HOME", home.join("share"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env_remove("MCSUP_LANG")
        .env_remove("LANG")
        .env_remove("LC_ALL")
        .env_remove("LC_CTYPE")
        .env_remove("MCSUP_ORIGIN")
        .env("MCSUP_WORKSPACE", "")
        .env("MCSUP_CREDENTIAL_STORE", "file")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("运行 mcsup")
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout 必须是 JSON: {err}\nstdout=[{}]\nstderr=[{}]",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn assert_ok(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} 失败: status={:?}\nstdout=[{}]\nstderr=[{}]",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// 无论断言是否失败都要把后台进程收掉，避免残留监听端口
struct Served {
    home: TempDir,
}

impl Served {
    fn new() -> Self {
        Self { home: temp_home() }
    }

    fn run(&self, args: &[&str]) -> Output {
        run(self.home.path(), args)
    }

    /// 起一个后台 agent（监听随机端口，避免与本机其他服务抢端口）
    fn start(&self) -> u64 {
        let out = self.run(&[
            "remote",
            "serve",
            "start",
            "--listen",
            "127.0.0.1:0",
            "--output",
            "json",
        ]);
        assert_ok(&out, "serve start");
        json(&out)["pid"].as_u64().expect("pid")
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        // 尽力而为：进程已退出时这里是幂等成功
        let _ = self.run(&["remote", "serve", "stop", "--force", "--output", "json"]);
    }
}

/// start → status → stop 全链路
#[test]
fn serve_lifecycle_starts_reports_and_stops() {
    let served = Served::new();

    // 起始状态：没有 agent、没有码
    let before = served.run(&["remote", "serve", "status", "--output", "json"]);
    assert!(
        !before.status.success(),
        "还没起过 serve 时应报没有身份文件"
    );

    let pid = served.start();
    assert!(pid > 0);

    let status = served.run(&["remote", "serve", "status", "--output", "json"]);
    assert_ok(&status, "serve status");
    let payload = json(&status);
    assert_eq!(payload["running"], true, "{payload}");
    assert_eq!(payload["pid"].as_u64(), Some(pid), "{payload}");
    assert_eq!(
        payload["pairing"],
        serde_json::Value::Null,
        "启动后不得持有配对码: {payload}"
    );
    let fingerprint = payload["fingerprint"]
        .as_str()
        .expect("fingerprint")
        .to_owned();
    assert!(!fingerprint.is_empty());
    let address = payload["address"].as_str().expect("address").to_owned();

    // 再起一次必须被拒（退出码 4 = 冲突），而不是又起一个
    let again = served.run(&[
        "remote",
        "serve",
        "start",
        "--listen",
        "127.0.0.1:0",
        "--output",
        "json",
    ]);
    assert!(!again.status.success(), "重复 start 必须失败");
    assert_eq!(again.status.code(), Some(4), "已在运行属于状态冲突");

    // 停止：成功且锁被释放
    let stopped = served.run(&["remote", "serve", "stop", "--output", "json"]);
    assert_ok(&stopped, "serve stop");
    assert_eq!(json(&stopped)["stopped"], true);

    let after = served.run(&["remote", "serve", "status", "--output", "json"]);
    assert_ok(&after, "status after stop");
    assert_eq!(json(&after)["running"], false, "停止后不应仍报运行中");
    assert_eq!(
        json(&after)["pid"],
        serde_json::Value::Null,
        "停止后 pid 文件应被清理"
    );

    // 端口应已不可连：用 status 里的地址探一次
    assert!(
        std::net::TcpStream::connect_timeout(
            &address.parse().expect("addr"),
            std::time::Duration::from_millis(500)
        )
        .is_err(),
        "停止后 {address} 不应仍可连接"
    );
}

/// 后台 agent 刚起来时没有任何码：拿任意码去配对必被拒
///
/// 这是本次设计的验收线：端口长期开着，但**不存在一枚等待被猜中的码**。
#[test]
fn a_freshly_started_background_agent_rejects_every_code() {
    let served = Served::new();
    served.start();

    let status = served.run(&["remote", "serve", "status", "--output", "json"]);
    assert_ok(&status, "serve status");
    let payload = json(&status);
    assert_eq!(payload["running"], true, "{payload}");
    assert_eq!(payload["pairing"], serde_json::Value::Null, "{payload}");

    let address = payload["address"].as_str().expect("address").to_owned();
    for guess in ["0000-0000", "1234-5678", "0000-0001"] {
        let out = served.run(&[
            "remote", "connect", &address, "--code", guess, "--name", "intruder", "-y", "--output",
            "json",
        ]);
        assert!(
            !out.status.success(),
            "无码时 {guess} 竟然配对成功
stdout=[{}]",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    // 失败的 Enroll 不得写进名册
    let after = served.run(&["remote", "serve", "status", "--output", "json"]);
    assert_ok(&after, "serve status after failed attempts");
    assert_eq!(
        json(&after)["clients"].as_array().map(Vec::len),
        Some(0),
        "失败的 Enroll 不得写入名册: {}",
        json(&after)
    );
}

/// 后台 agent 收到 `remote code` 的码后可以配对，且码只出现一次
#[test]
fn a_background_agent_accepts_the_code_minted_locally() {
    let served = Served::new();
    served.start();

    let minted = served.run(&["remote", "code", "--output", "json"]);
    assert_ok(&minted, "remote code");
    let code = json(&minted)["pairing_code"]
        .as_str()
        .expect("pairing_code")
        .to_owned();

    // 码落盘但不含明文：文件里只有摘要
    let status = served.run(&["remote", "serve", "status", "--output", "json"]);
    assert_ok(&status, "serve status");
    let text = json(&status).to_string();
    assert!(!text.contains(&code), "status 不得泄漏配对码: {text}");
    let pairing = &json(&status)["pairing"];
    assert!(pairing["expires_in"].as_u64().unwrap_or(0) > 0, "{text}");

    // 从本机配一次：用真实 CLI 客户端连上后台 agent
    let address = json(&status)["address"]
        .as_str()
        .expect("address")
        .to_owned();
    let connected = served.run(&[
        "remote", "connect", &address, "--code", &code, "--name", "lane", "-y", "--output", "json",
    ]);
    assert_ok(&connected, "connect to the background agent");

    // 配对成功后码被销毁
    let after = served.run(&["remote", "serve", "status", "--output", "json"]);
    assert_ok(&after, "serve status after pairing");
    assert_eq!(
        json(&after)["pairing"],
        serde_json::Value::Null,
        "兑换成功后不应还有码"
    );
}
