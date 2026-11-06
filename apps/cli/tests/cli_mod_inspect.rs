//! `mod inspect` 端到端测试：真实二进制 + 手工构造的 jar
//!
//! 只验证入口链路：参数接受、JSON 契约、退出码、中文化。
//! 判定规则本身由 `domain` / `modkit` 的单测覆盖，这里不重复。
//!
//! 隔离方式与其它 CLI e2e 一致：`HOME` / `XDG_*` 指向临时目录，stdin 置空。
//! **不依赖网络**：所有用例都带 `--offline`。

// 集成测试的辅助函数不是 #[test]，clippy 的 in-tests 白名单识别不到
#![allow(clippy::expect_used, clippy::panic)]

use std::io::Write;
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

fn run(home: &Path, args: &[&str]) -> Output {
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
        .env("MCSUP_WORKSPACE", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().expect("运行 mcsup")
}

/// 造一个最小 Fabric mod jar
fn write_fabric_jar(dir: &Path, name: &str, meta: &str) -> PathBuf {
    let path = dir.join(name);
    let file = std::fs::File::create(&path).expect("create jar");
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("fabric.mod.json", opts).expect("start");
    zip.write_all(meta.as_bytes()).expect("write meta");
    zip.finish().expect("finish zip");
    path
}

#[test]
fn json_output_exposes_contract_fields() {
    let home = temp_home();
    let jar = write_fabric_jar(
        home.path(),
        "client.jar",
        r#"{"schemaVersion":1,"id":"clientonly","version":"1.0",
            "environment":"client","depends":{"minecraft":"1.21.1"}}"#,
    );
    let out = run(
        home.path(),
        &[
            "mod",
            "inspect",
            jar.to_str().expect("path"),
            "--offline",
            "--output",
            "json",
        ],
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout 必须是 JSON");
    let first = &value[0];
    assert_eq!(first["id"], "clientonly");
    assert_eq!(first["server"], "unsupported");
    assert_eq!(first["risk"], "blocking");
    assert_eq!(first["loaders"][0], "fabric");
    // 依据必须给出，否则用户无从判断
    assert!(!first["evidence"].as_array().expect("evidence").is_empty());
}

#[test]
fn unknown_side_reports_review_not_ok() {
    let home = temp_home();
    let jar = write_fabric_jar(
        home.path(),
        "ambiguous.jar",
        r#"{"schemaVersion":1,"id":"ambiguous","version":"2.0","environment":"*",
            "entrypoints":{"main":["a.B"]}}"#,
    );
    let out = run(
        home.path(),
        &[
            "mod",
            "inspect",
            jar.to_str().expect("path"),
            "--offline",
            "--output",
            "json",
        ],
    );
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    // 判不出不能报 ok
    assert_eq!(value[0]["risk"], "review");
    assert_eq!(value[0]["server"], "unknown");
}

#[test]
fn broken_json_still_reported() {
    let home = temp_home();
    // 裸换行在字符串内：Debugify 的真实形态
    let jar = write_fabric_jar(
        home.path(),
        "broken.jar",
        "{\"schemaVersion\":1,\"id\":\"debugify\",\"version\":\"1.0\",\n \"description\":\"a\nb\"}",
    );
    let out = run(
        home.path(),
        &[
            "mod",
            "inspect",
            jar.to_str().expect("path"),
            "--offline",
            "--output",
            "json",
        ],
    );
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(value[0]["id"], "debugify");
}

#[test]
fn missing_dependency_is_blocking() {
    let home = temp_home();
    let jar = write_fabric_jar(
        home.path(),
        "needy.jar",
        r#"{"schemaVersion":1,"id":"needy","version":"1.0","environment":"server",
            "depends":{"minecraft":"1.21.1","absent-mod":"*"}}"#,
    );
    let out = run(
        home.path(),
        &[
            "mod",
            "inspect",
            jar.to_str().expect("path"),
            "--offline",
            "--output",
            "json",
        ],
    );
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(value[0]["risk"], "blocking");
    assert_eq!(value[0]["missing_deps"][0], "absent-mod");
}

#[test]
fn directory_scans_direct_children_only() {
    let home = temp_home();
    let mods = home.path().join("mods");
    std::fs::create_dir_all(mods.join("nested")).expect("mkdir");
    write_fabric_jar(
        &mods,
        "top.jar",
        r#"{"schemaVersion":1,"id":"top","version":"1.0","environment":"server"}"#,
    );
    // 嵌套目录里的 jar 不应被算进来
    write_fabric_jar(
        &mods.join("nested"),
        "inner.jar",
        r#"{"schemaVersion":1,"id":"inner","version":"1.0"}"#,
    );
    let out = run(
        home.path(),
        &[
            "mod",
            "inspect",
            mods.to_str().expect("path"),
            "--offline",
            "--output",
            "json",
        ],
    );
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    let ids: Vec<&str> = value
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|v| v["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["top"]);
}

#[test]
fn unreadable_file_is_reported_but_batch_continues() {
    let home = temp_home();
    let dir = home.path().join("mods");
    std::fs::create_dir_all(&dir).expect("mkdir");
    write_fabric_jar(
        &dir,
        "ok.jar",
        r#"{"schemaVersion":1,"id":"ok","version":"1.0","environment":"server"}"#,
    );
    std::fs::write(dir.join("corrupt.jar"), b"not a zip").expect("write");

    let out = run(
        home.path(),
        &[
            "mod",
            "inspect",
            dir.to_str().expect("path"),
            "--offline",
            "--output",
            "json",
        ],
    );
    assert!(out.status.success(), "坏文件不应让整批失败");
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    // 好文件仍然出结果
    assert_eq!(value.as_array().expect("array").len(), 1);
    assert_eq!(value[0]["id"], "ok");
}

#[test]
fn missing_path_argument_exits_with_usage_code() {
    let home = temp_home();
    let out = run(home.path(), &["mod", "inspect"]);
    assert_eq!(out.status.code(), Some(2), "缺参数应当是用法错误");
}

#[test]
fn human_output_mentions_review_reason() {
    let home = temp_home();
    let jar = write_fabric_jar(
        home.path(),
        "amb.jar",
        r#"{"schemaVersion":1,"id":"amb","version":"1.0","environment":"*",
            "entrypoints":{"client":["a.B"]}}"#,
    );
    let out = run(
        home.path(),
        &["mod", "inspect", jar.to_str().expect("path"), "--offline"],
    );
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    // 判不出时必须给出依据与建议，不能只打一行结论
    assert!(
        text.contains("↳"),
        "人类输出必须列出依据，实际输出:\n{text}"
    );
    assert!(
        text.contains("建议") || text.contains("suggestion"),
        "判不出时必须给建议，实际输出:\n{text}"
    );
}

/// `--loader` 决定采信哪份清单：Quilt 专属依赖在 fabric 口径下不该报缺失
#[test]
fn loader_flag_switches_dependency_scope() {
    let home = temp_home();
    let path = home.path().join("multi.jar");
    let file = std::fs::File::create(&path).expect("create jar");
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("fabric.mod.json", opts).expect("start");
    zip.write_all(
        br#"{"schemaVersion":1,"id":"multi","version":"1",
            "environment":"*","depends":{"minecraft":"1.21.1"}}"#,
    )
    .expect("write fabric meta");
    zip.start_file("quilt.mod.json", opts).expect("start");
    zip.write_all(
        br#"{"schemaVersion":1,"quilt_loader":{"id":"multi","version":"1",
            "depends":[{"id":"quilt_loader","versions":"*"}]}}"#,
    )
    .expect("write quilt meta");
    zip.finish().expect("finish zip");

    let jar = path.to_str().expect("path");
    let out = run(
        home.path(),
        &["mod", "inspect", jar, "--offline", "--output", "json"],
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    // 单个多加载器 jar：交集不唯一 → 目标加载器未知
    assert!(value[0]["target_loader"].is_null(), "{}", value);

    let out = run(
        home.path(),
        &[
            "mod",
            "inspect",
            jar,
            "--offline",
            "--loader",
            "fabric",
            "--output",
            "json",
        ],
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(value[0]["target_loader"], "fabric");
    // fabric-api / quilt_loader 都是内置项，不该报缺失
    assert_eq!(value[0]["missing_deps"], serde_json::json!([]));
}

/// 未知加载器名按用法错误退出（2），不猜一个
#[test]
fn unknown_loader_is_usage_error() {
    let home = temp_home();
    let jar = write_fabric_jar(
        home.path(),
        "x.jar",
        r#"{"schemaVersion":1,"id":"x","version":"1"}"#,
    );
    let out = run(
        home.path(),
        &[
            "mod",
            "inspect",
            jar.to_str().expect("path"),
            "--offline",
            "--loader",
            "not-a-loader",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}
