//! 按调用序号注入失败的 `ArtifactDownloader`
//!
//! 目的：让「传输失败」「校验和不符」「被取消」三条错误分支可被确定性触发，
//! 而不依赖真实网络。注入点按 `download` 的调用序号计（1 基）。
//!
//! 注入之外的两道本地判定（期望大小、取消令牌）**仍然真实执行**：故障注入
//! 只负责制造失败，不能顺带把其它语义也改成假的。

use async_trait::async_trait;
use port::{
    ArtifactDownloader, CancellationToken, DownloadError, DownloadRequest, DownloadedArtifact,
    ProgressSink,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// 注入的失败方式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectFailure {
    /// 传输中断
    Network,
    /// 校验和不符
    Checksum,
    /// 被取消
    Cancelled,
}

/// 一次下载调用的记录
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadCall {
    /// 目标路径
    pub destination: PathBuf,
    /// 是否给了进度观察者
    pub has_progress: bool,
}

#[derive(Debug, Default)]
struct Inner {
    calls: Vec<DownloadCall>,
    fail_on: Option<(usize, InjectFailure)>,
    /// 正常路径下写进目标文件的字节
    payload: Vec<u8>,
}

/// 可控下载器
#[derive(Debug, Clone, Default)]
pub struct FailingDownloader {
    inner: Arc<Mutex<Inner>>,
}

impl FailingDownloader {
    /// 不注入故障：每次调用都成功，写出 `payload`（默认 4 字节）
    #[must_use]
    pub fn new() -> Self {
        let downloader = Self::default();
        downloader.inner.lock().expect("lock").payload = b"fake".to_vec();
        downloader
    }

    /// 正常路径写出的内容
    #[must_use]
    pub fn with_payload(self, payload: impl Into<Vec<u8>>) -> Self {
        self.inner.lock().expect("lock").payload = payload.into();
        self
    }

    /// 第 `attempt` 次（1 基）调用以 `failure` 方式失败
    #[must_use]
    pub fn failing_on(self, attempt: usize, failure: InjectFailure) -> Self {
        self.inner.lock().expect("lock").fail_on = Some((attempt, failure));
        self
    }

    /// 已发生的调用
    #[must_use]
    pub fn calls(&self) -> Vec<DownloadCall> {
        self.inner.lock().expect("lock").calls.clone()
    }
}

#[async_trait]
impl ArtifactDownloader for FailingDownloader {
    async fn download(
        &self,
        request: &DownloadRequest,
        progress: Option<&dyn ProgressSink>,
        cancel: &CancellationToken,
    ) -> Result<DownloadedArtifact, DownloadError> {
        let (attempt, inject, payload) = {
            let mut inner = self.inner.lock().expect("lock");
            inner.calls.push(DownloadCall {
                destination: request.destination.clone(),
                has_progress: progress.is_some(),
            });
            (inner.calls.len(), inner.fail_on, inner.payload.clone())
        };

        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        if let Some((target, failure)) = inject
            && target == attempt
        {
            return Err(match failure {
                InjectFailure::Network => DownloadError::Source(port::SourceError::Network(
                    Box::new(std::io::Error::other("injected transport failure")),
                )),
                InjectFailure::Checksum => DownloadError::ChecksumMismatch {
                    expected: "<expected>".to_owned(),
                    actual: "<injected>".to_owned(),
                },
                InjectFailure::Cancelled => DownloadError::Cancelled,
            });
        }

        if let Some(expected) = request.expected_size
            && expected != payload.len() as u64
        {
            return Err(DownloadError::SizeMismatch {
                expected,
                actual: payload.len() as u64,
            });
        }
        if let Some(parent) = request.destination.parent() {
            std::fs::create_dir_all(parent).map_err(DownloadError::Io)?;
        }
        std::fs::write(&request.destination, &payload).map_err(DownloadError::Io)?;
        if let Some(sink) = progress {
            sink.on_progress(payload.len() as u64, Some(payload.len() as u64));
        }
        Ok(DownloadedArtifact {
            path: request.destination.clone(),
            size: payload.len() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assert::{Failure, assert_invariant};
    use url::Url;

    fn request(dest: PathBuf) -> DownloadRequest {
        DownloadRequest {
            location: Url::parse("http://127.0.0.1:1/x.jar").expect("url"),
            destination: dest,
            expected: None,
            expected_size: None,
        }
    }

    #[tokio::test]
    async fn successful_download_writes_payload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("server.jar");
        let downloader = FailingDownloader::new().with_payload(b"jar!");
        let done = downloader
            .download(&request(dest.clone()), None, &CancellationToken::new())
            .await
            .expect("download");
        assert_invariant(
            done.size == 4 && std::fs::read(&dest).expect("read") == b"jar!",
            || Failure::invariant("成功路径必须写出完整 payload").tree(dir.path()),
        );
    }

    #[tokio::test]
    async fn injected_failure_only_hits_the_named_attempt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let downloader = FailingDownloader::new().failing_on(2, InjectFailure::Network);
        let token = CancellationToken::new();
        downloader
            .download(&request(dir.path().join("a")), None, &token)
            .await
            .expect("first ok");
        assert_invariant(
            downloader
                .download(&request(dir.path().join("b")), None, &token)
                .await
                .is_err(),
            || Failure::invariant("第 2 次必须失败"),
        );
        downloader
            .download(&request(dir.path().join("c")), None, &token)
            .await
            .expect("third ok");
        assert_invariant(downloader.calls().len() == 3, || {
            Failure::invariant("三次调用都要被记录").actual(downloader.calls().len())
        });
    }

    #[tokio::test]
    async fn cancelled_token_fails_before_any_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("server.jar");
        let token = CancellationToken::new();
        token.cancel();
        let err = FailingDownloader::new()
            .download(&request(dest.clone()), None, &token)
            .await
            .expect_err("cancelled");
        assert_invariant(matches!(err, DownloadError::Cancelled), || {
            Failure::invariant("取消必须报 Cancelled").actual(err.to_string())
        });
        assert_invariant(!dest.exists(), || {
            Failure::invariant("取消后不得落盘").tree(dir.path())
        });
    }

    #[tokio::test]
    async fn size_mismatch_is_still_enforced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut req = request(dir.path().join("s"));
        req.expected_size = Some(99);
        let err = FailingDownloader::new()
            .download(&req, None, &CancellationToken::new())
            .await
            .expect_err("size");
        assert_invariant(matches!(err, DownloadError::SizeMismatch { .. }), || {
            Failure::invariant("大小不符必须被下载器拦下").actual(err.to_string())
        });
    }
}
