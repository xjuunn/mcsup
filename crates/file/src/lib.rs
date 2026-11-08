mod extract;
mod inspect;
mod nbt;

pub use extract::CommandExtractor;
pub use inspect::FileCoreInspector;

use async_trait::async_trait;
use domain::server::artifact::ChecksumAlgorithm;
use futures::{StreamExt, stream};
use port::{
    ArtifactDownloader, ByteSource, CancellationToken, DownloadError, DownloadRequest,
    DownloadedArtifact, ProgressSink, RangeSource, RemoteStat, SourceError,
};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::collections::HashSet;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use url::Url;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
/// 分段并发上限
const SEGMENT_CONCURRENCY: usize = 24;
/// 单段最小字节数，低于它不值得分段建立新建连接
const SEGMENT_MIN_BYTES: u64 = 2 * 1024 * 1024;
/// 整体小于该大小直接单流
const MIN_SEGMENTED_SIZE: u64 = 8 * 1024 * 1024;
/// 段最大尝试次数（含首次）
const SEGMENT_MAX_ATTEMPTS: u32 = 3;
/// 重发延迟
const SEGMENT_RETRY_DELAY: Duration = Duration::from_millis(250);
/// 取消标志的轮询粒度
const CANCEL_POLL: Duration = Duration::from_millis(25);
/// 批量下载未指定并发时的默认值
///
/// 与单文件内分段并发（[`SEGMENT_CONCURRENCY`]）是两层：批量并行让多个小文件
/// 同时在途，分段并行让单个大文件跑满一条链路。两者按场景各自生效，不叠加。
const BATCH_CONCURRENCY: usize = 8;
/// 文件级重试的最多尝试次数（含首次）
///
/// 清单类批量下载动辄数百个请求，瞬态 5xx / 连接重置必然出现；整批因此报废
/// 重来代价极高，值得给每个文件几次机会
const FILE_MAX_ATTEMPTS: u32 = 3;
/// 文件级重试退避基数（随尝试次数线性放大）
const FILE_RETRY_DELAY: Duration = Duration::from_millis(200);
/// 批量下载里的单文件进度换算
///
/// 每个文件独立的 `download` 回调报的是「本文件从 0 开始已传字节」，直接透传
/// 到批量进度条就是反复归零。本结构把它换算成**整批累计值**：
///
/// - `accounted` 记录本文件本轮尝试已计入的增量，用来算 delta，避免同一批字节
///   被重复累加（透传绝对值会重复计数：并发时 A 文件完成一跳，B 文件仍按旧基数
///   上报，总量立刻虚高）
/// - 重试作废上一轮时，调用方按 `accounted` 精确退还，不让作废的字节留在总数里
struct BatchSink<'a> {
    /// 整批已完成字节（所有文件共享）
    done: Arc<AtomicU64>,
    /// 本文件本轮尝试已计入的字节
    accounted: Arc<AtomicU64>,
    /// 整批声明总字节
    total: Option<u64>,
    /// 上层 sink
    upstream: &'a dyn ProgressSink,
}

impl ProgressSink for BatchSink<'_> {
    fn on_progress(&self, transferred: u64, _total: Option<u64>) {
        // 分段任务并发写同一个计数器，可能出现回退观测；饱和减法保证不为负
        let previous = self.accounted.swap(transferred, Ordering::Relaxed);
        let delta = transferred.saturating_sub(previous);
        if delta == 0 {
            return;
        }
        let done = self.done.fetch_add(delta, Ordering::Relaxed) + delta;
        self.upstream.on_progress(done, self.total);
    }
}

/// 取消可打断的网络等待
///
/// [`CancellationToken`] 是原子标志、没有唤醒机制，只能轮询；而建连、跟随
/// 重定向、首字节、重试退避这些等待可能持续数百毫秒到数秒，期间网络 future
/// 不返回，循环里的取消检查就一直执行不到——Ctrl-C 后观感是「下载又继续了
/// 一秒」。把网络 future 与 [`wait_cancelled`] 竞争，取消在轮询粒度内立即
/// 生效，并丢弃进行中的请求（连接随之中止）。
///
/// `cancel` 是令牌列表：用户取消（Ctrl-C）与分段停止令牌都需要能打断等待，
/// 任一置位即返回 [`DownloadError::Cancelled`]。
async fn until_cancelled<T>(
    future: impl Future<Output = T>,
    cancel: &[&CancellationToken],
) -> Result<T, DownloadError> {
    tokio::select! {
        biased;
        () = wait_cancelled(cancel) => Err(DownloadError::Cancelled),
        value = future => Ok(value),
    }
}

/// 轮询到任一令牌被置位为止
async fn wait_cancelled(cancel: &[&CancellationToken]) {
    while !cancel.iter().any(|token| token.is_cancelled()) {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

/// 下载的传输算法
///
/// 选择规则集在 [`FileDownloader::plan`]; 将来新增算法，只需要扩展本枚举
enum TransferPlan {
    /// 单流顺序写入；`total` 是探测到的总字节数，仅用于进度总长
    Single { total: Option<u64> },
    /// 分段并发写入
    Segmented {
        range: Arc<dyn RangeSource>,
        stat: RemoteStat,
    },
}

/// 一个分段任务的全部输入
struct SegmentTask {
    range_source: Arc<dyn RangeSource>,
    location: Url,
    temp_path: PathBuf,
    bounds: RangeInclusive<u64>,
    /// 全局在途许可；`None` 表示不限（见 [`FileDownloader::with_inflight_limit`]）
    inflight: Option<Arc<Semaphore>>,
    /// 本段在 [`plan_segments`] 结果里的下标，写完时报给编排侧记账
    index: usize,
}

/// 编排 future 被丢弃时通知分段任务尽快退出
struct StopOnDrop(CancellationToken);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// 基于 [`ByteSource`] 的下载器
pub struct FileDownloader {
    source: Arc<dyn ByteSource>,
    range: Option<Arc<dyn RangeSource>>,
    timeout: Duration,
    /// 全部在途连接的上限（批量并行 × 分段并行 × 重试都在它之下）
    ///
    /// 两层并发各自有界，乘起来没有界：8 个文件各分 24 段就是 192 条连接，
    /// 超过多数家庭带宽与上游的连接上限，反而让每条约到的速率更低。
    /// `None` 表示不限，保持注入自定义来源时的既有行为。
    inflight: Option<Arc<Semaphore>>,
}

impl std::fmt::Debug for FileDownloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileDownloader")
            .field("timeout", &self.timeout)
            .field(
                "inflight_limit",
                &self
                    .inflight
                    .as_ref()
                    .map(|semaphore| semaphore.available_permits()),
            )
            .finish_non_exhaustive()
    }
}

impl FileDownloader {
    /// 构建下载器
    #[must_use]
    pub fn new(source: Arc<dyn ByteSource>) -> Self {
        Self {
            source,
            range: None,
            timeout: DEFAULT_TIMEOUT,
            inflight: None,
        }
    }

    /// 构建下载器
    #[must_use]
    pub fn with_timeout(source: Arc<dyn ByteSource>, timeout: Duration) -> Self {
        Self {
            source,
            range: None,
            timeout,
            inflight: None,
        }
    }

    /// 限制全部在途连接数
    ///
    /// 与批量并发、分段并发是三个独立维度：那两个决定「排多少条队」，
    /// 这个决定「同时放几条真的出去」。`0` 视为不限——配置层已经挡掉了 0，
    /// 真传进来时按不限处理比让下载永久阻塞安全。
    #[must_use]
    pub fn with_inflight_limit(mut self, limit: usize) -> Self {
        self.inflight = (limit > 0).then(|| Arc::new(Semaphore::new(limit)));
        self
    }

    /// 注入按字节范围读取的能力
    ///
    /// 注入后大文件将分段并发下载；上游不支持范围或者探测失败时自动回退单流
    #[must_use]
    pub fn with_range_source(mut self, range: Arc<dyn RangeSource>) -> Self {
        self.range = Some(range);
        self
    }

    /// 依据来源能力与文件大小选择透传算法
    ///
    /// 以下任一条件成立即回退单流：未注入范围能力、元信息探测失败、上游未声明范围支持、
    /// 对象小于 [`MIN_SEGMENTED_SIZE`]、声明大小与元信息不一致。回退只损失分段加速，
    /// 不影响正确性，因此探测失败不额外记录日志。
    ///
    /// 探测等待可被打断：取消发生在探测期间时直接返回错误，不退化成单流
    /// （退化会再发起一次注定被取消的建连）
    async fn plan(
        &self,
        request: &DownloadRequest,
        cancel: &CancellationToken,
    ) -> Result<TransferPlan, DownloadError> {
        // 未注入范围能力或探测失败：总长退化为调用方声明的大小
        let Some(range) = self.range.clone() else {
            return Ok(TransferPlan::Single {
                total: request.expected_size,
            });
        };
        let Ok(stat) = until_cancelled(range.stat(&request.location), &[cancel]).await? else {
            return Ok(TransferPlan::Single {
                total: request.expected_size,
            });
        };
        // 探测到的总长比 expected_size 可靠（多数源不声明大小），进度总长优先用它
        let known = Some(stat.size);
        if !stat.supports_range || stat.size < MIN_SEGMENTED_SIZE {
            return Ok(TransferPlan::Single { total: known });
        }
        if request
            .expected_size
            .is_some_and(|expected| expected != stat.size)
        {
            return Ok(TransferPlan::Single { total: known });
        }
        Ok(TransferPlan::Segmented { range, stat })
    }

    async fn transfer(
        &self,
        temp_path: &Path,
        request: &DownloadRequest,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<u64, DownloadError> {
        match self.plan(request, cancel).await? {
            TransferPlan::Single { total } => {
                self.transfer_single(temp_path, request, total, progress, cancel)
                    .await
            }
            TransferPlan::Segmented { range, stat } => {
                self.transfer_segmented(range, stat, temp_path, request, progress, cancel)
                    .await
            }
        }
    }

    /// 单流顺序写入
    ///
    /// `total` 是调用方（[`Self::plan`]）探测到的总字节数，用于进度条总长；
    /// 未知时进度回显退化为 spinner + 已传字节
    async fn transfer_single(
        &self,
        temp_path: &Path,
        request: &DownloadRequest,
        total: Option<u64>,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<u64, DownloadError> {
        // 拿在途许可也要可打断：排队等到一半用户按了 Ctrl-C 不能继续等
        let _permit = match self.inflight.as_ref() {
            Some(semaphore) => Some(
                until_cancelled(Arc::clone(semaphore).acquire_owned(), &[cancel])
                    .await
                    .map_err(|_| DownloadError::Cancelled)?
                    .map_err(|_| DownloadError::Cancelled)?,
            ),
            None => None,
        };
        // 建连 / 跟随重定向 / 响应头都在这里等待，取消必须能打断它
        let mut reader = until_cancelled(self.source.open(&request.location), &[cancel]).await??;
        let mut file = tokio::fs::File::create(temp_path)
            .await
            .map_err(DownloadError::Io)?;
        let mut hasher = match request.expected.as_ref() {
            Some(checksum) => Some(Hasher::new(checksum.algorithm())?),
            None => None,
        };
        let mut transferred: u64 = 0;
        loop {
            // 等字节同样可打断：服务端停顿时 Ctrl-C 不能等到读超时才生效
            let Some(chunk) = until_cancelled(reader.next_chunk(), &[cancel]).await?? else {
                break;
            };
            if let Some(hasher) = hasher.as_mut() {
                hasher.update(&chunk);
            }
            file.write_all(&chunk).await.map_err(DownloadError::Io)?;
            transferred += chunk.len() as u64;
            if let Some(sink) = progress {
                sink.on_progress(transferred, total);
            }
        }
        file.flush().await.map_err(DownloadError::Io)?;
        file.sync_all().await.map_err(DownloadError::Io)?;
        verify(request, hasher, transferred)?;
        Ok(transferred)
    }

    async fn transfer_segmented(
        &self,
        range: Arc<dyn RangeSource>,
        stat: RemoteStat,
        temp_path: &Path,
        request: &DownloadRequest,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<u64, DownloadError> {
        let total = stat.size;
        let bounds_list = plan_segments(total);
        let marks_path = marks_path_for(temp_path);
        let done = prepare_partial(temp_path, &marks_path, total, bounds_list.len()).await?;

        let stop = CancellationToken::new();
        let _stop_guard = StopOnDrop(stop.clone());
        let transferred = Arc::new(AtomicU64::new(0));
        // 已完成的段先把字节记进进度，否则续传时进度条会从 0 重来一遍
        for index in &done {
            if let Some(bounds) = bounds_list.get(*index) {
                transferred.fetch_add(bounds.end() - bounds.start() + 1, Ordering::Relaxed);
            }
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Option<usize>>();
        let mut handles = Vec::with_capacity(bounds_list.len() - done.len());
        for (index, bounds) in bounds_list.iter().enumerate() {
            if done.contains(&index) {
                continue;
            }
            let task = SegmentTask {
                range_source: range.clone(),
                location: request.location.clone(),
                temp_path: temp_path.to_path_buf(),
                bounds: bounds.clone(),
                inflight: self.inflight.clone(),
                index,
            };
            handles.push(tokio::spawn(run_segment(
                task,
                transferred.clone(),
                tx.clone(),
                cancel.clone(),
                stop.clone(),
            )));
        }
        drop(tx);

        let progress_total = request.expected_size.or(Some(stat.size));
        record_segments(
            rx,
            &marks_path,
            done.is_empty(),
            &transferred,
            progress_total,
            progress,
        )
        .await?;
        let mut cancelled = false;
        let mut failure: Option<DownloadError> = None;
        for handle in handles {
            match handle.await {
                Err(err) => return Err(DownloadError::Io(std::io::Error::other(err))),
                Ok(Ok(())) => {}
                Ok(Err(DownloadError::Cancelled)) => cancelled = true,
                Ok(Err(err)) => {
                    if failure.is_none() {
                        failure = Some(err);
                    }
                }
            }
        }
        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        if let Some(err) = failure {
            if matches!(
                err,
                DownloadError::Source(SourceError::Status { status: 200 | 416 })
            ) {
                return self
                    .transfer_single(temp_path, request, Some(stat.size), progress, cancel)
                    .await;
            }
            return Err(err);
        }
        if cancelled {
            return Err(DownloadError::Cancelled);
        }
        let transferred = transferred.load(Ordering::Relaxed);
        sync_quiet(temp_path).await?;
        let hasher = match request.expected.as_ref() {
            Some(checksum) => Some(hash_file(temp_path, checksum.algorithm()).await?),
            None => None,
        };
        verify(request, hasher, transferred)?;
        Ok(transferred)
    }
}

#[async_trait]
impl ArtifactDownloader for FileDownloader {
    async fn download(
        &self,
        request: &DownloadRequest,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<DownloadedArtifact, DownloadError> {
        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        let destination = request.destination.clone();
        let parent = destination
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .ok_or_else(|| invalid_input("destination has no parent directory"))?;
        tokio::fs::create_dir_all(&parent)
            .await
            .map_err(DownloadError::Io)?;
        let temp_path = temp_path_for(&destination, request.expected_size);
        let marks_path = marks_path_for(&temp_path);
        let outcome = tokio::time::timeout(
            self.timeout,
            self.transfer(&temp_path, request, progress, cancel),
        )
        .await;
        let size = match outcome {
            Ok(Ok(size)) => size,
            Ok(Err(err)) => {
                discard_if_unusable(&temp_path, &marks_path, &err).await;
                return Err(err);
            }
            Err(_elapsed) => {
                // 超时是可重试的瞬态失败，残片留着供下一轮续传
                return Err(DownloadError::Timeout);
            }
        };
        if let Err(err) = tokio::fs::rename(&temp_path, &destination).await {
            remove_quietly(&temp_path).await;
            remove_quietly(&marks_path).await;
            return Err(DownloadError::Io(err));
        }
        // 落盘成功，段记录随之作废；留着会让下一次同目标的下载误以为已经写完
        remove_quietly(&marks_path).await;
        Ok(DownloadedArtifact {
            path: destination,
            size,
        })
    }

    /// 批量下载：有界并发 + 整批进度 + 单文件瞬态重试
    ///
    /// 三件事叠加起来才谈得上「快」：
    ///
    /// 1. **并发**：批量里的文件彼此独立，串行时链路大部分时间在等首字节；
    ///    有界并发让多个文件的重叠等待段并起来，聚合吞吐成倍提升
    /// 2. **整批进度**：`BatchSink` 把每个文件的从零进度换算成整批累计值，
    ///    进度条一条线走到底，不再被单个文件的完成反复归零
    /// 3. **瞬态重试**：批量动辄数百请求，偶发 5xx / 连接重置不该毁掉整批；
    ///    只对 [`DownloadError::retryable`] 的失败重试，校验失败与取消不重试
    ///
    /// 结果顺序与入参一致，某个文件彻底失败不影响其余文件继续下载。
    async fn download_many(
        &self,
        requests: &[DownloadRequest],
        concurrency: usize,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Vec<Result<DownloadedArtifact, DownloadError>> {
        let total: u64 = requests.iter().filter_map(|r| r.expected_size).sum();
        let all_known = requests.iter().all(|r| r.expected_size.is_some());
        let total = all_known.then_some(total);
        let done = Arc::new(AtomicU64::new(0));
        if let Some(sink) = progress {
            sink.on_progress(0, total);
        }
        // 并发为 0 时取默认值：调用方只想「并行」而不关心具体路数
        let concurrency = if concurrency == 0 {
            BATCH_CONCURRENCY
        } else {
            concurrency
        }
        .min(requests.len().max(1));

        // 先把每个文件的 future 实体化到 Vec，再交给 stream：
        // 直接在 `stream::iter(iter.map(..))` 里构造会让闭包绑定到某一个具体
        // 生命周期，`buffer_unordered` 要求它对任意生命周期成立，编译器会报
        // 「FnOnce is not general enough」。收集到 Vec 后生命周期在构建时即固定。
        let pending: Vec<_> = requests
            .iter()
            .enumerate()
            .map(|(index, request)| {
                let done = Arc::clone(&done);
                async move {
                    let outcome = self
                        .download_with_retry(request, &done, total, progress, cancel)
                        .await;
                    (index, outcome)
                }
            })
            .collect();

        let mut outcomes: Vec<(usize, Result<DownloadedArtifact, DownloadError>)> =
            stream::iter(pending)
                .buffer_unordered(concurrency)
                .collect()
                .await;

        // 并发完成顺序是乱的，按入参下标归位，保证结果与调用方预期一一对应
        outcomes.sort_by_key(|(index, _)| *index);
        outcomes.into_iter().map(|(_, outcome)| outcome).collect()
    }
}

impl FileDownloader {
    /// 带瞬态重试的单文件下载
    ///
    /// 重试时把上一轮尝试已计入 `done` 的字节精确退还：作废的传输不该留在进度
    /// 总数里，否则进度条会虚高甚至在重传后越过 100%。
    async fn download_with_retry(
        &self,
        request: &DownloadRequest,
        done: &Arc<AtomicU64>,
        total: Option<u64>,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<DownloadedArtifact, DownloadError> {
        let mut attempt = 1_u32;
        loop {
            // 每轮尝试重新计数：上一轮的增量已在失败分支退还
            let accounted = Arc::new(AtomicU64::new(0));
            let batch = progress.map(|upstream| BatchSink {
                done: Arc::clone(done),
                accounted: Arc::clone(&accounted),
                total,
                upstream,
            });
            let sink = batch.as_ref().map(|sink| sink as &dyn ProgressSink);
            match self.download(request, sink, cancel).await {
                Ok(artifact) => return Ok(artifact),
                Err(err)
                    if err.retryable() && attempt < FILE_MAX_ATTEMPTS && !cancel.is_cancelled() =>
                {
                    // 退还本轮已计入的增量：这些字节随失败作废，重传时会重新计入
                    let refund = accounted.load(Ordering::Relaxed);
                    if refund > 0 {
                        done.fetch_sub(refund, Ordering::Relaxed);
                    }
                    tokio::time::sleep(FILE_RETRY_DELAY * attempt).await;
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

/// 把总量切成闭区间列表
fn plan_segments(total: u64) -> Vec<RangeInclusive<u64>> {
    let count = (total / SEGMENT_MIN_BYTES).clamp(1, SEGMENT_CONCURRENCY as u64);
    let chunk = total / count;
    (0..count)
        .map(|index| {
            let start = index * chunk;
            let end = if index == count - 1 {
                total - 1
            } else {
                start + chunk - 1
            };
            start..=end
        })
        .collect()
}

/// 单段任务
async fn run_segment(
    task: SegmentTask,
    transferred: Arc<AtomicU64>,
    progress: UnboundedSender<Option<usize>>,
    cancel: CancellationToken,
    stop: CancellationToken,
) -> Result<(), DownloadError> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match fetch_segment(&task, transferred.clone(), &progress, &cancel, &stop).await {
            Ok(()) => {
                // 记账交给编排侧：并发写同一个记录文件会互相盖掉
                let _ = progress.send(Some(task.index));
                return Ok(());
            }
            Err(err) => {
                if attempt >= SEGMENT_MAX_ATTEMPTS || !err.retryable() {
                    stop.cancel();
                    return Err(err);
                }
                // 按段起点与尝试次数确定性错开，避免所有段同时重试；
                // 退避也要可打断：Ctrl-C 不等退避结束
                let jitter_ms = (task.bounds.start() % 200) + (u64::from(attempt) * 37 % 200);
                let delay = SEGMENT_RETRY_DELAY * attempt + Duration::from_millis(jitter_ms);
                if until_cancelled(tokio::time::sleep(delay), &[&cancel, &stop])
                    .await
                    .is_err()
                {
                    // 只有取消（用户 Ctrl-C 或兄弟段失败的 stop）能到这里，
                    // 两个令牌都已置位，无需再通知
                    return Err(DownloadError::Cancelled);
                }
            }
        }
    }
}

/// 拉取一个区间并写入对应偏移；失败时会退本次尝试的进度计数
async fn fetch_segment(
    task: &SegmentTask,
    transferred: Arc<AtomicU64>,
    progress: &UnboundedSender<Option<usize>>,
    cancel: &CancellationToken,
    stop: &CancellationToken,
) -> Result<(), DownloadError> {
    if cancel.is_cancelled() || stop.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }
    // 拿在途许可也要可打断：排队等到一半用户按了 Ctrl-C 不能继续等
    let _permit = match task.inflight.as_ref() {
        Some(semaphore) => Some(
            until_cancelled(Arc::clone(semaphore).acquire_owned(), &[cancel, stop])
                .await
                .map_err(|_| DownloadError::Cancelled)?
                .map_err(|_| DownloadError::Cancelled)?,
        ),
        None => None,
    };
    // 段连接的建立也要可打断：取消发生在建连期间不等响应头
    let mut reader = until_cancelled(
        task.range_source
            .open_range(&task.location, task.bounds.clone()),
        &[cancel, stop],
    )
    .await??;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&task.temp_path)
        .await
        .map_err(DownloadError::Io)?;
    file.seek(std::io::SeekFrom::Start(*task.bounds.start()))
        .await
        .map_err(DownloadError::Io)?;
    let expected = task.bounds.end() - task.bounds.start() + 1;
    let mut got: u64 = 0;
    let outcome: Result<(), DownloadError> = async {
        loop {
            let Some(chunks) = until_cancelled(reader.next_chunk(), &[cancel, stop]).await?? else {
                break;
            };
            file.write_all(&chunks).await.map_err(DownloadError::Io)?;
            got += chunks.len() as u64;
            transferred.fetch_add(chunks.len() as u64, Ordering::Relaxed);
            // `None` 只表示「有字节进账」，让编排侧刷新进度；
            // 段真正写完由 `run_segment` 发 `Some(下标)` 记账
            let _ = progress.send(None);
        }
        if got != expected {
            return Err(DownloadError::Source(SourceError::Network(Box::new(
                std::io::Error::other(format!("short segment read: {got} of {expected}")),
            ))));
        }
        Ok(())
    }
    .await;
    if outcome.is_err() && got > 0 {
        transferred.fetch_sub(got, Ordering::Relaxed);
        let _ = progress.send(None);
    }
    outcome
}

async fn hash_file(path: &Path, algorithm: ChecksumAlgorithm) -> Result<Hasher, DownloadError> {
    use tokio::io::AsyncReadExt;
    let mut hasher = Hasher::new(algorithm)?;
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(DownloadError::Io)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await.map_err(DownloadError::Io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher)
}

async fn sync_quiet(path: &Path) -> Result<(), DownloadError> {
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .await
        .map_err(DownloadError::Io)?;
    file.sync_all().await.map_err(DownloadError::Io)
}

fn verify(
    request: &DownloadRequest,
    hasher: Option<Hasher>,
    transferred: u64,
) -> Result<(), DownloadError> {
    if let (Some(expected), Some(hasher)) = (request.expected.as_ref(), hasher) {
        let actual = hasher.finalize_hex();
        if !expected.matches(&actual) {
            return Err(DownloadError::ChecksumMismatch {
                expected: expected.hex().to_owned(),
                actual,
            });
        }
    }
    if let Some(expected) = request.expected_size
        && expected != transferred
    {
        return Err(DownloadError::SizeMismatch {
            expected,
            actual: transferred,
        });
    }
    Ok(())
}

enum Hasher {
    Sha1(Sha1),
    Sha256(Sha256),
}

impl Hasher {
    fn new(algorithm: ChecksumAlgorithm) -> Result<Self, DownloadError> {
        match algorithm {
            ChecksumAlgorithm::Sha1 => Ok(Self::Sha1(Sha1::new())),
            ChecksumAlgorithm::Sha256 => Ok(Self::Sha256(Sha256::new())),
            _ => Err(DownloadError::UnsupportedAlgorithm),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha1(hasher) => hasher.update(data),
            Self::Sha256(hasher) => hasher.update(data),
        }
    }

    fn finalize_hex(self) -> String {
        match self {
            Self::Sha1(hasher) => to_hex(&hasher.finalize()),
            Self::Sha256(hasher) => to_hex(&hasher.finalize()),
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

async fn remove_quietly(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

fn invalid_input(message: &str) -> DownloadError {
    DownloadError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message,
    ))
}

/// 失败时按 [`discards_partial`] 决定是否清掉残片与段记录
async fn discard_if_unusable(temp_path: &Path, marks_path: &Path, err: &DownloadError) {
    if discards_partial(err) {
        remove_quietly(temp_path).await;
        remove_quietly(marks_path).await;
    }
}

/// 收段完成事件：记账并刷新进度，直到所有段任务结束
///
/// `fresh` 为真表示这一轮不是续传：残片已经重建，旧记录必须先截断，
/// 否则上一轮的段序号会被下一轮当成已写完的段直接跳过。
async fn record_segments(
    mut rx: UnboundedReceiver<Option<usize>>,
    marks_path: &Path,
    fresh: bool,
    transferred: &Arc<AtomicU64>,
    progress_total: Option<u64>,
    progress: Option<&dyn ProgressSink>,
) -> Result<(), DownloadError> {
    let mut marks_file = if fresh {
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(marks_path)
            .await
    } else {
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(marks_path)
            .await
    }
    .map_err(DownloadError::Io)?;
    while let Some(event) = rx.recv().await {
        if let Some(index) = event {
            marks_file
                .write_all(format!("{index}\n").as_bytes())
                .await
                .map_err(DownloadError::Io)?;
            marks_file.flush().await.map_err(DownloadError::Io)?;
        }
        if let Some(sink) = progress {
            sink.on_progress(transferred.load(Ordering::Relaxed), progress_total);
        }
    }
    if let Some(sink) = progress {
        sink.on_progress(transferred.load(Ordering::Relaxed), progress_total);
    }
    Ok(())
}

/// 打开（或重建）残片，返回可以跳过的段
///
/// 残片只有在长度与上游一致时才可信：长度不符说明是别的版本或上次写坏了，
/// 此时整份重来。记录读不懂同样按「没有可续进度」处理——它只是加速手段，
/// 不该让下载失败。
async fn prepare_partial(
    temp_path: &Path,
    marks_path: &Path,
    total: u64,
    segments: usize,
) -> Result<HashSet<usize>, DownloadError> {
    let complete = tokio::fs::metadata(temp_path)
        .await
        .is_ok_and(|meta| meta.len() == total);
    let done: HashSet<usize> = if complete {
        read_marks(marks_path)
            .await
            .into_iter()
            .filter(|index| *index < segments)
            .collect()
    } else {
        HashSet::new()
    };
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(done.is_empty())
        .open(temp_path)
        .await
        .map_err(DownloadError::Io)?;
    file.set_len(total).await.map_err(DownloadError::Io)?;
    Ok(done)
}

/// 下载中的临时文件路径
///
/// 刻意**确定**（不再用随机名）：同一目标的重试与跨进程重跑都要落回同一个
/// 文件，才谈得上接着上次写完的段续传。名字里带上清单声明的大小，换了版本
/// 就不会误用上一版的残片；若上游改了内容而大小恰好相同，收尾的整体校验会
/// 以 `ChecksumMismatch` 拒绝它——只浪费一次传输，不会落盘错内容。
fn temp_path_for(destination: &Path, expected_size: Option<u64>) -> PathBuf {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    let suffix = expected_size.map_or_else(|| "part".to_owned(), |size| format!("{size}.part"));
    destination.with_file_name(format!(".{name}.{suffix}"))
}

/// 段完成记录：每行一个已写完的段序号
fn marks_path_for(temp_path: &Path) -> PathBuf {
    let mut marks = temp_path.as_os_str().to_owned();
    marks.push(".marks");
    PathBuf::from(marks)
}

/// 读已完成的段序号
///
/// 记录缺失或内容不可解析都当作「没有可续的进度」：这份记录只是加速手段，
/// 读不懂就从零重来，不该让下载失败。
async fn read_marks(path: &Path) -> HashSet<usize> {
    let Ok(text) = tokio::fs::read_to_string(path).await else {
        return HashSet::new();
    };
    text.lines()
        .filter_map(|line| line.trim().parse::<usize>().ok())
        .collect()
}

/// 失败后残片是否必须丢弃
///
/// 只有校验对不上时才丢：那些字节已被证明不是要的内容，续传会把错的拼进去。
/// 其余失败（瞬态网络、超时、用户取消）一律保留——续传的全部意义正在于这些
/// 场景，丢掉就等于每次中断都从头再来。
fn discards_partial(err: &DownloadError) -> bool {
    matches!(
        err,
        DownloadError::ChecksumMismatch { .. } | DownloadError::SizeMismatch { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::server::artifact::Checksum;
    use port::{ByteReader, ByteSource, SourceError};
    use std::error::Error;
    use std::fmt::Write as _;
    use std::sync::atomic::AtomicU32;
    use tempfile::tempdir;
    use url::Url;

    struct FakeSource {
        chunks: Vec<Vec<u8>>,
    }

    struct FakeReader {
        chunks: std::vec::IntoIter<Vec<u8>>,
    }

    #[async_trait]
    impl ByteSource for FakeSource {
        async fn open(&self, _location: &Url) -> Result<Box<dyn ByteReader>, SourceError> {
            Ok(Box::new(FakeReader {
                chunks: self.chunks.clone().into_iter(),
            }))
        }
    }

    #[async_trait]
    impl ByteReader for FakeReader {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, SourceError> {
            Ok(self.chunks.next())
        }
    }

    fn fake_source(chunks: &[&[u8]]) -> Arc<dyn ByteSource> {
        Arc::new(FakeSource {
            chunks: chunks.iter().map(|chunk| chunk.to_vec()).collect(),
        })
    }

    fn sha1_hex(data: &[u8]) -> String {
        let mut hasher = Sha1::new();
        hasher.update(data);
        to_hex(&hasher.finalize())
    }

    struct FakeRangeSource {
        data: Arc<Vec<u8>>,
        supports_range: bool,
        stat_calls: AtomicU32,
        open_calls: AtomicU32,
        fail_opens: AtomicU32,
        short_reads: AtomicU32,
        range_ignored: bool,
        cancel: Option<CancellationToken>,
    }

    #[async_trait]
    impl RangeSource for FakeRangeSource {
        async fn stat(&self, _location: &Url) -> Result<RemoteStat, SourceError> {
            self.stat_calls.fetch_add(1, Ordering::Relaxed);
            Ok(RemoteStat {
                size: self.data.len() as u64,
                supports_range: self.supports_range,
            })
        }

        async fn open_range(
            &self,
            _location: &Url,
            range: RangeInclusive<u64>,
        ) -> Result<Box<dyn ByteReader>, SourceError> {
            self.open_calls.fetch_add(1, Ordering::Relaxed);
            if self.fail_opens.load(Ordering::Relaxed) > 0 {
                self.fail_opens.fetch_sub(1, Ordering::Relaxed);
                return Err(SourceError::Network(Box::new(std::io::Error::other(
                    "transient",
                ))));
            }
            if self.range_ignored || !self.supports_range {
                return Err(SourceError::Status { status: 200 });
            }
            let start = usize::try_from(*range.start()).expect("range start fits usize");
            let end = usize::try_from(*range.end()).expect("range end fits usize");
            let mut slice = self
                .data
                .get(start..=end)
                .ok_or(SourceError::Status { status: 416 })?
                .to_vec();
            if self.short_reads.load(Ordering::Relaxed) > 0 {
                self.short_reads.fetch_sub(1, Ordering::Relaxed);
                if slice.len() > 1 {
                    slice.pop();
                }
            }
            let reader: Box<dyn ByteReader> = match &self.cancel {
                Some(cancel) => Box::new(CancellingReader {
                    chunks: vec![slice].into_iter(),
                    cancel: cancel.clone(),
                    fired: false,
                }),
                None => Box::new(FakeReader {
                    chunks: vec![slice].into_iter(),
                }),
            };
            Ok(reader)
        }
    }

    struct CancellingReader {
        chunks: std::vec::IntoIter<Vec<u8>>,
        cancel: CancellationToken,
        fired: bool,
    }

    #[async_trait]
    impl ByteReader for CancellingReader {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, SourceError> {
            let chunk = self.chunks.next();
            if !self.fired {
                self.fired = true;
                self.cancel.cancel();
            }
            Ok(chunk)
        }
    }

    fn fake_range(data: &[u8], supports_range: bool) -> Arc<FakeRangeSource> {
        Arc::new(FakeRangeSource {
            data: Arc::new(data.to_vec()),
            supports_range,
            stat_calls: AtomicU32::new(0),
            open_calls: AtomicU32::new(0),
            fail_opens: AtomicU32::new(0),
            short_reads: AtomicU32::new(0),
            range_ignored: false,
            cancel: None,
        })
    }

    /// 大于 8MiB 触发分段；长度故意不整除，覆盖余数段
    fn big_payload() -> Vec<u8> {
        vec![0xA5; 8 * 1024 * 1024 + 4096]
    }

    #[derive(Default)]
    struct RecordSink {
        calls: std::sync::Mutex<Vec<(u64, Option<u64>)>>,
    }

    impl ProgressSink for RecordSink {
        fn on_progress(&self, transferred: u64, total: Option<u64>) {
            self.calls.lock().expect("lock").push((transferred, total));
        }
    }

    fn request_for(
        destination: &Path,
        payload: &[u8],
        expected: Option<Checksum>,
    ) -> DownloadRequest {
        DownloadRequest {
            location: "https://example.com/server.jar".parse().expect("url"),
            destination: destination.to_path_buf(),
            expected,
            expected_size: Some(payload.len() as u64),
            mirrors: Vec::new(),
        }
    }

    #[tokio::test]
    async fn verifies_and_atomically_replaces() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let destination = dir.path().join("server.jar");
        let payload = b"hello minecraft";
        let expected = Checksum::new(ChecksumAlgorithm::Sha1, sha1_hex(payload));

        let downloader = FileDownloader::new(fake_source(&[b"hello ", b"minecraft"]));
        let request = DownloadRequest {
            location: "https://example.com/server.jar".parse()?,
            destination: destination.clone(),
            expected: Some(expected),
            expected_size: Some(payload.len() as u64),
            mirrors: Vec::new(),
        };

        let done = downloader
            .download(&request, None, &CancellationToken::new())
            .await?;

        assert_eq!(done.size, payload.len() as u64);
        assert_eq!(std::fs::read(&destination)?, payload);
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_checksum_mismatch_and_cleans_up() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let destination = dir.path().join("server.jar");
        let wrong = Checksum::new(
            ChecksumAlgorithm::Sha1,
            "0000000000000000000000000000000000000000",
        );

        let downloader = FileDownloader::new(fake_source(&[b"data"]));
        let request = DownloadRequest {
            location: "https://example.com/server.jar".parse()?,
            destination: destination.clone(),
            expected: Some(wrong),
            expected_size: None,
            mirrors: Vec::new(),
        };

        let result = downloader
            .download(&request, None, &CancellationToken::new())
            .await;

        assert!(matches!(
            result,
            Err(DownloadError::ChecksumMismatch { .. })
        ));
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn aborts_when_cancelled() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let cancel = CancellationToken::new();
        cancel.cancel();

        let downloader = FileDownloader::new(fake_source(&[b"data"]));
        let request = DownloadRequest {
            location: "https://example.com/server.jar".parse()?,
            destination: dir.path().join("server.jar"),
            expected: None,
            expected_size: None,
            mirrors: Vec::new(),
        };

        let result = downloader.download(&request, None, &cancel).await;
        assert!(matches!(result, Err(DownloadError::Cancelled)));
        Ok(())
    }

    /// 建连 / 首字节等待期间按下 Ctrl-C：不等网络返回就中止
    ///
    /// 取消检查不能只放在分块循环顶部——网络 future 不返回，检查就执行不到，
    /// 观感是 Ctrl-C 后下载又继续了一秒。这里 open 永不完成，取消必须仍能打断
    /// （回归时表现为 5s 超时而非正常返回）。
    #[tokio::test]
    async fn cancel_during_hanging_open_returns_promptly() -> Result<(), Box<dyn Error>> {
        struct HangingSource;

        #[async_trait]
        impl ByteSource for HangingSource {
            async fn open(&self, _location: &Url) -> Result<Box<dyn ByteReader>, SourceError> {
                std::future::pending().await
            }
        }

        let dir = tempdir()?;
        let destination = dir.path().join("server.jar");
        let cancel = CancellationToken::new();
        let downloader = FileDownloader::new(Arc::new(HangingSource));
        let request = DownloadRequest {
            location: "https://example.com/server.jar".parse()?,
            destination: destination.clone(),
            expected: None,
            expected_size: None,
            mirrors: Vec::new(),
        };

        let token = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            token.cancel();
        });

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            downloader.download(&request, None, &cancel),
        )
        .await;
        let result = outcome.expect("cancel must abort the hanging connection, not wait on it");
        assert!(matches!(result, Err(DownloadError::Cancelled)));
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn segmented_download_verifies_and_reports_progress() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let destination = dir.path().join("server.jar");
        let payload = big_payload();
        let expected = Checksum::new(ChecksumAlgorithm::Sha1, sha1_hex(&payload));

        let downloader = FileDownloader::new(fake_source(&[&payload]))
            .with_range_source(fake_range(&payload, true));
        let request = request_for(&destination, &payload, Some(expected));
        let sink = RecordSink::default();

        let done = downloader
            .download(&request, Some(&sink), &CancellationToken::new())
            .await?;

        assert_eq!(done.size, payload.len() as u64);
        assert_eq!(std::fs::read(&destination)?, payload);
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
        let calls = sink.calls.lock().expect("lock");
        assert!(
            calls.iter().any(
                |(transferred, total)| *transferred > 0 && *total == Some(payload.len() as u64)
            )
        );
        Ok(())
    }

    /// 断点续传：残片与段记录都在时，只补没写完的那一段
    #[tokio::test]
    async fn resumes_from_partial_segments() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let destination = dir.path().join("server.jar");
        let payload = big_payload();
        let total = payload.len() as u64;
        let bounds = plan_segments(total);
        assert!(bounds.len() > 1, "载荷要够大才谈得上分段");

        // 造一个「除最后一段外都已完成」的残片：长度对齐上游，已完成段的字节是真的
        let temp = temp_path_for(&destination, Some(total));
        let marks = marks_path_for(&temp);
        let missing = bounds.len() - 1;
        let missing_start = usize::try_from(*bounds[missing].start())?;
        let mut partial = vec![0_u8; payload.len()];
        partial[..missing_start].copy_from_slice(&payload[..missing_start]);
        std::fs::write(&temp, &partial)?;
        let mut recorded = String::new();
        for index in 0..missing {
            writeln!(recorded, "{index}")?;
        }
        std::fs::write(&marks, recorded)?;

        let range = fake_range(&payload, true);
        let downloader =
            FileDownloader::new(fake_source(&[&payload])).with_range_source(range.clone());
        let request = request_for(
            &destination,
            &payload,
            Some(Checksum::new(ChecksumAlgorithm::Sha1, sha1_hex(&payload))),
        );
        let done = downloader
            .download(&request, None, &CancellationToken::new())
            .await?;

        assert_eq!(done.size, total);
        assert_eq!(std::fs::read(&destination)?, payload);
        assert_eq!(
            range.open_calls.load(Ordering::Relaxed),
            1,
            "只该补最后一段，其余从残片续上"
        );
        // 段记录随成功作废；留着会让下一次同目标的下载以为已经写完
        assert!(!marks.exists(), "续传记录应被清掉");
        Ok(())
    }

    /// 残片长度与上游不符时不得续传：那是别的版本或上次写坏的
    #[tokio::test]
    async fn partial_with_wrong_size_is_not_resumed() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let destination = dir.path().join("server.jar");
        let payload = big_payload();
        let total = payload.len() as u64;
        let bounds = plan_segments(total);

        let temp = temp_path_for(&destination, Some(total));
        let marks = marks_path_for(&temp);
        std::fs::write(&temp, vec![7_u8; 16])?;
        std::fs::write(&marks, "0\n")?;

        let range = fake_range(&payload, true);
        let downloader =
            FileDownloader::new(fake_source(&[&payload])).with_range_source(range.clone());
        let request = request_for(
            &destination,
            &payload,
            Some(Checksum::new(ChecksumAlgorithm::Sha1, sha1_hex(&payload))),
        );
        downloader
            .download(&request, None, &CancellationToken::new())
            .await?;

        assert_eq!(std::fs::read(&destination)?, payload);
        assert_eq!(
            range.open_calls.load(Ordering::Relaxed),
            u32::try_from(bounds.len())?,
            "长度不符必须整份重下，一个段都不能省"
        );
        Ok(())
    }

    #[tokio::test]
    async fn falls_back_to_single_stream_without_range_support() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let destination = dir.path().join("server.jar");
        let payload = big_payload();
        let range = fake_range(&payload, false);

        let downloader =
            FileDownloader::new(fake_source(&[&payload])).with_range_source(range.clone());
        let request = request_for(
            &destination,
            &payload,
            Some(Checksum::new(ChecksumAlgorithm::Sha1, sha1_hex(&payload))),
        );

        let done = downloader
            .download(&request, None, &CancellationToken::new())
            .await?;

        assert_eq!(done.size, payload.len() as u64);
        assert_eq!(std::fs::read(&destination)?, payload);
        assert_eq!(range.open_calls.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[tokio::test]
    async fn small_file_skips_segmentation() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let destination = dir.path().join("server.jar");
        let payload = vec![0x5A; 1024 * 1024];
        let range = fake_range(&payload, true);

        let downloader =
            FileDownloader::new(fake_source(&[&payload])).with_range_source(range.clone());
        let request = request_for(&destination, &payload, None);

        let done = downloader
            .download(&request, None, &CancellationToken::new())
            .await?;

        assert_eq!(done.size, payload.len() as u64);
        assert_eq!(std::fs::read(&destination)?, payload);
        assert_eq!(range.open_calls.load(Ordering::Relaxed), 0);
        assert_eq!(range.stat_calls.load(Ordering::Relaxed), 1);
        Ok(())
    }

    #[tokio::test]
    async fn retries_retryable_segment_failures() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let destination = dir.path().join("server.jar");
        let payload = big_payload();
        let range = fake_range(&payload, true);
        // 8MiB+4096 / 2MiB = 4 段，每段首次失败一次
        range.fail_opens.store(4, Ordering::Relaxed);

        let downloader =
            FileDownloader::new(fake_source(&[&payload])).with_range_source(range.clone());
        let request = request_for(
            &destination,
            &payload,
            Some(Checksum::new(ChecksumAlgorithm::Sha1, sha1_hex(&payload))),
        );

        let done = downloader
            .download(&request, None, &CancellationToken::new())
            .await?;

        assert_eq!(done.size, payload.len() as u64);
        assert_eq!(std::fs::read(&destination)?, payload);
        // 4 段各失败一次后重试成功：首试 4 + 重试 4
        assert_eq!(range.open_calls.load(Ordering::Relaxed), 8);
        Ok(())
    }

    #[tokio::test]
    async fn retries_short_segment_reads() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let destination = dir.path().join("server.jar");
        let payload = big_payload();
        let range = fake_range(&payload, true);
        range.short_reads.store(4, Ordering::Relaxed);

        let downloader =
            FileDownloader::new(fake_source(&[&payload])).with_range_source(range.clone());
        let request = request_for(
            &destination,
            &payload,
            Some(Checksum::new(ChecksumAlgorithm::Sha1, sha1_hex(&payload))),
        );

        let done = downloader
            .download(&request, None, &CancellationToken::new())
            .await?;

        assert_eq!(done.size, payload.len() as u64);
        assert_eq!(std::fs::read(&destination)?, payload);
        // 4 段首轮各短读一次后重试成功：首试 4 + 重试 4
        assert_eq!(range.open_calls.load(Ordering::Relaxed), 8);
        Ok(())
    }

    #[tokio::test]
    async fn falls_back_to_single_stream_when_range_ignored() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let destination = dir.path().join("server.jar");
        let payload = big_payload();
        let range = Arc::new(FakeRangeSource {
            data: Arc::new(payload.clone()),
            supports_range: true,
            stat_calls: AtomicU32::new(0),
            open_calls: AtomicU32::new(0),
            fail_opens: AtomicU32::new(0),
            short_reads: AtomicU32::new(0),
            range_ignored: true,
            cancel: None,
        });

        let downloader =
            FileDownloader::new(fake_source(&[&payload])).with_range_source(range.clone());
        let request = request_for(
            &destination,
            &payload,
            Some(Checksum::new(ChecksumAlgorithm::Sha1, sha1_hex(&payload))),
        );

        let done = downloader
            .download(&request, None, &CancellationToken::new())
            .await?;

        assert_eq!(done.size, payload.len() as u64);
        assert_eq!(std::fs::read(&destination)?, payload);
        assert!(range.open_calls.load(Ordering::Relaxed) >= 1);
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn segment_checksum_mismatch_cleans_up() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let destination = dir.path().join("server.jar");
        let payload = big_payload();
        let wrong = Checksum::new(
            ChecksumAlgorithm::Sha1,
            "0000000000000000000000000000000000000000",
        );

        let downloader = FileDownloader::new(fake_source(&[&payload]))
            .with_range_source(fake_range(&payload, true));
        let request = request_for(&destination, &payload, Some(wrong));

        let result = downloader
            .download(&request, None, &CancellationToken::new())
            .await;

        assert!(matches!(
            result,
            Err(DownloadError::ChecksumMismatch { .. })
        ));
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn size_conflict_falls_back_and_fails_verification() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let destination = dir.path().join("server.jar");
        let payload = big_payload();
        let range = fake_range(&payload, true);

        let downloader =
            FileDownloader::new(fake_source(&[&payload])).with_range_source(range.clone());
        let mut request = request_for(&destination, &payload, None);
        request.expected_size = Some(1);

        let result = downloader
            .download(&request, None, &CancellationToken::new())
            .await;

        assert!(matches!(
            result,
            Err(DownloadError::SizeMismatch { expected: 1, .. })
        ));
        assert_eq!(range.open_calls.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[tokio::test]
    async fn mid_flight_cancel_aborts_segmented_download() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let destination = dir.path().join("server.jar");
        let payload = big_payload();
        let cancel = CancellationToken::new();
        let range = Arc::new(FakeRangeSource {
            data: Arc::new(payload.clone()),
            supports_range: true,
            stat_calls: AtomicU32::new(0),
            open_calls: AtomicU32::new(0),
            fail_opens: AtomicU32::new(0),
            short_reads: AtomicU32::new(0),
            range_ignored: false,
            cancel: Some(cancel.clone()),
        });

        let downloader = FileDownloader::new(fake_source(&[&payload])).with_range_source(range);
        let request = request_for(&destination, &payload, None);

        let result = downloader.download(&request, None, &cancel).await;

        assert!(matches!(result, Err(DownloadError::Cancelled)));
        assert!(!destination.exists());
        Ok(())
    }

    /// 记录收到的每个进度样本
    struct RecordingSink {
        samples: std::sync::Mutex<Vec<(u64, Option<u64>)>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                samples: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn samples(&self) -> Vec<(u64, Option<u64>)> {
            self.samples.lock().expect("samples").clone()
        }
    }

    impl ProgressSink for RecordingSink {
        fn on_progress(&self, transferred: u64, total: Option<u64>) {
            self.samples
                .lock()
                .expect("samples")
                .push((transferred, total));
        }
    }

    fn batch_request(dir: &Path, name: &str, payload: &[u8]) -> DownloadRequest {
        DownloadRequest {
            location: format!("https://example.com/{name}").parse().expect("url"),
            destination: dir.join(name),
            expected: Some(Checksum::new(ChecksumAlgorithm::Sha1, sha1_hex(payload))),
            expected_size: Some(payload.len() as u64),
            mirrors: Vec::new(),
        }
    }

    /// 批量下载：全部落盘、结果顺序与入参一致、进度是整批口径且单调
    #[tokio::test]
    async fn download_many_reports_batch_level_monotonic_progress() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let payload = b"chunk-of-mod";
        let requests: Vec<DownloadRequest> = (0..6)
            .map(|index| batch_request(dir.path(), &format!("m{index}.jar"), payload))
            .collect();
        let sink = RecordingSink::new();

        let downloader = FileDownloader::new(fake_source(&[payload]));
        let outcomes = downloader
            .download_many(&requests, 4, Some(&sink), &CancellationToken::new())
            .await;

        assert_eq!(outcomes.len(), requests.len());
        for (index, outcome) in outcomes.iter().enumerate() {
            let artifact = outcome.as_ref().map_err(|err| format!("{err:?}"))?;
            // 顺序必须与入参一一对应：第 index 项落在第 index 个路径
            assert_eq!(artifact.path, requests[index].destination);
            assert!(requests[index].destination.is_file(), "m{index} 未落盘");
        }

        let expected_total = payload.len() as u64 * requests.len() as u64;
        let samples = sink.samples();
        // 每个样本的 total 都是整批字节总和
        assert!(
            samples
                .iter()
                .all(|(_, total)| *total == Some(expected_total)),
            "total 必须是整批字节：{samples:?}"
        );
        // 单调不回退，且最终到达整批总量
        let mut last = 0;
        for (done, _) in &samples {
            assert!(*done >= last, "进度回退：{samples:?}");
            last = *done;
        }
        assert_eq!(
            samples.last().map(|(done, _)| *done),
            Some(expected_total),
            "最终样本必须到达整批总量：{samples:?}"
        );
        Ok(())
    }

    /// 批量下载：单个文件失败不拖垮整批，失败项留在原位置
    #[tokio::test]
    async fn download_many_keeps_going_after_one_failure() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let payload = b"ok";
        let mut requests: Vec<DownloadRequest> = (0..3)
            .map(|index| batch_request(dir.path(), &format!("m{index}.jar"), payload))
            .collect();
        // 中间那个声明了错误的哈希，必然校验失败
        requests[1].expected = Some(Checksum::new(ChecksumAlgorithm::Sha1, "00".repeat(20)));

        let downloader = FileDownloader::new(fake_source(&[payload]));
        let outcomes = downloader
            .download_many(&requests, 2, None, &CancellationToken::new())
            .await;

        assert_eq!(outcomes.len(), 3);
        assert!(outcomes[0].is_ok(), "{:?}", outcomes[0]);
        assert!(
            matches!(outcomes[1], Err(DownloadError::ChecksumMismatch { .. })),
            "{:?}",
            outcomes[1]
        );
        assert!(outcomes[2].is_ok(), "{:?}", outcomes[2]);
        assert!(requests[0].destination.is_file());
        assert!(requests[2].destination.is_file());
        // 校验失败不应留下半成品
        assert!(!requests[1].destination.exists());
        Ok(())
    }

    /// 批量下载：并发度 0 取默认值，空批次不 panic
    #[tokio::test]
    async fn download_many_handles_empty_and_zero_concurrency() -> Result<(), Box<dyn Error>> {
        let downloader = FileDownloader::new(fake_source(&[b"x"]));
        let empty = downloader
            .download_many(&[], 0, None, &CancellationToken::new())
            .await;
        assert!(empty.is_empty());

        let dir = tempdir()?;
        let requests = vec![batch_request(dir.path(), "one.jar", b"x")];
        let outcomes = downloader
            .download_many(&requests, 0, None, &CancellationToken::new())
            .await;
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].is_ok());
        Ok(())
    }

    /// 记录并发峰值的来源：`open` 进入即占坑，读到流末尾才放
    #[derive(Default)]
    struct CountingState {
        live: AtomicU32,
        peak: AtomicU32,
    }

    struct CountingSource {
        state: Arc<CountingState>,
        payload: Vec<u8>,
    }

    struct CountingReader {
        payload: Option<Vec<u8>>,
        state: Arc<CountingState>,
    }

    #[async_trait]
    impl ByteSource for CountingSource {
        async fn open(&self, _location: &Url) -> Result<Box<dyn ByteReader>, SourceError> {
            let live = self.state.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.state.peak.fetch_max(live, Ordering::SeqCst);
            Ok(Box::new(CountingReader {
                payload: Some(self.payload.clone()),
                state: Arc::clone(&self.state),
            }))
        }
    }

    #[async_trait]
    impl ByteReader for CountingReader {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, SourceError> {
            let Some(payload) = self.payload.take() else {
                return Ok(None);
            };
            Ok(Some(payload))
        }
    }

    impl Drop for CountingReader {
        fn drop(&mut self) {
            self.state.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// 在途上限必须真的压住峰值：不限时 8 个文件会同时开 8 条连接
    #[tokio::test]
    async fn inflight_limit_caps_concurrent_connections() -> Result<(), Box<dyn Error>> {
        let dir = tempdir()?;
        let state = Arc::new(CountingState::default());
        let source = Arc::new(CountingSource {
            state: Arc::clone(&state),
            payload: b"jar!".to_vec(),
        });
        let downloader = FileDownloader::new(source).with_inflight_limit(2);
        let requests: Vec<_> = (0..8)
            .map(|index| batch_request(dir.path(), &format!("m{index}.jar"), b"jar!"))
            .collect();
        let outcomes = downloader
            .download_many(&requests, 8, None, &CancellationToken::new())
            .await;
        assert!(outcomes.iter().all(Result::is_ok), "{outcomes:?}");
        let peak = state.peak.load(Ordering::SeqCst);
        assert!(peak <= 2, "在途上限 2 却观测到峰值 {peak}");
        // 全部结束后坑位必须回零，否则上限会随下载次数累积泄漏
        assert_eq!(state.live.load(Ordering::SeqCst), 0, "在途计数未归零");
        Ok(())
    }
}
