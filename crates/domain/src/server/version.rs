use std::fmt;

pub use super::variant::{ServerVariant, VariantId};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VersionId(String);

impl VersionId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VersionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VersionKind {
    Release,
    Snapshot,
    Historical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerVersion {
    pub id: VersionId,
    pub kind: VersionKind,
    pub variant: ServerVariant,
    /// Minecraft 版本要求的 Java 大版本（如 1.21.1 需要 Java 21）
    /// 来源：版本清单中的 `javaVersion.majorVersion`
    pub java_major: Option<crate::java::JavaMajor>,
}

/// 按 Minecraft 版本推断所需的 Java 大版本
///
/// 原版来源从版本清单的 `javaVersion.majorVersion` 直接读到，用不到这个；但
/// **`Forge` / `NeoForge` 的 Maven 元数据里没有 Java 要求**，它只发布 installer。
/// 不给出版本就只能挑「本机最新 Java」，用 JDK 26 跑 1.16 的 Forge 会直接崩，
/// 用户看到的是一串 class 版本错误而不是「缺 Java 8」。
///
/// 分档依据是 Mojang 各版本的官方要求（写入 `version.json` 的最低 Java）：
///
/// | MC 版本 | Java |
/// | --- | --- |
/// | 1.20.5 及以上 | 21 |
/// | 1.18 – 1.20.4 | 17 |
/// | 1.17 | 16 |
/// | 1.12 – 1.16.5 | 8 |
/// | 1.6.4 – 1.11.2 | 8（同样在 Java 8 上运行） |
///
/// 解析不出主次版本号时返回 `None`，由调用方退回自己的兜底口径。
///
/// 只认 `主.次` 两段：快照版（如 `24w14a`）与未发布版一律返回 `None`。
#[must_use]
pub fn required_java_major(mc: &str) -> Option<crate::java::JavaMajor> {
    let mut parts = mc.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    if major != 1 {
        // 1.x 之外没有已知的 MC 版本命名，无法判断
        return None;
    }
    let minor: u32 = parts.next()?.parse().ok()?;
    let patch: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);

    let java = match minor {
        // 1.20.5 起要求 Java 21
        20 if patch >= 5 => 21,
        // 1.21 及之后
        21.. => 21,
        // 1.18 – 1.20.4
        18..=20 => 17,
        // 1.17
        17 => 16,
        // 1.16 及以下（Forge 从 1.6.4 开始有服务端安装器）
        5..=16 => 8,
        _ => return None,
    };
    crate::java::JavaMajor::parse(i64::from(java)).ok()
}

impl ServerVariant {
    /// 解析为**内置**变体
    ///
    /// 只认内置集合：用于 `--variant` 这类需要尽早报错的入口。第三方变体不经
    /// 此路径校验，而是由组合根已注册的来源集合决定（见
    /// [`Self::from_id`]）。
    ///
    /// # Errors
    ///
    /// 名称不在内置集合内时返回 [`super::error::UnknownVariant`]
    pub fn parse(raw: &str) -> Result<Self, super::error::UnknownVariant> {
        Self::known(raw).ok_or_else(|| super::error::UnknownVariant {
            raw: raw.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::required_java_major;

    fn java(mc: &str) -> Option<i16> {
        required_java_major(mc).map(crate::java::JavaMajor::as_i16)
    }

    #[test]
    fn modern_versions_need_java_21() {
        assert_eq!(java("1.21.1"), Some(21));
        assert_eq!(java("1.21"), Some(21));
        assert_eq!(java("1.20.5"), Some(21));
        assert_eq!(java("1.20.6"), Some(21));
    }

    #[test]
    fn one_twenty_before_five_needs_java_17() {
        assert_eq!(java("1.20.4"), Some(17));
        assert_eq!(java("1.20"), Some(17));
        assert_eq!(java("1.18"), Some(17));
        assert_eq!(java("1.18.2"), Some(17));
    }

    #[test]
    fn one_seventeen_needs_java_16() {
        assert_eq!(java("1.17"), Some(16));
        assert_eq!(java("1.17.1"), Some(16));
    }

    #[test]
    fn legacy_versions_need_java_8() {
        assert_eq!(java("1.16.5"), Some(8));
        assert_eq!(java("1.12.2"), Some(8));
        assert_eq!(java("1.6.4"), Some(8));
    }

    #[test]
    fn unknown_shapes_have_no_answer() {
        assert_eq!(java("24w14a"), None);
        assert_eq!(java("latest"), None);
        assert_eq!(java(""), None);
        assert_eq!(java("1"), None);
        assert_eq!(java("2.0"), None);
        // 早于 Forge 服务端安装器的版本不猜
        assert_eq!(java("1.2.5"), None);
    }
}
