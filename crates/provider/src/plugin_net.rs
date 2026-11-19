//! `net:outbound` 的宿主代发实现
//!
//! 复用 `reqwest` 阻塞客户端（与其它 provider 相同的 TLS / 代理配置），并在
//! 每次请求前做两层校验：
//!
//! 1. 主机名匹配 `manifest.net.allow`（精确或点边界后缀，禁止子串匹配）；
//! 2. 解析出的全部地址必须是公网地址，拒绝回环、私网、链路本地、CGNAT 与
//!    云元数据段。
//!
//! 校验通过后把 DNS 结果钉进 `ClientBuilder::resolve`，避免解析校验与实际
//! 连接之间被重新解析（DNS rebinding）。重定向不交给 reqwest：手动跟随最多
//! 3 跳，每一跳重新做主机与地址校验。

use port::plugin::ErrorCode;
use port::plugin::{HostError, NetPolicy, NetProxy, NetRequest, NetResponse};
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_LENGTH, LOCATION};
use reqwest::redirect::Policy;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

/// 响应体上限（docs/plugins.md 9.2）
pub const MAX_NET_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// 最多跟随的重定向次数
pub const MAX_NET_REDIRECTS: usize = 3;
/// 单次请求允许的最大超时
pub const MAX_NET_TIMEOUT: Duration = Duration::from_secs(120);
/// 建立连接超时
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 宿主代发 HTTP 客户端
pub struct HttpNetProxy {
    policy: Arc<dyn NetPolicy>,
}

impl std::fmt::Debug for HttpNetProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpNetProxy").finish_non_exhaustive()
    }
}

impl HttpNetProxy {
    /// 由域名策略构造
    #[must_use]
    pub fn new(policy: Arc<dyn NetPolicy>) -> Self {
        Self { policy }
    }
}

impl NetProxy for HttpNetProxy {
    fn request(
        &self,
        plugin: &domain::plugin::PluginId,
        request: NetRequest,
    ) -> Result<NetResponse, HostError> {
        let allowed = self.policy.allowed_hosts(plugin)?;
        if allowed.is_empty() {
            return Err(HostError::denied(
                "manifest.net.allow is empty; outbound net is denied",
            ));
        }
        let timeout = if request.timeout.is_zero() {
            MAX_NET_TIMEOUT
        } else {
            request.timeout.min(MAX_NET_TIMEOUT)
        };
        let deadline = Instant::now() + timeout;
        let mut url = Url::parse(&request.url)
            .map_err(|err| HostError::invalid_input(format!("invalid URL: {err}")))?;
        let mut method = request.method.to_ascii_uppercase();
        if method.is_empty() {
            "GET".clone_into(&mut method);
        }
        let mut body = request.body.clone();
        let mut headers = sanitize_headers(&request.headers);

        for hop in 0..=MAX_NET_REDIRECTS {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(HostError::new(ErrorCode::Timeout, "net request timed out"));
            }
            let validated = validate_url(&url, &allowed)?;
            let response = send(
                &method,
                &url,
                &validated,
                &headers,
                body.as_deref(),
                remaining,
            )?;
            let status = response.status();
            if status.is_redirection()
                && let Some(location) = response.headers().get(LOCATION)
            {
                if hop == MAX_NET_REDIRECTS {
                    return Err(HostError::limit_exceeded(format!(
                        "net request exceeded {MAX_NET_REDIRECTS} redirects"
                    )));
                }
                let location = location.to_str().map_err(|_| {
                    HostError::new(
                        ErrorCode::Unavailable,
                        "redirect location is not valid UTF-8",
                    )
                })?;
                let next = url.join(location).map_err(|err| {
                    HostError::new(
                        ErrorCode::Unavailable,
                        format!("invalid redirect target: {err}"),
                    )
                })?;
                // 303，或 301/302 的非 GET/HEAD，改为 GET 并丢弃请求体。
                if status.as_u16() == 303
                    || (matches!(status.as_u16(), 301 | 302)
                        && !matches!(method.as_str(), "GET" | "HEAD"))
                {
                    "GET".clone_into(&mut method);
                    body = None;
                    headers.retain(|(name, _)| !is_content_header(name));
                }
                url = next;
                continue;
            }
            return read_response(response);
        }
        Err(HostError::limit_exceeded(format!(
            "net request exceeded {MAX_NET_REDIRECTS} redirects"
        )))
    }
}

fn send(
    method: &str,
    url: &Url,
    addrs: &[SocketAddr],
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<reqwest::blocking::Response, HostError> {
    let mut builder = Client::builder()
        .redirect(Policy::none())
        .connect_timeout(CONNECT_TIMEOUT.min(timeout))
        .timeout(timeout)
        .user_agent(concat!("mcsup-plugin-net/", env!("CARGO_PKG_VERSION")));
    // 把校验过的全部地址钉进解析结果：只钉第一个会在目标只监听 IPv4（或
    // 只有 IPv6 出口）时把本来可用的连接打掉。IP 字面量无需解析。
    if let Some(host) = url.host_str()
        && host.parse::<IpAddr>().is_err()
        && !addrs.is_empty()
    {
        builder = builder.resolve_to_addrs(host, addrs);
    }
    let client = builder.build().map_err(|err| {
        HostError::new(ErrorCode::Unavailable, format!("build net client: {err}"))
    })?;
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|err| HostError::invalid_input(format!("invalid HTTP method: {err}")))?;
    let mut outgoing = client.request(method, url.clone());
    for (name, value) in headers {
        outgoing = outgoing.header(name.as_str(), value.as_str());
    }
    if let Some(body) = body {
        outgoing = outgoing.body(body.to_vec());
    }
    outgoing.send().map_err(|err| map_reqwest_error(&err))
}

fn read_response(response: reqwest::blocking::Response) -> Result<NetResponse, HostError> {
    if let Some(length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        && length > MAX_NET_RESPONSE_BYTES as u64
    {
        return Err(HostError::limit_exceeded(format!(
            "net response is {length} bytes, limit is {MAX_NET_RESPONSE_BYTES}"
        )));
    }
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect();
    let mut body = Vec::new();
    response
        .take(MAX_NET_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|err| {
            HostError::new(ErrorCode::Unavailable, format!("read net response: {err}"))
        })?;
    if body.len() > MAX_NET_RESPONSE_BYTES {
        return Err(HostError::limit_exceeded(format!(
            "net response exceeds {MAX_NET_RESPONSE_BYTES} bytes"
        )));
    }
    Ok(NetResponse {
        status,
        headers,
        body,
    })
}

fn map_reqwest_error(err: &reqwest::Error) -> HostError {
    if err.is_timeout() {
        HostError::new(ErrorCode::Timeout, format!("net request timed out: {err}"))
    } else if err.is_connect() {
        HostError::new(ErrorCode::Unavailable, format!("net connect failed: {err}"))
    } else {
        HostError::new(ErrorCode::Unavailable, format!("net request failed: {err}"))
    }
}

/// 校验 URL、主机白名单与解析地址，返回可用于钉住的地址
fn validate_url(url: &Url, allowed: &[String]) -> Result<Vec<SocketAddr>, HostError> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(HostError::unsupported(format!(
                "net scheme {other} is not supported; only http/https"
            )));
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(HostError::denied("URL userinfo is not allowed"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| HostError::invalid_input("URL has no host"))?;
    if !host_allowed(host, allowed) {
        return Err(HostError::denied(format!(
            "{host} is not in manifest.net.allow"
        )));
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => (host, port)
            .to_socket_addrs()
            .map_err(|err| {
                HostError::new(
                    ErrorCode::Unavailable,
                    format!("resolve {host}:{port}: {err}"),
                )
            })?
            .collect(),
    };
    if addrs.is_empty() {
        return Err(HostError::new(
            ErrorCode::Unavailable,
            format!("{host} resolved to no address"),
        ));
    }
    if let Some(addr) = addrs.iter().find(|addr| !is_public_ip(addr.ip())) {
        return Err(HostError::denied(format!(
            "{host} resolves to non-public address {}",
            addr.ip()
        )));
    }
    Ok(addrs)
}

/// 主机名是否命中白名单
///
/// 规则是精确匹配或点边界后缀：`telegram.org` 命中 `telegram.org` 与
/// `api.telegram.org`，不命中 `eviltelegram.org`。
#[must_use]
pub fn host_allowed(host: &str, allowed: &[String]) -> bool {
    let host = normalize_host(host);
    if host.is_empty() {
        return false;
    }
    allowed.iter().any(|rule| {
        let rule = normalize_host(rule.trim_start_matches("*."));
        !rule.is_empty() && (host == rule || host.ends_with(&format!(".{rule}")))
    })
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// 是否公网地址
///
/// 只放行全球单播地址；回环、私网、链路本地、CGNAT、文档、基准测试、组播
/// 与保留段全部拒绝。
#[must_use]
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            // IPv4-mapped / IPv4-compatible：按内嵌 IPv4 判定，否则
            // `::ffff:169.254.169.254` 会绕过私网检查。
            if segments.starts_with(&[0, 0, 0, 0, 0]) && matches!(segments[5], 0 | 0xffff) {
                return is_public_v4(v4_from(segments[6], segments[7]));
            }
            // NAT64 64:ff9b::/96 与 6to4 2002::/16 也携带 IPv4 地址，
            // 转换前缀不能成为访问私网或元数据地址的跳板。
            if segments.starts_with(&[0x0064, 0xff9b, 0, 0, 0, 0]) {
                return is_public_v4(v4_from(segments[6], segments[7]));
            }
            if segments[0] == 0x2002 {
                return is_public_v4(v4_from(segments[1], segments[2]));
            }
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || segments[0] & 0xfe00 == 0xfc00
                || segments[0] & 0xffc0 == 0xfe80
                || segments[0] & 0xffc0 == 0xfec0)
        }
    }
}

fn v4_from(hi: u16, lo: u16) -> Ipv4Addr {
    Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo))
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    let [a, b, c, _] = octets;
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && (18..=19).contains(&b))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn sanitize_headers(headers: &std::collections::BTreeMap<String, String>) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn is_content_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-length" | "content-type" | "content-encoding" | "transfer-encoding"
    )
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "proxy-connection"
            | "upgrade"
            | "keep-alive"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::plugin::PluginId;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    struct FixedPolicy {
        hosts: Mutex<Vec<String>>,
    }

    impl NetPolicy for FixedPolicy {
        fn allowed_hosts(&self, _plugin: &PluginId) -> Result<Vec<String>, HostError> {
            Ok(self.hosts.lock().expect("lock").clone())
        }
    }

    fn proxy(hosts: &[&str]) -> (HttpNetProxy, PluginId) {
        let policy = FixedPolicy {
            hosts: Mutex::new(hosts.iter().map(|host| (*host).to_owned()).collect()),
        };
        (
            HttpNetProxy::new(Arc::new(policy)),
            PluginId::parse("demo").expect("id"),
        )
    }

    fn request(url: &str) -> NetRequest {
        NetRequest {
            method: "GET".to_owned(),
            url: url.to_owned(),
            headers: BTreeMap::new(),
            body: None,
            timeout: Duration::from_secs(2),
        }
    }

    #[test]
    fn allowlist_uses_dot_boundaries() {
        let allow = vec!["telegram.org".to_owned()];
        assert!(host_allowed("telegram.org", &allow));
        assert!(host_allowed("api.telegram.org", &allow));
        assert!(!host_allowed("eviltelegram.org", &allow));
        assert!(!host_allowed("telegram.org.evil.com", &allow));
        let wildcard = vec!["*.example.com".to_owned()];
        assert!(host_allowed("a.example.com", &wildcard));
        assert!(host_allowed("example.com", &wildcard));
    }

    #[test]
    fn non_public_addresses_are_rejected() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "::10.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "2002:a9fe:a9fe::1",
        ] {
            assert!(!is_public_ip(ip.parse().expect("ip")), "{ip}");
        }
        assert!(is_public_ip("1.1.1.1".parse().expect("ip")));
        assert!(is_public_ip("2606:4700:4700::1111".parse().expect("ip")));
    }

    #[test]
    fn empty_allowlist_denies_everything() {
        let (proxy, plugin) = proxy(&[]);
        let err = proxy
            .request(&plugin, request("https://example.com"))
            .expect_err("denied");
        assert_eq!(err.code, ErrorCode::Denied);
    }

    #[test]
    fn localhost_is_denied_even_when_allowlisted() {
        let (proxy, plugin) = proxy(&["localhost"]);
        let err = proxy
            .request(&plugin, request("http://localhost:8080/"))
            .expect_err("ssrf");
        assert_eq!(err.code, ErrorCode::Denied);
        assert!(err.message.contains("non-public"), "{}", err.message);
    }

    #[test]
    fn scheme_and_userinfo_are_rejected() {
        let (proxy, plugin) = proxy(&["example.com"]);
        let err = proxy
            .request(&plugin, request("file:///etc/passwd"))
            .expect_err("scheme");
        assert_eq!(err.code, ErrorCode::Unsupported);
        let err = proxy
            .request(&plugin, request("https://user:pass@example.com/"))
            .expect_err("userinfo");
        assert_eq!(err.code, ErrorCode::Denied);
    }
}
