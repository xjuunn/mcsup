use crate::config;
use anyhow::{Context, Result};
use mcsup::file::{CommandExtractor, FileDownloader};
use mcsup::java::HostJavaScanner;
use mcsup::provider::http::HttpByteSource;
use mcsup::provider::java::JavaSourceRegistry;
use mcsup::service::{JavaService, ServerRegistry};
use mcsup::storage::SqliteStore;
use std::path::PathBuf;
use std::sync::Arc;

/// 打开元数据库（配置目录下的 `mcsup/db`）
///
/// 所有命令从这里取库，禁止各自拼 `config::database_path()` + `open`。
pub(crate) fn open_store() -> Result<Arc<SqliteStore>> {
    let path = config::database_path()?;
    let store =
        SqliteStore::open(&path).with_context(|| format!("打开元数据库 {}", path.display()))?;
    Ok(Arc::new(store))
}

/// 装配 `ServerRegistry`（服务器 + 工作目录两个仓库）
pub(crate) fn open_registry() -> Result<Arc<ServerRegistry>> {
    let store = open_store()?;
    Ok(Arc::new(ServerRegistry::new(store.clone(), store)))
}

/// 平台 Java 安装根目录（`{data_dir}/mcsup/java`），不存在则创建
pub(crate) fn java_root() -> Result<PathBuf> {
    let root = config::platform_data_dir()?.join("java");
    std::fs::create_dir_all(&root).with_context(|| format!("创建 Java 目录 {}", root.display()))?;
    Ok(root)
}

/// 组装下载器：HTTP 传输 + 断点续传范围请求
pub(crate) fn build_downloader() -> Result<FileDownloader> {
    let bytes = Arc::new(HttpByteSource::new().context("初始化 HTTP 传输")?);
    Ok(FileDownloader::new(bytes.clone()).with_range_source(bytes))
}

/// 装配 `JavaService`
///
/// 源列表在此只组装不打网络；延迟探测排序由 service 在安装阶段
/// （[`mcsup::service::InstallStage::Probing`]）触发，本地命令因此零网络开销。
pub(crate) fn open_java_service() -> Result<JavaService> {
    let scanner = Arc::new(HostJavaScanner::new());
    let registry = Arc::new(JavaSourceRegistry::all_sources());
    let downloader = Arc::new(build_downloader()?);
    let extractor = Arc::new(CommandExtractor::new());
    Ok(JavaService::new(
        open_store()?,
        scanner,
        registry,
        downloader,
        extractor,
        java_root()?,
    ))
}
