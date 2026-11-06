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
use domain::mod_::{ModCapability, ModLoader, Risk};
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

    /// 检测一批 jar，并显式指定目标加载器
    ///
    /// `target` 由调用方给出（`--loader`）时以它为准；为 `None` 则从这批文件
    /// 共同支持的加载器里推断。看不出唯一结论就保持 `None`，绝不猜。
    ///
    /// # Errors
    ///
    /// 与 [`Self::inspect_paths`] 相同
    pub async fn inspect_paths_for(
        &self,
        paths: &[PathBuf],
        allow_upstream: bool,
        target: Option<ModLoader>,
    ) -> Result<ModAudit, ModInspectServiceError> {
        let mut audit = ModAudit::default();

        for path in paths {
            match self.inspector.inspect(path) {
                Ok(cap) => audit.mods.push(ModReport {
                    capability: cap,
                    risk: Risk::Ok,
                    missing_deps: Vec::new(),
                }),
                Err(err) => {
                    warn!(path = %path.display(), error = %err, "mod inspection failed");
                    audit
                        .unreadable
                        .push((path.display().to_string(), err.to_string()));
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
        let present: Vec<String> = audit
            .mods
            .iter()
            .flat_map(|r| r.capability.provided_ids())
            .collect();
        // 目标加载器：显式指定优先，否则取这批文件共同支持的唯一加载器
        let target = target.or_else(|| common_loader(&audit.mods));
        for report in &mut audit.mods {
            report.missing_deps = report.capability.missing_mandatory_deps(&present, target);
            let mut risk = report.capability.risk();
            if !report.missing_deps.is_empty() {
                // 缺必需依赖：服务端起不来，与「仅客户端」同等严重
                risk = risk.max(Risk::Blocking);
            }
            report.risk = risk;
        }

        audit
            .mods
            .sort_by(|a, b| a.capability.path.cmp(&b.capability.path));
        audit.target_loader = target;
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
/// 手写实现以免为一次哈希引入 `sha1` 之外的依赖；`file` crate 已有同款
/// 依赖，但 modkit 不该依赖它。
fn sha1_of(path: &Path) -> Result<String, std::io::Error> {
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
        let audit = svc
            .inspect_paths_for(&[PathBuf::from("/a.jar")], false, Some(ModLoader::Quilt))
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
