//! SSH 接入的端到端链路：假 ssh + 真 agent + 真 CLI 二进制
//!
//! 覆盖 `remote connect user@host → list → ping → reconnect → workspace init
//! --origin → disconnect` 整条用户路径。`ssh` 由 `examples/fake_ssh.rs` 顶替
//! （本机没有 sshd），它实现 CLI 用到的两条命令契约：引导读握手、`-L` 做真
//! TCP 转发。agent 是真的（真 TLS、真名册、真 `service`），所以「隧道通了」
//! 与「命令走隧道连上了对端」是同一件事，而不是打桩打出来的。
//!
//! 与 `cli_remote.rs` 的分工：那边覆盖直连 + 配对码，这边只覆盖 SSH 路径。
//!
//! 集成测试的辅助函数不是 `#[test]`，clippy 的 in-tests 白名单识别不到
#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use tempfile::TempDir;

/// 隔离用临时 HOME（与 `cli_e2e` 同口径）
fn temp_home() -> TempDir {
    let home = TempDir::new().expect("tempdir");
    if cfg!(windows) {
        std::fs::create_dir_all(home.path().join("AppData").join("Roaming"))
            .expect("创建 AppData\\Roaming");
    }
    home
}

/// 假 ssh 可执行文件的路径：用 rustc 现场编译一次，进程内复用
///
/// `cargo test` 不构建 examples（`--test <name>` 只构建选中的目标），而在
/// 测试里再跑 `cargo build` 会按另一套 feature 组合重建整棵依赖树。假 ssh
/// 只用标准库，单独 rustc 编译不到一秒，与 cargo 的决议完全无关。
///
/// 编译产物放在临时目录里，随 [`TempDir`] 存活到测试进程结束。
fn fake_ssh() -> PathBuf {
    static BUILT: std::sync::OnceLock<(TempDir, PathBuf)> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("support")
                .join("fake_ssh.rs");
            assert!(source.exists(), "缺少假 ssh 源码：{}", source.display());
            let dir = TempDir::new().expect("fake ssh build dir");
            let exe = dir.path().join(if cfg!(windows) {
                "fake_ssh.exe"
            } else {
                "fake_ssh"
            });
            let status = Command::new(rustc())
                .arg("--edition")
                .arg("2021")
                .arg("-o")
                .arg(&exe)
                .arg(&source)
                .status()
                .expect("运行 rustc 编译假 ssh");
            assert!(status.success(), "编译假 ssh 失败（status={status:?}）");
            assert!(exe.exists(), "rustc 未产出 {}", exe.display());
            (dir, exe)
        })
        .1
        .clone()
}

/// rustc 路径：与 cargo 同目录（rustup / 官方安装包都是这样布局的）
fn rustc() -> PathBuf {
    let cargo = PathBuf::from(env!("CARGO"));
    let name = if cfg!(windows) { "rustc.exe" } else { "rustc" };
    match cargo.parent().map(|dir| dir.join(name)) {
        Some(candidate) if candidate.exists() => candidate,
        _ => PathBuf::from(name),
    }
}

/// 跑一次 mcsup（非 TTY、隔离 HOME、注入假 ssh 环境）
fn run(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mcsup"));
    command
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("运行 mcsup")
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

/// 一个已绑定并在后台运行的 agent（对端）
struct AgentHandle {
    addr: String,
    code: String,
    fingerprint: String,
    dir: TempDir,
    stop: mcsup::transport::StopHandle,
    join: Option<std::thread::JoinHandle<()>>,
}

impl AgentHandle {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("agent dir");
        let agent = Arc::new(mcsup::remote::Agent::open(dir.path(), "ssh-box").expect("agent"));
        let code = agent.issue_pairing_code().expect("pairing code");
        let fingerprint = agent.fingerprint().as_str().to_owned();
        let server = agent.bind("127.0.0.1:0").expect("bind");
        let addr = server.local_addr().expect("addr").to_string();
        let stop = server.stop_handle().expect("stop");
        let join = std::thread::spawn(move || {
            let _ = server.run();
        });
        Self {
            addr,
            code: code.display_form(),
            fingerprint,
            dir,
            stop,
            join: Some(join),
        }
    }

    /// 假 ssh 需要的环境变量：把「远端」描述成这台 agent
    fn env(&self, log: &Path) -> Vec<(&'static str, String)> {
        vec![
            ("MCSUP_SSH", fake_ssh().to_string_lossy().into_owned()),
            ("MCSUP_REMOTE_BIN", "mcsup".to_owned()),
            ("FAKE_SSH_AGENT_ADDR", self.addr.clone()),
            ("FAKE_SSH_FINGERPRINT", self.fingerprint.clone()),
            ("FAKE_SSH_PAIRING_CODE", self.code.clone()),
            ("FAKE_SSH_SERVER_NAME", "ssh-box".to_owned()),
            ("FAKE_SSH_LOG", log.to_string_lossy().into_owned()),
        ]
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

/// 把 `Vec<(&str, String)>` 变成 [`run`] 需要的借用切片
fn borrowed<'a>(env: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    env.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

#[test]
#[allow(clippy::too_many_lines)]
fn ssh_connect_then_every_command_rebuilds_the_tunnel() {
    let home = temp_home();
    let agent = AgentHandle::start();
    let log = home.path().join("fake-ssh.log");
    let env = agent.env(&log);
    let env = borrowed(&env);
    let home_path = home.path();

    // 1. 首次配对：SSH 引导给配对码，用户只需确认指纹
    let connected = run(
        home_path,
        &[
            "remote",
            "connect",
            "me@ssh-box",
            "--name",
            "box",
            "-y",
            "--output",
            "json",
        ],
        &env,
    );
    assert_ok(&connected, "remote connect user@host");
    let payload = json(&connected);
    assert_eq!(payload["name"], "box");
    assert_eq!(payload["target"], "me@ssh-box");
    assert_eq!(payload["fingerprint"], agent.fingerprint.as_str());

    // 2. 记录落盘为 ssh 接入，且不含 token 本体
    let config = std::fs::read_to_string(home_path.join("share").join("mcsup").join("config.toml"))
        .or_else(|_| {
            std::fs::read_to_string(
                home_path
                    .join("AppData")
                    .join("Roaming")
                    .join("mcsup")
                    .join("config.toml"),
            )
        })
        .expect("config.toml");
    assert!(config.contains("via = \"ssh\""), "{config}");
    assert!(config.contains("target = \"me@ssh-box\""), "{config}");
    assert!(!config.contains(&agent.code), "{config}");

    // 3. 后续每条命令都重新引导 + 建隧道：list 走真连接
    let listed = run(home_path, &["remote", "list", "--output", "json"], &env);
    assert_ok(&listed, "remote list");
    let items = json(&listed);
    let items = items["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "box");
    assert_eq!(items[0]["via"], "ssh");
    assert_eq!(items[0]["state"], "ok");

    // 4. ping 带版本与能力
    let ping = run(
        home_path,
        &["remote", "ping", "box", "--output", "json"],
        &env,
    );
    assert_ok(&ping, "remote ping");
    assert_eq!(json(&ping)["name"], "box");

    // 5. reconnect 复用记录（不再消费配对码）
    let reconnected = run(
        home_path,
        &["remote", "reconnect", "box", "--output", "json"],
        &env,
    );
    assert_ok(&reconnected, "remote reconnect");
    assert_eq!(json(&reconnected)["name"], "box");

    // 6. 业务命令同样走隧道：在对端建工作区并读回
    let ws_dir = agent.dir.path().join("ws");
    let created = run(
        home_path,
        &[
            "workspace",
            "init",
            "--origin",
            "box",
            "--dir",
            ws_dir.to_str().expect("utf8"),
            "--name",
            "w1",
            "--output",
            "json",
        ],
        &env,
    );
    assert_ok(&created, "workspace init --origin box");
    assert_eq!(json(&created)["name"], "w1");

    let workspaces = run(
        home_path,
        &["workspace", "ls", "--origin", "box", "--output", "json"],
        &env,
    );
    assert_ok(&workspaces, "workspace ls --origin box");
    let workspaces = json(&workspaces);
    let workspaces = workspaces.as_array().expect("array");
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0]["origin"], "box");

    // 7. 调用记录：引导与隧道两种形态都出现过，且目标就是 user@host
    let calls = std::fs::read_to_string(&log).expect("fake ssh log");
    assert!(
        calls
            .lines()
            .any(|line| line.contains("__serve-ssh") && line.contains("me@ssh-box")),
        "缺少引导调用：{calls}"
    );
    assert!(
        calls
            .lines()
            .any(|line| line.contains("-L") && line.contains("me@ssh-box")),
        "缺少隧道调用：{calls}"
    );

    // 8. 断开后本地不再有记录
    let disconnected = run(
        home_path,
        &["remote", "disconnect", "box", "--output", "json"],
        &env,
    );
    assert_ok(&disconnected, "remote disconnect");
    let after = run(home_path, &["remote", "list", "--output", "json"], &env);
    assert_ok(&after, "remote list after disconnect");
    assert_eq!(json(&after)["items"].as_array().expect("items").len(), 0);
}

/// SSH 不可用：配对必须失败并说清是握手没拿到，而不是挂死
#[test]
fn ssh_bootstrap_failure_reports_the_handshake_error() {
    let home = temp_home();
    let log = home.path().join("fake-ssh.log");
    let fake = fake_ssh();
    let env = vec![
        ("MCSUP_SSH", fake.to_string_lossy().into_owned()),
        ("FAKE_SSH_FAIL", "1".to_owned()),
        ("FAKE_SSH_LOG", log.to_string_lossy().into_owned()),
    ];
    let env = borrowed(&env);
    let out = run(
        home.path(),
        &[
            "remote",
            "connect",
            "me@broken-host",
            "--name",
            "broken",
            "-y",
            "--output",
            "json",
        ],
        &env,
    );
    assert!(!out.status.success(), "SSH 失败必须返回非零退出码");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("握手") || stderr.contains("handshake"),
        "错误必须指出握手没拿到: {stderr}"
    );
}

/// SSH 记录连不上时，list 报 unreachable 而不是让整条命令失败
#[test]
fn unreachable_ssh_remote_degrades_to_unreachable_in_list() {
    let home = temp_home();
    let log = home.path().join("fake-ssh.log");
    let agent = AgentHandle::start();
    let env = agent.env(&log);
    let env = borrowed(&env);

    // 先正常配对，拿到记录
    assert_ok(
        &run(
            home.path(),
            &[
                "remote",
                "connect",
                "me@ssh-box",
                "--name",
                "box",
                "-y",
                "--output",
                "json",
            ],
            &env,
        ),
        "connect",
    );

    // 之后 SSH 坏掉：隧道建不起来，list 仍成功，状态降级为 unreachable
    let mut broken = env.clone();
    broken.push(("FAKE_SSH_FAIL", "1"));
    let listed = run(
        home.path(),
        &["remote", "list", "--output", "json"],
        &broken,
    );
    assert_ok(&listed, "remote list with broken ssh");
    let items = json(&listed);
    let items = items["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["state"], "unreachable");
}
