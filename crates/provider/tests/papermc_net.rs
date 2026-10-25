//! Paper 系四变体（Paper / Folia / Velocity / Waterfall）的端到端契约测试
//!
//! 默认不跑；设 `MCSUP_NET_TESTS=1` 手动执行。
//!
//! 这里遍历 [`PaperProject`]，验证「一个实现服务四个变体」在真实上游成立：
//! 每个项目的版本列表都能拉到、都能解析出带 sha256 的下载地址。

use domain::server::artifact::ChecksumAlgorithm;
use domain::server::version::{ServerVariant, VersionId};
use port::{InstallKind, ServerJarSource};
use provider::papermc::{PaperProject, PaperSource};

const PROJECTS: [PaperProject; 4] = [
    PaperProject::Paper,
    PaperProject::Folia,
    PaperProject::Velocity,
    PaperProject::Waterfall,
];

fn enabled() -> bool {
    std::env::var("MCSUP_NET_TESTS").is_ok_and(|v| v == "1")
}

/// 四个项目各自拉到真实版本列表，并声明对应变体
#[tokio::test]
async fn every_project_lists_versions() {
    if !enabled() {
        return;
    }
    for project in PROJECTS {
        let source = PaperSource::new(project).expect("paper source");
        let versions = source.versions().await.expect("versions");
        assert!(!versions.is_empty(), "{project} 应有版本");
        assert!(
            versions.iter().all(|v| v.variant == project.variant()),
            "{project} 版本列表变体不一致"
        );
    }
}

/// Paper 的 `1.21.1` 解析出内嵌 sha256 与真实大小
#[tokio::test]
async fn paper_artifact_has_embedded_sha256() {
    if !enabled() {
        return;
    }
    let source = PaperSource::new(PaperProject::Paper).expect("paper source");
    let resolved = source
        .artifact(&VersionId::new("1.21.1"))
        .await
        .expect("artifact");
    assert_eq!(resolved.install, InstallKind::Direct);
    assert_eq!(
        resolved.artifact.checksum.algorithm(),
        ChecksumAlgorithm::Sha256
    );
    assert!(resolved.artifact.checksum.is_verified());
    assert!(resolved.artifact.size > 0, "应带 Content-Length");
    assert!(
        resolved
            .location
            .as_str()
            .starts_with("https://fill-data.papermc.io/"),
        "下载地址应指向 fill-data，而不是自行拼接的路径"
    );
}

/// 四个项目的 `latest` 都能解析出可下载物
#[tokio::test]
async fn every_project_resolves_latest() {
    if !enabled() {
        return;
    }
    for project in PROJECTS {
        let source = PaperSource::new(project).expect("paper source");
        let resolved = source
            .artifact(&VersionId::new("latest"))
            .await
            .unwrap_or_else(|err| panic!("{project} latest: {err:?}"));
        assert_eq!(resolved.install, InstallKind::Direct);
        assert_eq!(resolved.location.scheme(), "https");
    }
}

/// 不存在的 MC 版本报 `NotFound`
#[tokio::test]
async fn unknown_version_is_not_found() {
    if !enabled() {
        return;
    }
    let source = PaperSource::new(PaperProject::Paper).expect("paper source");
    let err = source
        .artifact(&VersionId::new("1.99.999"))
        .await
        .expect_err("应报 NotFound");
    assert!(matches!(err, port::SourceError::NotFound(_)), "{err:?}");
}

/// 代理变体的变体标识与项目一一对应
#[test]
fn variant_mapping_is_stable() {
    assert_eq!(PaperProject::Paper.variant(), ServerVariant::Paper);
    assert_eq!(PaperProject::Folia.variant(), ServerVariant::Folia);
    assert_eq!(PaperProject::Velocity.variant(), ServerVariant::Velocity);
    assert_eq!(PaperProject::Waterfall.variant(), ServerVariant::Waterfall);
}
