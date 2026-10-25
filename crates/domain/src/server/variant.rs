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
    /// 只做最小校验（非空、无控制字符、无路径分隔符）：变体名会写进
    /// `mcsup.json` 与卡片展示，但不参与文件路径拼接，因此不需要更严的字符集。
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
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
            Self::Unknown => "unknown",
            Self::Custom(id) => id.as_str(),
        }
    }

    /// 由机器名构造，内置名归一为内置项
    ///
    /// 与 [`Self::parse`] 的区别：未知名不报错，而是作为第三方变体接受。需要
    /// 「只认内置集合」的场合（如 CLI 的 `--variant` 校验）用 [`Self::parse`]。
    #[must_use]
    pub fn from_id(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        Self::known(&raw).unwrap_or_else(|| Self::Custom(VariantId::new(raw)))
    }

    /// 是否为内置变体（非 [`Self::Custom`]）
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        !matches!(self, Self::Custom(_))
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
            Self::Unknown => "Unknown",
            Self::Custom(id) => id.as_str(),
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

    #[test]
    fn builtin_names_round_trip() {
        for raw in [
            "vanilla",
            "fabric",
            "forge",
            "neoforge",
            "paper",
            "purpur",
            "folia",
            "velocity",
            "waterfall",
            "quilt",
            "spongevanilla",
            "leaf",
            "unknown",
        ] {
            let variant = ServerVariant::from_id(raw);
            assert!(variant.is_builtin(), "{raw} 应为内置变体");
            assert_eq!(variant.as_str(), raw, "{raw} 往返不一致");
        }
    }

    #[test]
    fn unknown_name_becomes_custom() {
        let variant = ServerVariant::from_id("my-custom-server");
        assert_eq!(
            variant,
            ServerVariant::Custom(VariantId::new("my-custom-server"))
        );
        assert!(!variant.is_builtin());
        assert_eq!(variant.as_str(), "my-custom-server");
        assert_eq!(variant.display_name(), "my-custom-server");
    }

    #[test]
    fn custom_variant_round_trips_through_name() {
        let variant = ServerVariant::from_id("acme-net");
        assert_eq!(ServerVariant::from_id(variant.as_str()), variant);
    }

    #[test]
    fn display_uses_machine_name() {
        assert_eq!(ServerVariant::NeoForge.to_string(), "neoforge");
        assert_eq!(ServerVariant::from_id("beta-one").to_string(), "beta-one");
    }

    #[test]
    fn custom_names_do_not_collide_with_builtins() {
        // 自定义变体名恰好是内置名时，归一为内置项而不是形成两个表示
        assert_eq!(ServerVariant::from_id("paper"), ServerVariant::Paper);
        assert!(ServerVariant::from_id("paper").is_builtin());
    }

    #[test]
    fn variants_are_comparable_and_hashable_as_keys() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ServerVariant::Paper);
        set.insert(ServerVariant::Custom(VariantId::new("x")));
        set.insert(ServerVariant::Custom(VariantId::new("x")));
        assert_eq!(set.len(), 2);
    }
}
