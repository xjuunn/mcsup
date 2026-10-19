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
}

impl Checksum {
    pub fn new(algorithm: ChecksumAlgorithm, hex: impl Into<String>) -> Self {
        Self {
            algorithm,
            hex: hex.into().to_ascii_lowercase(),
        }
    }

    #[must_use]
    pub fn algorithm(&self) -> ChecksumAlgorithm {
        self.algorithm
    }

    #[must_use]
    pub fn matches(&self, computed_hex: &str) -> bool {
        self.hex == computed_hex.to_ascii_lowercase()
    }

    #[must_use]
    pub fn hex(&self) -> &str {
        &self.hex
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JarArtifact {
    pub version: VersionId,
    pub size: u64,
    pub checksum: Checksum,
}
