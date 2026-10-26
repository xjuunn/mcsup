//! 证书指纹：TOFU 的信任锚
//!
//! 指纹是证书 DER 的 SHA-256，文本形态 `sha256:<64 位小写十六进制>`。选择
//! 整张证书而不是公钥：证书本身就是对端要出示的东西，重算指纹不需要解析
//! X.509，也就不需要引入解析库与「解析器有漏洞」的攻击面。
//!
//! 十六进制而不是 base64：用户要把它念给另一台机器的人听、要能一眼比对
//! 首尾几位；base64 的大小写敏感会让「看起来一样」变成陷阱。

use crate::error::TransportError;
use ring::digest;
use std::fmt;

/// 指纹前缀：区分算法，将来换算法时旧记录不会静默匹配
pub const PREFIX: &str = "sha256:";

/// 十六进制位数（SHA-256）
pub const HEX_LEN: usize = 64;

/// 证书指纹
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(String);

impl Fingerprint {
    /// 由证书 DER 计算
    #[must_use]
    pub fn of_der(der: &[u8]) -> Self {
        let digest = digest::digest(&digest::SHA256, der);
        let mut text = String::with_capacity(PREFIX.len() + HEX_LEN);
        text.push_str(PREFIX);
        for byte in digest.as_ref() {
            use std::fmt::Write as _;
            // 写入 String 不会失败；`write!` 的 Result 忽略即不可达
            let _ = write!(text, "{byte:02x}");
        }
        Self(text)
    }

    /// 解析文本形态（配置文件、命令行传入）
    ///
    /// 接受大小写十六进制并归一为小写：用户手抄时大小写不该成为失败原因，
    /// 归一化后比较仍然是逐字符相等。
    ///
    /// # Errors
    ///
    /// 前缀不是 `sha256:` 或十六进制位数不对时返回 [`TransportError::IdentityCorrupt`]
    pub fn parse(raw: &str) -> Result<Self, TransportError> {
        let trimmed = raw.trim();
        let hex = trimmed.strip_prefix(PREFIX).ok_or_else(|| {
            TransportError::IdentityCorrupt(format!("fingerprint must start with {PREFIX}: {raw}"))
        })?;
        if hex.len() != HEX_LEN || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(TransportError::IdentityCorrupt(format!(
                "fingerprint must be {HEX_LEN} hex digits: {raw}"
            )));
        }
        Ok(Self(format!("{PREFIX}{}", hex.to_ascii_lowercase())))
    }

    /// 文本形态
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 不含前缀的十六进制部分
    #[must_use]
    pub fn hex(&self) -> &str {
        self.0.strip_prefix(PREFIX).unwrap_or(&self.0)
    }

    /// 是否与给定证书匹配
    #[must_use]
    pub fn matches_der(&self, der: &[u8]) -> bool {
        *self == Self::of_der(der)
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_deterministic_and_prefixed() {
        let der = b"not really a certificate";
        let first = Fingerprint::of_der(der);
        assert_eq!(first, Fingerprint::of_der(der));
        assert!(first.as_str().starts_with(PREFIX));
        assert_eq!(first.hex().len(), HEX_LEN);
        assert!(first.matches_der(der));
        assert!(!first.matches_der(b"other"));
    }

    #[test]
    fn different_inputs_give_different_fingerprints() {
        assert_ne!(Fingerprint::of_der(b"a"), Fingerprint::of_der(b"b"));
    }

    #[test]
    fn parse_normalizes_case_and_rejects_garbage() {
        let text = Fingerprint::of_der(b"x").as_str().to_ascii_uppercase();
        let parsed = Fingerprint::parse(&format!("{PREFIX}{}", &text[PREFIX.len()..]))
            .expect("uppercase hex is valid");
        assert_eq!(parsed, Fingerprint::of_der(b"x"));

        assert!(Fingerprint::parse("sha256:abc").is_err());
        assert!(Fingerprint::parse("md5:00").is_err());
        assert!(Fingerprint::parse(&"0".repeat(HEX_LEN)).is_err());
        assert!(
            Fingerprint::parse(&format!("{PREFIX}{}", "z".repeat(HEX_LEN))).is_err(),
            "非十六进制必须拒绝"
        );
    }
}
