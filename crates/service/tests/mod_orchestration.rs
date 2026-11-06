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
use domain::server::content::{ContentCandidate, ContentKind, ContentRequest};
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

/// 假来源：候选与解析结果由用例给定
struct FakeSource {
    candidates: Vec<ContentCandidate>,
    resolved: Option<ResolvedContent>,
    /// 收到的候选请求，供断言过滤条件是否下发
    seen_requests: Arc<Mutex<Vec<ContentRequest>>>,
}

impl FakeSource {
    fn new() -> Self {
        Self {
            candidates: Vec::new(),
            resolved: None,
            seen_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_candidates(mut self, candidates: Vec<ContentCandidate>) -> Self {
        self.candidates = candidates;
        self
    }

    fn with_resolved(mut self, resolved: ResolvedContent) -> Self {
        self.resolved = Some(resolved);
        self
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

    async fn search(&self, _query: &ContentQuery) -> Result<ContentSearch, SourceError> {
        Ok(ContentSearch {
            hits: Vec::new(),
            offset: 0,
            total: 0,
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
        })
    }

    async fn candidates(
        &self,
        _id: &str,
        request: &ContentRequest,
    ) -> Result<Vec<ContentCandidate>, SourceError> {
        self.seen_requests
            .lock()
            .expect("seen")
            .push(request.clone());
        Ok(self.candidates.clone())
    }

    async fn resolve(&self, _id: &str, version: &str) -> Result<ResolvedContent, SourceError> {
        self.resolved
            .clone()
            .filter(|resolved| resolved.version_id == version)
            .ok_or(SourceError::Status { status: 404 })
    }
}

/// 假下载器：按预置字节真的写到目标路径
struct FakeDownloader {
    bytes: Vec<u8>,
    calls: Arc<AtomicUsize>,
}

impl FakeDownloader {
    fn new(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.to_vec(),
            calls: Arc::new(AtomicUsize::new(0)),
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

/// 假检查器：按**文件名**给结论（暂存路径每次不同，不能按全路径匹配）
struct FakeInspector {
    /// (文件名, 服务端归属, 缺失依赖)
    verdicts: Vec<(String, SideSupport, Vec<String>)>,
}

impl FakeInspector {
    fn new(verdicts: &[(&str, SideSupport)]) -> Self {
        Self {
            verdicts: verdicts
                .iter()
                .map(|(name, side)| ((*name).to_owned(), *side, Vec::new()))
                .collect(),
        }
    }
}

impl ModInspector for FakeInspector {
    fn inspect(&self, path: &Path) -> Result<ModCapability, ModInspectError> {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some((_, side, missing)) = self
            .verdicts
            .iter()
            .find(|(candidate, _, _)| *candidate == name)
        else {
            return Err(ModInspectError::NotAnArchive {
                path: path.to_path_buf(),
            });
        };
        let stem = name.trim_end_matches(".jar").to_owned();
        let mut capability = ModCapability::empty(path.display().to_string());
        capability.id = Some(stem);
        capability.version = Some("1.0.0".to_owned());
        capability.loaders.push(ModLoader::Fabric);
        capability.side = SideVerdict::declared(SideSupport::Required, *side);
        let _ = missing;
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
        let content = ContentService::new(
            Arc::new(ContentSourceRegistry::new(vec![Arc::new(
                FakeSource::new()
                    .with_candidates(vec![candidate("v2")])
                    .with_resolved(resolved("v2", "cool-mod.jar", bytes.len() as u64)),
            )
                as Arc<dyn ContentSource>])),
            Arc::clone(&downloader) as Arc<dyn ArtifactDownloader>,
        );
        let mods = ModService::new(
            registry,
            Arc::new(FakeBackend {
                supervisor: FakeSupervisor::new(log),
            }),
            content,
            Arc::new(FakeInspector::new(verdicts)),
        );
        Self {
            fixture,
            mods,
            downloader,
        }
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
    ResolvedContent {
        project_id: "cool-mod".to_owned(),
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
        .remove("alpha", "cool-mod.jar", false)
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
        .remove("alpha", "alpha-mod.jar", true)
        .expect("preview");
    assert_eq!(preview.files.len(), 1);
    assert!(preview.dry_run);
    assert!(harness.mods_dir().join("alpha-mod.jar").is_file());

    // 文件名删除
    harness
        .mods
        .remove("alpha", "alpha-mod.jar", false)
        .expect("remove by name");
    assert!(!harness.mods_dir().join("alpha-mod.jar").exists());

    // modid 删除（文件名不带 modid 时命中 capability.id）
    harness
        .mods
        .remove("alpha", "beta-mod", false)
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
        .remove("alpha", "gamma", false)
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
