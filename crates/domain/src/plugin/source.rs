//! 插件安装来源：输入解析、安装记录文本与 GitHub 仓库归档
//!
//! 安装来源在前端有两种文本形态，本模块是它们唯一的定义处：
//!
//! - **用户输入**（`mcsup plugin install <source>`）：本地路径、远程压缩包直链，
//!   或一个裸的 GitHub 仓库链接（`https://github.com/<owner>/<repo>`，可带
//!   `/tree/<ref>` 后缀）。裸链接表示**最新 Release**：安装器优先取该仓库
//!   `releases/latest` 里的 zip 资产，没有可用资产时回退默认分支归档；
//! - **安装记录**（`PluginRecord::source`）：`local:<path>` / `url:<url>` /
//!   `github:<owner>/<repo>[@<ref>]`。`plugin update` 直接把它喂回本模块，
//!   所以写入（[`PluginSource`] 的 `Display`）与读取（[`PluginSource::parse_record`]）
//!   必须由同一份代码定义——历史上两处各写一份，写 `@` 读 `|`，导致 git 来源
//!   的 `update` 永远失败。
//!
//! # 记录里存「引用」而不是「提交」
//!
//! GitHub 来源记录的是用户请求的引用（分支 / tag，缺省即「最新 Release」），
//! 不是解析出来的 commit。写成 commit 会丢掉分支名：作者把产物放在 `dist`
//! 分支、用户装 `.../tree/dist` 时，记录只留下一个 sha，`update` 就再也跟不动
//! `dist` 了。想钉死版本的用户可以显式 `--ref <tag>` 或 `--ref <sha>`，那时
//! 记录里本来就是不可移动的引用。
//!
//! # 为什么不接受任意 git 地址
//!
//! 旧版走「本机 `git` 浅取仓库 + 读仓库根的 `mcsup-plugin.json` 索引」，要求
//! 用户装 git、作者维护索引与 Release 资产。现在只认 GitHub 仓库归档：安装器
//! 用 `codeload` 直接下 zip，不执行仓库里的任何代码，也不需要本机有任何 git。

use std::fmt;

/// 记录文本前缀：本地目录 / 压缩包
const PREFIX_LOCAL: &str = "local:";
/// 记录文本前缀：远程压缩包直链
const PREFIX_URL: &str = "url:";
/// 记录文本前缀：GitHub 仓库归档
const PREFIX_GITHUB: &str = "github:";
/// 记录文本前缀：旧版 git 来源（仅用于识别并给出迁移提示）
const PREFIX_LEGACY_GIT: &str = "git:";

/// GitHub 主机名
const HOST_GITHUB: &str = "github.com";
/// GitHub 主机名（带 `www.` 的等价写法）
const HOST_GITHUB_WWW: &str = "www.github.com";

/// 插件安装来源
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSource {
    /// 本地目录或压缩包
    ///
    /// 存文本而不是 [`std::path::PathBuf`]：记录里必须与平台无关地
    /// 可往返，路径规范成绝对路径由入口负责（它才知道当前工作目录）。
    Local(String),
    /// 远程压缩包直链
    Url(String),
    /// GitHub 仓库归档
    ///
    /// `rev` 是分支 / tag / commit；`None` 表示**最新 Release**（仓库未发布
    /// 可用资产时回退默认分支归档）。
    Github {
        /// 仓库所有者
        owner: String,
        /// 仓库名（不含 `.git`）
        repo: String,
        /// 分支 / tag / commit
        rev: Option<String>,
    },
}

impl PluginSource {
    /// 解析用户输入
    ///
    /// 判定顺序：GitHub 仓库链接 → `http(s)://` 直链 → 其余一律当本地路径。
    /// 本地路径原样返回，绝对化由调用方完成。
    ///
    /// # Errors
    ///
    /// 输入为空、或形如 GitHub 链接但路径 / 所有者 / 引用不合法时返回
    /// [`InvalidSource`]。
    pub fn parse_input(raw: &str) -> Result<Self, InvalidSource> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(InvalidSource::new(raw, SourceReject::Empty));
        }
        if let Some((host, _)) = split_host_path(trimmed)
            && is_github_host(host)
        {
            return parse_github_link(trimmed);
        }
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return Ok(Self::Url(trimmed.to_owned()));
        }
        Ok(Self::Local(trimmed.to_owned()))
    }

    /// 解析安装记录里的来源文本
    ///
    /// 兼容旧版 `git:<url>|<rev>` 与 `git:<url>@<commit>`：URL 是 GitHub 裸仓库
    /// 时映射成 GitHub 来源，否则报「旧 git 来源不再支持」。旧记录只有这一条
    /// 路可走——`mcsup-plugin.json` 索引已经不再被读取，原样重装只会得到
    /// `not_found`，不如直接告诉用户怎么重来。
    ///
    /// # Errors
    ///
    /// 前缀未知、字段为空或取值不合法时返回 [`InvalidSource`]。
    pub fn parse_record(raw: &str) -> Result<Self, InvalidSource> {
        if let Some(path) = raw.strip_prefix(PREFIX_LOCAL) {
            if path.is_empty() {
                return Err(InvalidSource::new(raw, SourceReject::Empty));
            }
            return Ok(Self::Local(path.to_owned()));
        }
        if let Some(url) = raw.strip_prefix(PREFIX_URL) {
            if url.is_empty() {
                return Err(InvalidSource::new(raw, SourceReject::Empty));
            }
            return Ok(Self::Url(url.to_owned()));
        }
        if let Some(rest) = raw.strip_prefix(PREFIX_GITHUB) {
            return parse_github_record(raw, rest);
        }
        if let Some(rest) = raw.strip_prefix(PREFIX_LEGACY_GIT) {
            return parse_legacy_git_record(raw, rest);
        }
        Err(InvalidSource::new(raw, SourceReject::UnknownPrefix))
    }

    /// 请求的引用（分支 / tag / commit）；`None` 表示最新 Release
    #[must_use]
    pub fn rev(&self) -> Option<&str> {
        match self {
            Self::Github { rev, .. } => rev.as_deref(),
            Self::Local(_) | Self::Url(_) => None,
        }
    }

    /// GitHub 仓库的网页地址；非 GitHub 来源返回 `None`
    #[must_use]
    pub fn github_url(&self) -> Option<String> {
        match self {
            Self::Github { owner, repo, .. } => {
                Some(format!("https://{HOST_GITHUB}/{owner}/{repo}"))
            }
            Self::Local(_) | Self::Url(_) => None,
        }
    }
}

impl fmt::Display for PluginSource {
    /// 写入安装记录的文本形态
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(path) => write!(f, "{PREFIX_LOCAL}{path}"),
            Self::Url(url) => write!(f, "{PREFIX_URL}{url}"),
            Self::Github { owner, repo, rev } => match rev {
                Some(rev) => write!(f, "{PREFIX_GITHUB}{owner}/{repo}@{rev}"),
                None => write!(f, "{PREFIX_GITHUB}{owner}/{repo}"),
            },
        }
    }
}

/// 无法识别的安装来源
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid plugin source {raw:?}: {reason}")]
pub struct InvalidSource {
    /// 原始输入
    pub raw: String,
    /// 拒绝原因
    pub reason: SourceReject,
}

impl InvalidSource {
    fn new(raw: &str, reason: SourceReject) -> Self {
        Self {
            raw: raw.to_owned(),
            reason,
        }
    }
}

/// 来源被拒的具体原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceReject {
    /// 输入为空
    Empty,
    /// 记录前缀未知
    UnknownPrefix,
    /// GitHub 链接缺少所有者或仓库名
    Github,
    /// GitHub 仓库名后面跟了 `/tree/<ref>` 之外的路径
    GithubPath,
    /// 仓库所有者不合法
    Owner,
    /// 仓库名不合法
    Repo,
    /// 引用不合法
    Rev,
    /// 旧版 git 来源，且不是 GitHub 仓库
    LegacyGit,
}

impl fmt::Display for SourceReject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "must not be empty",
            Self::UnknownPrefix => "not a recorded source (expected `local:`, `url:` or `github:`)",
            Self::Github => {
                "expected a GitHub repository link such as https://github.com/<owner>/<repo>"
            }
            Self::GithubPath => "only /tree/<ref> may follow the repository name",
            Self::Owner => "invalid GitHub owner",
            Self::Repo => "invalid GitHub repository name",
            Self::Rev => "invalid revision; use a branch, tag or full commit id",
            Self::LegacyGit => {
                "installed from a git source, which is no longer supported; \
                 reinstall with `mcsup plugin install https://github.com/<owner>/<repo>`"
            }
        })
    }
}

/// 解析 GitHub 仓库链接
///
/// 接受 `https://github.com/o/r`、`github.com/o/r`、`git@github.com:o/r`
/// 以及带 `/tree/<ref>` 的产物分支写法。
fn parse_github_link(raw: &str) -> Result<PluginSource, InvalidSource> {
    let (_, path) =
        split_host_path(raw).ok_or_else(|| InvalidSource::new(raw, SourceReject::Github))?;
    let path = strip_query_fragment(path);
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    let owner = segments
        .next()
        .ok_or_else(|| InvalidSource::new(raw, SourceReject::Github))?;
    let repo = segments
        .next()
        .ok_or_else(|| InvalidSource::new(raw, SourceReject::Github))?;
    let repo = strip_git_suffix(repo);
    let mut rev = None;
    let rest = segments.collect::<Vec<_>>();
    if let Some((verb, tail)) = rest.split_first() {
        if *verb != "tree" {
            return Err(InvalidSource::new(raw, SourceReject::GithubPath));
        }
        let ref_path = tail.join("/");
        if ref_path.is_empty() {
            return Err(InvalidSource::new(raw, SourceReject::Rev));
        }
        validate_rev(raw, &ref_path)?;
        rev = Some(ref_path);
    }
    validate_owner(raw, owner)?;
    validate_repo(raw, repo)?;
    Ok(PluginSource::Github {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        rev,
    })
}

/// 解析 `github:<owner>/<repo>[@<ref>]`
fn parse_github_record(raw: &str, rest: &str) -> Result<PluginSource, InvalidSource> {
    // 所有者与仓库名的字符集不含 `@`，从右切分是安全的。
    let (slug, rev) = match rest.rsplit_once('@') {
        Some((slug, rev)) => (slug, Some(rev)),
        None => (rest, None),
    };
    let (owner, repo) = slug
        .split_once('/')
        .ok_or_else(|| InvalidSource::new(raw, SourceReject::Github))?;
    validate_owner(raw, owner)?;
    validate_repo(raw, repo)?;
    if let Some(rev) = rev {
        validate_rev(raw, rev)?;
    }
    Ok(PluginSource::Github {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        rev: rev.map(str::to_owned),
    })
}

/// 解析旧版 `git:<url>|<rev>` / `git:<url>@<commit>` / `git:<url>`
fn parse_legacy_git_record(raw: &str, rest: &str) -> Result<PluginSource, InvalidSource> {
    let (url, rev) = match rest.rsplit_once('|') {
        Some((url, rev)) => (url, Some(rev.to_owned())),
        // provider 曾经写的是 `git:<url>@<commit>`；只有「后缀是完整 sha」才敢
        // 当成 commit 拆，否则 `https://user@host/...` 里的 `@` 会被误切。
        None => match rest.rsplit_once('@') {
            Some((url, rev)) if is_full_commit(rev) => (url, Some(rev.to_owned())),
            _ => (rest, None),
        },
    };
    let (host, path) =
        split_host_path(url).ok_or_else(|| InvalidSource::new(raw, SourceReject::LegacyGit))?;
    if !is_github_host(host) {
        return Err(InvalidSource::new(raw, SourceReject::LegacyGit));
    }
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    let owner = segments
        .next()
        .ok_or_else(|| InvalidSource::new(raw, SourceReject::Github))?;
    let repo = segments
        .next()
        .ok_or_else(|| InvalidSource::new(raw, SourceReject::Github))?;
    let repo = strip_git_suffix(repo);
    validate_owner(raw, owner)?;
    validate_repo(raw, repo)?;
    if let Some(rev) = &rev {
        validate_rev(raw, rev)?;
    }
    Ok(PluginSource::Github {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        rev,
    })
}

/// 把输入拆成「主机 / 路径」两段
///
/// 认三种写法：`scheme://[user@]host[:port]/path`、scp 风格
/// `git@host:path`、以及裸的 `host/path`。其它带 `://` 的协议返回 `None`，
/// 交给调用方的通用分支处理。
fn split_host_path(raw: &str) -> Option<(&str, &str)> {
    if let Some(after) = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
    {
        let (host, path) = after.split_once('/')?;
        return Some((host, path));
    }
    if raw.contains("://") {
        return None;
    }
    // scp 风格：`user@host:owner/repo`。冒号必须在第一个斜杠之前。
    if let Some((user_host, path)) = raw.split_once(':')
        && !user_host.contains('/')
        && !user_host.is_empty()
    {
        let host = user_host.rsplit('@').next().unwrap_or(user_host);
        return Some((host, path.trim_start_matches('/')));
    }
    let (host, path) = raw.split_once('/')?;
    Some((host, path))
}

/// 主机名去掉 userinfo 与端口后是否等于 GitHub
fn is_github_host(host: &str) -> bool {
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    host.eq_ignore_ascii_case(HOST_GITHUB) || host.eq_ignore_ascii_case(HOST_GITHUB_WWW)
}

/// 去掉 `?query` 与 `#fragment`
fn strip_query_fragment(path: &str) -> &str {
    let end = path.find(['?', '#']).unwrap_or(path.len());
    &path[..end]
}

/// 去掉仓库名的 `.git` 后缀
fn strip_git_suffix(repo: &str) -> &str {
    repo.strip_suffix(".git").unwrap_or(repo)
}

fn validate_owner(raw: &str, owner: &str) -> Result<(), InvalidSource> {
    if owner.is_empty() || !owner.bytes().all(is_slug_byte) {
        return Err(InvalidSource::new(raw, SourceReject::Owner));
    }
    Ok(())
}

fn validate_repo(raw: &str, repo: &str) -> Result<(), InvalidSource> {
    if repo.is_empty() || !repo.bytes().all(is_slug_byte) {
        return Err(InvalidSource::new(raw, SourceReject::Repo));
    }
    Ok(())
}

/// 所有者 / 仓库名允许的字符：拒绝 `/`、`@`、`:` 等会改变 URL 结构的字符
fn is_slug_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

/// 引用只允许分支 / tag / commit 的字符集：拒绝选项注入与 `..` 路径逃逸
fn validate_rev(raw: &str, rev: &str) -> Result<(), InvalidSource> {
    let valid = !rev.is_empty()
        && rev != "."
        && !rev.starts_with('-')
        && !rev.contains("..")
        && !rev.contains("::")
        && rev.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/' | b'+')
        });
    if !valid {
        return Err(InvalidSource::new(raw, SourceReject::Rev));
    }
    Ok(())
}

/// 是否为完整 commit id（40 或 64 位 hex）
fn is_full_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github(owner: &str, repo: &str, rev: Option<&str>) -> PluginSource {
        PluginSource::Github {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            rev: rev.map(str::to_owned),
        }
    }

    #[test]
    fn bare_github_links_become_github_sources() {
        for raw in [
            "https://github.com/octocat/Hello-World",
            "https://github.com/octocat/Hello-World/",
            "http://github.com/octocat/Hello-World",
            "github.com/octocat/Hello-World",
            "www.github.com/octocat/Hello-World",
            "https://www.github.com/octocat/Hello-World",
            "https://github.com/octocat/Hello-World.git",
            "git@github.com:octocat/Hello-World.git",
            "  https://github.com/octocat/Hello-World  ",
            "https://github.com/octocat/Hello-World?tab=readme-ov-file#install",
        ] {
            assert_eq!(
                PluginSource::parse_input(raw).expect(raw),
                github("octocat", "Hello-World", None),
                "{raw}"
            );
        }
    }

    #[test]
    fn tree_links_carry_the_requested_ref() {
        assert_eq!(
            PluginSource::parse_input("https://github.com/o/r/tree/dist").expect("dist"),
            github("o", "r", Some("dist"))
        );
        assert_eq!(
            PluginSource::parse_input("https://github.com/o/r/tree/feature/nested")
                .expect("nested"),
            github("o", "r", Some("feature/nested"))
        );
        assert_eq!(
            PluginSource::parse_input("https://github.com/o/r/tree/v1.0.0").expect("tag"),
            github("o", "r", Some("v1.0.0"))
        );
    }

    #[test]
    fn non_github_links_stay_generic_urls() {
        for raw in [
            "https://example.com/plugin.zip",
            "https://github.com.evil.test/o/r",
            "https://notgithub.com/o/r",
        ] {
            assert_eq!(
                PluginSource::parse_input(raw).expect(raw),
                PluginSource::Url(raw.to_owned()),
                "{raw}"
            );
        }
    }

    #[test]
    fn plain_paths_become_local_sources() {
        for raw in ["./plugins/demo", "plugins/demo", "/opt/demo.zip"] {
            assert_eq!(
                PluginSource::parse_input(raw).expect(raw),
                PluginSource::Local(raw.to_owned()),
                "{raw}"
            );
        }
        assert!(matches!(
            PluginSource::parse_input(r"C:\Users\x\plugins\demo").expect("windows path"),
            PluginSource::Local(_)
        ));
    }

    #[test]
    fn rejects_github_paths_other_than_tree() {
        for raw in [
            "https://github.com/o/r/blob/main/manifest.json",
            "https://github.com/o/r/tree",
            "https://github.com/o",
            "https://github.com/",
        ] {
            let err = PluginSource::parse_input(raw).expect_err(raw);
            assert!(
                matches!(
                    err.reason,
                    SourceReject::Github | SourceReject::GithubPath | SourceReject::Rev
                ),
                "{raw}: {:?}",
                err.reason
            );
        }
    }

    #[test]
    fn rejects_url_injection_through_owner_and_ref() {
        let err =
            PluginSource::parse_input("https://github.com/o/r/tree/../etc").expect_err("dots");
        assert_eq!(err.reason, SourceReject::Rev);
        let err = PluginSource::parse_input("https://github.com/o/r/tree/-x").expect_err("leading");
        assert_eq!(err.reason, SourceReject::Rev);
        let err =
            PluginSource::parse_input("https://github.com/o/r/tree/a::b").expect_err("helper");
        assert_eq!(err.reason, SourceReject::Rev);
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(
            PluginSource::parse_input("   ").expect_err("empty").reason,
            SourceReject::Empty
        );
    }

    #[test]
    fn records_round_trip_through_text() {
        for source in [
            PluginSource::Local("/opt/plugins/demo".to_owned()),
            PluginSource::Url("https://example.com/plugin.zip".to_owned()),
            github("o", "r", None),
            github("o", "r", Some("dist")),
            github("o", "r", Some("7fd1a60b01f91b314f59955a4e4d4e80d8edf11d")),
        ] {
            let text = source.to_string();
            assert_eq!(
                PluginSource::parse_record(&text).expect(&text),
                source,
                "{text}"
            );
        }
    }

    #[test]
    fn record_writes_github_text() {
        assert_eq!(github("o", "r", None).to_string(), "github:o/r");
        assert_eq!(
            github("o", "r", Some("dist")).to_string(),
            "github:o/r@dist"
        );
    }

    #[test]
    fn rejects_unknown_record_prefix() {
        assert_eq!(
            PluginSource::parse_record("ftp:o/r")
                .expect_err("prefix")
                .reason,
            SourceReject::UnknownPrefix
        );
        assert_eq!(
            PluginSource::parse_record("github:o")
                .expect_err("slug")
                .reason,
            SourceReject::Github
        );
    }

    #[test]
    fn legacy_git_records_map_to_github() {
        assert_eq!(
            PluginSource::parse_record("git:https://github.com/o/r|v1.0.0").expect("pipe"),
            github("o", "r", Some("v1.0.0"))
        );
        assert_eq!(
            PluginSource::parse_record("git:git@github.com:o/r.git|v1.0.0").expect("scp"),
            github("o", "r", Some("v1.0.0"))
        );
        let sha = "7fd1a60b01f91b314f59955a4e4d4e80d8edf11d";
        assert_eq!(
            PluginSource::parse_record(&format!("git:https://github.com/o/r@{sha}")).expect("at"),
            github("o", "r", Some(sha))
        );
        assert_eq!(
            PluginSource::parse_record("git:https://github.com/o/r").expect("bare"),
            github("o", "r", None)
        );
    }

    #[test]
    fn legacy_git_records_outside_github_are_rejected() {
        let err = PluginSource::parse_record("git:https://example.com/o/r|v1").expect_err("legacy");
        assert_eq!(err.reason, SourceReject::LegacyGit);
        // 带 `@` 的 SSH 用户信息不能被当成 commit 拆开
        let err =
            PluginSource::parse_record("git:https://user@example.com/o/r").expect_err("userinfo");
        assert_eq!(err.reason, SourceReject::LegacyGit);
    }
}
