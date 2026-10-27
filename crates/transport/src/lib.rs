//! 远端传输：TLS、自签身份、帧收发与握手
//!
//! 这一层只回答「字节怎么安全地过去」，不回答「这条命令该不该执行」：
//! 帧类型来自 `protocol::remote`，权限判定来自 `auth`，业务由注入的
//! [`Handler`]（agent 侧）或调用方（客户端侧）承担。
//!
//! 刻意保持**同步**：`service` 的编排接口是同步的，CLI 是单命令进程，
//! 阻塞即本任务的全部工作。异步只会在中间多出一层 `block_on`，没有收益。
//!
//! # 连接生命周期
//!
//! ```text
//! TCP → TLS（客户端固定服务端证书指纹）→ Hello → [Enroll] → Authenticate
//!     → 一串 Request / Response
//! ```
//!
//! TLS 必须在任何业务帧之前完成，认证在业务帧之前完成：把两者拆开是为了
//! 让「首次配对」与「日常连接」共用同一条通道，而不是开一条明文端口做配对。
//!
//! # 为什么固定指纹而不是 CA
//!
//! 每台被连接端各自生成自签证书，没有公共 CA 可依赖。信任建立在
//! 「首次连接时用户确认的那一个指纹」上（TOFU，`docs/remote.md`「安全」）：
//! 客户端记住指纹，之后每次握手都比对，不一致即拒绝——比「有 CA 就信」更强，
//! 因为它不依赖任何第三方。

#![warn(missing_docs)]

pub mod client;
pub mod error;
pub mod fingerprint;
pub mod handler;
pub mod identity;
pub mod server;
pub mod session;
pub mod tls;

pub use client::{Client, Probe};
pub use error::TransportError;
pub use fingerprint::Fingerprint;
pub use handler::{Enrolled, Greeting, Handler, RemoteFailure, SessionInfo};
pub use identity::ServerIdentity;
pub use server::{Server, StopHandle};
pub use session::Session;
