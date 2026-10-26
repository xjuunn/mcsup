//! `.mrpack` 归档读取与解析的契约测试
//!
//! 归档现场用 `zip` crate 生成（与生产解压同一套实现），因此不依赖系统
//! `zip` / `unzip`。一部分用例用**真实归档**（`MCSUP_MRPACK_FIXTURE` 指向
//! 的文件）验证，不设该变量时跳过，避免把几百 KB 的二进制夹具入库。

#![allow(clippy::expect_used, reason = "测试夹具，失败直接 panic 才有用")]

use port::modpack::ModpackExtractor;
use provider::modrinth::MrpackExtractor;
use std::io::Write;
use std::path::{Path, PathBuf};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

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

/// 按给定条目生成 zip；名字按 UTF-8 写入
fn build_archive(path: &Path, entries: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).expect("create archive");
    let mut writer = ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    for (name, contents) in entries {
        writer.start_file(*name, options).expect("start entry");
        writer.write_all(contents).expect("write entry");
    }
    writer.finish().expect("finish archive");
}

/// 现场构造一个最小 `.mrpack`（含清单与 overrides）
fn build_mrpack(dir: &Path) -> PathBuf {
    let archive = dir.join("pack.mrpack");
    build_archive(
        &archive,
        &[
            (
                "modrinth.index.json",
                br#"{
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
            ),
            ("overrides/config/x.toml", b"server = true\n"),
        ],
    );
    archive
}

/// 读清单：字段完整、环境标记正确
#[test]
fn reads_index_from_generated_archive() {
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

/// 非 ASCII 文件名按 UTF-8 落盘，不产生乱码
#[test]
fn extracts_utf8_entry_names() {
    let dir = temp_dir("utf8");
    let name = "overrides/resourcepacks/ZI100D_僵尸入侵100天_t251202.zip";
    let archive = dir.join("pack.mrpack");
    build_archive(&archive, &[(name, b"inner zip bytes")]);
    let dest = dir.join("server");

    MrpackExtractor::new()
        .extract_overrides(&archive, &dest)
        .expect("铺 overrides");

    assert_eq!(
        std::fs::read(dest.join("resourcepacks/ZI100D_僵尸入侵100天_t251202.zip"))
            .expect("utf8 name survives extraction"),
        b"inner zip bytes"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `server-overrides/` 优先：同名文件以服务端专属版本为准
#[test]
fn server_overrides_take_precedence() {
    let dir = temp_dir("server-overrides");
    let archive = dir.join("pack.mrpack");
    build_archive(
        &archive,
        &[
            ("server-overrides/config/x.toml", b"server\n"),
            ("overrides/config/x.toml", b"plain\n"),
            ("overrides/config/y.toml", b"plain only\n"),
        ],
    );
    let dest = dir.join("server");

    MrpackExtractor::new()
        .extract_overrides(&archive, &dest)
        .expect("铺 overrides");

    assert_eq!(
        std::fs::read(dest.join("config/x.toml")).expect("x"),
        b"server\n"
    );
    assert_eq!(
        std::fs::read(dest.join("config/y.toml")).expect("y"),
        b"plain only\n"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 覆盖层里的路径越界：拒绝，而不是写到目标目录之外
#[test]
fn rejects_unsafe_override_path() {
    let dir = temp_dir("unsafe");
    let archive = dir.join("pack.mrpack");
    build_archive(&archive, &[("overrides/../../escape.txt", b"x")]);
    let dest = dir.join("server");

    MrpackExtractor::new()
        .extract_overrides(&archive, &dest)
        .expect_err("越界路径应报错");
    assert!(!dir.join("escape.txt").exists());
    std::fs::remove_dir_all(&dir).ok();
}

/// 没有清单的归档：报 MissingIndex，而不是解析空对象
#[test]
fn missing_index_is_reported() {
    let dir = temp_dir("noindex");
    let archive = dir.join("empty.mrpack");
    build_archive(&archive, &[("readme.txt", b"x")]);

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
