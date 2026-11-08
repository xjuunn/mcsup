//! 下载源路由：一次选择、落盘复用、失败重选
//!
//! 把 `domain::mirror` 的纯规则接上 IO：探测（[`SourceProbe`]）、持久化
//! （[`SourceRouteStore`]），再以 [`RoutingDownloader`] 装饰真正的下载器，
//! 让全部字节下载共用同一套选源逻辑。
//!
//! 为什么是装饰器而不是在各调用点分别选源：下载只有 [`ArtifactDownloader`]
//! 一个入口，装饰它即覆盖服务端安装、Mod 内容、整合包、JDK 全部路径；在各
//! 调用点各写一次，新加一条下载路径就会漏掉选源，而漏掉是静默的——只是慢。
//!
//! 选择粒度为**上游面**（[`domain::mirror::Upstream`]）而不是全局一个值：
//! 没有哪个镜像覆盖全部上游面（BMCLAPI 没有 Modrinth CDN，MCIM 没有 Mojang
//! 的 maven），全局单值必然把某个面固定到一个不覆盖它的源上。

use domain::mirror::{self, Candidate, Catalog, Sample, SourceId};
use port::{
    ArtifactDownloader, CancellationToken, DownloadError, DownloadRequest, DownloadedArtifact,
    ProgressSink, SourceProbe, SourceRouteStore,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use url::Url;

/// 一次路由决策
#[derive(Debug, Clone)]
pub struct Route {
    /// 上游面键（持久化用的键）
    pub upstream: String,
    /// 选中的源
    pub source: SourceId,
    /// 选中源的展示名
    pub label: String,
    /// 候选地址，选中源排第一
    pub candidates: Vec<Candidate>,
    /// `true` 表示本次刚探测决定，`false` 表示复用已记录的选择
    pub decided: bool,
}

impl Route {
    /// 主地址
    #[must_use]
    pub fn primary(&self) -> Option<&Candidate> {
        self.candidates.first()
    }
}

/// 选源器
///
/// 三处状态，职责不重叠：
///
/// - `store`：跨进程持久化，进程重启后仍复用上次选择
/// - `cache`：进程内缓存，几百个文件的批量下载不该每个文件都读一次盘
/// - `locks`：单飞，同一上游面的并发首探只打一次探测，避免一次整合包安装
///   同时发出上百个探测请求（镜像站对并发探测会直接 429）
pub struct SourceRouter {
    catalog: Arc<Catalog>,
    probe: Arc<dyn SourceProbe>,
    store: Arc<dyn SourceRouteStore>,
    cache: RwLock<HashMap<String, SourceId>>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl std::fmt::Debug for SourceRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceRouter")
            .field("sources", &self.catalog.sources().len())
            .field("upstreams", &self.catalog.upstreams().len())
            .finish_non_exhaustive()
    }
}

impl SourceRouter {
    /// 组合目录表、探测器与持久化
    #[must_use]
    pub fn new(
        catalog: Arc<Catalog>,
        probe: Arc<dyn SourceProbe>,
        store: Arc<dyn SourceRouteStore>,
    ) -> Self {
        Self {
            catalog,
            probe,
            store,
            cache: RwLock::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// 目录表
    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// 地址所属的上游面键；不在表里返回 `None`
    #[must_use]
    pub fn upstream_key(&self, location: &Url) -> Option<String> {
        self.catalog
            .resolve(location.as_str())
            .map(|resolved| resolved.upstream_key().to_owned())
    }

    /// 给一个地址选源
    ///
    /// 返回 `None` 表示这个地址不在目录表里（未被收录的主机），调用方应保持
    /// 原地址不变：没有可选的东西，硬套一个源只会把能下的地址改坏。
    ///
    /// 探测失败与持久化失败都不会让下载失败：失败的源不进候选，落盘失败只
    /// 退化成本进程内有效。
    pub async fn route(&self, location: &Url) -> Option<Route> {
        let resolved = self.catalog.resolve(location.as_str())?;
        let key = resolved.upstream_key().to_owned();

        if let Some(chosen) = self.cached(&key) {
            return Some(self.finish(key, &resolved, chosen, false));
        }

        // 单飞：同一上游面只让一个任务探测，其余等它把结果写进缓存
        let gate = self.gate(&key);
        let _held = gate.lock_owned().await;
        if let Some(chosen) = self.cached(&key) {
            return Some(self.finish(key, &resolved, chosen, false));
        }

        let order: Vec<SourceId> = resolved
            .candidates
            .iter()
            .map(|candidate| candidate.source.clone())
            .collect();
        let samples = self.probe_all(&resolved.candidates).await;

        if let Some(chosen) = mirror::select(&order, &samples) {
            self.remember(&key, &chosen);
            return Some(self.finish(key, &resolved, chosen, true));
        }

        // 全部候选都没响应：本次退回官方，但**不落盘**
        //
        // 落盘就等于把「一次网络抖动」记成「官方最好」，此后再也不探测镜像；
        // 不落盘则下次重新探测，代价只是一次注定失败的探测。
        let fallback = resolved
            .candidates
            .iter()
            .find(|candidate| candidate.source.is_official())
            .or_else(|| resolved.candidates.first())?
            .source
            .clone();
        tracing::warn!(
            upstream = %key,
            fallback = %fallback.as_str(),
            "全部候选探测失败，本次退回该源，选择不落盘"
        );
        Some(self.finish(key, &resolved, fallback, true))
    }

    /// 报告一次失败：作废该上游面的选择，下次重新探测
    ///
    /// 只在可重试的失败上调用。资源本身不存在（404）时换源不会变成功，
    /// 作废选择只会白探一次。
    pub fn report_failure(&self, upstream: &str, source: &SourceId) {
        if let Ok(mut cache) = self.cache.write() {
            cache.remove(upstream);
        }
        if let Err(err) = self.store.forget(upstream) {
            tracing::warn!(
                upstream = %upstream,
                error = %err,
                "作废下载源选择失败，本进程内仍会重新探测"
            );
        }
        tracing::warn!(
            upstream = %upstream,
            source = %source.as_str(),
            "下载源失败，作废该选择并将在下次重新探测"
        );
    }

    /// 进程内缓存命中，未命中则读盘并回填
    fn cached(&self, key: &str) -> Option<SourceId> {
        if let Ok(cache) = self.cache.read()
            && let Some(source) = cache.get(key)
        {
            return Some(source.clone());
        }
        // 落盘失败不影响本次：读不到就当作没记录，重新探测
        let stored = self.store.route(key).ok().flatten()?;
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(key.to_owned(), stored.clone());
        }
        Some(stored)
    }

    fn remember(&self, key: &str, source: &SourceId) {
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(key.to_owned(), source.clone());
        }
        if let Err(err) = self.store.remember(key, source) {
            // 持久化是优化而非正确性：写不下去只意味着下次要重新探测
            tracing::warn!(
                upstream = %key,
                source = %source.as_str(),
                error = %err,
                "下载源选择落盘失败，本次仍按该源下载"
            );
        }
    }

    fn gate(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(locks.entry(key.to_owned()).or_default())
    }

    /// 并发探测全部候选，只返回有响应的
    async fn probe_all(&self, candidates: &[Candidate]) -> Vec<Sample> {
        let mut set = tokio::task::JoinSet::new();
        for candidate in candidates {
            let probe = Arc::clone(&self.probe);
            let source = candidate.source.clone();
            let location = candidate.location.clone();
            set.spawn(async move {
                probe
                    .probe(&location)
                    .await
                    .map(|latency| Sample { source, latency })
            });
        }
        let mut samples = Vec::with_capacity(candidates.len());
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Ok(sample)) => samples.push(sample),
                Ok(Err(err)) => tracing::debug!(error = %err, "候选探测失败，已排除"),
                // 探测任务 panic 不该让整次下载失败
                Err(err) => tracing::debug!(error = %err, "候选探测任务失败，已排除"),
            }
        }
        samples
    }

    fn finish(
        &self,
        upstream: String,
        resolved: &mirror::Resolved<'_>,
        chosen: SourceId,
        decided: bool,
    ) -> Route {
        let label = self.catalog.source(&chosen).map_or_else(
            || chosen.as_str().to_owned(),
            |source| source.label().to_owned(),
        );
        Route {
            upstream,
            candidates: mirror::preferred(&resolved.candidates, Some(&chosen)),
            source: chosen,
            label,
            decided,
        }
    }
}

/// 按路由结果改写请求：主地址换成选中源，其余候选进 `mirrors`
///
/// 候选全部留在 `mirrors` 而不是只给一个地址：源站与镜像站同时可用时下载量
/// 可以在不同主机间分摊，而且一个挂了还有下一个，不必等下一次探测。
fn routed(request: &DownloadRequest, route: &Route) -> DownloadRequest {
    let locations: Vec<Url> = route
        .candidates
        .iter()
        .filter_map(|candidate| Url::parse(&candidate.location).ok())
        .collect();
    let Some((primary, rest)) = locations.split_first() else {
        return request.clone();
    };
    let mut mirrors = rest.to_vec();
    // 调用方显式给的镜像排在路由候选之后，重复地址不重复尝试
    for extra in &request.mirrors {
        if extra != primary && !mirrors.contains(extra) {
            mirrors.push(extra.clone());
        }
    }
    DownloadRequest {
        location: primary.clone(),
        mirrors,
        ..request.clone()
    }
}

/// 重试的结论
enum Retried {
    /// 重探还是同一个源（或已取消），没有可换的目标
    Unchanged,
    /// 换了源并重下一次
    Ran(Result<DownloadedArtifact, DownloadError>),
}

/// 带选源能力的下载器
///
/// 装饰而非替换：真正的传输、分段、校验、并发都在内层实现里，这一层只决定
/// 「从哪儿下」，并在失败后重新决定一次。
pub struct RoutingDownloader {
    inner: Arc<dyn ArtifactDownloader>,
    router: Arc<SourceRouter>,
}

impl std::fmt::Debug for RoutingDownloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingDownloader")
            .field("router", &self.router)
            .finish_non_exhaustive()
    }
}

impl RoutingDownloader {
    /// 装饰一个下载器
    #[must_use]
    pub fn new(inner: Arc<dyn ArtifactDownloader>, router: Arc<SourceRouter>) -> Self {
        Self { inner, router }
    }

    /// 选源器
    #[must_use]
    pub fn router(&self) -> &Arc<SourceRouter> {
        &self.router
    }

    /// 路由一个请求，返回改写后的请求与决策
    async fn route_one(
        &self,
        request: &DownloadRequest,
        scope: Scope,
    ) -> (DownloadRequest, Option<Route>) {
        let Some(route) = self.router.route(&request.location).await else {
            tracing::debug!(
                location = %request.location,
                "地址不在下载源表内，按原地址下载"
            );
            return (request.clone(), None);
        };
        log_route(&route, scope);
        (routed(request, &route), Some(route))
    }

    /// 可重试的失败：作废旧选择后重新选源再下一次
    ///
    /// 只做一次。无限重试会把「源站真的挂了」变成不返回；重探仍是同一个源时
    /// 也不重试，换它没有意义。
    async fn retry_once(
        &self,
        request: &DownloadRequest,
        failed: &Route,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Retried {
        self.router.report_failure(&failed.upstream, &failed.source);
        let Some(next) = self.router.route(&request.location).await else {
            return Retried::Unchanged;
        };
        if next.source == failed.source {
            tracing::warn!(
                upstream = %next.upstream,
                source = %next.source.as_str(),
                "重新探测仍选中同一源，不再重试"
            );
            return Retried::Unchanged;
        }
        // 新选择也要落一行：换源是用户能观察到的行为变化
        tracing::info!(
            upstream = %next.upstream,
            from = %failed.source.as_str(),
            to = %next.source.as_str(),
            label = %next.label,
            "下载源失败，换源重试"
        );
        Retried::Ran(
            self.inner
                .download(&routed(request, &next), progress, cancel)
                .await,
        )
    }
}

#[async_trait::async_trait]
impl ArtifactDownloader for RoutingDownloader {
    async fn download(
        &self,
        request: &DownloadRequest,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<DownloadedArtifact, DownloadError> {
        let (prepared, route) = self.route_one(request, Scope::Single).await;
        let failed = match self.inner.download(&prepared, progress, cancel).await {
            Ok(done) => return Ok(done),
            Err(err) => err,
        };
        let Some(route) = route else {
            return Err(failed);
        };
        if !failed.retryable() || cancel.is_cancelled() {
            return Err(failed);
        }
        // 重试不再向进度汇报告：上一条流已报过一遍，重报会让整批总量回退
        match self.retry_once(request, &route, None, cancel).await {
            Retried::Unchanged => Err(failed),
            Retried::Ran(result) => result,
        }
    }

    async fn download_many(
        &self,
        requests: &[DownloadRequest],
        concurrency: usize,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Vec<Result<DownloadedArtifact, DownloadError>> {
        let mut prepared = Vec::with_capacity(requests.len());
        let mut chosen: Vec<Option<Route>> = Vec::with_capacity(requests.len());
        for request in requests {
            let (prepared_request, route) = self.route_one(request, Scope::Batch).await;
            prepared.push(prepared_request);
            chosen.push(route);
        }
        log_batch(&chosen);

        let mut results = self
            .inner
            .download_many(&prepared, concurrency, progress, cancel)
            .await;

        // 整批里可重试的失败项，重新选源后单点重下。
        // 不在批内重排：一个文件失败不该把已经成功的项再下一遍。
        for (index, outcome) in results.iter_mut().enumerate() {
            if outcome.is_ok() || cancel.is_cancelled() {
                continue;
            }
            let Some(route) = chosen[index].as_ref() else {
                continue;
            };
            let retryable = outcome.as_ref().err().is_some_and(DownloadError::retryable);
            if !retryable {
                continue;
            }
            if let Retried::Ran(Ok(done)) =
                self.retry_once(&requests[index], route, None, cancel).await
            {
                *outcome = Ok(done);
            }
        }
        results
    }
}

/// 日志范围：单次下载逐条记，批量下载汇总记
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Single,
    Batch,
}

/// 每次下载一行：用了哪个源
///
/// 单次下载是用户直接等的那一件事，按 `info` 记；批量下载上百个文件时逐条
/// 记会刷屏，逐条降为 `debug`，由 [`log_batch`] 给一行汇总。
fn log_route(route: &Route, scope: Scope) {
    match scope {
        Scope::Single => tracing::info!(
            upstream = %route.upstream,
            source = %route.source.as_str(),
            label = %route.label,
            decided = route.decided,
            "使用下载源"
        ),
        Scope::Batch => tracing::debug!(
            upstream = %route.upstream,
            source = %route.source.as_str(),
            label = %route.label,
            decided = route.decided,
            "使用下载源"
        ),
    }
}

/// 一批下载汇总成一行：各源各多少文件
fn log_batch(routes: &[Option<Route>]) {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for route in routes.iter().flatten() {
        let label = format!("{} ({})", route.label, route.upstream);
        match counts.iter_mut().find(|(name, _)| *name == label) {
            Some((_, count)) => *count += 1,
            None => counts.push((label, 1)),
        }
    }
    if counts.is_empty() {
        return;
    }
    let summary = counts
        .iter()
        .map(|(label, count)| format!("{label} x{count}"))
        .collect::<Vec<_>>()
        .join(", ");
    tracing::info!(files = routes.len(), "批量下载源: {summary}");
}

#[cfg(test)]
mod tests;
