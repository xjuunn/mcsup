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
