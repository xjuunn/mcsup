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
/// 每个 host 保留的空闲连接数
///
/// 要覆盖两层并发：批量下载的文件级并发（8）与单文件内的分片并发（24）。
/// 取 32 让两层都能命中已建立的连接，又不会无节制占用文件句柄。
const IDLE_CONNECTIONS_PER_HOST: usize = 32;
/// 空闲连接保留时长
///
/// 批量下载的文件是一个接一个取走的，间隔通常是毫秒级；留 90 秒足够跨过
/// 短暂的排队与重试退避，又不至于长期占着不放。
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
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
        .redirect(redirect_policy())
        .build()
        .map_err(|e| SourceError::Init(Box::new(e)))
}

/// 大文件客户端
///
/// 空闲连接池按**并发下载路数**留足：reqwest 默认每个 host 只留 1 条空闲连接，
/// 批量下载（整合包清单动辄上百个文件、并发 8 路，单文件内还有分段并发）会
/// 反复丢弃并重建连接，每次都交一次 TCP + TLS 握手的钱。池子留够之后，同一
/// CDN host 的连接被复用，握手成本摊薄到接近零——大整合包的下载时间差别相当
/// 明显。
pub(crate) fn build_stream_client() -> Result<Client, SourceError> {
    install_crypto_provider();
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .user_agent(USER_AGENT)
        .http1_only()
        .pool_max_idle_per_host(IDLE_CONNECTIONS_PER_HOST)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .redirect(redirect_policy())
        .build()
        .map_err(|e| SourceError::Init(Box::new(e)))
}

/// 重定向策略：只跟 HTTP(S)，禁止 HTTPS 降级到 HTTP，最多 5 跳
///
/// 默认策略会跟随任意跳转。上游被篡改时，一次 `https -> http` 跳转就能把
/// 后续请求降成明文；跨协议跳转（例如 `file:`）也不该由下载器跟随。
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error(std::io::Error::other("too many redirects"));
        }
        let scheme = attempt.url().scheme().to_owned();
        if !matches!(scheme.as_str(), "http" | "https") {
            return attempt.error(std::io::Error::other(format!(
                "unsupported redirect scheme: {scheme}"
            )));
        }
        let downgrade = attempt
            .previous()
            .last()
            .is_some_and(|previous| previous.scheme() == "https" && scheme == "http");
        if downgrade {
            return attempt.error(std::io::Error::other(
                "refusing to downgrade an https redirect to http",
            ));
        }
        attempt.follow()
    })
}

/// 请求 URL 只接受 HTTP(S)
fn ensure_http_url(url: &Url) -> Result<(), SourceError> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(SourceError::Decode(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsupported URL scheme: {other}"),
        )))),
    }
}

/// 字符串形式的 URL 校验后返回解析结果
fn parse_http_url(url: &str) -> Result<Url, SourceError> {
    let parsed = Url::parse(url).map_err(|err| SourceError::Decode(Box::new(err)))?;
    ensure_http_url(&parsed)?;
    Ok(parsed)
}

/// 元数据响应体的上限
///
/// 上游正常返回是 KB 到数 MB（搜索结果、版本列表）；上限只兜异常响应，
/// 避免一次请求把任意大的 body 读进内存。
const RESPONSE_LIMIT: usize = 16 * 1024 * 1024;
/// 失败后重试一次前的等待
///
/// 上游的 429 / 5xx 多是瞬时排队或实例切换；一次短退避足以跨过，过长的
/// 退避会把单条命令拖慢，调用方自己的重试策略仍然保留。
const RETRY_DELAY: Duration = Duration::from_millis(250);

/// 发一个幂等请求，传输错误或 429 / 5xx 时重试一次
///
/// 只用于元数据 GET / HEAD。POST 不在此列：写请求重放可能重复生效，重试
/// 必须由明确知道语义的调用方决定。
async fn send_with_retry<F>(mut build: F, url: &str) -> Result<reqwest::Response, SourceError>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    match build().send().await {
        Ok(response) if is_retryable_status(response.status()) => {
            tracing::debug!(
                %url,
                status = response.status().as_u16(),
                "metadata request returned a retryable status; retrying once"
            );
            tokio::time::sleep(RETRY_DELAY).await;
            build().send().await.map_err(map_transport)
        }
        Ok(response) => Ok(response),
        Err(err) => {
            tracing::debug!(%url, %err, "metadata request failed; retrying once");
            tokio::time::sleep(RETRY_DELAY).await;
            build().send().await.map_err(map_transport)
        }
    }
}

/// 上游状态是否值得原样重试一次
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// 读取响应体，超过 [`RESPONSE_LIMIT`] 时报解码错误
///
/// `content_length` 只能提前拦住声明过大的响应；分块传输没有长度，必须
/// 边读边计数，否则上限形同虚设。
async fn read_body_limited(mut response: reqwest::Response) -> Result<Vec<u8>, SourceError> {
    if response
        .content_length()
        .is_some_and(|length| length > RESPONSE_LIMIT as u64)
    {
        return Err(SourceError::Decode(Box::new(std::io::Error::other(
            format!("response body exceeds {RESPONSE_LIMIT} bytes"),
        ))));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(map_transport)? {
        if body.len() + chunk.len() > RESPONSE_LIMIT {
            return Err(SourceError::Decode(Box::new(std::io::Error::other(
                format!("response body exceeds {RESPONSE_LIMIT} bytes"),
            ))));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// 只读响应体开头 `limit` 字节并转成文本
///
/// 用于错误说明：不关心完整 body，也不该为了一句文案把超大响应读完。
async fn read_text_prefix(mut response: reqwest::Response, limit: usize) -> String {
    let mut body = Vec::new();
    while body.len() < limit {
        let Ok(Some(chunk)) = response.chunk().await else {
            break;
        };
        let take = (limit - body.len()).min(chunk.len());
        body.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            break;
        }
    }
    String::from_utf8_lossy(&body).into_owned()
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
        ensure_http_url(location)?;
        let response = self
            .http
            .get(location.clone())
            .send()
            .await
            .map_err(map_transport)?;
        let status = response.status();
        if !status.is_success() {
            let code = status.as_u16();
            return Err(rejection(code, extract_message(response).await));
        }
        Ok(Box::new(HttpBody { response }))
    }
}

/// 上游响应体里可读的错误说明，最多取 [`MESSAGE_LIMIT`] 字节
///
/// 上游常以纯文本回一句原因（如 Fabric 的 `Unable to find valid version for
/// loader_version`），比只剩状态码有用得多；读不出来时返回空串。
const MESSAGE_LIMIT: usize = 512;

async fn extract_message(response: reqwest::Response) -> Option<String> {
    let text = read_text_prefix(response, MESSAGE_LIMIT * 4).await;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MESSAGE_LIMIT).collect())
}

/// 非 2xx 的归类：有可读说明时用 [`SourceError::Upstream`] 保留原文，
/// 否则退回 [`SourceError::Status`]
fn rejection(status: u16, message: Option<String>) -> SourceError {
    message.map_or(SourceError::Status { status }, |message| {
        SourceError::Upstream { status, message }
    })
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
    parse_http_url(url)?;
    let response = send_with_retry(|| client.get(url), url).await?;
    let status = response.status();
    if !status.is_success() {
        let code = status.as_u16();
        // 目标版本已判定不存在时不读 body：`NotFound` 比上游文案更贴合调用方语义
        if code == 404
            && let Some(id) = not_found_id
        {
            return Err(SourceError::NotFound(id));
        }
        return Err(rejection(code, extract_message(response).await));
    }
    let body = read_body_limited(response).await?;
    serde_json::from_slice(&body).map_err(|err| SourceError::Decode(Box::new(err)))
}

/// POST 一段 JSON body 并反序列化 JSON 响应
///
/// 供 Modrinth 的批量端点使用（`POST /version_files`）。错误归类与 [`get_json`]
/// 一致。`body` 是调用方已序列化好的 JSON 文本，而不是任意 `Serialize`
/// 值：`reqwest` 的 `.json()` 要求启用它的 `serde` feature，而本 crate 的
/// `reqwest` 只开了 `json`（响应侧），手工 `serde_json::to_string` 再以
/// `application/json` 发送既不依赖那层 feature，也让「本地序列化失败」在
/// 调用方就能处理。
///
/// # Errors
///
/// 网络失败、非成功状态码或响应非法时返回 [`SourceError`]
pub(crate) async fn post_json<T>(client: &Client, url: &str, body: &str) -> Result<T, SourceError>
where
    T: DeserializeOwned,
{
    parse_http_url(url)?;
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_owned())
        .send()
        .await
        .map_err(map_transport)?;
    let status = response.status();
    if !status.is_success() {
        return Err(rejection(status.as_u16(), extract_message(response).await));
    }
    let body = read_body_limited(response).await?;
    serde_json::from_slice(&body).map_err(|err| SourceError::Decode(Box::new(err)))
}

/// GET 并读取为纯文本，用于非 JSON 的小文件（如 Forge 的 `.sha1` 侧车）
///
/// 与 [`get_json`] 共用同一套错误归类：`not_found_id` 非空且状态码为 404 时归
/// 为 [`SourceError::NotFound`]，其余非 2xx 走 [`rejection`]。body 读取失败归
/// [`SourceError::Network`]。
///
/// # Errors
///
/// 网络失败、非成功状态码或 body 读取失败时返回 [`SourceError`]
pub(crate) async fn get_text(
    client: &Client,
    url: &str,
    not_found_id: Option<VersionId>,
) -> Result<String, SourceError> {
    parse_http_url(url)?;
    let response = send_with_retry(|| client.get(url), url).await?;
    let status = response.status();
    if !status.is_success() {
        let code = status.as_u16();
        if code == 404
            && let Some(id) = not_found_id
        {
            return Err(SourceError::NotFound(id));
        }
        return Err(rejection(code, extract_message(response).await));
    }
    let body = read_body_limited(response).await?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// 用 `HEAD` 取资源大小（字节）
///
/// 部分上游只给哈希、不给大小（如 Forge 的 `.sha1` 侧车），而下载器需要大小
/// 才能做长度校验与进度。取不到时返回 `None`——调用方应把大小当作「未声明」，
/// 而不是当成 0 做严格比对。
///
/// # Errors
///
/// 网络失败或非成功状态码时返回 [`SourceError`]；仅在「服务器不返回
/// `Content-Length`」时返回 `Ok(None)`
pub(crate) async fn head_size(client: &Client, url: &str) -> Result<Option<u64>, SourceError> {
    parse_http_url(url)?;
    let response = send_with_retry(|| client.head(url), url).await?;
    let status = response.status();
    if !status.is_success() {
        return Err(SourceError::Status {
            status: status.as_u16(),
        });
    }
    Ok(response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|size| *size > 0))
}

#[async_trait]
impl RangeSource for HttpByteSource {
    async fn stat(&self, location: &Url) -> Result<RemoteStat, SourceError> {
        ensure_http_url(location)?;
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
        ensure_http_url(location)?;
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

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "测试夹具，失败直接 panic 才有用"
    )]

    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// 读掉一个请求头，再按给定状态码与 body 回应并关闭连接
    async fn respond(stream: &mut TcpStream, status: u16, body: &str) {
        let mut buf = [0_u8; 1024];
        let _ = stream.read(&mut buf).await;
        let response = format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
    }

    /// 只回响应头，声明一个超大的 `Content-Length`
    async fn respond_with_length(stream: &mut TcpStream, length: usize) {
        let mut buf = [0_u8; 1024];
        let _ = stream.read(&mut buf).await;
        let response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n");
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
    }

    /// 429 / 5xx 是瞬时失败：GET 应重试一次并拿到第二次的结果
    #[tokio::test]
    async fn get_retries_once_on_server_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.expect("first");
            respond(&mut first, 503, "busy").await;
            let (mut second, _) = listener.accept().await.expect("second");
            respond(&mut second, 200, "ok").await;
        });

        let client = build_client().expect("client");
        let text = get_text(&client, &format!("http://{addr}/x"), None)
            .await
            .expect("重试后应当成功");
        assert_eq!(text, "ok");
        server.await.expect("server");
    }

    /// 声明超大的响应体在读取前就被拒绝，不按 Content-Length 预分配内存
    #[tokio::test]
    async fn rejects_oversized_response_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            respond_with_length(&mut stream, RESPONSE_LIMIT + 1).await;
        });

        let client = build_client().expect("client");
        let err = get_text(&client, &format!("http://{addr}/x"), None)
            .await
            .expect_err("超限响应应报错");
        assert!(matches!(err, SourceError::Decode(_)), "{err:?}");
        server.await.expect("server");
    }
}
