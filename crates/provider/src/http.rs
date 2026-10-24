use async_trait::async_trait;
use domain::server::version::VersionId;
use port::{ByteReader, ByteSource, RangeSource, RemoteStat, SourceError};
use reqwest::Client;
use serde::de::DeserializeOwned;
use std::ops::RangeInclusive;
use std::sync::Once;
use std::time::Duration;
use url::Url;

/// 元数据请求超时
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 建立连接的超时
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 流式响应两次可读之间的最大间隔
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("mcsup/", env!("CARGO_PKG_VERSION"));

/// 安装 rustls 的 ring crypto provider（进程内一次）
///
/// reqwest 走 `rustls-no-provider`（TLS 栈不锁 aws-lc，见 provider 的
/// Cargo.toml 注释），provider 由本 crate 显式提供：进程内首次构造
/// reqwest Client 前装一次；宿主已装过自己的 provider 时（try 语义）
/// 尊重宿主选择，不覆盖。
fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// 元数据客户端
pub(crate) fn build_client() -> Result<Client, SourceError> {
    install_crypto_provider();
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| SourceError::Init(Box::new(e)))
}

/// 大文件客户端
pub(crate) fn build_stream_client() -> Result<Client, SourceError> {
    install_crypto_provider();
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .user_agent(USER_AGENT)
        .http1_only()
        .build()
        .map_err(|e| SourceError::Init(Box::new(e)))
}

/// 基于 HTTP 的字节来源
#[derive(Debug)]
pub struct HttpByteSource {
    http: Client,
}

impl HttpByteSource {
    /// 构建字节来源
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败时返回 [`SourceError::Init`]
    pub fn new() -> Result<Self, SourceError> {
        Ok(Self {
            http: build_stream_client()?,
        })
    }
}

#[async_trait]
impl ByteSource for HttpByteSource {
    async fn open(&self, location: &Url) -> Result<Box<dyn ByteReader>, SourceError> {
        let response = self
            .http
            .get(location.clone())
            .send()
            .await
            .map_err(map_transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(SourceError::Status {
                status: status.as_u16(),
            });
        }
        Ok(Box::new(HttpBody { response }))
    }
}

struct HttpBody {
    response: reqwest::Response,
}

#[async_trait]
impl ByteReader for HttpBody {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, SourceError> {
        match self.response.chunk().await.map_err(map_transport)? {
            Some(bytes) => Ok(Some(bytes.to_vec())),
            None => Ok(None),
        }
    }
}

pub(crate) fn map_transport(err: reqwest::Error) -> SourceError {
    SourceError::Network(Box::new(err))
}

/// GET 并反序列化 JSON，各元数据源共用的唯一入口
///
/// - 非 2xx：`not_found_id` 非空且状态码为 404 时归为 [`SourceError::NotFound`]，
///   其余一律 [`SourceError::Status`]
/// - body 传输 / 解析错误：解析失败归 [`SourceError::Decode`]，其余归
///   [`SourceError::Network`]（与 `ByteSource` 侧分类保持一致）
///
/// # Errors
///
/// 网络失败、非成功状态码或 body 非法时返回 [`SourceError`]
pub(crate) async fn get_json<T>(
    client: &Client,
    url: &str,
    not_found_id: Option<VersionId>,
) -> Result<T, SourceError>
where
    T: DeserializeOwned,
{
    let response = client.get(url).send().await.map_err(map_transport)?;
    let status = response.status();
    if !status.is_success() {
        let code = status.as_u16();
        if code == 404
            && let Some(id) = not_found_id
        {
            return Err(SourceError::NotFound(id));
        }
        return Err(SourceError::Status { status: code });
    }
    response.json().await.map_err(|err| {
        if err.is_decode() {
            SourceError::Decode(Box::new(err))
        } else {
            SourceError::Network(Box::new(err))
        }
    })
}

#[async_trait]
impl RangeSource for HttpByteSource {
    async fn stat(&self, location: &Url) -> Result<RemoteStat, SourceError> {
        let response = self
            .http
            .head(location.clone())
            .send()
            .await
            .map_err(map_transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(SourceError::Status {
                status: status.as_u16(),
            });
        }
        let supports_range = response
            .headers()
            .get(reqwest::header::ACCEPT_RANGES)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("bytes"));
        let size = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                SourceError::Decode(Box::new(std::io::Error::other("missing content-length")))
            })?;
        Ok(RemoteStat {
            size,
            supports_range,
        })
    }

    async fn open_range(
        &self,
        location: &Url,
        range: RangeInclusive<u64>,
    ) -> Result<Box<dyn ByteReader>, SourceError> {
        let response = self
            .http
            .get(location.clone())
            .header(
                reqwest::header::RANGE,
                format!("bytes={}-{}", range.start(), range.end()),
            )
            .send()
            .await
            .map_err(map_transport)?;
        let status = response.status();
        if status != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(SourceError::Status {
                status: status.as_u16(),
            });
        }
        let expected_prefix = format!("bytes {}-{}/", range.start(), range.end());
        let content_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                SourceError::Decode(Box::new(std::io::Error::other("missing content-range")))
            })?;
        if !content_range.starts_with(&expected_prefix) {
            return Err(SourceError::Decode(Box::new(std::io::Error::other(
                format!(
                    "content-range mismatch: expected `{expected_prefix}*`, got `{content_range}`"
                ),
            ))));
        }
        Ok(Box::new(HttpBody { response }))
    }
}
