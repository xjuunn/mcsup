//! GitHub 仓库归档：`<owner>/<repo>[@<ref>]` → commit → 归档 zip
//!
//! 只做两件事，都不执行仓库里的代码：
//!
//! 1. 把分支 / tag / commit 解析成一个不可变的 commit id；
//! 2. 拼出 `codeload.github.com` 的归档地址交给调用方下载、解包与校验。
//!
//! # 为什么不再走 `git`
//!
//! 旧实现要求本机装 git、作者在仓库根维护 `mcsup-plugin.json` 并在 Release
//! 里挂平台产物。三步里任何一步没做好，用户就装不上。现在仓库本身就是产物：
//! 用户粘贴裸链接即可，作者只要把 `manifest.json` 提交进仓库（或提交进一个
//! `dist` 分支），发布就是 `git push`。
//!
//! # 解析 commit 的两条路
//!
//! - 先问 `api.github.com`：一条 JSON，字段稳定。未鉴权限 60 次/小时。
//! - 拿不到再问 `github.com/<owner>/<repo>/commits/<ref>.atom`：Atom feed 的
//!   首个 `<id>` 就是最新提交。同主机、无速率限制，用于 API 不可达或限流。
//!
//! 两条都失败才报 `unavailable`，并在消息里给出两条路的失败原因。参数本身是
//! 完整 40/64 位 commit id 时跳过解析——不再多发一次请求，也让「固定到某个
//! 提交」在离线可缓存的环境里成立。

use port::plugin::HostError;
use std::time::Duration;

/// 元数据请求超时
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// User-Agent：`api.github.com` 会拒绝没有 UA 的请求
const USER_AGENT: &str = concat!("mcsup/", env!("CARGO_PKG_VERSION"));

/// 解析到的归档定位
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubArchive {
    /// 解析到的 commit（小写 hex）
    pub commit: String,
    /// 归档 zip 的下载地址
    pub url: String,
}

/// 解析分支 / tag / commit，返回不可变的归档地址
///
/// # Errors
///
/// 引用解析不出来（仓库不存在、私有、断网、限流）时返回 `unavailable`。
pub(crate) fn resolve(
    owner: &str,
    repo: &str,
    rev: Option<&str>,
) -> Result<GithubArchive, HostError> {
    let rev = rev.unwrap_or("HEAD");
    let commit = if is_full_commit(rev) {
        rev.to_ascii_lowercase()
    } else {
        resolve_commit(owner, repo, rev)?
    };
    tracing::info!(
        owner,
        repo,
        rev,
        commit = %commit,
        "resolved GitHub archive"
    );
    Ok(GithubArchive {
        url: archive_url(owner, repo, &commit),
        commit,
    })
}

fn resolve_commit(owner: &str, repo: &str, rev: &str) -> Result<String, HostError> {
    let mut failures = Vec::new();
    match api_commit(owner, repo, rev) {
        Ok(commit) => return Ok(commit),
        Err(err) => failures.push(format!("api.github.com: {err}")),
    }
    match atom_commit(owner, repo, rev) {
        Ok(commit) => return Ok(commit),
        Err(err) => failures.push(format!("github.com: {err}")),
    }
    Err(HostError::new(
        port::plugin::ErrorCode::Unavailable,
        format!(
            "cannot resolve {owner}/{repo}@{rev}: {}; if the repository is private, \
             build the plugin locally and install it with --from",
            failures.join("; ")
        ),
    ))
}

/// 问 `api.github.com/repos/<owner>/<repo>/commits/<rev>`
fn api_commit(owner: &str, repo: &str, rev: &str) -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/commits/{rev}");
    let body = http_get(&url)?;
    parse_api_sha(&body).ok_or_else(|| "response did not contain a commit id".to_owned())
}

/// 问 `github.com/<owner>/<repo>/commits/<rev>.atom`
fn atom_commit(owner: &str, repo: &str, rev: &str) -> Result<String, String> {
    let url = format!("https://github.com/{owner}/{repo}/commits/{rev}.atom");
    let body = http_get(&url)?;
    parse_atom_sha(&body).ok_or_else(|| "feed did not contain a commit id".to_owned())
}

fn http_get(url: &str) -> Result<String, String> {
    // `rustls-no-provider` 下必须先装 crypto provider，否则 reqwest 内部线程 panic
    crate::http::install_crypto_provider();
    let client = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|err| format!("build http client: {err}"))?;
    let response = client
        .get(url)
        .header("user-agent", USER_AGENT)
        .header("accept", "application/vnd.github+json")
        .send()
        .map_err(|err| format!("{err}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    response.text().map_err(|err| format!("read body: {err}"))
}

/// `codeload` 归档地址
///
/// 顶层目录名是 `<repo>-<commit>/`，因此安装侧必须能容忍产物多包一层目录
/// （`unwrap_single_dir` 已经支持）。
fn archive_url(owner: &str, repo: &str, commit: &str) -> String {
    format!("https://codeload.github.com/{owner}/{repo}/zip/{commit}")
}

/// 从 commits API 的 JSON 里取 `sha`
fn parse_api_sha(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let sha = value.get("sha")?.as_str()?;
    is_full_commit(sha).then(|| sha.to_ascii_lowercase())
}

/// 从 commits Atom feed 里取首个 `<id>` 的 commit
///
/// feed 本身的 `<id>` 是 `tag:github.com,2008:/<owner>/<repo>/commits/<sha>`，
/// 恰好就是「最新提交」，所以只读第一处。
fn parse_atom_sha(body: &str) -> Option<String> {
    const MARKER: &str = "tag:github.com,2008:";
    const COMMITS: &str = "/commits/";
    let after_marker = &body[body.find(MARKER)? + MARKER.len()..];
    let after_commits = &after_marker[after_marker.find(COMMITS)? + COMMITS.len()..];
    let end = after_commits
        .find(|ch: char| !ch.is_ascii_hexdigit())
        .unwrap_or(after_commits.len());
    let sha = &after_commits[..end];
    is_full_commit(sha).then(|| sha.to_ascii_lowercase())
}

/// 是否为完整 commit id（40 或 64 位 hex）
fn is_full_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "测试夹具，失败直接 panic 才有用")]

    use super::*;

    const SHA: &str = "7fd1a60b01f91b314f59955a4e4d4e80d8edf11d";

    #[test]
    fn full_commit_skips_resolution() {
        let archive = resolve("octocat", "Hello-World", Some(SHA)).expect("resolve");
        assert_eq!(archive.commit, SHA);
        assert_eq!(
            archive.url,
            format!("https://codeload.github.com/octocat/Hello-World/zip/{SHA}")
        );
    }

    #[test]
    fn full_commit_is_lowercased() {
        let upper = SHA.to_ascii_uppercase();
        let archive = resolve("o", "r", Some(&upper)).expect("resolve");
        assert_eq!(archive.commit, SHA);
    }

    #[test]
    fn api_payload_yields_the_commit() {
        let body = format!(r#"{{"sha":"{SHA}","commit":{{"message":"init"}}}}"#);
        assert_eq!(parse_api_sha(&body).as_deref(), Some(SHA));
    }

    #[test]
    fn api_payload_without_a_commit_is_rejected() {
        assert!(parse_api_sha(r#"{"message":"Not Found"}"#).is_none());
        assert!(parse_api_sha("not json").is_none());
        // 截断的 sha 不能当成 commit
        assert!(parse_api_sha(r#"{"sha":"7fd1a60"}"#).is_none());
    }

    #[test]
    fn atom_feed_yields_the_first_commit() {
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <id>tag:github.com,2008:/octocat/Hello-World/commits/{SHA}</id>
  <entry><id>tag:github.com,2008:Repository/123/{SHA}</id></entry>
</feed>"#
        );
        assert_eq!(parse_atom_sha(&body).as_deref(), Some(SHA));
    }

    #[test]
    fn atom_feed_without_a_commit_is_rejected() {
        assert!(parse_atom_sha("<feed><id>tag:github.com,2008:</id></feed>").is_none());
        assert!(parse_atom_sha("not xml").is_none());
    }

    #[test]
    fn commit_ids_are_validated_by_length() {
        assert!(is_full_commit(SHA));
        assert!(is_full_commit(&"a".repeat(64)));
        assert!(!is_full_commit("7fd1a60"));
        assert!(!is_full_commit(&"z".repeat(40)));
    }
}
