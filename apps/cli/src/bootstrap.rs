use crate::config;
use crate::locale::Lang;
use anyhow::{Context, Result, anyhow};
use mcsup::bootstrap::Mcsup;
use mcsup::domain::server::version::ServerVariant;
use mcsup::port::ServerJarSource;
use mcsup::service::JavaService;
use std::path::PathBuf;
use std::sync::Arc;

/// 打开元数据仓库（配置目录下的 `mcsup/db`，一条记录一个 JSON 文件）
///
/// 所有命令从这里取库，禁止各自拼 `config::database_path()` + `open`。
pub(crate) fn open_store() -> Result<Arc<mcsup::storage::JsonStore>> {
    let path = config::database_path();
    let store = mcsup::storage::JsonStore::open(&path)
        .with_context(|| format!("打开元数据仓库 {}", path.display()))?;
    Ok(Arc::new(store))
}

/// 装配 `ServerRegistry`（服务器 + 工作目录两个仓库）
pub(crate) fn open_registry() -> Result<Arc<mcsup::service::ServerRegistry>> {
    let store = open_store()?;
    Ok(Arc::new(mcsup::service::ServerRegistry::new(
        store.clone(),
        store,
    )))
}

/// 平台 Java 安装根目录（`{data_dir}/mcsup/java`），不存在则创建
pub(crate) fn java_root() -> Result<PathBuf> {
    let root = config::platform_data_dir().join("java");
    std::fs::create_dir_all(&root).with_context(|| format!("创建 Java 目录 {}", root.display()))?;
    Ok(root)
}

/// 装配 `JavaService`
///
/// 源列表在此只组装不打网络；延迟探测排序由 service 在安装阶段
/// （[`mcsup::service::InstallStage::Probing`]）触发，本地命令因此零网络开销。
pub(crate) fn open_java_service() -> Result<JavaService> {
    let scanner = Arc::new(mcsup::java::HostJavaScanner::new());
    let registry = Arc::new(mcsup::provider::java::JavaSourceRegistry::all_sources());
    let downloader = Arc::new(build_downloader()?);
    let extractor = Arc::new(mcsup::file::CommandExtractor::new());
    Ok(JavaService::new(
        open_store()?,
        scanner,
        registry,
        downloader,
        extractor,
        java_root()?,
    ))
}

/// 组装下载器：HTTP 传输 + 断点续传范围请求
pub(crate) fn build_downloader() -> Result<mcsup::file::FileDownloader> {
    let bytes = Arc::new(mcsup::provider::http::HttpByteSource::new().context("初始化 HTTP 传输")?);
    Ok(mcsup::file::FileDownloader::new(bytes.clone()).with_range_source(bytes))
}

/// 已注册的服务端 JAR 来源（能力查询的数据源；接入新 provider 时在此装配）
///
/// 构造不打网络，只准备客户端。
pub(crate) fn jar_sources() -> Result<Vec<Arc<dyn ServerJarSource>>> {
    Ok(vec![
        Arc::new(mcsup::provider::mojang::MojangSource::new().context("初始化 Mojang 源")?),
        Arc::new(mcsup::provider::fabric::FabricSource::new().context("初始化 Fabric 源")?),
        Arc::new(mcsup::provider::forge::ForgeSource::new().context("初始化 Forge 源")?),
    ])
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
    variant: &ServerVariant,
    sources: &[Arc<dyn ServerJarSource>],
) -> Option<Arc<dyn ServerJarSource>> {
    let target = if *variant == ServerVariant::Unknown {
        &ServerVariant::Vanilla
    } else {
        variant
    };
    sources.iter().find(|s| &s.variant() == target).cloned()
}

/// 变体 → 来源；未注册变体报错并列出当前可用项
pub(crate) fn source_for(variant: &ServerVariant, lang: Lang) -> Result<Arc<dyn ServerJarSource>> {
    let sources = jar_sources()?;
    pick_source(variant, &sources).ok_or_else(|| {
        anyhow!(
            "{}",
            unsupported_message(variant, &available_of(&sources), lang)
        )
    })
}

/// 未支持 / 未知变体的统一错误文案（纯逻辑，入口与测试共用）
pub(crate) fn unsupported_message(
    variant: &ServerVariant,
    available: &[ServerVariant],
    lang: Lang,
) -> String {
    crate::i18n::pick(
        lang,
        format!(
            "暂未支持变体 {}；当前可用：{}",
            variant.as_str(),
            join_variants(available, lang)
        ),
        format!(
            "variant not supported yet: {}; currently available: {}",
            variant.as_str(),
            join_variants(available, lang)
        ),
    )
}

/// 可用变体拼接为 `vanilla、paper` / `vanilla, paper` 形式
pub(crate) fn join_variants(available: &[ServerVariant], lang: Lang) -> String {
    if available.is_empty() {
        return crate::i18n::text(lang, "（无）", "(none)").to_owned();
    }
    let sep = crate::i18n::text(lang, "、", ", ");
    available
        .iter()
        .map(ServerVariant::as_str)
        .collect::<Vec<_>>()
        .join(sep)
}

/// 库侧默认组合根（`mcsup::bootstrap`）：CLI 全部命令的统一装配口径。
///
/// 单一入口：路径来自 `config`，网络能力与默认实现全部来自库侧组合根，
/// CLI 不再自己拼 `JsonStore` / 下载器 / Java 服务。第三方宿主用同一个
/// 组合根即可获得与 CLI 完全一致的默认行为。
pub(crate) fn open_mcsup() -> Result<Mcsup> {
    let data_dir = config::platform_data_dir();
    Mcsup::open(&data_dir).context("装配 mcsup 组合根")
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
            self.variant.clone()
        }

        fn name(&self) -> &'static str {
            "fake"
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
            .map(|v| Arc::new(FakeSource { variant: v.clone() }) as Arc<dyn ServerJarSource>)
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
        assert!(pick_source(&ServerVariant::Paper, &list).is_some());
        // Unknown → vanilla 兜底
        assert!(pick_source(&ServerVariant::Unknown, &list).is_some());
        let vanilla_only = sources(&[ServerVariant::Vanilla]);
        assert!(pick_source(&ServerVariant::Paper, &vanilla_only).is_none());
        assert!(pick_source(&ServerVariant::Paper, &[]).is_none());
    }

    #[test]
    fn unsupported_message_lists_available_variants() {
        let msg = unsupported_message(&ServerVariant::Paper, &[ServerVariant::Vanilla], Lang::Zh);
        assert!(msg.contains("paper"), "{msg}");
        assert!(msg.contains("vanilla"), "{msg}");
        let empty = unsupported_message(&ServerVariant::Paper, &[], Lang::Zh);
        assert!(empty.contains("无"), "{empty}");
    }

    #[test]
    fn jar_sources_reports_vanilla_and_fabric_capability() {
        let available = available_variants().expect("来源装配");
        assert!(available.contains(&ServerVariant::Vanilla), "{available:?}");
        assert!(available.contains(&ServerVariant::Fabric), "{available:?}");
    }
}
