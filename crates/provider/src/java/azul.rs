//! Azul Zulu 直连源

use crate::http::{build_client, get_json};
use async_trait::async_trait;
use domain::java::JavaVendor;
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
            .filter(|pkg| {
                !pkg.name.contains("-crac-") && super::is_deployable_archive(&pkg.download_url)
            })
            .filter_map(|pkg| {
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
                // id 即下载地址：Azul 的列表响应已带 download_url，
                // 回查反而不可靠（见 resolve_binary）
                let id = pkg.download_url;
                Some(JavaPackage {
                    major: super::parse_major_or_drop(major, "azul", &id)?,
                    id,
                    vendor: JavaVendor::Zulu,
                    version,
                    package_type: request.package_type,
                })
            })
            .collect())
    }

    /// id 即下载地址，直接解析，不再回查 API
    ///
    /// 回查用的 `package_uuid` 过滤会被 Azul 静默忽略，返回的是未过滤的默认列表，
    /// 取其中第一项会拿到任意平台的包（实测第一项是 macOS aarch64 的 FX JRE）。
    async fn resolve_binary(&self, package: &JavaPackage) -> Result<ResolvedJava, SourceError> {
        super::resolved_from_id(package)
    }
}

impl AzulSource {
    async fn get<T>(&self, url: &str) -> Result<T, SourceError>
    where
        T: serde::de::DeserializeOwned,
    {
        get_json(&self.http, url, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::java::{JavaMajor, JavaPackageType};

    fn package(id: &str) -> JavaPackage {
        JavaPackage {
            id: id.to_owned(),
            vendor: JavaVendor::Zulu,
            major: JavaMajor::parse(27).expect("27"),
            version: "27.0.0".into(),
            package_type: JavaPackageType::Jre,
        }
    }

    /// id 即下载地址：不再回查 API。Azul 会静默忽略 `package_uuid` 过滤，
    /// 回查拿到的是未过滤的默认列表，取第一项会下载到任意平台的包
    #[tokio::test]
    async fn resolve_uses_package_id_as_url() {
        let source = AzulSource::new().expect("source");
        let id = "https://cdn.azul.com/zulu/bin/zulu27.28.103-ca-fx-jre27.0.0-win_x64.zip";
        let resolved = source.resolve_binary(&package(id)).await.expect("resolve");
        assert_eq!(resolved.location.as_str(), id);
        assert_eq!(resolved.package.id, id);
    }

    #[tokio::test]
    async fn resolve_rejects_non_url_id() {
        let source = AzulSource::new().expect("source");
        let err = source
            .resolve_binary(&package("not-a-url"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, SourceError::Decode(_)));
    }
}
