#![allow(
    clippy::unreachable,
    reason = "测试替身用 unreachable! 标记不该被调用的分支"
)]
use super::*;
use domain::java::{JavaMajor, JavaRuntime, JavaVendor, java_bin_under};
use domain::server::artifact::{Checksum, ChecksumAlgorithm};
use domain::server::meta::InstalledJar;
use domain::server::version::{ServerVariant, VersionId};
use domain::server::world::{DataVersion, WorldInfo};
use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
use port::{
    ArchiveExtractor, ArtifactDownloader, Backend, CancellationToken, CoreInspector, DownloadError,
    DownloadRequest, DownloadedArtifact, EventSink, ExtractError, InspectError, InstallerRunner,
    JavaPackageRequest, JavaRuntimeRepository, JavaScanner, JavaSourceRegistryPort, ProgressSink,
    RepositoryError, ServerJarSource, ServerRepository, SourceError, SupervisorPort,
    WorkspaceRepository,
};
use std::io::Write as _;
use std::sync::Mutex;

/// 单 JAR 启动规格（相对服务器目录）
fn mcsup_launch_jar(path: &str) -> domain::server::launch::LaunchSpec {
    domain::server::launch::LaunchSpec::jar(path)
}

// ---- 注册表 / Java 测试替身 ----

struct MemServers {
    items: Mutex<Vec<domain::server::meta::ServerMeta>>,
    /// `Some` 时把 `list_sourced` 的条目标成该远端来源（模拟镜像条目）
    origin: Mutex<Option<port::Origin>>,
}

impl MemServers {
    fn new() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
            origin: Mutex::new(None),
        }
    }

    /// 把后续 `list_sourced` 的结果都标成远端来源
    fn set_remote_origin(&self, remote: &str) {
        *self.origin.lock().expect("lock") = Some(port::Origin::Remote(
            port::RemoteName::parse(remote).expect("remote name"),
        ));
    }
}

impl ServerRepository for MemServers {
    fn create(&self, meta: &domain::server::meta::ServerMeta) -> Result<(), RepositoryError> {
        self.items.lock().expect("lock").push(meta.clone());
        Ok(())
    }
    fn get(
        &self,
        name: &domain::server::meta::ServerName,
    ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
        self.items
            .lock()
            .expect("lock")
            .iter()
            .find(|m| m.name == *name)
            .cloned()
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
    }
    fn list(&self) -> Result<Vec<domain::server::meta::ServerMeta>, RepositoryError> {
        Ok(self.items.lock().expect("lock").clone())
    }
    fn list_sourced(
        &self,
    ) -> Result<Vec<port::Sourced<domain::server::meta::ServerMeta>>, RepositoryError> {
        let items = self.items.lock().expect("lock").clone();
        let origin = self.origin.lock().expect("lock").clone();
        Ok(items
            .into_iter()
            .map(|meta| match origin.clone() {
                Some(origin) => port::Sourced::new(origin, meta),
                None => port::Sourced::local(meta),
            })
            .collect())
    }
    fn update_core(
        &self,
        name: &domain::server::meta::ServerName,
        version: &VersionId,
        jar: &InstalledJar,
        _launch: Option<&domain::server::launch::LaunchSpec>,
        required_java_major: Option<JavaMajor>,
        updated_at: i64,
    ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
        let mut items = self.items.lock().expect("lock");
        let meta = items
            .iter_mut()
            .find(|m| m.name == *name)
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
        meta.version = version.clone();
        meta.jar = Some(jar.clone());
        meta.java_major = required_java_major;
        meta.updated_at = updated_at;
        Ok(meta.clone())
    }
    fn update_settings(
        &self,
        name: &domain::server::meta::ServerName,
        settings: &domain::server::settings::ServerSettings,
        updated_at: i64,
    ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
        let mut items = self.items.lock().expect("lock");
        let meta = items
            .iter_mut()
            .find(|m| m.name == *name)
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
        meta.settings = settings.clone();
        meta.updated_at = updated_at;
        Ok(meta.clone())
    }
    fn remove(&self, name: &domain::server::meta::ServerName) -> Result<(), RepositoryError> {
        let mut items = self.items.lock().expect("lock");
        let before = items.len();
        items.retain(|m| m.name != *name);
        if items.len() == before {
            return Err(RepositoryError::NotFound(name.as_str().to_owned()));
        }
        Ok(())
    }
    fn remove_by_workspace(&self, _workspace_id: &WorkspaceId) -> Result<(), RepositoryError> {
        Ok(())
    }
    fn relink(
        &self,
        _name: &domain::server::meta::ServerName,
        _workspace_id: &WorkspaceId,
        _rel_dir: &Path,
        _updated_at: i64,
    ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
        unreachable!("not used in tests")
    }
}

struct MemWorkspaces {
    items: Mutex<Vec<Workspace>>,
}

impl MemWorkspaces {
    fn new(ws: Workspace) -> Self {
        Self {
            items: Mutex::new(vec![ws]),
        }
    }
}

impl WorkspaceRepository for MemWorkspaces {
    fn create(&self, ws: &Workspace) -> Result<(), RepositoryError> {
        self.items.lock().expect("lock").push(ws.clone());
        Ok(())
    }
    fn get(&self, id: &WorkspaceId) -> Result<Workspace, RepositoryError> {
        self.items
            .lock()
            .expect("lock")
            .iter()
            .find(|w| w.id == *id)
            .cloned()
            .ok_or_else(|| RepositoryError::NotFound(id.as_str().to_owned()))
    }
    fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError> {
        self.items
            .lock()
            .expect("lock")
            .iter()
            .find(|w| w.name == *name)
            .cloned()
            .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
    }
    fn list(&self) -> Result<Vec<Workspace>, RepositoryError> {
        Ok(self.items.lock().expect("lock").clone())
    }
    fn current(&self) -> Result<Option<Workspace>, RepositoryError> {
        Ok(self
            .items
            .lock()
            .expect("lock")
            .iter()
            .find(|w| w.is_current)
            .cloned())
    }
    fn set_current(&self, _id: &WorkspaceId) -> Result<(), RepositoryError> {
        Ok(())
    }
    fn remove(&self, _id: &WorkspaceId) -> Result<(), RepositoryError> {
        Ok(())
    }
    fn relink_path(
        &self,
        _id: &WorkspaceId,
        _path: &Path,
        _updated_at: i64,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
}

struct MemJavaRepo {
    runtimes: Mutex<Vec<JavaRuntime>>,
    scanned: Mutex<Option<i64>>,
}

impl MemJavaRepo {
    fn new(runtimes: Vec<JavaRuntime>, scanned: bool) -> Self {
        Self {
            runtimes: Mutex::new(runtimes),
            scanned: Mutex::new(if scanned { Some(1) } else { None }),
        }
    }
}

impl JavaRuntimeRepository for MemJavaRepo {
    fn upsert(&self, runtime: &JavaRuntime) -> Result<(), RepositoryError> {
        let mut list = self.runtimes.lock().expect("lock");
        list.retain(|r| r.path != runtime.path);
        list.push(runtime.clone());
        Ok(())
    }
    fn list(&self) -> Result<Vec<JavaRuntime>, RepositoryError> {
        Ok(self.runtimes.lock().expect("lock").clone())
    }
    fn get_by_path(&self, path: &Path) -> Result<JavaRuntime, RepositoryError> {
        self.runtimes
            .lock()
            .expect("lock")
            .iter()
            .find(|r| r.path == path)
            .cloned()
            .ok_or_else(|| RepositoryError::NotFound(path.display().to_string()))
    }
    fn remove(&self, path: &Path) -> Result<(), RepositoryError> {
        let mut list = self.runtimes.lock().expect("lock");
        let before = list.len();
        list.retain(|r| r.path != path);
        if list.len() == before {
            return Err(RepositoryError::NotFound(path.display().to_string()));
        }
        Ok(())
    }
    fn scan_state(&self) -> Result<Option<i64>, RepositoryError> {
        Ok(*self.scanned.lock().expect("lock"))
    }
    fn mark_scanned(&self, at: i64) -> Result<(), RepositoryError> {
        *self.scanned.lock().expect("lock") = Some(at);
        Ok(())
    }
}

/// 首次扫描上报固定候选项（用于验证 start 触发扫描）
struct FixedScanner {
    homes: Vec<PathBuf>,
}

impl JavaScanner for FixedScanner {
    fn scan(
        &self,
        on_found: &mut dyn FnMut(ScannedJava),
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for home in &self.homes {
            on_found(ScannedJava {
                path: home.clone(),
                major: JavaMajor::parse(21).expect("21"),
                version: "21.0.5".to_owned(),
                vendor: JavaVendor::Temurin,
            });
        }
        Ok(())
    }
}

struct NoopJavaRegistry;

#[async_trait::async_trait]
impl JavaSourceRegistryPort for NoopJavaRegistry {
    fn source_names(&self) -> Vec<&'static str> {
        vec!["noop"]
    }
    async fn ranked(&self) -> Arc<dyn JavaSourceRegistryPort> {
        Arc::new(NoopJavaRegistry)
    }
    async fn list_packages(
        &self,
        _request: &JavaPackageRequest,
    ) -> Result<Vec<port::JavaPackage>, SourceError> {
        Ok(Vec::new())
    }
    async fn resolve_binary(
        &self,
        _package: &port::JavaPackage,
    ) -> Result<port::ResolvedJava, SourceError> {
        Err(SourceError::NotFound(VersionId::new("none")))
    }
}

struct NoopDownloader;

#[async_trait::async_trait]
impl ArtifactDownloader for NoopDownloader {
    async fn download(
        &self,
        _request: &DownloadRequest,
        _progress: Option<&dyn ProgressSink>,
        _cancel: &CancellationToken,
    ) -> Result<DownloadedArtifact, DownloadError> {
        Err(DownloadError::Cancelled)
    }
}

struct NoopExtractor;

impl ArchiveExtractor for NoopExtractor {
    fn is_extractable(&self, _archive: &Path) -> bool {
        false
    }
    fn extract(&self, _archive: &Path, _dest: &Path) -> Result<(), ExtractError> {
        Ok(())
    }
}

/// 服务端来源替身：拒之门外即可（Busy 测试不会走到它）
struct NoopJarSource;

#[async_trait::async_trait]
impl ServerJarSource for NoopJarSource {
    fn variant(&self) -> ServerVariant {
        ServerVariant::Vanilla
    }
    fn name(&self) -> &'static str {
        "noop"
    }
    async fn versions(&self) -> Result<Vec<domain::server::version::ServerVersion>, SourceError> {
        Ok(Vec::new())
    }
    async fn artifact(&self, _version: &VersionId) -> Result<port::ResolvedArtifact, SourceError> {
        Err(SourceError::NotFound(VersionId::new("none")))
    }
}

/// 核心检查替身：无世界、无数据版本
struct NoopInspector;

impl CoreInspector for NoopInspector {
    fn world_info(&self, _server_dir: &Path) -> Result<Option<WorldInfo>, InspectError> {
        Ok(None)
    }
    fn jar_data_version(&self, _jar: &Path) -> Result<Option<DataVersion>, InspectError> {
        Ok(None)
    }
}

/// 只承载 supervisor 的测试 `Backend`；installer 在生命周期用例中不用
struct SupBackend {
    supervisor: Arc<dyn SupervisorPort>,
}

struct NoopInstaller;

impl InstallerRunner for NoopInstaller {
    fn run_installer(
        &self,
        _invocation: &port::InstallerInvocation<'_>,
        _output: Option<&dyn port::InstallerOutputSink>,
        _cancel: &CancellationToken,
    ) -> Result<(), port::InstallerError> {
        unreachable!("not used")
    }
}

impl Backend for SupBackend {
    fn supervisor(&self) -> &dyn SupervisorPort {
        self.supervisor.as_ref()
    }

    fn installer(&self) -> &dyn InstallerRunner {
        &NoopInstaller
    }
}

/// 可编排的 supervisor 替身
#[derive(Default)]
struct FakeSupervisor {
    status: Mutex<Option<SupervisorStatus>>,
    start_result: Mutex<Option<SupervisorError>>,
    stop_result: Mutex<Option<StopOutcome>>,
    send_error: Mutex<Option<SupervisorError>>,
    start_calls: Mutex<Vec<SupervisorStartRequest>>,
    stop_calls: Mutex<usize>,
    stop_specs: Mutex<Vec<StopSpec>>,
    send_calls: Mutex<Vec<String>>,
}

impl FakeSupervisor {
    fn with_status(status: Option<SupervisorStatus>) -> Self {
        Self {
            status: Mutex::new(status),
            ..Self::default()
        }
    }

    fn running() -> Self {
        Self::with_status(Some(SupervisorStatus {
            status: domain::server::status::ServerStatus::Running,
            pid: Some(4242),
            uptime: Some(Duration::from_secs(10)),
            exit_code: None,
        }))
    }

    fn starts(&self) -> Vec<SupervisorStartRequest> {
        self.start_calls.lock().expect("lock").clone()
    }
}

impl SupervisorPort for FakeSupervisor {
    fn start(
        &self,
        request: SupervisorStartRequest,
    ) -> Result<SupervisorStartOutcome, SupervisorError> {
        if let Some(err) = self.start_result.lock().expect("lock").take() {
            return Err(err);
        }
        self.start_calls.lock().expect("lock").push(request);
        *self.status.lock().expect("lock") = Some(SupervisorStatus {
            status: domain::server::status::ServerStatus::Starting,
            pid: Some(1000),
            uptime: None,
            exit_code: None,
        });
        Ok(SupervisorStartOutcome {
            supervisor_pid: 777,
        })
    }

    fn stop(
        &self,
        _server: &port::ServerRef<'_>,
        spec: StopSpec,
    ) -> Result<StopOutcome, SupervisorError> {
        *self.stop_calls.lock().expect("lock") += 1;
        self.stop_specs.lock().expect("lock").push(spec);
        let outcome =
            self.stop_result
                .lock()
                .expect("lock")
                .clone()
                .unwrap_or(StopOutcome::Stopped {
                    elapsed: Duration::from_secs(1),
                });
        if matches!(outcome, StopOutcome::Stopped { .. }) {
            *self.status.lock().expect("lock") = None;
        }
        Ok(outcome)
    }

    fn status(
        &self,
        _server: &port::ServerRef<'_>,
    ) -> Result<Option<SupervisorStatus>, SupervisorError> {
        Ok(self.status.lock().expect("lock").clone())
    }

    fn send_line(&self, _server: &port::ServerRef<'_>, line: &str) -> Result<(), SupervisorError> {
        if let Some(err) = self.send_error.lock().expect("lock").take() {
            return Err(err);
        }
        self.send_calls.lock().expect("lock").push(line.to_owned());
        Ok(())
    }

    fn shutdown(&self, _server: &port::ServerRef<'_>) -> Result<(), SupervisorError> {
        Ok(())
    }
}

// ---- 夹具 ----

fn ws_fixture(root: &Path) -> Workspace {
    let path = root.join("ws");
    crate::fs::create_dir_all(path.join("servers")).expect("mkdir");
    Workspace {
        id: WorkspaceId::parse("550e8400-e29b-41d4-a716-446655440001").expect("uuid"),
        name: WorkspaceName::parse("ws01").expect("valid"),
        path,
        is_current: true,
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    }
}

fn runtime(path: &Path, major: i64) -> JavaRuntime {
    JavaRuntime {
        id: format!("id-{major}"),
        path: path.to_path_buf(),
        major: JavaMajor::parse(major).expect("major"),
        version: format!("{major}.0.0"),
        vendor: JavaVendor::Temurin,
        managed: false,
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
    }
}

/// 建好 workspace + 服务器 + （可选）jar 与 eula 的完整夹具
struct Fixture {
    registry: Arc<ServerRegistry>,
    /// 与 `registry` 共用的条目仓库（测试里改来源标注用）
    servers: Arc<MemServers>,
    server_dir: PathBuf,
    name: String,
    id: domain::server::meta::ServerId,
    _tmp: tempfile::TempDir,
}

impl Fixture {
    fn new(with_jar: bool) -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = ws_fixture(tmp.path());
        let servers = Arc::new(MemServers::new());
        let registry = Arc::new(ServerRegistry::new(
            Arc::clone(&servers) as Arc<dyn ServerRepository>,
            Arc::new(MemWorkspaces::new(ws)),
        ));
        let server = registry
            .create(
                None,
                "alpha",
                &VersionId::new("1.21.1"),
                ServerVariant::Vanilla,
                None,
            )
            .expect("create");
        if with_jar {
            crate::fs::write(server.dir.join("server.jar"), b"jar").expect("jar");
            registry
                .update_core(
                    "alpha",
                    &VersionId::new("1.21.1"),
                    &InstalledJar {
                        path: PathBuf::from("server.jar"),
                        size: 3,
                        checksum: Checksum::new(ChecksumAlgorithm::Sha1, "0".repeat(40)),
                    },
                    None,
                    None,
                    1_700_000_000,
                )
                .expect("jar");
        }
        let name = server.meta.name.as_str().to_owned();
        let id = server.meta.id.clone();
        Self {
            registry,
            servers,
            server_dir: server.dir,
            name,
            id,
            _tmp: tmp,
        }
    }
}

fn java_service(repo: MemJavaRepo, homes: Vec<PathBuf>) -> JavaService {
    JavaService::new(
        Arc::new(repo),
        Arc::new(FixedScanner { homes }),
        Arc::new(NoopJavaRegistry),
        Arc::new(NoopDownloader),
        Arc::new(NoopExtractor),
        PathBuf::from("/nonexistent-java-root"),
    )
}

fn service(
    fixture: &Fixture,
    java: JavaService,
    supervisor: Arc<FakeSupervisor>,
) -> ServerLifecycleService {
    ServerLifecycleService::new(
        Arc::clone(&fixture.registry),
        java,
        Arc::new(SupBackend {
            supervisor: supervisor as Arc<dyn SupervisorPort>,
        }),
    )
}

fn opts(accept_eula: bool) -> StartOptions {
    StartOptions {
        accept_eula,
        ram_mib: None,
        supervisor_exe: PathBuf::from("/usr/local/bin/mcsup"),
        supervisor_args: vec![std::ffi::OsString::from("__supervise")],
    }
}

fn start(
    service: &ServerLifecycleService,
    name: &str,
    accept: bool,
) -> Result<SupervisorStartOutcome, LifecycleError> {
    service.start(name, &opts(accept), &mut || {}, &mut |_| {})
}

// ---- start ----

#[test]
fn start_rejects_running_before_anything_else() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::running());
    // 未扫描的 Java 仓库：若走到扫描说明提前拒绝失效
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(Vec::new(), false), vec![]),
        Arc::clone(&supervisor),
    );
    let err = start(&svc, &fixture.name, true).expect_err("must reject");
    assert!(matches!(err, LifecycleError::AlreadyRunning), "{err:?}");
    assert!(supervisor.starts().is_empty());
}

#[test]
fn start_requires_jar_before_eula() {
    let fixture = Fixture::new(false);
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::new(FakeSupervisor::default()),
    );
    let err = start(&svc, &fixture.name, true).expect_err("must fail");
    assert!(matches!(err, LifecycleError::NoJar { .. }), "{err:?}");
    assert!(!fixture.server_dir.join(EULA_FILE).exists());
}

#[test]
fn start_routes_remote_entry_without_touching_local_disk() {
    // 远端条目的工作区路径是对端的（如 /root/ws），本机不存在。修复前
    // `start` 用 `registry.get` 拼出这个假路径做「核心存在」检查，于是把
    // 好端端的远端服务器报成 `NoJar`。
    let fixture = Fixture::new(false); // 本地刻意没有 jar
    fixture.servers.set_remote_origin("box");
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], false), vec![]),
        Arc::clone(&supervisor),
    );
    start(&svc, &fixture.name, true).expect("remote start must not require a local jar");

    let calls = supervisor.starts();
    assert_eq!(calls.len(), 1, "远端条目应发一帧 Start");
    let request = &calls[0];
    assert!(
        request.server_dir.as_os_str().is_empty(),
        "本地拼不出的目录必须留空，由对端自行解析: {request:?}"
    );
    assert!(
        request.java_bin.as_os_str().is_empty(),
        "Java 运行时在对端，本端不该决定: {request:?}"
    );
    assert_eq!(request.server_id, fixture.id);
}

#[test]
fn start_still_rejects_local_entry_without_jar() {
    // 远端免检不能顺手放过本地条目：本地缺核心仍须报 NoJar。
    let fixture = Fixture::new(false);
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::clone(&supervisor),
    );
    let err = start(&svc, &fixture.name, true).expect_err("local must still fail");
    assert!(matches!(err, LifecycleError::NoJar { .. }), "{err:?}");
    assert!(supervisor.starts().is_empty());
}

#[test]
fn start_rejects_missing_jar_file() {
    let fixture = Fixture::new(true);
    crate::fs::remove_file(fixture.server_dir.join("server.jar")).expect("rm");
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::new(FakeSupervisor::default()),
    );
    let err = start(&svc, &fixture.name, true).expect_err("must fail");
    assert!(matches!(err, LifecycleError::NoJar { .. }), "{err:?}");
}

#[test]
fn start_without_accept_eula_fails_and_leaves_file_untouched() {
    let fixture = Fixture::new(true);
    crate::fs::write(fixture.server_dir.join(EULA_FILE), render_eula(false)).expect("eula");
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::clone(&supervisor),
    );
    let err = start(&svc, &fixture.name, false).expect_err("must fail");
    assert!(matches!(err, LifecycleError::EulaRequired), "{err:?}");
    let text = crate::fs::read_to_string(fixture.server_dir.join(EULA_FILE)).expect("read");
    assert_eq!(text, render_eula(false));
    assert!(supervisor.starts().is_empty());
}

#[test]
fn start_with_accept_eula_writes_true_and_picks_highest_java() {
    let fixture = Fixture::new(true);
    let runtimes = vec![
        runtime(Path::new("/opt/java17"), 17),
        runtime(Path::new("/opt/java21"), 21),
    ];
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(runtimes, true), vec![]),
        Arc::clone(&supervisor),
    );

    let outcome = start(&svc, &fixture.name, true).expect("start");
    assert_eq!(outcome.supervisor_pid, 777);

    let eula = crate::fs::read_to_string(fixture.server_dir.join(EULA_FILE)).expect("eula");
    assert_eq!(eula, render_eula(true));

    let requests = supervisor.starts();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.server_dir, fixture.server_dir);
    assert_eq!(
        request.launch,
        mcsup_launch_jar("server.jar"),
        "默认启动方式为相对服务器目录的 server.jar"
    );
    assert_eq!(request.java_bin, java_bin_under(Path::new("/opt/java21")));
    assert_eq!(request.ram_mib, Some(DEFAULT_RAM_MIB));
    assert_eq!(
        request.supervisor_exe,
        PathBuf::from("/usr/local/bin/mcsup")
    );
}

/// `server set java-version`：绑定大版本时只在该版本内选运行时
#[test]
fn start_honors_bound_java_major() {
    let fixture = Fixture::new(true);
    let runtimes = vec![
        runtime(Path::new("/opt/java17"), 17),
        runtime(Path::new("/opt/java21"), 21),
    ];
    fixture
        .registry
        .update_settings(
            &fixture.name,
            &domain::server::settings::ServerSettings {
                java_major: Some(JavaMajor::parse(17).expect("17")),
                ram_mib: None,
                autostart: false,
                backup_scope: domain::backup::BackupScope::all(),
            },
        )
        .expect("settings");
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(runtimes, true), vec![]),
        Arc::clone(&supervisor),
    );

    start(&svc, &fixture.name, true).expect("start");

    let requests = supervisor.starts();
    assert_eq!(
        requests[0].java_bin,
        java_bin_under(Path::new("/opt/java17"))
    );
}

/// 记录缺少 Java 要求时按 MC 版本补：`Fabric` / `Quilt` / `NeoForge` 的旧记录
/// 在 1.21.1 上必须推出 21，而不是落到本机最高 JDK
#[test]
fn meta_java_major_falls_back_to_mc_version() {
    assert_eq!(
        meta_java_major(None, "1.21.1"),
        Some(JavaMajor::parse(21).expect("21"))
    );
    // 已记录的要求优先，不被 MC 版本覆盖
    assert_eq!(
        meta_java_major(Some(JavaMajor::parse(17).expect("17")), "1.21.1"),
        Some(JavaMajor::parse(17).expect("17"))
    );
    // 快照版本无法推算时保持 None，由 `pick_runtime` 走原有兜底
    assert_eq!(meta_java_major(None, "24w14a"), None);
}

/// 版本自带 Java 要求（`required_java_major`）：未绑定时按版本要求选运行时，
/// 而不是盲目取最高大版本——class file 69.0 需要新版 JDK，选低了会
/// `UnsupportedClassVersionError`
#[test]
fn start_honors_version_required_java_major() {
    let fixture = Fixture::new(true);
    let runtimes = vec![
        runtime(Path::new("/opt/java17"), 17),
        runtime(Path::new("/opt/java21"), 21),
    ];
    fixture
        .registry
        .update_core(
            &fixture.name,
            &VersionId::new("1.21.1"),
            &InstalledJar {
                path: PathBuf::from("server.jar"),
                size: 3,
                checksum: Checksum::new(ChecksumAlgorithm::Sha1, "0".repeat(40)),
            },
            None,
            Some(JavaMajor::parse(21).expect("21")),
            1_700_000_000,
        )
        .expect("core");
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(runtimes, true), vec![]),
        Arc::clone(&supervisor),
    );

    start(&svc, &fixture.name, true).expect("start");

    let requests = supervisor.starts();
    assert_eq!(
        requests[0].java_bin,
        java_bin_under(Path::new("/opt/java21")),
        "版本要求的 Java 大版本必须被采纳"
    );
}

/// 版本要求的 Java 大版本缺失运行时：报 UnknownJavaMajor，交互入口可据此
/// 提示安装（class file 69.0 即 Java 25 编译产物，JDK 17 跑不起来）
#[test]
fn start_reports_unknown_when_version_required_major_missing() {
    let fixture = Fixture::new(true);
    fixture
        .registry
        .update_core(
            &fixture.name,
            &VersionId::new("26.1"),
            &InstalledJar {
                path: PathBuf::from("server.jar"),
                size: 3,
                checksum: Checksum::new(ChecksumAlgorithm::Sha1, "0".repeat(40)),
            },
            None,
            Some(JavaMajor::parse(25).expect("25")),
            1_700_000_000,
        )
        .expect("core");
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(
            MemJavaRepo::new(vec![runtime(Path::new("/opt/java17"), 17)], true),
            vec![],
        ),
        Arc::clone(&supervisor),
    );

    let err = start(&svc, &fixture.name, true).expect_err("must fail");
    assert!(
        matches!(err, LifecycleError::UnknownJavaMajor { major, .. } if major.as_i64() == 25),
        "{err:?}"
    );
    assert!(supervisor.starts().is_empty());
}

/// 绑定的大版本没有运行时：start 报 UnknownJavaMajor（而不是静默降级）
#[test]
fn start_reports_unknown_java_major_when_bound_runtime_missing() {
    let fixture = Fixture::new(true);
    fixture
        .registry
        .update_settings(
            &fixture.name,
            &domain::server::settings::ServerSettings {
                java_major: Some(JavaMajor::parse(17).expect("17")),
                ram_mib: None,
                autostart: false,
                backup_scope: domain::backup::BackupScope::all(),
            },
        )
        .expect("settings");
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(
            MemJavaRepo::new(vec![runtime(Path::new("/opt/java21"), 21)], true),
            vec![],
        ),
        Arc::clone(&supervisor),
    );

    let err = start(&svc, &fixture.name, true).expect_err("must fail");
    assert!(
        matches!(err, LifecycleError::UnknownJavaMajor { .. }),
        "{err:?}"
    );
    assert!(supervisor.starts().is_empty());
}

/// `server set ram`：服务属性定堆上限；显式选项优先于属性
#[test]
fn start_honors_ram_setting_and_opts_override_it() {
    let fixture = Fixture::new(true);
    fixture
        .registry
        .update_settings(
            &fixture.name,
            &domain::server::settings::ServerSettings {
                java_major: None,
                ram_mib: Some(4096),
                autostart: false,
                backup_scope: domain::backup::BackupScope::all(),
            },
        )
        .expect("settings");
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(
            MemJavaRepo::new(vec![runtime(Path::new("/opt/java21"), 21)], true),
            vec![],
        ),
        Arc::clone(&supervisor),
    );

    start(&svc, &fixture.name, true).expect("start");
    assert_eq!(supervisor.starts()[0].ram_mib, Some(4096));

    // 显式 StartOptions 覆盖属性（首个 start 已把假监督器置为运行中，
    // 换一套监督器验证覆盖关系）
    let explicit = StartOptions {
        accept_eula: true,
        ram_mib: Some(512),
        supervisor_exe: PathBuf::from("/usr/local/bin/mcsup"),
        supervisor_args: vec![std::ffi::OsString::from("__supervise")],
    };
    let explicit_supervisor = Arc::new(FakeSupervisor::default());
    let explicit_svc = service(
        &fixture,
        java_service(
            MemJavaRepo::new(vec![runtime(Path::new("/opt/java21"), 21)], true),
            vec![],
        ),
        Arc::clone(&explicit_supervisor),
    );
    explicit_svc
        .start(&fixture.name, &explicit, &mut || {}, &mut |_| {})
        .expect("start");
    assert_eq!(explicit_supervisor.starts()[0].ram_mib, Some(512));
}

#[test]
fn start_without_java_reports_no_java() {
    let fixture = Fixture::new(true);
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::new(FakeSupervisor::default()),
    );
    let err = start(&svc, &fixture.name, true).expect_err("must fail");
    assert!(matches!(err, LifecycleError::NoJava), "{err:?}");
}

#[test]
fn start_scans_when_never_scanned_and_uses_found_runtime() {
    let fixture = Fixture::new(true);
    let home = fixture.server_dir.join("fake-jvm");
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], false), vec![home.clone()]),
        Arc::new(FakeSupervisor::default()),
    );
    let mut first_scan = false;
    let outcome = svc
        .start(
            &fixture.name,
            &opts(true),
            &mut || first_scan = true,
            &mut |_| {},
        )
        .expect("start");
    assert_eq!(outcome.supervisor_pid, 777);
    assert!(first_scan, "first scan must be announced");
}

#[test]
fn start_maps_port_already_running() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::default());
    *supervisor.start_result.lock().expect("lock") = Some(SupervisorError::AlreadyRunning);
    let svc = service(
        &fixture,
        java_service(
            MemJavaRepo::new(vec![runtime(Path::new("/opt/java21"), 21)], true),
            vec![],
        ),
        Arc::clone(&supervisor),
    );
    let err = start(&svc, &fixture.name, true).expect_err("must fail");
    assert!(matches!(err, LifecycleError::AlreadyRunning), "{err:?}");
}

// ---- stop / restart ----

#[test]
fn stop_without_supervisor_is_idempotent() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::clone(&supervisor),
    );
    let outcome = svc
        .stop(&fixture.name, StopSpec::default_grace())
        .expect("stop");
    assert_eq!(outcome, StopOutcome::AlreadyStopped);
    assert_eq!(*supervisor.stop_calls.lock().expect("lock"), 0);
}

#[test]
fn stop_success_passes_outcome_through() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::with_status(Some(SupervisorStatus {
        status: domain::server::status::ServerStatus::Running,
        pid: Some(4242),
        uptime: Some(Duration::from_secs(10)),
        exit_code: None,
    })));
    *supervisor.stop_result.lock().expect("lock") = Some(StopOutcome::Stopped {
        elapsed: Duration::from_secs(3),
    });
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::clone(&supervisor),
    );
    let outcome = svc
        .stop(&fixture.name, StopSpec::default_grace())
        .expect("stop");
    assert_eq!(
        outcome,
        StopOutcome::Stopped {
            elapsed: Duration::from_secs(3)
        }
    );
    assert_eq!(*supervisor.stop_calls.lock().expect("lock"), 1);
}

/// 远端帧可以传 `u64::MAX` 秒：service 必须夹到领域上限再交给 supervisor，
/// 否则 supervisor 的等待循环永远不会结束
#[test]
fn stop_grace_is_clamped_before_reaching_supervisor() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::running());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::clone(&supervisor),
    );
    svc.stop(
        &fixture.name,
        StopSpec {
            grace: Duration::from_secs(u64::MAX),
            force: true,
        },
    )
    .expect("stop");
    let specs = supervisor.stop_specs.lock().expect("lock").clone();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].grace, domain::limits::MAX_STOP_GRACE);
    assert!(specs[0].force, "force 标志不能被判定顺手改掉");
}

#[test]
fn stop_timeout_maps_to_error() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::with_status(Some(SupervisorStatus {
        status: domain::server::status::ServerStatus::Running,
        pid: Some(4242),
        uptime: Some(Duration::from_secs(10)),
        exit_code: None,
    })));
    *supervisor.stop_result.lock().expect("lock") = Some(StopOutcome::TimedOut {
        waited: Duration::from_secs(30),
    });
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::clone(&supervisor),
    );
    let err = svc
        .stop(&fixture.name, StopSpec::default_grace())
        .expect_err("must fail");
    assert!(
        matches!(err, LifecycleError::StopTimedOut { waited } if waited == Duration::from_secs(30)),
        "{err:?}"
    );
}

#[test]
fn restart_stops_then_starts() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::running());
    let svc = service(
        &fixture,
        java_service(
            MemJavaRepo::new(vec![runtime(Path::new("/opt/java21"), 21)], true),
            vec![],
        ),
        Arc::clone(&supervisor),
    );
    let outcome = svc
        .restart(
            &fixture.name,
            &opts(true),
            StopSpec::default_grace(),
            &mut || {},
            &mut |_| {},
        )
        .expect("restart");
    assert_eq!(outcome.supervisor_pid, 777);
    assert_eq!(*supervisor.stop_calls.lock().expect("lock"), 1);
    assert_eq!(supervisor.starts().len(), 1);
}

#[test]
fn restart_aborts_on_stop_timeout_without_start() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::running());
    *supervisor.stop_result.lock().expect("lock") = Some(StopOutcome::TimedOut {
        waited: Duration::from_secs(30),
    });
    let svc = service(
        &fixture,
        java_service(
            MemJavaRepo::new(vec![runtime(Path::new("/opt/java21"), 21)], true),
            vec![],
        ),
        Arc::clone(&supervisor),
    );
    let err = svc
        .restart(
            &fixture.name,
            &opts(true),
            StopSpec::default_grace(),
            &mut || {},
            &mut |_| {},
        )
        .expect_err("must fail");
    assert!(
        matches!(err, LifecycleError::StopTimedOut { .. }),
        "{err:?}"
    );
    assert!(supervisor.starts().is_empty());
}

// ---- 生命周期敏感编排：remove / install ----

/// 运行中删除必须被拒（Linux 上会蒸发存档），判定收敛在 service
#[test]
fn remove_server_rejects_running() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::running());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(Vec::new(), false), vec![]),
        Arc::clone(&supervisor),
    );
    let err = svc
        .remove_server("alpha", false, &fixture.server_dir.join(".."))
        .expect_err("must fail");
    assert!(
        matches!(
            err,
            LifecycleError::Busy {
                op: BusyOp::Remove,
                ..
            }
        ),
        "{err:?}"
    );
    // 目录与登记都保留
    assert!(fixture.server_dir.exists());
}

/// 未运行时删除放行：注销登记（默认不删盘）
#[test]
fn remove_server_allows_when_stopped() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(Vec::new(), false), vec![]),
        Arc::clone(&supervisor),
    );
    let removed = svc
        .remove_server("alpha", false, &fixture.server_dir.join(".."))
        .expect("remove");
    assert_eq!(removed.server.meta.name.as_str(), "alpha");
    assert!(fixture.registry.get("alpha").is_err());
}

/// 运行中换核心必须被拒（Linux 上 rename 成功但 JVM 用旧 inode）
#[tokio::test]
async fn install_jar_checked_rejects_running() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::running());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(Vec::new(), false), vec![]),
        Arc::clone(&supervisor),
    );
    let installer = crate::ServerInstaller::new(Arc::new(NoopJarSource), Arc::new(NoopDownloader));
    let err = svc
        .install_jar_checked(
            "alpha",
            &VersionId::new("1.21.2"),
            &installer,
            &NoopInspector,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect_err("must fail");
    assert!(
        matches!(
            err,
            LifecycleError::Busy {
                op: BusyOp::InstallJar,
                ..
            }
        ),
        "{err:?}"
    );
    // 旧核心原样保留
    assert!(fixture.server_dir.join("server.jar").is_file());
}

// ---- status / send / logs ----

#[test]
fn status_and_send_map_not_running() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::clone(&supervisor),
    );
    assert!(svc.status(&fixture.name).expect("status").is_none());
    let err = svc.send_line(&fixture.name, "list").expect_err("must fail");
    assert!(matches!(err, LifecycleError::NotRunning), "{err:?}");
}

#[test]
fn send_maps_remote_not_running_error() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::running());
    *supervisor.send_error.lock().expect("lock") = Some(SupervisorError::Remote {
        kind: port::RemoteFailureKind::NotRunning,
        message: "not running".to_owned(),
    });
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::clone(&supervisor),
    );
    let err = svc.send_line(&fixture.name, "list").expect_err("must fail");
    assert!(matches!(err, LifecycleError::NotRunning), "{err:?}");
}

#[test]
fn send_reaches_supervisor() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::running());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::clone(&supervisor),
    );
    svc.send_line(&fixture.name, "list").expect("send");
    assert_eq!(
        supervisor.send_calls.lock().expect("lock").clone(),
        vec!["list".to_owned()]
    );
}

#[test]
fn describe_merges_meta_and_supervisor_view() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::running());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::clone(&supervisor),
    );
    let report = svc.describe(&fixture.name).expect("describe");
    assert_eq!(report.name, "alpha");
    assert_eq!(report.version, "1.21.1");
    assert_eq!(report.variant, "vanilla");
    let status = report.status.expect("running");
    assert_eq!(status.pid, Some(4242));

    // 无 supervisor：视图为 None，注册信息仍在
    let idle = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::new(FakeSupervisor::default()),
    );
    let report = idle.describe(&fixture.name).expect("describe");
    assert!(report.status.is_none());
    assert_eq!(report.version, "1.21.1");
}

#[test]
fn statuses_probes_all_registered_servers() {
    let fixture = Fixture::new(true);
    let running = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::new(FakeSupervisor::running()),
    );
    let map = running.statuses().expect("statuses");
    assert_eq!(map.len(), 1);
    let probe = map.get("alpha").expect("alpha");
    let view = probe.as_ref().expect("probe ok").as_ref().expect("view");
    assert!(view.status.is_alive());

    let idle = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::new(FakeSupervisor::default()),
    );
    let map = idle.statuses().expect("statuses");
    assert!(
        map.get("alpha")
            .expect("alpha")
            .as_ref()
            .expect("probe ok")
            .is_none()
    );
}

/// 单台探测失败记 Err，不拖死整个列表
#[test]
fn statuses_degrade_single_probe_failure() {
    let fixture = Fixture::new(true);
    let svc = ServerLifecycleService::new(
        Arc::clone(&fixture.registry),
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::new(SupBackend {
            supervisor: Arc::new(ProbeFailSupervisor),
        }),
    );
    let map = svc.statuses().expect("statuses");
    assert_eq!(map.len(), 1);
    assert!(map.get("alpha").expect("alpha").is_err());
}

/// 状态探测恒失败的替身
struct ProbeFailSupervisor;

impl SupervisorPort for ProbeFailSupervisor {
    fn start(
        &self,
        _request: SupervisorStartRequest,
    ) -> Result<SupervisorStartOutcome, SupervisorError> {
        Err(SupervisorError::Unreachable("probe fail".to_owned()))
    }
    fn stop(
        &self,
        _server: &port::ServerRef<'_>,
        _spec: StopSpec,
    ) -> Result<StopOutcome, SupervisorError> {
        Err(SupervisorError::Unreachable("probe fail".to_owned()))
    }
    fn status(
        &self,
        _server: &port::ServerRef<'_>,
    ) -> Result<Option<SupervisorStatus>, SupervisorError> {
        Err(SupervisorError::Protocol("version mismatch".to_owned()))
    }
    fn send_line(&self, _server: &port::ServerRef<'_>, _line: &str) -> Result<(), SupervisorError> {
        Err(SupervisorError::Unreachable("probe fail".to_owned()))
    }
    fn shutdown(&self, _server: &port::ServerRef<'_>) -> Result<(), SupervisorError> {
        Ok(())
    }
}

#[test]
fn log_watch_opens_at_tail_of_existing_file() {
    let fixture = Fixture::new(true);
    let log = console_log_path(&fixture.server_dir);
    crate::fs::write(&log, "old\n").expect("write");
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::clone(&supervisor),
    );
    let mut watch = svc.log_watch(&fixture.name).expect("watch");
    assert!(watch.read_new().expect("at tail").is_empty());
    let mut file = crate::fs::open_options()
        .append(true)
        .open(&log)
        .expect("open");
    writeln!(file, "new").expect("append");
    assert_eq!(watch.read_new().expect("new"), vec!["new".to_owned()]);
}

#[test]
fn log_tail_returns_last_lines_and_empty_when_missing() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::clone(&supervisor),
    );
    assert!(svc.log_tail(&fixture.name, 10).expect("missing").is_empty());

    let log = console_log_path(&fixture.server_dir);
    crate::fs::write(&log, "l1\nl2\r\nl3\n").expect("write");
    assert_eq!(
        svc.log_tail(&fixture.name, 2).expect("tail"),
        vec!["l2".to_owned(), "l3".to_owned()]
    );
    assert_eq!(
        svc.log_tail(&fixture.name, 99).expect("tail"),
        vec!["l1".to_owned(), "l2".to_owned(), "l3".to_owned()]
    );
}

#[test]
fn log_watch_reads_new_lines_and_holds_partial() {
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("console.log");
    crate::fs::write(&path, "before\n").expect("write");
    let mut watch = LogWatch::open(path.clone());
    assert!(watch.read_new().expect("empty").is_empty());

    let mut file = crate::fs::open_options()
        .append(true)
        .open(&path)
        .expect("open");
    writeln!(file, "after").expect("append");
    assert_eq!(watch.read_new().expect("new"), vec!["after".to_owned()]);

    write!(file, "half").expect("append");
    assert!(watch.read_new().expect("partial").is_empty());
    writeln!(file, "-line").expect("append");
    assert_eq!(
        watch.read_new().expect("completed"),
        vec!["half-line".to_owned()]
    );
}

#[test]
fn log_watch_tail_replays_history_then_only_new_lines() {
    let fixture = Fixture::new(true);
    let log = console_log_path(&fixture.server_dir);
    crate::fs::write(&log, "l1\nl2\nl3\nl4\n").expect("write");
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::clone(&supervisor),
    );
    let (history, mut watch) = svc.log_watch_tail(&fixture.name, 2).expect("watch tail");
    assert_eq!(history, vec!["l3".to_owned(), "l4".to_owned()]);
    // 历史与增量不重：快照之后才追加
    assert!(watch.read_new().expect("no dup").is_empty());
    let mut file = crate::fs::open_options()
        .append(true)
        .open(&log)
        .expect("open");
    writeln!(file, "l5").expect("append");
    assert_eq!(watch.read_new().expect("new"), vec!["l5".to_owned()]);
}

#[test]
fn log_watch_tail_missing_file_starts_empty_then_reads_from_start() {
    let fixture = Fixture::new(true);
    let supervisor = Arc::new(FakeSupervisor::default());
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::clone(&supervisor),
    );
    let (history, mut watch) = svc.log_watch_tail(&fixture.name, 30).expect("watch tail");
    assert!(history.is_empty());
    // 文件此时尚未创建：快照长度为 0，创建后从头跟踪
    let log = console_log_path(&fixture.server_dir);
    crate::fs::write(&log, "boot\nready\n").expect("write");
    assert_eq!(
        watch.read_new().expect("from start"),
        vec!["boot".to_owned(), "ready".to_owned()]
    );
}

#[test]
fn log_watch_open_tail_caps_history_at_max_lines() {
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("console.log");
    let content = (1..=100)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    crate::fs::write(&path, format!("{content}\n")).expect("write");
    let (mut watch, history) = LogWatch::open_tail(path.clone(), 5).expect("open tail");
    assert_eq!(
        history,
        vec!["line96", "line97", "line98", "line99", "line100"]
    );
    assert!(watch.read_new().expect("no dup").is_empty());
    let mut file = crate::fs::open_options()
        .append(true)
        .open(&path)
        .expect("open");
    writeln!(file, "line101").expect("append");
    assert_eq!(watch.read_new().expect("new"), vec!["line101".to_owned()]);
}

#[test]
fn log_watch_rereads_after_recreate() {
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("console.log");
    crate::fs::write(&path, "x".repeat(100)).expect("write");
    let mut watch = LogWatch::open(path.clone());
    assert!(watch.read_new().expect("empty").is_empty());

    // 重建更短的文件：偏移失效，回到文件头重读
    crate::fs::write(&path, "fresh\n").expect("rewrite");
    assert_eq!(watch.read_new().expect("reread"), vec!["fresh".to_owned()]);
}

#[test]
fn eula_error_surfaces_as_io_not_eula_required() {
    let fixture = Fixture::new(true);
    // eula.txt 是目录：读失败（非 NotFound）
    crate::fs::create_dir(fixture.server_dir.join(EULA_FILE)).expect("dir");
    let svc = service(
        &fixture,
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::new(FakeSupervisor::default()),
    );
    let err = start(&svc, &fixture.name, true).expect_err("must fail");
    assert!(matches!(err, LifecycleError::Io(_)), "{err:?}");
}

/// 捕获型事件出口：验证 service 在成功路径上确实发布了事实
#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<domain::server::event::DomainEvent>>,
}

impl EventSink for RecordingSink {
    fn publish(&self, event: domain::server::event::DomainEvent) {
        self.events.lock().expect("lock").push(event);
    }
}

#[test]
fn stop_publishes_server_stopped_event() {
    let fixture = Fixture::new(true);
    let sink = Arc::new(RecordingSink::default());
    let svc = ServerLifecycleService::with_events(
        Arc::clone(&fixture.registry),
        java_service(MemJavaRepo::new(vec![], true), vec![]),
        Arc::new(SupBackend {
            supervisor: Arc::new(FakeSupervisor::running()),
        }),
        sink.clone(),
    );
    svc.stop(&fixture.name, StopSpec::default_grace())
        .expect("stop");
    let events = sink.events.lock().expect("lock").clone();
    assert!(
        events.iter().any(|event| matches!(
            event,
            domain::server::event::DomainEvent::ServerStopped { name } if name == &fixture.name
        )),
        "{events:?}"
    );
}
