//! Professional-grade Minecraft server infrastructure management platform.
//!
//! MCSUP 把 Minecraft 服务器管理做成可嵌入的库：Rust 项目可以直接依赖
//! `mcsup`，非 Rust 项目通过 `mcsup` CLI 的 JSON / JSONL 输出或
//! `mcsup-server` 的 REST 与 WebSocket 接入。
//!
//! 门面结构：每个能力都是 feature 门控的可选依赖，`default = []` 强制显式
//! 选择；选中的 crate 在下方按名重导出。`port` 级联带出 `domain`，
//! `service` / `file` / `java` / `provider` / `storage` / `runtime` 级联
//! 带出 `port` 与 `domain`。
//!
//! 新增能力 crate 时同步三处：可选依赖、feature 级联、下方重导出。
//! `bootstrap` 是组合 feature：重导出全部内层并附 [`bootstrap`] 模块
//! （默认组合根，第三方零拼装装配）。

#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// 默认组合根：`McsupBuilder` 一次装配全部默认实现
#[cfg(feature = "bootstrap")]
#[cfg_attr(docsrs, doc(cfg(feature = "bootstrap")))]
pub mod bootstrap;

/// 被连接端：`transport::Handler` 的完整实现（`mcsup remote serve` 与 `mcsup-agent` 共用）
#[cfg(feature = "remote")]
#[cfg_attr(docsrs, doc(cfg(feature = "remote")))]
pub mod remote;

#[cfg(feature = "auth")]
#[cfg_attr(docsrs, doc(cfg(feature = "auth")))]
pub use auth;

#[cfg(feature = "domain")]
#[cfg_attr(docsrs, doc(cfg(feature = "domain")))]
pub use domain;

#[cfg(feature = "port")]
#[cfg_attr(docsrs, doc(cfg(feature = "port")))]
pub use port;

#[cfg(feature = "protocol")]
#[cfg_attr(docsrs, doc(cfg(feature = "protocol")))]
pub use protocol;

#[cfg(feature = "service")]
#[cfg_attr(docsrs, doc(cfg(feature = "service")))]
pub use service;

#[cfg(feature = "backup")]
#[cfg_attr(docsrs, doc(cfg(feature = "backup")))]
pub use backup;

#[cfg(feature = "backend")]
#[cfg_attr(docsrs, doc(cfg(feature = "backend")))]
pub use backend;

#[cfg(feature = "file")]
#[cfg_attr(docsrs, doc(cfg(feature = "file")))]
pub use file;

#[cfg(feature = "java")]
#[cfg_attr(docsrs, doc(cfg(feature = "java")))]
pub use java;

#[cfg(feature = "provider")]
#[cfg_attr(docsrs, doc(cfg(feature = "provider")))]
pub use provider;

#[cfg(feature = "storage")]
#[cfg_attr(docsrs, doc(cfg(feature = "storage")))]
pub use storage;

#[cfg(feature = "runtime")]
#[cfg_attr(docsrs, doc(cfg(feature = "runtime")))]
pub use runtime;

#[cfg(feature = "transport")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport")))]
pub use transport;
