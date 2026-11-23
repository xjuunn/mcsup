//! GitHub 归档来源的联网契约测试
//!
//! 默认不跑；设 `MCSUP_NET_TESTS=1` 手动执行。
//!
//! 只验证「解析引用 → 下 codeload 归档 → 解包」这条链路真的通到上游：
//! 用 `octocat/Hello-World`（永远是仓库、永远没有 manifest），因此成功的标志
//! 是**在 manifest 这一步失败**——说明前面的网络与解包都成功了。具体 commit
//! 会随上游变动，不做硬编码断言。
#![allow(clippy::expect_used, reason = "联网契约测试，失败直接 panic 才有用")]

use port::plugin::{ArtifactOrigin, ArtifactSource, ErrorCode};
use provider::plugin::PluginArtifactSource;

fn enabled() -> bool {
    std::env::var("MCSUP_NET_TESTS").is_ok_and(|value| value == "1")
}

#[test]
fn resolves_head_and_downloads_the_archive() {
    if !enabled() {
        return;
    }
    let dest = tempfile::tempdir().expect("dest");
    let err = PluginArtifactSource::new()
        .fetch(
            &ArtifactOrigin::Github {
                owner: "octocat".to_owned(),
                repo: "Hello-World".to_owned(),
                rev: None,
            },
            dest.path(),
        )
        .expect_err("Hello-World has no manifest.json");
    // 走到「缺 manifest」说明解析、下载、解包都成功了
    assert_eq!(err.code, ErrorCode::InvalidInput);
    assert!(err.message.contains("manifest.json"), "{}", err.message);
}

#[test]
fn resolves_a_tag() {
    if !enabled() {
        return;
    }
    let dest = tempfile::tempdir().expect("dest");
    let err = PluginArtifactSource::new()
        .fetch(
            &ArtifactOrigin::Github {
                owner: "octocat".to_owned(),
                repo: "Hello-World".to_owned(),
                rev: Some("master".to_owned()),
            },
            dest.path(),
        )
        .expect_err("Hello-World has no manifest.json");
    assert!(err.message.contains("manifest.json"), "{}", err.message);
}

#[test]
fn unknown_repository_reports_unavailable() {
    if !enabled() {
        return;
    }
    let dest = tempfile::tempdir().expect("dest");
    let err = PluginArtifactSource::new()
        .fetch(
            &ArtifactOrigin::Github {
                owner: "mcsup-no-such-owner".to_owned(),
                repo: "mcsup-no-such-repo".to_owned(),
                rev: None,
            },
            dest.path(),
        )
        .expect_err("repository does not exist");
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(err.message.contains("cannot resolve"), "{}", err.message);
}
