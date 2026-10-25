//! Purpur / Leaf / Quilt / `SpongeVanilla` 的端到端契约测试
//!
//! 默认不跑；设 `MCSUP_NET_TESTS=1` 手动执行。
//!
//! 这四个来源各自对应一种上游形态，放在一个文件里便于对照「形态 → 断言」：
//!
//! | 来源 | 形态 | 校验能力 |
//! | --- | --- | --- |
//! | Purpur | 自有 REST，单 JAR | 无哈希 |
//! | Leaf | 自有 REST，单 JAR | 内嵌 sha256 |
//! | Quilt | 自有 Meta + 通用安装器 | 无哈希 |
//! | SpongeVanilla | Maven XML，单 JAR | `.sha1` 侧车 |

use domain::server::artifact::ChecksumAlgorithm;
use domain::server::version::{ServerVariant, VersionId};
use port::{InstallKind, ServerJarSource};
use provider::leaf::LeafSource;
use provider::purpur::PurpurSource;
use provider::quilt::QuiltSource;
use provider::sponge::SpongeSource;

fn enabled() -> bool {
    std::env::var("MCSUP_NET_TESTS").is_ok_and(|v| v == "1")
}

/// Purpur：版本列表可达，`1.21.1` 解析出无校验的单 JAR
#[tokio::test]
async fn purpur_lists_and_resolves() {
    if !enabled() {
        return;
    }
    let source = PurpurSource::new().expect("purpur source");
    let versions = source.versions().await.expect("versions");
    assert!(!versions.is_empty());
    assert!(versions.iter().all(|v| v.variant == ServerVariant::Purpur));
    assert!(versions.iter().any(|v| v.id.as_str() == "1.21.1"));

    let resolved = source
        .artifact(&VersionId::new("1.21.1"))
        .await
        .expect("artifact");
    assert_eq!(resolved.install, InstallKind::Direct);
    assert!(
        resolved.location.as_str().ends_with("/download"),
        "{}",
        resolved.location
    );
    // 上游不提供哈希：只能是 unverified
    assert!(
        !resolved.artifact.checksum.is_verified(),
        "purpur 没有哈希，不应声称已验证"
    );
}

/// Leaf：版本列表可达，解析出内嵌 sha256
#[tokio::test]
async fn leaf_lists_and_resolves_with_sha256() {
    if !enabled() {
        return;
    }
    let source = LeafSource::new().expect("leaf source");
    let versions = source.versions().await.expect("versions");
    assert!(!versions.is_empty());
    assert!(versions.iter().all(|v| v.variant == ServerVariant::Leaf));

    let resolved = source
        .artifact(&VersionId::new("latest"))
        .await
        .expect("artifact");
    assert_eq!(resolved.install, InstallKind::Direct);
    assert_eq!(
        resolved.artifact.checksum.algorithm(),
        ChecksumAlgorithm::Sha256
    );
    assert!(resolved.artifact.checksum.is_verified());
    assert!(
        resolved.location.as_str().contains("/downloads/leaf-"),
        "{}",
        resolved.location
    );
}

/// Quilt：MC 版本列表带 stable 标记，`1.21.1` 解析出官方安装器
#[tokio::test]
async fn quilt_lists_and_resolves_installer() {
    if !enabled() {
        return;
    }
    let source = QuiltSource::new().expect("quilt source");
    let versions = source.versions().await.expect("versions");
    assert!(!versions.is_empty());
    assert!(versions.iter().all(|v| v.variant == ServerVariant::Quilt));
    assert!(
        versions.iter().any(|v| v.id.as_str() == "1.21.1"),
        "应包含 1.21.1"
    );

    let resolved = source
        .artifact(&VersionId::new("1.21.1"))
        .await
        .expect("artifact");
    assert_eq!(resolved.install, InstallKind::Installer);
    assert!(resolved.location.as_str().contains("quiltmc.org"));
    // 通用安装器地址不带版本号，没有哈希
    assert!(!resolved.artifact.checksum.is_verified());
}

/// SpongeVanilla：XML 元数据可达，`latest` 解析出带 `.sha1` 的单 JAR
#[tokio::test]
async fn spongevanilla_lists_and_resolves_with_sha1() {
    if !enabled() {
        return;
    }
    let source = SpongeSource::new().expect("sponge source");
    let versions = source.versions().await.expect("versions");
    assert!(!versions.is_empty());
    assert!(
        versions
            .iter()
            .all(|v| v.variant == ServerVariant::SpongeVanilla)
    );

    let resolved = source
        .artifact(&VersionId::new("latest"))
        .await
        .expect("artifact");
    assert_eq!(resolved.install, InstallKind::Direct);
    assert!(resolved.location.as_str().contains("spongevanilla-"));
    assert_eq!(
        resolved.artifact.checksum.algorithm(),
        ChecksumAlgorithm::Sha1
    );
}

/// 四个来源对不存在的版本都报 `NotFound`，不给出别的版本
#[tokio::test]
async fn unknown_versions_are_not_found() {
    if !enabled() {
        return;
    }
    let unknown = VersionId::new("1.99.999");
    let purpur = PurpurSource::new().expect("purpur");
    assert!(matches!(
        purpur.artifact(&unknown).await,
        Err(port::SourceError::NotFound(_))
    ));
    let leaf = LeafSource::new().expect("leaf");
    assert!(matches!(
        leaf.artifact(&unknown).await,
        Err(port::SourceError::NotFound(_))
    ));
    let quilt = QuiltSource::new().expect("quilt");
    assert!(matches!(
        quilt.artifact(&unknown).await,
        Err(port::SourceError::NotFound(_))
    ));
    let sponge = SpongeSource::new().expect("sponge");
    assert!(matches!(
        sponge.artifact(&unknown).await,
        Err(port::SourceError::NotFound(_))
    ));
}
