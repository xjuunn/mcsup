//! mod 检测编排：本地读取 → 上游补齐 → 风险汇总
//!
//! 只做编排与合并。判定（三态归属、风险等级、区间比较）全在
//! [`domain::mod_`]，读取在 `port::mod_` 的实现（[`modkit`] / Provider）。
//!
//! # 为什么本地优先
//!
//! 上游只能查到**在平台上发布过**的文件。整合包作者自己写、塞进
//! `overrides/mods/` 的 mod 永远查不到——而这类恰好是最需要检查的（作者
//! 可能把仅客户端 mod 一起打包）。因此顺序固定为：
//!
//! 1. 本地读取（永远可用、毫秒级）
//! 2. 本地判不出、且允许联网时，才向上游查
//! 3. 仍判不出 → [`domain::mod_::Risk::Review`]，如实报告「未知」
//!
//! 绝不把「判不出」写成「可以装」。
//!
//! # 与 [`crate::ModpackService`] 的关系
//!
//! 本服务只**报告**，不改开服流程：不删文件、不修改清单、不阻断安装。
//! 是否排除某个 mod 由用户决定。
//!
//! # 哈希
//!
//! 上游按 SHA1 索引。清单里 `mods/*.jar` 自带哈希，但 `overrides/` 里的
//! 文件没有，需要本地算——这一步由调用方决定是否值得（大包会很慢），
//! 因此只在需要联网兜底时才计算。

use crate::fs_error::FsError;
use domain::mod_::{ModCapability, ModLoader, Risk, ServerFit, ServerTarget};
use port::mod_::{ModInspectError, ModInspector, ModMetadataSource};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, warn};

/// mod 检测失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ModInspectServiceError {
    /// 本地读取失败（文件不可读 / 不是 zip）
    #[error(transparent)]
    Inspect(#[from] ModInspectError),
    /// 目录遍历失败
    #[error(transparent)]
    Fs(#[from] FsError),
    /// 上游查询失败
    ///
    /// 上游不可用**不应**让整次检测失败：本地结论仍然有效。调用方可以
    /// 选择降级（保留 `Unknown`）而不是报错，见
    /// [`ModInspectService::inspect_paths`] 的 `allow_upstream` 参数。
    #[error("upstream metadata lookup failed: {0}")]
    Upstream(String),
}

/// 一条检测结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModReport {
    /// 检测结果
    pub capability: ModCapability,
    /// 综合风险（已叠加依赖缺失）
    pub risk: Risk,
    /// 缺失的必需依赖（已在包内其他 mod 中找不到的）
    pub missing_deps: Vec<String>,
    /// 与目标服务端的匹配结论；未指定目标（`mod inspect` 无 `--mc` /
    /// `--server`）时为 `None`——没问就不给结论，别让「没问」看起来像
    /// 「查过了」
    pub fit: Option<ServerFit>,
}

/// 整批检测结果
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModAudit {
    /// 逐个 mod 的结果，按路径排序
    pub mods: Vec<ModReport>,
    /// 上游是否真的被查询过
    pub queried_upstream: bool,
    /// 读不出来的文件（路径 + 原因文本）
    pub unreadable: Vec<(String, String)>,
    /// 这批文件的目标加载器
    ///
    /// `None` 表示分属多个加载器、无法确定：此时依赖闭包只校验与加载器无关
    /// 的依赖，不拿单个加载器的清单去吓人（见 `ModDependency::applies`）。
    pub target_loader: Option<ModLoader>,
}

impl ModAudit {
    /// 高风险条目（`Blocking`）
    #[must_use]
    pub fn blocking(&self) -> Vec<&ModReport> {
        self.mods
            .iter()
            .filter(|r| r.risk == Risk::Blocking)
            .collect()
    }

    /// 需人工确认的条目（`Review`）
    #[must_use]
    pub fn review(&self) -> Vec<&ModReport> {
        self.mods
            .iter()
            .filter(|r| r.risk == Risk::Review)
            .collect()
    }

    /// 各风险等级计数
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut ok = 0;
        let mut review = 0;
        let mut blocking = 0;
        for m in &self.mods {
            match m.risk {
                Risk::Ok => ok += 1,
                Risk::Review => review += 1,
                Risk::Blocking => blocking += 1,
            }
        }
        (ok, review, blocking)
    }
}

/// 这批文件共同支持的唯一加载器
///
/// 取交集而不是并集：只有「每个 mod 都声明支持」的加载器才可能是这台服务器
/// 真正运行的加载器。没有任何加载器元数据的文件（资源包 / 认不出的 jar）
/// 不参与交集，否则一个坏文件就能把结论拖回「未知」。
///
/// 交集为空或不止一个都返回 `None`：宁可不做依赖校验，也不拿错清单误报。
fn common_loader(mods: &[ModReport]) -> Option<ModLoader> {
    let mut with_loaders = mods.iter().filter(|m| !m.capability.loaders.is_empty());
    let first = with_loaders.next()?;
    let mut common: Vec<ModLoader> = first.capability.loaders.clone();
    for report in with_loaders {
        common.retain(|loader| report.capability.loaders.contains(loader));
        if common.is_empty() {
            return None;
        }
    }
    match common.as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

/// mod 检测服务
///
/// 手动实现 `Debug`：`dyn` trait 对象不实现 `Debug`，派生会失败。只打
/// 能力名，不打内部字段（`Arc<dyn>` 的地址没有诊断价值）。
pub struct ModInspectService {
    inspector: Arc<dyn ModInspector>,
    upstream: Option<Arc<dyn ModMetadataSource>>,
}

impl std::fmt::Debug for ModInspectService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModInspectService")
            .field("upstream", &self.upstream.is_some())
            .finish_non_exhaustive()
    }
}

impl ModInspectService {
    /// 只用本地读取（完全离线）
    #[must_use]
    pub fn local(inspector: Arc<dyn ModInspector>) -> Self {
        Self {
            inspector,
            upstream: None,
        }
    }

    /// 本地读取 + 上游兜底
    #[must_use]
    pub fn with_upstream(
        inspector: Arc<dyn ModInspector>,
        upstream: Arc<dyn ModMetadataSource>,
    ) -> Self {
        Self {
            inspector,
            upstream: Some(upstream),
        }
    }

    /// 是否有上游能力
    #[must_use]
    pub fn has_upstream(&self) -> bool {
        self.upstream.is_some()
    }

    /// 检测一批 jar
    ///
    /// `allow_upstream = false` 时即使配置了上游也不联网。
    ///
    /// 读不出来的文件**不**中断整批检测：记进 [`ModAudit::unreadable`]
    /// 后继续。一个坏文件不该让用户看不到其余 99 个的结论。
    ///
    /// # Errors
    ///
    /// 只在目录本身无法遍历时返回错误。上游失败会被降级（记日志后继续），
    /// 因为本地结论独立有效。
    pub async fn inspect_paths(
        &self,
        paths: &[PathBuf],
        allow_upstream: bool,
    ) -> Result<ModAudit, ModInspectServiceError> {
        self.inspect_paths_for(paths, allow_upstream, None).await
    }

    /// 检测一批 jar，并显式指定目标服务端的安装条件
    ///
    /// `target` 由调用方给出（`--mc` / `--server`，或 `server mod list` 从
    /// 服务端元数据组装）：加载器同时用作依赖校验的口径，三个维度一起用来
    /// 判定「能不能装到这台服务端」。为 `None` 时不做匹配判定，依赖校验的
    /// 目标加载器从这批文件共同支持的加载器里推断。
    ///
    /// `context` 是参与**依赖闭包**但不进检测结果的文件（已装目录里的 jar）：
    /// 检查一个待装文件时，它声明的依赖如果已经装在 `mods/` 里，就不该报
    /// 「缺失」——加载器解析时看的是整个目录，不是这一个 jar。
    ///
    /// # Errors
    ///
    /// 与 [`Self::inspect_paths`] 相同
    pub async fn inspect_paths_for(
        &self,
        paths: &[PathBuf],
        allow_upstream: bool,
        target: Option<&ServerTarget>,
    ) -> Result<ModAudit, ModInspectServiceError> {
        self.inspect_with_context(paths, &[], allow_upstream, target)
            .await
    }

    /// [`Self::inspect_paths_for`] 的完整形态：额外指定依赖闭包的上下文
    ///
    /// # Errors
    ///
    /// 与 [`Self::inspect_paths`] 相同
    pub async fn inspect_with_context(
        &self,
        paths: &[PathBuf],
        context: &[PathBuf],
        allow_upstream: bool,
        target: Option<&ServerTarget>,
    ) -> Result<ModAudit, ModInspectServiceError> {
        let mut audit = ModAudit::default();

        for path in paths {
            match self.inspector.inspect(path) {
                Ok(cap) => audit.mods.push(ModReport {
                    capability: cap,
                    risk: Risk::Ok,
                    missing_deps: Vec::new(),
                    fit: None,
                }),
                Err(err) => {
                    warn!(path = %path.display(), error = %err, "mod inspection failed");
                    audit
                        .unreadable
                        .push((path.display().to_string(), err.to_string()));
                }
            }
        }

        // 上下文文件只贡献 modid，不进结果：读不出来就少一个可满足的依赖，
        // 与「文件不在」等价，不必单独报告
        let mut provided: Vec<String> = Vec::new();
        for path in context {
            match self.inspector.inspect(path) {
                Ok(cap) => provided.extend(cap.provided_ids()),
                Err(err) => {
                    debug!(path = %path.display(), error = %err, "context mod unreadable");
                }
            }
        }

        // 上游兜底：只对本地判不出服务端归属的条目查，联网不是免费的
        if allow_upstream && let Some(upstream) = &self.upstream {
            let pending: Vec<usize> = audit
                .mods
                .iter()
                .enumerate()
                .filter(|(_, r)| r.capability.side.server == domain::mod_::SideSupport::Unknown)
                .map(|(i, _)| i)
                .collect();
            if !pending.is_empty() {
                match self
                    .fill_from_upstream(upstream.as_ref(), paths, &pending, &mut audit)
                    .await
                {
                    Ok(()) => audit.queried_upstream = true,
                    // 上游挂了不影响本地结论：降级为「未知」继续
                    Err(err) => warn!(error = %err, "upstream mod metadata lookup failed"),
                }
            }
        }

        // 依赖闭包：先收集包内已有的 modid，再逐个比对
        // 依赖比对应基于「所有 mod 能提供的 modid」，含 provides 别名。
        // 只看 id 会把 balm → balm-fabric 这类别名误报成缺失依赖。
        // `context`（已装目录）同样参与闭包：检查待装文件时，它声明的依赖
        // 已经装在 `mods/` 里就不算缺失。
        let mut present = provided;
        present.extend(audit.mods.iter().flat_map(|r| r.capability.provided_ids()));
        // 目标加载器：显式指定优先，否则取这批文件共同支持的唯一加载器
        let loader = target
            .and_then(|t| t.loader)
            .or_else(|| common_loader(&audit.mods));
        for report in &mut audit.mods {
            report.missing_deps = report.capability.missing_mandatory_deps(&present, loader);
            let mut risk = report.capability.risk();
            if !report.missing_deps.is_empty() {
                // 缺必需依赖：服务端起不来，与「仅客户端」同等严重
                risk = risk.max(Risk::Blocking);
            }
            report.risk = risk;
            // 匹配判定与风险互不覆盖：一个答「是不是服务端的东西」，
            // 一个答「和这台服务端对不对得上」
            report.fit = target
                .filter(|t| !t.is_empty())
                .map(|t| report.capability.fit_for(t));
        }

        audit
            .mods
            .sort_by(|a, b| a.capability.path.cmp(&b.capability.path));
        audit.target_loader = loader;
        Ok(audit)
    }

    /// 向上游补齐服务端归属
    ///
    /// 需要 SHA1，而 `ModCapability` 不带哈希（领域模型不关心传输细节），
    /// 因此在这里现算。只对确实要查的条目算，不预先给全批算哈希。
    async fn fill_from_upstream(
        &self,
        upstream: &dyn ModMetadataSource,
        paths: &[PathBuf],
        pending: &[usize],
        audit: &mut ModAudit,
    ) -> Result<(), ModInspectServiceError> {
        let mut hashes = Vec::with_capacity(pending.len());
        let mut index_of_hash = Vec::with_capacity(pending.len());
        for &i in pending {
            // pending 的下标对应 audit.mods，与 paths 的顺序不一定一致
            // （读到一半可能有坏文件被剔除），因此按路径回查
            let path = &audit.mods[i].capability.path;
            let sha1 = match sha1_of(Path::new(path)) {
                Ok(h) => h,
                Err(err) => {
                    debug!(path, error = %err, "cannot hash mod file, skip upstream");
                    continue;
                }
            };
            hashes.push(sha1);
            index_of_hash.push(i);
        }
        // paths 与 pending 的对应关系仅用于文档；实际按审计结果里的路径算哈希
        let _ = paths;
        if hashes.is_empty() {
            return Ok(());
        }

        let found = upstream
            .server_support(&hashes)
            .await
            .map_err(|err| ModInspectServiceError::Upstream(err.to_string()))?;

        for side in found {
            let Some(pos) = hashes.iter().position(|h| h == &side.hash) else {
                continue;
            };
            let idx = index_of_hash[pos];
            let cap = &mut audit.mods[idx].capability;
            cap.side.client = side.client;
            cap.side.server = side.server;
            cap.side.confidence = domain::mod_::Confidence::Certain;
            cap.side.evidence.push(domain::mod_::Evidence::new(
                domain::mod_::EvidenceSource::Upstream,
                format!("上游 {upstream_name} 标注", upstream_name = upstream.name()),
            ));
            if let Some(slug) = side.slug {
                cap.side.evidence.push(domain::mod_::Evidence::new(
                    domain::mod_::EvidenceSource::Upstream,
                    format!("上游项目 {slug}"),
                ));
            }
        }
        Ok(())
    }
}

/// 算文件的 SHA1（十六进制小写）
///
/// `pub(crate)`：`mod` 编排（`server mod` 的更新检测）要用同一份实现按哈希反查
/// 上游，两处各写一遍必然出现「一边忽略大小写一边不忽略」这类分叉。
pub(crate) fn sha1_of(path: &Path) -> Result<String, std::io::Error> {
    use sha1::{Digest, Sha1};
    let bytes = crate::fs::read(path)?;
    let digest = Sha1::digest(&bytes);
    // sha1 0.11 的 GenericArray 不实现 LowerHex，手动转十六进制
    Ok(digest.iter().fold(String::with_capacity(40), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    }))
}

/// 从目录或文件路径收集 jar 列表
///
/// 目录只取**直接子项**里的 `.jar`，不递归：整合包的 `mods/` 就是一层的，
/// 递归会把作者的开发备份、`mods/disabled/` 之类一起算进来。
///
/// # Errors
///
/// 目录不可读时返回 [`FsError`]。
pub fn collect_jars(dir: &Path) -> Result<Vec<PathBuf>, FsError> {
    let mut out = Vec::new();
    let entries = crate::fs::read_dir(dir).map_err(|err| FsError::io(dir, err))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
        {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::mod_::{ModLoader, SideSupport, SideVerdict};

    /// 固定返回预设结果的读取器
    struct FakeInspector(Vec<ModCapability>);

    impl ModInspector for FakeInspector {
        fn inspect(&self, path: &Path) -> Result<ModCapability, ModInspectError> {
            self.0
                .iter()
                .find(|c| c.path == path.display().to_string())
                .cloned()
                .ok_or_else(|| ModInspectError::NotAnArchive {
                    path: path.to_path_buf(),
                })
        }
    }

    /// 固定返回空的上游
    struct EmptyUpstream;

    #[async_trait::async_trait]
    impl ModMetadataSource for EmptyUpstream {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn server_support(
            &self,
            _hashes: &[String],
        ) -> Result<Vec<port::mod_::UpstreamSide>, port::SourceError> {
            Ok(Vec::new())
        }
    }

    fn cap(path: &str, id: &str, server: SideSupport) -> ModCapability {
        let mut c = ModCapability::empty(path);
        c.id = Some(id.to_owned());
        c.loaders.push(ModLoader::Fabric);
        c.side = SideVerdict::declared(SideSupport::Required, server);
        c
    }

    #[tokio::test]
    async fn blocking_mods_are_reported() {
        let svc = ModInspectService::local(Arc::new(FakeInspector(vec![cap(
            "/a.jar",
            "a",
            SideSupport::Unsupported,
        )])));
        let audit = svc
            .inspect_paths(&[PathBuf::from("/a.jar")], false)
            .await
            .expect("audit");
        assert_eq!(audit.blocking().len(), 1);
        assert_eq!(audit.counts(), (0, 0, 1));
    }

    #[tokio::test]
    async fn unknown_becomes_review() {
        let svc = ModInspectService::local(Arc::new(FakeInspector(vec![cap(
            "/b.jar",
            "b",
            SideSupport::Unknown,
        )])));
        let audit = svc
            .inspect_paths(&[PathBuf::from("/b.jar")], false)
            .await
            .expect("audit");
        assert_eq!(audit.review().len(), 1);
        assert_eq!(audit.counts(), (0, 1, 0));
    }

    /// 没给目标服务端时不做匹配判定：`fit` 必须是 `None`，不能伪装成查过
    #[tokio::test]
    async fn no_target_means_no_fit() {
        let svc = ModInspectService::local(Arc::new(FakeInspector(vec![cap(
            "/a.jar",
            "a",
            SideSupport::Required,
        )])));
        let audit = svc
            .inspect_paths(&[PathBuf::from("/a.jar")], false)
            .await
            .expect("audit");
        assert!(audit.mods[0].fit.is_none());
    }

    /// 目标服务端的加载器与文件不符时给出 rejected，理由随之带出
    #[tokio::test]
    async fn mismatched_loader_is_rejected() {
        let mut a = cap("/a.jar", "a", SideSupport::Required);
        a.loaders = vec![ModLoader::Forge];
        let svc = ModInspectService::local(Arc::new(FakeInspector(vec![a])));
        let target = ServerTarget {
            loader: Some(ModLoader::Fabric),
            mc_version: Some("1.21.1".to_owned()),
            java_major: Some(21),
        };
        let audit = svc
            .inspect_paths_for(&[PathBuf::from("/a.jar")], false, Some(&target))
            .await
            .expect("audit");
        let fit = audit.mods[0].fit.as_ref().expect("fit");
        assert_eq!(fit.verdict, domain::mod_::FitVerdict::Rejected);
        assert_eq!(fit.loader, domain::mod_::Fit::Mismatch);
        assert!(!fit.issues.is_empty());
        // 匹配结论不覆盖风险：客/服归属照旧
        assert_eq!(audit.mods[0].risk, Risk::Ok);
    }

    /// 整批文件共同支持的唯一加载器被识别为目标加载器
    #[tokio::test]
    async fn target_loader_is_inferred_from_common_support() {
        let mut a = cap("/a.jar", "a", SideSupport::Required);
        a.loaders = vec![ModLoader::Fabric, ModLoader::Quilt];
        let mut b = cap("/b.jar", "b", SideSupport::Required);
        b.loaders = vec![ModLoader::Fabric];
        let svc = ModInspectService::local(Arc::new(FakeInspector(vec![a, b])));
        let audit = svc
            .inspect_paths(&[PathBuf::from("/a.jar"), PathBuf::from("/b.jar")], false)
            .await
            .expect("audit");
        assert_eq!(audit.target_loader, Some(ModLoader::Fabric));
    }

    /// 交集不唯一时目标加载器为未知，不能拿单个加载器的清单误报
    #[tokio::test]
    async fn ambiguous_batch_has_no_target_loader() {
        let mut a = cap("/a.jar", "a", SideSupport::Required);
        a.loaders = vec![ModLoader::Fabric];
        a.depends.push(
            domain::mod_::ModDependency::new("only_in_fabric", "", true)
                .with_loader(ModLoader::Fabric),
        );
        let mut b = cap("/b.jar", "b", SideSupport::Required);
        b.loaders = vec![ModLoader::Forge];
        let svc = ModInspectService::local(Arc::new(FakeInspector(vec![a, b])));
        let audit = svc
            .inspect_paths(&[PathBuf::from("/a.jar"), PathBuf::from("/b.jar")], false)
            .await
            .expect("audit");
        assert_eq!(audit.target_loader, None);
        // 目标未知：带加载器归属的依赖不参与，避免假缺失
        assert!(audit.mods[0].missing_deps.is_empty());
        assert_eq!(audit.mods[0].risk, Risk::Ok);
    }

    /// 显式指定加载器时以它为准，即使整批交集不同
    #[tokio::test]
    async fn explicit_target_wins_over_inference() {
        let mut a = cap("/a.jar", "a", SideSupport::Required);
        a.loaders = vec![ModLoader::Fabric, ModLoader::Quilt];
        a.depends.push(
            domain::mod_::ModDependency::new("quilt_only", "", true).with_loader(ModLoader::Quilt),
        );
        let svc = ModInspectService::local(Arc::new(FakeInspector(vec![a])));
        let target = ServerTarget {
            loader: Some(ModLoader::Quilt),
            ..ServerTarget::none()
        };
        let audit = svc
            .inspect_paths_for(&[PathBuf::from("/a.jar")], false, Some(&target))
            .await
            .expect("audit");
        assert_eq!(audit.target_loader, Some(ModLoader::Quilt));
        assert_eq!(audit.mods[0].missing_deps, vec!["quilt_only"]);
    }

    /// 目标加载器下的依赖缺失仍然升级为 blocking
    #[tokio::test]
    async fn missing_dep_under_target_escalates() {
        let mut a = cap("/a.jar", "a", SideSupport::Required);
        a.loaders = vec![ModLoader::Fabric];
        a.depends.push(
            domain::mod_::ModDependency::new("satin", "", true).with_loader(ModLoader::Fabric),
        );
        let svc = ModInspectService::local(Arc::new(FakeInspector(vec![a])));
        let audit = svc
            .inspect_paths(&[PathBuf::from("/a.jar")], false)
            .await
            .expect("audit");
        assert_eq!(audit.target_loader, Some(ModLoader::Fabric));
        assert_eq!(audit.mods[0].missing_deps, vec!["satin"]);
        assert_eq!(audit.mods[0].risk, Risk::Blocking);
    }

    #[tokio::test]
    async fn missing_mandatory_dep_escalates_to_blocking() {
        let mut a = cap("/a.jar", "a", SideSupport::Required);
        a.depends
            .push(domain::mod_::ModDependency::new("absent", "", true));
        let svc = ModInspectService::local(Arc::new(FakeInspector(vec![a])));
        let audit = svc
            .inspect_paths(&[PathBuf::from("/a.jar")], false)
            .await
            .expect("audit");
        assert_eq!(audit.mods[0].missing_deps, vec!["absent"]);
        assert_eq!(audit.mods[0].risk, Risk::Blocking);
    }

    #[tokio::test]
    async fn satisfied_dep_does_not_escalate() {
        let mut a = cap("/a.jar", "a", SideSupport::Required);
        a.depends
            .push(domain::mod_::ModDependency::new("b", "", true));
        let b = cap("/b.jar", "b", SideSupport::Required);
        let svc = ModInspectService::local(Arc::new(FakeInspector(vec![a, b])));
        let audit = svc
            .inspect_paths(&[PathBuf::from("/a.jar"), PathBuf::from("/b.jar")], false)
            .await
            .expect("audit");
        let a_report = audit
            .mods
            .iter()
            .find(|m| m.capability.id.as_deref() == Some("a"))
            .expect("a");
        assert!(a_report.missing_deps.is_empty());
        assert_eq!(a_report.risk, Risk::Ok);
    }

    /// 上下文里的文件满足依赖：已装的东西不该被判成缺失
    ///
    /// 场景：`mods/` 里已有 `satin`，装一个依赖 `satin` 的 mod。闭包只看
    /// 检测对象本身的话会误报缺失，用户只能 `--force`。
    #[tokio::test]
    async fn context_satisfies_dependencies() {
        let mut a = cap("/staging/a.jar", "a", SideSupport::Required);
        a.depends
            .push(domain::mod_::ModDependency::new("satin", "", true));
        let satin = cap("/mods/satin.jar", "satin", SideSupport::Required);
        let svc = ModInspectService::local(Arc::new(FakeInspector(vec![a, satin.clone()])));
        let audit = svc
            .inspect_with_context(
                &[PathBuf::from("/staging/a.jar")],
                &[PathBuf::from("/mods/satin.jar")],
                false,
                None,
            )
            .await
            .expect("audit");
        // 上下文只贡献 modid，不进结果
        assert_eq!(audit.mods.len(), 1);
        assert!(audit.mods[0].missing_deps.is_empty(), "{:?}", audit.mods[0]);
        assert_eq!(audit.mods[0].risk, Risk::Ok);
    }

    /// 上下文读不出来等价于「文件不在」：不报错，也不满足依赖
    #[tokio::test]
    async fn unreadable_context_does_not_satisfy_dependencies() {
        let mut a = cap("/staging/a.jar", "a", SideSupport::Required);
        a.depends
            .push(domain::mod_::ModDependency::new("satin", "", true));
        // FakeInspector 里没有 /mods/gone.jar，inspect 会失败
        let svc = ModInspectService::local(Arc::new(FakeInspector(vec![a])));
        let audit = svc
            .inspect_with_context(
                &[PathBuf::from("/staging/a.jar")],
                &[PathBuf::from("/mods/gone.jar")],
                false,
                None,
            )
            .await
            .expect("audit");
        assert_eq!(audit.mods.len(), 1);
        assert_eq!(audit.mods[0].missing_deps, vec!["satin"]);
        assert_eq!(audit.mods[0].risk, Risk::Blocking);
        assert!(audit.unreadable.is_empty(), "上下文的坏文件不进结果");
    }

    #[tokio::test]
    async fn unreadable_file_does_not_abort_batch() {
        let svc = ModInspectService::local(Arc::new(FakeInspector(vec![cap(
            "/ok.jar",
            "ok",
            SideSupport::Required,
        )])));
        let audit = svc
            .inspect_paths(
                &[PathBuf::from("/ok.jar"), PathBuf::from("/bad.jar")],
                false,
            )
            .await
            .expect("audit");
        assert_eq!(audit.mods.len(), 1);
        assert_eq!(audit.unreadable.len(), 1);
        assert_eq!(audit.unreadable[0].0, "/bad.jar");
    }

    #[tokio::test]
    async fn upstream_not_queried_when_disallowed() {
        let svc = ModInspectService::with_upstream(
            Arc::new(FakeInspector(vec![cap(
                "/a.jar",
                "a",
                SideSupport::Unknown,
            )])),
            Arc::new(EmptyUpstream),
        );
        let audit = svc
            .inspect_paths(&[PathBuf::from("/a.jar")], false)
            .await
            .expect("audit");
        assert!(!audit.queried_upstream);
    }
}
