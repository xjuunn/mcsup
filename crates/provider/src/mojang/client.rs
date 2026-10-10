use async_trait::async_trait;
use domain::server::{
    artifact::{self, Checksum, JarArtifact},
    version::{ServerVariant, ServerVersion, VersionId},
};
use port::{ResolvedArtifact, ServerJarSource, SourceError};
use reqwest::{Client, Url};

use super::manifest::{VersionDetails, VersionKind, VersionManifest, VersionManifestItem};

const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest.json";

pub(crate) struct MojangSource {
    http: Client,
}

impl MojangSource {
    pub(crate) fn new(http: Client) -> Self {
        Self { http }
    }

    async fn manifest(&self) -> Result<VersionManifest, SourceError> {
        self.http
            .get(MANIFEST_URL)
            .send()
            .await
            .map_err(map_transport)?
            .json()
            .await
            .map_err(map_transport)
    }

    async fn details(&self, url: &str) -> Result<VersionDetails, SourceError> {
        self.http
            .get(url)
            .send()
            .await
            .map_err(map_transport)?
            .json()
            .await
            .map_err(map_decode)
    }
}

#[async_trait]
impl ServerJarSource for MojangSource {
    async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError> {
        let manifest = self.manifest().await?;
        Ok(manifest.versions.into_iter().map(into_version).collect())
    }

    async fn artifact(&self, version: &VersionId) -> Result<ResolvedArtifact, SourceError> {
        let manifest = self.manifest().await?;
        let item = manifest
            .versions
            .into_iter()
            .find(|item| item.id == version.as_str())
            .ok_or_else(|| SourceError::NotFound(version.clone()))?;
        let server = self.details(&item.url).await?.downloads;
        let location = server
            .url
            .parse::<Url>()
            .map_err(|e| SourceError::Decode(Box::new(e)))?;
        let artifact = JarArtifact {
            version: version.clone(),
            size: server.size,
            checksum: Checksum::new(artifact::ChecksumAlgorithm::Sha1, server.sha1),
        };
        Ok(ResolvedArtifact { artifact, location })
    }
}

fn into_version(item: VersionManifestItem) -> ServerVersion {
    ServerVersion {
        id: VersionId::new(item.id),
        kind: item.kind.into(),
        variant: ServerVariant::Vanilla,
    }
}

impl From<VersionKind> for domain::server::version::VersionKind {
    fn from(kind: VersionKind) -> Self {
        match kind {
            VersionKind::Release => Self::Release,
            VersionKind::Snapshot => Self::Snapshot,
            VersionKind::OldAlpha | VersionKind::OldBeta => Self::Historical,
        }
    }
}

fn map_transport(err: reqwest::Error) -> SourceError {
    SourceError::NetWork(Box::new(err))
}

fn map_decode(err: reqwest::Error) -> SourceError {
    SourceError::Decode(Box::new(err))
}

#[cfg(test)]
mod tests {
    use domain::server::version::VersionKind as DomainVersionKind;

    use super::super::manifest::VersionManifest;
    use super::*;

    const SAMPLE: &str = include_str!("../../tests/fixtures/version_manifest.json");

    fn maps_release_snapshot_and_historical() -> Result<(), Box<dyn std::error::Error>> {
        let manifest: VersionManifest = serde_json::from_str(SAMPLE)?;
        let versions: Vec<_> = manifest.versions.into_iter().map(into_version).collect();

        assert_eq!(versions[0].kind, DomainVersionKind::Release);
        assert_eq!(versions[1].kind, DomainVersionKind::Snapshot);
        assert_eq!(versions[2].kind, DomainVersionKind::Historical);
        assert_eq!(versions[0].variant, ServerVariant::Vanilla);
        Ok(())
    }
}
