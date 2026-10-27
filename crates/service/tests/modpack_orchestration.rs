//! 整合包开服编排：清单 → 落盘 → 幂等 → 故障收场
//!
//! 用假来源与假读取器驱动 [`ModpackService`]，断言
//!
//! - 清单里的服务端文件落到服务端目录对应子目录（`mods/` / `config/`）
//! - `server: unsupported` 的文件不落盘
//! - 上游标注 `server_side=unsupported` 的模组即使清单写 `server: required`
//!   也不落盘，旧文件被清掉；只反查 `mods/` 下的 `.jar`
//! - overrides 先铺、清单文件后下（同名文件以清单为准）
//! - 已存在的同大小文件不重复下载
//! - 清单缺 MC 版本、归档非法、下载失败、元数据查询失败都以对应错误收场
//!
//! 真正的 zip 解包由 provider 的 `MrpackExtractor` 承担，另在
//! `crates/provider/tests/modrinth_mrpack.rs` 用真实 `.mrpack` 覆盖。

#![allow(clippy::expect_used)]

use async_trait::async_trait;
use domain::server::modpack::{ModpackDependencies, ModpackFile, ModpackHashes, ModpackIndex};
use port::content::{ContentFile, ContentSourceInfo};
use port::modpack::{
    ModpackDetail, ModpackExtractError, ModpackExtractor, ModpackQuery, ModpackSearch,
    ModpackSource, ModpackVersion, ResolvedModpack,
};
use port::{
    ArtifactDownloader, CancellationToken, DownloadError, DownloadRequest, DownloadedArtifact,
    ProgressSink, SourceError,
};
use service::{ModpackError, ModpackInstallOptions, ModpackPrepared, ModpackService};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 假整合包来源：固定返回一份解析结果，可注入「仅客户端」元数据
struct FakeModpackSource {
    resolved: ResolvedModpack,
    /// 上游判定为仅客户端的文件哈希
    client_only: Vec<String>,
    /// 模拟上游元数据查询失败
    metadata_fails: bool,
    /// 收到过的哈希批次（按调用顺序）
    queried: Arc<Mutex<Vec<Vec<String>>>>,
}

impl FakeModpackSource {
    fn new() -> Self {
        Self {
            resolved: resolved(),
            client_only: Vec::new(),
            metadata_fails: false,
            queried: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 指定上游判成仅客户端的哈希
    fn client_only(mut self, hashes: &[&str]) -> Self {
        self.client_only = hashes.iter().map(|hash| (*hash).to_owned()).collect();
        self
    }

    /// 让元数据查询失败
    fn failing(mut self) -> Self {
        self.metadata_fails = true;
        self
    }

    /// 共享的「收到的哈希批次」句柄
    fn queried(&self) -> Arc<Mutex<Vec<Vec<String>>>> {
        self.queried.clone()
    }
}

#[async_trait]
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

    async fn client_only_hashes(&self, hashes: &[String]) -> Result<HashSet<String>, SourceError> {
        self.queried.lock().expect("queried").push(hashes.to_vec());
        if self.metadata_fails {
            return Err(SourceError::Status { status: 503 });
        }
        Ok(hashes
            .iter()
            .filter(|hash| self.client_only.contains(hash))
            .cloned()
            .collect())
    }
}

/// 不提供元数据能力的来源：走 trait 默认实现，必须保持「按清单 env 装」的旧行为
struct LegacyModpackSource(FakeModpackSource);

#[async_trait]
impl ContentSourceInfo for LegacyModpackSource {
    fn name(&self) -> &'static str {
        "fake"
    }
}

#[async_trait]
impl ModpackSource for LegacyModpackSource {
    async fn search(&self, query: &ModpackQuery) -> Result<ModpackSearch, SourceError> {
        self.0.search(query).await
    }

    async fn detail(&self, id: &str) -> Result<ModpackDetail, SourceError> {
        self.0.detail(id).await
    }

    async fn versions(&self, id: &str) -> Result<Vec<ModpackVersion>, SourceError> {
        self.0.versions(id).await
    }

    async fn resolve(
        &self,
        id: &str,
        version: Option<&str>,
    ) -> Result<ResolvedModpack, SourceError> {
        self.0.resolve(id, version).await
    }
}

/// 假读取器：清单与 overrides 由用例给定
struct FakeExtractor {
    index: ModpackIndex,
    /// overrides 里的相对路径与内容
    overrides: Vec<(String, Vec<u8>)>,
}

impl ModpackExtractor for FakeExtractor {
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

/// 假下载器：记录请求、按预期大小写盘、可注入失败
struct FakeDownloader {
    requests: Arc<Mutex<Vec<DownloadRequest>>>,
    fail: bool,
}

impl FakeDownloader {
    fn ok() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            fail: false,
        }
    }

    fn failing() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            fail: true,
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
        let len = request.expected_size.unwrap_or(1).max(1);
        let len = usize::try_from(len).expect("size fits usize");
        std::fs::write(&request.destination, vec![b'x'; len]).expect("write");
        Ok(DownloadedArtifact {
            path: request.destination.clone(),
            size: len as u64,
        })
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mcsup-modpack-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("tempdir");
    dir
}

fn deps(pairs: &[(&str, &str)]) -> ModpackDependencies {
    ModpackDependencies {
        raw: pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn file(path: &str, size: u64, server: EnvTag) -> ModpackFile {
    ModpackFile {
        path: path.to_owned(),
        downloads: vec![format!("https://cdn.example.invalid/{path}")],
        size,
        hashes: ModpackHashes::default(),
        env: match server {
            EnvTag::None => None,
            EnvTag::Required => Some(domain::server::modpack::FileEnv {
                client: domain::server::modpack::EnvSupport::Required,
                server: domain::server::modpack::EnvSupport::Required,
            }),
            EnvTag::Unsupported => Some(domain::server::modpack::FileEnv {
                client: domain::server::modpack::EnvSupport::Required,
                server: domain::server::modpack::EnvSupport::Unsupported,
            }),
        },
    }
}

/// 带 sha1 的清单文件（仅客户端判定按 sha1 反查上游）
fn file_with_sha1(path: &str, size: u64, server: EnvTag, sha1: &str) -> ModpackFile {
    let mut file = file(path, size, server);
    file.hashes.sha1 = Some(sha1.to_owned());
    file
}

/// 40 位十六进制的假 sha1，内容不重要，只要可区分
fn sha1(tag: u8) -> String {
    format!("{tag:040x}")
}

#[derive(Clone, Copy)]
enum EnvTag {
    None,
    Required,
    Unsupported,
}

fn resolved() -> ResolvedModpack {
    ResolvedModpack {
        project_id: "P".to_owned(),
        version_id: "V".to_owned(),
        version_number: "1.0.0".to_owned(),
        file: ContentFile {
            location: "https://cdn.example.invalid/pack.mrpack"
                .parse()
                .expect("url"),
            filename: "pack.mrpack".to_owned(),
            size: 4,
            checksum: None,
        },
    }
}

fn service(
    index: ModpackIndex,
    overrides: Vec<(&str, &[u8])>,
    downloader: FakeDownloader,
) -> ModpackService {
    service_with_source(
        index,
        overrides,
        downloader,
        Arc::new(FakeModpackSource::new()),
    )
}

fn service_with_source(
    index: ModpackIndex,
    overrides: Vec<(&str, &[u8])>,
    downloader: FakeDownloader,
    source: Arc<dyn ModpackSource>,
) -> ModpackService {
    ModpackService::new(
        Arc::new(port::modpack::ModpackSourceRegistry::new(vec![source])),
        Arc::new(FakeExtractor {
            index,
            overrides: overrides
                .into_iter()
                .map(|(path, bytes)| (path.to_owned(), bytes.to_vec()))
                .collect(),
        }),
        Arc::new(downloader),
    )
}

/// 清单文件落到服务端对应子目录，且 overrides 也在
#[tokio::test]
async fn applies_files_and_overrides() {
    let index = ModpackIndex::new(
        1,
        "minecraft",
        "1.0.0",
        "Pack",
        deps(&[("minecraft", "1.21.1"), ("fabric-loader", "0.16.0")]),
        vec![
            file("mods/a.jar", 4, EnvTag::Required),
            file("config/x.toml", 2, EnvTag::None),
        ],
    )
    .expect("index");
    let service = service(index, vec![("pack.meta", b"meta")], FakeDownloader::ok());
    let dir = temp_dir("apply");

    let prepared = service
        .prepare(
            "P",
            ModpackInstallOptions::new(&dir),
            &CancellationToken::new(),
        )
        .await
        .expect("prepare");
    let applied = service
        .apply(&prepared, &dir, None, &CancellationToken::new())
        .await
        .expect("apply");

    assert_eq!(applied.file_count, 2);
    assert_eq!(applied.downloaded_bytes, 6);
    assert!(dir.join("mods/a.jar").is_file());
    assert!(dir.join("config/x.toml").is_file());
    // overrides 内容
    assert_eq!(std::fs::read(dir.join("pack.meta")).expect("meta"), b"meta");
    // 暂存的 .mrpack 在服务端目录下的 staging 里
    assert!(
        prepared
            .archive_path
            .starts_with(dir.join("modpack-staging"))
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `server: unsupported` 的文件不落盘
#[tokio::test]
async fn skips_server_unsupported_files() {
    let index = ModpackIndex::new(
        1,
        "minecraft",
        "1.0.0",
        "Pack",
        deps(&[("minecraft", "1.21.1")]),
        vec![
            file("mods/server.jar", 4, EnvTag::Required),
            file("mods/client-only.jar", 4, EnvTag::Unsupported),
        ],
    )
    .expect("index");
    let service = service(index, Vec::new(), FakeDownloader::ok());
    let dir = temp_dir("skip");

    let prepared = service
        .prepare(
            "P",
            ModpackInstallOptions::new(&dir),
            &CancellationToken::new(),
        )
        .await
        .expect("prepare");
    service
        .apply(&prepared, &dir, None, &CancellationToken::new())
        .await
        .expect("apply");

    assert!(dir.join("mods/server.jar").is_file());
    assert!(
        !dir.join("mods/client-only.jar").exists(),
        "客户端专属文件不该落盘"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 已存在同大小文件时不重复下载
#[tokio::test]
async fn reuses_existing_same_size_file() {
    let index = ModpackIndex::new(
        1,
        "minecraft",
        "1.0.0",
        "Pack",
        deps(&[("minecraft", "1.21.1")]),
        vec![file("mods/a.jar", 4, EnvTag::Required)],
    )
    .expect("index");
    let downloader = FakeDownloader::ok();
    let seen = downloader.requests.clone();
    let service = service(index, Vec::new(), downloader);
    let dir = temp_dir("reuse");
    std::fs::create_dir_all(dir.join("mods")).expect("mkdir");
    std::fs::write(dir.join("mods/a.jar"), b"xxxx").expect("seed");

    let prepared = service
        .prepare(
            "P",
            ModpackInstallOptions::new(&dir),
            &CancellationToken::new(),
        )
        .await
        .expect("prepare");
    let applied = service
        .apply(&prepared, &dir, None, &CancellationToken::new())
        .await
        .expect("apply");

    assert_eq!(applied.reused, 1);
    assert_eq!(applied.downloaded_bytes, 0);
    // 只有 .mrpack 那一次下载，没有任何 mod 下载
    assert_eq!(seen.lock().expect("seen").len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

/// 清单里没有 MC 版本：prepare 能读到清单，但调用方取版本时报错
#[tokio::test]
async fn missing_minecraft_is_reported() {
    let index = ModpackIndex::new(
        1,
        "minecraft",
        "1.0.0",
        "Pack",
        deps(&[("fabric-loader", "0.16.0")]),
        Vec::new(),
    )
    .expect("index");
    let service = service(index, Vec::new(), FakeDownloader::ok());
    let dir = temp_dir("nomc");

    let prepared = service
        .prepare(
            "P",
            ModpackInstallOptions::new(&dir),
            &CancellationToken::new(),
        )
        .await
        .expect("prepare");
    assert!(matches!(
        prepared.minecraft_version(),
        Err(ModpackError::MissingMinecraft(_))
    ));
    std::fs::remove_dir_all(&dir).ok();
}

/// 下载 .mrpack 失败 → prepare 报 Download 错误
#[tokio::test]
async fn mrpack_download_failure_propagates() {
    let index = ModpackIndex::new(
        1,
        "minecraft",
        "1.0.0",
        "Pack",
        deps(&[("minecraft", "1.21.1")]),
        Vec::new(),
    )
    .expect("index");
    let service = service(index, Vec::new(), FakeDownloader::failing());
    let dir = temp_dir("dlfail");

    let err = service
        .prepare(
            "P",
            ModpackInstallOptions::new(&dir),
            &CancellationToken::new(),
        )
        .await
        .expect_err("应失败");
    assert!(matches!(err, ModpackError::Download(_)), "{err:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// 清单文件下载失败 → apply 报 Download 错误，overrides 已铺（可重试）
#[tokio::test]
async fn file_download_failure_propagates() {
    let index = ModpackIndex::new(
        1,
        "minecraft",
        "1.0.0",
        "Pack",
        deps(&[("minecraft", "1.21.1")]),
        vec![file("mods/a.jar", 4, EnvTag::Required)],
    )
    .expect("index");
    let service = service(
        index,
        vec![("pack.meta", b"meta")],
        FakeDownloader::failing(),
    );
    let dir = temp_dir("filefail");

    let prepared = service
        .prepare(
            "P",
            ModpackInstallOptions::new(&dir),
            &CancellationToken::new(),
        )
        .await
        .expect_err("prepare 也要下载 .mrpack");
    assert!(
        matches!(prepared, ModpackError::Download(_)),
        "{prepared:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 未知来源与无来源
#[tokio::test]
async fn unknown_source_is_reported() {
    let index =
        ModpackIndex::new(1, "minecraft", "1.0.0", "Pack", deps(&[]), Vec::new()).expect("index");
    let service = service(index, Vec::new(), FakeDownloader::ok());
    let err = service
        .prepare(
            "P",
            ModpackInstallOptions::new(Path::new("/tmp")).source("absent"),
            &CancellationToken::new(),
        )
        .await
        .expect_err("应失败");
    assert!(matches!(err, ModpackError::UnknownSource(_)), "{err:?}");
}

/// 上游标注 `server_side=unsupported` 的模组，即使清单写 `server: required`
/// 也不装到服务端；下载列表里不应出现它
#[tokio::test]
async fn skips_upstream_client_only_mods() {
    let client_hash = sha1(1);
    let server_hash = sha1(2);
    let index = ModpackIndex::new(
        1,
        "minecraft",
        "1.0.0",
        "Pack",
        deps(&[("minecraft", "1.21.1")]),
        vec![
            file_with_sha1("mods/server.jar", 4, EnvTag::Required, &server_hash),
            file_with_sha1("mods/sodium.jar", 6, EnvTag::Required, &client_hash),
        ],
    )
    .expect("index");
    let source = FakeModpackSource::new().client_only(&[client_hash.as_str()]);
    let queried = source.queried();
    let downloader = FakeDownloader::ok();
    let seen = downloader.requests.clone();
    let service = service_with_source(index, Vec::new(), downloader, Arc::new(source));
    let dir = temp_dir("clientonly");

    let prepared = service
        .prepare(
            "P",
            ModpackInstallOptions::new(&dir),
            &CancellationToken::new(),
        )
        .await
        .expect("prepare");
    let applied = service
        .apply(&prepared, &dir, None, &CancellationToken::new())
        .await
        .expect("apply");

    assert!(
        dir.join("mods/server.jar").is_file(),
        "服务端模组应落盘: {}",
        dir.display()
    );
    assert!(
        !dir.join("mods/sodium.jar").exists(),
        "上游标注仅客户端的模组不该落盘: {}",
        dir.display()
    );
    assert_eq!(
        applied.skipped_client_only, 1,
        "应报告跳过了 1 个仅客户端模组: {applied:?}"
    );
    assert_eq!(
        applied.file_count, 1,
        "file_count 只算实际安装的文件: {applied:?}"
    );
    let downloads: Vec<String> = seen
        .lock()
        .expect("seen")
        .iter()
        .map(|request| request.location.to_string())
        .collect();
    assert!(
        downloads.iter().all(|url| !url.contains("sodium.jar")),
        "不该下载仅客户端模组，实际请求: {downloads:?}"
    );
    // 查询只发一次，且带上清单里两个 mod 的 sha1
    let batches = queried.lock().expect("queried").clone();
    assert_eq!(batches.len(), 1, "一次 apply 只应查一次元数据: {batches:?}");
    assert_eq!(
        batches[0].iter().cloned().collect::<HashSet<_>>(),
        HashSet::from([client_hash, server_hash]),
        "应把清单里 mods 的 sha1 全部交给来源: {batches:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 上一次安装留下的仅客户端模组要被清掉，否则重装后服务端仍然起不来
#[tokio::test]
async fn removes_previously_installed_client_only_mod() {
    let client_hash = sha1(3);
    let index = ModpackIndex::new(
        1,
        "minecraft",
        "1.0.0",
        "Pack",
        deps(&[("minecraft", "1.21.1")]),
        vec![file_with_sha1(
            "mods/embeddium.jar",
            5,
            EnvTag::Required,
            &client_hash,
        )],
    )
    .expect("index");
    let source = FakeModpackSource::new().client_only(&[client_hash.as_str()]);
    let downloader = FakeDownloader::ok();
    let service = service_with_source(index, Vec::new(), downloader, Arc::new(source));
    let dir = temp_dir("clientonly-stale");
    std::fs::create_dir_all(dir.join("mods")).expect("mkdir");
    std::fs::write(dir.join("mods/embeddium.jar"), b"stale").expect("seed");

    let prepared = service
        .prepare(
            "P",
            ModpackInstallOptions::new(&dir),
            &CancellationToken::new(),
        )
        .await
        .expect("prepare");
    let applied = service
        .apply(&prepared, &dir, None, &CancellationToken::new())
        .await
        .expect("apply");

    assert!(
        !dir.join("mods/embeddium.jar").exists(),
        "旧的仅客户端模组应被删除: {}",
        dir.display()
    );
    assert_eq!(applied.skipped_client_only, 1, "{applied:?}");
    assert_eq!(applied.reused, 0, "被删掉的文件不算复用: {applied:?}");
    assert_eq!(applied.downloaded_bytes, 0, "{applied:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// 只有 `mods/` 下的 `.jar` 才需要查上游：配置、资源包、清单已排除的
/// `server: unsupported` 文件、以及由 overrides 提供的文件都不该占用请求预算
#[tokio::test]
async fn only_mod_jars_are_queried() {
    // 没有下载地址：内容由 overrides/ 提供，不该反查也不能下载
    let mut override_mod = file_with_sha1("mods/from-override.jar", 4, EnvTag::Required, &sha1(15));
    override_mod.downloads.clear();
    let index = ModpackIndex::new(
        1,
        "minecraft",
        "1.0.0",
        "Pack",
        deps(&[("minecraft", "1.21.1")]),
        vec![
            file_with_sha1("mods/a.jar", 4, EnvTag::Required, &sha1(10)),
            file_with_sha1("mods/sub/b.JAR", 4, EnvTag::None, &sha1(11)),
            file_with_sha1("config/x.toml", 2, EnvTag::None, &sha1(12)),
            file_with_sha1("resourcepacks/rp.zip", 2, EnvTag::None, &sha1(13)),
            // 清单已判定服务端不支持，不必再问上游
            file_with_sha1("mods/client-env.jar", 4, EnvTag::Unsupported, &sha1(14)),
            // 清单没给 sha1：无法反查，仍按清单安装
            file("mods/no-hash.jar", 4, EnvTag::Required),
            override_mod,
        ],
    )
    .expect("index");
    let source = FakeModpackSource::new();
    let queried = source.queried();
    let service = service_with_source(index, Vec::new(), FakeDownloader::ok(), Arc::new(source));
    let dir = temp_dir("onlymods");

    let prepared = service
        .prepare(
            "P",
            ModpackInstallOptions::new(&dir),
            &CancellationToken::new(),
        )
        .await
        .expect("prepare");
    let applied = service
        .apply(&prepared, &dir, None, &CancellationToken::new())
        .await
        .expect("apply");

    let batches = queried.lock().expect("queried").clone();
    assert_eq!(batches.len(), 1, "{batches:?}");
    assert_eq!(
        batches[0].iter().cloned().collect::<HashSet<_>>(),
        HashSet::from([sha1(10), sha1(11)]),
        "只应反查 mods/ 下的 jar（扩展名大小写不敏感）: {batches:?}"
    );
    assert!(dir.join("mods/no-hash.jar").is_file(), "没有 sha1 也要装");
    assert!(
        !dir.join("mods/from-override.jar").exists(),
        "没有下载地址的文件只由 overrides 提供，apply 不该下载它"
    );
    assert_eq!(applied.skipped_client_only, 0, "{applied:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// 元数据查询失败时中止安装：带着未知的仅客户端状态继续落盘，会让服务端
/// 在启动阶段崩掉，且错误现场远离真正原因
#[tokio::test]
async fn metadata_failure_aborts_apply() {
    let index = ModpackIndex::new(
        1,
        "minecraft",
        "1.0.0",
        "Pack",
        deps(&[("minecraft", "1.21.1")]),
        vec![file_with_sha1("mods/a.jar", 4, EnvTag::Required, &sha1(20))],
    )
    .expect("index");
    let source = FakeModpackSource::new().failing();
    let downloader = FakeDownloader::ok();
    let seen = downloader.requests.clone();
    let service = service_with_source(index, Vec::new(), downloader, Arc::new(source));
    let dir = temp_dir("metadatafail");

    let prepared = service
        .prepare(
            "P",
            ModpackInstallOptions::new(&dir),
            &CancellationToken::new(),
        )
        .await
        .expect("prepare");
    let err = service
        .apply(&prepared, &dir, None, &CancellationToken::new())
        .await
        .expect_err("元数据失败应中止");

    assert!(matches!(err, ModpackError::Source(_)), "{err:?}");
    assert!(
        !dir.join("mods/a.jar").exists(),
        "中止后不该落任何清单文件: {}",
        dir.display()
    );
    // 只有 .mrpack 那一次下载
    assert_eq!(
        seen.lock().expect("seen").len(),
        1,
        "元数据失败前不该下载 mod"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 来源不支持元数据查询时保持旧行为：清单写什么就装什么
#[tokio::test]
async fn source_without_metadata_keeps_manifest_files() {
    let index = ModpackIndex::new(
        1,
        "minecraft",
        "1.0.0",
        "Pack",
        deps(&[("minecraft", "1.21.1")]),
        vec![file_with_sha1("mods/a.jar", 4, EnvTag::Required, &sha1(30))],
    )
    .expect("index");
    let legacy = LegacyModpackSource(FakeModpackSource::new());
    let service = service_with_source(index, Vec::new(), FakeDownloader::ok(), Arc::new(legacy));
    let dir = temp_dir("legacy");

    let prepared = service
        .prepare(
            "P",
            ModpackInstallOptions::new(&dir),
            &CancellationToken::new(),
        )
        .await
        .expect("prepare");
    let applied = service
        .apply(&prepared, &dir, None, &CancellationToken::new())
        .await
        .expect("apply");

    assert!(dir.join("mods/a.jar").is_file(), "旧行为应照常安装");
    assert_eq!(applied.skipped_client_only, 0, "{applied:?}");
    assert_eq!(applied.file_count, 1, "{applied:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// 清单里没有模组时不发元数据请求
#[tokio::test]
async fn no_mods_means_no_metadata_query() {
    let index = ModpackIndex::new(
        1,
        "minecraft",
        "1.0.0",
        "Pack",
        deps(&[("minecraft", "1.21.1")]),
        vec![file_with_sha1(
            "config/x.toml",
            2,
            EnvTag::Required,
            &sha1(40),
        )],
    )
    .expect("index");
    let source = FakeModpackSource::new();
    let queried = source.queried();
    let service = service_with_source(index, Vec::new(), FakeDownloader::ok(), Arc::new(source));
    let dir = temp_dir("nomods");

    let prepared = service
        .prepare(
            "P",
            ModpackInstallOptions::new(&dir),
            &CancellationToken::new(),
        )
        .await
        .expect("prepare");
    service
        .apply(&prepared, &dir, None, &CancellationToken::new())
        .await
        .expect("apply");

    assert!(
        queried.lock().expect("queried").is_empty(),
        "没有 mod 时不该查上游",
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `prepare` 记下的来源名在 `apply` 时找不到 → 报未知来源，而不是默默用默认来源
#[tokio::test]
async fn unknown_source_in_prepared_is_reported() {
    let index = ModpackIndex::new(
        1,
        "minecraft",
        "1.0.0",
        "Pack",
        deps(&[("minecraft", "1.21.1")]),
        vec![file_with_sha1("mods/a.jar", 4, EnvTag::Required, &sha1(50))],
    )
    .expect("index");
    let dir = temp_dir("ghost");
    let prepared = ModpackPrepared {
        source: "ghost",
        resolved: resolved(),
        index,
        archive_path: PathBuf::from("/tmp/ghost.mrpack"),
    };
    let service = service(prepared.index.clone(), Vec::new(), FakeDownloader::ok());

    let err = service
        .apply(&prepared, &dir, None, &CancellationToken::new())
        .await
        .expect_err("来源不在注册表里应报错");
    assert!(
        matches!(&err, ModpackError::UnknownSource(name) if name == "ghost"),
        "{err:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
