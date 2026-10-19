//! Azul Zulu 直连源

use crate::http::{build_client, map_transport};
use async_trait::async_trait;
use domain::java::{JavaMajor, JavaVendor};
use domain::server::version::VersionId;
use port::{JavaPackage, JavaPackageRequest, JavaReleaseSource, ResolvedJava, SourceError};
use reqwest::Client;
use serde::Deserialize;
use url::Url;

const PACKAGES_URL: &str = "https://api.azul.com/metadata/v1/zulu/packages/";

/// Azul Zulu 来源
#[derive(Debug)]
pub(super) struct AzulSource {
    http: Client,
}

impl AzulSource {
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
struct PackageDto {
    package_uuid: String,
    name: String,
    download_url: String,
    #[serde(default)]
    java_version: Vec<i64>,
}

#[async_trait]
impl JavaReleaseSource for AzulSource {
    fn name(&self) -> &'static str {
        "azul"
    }

    async fn list_packages(
        &self,
        request: &JavaPackageRequest,
    ) -> Result<Vec<JavaPackage>, SourceError> {
        // tar.gz / zip 优先，dmg / msi 不便于直接解压部署
        let archive_type = if request.os == "windows" {
            "zip"
        } else {
            "tar.gz"
        };
        let url = Url::parse_with_params(
            PACKAGES_URL,
            [
                ("java_version", request.major.as_i64().to_string()),
                ("os", request.os.clone()),
                ("arch", request.arch.clone()),
                ("java_package_type", request.package_type.as_str().into()),
                ("archive_type", archive_type.into()),
                ("javafx_bundled", "false".into()),
                ("latest", "true".into()),
                ("release_status", "ga".into()),
                ("availability_types", "CA".into()),
                ("limit", "20".into()),
            ],
        )
        .map_err(|err| SourceError::Decode(Box::new(err)))?;
        let packages: Vec<PackageDto> = self.get(url.as_str()).await?;
        Ok(packages
            .into_iter()
            .filter(|pkg| !pkg.name.contains("-crac-"))
            .map(|pkg| {
                let major = pkg
                    .java_version
                    .first()
                    .copied()
                    .unwrap_or(request.major.as_i64());
                let version = pkg
                    .java_version
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(".");
                JavaPackage {
                    id: pkg.package_uuid,
                    vendor: JavaVendor::Zulu,
                    major: JavaMajor::parse(major).unwrap_or(request.major),
                    version,
                    package_type: request.package_type,
                }
            })
            .collect())
    }

    async fn resolve_binary(&self, package: &JavaPackage) -> Result<ResolvedJava, SourceError> {
        // 用 id 回查拿 download_url
        let url = format!("{PACKAGES_URL}?package_uuid={}", package.id);
        let packages: Vec<PackageDto> = self.get(&url).await?;
        let pkg = packages
            .into_iter()
            .next()
            .ok_or_else(|| SourceError::NotFound(VersionId::new(package.major.to_string())))?;
        let location =
            Url::parse(&pkg.download_url).map_err(|err| SourceError::Decode(Box::new(err)))?;
        Ok(ResolvedJava {
            package: package.clone(),
            location,
            expected_size: None,
        })
    }
}

impl AzulSource {
    async fn get<T>(&self, url: &str) -> Result<T, SourceError>
    where
        T: serde::de::DeserializeOwned,
    {
        let response = self.http.get(url).send().await.map_err(map_transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(SourceError::Status {
                status: status.as_u16(),
            });
        }
        response
            .json()
            .await
            .map_err(|err| SourceError::Decode(Box::new(err)))
    }
}
