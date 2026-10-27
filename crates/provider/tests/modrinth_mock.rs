//! Modrinth 客户端的端点契约测试
//!
//! 默认不跑；设 `MCSUP_NET_TESTS=1` 手动执行。上游是**本地 mock 服务**，
//! 不打真实网络：这里要验证的是「请求长什么样」——方法、路径、query 参数、
//! 请求体、以及非 2xx 的错误归类。真实上游的正确性由 `modrinth_net.rs`
//! 承担（它才需要联网）。
//!
//! 用本地服务而不是 `wiremock` 之类的依赖，是因为断言点只有「收到什么」，
//! 用一个 `TcpListener` + 手写响应足够，且不给 crate 增加测试期依赖。

#![allow(clippy::expect_used, reason = "测试夹具，失败直接 panic 才有用")]

use port::modpack::ModpackSource;
use provider::modrinth::{
    Facet, FacetTree, HashAlgorithm, ModrinthModpackSource, ModrinthSource, ProjectType,
    SearchIndex, SearchQuery, VersionChannel, VersionFilter, primary_file,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SEARCH: &str = include_str!("fixtures/modrinth_search.json");
const PROJECT: &str = include_str!("fixtures/modrinth_project.json");
const VERSIONS: &str = include_str!("fixtures/modrinth_versions.json");
const DEPENDENCIES: &str = include_str!("fixtures/modrinth_dependencies.json");
const LOADERS: &str = include_str!("fixtures/modrinth_loaders.json");
const GAME_VERSIONS: &str = include_str!("fixtures/modrinth_game_versions.json");
const CATEGORIES: &str = include_str!("fixtures/modrinth_categories.json");
const PROJECT_TYPES: &str = include_str!("fixtures/modrinth_project_types.json");

fn enabled() -> bool {
    std::env::var("MCSUP_NET_TESTS").is_ok_and(|v| v == "1")
}

/// 一次收到的请求
#[derive(Debug, Clone)]
struct Received {
    method: String,
    /// 请求行里的路径 + query（原样，未解码）
    target: String,
    /// 请求体
    body: String,
}

/// mock 上游：按路径前缀返回预置响应，并记录收到的请求
struct Mock {
    base: String,
    seen: Arc<Mutex<Vec<Received>>>,
}

impl Mock {
    /// 启动 mock；`routes` 的键是「方法 + 路径」，值为 `(状态码, body)`
    async fn start(routes: Vec<(&'static str, &'static str, u16, String)>) -> Self {
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
                let routes = routes.clone();
                tokio::spawn(async move {
                    serve(stream, seen, routes).await;
                });
            }
        });
        Self {
            base: format!("http://{addr}/v2"),
            seen,
        }
    }

    /// 注入给客户端的 `base_url`
    fn base_https_placeholder(&self) -> String {
        self.base.clone()
    }

    fn requests(&self) -> Vec<Received> {
        self.seen.lock().expect("seen").clone()
    }
}

async fn serve(
    mut stream: TcpStream,
    seen: Arc<Mutex<Vec<Received>>>,
    routes: Vec<(&'static str, &'static str, u16, String)>,
) {
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
        if let Some(head_end) = find_subsequence(&buf, b"\r\n\r\n") {
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
            let body =
                String::from_utf8_lossy(&buf[body_start..body_start + content_length]).to_string();
            seen.lock().expect("seen").push(Received {
                method: method.clone(),
                target: target.clone(),
                body,
            });

            let path = target.split('?').next().unwrap_or_default();
            let matched = routes
                .iter()
                .find(|(m, p, _, _)| *m == method && *p == path);
            let (status, payload) = matched
                .map_or((404, "{\"error\":\"not_found\"}".to_owned()), |r| {
                    (r.2, r.3.clone())
                });
            let reason = match status {
                200 => "OK",
                400 => "Bad Request",
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
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 构造指向 mock 的来源
///
/// `ModrinthSource::builder` 只接受 `https`（生产约束），测试里要打本地
/// `http` 服务，因此这里用测试专用入口。
fn source_for(mock: &Mock) -> ModrinthSource {
    ModrinthSource::for_test(&mock.base_https_placeholder()).expect("source")
}

/// 搜索：query / facets / index / 分页都进 query，limit 被夹到 100
#[tokio::test]
async fn search_sends_expected_query() {
    if !enabled() {
        return;
    }
    let mock = Mock::start(vec![("GET", "/v2/search", 200, SEARCH.to_owned())]).await;
    let source = source_for(&mock);
    let facets = FacetTree::new()
        .and([Facet::eq("project_type", "mod")])
        .and([Facet::eq("versions", "1.21.1")]);
    let results = source
        .search(
            &SearchQuery::new()
                .query("sodium")
                .facets(facets)
                .index(SearchIndex::Downloads)
                .page(20, 500),
        )
        .await
        .expect("search");
    assert_eq!(results.total_hits, 2);

    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert!(
        request.target.starts_with("/v2/search?"),
        "{}",
        request.target
    );
    assert!(
        request.target.contains("query=sodium"),
        "{}",
        request.target
    );
    assert!(
        request.target.contains("index=downloads"),
        "{}",
        request.target
    );
    assert!(request.target.contains("offset=20"), "{}", request.target);
    // 上游上限 100：500 必须被夹住
    assert!(request.target.contains("limit=100"), "{}", request.target);
    // facets 是 JSON 数组字符串，必须百分号编码
    assert!(
        request
            .target
            .contains("facets=%5B%5B%22project_type%3Dmod%22%5D"),
        "{}",
        request.target
    );
}

/// 搜索不带关键词时不发 `query=` 参数
#[tokio::test]
async fn search_omits_empty_query() {
    if !enabled() {
        return;
    }
    let mock = Mock::start(vec![("GET", "/v2/search", 200, SEARCH.to_owned())]).await;
    let source = source_for(&mock);
    source.search(&SearchQuery::new()).await.expect("search");
    let target = mock.requests()[0].target.clone();
    assert!(!target.contains("query="), "{target}");
    assert!(!target.contains("facets="), "{target}");
    assert!(target.contains("index=relevance"), "{target}");
}

/// 项目详情：slug 进路径段
#[tokio::test]
async fn project_uses_path_segment() {
    if !enabled() {
        return;
    }
    let mock = Mock::start(vec![(
        "GET",
        "/v2/project/fabulously-optimized",
        200,
        PROJECT.to_owned(),
    )])
    .await;
    let source = source_for(&mock);
    let project = source
        .project("fabulously-optimized")
        .await
        .expect("project");
    assert_eq!(project.project_type(), ProjectType::Modpack);
    assert_eq!(
        mock.requests()[0].target,
        "/v2/project/fabulously-optimized"
    );
}

/// 批量项目：POST + JSON 数组请求体
#[tokio::test]
async fn projects_posts_json_array() {
    if !enabled() {
        return;
    }
    let body = format!("[{PROJECT}]");
    let mock = Mock::start(vec![("POST", "/v2/projects", 200, body)]).await;
    let source = source_for(&mock);
    let projects = source
        .projects(&["AABBCCDD".to_owned()])
        .await
        .expect("projects");
    assert_eq!(projects.len(), 1);
    let request = &mock.requests()[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.body, r#"["AABBCCDD"]"#);
}

/// 版本列表：`loaders` / `game_versions` 是 JSON 数组字符串，`featured` 与 changelog 开关都下发
#[tokio::test]
async fn project_versions_sends_filters() {
    if !enabled() {
        return;
    }
    let mock = Mock::start(vec![(
        "GET",
        "/v2/project/AABBCCDD/version",
        200,
        VERSIONS.to_owned(),
    )])
    .await;
    let source = source_for(&mock);
    let filter = VersionFilter::new()
        .loaders(["fabric", "quilt"])
        .game_versions(["1.21.1"])
        .without_changelog();
    let versions = source
        .project_versions("AABBCCDD", &filter)
        .await
        .expect("versions");
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].channel(), VersionChannel::Release);

    let target = &mock.requests()[0].target;
    assert!(
        target.starts_with("/v2/project/AABBCCDD/version?"),
        "{target}"
    );
    // JSON 数组字符串编码后仍含 `%22fabric%22`
    assert!(
        target.contains("loaders=%5B%22fabric%22%2C%22quilt%22%5D"),
        "{target}"
    );
    assert!(
        target.contains("game_versions=%5B%221.21.1%22%5D"),
        "{target}"
    );
    assert!(target.contains("include_changelog=false"), "{target}");
}

/// 单版本与批量版本
#[tokio::test]
async fn version_endpoints() {
    if !enabled() {
        return;
    }
    // 从夹具的数组里取出第一个版本对象
    let all: serde_json::Value = serde_json::from_str(VERSIONS).expect("versions 夹具");
    let single = all[0].to_string();
    let mock = Mock::start(vec![
        ("GET", "/v2/version/IIJJKKLL", 200, single),
        ("GET", "/v2/versions", 200, VERSIONS.to_owned()),
    ])
    .await;
    let source = source_for(&mock);
    let version = source.version("IIJJKKLL").await.expect("version");
    assert_eq!(version.id, "IIJJKKLL");

    let versions = source
        .versions(&["IIJJKKLL".to_owned(), "QQRRSSTT".to_owned()])
        .await
        .expect("versions");
    assert_eq!(versions.len(), 2);

    let requests = mock.requests();
    assert_eq!(requests[0].target, "/v2/version/IIJJKKLL");
    assert!(
        requests[1]
            .target
            .contains("ids=%5B%22IIJJKKLL%22%2C%22QQRRSSTT%22%5D"),
        "{}",
        requests[1].target
    );
}

/// 单哈希反查：算法进 query，未命中归 404
#[tokio::test]
async fn version_from_hash_and_not_found() {
    if !enabled() {
        return;
    }
    // 用一个能解析成 Version 的最小对象
    let minimal = r#"{"version_number":"1.0.0","id":"V","project_id":"P","author_id":"A","date_published":"2024-01-01T00:00:00Z","downloads":0,"files":[]}"#.to_owned();
    let mock = Mock::start(vec![("GET", "/v2/version_file/abc", 200, minimal)]).await;
    let source = source_for(&mock);
    let version = source
        .version_from_hash("abc", HashAlgorithm::Sha512)
        .await
        .expect("version");
    assert_eq!(version.id, "V");
    assert!(
        mock.requests()[0].target.contains("algorithm=sha512"),
        "{}",
        mock.requests()[0].target
    );

    let source = source_for(&mock);
    let err = source
        .version_from_hash("missing", HashAlgorithm::Sha1)
        .await
        .expect_err("应报未命中");
    assert!(
        matches!(err, port::SourceError::Upstream { status: 404, .. }),
        "{err:?}"
    );
}

/// 批量哈希反查：POST body 带 hashes 与 algorithm，返回 map
#[tokio::test]
async fn versions_from_hashes_posts_map() {
    if !enabled() {
        return;
    }
    let payload = r#"{"abc":{"version_number":"1.0.0","id":"V","project_id":"P","author_id":"A","date_published":"2024-01-01T00:00:00Z","downloads":0,"files":[]}}"#.to_owned();
    let mock = Mock::start(vec![("POST", "/v2/version_files", 200, payload)]).await;
    let source = source_for(&mock);
    let found = source
        .versions_from_hashes(&["abc".to_owned()], HashAlgorithm::Sha1)
        .await
        .expect("versions");
    assert_eq!(found.len(), 1);
    assert!(found.contains_key("abc"));

    let request = &mock.requests()[0];
    assert_eq!(request.method, "POST");
    let body: HashMap<String, serde_json::Value> =
        serde_json::from_str(&request.body).expect("body 应为 JSON");
    assert_eq!(body["algorithm"], serde_json::json!("sha1"));
    assert_eq!(body["hashes"], serde_json::json!(["abc"]));
}

/// 依赖图与标签端点
#[tokio::test]
async fn dependencies_and_tags() {
    if !enabled() {
        return;
    }
    let mock = Mock::start(vec![
        (
            "GET",
            "/v2/project/AABBCCDD/dependencies",
            200,
            DEPENDENCIES.to_owned(),
        ),
        ("GET", "/v2/tag/loader", 200, LOADERS.to_owned()),
        ("GET", "/v2/tag/game_version", 200, GAME_VERSIONS.to_owned()),
        ("GET", "/v2/tag/category", 200, CATEGORIES.to_owned()),
        ("GET", "/v2/tag/project_type", 200, PROJECT_TYPES.to_owned()),
    ])
    .await;
    let source = source_for(&mock);
    let graph = source.project_dependencies("AABBCCDD").await.expect("deps");
    assert_eq!(graph.projects.len(), 1);
    assert_eq!(source.loaders().await.expect("loaders").len(), 2);
    assert_eq!(source.game_versions().await.expect("games").len(), 2);
    assert_eq!(source.categories().await.expect("categories").len(), 2);
    assert_eq!(source.project_types().await.expect("types").len(), 7);

    let targets: Vec<String> = mock
        .requests()
        .into_iter()
        .map(|request| request.target)
        .collect();
    assert_eq!(
        targets,
        vec![
            "/v2/project/AABBCCDD/dependencies",
            "/v2/tag/loader",
            "/v2/tag/game_version",
            "/v2/tag/category",
            "/v2/tag/project_type",
        ]
    );
}

/// 团队成员端点：`/team/{id}/members`
#[tokio::test]
async fn team_members_endpoint() {
    if !enabled() {
        return;
    }
    let payload = r#"[{"team_id":"MMNNOOPP","user":{"id":"U","username":"robotkoer","name":"Robotkoer"},"role":"Owner","accepted":true}]"#.to_owned();
    let mock = Mock::start(vec![("GET", "/v2/team/MMNNOOPP/members", 200, payload)]).await;
    let source = source_for(&mock);
    let members = source.team_members("MMNNOOPP").await.expect("team");
    assert_eq!(members[0].user.username, "robotkoer");
    assert_eq!(members[0].user.name.as_deref(), Some("Robotkoer"));
    assert_eq!(mock.requests()[0].target, "/v2/team/MMNNOOPP/members");
}

/// 429 归类为可重试，404 归类为不可重试
#[tokio::test]
async fn error_classification() {
    if !enabled() {
        return;
    }
    let mock = Mock::start(vec![("GET", "/v2/project/limited", 429, "{}".to_owned())]).await;
    let source = source_for(&mock);
    let err = source.project("limited").await.expect_err("应报错");
    assert!(err.retryable(), "{err:?}");

    let mock = Mock::start(vec![]).await;
    let source = source_for(&mock);
    let err = source.project("missing").await.expect_err("应报错");
    assert!(!err.retryable(), "{err:?}");
    assert!(
        matches!(err, port::SourceError::Upstream { status: 404, .. }),
        "{err:?}"
    );
}

/// 上游返回非法 JSON 归类为解码错误（不可重试）
#[tokio::test]
async fn invalid_json_is_decode_error() {
    if !enabled() {
        return;
    }
    let mock = Mock::start(vec![(
        "GET",
        "/v2/project/broken",
        200,
        "not json".to_owned(),
    )])
    .await;
    let source = source_for(&mock);
    let err = source.project("broken").await.expect_err("应报错");
    assert!(matches!(err, port::SourceError::Decode(_)), "{err:?}");
    assert!(!err.retryable());
}

/// 版本的主文件选择在真实夹具上成立
#[tokio::test]
async fn primary_file_on_fixture() {
    if !enabled() {
        return;
    }
    let mock = Mock::start(vec![(
        "GET",
        "/v2/project/AABBCCDD/version",
        200,
        VERSIONS.to_owned(),
    )])
    .await;
    let source = source_for(&mock);
    let versions = source
        .project_versions("AABBCCDD", &VersionFilter::new())
        .await
        .expect("versions");
    let file = primary_file(&versions[0]).expect("主文件");
    assert_eq!(file.filename, "pack.mrpack");
}

/// 解析下载目标时优先取服务端归档，而不是 primary 的客户端包
#[tokio::test]
async fn resolve_prefers_server_archive() {
    if !enabled() {
        return;
    }
    let mut versions: Vec<serde_json::Value> =
        serde_json::from_str(VERSIONS).expect("versions fixture");
    let mut server = versions[0]["files"][0].clone();
    server["filename"] = serde_json::json!("pack [server].mrpack");
    server["url"] = serde_json::json!(
        "https://cdn.modrinth.com/data/AABBCCDD/versions/IIJJKKLL/pack%20%5Bserver%5D.mrpack"
    );
    server["primary"] = serde_json::json!(false);
    server["size"] = serde_json::json!(42);
    versions[0]["files"]
        .as_array_mut()
        .expect("files")
        .push(server);
    let body = serde_json::to_string(&versions[0]).expect("json");
    let mock = Mock::start(vec![("GET", "/v2/version/IIJJKKLL", 200, body)]).await;
    let source = ModrinthModpackSource::from_source(source_for(&mock));

    let resolved = source
        .resolve("AABBCCDD", Some("IIJJKKLL"))
        .await
        .expect("resolve");

    assert_eq!(resolved.file.filename, "pack [server].mrpack");
    assert_eq!(resolved.file.size, 42);
}

/// 不指定版本时，默认跳过没有服务端归档的最新正式版
#[tokio::test]
async fn resolve_defaults_to_version_with_server_archive() {
    if !enabled() {
        return;
    }
    let versions: Vec<serde_json::Value> =
        serde_json::from_str(VERSIONS).expect("versions fixture");
    let newest = versions[0].clone();
    let mut older = newest.clone();
    older["id"] = serde_json::json!("AABBCC11");
    older["version_number"] = serde_json::json!("4.9.0");
    older["date_published"] = serde_json::json!("2024-01-01T00:00:00Z");
    let mut server = older["files"][0].clone();
    server["filename"] = serde_json::json!("pack [server].mrpack");
    server["url"] = serde_json::json!(
        "https://cdn.modrinth.com/data/AABBCCDD/versions/AABBCC11/pack%20%5Bserver%5D.mrpack"
    );
    server["primary"] = serde_json::json!(false);
    server["size"] = serde_json::json!(42);
    older["files"].as_array_mut().expect("files").push(server);
    let list = serde_json::to_string(&vec![newest, older.clone()]).expect("list json");
    let older_body = serde_json::to_string(&older).expect("version json");
    let mock = Mock::start(vec![
        ("GET", "/v2/project/AABBCCDD/version", 200, list),
        ("GET", "/v2/version/AABBCC11", 200, older_body),
    ])
    .await;
    let source = ModrinthModpackSource::from_source(source_for(&mock));

    let resolved = source.resolve("AABBCCDD", None).await.expect("resolve");

    assert_eq!(resolved.version_id, "AABBCC11");
    assert_eq!(resolved.file.filename, "pack [server].mrpack");
    assert_eq!(resolved.file.size, 42);
    assert!(
        mock.requests()
            .iter()
            .all(|request| request.target != "/v2/version/IIJJKKLL"),
        "不应再去取没有服务端归档的最新版"
    );
}

/// mock 自身自检：没收到请求时断言用的辅助会失败
#[tokio::test]
async fn mock_records_nothing_when_unused() {
    if !enabled() {
        return;
    }
    let mock = Mock::start(vec![]).await;
    assert!(mock.requests().is_empty());
}
