//! mod jar 的本地读取实现
//!
//! 只做读取与解析，不做判定（判定在 [`domain::mod_`]）。三块能力：
//!
//! - [`ZipModInspector`]：读归档、认加载器、抽元数据
//! - `json`：容错 JSON 解析（真实 jar 里存在非法 JSON）
//! - `bytecode`：class 文件头取 Java 版本、常量池扫客户端类引用
//!
//! # 为什么需要容错 JSON
//!
//! 实测某整合包的 14 个 fabric mod 里有 2 个（Debugify、CobbleverseBadges）
//! 的 `fabric.mod.json` 在 `description` 字符串里直接写了**裸换行符**，任何
//! 严格 JSON 解析器都会拒绝。这两个 mod 本身完全可用——只是元数据不合规。
//! 若不修补，14% 的真实 mod 会被判成「格式未知」。

mod bytecode;
mod json;
mod zip;

pub use zip::ZipModInspector;
