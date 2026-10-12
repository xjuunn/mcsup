use super::version::VersionId;

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumAlgorithm {
    Sha1,
    Sha256,
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
