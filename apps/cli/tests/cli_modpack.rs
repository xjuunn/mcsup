//! `server create` 整合包变体的入口契约
//!
//! 两类用例：
//!
//! 1. **离线契约**（默认跑）：`modpack` 顶层命令已移除、`--modpack` 隐含变体、
//!    非交互缺项目时报错、整合包变体允许省略 `--version`。不联网。
//! 2. **联网契约**（`MCSUP_NET_TESTS=1`）：真实建一台整合包服务器并检查落盘。
//!
//! 隔离方式同 `cli_backup`：`HOME` / `XDG_*` 指向临时目录，stdin 置空。

// 集成测试辅助函数不是 #[test]，clippy 的 in-tests 白名单识别不到
#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

fn temp_home() -> TempDir {
    let home = TempDir::new().expect("tempdir");
    if cfg!(windows) {
        std::fs::create_dir_all(home.path().join("AppData").join("Roaming"))
            .expect("创建 AppData\\Roaming");
    }
    home
}

fn enabled() -> bool {
    std::env::var("MCSUP_NET_TESTS").is_ok_and(|value| value == "1")
}

const DEAD_PROXY: &str = "http://127.0.0.1:1";

fn run_full(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
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
        .env_remove("HTTPS_PROXY")
        .env_remove("HTTP_PROXY")
        .env_remove("ALL_PROXY")
        .env("MCSUP_WORKSPACE", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().expect("运行 mcsup")
}

fn run(home: &Path, args: &[&str]) -> Output {
    run_full(home, args, &[])
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn ok(out: &Output, what: &str) {
    assert_eq!(out.status.code(), Some(0), "{what} 失败: {}", stderr(out));
}

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

/// `modpack` 顶层命令已移除：解析失败是用法错误（退出码 2）
#[test]
fn modpack_toplevel_command_is_gone() {
    let home = temp_home();
    let out = run(home.path(), &["modpack", "list"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    let text = stderr(&out);
    assert!(
        text.contains("unrecognized subcommand"),
        "应报未知子命令: {text}"
    );
}

/// `server create --help` 列出 `--modpack`
#[test]
fn create_help_mentions_modpack() {
    let home = temp_home();
    let out = run(home.path(), &["server", "create", "--help"]);
    ok(&out, "server create --help");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("--modpack"), "{text}");
    assert!(text.contains("modrinth-modpack"), "{text}");
}

/// 非交互 + `--variant modrinth-modpack` + 无 `--modpack`：不能静默选包
#[test]
fn modpack_variant_without_project_fails_noninteractive() {
    let home = temp_home();
    let out = run_full(
        home.path(),
        &[
            "server",
            "create",
            "mp",
            "--variant",
            "modrinth-modpack",
            "--output",
            "json",
        ],
        &[
            ("HTTPS_PROXY", DEAD_PROXY),
            ("HTTP_PROXY", DEAD_PROXY),
            ("ALL_PROXY", DEAD_PROXY),
        ],
    );
    // 缺名称 / 缺项目都会失败；关键是不能静默创建
    assert_ne!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

/// 整合包变体不要求 `--version`：闸门不能先把它拦下
#[test]
fn modpack_variant_allows_missing_version() {
    let home = temp_home();
    let ws = init_workspace(&home, "ws");
    let _ = ws;
    let out = run_full(
        home.path(),
        &[
            "server",
            "create",
            "mp",
            "--variant",
            "modrinth-modpack",
            "--modpack",
            "fabulously-optimized",
            "--output",
            "json",
        ],
        &[
            ("HTTPS_PROXY", DEAD_PROXY),
            ("HTTP_PROXY", DEAD_PROXY),
            ("ALL_PROXY", DEAD_PROXY),
        ],
    );
    let text = stderr(&out);
    // 不能是「缺少 --version」的用法错误；离线会卡在网络失败
    assert!(
        !text.contains("缺少 --version") && !text.contains("missing --version"),
        "整合包不应要求 --version: {text}"
    );
}

/// `--modpack` 与显式 `--variant` 不冲突（前者隐含后者）
#[test]
fn modpack_flag_implies_variant() {
    let home = temp_home();
    let out = run_full(
        home.path(),
        &[
            "server",
            "create",
            "mp",
            "--modpack",
            "fabulously-optimized",
            "--output",
            "json",
        ],
        &[
            ("HTTPS_PROXY", DEAD_PROXY),
            ("HTTP_PROXY", DEAD_PROXY),
            ("ALL_PROXY", DEAD_PROXY),
        ],
    );
    // 缺名称是非交互用法错误；不应是「未知变体」
    let text = stderr(&out);
    assert!(!text.contains("unknown variant"), "{text}");
}

/// `--variant modrinth` 是短名别名，不应被当成未知变体
#[test]
fn modrinth_variant_alias_is_accepted() {
    let home = temp_home();
    let out = run_full(
        home.path(),
        &[
            "server",
            "create",
            "mp-alias",
            "--variant",
            "modrinth",
            "--modpack",
            "fabulously-optimized",
            "--output",
            "json",
        ],
        &[
            ("HTTPS_PROXY", DEAD_PROXY),
            ("HTTP_PROXY", DEAD_PROXY),
            ("ALL_PROXY", DEAD_PROXY),
        ],
    );
    let text = stderr(&out);
    assert!(
        !text.contains("未知变体")
            && !text.contains("unknown variant")
            && !text.contains("暂未支持"),
        "别名不应被拒绝: {text}"
    );
}

/// 真实建整合包服务器：落盘 mods / overrides / 核心，注册表记录 MC 版本
#[test]
fn net_modpack_create_installs_server() {
    if !enabled() {
        return;
    }
    let home = temp_home();
    let ws = init_workspace(&home, "ws");
    let out = run(
        home.path(),
        &[
            "server",
            "create",
            "mp",
            "--modpack",
            "fabulously-optimized",
            "--output",
            "json",
        ],
    );
    ok(&out, "整合包建站");
    let dir = ws.join("servers").join("mp");

    // 服务端目录：核心、mods、配置都在
    assert!(dir.is_dir(), "服务器目录不存在: {}", dir.display());
    assert!(
        dir.join("server.jar").is_file(),
        "缺少加载器核心: {}",
        dir.display()
    );
    let mods = dir.join("mods");
    assert!(mods.is_dir(), "缺少 mods/");
    let count = std::fs::read_dir(&mods).expect("读取 mods").count();
    assert!(count > 0, "整合包应至少装一个 mod");

    // staging 目录清理干净
    assert!(!dir.join("modpack-staging").exists(), "暂存目录应被清理");

    // JSON 结果：MC 版本来自清单，变体是整合包
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"source\":\"modrinth\""), "{text}");
    assert!(text.contains("\"variant\":\"modrinth-modpack\""), "{text}");

    // 注册表记录的版本是清单里的 MC 版本（不是整包版本号）
    let status = run(home.path(), &["server", "status", "mp", "--output", "json"]);
    let json: serde_json::Value = serde_json::from_slice(&status.stdout).expect("status json");
    let version = json["version"].as_str().expect("version");
    // MC 版本形如 1.21.1 / 26.2，至少含一个点
    assert!(version.contains('.'), "版本应是 MC 版本: {version}");
}

/// 重复建站：同名已存在时报冲突，不重复下载
#[test]
fn net_modpack_create_twice_conflicts() {
    if !enabled() {
        return;
    }
    let home = temp_home();
    let _ws = init_workspace(&home, "ws");
    let first = run(
        home.path(),
        &[
            "server",
            "create",
            "mp2",
            "--modpack",
            "fabulously-optimized",
            "--output",
            "json",
        ],
    );
    ok(&first, "首次建站");
    let second = run(
        home.path(),
        &[
            "server",
            "create",
            "mp2",
            "--modpack",
            "fabulously-optimized",
            "--output",
            "json",
        ],
    );
    assert_ne!(second.status.code(), Some(0), "同名应失败");
}
