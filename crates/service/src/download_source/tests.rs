//! 选源路由的行为契约
//!
//! 这里的测试全部用确定性的替身：探测延迟、失败状态码都由用例给定，
//! 不触网。真实延迟只影响「谁最快」，而「谁最快」是 `domain::mirror::select`
//! 的职责，已在领域层测过；这一层要保证的是**选择被记住、失败被作废**。

use super::*;
use domain::mirror::{Binding, Source, Upstream};
use port::{DownloadedArtifact, MemoryRouteStore, SourceError};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

/// 上游面（官方地址前缀）
fn upstreams() -> Vec<Upstream> {
    vec![
        Upstream::new("mojang-objects", "https://piston-data.mojang.com").expect("upstream"),
        Upstream::new("modrinth-cdn", "https://cdn.modrinth.com").expect("upstream"),
    ]
}

/// BMCLAPI 覆盖 Mojang 的对象存储，MCIM 覆盖 Modrinth 的 CDN
fn mirrors() -> Vec<Source> {
    let bmclapi = Source::new(SourceId::new("bmclapi").expect("id"), "BMCLAPI")
        .with(Binding::new("mojang-objects", "https://bmcl.example").expect("binding"));
    let mcim = Source::new(SourceId::new("mcim").expect("id"), "MCIM")
        .with(Binding::new("modrinth-cdn", "https://mcim.example").expect("binding"));
    vec![bmclapi, mcim]
}

fn catalog() -> Arc<Catalog> {
    Arc::new(Catalog::with_mirrors(upstreams(), mirrors()).expect("catalog"))
}

/// 服务端 jar 地址：官方前缀 + 路径，镜像保持同一路径
const SERVER_JAR: &str = "https://piston-data.mojang.com/v1/objects/abc/server.jar";

fn location() -> Url {
    Url::parse(SERVER_JAR).expect("url")
}

/// 探测延迟表：缺项的地址一律视为不可达
struct ScriptedProbe {
    probed: Mutex<Vec<String>>,
    latencies: Mutex<HashMap<String, u64>>,
    /// 探测成功过一次之后才不可达的地址：模拟「选源时通、下载时挂」
    dies_after: Mutex<HashSet<String>>,
    dead: Mutex<HashSet<String>>,
}

impl ScriptedProbe {
    fn new(latencies: &[(&str, u64)]) -> Arc<Self> {
        Arc::new(Self {
            probed: Mutex::new(Vec::new()),
            latencies: Mutex::new(
                latencies
                    .iter()
                    .map(|(location, millis)| ((*location).to_owned(), *millis))
                    .collect(),
            ),
            dies_after: Mutex::new(HashSet::new()),
            dead: Mutex::new(HashSet::new()),
        })
    }

    fn probed(&self) -> Vec<String> {
        self.probed.lock().expect("probe log").clone()
    }

    fn set(&self, location: &str, millis: u64) {
        self.latencies
            .lock()
            .expect("latencies")
            .insert(location.to_owned(), millis);
    }

    fn clear(&self) {
        self.latencies.lock().expect("latencies").clear();
    }

    /// 该地址在**下一次**探测时仍可达，此后不可达
    fn dies_after_next_probe(&self, location: &str) {
        self.dies_after
            .lock()
            .expect("dies_after")
            .insert(location.to_owned());
    }
}

#[async_trait::async_trait]
impl SourceProbe for ScriptedProbe {
    async fn probe(&self, location: &str) -> Result<Duration, SourceError> {
        self.probed
            .lock()
            .expect("probe log")
            .push(location.to_owned());
        if self.dead.lock().expect("dead").contains(location) {
            return Err(SourceError::Network("unreachable".into()));
        }
        let latencies = self.latencies.lock().expect("latencies");
        let latency = latencies
            .get(location)
            .map(|millis| Duration::from_millis(*millis))
            .ok_or_else(|| SourceError::Network("unreachable".into()))?;
        drop(latencies);
        // 本次算通，之后转为不可达：换源重试时才会选出另一个源
        if self.dies_after.lock().expect("dies_after").remove(location) {
            self.dead.lock().expect("dead").insert(location.to_owned());
        }
        Ok(latency)
    }
}

fn router(probe: Arc<ScriptedProbe>, store: Arc<dyn SourceRouteStore>) -> Arc<SourceRouter> {
    Arc::new(SourceRouter::new(catalog(), probe, store))
}

/// 最快的源胜出，且选择被落盘
#[tokio::test]
async fn fastest_source_wins_and_is_persisted() {
    let probe = ScriptedProbe::new(&[
        ("https://bmcl.example/v1/objects/abc/server.jar", 40),
        (SERVER_JAR, 900),
    ]);
    let store = Arc::new(MemoryRouteStore::default());
    let router = router(
        Arc::clone(&probe),
        Arc::clone(&store) as Arc<dyn SourceRouteStore>,
    );

    let route = router.route(&location()).await.expect("route");
    assert_eq!(route.source, SourceId::new("bmclapi").expect("id"));
    assert_eq!(route.label, "BMCLAPI");
    assert_eq!(route.upstream, "mojang-objects");
    assert!(route.decided, "首次选择应当标记为刚决定");
    assert_eq!(
        route.primary().expect("primary").location,
        "https://bmcl.example/v1/objects/abc/server.jar"
    );
    // 落盘：换成另一个路由器（模拟进程重启）也直接命中
    assert_eq!(
        store.route("mojang-objects").expect("read"),
        Some(SourceId::new("bmclapi").expect("id"))
    );
}

/// 第二次起不再探测：只探一次是「选一次、以后都用它」的核心
#[tokio::test]
async fn later_downloads_reuse_choice_without_probing_again() {
    let probe = ScriptedProbe::new(&[
        ("https://bmcl.example/v1/objects/abc/server.jar", 40),
        (SERVER_JAR, 900),
    ]);
    let store = Arc::new(MemoryRouteStore::default());
    let router = router(Arc::clone(&probe), store as Arc<dyn SourceRouteStore>);

    let first = router.route(&location()).await.expect("first");
    let probes_after_first = probe.probed().len();
    let second = router.route(&location()).await.expect("second");

    assert_eq!(second.source, first.source);
    assert!(!second.decided, "复用已记录的选择");
    assert_eq!(
        probe.probed().len(),
        probes_after_first,
        "复用时不得再探测任何地址"
    );
}

/// 失败作废后重新探测，并按新结果改写选择
#[tokio::test]
async fn failure_invalidates_choice_and_reprobes() {
    let probe = ScriptedProbe::new(&[
        ("https://bmcl.example/v1/objects/abc/server.jar", 40),
        (SERVER_JAR, 900),
    ]);
    let store = Arc::new(MemoryRouteStore::default());
    let router = router(
        Arc::clone(&probe),
        Arc::clone(&store) as Arc<dyn SourceRouteStore>,
    );

    let first = router.route(&location()).await.expect("first");
    assert_eq!(first.source.as_str(), "bmclapi");

    // 镜像挂了：官方变快，镜像不再可达
    probe.clear();
    probe.set(SERVER_JAR, 30);
    router.report_failure("mojang-objects", &first.source);
    assert_eq!(
        store.route("mojang-objects").expect("read"),
        None,
        "作废后不得留下旧选择"
    );

    let second = router.route(&location()).await.expect("second");
    assert_eq!(second.source.as_str(), "official");
    assert!(second.decided);
    assert_eq!(
        store.route("mojang-objects").expect("read"),
        Some(SourceId::new("official").expect("id")),
        "重新选择后要落盘新结果"
    );
}

/// 全部探测失败：本次退回官方，但不把「一次抖动」记成结论
#[tokio::test]
async fn all_probes_failing_falls_back_to_official_without_persisting() {
    let probe = ScriptedProbe::new(&[]);
    let store = Arc::new(MemoryRouteStore::default());
    let router = router(
        Arc::clone(&probe),
        Arc::clone(&store) as Arc<dyn SourceRouteStore>,
    );

    let route = router.route(&location()).await.expect("route");
    assert_eq!(route.source.as_str(), "official");
    assert_eq!(
        route.primary().expect("primary").location,
        SERVER_JAR,
        "退回官方即保持原地址"
    );
    assert_eq!(
        store.route("mojang-objects").expect("read"),
        None,
        "探测全失败不得落盘，否则以后再也不探镜像"
    );
}

/// 不在目录表里的主机保持原样
#[tokio::test]
async fn unlisted_host_has_no_route() {
    let probe = ScriptedProbe::new(&[]);
    let router = router(probe, Arc::new(MemoryRouteStore::default()));
    let other = Url::parse("https://api.papermc.io/v2/projects/paper").expect("url");
    assert!(router.route(&other).await.is_none());
}

/// 改写请求：选中源当主地址，其余候选进 mirrors 以承担回退与分载
#[tokio::test]
async fn routed_request_puts_chosen_first_and_keeps_others() {
    let probe = ScriptedProbe::new(&[
        ("https://bmcl.example/v1/objects/abc/server.jar", 40),
        (SERVER_JAR, 900),
    ]);
    let router = router(probe, Arc::new(MemoryRouteStore::default()));
    let route = router.route(&location()).await.expect("route");

    let request = DownloadRequest {
        location: location(),
        destination: PathBuf::from("server.jar"),
        expected: None,
        expected_size: None,
        mirrors: vec![Url::parse("https://extra.example/server.jar").expect("url")],
    };
    let prepared = routed(&request, &route);
    assert_eq!(
        prepared.location.as_str(),
        "https://bmcl.example/v1/objects/abc/server.jar"
    );
    let mirrors: Vec<&str> = prepared.mirrors.iter().map(Url::as_str).collect();
    assert_eq!(
        mirrors,
        vec![SERVER_JAR, "https://extra.example/server.jar"],
        "官方排在调用方给的额外镜像之前：官方是同一内容的权威入口"
    );
}

/// 记录下载请求的下载器；给定地址按给定状态码失败
struct ScriptedDownloader {
    seen: Mutex<Vec<String>>,
    failing: Mutex<Vec<(String, u16)>>,
}

impl ScriptedDownloader {
    fn failing(location: &str, status: u16) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            failing: Mutex::new(vec![(location.to_owned(), status)]),
        })
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("seen").clone()
    }
}

#[async_trait::async_trait]
impl ArtifactDownloader for ScriptedDownloader {
    async fn download(
        &self,
        request: &DownloadRequest,
        _progress: Option<&dyn ProgressSink>,
        _cancel: &CancellationToken,
    ) -> Result<DownloadedArtifact, DownloadError> {
        self.seen
            .lock()
            .expect("seen")
            .push(request.location.to_string());
        let failing = self.failing.lock().expect("failing");
        if let Some((_, status)) = failing
            .iter()
            .find(|(location, _)| location == request.location.as_str())
        {
            return Err(DownloadError::Source(SourceError::Status {
                status: *status,
            }));
        }
        Ok(DownloadedArtifact {
            path: request.destination.clone(),
            size: 1,
        })
    }
}

/// 主源失败会作废选择、换源再下一次（只一次）
#[tokio::test]
async fn downloader_retries_once_on_retryable_failure() {
    let probe = ScriptedProbe::new(&[
        ("https://bmcl.example/v1/objects/abc/server.jar", 40),
        (SERVER_JAR, 900),
    ]);
    // 镜像在选源时通、下载时挂：这样首轮才会选中镜像，失败后重探改选官方
    probe.dies_after_next_probe("https://bmcl.example/v1/objects/abc/server.jar");
    let store = Arc::new(MemoryRouteStore::default());
    let router = router(
        Arc::clone(&probe),
        Arc::clone(&store) as Arc<dyn SourceRouteStore>,
    );

    let inner = ScriptedDownloader::failing("https://bmcl.example/v1/objects/abc/server.jar", 503);
    let downloader =
        RoutingDownloader::new(Arc::clone(&inner) as Arc<dyn ArtifactDownloader>, router);

    let request = DownloadRequest {
        location: location(),
        destination: PathBuf::from("server.jar"),
        expected: None,
        expected_size: None,
        mirrors: Vec::new(),
    };
    let cancel = CancellationToken::new();
    let done = downloader
        .download(&request, None, &cancel)
        .await
        .expect("换源后应当成功");
    assert_eq!(done.size, 1);
    assert_eq!(
        inner.seen(),
        vec![
            "https://bmcl.example/v1/objects/abc/server.jar".to_owned(),
            SERVER_JAR.to_owned(),
        ],
        "先试镜像，失败后换官方"
    );
    assert_eq!(
        store.route("mojang-objects").expect("read"),
        Some(SourceId::new("official").expect("id")),
        "换源结果要落盘"
    );
}

/// 已取消时不重试：重试会把「用户放弃」变成继续下载
#[tokio::test]
async fn downloader_does_not_retry_after_cancel() {
    let probe = ScriptedProbe::new(&[(SERVER_JAR, 10)]);
    let router = router(probe, Arc::new(MemoryRouteStore::default()));
    let inner = ScriptedDownloader::failing(SERVER_JAR, 503);
    let downloader =
        RoutingDownloader::new(Arc::clone(&inner) as Arc<dyn ArtifactDownloader>, router);

    let request = DownloadRequest {
        location: location(),
        destination: PathBuf::from("server.jar"),
        expected: None,
        expected_size: None,
        mirrors: Vec::new(),
    };
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = downloader
        .download(&request, None, &cancel)
        .await
        .expect_err("取消后仍报错");
    assert!(!err.retryable() || cancel.is_cancelled());
    assert_eq!(inner.seen().len(), 1, "取消后不得换源重下");
}

/// 不可重试的失败（资源不存在）不作废选择：换源不会让它存在
#[tokio::test]
async fn downloader_keeps_choice_on_missing_resource() {
    let probe = ScriptedProbe::new(&[(SERVER_JAR, 10)]);
    let store = Arc::new(MemoryRouteStore::default());
    let router = router(
        Arc::clone(&probe),
        Arc::clone(&store) as Arc<dyn SourceRouteStore>,
    );
    let inner = ScriptedDownloader::failing(SERVER_JAR, 404);
    let downloader =
        RoutingDownloader::new(Arc::clone(&inner) as Arc<dyn ArtifactDownloader>, router);

    let request = DownloadRequest {
        location: location(),
        destination: PathBuf::from("server.jar"),
        expected: None,
        expected_size: None,
        mirrors: Vec::new(),
    };
    let cancel = CancellationToken::new();
    downloader
        .download(&request, None, &cancel)
        .await
        .expect_err("404 应当失败");
    assert_eq!(inner.seen().len(), 1, "404 不该触发换源");
    assert_eq!(
        store.route("mojang-objects").expect("read"),
        Some(SourceId::new("official").expect("id")),
        "404 不作废选择"
    );
}
