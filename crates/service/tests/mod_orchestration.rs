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
    /// 声明的必需依赖（modid，缺了就算缺失依赖）
    depends: Vec<String>,
}

/// 假检查器：按**文件名**给结论（暂存路径每次不同，不能按全路径匹配）
struct FakeInspector {
    verdicts: Mutex<Vec<Verdict>>,
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
        }
    }

    /// 给某个文件追加一条必需依赖（用于「缺依赖」判定）
    fn declare_dep(&self, file: &str, dep: &str) {
        let mut verdicts = self.verdicts.lock().expect("verdicts");
        if let Some(verdict) = verdicts.iter_mut().find(|verdict| verdict.file == file) {
            verdict.depends.push(dep.to_owned());
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
        for dep in &verdict.depends {
            capability.depends.push(domain::mod_::ModDependency {
                id: dep.clone(),
                version_range: String::new(),
                mandatory: true,
                loader: Some(ModLoader::Fabric),
                unless: Vec::new(),
            });
        }
        Ok(capability)
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
    // 上游只有仅客户端的 melody：服务端没有可补的提供方
    harness.source.configure(|config| {
        config.search_pool = vec![search_hit("melody", true)];
    });

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
