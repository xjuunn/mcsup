//! 内容安装编排：选版、落点、幂等、故障收场
//!
//! 这一层测的是 Service 的编排，不是某个实现：用假来源（可控候选 / 解析结果）
//! 与假下载器（记录请求、可注入失败），断言
//!
//! - 版本挑选遵循领域排序（加载器 → 游戏版本 → 正式版）
//! - 显式版本绕过挑选
//! - 文件落到正确的服务端子目录，文件名去掉路径段
//! - 已存在的同大小文件不重复下载
//! - 客户端资源、缺失候选、未知来源、下载失败都以对应错误收场
//! - 整合包下载到 `modpack/` 而不是 `mods/`

#![allow(clippy::expect_used)]

use async_trait::async_trait;
use domain::server::content::{ContentCandidate, ContentKind, ContentRequest};
use port::content::{
    ContentFile, ContentProject, ContentQuery, ContentSearch, ContentSource, ContentSourceInfo,
    ContentSourceRegistry, ResolvedContent,
};
use port::{
    ArtifactDownloader, CancellationToken, DownloadError, DownloadRequest, DownloadedArtifact,
    ProgressSink, SourceError,
};
use service::{ContentError, ContentService, InstallOptions, request_for};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// 假来源：候选与解析结果都由用例给定
struct FakeSource {
    name: &'static str,
    candidates: Vec<ContentCandidate>,
    resolved: Option<ResolvedContent>,
    /// 记录收到的候选请求，供断言过滤条件是否下发
    seen_requests: Arc<Mutex<Vec<ContentRequest>>>,
    /// 搜索被调用次数（断言不需要搜索时不搜）
    searches: Arc<AtomicUsize>,
}

impl FakeSource {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            candidates: Vec::new(),
            resolved: None,
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            searches: Arc::new(AtomicUsize::new(0)),
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
        self.name
    }
}

#[async_trait]
impl ContentSource for FakeSource {
    fn supported_kinds(&self) -> Vec<ContentKind> {
        ContentKind::ALL.to_vec()
    }

    async fn search(&self, _query: &ContentQuery) -> Result<ContentSearch, SourceError> {
        self.searches.fetch_add(1, Ordering::SeqCst);
        Ok(ContentSearch {
            hits: Vec::new(),
            offset: 0,
            total: 0,
        })
    }

    async fn project(&self, id: &str) -> Result<ContentProject, SourceError> {
        Ok(ContentProject {
            id: id.to_owned(),
            slug: None,
            title: id.to_owned(),
            description: String::new(),
            kind: ContentKind::Mod,
            also_kinds: Vec::new(),
            loaders: Vec::new(),
            game_versions: Vec::new(),
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

/// 假下载器：记录请求，按预置字节写盘，可注入失败
struct FakeDownloader {
    requests: Arc<Mutex<Vec<DownloadRequest>>>,
    fail: bool,
    bytes: usize,
}

impl FakeDownloader {
    fn ok(bytes: usize) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            fail: false,
            bytes,
        }
    }

    fn failing() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            fail: true,
            bytes: 0,
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
        self.requests.lock().expect("seen").push(request.clone());
        if self.fail {
            return Err(DownloadError::Io(std::io::Error::other("injected")));
        }
        let parent = request.destination.parent().expect("parent");
        std::fs::create_dir_all(parent).expect("mkdir");
        let payload = vec![b'x'; self.bytes];
        std::fs::write(&request.destination, &payload).expect("write");
        Ok(DownloadedArtifact {
            path: request.destination.clone(),
            size: payload.len() as u64,
        })
    }
}

fn candidate(id: &str, channel: &str, loaders: &[&str], games: &[&str]) -> ContentCandidate {
    ContentCandidate {
        version_id: id.to_owned(),
        version_number: id.to_owned(),
        channel: channel.to_owned(),
        featured: false,
        loaders: loaders.iter().map(|s| (*s).to_owned()).collect(),
        game_versions: games.iter().map(|s| (*s).to_owned()).collect(),
        date_published: "2024-01-01T00:00:00Z".to_owned(),
        score: 0,
    }
}

fn resolved(version: &str, filename: &str, size: u64) -> ResolvedContent {
    ResolvedContent {
        project_id: "P".to_owned(),
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

fn service(source: FakeSource, downloader: FakeDownloader) -> ContentService {
    ContentService::new(
        Arc::new(ContentSourceRegistry::new(vec![
            Arc::new(source) as Arc<dyn ContentSource>
        ])),
        Arc::new(downloader),
    )
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mcsup-content-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("tempdir");
    dir
}

/// 挑版本：加载器与游戏版本都匹配的正式版胜出
#[tokio::test]
async fn picks_release_matching_loader_and_game() {
    let source = FakeSource::new("fake")
        .with_candidates(vec![
            candidate("forge", "release", &["forge"], &["1.21.1"]),
            candidate("fabric", "release", &["fabric"], &["1.21.1"]),
            candidate("fabric-beta", "beta", &["fabric"], &["1.21.1"]),
        ])
        .with_resolved(resolved("fabric", "mod.jar", 4));
    let downloader = FakeDownloader::ok(4);
    let service = service(source, downloader);
    let dir = temp_dir("pick");

    let install = service
        .install(
            "P",
            &ContentRequest::new(ContentKind::Mod)
                .loader("fabric")
                .game_version("1.21.1"),
            InstallOptions::new(&dir),
            &CancellationToken::new(),
        )
        .await
        .expect("install");

    assert_eq!(install.version_id, "fabric");
    assert!(install.path.starts_with(dir.join("mods")));
    std::fs::remove_dir_all(&dir).ok();
}

/// 显式版本绕过候选挑选，且不触发候选查询
#[tokio::test]
async fn explicit_version_skips_ranking() {
    let mut source = FakeSource::new("fake").with_resolved(resolved("V9", "a.jar", 4));
    source.candidates = vec![candidate("V1", "release", &["fabric"], &["1.21.1"])];
    let seen = source.seen_requests.clone();
    let downloader = FakeDownloader::ok(4);
    let service = service(source, downloader);
    let dir = temp_dir("explicit");

    let install = service
        .install(
            "P",
            &ContentRequest::new(ContentKind::Mod),
            InstallOptions::new(&dir).version("V9"),
            &CancellationToken::new(),
        )
        .await
        .expect("install");
    assert_eq!(install.version_id, "V9");
    assert!(seen.lock().expect("seen").is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

/// 下载请求携带上层给出的地址、落点与大小，且落点文件名去掉路径段
#[tokio::test]
async fn download_request_is_sanitized() {
    let source = FakeSource::new("fake").with_resolved(resolved("V", "a/b/mod.jar", 4));
    let downloader = FakeDownloader::ok(4);
    let seen = downloader.requests.clone();
    let service = service(source, downloader);
    let dir = temp_dir("sanitize");

    let install = service
        .install(
            "P",
            &ContentRequest::new(ContentKind::Plugin),
            InstallOptions::new(&dir).version("V"),
            &CancellationToken::new(),
        )
        .await
        .expect("install");
    assert_eq!(install.path, dir.join("plugins").join("mod.jar"));

    let requests = seen.lock().expect("seen").clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].destination, dir.join("plugins").join("mod.jar"));
    assert_eq!(requests[0].expected_size, Some(4));
    assert!(requests[0].expected.is_none());
    std::fs::remove_dir_all(&dir).ok();
}

/// 已存在同大小文件时跳过下载
#[tokio::test]
async fn reuses_existing_same_size_file() {
    let source = FakeSource::new("fake").with_resolved(resolved("V", "mod.jar", 4));
    let downloader = FakeDownloader::ok(4);
    let seen = downloader.requests.clone();
    let service = service(source, downloader);
    let dir = temp_dir("reuse");
    let mods = dir.join("mods");
    std::fs::create_dir_all(&mods).expect("mkdir");
    std::fs::write(mods.join("mod.jar"), b"xxxx").expect("seed");

    let install = service
        .install(
            "P",
            &ContentRequest::new(ContentKind::Mod),
            InstallOptions::new(&dir).version("V"),
            &CancellationToken::new(),
        )
        .await
        .expect("install");
    assert!(install.reused);
    assert!(seen.lock().expect("seen").is_empty(), "不应发起下载");
    std::fs::remove_dir_all(&dir).ok();
}

/// 大小不一致时覆盖下载（版本升级场景）
#[tokio::test]
async fn replaces_existing_when_size_differs() {
    let source = FakeSource::new("fake").with_resolved(resolved("V", "mod.jar", 6));
    let downloader = FakeDownloader::ok(6);
    let seen = downloader.requests.clone();
    let service = service(source, downloader);
    let dir = temp_dir("replace");
    let mods = dir.join("mods");
    std::fs::create_dir_all(&mods).expect("mkdir");
    std::fs::write(mods.join("mod.jar"), b"old").expect("seed");

    let install = service
        .install(
            "P",
            &ContentRequest::new(ContentKind::Mod),
            InstallOptions::new(&dir).version("V"),
            &CancellationToken::new(),
        )
        .await
        .expect("install");
    assert!(!install.reused);
    assert_eq!(seen.lock().expect("seen").len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

/// 光影是客户端资源：拒绝安装，且不查询来源
#[tokio::test]
async fn rejects_client_only_kind() {
    let source = FakeSource::new("fake");
    let seen = source.seen_requests.clone();
    let service = service(source, FakeDownloader::ok(1));
    let dir = temp_dir("shader");

    let err = service
        .install(
            "P",
            &ContentRequest::new(ContentKind::Shader),
            InstallOptions::new(&dir).version("V"),
            &CancellationToken::new(),
        )
        .await
        .expect_err("应拒绝");
    assert!(matches!(err, ContentError::ClientOnly { .. }), "{err:?}");
    assert!(seen.lock().expect("seen").is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

/// 没有候选版本时给出可诊断的错误（带上过滤条件）
#[tokio::test]
async fn no_candidate_reports_filters() {
    let source = FakeSource::new("fake");
    let service = service(source, FakeDownloader::ok(1));
    let dir = temp_dir("nocand");

    let err = service
        .install(
            "P",
            &ContentRequest::new(ContentKind::Mod)
                .loader("fabric")
                .game_version("1.21.1"),
            InstallOptions::new(&dir),
            &CancellationToken::new(),
        )
        .await
        .expect_err("应报无候选");
    match err {
        ContentError::NoCandidate {
            loader, game, kind, ..
        } => {
            assert_eq!(kind, ContentKind::Mod);
            assert_eq!(loader.as_deref(), Some("fabric"));
            assert_eq!(game.as_deref(), Some("1.21.1"));
        }
        other => panic!("期望 NoCandidate，得到 {other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// 未知来源与未配置来源
#[tokio::test]
async fn unknown_and_missing_source() {
    let service = service(FakeSource::new("fake"), FakeDownloader::ok(1));
    let err = service
        .search(Some("absent"), &ContentQuery::new())
        .await
        .expect_err("应报未知来源");
    assert!(matches!(err, ContentError::UnknownSource(_)), "{err:?}");

    let empty = ContentService::new(
        Arc::new(ContentSourceRegistry::new(Vec::new())),
        Arc::new(FakeDownloader::ok(1)),
    );
    let err = empty
        .search(None, &ContentQuery::new())
        .await
        .expect_err("应报无来源");
    assert!(matches!(err, ContentError::NoSource), "{err:?}");
}

/// 下载失败原样冒泡，且不留下半成品路径的假成功
#[tokio::test]
async fn download_failure_propagates() {
    let source = FakeSource::new("fake").with_resolved(resolved("V", "mod.jar", 4));
    let service = service(source, FakeDownloader::failing());
    let dir = temp_dir("fail");

    let err = service
        .install(
            "P",
            &ContentRequest::new(ContentKind::Mod),
            InstallOptions::new(&dir).version("V"),
            &CancellationToken::new(),
        )
        .await
        .expect_err("应报下载失败");
    assert!(matches!(err, ContentError::Download(_)), "{err:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// 解析失败（版本不属于该项目 / 不存在）冒泡为来源错误
#[tokio::test]
async fn resolve_failure_propagates() {
    let source = FakeSource::new("fake").with_resolved(resolved("V", "mod.jar", 4));
    let service = service(source, FakeDownloader::ok(4));
    let dir = temp_dir("resolve");

    let err = service
        .install(
            "P",
            &ContentRequest::new(ContentKind::Mod),
            InstallOptions::new(&dir).version("MISSING"),
            &CancellationToken::new(),
        )
        .await
        .expect_err("应报来源错误");
    assert!(matches!(err, ContentError::Source(_)), "{err:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// 整合包落到 `modpack/`，不进 `mods/`
#[tokio::test]
async fn modpack_lands_in_dedicated_dir() {
    let source = FakeSource::new("fake")
        .with_candidates(vec![candidate("V", "release", &[], &[])])
        .with_resolved(resolved("V", "pack.mrpack", 4));
    let service = service(source, FakeDownloader::ok(4));
    let dir = temp_dir("modpack");

    let install = service
        .install_modpack("P", InstallOptions::new(&dir), &CancellationToken::new())
        .await
        .expect("install");
    assert_eq!(
        install.archive.path,
        dir.join("modpack").join("pack.mrpack")
    );
    assert_eq!(install.archive.version_id, "V");
    std::fs::remove_dir_all(&dir).ok();
}

/// `request_for` 把服务端变体映射成加载器条件
#[test]
fn request_for_maps_variant_loader() {
    use domain::server::version::ServerVariant;
    let request = request_for(ContentKind::Mod, &ServerVariant::Fabric, "1.21.1");
    assert_eq!(request.loader.as_deref(), Some("fabric"));
    assert_eq!(request.game_version.as_deref(), Some("1.21.1"));
    assert!(request.release_only);

    // 原版没有加载器：不按加载器过滤
    let request = request_for(ContentKind::Mod, &ServerVariant::Vanilla, "1.21.1");
    assert!(request.loader.is_none());

    // 数据包不看加载器
    let request = request_for(ContentKind::DataPack, &ServerVariant::Fabric, "1.21.1");
    assert_eq!(request.loader_filter(), None);
}

/// 进度回调会被转发给下载器（用真实下载器路径无法断言，这里断言编排不吞掉它）
#[tokio::test]
async fn progress_sink_is_accepted() {
    struct Counting(AtomicUsize);
    impl ProgressSink for Counting {
        fn on_progress(&self, _transferred: u64, _total: Option<u64>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let source = FakeSource::new("fake").with_resolved(resolved("V", "mod.jar", 4));
    let progress = Counting(AtomicUsize::new(0));
    let service = service(source, FakeDownloader::ok(4));
    let dir = temp_dir("progress");
    let _ = service
        .install(
            "P",
            &ContentRequest::new(ContentKind::Mod),
            InstallOptions::new(&dir).version("V").progress(&progress),
            &CancellationToken::new(),
        )
        .await
        .expect("install");
    // 假下载器不回调进度，这里只验证参数形状可编译、编排不 panic
    std::fs::remove_dir_all(&dir).ok();
}
