use async_trait::async_trait;
use domain::server::artifact::ChecksumAlgorithm;
use port::{
    ArtifactDownloader, ByteSource, CancellationToken, DownloadError, DownloadRequest,
    DownloadedArtifact, ProgressSink, RangeSource, RemoteStat, SourceError,
};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;
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
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 下载的传输算法
///
/// 选择规则集在 [`FileDownloader::plan`]; 将来新增算法，只需要扩展本枚举
enum TransferPlan {
    /// 单流顺序写入
    Single,
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
}

impl std::fmt::Debug for FileDownloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileDownloader")
            .field("timeout", &self.timeout)
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
        }
    }

    /// 构建下载器
    #[must_use]
    pub fn with_timeout(source: Arc<dyn ByteSource>, timeout: Duration) -> Self {
        Self {
            source,
            range: None,
            timeout,
        }
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
    /// 不影响正确性，因此探测失败不额外记录日志
    async fn plan(&self, request: &DownloadRequest) -> TransferPlan {
        let Some(range) = self.range.clone() else {
            return TransferPlan::Single;
        };
        let Ok(stat) = range.stat(&request.location).await else {
            return TransferPlan::Single;
        };
        if !stat.supports_range || stat.size < MIN_SEGMENTED_SIZE {
            return TransferPlan::Single;
        }
        if request
            .expected_size
            .is_some_and(|expected| expected != stat.size)
        {
            return TransferPlan::Single;
        }
        TransferPlan::Segmented { range, stat }
    }

    async fn transfer(
        &self,
        temp_path: &Path,
        request: &DownloadRequest,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<u64, DownloadError> {
        match self.plan(request).await {
            TransferPlan::Single => {
                self.transfer_single(temp_path, request, progress, cancel)
                    .await
            }
            TransferPlan::Segmented { range, stat } => {
                self.transfer_segmented(range, stat, temp_path, request, progress, cancel)
                    .await
            }
        }
    }

    /// 单流顺序写入
    async fn transfer_single(
        &self,
        temp_path: &Path,
        request: &DownloadRequest,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<u64, DownloadError> {
        let mut reader = self.source.open(&request.location).await?;
        let mut file = tokio::fs::File::create(temp_path)
            .await
            .map_err(DownloadError::Io)?;
        let mut hasher = match request.expected.as_ref() {
            Some(checksum) => Some(Hasher::new(checksum.algorithm())?),
            None => None,
        };
        let mut transferred: u64 = 0;
        loop {
            if cancel.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }
            let Some(chunk) = reader.next_chunk().await? else {
                break;
            };
            if let Some(hasher) = hasher.as_mut() {
                hasher.update(&chunk);
            }
            file.write_all(&chunk).await.map_err(DownloadError::Io)?;
            transferred += chunk.len() as u64;
            if let Some(sink) = progress {
                sink.on_progress(transferred, request.expected_size);
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
        {
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(temp_path)
                .await
                .map_err(DownloadError::Io)?;
            file.set_len(total).await.map_err(DownloadError::Io)?;
        }

        let stop = CancellationToken::new();
        let _stop_guard = StopOnDrop(stop.clone());
        let transferred = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let mut handles = Vec::with_capacity(bounds_list.len());
        for bounds in bounds_list {
            let task = SegmentTask {
                range_source: range.clone(),
                location: request.location.clone(),
                temp_path: temp_path.to_path_buf(),
                bounds,
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
        while rx.recv().await.is_some()
            && let Some(sink) = progress
        {
            sink.on_progress(transferred.load(Ordering::Relaxed), progress_total);
        }
        if let Some(sink) = progress {
            sink.on_progress(transferred.load(Ordering::Relaxed), progress_total);
        }
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
                    .transfer_single(temp_path, request, progress, cancel)
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
        let temp_path = temp_path_for(&destination);
        let outcome = tokio::time::timeout(
            self.timeout,
            self.transfer(&temp_path, request, progress, cancel),
        )
        .await;
        let size = match outcome {
            Ok(Ok(size)) => size,
            Ok(Err(err)) => {
                remove_quietly(&temp_path).await;
                return Err(err);
            }
            Err(_elapsed) => {
                remove_quietly(&temp_path).await;
                return Err(DownloadError::Timeout);
            }
        };
        if let Err(err) = tokio::fs::rename(&temp_path, &destination).await {
            remove_quietly(&temp_path).await;
            return Err(DownloadError::Io(err));
        }
        Ok(DownloadedArtifact {
            path: destination,
            size,
        })
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
    progress: UnboundedSender<()>,
    cancel: CancellationToken,
    stop: CancellationToken,
) -> Result<(), DownloadError> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match fetch_segment(&task, transferred.clone(), &progress, &cancel, &stop).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                if attempt >= SEGMENT_MAX_ATTEMPTS || !err.retryable() {
                    stop.cancel();
                    return Err(err);
                }
                // 按段起点与尝试次数确定性错开，避免所有段同时重试
                let jitter_ms = (task.bounds.start() % 200) + (u64::from(attempt) * 37 % 200);
                tokio::time::sleep(
                    SEGMENT_RETRY_DELAY * attempt + Duration::from_millis(jitter_ms),
                )
                .await;
            }
        }
    }
}

/// 拉取一个区间并写入对应偏移；失败时会退本次尝试的进度计数
async fn fetch_segment(
    task: &SegmentTask,
    transferred: Arc<AtomicU64>,
    progress: &UnboundedSender<()>,
    cancel: &CancellationToken,
    stop: &CancellationToken,
) -> Result<(), DownloadError> {
    if cancel.is_cancelled() || stop.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }
    let mut reader = task
        .range_source
        .open_range(&task.location, task.bounds.clone())
        .await?;
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
            if cancel.is_cancelled() || stop.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }
            let Some(chunks) = reader.next_chunk().await? else {
                break;
            };
            file.write_all(&chunks).await.map_err(DownloadError::Io)?;
            got += chunks.len() as u64;
            transferred.fetch_add(chunks.len() as u64, Ordering::Relaxed);
            let _ = progress.send(());
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
        let _ = progress.send(());
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

fn temp_path_for(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    destination.with_file_name(format!(".{name}.{nanos}.{seq}.tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::server::artifact::Checksum;
    use port::{ByteReader, ByteSource, SourceError};
    use std::error::Error;
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
        };

        let result = downloader.download(&request, None, &cancel).await;
        assert!(matches!(result, Err(DownloadError::Cancelled)));
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
}
