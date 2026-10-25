//! 各服务端来源共用的解析脚手架
//!
//! 本 crate 下的来源（Mojang / Fabric / Forge / Paper / Purpur / Quilt …）在
//! 「怎么拉元数据」上各不相同，但在「怎么把元数据落成 [`ResolvedArtifact`]」
//! 上是同一件事：校验版本号、拼 https 地址、装出 [`JarArtifact`]。这些逻辑
//! 分散在十来个来源里重复实现，等于给每个变体各写一套——改一处校验规则要改
//! 十几遍，且必然有漏改的那一个。
//!
//! 这里集中四类可复用件，来源只写自己独有的部分：
//!
//! - [`parse_https_url`] / [`normalize_base`]：下载地址与镜像根地址的统一校验
//!   （只接受 `https`，镜像根去尾斜杠）
//! - [`Downloads`]：`ResolvedArtifact` 的构造器，把「安装形态 / 校验和 / 大小 /
//!   Java 要求」的组装收敛到一处，避免每个来源自己拼结构体
//! - [`pick_by_version_aliases`]：`latest` / `latest-snapshot` 这类保留别名的
//!   统一解析，来源只需给出「已有版本列表」与「哪个是最新」
//! - [`UrlError`]：URL 校验失败的公开错误类型，供各来源复用
//!
//! # 为什么不做成 trait 的默认方法
//!
//! [`port::ServerJarSource`] 的每个方法都要访问来源自己的状态（客户端、镜像
//! 地址、选取策略），默认实现只能靠额外的抽象方法把状态取出来，反而把「来源
//! 各自的长处」压成一套别扭的最小公倍数。这些纯函数没有状态，直接调用更清楚。
//!
//! # 扩展第三方变体时
//!
//! 第三方来源不必经过本模块——它只要实现 [`port::ServerJarSource`] 就能注册。
//! 本模块是「内置来源少写重复代码」的内部脚手架，不是扩展契约的一部分。

use domain::server::artifact::{Checksum, JarArtifact};
use domain::server::version::VersionId;
use port::{InstallKind, ResolvedArtifact, SourceError};
use url::Url;

/// URL 处理失败
///
/// 各来源共用的错误类型：地址无法解析，或不是 `https`。对外统一经
/// [`SourceError::Decode`] / [`SourceError::Init`] 包装，这里保留具体原因。
#[derive(Debug, thiserror::Error)]
pub(crate) enum UrlError {
    /// 地址无法解析
    #[error("invalid URL: {0}")]
    Parse(#[source] url::ParseError),
    /// 只接受 `https`：下载地址与镜像根都不允许明文传输
    #[error("only https URLs are accepted, got schema {0:?}")]
    Insecure(String),
}

/// 解析下载地址并要求 `https`
///
/// 下载地址来自上游响应，必须校验：上游被劫持或配置错误时给出 `http://` 会
/// 让服务端核心以明文传输，是最容易被忽视的降级。
///
/// # Errors
///
/// 地址非法或非 `https` 时返回 [`SourceError::Decode`]
pub(crate) fn parse_https_url(raw: &str) -> Result<Url, SourceError> {
    let url = raw
        .parse::<Url>()
        .map_err(|e| SourceError::Decode(Box::new(UrlError::Parse(e))))?;
    if url.scheme() != "https" {
        return Err(SourceError::Decode(Box::new(UrlError::Insecure(
            url.scheme().to_owned(),
        ))));
    }
    Ok(url)
}

/// 归一化镜像根地址：去尾斜杠并要求 `https`
///
/// `base` 为 `None` 时用 `fallback`（官方地址）。返回的串不含尾斜杠，调用方
/// 直接 `format!("{base}/path")` 不会拼出双斜杠。
///
/// # Errors
///
/// 地址非法或非 `https` 时返回 [`SourceError::Init`]——这是构造期错误，
/// 应在装配阶段就失败，而不是等到下载才发现。
pub(crate) fn normalize_base(base: Option<String>, fallback: &str) -> Result<String, SourceError> {
    let base = base.unwrap_or_else(|| fallback.to_owned());
    let trimmed = base.trim_end_matches('/').to_owned();
    let parsed = trimmed
        .parse::<Url>()
        .map_err(|e| SourceError::Init(Box::new(UrlError::Parse(e))))?;
    if parsed.scheme() != "https" {
        return Err(SourceError::Init(Box::new(UrlError::Insecure(
            parsed.scheme().to_owned(),
        ))));
    }
    Ok(trimmed)
}

/// 一次「下载物」的完整描述
///
/// 把 [`ResolvedArtifact`] 的六个字段收成一个构造器，来源只提供自己有把握的
/// 部分：单个 JAR 的来源不关心 `install`，安装器来源不关心 `java_major`。
///
/// 语义与 [`ResolvedArtifact`] 一致，本类型只做组装，不做额外校验（`size: 0`
/// 与 `Checksum::unverified()` 的「未知」语义由下游解释）。
#[derive(Debug, Clone)]
pub(crate) struct Downloads {
    /// 下载地址（已校验 `https`）
    pub(crate) location: Url,
    /// 校验和；上游未提供时用 [`Checksum::unverified`]
    pub(crate) checksum: Checksum,
    /// 声明大小；`0` 表示未知（由下载结果覆盖）
    pub(crate) size: u64,
    /// 该版本要求的 Java 大版本
    pub(crate) java_major: Option<domain::java::JavaMajor>,
    /// 下载物用途
    pub(crate) install: InstallKind,
}

impl Downloads {
    /// 组装为可下载的服务端核心（单 JAR）
    ///
    /// 大多数来源（原版 / Fabric / Paper / Purpur）下载得到的就是可运行服务端。
    #[must_use]
    pub(crate) fn direct(location: Url) -> Self {
        Self {
            location,
            checksum: Checksum::unverified(),
            size: 0,
            java_major: None,
            install: InstallKind::Direct,
        }
    }

    /// 组装为需要运行的安装器（`Forge` / `NeoForge` / `Quilt` 形态）
    #[must_use]
    pub(crate) fn installer(location: Url) -> Self {
        Self {
            install: InstallKind::Installer,
            ..Self::direct(location)
        }
    }

    /// 指定校验和
    #[must_use]
    #[allow(
        dead_code,
        reason = "校验和构造口子：Paper / Purpur / Leaf 等内嵌哈希的来源使用"
    )]
    pub(crate) fn checksum(mut self, checksum: Checksum) -> Self {
        self.checksum = checksum;
        self
    }

    /// 指定声明大小
    #[must_use]
    #[allow(
        dead_code,
        reason = "声明大小构造口子：Paper 等给出 Content-Length 的来源使用"
    )]
    pub(crate) fn size(mut self, size: u64) -> Self {
        self.size = size;
        self
    }

    /// 指定版本要求的 Java 大版本
    #[must_use]
    #[allow(dead_code, reason = "Java 要求构造口子：带 javaVersion 的来源使用")]
    pub(crate) fn java_major(mut self, java_major: Option<domain::java::JavaMajor>) -> Self {
        self.java_major = java_major;
        self
    }

    /// 覆盖安装形态（构造器已经区分 direct / installer，这里供特殊来源微调）
    #[must_use]
    #[allow(dead_code, reason = "形态覆盖口子：安装形态需运行时决定的来源使用")]
    pub(crate) fn install(mut self, install: InstallKind) -> Self {
        self.install = install;
        self
    }

    /// 落成 [`ResolvedArtifact`]，`version` 由调用方的请求决定
    #[must_use]
    pub(crate) fn resolve(self, version: &VersionId) -> ResolvedArtifact {
        ResolvedArtifact {
            artifact: JarArtifact {
                version: version.clone(),
                size: self.size,
                checksum: self.checksum,
            },
            location: self.location,
            java_major: self.java_major,
            install: self.install,
        }
    }
}

/// 保留别名
pub(crate) const LATEST: &str = "latest";
/// 保留别名：最新快照
pub(crate) const LATEST_SNAPSHOT: &str = "latest-snapshot";

/// 版本标识经保留别名解析后的意图
///
/// 来源据此在自有列表里挑选，而不必各自写一遍 `match raw { "latest" => …, … }`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VersionIntent<'a> {
    /// 最新稳定版
    Latest,
    /// 最新快照版
    LatestSnapshot,
    /// 字面版本号
    Literal(&'a str),
}

impl<'a> VersionIntent<'a> {
    /// 把版本标识拆成「保留别名」或「字面版本」
    #[must_use]
    pub(crate) fn of(version: &'a VersionId) -> Self {
        match version.as_str() {
            LATEST => Self::Latest,
            LATEST_SNAPSHOT => Self::LatestSnapshot,
            other => Self::Literal(other),
        }
    }

    /// 字面版本本体；别名返回 `None`
    ///
    /// 来源需要在「是字面版本才查重」这类分支里拿到裸串（而非再 match 一次
    /// [`VersionIntent`]），用这个访问器比重复解构更清楚。
    #[must_use]
    #[allow(
        dead_code,
        reason = "字面版本访问器：来源需要在持有 VersionIntent 时取裸串"
    )]
    pub(crate) fn literal(&self) -> Option<&'a str> {
        match self {
            Self::Literal(raw) => Some(raw),
            Self::Latest | Self::LatestSnapshot => None,
        }
    }
}

/// 按别名 / 字面量挑选版本号
///
/// `candidates` 是来源已有的版本列表，顺序由来源决定（多数上游「新在前」）；
/// 别名取 `candidates` 里第一个满足 `is_latest` 的项，字面量要求**原样出现在
/// 列表里**——上游对不存在的版本往往回 `400` 或返回空列表，提前判定能把错误
/// 如实报成 [`SourceError::NotFound`]，而不是语义模糊的状态码错误。
///
/// `is_latest` 只对 `latest` 生效；`latest-snapshot` 用 `is_snapshot` 判定。
/// 两者都没有匹配项时返回 `None`，由调用方决定报 `NotFound` 还是回落。
pub(crate) fn pick_by_version_aliases<'a, I>(
    intent: VersionIntent<'a>,
    candidates: I,
    is_latest: impl Fn(&str) -> bool,
    is_snapshot: impl Fn(&str) -> bool,
) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    match intent {
        VersionIntent::Literal(raw) => candidates
            .into_iter()
            .find(|candidate| *candidate == raw)
            .map(str::to_owned),
        VersionIntent::Latest => candidates
            .into_iter()
            .find(|candidate| is_latest(candidate))
            .map(str::to_owned),
        VersionIntent::LatestSnapshot => candidates
            .into_iter()
            .find(|candidate| is_snapshot(candidate))
            .map(str::to_owned),
    }
}

/// 解析 sha1 侧车：纯 40 位十六进制
///
/// `Forge` / `NeoForge` / `SpongeVanilla` 都在 JAR 旁给出同名 `.sha1`，内容就是
/// 一行 40 位十六进制；格式不符时返回 `None`，由调用方降级为「无校验」或报错。
#[must_use]
pub(crate) fn parse_sha1_sidecar(raw: &str) -> Option<Checksum> {
    let trimmed = raw.trim();
    if trimmed.len() == 40 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(Checksum::new(
            domain::server::artifact::ChecksumAlgorithm::Sha1,
            trimmed,
        ))
    } else {
        None
    }
}

/// 解析 sha256 十六进制串（响应体内嵌的校验值，不是侧车文件）
///
/// Paper Fill v3 / Leaf 把 `sha256` 直接写在版本响应里，没有侧车文件可拉。
/// 格式不符时返回 `None`，由调用方降级为无校验。
#[must_use]
#[allow(
    dead_code,
    reason = "内嵌 sha256 解析：Paper / Leaf 等响应体自带哈希的来源使用"
)]
pub(crate) fn parse_sha256_hex(raw: &str) -> Option<Checksum> {
    let trimmed = raw.trim();
    if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(Checksum::new(
            domain::server::artifact::ChecksumAlgorithm::Sha256,
            trimmed,
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::server::artifact::ChecksumAlgorithm;

    #[test]
    fn parse_https_url_accepts_https_only() {
        assert!(parse_https_url("https://example.com/x.jar").is_ok());
        assert!(parse_https_url("http://example.com/x.jar").is_err());
        assert!(parse_https_url("not a url").is_err());
        assert!(parse_https_url("ftp://example.com/x.jar").is_err());
    }

    #[test]
    fn normalize_base_trims_and_requires_https() {
        assert_eq!(
            normalize_base(None, "https://official.example/").expect("默认"),
            "https://official.example"
        );
        assert_eq!(
            normalize_base(Some("https://mirror.example/maven//".to_owned()), "x").expect("镜像"),
            "https://mirror.example/maven"
        );
        assert!(normalize_base(Some("http://mirror.example".to_owned()), "x").is_err());
    }

    #[test]
    fn downloads_direct_has_unverified_checksum_and_zero_size() {
        let url: Url = "https://example.com/server.jar".parse().expect("url");
        let resolved = Downloads::direct(url).resolve(&VersionId::new("1.21.1"));
        assert_eq!(resolved.install, InstallKind::Direct);
        assert_eq!(resolved.artifact.size, 0);
        assert!(!resolved.artifact.checksum.is_verified());
        assert_eq!(resolved.artifact.version, VersionId::new("1.21.1"));
        assert_eq!(resolved.java_major, None);
    }

    /// `literal` 取出字面版本本体
    #[test]
    fn version_intent_literal_accessor() {
        assert_eq!(VersionIntent::Literal("1.21.1").literal(), Some("1.21.1"));
        assert_eq!(VersionIntent::Latest.literal(), None);
        assert_eq!(VersionIntent::LatestSnapshot.literal(), None);
    }

    /// `install` 可显式覆盖：少数来源的形态由运行时决定
    #[test]
    fn downloads_install_can_be_overridden() {
        let url: Url = "https://example.com/x.jar".parse().expect("url");
        let resolved = Downloads::direct(url)
            .install(InstallKind::Installer)
            .java_major(None)
            .resolve(&VersionId::new("1.21.1"));
        assert_eq!(resolved.install, InstallKind::Installer);
    }

    #[test]
    fn downloads_builders_carry_checksum_and_size() {
        let url: Url = "https://example.com/x.jar".parse().expect("url");
        let checksum = parse_sha256_hex(&"a".repeat(64)).expect("sha256");
        let resolved = Downloads::direct(url)
            .checksum(checksum.clone())
            .size(1024)
            .resolve(&VersionId::new("1.21.1"));
        assert_eq!(resolved.artifact.size, 1024);
        assert_eq!(
            resolved.artifact.checksum.algorithm(),
            ChecksumAlgorithm::Sha256
        );
        assert!(resolved.artifact.checksum.is_verified());
    }

    #[test]
    fn downloads_installer_sets_kind() {
        let url: Url = "https://example.com/installer.jar".parse().expect("url");
        let resolved = Downloads::installer(url).resolve(&VersionId::new("1.21.1"));
        assert_eq!(resolved.install, InstallKind::Installer);
    }

    #[test]
    fn version_intent_splits_aliases_from_literals() {
        assert_eq!(
            VersionIntent::of(&VersionId::new("latest")),
            VersionIntent::Latest
        );
        assert_eq!(
            VersionIntent::of(&VersionId::new("latest-snapshot")),
            VersionIntent::LatestSnapshot
        );
        assert_eq!(
            VersionIntent::of(&VersionId::new("1.21.1")),
            VersionIntent::Literal("1.21.1")
        );
        assert_eq!(
            VersionIntent::of(&VersionId::new("1.21.1")).literal(),
            Some("1.21.1")
        );
        assert_eq!(VersionIntent::of(&VersionId::new("latest")).literal(), None);
    }

    #[test]
    fn pick_aliases_prefers_first_matching_candidate() {
        let versions = ["26.3-rc-1", "26.3", "1.21.1", "1.21"];
        let stable: &[&str] = &["26.3", "1.21.1"];
        assert_eq!(
            pick_by_version_aliases(
                VersionIntent::Latest,
                versions,
                |v| stable.contains(&v),
                |v| v.contains('-'),
            ),
            Some("26.3".to_owned())
        );
        assert_eq!(
            pick_by_version_aliases(
                VersionIntent::LatestSnapshot,
                versions,
                |v| stable.contains(&v),
                |v| v.contains('-'),
            ),
            Some("26.3-rc-1".to_owned())
        );
    }

    #[test]
    fn pick_literal_requires_presence() {
        let versions = ["1.21.1", "1.21"];
        assert_eq!(
            pick_by_version_aliases(
                VersionIntent::Literal("1.21.1"),
                versions,
                |_| false,
                |_| false
            ),
            Some("1.21.1".to_owned())
        );
        assert_eq!(
            pick_by_version_aliases(
                VersionIntent::Literal("1.99.9"),
                versions,
                |_| false,
                |_| false
            ),
            None
        );
    }

    #[test]
    fn pick_aliases_without_match_returns_none() {
        let versions = ["1.21.1"];
        assert_eq!(
            pick_by_version_aliases(VersionIntent::LatestSnapshot, versions, |_| true, |_| false),
            None
        );
    }

    #[test]
    fn parse_sha1_sidecar_accepts_40_hex() {
        let raw = "00ef73bf94cb8629aab115035868ff96b43af01c\n";
        let checksum = parse_sha1_sidecar(raw).expect("sha1");
        assert_eq!(checksum.algorithm(), ChecksumAlgorithm::Sha1);
        assert_eq!(checksum.hex(), "00ef73bf94cb8629aab115035868ff96b43af01c");
    }

    #[test]
    fn parse_sha1_sidecar_rejects_invalid() {
        assert!(parse_sha1_sidecar("").is_none());
        assert!(parse_sha1_sidecar("abc").is_none());
        assert!(parse_sha1_sidecar(&"z".repeat(40)).is_none());
    }

    #[test]
    fn parse_sha256_hex_accepts_64_hex() {
        let checksum = parse_sha256_hex(&"A".repeat(64)).expect("sha256");
        assert_eq!(checksum.algorithm(), ChecksumAlgorithm::Sha256);
        assert!(parse_sha256_hex(&"a".repeat(63)).is_none());
        assert!(parse_sha256_hex(&"z".repeat(64)).is_none());
    }
}
