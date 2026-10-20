use std::fmt;
use std::path::PathBuf;
use thiserror::Error;

/// 工作目录名非法
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid workspace name: {raw}")]
pub struct InvalidWorkspaceName {
    pub raw: String,
}

/// 身份字符串非法（须为 UUID 文本）
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid id: {0}")]
pub struct InvalidId(pub String);

/// 名称字符集：非空、≤64 字节、仅 `[A-Za-z0-9_-]`
///
/// `WorkspaceName` 与 `ServerName` 共用——两者都会落到文件系统目录名，
/// 判定分叉会让一边过得了、另一边过不了。
pub(crate) fn name_is_valid(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 64
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub(crate) fn uuid_is_valid(raw: &str) -> bool {
    raw.len() == 36
        && raw.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// 工作目录名：与 `ServerName` 同字符集，注册表内唯一
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceName(String);

impl WorkspaceName {
    /// 校验并构造名称
    ///
    /// # Errors
    ///
    /// 名称为空、长度超过 64 或含 [A-Za-z0-9_-] 以外字符时返回 [`InvalidWorkspaceName`]
    pub fn parse(raw: &str) -> Result<Self, InvalidWorkspaceName> {
        if name_is_valid(raw) {
            Ok(Self(raw.to_owned()))
        } else {
            Err(InvalidWorkspaceName {
                raw: raw.to_owned(),
            })
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 工作目录稳定身份：`workspace.toml` 与数据库共用的 UUID 文本
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceId(String);

impl WorkspaceId {
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

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 工作目录：已注册目录的领域对象
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: WorkspaceName,
    /// 目录绝对路径；移动后由注册表更新
    pub path: PathBuf,
    pub is_default: bool,
    /// Unix 秒
    pub created_at: i64,
    /// Unix 秒
    pub updated_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_name_accepts_same_charset_as_server_name() {
        assert!(WorkspaceName::parse("ws-01_A").is_ok());
        assert!(WorkspaceName::parse("").is_err());
        assert!(WorkspaceName::parse("a/b").is_err());
    }

    #[test]
    fn uuid_shape_is_enforced() {
        let ok = "550e8400-e29b-41d4-a716-446655440000";
        assert!(WorkspaceId::parse(ok).is_ok());
        assert!(WorkspaceId::parse("not-a-uuid").is_err());
        assert!(WorkspaceId::parse("550e8400e29b41d4a716446655440000").is_err());
    }
}
