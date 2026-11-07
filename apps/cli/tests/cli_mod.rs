//! `mcsup server mod` 的入口契约：退出码、JSON 形状、只读路径的落盘结论
//!
//! 与 `cli_e2e` / `cli_contract` 同一套隔离手段（临时 HOME、stdin 置空、
//! 不可达代理），因此**不联网**：
//!
//! - 需要下载的路径（`install` / `update` 真正落盘）由 `service` 层的编排测试
//!   用假来源与假下载器覆盖，这里只断言入口把参数、错误与输出接对了
//! - 联网识别（`--project-info` / `--outdated` 的候选查询）由 `provider` 的
//!   回环 mock 覆盖
//!
//! 建服务端时同样让下载确定失败（`HTTPS_PROXY` 指向不可达地址）：`create` 的
//! 约定是失败也保留注册结果，因此能得到一台「目录真实存在、`mods/` 为空」的
//! 服务端，足够验证读取与参数路径。

#![allow(clippy::expect_used)]

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

fn run(home: &Path, args: &[&str]) -> Output {
    run_env(home, args, &[])
}

fn run_env(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
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

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// 取 stdout 的 JSON；不是 JSON 时用 `assert` 报出整段输出
///
/// 不用 `panic!` / `unreachable!`：门禁的 clippy 禁止它们，`assert!` 在测试里
/// 允许，而且失败信息能一次带出 stdout 与 stderr。
fn json(out: &Output) -> serde_json::Value {
    match serde_json::from_slice::<serde_json::Value>(&out.stdout) {
        Ok(value) => value,
        Err(err) => {
            let detail = format!(
                "stdout 必须是 JSON: {err}\nstdout=[{}]\nstderr=[{}]",
                stdout(out),
                stderr(out)
            );
            assert_eq!(detail, String::new(), "stdout 不是 JSON");
            serde_json::Value::Null
        }
    }
}

fn ok(out: &Output, what: &str) {
    assert_eq!(out.status.code(), Some(0), "{what} 失败: {}", stderr(out));
}

fn code(out: &Output) -> Option<i32> {
    out.status.code()
}

/// 建工作区 + 一台服务端（下载失败但记录与目录都留下）
fn workspace_with_server(home: &TempDir) -> (PathBuf, PathBuf) {
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
        ],
    );
    ok(&init, "workspace init");

    let create = run_env(
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
        &[("HTTPS_PROXY", DEAD_PROXY), ("HTTP_PROXY", DEAD_PROXY)],
    );
    // 下载必然失败：这一层只借「失败后保留注册结果」拿到一台真实服务端
    assert_eq!(
        code(&create),
        Some(5),
        "create 应进入下载并失败: {}",
        stderr(&create)
    );
    let dir = ws.join("servers").join("srv1");
    assert!(dir.join("mcsup.json").is_file(), "marker 应已写入");
    (ws, dir)
}

/// 在服务端的 `mods/` 里放一个不是 jar 的文件
fn write_broken_jar(dir: &Path) {
    let mods = dir.join("mods");
    std::fs::create_dir_all(&mods).expect("mkdir mods");
    std::fs::write(mods.join("broken.jar"), b"not-a-zip").expect("write");
}

// ---------------------------------------------------------------- 参数与退出码

/// 缺服务端名：非交互下是用法错误（2），不是「未实现」也不再是补参提问
#[test]
fn list_without_name_exits_2() {
    let home = temp_home();
    let out = run(home.path(), &["server", "mod", "list"]);
    assert_eq!(code(&out), Some(2), "stderr: {}", stderr(&out));
}

/// 服务端不存在：3
#[test]
fn list_unknown_server_exits_3() {
    let home = temp_home();
    let (_, _) = workspace_with_server(&home);
    let out = run(home.path(), &["server", "mod", "list", "nosuch"]);
    assert_eq!(code(&out), Some(3), "stderr: {}", stderr(&out));
}

/// 缺项目参数：非交互下是用法错误（2）
///
/// `install` / `in` / `ad` 都是 `add` 的别名，必须走到同一条路径。
#[test]
fn install_without_project_exits_2() {
    let home = temp_home();
    let (_, _) = workspace_with_server(&home);
    for sub in ["add", "install", "in", "ad"] {
        let out = run(home.path(), &["server", "mod", sub, "srv1"]);
        assert_eq!(code(&out), Some(2), "{sub} stderr: {}", stderr(&out));
        assert!(
            stderr(&out).contains("server mod add"),
            "{sub} 错误里应给出用法提示: {}",
            stderr(&out)
        );
    }
}

/// 纯客户端类别在联网之前就被拒绝：不能把 shader 装进服务端
#[test]
fn install_client_only_kind_exits_2() {
    let home = temp_home();
    let (_, _) = workspace_with_server(&home);
    let out = run(
        home.path(),
        &[
            "server", "mod", "install", "srv1", "sodium", "--kind", "shader",
        ],
    );
    assert_eq!(code(&out), Some(2), "stderr: {}", stderr(&out));
}

/// 非 modrinth 链接：当 slug 传给上游只会得到「没找到」，因此直接报用法错误
#[test]
fn install_rejects_unsupported_link_host() {
    let home = temp_home();
    let (_, _) = workspace_with_server(&home);
    let out = run(
        home.path(),
        &[
            "server",
            "mod",
            "install",
            "srv1",
            "https://www.curseforge.com/minecraft/mc-mods/jei",
        ],
    );
    assert_eq!(code(&out), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("curseforge.com"),
        "错误里应指出不支持的站点: {}",
        stderr(&out)
    );
}

/// 卸载：目标不存在是 3，不是静默成功
#[test]
fn remove_unknown_selector_exits_3() {
    let home = temp_home();
    let (_, _) = workspace_with_server(&home);
    let out = run(
        home.path(),
        &["server", "mod", "remove", "srv1", "nosuchmod"],
    );
    assert_eq!(code(&out), Some(3), "stderr: {}", stderr(&out));
}

// ---------------------------------------------------------------- JSON 契约

/// 空服务端：单行 JSON 数组，退出 0
#[test]
fn list_empty_server_is_single_json_array() {
    let home = temp_home();
    let (_, _) = workspace_with_server(&home);
    let out = run(
        home.path(),
        &["server", "mod", "list", "srv1", "--output", "json"],
    );
    ok(&out, "server mod list");
    assert_eq!(json(&out), serde_json::json!([]));
    assert_eq!(
        stdout(&out).lines().count(),
        1,
        "机器契约是单行: {}",
        stdout(&out)
    );
}

/// 读不出来的文件：JSON 仍只有数组，提示落 stderr（不能污染 stdout）
#[test]
fn list_reports_unreadable_files_on_stderr_in_json_mode() {
    let home = temp_home();
    let (_, dir) = workspace_with_server(&home);
    write_broken_jar(&dir);

    let out = run(
        home.path(),
        &["server", "mod", "list", "srv1", "--output", "json"],
    );
    ok(&out, "server mod list");
    assert_eq!(json(&out), serde_json::json!([]), "坏文件不进 mods 列表");
    assert_eq!(stdout(&out).lines().count(), 1);
    assert!(
        stderr(&out).contains("broken.jar"),
        "坏文件必须报到 stderr，否则脚本以为目录是干净的: {}",
        stderr(&out)
    );
}

/// 人类模式：坏文件也要出现在输出里
#[test]
fn list_reports_unreadable_files_in_human_mode() {
    let home = temp_home();
    let (_, dir) = workspace_with_server(&home);
    write_broken_jar(&dir);

    let out = run(
        home.path(),
        &["server", "mod", "list", "srv1", "--lang", "zh"],
    );
    ok(&out, "server mod list");
    let text = stdout(&out);
    assert!(text.contains("broken.jar"), "输出缺少坏文件: {text}");
}

/// `--kind` 决定看哪个目录：plugin 看 `plugins/`，空服务端同样是空数组
#[test]
fn list_plugin_kind_reads_plugins_directory() {
    let home = temp_home();
    let (_, dir) = workspace_with_server(&home);
    let plugins = dir.join("plugins");
    std::fs::create_dir_all(&plugins).expect("mkdir plugins");
    std::fs::write(plugins.join("luckperms.jar"), b"not-a-zip").expect("write");

    let out = run(
        home.path(),
        &[
            "server", "mod", "list", "srv1", "--kind", "plugin", "--output", "json",
        ],
    );
    ok(&out, "server mod list --kind plugin");
    assert_eq!(json(&out), serde_json::json!([]), "坏文件不进列表");
    // 读不出来的提示必须指向 plugins/，说明扫的是那个目录而不是 mods/
    let err = stderr(&out);
    assert!(err.contains("plugins"), "提示应指向 plugins/ 目录: {err}");
}

/// 空服务端的 `outdated`：形状固定，items 与 skipped 都是空数组
#[test]
fn outdated_on_empty_server_is_empty_plan() {
    let home = temp_home();
    let (_, _) = workspace_with_server(&home);
    let out = run(
        home.path(),
        &["server", "mod", "outdated", "srv1", "--output", "json"],
    );
    ok(&out, "server mod outdated");
    let payload = json(&out);
    assert_eq!(payload["server"], "srv1");
    assert_eq!(payload["items"], serde_json::json!([]));
    assert_eq!(payload["skipped"], serde_json::json!([]));
}

/// `outdated` / `update` 的 JSON 形状不随内容变化：永远是同一个对象
///
/// 这条是**机器契约**的核心：脚本按 `items` 取值，若「这次恰好没有更新」时
/// 变成裸数组，解析就断了。因此三个字段一个都不能少，且必须是单行。
#[test]
fn outdated_and_update_always_print_the_same_object_shape() {
    let home = temp_home();
    let (_, _) = workspace_with_server(&home);

    for args in [
        vec!["server", "mod", "outdated", "srv1", "--output", "json"],
        vec![
            "server",
            "mod",
            "update",
            "srv1",
            "--dry-run",
            "--output",
            "json",
        ],
    ] {
        let out = run(home.path(), &args);
        ok(&out, &args.join(" "));

        let payload = json(&out);
        assert!(
            payload.is_object(),
            "必须是对象而不是数组: {}",
            stdout(&out)
        );
        for field in ["server", "items", "skipped"] {
            assert!(
                payload.get(field).is_some(),
                "{field} 字段缺失: {}",
                stdout(&out)
            );
        }
        assert_eq!(payload["server"], "srv1");
        assert!(payload["items"].is_array(), "items 必须是数组");
        assert!(payload["skipped"].is_array(), "skipped 必须是数组");
        assert_eq!(
            stdout(&out).lines().count(),
            1,
            "机器契约是单行: {}",
            stdout(&out)
        );
    }
}

/// 空服务端的 `update`：没有可更新项时什么都不做（不下载、不写盘）
#[test]
fn update_on_empty_server_changes_nothing() {
    let home = temp_home();
    let (_, dir) = workspace_with_server(&home);
    let out = run(
        home.path(),
        &["server", "mod", "update", "srv1", "--output", "json"],
    );
    ok(&out, "server mod update");
    let payload = json(&out);
    // 没有可更新项时输出的是**计划**（空 items + 空 skipped），不是「已应用」
    assert_eq!(payload["server"], "srv1");
    assert_eq!(payload["items"], serde_json::json!([]));
    assert_eq!(payload["skipped"], serde_json::json!([]));
    assert!(!dir.join("mods").exists(), "没有更新项就不该造出 mods/");
}

/// `--dry-run` 的 install 也要先算计划：上游不可达时按失败收场，且不写盘
#[test]
fn install_dry_run_fails_before_writing_anything() {
    let home = temp_home();
    let (_, dir) = workspace_with_server(&home);
    let out = run_env(
        home.path(),
        &[
            "server",
            "mod",
            "install",
            "srv1",
            "sodium",
            "--dry-run",
            "--output",
            "json",
        ],
        &[("HTTPS_PROXY", DEAD_PROXY), ("HTTP_PROXY", DEAD_PROXY)],
    );
    // 上游不可达是后端失败（5）；重点是计划阶段不落任何东西
    assert_eq!(code(&out), Some(5), "stderr: {}", stderr(&out));
    assert!(!dir.join("mods").exists(), "计划阶段不能造出 mods/ 目录");
    assert!(
        !dir.join(".mcsup-staging").exists(),
        "计划阶段不能建暂存目录"
    );
}
