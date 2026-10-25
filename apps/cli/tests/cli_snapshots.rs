//! CLI 机器契约快照：JSON 输出与落盘格式一旦变形立即红
//!
//! 这里测的不是「某个字段等于某个值」（那是 `cli_contract.rs` 的事），而是
//! **输出形状本身**：字段名、嵌套层级、可空性、数组顺序。这些是脚本与前端
//! 直接依赖的东西，悄悄改掉一个字段名不会有任何测试失败，直到下游炸掉。
//!
//! 快照里不能出现临时路径、随机 UUID、时间戳，否则每次运行都不同。所以所有
//! 输出都先经过 [`normalize`]：把 `home` 与工作区路径替换成占位符，把 uuid
//! 与时间戳替换成稳定标记。
//!
//! 更新快照：
//!
//! ```text
//! cargo insta review        # 逐条人工确认（推荐）
//! INSTA_FORCE_UPDATE=1 cargo test -p cli --test cli_snapshots
//! ```

// 集成测试的辅助函数不是 #[test]，clippy 的 in-tests 白名单识别不到；
// 这里的 panic 全部是「stdout 不是 JSON」这类断言失败，不是业务错误。
#![allow(clippy::expect_used, clippy::panic)]

use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

// ---- 运行环境（与 cli_contract.rs 一致：隔离 HOME，stdin 非 TTY） ----

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
    cmd.current_dir(home)
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_DATA_HOME", home.join("share"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env_remove("MCSUP_LANG")
        .env_remove("LANG")
        .env_remove("LC_ALL")
        .env_remove("LC_CTYPE")
        .env("MCSUP_WORKSPACE", "")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().expect("运行 mcsup")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout_json(out: &Output) -> Value {
    assert_eq!(out.status.code(), Some(0), "失败: {}", stderr(out));
    serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout 不是 JSON: {err}\nstdout=[{}]",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

// ---- 归一化 ----

/// 把每次运行都会变的部分替换成占位符
///
/// 只做「值」的归一化，**不动字段名与结构**——结构正是要钉住的部分。
fn normalize(value: &mut Value, home: &Path, workspace: &Path) {
    match value {
        Value::String(text) => {
            let replaced = text
                .replace(
                    &workspace.to_string_lossy().replace('\\', "/"),
                    "<workspace>",
                )
                .replace(&workspace.to_string_lossy().into_owned(), "<workspace>")
                .replace(&home.to_string_lossy().replace('\\', "/"), "<home>")
                .replace(&home.to_string_lossy().into_owned(), "<home>");
            *text = replaced;
        }
        Value::Array(items) => {
            for item in items {
                normalize(item, home, workspace);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                normalize(item, home, workspace);
            }
        }
        Value::Number(number) => {
            // unix 秒级时间戳由外部决定，不参与形状断言
            if number.as_u64().is_some_and(|raw| raw > 1_600_000_000) {
                *value = serde_json::json!("<timestamp>");
            }
        }
        Value::Null | Value::Bool(_) => {}
    }
}

/// 递归结构快照：只保留键名与值类型
///
/// 为什么还要一份「去掉全部值」的形状：值往往随时间/机器变，形状不会。
/// 两份快照分工明确——结构快照管字段契约，原文快照管文案。
fn shape(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<(&String, &Value)> = map.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = serde_json::Map::new();
            for (key, item) in sorted {
                out.insert(key.clone(), shape(item));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(shape).collect()),
        Value::Null => serde_json::json!("null"),
        Value::Bool(_) => serde_json::json!("bool"),
        Value::Number(_) => serde_json::json!("number"),
        Value::String(_) => serde_json::json!("string"),
    }
}

// ---- 用例 ----

/// `workspace init` 的输出形状
#[test]
fn snapshot_workspace_init_output() {
    let home = temp_home();
    let ws = home.path().join("ws1");
    let out = run(
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
            "--lang",
            "zh",
        ],
    );
    let mut value = stdout_json(&out);
    normalize(&mut value, home.path(), &ws);
    insta::assert_json_snapshot!("workspace_init", value);
}

/// `workspace list` 的稳定形状（空 → 有 → 当前标记）
#[test]
fn snapshot_workspace_list_output() {
    let home = temp_home();
    let empty = stdout_json(&run(
        home.path(),
        &["workspace", "list", "--output", "json", "--lang", "zh"],
    ));
    insta::assert_json_snapshot!("workspace_list_empty", shape(&empty));

    for name in ["ws1", "ws2"] {
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
                "--lang",
                "zh",
            ],
        );
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    }

    let mut listed = stdout_json(&run(
        home.path(),
        &["workspace", "list", "--output", "json", "--lang", "zh"],
    ));
    normalize(&mut listed, home.path(), &home.path().join("ws1"));
    insta::assert_json_snapshot!("workspace_list_two", shape(&listed));
}

/// `workspace doctor` 的检查项形状：干净环境不误报
#[test]
fn snapshot_workspace_doctor_output() {
    let home = temp_home();
    let ws = home.path().join("ws1");
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
            "--lang",
            "zh",
        ],
    );
    assert_eq!(init.status.code(), Some(0), "{}", stderr(&init));

    let mut report = stdout_json(&run(
        home.path(),
        &["workspace", "doctor", "--output", "json", "--lang", "zh"],
    ));
    normalize(&mut report, home.path(), &ws);
    insta::assert_json_snapshot!("workspace_doctor_clean", shape(&report));
}

/// 用法错误的诊断形状：退出码 2 + stdout 空 + 结构化 stderr
///
/// 错误文案是用户直接看到的东西，改一个字都值得被看见。
#[test]
fn snapshot_usage_error_message() {
    let home = temp_home();
    let out = run(
        home.path(),
        &["server", "create", "--output", "json", "--lang", "zh"],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(out.stdout.is_empty(), "用法错误不得输出数据");
    insta::assert_snapshot!("usage_error_missing_name", stderr(&out));
}

/// `workspace.toml` 落盘格式：marker 是跨版本兼容契约
#[test]
fn snapshot_workspace_marker_file() {
    let home = temp_home();
    let ws = home.path().join("ws1");
    let out = run(
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
            "--lang",
            "zh",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let marker = std::fs::read_to_string(ws.join("workspace.toml")).expect("读 marker");
    // uuid 每次都不同，单独归一化
    let normalized: String = marker
        .lines()
        .map(|line| {
            if line.starts_with("id = ") {
                "id = \"<uuid>\"".to_owned()
            } else if line.starts_with("created_at = ") {
                "created_at = <timestamp>".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("workspace_marker", normalized);
}
