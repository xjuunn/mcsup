//! 模组 jar 的能力判定模型
//!
//! 一个 mod jar 能回答的问题分三档可靠度，判定规则全部收敛在这里：
//!
//! - **确定**：加载器类型、依赖关系、Java 编译版本（class 文件头字节）
//! - **高**：目标 MC 版本区间（元数据声明）、显式 `side` 声明
//! - **推断**：客户端 / 服务端归属。这一档**永远给三态**，判不出就是
//!   [`SideSupport::Unknown`]，不允许为了「好看」猜成 required
//!
//! # 为什么客/服判定必须三态
//!
//! 实测 Fabric 生态里 `environment` 字段几乎全是 `*`（某整合包 14 个 mod
//! 全为 `*`），这个字段实际不携带信息。同时整合包作者常把仅客户端 mod 在
//! 清单里标成 `server: required`。若把「判不出」压成二元结论，就会重演
//! 「装上客户端 mod 导致开服崩溃」这个本模块要解决的问题。
//!
//! 因此 [`SideSupport`] 有四档而不是两档，[`Risk`] 把 `Unknown` 单列为
//! `Review` 而不是 `Ok`。
//!
//! # 模块名
//!
//! `mod` 是 Rust 关键字，文件与模块名统一用 `mod_`，对外经 [`crate::mod_`]
//! 暴露。

use std::fmt;

/// 加载器 / 生态归属
///
/// 由归档内的元数据文件判定，不是用户输入。同一 jar 可能同时支持多种
/// （Forge + `NeoForge` 双支持 mod 会在 `META-INF/` 下同时放两份 toml）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum ModLoader {
    /// `fabric.mod.json`
    Fabric,
    /// `quilt.mod.json`
    Quilt,
    /// `META-INF/mods.toml`
    Forge,
    /// `META-INF/neoforge.mods.toml`
    NeoForge,
    /// `mcmod.info`（1.12 及更早）
    LegacyForge,
    /// `litemod.json`
    LiteLoader,
    /// `plugin.yml` / `paper-plugin.yml`
    Bukkit,
    /// 只含 `assets/` + `pack.mcmeta`：资源包
    ResourcePack,
    /// 只含 `data/` + `pack.mcmeta`：数据包
    DataPack,
}

impl ModLoader {
    /// 稳定机器名（CLI 参数与 JSON 字段）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fabric => "fabric",
            Self::Quilt => "quilt",
            Self::Forge => "forge",
            Self::NeoForge => "neoforge",
            Self::LegacyForge => "legacy-forge",
            Self::LiteLoader => "liteloader",
            Self::Bukkit => "bukkit",
            Self::ResourcePack => "resourcepack",
            Self::DataPack => "datapack",
        }
    }

    /// 该加载器是否是「模组加载器」（区别于插件 / 资源包）
    #[must_use]
    pub const fn is_mod_loader(self) -> bool {
        matches!(
            self,
            Self::Fabric
                | Self::Quilt
                | Self::Forge
                | Self::NeoForge
                | Self::LegacyForge
                | Self::LiteLoader
        )
    }
}

impl fmt::Display for ModLoader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 某一侧的支持情况
///
/// 四档而非两档：`Unknown` 是「判不出」，与 `Optional`（声明为可选）
/// 是完全不同的两件事，不能合并。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SideSupport {
    /// 该侧必需
    Required,
    /// 该侧可选（装上无害）
    Optional,
    /// 该侧明确不支持
    Unsupported,
    /// 判不出：元数据未声明，或声明了但不携带信息
    #[default]
    Unknown,
}

impl SideSupport {
    /// 稳定机器名
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Optional => "optional",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }

    /// 该侧是否装上无害
    ///
    /// `Unknown` 返回 `true`：判不出时宁可多装（误删真正的服务端模组
    /// 比多装一个的风险更高），由 [`Risk::Review`] 提示人工确认。
    #[must_use]
    pub const fn installable(self) -> bool {
        !matches!(self, Self::Unsupported)
    }
}

impl fmt::Display for SideSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 结论的可靠度
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Confidence {
    /// 元数据显式声明
    Certain,
    /// 强信号（如入口点缺失某侧且只有另一侧）
    High,
    /// 弱信号（启发式）
    Medium,
    /// 不足以支撑结论
    Low,
    /// 无任何信号
    #[default]
    None,
}

impl Confidence {
    /// 稳定机器名
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Certain => "certain",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::None => "none",
        }
    }
}

impl fmt::Display for Confidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条判定依据的来源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EvidenceSource {
    /// 加载器元数据文件（`fabric.mod.json` / `mods.toml` / ...）
    Manifest,
    /// 归档清单（文件路径、MANIFEST.MF）
    Archive,
    /// mixin 配置的 `client` / `server` 分段
    MixinConfig,
    /// 字节码常量池
    Bytecode,
    /// 上游 API（Modrinth）
    Upstream,
    /// 整合包清单的 `env` 字段
    ModpackIndex,
}

impl EvidenceSource {
    /// 稳定机器名
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Archive => "archive",
            Self::MixinConfig => "mixin-config",
            Self::Bytecode => "bytecode",
            Self::Upstream => "upstream",
            Self::ModpackIndex => "modpack-index",
        }
    }
}

impl fmt::Display for EvidenceSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条判定依据
///
/// 报告里的每个结论都要能追溯到具体依据：用户不接受「工具说它是客户端
/// mod」这种没有理由的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// 依据来源
    pub source: EvidenceSource,
    /// 人话说明（面向用户，不做机器解析）
    pub detail: String,
}

impl Evidence {
    /// 构造一条依据
    #[must_use]
    pub fn new(source: EvidenceSource, detail: impl Into<String>) -> Self {
        Self {
            source,
            detail: detail.into(),
        }
    }
}

/// 客 / 服归属的完整结论
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SideVerdict {
    /// 客户端侧
    pub client: SideSupport,
    /// 服务端侧
    pub server: SideSupport,
    /// 结论可靠度
    pub confidence: Confidence,
    /// 支撑依据
    pub evidence: Vec<Evidence>,
}

impl SideVerdict {
    /// 两侧都判不出
    #[must_use]
    pub fn unknown() -> Self {
        Self::default()
    }

    /// 由显式声明构造（元数据直接说了 client / server / both）
    #[must_use]
    pub fn declared(client: SideSupport, server: SideSupport) -> Self {
        Self {
            client,
            server,
            confidence: Confidence::Certain,
            evidence: Vec::new(),
        }
    }

    /// 追加一条依据
    #[must_use]
    pub fn with_evidence(mut self, evidence: Evidence) -> Self {
        self.evidence.push(evidence);
        self
    }

    /// 是否为「仅客户端」
    #[must_use]
    pub fn is_client_only(&self) -> bool {
        self.server == SideSupport::Unsupported
    }
}

/// 一个 MC 版本区间
///
/// 元数据里的写法五花八门（`1.21.1`、`>=1.21 <1.22`、`~1.21.1`、
/// `[1.21,1.22)`），统一解析成下界 + 上界。`None` 表示该端不设限。
///
/// # 为什么不用 semver 库
///
/// Minecraft 版本号不是 semver：`1.21` 与 `1.21.1` 是**不同的两个版本**，
/// 而 semver 认为 `1.21.0 == 1.21`。用 semver 比较会把 `1.21` 判成
/// 满足 `>=1.21.1`，进而把不兼容的 mod 判成兼容。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VersionRange {
    /// 原始写法（原样回显，便于用户核对）
    pub raw: String,
    /// 下界（含）
    pub min: Option<String>,
    /// 上界（不含）
    pub max: Option<String>,
}

impl VersionRange {
    /// 不设限
    #[must_use]
    pub fn any() -> Self {
        Self::default()
    }

    /// 单一版本
    #[must_use]
    pub fn exact(version: impl Into<String>) -> Self {
        let v = version.into();
        Self {
            raw: v.clone(),
            min: Some(v),
            max: None,
        }
    }

    /// 是否不设限
    #[must_use]
    pub fn is_any(&self) -> bool {
        self.min.is_none() && self.max.is_none()
    }

    /// 给定 MC 版本是否落在区间内
    ///
    /// 用 Minecraft 自己的比较口径（见 [`compare_mc_version`]），不套 semver。
    #[must_use]
    pub fn contains(&self, mc: &str) -> bool {
        if let Some(min) = &self.min
            && compare_mc_version(mc, min) == std::cmp::Ordering::Less
        {
            return false;
        }
        if let Some(max) = &self.max
            && compare_mc_version(mc, max) != std::cmp::Ordering::Less
        {
            return false;
        }
        true
    }
}

/// 由 class 文件大版本号换算 Java 大版本
///
/// class 文件头的 major 与 Java 版本的关系是常量偏移：`major - 44`。
/// 例如 52 → Java 8，61 → Java 17，65 → Java 21。
///
/// 直接展示 class major（65）对用户没有意义：用户说的是「需要 Java 21」。
///
/// # 为什么放在领域层
///
/// 这是「这个 mod 需要什么 Java」的判定，属于领域知识，不是读取细节。
/// [`ModCapability::java_compiled`] 的语义就是 Java 大版本，读取方必须
/// 经由此函数换算后再填入。
#[must_use]
pub const fn java_version_from_class_major(major: u32) -> Option<u32> {
    // 45 = Java 1.1 起的起点；低于它的不是有效 class 版本
    if major < 45 {
        return None;
    }
    Some(major - 44)
}

/// 按 Minecraft 版本口径比较两个版本号
///
/// 规则：按 `.` 切分成数字段逐段比较，缺失段补 0 参与比较**但不等价**。
/// 具体说 `1.21` < `1.21.1`（缺的段当 0，`0 < 1`），而 `1.21` 与
/// `1.21.0` 视为相等——这与 Mojang 的实际发版习惯一致。
///
/// 含 `-pre` / `-rc` 等后缀的版本按「去掉后缀比较数字部分、同数字时正式版
/// 更大」处理：`1.21-pre1` < `1.21`。
#[must_use]
pub fn compare_mc_version(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    fn split(raw: &str) -> (Vec<u64>, bool) {
        // 后缀（-pre1 / -rc2 / -snapshot）标记为预发布
        let (numeric, has_suffix) = match raw.split_once('-') {
            Some((head, _)) => (head, true),
            None => (raw, false),
        };
        let parts = numeric
            .split('.')
            .map(|seg| seg.parse::<u64>().unwrap_or(0))
            .collect();
        (parts, has_suffix)
    }

    let (an, apre) = split(a);
    let (bn, bpre) = split(b);
    let len = an.len().max(bn.len());
    for i in 0..len {
        let av = an.get(i).copied().unwrap_or(0);
        let bv = bn.get(i).copied().unwrap_or(0);
        match av.cmp(&bv) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    // 数字部分相同：正式版 > 预发布
    match (apre, bpre) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        _ => Ordering::Equal,
    }
}

/// 一条依赖声明
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModDependency {
    /// 依赖的 modid（`minecraft` / `java` / `fabric-api` / ...）
    pub id: String,
    /// 版本要求原文（可能为空）
    pub version_range: String,
    /// 是否必需（`false` = recommends / suggests）
    pub mandatory: bool,
}

/// 风险等级
///
/// 判定规则在本模块（[`Risk::of`]），入口只负责渲染。三档对应三种处置：
/// `Blocking` 直接建议排除，`Review` 默认仍装但提示人工确认，`Ok` 无需关注。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// 无风险
    Ok,
    /// 需要人工确认（判不出客/服归属，或存在非致命疑点）
    Review,
    /// 明确不可用于服务端
    Blocking,
}

impl Risk {
    /// 稳定机器名
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Review => "review",
            Self::Blocking => "blocking",
        }
    }

    /// 由客/服结论推出风险
    ///
    /// `Unsupported` → `Blocking`；`Unknown` → `Review`；其余 `Ok`。
    /// 「判不出」绝不落到 `Ok`：那会让用户以为检查过了。
    #[must_use]
    pub fn of(side: &SideVerdict) -> Self {
        match side.server {
            SideSupport::Unsupported => Self::Blocking,
            SideSupport::Unknown => Self::Review,
            SideSupport::Required | SideSupport::Optional => Self::Ok,
        }
    }

    /// 取两个风险里更严重的
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        if other > self { other } else { self }
    }
}

impl fmt::Display for Risk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一个 mod jar 的完整检测结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModCapability {
    /// 归档路径（展示用，判定不依赖它）
    pub path: String,
    /// modid（元数据给出时）
    pub id: Option<String>,
    /// 显示名
    pub name: Option<String>,
    /// mod 自身版本
    pub version: Option<String>,
    /// 加载器归属（可能多个）
    pub loaders: Vec<ModLoader>,
    /// 目标 MC 版本区间
    pub mc_versions: VersionRange,
    /// 元数据声明的 Java 大版本
    pub java_required: Option<u32>,
    /// 字节码实测的 Java 大版本（取所有 class 的最大值）
    pub java_compiled: Option<u32>,
    /// 客 / 服结论
    pub side: SideVerdict,
    /// 必需与可选依赖
    pub depends: Vec<ModDependency>,
    /// 声明不兼容的 modid
    pub breaks: Vec<String>,
    /// 内嵌的嵌套 jar 数量（`META-INF/jars/`）
    pub nested_jars: usize,
    /// 本 mod 同时提供的别名 modid
    ///
    /// Fabric 的 `provides` 字段：一个 jar 可以用多个 modid 被依赖。实测
    /// `balm` 声明 `provides: ["balm-fabric"]`，而依赖方写的是
    /// `balm-fabric`——不读这个字段就会把已满足的依赖误报成缺失。
    pub provides: Vec<String>,
}

impl ModCapability {
    /// 空结果：只有路径，其余待填
    #[must_use]
    pub fn empty(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            id: None,
            name: None,
            version: None,
            loaders: Vec::new(),
            mc_versions: VersionRange::any(),
            java_required: None,
            java_compiled: None,
            side: SideVerdict::unknown(),
            depends: Vec::new(),
            breaks: Vec::new(),
            nested_jars: 0,
            provides: Vec::new(),
        }
    }

    /// 综合风险等级
    ///
    /// 客/服结论是主判据；此外「必需依赖缺失」由调用方用
    /// [`Self::missing_mandatory_deps`] 另行判定并叠加。
    #[must_use]
    pub fn risk(&self) -> Risk {
        let base = Risk::of(&self.side);
        // 没有任何模组加载器元数据：可能是资源包 / 数据包 / 损坏文件，
        // 也可能只是我们认不出。判不出就降级为待确认，而不是放行。
        if self.loaders.is_empty() && base == Risk::Ok {
            return Risk::Review;
        }
        base
    }

    /// 本 mod 能提供的全部 modid（自身 id + 别名）
    ///
    /// 依赖比对应基于这个集合，而不是只看 `id`。
    #[must_use]
    pub fn provided_ids(&self) -> Vec<String> {
        let mut out: Vec<String> = self.provides.clone();
        if let Some(id) = &self.id
            && !out.iter().any(|p| p.eq_ignore_ascii_case(id))
        {
            out.push(id.clone());
        }
        out
    }

    /// 在给定已有 modid 集合下，列出缺失的必需依赖
    ///
    /// 内置项（`minecraft` / `java` / 各加载器自身）不参与比对：它们由服务端
    /// 与加载器提供，不在 `mods/` 里。
    #[must_use]
    pub fn missing_mandatory_deps(&self, present: &[String]) -> Vec<String> {
        self.depends
            .iter()
            .filter(|dep| dep.mandatory && !is_builtin_dep(&dep.id))
            .filter(|dep| !present.iter().any(|id| id.eq_ignore_ascii_case(&dep.id)))
            .map(|dep| dep.id.clone())
            .collect()
    }
}

/// 加载器与游戏自身提供的依赖，不要求在 `mods/` 里存在
fn is_builtin_dep(id: &str) -> bool {
    matches!(
        id.to_ascii_lowercase().as_str(),
        "minecraft"
            | "java"
            | "fabricloader"
            | "fabric-loader"
            | "fabric-api"
            | "forge"
            | "neoforge"
            | "quilt_loader"
            | "quiltloader"
            | "quilted_fabric_api"
            | "minecraftforge"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mc_version_ordering_treats_missing_segment_as_zero() {
        use std::cmp::Ordering;
        assert_eq!(compare_mc_version("1.21", "1.21.1"), Ordering::Less);
        assert_eq!(compare_mc_version("1.21.1", "1.21"), Ordering::Greater);
        assert_eq!(compare_mc_version("1.21", "1.21.0"), Ordering::Equal);
        assert_eq!(compare_mc_version("1.20.5", "1.21"), Ordering::Less);
        assert_eq!(compare_mc_version("1.9", "1.10"), Ordering::Less);
    }

    #[test]
    fn prerelease_is_less_than_release() {
        use std::cmp::Ordering;
        assert_eq!(compare_mc_version("1.21-pre1", "1.21"), Ordering::Less);
        assert_eq!(compare_mc_version("1.21", "1.21-pre1"), Ordering::Greater);
        assert_eq!(compare_mc_version("1.21-rc1", "1.21"), Ordering::Less);
    }

    #[test]
    fn range_contains_uses_mc_ordering() {
        let exact = VersionRange::exact("1.21.1");
        assert!(exact.contains("1.21.1"));
        // 关键回归：1.21 不等于 1.21.1
        assert!(!exact.contains("1.21"));

        let ranged = VersionRange {
            raw: ">=1.21 <1.22".to_owned(),
            min: Some("1.21".to_owned()),
            max: Some("1.22".to_owned()),
        };
        assert!(ranged.contains("1.21"));
        assert!(ranged.contains("1.21.1"));
        assert!(!ranged.contains("1.22"));
        assert!(!ranged.contains("1.20.6"));
    }

    #[test]
    fn any_range_contains_everything() {
        assert!(VersionRange::any().contains("1.21.1"));
        assert!(VersionRange::any().is_any());
    }

    #[test]
    fn client_only_maps_to_blocking() {
        let side = SideVerdict::declared(SideSupport::Required, SideSupport::Unsupported);
        assert_eq!(Risk::of(&side), Risk::Blocking);
    }

    #[test]
    fn unknown_server_side_is_review_not_ok() {
        // 这是本模块存在的理由：判不出不能算「没问题」
        let side = SideVerdict::declared(SideSupport::Required, SideSupport::Unknown);
        assert_eq!(Risk::of(&side), Risk::Review);
    }

    #[test]
    fn both_sides_required_is_ok() {
        let side = SideVerdict::declared(SideSupport::Required, SideSupport::Required);
        assert_eq!(Risk::of(&side), Risk::Ok);
    }

    #[test]
    fn unknown_side_is_installable() {
        // 判不出时宁可多装
        assert!(SideSupport::Unknown.installable());
        assert!(SideSupport::Optional.installable());
        assert!(!SideSupport::Unsupported.installable());
    }

    #[test]
    fn missing_deps_ignore_builtins() {
        let mut cap = ModCapability::empty("a.jar");
        cap.depends = vec![
            ModDependency {
                id: "minecraft".to_owned(),
                version_range: "1.21.1".to_owned(),
                mandatory: true,
            },
            ModDependency {
                id: "fabric-api".to_owned(),
                version_range: String::new(),
                mandatory: true,
            },
            ModDependency {
                id: "cobblemon".to_owned(),
                version_range: String::new(),
                mandatory: true,
            },
            ModDependency {
                id: "sodium".to_owned(),
                version_range: String::new(),
                mandatory: false,
            },
        ];
        let missing = cap.missing_mandatory_deps(&[]);
        // minecraft / fabric-api 是内置的，sodium 是可选的
        assert_eq!(missing, vec!["cobblemon"]);
    }

    #[test]
    fn provides_alias_satisfies_dependency() {
        // 实测形态：balm 声明 provides: ["balm-fabric"]，waystones 依赖 balm-fabric
        let mut balm = ModCapability::empty("balm.jar");
        balm.id = Some("balm".to_owned());
        balm.provides = vec!["balm-fabric".to_owned()];
        assert_eq!(balm.provided_ids(), vec!["balm-fabric", "balm"]);

        let mut waystones = ModCapability::empty("waystones.jar");
        waystones.id = Some("waystones".to_owned());
        waystones.depends = vec![ModDependency {
            id: "balm-fabric".to_owned(),
            version_range: "*".to_owned(),
            mandatory: true,
        }];
        let present = balm.provided_ids();
        assert!(waystones.missing_mandatory_deps(&present).is_empty());
    }

    #[test]
    fn provided_ids_dedupes_own_id() {
        let mut c = ModCapability::empty("a.jar");
        c.id = Some("a".to_owned());
        c.provides = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(c.provided_ids(), vec!["a", "b"]);
    }

    #[test]
    fn missing_deps_match_case_insensitively() {
        let mut cap = ModCapability::empty("a.jar");
        cap.depends = vec![ModDependency {
            id: "Cobblemon".to_owned(),
            version_range: String::new(),
            mandatory: true,
        }];
        assert!(
            cap.missing_mandatory_deps(&["cobblemon".to_owned()])
                .is_empty()
        );
    }

    #[test]
    fn no_loader_metadata_is_at_least_review() {
        let mut cap = ModCapability::empty("a.jar");
        cap.side = SideVerdict::declared(SideSupport::Required, SideSupport::Required);
        // 认得两侧都必需，但认不出加载器 → 仍需确认
        assert_eq!(cap.risk(), Risk::Review);
    }

    #[test]
    fn class_major_maps_to_java_version() {
        assert_eq!(java_version_from_class_major(52), Some(8));
        assert_eq!(java_version_from_class_major(55), Some(11));
        assert_eq!(java_version_from_class_major(61), Some(17));
        assert_eq!(java_version_from_class_major(65), Some(21));
        // 无效版本号
        assert_eq!(java_version_from_class_major(44), None);
        assert_eq!(java_version_from_class_major(0), None);
    }

    #[test]
    fn risk_max_picks_worse() {
        assert_eq!(Risk::Ok.max(Risk::Blocking), Risk::Blocking);
        assert_eq!(Risk::Review.max(Risk::Ok), Risk::Review);
        assert_eq!(Risk::Ok.max(Risk::Ok), Risk::Ok);
    }
}
