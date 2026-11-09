//! 服务端 mod 编排：列表 / 安装 / 卸载 / 运行中拒绝
//!
//! 测的是 [`service::ModService`] 的编排，不是某个实现：来源、下载器、检查器、
//! 执行位置全部是可控替身，磁盘是真的（`testkit::ServiceFixture` 的真实目录）。
//! 因此能断言的正是最要紧的几件事：
//!
//! - 落点必须是 `<server>/mods/`，且下载只经暂存区，暂存区用完就没了
//! - 检查判定不可用于服务端时不落盘（`--force` 才落，且照样带风险）
//! - 服务端运行中一律拒绝改动（不猜、不等）
//! - 卸载按文件名优先、modid 兜底；modid 撞车时不猜
//!
//! 联网相关的路径（识别、查候选更新）由 `provider` 的回环 mock 覆盖，这里不
//! 重复造上游。

#![allow(clippy::expect_used)]
#![allow(
    clippy::unreachable,
    reason = "测试替身用 unreachable! 标记不该被调用的分支"
)]

use async_trait::async_trait;
use domain::mod_::{ModCapability, ModLoader, SideSupport, SideVerdict};
use domain::server::content::{
    ContentCandidate, ContentIdentity, ContentKind, ContentRequest, UpdateDecision,
};
use domain::server::version::ServerVariant;
use port::content::{
    ContentFile, ContentProject, ContentQuery, ContentSearch, ContentSource, ContentSourceInfo,
    ContentSourceRegistry, ResolvedContent,
};
use port::mod_::{ModInspectError, ModInspector};
use port::{
    ArtifactDownloader, Backend, CancellationToken, DownloadError, DownloadRequest,
    DownloadedArtifact, InstallerError, InstallerInvocation, InstallerOutputSink, InstallerRunner,
    ProgressSink, SourceError, SupervisorPort,
};
use service::{
    ApplyOptions, ContentService, InstallSpec, ListOptions, ModError, ModService, ServerRegistry,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use testkit::{CommandLog, FakeSupervisor, MemServerRepo, ServiceFixture};

// ---------------------------------------------------------------- 替身

/// 假来源的可变部分：用例在调用前改，替身本身只读
struct SourceConfig {
    /// `candidates` 的返回值；空 = 上游没有兼容版本
    candidates: Vec<ContentCandidate>,
    /// `resolve` 能解析出的文件（按项目 + 版本 ID 查）
    resolved: Vec<ResolvedContent>,
    /// `by_hash` 的结论，按哈希出现顺序依次取；空 = 上游不认识任何哈希
    identities: Vec<IdentityPreset>,
    /// 收到的候选请求，供断言过滤条件是否下发
    seen: Vec<ContentRequest>,
    /// `by_hash` 收到过的哈希
    hashed: Vec<String>,
    /// 全部可搜索的项目（替身按 `server_usable_only` 模拟上游过滤）
    search_pool: Vec<ContentProject>,
}

/// 「这个哈希属于某项目的某个版本」
#[derive(Clone)]
struct IdentityPreset {
    project: String,
    version_id: String,
    version_number: String,
}

/// 假来源：候选、解析结果与反查结论都由用例给定
struct FakeSource {
    config: Mutex<SourceConfig>,
}

impl FakeSource {
    fn new() -> Self {
        Self {
            config: Mutex::new(SourceConfig {
                candidates: Vec::new(),
                resolved: Vec::new(),
                identities: Vec::new(),
                seen: Vec::new(),
                hashed: Vec::new(),
                search_pool: Vec::new(),
            }),
        }
    }

    fn configure(&self, edit: impl FnOnce(&mut SourceConfig)) {
        edit(&mut self.config.lock().expect("source config"));
    }
}

impl ContentSourceInfo for FakeSource {
    fn name(&self) -> &'static str {
        "fake"
    }
}

#[async_trait]
impl ContentSource for FakeSource {
    fn supported_kinds(&self) -> Vec<ContentKind> {
        ContentKind::ALL.to_vec()
    }

    async fn search(&self, query: &ContentQuery) -> Result<ContentSearch, SourceError> {
        let config = self.config.lock().expect("source config");
        // 模拟上游 facet 行为：`server_usable_only` 时只返回非仅客户端项目
        let hits: Vec<ContentProject> = config
            .search_pool
            .iter()
            .filter(|hit| !query.server_usable_only || !hit.client_only)
            .cloned()
            .collect();
        Ok(ContentSearch {
            total: hits.len() as u64,
            offset: 0,
            hits,
        })
    }

    async fn project(&self, id: &str) -> Result<ContentProject, SourceError> {
        Ok(ContentProject {
            id: id.to_owned(),
            slug: Some(id.to_owned()),
            title: id.to_owned(),
            description: String::new(),
            kind: ContentKind::Mod,
            also_kinds: Vec::new(),
            loaders: vec!["fabric".to_owned()],
            game_versions: vec!["1.21.1".to_owned()],
            author: None,
            downloads: 0,
            icon_url: None,
            client_only: false,
        })
    }

    async fn candidates(
        &self,
        _id: &str,
        request: &ContentRequest,
    ) -> Result<Vec<ContentCandidate>, SourceError> {
        let mut config = self.config.lock().expect("source config");
        config.seen.push(request.clone());
        Ok(config.candidates.clone())
    }

    async fn resolve(&self, id: &str, version: &str) -> Result<ResolvedContent, SourceError> {
        self.config
            .lock()
            .expect("source config")
            .resolved
            .iter()
            .find(|resolved| resolved.project_id == id && resolved.version_id == version)
            .cloned()
            .ok_or(SourceError::Status { status: 404 })
    }

    async fn by_hash(&self, hashes: &[String]) -> Result<Vec<ContentIdentity>, SourceError> {
        let mut config = self.config.lock().expect("source config");
        config.hashed.extend(hashes.iter().cloned());
        // 按哈希出现顺序逐个取预置结论：顺序来自 `collect_jars` 的文件名排序，
        // 因此用例只要安排「第一个文件属于 A、第二个属于 B」即可
        Ok(hashes
            .iter()
            .zip(config.identities.iter())
            .map(|(hash, preset)| ContentIdentity {
                hash: hash.clone(),
                project_id: preset.project.clone(),
                slug: Some(preset.project.clone()),
                title: Some(preset.project.clone()),
                version_id: preset.version_id.clone(),
                version_number: preset.version_number.clone(),
                channel: "release".to_owned(),
                // 已装版本比任何预置候选都旧：用例里的候选都代表「上游有更新」
                date_published: "2020-01-01T00:00:00Z".to_owned(),
                kind: Some(ContentKind::Mod),
            })
            .collect())
    }
}

/// 假下载器：按预置字节真的写到目标路径，可切成「被取消」或「某个文件必失败」
struct FakeDownloader {
    bytes: Vec<u8>,
    calls: Arc<AtomicUsize>,
    cancelled: std::sync::atomic::AtomicBool,
    /// 命中这个文件名的下载一律超时（模拟中途失败）
    fail_on: Mutex<Option<String>>,
}

impl FakeDownloader {
    fn new(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.to_vec(),
            calls: Arc::new(AtomicUsize::new(0)),
            cancelled: std::sync::atomic::AtomicBool::new(false),
            fail_on: Mutex::new(None),
        }
    }
}

#[async_trait]
impl ArtifactDownloader for FakeDownloader {
    async fn download(
        &self,
        request: &DownloadRequest,
        _progress: Option<&dyn ProgressSink>,
        _cancel: &CancellationToken,
    ) -> Result<DownloadedArtifact, DownloadError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(DownloadError::Cancelled);
        }
        let failing = self
            .fail_on
            .lock()
            .expect("fail_on")
            .as_deref()
            .is_some_and(|name| request.destination.ends_with(name));
        if failing {
            return Err(DownloadError::Timeout);
        }
        if let Some(parent) = request.destination.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&request.destination, &self.bytes).expect("write");
        Ok(DownloadedArtifact {
            path: request.destination.clone(),
            size: self.bytes.len() as u64,
        })
    }
}

/// 假检查器的一条结论
struct Verdict {
    file: String,
    side: SideSupport,
    /// 声明的必需依赖（modid + 生效侧，缺了就算缺失依赖）
    depends: Vec<(String, domain::mod_::DepSide)>,
}

/// 假检查器：按**文件名**给结论（暂存路径每次不同，不能按全路径匹配）
struct FakeInspector {
    verdicts: Mutex<Vec<Verdict>>,
    /// 文件名 → 归档内的类名（内部名），供崩溃归因反查
    classes: Mutex<BTreeMap<String, Vec<String>>>,
}

impl FakeInspector {
    fn new(verdicts: &[(&str, SideSupport)]) -> Self {
        Self {
            verdicts: Mutex::new(
                verdicts
                    .iter()
                    .map(|(name, side)| Verdict {
                        file: (*name).to_owned(),
                        side: *side,
                        depends: Vec::new(),
                    })
                    .collect(),
            ),
            classes: Mutex::new(BTreeMap::new()),
        }
    }

    /// 声明某个文件包含哪些类（内部名）
    fn declares_classes(&self, file: &str, classes: &[&str]) {
        self.classes.lock().expect("classes").insert(
            file.to_owned(),
            classes.iter().map(|class| (*class).to_owned()).collect(),
        );
    }

    /// 给某个文件追加一条必需依赖（用于「缺依赖」判定）
    fn declare_dep(&self, file: &str, dep: &str) {
        self.declare_dep_with_side(file, dep, domain::mod_::DepSide::Both);
    }

    /// 同上，但指定这条依赖在哪一侧生效
    fn declare_dep_with_side(&self, file: &str, dep: &str, side: domain::mod_::DepSide) {
        let mut verdicts = self.verdicts.lock().expect("verdicts");
        if let Some(verdict) = verdicts.iter_mut().find(|verdict| verdict.file == file) {
            verdict.depends.push((dep.to_owned(), side));
        }
    }
}

impl ModInspector for FakeInspector {
    fn inspect(&self, path: &Path) -> Result<ModCapability, ModInspectError> {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let verdicts = self.verdicts.lock().expect("verdicts");
        let Some(verdict) = verdicts.iter().find(|verdict| verdict.file == name) else {
            return Err(ModInspectError::NotAnArchive {
                path: path.to_path_buf(),
            });
        };
        let stem = name.trim_end_matches(".jar").to_owned();
        let mut capability = ModCapability::empty(path.display().to_string());
        capability.id = Some(stem);
        capability.version = Some("1.0.0".to_owned());
        capability.loaders.push(ModLoader::Fabric);
        capability.side = SideVerdict::declared(SideSupport::Required, verdict.side);
        for (dep, dep_side) in &verdict.depends {
            capability.depends.push(domain::mod_::ModDependency {
                id: dep.clone(),
                version_range: String::new(),
                mandatory: true,
                loader: Some(ModLoader::Fabric),
                side: *dep_side,
                unless: Vec::new(),
            });
        }
        Ok(capability)
    }

    fn class_names(&self, path: &Path) -> Result<Option<Vec<String>>, ModInspectError> {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let classes = self.classes.lock().expect("classes");
        Ok(Some(classes.get(&name).cloned().unwrap_or_default()))
    }
}

/// 假执行位置：只关心进程生命周期
struct FakeBackend {
    supervisor: FakeSupervisor,
}

impl Backend for FakeBackend {
    fn supervisor(&self) -> &dyn SupervisorPort {
        &self.supervisor
    }

    fn installer(&self) -> &dyn InstallerRunner {
        &NoopInstaller
    }
}

struct NoopInstaller;

impl InstallerRunner for NoopInstaller {
    fn run_installer(
        &self,
        _invocation: &InstallerInvocation<'_>,
        _output: Option<&dyn InstallerOutputSink>,
        _cancel: &CancellationToken,
    ) -> Result<(), InstallerError> {
        unreachable!("mod 相关用例不跑安装器")
    }
}

// ---------------------------------------------------------------- 装配

/// 一台 Fabric 服务端 + 一整套替身
struct Harness {
    fixture: ServiceFixture,
    mods: ModService,
    downloader: Arc<FakeDownloader>,
    source: Arc<FakeSource>,
    inspector: Arc<FakeInspector>,
}

impl Harness {
    fn new(log: CommandLog, verdicts: &[(&str, SideSupport)], bytes: &[u8]) -> Self {
        let fixture = ServiceFixture::new();
        let repo = Arc::new(MemServerRepo::new());
        let mut meta = testkit::server_meta("alpha", fixture.workspace().id.as_str());
        meta.variant = ServerVariant::Fabric;
        repo.insert(meta);
        let dir = fixture.root().join("servers").join("alpha");
        std::fs::create_dir_all(&dir).expect("mkdir server");

        let registry = Arc::new(ServerRegistry::new(
            Arc::clone(&repo) as Arc<dyn port::ServerRepository>,
            fixture.workspace_repo(),
        ));
        let downloader = Arc::new(FakeDownloader::new(bytes));
        let source = Arc::new(FakeSource::new());
        source.configure(|config| {
            config.candidates = vec![candidate("v2")];
            config.resolved = vec![resolved("v2", "cool-mod.jar", bytes.len() as u64)];
        });
        let content = ContentService::new(
            Arc::new(ContentSourceRegistry::new(vec![
                Arc::clone(&source) as Arc<dyn ContentSource>
            ])),
            Arc::clone(&downloader) as Arc<dyn ArtifactDownloader>,
        );
        let inspector = Arc::new(FakeInspector::new(verdicts));
        let mods = ModService::new(
            registry,
            Arc::new(FakeBackend {
                supervisor: FakeSupervisor::new(log),
            }),
            content,
            Arc::clone(&inspector) as Arc<dyn ModInspector>,
        );
        Self {
            fixture,
            mods,
            downloader,
            source,
            inspector,
        }
    }

    /// 让「已装文件」被上游识别为某项目的某个版本
    fn identify_as(&self, project: &str, version_id: &str, version_number: &str) {
        self.identify_each(&[(project, version_id, version_number)]);
    }

    /// 多个已装文件分别属于哪个项目的哪个版本（按文件名排序依次对应）
    fn identify_each(&self, presets: &[(&str, &str, &str)]) {
        let presets: Vec<IdentityPreset> = presets
            .iter()
            .map(|(project, version_id, version_number)| IdentityPreset {
                project: (*project).to_owned(),
                version_id: (*version_id).to_owned(),
                version_number: (*version_number).to_owned(),
            })
            .collect();
        self.source.configure(|config| config.identities = presets);
    }

    /// 上游候选版本
    fn set_candidates(&self, versions: &[&str]) {
        let versions: Vec<ContentCandidate> = versions.iter().map(|v| candidate(v)).collect();
        self.source.configure(|config| config.candidates = versions);
    }

    /// 上游解析结果：版本 ID → 文件名（项目记作 `cool-mod`）
    fn set_resolved(&self, files: &[(&str, &str)]) {
        self.set_resolved_for("cool-mod", files);
    }

    /// 上游解析结果：某个项目的版本 ID → 文件名
    fn set_resolved_for(&self, project: &str, files: &[(&str, &str)]) {
        let size = self.downloader.bytes.len() as u64;
        let files: Vec<ResolvedContent> = files
            .iter()
            .map(|(version, name)| resolved_for(project, version, name, size))
            .collect();
        self.source.configure(|config| config.resolved = files);
    }

    /// 追加解析结果（多个项目各自可解析时用）
    fn add_resolved_for(&self, project: &str, files: &[(&str, &str)]) {
        let size = self.downloader.bytes.len() as u64;
        self.source.configure(|config| {
            for (version, name) in files {
                config
                    .resolved
                    .push(resolved_for(project, version, name, size));
            }
        });
    }

    fn cancel_downloads(&self) {
        self.downloader.cancelled.store(true, Ordering::SeqCst);
    }

    /// 让某个文件名的下载失败（模拟中途超时）
    fn fail_download_of(&self, file: &str) {
        *self.downloader.fail_on.lock().expect("fail_on") = Some(file.to_owned());
    }

    fn server_dir(&self) -> PathBuf {
        self.fixture.root().join("servers").join("alpha")
    }

    fn mods_dir(&self) -> PathBuf {
        self.server_dir().join("mods")
    }

    fn staging_entries(&self) -> usize {
        std::fs::read_dir(self.server_dir().join(".mcsup-staging")).map_or(0, Iterator::count)
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.server_dir().join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("write");
    }
}

fn candidate(version: &str) -> ContentCandidate {
    ContentCandidate {
        version_id: version.to_owned(),
        version_number: version.to_owned(),
        channel: "release".to_owned(),
        featured: false,
        loaders: vec!["fabric".to_owned()],
        game_versions: vec!["1.21.1".to_owned()],
        date_published: "2024-01-01T00:00:00Z".to_owned(),
        score: 0,
    }
}

fn resolved(version: &str, filename: &str, size: u64) -> ResolvedContent {
    resolved_for("cool-mod", version, filename, size)
}

fn resolved_for(project: &str, version: &str, filename: &str, size: u64) -> ResolvedContent {
    ResolvedContent {
        project_id: project.to_owned(),
        version_id: version.to_owned(),
        version_number: version.to_owned(),
        file: ContentFile {
            location: "https://cdn.example.invalid/file.jar".parse().expect("url"),
            filename: filename.to_owned(),
            size,
            checksum: None,
        },
    }
}

fn install_spec() -> InstallSpec<'static> {
    InstallSpec {
        version: None,
        source: None,
        allow_prerelease: false,
    }
}

fn search_hit(id: &str, client_only: bool) -> ContentProject {
    ContentProject {
        id: id.to_owned(),
        slug: Some(id.to_owned()),
        title: id.to_owned(),
        description: String::new(),
        kind: ContentKind::Mod,
        also_kinds: Vec::new(),
        loaders: vec!["fabric".to_owned()],
        game_versions: vec!["1.21.1".to_owned()],
        author: None,
        downloads: 0,
        icon_url: None,
        client_only,
    }
}

/// 服务端检索为空时带回落选的仅客户端项目（satin 现场的基础）
///
/// 上游标注本身有误标：`environment: "client"` 的 API 库可能服务端照样装载
/// （satin 是一众服务端 mod 的必需依赖）。过滤后为空不等于「没有这个东西」，
/// 把仅客户端的条目带回来并标记，选择权交还用户。
#[tokio::test]
async fn empty_filtered_search_falls_back_to_client_only_hits() {
    let harness = Harness::new(CommandLog::stopped(), &[], &[b'x'; 4]);
    harness.source.configure(|config| {
        config.search_pool = vec![
            search_hit("satin", true),
            search_hit("some-client-mod", true),
        ];
    });

    let found = harness
        .mods
        .search(
            "alpha",
            ContentKind::Mod,
            &ContentQuery::new().keyword("satin"),
            None,
        )
        .await
        .expect("search");
    assert_eq!(found.hits.len(), 2, "{:?}", found.hits);
    assert!(found.hits.iter().all(|hit| hit.client_only));
    assert!(found.hits.iter().any(|hit| hit.id == "satin"));
}

/// 服务器可用的结果存在时不放宽：正常列表不带仅客户端噪音
#[tokio::test]
async fn nonempty_filtered_search_keeps_client_only_excluded() {
    let harness = Harness::new(CommandLog::stopped(), &[], &[b'x'; 4]);
    harness.source.configure(|config| {
        config.search_pool = vec![search_hit("satin", true), search_hit("fabric-api", false)];
    });

    let found = harness
        .mods
        .search(
            "alpha",
            ContentKind::Mod,
            &ContentQuery::new().keyword("api"),
            None,
        )
        .await
        .expect("search");
    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.hits[0].id, "fabric-api");
    assert!(!found.hits[0].client_only);
}

/// 修复计划：缺失依赖找到提供方 → 规划补装
#[tokio::test]
async fn plan_fix_installs_found_dependency() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("cool-mod.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("cool-mod.jar", "some-lib");
    harness.write("mods/cool-mod.jar", b"cool");
    harness.source.configure(|config| {
        config.search_pool = vec![search_hit("some-lib", false)];
    });
    // 提供方要能解析出文件供补装
    harness.set_resolved_for("some-lib", &[("v1", "some-lib.jar")]);

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    assert_eq!(plan.installs.len(), 1, "{plan:?}");
    assert_eq!(plan.installs[0].missing, "some-lib");
    assert_eq!(plan.installs[0].project_id, "some-lib");
    assert!(plan.unresolved.is_empty());
    assert!(plan.removals.is_empty());
}

/// 提供方搜不到：不猜，依赖进 unresolved
#[tokio::test]
async fn plan_fix_reports_unresolved_when_no_provider() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("cool-mod.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("cool-mod.jar", "obscure-lib");
    harness.write("mods/cool-mod.jar", b"cool");

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    assert!(plan.installs.is_empty());
    assert_eq!(plan.unresolved, vec!["obscure-lib".to_owned()]);
}

/// 依赖带版本尾巴（geckolib3）而上游项目名不带（geckolib）：
/// 原词落空后去尾重搜，仍能找到提供方
#[tokio::test]
async fn plan_fix_strips_trailing_digits_when_searching_provider() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("cool-mod.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("cool-mod.jar", "geckolib3");
    harness.write("mods/cool-mod.jar", b"cool");
    harness.source.configure(|config| {
        // 模拟上游：搜「geckolib3」没有本体，搜「geckolib」才有
        config.search_pool = vec![search_hit("geckolib", false)];
        config.candidates = vec![candidate("v1")];
    });
    harness.set_resolved_for("geckolib", &[("v1", "geckolib.jar")]);

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    assert_eq!(plan.installs.len(), 1, "{plan:?}");
    assert_eq!(plan.installs[0].missing, "geckolib3");
    assert_eq!(plan.installs[0].project_id, "geckolib");
}

/// 提供方被上游标成仅客户端时仍要能补上：库类 mod 的标注经常有误
///
/// 现场：`fancymenu` 要求 `melody >=1.0.17`。`melody` 在 Modrinth 上确实
/// 存在且 `26.3` 有版本，jar 内 `environment: "*"`、带 `main` 入口，服务端
/// 装载正常，但上游把项目标成 `server_side: unsupported`。按该标注过滤后
/// 它整条从检索结果里消失，只剩 slug 里含 `melody` 的 `music-and-melody`，
/// 补装被 `expect_modid` 拒，最后报成「上游找不到提供方」——用户看到的
/// 结论是「这个 mod 不存在」，而它就在那儿。
#[tokio::test]
async fn plan_fix_finds_client_only_library_provider() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("fancymenu.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("fancymenu.jar", "melody");
    harness.write("mods/fancymenu.jar", b"fm");
    harness.source.configure(|config| {
        // 上游把本体标成仅客户端；同名相近的那个服务端可用但不是本体
        config.search_pool = vec![
            search_hit("music-and-melody", false),
            search_hit("melody", true),
        ];
    });
    harness.set_resolved_for("melody", &[("v1", "melody.jar")]);

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    // 候选全部进计划，第一个必须是本体：安装时逐个按 modid 校验，
    // 名字相近的那个排在后面当备份
    assert_eq!(plan.installs.len(), 2, "{plan:?}");
    assert_eq!(plan.installs[0].missing, "melody");
    assert_eq!(plan.installs[0].project_id, "melody");
    assert_eq!(plan.installs[1].project_id, "music-and-melody");
    assert!(plan.unresolved.is_empty(), "{plan:?}");
}

/// 名字只是包含检索词的项目不算本体：相等优先
///
/// 只做子串匹配时，`melody` 会命中 `music-and-melody`；那个项目的 jar 提供
/// 的是 `music_and_melody`，补装必然被拒。本体应当按 `slug` / `id` 相等挑。
#[tokio::test]
async fn plan_fix_prefers_exact_slug_over_substring_hit() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("fancymenu.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("fancymenu.jar", "melody");
    harness.write("mods/fancymenu.jar", b"fm");
    harness.source.configure(|config| {
        // 顺序模拟上游按相关度排序：下载量更高的近似名排在前面
        config.search_pool = vec![
            search_hit("music-and-melody", false),
            search_hit("melody", true),
        ];
    });
    harness.set_resolved_for("melody", &[("v1", "melody.jar")]);

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    assert_eq!(plan.installs.len(), 2, "{plan:?}");
    assert_eq!(
        plan.installs[0].project_id, "melody",
        "应当把名字与 modid 完全一致的项目排在前面，而不是名字里包含它的项目"
    );
    assert_eq!(
        plan.installs[1].project_id, "music-and-melody",
        "不提供该 modid 的项目留作备选，安装时会被逐个校验筛掉"
    );
}

/// 同一个依赖被多个文件缺：计划里只出现一次，不重复刷屏
#[tokio::test]
async fn plan_fix_dedupes_the_same_missing_dependency() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("one.jar", SideSupport::Required),
            ("two.jar", SideSupport::Required),
            ("three.jar", SideSupport::Required),
        ],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("one.jar", "geckolib3");
    harness.inspector.declare_dep("two.jar", "geckolib3");
    harness.inspector.declare_dep("three.jar", "geckolib3");
    harness.write("mods/one.jar", b"1");
    harness.write("mods/two.jar", b"2");
    harness.write("mods/three.jar", b"3");

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    assert!(plan.installs.is_empty());
    assert_eq!(plan.unresolved, vec!["geckolib3".to_owned()]);
}

// ---------------------------------------------------------------- 崩溃归因

/// 现场日志：CalcMod 在服务端注册命令时碰到客户端类
///
/// 这个 mod 的 `fabric.mod.json` 是 `environment: "*"` 且 `client` / `server`
/// 两个入口点都有，上游标 `server_side: optional`——元数据层面毫无破绽，
/// 三层静态判定全都放行，只有真的启动崩一次才能发现。
const CALCMOD_CRASH_LOG: &str = r"
[10:13:47] [main/WARN]: Failed to load datapacks, can't proceed with server load.
java.util.concurrent.ExecutionException: java.lang.NoClassDefFoundError: net/minecraft/client/player/LocalPlayer
	at java.base/java.util.concurrent.CompletableFuture.wrapInExecutionException(CompletableFuture.java:345)
	at knot//net.minecraft.server.Main.main(Main.java:193)
Caused by: java.lang.NoClassDefFoundError: net/minecraft/client/player/LocalPlayer
	at knot//net.jsa2025.calcmod.commands.CalcCommand.registerServer(CalcCommand.java:89)
	at knot//net.fabricmc.fabric.api.command.v2.CommandRegistrationCallback.lambda$static$1(CommandRegistrationCallback.java:43)
	at knot//net.minecraft.commands.Commands.<init>(Commands.java:339)
	... 14 more
Caused by: java.lang.ClassNotFoundException: net.minecraft.client.player.LocalPlayer
";

/// 崩溃栈能落到具体文件：规划移除该 jar，并带上缺失类作为理由
#[tokio::test]
async fn plan_fix_from_log_removes_the_jar_the_stack_points_at() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("calcmod.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    harness.write("mods/calcmod.jar", b"payload");
    harness
        .inspector
        .declares_classes("calcmod.jar", &["net/jsa2025/calcmod/commands/CalcCommand"]);
    harness.write("console.log", CALCMOD_CRASH_LOG.as_bytes());

    let plan = harness
        .mods
        .plan_fix_from_log("alpha", ContentKind::Mod, 500)
        .await
        .expect("plan_fix_from_log");

    assert_eq!(plan.removals.len(), 1, "{plan:?}");
    assert_eq!(
        plan.removals[0].file,
        harness.mods_dir().join("calcmod.jar"),
        "{plan:?}"
    );
    // 原因里要能看出「谁引用了什么」，用户才能自己核对结论
    assert!(
        plan.removals[0].reason.contains("CalcCommand"),
        "{:?}",
        plan.removals[0].reason
    );
    assert!(
        plan.removals[0].reason.contains("LocalPlayer"),
        "{:?}",
        plan.removals[0].reason
    );
    let crash = plan.crash.as_ref().expect("归因结果");
    assert_eq!(
        crash.missing,
        vec!["net/minecraft/client/player/LocalPlayer"]
    );
    assert!(plan.unattributed.is_empty(), "{plan:?}");
}

/// 栈里的类不属于任何文件：如实列进 unattributed，不猜文件
#[tokio::test]
async fn plan_fix_from_log_reports_unattributed_classes() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("innocent.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    harness.write("mods/innocent.jar", b"payload");
    harness
        .inspector
        .declares_classes("innocent.jar", &["com/example/innocent/Main"]);
    harness.write("console.log", CALCMOD_CRASH_LOG.as_bytes());

    let plan = harness
        .mods
        .plan_fix_from_log("alpha", ContentKind::Mod, 500)
        .await
        .expect("plan_fix_from_log");

    assert!(plan.removals.is_empty(), "{plan:?}");
    assert_eq!(
        plan.unattributed,
        vec!["net.jsa2025.calcmod.commands.CalcCommand".to_owned()]
    );
}

/// 日志里没有崩溃栈（没崩过、日志被清空）：空计划，不误伤任何文件
#[tokio::test]
async fn plan_fix_from_log_without_stack_is_empty() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("innocent.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    harness.write("mods/innocent.jar", b"payload");
    harness
        .inspector
        .declares_classes("innocent.jar", &["com/example/innocent/Main"]);
    harness.write(
        "console.log",
        b"[10:00:00] [main/INFO]: Done (12.3s)! For help, type \"help\"\n",
    );

    let plan = harness
        .mods
        .plan_fix_from_log("alpha", ContentKind::Mod, 500)
        .await
        .expect("plan_fix_from_log");

    assert!(plan.removals.is_empty(), "{plan:?}");
    assert!(plan.unattributed.is_empty(), "{plan:?}");
    assert!(
        plan.crash
            .as_ref()
            .is_some_and(domain::mod_::CrashAttribution::is_empty)
    );
}

/// 归因只看日志末尾：更早那次崩溃的栈不该被当成这次的证据
///
/// supervisor 以追加方式写 `console.log`，跨多次启动的栈会同时存在。
/// 取固定行数尾部既覆盖「最后一次启动」，又不会把历史问题翻出来。
#[tokio::test]
async fn plan_fix_from_log_reads_only_the_log_tail() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("old.jar", SideSupport::Required),
            ("new.jar", SideSupport::Required),
        ],
        b"jar-bytes",
    );
    harness.write("mods/old.jar", b"old");
    harness.write("mods/new.jar", b"new");
    harness
        .inspector
        .declares_classes("old.jar", &["com/old/Crash"]);
    harness
        .inspector
        .declares_classes("new.jar", &["com/new/Crash"]);
    // 旧崩溃在前，几十行填充隔开，最后一次崩溃在末尾
    let mut log = String::from(CALCMOD_CRASH_LOG);
    log = log.replace("net.jsa2025.calcmod.commands.CalcCommand", "com.old.Crash");
    for i in 0..40 {
        let _ = writeln!(log, "[10:00:{i:02}] [main/INFO]: tick {i}");
    }
    let mut tail =
        CALCMOD_CRASH_LOG.replace("net.jsa2025.calcmod.commands.CalcCommand", "com.new.Crash");
    if !tail.ends_with('\n') {
        tail.push('\n');
    }
    log.push_str(&tail);
    harness.write("console.log", log.as_bytes());

    // 尾部只留 12 行：旧崩溃在窗口外
    let plan = harness
        .mods
        .plan_fix_from_log("alpha", ContentKind::Mod, 12)
        .await
        .expect("plan_fix_from_log");

    assert_eq!(plan.removals.len(), 1, "{plan:?}");
    assert_eq!(plan.removals[0].file, harness.mods_dir().join("new.jar"));
}

/// 执行修复：按日志归因的移除同样进回收站，不直接删除
#[tokio::test]
async fn apply_fix_from_log_moves_the_blamed_jar_to_trash() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("calcmod.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    harness.write("mods/calcmod.jar", b"payload");
    harness
        .inspector
        .declares_classes("calcmod.jar", &["net/jsa2025/calcmod/commands/CalcCommand"]);
    harness.write("console.log", CALCMOD_CRASH_LOG.as_bytes());

    let plan = harness
        .mods
        .plan_fix_from_log("alpha", ContentKind::Mod, 500)
        .await
        .expect("plan_fix_from_log");
    let applied = harness
        .mods
        .apply_fix(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("apply_fix");

    assert_eq!(applied.removed.len(), 1, "{applied:?}");
    assert!(!harness.mods_dir().join("calcmod.jar").exists());
    let trashed = harness
        .server_dir()
        .join(service::TRASH_DIR)
        .join("calcmod.jar");
    assert_eq!(
        std::fs::read(&trashed).expect("回收站里应当有原文件"),
        b"payload"
    );
}

/// 执行修复：补装落盘
#[tokio::test]
async fn apply_fix_installs_the_missing_dependency() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("cool-mod.jar", SideSupport::Required),
            ("some-lib.jar", SideSupport::Required),
        ],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("cool-mod.jar", "some-lib");
    harness.write("mods/cool-mod.jar", b"cool");
    harness.source.configure(|config| {
        config.search_pool = vec![search_hit("some-lib", false)];
        config.candidates = vec![candidate("v1")];
    });
    harness.set_resolved_for("some-lib", &[("v1", "some-lib.jar")]);

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    let applied = harness
        .mods
        .apply_fix(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("apply_fix");
    assert_eq!(applied.installed.len(), 1, "{applied:?}");
    assert!(applied.refused.is_empty());
    // 补装后的文件在 mods/ 目录里
    assert!(harness.mods_dir().join("some-lib.jar").is_file());
}

/// 首个候选不提供该 modid 时自动改用下一个：`satin` 现场
///
/// 上游搜 `satin` 首位是 `satin-free-wakes`（modid 是 `wakes`），真正的
/// `satin-api` 排第二。只排第一个会让补装被 `expect_modid` 拒，然后整条
/// 依赖被误报成「上游找不到提供方」——提供方就在结果里。
#[tokio::test]
async fn apply_fix_tries_the_next_candidate_when_the_first_lacks_the_modid() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("environmentz.jar", SideSupport::Required),
            ("satin.jar", SideSupport::Required),
        ],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("environmentz.jar", "satin");
    harness.write("mods/environmentz.jar", b"env");
    harness.source.configure(|config| {
        config.search_pool = vec![
            search_hit("satin-free-wakes", true),
            search_hit("satin-api", true),
        ];
        config.candidates = vec![candidate("v1")];
    });
    // 只有 `satin-api` 的 jar 真的提供 modid `satin`
    harness.set_resolved_for("satin-free-wakes", &[("v1", "wakes.jar")]);
    harness.add_resolved_for("satin-api", &[("v1", "satin.jar")]);

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    assert!(plan.unresolved.is_empty(), "{plan:?}");

    let applied = harness
        .mods
        .apply_fix(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("apply_fix");
    assert_eq!(applied.installed.len(), 1, "{applied:?}");
    assert!(
        applied.unresolved.is_empty(),
        "候选还没试完就报「没有提供方」：{applied:?}"
    );
    assert!(harness.mods_dir().join("satin.jar").is_file());
}

/// 有整合包清单快照时，补依赖按快照里的项目 / 版本 ID 直装，不再搜索
///
/// 搜索是按名字猜：搜 `satin` 的首位命中是毫不相干的 `satin-free-wakes`。
/// 快照里记着「提供 satin 的是项目 fRbqPLg4、版本 Tq9qJzQz」，直接用它即可。
#[tokio::test]
async fn apply_fix_uses_manifest_snapshot_ids_instead_of_searching() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("environmentz.jar", SideSupport::Required),
            ("satin.jar", SideSupport::Required),
        ],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("environmentz.jar", "satin");
    harness.write("mods/environmentz.jar", b"env");
    // 上游检索结果里放一个错的项目：若实现仍在搜索，会挑中它并装错
    harness.source.configure(|config| {
        config.search_pool = vec![search_hit("satin-free-wakes", true)];
        config.candidates = vec![candidate("v1")];
    });
    harness.set_resolved_for("fRbqPLg4", &[("Tq9qJzQz", "satin.jar")]);

    // 写一份清单快照：satin 由项目 fRbqPLg4 的版本 Tq9qJzQz 提供
    let mut snapshot = service::ModpackManifestSnapshot {
        format: service::MANIFEST_FORMAT,
        source: "modrinth".to_owned(),
        project_id: "i4XHCd7Q".to_owned(),
        version_id: "kaxASXpi".to_owned(),
        version_number: "4.0.1".to_owned(),
        name: "Aged".to_owned(),
        pack_version: "4.0.1".to_owned(),
        minecraft: "1.21.1".to_owned(),
        loader: None,
        dependencies: BTreeMap::new(),
        installed_at: 0,
        files: Vec::new(),
    };
    snapshot.files.push(service::SnapshotFile {
        path: "mods/satin-2.0.0.jar".to_owned(),
        downloads: vec![
            "https://cdn.modrinth.com/data/fRbqPLg4/versions/Tq9qJzQz/satin-2.0.0.jar".to_owned(),
        ],
        size: 4,
        sha1: None,
        sha512: None,
        client: None,
        server: Some("unsupported".to_owned()),
        project_id: Some("fRbqPLg4".to_owned()),
        version_id: Some("Tq9qJzQz".to_owned()),
        modids: vec!["satin".to_owned()],
        mod_version: Some("2.0.0".to_owned()),
    });
    snapshot.save(&harness.server_dir()).await.expect("写快照");

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    assert_eq!(plan.installs.len(), 1, "{plan:?}");
    assert_eq!(
        plan.installs[0].project_id, "fRbqPLg4",
        "应当用清单里的项目 ID，而不是搜索命中的 satin-free-wakes"
    );
    assert_eq!(
        plan.installs[0].version_id.as_deref(),
        Some("Tq9qJzQz"),
        "清单点名了版本就按它装"
    );

    let applied = harness
        .mods
        .apply_fix(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("apply_fix");
    assert_eq!(applied.installed.len(), 1, "{applied:?}");
    assert!(harness.mods_dir().join("satin.jar").is_file());
}

/// 依赖是链不是一层：补装的 mod 自己还缺依赖时继续往下解析
///
/// 现场：fancymenu 缺 melody，melody 又缺 unified-api。装 melody 被安装
/// 检查拒绝（缺 unified-api）时不能就此停手，否则每跑一次只能前进一层。
#[tokio::test]
async fn apply_fix_resolves_a_dependency_chain() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("fancymenu.jar", SideSupport::Required),
            ("melody.jar", SideSupport::Required),
            ("unified-api.jar", SideSupport::Required),
        ],
        b"jar-bytes",
    );
    // fancymenu → melody → unified-api
    harness.inspector.declare_dep("fancymenu.jar", "melody");
    harness.inspector.declare_dep("melody.jar", "unified-api");
    harness.write("mods/fancymenu.jar", b"fm");
    harness.source.configure(|config| {
        config.search_pool = vec![
            search_hit("melody", false),
            search_hit("unified-api", false),
        ];
        config.candidates = vec![candidate("v1")];
    });
    harness.set_resolved_for("melody", &[("v1", "melody.jar")]);
    harness.add_resolved_for("unified-api", &[("v1", "unified-api.jar")]);

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    assert_eq!(plan.installs.len(), 1, "{plan:?}");
    assert_eq!(plan.installs[0].missing, "melody");

    let applied = harness
        .mods
        .apply_fix(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("apply_fix");
    let installed: Vec<&str> = applied
        .installed
        .iter()
        .map(|item| item.file.as_str())
        .collect();
    assert_eq!(
        installed,
        vec!["unified-api.jar", "melody.jar"],
        "{applied:?}"
    );
    assert!(applied.refused.is_empty(), "{applied:?}");
    assert!(applied.unresolved.is_empty(), "{applied:?}");
}

/// 依赖补不上、文件又只是客户端功能：计划给出移除，而不是反复补装
///
/// 「补不上」指上游根本没有提供该 modid 的项目。上游有、只是被标成仅
/// 客户端的库不算补不上——那类标注经常有误，安装检查才是判据。
#[tokio::test]
async fn plan_fix_proposes_removing_client_mod_with_unsatisfiable_dependency() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("fancymenu.jar", SideSupport::Optional),
            ("good.jar", SideSupport::Required),
        ],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("fancymenu.jar", "melody");
    harness.write("mods/fancymenu.jar", b"fm");
    harness.write("mods/good.jar", b"good");
    // 上游没有提供 melody 的项目
    harness
        .source
        .configure(|config| config.search_pool = Vec::new());

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    assert!(plan.installs.is_empty(), "{plan:?}");
    assert_eq!(plan.unresolved, vec!["melody".to_owned()]);
    assert_eq!(plan.removals.len(), 1, "{plan:?}");
    assert!(plan.removals[0].file.ends_with("fancymenu.jar"));
    assert!(
        plan.removals[0].reason.contains("melody"),
        "reason should name the dependency: {}",
        plan.removals[0].reason
    );
}

/// 上游有本体但被标成仅客户端：不算「补不上」，不规划移除
///
/// 与上一条的分界：这里上游确实存在提供 `melody` 的项目，只是标注说它
/// 只跑客户端。移不移除的另一半判据是安装检查，计划阶段不该按上游标注
/// 提前放弃——实测 `melody` 装到服务端正常装载，把它判成「没有提供方」
/// 会让用户以为上游没有这个 mod。
#[tokio::test]
async fn plan_fix_keeps_client_mod_whose_dependency_exists_upstream() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("fancymenu.jar", SideSupport::Optional)],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("fancymenu.jar", "melody");
    harness.write("mods/fancymenu.jar", b"fm");
    harness.source.configure(|config| {
        config.search_pool = vec![search_hit("melody", true)];
    });
    harness.set_resolved_for("melody", &[("v1", "melody.jar")]);

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    assert_eq!(plan.installs.len(), 1, "{plan:?}");
    assert_eq!(plan.installs[0].project_id, "melody");
    assert!(plan.unresolved.is_empty(), "{plan:?}");
    assert!(
        plan.removals.is_empty(),
        "依赖有提供方时不该规划移除：{plan:?}"
    );
}

/// 服务端真正需要的 mod 缺依赖时不提议移除：删掉它能起，但业务就没了
#[tokio::test]
async fn plan_fix_keeps_server_mod_with_unsatisfiable_dependency() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("gameplay.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("gameplay.jar", "some-lib");
    harness.write("mods/gameplay.jar", b"g");
    harness
        .source
        .configure(|config| config.search_pool = Vec::new());

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    assert!(plan.removals.is_empty(), "{plan:?}");
    assert_eq!(plan.unresolved, vec!["some-lib".to_owned()]);
}

/// 依赖声明成 `side="CLIENT"` 时不算缺失：服务端加载器整段跳过
///
/// 现场：`embeddiumplus` 的四个依赖块全写 `side="CLIENT"`，装到 dedicated
/// server 上正常启动。旧口径把它报成「缺少必需依赖 `embeddium` 与
/// `textrues_embeddium_options`」，进而把一个能跑的 mod 规划成「必须移除」。
#[tokio::test]
async fn plan_fix_ignores_client_side_dependency() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("embeddiumplus.jar", SideSupport::Optional)],
        b"jar-bytes",
    );
    harness.inspector.declare_dep_with_side(
        "embeddiumplus.jar",
        "embeddium",
        domain::mod_::DepSide::Client,
    );
    harness.write("mods/embeddiumplus.jar", b"ep");
    harness
        .source
        .configure(|config| config.search_pool = Vec::new());

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    assert!(plan.installs.is_empty(), "{plan:?}");
    assert!(plan.removals.is_empty(), "{plan:?}");
    assert!(plan.unresolved.is_empty(), "{plan:?}");
}

/// 移除不是销毁：文件进 `.mcsup-trash/`，用户随时能取回
///
/// 判定是工具的推断，可能是错的——实测已误删过两个能正常装载的 jar，只能
/// 重新下载。移除动作必须可逆。
#[tokio::test]
async fn apply_fix_moves_removed_files_to_trash() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("client-only.jar", SideSupport::Unsupported)],
        b"jar-bytes",
    );
    harness.write("mods/client-only.jar", b"payload");

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    assert_eq!(plan.removals.len(), 1, "{plan:?}");

    let applied = harness
        .mods
        .apply_fix(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("apply_fix");

    assert_eq!(applied.removed.len(), 1, "{applied:?}");
    assert!(!harness.mods_dir().join("client-only.jar").exists());
    let trashed = harness
        .server_dir()
        .join(service::TRASH_DIR)
        .join("client-only.jar");
    assert_eq!(
        std::fs::read(&trashed).expect("回收站里应当有原文件"),
        b"payload"
    );
}

/// 回收站里的同名文件不覆盖：加序号，两份都留着
#[tokio::test]
async fn trash_never_overwrites_an_earlier_copy() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("client-only.jar", SideSupport::Unsupported)],
        b"jar-bytes",
    );
    harness.write("mods/client-only.jar", b"second");
    // 上一轮移除时已经在回收站里放过同名文件
    harness.write(".mcsup-trash/client-only.jar", b"first");

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    harness
        .mods
        .apply_fix(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("apply_fix");

    let trash = harness.server_dir().join(service::TRASH_DIR);
    assert_eq!(
        std::fs::read(trash.join("client-only.jar")).expect("read"),
        b"first"
    );
    assert_eq!(
        std::fs::read(trash.join("client-only-1.jar")).expect("read"),
        b"second"
    );
}

/// 上游命中的项目不提供要补的 modid：不许落盘
///
/// 现场：`melody` 与 `music-and-melody` 是两个项目，后者的 modid 是
/// `music_and_melody`。按名字检索会把不相干的 jar 装进 `mods/`，之后依赖
/// 依旧缺失，用户只会看到「补装了但没修好」。
#[tokio::test]
async fn apply_fix_refuses_a_provider_that_does_not_provide_the_modid() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("fancymenu.jar", SideSupport::Required),
            ("music-and-melody.jar", SideSupport::Required),
        ],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("fancymenu.jar", "melody");
    harness.write("mods/fancymenu.jar", b"fm");
    harness.source.configure(|config| {
        config.search_pool = vec![search_hit("melody", false)];
        config.candidates = vec![candidate("v1")];
    });
    // 命中的项目给出来的是名字相近的另一个 mod
    harness.set_resolved_for("melody", &[("v1", "music-and-melody.jar")]);

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    let applied = harness
        .mods
        .apply_fix(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("apply_fix");
    assert!(applied.installed.is_empty(), "{applied:?}");
    assert!(!harness.mods_dir().join("music-and-melody.jar").exists());
    let refused = applied.refused.first().expect("refused entry");
    assert!(
        refused.reason.contains("melody"),
        "reason should name the modid: {}",
        refused.reason
    );
    assert!(
        applied.unresolved.contains(&"melody".to_owned()),
        "melody is still missing: {applied:?}"
    );
}

/// 上游只发预发布时依赖照样补得上
///
/// 现场：`unified-api` 在 26.3 上只发 beta。坚持「不要预发布」会让必需
/// 依赖永远补不上，修复命令永远停在同一步。
#[tokio::test]
async fn apply_fix_falls_back_to_prerelease_for_dependencies() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("fancymenu.jar", SideSupport::Required),
            ("unified-api.jar", SideSupport::Required),
        ],
        b"jar-bytes",
    );
    harness
        .inspector
        .declare_dep("fancymenu.jar", "unified-api");
    harness.write("mods/fancymenu.jar", b"fm");
    harness.source.configure(|config| {
        config.search_pool = vec![search_hit("unified-api", false)];
        // 只有 beta，没有正式版
        config.candidates = vec![ContentCandidate {
            channel: "beta".to_owned(),
            ..candidate("26.3-1.0-beta-23")
        }];
    });
    harness.set_resolved_for("unified-api", &[("26.3-1.0-beta-23", "unified-api.jar")]);

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    let applied = harness
        .mods
        .apply_fix(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("apply_fix");
    assert_eq!(applied.installed.len(), 1, "{applied:?}");
    assert!(applied.refused.is_empty(), "{applied:?}");
    assert!(harness.mods_dir().join("unified-api.jar").is_file());
}

/// 提供方项目存在但没有匹配这台服务端的版本：记成 refused 而不是整批报错
#[tokio::test]
async fn apply_fix_records_provider_without_candidate() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("fancymenu.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("fancymenu.jar", "melody");
    harness.write("mods/fancymenu.jar", b"fm");
    harness.source.configure(|config| {
        config.search_pool = vec![search_hit("melody", false)];
        // 候选为空：这台服务端没有可装版本
    });

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    let applied = harness
        .mods
        .apply_fix(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("apply_fix must not fail the batch");
    assert!(applied.installed.is_empty(), "{applied:?}");
    let refused = applied.refused.first().expect("refused entry");
    assert_eq!(refused.project_id, "melody");
    assert!(!refused.reason.is_empty());
}

/// 链上某一环在上游找不到：如实报出缺的是哪个依赖，不整批失败
#[tokio::test]
async fn apply_fix_reports_chain_dependency_without_provider() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("fancymenu.jar", SideSupport::Required),
            ("melody.jar", SideSupport::Required),
        ],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("fancymenu.jar", "melody");
    harness.inspector.declare_dep("melody.jar", "unified-api");
    harness.write("mods/fancymenu.jar", b"fm");
    harness.source.configure(|config| {
        // melody 找得到，unified-api 找不到
        config.search_pool = vec![search_hit("melody", false)];
        config.candidates = vec![candidate("v1")];
    });
    harness.set_resolved_for("melody", &[("v1", "melody.jar")]);

    let plan = harness
        .mods
        .plan_fix("alpha", ContentKind::Mod)
        .await
        .expect("plan_fix");
    let applied = harness
        .mods
        .apply_fix(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("apply_fix");

    assert!(applied.installed.is_empty(), "{applied:?}");
    // 结论必须说清是哪个依赖缺，而不是一句「被拒绝」
    assert_eq!(applied.unresolved, vec!["unified-api".to_owned()]);
    let refused = applied.refused.first().expect("refused entry");
    assert!(
        refused.reason.contains("unified-api"),
        "reason should name the missing dependency: {}",
        refused.reason
    );
}

// ---------------------------------------------------------------- 用例

/// 空服务端：没有 `mods/` 目录也照常回答「没有 mod」，不报错
#[tokio::test]
async fn list_on_server_without_mods_directory_is_empty() {
    let harness = Harness::new(CommandLog::stopped(), &[], &[b'x'; 4]);
    let listed = harness
        .mods
        .list("alpha", ListOptions::default())
        .await
        .expect("list");
    assert!(listed.mods.is_empty(), "{:?}", listed.mods);
    assert!(listed.unreadable.is_empty());
    assert_eq!(listed.dir, harness.mods_dir());
}

/// 本地检查结论直接进列表，不需要联网
#[tokio::test]
async fn list_reports_local_verdicts_without_upstream() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("good.jar", SideSupport::Required),
            ("client.jar", SideSupport::Unsupported),
        ],
        &[b'x'; 4],
    );
    harness.write("mods/good.jar", b"good");
    harness.write("mods/client.jar", b"client");

    let listed = harness
        .mods
        .list("alpha", ListOptions::default())
        .await
        .expect("list");

    assert_eq!(listed.mods.len(), 2);
    let client = listed
        .mods
        .iter()
        .find(|entry| entry.file == "client.jar")
        .expect("client entry");
    assert_eq!(client.report.risk, domain::mod_::Risk::Blocking);
    // 没开识别就不该有上游身份，也不该误报「已是最新」
    assert!(client.identity.is_none());
    assert!(client.update.is_none());
}

/// 安装落到 `mods/`，暂存区用完即删
#[tokio::test]
async fn install_writes_to_mods_and_cleans_staging() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("cool-mod.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    let plan = harness
        .mods
        .plan_install("alpha", "cool-mod", ContentKind::Mod, install_spec())
        .await
        .expect("plan");
    assert_eq!(plan.target, harness.mods_dir().join("cool-mod.jar"));
    assert!(plan.replaces.is_none());

    let installed = harness
        .mods
        .install(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("install");

    assert_eq!(installed.risk, domain::mod_::Risk::Ok);
    assert_eq!(
        std::fs::read(harness.mods_dir().join("cool-mod.jar")).expect("read"),
        b"jar-bytes"
    );
    assert_eq!(harness.downloader.calls.load(Ordering::SeqCst), 1);
    assert_eq!(harness.staging_entries(), 0, "暂存目录必须清空");
}

/// 检查判定不可用于服务端：不落盘，暂存也删掉
#[tokio::test]
async fn install_refuses_client_only_mod_without_force() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("cool-mod.jar", SideSupport::Unsupported)],
        b"jar-bytes",
    );
    let plan = harness
        .mods
        .plan_install("alpha", "cool-mod", ContentKind::Mod, install_spec())
        .await
        .expect("plan");

    let err = harness
        .mods
        .install(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect_err("必须拒绝");

    assert!(matches!(err, ModError::Refused { .. }), "{err:?}");
    assert_eq!(err.category(), port::ErrorCategory::Conflict);
    assert!(
        !harness.mods_dir().join("cool-mod.jar").exists(),
        "拒绝后不能留下文件"
    );
    assert_eq!(harness.staging_entries(), 0, "暂存目录必须清空");
}

/// `--force` 才落盘，但风险仍然写进结果
#[tokio::test]
async fn install_with_force_keeps_risk_visible() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("cool-mod.jar", SideSupport::Unsupported)],
        b"jar-bytes",
    );
    let plan = harness
        .mods
        .plan_install("alpha", "cool-mod", ContentKind::Mod, install_spec())
        .await
        .expect("plan");

    let installed = harness
        .mods
        .install(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: true,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("强制安装");

    assert_eq!(installed.risk, domain::mod_::Risk::Blocking);
    assert!(harness.mods_dir().join("cool-mod.jar").is_file());
}

/// 已有的同名文件被替换，旧内容不残留
#[tokio::test]
async fn install_replaces_existing_file() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("cool-mod.jar", SideSupport::Required)],
        b"new-bytes",
    );
    harness.write("mods/cool-mod.jar", b"old");
    let plan = harness
        .mods
        .plan_install("alpha", "cool-mod", ContentKind::Mod, install_spec())
        .await
        .expect("plan");
    assert_eq!(
        plan.replaces.as_deref(),
        Some(harness.mods_dir().join("cool-mod.jar").as_path())
    );

    harness
        .mods
        .install(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("install");

    assert_eq!(
        std::fs::read(harness.mods_dir().join("cool-mod.jar")).expect("read"),
        b"new-bytes"
    );
}

/// 运行中的服务端一律拒绝：计划与落盘都不能过
#[tokio::test]
async fn running_server_blocks_plan_and_remove() {
    let harness = Harness::new(
        CommandLog::running(),
        &[("cool-mod.jar", SideSupport::Required)],
        &[b'x'; 4],
    );
    harness.write("mods/cool-mod.jar", b"old");

    let plan = harness
        .mods
        .plan_install("alpha", "cool-mod", ContentKind::Mod, install_spec())
        .await
        .expect_err("运行中不能规划安装");
    assert!(matches!(plan, ModError::Running { .. }), "{plan:?}");
    assert_eq!(plan.category(), port::ErrorCategory::Conflict);

    let removed = harness
        .mods
        .remove("alpha", "cool-mod.jar", ContentKind::Mod, false)
        .expect_err("运行中不能删除");
    assert!(matches!(removed, ModError::Running { .. }), "{removed:?}");
    assert!(
        harness.mods_dir().join("cool-mod.jar").is_file(),
        "文件必须还在"
    );
}

/// 卸载：先按文件名，再按 modid；预览不动盘
#[tokio::test]
async fn remove_matches_by_file_name_then_modid() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("alpha-mod.jar", SideSupport::Required),
            ("beta-mod.jar", SideSupport::Required),
        ],
        &[b'x'; 4],
    );
    harness.write("mods/alpha-mod.jar", b"a");
    harness.write("mods/beta-mod.jar", b"b");

    // 预览：命中但不是真删
    let preview = harness
        .mods
        .remove("alpha", "alpha-mod.jar", ContentKind::Mod, true)
        .expect("preview");
    assert_eq!(preview.files.len(), 1);
    assert!(preview.dry_run);
    assert!(harness.mods_dir().join("alpha-mod.jar").is_file());

    // 文件名删除
    harness
        .mods
        .remove("alpha", "alpha-mod.jar", ContentKind::Mod, false)
        .expect("remove by name");
    assert!(!harness.mods_dir().join("alpha-mod.jar").exists());

    // modid 删除（文件名不带 modid 时命中 capability.id）
    harness
        .mods
        .remove("alpha", "beta-mod", ContentKind::Mod, false)
        .expect("remove by modid");
    assert!(!harness.mods_dir().join("beta-mod.jar").exists());
}

/// 选择器没命中：报「没找到」，不是静默成功
#[tokio::test]
async fn remove_unknown_selector_is_not_found() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("alpha-mod.jar", SideSupport::Required)],
        &[b'x'; 4],
    );
    harness.write("mods/alpha-mod.jar", b"a");

    let err = harness
        .mods
        .remove("alpha", "gamma", ContentKind::Mod, false)
        .expect_err("必须报没找到");
    assert!(matches!(err, ModError::NotFound { .. }), "{err:?}");
    assert_eq!(err.category(), port::ErrorCategory::NotFound);
}

/// 远端条目不在本机处理
#[tokio::test]
async fn remote_server_is_rejected() {
    let harness = Harness::new(CommandLog::stopped(), &[], &[b'x'; 4]);
    // 名字前缀 `edge/` 让注册表按远端解析（与 `--origin` 同一套限定名）
    let err = harness
        .mods
        .list("edge/alpha", ListOptions::default())
        .await
        .expect_err("远端不该在这里处理");
    assert!(
        matches!(err, ModError::Registry(_) | ModError::Remote { .. }),
        "{err:?}"
    );
}

// ---------------------------------------------------------------- 更新

/// `--outdated` 口径：只开 `updates` 也必须拿到结论（隐含识别）
#[tokio::test]
async fn list_resolves_updates_without_explicit_identify() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("cool-mod-1.0.jar", SideSupport::Required)],
        &[b'x'; 4],
    );
    harness.identify_as("cool-mod", "v1", "1.0");
    harness.set_candidates(&["v2"]);
    harness.write("mods/cool-mod-1.0.jar", b"old");

    let listed = harness
        .mods
        .list(
            "alpha",
            ListOptions {
                kind: ContentKind::Mod,
                identify: false,
                source: None,
                updates: true,
            },
        )
        .await
        .expect("list");

    let entry = listed.mods.first().expect("one mod");
    assert_eq!(
        entry.update,
        Some(UpdateDecision::Available {
            version_id: "v2".to_owned(),
            version_number: "v2".to_owned(),
        })
    );
}

/// 更新：新版本换了文件名时，旧文件必须被删掉
#[tokio::test]
async fn update_replaces_renamed_file_and_drops_the_old_one() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("cool-mod-1.0.jar", SideSupport::Required),
            ("cool-mod-2.0.jar", SideSupport::Required),
        ],
        b"new-bytes",
    );
    harness.identify_as("cool-mod", "v1", "1.0");
    harness.set_candidates(&["v2"]);
    harness.set_resolved(&[("v2", "cool-mod-2.0.jar")]);
    harness.write("mods/cool-mod-1.0.jar", b"old");

    let plan = harness
        .mods
        .plan_update("alpha", None, ListOptions::default())
        .await
        .expect("plan");
    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.items[0].from_version, "1.0");
    assert_eq!(plan.items[0].to_version, "v2");
    assert_eq!(
        plan.items[0].current,
        harness.mods_dir().join("cool-mod-1.0.jar")
    );
    assert!(
        plan.skipped.is_empty(),
        "识别成功的文件不该进 skipped: {:?}",
        plan.skipped
    );

    let applied = harness
        .mods
        .apply_update(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("apply");

    assert_eq!(applied.changes.len(), 1);
    let change = &applied.changes[0];
    assert_eq!(
        change.removed.as_deref(),
        Some(harness.mods_dir().join("cool-mod-1.0.jar").as_path())
    );
    assert_eq!(
        std::fs::read(harness.mods_dir().join("cool-mod-2.0.jar")).expect("read"),
        b"new-bytes"
    );
    assert!(
        !harness.mods_dir().join("cool-mod-1.0.jar").exists(),
        "旧文件必须删除，两份同 modid 的 jar 会让服务端起不来"
    );
    assert_eq!(harness.staging_entries(), 0);
}

/// 上游不认识的文件进 `skipped`，不能当作「已是最新」
#[tokio::test]
async fn update_marks_unknown_files_as_skipped() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("manually-dropped.jar", SideSupport::Required)],
        &[b'x'; 4],
    );
    // 不调用 identify_as：`by_hash` 返回空，等价于上游没收录这个文件
    harness.write("mods/manually-dropped.jar", b"a");

    let plan = harness
        .mods
        .plan_update("alpha", None, ListOptions::default())
        .await
        .expect("plan");

    assert!(plan.items.is_empty(), "不该凭空造出更新项");
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].0, "manually-dropped.jar");
}

/// 读不出来的文件同样进 `skipped`：既不能识别也不能更新
#[tokio::test]
async fn update_marks_unreadable_files_as_skipped() {
    // 检查器不认识 broken.jar → 归档读不出来
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("good.jar", SideSupport::Required)],
        &[b'x'; 4],
    );
    harness.identify_as("cool-mod", "v1", "1.0");
    harness.set_candidates(&["v1"]);
    harness.write("mods/broken.jar", b"not-a-zip");

    let plan = harness
        .mods
        .plan_update("alpha", None, ListOptions::default())
        .await
        .expect("plan");

    assert!(plan.items.is_empty());
    assert_eq!(
        plan.skipped.len(),
        1,
        "读不出来的文件必须说明，而不是静默消失: {:?}",
        plan.skipped
    );
    assert_eq!(plan.skipped[0].0, "broken.jar");
}

/// 上游没有兼容候选 = 已是最新，不是失败
#[tokio::test]
async fn update_without_candidates_counts_as_up_to_date() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("cool-mod-1.0.jar", SideSupport::Required)],
        &[b'x'; 4],
    );
    harness.identify_as("cool-mod", "v1", "1.0");
    harness.set_candidates(&[]);
    harness.write("mods/cool-mod-1.0.jar", b"old");

    let plan = harness
        .mods
        .plan_update("alpha", None, ListOptions::default())
        .await
        .expect("plan");

    assert!(plan.items.is_empty());
    assert!(
        plan.skipped.is_empty(),
        "没有候选是「已最新」，不是「判断不了」: {:?}",
        plan.skipped
    );
}

/// 选择器只更新命中的那一个
#[tokio::test]
async fn update_honours_the_selector() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("cool-mod-1.0.jar", SideSupport::Required),
            ("other-mod.jar", SideSupport::Required),
        ],
        &[b'x'; 4],
    );
    harness.identify_as("cool-mod", "v1", "1.0");
    harness.set_candidates(&["v2"]);
    harness.set_resolved(&[("v2", "cool-mod.jar")]);
    harness.write("mods/cool-mod-1.0.jar", b"old");
    harness.write("mods/other-mod.jar", b"other");

    let plan = harness
        .mods
        .plan_update("alpha", Some("cool-mod-1.0.jar"), ListOptions::default())
        .await
        .expect("plan");

    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.items[0].file, "cool-mod-1.0.jar");
}

// ---------------------------------------------------------------- 失败收场

/// 检查器读不出暂存文件：不落盘，暂存清空，报「读不出来」
#[tokio::test]
async fn install_reports_unreadable_staged_archive() {
    // 检查器不认识 cool-mod.jar（resolved 出的文件名）
    let harness = Harness::new(CommandLog::stopped(), &[], b"jar-bytes");
    let plan = harness
        .mods
        .plan_install("alpha", "cool-mod", ContentKind::Mod, install_spec())
        .await
        .expect("plan");

    let err = harness
        .mods
        .install(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect_err("读不出来必须报错");

    assert!(matches!(err, ModError::Unreadable { .. }), "{err:?}");
    assert_eq!(err.category(), port::ErrorCategory::Backend);
    assert!(!harness.mods_dir().join("cool-mod.jar").exists());
    assert_eq!(harness.staging_entries(), 0);
}

/// 下载被取消：按取消退出（130），不留半截文件
#[tokio::test]
async fn cancelled_download_leaves_nothing_behind() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("cool-mod.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    harness.cancel_downloads();
    let plan = harness
        .mods
        .plan_install("alpha", "cool-mod", ContentKind::Mod, install_spec())
        .await
        .expect("plan");

    let err = harness
        .mods
        .install(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect_err("取消必须报错");

    assert_eq!(err.category(), port::ErrorCategory::Cancelled, "{err:?}");
    assert!(!harness.mods_dir().join("cool-mod.jar").exists());
    assert_eq!(harness.staging_entries(), 0);
}

/// 缺必需依赖：判定为 blocking，`--force` 装下去时缺失项必须照实报出
#[tokio::test]
async fn missing_required_dependency_is_reported_even_when_forced() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("cool-mod.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    // `fabric-api` 这类加载器自带的依赖不算缺失，这里用一个真正的外部依赖
    harness.inspector.declare_dep("cool-mod.jar", "some-lib");
    let plan = harness
        .mods
        .plan_install("alpha", "cool-mod", ContentKind::Mod, install_spec())
        .await
        .expect("plan");

    let refused = harness
        .mods
        .install(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect_err("缺必需依赖必须拦住");
    assert!(matches!(refused, ModError::Refused { .. }), "{refused:?}");

    let installed = harness
        .mods
        .install(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: true,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("强制安装");
    assert_eq!(installed.risk, domain::mod_::Risk::Blocking);
    assert_eq!(installed.missing_deps, vec!["some-lib".to_owned()]);
}

/// 依赖已经装在 `mods/` 里：不判缺失，正常安装
///
/// 回归：安装检查只看暂存区那一批文件，`mods/` 里已有的 `satin` 满足不了
/// 待装文件声明的依赖，被误报成「缺少必需依赖」，只能 `--force`。加载器
/// 解析时看的是整个目录。
#[tokio::test]
async fn dependency_present_in_mods_dir_is_not_missing() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("cool-mod.jar", SideSupport::Required),
            ("satin.jar", SideSupport::Required),
        ],
        b"jar-bytes",
    );
    harness.inspector.declare_dep("cool-mod.jar", "satin");
    // 预置依赖提供方：FakeInspector 按文件名给结论，磁盘上放同名文件
    harness.write("mods/satin.jar", b"installed-lib");
    let plan = harness
        .mods
        .plan_install("alpha", "cool-mod", ContentKind::Mod, install_spec())
        .await
        .expect("plan");

    let installed = harness
        .mods
        .install(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("依赖已装就不该拦");
    assert_eq!(installed.risk, domain::mod_::Risk::Ok, "{installed:?}");
    assert!(installed.missing_deps.is_empty());
}

/// 被替换的旧文件不参与依赖闭包：升级后旧文件不存在了，不能拿它满足依赖
///
/// 场景：旧 `cool-mod.jar` 提供 `old-api`，新版本不再提供但仍声明依赖它
/// （上游改了依赖名）——闭包若把即将被替换的文件也算进去，就会漏判。
/// 实际更常见的形态是「同名文件被覆盖」：闭包里必须排除 `plan.replaces`。
#[tokio::test]
async fn replaced_file_is_excluded_from_dependency_closure() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("cool-mod.jar", SideSupport::Required)],
        b"jar-bytes-new",
    );
    // 新文件依赖 `satin`；磁盘上的旧 cool-mod.jar 不提供它（旧文件会被替换）
    harness.inspector.declare_dep("cool-mod.jar", "satin");
    harness.write("mods/cool-mod.jar", b"jar-bytes-old");
    // 让计划解析出同名文件 → replaces 命中旧文件
    harness.set_resolved(&[("v2", "cool-mod.jar")]);
    let plan = harness
        .mods
        .plan_install("alpha", "cool-mod", ContentKind::Mod, install_spec())
        .await
        .expect("plan");
    assert!(plan.replaces.is_some(), "应当命中同名旧文件");

    let refused = harness
        .mods
        .install(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect_err("替换后旧文件不能继续满足依赖");
    assert!(matches!(refused, ModError::Refused { .. }), "{refused:?}");
}

/// 更新中途失败：已完成项保留，失败项停在原版本，错误说清停在哪
#[tokio::test]
async fn update_stops_at_the_failing_item_and_reports_progress() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("a-mod-1.0.jar", SideSupport::Required),
            ("b-mod-1.0.jar", SideSupport::Required),
            ("a-mod-2.0.jar", SideSupport::Required),
            ("b-mod-2.0.jar", SideSupport::Required),
        ],
        b"new-bytes",
    );
    // 文件名排序：a 在前、b 在后，两个文件分别属于两个项目
    harness.identify_each(&[("a-mod", "v1", "1.0"), ("b-mod", "v1", "1.0")]);
    harness.set_candidates(&["v2"]);
    harness.source.configure(|config| {
        config.resolved = vec![
            resolved_for("a-mod", "v2", "a-mod-2.0.jar", 9),
            resolved_for("b-mod", "v2", "b-mod-2.0.jar", 9),
        ];
    });
    harness.write("mods/a-mod-1.0.jar", b"a-old");
    harness.write("mods/b-mod-1.0.jar", b"b-old");
    harness.fail_download_of("b-mod-2.0.jar");

    let plan = harness
        .mods
        .plan_update("alpha", None, ListOptions::default())
        .await
        .expect("plan");
    assert_eq!(
        plan.items.len(),
        2,
        "两个文件都该有更新: {:?}",
        plan.skipped
    );

    let err = harness
        .mods
        .apply_update(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect_err("第二项下载失败必须报错");

    match &err {
        ModError::UpdateAborted {
            failed, applied, ..
        } => {
            assert_eq!(failed, "b-mod-1.0.jar", "要指出失败的那一项");
            assert_eq!(*applied, 1, "已完成项数必须照实报出");
        }
        other => panic!("应报 UpdateAborted，实际 {other:?}"),
    }
    // 失败等级沿用底层原因：下载超时是后端失败（5）
    assert_eq!(err.category(), port::ErrorCategory::Backend);

    // 第一项已经落盘且旧文件已删；第二项保持原样，不留半截文件
    assert!(harness.mods_dir().join("a-mod-2.0.jar").exists());
    assert!(!harness.mods_dir().join("a-mod-1.0.jar").exists());
    assert!(harness.mods_dir().join("b-mod-1.0.jar").exists());
    assert!(!harness.mods_dir().join("b-mod-2.0.jar").exists());
    assert_eq!(harness.staging_entries(), 0);
}

/// 类别决定目录：插件落 `plugins/`，与 `mods/` 互不干扰
#[tokio::test]
async fn plugin_kind_uses_the_plugins_directory() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[
            ("cool-mod.jar", SideSupport::Required),
            ("alpha-mod.jar", SideSupport::Required),
        ],
        b"jar-bytes",
    );
    harness.write("mods/alpha-mod.jar", b"a");

    let plan = harness
        .mods
        .plan_install("alpha", "cool-mod", ContentKind::Plugin, install_spec())
        .await
        .expect("plan");
    assert_eq!(
        plan.target,
        harness.server_dir().join("plugins").join("cool-mod.jar")
    );

    harness
        .mods
        .install(
            "alpha",
            &plan,
            ApplyOptions {
                allow_blocking: false,
                progress: None,
                cancel: &CancellationToken::new(),
            },
        )
        .await
        .expect("install");

    // 两个目录各看各的：列 mod 只看到 mods/，列插件只看到 plugins/
    let mods = harness
        .mods
        .list("alpha", ListOptions::of(ContentKind::Mod))
        .await
        .expect("list mods");
    assert_eq!(mods.dir, harness.mods_dir());
    assert_eq!(
        mods.mods
            .iter()
            .map(|entry| entry.file.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha-mod.jar"]
    );

    let plugins = harness
        .mods
        .list("alpha", ListOptions::of(ContentKind::Plugin))
        .await
        .expect("list plugins");
    assert_eq!(plugins.dir, harness.server_dir().join("plugins"));
    assert_eq!(
        plugins
            .mods
            .iter()
            .map(|entry| entry.file.as_str())
            .collect::<Vec<_>>(),
        vec!["cool-mod.jar"]
    );

    // 卸载同样按类别定位：拿 mod 口径删插件应当「没找到」
    let err = harness
        .mods
        .remove("alpha", "cool-mod.jar", ContentKind::Mod, true)
        .expect_err("mods/ 里没有这个插件");
    assert!(matches!(err, ModError::NotFound { .. }), "{err:?}");
    harness
        .mods
        .remove("alpha", "cool-mod.jar", ContentKind::Plugin, false)
        .expect("按插件类别删除");
    assert!(!harness.server_dir().join("plugins/cool-mod.jar").exists());
}

/// 计划里带的是服务端口径的条件：加载器与 MC 版本都来自注册表
#[tokio::test]
async fn plan_uses_server_loader_and_game_version() {
    let harness = Harness::new(
        CommandLog::stopped(),
        &[("cool-mod.jar", SideSupport::Required)],
        b"jar-bytes",
    );
    harness
        .mods
        .plan_install("alpha", "cool-mod", ContentKind::Mod, install_spec())
        .await
        .expect("plan");

    let requests = harness.source.config.lock().expect("config").seen.clone();
    let request = requests.first().expect("候选请求");
    assert_eq!(request.loader.as_deref(), Some("fabric"));
    assert_eq!(request.game_version.as_deref(), Some("1.21.1"));
    assert_eq!(request.kind, ContentKind::Mod);
}
