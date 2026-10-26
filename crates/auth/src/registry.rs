//! 服务端侧的客户端名册：token 摘要、角色、撤销
//!
//! 客户端侧只存自己的 token（[`crate::CredentialStore`]）；服务端要回答的是
//! 另外三个问题：**这个 token 是谁**、**它是什么角色**、**它被撤销了吗**。
//! 三者必须一起落盘，分开存会出现「token 还在但角色丢了」这类半截状态。
//!
//! 落盘形态与凭证文件同构（一行一条 TOML，`0600`）：服务端也可能被用户手查、
//! 手删，二进制格式会让「怎么清掉这个客户端」变成需要文档才能回答的问题。
//!
//! 这里只做**登记与查找**，不做配对码校验、不做 TLS、不决定监听地址——
//! 那些是 `transport` 与 agent 的事。

use crate::code::PairingCodeError;
use crate::credential::{CredentialError, read_private, write_private};
use crate::token::{ClientId, Role, TokenDigest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// 一个已配对客户端
///
/// 不存明文 token：泄漏名册等于泄漏访问权。`digest` 只够校验，不够冒用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ClientRecord {
    /// 服务端分配的身份
    pub client_id: ClientId,
    /// 客户端自称（`remote connect` 时用户输入或主机名）
    pub name: String,
    /// token 摘要
    pub digest: TokenDigest,
    /// 角色
    pub role: Role,
    /// 配对时间（Unix 秒）
    pub paired_at: i64,
}

/// 名册操作失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// 读写失败、文件损坏或权限过宽
    #[error(transparent)]
    Store(#[from] CredentialError),
    /// 配对码不可用
    #[error(transparent)]
    Pairing(#[from] PairingCodeError),
    /// 客户端名非法（空或过长）
    #[error("invalid client name: {0}")]
    InvalidName(String),
    /// 系统随机源不可用
    #[error(transparent)]
    Token(#[from] crate::token::TokenError),
}

/// 落盘形态：`client-id = { name, digest, role, paired-at }`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
struct RegistryFile(BTreeMap<String, ClientRecord>);

/// 客户端名册
///
/// 每次读写都重新读盘：agent 是多线程的，进程内缓存会与「用户手改了名册」
/// 分叉；而配对与认证都是低频操作，多一次读盘不构成问题。
#[derive(Debug, Clone)]
pub struct ClientRegistry {
    path: PathBuf,
}

impl ClientRegistry {
    /// 以给定文件路径构造（不创建文件）
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 文件路径
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读整份名册；文件不存在时为空
    ///
    /// # Errors
    ///
    /// 读取失败、权限过宽或内容非法时返回 [`RegistryError`]
    pub fn load(&self) -> Result<Vec<ClientRecord>, RegistryError> {
        let Some(text) = read_private(&self.path)? else {
            return Ok(Vec::new());
        };
        let file: RegistryFile = toml::from_str(&text).map_err(|err| CredentialError::Corrupt {
            path: self.path.clone(),
            reason: err.to_string(),
        })?;
        Ok(file.0.into_values().collect())
    }

    /// 已配对客户端数量
    ///
    /// # Errors
    ///
    /// 同 [`Self::load`]
    pub fn len(&self) -> Result<usize, RegistryError> {
        Ok(self.load()?.len())
    }

    /// 是否还没有任何客户端（决定 `remote connect` 能否直接 `Enroll`）
    ///
    /// # Errors
    ///
    /// 同 [`Self::load`]
    pub fn is_empty(&self) -> Result<bool, RegistryError> {
        Ok(self.load()?.is_empty())
    }

    /// 登记一个新客户端（同名旧记录按身份替换）
    ///
    /// # Errors
    ///
    /// 名字非法或写入失败时返回 [`RegistryError`]
    pub fn insert(&self, record: ClientRecord) -> Result<(), RegistryError> {
        validate_name(&record.name)?;
        let mut file = self.read_file()?;
        file.0.insert(record.client_id.as_str().to_owned(), record);
        self.write_file(&file)
    }

    /// 按 token 摘要查客户端
    ///
    /// 返回 `None` 表示没有这个 token；**撤销状态不在这里判定**——
    /// 名册里没有「已撤销」的记录，撤销即删除，见 [`Self::revoke`]。
    ///
    /// # Errors
    ///
    /// 读取失败时返回 [`RegistryError`]
    pub fn find_by_digest(
        &self,
        digest: &TokenDigest,
    ) -> Result<Option<ClientRecord>, RegistryError> {
        Ok(self
            .load()?
            .into_iter()
            .find(|record| &record.digest == digest))
    }

    /// 按身份查客户端
    ///
    /// # Errors
    ///
    /// 读取失败时返回 [`RegistryError`]
    pub fn find(&self, client: &ClientId) -> Result<Option<ClientRecord>, RegistryError> {
        Ok(self
            .load()?
            .into_iter()
            .find(|record| &record.client_id == client))
    }

    /// 撤销一个客户端；本来就不存在视为成功（幂等）
    ///
    /// 「撤销即删除」而不是打标记：名册里留着一条无效记录，下次按摘要查找时
    /// 还得再判一次状态，两处判定迟早分叉。
    ///
    /// # Errors
    ///
    /// 写入失败时返回 [`RegistryError`]
    pub fn revoke(&self, client: &ClientId) -> Result<bool, RegistryError> {
        let mut file = self.read_file()?;
        let removed = file.0.remove(client.as_str()).is_some();
        if removed {
            self.write_file(&file)?;
        }
        Ok(removed)
    }

    /// 改一个客户端的角色（重新配对或管理员调整权限）
    ///
    /// # Errors
    ///
    /// 客户端不存在或写入失败时返回 [`RegistryError`]
    pub fn set_role(&self, client: &ClientId, role: Role) -> Result<(), RegistryError> {
        let mut file = self.read_file()?;
        let record = file
            .0
            .get_mut(client.as_str())
            .ok_or_else(|| RegistryError::InvalidName(format!("unknown client {client}")))?;
        record.role = role;
        self.write_file(&file)
    }

    fn read_file(&self) -> Result<RegistryFile, RegistryError> {
        let Some(text) = read_private(&self.path)? else {
            return Ok(RegistryFile::default());
        };
        toml::from_str(&text).map_err(|err| {
            CredentialError::Corrupt {
                path: self.path.clone(),
                reason: err.to_string(),
            }
            .into()
        })
    }

    fn write_file(&self, file: &RegistryFile) -> Result<(), RegistryError> {
        let text = toml::to_string_pretty(file).map_err(|err| CredentialError::Corrupt {
            path: self.path.clone(),
            reason: err.to_string(),
        })?;
        write_private(&self.path, &text)?;
        Ok(())
    }
}

/// 客户端名：非空、≤ 64 字符、不含控制字符
fn validate_name(name: &str) -> Result<(), RegistryError> {
    if name.trim().is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(RegistryError::InvalidName(name.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Token;

    fn registry() -> (tempfile::TempDir, ClientRegistry) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let registry = ClientRegistry::new(dir.path().join("clients.toml"));
        (dir, registry)
    }

    fn record(name: &str, token: &Token, role: Role) -> ClientRecord {
        ClientRecord {
            client_id: ClientId::generate(),
            name: name.to_owned(),
            digest: token.digest(),
            role,
            paired_at: 1_700_000_000,
        }
    }

    #[test]
    fn empty_registry_reports_no_clients() {
        let (_dir, registry) = registry();
        assert!(registry.is_empty().expect("empty"));
        assert_eq!(registry.len().expect("len"), 0);
        assert!(registry.load().expect("load").is_empty());
    }

    #[test]
    fn insert_then_find_by_digest_and_id() {
        let (_dir, registry) = registry();
        let token = Token::generate().expect("token");
        let rec = record("laptop", &token, Role::Operator);
        registry.insert(rec.clone()).expect("insert");

        assert!(!registry.is_empty().expect("empty"));
        assert_eq!(registry.len().expect("len"), 1);
        assert_eq!(
            registry.find_by_digest(&token.digest()).expect("digest"),
            Some(rec.clone())
        );
        assert_eq!(
            registry.find(&rec.client_id).expect("find"),
            Some(rec.clone())
        );
    }

    #[test]
    fn unknown_digest_is_none() {
        let (_dir, registry) = registry();
        let token = Token::generate().expect("token");
        assert!(
            registry
                .find_by_digest(&token.digest())
                .expect("digest")
                .is_none()
        );
    }

    /// 撤销即删除：撤销后按摘要查不到，重复撤销幂等
    #[test]
    fn revoke_removes_the_record() {
        let (_dir, registry) = registry();
        let token = Token::generate().expect("token");
        let rec = record("laptop", &token, Role::Admin);
        registry.insert(rec.clone()).expect("insert");

        assert!(registry.revoke(&rec.client_id).expect("revoke"));
        assert!(
            registry
                .find_by_digest(&token.digest())
                .expect("digest")
                .is_none()
        );
        assert!(!registry.revoke(&rec.client_id).expect("revoke again"));
        assert!(registry.is_empty().expect("empty"));
    }

    #[test]
    fn set_role_updates_only_the_target() {
        let (_dir, registry) = registry();
        let first = record("a", &Token::generate().expect("t"), Role::Viewer);
        let second = record("b", &Token::generate().expect("t"), Role::Operator);
        registry.insert(first.clone()).expect("insert");
        registry.insert(second.clone()).expect("insert");

        registry
            .set_role(&first.client_id, Role::Admin)
            .expect("set role");
        assert_eq!(
            registry
                .find(&first.client_id)
                .expect("find")
                .expect("some")
                .role,
            Role::Admin
        );
        assert_eq!(
            registry
                .find(&second.client_id)
                .expect("find")
                .expect("some")
                .role,
            Role::Operator
        );
    }

    #[test]
    fn set_role_on_unknown_client_fails() {
        let (_dir, registry) = registry();
        let err = registry
            .set_role(&ClientId::generate(), Role::Admin)
            .expect_err("unknown");
        assert!(matches!(err, RegistryError::InvalidName(_)), "{err}");
    }

    #[test]
    fn invalid_names_are_rejected() {
        let (_dir, registry) = registry();
        for name in ["", "   ", "a\nb"] {
            let err = registry
                .insert(record(name, &Token::generate().expect("t"), Role::Viewer))
                .expect_err("invalid");
            assert!(matches!(err, RegistryError::InvalidName(_)), "{name:?}");
        }
        let long = "a".repeat(65);
        assert!(
            registry
                .insert(record(&long, &Token::generate().expect("t"), Role::Viewer))
                .is_err()
        );
    }

    #[test]
    fn corrupt_file_is_reported_with_path() {
        let (dir, registry) = registry();
        std::fs::write(registry.path(), "not a registry").expect("write");
        let err = registry.load().expect_err("corrupt");
        assert!(err.to_string().contains("clients.toml"), "{err}");
        assert!(dir.path().exists());
    }

    /// 名册里绝不出现明文 token
    #[test]
    fn file_holds_digest_not_token() {
        let (_dir, registry) = registry();
        let token = Token::generate().expect("token");
        registry
            .insert(record("laptop", &token, Role::Operator))
            .expect("insert");
        let text = std::fs::read_to_string(registry.path()).expect("read");
        assert!(!text.contains(token.expose()), "名册不得含明文 token");
        assert!(text.contains(token.digest().as_str()));
    }
}
