//! token、客户端身份、角色与撤销
//!
//! token 的存储口径：**服务端只存摘要**。拿到 token 的人等于拿到访问权，
//! 所以磁盘上出现明文 token 就等于泄漏；[`TokenDigest`] 是 SHA-256 的十六进制
//! 文本，比对时用常量时间比较，避免按字节提前返回泄漏前缀长度。
//!
//! token 用 32 字节系统随机源生成，不可预测、不可枚举；不加密、不签名——
//! 它本身就是不透明凭据，服务端靠摘要查表识别，不需要从中解出任何信息。

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;

/// token 原始字节数（256 位熵）
const TOKEN_BYTES: usize = 32;

/// token 文本长度（十六进制编码）
pub const TOKEN_LEN: usize = TOKEN_BYTES * 2;

/// 新配对 token 的默认有效期：30 天
///
/// 服务端在配对时把 `paired_at + TTL` 写进客户端记录；过期后认证被拒，用户
/// 需重新 `remote connect`。引入 TTL 之前的旧记录没有过期时间，按「不过期」
/// 兼容处理。
pub const DEFAULT_TOKEN_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// 客户端身份
///
/// 由服务端在配对成功时生成并写进 token 记录，用来实现「撤销某个客户端」
/// 与审计归因。用 UUID 文本是因为它无需协调就能全局唯一，且能直接当文件名。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientId(String);

impl ClientId {
    /// 校验并构造
    ///
    /// # Errors
    ///
    /// 不是合法 UUID 文本时返回 [`TokenError::InvalidClientId`]
    pub fn parse(raw: &str) -> Result<Self, TokenError> {
        if uuid::Uuid::try_parse(raw).is_ok() {
            Ok(Self(raw.to_owned()))
        } else {
            Err(TokenError::InvalidClientId(raw.to_owned()))
        }
    }

    /// 生成一个新身份
    #[must_use]
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// 原始文本
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 权限：最小集合，先满足真实需求
///
/// 粒度到「资源族 × 读/写」为止，不做到字段级——字段级 RBAC 在没有真实诉求
/// 时只会变成负担（`docs/architecture.md`「Auth」一节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    /// 读工作区 / 服务器 / Java / 备份的元数据
    Read,
    /// 启停服务器、注入控制台命令
    Control,
    /// 创建工作区 / 服务器、安装核心
    Write,
    /// 创建与删除备份、恢复
    Backup,
    /// 读远端日志与控制台输出
    Logs,
    /// 管理已配对客户端（签发 / 撤销）
    Manage,
}

impl Permission {
    /// 全部权限（`Admin` 用）
    pub const ALL: &'static [Self] = &[
        Self::Read,
        Self::Control,
        Self::Write,
        Self::Backup,
        Self::Logs,
        Self::Manage,
    ];

    /// 稳定文本（审计日志、错误提示共用）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Control => "control",
            Self::Write => "write",
            Self::Backup => "backup",
            Self::Logs => "logs",
            Self::Manage => "manage",
        }
    }
}

impl std::fmt::Display for Permission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 角色：Admin / Operator / Viewer
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// 只读
    Viewer,
    /// 日常运维：读写资源、控制进程、备份，但不能管理其他客户端
    Operator,
    /// 全部权限
    Admin,
}

impl Role {
    /// 稳定文本
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Operator => "operator",
            Self::Admin => "admin",
        }
    }

    /// 该角色是否拥有某项权限
    #[must_use]
    pub fn permits(self, permission: Permission) -> bool {
        match self {
            Self::Admin => true,
            Self::Operator => permission != Permission::Manage,
            Self::Viewer => matches!(permission, Permission::Read | Permission::Logs),
        }
    }

    /// 该角色拥有的全部权限（按 [`Permission::ALL`] 顺序）
    pub fn permissions(self) -> impl Iterator<Item = Permission> {
        Permission::ALL
            .iter()
            .copied()
            .filter(move |permission| self.permits(*permission))
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 校验失败
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TokenError {
    /// 客户端身份不是合法 UUID
    #[error("invalid client id: {0}")]
    InvalidClientId(String),
    /// token 文本长度或字符集不对
    #[error("malformed token")]
    Malformed,
    /// 系统随机源不可用
    #[error("secure random source unavailable")]
    Random,
    /// 摘要与预期不符（token 失效或伪造）
    #[error("token does not match")]
    Mismatch,
}

/// 明文 token：只在签发与出示的瞬间存在于内存
///
/// 刻意不实现 `Serialize`：序列化会把明文写进日志、JSON 输出或错误消息。
/// 需要落盘时用 [`Token::digest`]。
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// 用系统随机源生成
    ///
    /// # Errors
    ///
    /// 系统随机源不可用时返回 [`TokenError::Random`]
    pub fn generate() -> Result<Self, TokenError> {
        let mut bytes = [0u8; TOKEN_BYTES];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| TokenError::Random)?;
        Ok(Self(hex(&bytes)))
    }

    /// 解析已有文本（客户端从凭证存储读回时用）
    ///
    /// # Errors
    ///
    /// 长度不是 [`TOKEN_LEN`] 或含非十六进制字符时返回 [`TokenError::Malformed`]
    pub fn parse(raw: &str) -> Result<Self, TokenError> {
        if raw.len() != TOKEN_LEN || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(TokenError::Malformed);
        }
        Ok(Self(raw.to_owned()))
    }

    /// 明文（仅用于发给对端与写入凭证存储）
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// 摘要：服务端只存这个
    #[must_use]
    pub fn digest(&self) -> TokenDigest {
        TokenDigest(sha256_hex(self.0.as_bytes()))
    }
}

impl std::fmt::Debug for Token {
    /// 打印成 `Token(***)`：调试输出与日志不得泄漏明文
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(***)")
    }
}

/// token 摘要：服务端持久化的形态
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenDigest(String);

impl TokenDigest {
    /// 从十六进制文本还原（读回服务端记录时用）
    ///
    /// # Errors
    ///
    /// 不是 64 位十六进制时返回 [`TokenError::Malformed`]
    pub fn parse(raw: &str) -> Result<Self, TokenError> {
        if raw.len() != 64 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(TokenError::Malformed);
        }
        Ok(Self(raw.to_ascii_lowercase()))
    }

    /// 十六进制文本
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 常量时间比较：出示的 token 是否就是这个摘要
    ///
    /// # Errors
    ///
    /// 不匹配时返回 [`TokenError::Mismatch`]
    pub fn verify(&self, presented: &Token) -> Result<(), TokenError> {
        if ct_eq(self.0.as_bytes(), presented.digest().0.as_bytes()) {
            Ok(())
        } else {
            Err(TokenError::Mismatch)
        }
    }
}

impl std::fmt::Display for TokenDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 已撤销的客户端集合
///
/// 撤销是**立即生效**的口径（`docs/remote.md`「安全」）：服务端每次接受连接
/// 都查这张表，而不是等 token 自然过期。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RevocationList(BTreeSet<ClientId>);

impl RevocationList {
    /// 空表
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 撤销一个客户端（幂等）
    pub fn revoke(&mut self, client: ClientId) {
        self.0.insert(client);
    }

    /// 是否已撤销
    #[must_use]
    pub fn is_revoked(&self, client: &ClientId) -> bool {
        self.0.contains(client)
    }

    /// 已撤销的客户端，按 id 升序
    pub fn iter(&self) -> impl Iterator<Item = &ClientId> {
        self.0.iter()
    }

    /// 条目数
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 是否为空
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// 常量时间字节比较
///
/// 逐字节累积差异位，不在第一个不同处提前返回——否则比较耗时会泄漏
/// 「前多少字节猜对了」。长度不同直接返回 `false`：被比较的两边都是定长
/// 摘要 / 定长码，长度本身不携带秘密。
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

/// 十六进制编码（小写）
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // 写入 String 不会失败；忽略返回值以保持无 panic
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// SHA-256 十六进制
fn sha256_hex(input: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, input);
    hex(digest.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_well_formed_and_unique() {
        let a = Token::generate().expect("random");
        let b = Token::generate().expect("random");
        assert_eq!(a.expose().len(), TOKEN_LEN);
        assert!(a.expose().bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(a, b, "两次生成不得相同");
        assert_eq!(Token::parse(a.expose()).expect("parse"), a);
    }

    #[test]
    fn token_parse_rejects_malformed_input() {
        assert_eq!(Token::parse(""), Err(TokenError::Malformed));
        assert_eq!(
            Token::parse(&"a".repeat(TOKEN_LEN - 1)),
            Err(TokenError::Malformed)
        );
        assert_eq!(
            Token::parse(&"z".repeat(TOKEN_LEN)),
            Err(TokenError::Malformed)
        );
    }

    /// 明文不得出现在 Debug 输出里（日志与错误消息都会走它）
    #[test]
    fn debug_output_never_leaks_the_secret() {
        let token = Token::generate().expect("random");
        let rendered = format!("{token:?}");
        assert_eq!(rendered, "Token(***)");
        assert!(!rendered.contains(token.expose()));
    }

    #[test]
    fn digest_verifies_only_the_matching_token() {
        let token = Token::generate().expect("random");
        let digest = token.digest();
        assert!(digest.verify(&token).is_ok());
        let other = Token::generate().expect("random");
        assert_eq!(digest.verify(&other), Err(TokenError::Mismatch));
    }

    #[test]
    fn digest_roundtrips_through_text() {
        let token = Token::generate().expect("random");
        let digest = token.digest();
        assert_eq!(digest.as_str().len(), 64);
        assert_eq!(
            TokenDigest::parse(digest.as_str()).expect("parse"),
            digest,
            "摘要必须能落盘再读回"
        );
        assert!(TokenDigest::parse("short").is_err());
    }

    #[test]
    fn viewer_cannot_control_and_operator_cannot_manage() {
        assert!(Role::Viewer.permits(Permission::Read));
        assert!(Role::Viewer.permits(Permission::Logs));
        assert!(!Role::Viewer.permits(Permission::Control));
        assert!(!Role::Viewer.permits(Permission::Write));
        assert!(!Role::Viewer.permits(Permission::Backup));

        assert!(Role::Operator.permits(Permission::Control));
        assert!(Role::Operator.permits(Permission::Backup));
        assert!(!Role::Operator.permits(Permission::Manage));

        for permission in Permission::ALL {
            assert!(Role::Admin.permits(*permission), "{permission}");
        }
    }

    /// 角色权限是单调的：高角色必须覆盖低角色的全部权限
    #[test]
    fn roles_are_monotonic() {
        let viewer: Vec<_> = Role::Viewer.permissions().collect();
        let operator: Vec<_> = Role::Operator.permissions().collect();
        let admin: Vec<_> = Role::Admin.permissions().collect();
        for permission in &viewer {
            assert!(
                operator.contains(permission),
                "{permission} 必须被 Operator 继承"
            );
        }
        for permission in &operator {
            assert!(admin.contains(permission), "{permission} 必须被 Admin 继承");
        }
    }

    #[test]
    fn client_ids_are_validated_and_generated() {
        let id = ClientId::generate();
        assert_eq!(ClientId::parse(id.as_str()).expect("parse"), id);
        assert!(ClientId::parse("not-a-uuid").is_err());
    }

    #[test]
    fn revocation_is_idempotent_and_queryable() {
        let mut list = RevocationList::new();
        let client = ClientId::generate();
        assert!(list.is_empty());
        assert!(!list.is_revoked(&client));
        list.revoke(client.clone());
        list.revoke(client.clone());
        assert_eq!(list.len(), 1, "重复撤销不增加条目");
        assert!(list.is_revoked(&client));
        assert_eq!(list.iter().count(), 1);
    }
}
