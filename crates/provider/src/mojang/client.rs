use crate::http::{build_client, get_json};
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
        get_json(&self.http, url, None).await
    }
}

#[async_trait]
impl ServerJarSource for MojangSource {
    fn variant(&self) -> ServerVariant {
        ServerVariant::Vanilla
    }

    fn name(&self) -> &'static str {
        "mojang"
    }

    async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError> {
        let manifest = self.manifest().await?;
        Ok(manifest.versions.into_iter().map(into_version).collect())
    }

    async fn artifact(&self, version: &VersionId) -> Result<ResolvedArtifact, SourceError> {
        let manifest = self.manifest().await?;
        let target = resolve_item_id(&manifest, version).to_owned();
        let item = manifest
            .versions
            .into_iter()
            .find(|item| item.id == target)
            .ok_or_else(|| SourceError::NotFound(version.clone()))?;
        let details = self.details(&parse_https_url(&item.url)?).await?;
        into_atrifact(&VersionId::new(target.as_str()), details)
    }
}

/// `latest` / `latest-snapshot` 保留别名 → 最新稳定版 / 最新快照；其余按字面匹配
fn resolve_item_id<'a>(manifest: &'a VersionManifest, version: &'a VersionId) -> &'a str {
    match version.as_str() {
        "latest" => &manifest.latest.release,
        "latest-snapshot" => &manifest.latest.snapshot,
        other => other,
    }
}

fn into_version(item: VersionManifestItem) -> ServerVersion {
    ServerVersion {
        id: VersionId::new(item.id),
        kind: into_domain_kind(item.kind),
        variant: ServerVariant::Vanilla,
        java_major: None, // 版本列表不包含 Java 版本，详情接口才有
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

fn into_atrifact(
    version: &VersionId,
    details: VersionDetails,
) -> Result<ResolvedArtifact, SourceError> {
    let server = details.downloads.server;
    let location = parse_https_url(&server.url)?;
    let artifact = JarArtifact {
        version: version.clone(),
        size: server.size,
        checksum: Checksum::new(ChecksumAlgorithm::Sha1, server.sha1),
    };
    let java_major = details
        .java_version
        .and_then(|jv| domain::java::JavaMajor::parse(jv.major_version).ok());
    Ok(ResolvedArtifact {
        artifact,
        location,
        java_major,
        install: port::InstallKind::Direct,
        installer_args_dir: None,
    })
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

    /// `latest` 别名解析到最新稳定版；字面版本原样返回
    #[test]
    fn latest_alias_resolves_to_stable_release() -> Result<(), Box<dyn std::error::Error>> {
        let manifest: VersionManifest = serde_json::from_str(SAMPLE)?;
        assert_eq!(
            resolve_item_id(&manifest, &VersionId::new("latest")),
            manifest.latest.release
        );
        assert_eq!(
            resolve_item_id(&manifest, &VersionId::new("latest")),
            "1.21.1"
        );
        assert_eq!(
            resolve_item_id(&manifest, &VersionId::new("latest-snapshot")),
            "24w33a"
        );
        assert_eq!(
            resolve_item_id(&manifest, &VersionId::new("24w33a")),
            "24w33a"
        );
        Ok(())
    }

    /// 能力查询：Mojang 来源声明 vanilla
    #[test]
    fn mojang_source_declares_vanilla_variant() {
        let source = MojangSource::new().expect("Mojang 源初始化");
        assert_eq!(source.variant(), ServerVariant::Vanilla);
    }

    #[test]
    fn rejects_non_https_locations() {
        assert!(parse_https_url("http://example.com/file.jar").is_err());
        assert!(parse_https_url("not a url").is_err());
        assert!(parse_https_url("https://example.com/jar").is_ok());
    }

    #[test]
    fn resolves_server_download_from_details() -> Result<(), Box<dyn std::error::Error>> {
        let details: VersionDetails =
            serde_json::from_str(include_str!("../../tests/fixtures/version_details.json"))?;
        let resolved = into_atrifact(&VersionId::new("1.21.1"), details)?;

        assert_eq!(resolved.artifact.size, 6245_8029);
        assert_eq!(resolved.artifact.version, VersionId::new("1.21.1"));
        assert!(
            resolved
                .artifact
                .checksum
                .matches("2d89c95c030e635387448f332961074ce1adbb4b")
        );
        assert_eq!(
            resolved.location.as_str(),
            "https://example.invalid/server.jar"
        );
        // 版本详情的 javaVersion.majorVersion 必须随 artifact 带出：
        // 启动时按它选运行时，缺失会装错 Java（class file 69.0 报错的根因）
        assert_eq!(
            resolved.java_major.map(domain::java::JavaMajor::as_i64),
            Some(21)
        );
        Ok(())
    }

    /// 老版本详情无 `javaVersion` 字段：不报错，按 `None` 带出
    #[test]
    fn missing_java_version_is_none_not_error() -> Result<(), Box<dyn std::error::Error>> {
        let legacy = r#"{
            "downloads": {
                "server": {
                    "sha1": "2d89c95c030e635387448f332961074ce1adbb4b",
                    "size": 62458029,
                    "url": "https://example.invalid/server.jar"
                }
            }
        }"#;
        let details: VersionDetails = serde_json::from_str(legacy)?;
        let resolved = into_atrifact(&VersionId::new("1.7.10"), details)?;
        assert_eq!(resolved.java_major, None);
        Ok(())
    }
}
