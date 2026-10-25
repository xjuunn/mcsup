//! Forge 来源的端到端契约测试（真实请求官方 API）
//!
//! 默认不跑：联网测试在 CI 上不稳定，且会拖慢门禁。设 `MCSUP_NET_TESTS=1`
//! 手动执行，用于验证「端点可达 + 解析结果与官方一致」。

use domain::server::artifact::ChecksumAlgorithm;
use domain::server::version::{ServerVariant, VersionId};
use port::ServerJarSource;
use provider::forge::ForgeSource;

fn enabled() -> bool {
    std::env::var("MCSUP_NET_TESTS").is_ok_and(|v| v == "1")
}

/// `versions()` 能拉到真实 MC 版本列表，且声明 forge 变体
#[tokio::test]
async fn versions_reach_official_api() {
    if !enabled() {
        return;
    }
    let source = ForgeSource::new().expect("forge source");
    let versions = source.versions().await.expect("versions");
    assert!(!versions.is_empty(), "官方应返回 MC 版本列表");
    assert!(versions.iter().all(|v| v.variant == ServerVariant::Forge));
    assert!(
        versions.iter().any(|v| v.id.as_str() == "1.21.1"),
        "应包含 1.21.1"
    );
}

/// `artifact("1.21.1")` 解析出安装器地址与真实 SHA1
#[tokio::test]
async fn artifact_resolves_installer_with_sha1() {
    if !enabled() {
        return;
    }
    let source = ForgeSource::new().expect("forge source");
    let resolved = source
        .artifact(&VersionId::new("1.21.1"))
        .await
        .expect("artifact");
    let url = resolved.location.as_str();
    assert!(
        url.starts_with("https://maven.minecraftforge.net/"),
        "{url}"
    );
    assert!(url.ends_with("-installer.jar"), "{url}");
    let checksum = &resolved.artifact.checksum;
    assert!(checksum.is_verified(), "Forge 有 SHA1 侧车，应强校验");
    assert_eq!(checksum.algorithm(), ChecksumAlgorithm::Sha1);
    assert_eq!(checksum.hex().len(), 40);
}

/// 不存在的 MC 版本如实报 `NotFound`
#[tokio::test]
async fn missing_mc_version_is_not_found() {
    if !enabled() {
        return;
    }
    let source = ForgeSource::new().expect("forge source");
    let err = source
        .artifact(&VersionId::new("1.99.999"))
        .await
        .expect_err("不存在的版本应报错");
    assert!(matches!(err, port::SourceError::NotFound(_)), "{err:?}");
}
