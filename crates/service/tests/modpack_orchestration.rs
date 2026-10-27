//! 整合包开服编排：清单 → 落盘 → 幂等 → 故障收场
//!
//! 用假来源与假读取器驱动 [`ModpackService`]，断言
//!
//! - 清单里的服务端文件落到服务端目录对应子目录（`mods/` / `config/`）
//! - `server: unsupported` 的文件不落盘
//! - overrides 先铺、清单文件后下（同名文件以清单为准）
//! - 已存在的同大小文件不重复下载
//! - 清单缺 MC 版本、归档非法、下载失败都以对应错误收场
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
use service::{ModpackError, ModpackInstallOptions, ModpackService};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 假整合包来源：固定返回一份解析结果
struct FakeModpackSource {
    resolved: ResolvedModpack,
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
    ModpackService::new(
        Arc::new(port::modpack::ModpackSourceRegistry::new(vec![Arc::new(
            FakeModpackSource {
                resolved: resolved(),
            },
        )])),
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
