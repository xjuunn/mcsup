//! `NeoForge` 来源的端到端契约测试（真实请求官方 Maven）
//!
//! 默认不跑：联网测试在 CI 上不稳定，且会拖慢门禁。设 `MCSUP_NET_TESTS=1`
//! 手动执行，用于验证「端点可达 + 解析结果与官方一致」。

use domain::server::artifact::ChecksumAlgorithm;
use domain::server::version::{ServerVariant, VersionId};
use port::{InstallKind, ServerJarSource};
use provider::neoforge::NeoForgeSource;

fn enabled() -> bool {
    std::env::var("MCSUP_NET_TESTS").is_ok_and(|v| v == "1")
}

/// `versions()` 能拉到真实 MC 版本列表，且声明 neoforge 变体
#[tokio::test]
async fn versions_reach_official_api() {
    if !enabled() {
        return;
    }
    let source = NeoForgeSource::new().expect("neoforge source");
    let versions = source.versions().await.expect("versions");
    assert!(!versions.is_empty(), "官方应返回 MC 版本列表");
    assert!(
        versions
            .iter()
            .all(|v| v.variant == ServerVariant::NeoForge)
    );
    // 不应出现无法映射到正式 MC 的快照项；MC 从 `1.21.11` 之后改用年份制版本号
    // （`26.x`），两代形状都应出现，且绝不能出现虚构的 `1.26.x`
    assert!(
        versions.iter().all(|v| {
            let id = v.id.as_str();
            (id.starts_with("1.") || id.starts_with("26.")) && !id.starts_with("1.26.")
        }),
        "只应包含正式 MC 版本"
    );
    assert!(
        versions.iter().any(|v| v.id.as_str() == "1.21.1"),
        "应包含 1.21.1"
    );
    assert!(
        versions.iter().any(|v| v.id.as_str().starts_with("26.")),
        "应包含年份制 26.x 版本"
    );
}

/// 年份制 MC 版本（`26.3`）解析出四段 build 的 installer 与真实 SHA1
#[tokio::test]
async fn artifact_resolves_year_scheme_version() {
    if !enabled() {
        return;
    }
    let source = NeoForgeSource::new().expect("neoforge source");
    let resolved = source
        .artifact(&VersionId::new("26.3"))
        .await
        .expect("artifact");
    assert_eq!(resolved.install, InstallKind::Installer);
    assert!(
        resolved
            .location
            .as_str()
            .contains("/releases/net/neoforged/neoforge/26.3.0."),
        "{}",
        resolved.location.as_str()
    );
    assert!(
        resolved.artifact.checksum.is_verified(),
        "官方提供 .sha1，应强校验"
    );
}

/// `artifact("1.21.1")` 解析出 installer 地址与真实 SHA1
#[tokio::test]
async fn artifact_resolves_installer_with_sha1() {
    if !enabled() {
        return;
    }
    let source = NeoForgeSource::new().expect("neoforge source");
    let resolved = source
        .artifact(&VersionId::new("1.21.1"))
        .await
        .expect("artifact");
    assert_eq!(resolved.install, InstallKind::Installer);
    assert!(
        resolved
            .location
            .as_str()
            .contains("/releases/net/neoforged/neoforge/21.1.")
    );
    assert!(resolved.location.as_str().ends_with("-installer.jar"));
    assert_eq!(
        resolved.artifact.checksum.algorithm(),
        ChecksumAlgorithm::Sha1
    );
    assert!(
        resolved.artifact.checksum.is_verified(),
        "官方提供 .sha1，应强校验"
    );
    assert_eq!(resolved.artifact.version, VersionId::new("1.21.1"));
}

/// 不存在的 MC 版本报 `NotFound`，而不是给出某个别的版本
#[tokio::test]
async fn unknown_version_is_not_found() {
    if !enabled() {
        return;
    }
    let source = NeoForgeSource::new().expect("neoforge source");
    let err = source
        .artifact(&VersionId::new("1.99.999"))
        .await
        .expect_err("应报 NotFound");
    assert!(matches!(err, port::SourceError::NotFound(_)), "{err:?}");
}

/// 安装器地址真实可下载（HEAD 200）
#[tokio::test]
async fn installer_url_is_reachable() {
    if !enabled() {
        return;
    }
    let source = NeoForgeSource::new().expect("neoforge source");
    let resolved = source
        .artifact(&VersionId::new("1.21.1"))
        .await
        .expect("artifact");
    let response = reqwest::Client::new()
        .head(resolved.location)
        .header("user-agent", "mcsup-net-test")
        .send()
        .await
        .expect("head installer");
    assert!(response.status().is_success(), "{}", response.status());
}
