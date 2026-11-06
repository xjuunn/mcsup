use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct VersionManifest {
    /// 最新稳定版 / 最新快照（`latest` 别名的解析依据）
    pub(crate) latest: LatestVersions,
    pub(crate) versions: Vec<VersionManifestItem>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LatestVersions {
    pub(crate) release: String,
    pub(crate) snapshot: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct VersionManifestItem {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) kind: VersionKind,
    /// 发布时间，RFC 3339 UTC 原文（如 `2026-11-03T13:08:56+00:00`）
    ///
    /// 清单里每条都有，但按可选解析：字段缺失时退回版本号排序，不让整个
    /// 版本列表因为一条缺字段而获取失败。
    #[serde(rename = "releaseTime")]
    pub(crate) released: Option<String>,
    pub(crate) url: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VersionKind {
    Release,
    Snapshot,
    OldAlpha,
    OldBeta,
}

#[derive(Debug, Deserialize)]
pub(crate) struct VersionDetails {
    pub(crate) downloads: Downloads,
    /// Java 版本要求（如 `"javaVersion": {"component": "java-runtime-delta", "majorVersion": 21}`）
    #[serde(rename = "javaVersion")]
    pub(crate) java_version: Option<JavaVersion>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JavaVersion {
    #[serde(rename = "majorVersion")]
    pub(crate) major_version: i64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Downloads {
    pub(crate) server: ServerDownload,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ServerDownload {
    pub(crate) sha1: String,
    pub(crate) size: u64,
    pub(crate) url: String,
}
