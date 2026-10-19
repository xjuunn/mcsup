use super::artifact::Checksum;
use super::error::InvalidServerName;
use super::version::ServerVariant;
use std::fmt;
use std::path::PathBuf;

/// 服务器名称： 用户可见的唯一标识，同时用作目录名
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServerName(String);

impl ServerName {
    /// 校验并构造名称
    ///
    /// # Errors
    ///
    /// 名称为空、长度超过 64 或含 [A-Za-z0-9_-] 以外字符时返回 [`InvalidServerName`]
    pub fn parse(raw: &str) -> Result<Self, InvalidServerName> {
        let valid = !raw.is_empty()
            && raw.len() <= 64
            && raw
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if valid {
            Ok(Self(raw.to_owned()))
        } else {
            Err(InvalidServerName {
                raw: raw.to_owned(),
            })
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ServerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 已安装并校验通过的服务端核心
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledJar {
    pub path: PathBuf,
    pub size: u64,
    pub checksum: Checksum,
}

/// 服务元数据：`storage` 持久化的领域对象
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerMeta {
    pub name: ServerName,
    pub dir: PathBuf,
    pub version: super::version::VersionId,
    pub variant: ServerVariant,
    /// 尚未执行 `server install` 时为 `None`
    pub jar: Option<InstalledJar>,
    /// Unix 秒
    pub created_at: i64,
    /// Unix 秒
    pub updated_at: i64,
}

#[cfg(test)]
mod tests {
    use crate::server::artifact::ChecksumAlgorithm;

    use super::*;

    #[test]
    fn accepts_ascii_word_names() {
        assert!(ServerName::parse("survival-01").is_ok());
        assert!(ServerName::parse("A_b").is_ok());
    }

    #[test]
    fn rejects_path_and_empty_names() {
        assert!(ServerName::parse("").is_err());
        assert!(ServerName::parse("..").is_err());
        assert!(ServerName::parse("a/b").is_err());
        assert!(ServerName::parse("a b").is_err());
        assert!(ServerName::parse(&"x".repeat(65)).is_err());
    }

    #[test]
    fn checksum_algorithm_roundtrip() {
        assert_eq!(
            ChecksumAlgorithm::parse("sha1"),
            Ok(ChecksumAlgorithm::Sha1)
        );
        assert_eq!(ChecksumAlgorithm::Sha256.as_str(), "sha256");
        assert!(ChecksumAlgorithm::parse("md5").is_err());
    }

    #[test]
    fn variant_roundtrip() {
        assert_eq!(ServerVariant::parse("vanilla"), Ok(ServerVariant::Vanilla));
        assert_eq!(ServerVariant::NeoForge.as_str(), "neoforge");
        assert!(ServerVariant::parse("quilt").is_err());
    }
}
