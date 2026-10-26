//! `.mrpack` 归档读取与解析的契约测试
//!
//! 一部分用例用**真实归档**（`MCSUP_MRPACK_FIXTURE` 指向的文件）验证 `unzip`
//! 路径；不设该变量时跳过，避免把几百 KB 的二进制夹具入库。另一部分用测试
//! 现场生成最小 zip（`zip` 命令），确保 `read_index` 的行为不依赖外部夹具，
//! 但同样需要系统有 `unzip`——与生产实现的前提一致。

#![allow(clippy::expect_used, reason = "测试夹具，失败直接 panic 才有用")]

use port::modpack::ModpackExtractor;
use provider::modrinth::MrpackExtractor;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 系统是否有 `zip`
fn has_zip() -> bool {
    Command::new("zip")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// 系统是否有 `unzip`
fn has_unzip() -> bool {
    Command::new("unzip")
        .arg("-v")
        .output()
        .is_ok_and(|out| out.status.success())
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mcsup-mrpack-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("tempdir");
    dir
}

/// 现场构造一个最小 `.mrpack`（含清单与 overrides）
fn build_mrpack(dir: &Path) -> PathBuf {
    let src = dir.join("src");
    std::fs::create_dir_all(src.join("overrides/config")).expect("mkdir overrides");
    std::fs::write(
        src.join("modrinth.index.json"),
        r#"{
            "formatVersion": 1,
            "game": "minecraft",
            "versionId": "9.9.9",
            "name": "Fixture Pack",
            "files": [
                {
                    "path": "mods/server.jar",
                    "hashes": {"sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                    "env": {"client": "required", "server": "required"},
                    "downloads": ["https://cdn.example.invalid/server.jar"],
                    "fileSize": 10
                },
                {
                    "path": "mods/client.jar",
                    "env": {"client": "required", "server": "unsupported"},
                    "downloads": ["https://cdn.example.invalid/client.jar"],
                    "fileSize": 10
                }
            ],
            "dependencies": {"minecraft": "1.21.1", "fabric-loader": "0.16.0"}
        }"#,
    )
    .expect("write index");
    std::fs::write(src.join("overrides/config/x.toml"), b"server = true\n")
        .expect("write override");

    let archive = dir.join("pack.mrpack");
    let status = Command::new("zip")
        .arg("-q")
        .arg("-r")
        .arg(&archive)
        .arg(".")
        .current_dir(&src)
        .status()
        .expect("run zip");
    assert!(status.success(), "zip 打包失败");
    archive
}

/// 读清单：字段完整、环境标记正确
#[test]
fn reads_index_from_generated_archive() {
    if !has_zip() || !has_unzip() {
        return;
    }
    let dir = temp_dir("index");
    let archive = build_mrpack(&dir);

    let index = MrpackExtractor::new().read_index(&archive).expect("读清单");

    assert_eq!(index.version_id, "9.9.9");
    assert_eq!(index.name, "Fixture Pack");
    assert_eq!(index.minecraft_version().expect("mc"), "1.21.1");
    assert_eq!(index.loader(), Some(("fabric", "0.16.0")));
    assert_eq!(index.files.len(), 2);
    // 服务端只要 server.jar；client.jar 是 server: unsupported
    let server: Vec<&str> = index
        .server_files()
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(server, vec!["mods/server.jar"]);
    // sha1 合法则可校验
    assert!(index.files[0].hashes.sha1_checksum().is_some());
    std::fs::remove_dir_all(&dir).ok();
}

/// 铺 overrides：落到目标目录，且不包含清单与 client-overrides
#[test]
fn extracts_overrides_only() {
    if !has_zip() || !has_unzip() {
        return;
    }
    let dir = temp_dir("overrides");
    let archive = build_mrpack(&dir);
    let dest = dir.join("server");

    MrpackExtractor::new()
        .extract_overrides(&archive, &dest)
        .expect("铺 overrides");

    assert_eq!(
        std::fs::read(dest.join("config/x.toml")).expect("override"),
        b"server = true\n"
    );
    // 清单不属于 overrides
    assert!(!dest.join("modrinth.index.json").exists());
    std::fs::remove_dir_all(&dir).ok();
}

/// 没有清单的归档：报 MissingIndex，而不是解析空对象
#[test]
fn missing_index_is_reported() {
    if !has_zip() || !has_unzip() {
        return;
    }
    let dir = temp_dir("noindex");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    std::fs::write(src.join("readme.txt"), b"x").expect("write");
    let archive = dir.join("empty.mrpack");
    let status = Command::new("zip")
        .arg("-q")
        .arg("-r")
        .arg(&archive)
        .arg(".")
        .current_dir(&src)
        .status()
        .expect("run zip");
    assert!(status.success());

    let err = MrpackExtractor::new()
        .read_index(&archive)
        .expect_err("应报缺清单");
    assert!(
        matches!(err, port::modpack::ModpackExtractError::MissingIndex(_)),
        "{err:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 非 zip 文件：报缺清单或解压失败，而不是 panic
#[test]
fn non_zip_reports_error() {
    if !has_unzip() {
        return;
    }
    let dir = temp_dir("notzip");
    let archive = dir.join("bad.mrpack");
    std::fs::write(&archive, b"not a zip").expect("write");
    let err = MrpackExtractor::new()
        .read_index(&archive)
        .expect_err("应报错");
    assert!(
        matches!(
            err,
            port::modpack::ModpackExtractError::MissingIndex(_)
                | port::modpack::ModpackExtractError::Extract(_)
        ),
        "{err:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 外部真实夹具（可选）：设 `MCSUP_MRPACK_FIXTURE=/path/x.mrpack` 时验证
#[test]
fn reads_external_fixture_when_provided() {
    let Ok(path) = std::env::var("MCSUP_MRPACK_FIXTURE") else {
        return;
    };
    let archive = PathBuf::from(path);
    if !archive.is_file() {
        return;
    }
    let index = MrpackExtractor::new()
        .read_index(&archive)
        .expect("真实夹具应能解析");
    assert!(!index.files.is_empty());
    assert!(index.minecraft_version().is_ok());
}
