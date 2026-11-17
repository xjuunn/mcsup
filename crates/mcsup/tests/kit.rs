//! 任务级接口的真实装配：真注册表、真安装编排、假来源与假下载器
//!
//! 这里不打桩 service：`Kit::create_server` 走的是与 CLI 同一条
//! 注册表 → 来源 → 下载 → 校验 → 登记核心链路，只把网络换成可控假件。
//! 因此「一行建服」是否真的落盘、是否只下载一次、失败前有没有副作用，
//! 都在这里被钉住。

// feature powerset 会单独开每个 feature：没开 `kit` 时这个测试文件整体不编译
#![cfg(feature = "kit")]
// 集成测试不在 `#[test]` 函数体内，clippy 的 `expect_used` 豁免只覆盖 cfg(test)
#![allow(clippy::expect_used)]

use async_trait::async_trait;
use mcsup::bootstrap::{Mcsup, McsupBuilder};
use mcsup::domain::server::artifact::{Checksum, JarArtifact};
use mcsup::domain::server::variant::ServerVariant;
use mcsup::domain::server::version::{ServerVersion, VersionId};
use mcsup::kit::{CreateServer, KitConfig, KitError, StartServer};
use mcsup::port::{
    ArtifactDownloader, ErrorCategory, InstallKind, ResolvedArtifact, ServerJarSource, SourceError,
};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use testkit::FailingDownloader;
use url::Url;

/// 一个永远返回同一份「服务端」的假来源
struct FixedSource {
    variant: ServerVariant,
    location: Url,
}

impl FixedSource {
    fn vanilla() -> Self {
        Self {
            variant: ServerVariant::Vanilla,
            location: Url::parse("http://127.0.0.1:1/server.jar").expect("url"),
        }
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
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        zip.start_file(
            "META-INF/MANIFEST.MF",
            zip::write::SimpleFileOptions::default(),
        )
        .expect("start entry");
        zip.write_all(b"Manifest-Version: 1.0\n").expect("write");
        zip.finish().expect("finish zip");
    }
    buffer.into_inner()
}

/// 装配一套「不联网」的完整设施
fn build(
    sources: Vec<Arc<dyn ServerJarSource>>,
) -> (tempfile::TempDir, Mcsup, Arc<FailingDownloader>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let downloader = Arc::new(FailingDownloader::new().with_payload(jar_bytes()));
    let as_port: Arc<dyn ArtifactDownloader> = downloader.clone();
    let mcsup = McsupBuilder::new()
        .jar_sources(sources)
        .downloader(as_port)
        .build(dir.path())
        .expect("bootstrap");
    (dir, mcsup, downloader)
}

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

/// 工作目录初始化可重复执行：第二次不换 id、不重建目录
#[test]
fn ensure_workspace_is_idempotent() {
    let (dir, mcsup, _downloader) = build(Vec::new());
    let workspace_dir = dir.path().join("workspace");
    let kit = mcsup.kit(KitConfig::default());
    let first = kit
        .ensure_workspace(&workspace_dir, Some("main"))
        .expect("init");
    let second = kit
        .ensure_workspace(&workspace_dir, Some("main"))
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
