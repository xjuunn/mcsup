use std::fmt;

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

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServerVariant {
    Vanilla,
    Fabric,
    Forge,
    NeoForge,
    Paper,
    /// 外部目录链接（`server link`）进来的服务器，变体未知
    Unknown,
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
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vanilla => "vanilla",
            Self::Fabric => "fabric",
            Self::Forge => "forge",
            Self::NeoForge => "neoforge",
            Self::Paper => "paper",
            Self::Unknown => "unknown",
        }
    }

    /// 解析变体名
    ///
    /// # Errors
    ///
    /// 名称不在已知集合内时返回 [`super::error::UnknownVariant`]
    pub fn parse(raw: &str) -> Result<Self, super::error::UnknownVariant> {
        match raw {
            "vanilla" => Ok(Self::Vanilla),
            "fabric" => Ok(Self::Fabric),
            "forge" => Ok(Self::Forge),
            "neoforge" => Ok(Self::NeoForge),
            "paper" => Ok(Self::Paper),
            "unknown" => Ok(Self::Unknown),
            other => Err(super::error::UnknownVariant {
                raw: other.to_owned(),
            }),
        }
    }
}
