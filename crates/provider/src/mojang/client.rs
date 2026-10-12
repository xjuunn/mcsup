use crate::http::{build_client, map_transport};
use async_trait::async_trait;
use domain::server::{
    artifact::{Checksum, ChecksumAlgorithm, JarArtifact},
    version::{ServerVariant, ServerVersion, VersionId},
};
use port::{ResolvedArtifact, ServerJarSource, SourceError};
use reqwest::Client;
use serde::de::DeserializeOwned;
use thiserror::Error;
use url::Url;

use super::manifest::{VersionDetails, VersionKind, VersionManifest, VersionManifestItem};

/// 版本清单地址
const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest.json";

/// Mojang 官方服务端来源
#[derive(Debug)]
pub struct MojangSource {
    http: Client,
}

impl MojangSource {
    /// 用内置配置构建来源
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败时返回 [`SourceError::Init`]
    pub fn new() -> Result<Self, SourceError> {
        Ok(Self {
            http: build_client()?,
        })
    }

    async fn manifest(&self) -> Result<VersionManifest, SourceError> {
        self.get(MANIFEST_URL).await
    }

    async fn details(&self, url: &Url) -> Result<VersionDetails, SourceError> {
        self.get(url.as_str()).await
    }

    async fn get<T>(&self, url: &str) -> Result<T, SourceError>
    where
        T: DeserializeOwned,
    {
        let response = self.http.get(url).send().await.map_err(map_transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(SourceError::Status {
                status: status.as_u16(),
            });
        }
        response.json().await.map_err(map_decode)
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
        let details_url = parse_https_url(&item.url)?;
        let server = self.details(&details_url).await?.downloads.server;
        let location = parse_https_url(&server.url)?;
        let artifact = JarArtifact {
            version: version.clone(),
            size: server.size,
            checksum: Checksum::new(ChecksumAlgorithm::Sha1, server.sha1),
        };
        Ok(ResolvedArtifact { artifact, location })
    }
}

fn into_version(item: VersionManifestItem) -> ServerVersion {
    ServerVersion {
        id: VersionId::new(item.id),
        kind: into_domain_kind(item.kind),
        variant: ServerVariant::Vanilla,
    }
}

fn into_domain_kind(kind: VersionKind) -> domain::server::version::VersionKind {
    use domain::server::version::VersionKind as Domain;
    match kind {
        VersionKind::Release => Domain::Release,
        VersionKind::Snapshot => Domain::Snapshot,
        VersionKind::OldAlpha | VersionKind::OldBeta => Domain::Historical,
    }
}

fn parse_https_url(raw: &str) -> Result<Url, SourceError> {
    let url = raw
        .parse::<Url>()
        .map_err(|e| SourceError::Decode(Box::new(UrlError::Parse(e))))?;
    if url.scheme() != "https" {
        return Err(SourceError::Decode(Box::new(UrlError::Insecure(
            url.scheme().to_owned(),
        ))));
    }
    Ok(url)
}

fn map_decode(err: reqwest::Error) -> SourceError {
    if err.is_decode() {
        SourceError::Decode(Box::new(err))
    } else {
        SourceError::Network(Box::new(err))
    }
}

#[derive(Debug, Error)]
enum UrlError {
    #[error("invalid URL: {0}")]
    Parse(#[source] url::ParseError),
    #[error("only https URLs are accepted, got schema {0:?}")]
    Insecure(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mojang::manifest::VersionManifest;
    use domain::server::version::VersionKind as Domain;

    const SAMPLE: &str = include_str!("../../tests/fixtures/version_manifest.json");

    #[test]
    fn maps_upstream_kinds_to_domain_kinds() -> Result<(), Box<dyn std::error::Error>> {
        let manifest: VersionManifest = serde_json::from_str(SAMPLE)?;
        let versions: Vec<_> = manifest.versions.into_iter().map(into_version).collect();
        assert_eq!(versions[0].kind, Domain::Release);
        assert_eq!(versions[1].kind, Domain::Snapshot);
        assert_eq!(versions[2].kind, Domain::Historical);
        assert_eq!(versions[3].kind, Domain::Historical);
        assert_eq!(versions[0].variant, ServerVariant::Vanilla);
        Ok(())
    }

    #[test]
    fn rejects_non_https_locations() {
        assert!(parse_https_url("http://example.com/file.jar").is_err());
        assert!(parse_https_url("not a url").is_err());
        assert!(parse_https_url("https://example.com/jar").is_ok());
    }
}
