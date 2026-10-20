//! Eclipse Temurin（Adoptium）直连源

use crate::http::{build_client, get_json};
use async_trait::async_trait;
use domain::java::JavaVendor;
use domain::server::version::VersionId;
use port::{JavaPackage, JavaPackageRequest, JavaReleaseSource, ResolvedJava, SourceError};
use reqwest::Client;
use serde::Deserialize;

const ASSETS_URL: &str = "https://api.adoptium.net/v3/assets/latest";

/// Adoptium Temurin 来源
#[derive(Debug)]
pub(super) struct AdoptiumSource {
    http: Client,
}

impl AdoptiumSource {
    /// 构建来源
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败时返回 [`SourceError::Init`]
    pub(super) fn new() -> Result<Self, SourceError> {
        Ok(Self {
            http: build_client()?,
        })
    }
}

#[derive(Debug, Deserialize)]
struct Asset {
    binary: Binary,
    #[serde(default)]
    version: Option<VersionInfo>,
}

#[derive(Debug, Deserialize)]
struct Binary {
    #[serde(default)]
    package: Option<PackageInfo>,
}

#[derive(Debug, Deserialize)]
struct PackageInfo {
    link: String,
}

#[derive(Debug, Deserialize)]
struct VersionInfo {
    #[serde(default)]
    major: Option<i64>,
    #[serde(default)]
    openjdk_version: Option<String>,
}

#[async_trait]
impl JavaReleaseSource for AdoptiumSource {
    fn name(&self) -> &'static str {
        "adoptium"
    }

    async fn list_packages(
        &self,
        request: &JavaPackageRequest,
    ) -> Result<Vec<JavaPackage>, SourceError> {
        let image_type = request.package_type.as_str();
        let archive_type = if request.os == "windows" {
            "zip"
        } else {
            "tar.gz"
        };
        let url = format!(
            "{ASSETS_URL}/{}/{}/?architecture={}&image_type={}&os={}&archive_type={}&vendor=eclipse&heap_size=normal&project=jdk&sort_method=DEFAULT&sort_order=DESC",
            request.major.as_i64(),
            "hotspot",
            request.arch,
            image_type,
            request.os,
            archive_type,
        );
        let assets: Vec<Asset> = self.get(&url).await?;
        Ok(assets
            .into_iter()
            .filter_map(|asset| {
                let package = asset.binary.package?;
                if !super::is_deployable_archive(&package.link) {
                    return None;
                }
                let major = asset
                    .version
                    .as_ref()
                    .and_then(|v| v.major)
                    .unwrap_or(request.major.as_i64());
                let version = asset
                    .version
                    .as_ref()
                    .and_then(|v| v.openjdk_version.clone())
                    .unwrap_or_else(|| major.to_string());
                Some(JavaPackage {
                    id: package.link.clone(),
                    vendor: JavaVendor::Temurin,
                    major: super::parse_major_or_drop(major, "adoptium", &package.link)?,
                    version,
                    package_type: request.package_type,
                })
            })
            .collect())
    }

    /// id 即下载地址，共用解析见 [`super::resolved_from_id`]
    async fn resolve_binary(&self, package: &JavaPackage) -> Result<ResolvedJava, SourceError> {
        super::resolved_from_id(package)
    }
}

impl AdoptiumSource {
    async fn get<T>(&self, url: &str) -> Result<T, SourceError>
    where
        T: serde::de::DeserializeOwned,
    {
        get_json(&self.http, url, Some(VersionId::new("adoptium"))).await
    }
}
