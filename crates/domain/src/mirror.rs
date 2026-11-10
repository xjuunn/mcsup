//! 下载源：同一份资源还能去哪个站点取
//!
//! 三段结构，越往下越具体：
//!
//! - [`Upstream`]：**上游面**，镜像的最小编址单位——官方站点上的一段地址前缀
//!   （如 `https://maven.minecraftforge.net/`）。同一份文件在不同站点上的路径
//!   余量相同，这是镜像能够工作的前提。
//! - [`Source`]：**下载源**，声明它覆盖哪些上游面、各自替换成哪个前缀。
//! - [`Catalog`]：上游面与源的集合，负责把一条上游地址解析成候选列表。
//!
//! 因此新增一个镜像只需要往目录里加一条 [`Source`]：下载、安装、服务端生命周期
//! 都不认识具体站点，它们只消费「候选地址列表」。
//!
//! 官方自身也是一个源（[`Source::official`]）：它把上游面映射回原地址。这样
//! 「官方」与「镜像」在选源、回退、日志里走同一条路，不出现两套分支——这是
//! 「失败时回退到官方」不需要特判的原因。
//!
//! 本模块只有纯函数：不联网、不读盘、不记日志。

use std::fmt;
use std::time::Duration;

/// 源的标识最大长度
///
/// 它要进配置文件、持久化状态与日志：太长没人读，也容易被误当成路径。
pub const MAX_ID_LEN: usize = 32;

/// 下载源标识
///
/// 限定 kebab-case ASCII。它会写进 `config.toml` 与选源状态文件，也会出现在
/// 日志与错误里：允许大小写或空白就会让「BMCLAPI」「bmclapi」变成两个源，
/// 各自探测、各自持久化，最后谁也说不清当前用的是哪个。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(String);

impl SourceId {
    /// 官方源的标识
    ///
    /// 固定值而不是可配置项：回退目标必须唯一且稳定，否则「回退到官方」
    /// 在不同配置下指向不同站点。
    pub const OFFICIAL: &'static str = "official";

    /// 校验并构造
    ///
    /// # Errors
    ///
    /// 空、超长或含 kebab-case 之外的字符时返回 [`InvalidSourceId`]
    pub fn new(raw: impl Into<String>) -> Result<Self, InvalidSourceId> {
        let raw = raw.into();
        if is_kebab_case(&raw) {
            Ok(Self(raw))
        } else {
            Err(InvalidSourceId { raw })
        }
    }

    /// 官方源
    #[must_use]
    pub fn official() -> Self {
        Self(Self::OFFICIAL.to_owned())
    }

    /// 稳定文本（配置、持久化、日志共用）
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 是否是官方源
    #[must_use]
    pub fn is_official(&self) -> bool {
        self.0 == Self::OFFICIAL
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 下载源标识不合法
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid download source id {raw:?}: expected 1..={MAX_ID_LEN} chars of [a-z0-9-], no leading/trailing '-'"
)]
pub struct InvalidSourceId {
    /// 被拒的原始值
    pub raw: String,
}

/// kebab-case：非空、不超长、仅小写字母/数字/连字符，且连字符不落在两端
fn is_kebab_case(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > MAX_ID_LEN {
        return false;
    }
    raw.split('-').all(|piece| {
        !piece.is_empty()
            && piece
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    })
}

/// 上游面：镜像的最小编址单位
///
/// 一段官方地址前缀就是一面。前缀必须以 `/` 结尾（构造时补齐），否则
/// `https://a.example/` 会匹配上 `https://a.example.evil/`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    /// 持久化与日志用的稳定键（如 `forge-maven`）
    key: String,
    /// 官方地址前缀，规整为以 `/` 结尾
    prefix: String,
}

impl Upstream {
    /// 构造；前缀末尾的 `/` 自动补齐
    ///
    /// # Errors
    ///
    /// 键不合法（同 [`SourceId`] 的规则）或前缀为空时返回 [`InvalidUpstream`]
    pub fn new(key: impl Into<String>, prefix: impl Into<String>) -> Result<Self, InvalidUpstream> {
        let key = key.into();
        if !is_kebab_case(&key) {
            return Err(InvalidUpstream::Key { key });
        }
        let prefix = normalize_prefix(&prefix.into());
        if prefix.is_empty() {
            return Err(InvalidUpstream::EmptyPrefix { key });
        }
        Ok(Self { key, prefix })
    }

    /// 稳定键
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// 官方地址前缀
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// 地址属于这一面时返回路径余量（可以沿用它的整个地址）
    ///
    /// 用 `strip_prefix` 而不是字符串查找：前缀自带尾 `/`，边界天然对齐，
    /// 不会把 `https://a.example/x` 认成 `https://a.example/xy` 的一部分。
    #[must_use]
    pub fn strip<'a>(&self, location: &'a str) -> Option<&'a str> {
        location.strip_prefix(self.prefix.as_str())
    }
}

/// 上游面不合法
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InvalidUpstream {
    /// 键不符合 kebab-case
    #[error("invalid upstream key {key:?}: expected kebab-case ascii")]
    Key {
        /// 被拒的键
        key: String,
    },
    /// 前缀为空
    #[error("upstream {key:?} has an empty address prefix")]
    EmptyPrefix {
        /// 该面的键
        key: String,
    },
}

/// 补齐前缀末尾的 `/`
fn normalize_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.ends_with('/') {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/")
    }
}

/// 一条上游面上的候选地址
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// 提供这个地址的源
    pub source: SourceId,
    /// 改写后的完整地址
    pub location: String,
}

/// 一个源在上一个上游面上的入口
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// 上游面的键
    upstream: String,
    /// 替换用的地址前缀，规整为以 `/` 结尾
    base: String,
}

impl Binding {
    /// 构造；前缀末尾的 `/` 自动补齐
    ///
    /// # Errors
    ///
    /// 上游键不合法或前缀为空时返回 [`InvalidBinding`]
    pub fn new(
        upstream: impl Into<String>,
        base: impl Into<String>,
    ) -> Result<Self, InvalidBinding> {
        let upstream = upstream.into();
        if !is_kebab_case(&upstream) {
            return Err(InvalidBinding::Upstream { upstream });
        }
        let base = normalize_prefix(&base.into());
        if base.is_empty() {
            return Err(InvalidBinding::EmptyBase { upstream });
        }
        Ok(Self { upstream, base })
    }

    /// 上游面的键
    #[must_use]
    pub fn upstream(&self) -> &str {
        &self.upstream
    }

    /// 替换用的地址前缀
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }
}

/// 绑定不合法
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InvalidBinding {
    /// 上游键不符合 kebab-case
    #[error("invalid upstream key {upstream:?}: expected kebab-case ascii")]
    Upstream {
        /// 被拒的键
        upstream: String,
    },
    /// 前缀为空
    #[error("binding on upstream {upstream:?} has an empty address prefix")]
    EmptyBase {
        /// 该绑定所属的上游面
        upstream: String,
    },
}

/// 一个下载源
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// 标识（配置、持久化、日志共用）
    id: SourceId,
    /// 人类可读名，缺省等于标识
    label: String,
    /// 覆盖的上游面
    bindings: Vec<Binding>,
}

impl Source {
    /// 构造；`label` 为空时用标识兜底
    #[must_use]
    pub fn new(id: SourceId, label: impl Into<String>) -> Self {
        let label = label.into();
        let label = if label.trim().is_empty() {
            id.as_str().to_owned()
        } else {
            label
        };
        Self {
            id,
            label,
            bindings: Vec::new(),
        }
    }

    /// 官方源：覆盖全部上游面，各自映射回官方地址
    ///
    /// 官方绑定是「候选列表里一定有原地址」的唯一保证。没有它，镜像全挂时
    /// 就没有回退目标。
    #[must_use]
    pub fn official(upstreams: &[Upstream]) -> Self {
        let mut source = Self::new(SourceId::official(), "官方");
        for upstream in upstreams {
            source.bindings.push(Binding {
                upstream: upstream.key.clone(),
                base: upstream.prefix.clone(),
            });
        }
        source
    }

    /// 追加一条绑定（同一上游面重复绑定时后者生效）
    #[must_use]
    pub fn with(mut self, binding: Binding) -> Self {
        self.bindings
            .retain(|other| other.upstream != binding.upstream);
        self.bindings.push(binding);
        self
    }

    /// 标识
    #[must_use]
    pub fn id(&self) -> &SourceId {
        &self.id
    }

    /// 人类可读名
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// 全部绑定
    #[must_use]
    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    fn binding(&self, upstream: &str) -> Option<&Binding> {
        self.bindings
            .iter()
            .find(|binding| binding.upstream == upstream)
    }
}

/// 一条地址的解析结果
#[derive(Debug, Clone)]
pub struct Resolved<'a> {
    /// 命中的上游面
    pub upstream: &'a Upstream,
    /// 候选地址，顺序即源在目录里的顺序
    ///
    /// 官方源在目录里排第一时它就是第一个元素；同一地址只会出现一次
    /// （两个源给出同样的前缀时，先出现的胜出）。
    pub candidates: Vec<Candidate>,
}

impl Resolved<'_> {
    /// 上游面的键
    #[must_use]
    pub fn upstream_key(&self) -> &str {
        self.upstream.key()
    }
}

/// 上游面与下载源的集合
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    upstreams: Vec<Upstream>,
    sources: Vec<Source>,
}

impl Catalog {
    /// 用上游面与源构造
    ///
    /// 要求至少一个上游面、至少一个源，且含官方源：目录的用途就是「给出候选」，
    /// 缺官方源时镜像一旦全挂就无路可退。
    ///
    /// # Errors
    ///
    /// 见 [`CatalogError`]
    pub fn new(upstreams: Vec<Upstream>, sources: Vec<Source>) -> Result<Self, CatalogError> {
        if upstreams.is_empty() {
            return Err(CatalogError::NoUpstream);
        }
        if sources.is_empty() {
            return Err(CatalogError::NoSource);
        }
        let mut seen = Vec::with_capacity(upstreams.len());
        for upstream in &upstreams {
            if seen.contains(&upstream.key) {
                return Err(CatalogError::DuplicateUpstream(upstream.key.clone()));
            }
            seen.push(upstream.key.clone());
        }
        let mut ids: Vec<&SourceId> = Vec::with_capacity(sources.len());
        for source in &sources {
            if ids.contains(&&source.id) {
                return Err(CatalogError::DuplicateSource(source.id.clone()));
            }
            ids.push(&source.id);
            for binding in &source.bindings {
                if !seen.contains(&binding.upstream) {
                    return Err(CatalogError::UnknownUpstream {
                        id: source.id.clone(),
                        upstream: binding.upstream.clone(),
                    });
                }
            }
        }
        if !ids.iter().any(|id| id.is_official()) {
            return Err(CatalogError::NoOfficial);
        }
        Ok(Self { upstreams, sources })
    }

    /// 官方优先的目录：官方源排第一，其余源依次排在后面
    ///
    /// 这是生产装配的唯一入口。官方的位置由此结构性保证：候选列表的第一个
    /// 元素永远是原地址，镜像探测失败时不需要额外分支就能退回官方。
    ///
    /// # Errors
    ///
    /// 见 [`CatalogError`]
    pub fn with_mirrors(
        upstreams: Vec<Upstream>,
        mirrors: Vec<Source>,
    ) -> Result<Self, CatalogError> {
        let official = Source::official(&upstreams);
        let mut sources = Vec::with_capacity(mirrors.len() + 1);
        sources.push(official);
        sources.extend(mirrors);
        Self::new(upstreams, sources)
    }

    /// 空目录：全部地址都解析不出候选
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// 追加一个源；已存在同标识的源时被替换（配置覆盖内置定义）
    ///
    /// 不返回 `Result`：新增源是目录的常规扩展动作，冲突按「后者生效」处理，
    /// 与 [`Source::with`] 对同一个上游面的口径一致。
    pub fn insert(&mut self, source: Source) {
        self.sources.retain(|other| other.id != source.id);
        self.sources.push(source);
    }

    /// 全部上游面
    #[must_use]
    pub fn upstreams(&self) -> &[Upstream] {
        &self.upstreams
    }

    /// 全部源，顺序即缺省优先级
    #[must_use]
    pub fn sources(&self) -> &[Source] {
        &self.sources
    }

    /// 命中的上游面（最长前缀优先）
    ///
    /// 取最长而不是先声明：`piston-meta` 与 `piston` 这类嵌套前缀同时存在时，
    /// 顺序会决定命中哪一个，而顺序是最容易被一次插入改动的东西。
    #[must_use]
    pub fn upstream_of(&self, location: &str) -> Option<&Upstream> {
        self.upstreams
            .iter()
            .filter(|upstream| location.starts_with(upstream.prefix()))
            .max_by_key(|upstream| upstream.prefix().len())
    }

    /// 解析一条上游地址
    ///
    /// 返回 `None` 表示这条地址不属于任何上游面：没有可切换的站点，
    /// 调用方应当原样使用。
    #[must_use]
    pub fn resolve(&self, location: &str) -> Option<Resolved<'_>> {
        let upstream = self.upstream_of(location)?;
        let rest = upstream.strip(location)?;
        let mut candidates: Vec<Candidate> = Vec::with_capacity(self.sources.len());
        for source in &self.sources {
            let Some(binding) = source.binding(upstream.key()) else {
                continue;
            };
            let candidate = Candidate {
                source: source.id.clone(),
                location: format!("{}{rest}", binding.base()),
            };
            if candidates
                .iter()
                .any(|known| known.location == candidate.location)
            {
                continue;
            }
            candidates.push(candidate);
        }
        (!candidates.is_empty()).then_some(Resolved {
            upstream,
            candidates,
        })
    }

    /// 按源标识取源
    #[must_use]
    pub fn source(&self, id: &SourceId) -> Option<&Source> {
        self.sources.iter().find(|source| &source.id == id)
    }
}

/// 目录不合法
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CatalogError {
    /// 没有任何上游面
    #[error("catalog has no upstream")]
    NoUpstream,
    /// 没有任何源
    #[error("catalog has no download source")]
    NoSource,
    /// 上游面键重复
    #[error("duplicate upstream key {0:?}")]
    DuplicateUpstream(String),
    /// 源标识重复
    #[error("duplicate download source {0}")]
    DuplicateSource(SourceId),
    /// 源绑定了未声明的上游面
    #[error("source {id} binds unknown upstream {upstream:?}")]
    UnknownUpstream {
        /// 该源标识（不叫 `source`：thiserror 会把同名字段当成错误链的下一层）
        id: SourceId,
        /// 未声明的上游面键
        upstream: String,
    },
    /// 缺官方源
    #[error("catalog has no official source: nothing to fall back to")]
    NoOfficial,
    /// 上游面定义不合法
    ///
    /// 与 [`Self::Binding`] 一起让目录表的构造可以全程用 `?`：内置表是一串
    /// 字面量，任何一条写错都该在装配时报出来，而不是在下载时才暴露。
    #[error(transparent)]
    Upstream(#[from] InvalidUpstream),
    /// 绑定定义不合法
    #[error(transparent)]
    Binding(#[from] InvalidBinding),
    /// 源标识不合法
    #[error(transparent)]
    Source(#[from] InvalidSourceId),
}

/// 一次探测样本
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// 被测的源
    pub source: SourceId,
    /// 往返耗时
    pub latency: Duration,
}

/// 从样本里选源：取耗时最短者，并列时取 `order` 里靠前的
///
/// `order` 是目录里的源顺序。并列必须有确定的次序：毫秒量化的耗时常打平，
/// 没有次序时同一个网络会在两次选择里给出不同的源，持久化的意义随之消失。
#[must_use]
pub fn select(order: &[SourceId], samples: &[Sample]) -> Option<SourceId> {
    let rank = |id: &SourceId| {
        order
            .iter()
            .position(|other| other == id)
            .unwrap_or(usize::MAX)
    };
    samples
        .iter()
        .min_by(|left, right| {
            left.latency
                .cmp(&right.latency)
                .then_with(|| rank(&left.source).cmp(&rank(&right.source)))
        })
        .map(|sample| sample.source.clone())
}

/// 把选中的源排到最前，其余保持目录顺序
///
/// 选择的意义就是「先用它」：只把中选地址放进候选列表而不排到前面，
/// 下载仍会先打官方，选源等于白做。
#[must_use]
pub fn preferred(candidates: &[Candidate], chosen: Option<&SourceId>) -> Vec<Candidate> {
    let Some(chosen) = chosen else {
        return candidates.to_vec();
    };
    let mut ordered: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| &candidate.source == chosen)
        .cloned()
        .collect();
    ordered.extend(
        candidates
            .iter()
            .filter(|candidate| &candidate.source != chosen)
            .cloned(),
    );
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(key: &str, prefix: &str) -> Upstream {
        Upstream::new(key, prefix).expect("upstream")
    }

    fn binding(upstream: &str, base: &str) -> Binding {
        Binding::new(upstream, base).expect("binding")
    }

    fn sample(source: &str, millis: u64) -> Sample {
        Sample {
            source: SourceId::new(source).expect("id"),
            latency: Duration::from_millis(millis),
        }
    }

    fn catalog() -> Catalog {
        let upstreams = vec![upstream("forge-maven", "https://maven.minecraftforge.net")];
        let mirror = Source::new(SourceId::new("bmclapi").expect("id"), "BMCLAPI").with(binding(
            "forge-maven",
            "https://bmclapi2.bangbang93.com/maven",
        ));
        // 官方优先：候选列表以原地址开头，镜像跟在后面
        Catalog::with_mirrors(upstreams, vec![mirror]).expect("catalog")
    }

    /// 镜像优先的目录：用来看「中选源排到最前」的效果
    fn mirror_first_catalog() -> Catalog {
        let upstreams = vec![upstream("forge-maven", "https://maven.minecraftforge.net")];
        let official = Source::official(&upstreams);
        let mirror = Source::new(SourceId::new("bmclapi").expect("id"), "BMCLAPI").with(binding(
            "forge-maven",
            "https://bmclapi2.bangbang93.com/maven",
        ));
        Catalog::new(upstreams, vec![mirror, official]).expect("catalog")
    }

    #[test]
    fn source_id_accepts_kebab_case_only() {
        assert!(SourceId::new("bmclapi").is_ok());
        assert!(SourceId::new("tuna-adoptium").is_ok());
        assert!(SourceId::new("mcim2").is_ok());
        for bad in [
            "",
            "BMCLAPI",
            "-lead",
            "trail-",
            "two--dash",
            "space id",
            "dot.ted",
        ] {
            assert!(SourceId::new(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(SourceId::new("a".repeat(MAX_ID_LEN + 1)).is_err());
    }

    #[test]
    fn official_is_pinned() {
        assert!(SourceId::official().is_official());
        assert_eq!(SourceId::official().as_str(), "official");
        assert!(!SourceId::new("mirror").expect("id").is_official());
    }

    #[test]
    fn upstream_prefix_gets_trailing_slash() {
        let upstream = upstream("forge-maven", "https://maven.minecraftforge.net");
        assert_eq!(upstream.prefix(), "https://maven.minecraftforge.net/");
        assert_eq!(
            upstream.strip("https://maven.minecraftforge.net/net/x"),
            Some("net/x")
        );
        // 边界：同前缀但不同域名不算命中
        assert_eq!(
            upstream.strip("https://maven.minecraftforge.net.evil/x"),
            None
        );
    }

    #[test]
    fn upstream_rejects_bad_key_and_empty_prefix() {
        assert!(Upstream::new("Forge", "https://a.example").is_err());
        assert!(Upstream::new("forge", "  ").is_err());
    }

    #[test]
    fn resolve_rewrites_prefix_and_keeps_rest() {
        let catalog = mirror_first_catalog();
        let resolved = catalog
            .resolve(
                "https://maven.minecraftforge.net/net/minecraftforge/forge/1.20.1/installer.jar",
            )
            .expect("resolved");
        assert_eq!(resolved.upstream_key(), "forge-maven");
        assert_eq!(
            resolved
                .candidates
                .iter()
                .map(|candidate| (candidate.source.as_str(), candidate.location.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "bmclapi",
                    "https://bmclapi2.bangbang93.com/maven/net/minecraftforge/forge/1.20.1/installer.jar"
                ),
                (
                    "official",
                    "https://maven.minecraftforge.net/net/minecraftforge/forge/1.20.1/installer.jar"
                ),
            ]
        );
    }

    #[test]
    fn with_mirrors_puts_official_first() {
        let catalog = catalog();
        let resolved = catalog
            .resolve("https://maven.minecraftforge.net/x.jar")
            .expect("resolved");
        let sources: Vec<&str> = resolved
            .candidates
            .iter()
            .map(|candidate| candidate.source.as_str())
            .collect();
        assert_eq!(sources, vec!["official", "bmclapi"]);
    }

    #[test]
    fn resolve_without_match_returns_none() {
        let catalog = catalog();
        assert!(catalog.resolve("https://example.com/file.jar").is_none());
    }

    #[test]
    fn resolve_keeps_query() {
        let catalog = mirror_first_catalog();
        let resolved = catalog
            .resolve("https://maven.minecraftforge.net/net/x.jar?ts=1")
            .expect("resolved");
        assert_eq!(
            resolved.candidates[0].location,
            "https://bmclapi2.bangbang93.com/maven/net/x.jar?ts=1"
        );
    }

    #[test]
    fn longest_upstream_prefix_wins() {
        let upstreams = vec![
            upstream("short", "https://a.example/libraries"),
            upstream("long", "https://a.example/libraries/snapshots"),
        ];
        let mut catalog = Catalog::with_mirrors(
            upstreams,
            vec![
                Source::new(SourceId::new("mirror").expect("id"), "镜像")
                    .with(binding("short", "https://m.example/l"))
                    .with(binding("long", "https://m.example/ls")),
            ],
        )
        .expect("catalog");
        let resolved = catalog
            .resolve("https://a.example/libraries/snapshots/x.jar")
            .expect("resolved");
        assert_eq!(resolved.upstream_key(), "long");
        let mirror = resolved
            .candidates
            .iter()
            .find(|candidate| candidate.source.as_str() == "mirror")
            .expect("mirror candidate");
        assert_eq!(mirror.location, "https://m.example/ls/x.jar");
        // 换个顺序再插一次不影响结果：命中的是更长的那一面
        catalog.insert(Source::new(SourceId::new("mirror").expect("id"), "镜像"));
        let resolved = catalog
            .resolve("https://a.example/libraries/snapshots/x.jar")
            .expect("resolved");
        assert_eq!(resolved.upstream_key(), "long");
    }

    #[test]
    fn resolve_dedupes_identical_candidate_locations() {
        let upstreams = vec![upstream("maven", "https://a.example/maven")];
        let catalog = Catalog::new(
            upstreams.clone(),
            vec![
                Source::official(&upstreams),
                Source::new(SourceId::new("same").expect("id"), "同址")
                    .with(binding("maven", "https://a.example/maven")),
            ],
        )
        .expect("catalog");
        let resolved = catalog
            .resolve("https://a.example/maven/x.jar")
            .expect("resolved");
        assert_eq!(resolved.candidates.len(), 1);
        assert!(resolved.candidates[0].source.is_official());
    }

    #[test]
    fn source_without_binding_is_not_a_candidate() {
        let upstreams = vec![upstream("maven", "https://a.example/maven")];
        let mut catalog =
            Catalog::new(upstreams.clone(), vec![Source::official(&upstreams)]).expect("catalog");
        catalog.insert(
            Source::new(SourceId::new("other").expect("id"), "别处")
                .with(binding("elsewhere", "https://b.example")),
        );
        // 未声明的上游面在构造时就被拒；这里确认「不绑定即不出现」
        let resolved = catalog
            .resolve("https://a.example/maven/x.jar")
            .expect("resolved");
        assert_eq!(resolved.candidates.len(), 1);
    }

    #[test]
    fn catalog_rejects_missing_official_and_unknown_upstream() {
        let upstreams = vec![upstream("maven", "https://a.example/maven")];
        assert!(matches!(
            Catalog::new(
                upstreams.clone(),
                vec![Source::new(SourceId::new("m").expect("id"), "m")]
            ),
            Err(CatalogError::NoOfficial)
        ));
        assert!(matches!(
            Catalog::new(
                upstreams.clone(),
                vec![
                    Source::official(&upstreams),
                    Source::new(SourceId::new("m").expect("id"), "m")
                        .with(binding("nowhere", "https://b.example"))
                ]
            ),
            Err(CatalogError::UnknownUpstream { .. })
        ));
    }

    #[test]
    fn catalog_insert_replaces_same_id() {
        let mut catalog = catalog();
        let official = Source::official(catalog.upstreams())
            .with(binding("forge-maven", "https://override.example/"));
        catalog.insert(official);
        let resolved = catalog
            .resolve("https://maven.minecraftforge.net/x.jar")
            .expect("resolved");
        let official = resolved
            .candidates
            .iter()
            .find(|candidate| candidate.source.is_official())
            .expect("official candidate");
        assert_eq!(official.location, "https://override.example/x.jar");
        assert_eq!(
            catalog
                .sources()
                .iter()
                .filter(|source| source.id().is_official())
                .count(),
            1
        );
    }

    #[test]
    fn select_picks_fastest_and_breaks_ties_by_order() {
        let order = vec![
            SourceId::official(),
            SourceId::new("bmclapi").expect("id"),
            SourceId::new("mcim").expect("id"),
        ];
        assert_eq!(
            select(
                &order,
                &[
                    sample("official", 300),
                    sample("bmclapi", 40),
                    sample("mcim", 120)
                ]
            ),
            Some(SourceId::new("bmclapi").expect("id"))
        );
        // 并列：目录里靠前的胜出
        assert_eq!(
            select(&order, &[sample("mcim", 40), sample("official", 40)]),
            Some(SourceId::official())
        );
        assert_eq!(select(&order, &[]), None);
    }

    #[test]
    fn preferred_moves_choice_to_front() {
        let candidates = vec![
            Candidate {
                source: SourceId::official(),
                location: "https://a.example/x.jar".to_owned(),
            },
            Candidate {
                source: SourceId::new("bmclapi").expect("id"),
                location: "https://m.example/x.jar".to_owned(),
            },
        ];
        let ordered = preferred(&candidates, Some(&SourceId::new("bmclapi").expect("id")));
        assert_eq!(ordered[0].location, "https://m.example/x.jar");
        assert_eq!(ordered[1].location, "https://a.example/x.jar");
        // 未知选择或未选择时保持原顺序
        assert_eq!(preferred(&candidates, None), candidates);
        assert_eq!(
            preferred(&candidates, Some(&SourceId::new("absent").expect("id"))),
            candidates
        );
    }
}
