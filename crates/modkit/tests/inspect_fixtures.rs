//! 对真实形态 jar 的端到端检测
//!
//! fixture 里的 jar 都是手工构造的最小归档，形态取自实测的真实 mod
//! （例如 `broken_json.jar` 复刻 Debugify 在 description 里写裸换行的
//! 真实缺陷）。判定规则的正确性由 `modkit` 单元测试保证，这里验证
//! 「从字节到结论」这条链路。

// 测试用 expect 是刻意的：失败即说明 fixture 或读取链路坏了
#![allow(clippy::expect_used)]

use domain::mod_::{Confidence, ModLoader, Risk, SideSupport};
use modkit::ZipModInspector;
use port::mod_::ModInspector;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn inspect(name: &str) -> domain::mod_::ModCapability {
    ZipModInspector::new()
        .inspect(&fixture(name))
        .expect("fixture 应当可读")
}

#[test]
fn explicit_client_environment_is_blocking_for_server() {
    let cap = inspect("client_explicit.jar");
    assert_eq!(cap.id.as_deref(), Some("optifine_like"));
    assert_eq!(cap.loaders, vec![ModLoader::Fabric]);
    assert_eq!(cap.side.server, SideSupport::Unsupported);
    assert_eq!(cap.side.confidence, Confidence::Certain);
    assert_eq!(cap.risk(), Risk::Blocking);
}

#[test]
fn explicit_server_environment_is_ok() {
    let cap = inspect("server_explicit.jar");
    assert_eq!(cap.side.server, SideSupport::Required);
    assert_eq!(cap.risk(), Risk::Ok);
    // >=1.21 <1.22
    assert!(cap.mc_versions.contains("1.21.1"));
    assert!(!cap.mc_versions.contains("1.22"));
}

#[test]
fn both_environment_records_dependencies() {
    let cap = inspect("both.jar");
    assert_eq!(cap.side.server, SideSupport::Unknown);
    // 判不出服务端归属 → review，不是 ok
    assert_eq!(cap.risk(), Risk::Review);
    // 依赖要读全，cobblemon 是必需且非内置（该 fixture 是 Fabric 形态）
    let missing = cap.missing_mandatory_deps(&[], Some(ModLoader::Fabric));
    assert_eq!(missing, vec!["cobblemon"]);
    assert!(cap.java_required.is_none());
}

#[test]
fn broken_json_still_yields_metadata() {
    // Debugify 的真实形态：description 里有裸换行
    let cap = inspect("broken_json.jar");
    assert_eq!(cap.id.as_deref(), Some("debugify"));
    assert!(cap.mc_versions.contains("1.21.1"));
}

#[test]
fn forge_client_side_is_recorded_but_not_blocking() {
    // Forge 的 `[[mods]] side="CLIENT"` 装载器并不读，实测有大量服务端可用
    // 的 mod 这么写。只当证据留着，不据此判死。
    let cap = inspect("forge_client.jar");
    assert_eq!(cap.id.as_deref(), Some("sodiumlike"));
    assert_eq!(cap.loaders, vec![ModLoader::Forge]);
    assert_eq!(cap.side.client, SideSupport::Required);
    assert_eq!(cap.side.server, SideSupport::Unknown);
    assert_eq!(cap.risk(), Risk::Review);
}

#[test]
fn bukkit_plugin_is_server_only() {
    let cap = inspect("bukkit_plugin.jar");
    assert_eq!(cap.loaders, vec![ModLoader::Bukkit]);
    assert_eq!(cap.side.server, SideSupport::Required);
    assert_eq!(cap.side.client, SideSupport::Unsupported);
    assert_eq!(cap.risk(), Risk::Ok);
}

/// `api-version` 是插件唯一的版本线索，必须当成 MC 下界
///
/// 不当成区间的话，插件在「能否装到这台服务端」里永远只能给出「判不出」，
/// 而 `api-version: 1.21` 明明已经说明了它跑在 1.21 及以上。
#[test]
fn bukkit_api_version_becomes_mc_lower_bound() {
    let cap = inspect("bukkit_plugin.jar");
    assert_eq!(cap.mc_versions.min.as_deref(), Some("1.21"));
    assert_eq!(cap.mc_versions.max, None);
    assert!(cap.mc_versions.contains("1.21.1"));
    assert!(!cap.mc_versions.contains("1.20.6"));
}

#[test]
fn resource_pack_is_recognized_as_such() {
    let cap = inspect("resource_pack.jar");
    assert_eq!(cap.loaders, vec![ModLoader::ResourcePack]);
}

#[test]
fn unknown_archive_is_review_not_ok() {
    let cap = inspect("unknown.jar");
    assert!(cap.loaders.is_empty());
    assert_eq!(cap.risk(), Risk::Review);
}

#[test]
fn java_version_comes_from_bytecode() {
    // fixture 里 class 头写的是 0x41 = 65（class major），换算成 Java 21
    let cap = inspect("client_explicit.jar");
    assert_eq!(cap.java_compiled, Some(21));
    // 元数据声明 >=21 也读到了
    assert_eq!(cap.java_required, Some(21));
}

#[test]
fn non_zip_file_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("not-a-jar.jar");
    std::fs::write(&path, b"this is not a zip").expect("write");
    let err = ZipModInspector::new()
        .inspect(&path)
        .expect_err("must fail");
    assert!(matches!(
        err,
        port::mod_::ModInspectError::NotAnArchive { .. }
    ));
}
