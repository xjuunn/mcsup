//! 任务级接口的真实装配：真注册表、真安装编排、假来源与假下载器
//!
//! 这里不打桩 service：`Kit::create_server` 走的是与 CLI 同一条
//! 注册表 → 来源 → 下载 → 校验 → 登记核心链路，只把网络换成可控假件。
//! 因此「一行建服」是否真的落盘、是否只下载一次、失败前有没有副作用，
//! 都在这里被钉住。
//!
//! 覆盖范围按任务分块：建服 / 起停 / 换核心 / 删服 / 备份全流程 / 内容
//! 安装与更新 / 整合包开服 / Java 安装与本机列表。

// feature powerset 会单独开每个 feature：没开 `kit` 时这个测试文件整体不编译
#![cfg(feature = "kit")]
// 集成测试不在 `#[test]` 函数体内，clippy 的 `expect_used` 豁免只覆盖 cfg(test)
#![allow(clippy::expect_used)]

use async_trait::async_trait;
use mcsup::bootstrap::{Mcsup, McsupBuilder};
use mcsup::domain::java::{JavaMajor, JavaPackageType, JavaRuntime, JavaVendor, java_exe_name};
use mcsup::domain::server::artifact::{Checksum, JarArtifact};
use mcsup::domain::server::content::{
    ContentCandidate, ContentIdentity, ContentKind, ContentRequest,
};
use mcsup::domain::server::modpack::{
    EnvSupport, FileEnv, ModpackDependencies, ModpackFile, ModpackHashes, ModpackIndex,
};
use mcsup::domain::server::status::ServerStatus;
use mcsup::domain::server::variant::ServerVariant;
use mcsup::domain::server::version::{ServerVersion, VersionId};
use mcsup::kit::{
    BackupNow, CreateServer, InstallCore, InstallJava, InstallMod, KitConfig, KitError,
    ListBackups, PruneBackups, RemoveMod, RemoveServer, RestoreBackup, StartServer, StopServer,
    UpdateMods,
};
use mcsup::port::content::{
    ContentFile, ContentProject, ContentQuery, ContentSearch, ContentSource, ContentSourceInfo,
    ResolvedContent,
};
use mcsup::port::modpack::{
    ModpackDetail, ModpackExtractError, ModpackExtractor, ModpackQuery, ModpackSearch,
    ModpackSource, ModpackVersion, ResolvedModpack,
};
use mcsup::port::{
    ArchiveExtractor, ArtifactDownloader, ErrorCategory, ExtractError, InstallKind, JavaPackage,
    JavaPackageRequest, JavaScanner, JavaSourceRegistryPort, ResolvedArtifact, ResolvedJava,
    ScannedJava, ServerJarSource, ServerRef, SourceError, StopOutcome, StopSpec, SupervisorError,
    SupervisorPort, SupervisorStartOutcome, SupervisorStartRequest, SupervisorStatus,
};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use testkit::FailingDownloader;
use url::Url;

// ---------------------------------------------------------------- 假件

/// 一个永远返回同一份「服务端」的假来源
struct FixedSource {
    variant: ServerVariant,
    location: Url,
}

impl FixedSource {
    fn new(variant: ServerVariant, file: &str) -> Self {
        Self {
            variant,
            location: Url::parse(&format!("http://127.0.0.1:1/{file}")).expect("url"),
        }
    }

    fn vanilla() -> Self {
        Self::new(ServerVariant::Vanilla, "server.jar")
    }

    fn fabric() -> Self {
        Self::new(ServerVariant::Fabric, "fabric-server-launch.jar")
    }
}

#[async_trait]
impl ServerJarSource for FixedSource {
    fn variant(&self) -> ServerVariant {
        self.variant.clone()
    }

    async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError> {
        Ok(Vec::new())
    }

    async fn artifact(&self, version: &VersionId) -> Result<ResolvedArtifact, SourceError> {
        Ok(ResolvedArtifact {
            artifact: JarArtifact {
                version: version.clone(),
                size: jar_bytes().len() as u64,
                // 假来源不提供哈希：走「无校验」分支，与本仓库对 Fabric 的处理一致
                checksum: Checksum::unverified(),
            },
            location: self.location.clone(),
            java_major: None,
            install: InstallKind::Direct,
            installer_args: Vec::new(),
            installer_args_dir: None,
            vanilla_jar: None,
        })
    }

    fn name(&self) -> &'static str {
        "fixed"
    }
}

/// 一份结构合法的服务端 jar（没有 `version.json`，检查器按「无数据版本」处理）
fn jar_bytes() -> Vec<u8> {
    zip_bytes(&[(
        "META-INF/MANIFEST.MF",
        b"Manifest-Version: 1.0\n".as_slice(),
    )])
}

/// 一份能被 [`modkit::ZipModInspector`] 识别为 Fabric mod 的 jar
///
/// 依赖只写加载器与游戏本身（`is_builtin_dep` 视为已满足），因此不需要
/// 预装任何东西就能通过安装检查。
fn mod_jar_bytes(id: &str, version: &str) -> Vec<u8> {
    let manifest = format!(
        r#"{{"schemaVersion":1,"id":"{id}","version":"{version}","environment":"*","depends":{{"fabricloader":">=0.14.0","minecraft":"1.21.4"}}}}"#
    );
    zip_bytes(&[
        (
            "META-INF/MANIFEST.MF",
            b"Manifest-Version: 1.0\n".as_slice(),
        ),
        ("fabric.mod.json", manifest.as_bytes()),
    ])
}

/// 把若干文件压成一个 jar 形态的归档
fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        for (name, bytes) in entries {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .expect("start entry");
            zip.write_all(bytes).expect("write");
        }
        zip.finish().expect("finish zip");
    }
    buffer.into_inner()
}

/// 能真正拉起 / 停下的 supervisor：只改内存状态，不碰进程
#[derive(Debug, Default)]
struct SwitchableSupervisor {
    alive: AtomicBool,
    starts: AtomicUsize,
    stops: AtomicUsize,
}

impl SwitchableSupervisor {
    fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }

    fn stops(&self) -> usize {
        self.stops.load(Ordering::SeqCst)
    }

    fn set_alive(&self, alive: bool) {
        self.alive.store(alive, Ordering::SeqCst);
    }
}

impl SupervisorPort for SwitchableSupervisor {
    fn start(
        &self,
        _request: SupervisorStartRequest,
    ) -> Result<SupervisorStartOutcome, SupervisorError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.set_alive(true);
        Ok(SupervisorStartOutcome {
            supervisor_pid: 4242,
        })
    }

    fn stop(
        &self,
        _server: &ServerRef<'_>,
        _spec: StopSpec,
    ) -> Result<StopOutcome, SupervisorError> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.set_alive(false);
        Ok(StopOutcome::Stopped {
            elapsed: Duration::from_millis(1),
        })
    }

    fn status(&self, _server: &ServerRef<'_>) -> Result<Option<SupervisorStatus>, SupervisorError> {
        Ok(self
            .alive
            .load(Ordering::SeqCst)
            .then_some(SupervisorStatus {
                status: ServerStatus::Running,
                pid: Some(4242),
                uptime: Some(Duration::ZERO),
                exit_code: None,
            }))
    }

    fn send_line(&self, _server: &ServerRef<'_>, _line: &str) -> Result<(), SupervisorError> {
        Ok(())
    }

    fn shutdown(&self, _server: &ServerRef<'_>) -> Result<(), SupervisorError> {
        Ok(())
    }
}

/// 报告一个固定运行时的扫描器：起服选 Java 用
struct OneJavaScanner {
    home: PathBuf,
}

impl JavaScanner for OneJavaScanner {
    fn scan(
        &self,
        on_found: &mut dyn FnMut(ScannedJava),
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        on_found(ScannedJava {
            path: self.home.clone(),
            major: JavaMajor::parse(21).expect("21"),
            version: "21.0.5".to_owned(),
            vendor: JavaVendor::Temurin,
        });
        Ok(())
    }
}

/// 假内容来源：候选、解析目标与哈希反查都由用例给定
struct FakeContentSource {
    candidates: Vec<ContentCandidate>,
    resolved: Vec<ResolvedContent>,
}

impl FakeContentSource {
    fn v1_then_v2(payload_len: u64, id: &str) -> Self {
        Self {
            candidates: vec![candidate("v2", "2.0.0", "2024-02-01T00:00:00Z")],
            resolved: vec![
                resolved_content(id, "v1", "1.0.0", "testmod-1.0.jar", payload_len),
                resolved_content(id, "v2", "2.0.0", "testmod-2.0.jar", payload_len),
            ],
        }
    }
}

impl ContentSourceInfo for FakeContentSource {
    fn name(&self) -> &'static str {
        "fake"
    }
}

#[async_trait]
impl ContentSource for FakeContentSource {
    fn supported_kinds(&self) -> Vec<ContentKind> {
        ContentKind::ALL.to_vec()
    }

    async fn search(&self, _query: &ContentQuery) -> Result<ContentSearch, SourceError> {
        Ok(ContentSearch {
            hits: Vec::<ContentProject>::new(),
            offset: 0,
            total: 0,
        })
    }

    async fn project(&self, id: &str) -> Result<ContentProject, SourceError> {
        Ok(project(id))
    }

    async fn candidates(
        &self,
        _id: &str,
        _request: &ContentRequest,
    ) -> Result<Vec<ContentCandidate>, SourceError> {
        Ok(self.candidates.clone())
    }

    async fn resolve(&self, id: &str, version: &str) -> Result<ResolvedContent, SourceError> {
        self.resolved
            .iter()
            .find(|resolved| resolved.project_id == id && resolved.version_id == version)
            .cloned()
            .ok_or(SourceError::Status { status: 404 })
    }

    async fn by_hash(&self, hashes: &[String]) -> Result<Vec<ContentIdentity>, SourceError> {
        // 只认识「已装的 v1」：更新判定据此知道当前版本，磁盘上没有别的文件
        Ok(hashes
            .iter()
            .map(|hash| ContentIdentity {
                hash: hash.clone(),
                project_id: "testmod".to_owned(),
                slug: Some("testmod".to_owned()),
                title: Some("Test Mod".to_owned()),
                version_id: "v1".to_owned(),
                version_number: "1.0.0".to_owned(),
                channel: "release".to_owned(),
                date_published: "2024-01-01T00:00:00Z".to_owned(),
                kind: Some(ContentKind::Mod),
            })
            .collect())
    }
}

fn candidate(version_id: &str, version_number: &str, date: &str) -> ContentCandidate {
    ContentCandidate {
        version_id: version_id.to_owned(),
        version_number: version_number.to_owned(),
        channel: "release".to_owned(),
        featured: false,
        loaders: vec!["fabric".to_owned()],
        game_versions: vec!["1.21.4".to_owned()],
        date_published: date.to_owned(),
        score: 0,
    }
}

fn resolved_content(
    project: &str,
    version_id: &str,
    version_number: &str,
    filename: &str,
    size: u64,
) -> ResolvedContent {
    ResolvedContent {
        project_id: project.to_owned(),
        version_id: version_id.to_owned(),
        version_number: version_number.to_owned(),
        file: ContentFile {
            location: Url::parse(&format!("http://127.0.0.1:1/{filename}")).expect("url"),
            filename: filename.to_owned(),
            // 0 = 不校验大小（假下载器写固定 payload）
            size,
            checksum: None,
        },
    }
}

fn project(id: &str) -> ContentProject {
    ContentProject {
        id: id.to_owned(),
        slug: Some(id.to_owned()),
        title: id.to_owned(),
        description: String::new(),
        kind: ContentKind::Mod,
        also_kinds: Vec::new(),
        loaders: vec!["fabric".to_owned()],
        game_versions: vec!["1.21.4".to_owned()],
        author: None,
        downloads: 0,
        icon_url: None,
        client_only: false,
    }
}

/// 假整合包来源：固定返回一份「最新版」解析结果
struct FakeModpackSource {
    resolved: ResolvedModpack,
}

impl FakeModpackSource {
    fn new() -> Self {
        Self {
            resolved: ResolvedModpack {
                project_id: "PACK".to_owned(),
                version_id: "PACKV".to_owned(),
                version_number: "1.0.0".to_owned(),
                // 0 字节：假下载器写固定 payload，不做大小校验
                file: ContentFile {
                    location: Url::parse("https://cdn.example.invalid/pack.mrpack").expect("url"),
                    filename: "pack.mrpack".to_owned(),
                    size: 0,
                    checksum: None,
                },
            },
        }
    }
}

impl ContentSourceInfo for FakeModpackSource {
    fn name(&self) -> &'static str {
        "fake"
    }
}

#[async_trait]
impl ModpackSource for FakeModpackSource {
    async fn search(&self, _query: &ModpackQuery) -> Result<ModpackSearch, SourceError> {
        Ok(ModpackSearch {
            hits: Vec::new(),
            offset: 0,
            total: 0,
        })
    }

    async fn detail(&self, _id: &str) -> Result<ModpackDetail, SourceError> {
        Err(SourceError::Status { status: 404 })
    }

    async fn versions(&self, _id: &str) -> Result<Vec<ModpackVersion>, SourceError> {
        Ok(Vec::new())
    }

    async fn resolve(
        &self,
        _id: &str,
        _version: Option<&str>,
    ) -> Result<ResolvedModpack, SourceError> {
        Ok(self.resolved.clone())
    }
}

/// 假清单读取器：清单与 overrides 由用例给定
struct FakeModpackExtractor {
    index: ModpackIndex,
    overrides: Vec<(&'static str, &'static [u8])>,
}

impl ModpackExtractor for FakeModpackExtractor {
    fn read_index(&self, _archive: &Path) -> Result<ModpackIndex, ModpackExtractError> {
        Ok(self.index.clone())
    }

    fn extract_overrides(&self, _archive: &Path, dest: &Path) -> Result<(), ModpackExtractError> {
        for (path, bytes) in &self.overrides {
            let target = dest.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| ModpackExtractError::Extract(err.to_string()))?;
            }
            std::fs::write(&target, bytes)
                .map_err(|err| ModpackExtractError::Extract(err.to_string()))?;
        }
        Ok(())
    }
}

/// 一份最小整合包清单：Fabric 1.21.1 + 一个配置文件
fn modpack_index() -> ModpackIndex {
    let mut raw = BTreeMap::new();
    raw.insert("minecraft".to_owned(), "1.21.1".to_owned());
    raw.insert("fabric-loader".to_owned(), "0.16.0".to_owned());
    ModpackIndex::new(
        1,
        "minecraft",
        "PACKV",
        "Pack",
        ModpackDependencies { raw },
        vec![ModpackFile::new(
            "config/x.toml".to_owned(),
            vec!["https://cdn.example.invalid/config/x.toml".to_owned()],
            0,
            ModpackHashes::default(),
            Some(FileEnv {
                client: domain_env(),
                server: domain_env(),
            }),
        )],
    )
    .expect("index")
}

/// `FileEnv` 的必装取值
fn domain_env() -> EnvSupport {
    EnvSupport::Required
}

/// 假 Java 来源：一个包、一个下载地址
struct FakeJavaRegistry {
    package: JavaPackage,
    resolved: ResolvedJava,
}

impl FakeJavaRegistry {
    fn new(major: JavaMajor) -> Self {
        let package = JavaPackage {
            id: "fake-jdk".to_owned(),
            vendor: JavaVendor::Temurin,
            major,
            version: "21.0.5+11".to_owned(),
            package_type: JavaPackageType::Jdk,
            expected_checksum: None,
        };
        let resolved = ResolvedJava {
            location: Url::parse("https://cdn.example.invalid/jdk.zip").expect("url"),
            package: package.clone(),
            expected_size: None,
            expected_checksum: None,
        };
        Self { package, resolved }
    }
}

#[async_trait]
impl JavaSourceRegistryPort for FakeJavaRegistry {
    fn source_names(&self) -> Vec<&'static str> {
        vec!["fake"]
    }

    async fn ranked(&self) -> Arc<dyn JavaSourceRegistryPort> {
        Arc::new(Self {
            package: self.package.clone(),
            resolved: self.resolved.clone(),
        })
    }

    async fn list_packages(
        &self,
        _request: &JavaPackageRequest,
    ) -> Result<Vec<JavaPackage>, SourceError> {
        Ok(vec![self.package.clone()])
    }

    async fn resolve_binary(&self, _package: &JavaPackage) -> Result<ResolvedJava, SourceError> {
        Ok(self.resolved.clone())
    }
}

/// 假解压器：在目标目录造一个 `bin/java`
struct FakeArchiveExtractor;

impl ArchiveExtractor for FakeArchiveExtractor {
    fn is_extractable(&self, _archive: &Path) -> bool {
        true
    }

    fn extract(&self, _archive: &Path, dest: &Path) -> Result<(), ExtractError> {
        let bin = dest.join("bin");
        std::fs::create_dir_all(&bin).map_err(|err| ExtractError(err.to_string()))?;
        std::fs::write(bin.join(java_exe_name()), b"stub")
            .map_err(|err| ExtractError(err.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------- 装配

/// 一套可改装配的夹具
struct Harness {
    dir: tempfile::TempDir,
    mcsup: Mcsup,
    downloader: Arc<FailingDownloader>,
}

/// 装配一套「不联网」的完整设施
///
/// `edit` 在默认装配（自定义下载器）之上追加来源 / supervisor / 扫描器；
/// 不写就是本仓库的默认实现。
fn harness(payload: Vec<u8>, edit: impl FnOnce(McsupBuilder) -> McsupBuilder) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let downloader = Arc::new(FailingDownloader::new().with_payload(payload));
    let as_port: Arc<dyn ArtifactDownloader> = downloader.clone();
    let builder = McsupBuilder::new().downloader(as_port);
    let mcsup = edit(builder).build(dir.path()).expect("bootstrap");
    Harness {
        dir,
        mcsup,
        downloader,
    }
}

/// 只关心「建服 + 核心」时的最简夹具
fn build(
    sources: Vec<Arc<dyn ServerJarSource>>,
) -> (tempfile::TempDir, Mcsup, Arc<FailingDownloader>) {
    let fixture = harness(jar_bytes(), |builder| builder.jar_sources(sources));
    (fixture.dir, fixture.mcsup, fixture.downloader)
}

/// 建一台服务器并返回它的目录
async fn create_server(mcsup: &Mcsup, dir: &Path, name: &str) -> PathBuf {
    create_server_with(mcsup, dir, name, ServerVariant::Vanilla).await
}

/// 按指定变体建服（内容测试用 Fabric）
async fn create_server_with(
    mcsup: &Mcsup,
    dir: &Path,
    name: &str,
    variant: ServerVariant,
) -> PathBuf {
    let workspace = dir.join("workspace");
    let created = mcsup
        .kit(KitConfig::default())
        .create_server(
            CreateServer::new(name)
                .workspace_dir(&workspace)
                .variant(variant)
                .version("1.21.4"),
        )
        .await
        .expect("create");
    created.core.server.dir
}

/// 内容测试统一在 Fabric 服务端上跑
async fn create_fabric_server(mcsup: &Mcsup, dir: &Path, name: &str) -> PathBuf {
    create_server_with(mcsup, dir, name, ServerVariant::Fabric).await
}

// ---------------------------------------------------------------- 建服 / 核心

/// 一行建服：目录 + marker + 入库 + 装核心，全部落地
#[tokio::test]
async fn create_server_registers_and_installs_core() {
    let (dir, mcsup, downloader) = build(vec![Arc::new(FixedSource::vanilla())]);
    let workspace_dir = dir.path().join("workspace");
    let created = mcsup
        .kit(KitConfig::default())
        .create_server(
            CreateServer::new("survival")
                .workspace_dir(&workspace_dir)
                .version("1.21.4"),
        )
        .await
        .expect("create");

    assert!(created.started.is_none(), "默认不起服");
    assert_eq!(created.core.server.meta.version.as_str(), "1.21.4");
    assert!(
        created.core.server.meta.jar.is_some(),
        "核心元数据必须回写注册表"
    );
    assert!(
        created.core.server.dir.join("server.jar").is_file(),
        "核心必须落到服务器目录"
    );
    assert_eq!(downloader.calls().len(), 1, "核心只下载一次");
    assert_eq!(
        mcsup
            .registry
            .get("survival")
            .expect("registered")
            .meta
            .name
            .as_str(),
        "survival"
    );
    assert!(workspace_dir.join("workspace.toml").is_file());
}

/// 未接受 EULA 却要求起服：在创建任何目录 / 记录之前就拒绝
#[tokio::test]
async fn create_with_start_refuses_unaccepted_eula() {
    let (dir, mcsup, downloader) = build(vec![Arc::new(FixedSource::vanilla())]);
    let workspace_dir = dir.path().join("workspace");
    let err = mcsup
        .kit(KitConfig::default())
        .create_server(
            CreateServer::new("survival")
                .workspace_dir(&workspace_dir)
                .start(true),
        )
        .await
        .expect_err("EULA 未接受");

    assert!(matches!(err, KitError::EulaNotAccepted));
    assert_eq!(err.category(), ErrorCategory::Usage);
    assert!(
        mcsup.registry.list().expect("list").is_empty(),
        "拒绝必须发生在副作用之前"
    );
    assert!(!workspace_dir.exists(), "工作目录也不该被创建");
    assert!(downloader.calls().is_empty(), "不该发起任何下载");
}

/// 本实例没有承载该变体的来源：不留下空目录与注册记录
#[tokio::test]
async fn unknown_variant_leaves_nothing_behind() {
    let (dir, mcsup, _downloader) = build(Vec::new());
    let workspace_dir = dir.path().join("workspace");
    let err = mcsup
        .kit(KitConfig::default())
        .create_server(
            CreateServer::new("survival")
                .variant(ServerVariant::Vanilla)
                .workspace_dir(&workspace_dir),
        )
        .await
        .expect_err("没有来源");

    assert!(matches!(err, KitError::UnsupportedVariant { .. }));
    assert_eq!(err.category(), ErrorCategory::Usage);
    assert!(!workspace_dir.exists());
    assert!(mcsup.registry.list().expect("list").is_empty());
}

/// 配置层的 EULA / 起服默认值会作用到单次调用
#[tokio::test]
async fn create_uses_config_defaults_for_start_and_eula() {
    let supervisor = Arc::new(SwitchableSupervisor::default());
    let home = PathBuf::from("/nonexistent/jdk-21");
    let fixture = harness(jar_bytes(), |builder| {
        builder
            .jar_sources(vec![Arc::new(FixedSource::vanilla())])
            .supervisor(supervisor.clone())
            .scanner(Arc::new(OneJavaScanner { home }))
    });
    let workspace_dir = fixture.dir.path().join("workspace");
    let kit = fixture.mcsup.kit(KitConfig {
        accept_eula: true,
        start_after_create: true,
        ..KitConfig::default()
    });

    let created = kit
        .create_server(
            CreateServer::new("survival")
                .workspace_dir(&workspace_dir)
                .version("1.21.4"),
        )
        .await
        .expect("create");

    assert!(created.started.is_some(), "配置里要求建完起服");
    assert_eq!(supervisor.starts(), 1);
    let eula = std::fs::read_to_string(created.core.server.dir.join("eula.txt")).expect("eula");
    assert!(eula.contains("eula=true"));
}

/// 换核心：缺省沿用注册表记录，显式版本会写回元数据
#[tokio::test]
async fn install_core_replaces_the_jar_and_updates_metadata() {
    let fixture = harness(jar_bytes(), |builder| {
        builder.jar_sources(vec![Arc::new(FixedSource::vanilla())])
    });
    let mcsup = &fixture.mcsup;
    let dir = fixture.dir.path();
    let server_dir = create_server(mcsup, dir, "survival").await;
    assert!(server_dir.join("server.jar").is_file());

    let installed = mcsup
        .kit(KitConfig::default())
        .install_core("survival", InstallCore::default().version("1.21.5"))
        .await
        .expect("reinstall");
    assert_eq!(installed.server.meta.version.as_str(), "1.21.5");
    assert!(server_dir.join("server.jar").is_file());
    assert_eq!(fixture.downloader.calls().len(), 2, "建服 + 换核心各一次");
}

/// 注册表里的变体在本实例没有来源：用法错误，不动盘
#[tokio::test]
async fn install_core_rejects_variant_without_source() {
    let (dir, mcsup, _downloader) = build(vec![Arc::new(FixedSource::fabric())]);
    let server_dir =
        create_server_with(&mcsup, dir.path(), "survival", ServerVariant::Fabric).await;
    let before = std::fs::read(server_dir.join("server.jar")).expect("jar");

    // 同一份数据目录、显式注入空来源集合的实例：变体查不到来源
    // （不注入时 builder 会装真实 HTTP 来源，测试会联网）
    let bare = McsupBuilder::new()
        .jar_sources(Vec::new())
        .build(dir.path())
        .expect("bootstrap");
    let err = bare
        .kit(KitConfig::default())
        .install_core("survival", InstallCore::default())
        .await
        .expect_err("没有 Fabric 来源");
    assert!(matches!(err, KitError::UnsupportedVariant { .. }));
    assert_eq!(err.category(), ErrorCategory::Usage);
    assert_eq!(
        std::fs::read(server_dir.join("server.jar")).expect("jar"),
        before,
        "拒绝必须发生在换核心之前"
    );
}

/// 默认 supervisor 需要可执行文件：缺了要在拉起之前指名报错
#[test]
fn start_requires_supervisor_exe_for_default_supervisor() {
    let (_dir, mcsup, _downloader) = build(vec![Arc::new(FixedSource::vanilla())]);
    let err = mcsup
        .kit(KitConfig::default())
        .start_server("survival", StartServer::default())
        .expect_err("缺 supervisor_exe");
    assert!(matches!(err, KitError::MissingSupervisorExe));
    assert_eq!(err.category(), ErrorCategory::Usage);
}

// ---------------------------------------------------------------- 生命周期

/// 起服 → 查状态 → 重启 → 停服：同一套 supervisor 状态贯穿
#[tokio::test]
async fn start_status_restart_and_stop_share_one_supervisor() {
    let supervisor = Arc::new(SwitchableSupervisor::default());
    let home = PathBuf::from("/nonexistent/jdk-21");
    let fixture = harness(jar_bytes(), |builder| {
        builder
            .jar_sources(vec![Arc::new(FixedSource::vanilla())])
            .supervisor(supervisor.clone())
            .scanner(Arc::new(OneJavaScanner { home }))
    });
    let mcsup = &fixture.mcsup;
    let dir = fixture.dir.path();
    create_server(mcsup, dir, "survival").await;
    let kit = mcsup.kit(KitConfig {
        accept_eula: true,
        ..KitConfig::default()
    });

    assert!(kit.server_status("survival").expect("status").is_none());
    kit.start_server("survival", StartServer::default())
        .expect("start");
    let running = kit
        .server_status("survival")
        .expect("status")
        .expect("running");
    assert_eq!(running.status, ServerStatus::Running);
    assert_eq!(supervisor.starts(), 1);

    kit.restart_server("survival", StartServer::default(), StopServer::default())
        .expect("restart");
    assert_eq!(supervisor.starts(), 2, "重启要真的再拉起一次");
    assert_eq!(supervisor.stops(), 1);

    let stopped = kit
        .stop_server("survival", StopServer::default())
        .expect("stop");
    assert_eq!(
        stopped,
        StopOutcome::Stopped {
            elapsed: Duration::from_millis(1)
        }
    );
    assert!(kit.server_status("survival").expect("status").is_none());
    // 幂等：已经停了再停一次不报错
    assert_eq!(
        kit.stop_server("survival", StopServer::default())
            .expect("stop again"),
        StopOutcome::AlreadyStopped
    );
}

/// 运行中删服：由 service 拒绝，注册记录与目录都保留
#[tokio::test]
async fn remove_running_server_is_rejected() {
    let supervisor = Arc::new(SwitchableSupervisor::default());
    let fixture = harness(jar_bytes(), |builder| {
        builder
            .jar_sources(vec![Arc::new(FixedSource::vanilla())])
            .supervisor(supervisor.clone())
    });
    let mcsup = &fixture.mcsup;
    let server_dir = create_server(mcsup, fixture.dir.path(), "survival").await;
    // 建服要装核心，运行中会被拒；先建好再宣布它在跑
    supervisor.set_alive(true);

    let err = mcsup
        .kit(KitConfig::default())
        .remove_server(
            "survival",
            RemoveServer::default().purge(true).cwd(fixture.dir.path()),
        )
        .expect_err("运行中删除必须被拒");
    assert_eq!(err.category(), ErrorCategory::Conflict);
    assert!(server_dir.is_dir());
    assert!(mcsup.registry.get("survival").is_ok());
}

/// 只注销登记：目录保留，可以再 link 回来
#[tokio::test]
async fn remove_without_purge_keeps_the_directory() {
    let fixture = harness(jar_bytes(), |builder| {
        builder.jar_sources(vec![Arc::new(FixedSource::vanilla())])
    });
    let mcsup = &fixture.mcsup;
    let server_dir = create_server(mcsup, fixture.dir.path(), "survival").await;

    let outcome = mcsup
        .kit(KitConfig::default())
        .remove_server("survival", RemoveServer::default().cwd(fixture.dir.path()))
        .expect("remove");
    assert!(!outcome.disk_purged);
    assert!(server_dir.is_dir(), "不 purge 时必须保留数据");
    assert!(mcsup.registry.get("survival").is_err());
}

/// 带 purge 删除：注册记录与目录一起消失
#[tokio::test]
async fn remove_with_purge_deletes_the_directory() {
    let fixture = harness(jar_bytes(), |builder| {
        builder.jar_sources(vec![Arc::new(FixedSource::vanilla())])
    });
    let mcsup = &fixture.mcsup;
    let server_dir = create_server(mcsup, fixture.dir.path(), "survival").await;

    let outcome = mcsup
        .kit(KitConfig::default())
        .remove_server(
            "survival",
            RemoveServer::default().purge(true).cwd(fixture.dir.path()),
        )
        .expect("remove");
    assert!(outcome.disk_purged);
    assert!(!server_dir.exists());
    assert!(mcsup.registry.get("survival").is_err());
}

// ---------------------------------------------------------------- 备份

/// 建快照 → 列表 → 校验 → 恢复 → 删除 → 保留策略清理
#[tokio::test]
async fn backup_lifecycle_runs_through_kit() {
    let fixture = harness(jar_bytes(), |builder| {
        builder.jar_sources(vec![Arc::new(FixedSource::vanilla())])
    });
    let mcsup = &fixture.mcsup;
    let server_dir = create_server(mcsup, fixture.dir.path(), "survival").await;
    std::fs::create_dir_all(server_dir.join("world")).expect("world");
    std::fs::write(server_dir.join("world/level.dat"), b"first").expect("level");
    let kit = mcsup.kit(KitConfig::default());

    let first = kit
        .backup_now("survival", BackupNow::default().label("before-upgrade"))
        .expect("backup");
    assert_eq!(first.skipped, 0);
    std::fs::write(server_dir.join("world/level.dat"), b"second").expect("level");
    let second = kit
        .backup_now("survival", BackupNow::default())
        .expect("backup");

    let page = kit
        .list_backups("survival", ListBackups::default())
        .expect("list");
    assert_eq!(page.total, 2);
    assert_eq!(page.items.len(), 2);
    assert!(
        page.items.iter().any(|item| item.id == second.snapshot.id),
        "列表必须包含刚建的那份"
    );

    let verified = kit
        .verify_backup("survival", first.snapshot.id.as_str())
        .expect("verify");
    assert!(verified.broken.is_empty());
    assert!(verified.objects > 0);

    let restored = kit
        .restore_backup(
            "survival",
            first.snapshot.id.as_str(),
            RestoreBackup::default(),
        )
        .expect("restore");
    assert_eq!(
        std::fs::read(server_dir.join("world/level.dat")).expect("level"),
        b"first"
    );
    // 原地恢复前会自动留一份安全快照
    let after_restore = kit
        .list_backups("survival", ListBackups::default())
        .expect("list");
    assert_eq!(after_restore.total, 3);
    assert_eq!(restored.snapshot.id, first.snapshot.id);
    let safety = restored.safety_snapshot.expect("原地恢复必须先留安全快照");
    assert_ne!(safety, first.snapshot.id);

    let deleted = kit
        .delete_backup("survival", second.snapshot.id.as_str())
        .expect("delete");
    assert_eq!(deleted.snapshot.id, second.snapshot.id);

    let pruned = kit
        .prune_backups("survival", PruneBackups::default().keep(1))
        .expect("prune");
    assert_eq!(pruned.removed.len(), 1);
    let final_page = kit
        .list_backups("survival", ListBackups::default())
        .expect("list");
    assert_eq!(final_page.total, 1);
}

/// 保留策略冲突交给 service 判定，kit 不抢先择一
#[tokio::test]
async fn prune_with_conflicting_retention_is_rejected() {
    let fixture = harness(jar_bytes(), |builder| {
        builder.jar_sources(vec![Arc::new(FixedSource::vanilla())])
    });
    let mcsup = &fixture.mcsup;
    create_server(mcsup, fixture.dir.path(), "survival").await;

    let err = mcsup
        .kit(KitConfig::default())
        .prune_backups(
            "survival",
            PruneBackups::default()
                .keep(1)
                .older_than(Duration::from_secs(86_400)),
        )
        .expect_err("两个策略不能同时给");
    assert_eq!(err.category(), ErrorCategory::Usage);
}

/// 分页参数真的传到 service
#[tokio::test]
async fn list_backups_honours_page_size() {
    let fixture = harness(jar_bytes(), |builder| {
        builder.jar_sources(vec![Arc::new(FixedSource::vanilla())])
    });
    let mcsup = &fixture.mcsup;
    create_server(mcsup, fixture.dir.path(), "survival").await;
    let kit = mcsup.kit(KitConfig::default());
    for _ in 0..3 {
        kit.backup_now("survival", BackupNow::default())
            .expect("backup");
    }

    let page = kit
        .list_backups("survival", ListBackups::default().size(2))
        .expect("list");
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.total, 3);
}

// ---------------------------------------------------------------- 内容

/// 一行装 mod：计划与落盘一致，文件真的在 `mods/` 下
#[tokio::test]
async fn install_mod_lands_in_the_mods_directory() {
    let payload = mod_jar_bytes("testmod", "1.0.0");
    let fixture = harness(payload, |builder| {
        builder
            .jar_sources(vec![Arc::new(FixedSource::fabric())])
            .content_sources(vec![Arc::new(FakeContentSource::v1_then_v2(0, "testmod"))])
    });
    let mcsup = &fixture.mcsup;
    let server_dir = create_fabric_server(mcsup, fixture.dir.path(), "survival").await;
    let kit = mcsup.kit(KitConfig::default());

    let outcome = kit
        .install_mod("survival", "testmod", InstallMod::default())
        .await
        .expect("install");
    assert_eq!(outcome.plan.version_id, "v2", "没给版本时取最新候选");
    assert_eq!(outcome.plan.file, "testmod-2.0.jar");
    assert!(outcome.plan.replaces.is_none());
    let applied = outcome.applied.expect("dry_run 之外的调用必须落盘");
    assert_eq!(applied.path, server_dir.join("mods/testmod-2.0.jar"));
    assert!(server_dir.join("mods/testmod-2.0.jar").is_file());
    let staging = server_dir.join(".mcsup-staging");
    let leftovers = std::fs::read_dir(&staging).map_or(0, Iterator::count);
    assert_eq!(leftovers, 0, "暂存目录用完必须清掉");
}

/// 预览只出计划：不下载、不落盘
#[tokio::test]
async fn install_mod_dry_run_does_not_touch_disk() {
    let payload = mod_jar_bytes("testmod", "1.0.0");
    let fixture = harness(payload, |builder| {
        builder
            .jar_sources(vec![Arc::new(FixedSource::fabric())])
            .content_sources(vec![Arc::new(FakeContentSource::v1_then_v2(0, "testmod"))])
    });
    let mcsup = &fixture.mcsup;
    let server_dir = create_fabric_server(mcsup, fixture.dir.path(), "survival").await;
    let before = fixture.downloader.calls().len();

    let outcome = mcsup
        .kit(KitConfig::default())
        .install_mod(
            "survival",
            "testmod",
            InstallMod::default().dry_run(true).version("v1"),
        )
        .await
        .expect("plan");
    assert!(outcome.applied.is_none());
    assert_eq!(outcome.plan.version_id, "v1");
    assert_eq!(fixture.downloader.calls().len(), before, "预览不下载");
    assert!(!server_dir.join("mods").exists());
}

/// 检查并更新：旧文件删除、新文件落盘，计划如实给出原 / 新版本
#[tokio::test]
async fn update_mods_replaces_the_old_file() {
    let payload = mod_jar_bytes("testmod", "1.0.0");
    let fixture = harness(payload, |builder| {
        builder
            .jar_sources(vec![Arc::new(FixedSource::fabric())])
            .content_sources(vec![Arc::new(FakeContentSource::v1_then_v2(0, "testmod"))])
    });
    let mcsup = &fixture.mcsup;
    let server_dir = create_fabric_server(mcsup, fixture.dir.path(), "survival").await;
    let kit = mcsup.kit(KitConfig::default());
    kit.install_mod("survival", "testmod", InstallMod::default().version("v1"))
        .await
        .expect("install v1");
    assert!(server_dir.join("mods/testmod-1.0.jar").is_file());

    let outcome = kit
        .update_mods("survival", UpdateMods::default())
        .await
        .expect("update");
    assert_eq!(outcome.plan.items.len(), 1, "已装的 v1 应当有更新");
    assert_eq!(outcome.plan.items[0].from_version, "1.0.0");
    assert_eq!(outcome.plan.items[0].to_version, "2.0.0");
    let applied = outcome.applied.expect("非 dry_run 必须落盘");
    assert_eq!(applied.changes.len(), 1);

    assert!(server_dir.join("mods/testmod-2.0.jar").is_file());
    assert!(
        !server_dir.join("mods/testmod-1.0.jar").exists(),
        "同名 modid 的旧文件必须删掉"
    );
}

/// 更新预览不落盘
#[tokio::test]
async fn update_mods_dry_run_reports_the_plan_only() {
    let payload = mod_jar_bytes("testmod", "1.0.0");
    let fixture = harness(payload, |builder| {
        builder
            .jar_sources(vec![Arc::new(FixedSource::fabric())])
            .content_sources(vec![Arc::new(FakeContentSource::v1_then_v2(0, "testmod"))])
    });
    let mcsup = &fixture.mcsup;
    let server_dir = create_fabric_server(mcsup, fixture.dir.path(), "survival").await;
    let kit = mcsup.kit(KitConfig::default());
    kit.install_mod("survival", "testmod", InstallMod::default().version("v1"))
        .await
        .expect("install v1");

    let outcome = kit
        .update_mods("survival", UpdateMods::default().dry_run(true))
        .await
        .expect("plan update");
    assert!(outcome.applied.is_none());
    assert!(server_dir.join("mods/testmod-1.0.jar").is_file());
    assert!(!server_dir.join("mods/testmod-2.0.jar").exists());
}

/// 移除：文件名精确命中，文件消失、服务端记录不受影响
#[tokio::test]
async fn remove_mod_deletes_the_selected_file() {
    let payload = mod_jar_bytes("testmod", "1.0.0");
    let fixture = harness(payload, |builder| {
        builder
            .jar_sources(vec![Arc::new(FixedSource::fabric())])
            .content_sources(vec![Arc::new(FakeContentSource::v1_then_v2(0, "testmod"))])
    });
    let mcsup = &fixture.mcsup;
    let server_dir = create_fabric_server(mcsup, fixture.dir.path(), "survival").await;
    let kit = mcsup.kit(KitConfig::default());
    kit.install_mod("survival", "testmod", InstallMod::default().version("v1"))
        .await
        .expect("install");

    let preview = kit
        .remove_mod(
            "survival",
            "testmod-1.0.jar",
            RemoveMod::default().dry_run(true),
        )
        .expect("preview");
    assert!(preview.dry_run);
    assert!(
        server_dir.join("mods/testmod-1.0.jar").is_file(),
        "预览不动盘"
    );

    let removed = kit
        .remove_mod("survival", "testmod-1.0.jar", RemoveMod::default())
        .expect("remove");
    assert_eq!(removed.files, vec![server_dir.join("mods/testmod-1.0.jar")]);
    assert!(!removed.dry_run);
    assert!(!server_dir.join("mods/testmod-1.0.jar").exists());
}

// ---------------------------------------------------------------- 整合包

/// 按整合包建服：清单决定 MC 版本与加载器，overrides 与清单文件都落地
#[tokio::test]
async fn create_server_from_modpack_uses_the_manifest() {
    let fixture = harness(jar_bytes(), |builder| {
        builder
            .jar_sources(vec![Arc::new(FixedSource::fabric())])
            .modpack_sources(vec![Arc::new(FakeModpackSource::new())])
            .modpack_extractor(Arc::new(FakeModpackExtractor {
                index: modpack_index(),
                overrides: vec![("server.properties", b"motd=pack\n")],
            }))
    });
    let mcsup = &fixture.mcsup;
    let workspace_dir = fixture.dir.path().join("workspace");
    let created = mcsup
        .kit(KitConfig::default())
        .create_server(
            CreateServer::new("packed")
                .workspace_dir(&workspace_dir)
                .modpack("all-the-mods"),
        )
        .await
        .expect("create from modpack");

    assert_eq!(created.core.server.meta.version.as_str(), "1.21.1");
    assert_eq!(created.core.server.meta.variant, ServerVariant::Fabric);
    let dir = &created.core.server.dir;
    assert!(dir.join("server.jar").is_file(), "加载器核心必须装好");
    assert_eq!(
        std::fs::read_to_string(dir.join("server.properties")).expect("overrides"),
        "motd=pack\n"
    );
    assert!(dir.join("config/x.toml").is_file(), "清单文件必须下载");
    // 暂存目录由守卫删除
    let staging =
        std::env::temp_dir().join(format!("mcsup-kit-modpack-{}-packed", std::process::id()));
    assert!(!staging.exists(), "暂存目录必须清掉");
}

/// 整合包模式给了矛盾的显式变体：在取回与落库之前拒绝
#[tokio::test]
async fn modpack_mode_rejects_a_conflicting_variant() {
    let fixture = harness(jar_bytes(), |builder| {
        builder
            .jar_sources(vec![Arc::new(FixedSource::fabric())])
            .modpack_sources(vec![Arc::new(FakeModpackSource::new())])
            .modpack_extractor(Arc::new(FakeModpackExtractor {
                index: modpack_index(),
                overrides: Vec::new(),
            }))
    });
    let mcsup = &fixture.mcsup;
    let workspace_dir = fixture.dir.path().join("workspace");
    let err = mcsup
        .kit(KitConfig::default())
        .create_server(
            CreateServer::new("packed")
                .workspace_dir(&workspace_dir)
                .modpack("all-the-mods")
                .variant(ServerVariant::Paper),
        )
        .await
        .expect_err("矛盾变体");

    assert!(matches!(err, KitError::ConflictingVariant { .. }));
    assert_eq!(err.category(), ErrorCategory::Usage);
    assert!(mcsup.registry.list().expect("list").is_empty());
    assert!(!workspace_dir.exists());
}

/// 没给包版本时交给来源挑，不把配置里的 `latest` 当成包版本 ID
#[tokio::test]
async fn modpack_version_defaults_to_the_source_pick() {
    let fixture = harness(jar_bytes(), |builder| {
        builder
            .jar_sources(vec![Arc::new(FixedSource::fabric())])
            .modpack_sources(vec![Arc::new(FakeModpackSource::new())])
            .modpack_extractor(Arc::new(FakeModpackExtractor {
                index: modpack_index(),
                overrides: Vec::new(),
            }))
    });
    let mcsup = &fixture.mcsup;
    let workspace_dir = fixture.dir.path().join("workspace");
    let created = mcsup
        .kit(KitConfig {
            default_version: "latest".to_owned(),
            ..KitConfig::default()
        })
        .create_server(
            CreateServer::new("packed")
                .workspace_dir(&workspace_dir)
                .modpack("all-the-mods"),
        )
        .await
        .expect("create");
    assert_eq!(created.core.server.meta.version.as_str(), "1.21.1");
}

/// 把整合包装到已有服务器（建服中途失败后的重试入口）：只布置，不重建
#[tokio::test]
async fn install_modpack_onto_existing_server_layouts_content() {
    let fixture = harness(jar_bytes(), |builder| {
        builder
            .jar_sources(vec![Arc::new(FixedSource::fabric())])
            .modpack_sources(vec![Arc::new(FakeModpackSource::new())])
            .modpack_extractor(Arc::new(FakeModpackExtractor {
                index: modpack_index(),
                overrides: vec![("server.properties", b"motd=pack\n")],
            }))
    });
    let mcsup = &fixture.mcsup;
    // 先建一台普通服，再往它上面装整合包
    let server_dir = create_fabric_server(mcsup, fixture.dir.path(), "survival").await;
    let before = fixture.downloader.calls().len();

    let installed = mcsup
        .kit(KitConfig::default())
        .install_modpack(
            "survival",
            "all-the-mods",
            mcsup::kit::InstallModpack::default(),
        )
        .await
        .expect("install modpack");

    assert_eq!(
        installed.variant,
        ServerVariant::Fabric,
        "清单加载器决定变体"
    );
    assert_eq!(installed.minecraft, "1.21.1");
    assert!(
        server_dir.join("config/x.toml").is_file(),
        "清单里的服务端文件必须落盘"
    );
    assert_eq!(
        std::fs::read_to_string(server_dir.join("server.properties")).expect("overrides"),
        "motd=pack\n"
    );
    assert_eq!(
        mcsup
            .registry
            .get("survival")
            .expect("server")
            .meta
            .version
            .as_str(),
        "1.21.1",
        "core 元数据必须写回清单里的 MC 版本"
    );
    // 重试口径：清单归档 + 加载器核心 + 清单里的一个文件，各下载一次
    assert_eq!(
        fixture.downloader.calls().len() - before,
        3,
        "整合包只下清单、核心与清单文件各一次"
    );
    let staging =
        std::env::temp_dir().join(format!("mcsup-kit-modpack-{}-survival", std::process::id()));
    assert!(!staging.exists(), "暂存目录必须清掉");
}

/// 装整合包的服务器不存在：装核心那一步如实报错，不留半成品
#[tokio::test]
async fn install_modpack_onto_missing_server_is_rejected() {
    let fixture = harness(jar_bytes(), |builder| {
        builder
            .jar_sources(vec![Arc::new(FixedSource::fabric())])
            .modpack_sources(vec![Arc::new(FakeModpackSource::new())])
            .modpack_extractor(Arc::new(FakeModpackExtractor {
                index: modpack_index(),
                overrides: Vec::new(),
            }))
    });
    let err = fixture
        .mcsup
        .kit(KitConfig::default())
        .install_modpack(
            "no-such-server",
            "all-the-mods",
            mcsup::kit::InstallModpack::default(),
        )
        .await
        .expect_err("服务器不存在");
    // 服务器不存在在取回之前就查出，不会先下一个整包才报错
    assert!(matches!(err, KitError::Registry(_)));
    assert_eq!(err.category(), ErrorCategory::NotFound);
    assert!(
        fixture.downloader.calls().is_empty(),
        "拒绝必须发生在任何下载之前"
    );
}

// ---------------------------------------------------------------- Java

/// 没给大版本：用法错误，不猜默认值
#[tokio::test]
async fn install_java_requires_a_major_version() {
    let fixture = harness(b"jdk".to_vec(), |builder| builder);
    let err = fixture
        .mcsup
        .kit(KitConfig::default())
        .install_java(InstallJava::default())
        .await
        .expect_err("缺大版本");
    assert!(matches!(err, KitError::MissingJavaMajor));
    assert_eq!(err.category(), ErrorCategory::Usage);
}

/// 装 Java：下载 → 解压 → 验出 JVM home → 注册，之后能在列表里看到
#[tokio::test]
async fn install_java_registers_a_managed_runtime() {
    let fixture = harness(b"jdk archive".to_vec(), |builder| {
        builder
            .java_registry(Arc::new(FakeJavaRegistry::new(
                JavaMajor::parse(21).expect("21"),
            )))
            .extractor(Arc::new(FakeArchiveExtractor))
    });
    let kit = fixture.mcsup.kit(KitConfig::default());

    let outcome = kit
        .install_java(InstallJava::default().major(JavaMajor::parse(21).expect("21")))
        .await
        .expect("install java");
    assert_eq!(outcome.runtime.major, JavaMajor::parse(21).expect("21"));
    assert_eq!(outcome.runtime.vendor, JavaVendor::Temurin);
    assert!(outcome.runtime.managed);
    assert!(
        outcome
            .runtime
            .path
            .join("bin")
            .join(java_exe_name())
            .is_file(),
        "安装目录必须是一个可用的 JVM home"
    );

    let runtimes = kit.java_runtimes().expect("list");
    assert!(
        runtimes
            .iter()
            .any(|runtime: &JavaRuntime| runtime.path == outcome.runtime.path),
        "安装结果必须进运行时注册表"
    );
}

/// 配置里的 Java 默认值会作用到单次调用
#[tokio::test]
async fn install_java_uses_config_defaults() {
    let fixture = harness(b"jdk archive".to_vec(), |builder| {
        builder
            .java_registry(Arc::new(FakeJavaRegistry::new(
                JavaMajor::parse(21).expect("21"),
            )))
            .extractor(Arc::new(FakeArchiveExtractor))
    });
    let kit = fixture.mcsup.kit(KitConfig {
        java_major: Some(JavaMajor::parse(21).expect("21")),
        java_package_type: JavaPackageType::Jre,
        java_vendor: Some(JavaVendor::Temurin),
        ..KitConfig::default()
    });

    let outcome = kit
        .install_java(InstallJava::default())
        .await
        .expect("install");
    assert_eq!(outcome.runtime.major, JavaMajor::parse(21).expect("21"));
    assert_eq!(outcome.runtime.vendor, JavaVendor::Temurin);
}

// ---------------------------------------------------------------- 工作目录

/// 工作目录初始化可重复执行：第二次不换 id、不重建目录
#[test]
fn ensure_workspace_is_idempotent() {
    let (_dir, mcsup, _downloader) = build(Vec::new());
    let workspace_dir = tempfile::tempdir().expect("workspace");
    let kit = mcsup.kit(KitConfig::default());
    let first = kit
        .ensure_workspace(workspace_dir.path(), Some("main"))
        .expect("init");
    let second = kit
        .ensure_workspace(workspace_dir.path(), Some("main"))
        .expect("again");
    assert_eq!(first.id, second.id);
    assert_eq!(second.name.as_str(), "main");
}

/// 同名服务器第二次创建是状态冲突，不是静默覆盖
#[tokio::test]
async fn creating_the_same_name_twice_is_a_conflict() {
    let (dir, mcsup, _downloader) = build(vec![Arc::new(FixedSource::vanilla())]);
    let workspace_dir: PathBuf = dir.path().join("workspace");
    let kit = mcsup.kit(KitConfig::default());
    kit.create_server(
        CreateServer::new("survival")
            .workspace_dir(&workspace_dir)
            .version("1.21.4"),
    )
    .await
    .expect("first");
    let err = kit
        .create_server(
            CreateServer::new("survival")
                .workspace_dir(&workspace_dir)
                .version("1.21.4"),
        )
        .await
        .expect_err("second");
    assert_eq!(err.category(), ErrorCategory::Conflict);
}
