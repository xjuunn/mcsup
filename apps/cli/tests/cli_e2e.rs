//! CLI 端到端测试：真实临时目录 + 真实二进制，走完整命令链路
//!
//! 与 `cli_contract` 的分工：那边断言「输出形状与退出码」这一层契约，这边断言
//! 「命令串起来之后磁盘与注册表是否一致」。区别在于每个用例都建一个真实工作区，
//! 并检查落盘结果（`servers/<name>/mcsup.json`、目录是否存在），而不只看 stdout。
//!
//! 隔离手段与 `cli_contract` 相同：`HOME` / `USERPROFILE` / `XDG_*` 全部指向临时
//! 目录，stdin 置空保证非 TTY。**不依赖网络**：需要下载核心的路径在空的临时
//! `current_dir` 下运行，下载失败时断言的是「失败后注册表与磁盘保持一致」。

// 集成测试的辅助函数不是 #[test]，clippy 的 in-tests 白名单识别不到
#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

/// 隔离用临时 HOME（Windows 需预建 AppData\Roaming，否则数据目录解析失败）
fn temp_home() -> TempDir {
    let home = TempDir::new().expect("tempdir");
    if cfg!(windows) {
        std::fs::create_dir_all(home.path().join("AppData").join("Roaming"))
            .expect("创建 AppData\\Roaming");
    }
    home
}

/// 不可达代理：让下载走确定的失败路径，不依赖本机网络与服务器时间
const DEAD_PROXY: &str = "http://127.0.0.1:1";

/// 跑一次 mcsup；`cwd` 为子进程工作目录，`None` 表示继承
fn run_in(home: &Path, cwd: Option<&Path>, args: &[&str]) -> Output {
    run_full(home, cwd, args, &[])
}

/// 带额外环境变量的运行；语言相关断言必须显式给 `--lang`，否则随机器 locale 变
fn run_in_env(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    run_full(home, None, args, envs)
}

fn run_full(home: &Path, cwd: Option<&Path>, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mcsup"));
    cmd.args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_DATA_HOME", home.join("share"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env_remove("MCSUP_LANG")
        .env_remove("LANG")
        .env_remove("LC_ALL")
        .env_remove("LC_CTYPE")
        // 继承来的代理会让下载行为随环境漂移，一律清掉再由用例显式给出
        .env_remove("HTTPS_PROXY")
        .env_remove("HTTP_PROXY")
        .env_remove("ALL_PROXY")
        .env("MCSUP_WORKSPACE", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().expect("运行 mcsup")
}

fn run(home: &Path, args: &[&str]) -> Output {
    run_in(home, None, args)
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout_json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout 必须是 JSON: {err}\nstdout=[{}]\nstderr=[{}]",
            String::from_utf8_lossy(&out.stdout),
            stderr(out)
        )
    })
}

fn ok(out: &Output, what: &str) {
    assert_eq!(out.status.code(), Some(0), "{what} 失败: {}", stderr(out));
}

/// 建一个真实工作区，返回其路径
fn init_workspace(home: &TempDir, name: &str) -> PathBuf {
    let ws = home.path().join(name);
    let out = run(
        home.path(),
        &[
            "workspace",
            "init",
            "--dir",
            ws.to_str().expect("utf8"),
            "--name",
            name,
            "--output",
            "json",
        ],
    );
    ok(&out, "workspace init");
    ws
}

/// 工作区内某服务器的 marker 路径
fn marker_of(ws: &Path, server: &str) -> PathBuf {
    ws.join("servers").join(server).join("mcsup.json")
}

/// 全链路：init → create（下载失败）→ list → 注册表与磁盘保持一致
///
/// 用 `HTTPS_PROXY` 指向不可达地址让下载确定失败（reqwest 读标准代理变量），
/// 不依赖本机是否联网、也不依赖服务器时间。失败时 `create` 的约定是
/// **保留已注册结果**（提示用 `server install` 重试），不是回滚——所以断言的是
/// 「列表有记录就必须有 marker，且 jar 为空」，而不是「什么都没留下」。
///
/// 这条用例拦的是：落库成功但 marker 缺失、列表与磁盘不一致、失败后退出码变成
/// 用法码这类问题。它比契约测试强的地方在于真的执行了 create 的完整流程。
#[test]
fn create_with_download_failure_keeps_registry_and_disk_in_sync() {
    let home = temp_home();
    let ws = init_workspace(&home, "ws1");

    let create = run_in_env(
        home.path(),
        &[
            "server",
            "create",
            "srv1",
            "--version",
            "1.21.1",
            "--output",
            "json",
        ],
        &[
            ("HTTPS_PROXY", DEAD_PROXY),
            ("HTTP_PROXY", DEAD_PROXY),
            ("ALL_PROXY", DEAD_PROXY),
        ],
    );
    assert_eq!(
        create.status.code(),
        Some(5),
        "下载失败应是后端失败 5，stderr: {}",
        stderr(&create)
    );

    // 失败路径不得往 stdout 写数据（机器契约）
    assert!(
        create.stdout.is_empty(),
        "失败时 stdout 应为空: {}",
        String::from_utf8_lossy(&create.stdout)
    );

    let list = run(home.path(), &["server", "list", "--output", "json"]);
    ok(&list, "server list");
    let parsed = stdout_json(&list);
    let rows = parsed.as_array().expect("list 是数组");
    assert_eq!(rows.len(), 1, "create 失败应保留注册结果: {parsed}");
    assert_eq!(rows[0]["name"], "srv1");
    assert!(rows[0]["jar"].is_null(), "下载失败时 jar 应为 null");

    let dir = ws.join("servers").join("srv1");
    assert!(dir.is_dir(), "已注册但目录不存在: {}", dir.display());
    assert!(
        marker_of(&ws, "srv1").is_file(),
        "已注册但缺少 mcsup.json，磁盘与注册表不一致"
    );
}

/// Fabric 变体走的是与 vanilla 相同的链路，但来源换成 Fabric：显式 `--variant
/// fabric` 必须被识别（而不是回落到 vanilla），下载失败时同样保留注册结果。
#[test]
fn create_fabric_variant_keeps_registry_on_download_failure() {
    let home = temp_home();
    let ws = init_workspace(&home, "ws1");

    let create = run_in_env(
        home.path(),
        &[
            "server",
            "create",
            "fab1",
            "--version",
            "1.21.1",
            "--variant",
            "fabric",
            "--output",
            "json",
        ],
        &[
            ("HTTPS_PROXY", DEAD_PROXY),
            ("HTTP_PROXY", DEAD_PROXY),
            ("ALL_PROXY", DEAD_PROXY),
        ],
    );
    // 变体必须是已注册能力：未支持会以用法错误 2 退出，下载失败才是 5
    assert_eq!(
        create.status.code(),
        Some(5),
        "fabric 变体应被识别并进入下载（失败为 5），stderr: {}",
        stderr(&create)
    );
    assert!(create.stdout.is_empty(), "失败时 stdout 应为空");

    let list = run(home.path(), &["server", "list", "--output", "json"]);
    ok(&list, "server list");
    let parsed = stdout_json(&list);
    let rows = parsed.as_array().expect("list 是数组");
    assert_eq!(rows.len(), 1, "create 失败应保留注册结果: {parsed}");
    assert_eq!(rows[0]["name"], "fab1");
    assert_eq!(rows[0]["variant"], "fabric", "变体必须如实记录");
    assert!(marker_of(&ws, "fab1").is_file(), "磁盘与注册表不一致");
}

/// Forge 变体必须被识别并进入安装流程：显式 `--variant forge` 未注册时会以
/// 用法错误 2 退出，已注册则走到下载（离线失败为 5）；失败同样保留注册结果。
///
/// Forge 的下载物是安装器，但离线环境下连元数据都取不到，压根不会进入
/// 「执行安装器」阶段——本测试只覆盖变体注册与失败映射，不触碰真实安装。
#[test]
fn create_forge_variant_keeps_registry_on_download_failure() {
    let home = temp_home();
    let ws = init_workspace(&home, "ws1");

    let create = run_in_env(
        home.path(),
        &[
            "server",
            "create",
            "forge1",
            "--version",
            "1.21.1",
            "--variant",
            "forge",
            "--output",
            "json",
        ],
        &[
            ("HTTPS_PROXY", DEAD_PROXY),
            ("HTTP_PROXY", DEAD_PROXY),
            ("ALL_PROXY", DEAD_PROXY),
        ],
    );
    assert_eq!(
        create.status.code(),
        Some(5),
        "forge 变体应被识别并进入下载（失败为 5），stderr: {}",
        stderr(&create)
    );
    assert!(create.stdout.is_empty(), "失败时 stdout 应为空");

    let list = run(home.path(), &["server", "list", "--output", "json"]);
    ok(&list, "server list");
    let parsed = stdout_json(&list);
    let rows = parsed.as_array().expect("list 是数组");
    assert_eq!(rows.len(), 1, "create 失败应保留注册结果: {parsed}");
    assert_eq!(rows[0]["name"], "forge1");
    assert_eq!(rows[0]["variant"], "forge", "变体必须如实记录");
    assert!(marker_of(&ws, "forge1").is_file(), "磁盘与注册表不一致");
}

/// 完整读路径：init → list 空 → remove 不存在的服务器 → 退出码 3
///
/// 不联网，覆盖「注册表为空时的读路径」与「未找到 → 3」的端到端映射。
#[test]
fn empty_workspace_list_then_remove_missing_exits_3() {
    let home = temp_home();
    let ws = init_workspace(&home, "ws1");

    let list = run(home.path(), &["server", "list", "--output", "json"]);
    ok(&list, "server list");
    assert_eq!(stdout_json(&list), serde_json::json!([]));

    let remove = run(
        home.path(),
        &["server", "remove", "nope", "--output", "json"],
    );
    assert_eq!(
        remove.status.code(),
        Some(3),
        "未找到应为 3: {}",
        stderr(&remove)
    );
    assert!(remove.stdout.is_empty(), "错误路径不得输出数据");
    assert!(
        !ws.join("servers").join("nope").exists(),
        "删除不存在的服务器不得创建目录"
    );
}

/// 工作区列表与当前标记：init 后当前工作区指向它，标记落在用户路径本身
#[test]
fn init_marks_workspace_current_and_writes_ownership_marker() {
    let home = temp_home();
    let ws = init_workspace(&home, "ws1");

    let list = run(home.path(), &["workspace", "list", "--output", "json"]);
    ok(&list, "workspace list");
    let parsed = stdout_json(&list);
    let rows = parsed.as_array().expect("数组");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "ws1");
    assert_eq!(rows[0]["current"], true);

    assert!(ws.join("workspace.toml").is_file(), "缺少 workspace.toml");
    assert!(ws.join("servers").is_dir(), "缺少 servers/");
}

/// 删除工作区目录后 doctor 报错误级发现（退出 1，不是崩溃）
#[test]
fn doctor_after_workspace_dir_deleted_reports_error() {
    let home = temp_home();
    let ws = init_workspace(&home, "ws1");
    std::fs::remove_dir_all(&ws).expect("删掉工作区目录");

    let out = run(home.path(), &["workspace", "doctor", "--output", "json"]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    let payload = stdout_json(&out);
    assert_eq!(payload["has_errors"], true, "{payload}");
    assert!(
        payload["errors"].as_u64().unwrap_or(0) >= 1,
        "应至少一条错误级发现: {payload}"
    );
}
