//! Git 来源解析：`--git <url> --ref <tag|sha>` → 索引 + 平台产物
//!
//! 只做三件事，任何一步都不执行仓库里的代码：
//!
//! 1. 校验 ref 是 tag 或完整 commit sha（拒绝默认跟随分支头）；
//! 2. 浅取该 ref 对应的提交，读出仓库根目录的 `mcsup-plugin.json`；
//! 3. 按当前平台选出一条产物，交给调用方下载与校验。
//!
//! 走 `git` 可执行文件而不是嵌入 libgit2：宿主只在安装时需要它，装不上时
//! 给出明确的 `unsupported`，不必为一个可选路径引入整套 C 依赖。
//!
//! 索引本身随 commit 固定：先取到 commit，再从该 commit 的工作树读索引，
//! 不允许读分支头的可变索引（`docs/plugins.md` 附录 E）。

use domain::plugin::platform_key;
use port::plugin::HostError;
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

/// Git 来源索引文件名（`docs/plugins.md` 附录 E）
pub(crate) const INDEX_FILE: &str = "mcsup-plugin.json";

/// 索引 schema 版本
///
/// 与 manifest / descriptor / 协议版本互不推导：索引只描述「哪个平台取
/// 哪个文件」，与包内元数据的演进节奏不同。
const INDEX_SCHEMA_VERSION: u32 = 1;

/// 浅取目录名（临时目录内的子目录）
const CHECKOUT_DIR: &str = "checkout";

/// Git 来源解析结果
#[derive(Debug, Clone)]
pub(crate) struct GitSource {
    /// 解析到的 commit（40 或 64 位 hex）
    pub commit: String,
    /// 索引声明的插件 id
    pub id: String,
    /// 索引声明的版本
    pub version: String,
    /// 索引声明的 API 主版本
    pub api: u32,
    /// 当前平台产物的下载地址
    pub artifact_url: String,
    /// 当前平台产物的 sha256（小写 hex）
    pub sha256: String,
    /// 当前平台产物的字节数；索引未声明时为 `None`
    pub size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Index {
    schema: u32,
    id: String,
    version: String,
    api: u32,
    artifacts: Vec<IndexArtifact>,
}

#[derive(Debug, Clone, Deserialize)]
struct IndexArtifact {
    platform: String,
    url: String,
    sha256: String,
    #[serde(default)]
    size: Option<u64>,
}

impl Index {
    fn parse(bytes: &[u8]) -> Result<Self, HostError> {
        let index: Self = serde_json::from_slice(bytes)
            .map_err(|err| HostError::invalid_input(format!("invalid {INDEX_FILE}: {err}")))?;
        if index.schema != INDEX_SCHEMA_VERSION {
            return Err(HostError::invalid_input(format!(
                "{INDEX_FILE} schema {} is not supported; expected {INDEX_SCHEMA_VERSION}",
                index.schema
            )));
        }
        if index.id.is_empty() || index.version.is_empty() {
            return Err(HostError::invalid_input(format!(
                "{INDEX_FILE} must declare id and version"
            )));
        }
        if index.artifacts.is_empty() {
            return Err(HostError::invalid_input(format!(
                "{INDEX_FILE} declares no artifacts"
            )));
        }
        for artifact in &index.artifacts {
            validate_artifact_url(&artifact.url)?;
            if !is_full_sha(&artifact.sha256) {
                return Err(HostError::invalid_input(format!(
                    "{INDEX_FILE} artifact {} has an invalid sha256",
                    artifact.platform
                )));
            }
        }
        Ok(index)
    }

    fn select(&self, platform: &str) -> Result<&IndexArtifact, HostError> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.platform == platform)
            .ok_or_else(|| {
                HostError::unsupported(format!(
                    "plugin {} {} has no artifact for platform {platform}",
                    self.id, self.version
                ))
            })
    }
}

/// 解析 git 来源，返回 commit 与当前平台产物
///
/// `work_dir` 由调用方提供，通常是安装临时目录；浅取的工作树在其
/// [`CHECKOUT_DIR`] 子目录内，读完索引即删除。
pub(crate) fn resolve(
    url: &str,
    rev: Option<&str>,
    work_dir: &Path,
) -> Result<GitSource, HostError> {
    let rev = rev.ok_or_else(|| {
        HostError::invalid_input(
            "git sources require --ref (a tag or a full commit sha); \
             a branch head is not allowed because it is mutable",
        )
    })?;
    validate_url(url)?;
    validate_rev(rev)?;

    let checkout = work_dir.join(CHECKOUT_DIR);
    if checkout.exists() {
        std::fs::remove_dir_all(&checkout)
            .map_err(|err| HostError::internal(format!("clear git checkout: {err}")))?;
    }
    if is_full_sha(rev) {
        fetch_commit(url, rev, &checkout)?;
    } else {
        clone_tag(url, rev, &checkout)?;
    }
    let commit = run(
        Command::new("git")
            .current_dir(&checkout)
            .args(["rev-parse", "HEAD"]),
        "rev-parse",
    )?
    .trim()
    .to_owned();
    if !is_full_sha(&commit) {
        return Err(HostError::internal(format!(
            "git rev-parse returned an unexpected commit id: {commit}"
        )));
    }
    let index_path = checkout.join(INDEX_FILE);
    let bytes = std::fs::read(&index_path).map_err(|err| {
        HostError::not_found(format!(
            "{} is not in commit {commit}: {err}",
            index_path.display()
        ))
    })?;
    let index = Index::parse(&bytes)?;
    let platform = platform_key();
    let artifact = index.select(&platform)?.clone();
    let source = GitSource {
        commit,
        id: index.id,
        version: index.version,
        api: index.api,
        artifact_url: artifact.url.clone(),
        sha256: artifact.sha256.to_ascii_lowercase(),
        size: artifact.size,
    };
    // 工作树只用于读索引，产物是另外下载的压缩包。
    let _ = std::fs::remove_dir_all(&checkout);
    Ok(source)
}

/// 只接受 tag：`--branch` 也会接受分支名，先确认 ref 出现在 tag 列表里
fn clone_tag(url: &str, tag: &str, dest: &Path) -> Result<(), HostError> {
    let reference = format!("refs/tags/{tag}");
    let listing = run(
        Command::new("git").args(["ls-remote", "--tags", "--refs", url, &reference]),
        "ls-remote",
    )?;
    if listing.trim().is_empty() {
        return Err(HostError::invalid_input(format!(
            "{tag} is not a tag of {url}; --ref must be a tag or a full commit sha"
        )));
    }
    let dest = path_arg(dest)?;
    run(
        Command::new("git").args([
            "clone",
            "--quiet",
            "--depth",
            "1",
            "--branch",
            tag,
            "--single-branch",
            "--",
            url,
            dest,
        ]),
        "clone",
    )?;
    Ok(())
}

/// 按完整 commit sha 浅取
///
/// GitHub / GitLab 允许取可达提交；不可达时 git 自己会拒绝，错误原文保留在
/// `unavailable` 的 message 里。
fn fetch_commit(url: &str, sha: &str, dest: &Path) -> Result<(), HostError> {
    std::fs::create_dir_all(dest)
        .map_err(|err| HostError::internal(format!("create git checkout: {err}")))?;
    // `git init` 在 `current_dir` 内初始化；工作树位置已由目录本身确定，
    // 不需要再把路径当参数传一次。
    run(
        Command::new("git")
            .current_dir(dest)
            .args(["init", "--quiet"]),
        "init",
    )?;
    run(
        Command::new("git")
            .current_dir(dest)
            .args(["remote", "add", "origin", url]),
        "remote add",
    )?;
    run(
        Command::new("git")
            .current_dir(dest)
            .args(["fetch", "--quiet", "--depth", "1", "origin", sha]),
        "fetch",
    )?;
    run(
        Command::new("git").current_dir(dest).args([
            "checkout",
            "--quiet",
            "--detach",
            "FETCH_HEAD",
        ]),
        "checkout",
    )?;
    Ok(())
}

fn run(command: &mut Command, what: &str) -> Result<String, HostError> {
    // git 隔离：这些变量从父进程继承时会越过 `current_dir` 作用到别的仓库——
    // pre-commit 钩子运行期间 `git commit` 会 export `GIT_DIR` / `GIT_INDEX_FILE`，
    // 不清除的话测试里的 git 操作会作用到主仓库（曾把整个暂存区以测试信息
    // 提交进仓库）。
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(var);
    }
    // git 会读用户与系统配置，这里只覆盖会阻塞的两项：需要凭据时直接失败，
    // 不要在没有终端的宿主进程里等待输入。
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "echo")
        .env("GCM_INTERACTIVE", "never");
    let output = command.output().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            HostError::unsupported("git is not installed; install git or use --from / --url")
        } else {
            HostError::new(
                port::plugin::ErrorCode::Unavailable,
                format!("run git {what}: {err}"),
            )
        }
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("no stderr");
        return Err(HostError::new(
            port::plugin::ErrorCode::Unavailable,
            format!("git {what} failed: {message}"),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 临时目录路径转命令行参数
fn path_arg(path: &Path) -> Result<&str, HostError> {
    path.to_str().ok_or_else(|| {
        HostError::invalid_input(format!(
            "temporary path is not valid UTF-8: {}",
            path.display()
        ))
    })
}

/// 拒绝会执行本地命令的传输协议与形似选项的地址
fn validate_url(url: &str) -> Result<(), HostError> {
    if url.is_empty() || url.starts_with('-') {
        return Err(HostError::invalid_input(
            "git url must not be empty or start with '-'",
        ));
    }
    // `ext::` / `fd::` 等远程助手协议会执行本地命令，克隆来源不得触发它们。
    if url.contains("::") {
        return Err(HostError::invalid_input(
            "git url uses a transport helper, which is not allowed",
        ));
    }
    Ok(())
}

fn validate_artifact_url(url: &str) -> Result<(), HostError> {
    let parsed = url::Url::parse(url)
        .map_err(|err| HostError::invalid_input(format!("invalid artifact url: {err}")))?;
    match parsed.scheme() {
        "http" | "https" => Ok(()),
        other => Err(HostError::invalid_input(format!(
            "artifact url scheme {other} is not supported; only http/https"
        ))),
    }
}

/// ref 只允许 tag 字符集：拒绝选项注入与路径逃逸
fn validate_rev(rev: &str) -> Result<(), HostError> {
    let valid = !rev.is_empty()
        && !rev.starts_with('-')
        && !rev.contains("..")
        && !rev.contains("::")
        && rev.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/' | b'+')
        });
    if !valid {
        return Err(HostError::invalid_input(format!(
            "invalid git ref `{rev}`; use a tag or a full commit sha"
        )));
    }
    Ok(())
}

fn is_full_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "测试夹具，失败直接 panic 才有用"
    )]

    use super::*;

    fn index_json(platform: &str) -> String {
        format!(
            r#"{{"schema":1,"id":"chat-bridge","version":"0.3.1","api":1,
               "artifacts":[{{"platform":"{platform}","url":"https://example.com/a.zip",
               "sha256":"{}","size":10}}]}}"#,
            "a".repeat(64)
        )
    }

    #[test]
    fn index_selects_the_current_platform_only() {
        let index = Index::parse(index_json("linux-x64").as_bytes()).expect("parse");
        assert!(index.select("linux-x64").is_ok());
        let err = index.select("win32-x64").expect_err("missing platform");
        assert_eq!(err.code, port::plugin::ErrorCode::Unsupported);
    }

    #[test]
    fn index_rejects_unknown_schema_and_bad_digest() {
        let unknown = index_json("linux-x64").replace("\"schema\":1", "\"schema\":2");
        let err = Index::parse(unknown.as_bytes()).expect_err("schema");
        assert_eq!(err.code, port::plugin::ErrorCode::InvalidInput);

        let bad_digest = index_json("linux-x64").replace(&"a".repeat(64), "not-a-digest");
        let err = Index::parse(bad_digest.as_bytes()).expect_err("digest");
        assert_eq!(err.code, port::plugin::ErrorCode::InvalidInput);
    }

    #[test]
    fn index_rejects_non_http_artifact_urls() {
        let local =
            index_json("linux-x64").replace("https://example.com/a.zip", "file:///etc/x.zip");
        let err = Index::parse(local.as_bytes()).expect_err("scheme");
        assert_eq!(err.code, port::plugin::ErrorCode::InvalidInput);
    }

    #[test]
    fn rev_rejects_option_injection_and_branch_like_paths() {
        for bad in ["--upload-pack=sh", "a..b", "-x", "refs/heads/main;rm", ""] {
            assert!(validate_rev(bad).is_err(), "{bad}");
        }
        assert!(validate_rev("v0.3.1").is_ok());
        assert!(validate_rev(&"a".repeat(40)).is_ok());
    }

    #[test]
    fn transport_helpers_are_rejected() {
        assert!(validate_url("ext::sh -c touch /tmp/x").is_err());
        assert!(validate_url("--upload-pack=sh").is_err());
        assert!(validate_url("https://github.com/example/chat-bridge").is_ok());
    }

    /// 没有 git 的环境（精简容器）跳过需要真实 git 的用例
    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn git(dir: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        command.current_dir(dir).args(args);
        // 与生产 `run` 同一套隔离：钩子环境里的 GIT_* 会把操作引到主仓库
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
        ] {
            command.env_remove(var);
        }
        let output = command.output().expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn resolve_reads_the_index_at_a_tag() {
        if !git_available() {
            return;
        }
        let repo = tempfile::tempdir().expect("repo");
        let work = tempfile::tempdir().expect("work");
        git(repo.path(), &["init", "--quiet"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "test"]);
        std::fs::write(repo.path().join(INDEX_FILE), index_json(&platform_key())).expect("index");
        git(repo.path(), &["add", INDEX_FILE]);
        git(repo.path(), &["commit", "--quiet", "-m", "index"]);
        git(repo.path(), &["tag", "v0.3.1"]);

        let url = repo.path().to_str().expect("utf-8 path");
        let source = resolve(url, Some("v0.3.1"), work.path()).expect("resolve");
        assert_eq!(source.id, "chat-bridge");
        assert_eq!(source.commit.len(), 40);
        assert!(source.artifact_url.ends_with("a.zip"));
        assert!(
            !work.path().join(CHECKOUT_DIR).exists(),
            "checkout is removed"
        );
    }

    #[test]
    fn resolve_rejects_a_branch_name() {
        if !git_available() {
            return;
        }
        let repo = tempfile::tempdir().expect("repo");
        let work = tempfile::tempdir().expect("work");
        git(repo.path(), &["init", "--quiet"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "test"]);
        std::fs::write(repo.path().join("README.md"), "x").expect("readme");
        git(repo.path(), &["add", "README.md"]);
        git(repo.path(), &["commit", "--quiet", "-m", "init"]);

        let url = repo.path().to_str().expect("utf-8 path");
        let err = resolve(url, Some("master"), work.path()).expect_err("branch");
        assert_eq!(err.code, port::plugin::ErrorCode::InvalidInput);
        assert!(err.message.contains("not a tag"), "{}", err.message);
    }

    #[test]
    fn resolve_requires_a_ref() {
        let work = tempfile::tempdir().expect("work");
        let err = resolve("https://example.com/repo", None, work.path()).expect_err("ref");
        assert_eq!(err.code, port::plugin::ErrorCode::InvalidInput);
    }
}
