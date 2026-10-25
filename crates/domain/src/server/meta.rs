use super::artifact::Checksum;
use super::error::InvalidServerName;
use super::version::ServerVariant;
use crate::java::JavaMajor;
use crate::workspace::{InvalidId, WorkspaceId, name_is_valid, uuid_is_valid};
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
        if name_is_valid(raw) {
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

/// 服务器稳定身份：`mcsup.json` 与数据库共用的 UUID 文本
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerId(String);

impl ServerId {
    /// 校验并构造身份
    ///
    /// # Errors
    ///
    /// 非 8-4-4-4-12 十六进制 UUID 文本时返回 [`InvalidId`]
    pub fn parse(raw: &str) -> Result<Self, InvalidId> {
        if uuid_is_valid(raw) {
            Ok(Self(raw.to_owned()))
        } else {
            Err(InvalidId(raw.to_owned()))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 已安装并校验通过的服务端核心
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledJar {
    /// 相对服务器目录（固定 `server.jar`），不存绝对路径
    pub path: PathBuf,
    pub size: u64,
    pub checksum: Checksum,
}

/// 服务元数据：`storage` 持久化的领域对象
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerMeta {
    pub id: ServerId,
    pub name: ServerName,
    pub workspace_id: WorkspaceId,
    /// 相对工作目录 `servers/` 下的目录名
    pub rel_dir: PathBuf,
    pub version: super::version::VersionId,
    pub variant: ServerVariant,
    /// 尚未执行 `server install` 时为 `None`
    pub jar: Option<InstalledJar>,
    /// 服务端启动方式；`None` 时按 [`super::launch::LaunchSpec::jar`] 的
    /// 历史口径（`server.jar`）推断，兼容旧库记录
    pub launch: Option<super::launch::LaunchSpec>,
    /// 服务属性（`server set` 持久化）
    pub settings: super::settings::ServerSettings,
    /// 该服务端版本要求的 Java 大版本（来自版本详情的 javaVersion.majorVersion）
    /// 用于启动时自动选择正确的 Java 运行时
    pub java_major: Option<JavaMajor>,
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
    fn server_id_requires_uuid_shape() {
        let ok = "550e8400-e29b-41d4-a716-446655440000";
        assert!(ServerId::parse(ok).is_ok());
        assert!(ServerId::parse("1").is_err());
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
        // 内置集合里的新变体都已登记；不在内置集合的名字仍然判错
        assert_eq!(ServerVariant::parse("quilt"), Ok(ServerVariant::Quilt));
        assert!(ServerVariant::parse("not-a-variant").is_err());
    }
}
