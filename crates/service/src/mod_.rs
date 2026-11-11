//! 服务端 mod 编排：列表 / 安装 / 卸载 / 检查更新 / 更新
//!
//! 与 [`crate::ContentService`] 的分工：那个只回答「把一个项目装到某个目录」，
//! 不知道「服务端」是什么；本模块回答「这台服务端的 `mods/` 里有什么、能装
//! 什么、该不该装」，因此需要注册表（目录与加载器）、运行态（[`Backend`]）与
//! 检查器（[`port::mod_::ModInspector`]）。
//!
//! # 状态从磁盘读，不建表
//!
//! `mods/` 目录本身就是权威事实。新增「已装 mod」表会引入「表说装了、磁盘上
//! 没有」这类最难查的漂移，用户手工丢进 `mods/` 的文件也会漏掉。代价是每次都
//! 要读 zip，联网识别还要现算 SHA1——因此识别与查更新都是**显式可选**的，
//! 默认只做本地检查。
//!
//! # 落盘顺序：暂存 → 检查 → 原子替换
//!
//! 下载先落到 `<server>/.mcsup-staging/<n>/mods/`，检查通过才换入正式位置。
//! 半截文件与「装进去必然让服务端起不来」的文件都不会留在 `mods/` 里；暂存
//! 目录与正式位置同卷，换入是 rename 而不是复制。
//!
//! # 运行中的服务端一律拒绝改动
//!
//! 写 `mods/` 时加载器正在读那些文件，结果是随机崩溃或半加载。判定与服务端
//! 启动同源：问 [`Backend::supervisor`] 的实时状态，不做「目录里有没有锁文件」
//! 之类的猜测。

use crate::content::{ContentError, ContentService, InstallOptions, request_for};
use crate::fs_error::FsError;
use crate::mod_inspect::{
    ModAudit, ModInspectService, ModInspectServiceError, ModReport, collect_jars, sha1_of,
};
use crate::modpack_manifest::ModpackManifestSnapshot;
use crate::registry::{RegisteredServer, ServerRegistry, ServerRegistryError};
use domain::mod_::{
    CrashAttribution, FitVerdict, InstallVerdict, ModLoader, Risk, ServerTarget, SideSupport,
    attribute_crash,
};
use domain::server::content::{
    ContentCandidate, ContentIdentity, ContentKind, ContentRequest, UpdateDecision, decide_update,
};
use domain::server::layout::console_log_path;
use domain::server::version::ServerVariant;
use port::content::{ContentProject, ContentQuery, ContentSearch, safe_filename};
use port::mod_::ModInspector;
use port::{Backend, CancellationToken, ErrorCategory, ProgressSink};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tracing::{info, warn};

/// 下载暂存目录名（相对服务端目录）
///
/// 与 `mods/` 同级而不是放在里面：放进 `mods/` 会被加载器当成待加载的 mod，
/// 也会被 [`collect_jars`] 扫进来。以 `.` 开头的名字同时避开备份作用域里的
/// 常见过滤口径。
pub const STAGING_DIR: &str = ".mcsup-staging";

pub use crate::trash::TRASH_DIR;

/// 一次修复最多向下展开多少层依赖
///
/// 依赖链正常只有一两层；设上限是为了让「互相依赖」的坏数据停在一个可
/// 报告的位置，而不是无限递归。
const MAX_DEPENDENCY_DEPTH: usize = 16;

/// mod 管理失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ModError {
    /// 注册表访问失败
    #[error(transparent)]
    Registry(#[from] ServerRegistryError),
    /// 内容来源失败
    #[error(transparent)]
    Content(#[from] ContentError),
    /// 上游来源失败
    #[error(transparent)]
    Source(#[from] port::SourceError),
    /// 本地检查失败
    #[error(transparent)]
    Inspect(#[from] ModInspectServiceError),
    /// 文件系统失败
    #[error(transparent)]
    Fs(#[from] FsError),
    /// 该服务器的文件不在本机
    #[error("server {name} is remote; manage its mods on the host that owns it")]
    Remote {
        /// 服务器名
        name: String,
    },
    /// 服务端正在运行
    #[error("server {name} is running; stop it before changing mods")]
    Running {
        /// 服务器名
        name: String,
    },
    /// 运行态无法确认（不猜「应该没在跑」）
    #[error("cannot determine whether server {name} is running: {reason}")]
    UnknownState {
        /// 服务器名
        name: String,
        /// 底层原因
        reason: String,
    },
    /// 该类内容是纯客户端的，装到服务端目录没有意义
    #[error("{kind} is client-only and cannot be installed on a server")]
    ClientOnly {
        /// 内容类别
        kind: ContentKind,
    },
    /// 按当前服务端条件没有可安装的版本
    #[error("no installable version for {project}")]
    NoCandidate {
        /// 项目标识
        project: String,
    },
    /// 选择器没有命中任何文件
    #[error("{selector} does not match any mod in {dir}")]
    NotFound {
        /// 用户给的选择器
        selector: String,
        /// 查找的目录
        dir: PathBuf,
    },
    /// 选择器命中多个文件
    #[error("{selector} matches several mods: {candidates:?}")]
    Ambiguous {
        /// 用户给的选择器
        selector: String,
        /// 命中的文件名
        candidates: Vec<String>,
    },
    /// 检查判定为不可安装
    #[error(
        "{file} was refused by the server-side check (risk={risk}, missing={missing_deps:?}): {evidence:?}"
    )]
    Refused {
        /// 文件名
        file: String,
        /// 风险等级
        risk: Risk,
        /// 缺失的必需依赖（`risk` 为 `Blocking` 时的主要成因）
        missing_deps: Vec<String>,
        /// 判定依据
        evidence: Vec<String>,
    },
    /// 下载物读不出来（不是有效的 mod 归档）
    #[error("{file} cannot be inspected: {reason}")]
    Unreadable {
        /// 文件名
        file: String,
        /// 底层原因
        reason: String,
    },
    /// 上游命中的项目不提供需要的 modid（同名/同义词检索的误命中）
    #[error("{file} does not provide the modid {need}")]
    WrongProvider {
        /// 要补的 modid
        need: String,
        /// 命中的文件名
        file: String,
        /// 该 jar 实际提供的 modid
        provides: Vec<String>,
    },
    /// 落点越出服务端目录（防御性检查，正常路径不会触发）
    #[error("refusing to write outside the server directory: {path}")]
    Escapes {
        /// 越界的路径
        path: PathBuf,
    },
    /// 更新中途失败：前面的项**已经落盘**，盘上处于「部分更新」状态
    #[error("update of {server} stopped at {failed} after {applied} installed item(s): {source}")]
    UpdateAborted {
        /// 服务端名
        server: String,
        /// 失败的文件
        failed: String,
        /// 失败之前已经完成的项数
        applied: usize,
        /// 失败原因
        source: Box<ModError>,
    },
}

impl ModError {
    /// 失败等级（决定 CLI 退出码）
    ///
    /// 包装变体一律**委托**：底层已知道自己属于哪一类，本层再判一次就会出现
    /// 两套口径。自有变体按语义选：状态冲突归 `Conflict`，选择器问题归
    /// `Usage`，找不到归 `NotFound`。
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Registry(err) => err.category(),
            Self::Content(err) => err.category(),
            Self::Source(err) => err.category(),
            Self::UpdateAborted { source, .. } => source.category(),
            Self::Fs(_) | Self::Inspect(_) => ErrorCategory::Backend,
            Self::Remote { .. } | Self::ClientOnly { .. } | Self::Ambiguous { .. } => {
                ErrorCategory::Usage
            }
            Self::Running { .. } | Self::Refused { .. } => ErrorCategory::Conflict,
            Self::UnknownState { .. } | Self::Unreadable { .. } | Self::Escapes { .. } => {
                ErrorCategory::Backend
            }
            Self::NoCandidate { .. } | Self::NotFound { .. } | Self::WrongProvider { .. } => {
                ErrorCategory::NotFound
            }
        }
    }
}

/// 列表选项
#[derive(Debug, Clone, Copy)]
pub struct ListOptions<'a> {
    /// 内容类别：决定扫 `mods/` 还是 `plugins/`（缺省 `mod`）
    pub kind: ContentKind,
    /// 联网识别已装文件的来源与版本（要算 SHA1 + 访问上游）
    pub identify: bool,
    /// 识别用的来源名；`None` 用默认来源
    pub source: Option<&'a str>,
    /// 顺带判断有没有更新（每个已识别条目一次上游请求）
    pub updates: bool,
}

impl Default for ListOptions<'_> {
    fn default() -> Self {
        Self {
            kind: ContentKind::Mod,
            identify: false,
            source: None,
            updates: false,
        }
    }
}

impl ListOptions<'_> {
    /// 只扫某个类别的目录，其余按缺省（不联网）
    #[must_use]
    pub const fn of(kind: ContentKind) -> Self {
        Self {
            kind,
            identify: false,
            source: None,
            updates: false,
        }
    }
}

/// 一条已装 mod 的记录
#[derive(Debug, Clone)]
pub struct ModEntry {
    /// 本地检查结论（能力、风险、缺失依赖）
    pub report: ModReport,
    /// 文件名（`mods/` 下的直接子项）
    pub file: String,
    /// 上游身份；未联网识别时为 `None`
    pub identity: Option<ContentIdentity>,
    /// 更新结论；未查更新时为 `None`
    pub update: Option<UpdateDecision>,
}

/// `server mod list` 的结果
#[derive(Debug, Clone)]
pub struct ModList {
    /// 服务器名
    pub server: String,
    /// 被扫描的目录
    pub dir: PathBuf,
    /// 已装 mod（按文件名排序）
    pub mods: Vec<ModEntry>,
    /// 整批文件共同支持的加载器；判不出唯一结论时为 `None`
    pub target_loader: Option<ModLoader>,
    /// 读不出来的文件与原因：一个坏文件不该让其余结论消失
    pub unreadable: Vec<(String, String)>,
}

/// 安装条件
#[derive(Debug, Clone, Copy, Default)]
pub struct InstallSpec<'a> {
    /// 精确版本 ID；`None` 按服务端条件挑最优候选
    pub version: Option<&'a str>,
    /// 来源名；`None` 用默认来源
    pub source: Option<&'a str>,
    /// 允许测试版 / 内测版
    pub allow_prerelease: bool,
}

/// 一次安装的计划（预览，不写盘）
#[derive(Debug, Clone)]
pub struct InstallPlan {
    /// 来源标识
    pub source: &'static str,
    /// 项目 ID
    pub project_id: String,
    /// 项目标题（展示用）
    pub project_title: Option<String>,
    /// 版本 ID
    pub version_id: String,
    /// 版本号（展示用）
    pub version_number: String,
    /// 内容类别（决定落点是 `mods/` 还是 `plugins/`）
    pub kind: ContentKind,
    /// 落点文件名
    pub file: String,
    /// 字节数
    pub size: u64,
    /// 落点绝对路径
    pub target: PathBuf,
    /// 已存在的同名文件（会被替换）
    pub replaces: Option<PathBuf>,
    /// 下载条件；仅供 [`ModService::install`] 复用
    request: ContentRequest,
    /// 这次安装必须由该 jar 提供的 modid（补装依赖时用）
    ///
    /// 上游检索按名字匹配，名字相关的项目未必提供这个 modid：实测
    /// `melody` 与 `music-and-melody` 是两个项目，后者不提供 `melody`。
    /// 没有这道校验就会把不相干的 jar 装进 `mods/`。
    pub expect_modid: Option<String>,
}

impl InstallPlan {
    /// 下载条件（入口只读展示，不参与构造）
    #[must_use]
    pub fn request(&self) -> &ContentRequest {
        &self.request
    }
}

/// 安装结果
#[derive(Debug, Clone)]
pub struct ModInstalled {
    /// 落点文件名
    pub file: String,
    /// 落点绝对路径
    pub path: PathBuf,
    /// 字节数
    pub size: u64,
    /// 被替换掉的旧文件
    pub replaced: Option<PathBuf>,
    /// 检查得出的风险等级
    pub risk: Risk,
    /// 缺失的必需依赖
    pub missing_deps: Vec<String>,
    /// 判定依据（`--force` 装下去时用户要看到这些）
    pub evidence: Vec<String>,
}

/// 卸载结果
#[derive(Debug, Clone)]
pub struct ModRemoved {
    /// 被删除（或将要删除）的文件
    pub files: Vec<PathBuf>,
    /// 只预览没动盘
    pub dry_run: bool,
}

/// 一次停用的结果
#[derive(Debug, Clone)]
pub struct ModDisabled {
    /// 已移进停用目录的条目：原相对路径 → 新相对路径
    pub moved: Vec<(String, String)>,
}

/// 修复计划的一项：补装缺失依赖
#[derive(Debug, Clone)]
pub struct FixInstall {
    /// 依赖它的文件（展示用）
    pub needed_by: String,
    /// 缺失的依赖 modid
    pub missing: String,
    /// 上游项目 ID（按 modid 检索命中）
    pub project_id: String,
    /// 项目标题
    pub title: String,
    /// 上游项目自身是否标注仅客户端
    pub client_only: bool,
    /// 精确版本 ID
    ///
    /// 整合包清单里记着这个文件的确切版本（从下载地址解析），补装时照它
    /// 装即可，不必再按服务端条件挑版本——挑版本会漂移到更新的版本，与
    /// 整合包作者验证过的组合不再一致。来源检索命中时没有这个信息，
    /// 为 `None`，退回按条件挑最新合适版本。
    pub version_id: Option<String>,
}

/// 修复计划的一项：移除带问题的 mod 文件
#[derive(Debug, Clone)]
pub struct FixRemove {
    /// 要移除的文件
    pub file: PathBuf,
    /// 移除原因（展示用）
    pub reason: String,
}

/// 一次修复计划（预览，不写盘）
///
/// 由 [`ModService::plan_fix`] 或 [`ModService::plan_fix_from_log`] 产出：
/// 缺失依赖补装、带问题的文件移除、上游找不到提供方的依赖如实列出。
#[derive(Debug, Clone)]
pub struct ModFixPlan {
    /// 服务器名
    pub server: String,
    /// 计划针对的内容类别
    ///
    /// 移除校验与补装落点都由它决定（`plugins/` 与 `mods/` 不是一回事）。
    /// 计划必须在执行时带着产出它的类别，不能假定为 `Mod`。
    pub kind: ContentKind,
    /// 补装项
    pub installs: Vec<FixInstall>,
    /// 移除项
    pub removals: Vec<FixRemove>,
    /// 上游找不到提供方的依赖（不猜，交给用户）
    pub unresolved: Vec<String>,
    /// 启动日志的归因结果；只有 `plan_fix_from_log` 会填
    ///
    /// 保留完整结果（含缺失类与原始栈帧）而不只留结论：用户要能自己核对
    /// 「工具凭什么认定是这个 mod」。
    pub crash: Option<CrashAttribution>,
    /// 归因到、但本机 `mods/` 里找不到对应文件的类
    ///
    /// 常见于类来自内嵌 jar、或日志来自另一份安装。如实列出，不猜文件。
    pub unattributed: Vec<String>,
}

/// 修复执行结果
#[derive(Debug, Clone)]
pub struct ModFixApplied {
    /// 实际移除的文件
    pub removed: Vec<PathBuf>,
    /// 实际补装的 mod
    pub installed: Vec<ModInstalled>,
    /// 没能装上的项目，带原因
    pub refused: Vec<FixRefused>,
    /// 上游找不到提供方的依赖
    pub unresolved: Vec<String>,
}

/// 一项没能装上的修复动作
///
/// 「被安装检查拒绝」本身不是给用户看的结论：缺依赖、上游仅客户端、
/// 没有匹配版本的处置各不相同。把原因一路带出来，入口只负责展示。
#[derive(Debug, Clone)]
pub struct FixRefused {
    /// 项目 ID
    pub project_id: String,
    /// 拒绝原因（展示用，已按成因措辞）
    pub reason: String,
}

/// 一个可补装的候选（内部用，避免与对外 DTO 混淆）
#[derive(Debug, Clone)]
struct FixCandidate {
    /// 上游项目 ID
    project_id: String,
    /// 展示名（整合包清单里取文件名，检索命中取项目标题）
    title: String,
    /// 上游项目自身是否标注仅客户端
    client_only: bool,
    /// 精确版本 ID（清单给出时）
    version_id: Option<String>,
}

/// 一个缺失 modid 的解析结果
#[derive(Debug, Clone)]
enum FixTarget {
    /// 有可补装的候选（可能多个，安装时逐个校验 modid）
    Installs(Vec<FixCandidate>),
    /// 上游确实没有可补装的东西
    Unresolved,
}

/// 待补装的一项（内部工作队列元素）
#[derive(Debug, Clone)]
struct PendingFix {
    /// 项目 ID
    project_id: String,
    /// 这次安装要补的 modid（安装时校验 jar 是否真的提供它）
    missing: Option<String>,
    /// 精确版本 ID（整合包清单给出时按它装，避免漂移到别的版本）
    version_id: Option<String>,
    /// 是否已经为它展开过一轮依赖（只重试一次）
    retried: bool,
}

/// 补装过程中的可变状态
///
/// 这几样东西在队列推进、依赖展开、逐项安装里都要原地改，散成多个参数
/// 会让 `expand_dependency_chain` 一路膨胀到 clippy 的阈值之上。聚成一个
/// 结构后，新增状态不必再改每个签名。
#[derive(Debug, Default)]
struct FixState {
    /// 已尝试过的项目，避免互相依赖时死循环
    visited: HashSet<String>,
    /// 真的没补上的 modid（展示给用户）
    unresolved: Vec<String>,
    /// 被拒的项目与原因（展示给用户）
    refused: Vec<FixRefused>,
    /// 每个待补 modid 还剩几个候选没试
    pending_candidates: HashMap<String, usize>,
}

impl FixState {
    /// 登记一个待补 modid 的一个候选
    fn plan_candidate(&mut self, missing: &str) {
        *self
            .pending_candidates
            .entry(missing.to_owned())
            .or_insert(0) += 1;
    }

    /// 记一个候选不提供该 modid，试完最后一个才进 `unresolved`
    ///
    /// 中途就记会把「第一个名字最像的不是本体」说成「上游没有提供方」——
    /// 实测 `satin` 就是这么被误报的：首位是 `satin-free-wakes`（modid
    /// 是 `wakes`），真正的 `satin-api` 排第二。
    fn candidate_rejected(&mut self, missing: &str) {
        if let Some(left) = self.pending_candidates.get_mut(missing) {
            *left = left.saturating_sub(1);
            if *left > 0 {
                return;
            }
        }
        self.no_provider(missing);
    }

    /// 记为「没有可补装的提供方」
    fn no_provider(&mut self, missing: &str) {
        if !self.unresolved.iter().any(|item| item == missing) {
            self.unresolved.push(missing.to_owned());
        }
    }
}

/// 更新计划的一项
#[derive(Debug, Clone)]
pub struct UpdateItem {
    /// 当前文件名
    pub file: String,
    /// 当前文件的绝对路径
    ///
    /// 新版本换了文件名时必须删掉它：留着会变成同一 mod 的两份，
    /// 加载器启动阶段直接报重复 modid。
    pub current: PathBuf,
    /// 项目 ID
    pub project_id: String,
    /// 当前版本号
    pub from_version: String,
    /// 目标版本号
    pub to_version: String,
    /// 应用到该文件上的安装计划
    pub plan: InstallPlan,
}

/// 一次更新的计划（预览，不写盘）
#[derive(Debug, Clone)]
pub struct UpdatePlan {
    /// 需要更新的文件
    pub items: Vec<UpdateItem>,
    /// 无法判断是否有更新的文件与原因（例如上游不认识该哈希）
    pub skipped: Vec<(String, String)>,
}

/// 单个文件更新后的结果
#[derive(Debug, Clone)]
pub struct UpdateChange {
    /// 文件名
    pub file: String,
    /// 原版本号
    pub from_version: String,
    /// 新版本号
    pub to_version: String,
    /// 安装结果
    pub installed: ModInstalled,
    /// 被删除的旧文件（新版本换了文件名时才有）
    pub removed: Option<PathBuf>,
}

/// 更新执行结果
#[derive(Debug, Clone)]
pub struct UpdateApplied {
    /// 已应用的更新
    pub changes: Vec<UpdateChange>,
}

/// 落盘参数
pub struct ApplyOptions<'a> {
    /// 允许安装检查判定为不可用于服务端的文件（`--force`）
    pub allow_blocking: bool,
    /// 下载进度观察者
    pub progress: Option<&'a dyn ProgressSink>,
    /// 取消令牌
    pub cancel: &'a CancellationToken,
}

impl std::fmt::Debug for ApplyOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplyOptions")
            .field("allow_blocking", &self.allow_blocking)
            .finish_non_exhaustive()
    }
}

/// mod 管理编排
pub struct ModService {
    registry: Arc<ServerRegistry>,
    backend: Arc<dyn Backend>,
    content: ContentService,
    inspector: Arc<dyn ModInspector>,
}

impl std::fmt::Debug for ModService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModService")
            .field("sources", &self.content.source_names())
            .finish_non_exhaustive()
    }
}

impl ModService {
    /// 组合注册表、执行位置、内容编排与检查器
    #[must_use]
    pub fn new(
        registry: Arc<ServerRegistry>,
        backend: Arc<dyn Backend>,
        content: ContentService,
        inspector: Arc<dyn ModInspector>,
    ) -> Self {
        Self {
            registry,
            backend,
            content,
            inspector,
        }
    }

    /// 列出服务端的 mod
    ///
    /// 只读：不改动任何文件，服务端运行中也能查。
    ///
    /// # Errors
    ///
    /// 服务器不存在、位于远端、目录不可读，或（开了识别时）上游异常时返回
    /// [`ModError`]
    pub async fn list(&self, name: &str, options: ListOptions<'_>) -> Result<ModList, ModError> {
        let server = self.local_server(name)?;
        let dir = mods_dir(&server, options.kind);
        let jars = jars_in(&dir)?;
        let audit = self.inspect(&jars, &[], &server).await?;

        // 哈希只算一次：识别用它，之后按路径回查，避免每个文件算两遍。
        // `updates` 依赖识别结果，因此隐含打开识别——调用方漏传不该让结论变空。
        let resolve_upstream = options.identify || options.updates;
        let mut by_path: HashMap<PathBuf, ContentIdentity> = HashMap::new();
        if resolve_upstream && !jars.is_empty() {
            let mut hashes = Vec::with_capacity(jars.len());
            for path in &jars {
                match sha1_of(path) {
                    Ok(hash) => hashes.push((path.clone(), hash)),
                    // 读不出内容时识别不了，但本地结论仍然有效：降级为未识别
                    Err(err) => warn!(path = %path.display(), error = %err, "cannot hash mod file"),
                }
            }
            if !hashes.is_empty() {
                let query: Vec<String> = hashes.iter().map(|(_, hash)| hash.clone()).collect();
                let found: HashMap<String, ContentIdentity> = self
                    .content
                    .identify(options.source, &query)
                    .await?
                    .into_iter()
                    .map(|identity| (identity.hash.clone(), identity))
                    .collect();
                for (path, hash) in hashes {
                    if let Some(identity) = found.get(&hash) {
                        by_path.insert(path, identity.clone());
                    }
                }
            }
        }

        let mut mods = Vec::with_capacity(audit.mods.len());
        for report in &audit.mods {
            let path = PathBuf::from(&report.capability.path);
            let identity = by_path.get(&path).cloned();
            let update = if options.updates {
                match &identity {
                    Some(identity) => {
                        self.update_decision(options.source, &server, identity)
                            .await?
                    }
                    None => None,
                }
            } else {
                None
            };
            mods.push(ModEntry {
                report: report.clone(),
                file: path
                    .file_name()
                    .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
                identity,
                update,
            });
        }

        Ok(ModList {
            server: server.meta.name.as_str().to_owned(),
            dir,
            mods,
            target_loader: audit.target_loader,
            unreadable: audit.unreadable,
        })
    }

    /// 按服务端条件搜索可安装的内容
    ///
    /// 交互选择（`server mod add` 不带项目时）用它列出「装了能跑」的项目：
    /// 类别限定为用户要装的类别，加载器与游戏版本取这台服务端的实际条件。
    /// 过滤条件在这里拼而不是在入口拼——入口只该知道「哪台服务端、要什么
    /// 关键词、第几页」，不该知道服务端变体对应哪个加载器。
    ///
    /// # Errors
    ///
    /// 服务器不存在、位于远端、类别不可用于服务端或上游异常时返回 [`ModError`]
    pub async fn search(
        &self,
        name: &str,
        kind: ContentKind,
        query: &ContentQuery,
        source: Option<&str>,
    ) -> Result<ContentSearch, ModError> {
        let server = self.local_server(name)?;
        if !kind.is_server_usable() {
            return Err(ModError::ClientOnly { kind });
        }
        let search = self
            .content
            .search(
                source,
                &server_query(
                    &server.meta.variant,
                    server.meta.version.as_str(),
                    kind,
                    query,
                ),
            )
            .await
            .map_err(ModError::from)?;
        // 过滤后为空不等于「没有这个东西」：上游标注本身有误标（API 库的
        // `environment: "client"` 常见，实测 satin 是服务端 mod 的必需依赖，
        // 服务端装载正常）。放宽过滤再查一次，把仅客户端的条目带回来并
        // 标记——选择权交还用户，而不是把「搜不到」留给用户猜。
        if search.total == 0 {
            let mut relaxed = server_query(
                &server.meta.variant,
                server.meta.version.as_str(),
                kind,
                query,
            );
            relaxed.server_usable_only = false;
            let relaxed = self
                .content
                .search(source, &relaxed)
                .await
                .map_err(ModError::from)?;
            // 只带回落选结果里被过滤掉的：上游同时命中两者的场景不存在
            // （原查询为空），这里是防御性写法，口径保持「过滤为空才放宽」
            let client_only: Vec<_> = relaxed
                .hits
                .into_iter()
                .filter(|hit| hit.client_only)
                .collect();
            if !client_only.is_empty() {
                return Ok(ContentSearch {
                    total: client_only.len() as u64,
                    offset: 0,
                    hits: client_only,
                });
            }
        }
        Ok(search)
    }

    /// 列出某个项目在这台服务端上可安装的候选版本（已按适配度排序）
    ///
    /// 供交互选择版本：入口把列表展示给用户，选中的版本 ID 再交给
    /// [`Self::plan_install`] 的 `spec.version`，因此挑版本的口径与自动挑
    /// 完全一致，不会出现「交互里能选、非交互装不上」的分叉。
    ///
    /// # Errors
    ///
    /// 服务器不存在、位于远端、类别不可用于服务端、来源异常或没有可安装
    /// 版本时返回 [`ModError`]
    pub async fn candidates(
        &self,
        name: &str,
        project: &str,
        kind: ContentKind,
        spec: InstallSpec<'_>,
    ) -> Result<Vec<ContentCandidate>, ModError> {
        let server = self.local_server(name)?;
        if !kind.is_server_usable() {
            return Err(ModError::ClientOnly { kind });
        }
        let request = server_request(
            &server.meta.variant,
            server.meta.version.as_str(),
            kind,
            spec.allow_prerelease,
        );
        self.content
            .candidates(spec.source, project, &request)
            .await
            .map_err(ModError::from)
    }

    /// 规划一次安装：解析版本与落点，**不写盘**
    ///
    /// 入口把结果展示给用户确认，确认后才调用 [`Self::install`]。中间可能过了
    /// 一段时间，因此 `install` 会重新确认服务端状态。
    ///
    /// # Errors
    ///
    /// 服务器不存在、位于远端、正在运行、类别不可用于服务端、来源异常或没有
    /// 可安装版本时返回 [`ModError`]
    pub async fn plan_install(
        &self,
        name: &str,
        project: &str,
        kind: ContentKind,
        spec: InstallSpec<'_>,
    ) -> Result<InstallPlan, ModError> {
        let server = self.local_server(name)?;
        self.ensure_stopped(&server)?;
        if !kind.is_server_usable() {
            return Err(ModError::ClientOnly { kind });
        }
        let source = self.content.select_source(spec.source)?;
        let request = server_request(
            &server.meta.variant,
            server.meta.version.as_str(),
            kind,
            spec.allow_prerelease,
        );
        let version_id = match spec.version {
            Some(explicit) => explicit.to_owned(),
            None => self
                .content
                .candidates(spec.source, project, &request)
                .await?
                .into_iter()
                .next()
                .map(|candidate| candidate.version_id)
                .ok_or_else(|| ModError::NoCandidate {
                    project: project.to_owned(),
                })?,
        };
        let resolved = source.resolve(project, &version_id).await?;
        // 标题只用于展示，取不到不算失败
        let project_title = source
            .project(project)
            .await
            .ok()
            .map(|detail| detail.title);
        let file = safe_filename(&resolved.file.filename).map_err(ContentError::from)?;
        let target = mods_dir(&server, kind).join(&file);
        if !target.starts_with(&server.dir) {
            return Err(ModError::Escapes { path: target });
        }
        let replaces = target.exists().then(|| target.clone());
        Ok(InstallPlan {
            source: source.name(),
            project_id: resolved.project_id,
            project_title,
            version_id: resolved.version_id,
            version_number: resolved.version_number,
            kind,
            file,
            size: resolved.file.size,
            target,
            replaces,
            request,
            expect_modid: None,
        })
    }

    /// 为修复补装选版本的入口：正式版优先，没有正式版才接受预发布
    ///
    /// 依赖能不能补上不受版本渠道摆布：实测 `unified-api` 在 26.3 上只发
    /// beta，若坚持 `allow_prerelease = false`，必需依赖永远补不上，修复
    /// 命令等于永远失败。排名本身让正式版胜出（见 `content` 的渠道测试），
    /// 因此这里只在「一个正式版都没有」时退一步。
    ///
    /// `version_id` 非空时按该版本精确安装，不再按条件挑：整合包清单给出了
    /// 作者验证过的确切版本，重新挑会漂移。
    async fn plan_dependency_install(
        &self,
        name: &str,
        project: &str,
        kind: ContentKind,
        expect_modid: Option<&str>,
        version_id: Option<&str>,
    ) -> Result<InstallPlan, ModError> {
        // 清单点名了版本就不存在「挑不到正式版」的问题，不必再退一步
        let planned = if version_id.is_some() {
            self.plan_install(
                name,
                project,
                kind,
                InstallSpec {
                    version: version_id,
                    source: None,
                    allow_prerelease: false,
                },
            )
            .await?
        } else {
            match self
                .plan_install(
                    name,
                    project,
                    kind,
                    InstallSpec {
                        version: None,
                        source: None,
                        allow_prerelease: false,
                    },
                )
                .await
            {
                Err(ModError::Content(ContentError::NoCandidate { .. })) => {
                    self.plan_install(
                        name,
                        project,
                        kind,
                        InstallSpec {
                            version: None,
                            source: None,
                            allow_prerelease: true,
                        },
                    )
                    .await
                }
                other => other,
            }?
        };
        Ok(InstallPlan {
            expect_modid: expect_modid.map(ToOwned::to_owned),
            ..planned
        })
    }

    /// 执行安装计划
    ///
    /// 下载到暂存目录 → 检查 → 原子换入。检查判定为 [`InstallVerdict::Refuse`]
    /// 时删除暂存文件并返回 [`ModError::Refused`]，`mods/` 保持原样。
    ///
    /// # Errors
    ///
    /// 服务器状态在计划之后发生变化、下载失败、检查判定拒绝、换入失败时返回
    /// [`ModError`]
    pub async fn install(
        &self,
        name: &str,
        plan: &InstallPlan,
        options: ApplyOptions<'_>,
    ) -> Result<ModInstalled, ModError> {
        let server = self.local_server(name)?;
        self.ensure_stopped(&server)?;
        if !plan.target.starts_with(&server.dir) {
            return Err(ModError::Escapes {
                path: plan.target.clone(),
            });
        }
        let staging = new_staging(&server);
        // 守卫在函数返回（含提前返回与错误）时删除整个暂存目录
        let _guard = StagingGuard(&staging);

        let mut install_options = InstallOptions::new(&staging)
            .source(plan.source)
            .version(&plan.version_id);
        if let Some(progress) = options.progress {
            install_options = install_options.progress(progress);
        }
        let install = self
            .content
            .install(
                &plan.project_id,
                &plan.request,
                install_options,
                options.cancel,
            )
            .await?;

        // 依赖闭包看整个 `mods/` 目录（排除本次要替换的那个文件——它马上就
        // 不在了）：待装文件声明的依赖已装在目录里时不算缺失。只看暂存区
        // 会把「satin 已装好」误报成「缺少 satin」，用户只能 --force。
        let existing = jars_in(&mods_dir(&server, plan.kind))?
            .into_iter()
            .filter(|path| Some(path.as_path()) != plan.replaces.as_deref())
            .collect::<Vec<_>>();
        let staged = [install.path.clone()];
        let audit = self.inspect(&staged, &existing, &server).await?;
        let staged_verdict = verdict_of(audit, &plan.file)?;
        let StagedVerdict {
            risk,
            missing_deps,
            evidence,
            provides,
        } = staged_verdict;
        // 补装依赖时必须确认这个 jar 真的提供要补的 modid：上游按名字检索，
        // 名字相近的项目可能是别的东西（`melody` ≠ `music-and-melody`）
        if let Some(need) = &plan.expect_modid
            && !provides.iter().any(|id| id == need)
        {
            return Err(ModError::WrongProvider {
                need: need.clone(),
                file: plan.file.clone(),
                provides,
            });
        }
        let verdict = InstallVerdict::of(risk, options.allow_blocking);
        if !verdict.is_allowed() {
            return Err(ModError::Refused {
                file: plan.file.clone(),
                risk,
                missing_deps,
                evidence,
            });
        }

        if let Some(parent) = plan.target.parent() {
            crate::fs::create_dir_all(parent).map_err(|err| FsError::io(parent, err))?;
        }
        crate::atomic::replace_file(&install.path, &plan.target)?;
        info!(
            server = name,
            file = %plan.file,
            version = %plan.version_id,
            risk = risk.as_str(),
            "mod installed"
        );
        Ok(ModInstalled {
            file: plan.file.clone(),
            path: plan.target.clone(),
            size: install.size,
            replaced: plan.replaces.clone(),
            risk,
            missing_deps,
            evidence,
        })
    }

    /// 卸载一个 mod
    ///
    /// 选择器先按文件名精确匹配（含省略 `.jar` 的写法），再退回按 modid 匹配；
    /// 按 modid 命中多个文件时不猜，把候选列出来交给用户重选。
    ///
    /// # Errors
    ///
    /// 服务器不存在、位于远端、选择器未命中或命中多个、服务端运行中（非预览
    /// 时）、删除失败时返回 [`ModError`]
    pub fn remove(
        &self,
        name: &str,
        selector: &str,
        kind: ContentKind,
        dry_run: bool,
    ) -> Result<ModRemoved, ModError> {
        let server = self.local_server(name)?;
        let dir = mods_dir(&server, kind);
        let jars = jars_in(&dir)?;
        let hits = match_files(self.inspector.as_ref(), &jars, selector, &dir)?;
        if !dry_run {
            self.ensure_stopped(&server)?;
            for path in &hits {
                crate::trash::trash_file(&server.dir, path)?;
            }
            info!(server = name, removed = hits.len(), "mods removed");
        }
        Ok(ModRemoved {
            files: hits,
            dry_run,
        })
    }

    /// 把若干 mod 文件移进停用目录
    ///
    /// `files` 是相对服务端目录的路径（`mods/x.jar`），通常直接来自安装后
    /// 复核的 [`crate::ModBlocked::file`]。与 [`Self::remove`] 的区别是**语义**：
    /// 移除表达「不要了」，停用表达「暂时不用」，文件留在
    /// [`crate::DISABLED_DIR`] 里，原名不变，用户随时能搬回来。
    ///
    /// 只接受 `mods/` 与 `plugins/` 下的文件：这两个目录之外的东西不是加载
    /// 器读的 mod，把 `server.properties` 挪进停用目录只会让人以为配置丢了。
    ///
    /// # Errors
    ///
    /// 服务器不存在、位于远端、正在运行、路径越界、目标不是普通文件或移动
    /// 失败时返回 [`ModError`]。任一条目失败即整体返回错误——已移动的部分
    /// 保留在停用目录里（可回滚），不试图「回滚一半」再失败一次。
    pub fn disable(&self, name: &str, files: &[String]) -> Result<ModDisabled, ModError> {
        let server = self.local_server(name)?;
        self.ensure_stopped(&server)?;
        let mut moved = Vec::new();
        for file in files {
            let path = server.dir.join(file);
            // 路径来自用户输入（可能是远端帧、可能是手敲），必须确认它落在
            // 加载器会读的两个目录里，且规范化后没有借 `..` 跑出去
            let relative = path
                .strip_prefix(&server.dir)
                .map_err(|_| ModError::Escapes { path: path.clone() })?;
            let inside = relative
                .components()
                .next()
                .is_some_and(|first| first.as_os_str() == "mods" || first.as_os_str() == "plugins");
            if !inside || relative.components().any(|c| c.as_os_str() == "..") {
                return Err(ModError::Escapes { path: path.clone() });
            }
            if !path.is_file() {
                return Err(ModError::NotFound {
                    selector: file.clone(),
                    dir: server.dir.clone(),
                });
            }
            let target = crate::disabled::disable_file(&server.dir, &path)?;
            let to = target.strip_prefix(&server.dir).map_or_else(
                |_| target.display().to_string(),
                |rel| rel.display().to_string(),
            );
            info!(server = name, from = %file, to = %to, "mod disabled");
            moved.push((file.clone(), to));
        }
        Ok(ModDisabled { moved })
    }

    /// 规划一次修复：找出会让服务端起不来的问题并给出处置，**不写盘**
    ///
    /// 与启动预检同一套判定（同一个审计、同一个 `ServerTarget`）：
    ///
    /// - 缺必需依赖 → 在上游按 modid 检索提供方，命中就规划补装；找不到
    ///   提供方的进 `unresolved`，不猜
    /// - 明确不可用（仅客户端且非库 / 与服务端明显不匹配）→ 规划移除该文件，
    ///   原因照实记录
    ///
    /// 上游检索按 modid 找本体：`slug` / `id` 与之相等的优先，名字只是包含
    /// 的次之；不按「服务端可用」过滤，因为库类 mod 的侧别标注经常有误，
    /// 侧别交给安装检查判。modid 与项目 slug
    /// 只是约定不是保证，因此应用前必须经用户确认（CLI 的确认提示）。
    ///
    /// # Errors
    ///
    /// 服务器不存在、位于远端、正在运行、目录无法遍历或上游异常时返回
    /// [`ModError`]
    pub async fn plan_fix(&self, name: &str, kind: ContentKind) -> Result<ModFixPlan, ModError> {
        let server = self.local_server(name)?;
        self.ensure_stopped(&server)?;
        if !kind.is_server_usable() {
            return Err(ModError::ClientOnly { kind });
        }
        let dir = mods_dir(&server, kind);
        let jars = jars_in(&dir)?;
        let audit = self.inspect(&jars, &[], &server).await?;

        let mut installs: Vec<FixInstall> = Vec::new();
        let mut unresolved: Vec<String> = Vec::new();
        // 整合包清单快照：装整合包时留下的权威数据，里面有每个文件的
        // 项目 ID 与版本 ID。补依赖先查它，命中就按 ID 直装——搜索是按
        // 名字猜，会挑错项目（`satin` 搜出来首位是 `satin-free-wakes`）
        let mut snapshot = self.modpack_snapshot(&server).await;
        for report in &audit.mods {
            if report.missing_deps.is_empty() {
                continue;
            }
            let needed_by = file_name_of(&report.capability.path);
            for missing in &report.missing_deps {
                // 同一个依赖被多个文件缺：只检索一次，计划里不重复刷屏。
                // 判重按「已为该 modid 排过候选」而不是「该 modid 出现过」，
                // 多候选才可能都排进来
                if unresolved.iter().any(|seen| seen == missing)
                    || installs.iter().any(|seen| &seen.missing == missing)
                {
                    continue;
                }
                let found = self
                    .resolve_fix_target(missing, snapshot.as_ref(), &server)
                    .await?;
                match found {
                    FixTarget::Installs(candidates) => {
                        for item in candidates {
                            installs.push(FixInstall {
                                needed_by: needed_by.clone(),
                                missing: missing.clone(),
                                project_id: item.project_id,
                                title: item.title,
                                client_only: item.client_only,
                                version_id: item.version_id,
                            });
                        }
                    }
                    FixTarget::Unresolved => unresolved.push(missing.clone()),
                }
            }
        }
        // 把这次读到的 modid 回填进快照：下次「缺 X」就能直接查到提供方。
        // 只在真有变化时重写，普通一次修复不会反复改动文件
        if let Some(snapshot) = snapshot.as_mut() {
            self.refresh_snapshot_modids(&server, snapshot, &audit)
                .await;
        }

        let mut removals: Vec<FixRemove> = Vec::new();
        for report in &audit.mods {
            // 缺依赖的文件默认由补装项处置；补不上的、且文件本身只是客户端
            // 功能时，移除才是能让服务端起得来的处置——这种情况多见于整合
            // 包把客户端界面 mod（要客户端音乐库）一起摊到服务端上
            if !report.missing_deps.is_empty() {
                let unsatisfiable = report
                    .missing_deps
                    .iter()
                    .all(|missing| unresolved.iter().any(|seen| seen == missing));
                let client_feature = report.capability.side.client == SideSupport::Required
                    && report.capability.side.server != SideSupport::Required;
                if unsatisfiable && client_feature {
                    removals.push(FixRemove {
                        file: PathBuf::from(&report.capability.path),
                        reason: format!(
                            "仅客户端功能，且必需依赖 {} 在服务端找不到可补装的提供方",
                            report.missing_deps.join("、")
                        ),
                    });
                }
                continue;
            }
            if report.risk == Risk::Blocking {
                removals.push(FixRemove {
                    file: PathBuf::from(&report.capability.path),
                    reason: "上游标注或清单判定为仅客户端，服务端无法装载".to_owned(),
                });
                continue;
            }
            if let Some(fit) = &report.fit
                && fit.verdict == FitVerdict::Rejected
            {
                let detail = fit
                    .issues
                    .first()
                    .map_or_else(|| "与服务端不匹配".to_owned(), |issue| issue.detail.clone());
                removals.push(FixRemove {
                    file: PathBuf::from(&report.capability.path),
                    reason: format!("与这台服务端不匹配（{detail}）"),
                });
            }
        }

        Ok(ModFixPlan {
            server: server.meta.name.as_str().to_owned(),
            kind,
            installs,
            removals,
            unresolved,
            crash: None,
            unattributed: Vec::new(),
        })
    }

    /// 规划一次**按启动崩溃栈**的修复：定位到具体文件并规划移除，**不写盘**
    ///
    /// 与 [`Self::plan_fix`] 的分工：那个看的是「文件自己声称什么」（元数据、
    /// 上游标注），本函数看的是「加载器实际崩在哪里」。后者是事后事实，
    /// 因此不需要猜——实测里 `environment: "*"` 且声明了 `server` 入口点的
    /// mod（CalcMod）在服务端注册命令时才触发客户端类加载，任何静态判定
    /// 都看不出来。
    ///
    /// # 归因口径
    ///
    /// 栈帧里的类名反查回 `mods/` 下的具体 jar：命中即规划移除该文件。
    /// 一个文件都不命中的类进 `unattributed`，不猜、不按名字相似度匹配。
    ///
    /// # 不做什么
    ///
    /// 不自动补装、不检查依赖：崩溃栈只说明「谁炸了」，不说明「缺什么」。
    /// 把两类问题混进一份计划会让用户无法判断该执行哪一半。
    ///
    /// # Errors
    ///
    /// 服务器不存在、位于远端、正在运行、日志不可读或目录无法遍历时返回
    /// 对应的 [`ModError`]
    pub async fn plan_fix_from_log(
        &self,
        name: &str,
        kind: ContentKind,
        lines: usize,
    ) -> Result<ModFixPlan, ModError> {
        let server = self.local_server(name)?;
        self.ensure_stopped(&server)?;
        if !kind.is_server_usable() {
            return Err(ModError::ClientOnly { kind });
        }

        let log_path = console_log_path(&server.dir);
        let tail = crate::lifecycle::tail_lines(&log_path, lines)
            .map_err(|err| FsError::io(&log_path, err))?;
        let crash = attribute_crash(&tail.join("\n"));

        // 类名 → 文件索引：每个 jar 读一次中央目录。日志行数越多可疑帧越多，
        // 但文件数固定，因此索引一次比「每个可疑帧重扫一遍 jar」快得多。
        //
        // 读 zip 是阻塞 IO 且要遍历中央目录，放到阻塞线程池：这条路径由 CLI
        // 调用，跑在异步运行时上，几百 MB 的 jar 直接在这里读会占住工作线程。
        let dir = mods_dir(&server, kind);
        let jars = jars_in(&dir)?;
        let inspector = Arc::clone(&self.inspector);
        let owners: Vec<(PathBuf, HashSet<String>)> = tokio::task::spawn_blocking(move || {
            let mut owners: Vec<(PathBuf, HashSet<String>)> = Vec::with_capacity(jars.len());
            for jar in jars {
                // 读不出类的文件不参与归因：它要么是坏归档（别的问题），要么
                // 实现不支持按类名检索（`None`）。两种情况都不该让它成为结论，
                // 更不该让一个坏 jar 让整次归因失败
                if let Ok(Some(classes)) = inspector.class_names(&jar) {
                    owners.push((jar, classes.into_iter().collect()));
                }
            }
            owners
        })
        .await
        .map_err(|err| {
            FsError::io(
                &dir,
                std::io::Error::other(format!("blocking task failed: {err}")),
            )
        })?;

        let mut removals: Vec<FixRemove> = Vec::new();
        let mut unattributed: Vec<String> = Vec::new();

        // 优先用加载器指认的肇事类：它们比 at 帧更权威（Kotlin 反射
        // 加载的类不会以 at 帧出现，但 Fabric Loader 会在错误消息里写出
        // `provided by 'lilac' at 'opekope2.lilac.impl.tick.TickNotifier'`）。
        // 只要指认命中了一个文件，就不再扫 at 帧——框架类（如
        // supermartijn642corelib）会跟着出现在栈上，但它们是旁观者。
        for blamed in &crash.blamed {
            let owner = owners
                .iter()
                .find(|(_, classes)| classes.contains(&blamed.class));
            match owner {
                Some((file, _)) => {
                    let reason = match &crash.missing {
                        missing if !missing.is_empty() => format!(
                            "启动崩溃归因：加载器指认 {} 引用了服务端不存在的 {}",
                            blamed.dotted_class(),
                            missing.join("、").replace('/', ".")
                        ),
                        _ => format!(
                            "启动崩溃归因：加载器指认 {} 是肇事者",
                            blamed.dotted_class()
                        ),
                    };
                    if !removals.iter().any(|item| item.file == *file) {
                        removals.push(FixRemove {
                            file: file.clone(),
                            reason,
                        });
                    }
                }
                None => unattributed.push(blamed.dotted_class()),
            }
        }

        // blamed 命中时不再扫 at 帧：栈上的框架类（supermartijn642corelib 等）
        // 只是旁观者，把它们的 jar 也删掉是误伤。
        if removals.is_empty() {
            for suspect in &crash.suspects {
                let owner = owners
                    .iter()
                    .find(|(_, classes)| classes.contains(&suspect.class));
                match owner {
                    Some((file, _)) => {
                        let reason = match &crash.missing {
                            // 缺失类是崩溃的直接原因，写进原因里用户才能核对
                            missing if !missing.is_empty() => format!(
                                "启动崩溃归因：{} 引用了服务端不存在的 {}",
                                suspect.dotted_class(),
                                missing.join("、").replace('/', ".")
                            ),
                            _ => format!("启动崩溃归因：崩在 {}", suspect.dotted_class()),
                        };
                        if !removals.iter().any(|item| item.file == *file) {
                            removals.push(FixRemove {
                                file: file.clone(),
                                reason,
                            });
                        }
                    }
                    None => unattributed.push(suspect.dotted_class()),
                }
            }
        }

        Ok(ModFixPlan {
            server: server.meta.name.as_str().to_owned(),
            kind,
            installs: Vec::new(),
            removals,
            unresolved: Vec::new(),
            crash: Some(crash),
            unattributed,
        })
    }

    /// 读取整合包清单快照
    ///
    /// 没有快照（不是整合包开服、装完被删、格式对不上）时返回 `None`，
    /// 调用方退回按名字检索的旧路径。
    async fn modpack_snapshot(&self, server: &RegisteredServer) -> Option<ModpackManifestSnapshot> {
        ModpackManifestSnapshot::load(&server.dir).await
    }

    /// 为一个缺失的 modid 找出可补装的候选
    ///
    /// 优先查整合包清单快照：清单里记着每个文件的**项目 ID 与版本 ID**
    /// （藏在下载地址里），命中就按 ID 直装，一个检索请求都不发。搜索是按
    /// 名字猜，实测搜 `satin` 首位命中是毫不相干的 `satin-free-wakes`；
    /// 只有快照里查不到时才退回检索。
    async fn resolve_fix_target(
        &self,
        modid: &str,
        snapshot: Option<&ModpackManifestSnapshot>,
        server: &RegisteredServer,
    ) -> Result<FixTarget, ModError> {
        if let Some(hit) = snapshot.and_then(|snapshot| snapshot.file_providing(modid))
            && let Some(project_id) = hit.project_id.clone()
        {
            return Ok(FixTarget::Installs(vec![FixCandidate {
                project_id,
                title: file_name_of(&hit.path),
                client_only: hit.server.as_deref() == Some("unsupported"),
                version_id: hit.version_id.clone(),
            }]));
        }
        // 全部候选都要排进去：安装时逐个校验 modid，第一个不提供就换下一个。
        // 只排第一个会让 `satin` 这类「本体换了后缀」的依赖在计划阶段就被判死
        let hits = self.find_provider(modid, server).await?;
        if hits.is_empty() {
            return Ok(FixTarget::Unresolved);
        }
        Ok(FixTarget::Installs(
            hits.into_iter()
                .map(|hit| FixCandidate {
                    project_id: hit.id,
                    title: hit.title,
                    client_only: hit.client_only,
                    // 检索命中没有精确版本，安装时按服务端条件挑
                    version_id: None,
                })
                .collect(),
        ))
    }

    /// 把刚读到的 modid 回填进快照，并重写文件
    ///
    /// 清单只说文件名，不说 modid；装好之后读 jar 才知道谁提供什么。回填
    /// 一次之后，「缺 modid X 该装哪个项目」就有确定答案，不必搜索。
    async fn refresh_snapshot_modids(
        &self,
        server: &RegisteredServer,
        snapshot: &mut ModpackManifestSnapshot,
        audit: &ModAudit,
    ) {
        let mut changed = false;
        for report in &audit.mods {
            let filename = file_name_of(&report.capability.path);
            let Some(entry) = snapshot.file_named(&filename) else {
                continue;
            };
            let path = entry.path.clone();
            // 自己声明的 modid 加上它别名提供的（`provides`），两者都能满足依赖
            let mut modids: Vec<String> = report.capability.id.iter().cloned().collect();
            for alias in &report.capability.provides {
                if !modids.contains(alias) {
                    modids.push(alias.clone());
                }
            }
            let version = report.capability.version.clone();
            changed |= snapshot.record_modids(&path, modids, version);
        }
        if changed && snapshot.save(&server.dir).await.is_err() {
            // 回填失败不影响本次修复：快照只是加速后续查询的缓存
            tracing::debug!("modpack manifest snapshot write-back skipped");
        }
    }

    /// 在上游检索某个 modid 的提供方
    ///
    /// 检索是「尽力而为」：上游检索不到就返回 `None`，不猜也不报错——修复
    /// 计划里如实进 `unresolved`。
    ///
    /// 检索口径**不**按「服务端可用」过滤，与 [`Self::search`] 不同。缺依赖
    /// 要补的是「谁提供这个 modid」，那个项目是不是被上游标成仅客户端，不能
    /// 由这一层判掉：库类 mod 的上游标注经常有误（`satin`、`melody`），而
    /// 真正只有客户端代码的 jar 会得到 `Blocking` 并被安装检查拒绝，带
    /// `main` 入口的库只会得到 `Review` 并放行。实测反例：`fancymenu` 在
    /// `fabric.mod.json` 里要求 `melody >=1.0.17`，`melody` 本体在 Modrinth
    /// 上确实存在（`CVT4pFB2`，`26.3` 有 `1.0.17`，jar 内 `id=melody`、
    /// `environment: "*"`、有 `main` 入口，服务端装载正常），但被上游标成
    /// `server_side: unsupported`；按该标注过滤后整条消失，检索只剩名字里
    /// 含 `melody` 的 `music-and-melody`（modid 是 `music_and_melody`），
    /// 补装必然被 `expect_modid` 拒，最后报成「上游找不到提供方」。
    async fn find_provider(
        &self,
        modid: &str,
        server: &RegisteredServer,
    ) -> Result<Vec<ContentProject>, ModError> {
        // 上游检索把 modid 当整词条：`geckolib3` 搜不到 `geckolib`（实测
        // 全文索引里带数字尾巴的旧命名排不进结果）。原词落空后去掉尾部
        // 版本数字再试一次。
        //
        // 返回**全部**候选而不是第一个：上游按名字检索，名字最像的未必
        // 提供该 modid。实测 `satin` 的首位是 `satin-free-wakes`（modid
        // 是 `wakes`），真正的 `satin-api` 排第二；只取首位会被
        // `expect_modid` 拒掉，然后整条依赖被判成「上游找不到提供方」，
        // 明明提供方就在结果里。
        for term in provider_search_terms(modid) {
            let query = ContentQuery::new().keyword(&term).page(0, 5);
            let mut relaxed = server_query(
                &server.meta.variant,
                server.meta.version.as_str(),
                ContentKind::Mod,
                &query,
            );
            relaxed.server_usable_only = false;
            let found = self.content.search(None, &relaxed).await?;
            let candidates = rank_provider_hits(found.hits, &term);
            if !candidates.is_empty() {
                return Ok(candidates);
            }
        }
        Ok(Vec::new())
    }

    /// 执行修复计划：移除带问题的文件，补装缺失依赖
    ///
    /// 先移除后补装：移除会改变目录的依赖闭包，先移除能避免给已被判
    /// 「不匹配」的 mod 旧版本补装依赖。补装沿用单文件安装的完整门禁
    /// （下载 → 检查 → 原子换入），拒绝时不中断其余项，逐项汇总。
    ///
    /// # Errors
    ///
    /// 服务器不存在、位于远端、状态在计划之后发生变化或移除失败时返回
    /// [`ModError`]
    pub async fn apply_fix(
        &self,
        name: &str,
        plan: &ModFixPlan,
        options: ApplyOptions<'_>,
    ) -> Result<ModFixApplied, ModError> {
        let server = self.local_server(name)?;
        self.ensure_stopped(&server)?;
        // 计划自己带着产出它的类别：插件计划的移除校验与补装落点都在
        // `plugins/`，按 `mods/` 执行会把合法移除报成越界、补装落错目录
        let kind = plan.kind;
        if !kind.is_server_usable() {
            return Err(ModError::ClientOnly { kind });
        }

        let mut removed: Vec<PathBuf> = Vec::new();
        for item in &plan.removals {
            // 路径出自本服务的审计（mods/ 目录的直接子文件），仍然校验
            // 落点在服务端目录内，防计划在手工流转中被篡改
            if !item.file.starts_with(server.dir.join(kind.install_dir())) {
                return Err(ModError::Escapes {
                    path: item.file.clone(),
                });
            }
            if item.file.is_file() {
                crate::trash::trash_file(&server.dir, &item.file)?;
                removed.push(item.file.clone());
            }
        }

        let mut unresolved: Vec<String> = plan.unresolved.clone();
        let (installed, refused) = self
            .install_fix_batch(name, &server, kind, plan, options, &mut unresolved)
            .await?;

        Ok(ModFixApplied {
            removed,
            installed,
            refused,
            unresolved,
        })
    }

    /// 按依赖链逐项补装，汇总装上的与被拒的
    ///
    /// 依赖是链不是一层：补装的 mod 自己可能还缺依赖（实测现场：fancymenu
    /// 缺 melody，melody 又缺 unified-api）。装被拒不等于修不了——把被拒时
    /// 暴露的缺失依赖排到队尾继续解析，本项的重试排在依赖之后。单项失败只
    /// 记一笔，不中断其余项。
    async fn install_fix_batch(
        &self,
        name: &str,
        server: &RegisteredServer,
        kind: ContentKind,
        plan: &ModFixPlan,
        options: ApplyOptions<'_>,
        unresolved: &mut Vec<String>,
    ) -> Result<(Vec<ModInstalled>, Vec<FixRefused>), ModError> {
        let mut installed: Vec<ModInstalled> = Vec::new();
        let mut queue: VecDeque<PendingFix> = plan
            .installs
            .iter()
            .map(|item| PendingFix {
                project_id: item.project_id.clone(),
                missing: Some(item.missing.clone()),
                version_id: item.version_id.clone(),
                retried: false,
            })
            .collect();
        let mut state = FixState::default();
        for item in &plan.installs {
            state.plan_candidate(&item.missing);
        }
        let mut expansions = 0usize;

        while let Some(pending) = queue.pop_front() {
            if state.visited.contains(&pending.project_id) {
                continue;
            }
            let install_plan = match self
                .plan_dependency_install(
                    name,
                    &pending.project_id,
                    kind,
                    pending.missing.as_deref(),
                    pending.version_id.as_deref(),
                )
                .await
            {
                Ok(plan) => plan,
                // 逐项汇总：某一项在上游没有匹配版本（或上游本身出错）不该
                // 让整批停下——其余待修项仍然可以装好
                Err(err) => {
                    state.visited.insert(pending.project_id.clone());
                    if let Some(missing) = pending.missing.as_deref() {
                        state.candidate_rejected(missing);
                    }
                    state.refused.push(FixRefused {
                        project_id: pending.project_id.clone(),
                        reason: err.to_string(),
                    });
                    continue;
                }
            };
            match self
                .install(
                    name,
                    &install_plan,
                    ApplyOptions {
                        allow_blocking: options.allow_blocking,
                        progress: options.progress,
                        cancel: options.cancel,
                    },
                )
                .await
            {
                Ok(installed_mod) => {
                    state.visited.insert(pending.project_id.clone());
                    installed.push(installed_mod);
                }
                // 缺依赖被拒：先补它自己的依赖，再回头重试本项
                Err(ModError::Refused { missing_deps, .. })
                    if may_expand_dependencies(&pending, &missing_deps, expansions) =>
                {
                    expansions += 1;
                    self.expand_dependency_chain(
                        &pending,
                        &missing_deps,
                        server,
                        &mut queue,
                        &mut state,
                    )
                    .await?;
                }
                Err(ModError::Refused { risk, evidence, .. }) => {
                    state.visited.insert(pending.project_id.clone());
                    if let Some(missing) = pending.missing.as_deref() {
                        state.candidate_rejected(missing);
                    }
                    state.refused.push(FixRefused {
                        project_id: pending.project_id.clone(),
                        reason: refusal_reason(risk, &evidence),
                    });
                }
                Err(err) => {
                    state.visited.insert(pending.project_id.clone());
                    record_install_failure(&pending, &err, &mut state);
                }
            }
        }

        *unresolved = state.unresolved;
        Ok((installed, state.refused))
    }

    /// 缺依赖被拒后展开一层：依赖排进队列，本项排在其后重试
    ///
    /// 依赖一项都排不进去时，本项记为 refused 并说明缺哪个 modid。
    async fn expand_dependency_chain(
        &self,
        pending: &PendingFix,
        missing_deps: &[String],
        server: &RegisteredServer,
        queue: &mut VecDeque<PendingFix>,
        state: &mut FixState,
    ) -> Result<(), ModError> {
        let planned = self
            .queue_missing_deps(missing_deps, server, queue, state)
            .await?;
        if planned {
            queue.push_back(PendingFix {
                project_id: pending.project_id.clone(),
                missing: pending.missing.clone(),
                version_id: pending.version_id.clone(),
                retried: true,
            });
            return Ok(());
        }
        state.refused.push(FixRefused {
            project_id: pending.project_id.clone(),
            reason: format!(
                "缺少必需依赖 {}，上游找不到可自动补装的提供方",
                missing_deps.join("、")
            ),
        });
        Ok(())
    }

    /// 把一组缺失依赖的提供方排进补装队列
    ///
    /// 返回「是否至少排进去一项」：一项都排不进去说明上游确实没有可补的
    /// 东西，调用方据此把该项记为 refused 而不是反复重试。找不到提供方的
    /// 依赖同时进 `unresolved`，让用户看到缺的到底是哪个 modid。
    ///
    /// 同一 modid 的**全部**候选都会入队，由安装时的 `expect_modid` 逐个
    /// 校验：名字最像的未必提供这个 modid，被拒就换下一个。
    ///
    /// 与 [`Self::plan_fix`] 一样先查整合包清单快照：清单里有项目与版本 ID
    /// 时按 ID 直装，只有在快照里查不到时才回到按名字检索。
    async fn queue_missing_deps(
        &self,
        missing_deps: &[String],
        server: &RegisteredServer,
        queue: &mut VecDeque<PendingFix>,
        state: &mut FixState,
    ) -> Result<bool, ModError> {
        let snapshot = self.modpack_snapshot(server).await;
        let mut planned = false;
        for missing in missing_deps {
            if let Some(hit) = snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.file_providing(missing))
                && let Some(project_id) = hit.project_id.clone()
            {
                if !state.visited.contains(&project_id) {
                    state.plan_candidate(missing);
                    queue.push_back(PendingFix {
                        project_id,
                        missing: Some(missing.clone()),
                        version_id: hit.version_id.clone(),
                        retried: false,
                    });
                    planned = true;
                }
                continue;
            }
            let fresh = self
                .find_provider(missing, server)
                .await?
                .into_iter()
                .filter(|hit| !state.visited.contains(&hit.id))
                .collect::<Vec<_>>();
            if fresh.is_empty() {
                state.no_provider(missing);
                continue;
            }
            for hit in fresh {
                state.plan_candidate(missing);
                queue.push_back(PendingFix {
                    project_id: hit.id,
                    missing: Some(missing.clone()),
                    // 检索命中没有精确版本，按服务端条件挑
                    version_id: None,
                    retried: false,
                });
                planned = true;
            }
        }
        Ok(planned)
    }

    /// 规划一次更新：识别已装文件并算出可更新的项，**不写盘**
    ///
    /// `selector` 为 `None` 时检查全部；给出时只检查命中的文件（文件名或
    /// modid 口径与 [`Self::remove`] 相同）。上游不认识的文件进
    /// [`UpdatePlan::skipped`]，不会被当成「已是最新」。
    ///
    /// # Errors
    ///
    /// 服务器不存在、位于远端、正在运行、上游异常，或选择器未命中/命中多个时
    /// 返回 [`ModError`]
    pub async fn plan_update(
        &self,
        name: &str,
        selector: Option<&str>,
        options: ListOptions<'_>,
    ) -> Result<UpdatePlan, ModError> {
        let server = self.local_server(name)?;
        self.ensure_stopped(&server)?;
        let list = self
            .list(
                name,
                ListOptions {
                    kind: options.kind,
                    identify: true,
                    source: options.source,
                    updates: true,
                },
            )
            .await?;

        // 选择器只在已装文件里挑：先在磁盘口径上解析，未命中就是未命中
        let selected: Vec<String> = match selector {
            Some(selector) => {
                let jars: Vec<PathBuf> = list
                    .mods
                    .iter()
                    .map(|entry| PathBuf::from(&entry.report.capability.path))
                    .collect();
                match_files(self.inspector.as_ref(), &jars, selector, &list.dir)?
                    .into_iter()
                    .filter_map(|path| {
                        path.file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                    })
                    .collect()
            }
            None => Vec::new(),
        };

        let mut items = Vec::new();
        // 读不出来的文件同样进 `skipped`：它们既不能被识别也不能被更新，
        // 静默消失等于宣称「已是最新」。列表记的是路径，这里统一成文件名——
        // 与 `items` 里的口径一致，入口不必再各自解析。
        let mut skipped: Vec<(String, String)> = list
            .unreadable
            .iter()
            .map(|(path, reason)| (file_name_of(path), reason.clone()))
            .collect();
        for entry in list.mods {
            if !selected.is_empty() && !selected.contains(&entry.file) {
                continue;
            }
            let current = PathBuf::from(&entry.report.capability.path);
            let Some(identity) = entry.identity else {
                skipped.push((entry.file, "upstream does not know this file".to_owned()));
                continue;
            };
            match entry.update {
                Some(UpdateDecision::Available {
                    version_id,
                    version_number,
                }) => {
                    let spec = InstallSpec {
                        version: Some(&version_id),
                        source: options.source,
                        allow_prerelease: true,
                    };
                    let plan = self
                        .plan_install(name, &identity.project_id, options.kind, spec)
                        .await?;
                    items.push(UpdateItem {
                        file: entry.file,
                        current,
                        project_id: identity.project_id,
                        from_version: identity.version_number,
                        to_version: version_number,
                        plan,
                    });
                }
                Some(UpdateDecision::UpToDate) => {}
                None => skipped.push((entry.file, "update not resolved".to_owned())),
            }
        }
        Ok(UpdatePlan { items, skipped })
    }

    /// 执行更新计划
    ///
    /// 逐项调用 [`Self::install`]，因此每项都会重新确认服务端已停机并重新做
    /// 服务端可用性检查。新版本换了文件名时，装成功后删掉旧文件：两份同 modid
    /// 的 jar 留在 `mods/` 里，加载器启动阶段会直接报重复。
    ///
    /// # Errors
    ///
    /// 任一项安装失败时立即停止：**前面几项已经落盘**，盘上处于部分更新状态，
    /// 因此返回 [`ModError::UpdateAborted`]，把「停在哪一项、已完成几项」连同
    /// 真实原因一起报出来——只报底层「下载失败」会让用户以为盘上没变。
    pub async fn apply_update(
        &self,
        name: &str,
        plan: &UpdatePlan,
        options: ApplyOptions<'_>,
    ) -> Result<UpdateApplied, ModError> {
        let mut changes = Vec::with_capacity(plan.items.len());
        for item in &plan.items {
            let installed = match self
                .install(
                    name,
                    &item.plan,
                    ApplyOptions {
                        allow_blocking: options.allow_blocking,
                        progress: options.progress,
                        cancel: options.cancel,
                    },
                )
                .await
            {
                Ok(installed) => installed,
                Err(err) => {
                    warn!(
                        server = name,
                        failed = %item.file,
                        applied = changes.len(),
                        error = %err,
                        "mod update aborted"
                    );
                    return Err(ModError::UpdateAborted {
                        server: name.to_owned(),
                        failed: item.file.clone(),
                        applied: changes.len(),
                        source: Box::new(err),
                    });
                }
            };
            let removed = remove_replaced(&item.current, &installed.path)?;
            changes.push(UpdateChange {
                file: item.file.clone(),
                from_version: item.from_version.clone(),
                to_version: item.to_version.clone(),
                installed,
                removed,
            });
        }
        Ok(UpdateApplied { changes })
    }

    /// 取本地服务器记录；远端条目不在这里处理
    fn local_server(&self, name: &str) -> Result<RegisteredServer, ModError> {
        let sourced = self.registry.get_sourced(name)?;
        if !sourced.origin.is_local() {
            return Err(ModError::Remote {
                name: name.to_owned(),
            });
        }
        Ok(sourced.value)
    }

    /// 拒绝在运行中的服务端上改动 `mods/`
    ///
    /// 状态取不到时**不猜**：判不出是否在跑就不动盘，否则可能写进正在被加载器
    /// 读取的目录。
    fn ensure_stopped(&self, server: &RegisteredServer) -> Result<(), ModError> {
        let status = self
            .backend
            .supervisor()
            .status(&server.server_ref())
            .map_err(|err| ModError::UnknownState {
                name: server.meta.name.as_str().to_owned(),
                reason: err.to_string(),
            })?;
        if status.is_some_and(|status| status.status.is_alive()) {
            return Err(ModError::Running {
                name: server.meta.name.as_str().to_owned(),
            });
        }
        Ok(())
    }

    /// 本地检查一批 jar（不联网）
    ///
    /// 目标服务端条件取自服务端元数据：加载器、MC 版本、Java 大版本。
    /// 三者一起构成「能不能装到这台服务端」的判定依据。
    ///
    /// `context` 是参与依赖闭包但不进结果的文件：安装检查传入已装的 `mods/`
    /// 目录，待装文件声明的依赖已装在目录里时不算缺失——加载器解析看的是
    /// 整个目录，不是这一个 jar。列表检查不需要（那批文件本身就是目录）。
    async fn inspect(
        &self,
        paths: &[PathBuf],
        context: &[PathBuf],
        server: &RegisteredServer,
    ) -> Result<ModAudit, ModError> {
        let service = ModInspectService::local(Arc::clone(&self.inspector));
        let target = server_target(server);
        Ok(service
            .inspect_with_context(paths, context, false, Some(&target))
            .await?)
    }

    /// 单个已识别文件的更新结论
    ///
    /// 上游没有兼容版本（[`ContentError::NoCandidate`]）是「没有可装的更新」，
    /// 不是失败；其余错误照常上抛，避免把「查询挂了」说成「已是最新」。
    async fn update_decision(
        &self,
        source: Option<&str>,
        server: &RegisteredServer,
        identity: &ContentIdentity,
    ) -> Result<Option<UpdateDecision>, ModError> {
        // 识别出来是插件就按插件的加载器条件问：拿 mod 的条件去筛插件版本永远
        // 筛不出候选，会把「有更新」误报成「已最新」
        let kind = identity.kind.unwrap_or(ContentKind::Mod);
        let request = request_for(kind, &server.meta.variant, server.meta.version.as_str());
        match self
            .content
            .candidates(source, &identity.project_id, &request)
            .await
        {
            Ok(candidates) => Ok(Some(decide_update(identity, &candidates))),
            Err(ContentError::NoCandidate { .. }) => Ok(Some(UpdateDecision::UpToDate)),
            Err(err) => Err(err.into()),
        }
    }
}

/// 暂存目录守卫：离开作用域就整目录删除
struct StagingGuard<'a>(&'a Path);

impl Drop for StagingGuard<'_> {
    fn drop(&mut self) {
        let _ = crate::fs::remove_dir_all(self.0);
    }
}

/// 一台服务端的某个内容目录（`mods/` / `plugins/` / …）
fn mods_dir(server: &RegisteredServer, kind: ContentKind) -> PathBuf {
    server.dir.join(kind.install_dir())
}

/// 这台服务端装某类内容时的版本挑选条件
///
/// 加载器与游戏版本都取自服务端元数据，因此「交互里列出的版本」与「自动挑
/// 的版本」用的是同一份条件。
fn server_request(
    variant: &ServerVariant,
    game_version: &str,
    kind: ContentKind,
    allow_prerelease: bool,
) -> ContentRequest {
    let mut request = request_for(kind, variant, game_version);
    if allow_prerelease {
        request = request.allow_prerelease();
    }
    request
}

/// 把用户的检索条件加上这台服务端的硬性条件
///
/// 类别限定为用户要装的类别；加载器只在类别按变体过滤时生效
/// （数据包不看加载器）；游戏版本总是限定，否则会列出这台服务端跑不起来的
/// 项目，用户选完才发现没有可安装版本。同样地，仅客户端的项目（光影加载器、
/// 小地图…）也挡在列表外——它们能装进 `mods/`，但装上去服务端起不来。
fn server_query(
    variant: &ServerVariant,
    game_version: &str,
    kind: ContentKind,
    query: &ContentQuery,
) -> ContentQuery {
    let request = server_request(variant, game_version, kind, false);
    let mut query = query.clone();
    query.kinds = vec![kind];
    query.loaders = request
        .loader_filter()
        .map(<str>::to_owned)
        .into_iter()
        .collect();
    query.game_versions = request
        .game_version
        .as_deref()
        .map(<str>::to_owned)
        .into_iter()
        .collect();
    query.server_usable_only = true;
    query
}

/// 目录里的 jar；目录不存在就是空（先装第一个 mod 之前目录本来就不在）
fn jars_in(dir: &Path) -> Result<Vec<PathBuf>, ModError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    Ok(collect_jars(dir)?)
}

/// 服务端变体对应的 mod 加载器
fn loader_of(variant: &ServerVariant) -> Option<ModLoader> {
    variant.content_loader().and_then(ModLoader::parse)
}

/// 这台服务端的安装条件
///
/// Java 大版本优先取元数据里记录的（启动时实际会用的那个）；旧记录没有这个
/// 字段，按 MC 版本推算补上——与启动时的判定口径一致，两处不一致会让
/// 「检查说没问题、启动说缺 Java」。
///
/// 入口（`mod inspect --server`）也用它，避免 CLI 与 service 各拼一份条件。
#[must_use]
pub fn server_target(server: &RegisteredServer) -> ServerTarget {
    let meta = &server.meta;
    ServerTarget {
        loader: loader_of(&meta.variant),
        mc_version: Some(meta.version.as_str().to_owned()),
        java_major: target_java(meta.java_major, meta.version.as_str()),
        bridged_loaders: Vec::new(),
    }
}

/// 这台服务端实际会用的 Java 大版本
///
/// 元数据记录的优先（启动时按它挑运行时），旧记录没有这个字段就按 MC 版本
/// 推算——与启动判定同源，避免「检查说没问题、启动说缺 Java」。
fn target_java(recorded: Option<domain::java::JavaMajor>, mc: &str) -> Option<u32> {
    recorded
        .or_else(|| domain::server::version::required_java_major(mc))
        .and_then(|major| u32::try_from(major.as_i16()).ok())
}

/// 新建一个本次安装专用的暂存目录（同卷，便于 rename 换入）
fn new_staging(server: &RegisteredServer) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    server.dir.join(STAGING_DIR).join(format!(
        "{}-{}-{seq}",
        crate::atomic::unix_now(),
        std::process::id()
    ))
}

/// 暂存文件的一次检查结论
struct StagedVerdict {
    /// 风险等级
    risk: Risk,
    /// 缺失的必需依赖
    missing_deps: Vec<String>,
    /// 判定依据
    evidence: Vec<String>,
    /// 这个 jar 能提供的 modid（含 `provides` 别名）
    provides: Vec<String>,
}

/// 从「只装了一个文件」的审计结果里取结论
///
/// 读不出来时**不**当成 `Ok`：那样用户会以为检查过了。
fn verdict_of(audit: ModAudit, file: &str) -> Result<StagedVerdict, ModError> {
    if let Some(report) = audit.mods.into_iter().next() {
        let evidence: Vec<String> = report
            .capability
            .side
            .evidence
            .iter()
            .map(|item| item.detail.clone())
            .collect();
        let provides = report.capability.provided_ids();
        return Ok(StagedVerdict {
            risk: report.risk,
            missing_deps: report.missing_deps,
            evidence,
            provides,
        });
    }
    let reason = audit.unreadable.into_iter().next().map_or_else(
        || "inspection produced no result".to_owned(),
        |(_, why)| why,
    );
    Err(ModError::Unreadable {
        file: file.to_owned(),
        reason,
    })
}

/// 该不该为这一项继续往下展开依赖
///
/// 只在「确实缺依赖、还没为它展开过、且没到深度上限」时展开：成环的数据
/// 必须停在一个可报告的位置。
fn may_expand_dependencies(
    pending: &PendingFix,
    missing_deps: &[String],
    expansions: usize,
) -> bool {
    !missing_deps.is_empty() && !pending.retried && expansions < MAX_DEPENDENCY_DEPTH
}

/// 安装被拒时的展示措辞（非依赖成因，如上游标注仅客户端）
fn refusal_reason(risk: Risk, evidence: &[String]) -> String {
    if evidence.is_empty() {
        format!("安装检查判定为 {}，服务端无法装载", risk.as_str())
    } else {
        format!("安装检查判定为 {}：{}", risk.as_str(), evidence.join("；"))
    }
}

/// 记下一项补装失败
///
/// 命中的项目不提供要补的 modid 时只是「这个候选不是本体」——还有别的候选
/// 没试完，只有全部试完才算「上游没有提供方」。这类候选是检索噪声（名字像
/// 但不是本体），不逐条进 `refused`，最终由 `unresolved` 汇总成一行，用户
/// 不必读一串同义报错。
fn record_install_failure(pending: &PendingFix, err: &ModError, state: &mut FixState) {
    if let ModError::WrongProvider { need, .. } = err {
        state.candidate_rejected(need);
        return;
    }
    if let Some(missing) = pending.missing.as_deref() {
        state.candidate_rejected(missing);
    }
    state.refused.push(FixRefused {
        project_id: pending.project_id.clone(),
        reason: err.to_string(),
    });
}

/// 路径的最后一段（展示用文件名）
fn file_name_of(path: &str) -> String {
    Path::new(path).file_name().map_or_else(
        || path.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// 修复补装时检索提供方的检索词，按优先级排列
///
/// 依赖 modid 常带版本尾巴（`geckolib3`、`clothconfig2`），而上游项目
/// 名通常不带（`geckolib`、`cloth-config`）；上游全文检索把检索词当
/// 整词条，带尾巴的原词往往搜不到本体。因此原词之外补一个去掉尾部
/// 版本数字的词——只剥末尾数字，不动 `fabric-api` 这类正常命名。
fn provider_search_terms(modid: &str) -> Vec<String> {
    let mut terms = vec![modid.to_owned()];
    let trimmed = modid.trim_end_matches(|c: char| c.is_ascii_digit());
    if trimmed.len() != modid.len() && !trimmed.is_empty() {
        terms.push(trimmed.to_owned());
    }
    terms
}

/// 命中项目与检索词的相关程度，越小越相关；不相关为 `None`
///
/// 上游检索按相关度排序，但缺依赖 modid 往往命中同名无关项目——实测
/// 反例：缺 `cupboard` 时第一个可用命中是 `aesthetic-storage`（Modrinth
/// 上搜不到那个库本体），照单全收就会给用户装进毫不相干的 mod。
///
/// 只做「包含」判断也不够：它会把「名字里恰好带这个词」的项目排到本体
/// 前面——搜 `melody` 时 `music-and-melody` 的 slug 也包含 `melody`。
///
/// 因此分三档：
///
/// - `0`：`slug` / `id` **等于** modid，本体（modid 与项目 slug 同名是常态）
/// - `1`：`slug` 以 `<modid>-` 开头，本体换了后缀命名。实测 `satin` 的
///   本体叫 `satin-api`，而首位是 `satin-free-wakes`；这两者按「包含」
///   同档时只能靠 `client_only` 分先后，于是选中了那个不提供 `satin`
///   的项目。带连字符的后缀是命名惯例（`-api` / `-lib` / `-core`），
///   与「名字里恰好含这个词」是两件事
/// - `2`：其余包含，最后才考虑
fn provider_hit_rank(hit: &ContentProject, term: &str) -> Option<u8> {
    let term = term.to_ascii_lowercase();
    let equals = |text: &str| text.to_ascii_lowercase() == term;
    let prefixed = |text: &str| {
        text.to_ascii_lowercase()
            .strip_prefix(&term)
            .is_some_and(|rest| rest.starts_with('-'))
    };
    let contains = |text: &str| text.to_ascii_lowercase().contains(&term);
    let slug = hit.slug.as_deref();
    if equals(&hit.id) || slug.is_some_and(equals) {
        Some(0)
    } else if slug.is_some_and(prefixed) {
        Some(1)
    } else if slug.is_some_and(contains) || contains(&hit.id) || contains(&hit.title) {
        Some(2)
    } else {
        None
    }
}

/// 按相关度排好全部候选，供逐个尝试
///
/// 排序键的顺序不能反：`melody` 本体被上游标成仅客户端，而排位更靠后的
/// `music-and-melody` 是「服务端可用」；若先按服务端可用挑，挑中的就是
/// 那个不提供 `melody` modid 的项目。名字与该 modid 一致的项目才是要补
/// 的本体，它的侧别由安装检查判，不由检索判。
///
/// 并列时按下载量降序：同档候选里更主流的那个更可能是本体。
fn rank_provider_hits(hits: Vec<ContentProject>, term: &str) -> Vec<ContentProject> {
    let mut ranked: Vec<(u8, ContentProject)> = hits
        .into_iter()
        .filter_map(|hit| provider_hit_rank(&hit, term).map(|rank| (rank, hit)))
        .collect();
    ranked.sort_by(|(left_rank, left), (right_rank, right)| {
        left_rank
            .cmp(right_rank)
            .then_with(|| left.client_only.cmp(&right.client_only))
            .then_with(|| right.downloads.cmp(&left.downloads))
    });
    ranked.into_iter().map(|(_, hit)| hit).collect()
}

/// 换名更新后删掉旧文件
///
/// 只在路径不同时动手：同名替换由 [`crate::atomic::replace_file`] 自己处理掉了。
/// 旧文件已经不存在不算失败——目标状态就是「旧的不在」，重复执行也应当成功。
fn remove_replaced(current: &Path, installed: &Path) -> Result<Option<PathBuf>, ModError> {
    if current == installed {
        return Ok(None);
    }
    match crate::fs::remove_file(current) {
        Ok(()) => Ok(Some(current.to_path_buf())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(FsError::io(current, err).into()),
    }
}

/// 按文件名或 modid 在已装文件里挑出目标
///
/// 文件名优先：用户从 `list` 里复制的就是文件名，精确命中不该再走 modid 匹配
/// （modid 可能命中多个同 id 的不同版本）。
fn match_files(
    inspector: &dyn ModInspector,
    jars: &[PathBuf],
    selector: &str,
    dir: &Path,
) -> Result<Vec<PathBuf>, ModError> {
    let wanted = selector.trim();
    let not_found = || ModError::NotFound {
        selector: selector.to_owned(),
        dir: dir.to_path_buf(),
    };
    if wanted.is_empty() {
        return Err(not_found());
    }

    let by_name: Vec<PathBuf> = jars
        .iter()
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.eq_ignore_ascii_case(wanted)
                    || name
                        .strip_suffix(".jar")
                        .is_some_and(|stem| stem.eq_ignore_ascii_case(wanted))
            })
        })
        .cloned()
        .collect();
    if !by_name.is_empty() {
        return Ok(by_name);
    }

    let mut hits = Vec::new();
    for path in jars {
        let Ok(capability) = inspector.inspect(path) else {
            continue;
        };
        if capability
            .provided_ids()
            .iter()
            .any(|id| id.eq_ignore_ascii_case(wanted))
        {
            hits.push(path.clone());
        }
    }
    match hits.len() {
        0 => Err(not_found()),
        1 => Ok(hits),
        _ => Err(ModError::Ambiguous {
            selector: selector.to_owned(),
            candidates: hits
                .iter()
                .filter_map(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::mod_::ModCapability;
    use port::mod_::ModInspectError;

    /// 按路径返回预设结论的检查器
    struct FakeInspector(Vec<ModCapability>);

    impl ModInspector for FakeInspector {
        fn inspect(&self, path: &Path) -> Result<ModCapability, ModInspectError> {
            self.0
                .iter()
                .find(|capability| capability.path == path.display().to_string())
                .cloned()
                .ok_or_else(|| ModInspectError::NotAnArchive {
                    path: path.to_path_buf(),
                })
        }
    }

    fn cap(path: &str, id: &str) -> ModCapability {
        let mut capability = ModCapability::empty(path);
        capability.id = Some(id.to_owned());
        capability.loaders.push(ModLoader::Fabric);
        capability
    }

    #[test]
    fn selector_prefers_exact_file_name() {
        let dir = Path::new("/srv/mods");
        let jars = vec![
            PathBuf::from("/srv/mods/jei.jar"),
            PathBuf::from("/srv/mods/other.jar"),
        ];
        let inspector = FakeInspector(vec![cap("/srv/mods/jei.jar", "jei")]);

        let hits = match_files(&inspector, &jars, "jei.jar", dir).expect("hit");
        assert_eq!(hits, vec![PathBuf::from("/srv/mods/jei.jar")]);

        // 省略 `.jar` 也算文件名命中，不必退到 modid
        let hits = match_files(&inspector, &jars, "JEI", dir).expect("hit");
        assert_eq!(hits, vec![PathBuf::from("/srv/mods/jei.jar")]);
    }

    #[test]
    fn selector_falls_back_to_modid() {
        let dir = Path::new("/srv/mods");
        let jars = vec![PathBuf::from("/srv/mods/abc-1.0.jar")];
        let inspector = FakeInspector(vec![cap("/srv/mods/abc-1.0.jar", "abc")]);

        let hits = match_files(&inspector, &jars, "abc", dir).expect("hit");
        assert_eq!(hits, vec![PathBuf::from("/srv/mods/abc-1.0.jar")]);
    }

    /// 同一个 modid 命中两个文件时不猜：交给用户重选
    #[test]
    fn selector_reports_ambiguity() {
        let dir = Path::new("/srv/mods");
        let jars = vec![
            PathBuf::from("/srv/mods/abc-1.0.jar"),
            PathBuf::from("/srv/mods/abc-1.1.jar"),
        ];
        let inspector = FakeInspector(vec![
            cap("/srv/mods/abc-1.0.jar", "abc"),
            cap("/srv/mods/abc-1.1.jar", "abc"),
        ]);

        let err = match_files(&inspector, &jars, "abc", dir).expect_err("ambiguous");
        assert!(matches!(err, ModError::Ambiguous { .. }), "{err:?}");
        assert_eq!(err.category(), ErrorCategory::Usage);
    }

    #[test]
    fn selector_without_hits_is_not_found() {
        let dir = Path::new("/srv/mods");
        let jars = vec![PathBuf::from("/srv/mods/abc-1.0.jar")];
        let inspector = FakeInspector(vec![cap("/srv/mods/abc-1.0.jar", "abc")]);
        let err = match_files(&inspector, &jars, "zzz", dir).expect_err("missing");
        assert!(matches!(err, ModError::NotFound { .. }), "{err:?}");
        assert_eq!(err.category(), ErrorCategory::NotFound);
    }

    #[test]
    fn loader_maps_known_variants_only() {
        assert_eq!(loader_of(&ServerVariant::Fabric), Some(ModLoader::Fabric));
        assert_eq!(loader_of(&ServerVariant::Vanilla), None);
    }

    /// 带版本尾巴的 modid 要补一个去尾词：上游搜 `geckolib3` 搜不到 `geckolib`
    #[test]
    fn provider_terms_strip_trailing_version_digits() {
        assert_eq!(
            provider_search_terms("geckolib3"),
            vec!["geckolib3".to_owned(), "geckolib".to_owned()]
        );
        // 正常命名不受影响
        assert_eq!(
            provider_search_terms("fabric-api"),
            vec!["fabric-api".to_owned()]
        );
        // 全是数字的 modid 不剥（剥完就空了）
        assert_eq!(provider_search_terms("42"), vec!["42".to_owned()]);
    }

    /// 命中项目必须与检索词相关：slug/id/标题都不含词的照单不收
    #[test]
    fn provider_hit_requires_name_relevance() {
        let hit = |id: &str, title: &str| ContentProject {
            id: id.to_owned(),
            slug: Some(id.to_owned()),
            title: title.to_owned(),
            description: String::new(),
            kind: ContentKind::Mod,
            also_kinds: Vec::new(),
            loaders: Vec::new(),
            game_versions: Vec::new(),
            author: None,
            downloads: 0,
            icon_url: None,
            client_only: false,
        };
        // cupboard 搜出的无关项目不收（实测反例）
        assert_eq!(
            provider_hit_rank(&hit("aesthetic-storage", "Aesthetic Storage"), "cupboard"),
            None
        );
        // slug 含词的收（geckolib 现场）
        assert!(provider_hit_rank(&hit("geckolib", "GeckoLib"), "geckolib").is_some());
        // 名字只是包含检索词的排最后（melody 现场）
        assert_eq!(
            provider_hit_rank(&hit("music-and-melody", "Music and Melody"), "melody"),
            Some(2)
        );
        assert_eq!(
            provider_hit_rank(&hit("melody", "Melody"), "melody"),
            Some(0)
        );
        // 本体带后缀：`satin` 的真身是 `satin-api`，排在纯包含之前
        assert_eq!(
            provider_hit_rank(&hit("satin-api", "Satin API"), "satin"),
            Some(1)
        );
        assert_eq!(
            provider_hit_rank(&hit("satin-free-wakes", "Satin Free Wakes"), "satin"),
            Some(1)
        );
    }

    /// 相等命中优先于「服务端可用」：melody 现场
    ///
    /// 本体被上游标成仅客户端，名字里含该词的近似项目反而标着服务端可用。
    /// 若按服务端可用先挑，挑中的是不提供该 modid 的那个项目，补装必然失败。
    #[test]
    fn provider_pick_prefers_exact_slug_over_server_usable() {
        let hit = |id: &str, client_only: bool| ContentProject {
            id: id.to_owned(),
            slug: Some(id.to_owned()),
            title: id.to_owned(),
            description: String::new(),
            kind: ContentKind::Mod,
            also_kinds: Vec::new(),
            loaders: Vec::new(),
            game_versions: Vec::new(),
            author: None,
            downloads: 0,
            icon_url: None,
            client_only,
        };
        let picked = rank_provider_hits(
            vec![hit("music-and-melody", false), hit("melody", true)],
            "melody",
        );
        assert_eq!(picked.first().expect("provider").id, "melody");
    }

    /// `satin` 现场：候选全部返回，由安装时的 modid 校验逐个筛
    ///
    /// 上游搜 `satin` 的首位是 `satin-free-wakes`（modid 是 `wakes`），
    /// 真正的 `satin-api` 排第二。只取首位会让补装被 `expect_modid`
    /// 拒掉，然后整条依赖被误报成「上游找不到提供方」。
    #[test]
    fn provider_rank_keeps_every_candidate() {
        let hit = |id: &str, client_only: bool, downloads: u64| ContentProject {
            id: id.to_owned(),
            slug: Some(id.to_owned()),
            title: id.to_owned(),
            description: String::new(),
            kind: ContentKind::Mod,
            also_kinds: Vec::new(),
            loaders: Vec::new(),
            game_versions: Vec::new(),
            author: None,
            downloads,
            icon_url: None,
            client_only,
        };
        let ranked = rank_provider_hits(
            vec![
                hit("satin-free-wakes", true, 500),
                hit("satin-api", true, 1_800_000),
            ],
            "satin",
        );
        let ids: Vec<&str> = ranked.iter().map(|hit| hit.id.as_str()).collect();
        assert_eq!(ids, vec!["satin-api", "satin-free-wakes"], "下载量高的排前");
    }

    /// 记录的 Java 优先；没有记录时按 MC 版本推算，与启动判定同源
    #[test]
    fn target_java_prefers_recorded_then_falls_back_to_mc() {
        let recorded = domain::java::JavaMajor::parse(17).expect("17");
        assert_eq!(target_java(Some(recorded), "1.21.1"), Some(17));
        assert_eq!(target_java(None, "1.21.1"), Some(21));
        // 推不出的版本不给结论，绝不猜一个
        assert_eq!(target_java(None, "24w14a"), None);
    }

    /// 搜索条件要带上这台服务端的加载器与游戏版本，且类别固定为用户要装的类别
    #[test]
    fn server_query_pins_server_conditions() {
        let query = server_query(
            &ServerVariant::Fabric,
            "1.21.1",
            ContentKind::Mod,
            &ContentQuery::new().keyword("sodium").page(20, 20),
        );
        assert_eq!(query.kinds, vec![ContentKind::Mod], "类别固定为要装的类别");
        assert_eq!(
            query.loaders,
            vec!["fabric".to_owned()],
            "加载器取自服务端变体"
        );
        assert_eq!(
            query.game_versions,
            vec!["1.21.1".to_owned()],
            "游戏版本取自服务端"
        );
        assert_eq!(query.keyword.as_deref(), Some("sodium"), "关键词原样保留");
        assert_eq!(query.offset, 20, "分页原样保留");
        assert_eq!(query.limit, 20);
        assert!(
            query.server_usable_only,
            "仅客户端的项目不能出现在服务端安装列表里"
        );
    }

    /// 数据包不看加载器：搜索不按加载器过滤，但游戏版本仍限定
    #[test]
    fn server_query_skips_loader_for_datapack() {
        let query = server_query(
            &ServerVariant::Fabric,
            "1.21.1",
            ContentKind::DataPack,
            &ContentQuery::new(),
        );
        assert_eq!(query.kinds, vec![ContentKind::DataPack]);
        assert!(query.loaders.is_empty(), "数据包不过滤加载器");
        assert_eq!(query.game_versions, vec!["1.21.1".to_owned()]);
    }
}
