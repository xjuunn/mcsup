//! Professional-grade Minecraft server infrastructure management platform.
//!
//! MCSUP 把 Minecraft 服务器管理做成可嵌入的库：Rust 项目可以直接依赖
//! `mcsup`，非 Rust 项目通过 `mcsup` CLI 的 JSON / JSONL 输出或
//! `mcsup-server` 的 REST 与 WebSocket 接入。
//!
//! 分层：`domain` 与 `port` 无条件编译，其余模块一律 feature 门控，
//! 使用方只编译自己用到的部分。
//!
//! 模块随实现逐个加入本文件，feature 跟着模块一起声明。

#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
