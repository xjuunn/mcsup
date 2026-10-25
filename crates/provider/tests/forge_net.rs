//! Forge 来源的端到端探测（真实请求官方 API）
//!
//! **默认不跑**，用 `#[ignore]` 标出，而不是靠环境变量提前 `return`：
//!
//! ```text
//! cargo test -p provider --test forge_net -- --ignored --nocapture
//! ```
//!
//! 原因：靠环境变量跳过的测试在报告里仍是 `passed`，看起来「覆盖了联网路径」，
//! 实际一行都没跑。`ignored` 在结果里单独计数，一眼就能看出它没被执行。
//! 夜间 job 会显式跑这一组（见 `.github/workflows/test.yml`）。
//!
//! 这里只验证「端点可达 + 解析结果与官方一致」，与 `testkit::http` 的离线
//! 回环测试互补：离线测试钉协议层，这组钉上游的真实响应形状。

use domain::server::artifact::ChecksumAlgorithm;
use domain::server::version::{ServerVariant, VersionId};
use port::ServerJarSource;
use provider::forge::ForgeSource;

/// `versions()` 能拉到真实 MC 版本列表，且声明 forge 变体
#[tokio::test]
#[ignore = "需要真实网络：用 --ignored 手动执行，或由夜间 job 跑"]
async fn versions_reach_official_api() {
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
#[ignore = "需要真实网络：用 --ignored 手动执行，或由夜间 job 跑"]
async fn artifact_resolves_installer_with_sha1() {
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
#[ignore = "需要真实网络：用 --ignored 手动执行，或由夜间 job 跑"]
async fn missing_mc_version_is_not_found() {
    let source = ForgeSource::new().expect("forge source");
    let err = source
        .artifact(&VersionId::new("1.99.999"))
        .await
        .expect_err("不存在的版本应报错");
    assert!(matches!(err, port::SourceError::NotFound(_)), "{err:?}");
}
