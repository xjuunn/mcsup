//! 回环假 HTTP 服务：替代真实网络
//!
//! `provider/tests/forge_net.rs` 现在用 `MCSUP_NET_TESTS=1` 开关控制联网，
//! CI 里永远不跑——等于长期静默失效。真实网络探测应保留为**手动**手段；
//! 协议层（状态码映射、JSON 解析、404 归类）必须能离线测。
//!
//! ```
//! use testkit::{HttpServer, Response};
//!
//! let server = HttpServer::start();
//! server.route("/meta.json", Response::json(r#"{"v":1}"#));
//! let url = server.url("/meta.json"); // http://127.0.0.1:<port>/meta.json
//! ```
//!
//! 只支持「固定回应」：收到请求就写回预设响应。不做路由匹配以外的抽象，
//! 避免测试去模仿一个真服务器。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 假服务返回的一次响应
#[derive(Debug, Clone)]
pub struct Response {
    /// HTTP 状态码
    pub status: u16,
    /// 原因短语
    pub reason: String,
    /// body
    pub body: Vec<u8>,
    /// `Content-Type`
    pub content_type: String,
}

impl Response {
    /// 200 + JSON
    #[must_use]
    pub fn json(body: impl Into<String>) -> Self {
        Self::new(200, "OK", body.into().into_bytes(), "application/json")
    }

    /// 200 + 纯文本
    #[must_use]
    pub fn text(body: impl Into<String>) -> Self {
        Self::new(200, "OK", body.into().into_bytes(), "text/plain")
    }

    /// 指定状态码 + 纯文本 body
    #[must_use]
    pub fn with_status(status: u16, body: impl Into<String>) -> Self {
        Self::new(status, "ERROR", body.into().into_bytes(), "text/plain")
    }

    /// 指定状态码 + 空 body（404 / 500）
    #[must_use]
    pub fn error(status: u16, reason: &str) -> Self {
        Self::new(status, reason, Vec::new(), "text/plain")
    }

    fn new(status: u16, reason: &str, body: Vec<u8>, content_type: &str) -> Self {
        Self {
            status,
            reason: reason.to_owned(),
            body,
            content_type: content_type.to_owned(),
        }
    }

    fn render(&self) -> Vec<u8> {
        let head = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: {}\r\n\r\n",
            self.status,
            self.reason,
            self.body.len(),
            self.content_type
        );
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

#[derive(Debug, Default)]
struct State {
    routes: HashMap<String, Response>,
    /// 收到的路径（按到达顺序）
    requests: Vec<String>,
    /// 收到请求后直接断开（模拟传输中断）
    drop_connection: bool,
}

/// 回环假 HTTP 服务
///
/// 后台线程随实例存活；实例 drop 后线程在下一次 accept 时发现监听已关闭并退出。
#[derive(Debug)]
pub struct HttpServer {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
}

impl HttpServer {
    /// 在随机端口启动
    #[must_use]
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定回环端口");
        let addr = listener.local_addr().expect("local_addr");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");

        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                let listener = TcpListener::from_std(listener).expect("adopt listener");
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let shared = Arc::clone(&shared);
                    tokio::spawn(async move {
                        serve(stream, shared).await;
                    });
                }
            });
        });

        Self { addr, state }
    }

    /// 注册一条固定回应
    pub fn route(&self, path: impl Into<String>, response: Response) -> &Self {
        self.state
            .lock()
            .expect("lock")
            .routes
            .insert(path.into(), response);
        self
    }

    /// 让之后的所有请求直接断开连接
    #[must_use]
    pub fn drop_connections(&self) -> &Self {
        self.state.lock().expect("lock").drop_connection = true;
        self
    }

    /// 拼出完整 URL
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// 已收到的请求路径
    #[must_use]
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().expect("lock").requests.clone()
    }
}

async fn serve(mut stream: TcpStream, state: Arc<Mutex<State>>) {
    let mut buffer = vec![0u8; 4096];
    let mut raw = Vec::new();
    // 只读到请求头结束：body 不参与假服务的判定
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                raw.extend_from_slice(&buffer[..read]);
                if raw.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let path = text
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_owned();

    let (response, drop_now) = {
        let mut state = state.lock().expect("lock");
        state.requests.push(path.clone());
        let drop_now = state.drop_connection;
        (state.routes.get(&path).cloned(), drop_now)
    };

    if drop_now {
        return;
    }
    let response = response.unwrap_or_else(|| Response::error(404, "Not Found"));
    let _ = stream.write_all(&response.render()).await;
    let _ = stream.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assert::{Failure, assert_invariant};
    use std::io::{Read, Write};

    fn get(url: &str) -> (u16, String) {
        let without_scheme = url.trim_start_matches("http://");
        let (addr, path) = match without_scheme.split_once('/') {
            Some((addr, rest)) => (addr.to_owned(), format!("/{rest}")),
            None => (without_scheme.to_owned(), "/".to_owned()),
        };
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("write");
        let mut out = String::new();
        stream.read_to_string(&mut out).expect("read");
        let status = out
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let body = out
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_owned())
            .unwrap_or_default();
        (status, body)
    }

    #[test]
    fn serves_registered_routes() {
        let server = HttpServer::start();
        server.route("/a.json", Response::json(r#"{"ok":true}"#));
        let (status, body) = get(&server.url("/a.json"));
        assert_invariant(status == 200 && body == r#"{"ok":true}"#, || {
            Failure::invariant("已注册路由必须原样返回")
                .expected(r#"200 {"ok":true}"#)
                .actual(format!("{status} {body}"))
        });
    }

    #[test]
    fn unregistered_path_is_404() {
        let server = HttpServer::start();
        let (status, _) = get(&server.url("/missing"));
        assert_invariant(status == 404, || {
            Failure::invariant("未注册路径必须 404").actual(status)
        });
    }

    #[test]
    fn requests_are_recorded() {
        let server = HttpServer::start();
        server.route("/x", Response::text("x"));
        let _ = get(&server.url("/x"));
        let _ = get(&server.url("/x"));
        assert_invariant(server.requests() == ["/x", "/x"], || {
            Failure::invariant("每次请求都要留痕").actual(format!("{:?}", server.requests()))
        });
    }

    #[test]
    fn status_codes_are_forwarded() {
        let server = HttpServer::start();
        server.route("/boom", Response::error(500, "Internal Server Error"));
        let (status, _) = get(&server.url("/boom"));
        assert_invariant(status == 500, || {
            Failure::invariant("状态码必须原样回传").actual(status)
        });
    }

    #[test]
    fn query_string_is_stripped_from_recorded_path() {
        let server = HttpServer::start();
        server.route("/v", Response::text("ok"));
        let _ = get(&server.url("/v?x=1"));
        assert_invariant(server.requests() == ["/v"], || {
            Failure::invariant("记录路径时要去掉 query")
                .expected("[/v]")
                .actual(format!("{:?}", server.requests()))
        });
    }
}
