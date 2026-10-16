//! Professional-grade Minecraft server infrastructure management platform.
//!
//! MCSUP 把 Minecraft 服务器管理做成可嵌入的库：Rust 项目可以直接依赖
//! `mcsup`，非 Rust 项目通过 `mcsup` CLI 的 JSON / JSONL 输出或
//! `mcsup-server` 的 REST 与 WebSocket 接入。
//!
//! 门面结构：每个能力都是 feature 门控的可选依赖，`default = []` 强制显式
//! 选择；选中的 crate 在下方按名重导出。`port` 级联带出 `domain`，
//! `service` / `file` / `provider` 级联带出 `port` 与 `domain`。
//!
//! 新增能力 crate 时同步三处：可选依赖、feature 级联、下方重导出。

#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "domain")]
#[cfg_attr(docsrs, doc(cfg(feature = "domain")))]
pub use domain;

#[cfg(feature = "port")]
#[cfg_attr(docsrs, doc(cfg(feature = "port")))]
pub use port;

#[cfg(feature = "service")]
#[cfg_attr(docsrs, doc(cfg(feature = "service")))]
pub use service;

#[cfg(feature = "file")]
#[cfg_attr(docsrs, doc(cfg(feature = "file")))]
pub use file;

#[cfg(feature = "provider")]
#[cfg_attr(docsrs, doc(cfg(feature = "provider")))]
pub use provider;
