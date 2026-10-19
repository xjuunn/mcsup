//! Java 下载源注册表：并发探测延迟排序 + 故障切换

use super::{adoptium::AdoptiumSource, azul::AzulSource, foojay::FoojaySource};
use domain::java::{JavaMajor, JavaPackageType};
use port::{JavaPackage, JavaPackageRequest, JavaReleaseSource, ResolvedJava, SourceError};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// 元数据探测超时
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// 按探测延迟排序后的源列表；下载失败时按序切换
pub struct JavaSourceRegistry {
    sources: Vec<Arc<dyn JavaReleaseSource>>,
}

impl std::fmt::Debug for JavaSourceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JavaSourceRegistry")
            .field("sources", &self.sources.len())
            .finish_non_exhaustive()
    }
}

impl JavaSourceRegistry {
    /// 组合已排序的源
    #[must_use]
    pub fn new(sources: Vec<Arc<dyn JavaReleaseSource>>) -> Self {
        Self { sources }
    }

    /// 当前启用的源名列表（按优先级）
    #[must_use]
    pub fn source_names(&self) -> Vec<&'static str> {
        self.sources.iter().map(|s| s.name()).collect()
    }

    /// 按优先级列出可用包；单源失败切换下一个，全部失败聚合错误
    ///
    /// # Errors
    ///
    /// 所有源均失败时返回最后一个 [`SourceError`]
    pub async fn list_packages(
        &self,
        request: &JavaPackageRequest,
    ) -> Result<Vec<JavaPackage>, SourceError> {
        let mut last_err = None;
        for source in &self.sources {
            match source.list_packages(request).await {
                Ok(packages) if !packages.is_empty() => {
                    info!(
                        source = source.name(),
                        count = packages.len(),
                        "java packages loaded"
                    );
                    return Ok(packages);
                }
                Ok(_) => {
                    warn!(source = source.name(), "empty package list, trying next");
                }
                Err(err) => {
                    warn!(source = source.name(), error = %err, "list failed, trying next");
                    last_err = Some(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            SourceError::Init(Box::new(std::io::Error::other("no java source available")))
        }))
    }

    /// 解析下载地址；单源失败切换下一个
    ///
    /// # Errors
    ///
    /// 所有源均失败时返回最后一个 [`SourceError`]
    pub async fn resolve_binary(&self, package: &JavaPackage) -> Result<ResolvedJava, SourceError> {
        let mut last_err = None;
        for source in &self.sources {
            match source.resolve_binary(package).await {
                Ok(resolved) => {
                    info!(source = source.name(), url = %resolved.location, "java binary resolved");
                    return Ok(resolved);
                }
                Err(err) => {
                    warn!(source = source.name(), error = %err, "resolve failed, trying next");
                    last_err = Some(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            SourceError::Init(Box::new(std::io::Error::other("no java source available")))
        }))
    }
}

/// 构建默认请求（当前 OS / 架构）
#[must_use]
pub fn default_request(major: JavaMajor, package_type: JavaPackageType) -> JavaPackageRequest {
    JavaPackageRequest {
        major,
        package_type,
        os: super::current_os().to_owned(),
        arch: super::current_arch().to_owned(),
    }
}

/// 并发探测各源元数据端点，按响应延迟升序排列
///
/// 探测失败的源不进入返回列表。
pub async fn rank_sources() -> JavaSourceRegistry {
    let mut candidates: Vec<Arc<dyn JavaReleaseSource>> = Vec::new();
    if let Ok(source) = FoojaySource::new() {
        candidates.push(Arc::new(source));
    }
    if let Ok(source) = AdoptiumSource::new() {
        candidates.push(Arc::new(source));
    }
    if let Ok(source) = AzulSource::new() {
        candidates.push(Arc::new(source));
    }
    rank_sources_from(candidates).await
}

/// 对给定候选源做延迟排序（测试可注入 mock）
pub(crate) async fn rank_sources_from(
    candidates: Vec<Arc<dyn JavaReleaseSource>>,
) -> JavaSourceRegistry {
    let mut probed = probe_all(candidates).await;
    probed.sort_by_key(|(latency, _)| *latency);
    JavaSourceRegistry::new(probed.into_iter().map(|(_, s)| s).collect())
}

/// 并发探测全部候选源；失败的被丢弃
async fn probe_all(
    candidates: Vec<Arc<dyn JavaReleaseSource>>,
) -> Vec<(Duration, Arc<dyn JavaReleaseSource>)> {
    let mut tasks = Vec::with_capacity(candidates.len());
    for source in candidates {
        tasks.push(tokio::spawn(async move {
            let latency = probe_latency(&source).await;
            (latency, source)
        }));
    }
    let mut results = Vec::new();
    for task in tasks {
        match task.await {
            Ok((Some(latency), source)) => {
                let latency_ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX);
                info!(source = source.name(), latency_ms, "source probed");
                results.push((latency, source));
            }
            Ok((None, source)) => {
                warn!(source = source.name(), "source probe failed, excluded");
            }
            Err(err) => {
                warn!(error = %err, "probe task join failed");
            }
        }
    }
    results
}

/// 对单个源发一次轻量请求量延迟；失败返回 `None`
async fn probe_latency(source: &Arc<dyn JavaReleaseSource>) -> Option<Duration> {
    let request = JavaPackageRequest {
        major: JavaMajor::parse(21).ok()?,
        package_type: JavaPackageType::Jdk,
        os: super::current_os().to_owned(),
        arch: super::current_arch().to_owned(),
    };
    let start = Instant::now();
    let result = tokio::time::timeout(PROBE_TIMEOUT, source.list_packages(&request)).await;
    match result {
        Ok(Ok(_)) => Some(start.elapsed()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct FakeSource {
        name: &'static str,
        fail: bool,
    }

    #[async_trait]
    impl JavaReleaseSource for FakeSource {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn list_packages(
            &self,
            _request: &JavaPackageRequest,
        ) -> Result<Vec<JavaPackage>, SourceError> {
            if self.fail {
                Err(SourceError::Status { status: 503 })
            } else {
                Ok(vec![JavaPackage {
                    id: "p1".into(),
                    vendor: domain::java::JavaVendor::Temurin,
                    major: JavaMajor::parse(21).expect("21"),
                    version: "21.0.5".into(),
                    package_type: JavaPackageType::Jdk,
                }])
            }
        }

        async fn resolve_binary(&self, package: &JavaPackage) -> Result<ResolvedJava, SourceError> {
            if self.fail {
                return Err(SourceError::Status { status: 503 });
            }
            Ok(ResolvedJava {
                package: package.clone(),
                location: url::Url::parse("https://example.com/jdk.tar.gz").expect("url"),
                expected_size: None,
            })
        }
    }

    #[tokio::test]
    async fn failover_skips_broken_source() {
        let sources: Vec<Arc<dyn JavaReleaseSource>> = vec![
            Arc::new(FakeSource {
                name: "broken",
                fail: true,
            }),
            Arc::new(FakeSource {
                name: "good",
                fail: false,
            }),
        ];
        let registry = JavaSourceRegistry::new(sources);
        let request = default_request(JavaMajor::parse(21).expect("21"), JavaPackageType::Jdk);
        let packages = registry.list_packages(&request).await.expect("packages");
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version, "21.0.5");
    }

    #[tokio::test]
    async fn all_sources_failing_returns_error() {
        let sources: Vec<Arc<dyn JavaReleaseSource>> = vec![Arc::new(FakeSource {
            name: "broken",
            fail: true,
        })];
        let registry = JavaSourceRegistry::new(sources);
        let request = default_request(JavaMajor::parse(21).expect("21"), JavaPackageType::Jdk);
        let err = registry
            .list_packages(&request)
            .await
            .expect_err("all fail");
        assert!(matches!(err, SourceError::Status { status: 503 }));
    }

    #[tokio::test]
    async fn rank_excludes_failing_sources() {
        let candidates: Vec<Arc<dyn JavaReleaseSource>> = vec![
            Arc::new(FakeSource {
                name: "broken",
                fail: true,
            }),
            Arc::new(FakeSource {
                name: "good",
                fail: false,
            }),
        ];
        let registry = rank_sources_from(candidates).await;
        assert_eq!(registry.source_names(), vec!["good"]);
    }
}
