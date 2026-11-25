//! `CurseForge` 来源的端点契约测试
//!
//! 上游是**本地回环 mock**，不打外部网络：这里验证「请求长什么样」与「响应
//! 怎么翻译成 port 契约」——query 参数、检索回退顺序、slug 反查、版本候选、
//! 文件解析与 CDN 地址换算。真实上游的正确性由 `MCSUP_NET_TESTS=1` 的手动用例
//! 承担。
//!
//! 用本地 `TcpListener` 而不是 `wiremock`：断言点只有「收到什么」，手写响应
//! 足够，且不给 crate 增加测试期依赖。

#![allow(clippy::expect_used, reason = "测试夹具，失败直接 panic 才有用")]

use domain::server::content::{ContentKind, ContentRequest};
use port::content::{ContentQuery, ContentSource, ContentSourceInfo};
use provider::curseforge::CurseForgeSource;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SEARCH_HIT: &str = r#"{
  "id": 227639,
  "name": "The Twilight Forest",
  "slug": "the-twilight-forest",
  "summary": "A realm basked in mystery",
  "classId": 6,
  "downloadCount": 213729220,
  "logo": {
    "thumbnailUrl": "https://media.forgecdn.net/avatars/thumb.png",
    "url": "https://media.forgecdn.net/avatars/full.png"
  },
  "authors": [{"name": "Benimatic"}],
  "latestFilesIndexes": [{"gameVersion": "1.20.1", "modLoader": 1}],
  "links": {"websiteUrl": "https://www.curseforge.com/minecraft/mc-mods/the-twilight-forest"}
}"#;

const FILE: &str = r#"{
  "id": 5468648,
  "modId": 227639,
  "fileName": "twilightforest-1.20.1-4.3.2508-universal.jar",
  "displayName": "Twilight Forest - MC 1.20.1 - 4.3.2508",
  "releaseType": 1,
  "fileDate": "2024-06-24T18:35:07.257Z",
  "fileLength": 23332091,
  "downloadUrl": null,
  "hashes": [
    {"value": "fa3b506a45d3e9c465551cd533fd50c899496e9f", "algo": 1},
    {"value": "0e1ca36ec2ca7e35059dea1600aa31ee", "algo": 2}
  ],
  "gameVersions": ["Client", "NeoForge", "1.20.1", "Forge", "Server"]
}"#;

/// 一次收到的请求
#[derive(Debug, Clone)]
struct Received {
    /// 请求行里的路径 + query（原样，未解码）
    target: String,
}

/// 一条预设响应
struct Route {
    method: &'static str,
    path: &'static str,
    /// 可选：`target` 必须包含该子串才命中（同一路径的不同 query 用）
    contains: Option<&'static str>,
    status: u16,
    body: String,
}

fn route(method: &'static str, path: &'static str, body: String) -> Route {
    Route {
        method,
        path,
        contains: None,
        status: 200,
        body,
    }
}

fn route_when(
    method: &'static str,
    path: &'static str,
    contains: &'static str,
    body: String,
) -> Route {
    Route {
        method,
        path,
        contains: Some(contains),
        status: 200,
        body,
    }
}

/// mock 上游：按「方法 + 路径 + query 子串」返回预置响应，并记录收到的请求
struct Mock {
    base: String,
    seen: Arc<Mutex<Vec<Received>>>,
}

impl Mock {
    async fn start(routes: Vec<Route>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_for_task = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen_for_task.clone();
                let routes = routes
                    .iter()
                    .map(|route| Route {
                        method: route.method,
                        path: route.path,
                        contains: route.contains,
                        status: route.status,
                        body: route.body.clone(),
                    })
                    .collect::<Vec<_>>();
                tokio::spawn(async move {
                    serve(stream, seen, routes).await;
                });
            }
        });
        Self {
            base: format!("http://{addr}/curseforge/v1"),
            seen,
        }
    }

    fn source(&self) -> Box<dyn ContentSource> {
        // 返回 trait 对象：`CurseForgeSource` 自身也有 `search` / `project` /
        // `candidates` / `resolve` 这几个同名方法（参数是上游模型），直接
        // 调用会解析到固有方法而不是 port 契约
        Box::new(CurseForgeSource::for_test(&self.base).expect("source"))
    }

    fn requests(&self) -> Vec<Received> {
        self.seen.lock().expect("seen").clone()
    }
}

async fn serve(mut stream: TcpStream, seen: Arc<Mutex<Vec<Received>>>, routes: Vec<Route>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let Ok(n) = stream.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let Some(head_end) = find_subsequence(&buf, b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        let body_start = head_end + 4;
        if buf.len() < body_start + content_length {
            continue;
        }
        let mut parts = head.lines().next().unwrap_or_default().split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let target = parts.next().unwrap_or_default().to_owned();
        seen.lock().expect("seen").push(Received {
            target: target.clone(),
        });

        let path = target.split('?').next().unwrap_or_default();
        let matched = routes.iter().find(|route| {
            route.method == method
                && route.path == path
                && route.contains.is_none_or(|needle| target.contains(needle))
        });
        let (status, payload) = matched
            .map_or((404, "{\"error\":\"not_found\"}".to_owned()), |r| {
                (r.status, r.body.clone())
            });
        let reason = match status {
            200 => "OK",
            404 => "Not Found",
            429 => "Too Many Requests",
            _ => "Internal Server Error",
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
        return;
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn empty_page() -> String {
    r#"{"data":[],"pagination":{"index":0,"pageSize":20,"resultCount":0,"totalCount":0}}"#
        .to_owned()
}

fn hit_page() -> String {
    format!(
        r#"{{"data":[{SEARCH_HIT}],"pagination":{{"index":0,"pageSize":20,"resultCount":1,"totalCount":123}}}}"#
    )
}

fn file_envelope(mod_id: u64) -> String {
    // `modId` 参与归属校验，用例按需改写
    format!(
        r#"{{"data":{}}}"#,
        FILE.replace("\"modId\": 227639", &format!("\"modId\": {mod_id}"))
    )
}

#[tokio::test]
async fn search_sends_expected_query_and_parses_hits() {
    let mock = Mock::start(vec![route("GET", "/curseforge/v1/mods/search", hit_page())]).await;
    let source = mock.source();
    let query = ContentQuery::new()
        .keyword("twilight forest")
        .kinds([ContentKind::Mod])
        .loaders(["forge"])
        .game_versions(["1.20.1"])
        .page(0, 100);

    let found = source.search(&query).await.expect("search");
    assert_eq!(found.total, 123);
    assert_eq!(found.hits.len(), 1);
    let hit = &found.hits[0];
    assert_eq!(hit.source, "curseforge");
    assert_eq!(hit.id, "227639");
    assert_eq!(hit.slug.as_deref(), Some("the-twilight-forest"));
    assert_eq!(hit.loaders, vec!["forge"]);
    assert_eq!(hit.game_versions, vec!["1.20.1"]);

    let target = &mock.requests()[0].target;
    for expected in [
        "gameId=432",
        "classId=6",
        "searchFilter=twilight",
        "gameVersion=1.20.1",
        "modLoaderType=1",
        "sortField=2",
        "sortOrder=desc",
        "index=0",
        // limit 被夹到上游上限
        "pageSize=50",
    ] {
        assert!(target.contains(expected), "{expected} missing in {target}");
    }
}

/// 关键词不含空白时先试精确 slug：一次命中就停，不再发全文检索
#[tokio::test]
async fn search_prefers_exact_slug() {
    let mock = Mock::start(vec![route_when(
        "GET",
        "/curseforge/v1/mods/search",
        "slug=the-twilight-forest",
        hit_page(),
    )])
    .await;
    let source = mock.source();
    let query = ContentQuery::new().keyword("the-twilight-forest");
    let found = source.search(&query).await.expect("search");
    assert_eq!(found.hits.len(), 1);
    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "{requests:#?}");
    assert!(requests[0].target.contains("slug=the-twilight-forest"));
    assert!(!requests[0].target.contains("searchFilter="));
}

/// 连写 modid 像 `twilightforest`：原词落空后按切词结果重试并命中
///
/// 上游全文检索按分隔符切词，连写词搜不到本体（实测）。回退顺序必须是
/// slug → 原词 → 切两段，且命中即停。
#[tokio::test]
async fn search_falls_back_to_split_terms() {
    let mock = Mock::start(vec![
        route_when(
            "GET",
            "/curseforge/v1/mods/search",
            "slug=twilightforest",
            empty_page(),
        ),
        route_when(
            "GET",
            "/curseforge/v1/mods/search",
            "searchFilter=twilightforest",
            empty_page(),
        ),
        // 切词后的任意一段命中即返回（编码可能是 %20 或 +）
        route_when(
            "GET",
            "/curseforge/v1/mods/search",
            "searchFilter=twilight",
            hit_page(),
        ),
    ])
    .await;
    let source = mock.source();
    let found = source
        .search(&ContentQuery::new().keyword("twilightforest"))
        .await
        .expect("search");
    assert_eq!(found.hits.len(), 1);
    let targets: Vec<String> = mock
        .requests()
        .into_iter()
        .map(|request| request.target)
        .collect();
    assert_eq!(targets.len(), 3, "{targets:#?}");
    assert!(targets[0].contains("slug=twilightforest"), "{targets:#?}");
    assert!(
        targets[1].contains("searchFilter=twilightforest"),
        "{targets:#?}"
    );
    assert!(targets[2].contains("searchFilter=twilight"), "{targets:#?}");
}

/// 不带关键词即浏览：只发一次请求，不带 slug / searchFilter
#[tokio::test]
async fn browse_omits_keyword_filters() {
    let mock = Mock::start(vec![route("GET", "/curseforge/v1/mods/search", hit_page())]).await;
    let source = mock.source();
    source
        .search(&ContentQuery::new())
        .await
        .expect("browse search");
    let target = &mock.requests()[0].target;
    assert!(!target.contains("slug="), "{target}");
    assert!(!target.contains("searchFilter="), "{target}");
}

/// 数值 ID 直接取详情，不做检索
#[tokio::test]
async fn project_by_numeric_id_skips_search() {
    let mock = Mock::start(vec![route(
        "GET",
        "/curseforge/v1/mods/227639",
        format!("{{\"data\":{SEARCH_HIT}}}"),
    )])
    .await;
    let source = mock.source();
    let project = source.project("227639").await.expect("project");
    assert_eq!(project.source, "curseforge");
    assert_eq!(project.id, "227639");
    assert_eq!(project.slug.as_deref(), Some("the-twilight-forest"));
    assert_eq!(project.loaders, vec!["forge"]);
    assert_eq!(mock.requests().len(), 1);
}

/// slug 先反查再取详情：只认 slug 精确相等的那一条，不拿首位凑数
#[tokio::test]
async fn project_by_slug_resolves_exact_match() {
    let other = SEARCH_HIT
        .replace("\"id\": 227639", "\"id\": 1")
        .replace("the-twilight-forest", "twilight-forest-unofficial");
    let page = format!(
        r#"{{"data":[{other},{SEARCH_HIT}],"pagination":{{"index":0,"pageSize":10,"resultCount":2,"totalCount":2}}}}"#
    );
    let mock = Mock::start(vec![
        route_when(
            "GET",
            "/curseforge/v1/mods/search",
            "slug=the-twilight-forest",
            page,
        ),
        route(
            "GET",
            "/curseforge/v1/mods/227639",
            format!("{{\"data\":{SEARCH_HIT}}}"),
        ),
    ])
    .await;
    let source = mock.source();
    let project = source
        .project("the-twilight-forest")
        .await
        .expect("project by slug");
    assert_eq!(project.id, "227639");
    let targets: Vec<String> = mock
        .requests()
        .into_iter()
        .map(|request| request.target)
        .collect();
    assert_eq!(targets.len(), 2, "{targets:#?}");
    assert!(targets[0].contains("slug=the-twilight-forest"));
    assert_eq!(targets[1], "/curseforge/v1/mods/227639");
}

/// 版本候选：gameVersion / modLoaderType / pageSize 都进 query，且只留正式版
#[tokio::test]
async fn candidates_filters_and_ranks_files() {
    let beta = FILE
        .replace("\"id\": 5468648", "\"id\": 5468649")
        .replace("\"releaseType\": 1", "\"releaseType\": 2");
    let page = format!(
        r#"{{"data":[{FILE},{beta}],"pagination":{{"index":0,"pageSize":50,"resultCount":2,"totalCount":2}}}}"#
    );
    let mock = Mock::start(vec![route("GET", "/curseforge/v1/mods/227639/files", page)]).await;
    let source = mock.source();
    let request = ContentRequest::new(ContentKind::Mod)
        .loader("forge")
        .game_version("1.20.1");
    let candidates = source
        .candidates("227639", &request)
        .await
        .expect("candidates");
    assert_eq!(candidates.len(), 1, "{candidates:#?}");
    assert_eq!(candidates[0].version_id, "5468648");
    assert_eq!(candidates[0].channel, "release");
    assert_eq!(candidates[0].loaders, vec!["neoforge", "forge"]);
    assert_eq!(candidates[0].game_versions, vec!["1.20.1"]);

    let target = &mock.requests()[0].target;
    for expected in ["gameVersion=1.20.1", "modLoaderType=1", "pageSize=50"] {
        assert!(target.contains(expected), "{expected} missing in {target}");
    }
}

/// 解析文件：版本 ID 是文件 ID，下载地址缺失时按文件 ID 换算官方 CDN
#[tokio::test]
async fn resolve_maps_file_to_download() {
    let mock = Mock::start(vec![route(
        "GET",
        "/curseforge/v1/mods/227639/files/5468648",
        file_envelope(227_639),
    )])
    .await;
    let source = mock.source();
    let resolved = source.resolve("227639", "5468648").await.expect("resolve");
    assert_eq!(resolved.project_id, "227639");
    assert_eq!(resolved.version_id, "5468648");
    assert_eq!(
        resolved.file.filename,
        "twilightforest-1.20.1-4.3.2508-universal.jar"
    );
    assert_eq!(resolved.file.size, 23_332_091);
    assert_eq!(
        resolved.file.location.as_str(),
        "https://mediafilez.forgecdn.net/files/5468/648/twilightforest-1.20.1-4.3.2508-universal.jar"
    );
    let checksum = resolved.file.checksum.expect("sha1");
    assert!(checksum.is_verified());
}

/// 文件声明的 `modId` 与目标项目不符：拼错版本 ID 时必须报错，不能装错
#[tokio::test]
async fn resolve_rejects_file_from_another_project() {
    let mock = Mock::start(vec![route(
        "GET",
        "/curseforge/v1/mods/227639/files/5468648",
        file_envelope(999),
    )])
    .await;
    let source = mock.source();
    let error = source
        .resolve("227639", "5468648")
        .await
        .expect_err("must reject");
    assert!(
        format!("{error}").contains("does not belong"),
        "unexpected error: {error}"
    );
}

/// 上游不提供按哈希反查：默认实现必须返回空，而不是发请求
#[tokio::test]
async fn by_hash_is_not_supported() {
    let mock = Mock::start(vec![]).await;
    let source = mock.source();
    let identities = source
        .by_hash(&["fa3b506a45d3e9c465551cd533fd50c899496e9f".to_owned()])
        .await
        .expect("by_hash");
    assert!(identities.is_empty());
    assert!(mock.requests().is_empty());
}

#[test]
fn source_declares_name_and_kinds() {
    let source = CurseForgeSource::for_test("http://127.0.0.1:1/curseforge/v1").expect("source");
    assert_eq!(ContentSourceInfo::name(&source), "curseforge");
    assert_eq!(source.supported_kinds(), ContentKind::ALL.to_vec());
}
