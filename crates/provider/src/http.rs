use async_trait::async_trait;
use port::{ByteReader, ByteSource, SourceError};
use reqwest::Client;
use std::time::Duration;
use url::Url;

/// 元数据请求超时
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 建立连接的超时
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 流式响应两次可读之间的最大间隔
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("mcsup/", env!("CARGO_PKG_VERSION"));

/// 元数据客户端
pub(crate) fn build_client() -> Result<Client, SourceError> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| SourceError::Init(Box::new(e)))
}

/// 大文件客户端
pub(crate) fn build_stream_client() -> Result<Client, SourceError> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .user_agent(USER_AGENT)
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
