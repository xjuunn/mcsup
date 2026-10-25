use super::version::VersionId;

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumAlgorithm {
    Sha1,
    Sha256,
}

impl ChecksumAlgorithm {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }

    /// 解析算法名
    ///
    /// # Errors
    ///
    /// 名称不是 `sha1` / `sha256` 时返回 [`super::error::UnknownChecksumAlgorithm`]
    pub fn parse(raw: &str) -> Result<Self, super::error::UnknownChecksumAlgorithm> {
        match raw {
            "sha1" => Ok(Self::Sha1),
            "sha256" => Ok(Self::Sha256),
            other => Err(super::error::UnknownChecksumAlgorithm(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    algorithm: ChecksumAlgorithm,
    hex: String,
    /// 上游未提供校验和时为 `true`：`hex` 是占位值，不得作为校验依据
    unverified: bool,
}

impl Checksum {
    pub fn new(algorithm: ChecksumAlgorithm, hex: impl Into<String>) -> Self {
        Self {
            algorithm,
            hex: hex.into().to_ascii_lowercase(),
            unverified: false,
        }
    }

    /// 上游未提供校验和时的占位值
    ///
    /// 部分来源（如 Fabric 服务端 JAR）只给出下载地址，不提供任何哈希。
    /// 该值只用于满足 [`JarArtifact`] 的必填字段，**不得**参与校验：
    /// [`Self::matches`] 恒为 `false`，下载方必须据 [`Self::is_verified`]
    /// 跳过校验，而不是拿占位值去比对。
    #[must_use]
    pub fn unverified() -> Self {
        Self {
            algorithm: ChecksumAlgorithm::Sha1,
            hex: String::new(),
            unverified: true,
        }
    }

    #[must_use]
    pub fn algorithm(&self) -> ChecksumAlgorithm {
        self.algorithm
    }

    /// 上游是否提供了可用于校验的哈希
    #[must_use]
    pub fn is_verified(&self) -> bool {
        !self.unverified
    }

    /// 十六进制哈希值；未校验时为空串
    #[must_use]
    pub fn hex(&self) -> &str {
        &self.hex
    }

    /// 比对计算出的哈希值
    ///
    /// 未校验的占位值恒返回 `false`：它不代表任何真实内容，比对成功即意味着
    /// 把任意字节当成合格产物。
    #[must_use]
    pub fn matches(&self, computed_hex: &str) -> bool {
        !self.unverified && self.hex == computed_hex.to_ascii_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_checksum_is_verified_and_matches_case_insensitively() {
        let checksum = Checksum::new(ChecksumAlgorithm::Sha1, "ABCDEF");
        assert!(checksum.is_verified());
        assert_eq!(checksum.hex(), "abcdef");
        assert!(checksum.matches("abcdef"));
        assert!(checksum.matches("ABCDEF"));
        assert!(!checksum.matches("000000"));
    }

    /// 未校验占位值不代表任何内容：即使传入空串也必须比对失败
    #[test]
    fn unverified_checksum_never_matches() {
        let checksum = Checksum::unverified();
        assert!(!checksum.is_verified());
        assert_eq!(checksum.hex(), "");
        assert!(!checksum.matches(""));
        assert!(!checksum.matches("0000000000000000000000000000000000000000"));
    }

    /// 未校验与任何真实校验和都不相等，避免占位值被当成有效哈希传播
    #[test]
    fn unverified_is_distinct_from_real_checksum() {
        let placeholder = Checksum::unverified();
        let real = Checksum::new(ChecksumAlgorithm::Sha1, "");
        assert_ne!(placeholder, real);
        assert!(real.is_verified());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JarArtifact {
    pub version: VersionId,
    pub size: u64,
    pub checksum: Checksum,
}
