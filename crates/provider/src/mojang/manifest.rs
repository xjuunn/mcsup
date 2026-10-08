use serde::Deserialize;

// https://piston-meta.mojang.com/mc/game/version_manifest.json
#[derive(Debug, Deserialize)]
pub(crate) struct VersionManifest {
    latest: VersionManifestLatest,
    versions: Vec<VersionManifestItem>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct VersionManifestLatest {
    release: String,
    snapshot: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct VersionManifestItem {
    id: String,
    #[serde(rename = "type")]
    kind: VersionKind,
    url: String,
    time: String,
    #[serde(rename = "releaseTime")]
    release_time: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum VersionKind {
    Snapshot,
    Release,
    OldAlpha,
    OldBeta,
}

// https://piston-meta.mojang.com/v1/packages/702fe59163c6ee6578607daa85811d9bc9c7cc40/26.3.json
#[derive(Debug, Deserialize)]
pub(crate) struct VersionManifestDetails {
    downloads: VersionManifestDetailsDownloads,
    #[serde(rename = "javaVersion")]
    java_version: VersionManifestDetailsJavaVersion,
}

#[derive(Debug, Deserialize)]
pub(crate) struct VersionManifestDetailsDownloads {
    client: VersionManifestDetailsClient,
    server: VersionManifestDetailsServer,
}

#[derive(Debug, Deserialize)]
pub(crate) struct VersionManifestDetailsClient {
    sha1: String,
    size: u64,
    url: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct VersionManifestDetailsServer {
    sha1: String,
    size: u64,
    url: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct VersionManifestDetailsJavaVersion {
    component: String,
    #[serde(rename = "majorVersion")]
    major_version: u32,
}
