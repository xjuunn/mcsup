//! 服务端变体标识
//!
//! 变体是「哪套服务端软件」，它决定三件事：从哪个来源解析版本、下载物是什么
//! 形态、怎么启动。本项目同时面对两类变体：
//!
//! - **内置变体**：原版 / Fabric / Forge / `NeoForge` / Paper / Purpur 等由官方
//!   API 直接支撑的常见服务端。它们在本模块里是独立的枚举变体，比较与展示零
//!   成本，也让内置代码能用 `match` 穷举而不必 `downcast`。
//! - **第三方变体**：[`ServerVariant::Custom`] 承载任意自定义变体名（如宿主
//!   私有的整合服务端）。第三方只需实现 `port::ServerJarSource` 并在组合根
//!   注册，无需改动本项目任何枚举——这是「库可发布」的前提。
//!
//! 因此「可用的变体集合」**不由本枚举决定**，而是由运行时注册的来源集合决定
//! （见 `port::ServerJarSource::variant`）。本枚举只负责标识与展示。
//!
//! # 为什么不用纯字符串
//!
//! 把变体做成纯 `String` 会让内置路径全是字符串比较，丢掉编译期穷尽检查，
//! 且 `Copy` 语义也没了（大量调用点按值传递）。保留内置枚举项、只给第三方开
//! 一个 `Custom` 口子，是兼顾两者成本最低的形态。

use super::error::UnknownVariant;
use std::fmt;

/// 内置变体的机器名与展示名的唯一口径
///
/// 内置变体名是**稳定契约**：它出现在 `mcsup.json`、数据库列与 CLI 参数里，
/// 改名等于破坏既有安装记录。第三方变体名由宿主自定义，同样按此约定持久化。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VariantId(String);

impl VariantId {
    /// 由任意非空名称构造
    ///
    /// 只做最小校验（非空、无首尾空白、无控制字符、无路径分隔符）：变体名会
    /// 写进 `mcsup.json` 与卡片展示，但不参与文件路径拼接，因此不需要更严的
    /// 字符集。
    ///
    /// **不 trim**：`" paper"` 与 `"paper"` 是两个不同的变体名，静默改写会让
    /// 同一个变体在存储里出现两种写法。首尾空白因此直接判非法。
    ///
    /// 校验不通过时把原串**原样保留**并返回 `Err`，而不是替换成占位值：调用方
    /// 需要知道用户到底敲了什么才能报出有用的错误。占位值在这里没有意义——
    /// 持有非法 id 的变体不该存在。
    ///
    /// # Errors
    ///
    /// 名称为空、含首尾空白、含控制字符或含路径分隔符时返回 [`UnknownVariant`]
    pub fn new(raw: impl Into<String>) -> Result<Self, UnknownVariant> {
        let raw = raw.into();
        if raw.is_empty()
            || raw.trim() != raw
            || raw
                .chars()
                .any(|c| c.is_control() || matches!(c, '/' | '\\'))
        {
            return Err(UnknownVariant { raw });
        }
        Ok(Self(raw))
    }

    /// 名称本体
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VariantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 服务端变体
///
/// 内置项是固定集合；第三方变体一律走 [`Self::Custom`]。构造第三方变体用
/// [`Self::from_id`]，它会把已知的内置名归一成对应内置项，避免同一变体出现
/// 「内置」与「Custom」两种表示。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ServerVariant {
    /// Mojang 原版
    Vanilla,
    /// Fabric 模组加载器
    Fabric,
    /// Forge 模组加载器
    Forge,
    /// `NeoForge` 模组加载器
    NeoForge,
    /// Paper 高性能服务端
    Paper,
    /// Purpur（Paper 的可配置分支）
    Purpur,
    /// Folia（Paper 的区域化多线程分支）
    Folia,
    /// Velocity 代理
    Velocity,
    /// Waterfall 代理（BungeeCord 分支，已停止新构建）
    Waterfall,
    /// Quilt 模组加载器
    Quilt,
    /// `SpongeVanilla` 混合服务端
    SpongeVanilla,
    /// Leaf（Paper 的轻量分支）
    Leaf,
    /// Modrinth 整合包：由整合包清单决定加载器与游戏版本
    ///
    /// 它不是一种「服务端软件」，而是「按整合包布置服务端」的来源：安装时先
    /// 取回 `.mrpack`，读清单拿到 Minecraft 版本与加载器版本，再装对应 loader
    /// 并把服务端侧文件落到各自目录。因此它没有 `ServerJarSource`，只有
    /// `port::ModpackSource`。
    ModrinthModpack,
    /// 外部目录链接（`server link`）进来的服务器，变体未知
    Unknown,
    /// 第三方自定义变体
    ///
    /// 名称由注册该来源的宿主决定；与内置变体名冲突时以
    /// [`Self::parse`] 的行为为准（识别为内置项）。
    Custom(VariantId),
}

impl ServerVariant {
    /// 变体的稳定机器名
    ///
    /// 内置项返回固定字符串，[`Self::Custom`] 返回宿主给的名字。该值会写进
    /// `mcsup.json`、数据库列与 CLI 参数，改动等于破坏既有安装记录。
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Vanilla => "vanilla",
            Self::Fabric => "fabric",
            Self::Forge => "forge",
            Self::NeoForge => "neoforge",
            Self::Paper => "paper",
            Self::Purpur => "purpur",
            Self::Folia => "folia",
            Self::Velocity => "velocity",
            Self::Waterfall => "waterfall",
            Self::Quilt => "quilt",
            Self::SpongeVanilla => "spongevanilla",
            Self::Leaf => "leaf",
            Self::ModrinthModpack => "modrinth-modpack",
            Self::Unknown => "unknown",
            Self::Custom(id) => id.as_str(),
        }
    }

    /// 该变体的下载物是安装器，必须先运行一次才能得到可启动的核心
    ///
    /// `Forge` / `NeoForge` 发布的是 installer，不是服务端 JAR。入口在拿到
    /// artifact 之前就要决定「要不要先挑 Java 跑安装器」，因此这条判定必须在
    /// 领域层可回答，不能由入口按变体名重写一份。
    ///
    /// 与 `port::InstallKind` 的关系：来源解析出的 artifact 上带的安装类型是
    /// 运行期权威值（artifact 已经拿到时以它为准）；本方法是同一事实在领域侧
    /// 的投影，供「解析之前」的编排使用。新增安装器型变体时两处一起改。
    #[must_use]
    pub fn needs_installer(&self) -> bool {
        matches!(self, Self::Forge | Self::NeoForge)
    }

    /// 由机器名构造，内置名归一为内置项
    ///
    /// 内置名 → 对应内置项；其余名字作为第三方变体接受。需要「只认内置集合」
    /// 的场合用 [`Self::parse`]。
    ///
    /// 这是**持久化读取**的入口（`mcsup.json` / 数据库列）：第三方变体必须能
    /// 原样读回，否则宿主注册的变体装完就丢。
    ///
    /// # Errors
    ///
    /// 名字为空、含控制字符或路径分隔符时返回 [`UnknownVariant`]
    pub fn from_id(raw: impl Into<String>) -> Result<Self, UnknownVariant> {
        let raw = raw.into();
        match Self::known(&raw) {
            Some(builtin) => Ok(builtin),
            None => VariantId::new(raw).map(Self::Custom),
        }
    }

    /// 由名字构造**第三方变体**，内置名会归一为内置项
    ///
    /// 与 [`Self::from_id`] 的区别只在语义：`from_id` 用于「读回来的是什么就是
    /// 什么」，本函数用于「我要注册一个自定义变体」。两者实现相同——内置名不能
    /// 同时以 `Custom` 存在，否则同一个变体会有两种表示。
    ///
    /// # Errors
    ///
    /// 名字为空、含控制字符或路径分隔符时返回 [`UnknownVariant`]
    pub fn custom(raw: impl Into<String>) -> Result<Self, UnknownVariant> {
        Self::from_id(raw)
    }

    /// 是否为内置变体（非 [`Self::Custom`]）
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        !matches!(self, Self::Custom(_))
    }

    /// 全部内置变体（含 [`Self::Unknown`]）
    ///
    /// 内置集合在 `as_str` / `display_name` / `known` 三处各列一遍，这是第四
    /// 处——但它把「有哪些内置变体」变成可遍历的事实，让组合根能断言「每个
    /// 内置变体都有来源」，也让新增变体时漏改一处会在测试里立刻暴露，而不是
    /// 等用户撞上一个没有来源的变体。
    ///
    /// [`Self::Unknown`] 也在列表里：它是历史数据兜底项，不是「可安装变体」，
    /// 调用方按需过滤。
    #[must_use]
    pub const fn builtin() -> &'static [Self] {
        &[
            Self::Vanilla,
            Self::Fabric,
            Self::Forge,
            Self::NeoForge,
            Self::Paper,
            Self::Purpur,
            Self::Folia,
            Self::Velocity,
            Self::Waterfall,
            Self::Quilt,
            Self::SpongeVanilla,
            Self::Leaf,
            Self::ModrinthModpack,
            Self::Unknown,
        ]
    }

    /// 内置变体的展示名（英文），第三方变体回落到机器名
    ///
    /// 仅用于 UI；需要本地化文案时由渲染层基于 [`Self::as_str`] 查表。
    #[must_use]
    pub fn display_name(&self) -> &str {
        match self {
            Self::Vanilla => "Vanilla",
            Self::Fabric => "Fabric",
            Self::Forge => "Forge",
            Self::NeoForge => "NeoForge",
            Self::Paper => "Paper",
            Self::Purpur => "Purpur",
            Self::Folia => "Folia",
            Self::Velocity => "Velocity",
            Self::Waterfall => "Waterfall",
            Self::Quilt => "Quilt",
            Self::SpongeVanilla => "SpongeVanilla",
            Self::Leaf => "Leaf",
            Self::ModrinthModpack => "Modrinth Modpack",
            Self::Unknown => "Unknown",
            Self::Custom(id) => id.as_str(),
        }
    }

    /// 该变体在内容资源目录里对应的加载器名
    ///
    /// Modrinth 用加载器名过滤版本（`fabric` / `forge` / `neoforge` / `quilt` /
    /// `paper` …）。原版与未知变体没有加载器，返回 `None`——调用方据此不按
    /// 加载器过滤，而不是拿一个上游不认识的名字去查。
    ///
    /// 整合包同理返回 `None`：它的加载器由清单里的 `dependencies` 决定，而不是
    /// 由变体本身决定。
    ///
    /// 映射口径收敛在领域层：入口与服务端都从这里取，不各自维护一张表。
    #[must_use]
    pub fn content_loader(&self) -> Option<&str> {
        match self {
            Self::Vanilla | Self::Unknown | Self::ModrinthModpack => None,
            Self::Fabric => Some("fabric"),
            Self::Forge => Some("forge"),
            Self::NeoForge => Some("neoforge"),
            Self::Quilt => Some("quilt"),
            // Paper / Purpur / Folia / Leaf 跑 Bukkit 系插件，上游把它们统一
            // 归在 `paper` / `bukkit` 加载器下；`paper` 覆盖面最广
            Self::Paper | Self::Purpur | Self::Folia | Self::Leaf => Some("paper"),
            Self::Velocity => Some("velocity"),
            Self::Waterfall => Some("waterfall"),
            Self::SpongeVanilla => Some("sponge"),
            Self::Custom(id) => Some(id.as_str()),
        }
    }

    /// 内置名 → 内置变体；不是内置名时返回 `None`
    pub(crate) fn known(raw: &str) -> Option<Self> {
        Some(match raw {
            "vanilla" => Self::Vanilla,
            "fabric" => Self::Fabric,
            "forge" => Self::Forge,
            "neoforge" => Self::NeoForge,
            "paper" => Self::Paper,
            "purpur" => Self::Purpur,
            "folia" => Self::Folia,
            "velocity" => Self::Velocity,
            "waterfall" => Self::Waterfall,
            "quilt" => Self::Quilt,
            "spongevanilla" => Self::SpongeVanilla,
            "leaf" => Self::Leaf,
            "modrinth-modpack" => Self::ModrinthModpack,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }
}

impl fmt::Display for ServerVariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 加载器映射：模组加载器一对一，原版与未知无加载器
    #[test]
    fn content_loader_maps_mod_loaders() {
        assert_eq!(ServerVariant::Fabric.content_loader(), Some("fabric"));
        assert_eq!(ServerVariant::Forge.content_loader(), Some("forge"));
        assert_eq!(ServerVariant::NeoForge.content_loader(), Some("neoforge"));
        assert_eq!(ServerVariant::Quilt.content_loader(), Some("quilt"));
        assert_eq!(ServerVariant::Velocity.content_loader(), Some("velocity"));
        assert_eq!(ServerVariant::Waterfall.content_loader(), Some("waterfall"));
        assert_eq!(
            ServerVariant::SpongeVanilla.content_loader(),
            Some("sponge")
        );
        // Bukkit 系服务端统一按 paper 过滤
        for variant in [
            ServerVariant::Paper,
            ServerVariant::Purpur,
            ServerVariant::Folia,
            ServerVariant::Leaf,
        ] {
            assert_eq!(variant.content_loader(), Some("paper"), "{variant:?}");
        }
        // 没有加载器：不能拿一个上游不认识的名字去查
        assert_eq!(ServerVariant::Vanilla.content_loader(), None);
        assert_eq!(ServerVariant::Unknown.content_loader(), None);
        // 第三方变体按自己的名字查
        let custom = ServerVariant::custom("my-loader").expect("custom");
        assert_eq!(custom.content_loader(), Some("my-loader"));
    }

    /// 每个内置项的机器名都能往返，且 `builtin()` 与实际枚举项一致
    ///
    /// 列表由 [`ServerVariant::builtin`] 驱动而不是手写：手写列表与 `as_str`
    /// 是同一份「知识」抄两遍，漏一项时测试照样通过。
    #[test]
    fn builtin_names_round_trip() {
        for expected in ServerVariant::builtin() {
            let raw = expected.as_str();
            let variant = ServerVariant::from_id(raw).expect("内置名应可解析");
            assert!(variant.is_builtin(), "{raw} 应为内置变体");
            assert_eq!(&variant, expected, "{raw} 往返不一致");
            assert_eq!(ServerVariant::parse(raw).as_ref(), Ok(expected), "{raw}");
            // `Custom` 不参与内置集合
            assert!(!matches!(expected, ServerVariant::Custom(_)));
            // 展示名对内置项不落回机器名的原样（`Unknown` 除外）
            if *expected != ServerVariant::Unknown {
                assert_ne!(expected.display_name(), raw, "{raw} 缺展示名");
            }
        }
    }

    /// 内置名互不重复：重复会让 `from_id` 的行为取决于书写顺序
    #[test]
    fn builtin_names_are_unique() {
        let mut seen: Vec<&str> = Vec::new();
        for variant in ServerVariant::builtin() {
            assert!(!seen.contains(&variant.as_str()), "{variant} 重复");
            seen.push(variant.as_str());
        }
    }

    #[test]
    fn unknown_name_becomes_custom() {
        let variant = ServerVariant::from_id("my-custom-server").expect("第三方变体");
        assert_eq!(
            variant,
            ServerVariant::Custom(VariantId::new("my-custom-server").expect("合法 id"))
        );
        assert!(!variant.is_builtin());
        assert_eq!(variant.as_str(), "my-custom-server");
        assert_eq!(variant.display_name(), "my-custom-server");
    }

    #[test]
    fn custom_variant_round_trips_through_name() {
        let variant = ServerVariant::from_id("acme-net").expect("第三方变体");
        assert_eq!(
            ServerVariant::from_id(variant.as_str()).expect("回读"),
            variant
        );
    }

    #[test]
    fn display_uses_machine_name() {
        assert_eq!(ServerVariant::NeoForge.to_string(), "neoforge");
        assert_eq!(
            ServerVariant::from_id("beta-one")
                .expect("变体")
                .to_string(),
            "beta-one"
        );
    }

    #[test]
    fn custom_names_do_not_collide_with_builtins() {
        // 自定义变体名恰好是内置名时，归一为内置项而不是形成两个表示
        assert_eq!(
            ServerVariant::from_id("paper").expect("内置名"),
            ServerVariant::Paper
        );
        assert!(ServerVariant::custom("paper").expect("内置名").is_builtin());
    }

    /// 空名 / 控制字符 / 路径分隔符一律拒绝
    ///
    /// 变体名会写进 `mcsup.json`，也出现在命令行与日志里：控制字符能伪造终端
    /// 输出，路径分隔符会让任何「按名字拼路径」的未来代码出问题。
    #[test]
    fn invalid_variant_names_are_rejected() {
        for raw in ["", " ", "\t", " a", "a ", "a\u{0}b", "a\nb", "a/b", "a\\b"] {
            assert!(ServerVariant::from_id(raw).is_err(), "{raw:?} 应被拒绝");
            assert!(VariantId::new(raw).is_err(), "{raw:?} 应被拒绝");
        }
        // 合法但少见的字符放行：变体名是宿主自定义的，不该过度限制字符集
        assert!(ServerVariant::from_id("acme_server.v2").is_ok());
        // 内部空格不是首尾空白，放行
        assert!(ServerVariant::from_id("acme server").is_ok());
    }

    #[test]
    fn variants_are_comparable_and_hashable_as_keys() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ServerVariant::Paper);
        set.insert(ServerVariant::Custom(VariantId::new("x").expect("id")));
        set.insert(ServerVariant::Custom(VariantId::new("x").expect("id")));
        assert_eq!(set.len(), 2);
    }
}
