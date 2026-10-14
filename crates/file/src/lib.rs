use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use domain::server::artifact::ChecksumAlgorithm;
use port::{
    ArtifactDownloader, ByteSource, CancellationToken, DownloadError, DownloadRequest,
    DownloadedArtifact, ProgressSink,
};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use tokio::io::AsyncWriteExt;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 基于 [`ByteSource`] 的下载器
pub struct FileDownloader {
    source: Arc<dyn ByteSource>,
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
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// 构建下载器
    #[must_use]
    pub fn with_timeout(source: Arc<dyn ByteSource>, timeout: Duration) -> Self {
        Self { source, timeout }
    }

    async fn transfer(
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
}
