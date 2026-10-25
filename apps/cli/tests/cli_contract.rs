//! CLI 机器契约集成测试：退出码、stdout/stderr 边界、交互兜底
//!
//! 走 `CARGO_BIN_EXE_mcsup` 跑真实二进制；stdin 置空保证非 TTY，
//! HOME 指向临时目录隔离元数据库，绝不触碰真实用户数据。

// 集成测试的辅助函数不是 #[test]，clippy 的 in-tests 白名单识别不到
#![allow(clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

/// 隔离用临时 HOME
///
/// Windows 上 `dirs::data_dir()` 走 Known Folder API，按 `%USERPROFILE%` 展开
/// `User Shell Folders` 模板得到 `AppData\Roaming`，目录不存在时解析失败，
/// CLI 会以「无法确定系统数据目录」退出 1，因此先把该目录建出来。
fn temp_home() -> TempDir {
    let home = TempDir::new().expect("tempdir");
    if cfg!(windows) {
        std::fs::create_dir_all(home.path().join("AppData").join("Roaming"))
            .expect("创建 AppData\\Roaming");
    }
    home
}

fn run(home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mcsup"));
    cmd.args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_DATA_HOME", home.join("share"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        // 清掉宿主环境的干扰变量（空值在入口按缺省处理）
        .env("MCSUP_WORKSPACE", "")
        .stdin(Stdio::null()) // 非 TTY：交互兜底必须关闭
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().expect("运行 mcsup")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// 非交互缺名称 → 用法错误 2；不落库、不联网，stdout 无输出
#[test]
fn create_without_name_exits_2() {
    let home = temp_home();
    let out = run(home.path(), &["server", "create", "--output", "json"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(out.stdout.is_empty(), "用法错误不得输出数据");
    assert!(stderr(&out).contains("缺少服务器名称"), "{}", stderr(&out));
}

/// 非交互缺 --version → 用法错误 2（交互下才进版本选择）
#[test]
fn create_without_version_exits_2() {
    let home = temp_home();
    let out = run(
        home.path(),
        &["server", "create", "srv1", "--output", "json"],
    );
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert!(stderr(&out).contains("--version"), "{}", stderr(&out));
}

/// 显式未支持的变体 → 用法错误 2，文案列出当前可用变体（能力查询口径）
#[test]
fn create_unsupported_variant_exits_2_and_lists_available() {
    let home = temp_home();
    let out = run(
        home.path(),
        &[
            "server",
            "create",
            "srv1",
            "--version",
            "1.21.1",
            "--variant",
            "paper",
            "--output",
            "json",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(out.stdout.is_empty());
    let err = stderr(&out);
    assert!(err.contains("暂未支持变体"), "{err}");
    assert!(err.contains("vanilla"), "{err}");
}

/// 未知变体拼写同样判用法错误 2 并列出可用项
#[test]
fn create_unknown_variant_exits_2() {
    let home = temp_home();
    let out = run(
        home.path(),
        &[
            "server",
            "create",
            "srv1",
            "--version",
            "1.21.1",
            "--variant",
            "paperx",
            "--output",
            "json",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("未知变体"), "{err}");
}

/// 无参 `workspace use` 非交互 → 用法错误 2（不提问）
#[test]
fn workspace_use_without_name_exits_2() {
    let home = temp_home();
    let out = run(home.path(), &["workspace", "use", "--output", "json"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert!(stderr(&out).contains("工作区名称"), "{}", stderr(&out));
}

/// 服务器不存在 → 未找到 3（能力解析之前先查注册表）
#[test]
fn install_missing_server_exits_3() {
    let home = temp_home();
    let out = run(
        home.path(),
        &["server", "install", "nope", "--output", "json"],
    );
    assert_eq!(out.status.code(), Some(3), "stderr: {}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert!(stderr(&out).contains("nope"), "{}", stderr(&out));
}

/// 非交互 init：stdout 单行 JSON 对象（机器契约），随后 list 可见且为当前
#[test]
fn workspace_init_then_list_stdout_single_json() {
    let home = temp_home();
    let ws = home.path().join("wsdir");
    let init = run(
        home.path(),
        &[
            "workspace",
            "init",
            "--dir",
            ws.to_str().expect("utf8"),
            "--name",
            "ws1",
            "--output",
            "json",
        ],
    );
    assert_eq!(init.status.code(), Some(0), "stderr: {}", stderr(&init));
    let row: serde_json::Value =
        serde_json::from_slice(&init.stdout).expect("init stdout 必须是单个 JSON 对象");
    assert_eq!(row["name"], "ws1");
    assert_eq!(row["status"], "created");

    let list = run(home.path(), &["workspace", "list", "--output", "json"]);
    assert_eq!(list.status.code(), Some(0), "stderr: {}", stderr(&list));
    let rows: serde_json::Value =
        serde_json::from_slice(&list.stdout).expect("list stdout 必须是 JSON 数组");
    let rows = rows.as_array().expect("array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "ws1");
    assert_eq!(rows[0]["current"], true);
}

/// clap 硬编码的英文标题 / 内置 flag 说明 / 内置 help 子命令说明必须全部中文化
#[test]
fn help_output_is_chinese() {
    const ENGLISH: [&str; 11] = [
        "Usage:",
        "Commands:",
        "Arguments:",
        "Options:",
        "Print help",
        "Print version",
        "Print this message",
        "Increase logging verbosity",
        "Decrease logging verbosity",
        "[default: ",
        "[possible values: ",
    ];
    let cases: [&[&str]; 4] = [
        &["--help"],
        &["workspace", "-h"],
        &["server", "backup", "create", "--help"],
        &["help", "server"],
    ];
    let home = temp_home();
    for args in cases {
        let out = run(home.path(), args);
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        assert!(out.stderr.is_empty(), "{args:?} 帮助不应写 stderr");
        let text = String::from_utf8_lossy(&out.stdout);
        for english in ENGLISH {
            assert!(
                !text.contains(english),
                "{args:?} 残留英文 {english}:\n{text}"
            );
        }
        for chinese in ["用法:", "选项:", "打印帮助信息"] {
            assert!(text.contains(chinese), "{args:?} 缺少 {chinese}:\n{text}");
        }
    }
}

/// 无参缺子命令：仍是帮助 + 用法错误 2，只是文案换成中文
#[test]
fn no_args_help_is_chinese_on_stderr() {
    let home = temp_home();
    let out = run(home.path(), &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "用法错误不得输出数据");
    let err = stderr(&out);
    assert!(err.contains("用法:"), "{err}");
    assert!(!err.contains("Usage:"), "{err}");
    assert!(err.contains("命令:"), "{err}");
}

/// `--version` 走中文化后的帮助通道，仍退 0 且只写 stdout
#[test]
fn version_exits_0_on_stdout() {
    let home = temp_home();
    let out = run(home.path(), &["--version"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
    assert!(String::from_utf8_lossy(&out.stdout).contains("mcsup"));
}
