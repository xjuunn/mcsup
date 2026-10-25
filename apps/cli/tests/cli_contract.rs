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
    run_in(home, home, args)
}

/// 指定子进程工作目录的变体（验证「终端停在待删目录」这类路径相关行为）
fn run_in(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    run_full(cwd, home, args, &[])
}

/// 带额外环境变量的运行：语言相关断言必须显式指定，否则结果随机器 locale 变
fn run_with_env(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    run_full(home, home, args, envs)
}

/// `run_in` + `run_with_env` 的公共实现：工作目录与附加环境变量互不排斥
fn run_full(cwd: &Path, home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mcsup"));
    cmd.current_dir(cwd)
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_DATA_HOME", home.join("share"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        // 清掉可能干扰语言判定的外部变量，由调用方显式给出
        .env_remove("MCSUP_LANG")
        .env_remove("LANG")
        .env_remove("LC_ALL")
        .env_remove("LC_CTYPE")
        // 空值覆盖到配置文件里的默认值，避免落到用户真实环境
        .env("MCSUP_WORKSPACE", "")
        .stdin(Stdio::null()) // 非 TTY：交互提问会被拒
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

/// 非交互缺名称 → 用法错误 2；不落库、不联网，stdout 无输出
#[test]
fn create_without_name_exits_2() {
    let home = temp_home();
    let out = run(
        home.path(),
        &["server", "create", "--output", "json", "--lang", "zh"],
    );
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
            "--lang",
            "zh",
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
            "--lang",
            "zh",
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
    let out = run(
        home.path(),
        &["workspace", "use", "--output", "json", "--lang", "zh"],
    );
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

/// 终端工作目录就在待删工作区里：`workspace remove --purge` 退出 4，目录与登记都保留
///
/// Windows 上子进程改不了父进程的工作目录，真删掉之后调用方终端会停在已不
/// 存在的路径上，之后任何子进程都拿不到工作目录（命令静默不执行）。
#[test]
fn workspace_purge_from_inside_dir_exits_4() {
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

    // 子进程的工作目录 = 工作区目录本身
    let out = run_full(
        &ws,
        home.path(),
        &["workspace", "remove", "ws1", "--purge", "--output", "json"],
        &[("MCSUP_LANG", "en")],
    );
    assert_eq!(out.status.code(), Some(4), "stderr: {}", stderr(&out));
    assert!(out.stdout.is_empty(), "拒绝时不得输出数据");
    let err = stderr(&out);
    assert!(err.contains("is inside"), "{err}");
    assert!(err.contains("move out of it first"), "{err}");

    let list = run(home.path(), &["workspace", "list", "--output", "json"]);
    let rows: serde_json::Value = serde_json::from_slice(&list.stdout).expect("list 输出数组");
    assert_eq!(rows.as_array().expect("array").len(), 1, "登记必须保留");
    assert!(ws.join("workspace.toml").is_file(), "目录必须原样保留");
}

/// `--purge` 时目录本就不存在：退出 0 注销登记，JSON 必须如实区分
/// 「请求了删盘」与「磁盘真的删了」
#[test]
fn workspace_purge_missing_dir_reports_disk_not_purged() {
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
    // 目录被人手工删掉，登记还在
    std::fs::remove_dir_all(&ws).expect("rmdir");

    let out = run(
        home.path(),
        &["workspace", "remove", "ws1", "--purge", "--output", "json"],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let payload: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("remove stdout 必须是单个 JSON 对象");
    assert_eq!(payload["name"], "ws1");
    assert_eq!(payload["purged"], true, "请求了删盘");
    assert_eq!(payload["disk_purged"], false, "磁盘目录本就不存在");

    let list = run(home.path(), &["workspace", "list", "--output", "json"]);
    let rows: serde_json::Value = serde_json::from_slice(&list.stdout).expect("list 输出数组");
    assert!(rows.as_array().expect("array").is_empty(), "登记必须清理");
}

/// 正常删盘：`purged` 与 `disk_purged` 都为 true
#[test]
fn workspace_purge_existing_dir_reports_disk_purged() {
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

    let out = run(
        home.path(),
        &["workspace", "remove", "ws1", "--purge", "--output", "json"],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let payload: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("remove stdout 必须是单个 JSON 对象");
    assert_eq!(payload["purged"], true);
    assert_eq!(payload["disk_purged"], true);
    assert!(!ws.exists());
}

/// 空注册表：doctor 退出 0，stdout 单行 JSON，字段形状固定
#[test]
fn workspace_doctor_clean_registry_exits_0() {
    let home = temp_home();
    let out = run(home.path(), &["workspace", "doctor", "--output", "json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let payload: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("doctor stdout 必须是单个 JSON 对象");
    assert_eq!(payload["schema"], 1);
    assert_eq!(payload["has_errors"], false);
    assert_eq!(payload["errors"], 0);
    assert_eq!(payload["warnings"], 0);
    assert_eq!(payload["findings"].as_array().expect("array").len(), 0);
}

/// 工作目录被删 → 错误级发现，退出 1（数据要人处理，不是程序崩溃）
#[test]
fn workspace_doctor_missing_dir_exits_1() {
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
    std::fs::remove_dir_all(&ws).expect("rmdir");

    let out = run(home.path(), &["workspace", "doctor", "--output", "json"]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    let payload: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(payload["has_errors"], true);
    let findings = payload["findings"].as_array().expect("array");
    let kinds: Vec<&str> = findings
        .iter()
        .map(|f| f["kind"].as_str().expect("kind"))
        .collect();
    assert!(kinds.contains(&"workspace-dir-missing"), "{kinds:?}");
    // 结构化字段可寻址，不是只给人看的字符串
    let missing = findings
        .iter()
        .find(|f| f["kind"] == "workspace-dir-missing")
        .expect("finding");
    assert_eq!(missing["level"], "error");
    assert_eq!(missing["workspace"], "ws1");
    assert_eq!(missing["detail"]["kind"], "workspace-dir-missing");
    assert!(missing["detail"]["path"].is_string());
    assert!(missing["message"].is_string(), "兼容字段仍在");
}

/// `--only` 拼错 → 用法错误 2，列出可用检查项
#[test]
fn workspace_doctor_unknown_only_exits_2() {
    let home = temp_home();
    let out = run(
        home.path(),
        &[
            "workspace",
            "doctor",
            "--only",
            "nope",
            "--output",
            "json",
            "--lang",
            "zh",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(out.stdout.is_empty(), "用法错误不得输出数据");
    let err = stderr(&out);
    assert!(err.contains("未知检查项"), "{err}");
    assert!(err.contains("dangling-link"), "{err}");
}

/// `--only` 过滤只影响展示：没选中的错误级发现不左右退出码
#[test]
fn workspace_doctor_only_filters_report_and_exit() {
    let home = temp_home();
    let ws = home.path().join("wsdir");
    run(
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
    std::fs::remove_dir_all(&ws).expect("rmdir");

    let out = run(
        home.path(),
        &[
            "workspace",
            "doctor",
            "--only",
            "dangling-link",
            "--output",
            "json",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let payload: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(payload["errors"], 0);
    let findings = payload["findings"].as_array().expect("array");
    assert!(
        findings
            .iter()
            .all(|f| f["kind"] != "workspace-dir-missing"),
        "{findings:?}"
    );
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
        &["--help", "--lang", "zh"],
        &["workspace", "-h", "--lang", "zh"],
        &["server", "backup", "create", "--help", "--lang", "zh"],
        &["--lang", "zh", "help", "server"],
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
        for chinese in ["用法:", "选项:"] {
            assert!(text.contains(chinese), "{args:?} 缺少 {chinese}:\n{text}");
        }
    }
}

/// 内建 `-h` / `-V` 的说明也要中文化，长/短两种 help 提示都覆盖
#[test]
fn builtin_flag_help_is_chinese() {
    let home = temp_home();
    let out = run(home.path(), &["--help", "--lang", "zh"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("打印帮助信息"), "{text}");
    assert!(text.contains("打印版本信息"), "{text}");
    assert!(!text.contains("Print help"), "{text}");
    assert!(!text.contains("Print version"), "{text}");
}

/// 无参缺子命令：仍是帮助 + 用法错误 2，只是文案换成中文
///
/// 空参数走 clap 的 `arg_required_else_help` → 帮助渲染通道；语言只能由
/// 环境决定，此时未给任何命令行参数
#[test]
fn no_args_help_is_chinese_on_stderr() {
    let home = temp_home();
    let out = run_with_env(home.path(), &[], &[("MCSUP_LANG", "zh_CN.UTF-8")]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "用法错误不得输出数据");
    let err = stderr(&out);
    assert!(err.contains("用法:"), "{err}");
    assert!(!err.contains("Usage:"), "{err}");
    assert!(err.contains("命令:"), "{err}");
}

/// `--lang en` 在中文系统上也必须给英文帮助，且整份不残留任何中文
#[test]
fn help_output_follows_lang_flag() {
    let home = temp_home();
    // 覆盖各层：顶层 / 一级子命令 / 二级子命令 / 带参数说明的命令
    let cases: [&[&str]; 6] = [
        &["--help", "--lang", "en"],
        &["workspace", "-h", "--lang", "en"],
        &["server", "--help", "--lang", "en"],
        &["server", "create", "--help", "--lang", "en"],
        &["java", "install", "--help", "--lang", "en"],
        &["workspace", "doctor", "--help", "--lang", "en"],
    ];
    for args in cases {
        let out = run(home.path(), args);
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("Usage:"), "{args:?}\n{text}");
        assert!(text.contains("Options:"), "{args:?}\n{text}");
        assert!(
            !has_cjk(&text),
            "{args:?} 英文帮助残留中文:\n{}",
            first_cjk_run(&text)
        );
    }
    // `--lang=en` 等号写法与分开写等价，且在子命令之前也生效
    let out = run(home.path(), &["--lang=en", "workspace", "-h"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("Usage:"));
}

/// 是否含中日韩统一表意文字
fn has_cjk(text: &str) -> bool {
    text.chars()
        .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3000}'..='\u{303f}').contains(&c))
}

/// `--lang en` 下错误提示与界面文案不得混入中文
///
/// 只断言不出现 CJK，不断言具体措辞：文案措辞会调整，语言混排是回归。
#[test]
fn english_errors_have_no_chinese() {
    let home = temp_home();
    // 注册表错误（服务层 kind）
    let out = run(home.path(), &["server", "remove", "nope", "--lang", "en"]);
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !has_cjk(&stderr),
        "stderr 混入中文:\n{}",
        first_cjk_run(&stderr)
    );
    assert!(stderr.contains("not found"), "{stderr}");

    // 工作区错误（service 判定 + CLI 渲染）
    let missing = format!("{}", home.path().join("missing").display());
    let out = run(home.path(), &["workspace", "use", &missing, "--lang", "en"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !has_cjk(&stderr),
        "stderr 混入中文:\n{}",
        first_cjk_run(&stderr)
    );
}

/// `--lang zh` 下同一类错误给出中文
#[test]
fn chinese_errors_are_chinese() {
    let home = temp_home();
    let out = run(home.path(), &["server", "remove", "nope", "--lang", "zh"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(has_cjk(&stderr), "{stderr}");
    assert!(stderr.contains("不存在"), "{stderr}");
    // 不应再有英文兜底残留（Display 的英文短句）
    assert!(!stderr.contains("server not found"), "{stderr}");
}

/// 报错时给出第一段中文及上下文，便于定位漏译条目
fn first_cjk_run(text: &str) -> String {
    for line in text.lines() {
        if has_cjk(line) {
            return format!("  {line}");
        }
    }
    "  (none)".to_owned()
}

/// 未给 `--lang` 时读 `MCSUP_LANG`
#[test]
fn mcsup_lang_env_selects_language() {
    let home = temp_home();
    let out = run_with_env(home.path(), &["--help"], &[("MCSUP_LANG", "en")]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("Usage:"));
    let out = run_with_env(home.path(), &["--help"], &[("MCSUP_LANG", "zh_CN.UTF-8")]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("用法:"));
}

/// 平台 locale 决定语言：`LANG=en_US.UTF-8` 下不给中文
///
/// 只在 Linux 生效，理由同 `platform_locale_zh_gives_chinese`：macOS 不读 `LANG`。
#[test]
#[cfg(target_os = "linux")]
fn platform_locale_en_gives_english() {
    let home = temp_home();
    let out = run_with_env(home.path(), &["--help"], &[("LANG", "en_US.UTF-8")]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Usage:"), "{text}");
    assert!(!has_cjk(&text), "LANG=en_US.UTF-8 下不应出现中文:\n{text}");
}

/// 平台 locale 决定语言：`LANG=C` 无语言信息，回退英文
///
/// 只在 Linux 生效：macOS 不读 `LANG`，runner 上无论怎么设都可能是中文。
#[test]
#[cfg(target_os = "linux")]
fn platform_locale_c_falls_back_to_english() {
    let home = temp_home();
    let out = run_with_env(home.path(), &["--help"], &[("LANG", "C")]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Usage:"), "{text}");
    assert!(!has_cjk(&text), "LANG=C 下不应出现中文:\n{text}");
}

/// 平台 locale 决定语言：`LANG=zh_CN.UTF-8` 给中文
///
/// 只在 Linux 生效。macOS 的 `sys_locale` 走 `CFLocaleCopyPreferredLanguages()`
/// 读系统语言偏好，**不读 `LANG`**，所以在 runner 上设 `LANG` 不会改变结果。
/// 用 `target_os` 而不是 `cfg(unix)`：后者会把 macOS 一起算进来。
#[test]
#[cfg(target_os = "linux")]
fn platform_locale_zh_gives_chinese() {
    let home = temp_home();
    let out = run_with_env(home.path(), &["--help"], &[("LANG", "zh_CN.UTF-8")]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("用法:"), "{text}");
}

/// 显式 `--lang` 不能被平台 locale 翻回去
///
/// 仍在 `cfg(unix)` 下跑，包括 macOS：`--lang` 走显式分支，完全不碰 `sys_locale`，
/// 所以这条在 macOS 上是有效的断言，与读不读 `LANG` 无关。
#[test]
#[cfg(unix)]
fn lang_flag_beats_platform_locale() {
    let home = temp_home();
    let out = run_with_env(
        home.path(),
        &["--help", "--lang", "en"],
        &[("LANG", "zh_CN.UTF-8")],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Usage:"), "{text}");
    assert!(!has_cjk(&text), "{text}");
}

/// `--lang` 压过 `MCSUP_LANG`
#[test]
fn lang_flag_overrides_env() {
    let home = temp_home();
    let out = run_with_env(
        home.path(),
        &["--help", "--lang", "en"],
        &[("MCSUP_LANG", "zh_CN.UTF-8")],
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("Usage:"));
}

/// 机器可读输出与语言无关：`--output json` 的 stdout 在两种语言下逐字节相同
#[test]
fn json_stdout_is_language_independent() {
    let home = temp_home();
    let zh = run(
        home.path(),
        &["workspace", "list", "--output", "json", "--lang", "zh"],
    );
    let en = run(
        home.path(),
        &["workspace", "list", "--output", "json", "--lang", "en"],
    );
    assert_eq!(zh.status.code(), en.status.code());
    assert!(zh.status.success(), "stderr: {}", stderr(&zh));
    assert_eq!(zh.stdout, en.stdout, "JSON 输出不得随语言变化");
    assert!(zh.stderr.is_empty(), "stderr: {}", stderr(&zh));
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

/// release CI 逐行比对 `mcsup --version` 与 workspace 版本：格式必须精确
#[test]
fn dash_version_is_exact_line() {
    let home = temp_home();
    let out = run(home.path(), &["--version"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim_end(),
        concat!("mcsup ", env!("CARGO_PKG_VERSION"))
    );
}

/// `mcsup version` 与 `--version` 是两条通道：JSON 字段齐全且版本号一致
#[test]
fn version_json_reports_build_fields() {
    let home = temp_home();
    let out = run(home.path(), &["version", "--output", "json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let payload: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("version stdout 必须是单个 JSON 对象");
    assert_eq!(payload["name"], "mcsup");
    assert_eq!(payload["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        payload["target"].as_str().is_some_and(|t| !t.is_empty()),
        "{payload}"
    );
    assert!(matches!(
        payload["profile"].as_str(),
        Some("debug" | "release")
    ));
    assert_eq!(payload["protocol_version"], 1);
}

/// 配置文件不存在时 `config show` 仍给出全部键的生效值，来源标 `default`
#[test]
fn config_show_without_file_reports_defaults() {
    let home = temp_home();
    let out = run(home.path(), &["config", "show", "--output", "json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let payload: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("config show 必须输出单个 JSON 对象");
    assert_eq!(payload["exists"], false);
    let items = payload["items"].as_array().expect("items");
    let keys: Vec<&str> = items
        .iter()
        .map(|i| i["key"].as_str().expect("key"))
        .collect();
    assert_eq!(keys, ["data-dir", "download-mirror", "java-source"]);
    assert!(items.iter().all(|i| i["source"] == "default"), "{items:?}");
}

/// `set` → `show` → `unset` 往返：文件落地、生效值回退、`unset` 幂等
#[test]
fn config_set_show_unset_roundtrip() {
    let home = temp_home();
    let config = home.path().join("custom").join("config.toml");
    let config = config.to_str().expect("utf8");

    let set = run(
        home.path(),
        &[
            "--config",
            config,
            "config",
            "set",
            "java-source",
            "azul",
            "--output",
            "json",
        ],
    );
    assert_eq!(set.status.code(), Some(0), "stderr: {}", stderr(&set));
    let set_payload: serde_json::Value =
        serde_json::from_slice(&set.stdout).expect("config set 单行对象");
    assert_eq!(set_payload["key"], "java-source");
    assert_eq!(set_payload["value"], "azul");
    assert_eq!(set_payload["previous"], serde_json::Value::Null);

    let show = run(
        home.path(),
        &["--config", config, "config", "show", "--output", "json"],
    );
    assert_eq!(show.status.code(), Some(0), "stderr: {}", stderr(&show));
    let show_payload: serde_json::Value = serde_json::from_slice(&show.stdout).expect("show");
    assert_eq!(show_payload["exists"], true);
    let java = show_payload["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["key"] == "java-source")
        .expect("java-source 行");
    assert_eq!(java["value"], "azul");
    assert_eq!(java["source"], "file");

    let unset = run(
        home.path(),
        &[
            "--config",
            config,
            "config",
            "unset",
            "java-source",
            "--output",
            "json",
        ],
    );
    assert_eq!(unset.status.code(), Some(0), "stderr: {}", stderr(&unset));
    let unset_payload: serde_json::Value = serde_json::from_slice(&unset.stdout).expect("unset");
    assert_eq!(unset_payload["value"], "all");
    assert_eq!(unset_payload["previous"], "azul");

    // 已是缺省再 unset：幂等成功，不制造「改了什么」的错觉
    let again = run(
        home.path(),
        &[
            "--config",
            config,
            "config",
            "unset",
            "java-source",
            "--output",
            "json",
        ],
    );
    assert_eq!(again.status.code(), Some(0), "stderr: {}", stderr(&again));
    let again_payload: serde_json::Value = serde_json::from_slice(&again.stdout).expect("unset2");
    assert_eq!(again_payload["previous"], serde_json::Value::Null);
    assert_eq!(again_payload["value"], "all");
}

/// 键名拼错 → 用法错误 2，并列出可用键
#[test]
fn config_set_unknown_key_exits_2() {
    let home = temp_home();
    let out = run(home.path(), &["config", "set", "nope", "1", "--lang", "zh"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(out.stdout.is_empty(), "用法错误不得输出数据");
    let err = stderr(&out);
    assert!(err.contains("未知配置键"), "{err}");
    assert!(err.contains("data-dir"), "{err}");
}

/// 取值不在枚举表内 → 用法错误 2，文件保持未创建（先校验后落地）
#[test]
fn config_set_invalid_value_exits_2_and_writes_nothing() {
    let home = temp_home();
    let config = home.path().join("config.toml");
    let config = config.to_str().expect("utf8");
    let out = run(
        home.path(),
        &[
            "--config",
            config,
            "config",
            "set",
            "java-source",
            "nope",
            "--lang",
            "zh",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("adoptium"), "{err}");
    assert!(!Path::new(config).exists(), "校验失败不得写文件");
}

/// 非交互下 `config edit` 无法进编辑器：退 2 而不是静默成功
#[test]
fn config_edit_without_tty_exits_2() {
    let home = temp_home();
    let out = run(home.path(), &["config", "edit", "--lang", "zh"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(out.stdout.is_empty(), "用法错误不得输出数据");
    assert!(stderr(&out).contains("交互终端"), "{}", stderr(&out));
}

/// `doctor --offline` 跳过联网检查：payload 明说 `online: false`，不伪装成通过
#[test]
fn doctor_offline_json_reports_online_false() {
    let home = temp_home();
    let out = run(home.path(), &["doctor", "--offline", "--output", "json"]);
    let payload: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("doctor 必须输出单个 JSON 对象");
    assert_eq!(payload["online"], false, "{payload}");
    assert_eq!(payload["schema"], 1);
    assert!(payload["findings"].is_array(), "{payload}");
    // 离线时不能出现联网检查项的结论
    let findings = payload["findings"].as_array().expect("findings");
    assert!(
        findings
            .iter()
            .all(|f| f["kind"] != "source-unreachable" && f["kind"] != "source-empty"),
        "{findings:?}"
    );
}

/// `doctor --only` 拼错 → 用法错误 2，列出可用检查项
#[test]
fn doctor_unknown_only_exits_2() {
    let home = temp_home();
    let out = run(home.path(), &["doctor", "--only", "nope"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("data-dir-missing"), "{err}");
}

/// 配置损坏时 doctor 必须报 `config-invalid` 并退 1：体检工具不能把故障报成通过
#[test]
fn doctor_reports_invalid_config() {
    let home = temp_home();
    let config = home.path().join("config.toml");
    std::fs::write(&config, "java-source = \"nope\"\n").expect("写入非法配置");
    let out = run(
        home.path(),
        &[
            "--config",
            config.to_str().expect("utf8"),
            "doctor",
            "--offline",
            "--output",
            "json",
        ],
    );
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    let payload: serde_json::Value = serde_json::from_slice(&out.stdout).expect("doctor json");
    assert_eq!(payload["has_errors"], true, "{payload}");
    let findings = payload["findings"].as_array().expect("findings");
    let invalid = findings
        .iter()
        .find(|f| f["kind"] == "config-invalid")
        .expect("必须报 config-invalid");
    assert_eq!(invalid["level"], "error");
    assert!(
        invalid["fix"]
            .as_str()
            .is_some_and(|f| f.contains("config")),
        "{invalid}"
    );
}

/// 简写与全拼是同一个命令：stdout 逐字节一致、退出码一致
#[test]
fn subcommand_shorthand_matches_long_form() {
    let home = temp_home();
    let ws = home.path().join("wsdir");
    let init = run(
        home.path(),
        &[
            "ws",
            "in",
            "--dir",
            ws.to_str().expect("utf8"),
            "--name",
            "ws1",
            "--output",
            "json",
        ],
    );
    assert_eq!(init.status.code(), Some(0), "stderr: {}", stderr(&init));

    for (short, long) in [
        (
            vec!["ws", "ls", "--output", "json"],
            vec!["workspace", "list", "--output", "json"],
        ),
        (
            vec!["srv", "ls", "--output", "json"],
            vec!["server", "list", "--output", "json"],
        ),
        (
            vec!["jv", "ls", "--output", "json"],
            vec!["java", "list", "--output", "json"],
        ),
    ] {
        let from_short = run(home.path(), &short);
        let from_long = run(home.path(), &long);
        assert_eq!(
            from_short.status.code(),
            from_long.status.code(),
            "{short:?} 与 {long:?} 退出码不同"
        );
        assert!(
            !from_short.stdout.is_empty(),
            "{short:?} 无 stdout，stderr: {}",
            stderr(&from_short)
        );
        assert_eq!(
            String::from_utf8_lossy(&from_short.stdout),
            String::from_utf8_lossy(&from_long.stdout),
            "{short:?} 与 {long:?} stdout 不一致"
        );
    }
}

/// 单字母不提供简写：按未知子命令报错退出，不推断、不执行
#[test]
fn single_letter_shorthand_is_rejected() {
    let home = temp_home();
    for args in [vec!["s"], vec!["w"], vec!["server", "l"]] {
        let out = run(home.path(), &args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} stderr: {}",
            stderr(&out)
        );
        assert!(out.stdout.is_empty(), "{args:?} 不得输出数据");
        assert!(
            stderr(&out).contains("unrecognized subcommand"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
}
