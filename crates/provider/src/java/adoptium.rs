//! Eclipse Temurin（Adoptium）直连源

use crate::http::{build_client, map_transport};
use async_trait::async_trait;
use domain::java::{JavaMajor, JavaVendor};
use domain::server::version::VersionId;
use port::{JavaPackage, JavaPackageRequest, JavaReleaseSource, ResolvedJava, SourceError};
use reqwest::Client;
use serde::Deserialize;
use url::Url;

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
        let url = format!(
            "{ASSETS_URL}/{}/{}/?architecture={}&image_type={}&os={}&vendor=eclipse&heap_size=normal&project=jdk&sort_method=DEFAULT&sort_order=DESC",
            request.major.as_i64(),
            "hotspot",
            request.arch,
            image_type,
            request.os,
        );
        let assets: Vec<Asset> = self.get(&url).await?;
        Ok(assets
            .into_iter()
            .filter_map(|asset| {
                let package = asset.binary.package?;
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
                    major: JavaMajor::parse(major).ok()?,
                    version,
                    package_type: request.package_type,
                })
            })
            .collect())
    }

    async fn resolve_binary(&self, package: &JavaPackage) -> Result<ResolvedJava, SourceError> {
        let location = Url::parse(&package.id).map_err(|err| SourceError::Decode(Box::new(err)))?;
        Ok(ResolvedJava {
            package: package.clone(),
            location,
            expected_size: None,
        })
    }
}

impl AdoptiumSource {
    async fn get<T>(&self, url: &str) -> Result<T, SourceError>
    where
        T: serde::de::DeserializeOwned,
    {
        let response = self.http.get(url).send().await.map_err(map_transport)?;
        let status = response.status();
        if !status.is_success() {
            let status_code = status.as_u16();
            if status_code == 404 {
                return Err(SourceError::NotFound(VersionId::new("adoptium")));
            }
            return Err(SourceError::Status {
                status: status_code,
            });
        }
        response
            .json()
            .await
            .map_err(|err| SourceError::Decode(Box::new(err)))
    }
}
