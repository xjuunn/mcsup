use crate::config;
use anyhow::{Context, Result, anyhow};
use mcsup::domain::server::version::ServerVariant;
use mcsup::file::{CommandExtractor, FileDownloader};
use mcsup::java::HostJavaScanner;
use mcsup::port::ServerJarSource;
use mcsup::provider::http::HttpByteSource;
use mcsup::provider::java::JavaSourceRegistry;
use mcsup::provider::mojang::MojangSource;
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

/// 已注册的服务端 JAR 来源（能力查询的数据源；接入新 provider 时在此装配）
///
/// 构造不打网络，只准备客户端。
pub(crate) fn jar_sources() -> Result<Vec<Arc<dyn ServerJarSource>>> {
    Ok(vec![Arc::new(
        MojangSource::new().context("初始化 Mojang 源")?,
    )])
}

/// 可创建 / 可安装的变体集合：来源 `variant()` 去重，保持注册顺序
pub(crate) fn available_variants() -> Result<Vec<ServerVariant>> {
    let sources = jar_sources()?;
    Ok(available_of(&sources))
}

/// [`available_variants`] 的纯逻辑内核（可注入假来源测试）
pub(crate) fn available_of(sources: &[Arc<dyn ServerJarSource>]) -> Vec<ServerVariant> {
    let mut out: Vec<ServerVariant> = Vec::new();
    for source in sources {
        let variant = source.variant();
        if !out.contains(&variant) {
            out.push(variant);
        }
    }
    out
}

/// 变体 → 来源（[`available_of`] 的纯逻辑内核）
///
/// `Unknown` 是历史数据兜底（旧注册 / marker 缺失），回落 vanilla 来源；
/// 其余未注册变体返回 `None`，由调用方给出「可用变体」错误。
pub(crate) fn pick_source(
    variant: ServerVariant,
    sources: &[Arc<dyn ServerJarSource>],
) -> Option<Arc<dyn ServerJarSource>> {
    let target = if variant == ServerVariant::Unknown {
        ServerVariant::Vanilla
    } else {
        variant
    };
    sources.iter().find(|s| s.variant() == target).cloned()
}

/// 变体 → 来源；未注册变体报错并列出当前可用项
pub(crate) fn source_for(variant: ServerVariant) -> Result<Arc<dyn ServerJarSource>> {
    let sources = jar_sources()?;
    pick_source(variant, &sources)
        .ok_or_else(|| anyhow!("{}", unsupported_message(variant, &available_of(&sources))))
}

/// 未支持 / 未知变体的统一错误文案（纯逻辑，入口与测试共用）
pub(crate) fn unsupported_message(variant: ServerVariant, available: &[ServerVariant]) -> String {
    format!(
        "暂未支持变体 {}；当前可用：{}",
        variant.as_str(),
        join_variants(available)
    )
}

/// 可用变体拼接为 `vanilla、paper` 形式
pub(crate) fn join_variants(available: &[ServerVariant]) -> String {
    if available.is_empty() {
        return "（无）".to_owned();
    }
    available
        .iter()
        .map(|v| v.as_str())
        .collect::<Vec<_>>()
        .join("、")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcsup::domain::server::version::{ServerVersion, VersionId};
    use mcsup::port::{ResolvedArtifact, SourceError};

    struct FakeSource {
        variant: ServerVariant,
    }

    #[async_trait::async_trait]
    impl ServerJarSource for FakeSource {
        fn variant(&self) -> ServerVariant {
            self.variant
        }

        async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError> {
            Ok(Vec::new())
        }

        async fn artifact(&self, version: &VersionId) -> Result<ResolvedArtifact, SourceError> {
            // 能力查询测试不触网：版本解析走 NotFound 即可
            Err(SourceError::NotFound(version.clone()))
        }
    }

    fn sources(variants: &[ServerVariant]) -> Vec<Arc<dyn ServerJarSource>> {
        variants
            .iter()
            .map(|v| Arc::new(FakeSource { variant: *v }) as Arc<dyn ServerJarSource>)
            .collect()
    }

    #[test]
    fn available_variants_dedupe_in_registration_order() {
        let sources = sources(&[
            ServerVariant::Vanilla,
            ServerVariant::Paper,
            ServerVariant::Vanilla,
        ]);
        assert_eq!(
            available_of(&sources),
            vec![ServerVariant::Vanilla, ServerVariant::Paper]
        );
        assert!(available_of(&[]).is_empty());
    }

    #[test]
    fn pick_source_finds_by_variant_and_unknown_falls_back_to_vanilla() {
        let list = sources(&[ServerVariant::Vanilla, ServerVariant::Paper]);
        assert!(pick_source(ServerVariant::Paper, &list).is_some());
        // Unknown → vanilla 兜底
        assert!(pick_source(ServerVariant::Unknown, &list).is_some());
        let vanilla_only = sources(&[ServerVariant::Vanilla]);
        assert!(pick_source(ServerVariant::Paper, &vanilla_only).is_none());
        assert!(pick_source(ServerVariant::Paper, &[]).is_none());
    }

    #[test]
    fn unsupported_message_lists_available_variants() {
        let msg = unsupported_message(ServerVariant::Paper, &[ServerVariant::Vanilla]);
        assert!(msg.contains("paper"), "{msg}");
        assert!(msg.contains("vanilla"), "{msg}");
        let empty = unsupported_message(ServerVariant::Paper, &[]);
        assert!(empty.contains("无"), "{empty}");
    }

    #[test]
    fn jar_sources_reports_vanilla_capability() {
        let available = available_variants().expect("来源装配");
        assert!(available.contains(&ServerVariant::Vanilla), "{available:?}");
    }
}
