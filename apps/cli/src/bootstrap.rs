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

/// 已注册的服务端 JAR 来源（能力查询的数据源）
///
/// 直接复用库侧组合根的 [`mcsup::bootstrap::default_jar_sources`]：CLI 与库侧
/// **只有一份**变体注册表，新增变体不会出现「库支持、CLI 不支持」的漂移。
///
/// 构造不打网络，只准备客户端。
pub(crate) fn jar_sources() -> Result<Vec<Arc<dyn ServerJarSource>>> {
    mcsup::bootstrap::default_jar_sources().context("初始化服务端来源")
}

/// 可创建 / 可安装的变体集合：来源 `variant()` 去重，保持注册顺序
///
/// 在此基础上追加「非 JAR 来源」的变体：整合包由 `port::ModpackSource` 承载，
/// 没有 `ServerJarSource`，但同样应出现在 `server create` 的候选列表里。
pub(crate) fn available_variants() -> Result<Vec<ServerVariant>> {
    let sources = jar_sources()?;
    Ok(available_with_modpack(available_of(&sources)))
}

/// 在可用变体里补上整合包变体，并按「用户需要懂多少」排序
///
/// 排序口径（越不需要用户懂，越靠前）：
///
/// 1. `vanilla`：不用选加载器
/// 2. `modrinth-modpack`：加载器由整包决定，同样不用选
/// 3. 模组加载器（`fabric` / `forge` / `neoforge` / `quilt`）
/// 4. 服务端分支与代理（`paper` / `purpur` / `folia` / `leaf` / `velocity` …）
/// 5. 其余第三方变体保持注册顺序
///
/// 这样整合包一眼可见，且位置有明确理由，而不是随手提到前面。
pub(crate) fn available_with_modpack(mut available: Vec<ServerVariant>) -> Vec<ServerVariant> {
    if !available.contains(&ServerVariant::ModrinthModpack) {
        available.push(ServerVariant::ModrinthModpack);
    }
    sort_by_discovery_order(&mut available);
    available
}

/// 变体在「发现顺序」里的档位；越小越靠前
fn discovery_rank(variant: &ServerVariant) -> u8 {
    match variant {
        ServerVariant::Vanilla => 0,
        ServerVariant::ModrinthModpack => 1,
        ServerVariant::Fabric
        | ServerVariant::Forge
        | ServerVariant::NeoForge
        | ServerVariant::Quilt => 2,
        ServerVariant::Paper
        | ServerVariant::Purpur
        | ServerVariant::Folia
        | ServerVariant::Leaf
        | ServerVariant::Velocity
        | ServerVariant::Waterfall
        | ServerVariant::SpongeVanilla => 3,
        // `Unknown` 是历史数据兜底，不出现在可选列表里；`Custom` 与其余
        // 内置项保持注册顺序
        _ => 4,
    }
}

/// 按档位稳定排序（同档保持注册顺序）
fn sort_by_discovery_order(variants: &mut [ServerVariant]) {
    variants.sort_by_key(discovery_rank);
}

/// 变体在选择器里的展示标签
///
/// 只给「名字本身不足以说明后果」的项加说明。`vanilla` / `fabric` 这类
/// 用户已认识的短名不加后缀，避免整列变长。
#[must_use]
pub(crate) fn variant_label(variant: &ServerVariant) -> String {
    match variant {
        ServerVariant::Vanilla => "vanilla（原版，无需加载器）".to_owned(),
        ServerVariant::ModrinthModpack => "modrinth（从 Modrinth 安装整合包）".to_owned(),
        ServerVariant::Fabric => "fabric（模组加载器）".to_owned(),
        ServerVariant::Forge => "forge（模组加载器）".to_owned(),
        ServerVariant::NeoForge => "neoforge（模组加载器）".to_owned(),
        ServerVariant::Quilt => "quilt（模组加载器）".to_owned(),
        ServerVariant::Paper => "paper（高性能服务端）".to_owned(),
        other => other.as_str().to_owned(),
    }
}

/// 展示标签 → 变体
///
/// 与 [`variant_label`] 互逆。候选里同一变体只有一种标签，因此按全名匹配即可。
#[must_use]
pub(crate) fn variant_from_label(
    picked: &str,
    available: &[ServerVariant],
) -> Option<ServerVariant> {
    available
        .iter()
        .find(|variant| variant_label(variant) == picked)
        .cloned()
}

/// 该变体是否由整合包承载（需要走整合包开服流程）
pub(crate) fn is_modpack(variant: &ServerVariant) -> bool {
    mcsup::service::is_modpack_variant(variant)
}

/// 整合包开服服务
pub(crate) fn modpack_service() -> Result<mcsup::service::ModpackService> {
    let data_dir = config::platform_data_dir();
    let mc = Mcsup::open(&data_dir).context("装配整合包服务")?;
    Ok(mc.modpack)
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
    fn modpack_sorts_right_after_vanilla() {
        let available = available_with_modpack(vec![
            ServerVariant::Paper,
            ServerVariant::Fabric,
            ServerVariant::Vanilla,
        ]);
        assert_eq!(
            available,
            vec![
                ServerVariant::Vanilla,
                ServerVariant::ModrinthModpack,
                ServerVariant::Fabric,
                ServerVariant::Paper,
            ]
        );
    }

    #[test]
    fn modpack_not_duplicated_when_already_available() {
        let available =
            available_with_modpack(vec![ServerVariant::Vanilla, ServerVariant::ModrinthModpack]);
        assert_eq!(
            available
                .iter()
                .filter(|v| **v == ServerVariant::ModrinthModpack)
                .count(),
            1
        );
    }

    #[test]
    fn variant_label_round_trips_through_lookup() {
        let available = available_with_modpack(vec![
            ServerVariant::Vanilla,
            ServerVariant::Fabric,
            ServerVariant::Paper,
        ]);
        for variant in &available {
            let label = variant_label(variant);
            assert_eq!(
                variant_from_label(&label, &available).as_ref(),
                Some(variant),
                "标签 {label} 无法回映"
            );
        }
        assert!(variant_label(&ServerVariant::ModrinthModpack).contains("modrinth"));
    }

    #[test]
    fn jar_sources_reports_vanilla_and_fabric_capability() {
        let available = available_variants().expect("来源装配");
        assert!(available.contains(&ServerVariant::Vanilla), "{available:?}");
        assert!(available.contains(&ServerVariant::Fabric), "{available:?}");
    }
}
