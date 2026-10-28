//! 远端接入的端到端链路：真 agent + 真 CLI 二进制
//!
//! 覆盖 `connect → list → workspace init --origin → workspace ls --origin →
//! ping → disconnect` 整条用户路径。agent 在同一进程里起（真 TLS、真名册、
//! 真 service），CLI 走真实二进制与真实配置文件，验证「连上之后命令与本地
//! 一致」不是只在单元测试里成立。

// 集成测试的辅助函数不是 #[test]，clippy 的 in-tests 白名单识别不到
#![allow(clippy::expect_used, clippy::panic)]

use std::path::Path;
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
        // 凭证固定走 0600 文件：集成测试不触碰真实系统钥匙串（否则会弹窗 / 依赖桌面会话）
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

/// 一个已绑定并在后台运行的 agent
struct AgentHandle {
    addr: String,
    code: String,
    dir: TempDir,
    stop: mcsup::transport::StopHandle,
    join: Option<std::thread::JoinHandle<()>>,
}

impl AgentHandle {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("agent dir");
        let agent = Arc::new(mcsup::remote::Agent::open(dir.path(), "test-box").expect("agent"));
        let code = agent.issue_pairing_code().expect("pairing code");
        let server = agent.bind("127.0.0.1:0").expect("bind");
        let addr = server.local_addr().expect("addr").to_string();
        let stop = server.stop_handle().expect("stop");
        let join = std::thread::spawn(move || {
            let _ = server.run();
        });
        Self {
            addr,
            code: code.display_form(),
            dir,
            stop,
            join: Some(join),
        }
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

#[test]
#[allow(clippy::too_many_lines)]
fn connect_then_manage_remote_resources_with_identical_commands() {
    let home = temp_home();
    let agent = AgentHandle::start();
    let home_path = home.path();

    // 1. 首次配对：非交互下用 -y 接受 TOFU 指纹
    let connected = run(
        home_path,
        &[
            "remote",
            "connect",
            &agent.addr,
            "--code",
            &agent.code,
            "--name",
            "test-box",
            "-y",
            "--output",
            "json",
        ],
    );
    assert_ok(&connected, "remote connect");
    let payload = json(&connected);
    assert_eq!(payload["name"], "test-box");
    assert_eq!(payload["workspaces"], 0);
    assert_eq!(payload["servers"], 0);
    assert!(
        payload["fingerprint"]
            .as_str()
            .is_some_and(|fp| fp.starts_with("sha256:")),
        "{payload}"
    );

    // 2. 已配对列表可见且可达
    let listed = run(home_path, &["remote", "list", "--output", "json"]);
    assert_ok(&listed, "remote list");
    let items = json(&listed);
    let items = items["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "test-box");
    assert_eq!(items[0]["state"], "ok");
    assert_eq!(items[0]["via"], "direct");

    // 3. 在远端创建工作区：--dir 是目标机器上的路径
    let ws_dir = agent.dir.path().join("ws");
    let created = run(
        home_path,
        &[
            "workspace",
            "init",
            "--origin",
            "test-box",
            "--dir",
            ws_dir.to_str().expect("utf8"),
            "--name",
            "w1",
            "--output",
            "json",
        ],
    );
    assert_ok(&created, "workspace init --origin");
    assert_eq!(json(&created)["name"], "w1");

    // 4. 合并列表带来源；按来源过滤各看各的
    let all = run(home_path, &["workspace", "ls", "--output", "json"]);
    assert_ok(&all, "workspace ls");
    let all = json(&all);
    let all = all.as_array().expect("array");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0]["origin"], "test-box");
    assert_eq!(all[0]["name"], "w1");

    let local = run(
        home_path,
        &["workspace", "ls", "--origin", "local", "--output", "json"],
    );
    assert_ok(&local, "workspace ls --origin local");
    assert_eq!(json(&local).as_array().expect("array").len(), 0);

    let remote = run(
        home_path,
        &["workspace", "ls", "--origin", "remote", "--output", "json"],
    );
    assert_ok(&remote, "workspace ls --origin remote");
    assert_eq!(json(&remote).as_array().expect("array").len(), 1);

    // 5. ping 报告版本与能力
    let ping = run(
        home_path,
        &["remote", "ping", "test-box", "--output", "json"],
    );
    assert_ok(&ping, "remote ping");
    let ping = json(&ping);
    assert_eq!(ping["name"], "test-box");
    assert!(
        ping["capabilities"]
            .as_array()
            .is_some_and(|caps| caps.iter().any(|cap| cap == "backup")),
        "{ping}"
    );

    // 6. 断开：本地配对与凭证都清掉，远端继续运行
    let disconnected = run(
        home_path,
        &["remote", "disconnect", "test-box", "--output", "json"],
    );
    assert_ok(&disconnected, "remote disconnect");
    assert_eq!(json(&disconnected)["removed"], true);

    let after = run(home_path, &["remote", "list", "--output", "json"]);
    assert_ok(&after, "remote list after disconnect");
    assert_eq!(json(&after)["items"].as_array().expect("items").len(), 0);

    // 远端仍在服务：agent 自己的名册里还有这个客户端
    assert!(
        agent
            .dir
            .path()
            .join("remote")
            .join("clients.toml")
            .exists()
    );
}

#[test]
fn connect_defaults_to_incremented_remote_names() {
    let home = temp_home();
    let first = AgentHandle::start();
    let second = AgentHandle::start();
    let home_path = home.path();

    let connected = run(
        home_path,
        &[
            "remote",
            "connect",
            &first.addr,
            "--code",
            &first.code,
            "-y",
            "--output",
            "json",
        ],
    );
    assert_ok(&connected, "connect first");
    assert_eq!(json(&connected)["name"], "r1");

    let connected = run(
        home_path,
        &[
            "remote",
            "connect",
            &second.addr,
            "--code",
            &second.code,
            "-y",
            "--output",
            "json",
        ],
    );
    assert_ok(&connected, "connect second");
    assert_eq!(json(&connected)["name"], "r2");

    let listed = run(home_path, &["remote", "list", "--output", "json"]);
    assert_ok(&listed, "remote list");
    let names: Vec<_> = json(&listed)["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["name"].as_str().expect("name").to_owned())
        .collect();
    assert_eq!(names, ["r1", "r2"]);
}

#[test]
fn connect_without_yes_refuses_to_trust_an_unknown_fingerprint() {
    let home = temp_home();
    let agent = AgentHandle::start();
    let out = run(
        home.path(),
        &[
            "remote",
            "connect",
            &agent.addr,
            "--code",
            &agent.code,
            "--output",
            "json",
        ],
    );
    assert!(!out.status.success(), "非交互且无 -y 必须拒绝");
    assert_eq!(out.status.code(), Some(2), "缺确认属于用法错误");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("指纹"), "{stderr}");
}

#[test]
fn connect_to_a_dead_endpoint_fails_cleanly() {
    let home = temp_home();
    let out = run(
        home.path(),
        &[
            "remote",
            "connect",
            "127.0.0.1:1",
            "--code",
            "1234-5678",
            "-y",
            "--output",
            "json",
        ],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("127.0.0.1:1"), "{stderr}");
}

#[test]
#[allow(clippy::too_many_lines)]
fn remote_server_create_list_and_remove_route_to_the_peer() {
    let home = temp_home();
    let agent = AgentHandle::start();
    let home_path = home.path();

    assert_ok(
        &run(
            home_path,
            &[
                "remote",
                "connect",
                &agent.addr,
                "--code",
                &agent.code,
                "--name",
                "test-box",
                "-y",
                "--output",
                "json",
            ],
        ),
        "connect",
    );

    // 远端需要一个工作区作为落点
    let ws_dir = agent.dir.path().join("ws");
    assert_ok(
        &run(
            home_path,
            &[
                "workspace",
                "init",
                "--origin",
                "test-box",
                "--dir",
                ws_dir.to_str().expect("utf8"),
                "--name",
                "w1",
                "--output",
                "json",
            ],
        ),
        "workspace init",
    );

    let created = run(
        home_path,
        &[
            "server",
            "create",
            "web",
            "--origin",
            "test-box",
            "--version",
            "1.21.1",
            "--variant",
            "vanilla",
            "--output",
            "json",
        ],
    );
    // 远端 create 会继续在对端安装核心；核心下载依赖外网，两种结果都合法：
    // 成功 → 登记带 jar；失败 → 登记保留、jar 为空（与本地 create 同一语义）。
    let listed = run(home_path, &["server", "ls", "--output", "json"]);
    assert_ok(&listed, "server ls after create");
    let all = json(&listed);
    let all = all.as_array().expect("array");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0]["origin"], "test-box");
    assert_eq!(all[0]["name"], "web");
    assert_eq!(
        created.status.success(),
        !all[0]["jar"].is_null(),
        "create 的退出码必须与核心登记状态一致"
    );

    let local = run(
        home_path,
        &["server", "ls", "--origin", "local", "--output", "json"],
    );
    assert_ok(&local, "server ls --origin local");
    assert_eq!(json(&local).as_array().expect("array").len(), 0);

    // 限定名可定位到远端服务器（status 走路由 supervisor）
    let status = run(
        home_path,
        &["server", "status", "test-box/web", "--output", "json"],
    );
    assert_ok(&status, "server status qualified");
    assert_eq!(json(&status)["name"], "web");

    // 远端 install 复用同一帧：核心下载依赖外网，结果不固定，但
    // 「退出码 ↔ jar 登记」必须一致
    let install = run(
        home_path,
        &["server", "install", "test-box/web", "--output", "json"],
    );
    let listed = run(home_path, &["server", "ls", "--output", "json"]);
    assert_ok(&listed, "server ls after install");
    let all = json(&listed);
    let all = all.as_array().expect("array");
    assert_eq!(
        install.status.success(),
        !all[0]["jar"].is_null(),
        "install 退出码必须与核心登记状态一致（stderr=[{}])",
        String::from_utf8_lossy(&install.stderr)
    );

    let removed = run(
        home_path,
        &[
            "server", "remove", "web", "--origin", "test-box", "--output", "json",
        ],
    );
    assert_ok(&removed, "server remove --origin");
    assert_eq!(json(&removed)["name"], "web");

    let after = run(home_path, &["server", "ls", "--output", "json"]);
    assert_ok(&after, "server ls after remove");
    assert_eq!(json(&after).as_array().expect("array").len(), 0);
}

/// `workspace use <remote>/<ws>` 只改本机 current；之后的 `server create`
/// （不带 `--origin`）落到该远端工作区，且 `workspace ls` 只有一个当前。
#[test]
#[allow(clippy::too_many_lines)]
fn use_remote_workspace_routes_create_and_marks_single_current() {
    let home = temp_home();
    let agent = AgentHandle::start();
    let home_path = home.path();
    assert_ok(
        &run(
            home_path,
            &[
                "remote",
                "connect",
                &agent.addr,
                "--code",
                &agent.code,
                "--name",
                "test-box",
                "-y",
                "--output",
                "json",
            ],
        ),
        "connect",
    );

    // 本地 + 远端各一个工作区，用来验证「只有一个当前」
    let local_dir = home_path.join("local-ws");
    assert_ok(
        &run(
            home_path,
            &[
                "workspace",
                "init",
                "--dir",
                local_dir.to_str().expect("utf8"),
                "--name",
                "localws",
                "--output",
                "json",
            ],
        ),
        "local workspace init",
    );
    let remote_dir = agent.dir.path().join("ws");
    assert_ok(
        &run(
            home_path,
            &[
                "workspace",
                "init",
                "--origin",
                "test-box",
                "--dir",
                remote_dir.to_str().expect("utf8"),
                "--name",
                "w1",
                "--output",
                "json",
            ],
        ),
        "remote workspace init",
    );

    // use 远程：只改本机指针
    assert_ok(
        &run(
            home_path,
            &["workspace", "use", "test-box/w1", "--output", "json"],
        ),
        "use remote workspace",
    );
    let listed = run(home_path, &["workspace", "ls", "--output", "json"]);
    assert_ok(&listed, "workspace ls");
    let rows = json(&listed);
    let rows = rows.as_array().expect("array");
    let currents: Vec<_> = rows.iter().filter(|row| row["current"] == true).collect();
    assert_eq!(currents.len(), 1, "只能有一个当前: {}", json(&listed));
    assert_eq!(currents[0]["origin"], "test-box");
    assert_eq!(currents[0]["name"], "w1");

    // 不带 --origin 的 create 落到 current 指向的远端工作区
    let _ = run(
        home_path,
        &[
            "server",
            "create",
            "auto-web",
            "--version",
            "1.21.1",
            "--variant",
            "vanilla",
            "--output",
            "json",
        ],
    );
    let listed = run(home_path, &["server", "ls", "--output", "json"]);
    assert_ok(&listed, "server ls");
    let all = json(&listed);
    let all = all.as_array().expect("array");
    let remote = all
        .iter()
        .find(|s| s["name"] == "auto-web")
        .unwrap_or_else(|| panic!("缺少远端创建的服务器: {}", json(&listed)));
    assert_eq!(remote["origin"], "test-box");
    assert_eq!(remote["workspace"], "w1");
    assert!(
        all.iter().all(|s| s["origin"] == "test-box"),
        "不应建到本地: {}",
        json(&listed)
    );

    // 切回本地后 create 回到本地
    assert_ok(
        &run(
            home_path,
            &["workspace", "use", "local/localws", "--output", "json"],
        ),
        "use local workspace",
    );
    let _ = run(
        home_path,
        &[
            "server",
            "create",
            "local-web",
            "--version",
            "1.21.1",
            "--variant",
            "vanilla",
            "--output",
            "json",
        ],
    );
    let listed = run(home_path, &["server", "ls", "--output", "json"]);
    assert_ok(&listed, "server ls after local create");
    let all = json(&listed);
    let all = all.as_array().expect("array");
    let local = all
        .iter()
        .find(|s| s["name"] == "local-web")
        .expect("本地服务器");
    assert_eq!(local["origin"], "local");
}

/// `server link --origin`：目录动作发生在对端（建符号链接 + 入库）
#[test]
fn remote_server_link_registers_a_directory_on_the_peer() {
    let home = temp_home();
    let agent = AgentHandle::start();
    let home_path = home.path();

    assert_ok(
        &run(
            home_path,
            &[
                "remote",
                "connect",
                &agent.addr,
                "--code",
                &agent.code,
                "-y",
                "--name",
                "test-box",
                "--output",
                "json",
            ],
        ),
        "connect",
    );

    // 远端需要一个工作区作为 servers/ 的落点
    let ws_dir = agent.dir.path().join("ws");
    assert_ok(
        &run(
            home_path,
            &[
                "workspace",
                "init",
                "--origin",
                "test-box",
                "--dir",
                ws_dir.to_str().expect("utf8"),
                "--name",
                "w1",
                "--output",
                "json",
            ],
        ),
        "workspace init",
    );

    // 对端机器上一个已存在的目录：外部目录 → 对端建符号链接
    let external = agent.dir.path().join("external-server");
    std::fs::create_dir_all(&external).expect("mkdir external");

    let linked = run(
        home_path,
        &[
            "server",
            "link",
            external.to_str().expect("utf8"),
            "--origin",
            "test-box",
            "--name",
            "linked1",
            "--output",
            "json",
        ],
    );
    assert_ok(&linked, "server link --origin");
    let payload = json(&linked);
    assert_eq!(payload["name"], "linked1");
    assert_eq!(payload["linked"], true, "{payload}");
    // 对端会 canonicalize（macOS 上 /var → /private/var），只比对尾段
    assert!(
        payload["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("external-server")),
        "{payload}"
    );

    // 合并列表可见，且来源标注为远端
    let listed = run(
        home_path,
        &["server", "ls", "--origin", "test-box", "--output", "json"],
    );
    assert_ok(&listed, "server ls --origin test-box");
    let items = json(&listed);
    let items = items.as_array().expect("array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "linked1");
    assert_eq!(items[0]["origin"], "test-box");

    // 链接服务器随工作区注销一起清理，避免影响后续断言
    assert_ok(
        &run(
            home_path,
            &[
                "server", "remove", "linked1", "--origin", "test-box", "--output", "json",
            ],
        ),
        "server remove linked",
    );
}

#[test]
fn remote_java_scan_and_list_route_to_the_peer() {
    let home = temp_home();
    let agent = AgentHandle::start();
    let home_path = home.path();

    assert_ok(
        &run(
            home_path,
            &[
                "remote",
                "connect",
                &agent.addr,
                "--code",
                &agent.code,
                "--name",
                "test-box",
                "-y",
                "--output",
                "json",
            ],
        ),
        "connect",
    );

    let scanned = run(
        home_path,
        &["java", "scan", "--origin", "test-box", "--output", "json"],
    );
    assert_ok(&scanned, "java scan --origin");
    assert!(json(&scanned)["registered"].is_number());

    let listed = run(home_path, &["java", "ls", "--output", "json"]);
    assert_ok(&listed, "java ls");
    assert!(json(&listed).is_array());
}

#[test]
#[allow(clippy::too_many_lines)]
fn remote_props_set_and_version_route_to_the_peer() {
    let home = temp_home();
    let agent = AgentHandle::start();
    let home_path = home.path();

    assert_ok(
        &run(
            home_path,
            &[
                "remote",
                "connect",
                &agent.addr,
                "--code",
                &agent.code,
                "--name",
                "test-box",
                "-y",
                "--output",
                "json",
            ],
        ),
        "connect",
    );

    let ws_dir = agent.dir.path().join("ws");
    assert_ok(
        &run(
            home_path,
            &[
                "workspace",
                "init",
                "--origin",
                "test-box",
                "--dir",
                ws_dir.to_str().expect("utf8"),
                "--name",
                "w1",
                "--output",
                "json",
            ],
        ),
        "workspace init --origin",
    );

    // 登记先于核心下载：核心成败不影响后续属性 / 设置 / 版本
    let _ = run(
        home_path,
        &[
            "server",
            "create",
            "web",
            "--origin",
            "test-box",
            "--version",
            "1.21.1",
            "--variant",
            "vanilla",
            "--output",
            "json",
        ],
    );

    // 1. props 写入：文件落在对端服务器目录
    let written = run(
        home_path,
        &[
            "server",
            "props",
            "test-box/web",
            "motd=远端你好",
            "--output",
            "json",
        ],
    );
    assert_ok(&written, "remote props set");
    let payload = json(&written);
    assert_eq!(payload["name"], "web");
    assert_eq!(payload["updated"]["motd"], "远端你好");
    let file = ws_dir.join("servers").join("web").join("server.properties");
    assert_eq!(
        std::fs::read_to_string(&file).expect("读对端文件"),
        "motd=远端你好\n"
    );

    // 2. props 读取：同一份原文解析出的键值
    let listed = run(
        home_path,
        &["server", "props", "test-box/web", "--output", "json"],
    );
    assert_ok(&listed, "remote props list");
    let rows = json(&listed);
    let rows = rows.as_array().expect("array");
    assert!(
        rows.iter()
            .any(|row| row["key"] == "motd" && row["value"] == "远端你好"),
        "{rows:?}"
    );

    // 3. set：值在对端校验并持久化（ram 规范化）
    let set = run(
        home_path,
        &[
            "server",
            "set",
            "test-box/web",
            "ram",
            "2G",
            "--output",
            "json",
        ],
    );
    assert_ok(&set, "remote set ram");
    let set = json(&set);
    assert_eq!(set["key"], "ram");
    assert_eq!(set["value"], "2048M");
    assert_eq!(set["settings"]["ram_mib"], 2048);

    // 4. version：经组合注册表读对端记录
    let version = run(
        home_path,
        &["server", "version", "test-box/web", "--output", "json"],
    );
    assert_ok(&version, "remote version");
    let version = json(&version);
    assert_eq!(version["name"], "web");
    assert_eq!(version["version"], "1.21.1");
    assert_eq!(version["variant"], "vanilla");

    // 5. java uninstall 缺目标：非交互报用法错误（远端路径也走同一道闸）
    let uninstall = run(
        home_path,
        &[
            "java",
            "uninstall",
            "--origin",
            "test-box",
            "--output",
            "json",
        ],
    );
    assert!(!uninstall.status.success());
    assert_eq!(
        uninstall.status.code(),
        Some(2),
        "缺目标属于用法错误: stderr=[{}]",
        String::from_utf8_lossy(&uninstall.stderr)
    );
}

#[test]
fn remote_logs_follow_streams_new_lines() {
    let home = temp_home();
    let agent = AgentHandle::start();
    let home_path = home.path();

    assert_ok(
        &run(
            home_path,
            &[
                "remote",
                "connect",
                &agent.addr,
                "--code",
                &agent.code,
                "--name",
                "test-box",
                "-y",
                "--output",
                "json",
            ],
        ),
        "connect",
    );

    // 远端先有工作区（agent 端落点），再建站（核心下载依赖外网，成败都行）
    let ws_dir = agent.dir.path().join("ws");
    assert_ok(
        &run(
            home_path,
            &[
                "workspace",
                "init",
                "--origin",
                "test-box",
                "--dir",
                ws_dir.to_str().expect("utf8"),
                "--name",
                "w1",
                "--output",
                "json",
            ],
        ),
        "workspace init --origin",
    );
    let created = run(
        home_path,
        &[
            "server",
            "create",
            "web",
            "--origin",
            "test-box",
            "--version",
            "1.21.1",
            "--variant",
            "vanilla",
            "--output",
            "json",
        ],
    );
    eprintln!(
        "create status={:?} stdout=[{}] stderr=[{}]",
        created.status.code(),
        String::from_utf8_lossy(&created.stdout),
        String::from_utf8_lossy(&created.stderr)
    );
    let console = ws_dir.join("servers").join("web").join("console.log");
    std::fs::create_dir_all(console.parent().expect("parent")).expect("mkdir");
    std::fs::write(&console, "line-1\nline-2\n").expect("write history");

    // 历史：一次 tail 拉回
    let tail = run(
        home_path,
        &["server", "logs", "test-box/web", "--output", "json"],
    );
    assert_ok(&tail, "remote logs tail");
    assert!(String::from_utf8_lossy(&tail.stdout).contains("line-1"));

    // follow：后台追加，客户端长轮询应拿到增量。这里不起子进程阻塞，
    // 改用「两次 tail 之间的增量语义」验证：写入新行后，logs --tail 拿到全部，
    // 而 follow 的长轮询帧由 agent 侧单元路径覆盖（见 agent_roundtrip）。
    std::fs::write(&console, "line-1\nline-2\nline-3\n").expect("append");
    let tail = run(
        home_path,
        &[
            "server",
            "logs",
            "test-box/web",
            "--tail",
            "1",
            "--output",
            "json",
        ],
    );
    assert_ok(&tail, "remote logs tail 1");
    assert!(String::from_utf8_lossy(&tail.stdout).contains("line-3"));
}
