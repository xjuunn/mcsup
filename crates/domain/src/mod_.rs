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

    /// 从用户输入的加载器名解析（`--loader`）
    ///
    /// 接受 [`Self::as_str`] 的稳定名，外加几个口语别名：`paper` / `spigot`
    /// 归到 Bukkit（服务端插件），`neoforge.mods.toml` 的旧写法 `neo` 也认。
    /// 解析不了返回 `None`，由入口报用法错误而不是猜一个。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let key = raw.trim().to_ascii_lowercase().replace('_', "-");
        Some(match key.as_str() {
            "fabric" => Self::Fabric,
            "quilt" => Self::Quilt,
            "forge" => Self::Forge,
            "neo" | "neoforge" => Self::NeoForge,
            "legacy-forge" | "legacyforge" => Self::LegacyForge,
            "liteloader" => Self::LiteLoader,
            "bukkit" | "paper" | "spigot" | "purpur" => Self::Bukkit,
            "resourcepack" | "resource-pack" => Self::ResourcePack,
            "datapack" | "data-pack" => Self::DataPack,
            _ => return None,
        })
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
///
/// 依赖**按声明它的加载器归类**：一个 jar 可以同时带 `fabric.mod.json` 与
/// `quilt.mod.json`，两份清单的依赖各自只在自己的加载器下生效。把不同加载器
/// 的依赖合并成一张表会造出假缺失——实测 `lukis-*` 系列在 Quilt 清单里写
/// `quilt_resource_loader`（且带 `unless` 豁免），Fabric 服务端根本没有这个
/// 概念，合并后就会误判成「不可用于服务端」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModDependency {
    /// 依赖的 modid（`minecraft` / `java` / `fabric-api` / ...）
    pub id: String,
    /// 版本要求原文（可能为空）
    pub version_range: String,
    /// 是否必需（`false` = recommends / suggests）
    pub mandatory: bool,
    /// 声明这条依赖的加载器；`None` 表示与加载器无关
    pub loader: Option<ModLoader>,
    /// 条件豁免：`present` 中任一 modid 存在时，本依赖不再必需
    ///
    /// 对应 Quilt 的 `unless` / `unless_any`：`{"id": "quilt_resource_loader",
    /// "unless": "fabric-resource-loader-v0"}` 在 Fabric 服务端由 fabric-api
    /// 满足，不应报缺失。
    pub unless: Vec<String>,
}

impl ModDependency {
    /// 构造一条不带加载器归属、无豁免的依赖
    #[must_use]
    pub fn new(id: impl Into<String>, version_range: impl Into<String>, mandatory: bool) -> Self {
        Self {
            id: id.into(),
            version_range: version_range.into(),
            mandatory,
            loader: None,
            unless: Vec::new(),
        }
    }

    /// 标注声明它的加载器
    #[must_use]
    pub fn with_loader(mut self, loader: ModLoader) -> Self {
        self.loader = Some(loader);
        self
    }

    /// 设定条件豁免（任一存在即不需本依赖）
    #[must_use]
    pub fn unless_any(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.unless.extend(ids);
        self
    }

    /// 在给定「已存在 modid」集合与目标加载器下，本依赖是否成立
    ///
    /// `target` 为 `None`（未确定目标加载器）时，带加载器归属的依赖一律**不**
    /// 参与判定：没有目标就无从说它该被哪个加载器满足，报出来只会是噪声。
    #[must_use]
    pub fn applies(&self, present: &[String], target: Option<ModLoader>) -> bool {
        if !self.mandatory || is_builtin_dep(&self.id) {
            return false;
        }
        match (self.loader, target) {
            (Some(loader), Some(target)) if loader != target => return false,
            (Some(_), None) => return false,
            _ => {}
        }
        if self.unless.iter().any(|id| present_has(present, id)) {
            return false;
        }
        !present_has(present, &self.id)
    }
}

/// 已存在集合里是否含某个 modid（大小写不敏感）
fn present_has(present: &[String], id: &str) -> bool {
    present.iter().any(|have| have.eq_ignore_ascii_case(id))
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
    /// 只比对本 mod 声明的依赖与「整批文件能提供的 modid」：
    ///
    /// - 内置项（`minecraft` / `java` / 各加载器自身）不参与比对，它们由服务端
    ///   与加载器提供，不在 `mods/` 里
    /// - 依赖按声明它的加载器归类，`target` 决定采信哪一份清单（见
    ///   [`ModDependency::applies`]）；未确定目标加载器时不拿带归属的依赖吓人
    /// - 命中 `unless` 豁免的依赖不算缺失
    #[must_use]
    pub fn missing_mandatory_deps(
        &self,
        present: &[String],
        target: Option<ModLoader>,
    ) -> Vec<String> {
        self.depends
            .iter()
            .filter(|dep| dep.applies(present, target))
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

/// 安装一个 jar 之前应当如何处置
///
/// 判定只在 `domain` 一处收敛：若入口各自决定「`Blocking` 到底拦不拦」，CLI
/// 与 API 迟早给出两种结论。`--force` 的含义也由这里的 `allow_blocking` 定义，
/// 不是各命令自己解释。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallVerdict {
    /// 可以直接落盘
    Allow,
    /// 可以落盘，但必须把风险提示给用户
    Review,
    /// 拒绝落盘（除非用户显式允许）
    Refuse,
}

impl InstallVerdict {
    /// 由风险等级与「用户是否已显式允许装 `Blocking`」推出处置
    ///
    /// `Blocking` + 允许 → 仍然只降级成 [`Self::Review`]：装了也要留提示，
    /// 不能让 `--force` 把「明确不可用于服务端」这件事从输出里抹掉。
    #[must_use]
    pub fn of(risk: Risk, allow_blocking: bool) -> Self {
        match risk {
            Risk::Ok => Self::Allow,
            Risk::Review => Self::Review,
            Risk::Blocking if allow_blocking => Self::Review,
            Risk::Blocking => Self::Refuse,
        }
    }

    /// 是否允许落盘
    #[must_use]
    pub fn is_allowed(self) -> bool {
        !matches!(self, Self::Refuse)
    }
}

/// 目标服务端的安装条件
///
/// 三个字段都是 `Option`，`None` 表示这一维**不参与判定**——`mod inspect`
/// 检查任意 jar 时并不知道要装到哪台服务端。两件事必须分开：
///
/// - 字段为 `None`：这一维没问
/// - [`Fit::Unknown`]：问了，但文件没给出可判定的证据
///
/// 混为一谈会把「没问」渲染成「有问题」，用户看到一片「待确认」就再也不看
/// 这一列了。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerTarget {
    /// 目标加载器（由服务端变体换算）
    pub loader: Option<ModLoader>,
    /// 目标 MC 版本（服务端实际跑的版本）
    pub mc_version: Option<String>,
    /// 目标 Java 大版本（服务端实际会用的那个）
    pub java_major: Option<u32>,
}

impl ServerTarget {
    /// 什么都不问
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// 是否一维都没问
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.loader.is_none() && self.mc_version.is_none() && self.java_major.is_none()
    }
}

/// 单个维度的匹配结论
///
/// 与 [`SideSupport`] 同样不压缩成二元：`Unknown` 是「判不出」，不是
/// 「不匹配」。把判不出当成不匹配会让工具开始骗人。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// 满足
    Match,
    /// 明确不满足
    Mismatch,
    /// 判不出
    #[default]
    Unknown,
}

impl Fit {
    /// 稳定机器名
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Mismatch => "mismatch",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for Fit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 参与匹配判定的维度
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FitDimension {
    /// 加载器
    Loader,
    /// MC 版本
    McVersion,
    /// Java 大版本
    Java,
}

impl FitDimension {
    /// 稳定机器名
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Loader => "loader",
            Self::McVersion => "mc_version",
            Self::Java => "java",
        }
    }
}

impl fmt::Display for FitDimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条匹配说明
///
/// `Mismatch` 与「有信息量的 `Unknown`」各一条：用户要的是「为什么不行」和
/// 「差什么才能判」，不是三个字母的枚举。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FitIssue {
    /// 哪个维度
    pub dimension: FitDimension,
    /// 人话说明
    pub detail: String,
}

impl FitIssue {
    /// 构造一条
    #[must_use]
    pub fn new(dimension: FitDimension, detail: impl Into<String>) -> Self {
        Self {
            dimension,
            detail: detail.into(),
        }
    }
}

/// 「这个文件能不能装到这台服务端」的总结论
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitVerdict {
    /// 三个维度都满足
    Installable,
    /// 没有不匹配，但有维度判不出
    Unclear,
    /// 至少一个维度明确不满足
    Rejected,
}

impl FitVerdict {
    /// 稳定机器名
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Installable => "installable",
            Self::Unclear => "unclear",
            Self::Rejected => "rejected",
        }
    }
}

impl fmt::Display for FitVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 文件与目标服务端的匹配结论
///
/// # 与 [`Risk`] 的分工
///
/// `Risk` 回答「这个 jar 本身是不是服务端的东西」（客/服归属、依赖闭包），
/// 与装到哪台服务端无关；`ServerFit` 回答「它和这台服务端对不对得上」
/// （加载器、MC 版本、Java）。同一份文件在不同服务端上的 `Fit` 不同，
/// 但 `Risk` 不变——两者合并成一个数就没法分别展示，也无法解释。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFit {
    /// 加载器维度
    pub loader: Fit,
    /// MC 版本维度
    pub mc_version: Fit,
    /// Java 维度
    pub java: Fit,
    /// 总结论
    pub verdict: FitVerdict,
    /// 逐条说明（不匹配的必有一条；判不出的看情况给）
    pub issues: Vec<FitIssue>,
}

impl ModCapability {
    /// 判定本文件能否装到 `target` 描述的服务端
    ///
    /// 判定口径：
    ///
    /// - **加载器**：文件声明的加载器里有目标加载器即满足。`Quilt` 运行时
    ///   保留 Fabric 兼容层，因此 `Quilt` 目标接受 Fabric mod；反向不成立。
    ///   其余跨加载器兼容（`NeoForge` 1.20.1 的 Forge 血统等）版本相关，
    ///   不能一概而论，宁可不判。
    /// - **MC 版本**：文件声明的区间是否含目标版本。未声明区间就是判不出，
    ///   不按「大概能跑」放行。
    /// - **Java**：按 class 文件头实测的最大版本比。**只可能给出不匹配**：
    ///   没扫到 class（数据包、扫描预算用尽）说明不了任何问题，不该因此
    ///   把结论降成「待确认」，否则每个数据包都会变成待确认。
    #[must_use]
    pub fn fit_for(&self, target: &ServerTarget) -> ServerFit {
        let mut issues = Vec::new();

        let loader = match target.loader {
            None => Fit::Unknown,
            Some(want) => {
                if self.loaders.is_empty() {
                    issues.push(FitIssue::new(
                        FitDimension::Loader,
                        format!("jar 内没有可识别的加载器元数据，判不出是否支持 {want}"),
                    ));
                    Fit::Unknown
                } else if self.loaders.iter().any(|have| loads(want, *have)) {
                    Fit::Match
                } else {
                    let have: Vec<&str> = self.loaders.iter().map(|l| l.as_str()).collect();
                    issues.push(FitIssue::new(
                        FitDimension::Loader,
                        format!("只支持 {}，服务端是 {want}", have.join(" / ")),
                    ));
                    Fit::Mismatch
                }
            }
        };

        let mc_version = match (&target.mc_version, self.mc_versions.is_any()) {
            (None, _) => Fit::Unknown,
            (Some(_), true) => {
                issues.push(FitIssue::new(
                    FitDimension::McVersion,
                    "文件未声明 MC 版本区间，判不出是否兼容".to_owned(),
                ));
                Fit::Unknown
            }
            (Some(mc), false) => {
                if self.mc_versions.contains(mc) {
                    Fit::Match
                } else {
                    issues.push(FitIssue::new(
                        FitDimension::McVersion,
                        format!("声明区间 {}，服务端是 {mc}", self.mc_versions.raw),
                    ));
                    Fit::Mismatch
                }
            }
        };

        let java = match (target.java_major, self.java_compiled) {
            (Some(want), Some(compiled)) => {
                if compiled <= want {
                    Fit::Match
                } else {
                    issues.push(FitIssue::new(
                        FitDimension::Java,
                        format!("需要 Java {compiled}，服务端用 Java {want}"),
                    ));
                    Fit::Mismatch
                }
            }
            _ => Fit::Unknown,
        };

        // 只有「问了」的维度才可能让结论变成待确认：没问不等于有问题
        let unresolved = (target.loader.is_some() && loader == Fit::Unknown)
            || (target.mc_version.is_some() && mc_version == Fit::Unknown);
        let verdict =
            if loader == Fit::Mismatch || mc_version == Fit::Mismatch || java == Fit::Mismatch {
                FitVerdict::Rejected
            } else if unresolved {
                FitVerdict::Unclear
            } else {
                FitVerdict::Installable
            };

        ServerFit {
            loader,
            mc_version,
            java,
            verdict,
            issues,
        }
    }
}

/// `target` 加载器能否载入 `have` 声明的 mod
fn loads(target: ModLoader, have: ModLoader) -> bool {
    if target == have {
        return true;
    }
    // Quilt Loader 保留 Fabric 兼容层；反向不成立。
    // NeoForge 与 Forge 的兼容性随版本变化（1.20.1 是同一血统，1.20.2 起
    // 分道扬镳），无法用「加载器」这一维表达，因此不猜。
    matches!((target, have), (ModLoader::Quilt, ModLoader::Fabric))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_verdict_refuses_blocking_unless_allowed() {
        assert_eq!(InstallVerdict::of(Risk::Ok, false), InstallVerdict::Allow);
        assert_eq!(
            InstallVerdict::of(Risk::Review, false),
            InstallVerdict::Review
        );
        assert_eq!(
            InstallVerdict::of(Risk::Blocking, false),
            InstallVerdict::Refuse
        );
        // 强制放行不等于「无风险」：仍然要给用户看到提示
        assert_eq!(
            InstallVerdict::of(Risk::Blocking, true),
            InstallVerdict::Review
        );
        assert!(!InstallVerdict::of(Risk::Blocking, false).is_allowed());
        assert!(InstallVerdict::of(Risk::Blocking, true).is_allowed());
    }

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

    /// 依赖按加载器归类：只采信目标加载器那份清单
    ///
    /// 实测形态：`lukis-*` 同时带 fabric 与 quilt 清单，Quilt 侧声明
    /// `quilt_resource_loader`。Fabric 服务端不该因为这份清单报缺失。
    #[test]
    fn dependency_only_counts_for_target_loader() {
        let mut cap = ModCapability::empty("a.jar");
        cap.loaders = vec![ModLoader::Fabric, ModLoader::Quilt];
        cap.depends = vec![
            ModDependency::new("fabric-api", "", true).with_loader(ModLoader::Fabric),
            ModDependency::new("quilt_resource_loader", "", true).with_loader(ModLoader::Quilt),
        ];
        // fabric-api 是内置项，真正参与比对的是 quilt 那条
        assert_eq!(
            cap.missing_mandatory_deps(&[], Some(ModLoader::Quilt)),
            vec!["quilt_resource_loader"]
        );
        // Fabric 口径下不该拿 Quilt 的依赖算账
        assert!(
            cap.missing_mandatory_deps(&[], Some(ModLoader::Fabric))
                .is_empty()
        );
        // 目标未确定：带加载器归属的依赖一律不参与，避免制造假缺失
        assert!(cap.missing_mandatory_deps(&[], None).is_empty());
    }

    /// `unless` 命中时依赖不再必需（Quilt 的条件豁免）
    #[test]
    fn unless_suppresses_dependency() {
        let mut cap = ModCapability::empty("a.jar");
        cap.depends = vec![
            ModDependency::new("quilt_resource_loader", "*", true)
                .with_loader(ModLoader::Quilt)
                .unless_any(vec!["fabric-resource-loader-v0".to_owned()]),
        ];
        // 豁免项存在 → 不缺
        let present = vec!["fabric-resource-loader-v0".to_owned()];
        assert!(
            cap.missing_mandatory_deps(&present, Some(ModLoader::Quilt))
                .is_empty()
        );
        // 豁免项不存在 → 如实报缺失
        assert_eq!(
            cap.missing_mandatory_deps(&[], Some(ModLoader::Quilt)),
            vec!["quilt_resource_loader"]
        );
    }

    /// 与加载器无关的依赖（`loader = None`）在任何口径下都参与校验
    #[test]
    fn loader_agnostic_dependency_always_checked() {
        let mut cap = ModCapability::empty("a.jar");
        cap.depends = vec![ModDependency::new("cobblemon", "", true)];
        assert_eq!(cap.missing_mandatory_deps(&[], None), vec!["cobblemon"]);
        assert_eq!(
            cap.missing_mandatory_deps(&[], Some(ModLoader::Fabric)),
            vec!["cobblemon"]
        );
    }

    #[test]
    fn missing_deps_ignore_builtins() {
        let mut cap = ModCapability::empty("a.jar");
        cap.depends = vec![
            ModDependency::new("minecraft", "1.21.1", true),
            ModDependency::new("fabric-api", "", true),
            ModDependency::new("cobblemon", "", true),
            ModDependency::new("sodium", "", false),
        ];
        let missing = cap.missing_mandatory_deps(&[], None);
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
        waystones.depends = vec![ModDependency::new("balm-fabric", "*", true)];
        let present = balm.provided_ids();
        assert!(waystones.missing_mandatory_deps(&present, None).is_empty());
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
        cap.depends = vec![ModDependency::new("Cobblemon", "", true)];
        assert!(
            cap.missing_mandatory_deps(&["cobblemon".to_owned()], None)
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

    /// 构造一个只填了匹配判定相关字段的文件
    fn cap(loaders: &[ModLoader], mc: &str, java: Option<u32>) -> ModCapability {
        let mut cap = ModCapability::empty("/x.jar");
        cap.loaders = loaders.to_vec();
        cap.mc_versions = if mc.is_empty() {
            VersionRange::any()
        } else {
            exact_range(mc)
        };
        cap.java_compiled = java;
        cap
    }

    /// 单个版本的区间：`[v, 下一 patch)`
    ///
    /// 与 `modkit::parse_mc_range` 对裸版本号的口径一致。不能用
    /// [`VersionRange::exact`]：它只设下界，等于「v 及以上」，判不出上界。
    fn exact_range(v: &str) -> VersionRange {
        let mut parts: Vec<String> = v.split('.').map(ToOwned::to_owned).collect();
        if let Some(last) = parts.last_mut() {
            let bumped = last.parse::<u64>().unwrap_or(0) + 1;
            *last = bumped.to_string();
        }
        VersionRange {
            raw: v.to_owned(),
            min: Some(v.to_owned()),
            max: Some(parts.join(".")),
        }
    }

    fn target(loader: ModLoader, mc: &str, java: Option<u32>) -> ServerTarget {
        ServerTarget {
            loader: Some(loader),
            mc_version: Some(mc.to_owned()),
            java_major: java,
        }
    }

    #[test]
    fn fit_accepts_matching_loader_and_version() {
        let fit = cap(&[ModLoader::Fabric], "1.21.1", Some(21)).fit_for(&target(
            ModLoader::Fabric,
            "1.21.1",
            Some(21),
        ));
        assert_eq!(fit.loader, Fit::Match);
        assert_eq!(fit.mc_version, Fit::Match);
        assert_eq!(fit.java, Fit::Match);
        assert_eq!(fit.verdict, FitVerdict::Installable);
        assert!(fit.issues.is_empty());
    }

    #[test]
    fn fit_rejects_foreign_loader_with_reason() {
        let fit = cap(&[ModLoader::Forge], "1.21.1", None).fit_for(&target(
            ModLoader::Fabric,
            "1.21.1",
            None,
        ));
        assert_eq!(fit.loader, Fit::Mismatch);
        assert_eq!(fit.verdict, FitVerdict::Rejected);
        // 理由要能直接回答「为什么不行」
        assert!(
            fit.issues
                .iter()
                .any(|i| i.dimension == FitDimension::Loader
                    && i.detail.contains("forge")
                    && i.detail.contains("fabric")),
            "{:?}",
            fit.issues
        );
    }

    /// Quilt 运行时能载入 Fabric mod，反过来不行
    #[test]
    fn quilt_target_loads_fabric_mod_but_not_the_reverse() {
        let fabric_mod = cap(&[ModLoader::Fabric], "1.21.1", None);
        assert_eq!(
            fabric_mod
                .fit_for(&target(ModLoader::Quilt, "1.21.1", None))
                .loader,
            Fit::Match
        );
        let quilt_mod = cap(&[ModLoader::Quilt], "1.21.1", None);
        assert_eq!(
            quilt_mod
                .fit_for(&target(ModLoader::Fabric, "1.21.1", None))
                .loader,
            Fit::Mismatch
        );
    }

    #[test]
    fn fit_rejects_out_of_range_mc_version() {
        let fit = cap(&[ModLoader::Fabric], "1.20.1", None).fit_for(&target(
            ModLoader::Fabric,
            "1.21.1",
            None,
        ));
        assert_eq!(fit.mc_version, Fit::Mismatch);
        assert_eq!(fit.verdict, FitVerdict::Rejected);
        assert!(
            fit.issues
                .iter()
                .any(|i| i.dimension == FitDimension::McVersion && i.detail.contains("1.20.1")),
            "{:?}",
            fit.issues
        );
    }

    #[test]
    fn fit_rejects_mod_needing_newer_java() {
        let fit = cap(&[ModLoader::Fabric], "1.21.1", Some(21)).fit_for(&target(
            ModLoader::Fabric,
            "1.21.1",
            Some(17),
        ));
        assert_eq!(fit.java, Fit::Mismatch);
        assert_eq!(fit.verdict, FitVerdict::Rejected);
    }

    /// 未声明 MC 区间是「判不出」，不是「不匹配」：结论降为待确认
    #[test]
    fn undeclared_mc_range_is_unclear_not_rejected() {
        let fit =
            cap(&[ModLoader::Fabric], "", None).fit_for(&target(ModLoader::Fabric, "1.21.1", None));
        assert_eq!(fit.mc_version, Fit::Unknown);
        assert_eq!(fit.verdict, FitVerdict::Unclear);
        assert!(
            fit.issues
                .iter()
                .any(|i| i.dimension == FitDimension::McVersion),
            "判不出必须给出理由"
        );
    }

    /// 没问的维度不参与结论：只问加载器时，MC / Java 未知不该拖成待确认
    #[test]
    fn unasked_dimensions_do_not_downgrade_verdict() {
        let fit = cap(&[ModLoader::Fabric], "", None).fit_for(&ServerTarget {
            loader: Some(ModLoader::Fabric),
            mc_version: None,
            java_major: None,
        });
        assert_eq!(fit.mc_version, Fit::Unknown);
        assert_eq!(fit.java, Fit::Unknown);
        assert_eq!(fit.verdict, FitVerdict::Installable);
    }

    /// 没扫到 class（数据包、预算用尽）说明不了 Java 要求，不能算不匹配
    #[test]
    fn missing_bytecode_never_rejects() {
        let fit = cap(&[ModLoader::DataPack], "1.21.1", None).fit_for(&target(
            ModLoader::DataPack,
            "1.21.1",
            Some(21),
        ));
        assert_eq!(fit.java, Fit::Unknown);
        assert_eq!(fit.verdict, FitVerdict::Installable);
    }

    /// 空目标：一维都没问
    #[test]
    fn empty_target_asks_nothing() {
        let fit = cap(&[ModLoader::Fabric], "1.21.1", Some(21)).fit_for(&ServerTarget::none());
        assert!(ServerTarget::none().is_empty());
        assert_eq!(fit.verdict, FitVerdict::Installable);
        assert!(fit.issues.is_empty());
    }
}
