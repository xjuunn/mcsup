//! GitHub 来源定位：仓库 / Release 资产 → 可直接下载的 zip
//!
//! 只做定位，不执行仓库里的代码：
//!
//! - 带引用（`/tree/<ref>`、`--ref`）时，把分支 / tag / commit 解析成不可变的
//!   commit id，拼出 `codeload.github.com` 的归档地址；
//! - 裸链接（`https://github.com/<owner>/<repo>`）表示**最新 Release**：取
//!   `releases/latest` 里的 zip 资产，仓库用 Release 分发成品时用户不用记
//!   版本号与资产名；
//! - 最新 Release 没有可用资产（或接口不可达）时回退到默认分支归档，保持
//!   「把产物提交进仓库」这条路可用。
//!
//! # 资产选择
//!
//! 只看 `.zip` 资产（`.zip.sha256` 不算）。恰好一个就直接用；有多个时优先
//! 名字以 `<repo>` 开头的那一个；仍不唯一就当作没有可用资产而回退——猜错会
//! 装进别的产物，宁可落到仓库归档让作者补一个命名明确的资产。
//!
//! # 为什么不再走 `git`
//!
//! 旧实现要求本机装 git、作者在仓库根维护 `mcsup-plugin.json` 并在 Release
//! 里挂平台产物。现在用户粘贴裸链接即可，作者可以选「把 `manifest.json` 提交
//! 进仓库（或 `dist` 分支）」或「在 Release 里挂 zip」任一种。
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

/// 解析到的定位
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubArchive {
    /// 定位标识：走仓库归档时是 commit id（小写 hex），走 Release 资产时是 tag
    pub resolved: String,
    /// 归档 zip 的下载地址
    pub url: String,
}

/// 最新 Release 里的插件压缩包
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseAsset {
    /// Release 的 tag
    tag: String,
    /// 资产的下载地址
    url: String,
}

/// 定位下载地址
///
/// `rev` 为 `None`（裸链接）时先取最新 Release 的 zip 资产，取不到可用资产
/// 再回退默认分支归档。
///
/// # Errors
///
/// 引用解析不出来（仓库不存在、私有、断网、限流）时返回 `unavailable`。
pub(crate) fn resolve(
    owner: &str,
    repo: &str,
    rev: Option<&str>,
) -> Result<GithubArchive, HostError> {
    if rev.is_none() {
        match latest_release_asset(owner, repo) {
            Ok(Some(asset)) => {
                tracing::info!(
                    owner,
                    repo,
                    tag = %asset.tag,
                    url = %asset.url,
                    "resolved GitHub release asset"
                );
                return Ok(GithubArchive {
                    resolved: asset.tag,
                    url: asset.url,
                });
            }
            Ok(None) => tracing::info!(
                owner,
                repo,
                "latest GitHub release has no plugin archive; using the default branch archive"
            ),
            Err(reason) => tracing::warn!(
                owner,
                repo,
                %reason,
                "cannot use the latest GitHub release; using the default branch archive"
            ),
        }
    }
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
        resolved: commit,
    })
}

/// 取最新 Release 里的插件压缩包
///
/// `Ok(None)` 表示没有可用资产（未发布 Release 也算），由调用方回退默认分支；
/// `Err` 表示查询失败或资产有歧义，同样回退但记一条 warn。
fn latest_release_asset(owner: &str, repo: &str) -> Result<Option<ReleaseAsset>, String> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
    let Some(body) = http_get_optional(&url)? else {
        return Ok(None);
    };
    pick_release_asset(&body, repo)
}

/// 从 `releases/latest` 的 JSON 里挑出插件压缩包
///
/// # Errors
///
/// 响应不是合法 JSON，或存在多个同样合用的 zip 资产时返回原因。
fn pick_release_asset(body: &str, repo: &str) -> Result<Option<ReleaseAsset>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("invalid release payload: {err}"))?;
    let tag = value
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("latest")
        .to_owned();
    let Some(assets) = value.get("assets").and_then(serde_json::Value::as_array) else {
        return Ok(None);
    };
    let candidates = assets
        .iter()
        .filter_map(|asset| {
            let name = asset.get("name")?.as_str()?;
            let url = asset.get("browser_download_url")?.as_str()?;
            is_plugin_archive(name).then(|| (name.to_owned(), url.to_owned()))
        })
        .collect::<Vec<_>>();
    let picked = match candidates.len() {
        0 => return Ok(None),
        1 => candidates.into_iter().next().map(|(_, url)| url),
        // 多个 zip：优先 `<repo>-<version>.zip` 这种以仓库名开头的资产
        _ => {
            let prefix = format!("{repo}-").to_ascii_lowercase();
            let exact = format!("{repo}.zip").to_ascii_lowercase();
            let preferred = candidates
                .iter()
                .filter(|(name, _)| {
                    let lower = name.to_ascii_lowercase();
                    lower.starts_with(&prefix) || lower == exact
                })
                .map(|(_, url)| url.clone())
                .collect::<Vec<_>>();
            if let [url] = preferred.as_slice() {
                Some(url.clone())
            } else {
                let names = candidates
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "release {tag} has multiple zip assets ({names}); \
                     install one of them with --url"
                ));
            }
        }
    };
    let Some(url) = picked else {
        return Ok(None);
    };
    Ok(Some(ReleaseAsset { tag, url }))
}

/// 是否为可安装的压缩包资产：只看 `.zip`，`.zip.sha256` 是校验文件
fn is_plugin_archive(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".zip")
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
    let (status, body) = http_get_raw(url)?;
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    Ok(body)
}

/// 同 [`http_get`]，但把 404 当成「没有」而不是失败
///
/// 仓库没有 Release 时 `releases/latest` 返回 404，这是常态，不该按错误处理。
fn http_get_optional(url: &str) -> Result<Option<String>, String> {
    let (status, body) = http_get_raw(url)?;
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    Ok(Some(body))
}

/// 发 GET，返回状态码与正文
fn http_get_raw(url: &str) -> Result<(reqwest::StatusCode, String), String> {
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
    let body = response.text().map_err(|err| format!("read body: {err}"))?;
    Ok((status, body))
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
        assert_eq!(archive.resolved, SHA);
        assert_eq!(
            archive.url,
            format!("https://codeload.github.com/octocat/Hello-World/zip/{SHA}")
        );
    }

    #[test]
    fn full_commit_is_lowercased() {
        let upper = SHA.to_ascii_uppercase();
        let archive = resolve("o", "r", Some(&upper)).expect("resolve");
        assert_eq!(archive.resolved, SHA);
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

    /// 构造 `releases/latest` 响应：`assets` 是 `(名字, 地址)` 列表
    fn release_json(tag: &str, assets: &[(&str, &str)]) -> String {
        let assets = assets
            .iter()
            .map(|(name, url)| {
                serde_json::json!({
                    "name": name,
                    "browser_download_url": url,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({ "tag_name": tag, "assets": assets }).to_string()
    }

    #[test]
    fn release_asset_is_the_only_zip() {
        let body = release_json(
            "v0.1.0",
            &[
                (
                    "plugin-0.1.0.zip",
                    "https://github.com/o/r/releases/download/v0.1.0/plugin-0.1.0.zip",
                ),
                (
                    "plugin-0.1.0.zip.sha256",
                    "https://github.com/o/r/releases/download/v0.1.0/plugin-0.1.0.zip.sha256",
                ),
            ],
        );
        let asset = pick_release_asset(&body, "plugin")
            .expect("pick")
            .expect("asset");
        assert_eq!(asset.tag, "v0.1.0");
        assert_eq!(
            asset.url,
            "https://github.com/o/r/releases/download/v0.1.0/plugin-0.1.0.zip"
        );
    }

    #[test]
    fn release_without_a_zip_has_no_usable_asset() {
        let body = release_json(
            "v1",
            &[("plugin.tar.gz", "https://example.test/plugin.tar.gz")],
        );
        assert!(
            pick_release_asset(&body, "plugin")
                .expect("parse")
                .is_none()
        );
        // 没有 assets 字段（预发布已删除全部资产）同样视为没有
        assert!(
            pick_release_asset(r#"{"tag_name":"v1"}"#, "plugin")
                .expect("parse")
                .is_none()
        );
    }

    #[test]
    fn multiple_zips_prefer_the_repo_prefixed_one() {
        let body = release_json(
            "v1",
            &[
                ("docs.zip", "https://example.test/docs.zip"),
                ("plugin-1.0.0.zip", "https://example.test/plugin-1.0.0.zip"),
            ],
        );
        let asset = pick_release_asset(&body, "plugin")
            .expect("pick")
            .expect("asset");
        assert_eq!(asset.url, "https://example.test/plugin-1.0.0.zip");
        // 仓库名恰好就是压缩包名也算明确
        let body = release_json(
            "v1",
            &[
                ("plugin.zip", "https://example.test/plugin.zip"),
                ("extra.zip", "https://example.test/extra.zip"),
            ],
        );
        let asset = pick_release_asset(&body, "plugin")
            .expect("pick")
            .expect("asset");
        assert_eq!(asset.url, "https://example.test/plugin.zip");
    }

    #[test]
    fn ambiguous_zips_are_rejected() {
        let body = release_json(
            "v1",
            &[
                ("a.zip", "https://example.test/a.zip"),
                ("b.zip", "https://example.test/b.zip"),
            ],
        );
        let err = pick_release_asset(&body, "plugin").expect_err("ambiguous");
        assert!(err.contains("a.zip"), "{err}");
        assert!(err.contains("--url"), "{err}");
    }

    #[test]
    fn invalid_release_payload_is_rejected() {
        assert!(pick_release_asset("not json", "plugin").is_err());
    }
}
