//! mod jar 的读取与上游元数据补齐
//!
//! 两个能力分开而不是合成一个 trait：本地读取（[`ModInspector`]）与本机无关、
//! 永远可用；上游补齐（[`ModMetadataSource`]）需要联网、可能查不到。合并会
//! 让「离线检测」变成「必须联网才能工作」。
//!
//! # 判定不在这一层
//!
//! 这里只回答「归档里有什么」，不回答「这个 mod 能不能装在服务端」。
//! 后者是领域判定，见 [`domain::mod_`]。实现者若把判定写进来，Local 与
//! Remote、CLI 与将来的 API 就会各写一份。

use async_trait::async_trait;
use domain::mod_::{ModCapability, SideSupport};
use std::path::Path;

use crate::SourceError;

/// 本地读取一个 mod jar 的原始信息
///
/// 实现只做读取与解析，不做判断：`side` 字段里只填**能从元数据直接读出的**
/// 值（如 Fabric 的 `environment`、`NeoForge` 的 `side`），其余留
/// [`SideSupport::Unknown`]，由 service 层按证据逐级推断。
pub trait ModInspector: Send + Sync {
    /// 读取一个本地 jar
    ///
    /// `display_path` 只用于结果回显，不参与判定。
    ///
    /// # Errors
    ///
    /// 文件不可读、不是 zip、或归档结构损坏时返回 [`ModInspectError`]。
    /// 元数据文件内容非法**不算**错误：按「认不出」处理，仍返回结果。
    fn inspect(&self, path: &Path) -> Result<ModCapability, ModInspectError>;

    /// 列出归档里的全部类（内部名，`a/b/C`）
    ///
    /// 只给启动崩溃归因用：把日志栈帧里的类反查回具体 jar。
    ///
    /// 默认实现返回 `None`，表示该实现不支持按类名检索。调用方必须把
    /// `None` 当作「不知道」而不是「不含」——后者会让归因静默漏掉整个 mod，
    /// 报告出一份「什么都没查到」的空计划。
    ///
    /// # Errors
    ///
    /// 文件不可读或不是 zip 归档时返回 [`ModInspectError`]
    fn class_names(&self, path: &Path) -> Result<Option<Vec<String>>, ModInspectError> {
        let _ = path;
        Ok(None)
    }
}

/// 本地读取失败
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ModInspectError {
    /// 文件打不开或读失败
    #[error("cannot read {path}: {source}")]
    Io {
        /// 出错的路径
        path: std::path::PathBuf,
        /// 底层原因
        #[source]
        source: std::io::Error,
    },
    /// 不是 zip 归档（改后缀的 exe、截断的下载）
    #[error("not a valid jar/zip archive: {path}")]
    NotAnArchive {
        /// 出错的路径
        path: std::path::PathBuf,
    },
}

/// 上游对某个文件的服务端支持结论
///
/// 与 [`ModCapability`] 不同：上游只给「这个版本能不能装服务端」，不给
/// 加载器 / MC 区间等（那些本地已有）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamSide {
    /// 文件哈希（调用方传入的那个，用于回填）
    pub hash: String,
    /// 上游判定客户端侧
    pub client: SideSupport,
    /// 上游判定服务端侧
    pub server: SideSupport,
    /// 项目 slug（展示用）
    pub slug: Option<String>,
}

/// 按文件哈希向上游查权威的服务端支持标记
///
/// # 为什么按哈希而不是按文件名
///
/// 文件名由整合包作者随意起（`CobbleFurnies-fabric-1.2.jar` 里没有 modid），
/// 而 SHA1 是内容指纹，上游能精确对应到版本。整合包清单里 `mods/*.jar` 的
/// 哈希现成可用；作者自带塞进 `overrides/` 的 jar 则需本地算。
///
/// # 覆盖范围
///
/// 只能查到在 `Modrinth` 上发布过的文件。整合包作者自己写、未公开的 mod
/// 永远查不到——这类必须靠本地证据或实跑验证，调用方要如实报「未知」。
#[async_trait]
pub trait ModMetadataSource: Send + Sync {
    /// 能力名（展示与诊断用）
    fn name(&self) -> &'static str;

    /// 批量查询服务端支持标记
    ///
    /// 返回的集合只含**查到**的哈希；查不到的哈希不出现在结果里，调用方
    /// 据此判定为「上游没有信息」，而不是「上游说是可选」。
    ///
    /// # Errors
    ///
    /// 上游不可达、状态码异常或响应无法解析时返回 [`SourceError`]
    async fn server_support(&self, hashes: &[String]) -> Result<Vec<UpstreamSide>, SourceError>;
}
