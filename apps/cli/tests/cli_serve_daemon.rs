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
///
/// **不用管道、也不用 `Command::output()`**。`output()` 会读完 stdout 与
/// stderr 到 EOF 才返回，而这里测的 `serve start` 会派生一个**长期存活**的
/// 后台 agent；只要那个 agent 手里有管道写端的副本，EOF 就永远不会来。
///
/// 在 Windows 上它确实可能拿到，这是 CI 里「running for over 60 seconds」的
/// 最可能解释：std 的 `CreateProcessW` 传的是 `bInheritHandles = TRUE`
/// （`sys/process/windows.rs` 里 `inherit_handles` 默认为 `true`），父进程的
/// 可继承句柄会被复制进子进程；而子进程的 stdout/stderr 虽已重定向到日志，
/// 那三个标准句柄仍以 `Stdio::Null` 参与创建，于是长期存活的 agent 可能持有
/// 测试进程 stdout 的副本，管道便永远等不到 EOF。Unix 上 spawn 只 dup 0/1/2、
/// 其余带 CLOEXEC，所以本机复现不出来（实测 `.output()` 版本 1.4s 通过）。
///
/// 这条链路是**代码审读结论，未在 Windows 上实测**（本机无 Windows 执行环境，
/// 交叉编译也过不了 `ring` 的 C 构建）。因此下面的写法不依赖上述解释成立：
/// 换成重定向到**文件**后，无论句柄是否被继承、是否泄漏，`wait()` 一返回就
/// 能读到内容，不涉及任何「等 EOF」或超时。修的是可观测性，不是猜某个平台
/// 细节——即使真实原因另有其它，这版也不会挂。
fn run(home: &Path, args: &[&str]) -> Output {
    // 放子目录：HOME 根目录会被 mcsup 当成工作区候选扫到，探针不该出现在那里
    let probe = home.join(".probe");
    std::fs::create_dir_all(&probe).expect("创建探针目录");
    let out_path = probe.join("stdout");
    let err_path = probe.join("stderr");
    let out_file = std::fs::File::create(&out_path).expect("创建 stdout 文件");
    let err_file = std::fs::File::create(&err_path).expect("创建 stderr 文件");

    let status = Command::new(env!("CARGO_BIN_EXE_mcsup"))
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
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .status()
        .expect("运行 mcsup");

    Output {
        status,
        stdout: std::fs::read(&out_path).unwrap_or_default(),
        stderr: std::fs::read(&err_path).unwrap_or_default(),
    }
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

/// 反复 start / stop 之后状态必须是干净的：不得残留「已在运行」
///
/// 这是**症状级**的守卫，不是竞态的确定性复现。曾经的竞态是：就绪探测走
/// `InstanceLock::is_held`，而它内部是 `try_lock`——为了回答「有没有人持锁」
/// 必须先真的去拿一次。父进程每 50ms 探一次，子进程此时正在 `acquire`，撞上
/// 就被子进程判成「别人持有」而退出，`start` 随后报「本机 agent 已在运行」，
/// 可实际上谁也没在跑（详见 `serve_start` 处的注释）。
///
/// 那个窗口只有几微秒，靠单进程顺序 start/stop 撞不出来——它是在全量并行
/// 跑（45 个测试目标抢 CPU、父进程被抢占后持锁窗口被拉大）时才暴露的。所以
/// 这个测试只能在那一类负载下真正起到抓回退的作用；单跑时它证明的是「反复
/// 起停后状态收敛」，这本身也值得守住。
#[test]
fn repeated_start_stop_never_reports_a_phantom_agent() {
    let served = Served::new();

    for round in 1..=5 {
        let started = served.run(&[
            "remote",
            "serve",
            "start",
            "--listen",
            "127.0.0.1:0",
            "--output",
            "json",
        ]);
        assert_ok(&started, &format!("第 {round} 轮 start"));

        let stopped = served.run(&["remote", "serve", "stop", "--output", "json"]);
        assert_ok(&stopped, &format!("第 {round} 轮 stop"));

        // 停完之后必须真的没人持锁：下一轮的 start 才有意义
        let status = served.run(&["remote", "serve", "status", "--output", "json"]);
        if status.status.success() {
            assert_eq!(
                json(&status)["running"],
                false,
                "第 {round} 轮 stop 之后仍报运行中: {}",
                json(&status)
            );
        }
    }
}
