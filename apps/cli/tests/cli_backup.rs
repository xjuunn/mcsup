//! `server backup` 端到端测试：真实二进制 + 真实临时工作区
//!
//! 与 `cli_contract` 的分工：那边钉「帮助中文化 / 参数缺失退出码」这类输出契约，
//! 这边跑完整命令链路（create → ls → verify → restore → delete → prune）并检查
//! 磁盘结果与 JSON 字段。
//!
//! 隔离与 `cli_e2e` 完全一致：`HOME` / `USERPROFILE` / `XDG_*` 指向临时目录，
//! stdin 置空保证非 TTY。**不依赖网络**：借 `server create` 在死代理下「注册成功
//! 但下载失败」的既有语义拿到一台已注册服务器（目录与 `mcsup.json` 都在），
//! 再往目录里写世界文件做备份。`server link` 在 Windows 上要符号链接权限，
//! 这里不用它，保证测试在任何 CI 上都能跑。

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

/// 建真实工作区，返回路径
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

/// 注册一台服务器（借死代理让下载失败，但注册结果保留），返回其目录
fn register_server(home: &TempDir, ws: &Path, name: &str) -> PathBuf {
    // 工作区在 init 时已是 current；create 会落到它的 servers/<name>
    let out = run_full(
        home.path(),
        &[
            "server",
            "create",
            name,
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
    // 下载失败是预期的（后端失败 5），但注册与目录必须保留
    assert_eq!(out.status.code(), Some(5), "stderr: {}", stderr(&out));
    let dir = ws.join("servers").join(name);
    assert!(dir.is_dir(), "已注册但目录不存在: {}", dir.display());
    dir
}

/// 往服务器目录写一份可备份的现场（含 `mcsup.json` 由 create 写入）
fn seed(server_dir: &Path) {
    std::fs::create_dir_all(server_dir.join("world")).expect("mkdir world");
    std::fs::write(server_dir.join("world").join("level.dat"), b"level-v1").expect("level");
    std::fs::write(server_dir.join("server.properties"), b"motd=one").expect("props");
}

fn backup(home: &TempDir, args: &[&str]) -> Output {
    let mut full = vec!["server", "backup"];
    full.extend_from_slice(args);
    run(home.path(), &full)
}

/// 完整往返：create → ls → verify → restore(--to) → delete → prune
///
/// 覆盖需求里的核心动作，且每一步都断言 JSON 字段而非只看退出码：
/// 增量复用、分页信封、恢复目标、删除后对象仍被引用、prune 回收计数。
#[test]
fn backup_roundtrip_through_cli() {
    let home = temp_home();
    let ws = init_workspace(&home, "ws1");
    let srv = register_server(&home, &ws, "alpha");
    seed(&srv);

    // ---- create #1：全量 ----
    let create = backup(
        &home,
        &["create", "alpha", "--label", "first", "--output", "json"],
    );
    ok(&create, "backup create #1");
    let first = stdout_json(&create);
    assert_eq!(first["snapshot"]["server"], "alpha");
    assert_eq!(first["snapshot"]["label"], "first");
    assert_eq!(first["snapshot"]["parent"], serde_json::Value::Null);
    // world/level.dat + server.properties + mcsup.json
    assert_eq!(first["snapshot"]["files"], 3, "{first}");
    assert_eq!(first["snapshot"]["reused"], 0, "{first}");
    let id1 = first["snapshot"]["id"].as_str().expect("id1").to_owned();

    // ---- create #2：改一个文件 → 增量只重读它 ----
    std::fs::write(srv.join("world").join("level.dat"), b"level-v2-longer").expect("modify");
    // 让 mtime 与上一份拉开，避免同秒复用判定依赖文件系统精度
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let create2 = backup(
        &home,
        &["create", "alpha", "--label", "second", "--output", "json"],
    );
    ok(&create2, "backup create #2");
    let second = stdout_json(&create2);
    assert_eq!(
        second["snapshot"]["parent"], id1,
        "第二份的父快照应是第一份"
    );
    assert_eq!(
        second["snapshot"]["added"], 1,
        "只新增改动的 level.dat: {second}"
    );
    assert_eq!(second["snapshot"]["reused"], 2, "其余两份复用: {second}");
    let id2 = second["snapshot"]["id"].as_str().expect("id2").to_owned();

    // ---- ls：分页信封 + 时间倒序 ----
    let list = backup(&home, &["ls", "alpha", "--output", "json"]);
    ok(&list, "backup ls");
    let page = stdout_json(&list);
    assert_eq!(page["total"], 2, "{page}");
    assert_eq!(page["page"], 1);
    assert_eq!(page["pages"], 1);
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"].as_str(), Some(id2.as_str()), "最新在前");
    assert_eq!(items[1]["id"].as_str(), Some(id1.as_str()));

    // ---- verify：完好 ----
    let verify = backup(&home, &["verify", "alpha", &id1, "--output", "json"]);
    ok(&verify, "backup verify");
    let report = stdout_json(&verify);
    assert_eq!(report["healthy"], true, "{report}");
    assert_eq!(report["broken"].as_array().expect("broken").len(), 0);

    // ---- restore --to：恢复到新目录，原服务器不动 ----
    let restore = backup(
        &home,
        &["restore", "alpha", &id1, "--to", "copy", "--output", "json"],
    );
    ok(&restore, "backup restore --to");
    let restored = stdout_json(&restore);
    assert_eq!(
        restored["safety_snapshot"],
        serde_json::Value::Null,
        "--to 不额外生成安全快照"
    );
    let dest = restored["destination"].as_str().expect("destination");
    let copied = std::fs::read(Path::new(dest).join("world").join("level.dat")).expect("copy read");
    assert_eq!(copied, b"level-v1", "恢复到新目录应是快照里的旧内容");
    // 原服务器内容不受影响
    assert_eq!(
        std::fs::read(srv.join("world").join("level.dat")).expect("orig read"),
        b"level-v2-longer"
    );

    // ---- delete：删除一份后，被另一份引用的对象不释放 ----
    let del = backup(&home, &["delete", "alpha", &id2, "--output", "json"]);
    ok(&del, "backup delete");
    let deleted = stdout_json(&del);
    assert_eq!(deleted["id"].as_str(), Some(id2.as_str()));
    // level.dat(v2) 只被 id2 引用 → 可孤儿化；其余被 id1 共享
    assert!(
        deleted["orphaned"].as_u64().expect("orphaned") >= 1,
        "{deleted}"
    );

    // ---- prune：剩 1 份，回收孤儿对象 ----
    let prune = backup(
        &home,
        &["prune", "alpha", "--keep", "1", "--output", "json"],
    );
    ok(&prune, "backup prune");
    let pruned = stdout_json(&prune);
    assert_eq!(pruned["dry_run"], false);
    // 已经没有可删的快照（只剩 1 份 = keep 1），但 GC 会回收上一步的孤儿
    assert!(
        pruned["objects_reclaimed"].as_u64().expect("objects") >= 1,
        "{pruned}"
    );

    // ---- 再次 ls：只剩 1 份 ----
    let list = backup(&home, &["ls", "alpha", "--output", "json"]);
    ok(&list, "backup ls final");
    assert_eq!(stdout_json(&list)["total"], 1);
}

/// `--to` 的目标名直接拼进 `servers/<name>`：非法名字必须是用法错误（2），
/// 且在任何写盘动作之前就被拒绝
///
/// 这条防的是路径穿越：`--to ../escape` 在入口或服务层漏判一次，恢复就会把
/// 整个快照写到工作区之外。
#[test]
fn backup_restore_rejects_unsafe_to_name() {
    let home = temp_home();
    let ws = init_workspace(&home, "ws1");
    let srv = register_server(&home, &ws, "alpha");
    seed(&srv);
    let create = backup(&home, &["create", "alpha", "--output", "json"]);
    ok(&create, "backup create");
    let id = stdout_json(&create)["snapshot"]["id"]
        .as_str()
        .expect("id")
        .to_owned();

    for bad in ["../escape", "a/b", "a\\b", "C:evil", ""] {
        let out = backup(&home, &["restore", "alpha", &id, "--to", bad]);
        assert_eq!(
            out.status.code(),
            Some(2),
            "--to {bad:?} 应是用法错误: {}",
            stderr(&out)
        );
    }
    assert!(!ws.join("escape").exists(), "工作区外不应出现恢复产物");
    assert!(!home.path().join("escape").exists());
}

/// `--scope` 只备份指定顶层项：world 之外的文件不进快照
#[test]
fn backup_scope_limits_snapshot() {
    let home = temp_home();
    let ws = init_workspace(&home, "ws1");
    let srv = register_server(&home, &ws, "alpha");
    seed(&srv);

    let out = backup(
        &home,
        &["create", "alpha", "--scope", "world", "--output", "json"],
    );
    ok(&out, "backup create --scope");
    let payload = stdout_json(&out);
    assert_eq!(payload["snapshot"]["scope"], serde_json::json!(["world"]));
    // 只 world/level.dat；server.properties 与 mcsup.json 被排除
    assert_eq!(payload["snapshot"]["files"], 1, "{payload}");
    assert_eq!(
        payload["skipped"].as_u64().expect("skipped"),
        2,
        "{payload}"
    );
}

/// `--keep` 与 `--older-than` 同时给 → 用法错误 2（二选一）
#[test]
fn backup_prune_conflicting_retention_exits_2() {
    let home = temp_home();
    let ws = init_workspace(&home, "ws1");
    let _ = register_server(&home, &ws, "alpha");

    let out = backup(
        &home,
        &[
            "prune",
            "alpha",
            "--keep",
            "3",
            "--older-than",
            "7d",
            "--lang",
            "zh",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(out.stdout.is_empty(), "用法错误不得输出数据");
    assert!(stderr(&out).contains("二选一"), "{}", stderr(&out));
}

/// `--scope` 带路径分隔符 → 用法错误 2（范围只能是顶层项）
#[test]
fn backup_scope_with_separator_exits_2() {
    let home = temp_home();
    let ws = init_workspace(&home, "ws1");
    let _ = register_server(&home, &ws, "alpha");

    let out = backup(
        &home,
        &["create", "alpha", "--scope", "world/region", "--lang", "zh"],
    );
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert!(stderr(&out).contains("范围"), "{}", stderr(&out));
}

/// 非交互缺快照 id → 用法错误 2（不提问、不落数据）
#[test]
fn backup_verify_without_id_exits_2() {
    let home = temp_home();
    let ws = init_workspace(&home, "ws1");
    let _ = register_server(&home, &ws, "alpha");

    let out = backup(&home, &["verify", "alpha", "--lang", "zh"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(out.stdout.is_empty(), "用法错误不得输出数据");
    assert!(stderr(&out).contains("快照 id"), "{}", stderr(&out));
}

/// 服务器不存在 → 未找到 3
#[test]
fn backup_create_missing_server_exits_3() {
    let home = temp_home();
    let _ = init_workspace(&home, "ws1");

    let out = backup(&home, &["create", "nope", "--output", "json"]);
    assert_eq!(out.status.code(), Some(3), "stderr: {}", stderr(&out));
    assert!(out.stdout.is_empty(), "错误路径不得输出数据");
}

/// 不存在的快照 id → 未找到 3（delete / verify 一致）
#[test]
fn backup_missing_snapshot_exits_3() {
    let home = temp_home();
    let ws = init_workspace(&home, "ws1");
    let _ = register_server(&home, &ws, "alpha");

    for sub in ["delete", "verify"] {
        let out = backup(&home, &[sub, "alpha", "nonexistent-id", "--output", "json"]);
        assert_eq!(out.status.code(), Some(3), "{sub} stderr: {}", stderr(&out));
        assert!(out.stdout.is_empty(), "{sub} 错误路径不得输出数据");
    }
}
