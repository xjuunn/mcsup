//! Foojay Disco 聚合源：一个 API 覆盖 Temurin / Zulu / Liberica / Corretto 等

use crate::http::{build_client, get_json};
use async_trait::async_trait;
use domain::java::JavaVendor;
use domain::server::version::VersionId;
use port::{JavaPackage, JavaPackageRequest, JavaReleaseSource, ResolvedJava, SourceError};
use reqwest::Client;
use serde::Deserialize;
use url::Url;

const PACKAGES_URL: &str = "https://api.foojay.io/disco/v3.0/packages";

/// Foojay Disco 发行包来源
#[derive(Debug)]
pub(super) struct FoojaySource {
    http: Client,
}

impl FoojaySource {
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
struct PackagesResponse {
    result: Vec<PackageDto>,
}

#[derive(Debug, Deserialize)]
struct PackageDto {
    id: String,
    distribution: String,
    major_version: i64,
    java_version: String,
    filename: String,
    directly_downloadable: bool,
    /// 上游架构标识（`x64` / `aarch64` / `arm64` …）
    #[serde(default)]
    architecture: String,
    /// 上游操作系统标识
    #[serde(default)]
    operating_system: String,
    #[serde(default)]
    size: i64,
    links: Links,
}

#[derive(Debug, Deserialize)]
struct Links {
    pkg_download_redirect: String,
}

#[async_trait]
impl JavaReleaseSource for FoojaySource {
    fn name(&self) -> &'static str {
        "foojay"
    }

    async fn list_packages(
        &self,
        request: &JavaPackageRequest,
    ) -> Result<Vec<JavaPackage>, SourceError> {
        let url = Url::parse_with_params(
            PACKAGES_URL,
            [
                ("version", request.major.as_i64().to_string()),
                ("include_versions", "false".into()),
                ("include_downloads", "false".into()),
                // 参数名是 `architecture`；写成 `arch` 会被上游静默忽略，
                // 返回全架构的包，进而把 x64 的 JVM 装到 aarch64 上
                ("architecture", request.arch.clone()),
                ("operating_system", request.os.clone()),
                ("package_type", request.package_type.as_str().into()),
                ("latest", "available".into()),
                ("directly_downloadable", "true".into()),
                ("free_use_in_production", "true".into()),
                ("release_status", "ga".into()),
                ("limit", "200".into()),
            ],
        )
        .map_err(|err| SourceError::Decode(Box::new(err)))?;
        let response: PackagesResponse = self.get(url.as_str()).await?;
        Ok(response
            .result
            .into_iter()
            .filter(|pkg| {
                pkg.directly_downloadable
                    && !pkg.filename.is_empty()
                    && super::is_deployable_archive(&pkg.filename)
            })
            // 上游过滤不可全信：同一 id 回查、镜像差异都可能带出别的架构，
            // 这里按声明的架构与文件名双重复核
            .filter(|pkg| {
                super::arch_matches(&request.arch, &pkg.architecture)
                    && super::filename_arch_matches(&request.arch, &pkg.filename)
            })
            .filter(|pkg| {
                pkg.operating_system.is_empty()
                    || super::os_matches(&request.os, &pkg.operating_system)
            })
            .filter_map(|pkg| {
                Some(JavaPackage {
                    major: super::parse_major_or_drop(pkg.major_version, "foojay", &pkg.id)?,
                    id: pkg.id,
                    vendor: JavaVendor::parse_fuzzy(&pkg.distribution),
                    version: pkg.java_version,
                    package_type: request.package_type,
                })
            })
            .collect())
    }

    async fn resolve_binary(&self, package: &JavaPackage) -> Result<ResolvedJava, SourceError> {
        // 用 id 回查拿完整下载信息
        let url = format!("{PACKAGES_URL}/{}", package.id);
        let response: PackagesResponse = self.get(&url).await?;
        let pkg = response
            .result
            .into_iter()
            .next()
            .ok_or_else(|| SourceError::NotFound(VersionId::new(package.major.to_string())))?;
        let location = Url::parse(&pkg.links.pkg_download_redirect)
            .map_err(|err| SourceError::Decode(Box::new(err)))?;
        let expected_size = u64::try_from(pkg.size).ok().filter(|s| *s > 0);
        Ok(ResolvedJava {
            package: package.clone(),
            location,
            expected_size,
        })
    }
}

impl FoojaySource {
    async fn get<T>(&self, url: &str) -> Result<T, SourceError>
    where
        T: serde::de::DeserializeOwned,
    {
        get_json(&self.http, url, None).await
    }
}
