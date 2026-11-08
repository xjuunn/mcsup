//! 整合包清单快照：装完把清单留在服务端目录里，供后续操作直接查
//!
//! # 为什么要在本地留一份
//!
//! `.mrpack` 的 `modrinth.index.json` 是这份整合包**唯一的权威描述**：每个
//! 文件的下载地址、字节数、sha1、两侧环境标记都在里面。而 Modrinth 的下载
//! 地址形如
//!
//! ```text
//! https://cdn.modrinth.com/data/{project_id}/versions/{version_id}/{filename}
//! ```
//!
//! 项目 ID 与版本 ID 就藏在地址里。有了这两个 ID，后续「补依赖」「查更新」
//! 都能**按 ID 直接定位**，不必拿 modid 去搜索。搜索是按名字猜，会挑错：
//! 真身叫 `satin-api` 的库，搜 `satin` 的首位命中是毫不相干的
//! `satin-free-wakes`（实测）。
//!
//! 装完就把 `.mrpack` 删掉的话，这份权威数据随之丢失，之后只能重新下载
//! 整个整合包、或退回搜索。因此安装完成时把清单落到
//! `<server>/.mcsup-modpack/manifest.json`，与 `.mcsup-staging` / `.mcsup-trash`
//! 一样是服务端目录内的工具目录。
//!
//! # 与 `mods.toml` 的关系
//!
//! 不是一回事：`mods.toml` 记的是用户自己 `mod add` 的条目，本快照记的是
//! **整合包下发**的文件全集。整合包更新要按 `project_id` 比对前后版本，
//! 因此快照必须保留清单原样（含未被安装的仅客户端条目），而不是只留已装文件。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use domain::server::modpack::{EnvSupport, ModpackFile, ModpackIndex};
use serde::{Deserialize, Serialize};

/// 快照目录（服务端目录下）
pub const MANIFEST_DIR: &str = ".mcsup-modpack";

/// 快照文件名
pub const MANIFEST_FILE: &str = "manifest.json";

/// 当前快照格式版本
///
/// 读取时版本不符不做静默降级：宁可当作没有快照，也不要拿旧结构当新结构
/// 解释，进而按错的 ID 去安装。
pub const MANIFEST_FORMAT: u32 = 1;

/// 清单里一个文件的快照
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotFile {
    /// 相对整合包根的路径
    pub path: String,
    /// 下载地址（按顺序尝试）
    #[serde(default)]
    pub downloads: Vec<String>,
    /// 字节数
    #[serde(default)]
    pub size: u64,
    /// sha1（十六进制）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha1: Option<String>,
    /// sha512（十六进制）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha512: Option<String>,
    /// 清单声明的客户端支持情况
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// 清单声明的服务端支持情况
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// 上游项目 ID（从下载地址解析）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// 上游版本 ID（从下载地址解析）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// 这个 jar 实际提供的 modid（装好后读 jar 回填，可多个）
    ///
    /// 装之前无从得知——清单只说文件名，不说 modid。回填之后
    /// 「缺 modid X 该装哪个项目」就有了确定答案，不用搜索。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modids: Vec<String>,
    /// jar 自己声明的版本号（同上，回填）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_version: Option<String>,
}

impl SnapshotFile {
    /// 从清单条目生成
    fn from_manifest(file: &ModpackFile) -> Self {
        let (client, server) = match file.env {
            Some(env) => (Some(env_name(env.client)), Some(env_name(env.server))),
            None => (None, None),
        };
        Self {
            path: file.path.clone(),
            downloads: file.downloads.clone(),
            size: file.size,
            sha1: file.hashes.sha1.clone(),
            sha512: file.hashes.sha512.clone(),
            client,
            server,
            project_id: file.project_id.clone(),
            version_id: file.version_id.clone(),
            modids: Vec::new(),
            mod_version: None,
        }
    }
}

/// 环境标记 → 字符串（与上游 `required` / `optional` / `unsupported` 同形）
fn env_name(support: EnvSupport) -> String {
    match support {
        EnvSupport::Required => "required",
        EnvSupport::Optional => "optional",
        EnvSupport::Unsupported => "unsupported",
    }
    .to_owned()
}

/// 安装时使用的加载器
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotLoader {
    /// 加载器名（`fabric` / `forge` / `neoforge` / `quilt`）
    pub name: String,
    /// 加载器版本
    pub version: String,
}

/// 整合包清单快照
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModpackManifestSnapshot {
    /// 快照格式版本（见 [`MANIFEST_FORMAT`]）
    pub format: u32,
    /// 来源名（`modrinth`）
    pub source: String,
    /// 整合包项目 ID
    pub project_id: String,
    /// 整合包版本 ID
    pub version_id: String,
    /// 整合包版本号
    #[serde(default)]
    pub version_number: String,
    /// 整合包名
    #[serde(default)]
    pub name: String,
    /// 清单自带的版本号（`modrinth.index.json` 的 `versionId`）
    #[serde(default)]
    pub pack_version: String,
    /// 清单声明的 Minecraft 版本
    #[serde(default)]
    pub minecraft: String,
    /// 清单要求的加载器
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader: Option<SnapshotLoader>,
    /// 清单的 `dependencies` 原样保留（含未知键）
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    /// 安装时间（Unix 秒）
    pub installed_at: i64,
    /// 清单里的全部文件（含未装到服务端的条目）
    pub files: Vec<SnapshotFile>,
}

impl ModpackManifestSnapshot {
    /// 从清单与整合包身份生成快照
    ///
    /// 保留**全部**清单文件，包括 `server: unsupported` 的条目：整合包更新要
    /// 按项目 ID 比对前后版本的完整集合，只留已装文件会把「上游把某个仅客户端
    /// 库改成了服务端必需」这种情况漏掉。
    #[must_use]
    pub fn from_index(
        source: &str,
        project_id: &str,
        version_id: &str,
        version_number: &str,
        index: &ModpackIndex,
        installed_at: i64,
    ) -> Self {
        let loader = index
            .dependencies
            .loader()
            .map(|(name, version)| SnapshotLoader {
                name: name.to_owned(),
                version: version.to_owned(),
            });
        let minecraft = index
            .dependencies
            .minecraft()
            .unwrap_or_default()
            .to_owned();
        Self {
            format: MANIFEST_FORMAT,
            source: source.to_owned(),
            project_id: project_id.to_owned(),
            version_id: version_id.to_owned(),
            version_number: version_number.to_owned(),
            name: index.name.clone(),
            pack_version: index.version_id.clone(),
            minecraft,
            loader,
            dependencies: index.dependencies.raw.clone(),
            installed_at,
            files: index
                .files
                .iter()
                .map(SnapshotFile::from_manifest)
                .collect(),
        }
    }

    /// 快照文件路径
    #[must_use]
    pub fn path_in(server_dir: &Path) -> PathBuf {
        server_dir.join(MANIFEST_DIR).join(MANIFEST_FILE)
    }

    /// 写入快照
    ///
    /// # Errors
    ///
    /// 目录创建、序列化或写入失败时返回 [`std::io::Error`]
    pub async fn save(&self, server_dir: &Path) -> std::io::Result<()> {
        let dir = server_dir.join(MANIFEST_DIR);
        tokio::fs::create_dir_all(&dir).await?;
        let text = serde_json::to_string_pretty(self)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        tokio::fs::write(dir.join(MANIFEST_FILE), text).await
    }

    /// 读取快照
    ///
    /// 文件不存在、格式版本不符、JSON 损坏时统一返回 `None`——调用方据此
    /// 退回「没有快照」的旧路径，而不是拿可疑数据去安装。
    #[must_use]
    pub async fn load(server_dir: &Path) -> Option<Self> {
        let text = tokio::fs::read_to_string(Self::path_in(server_dir))
            .await
            .ok()?;
        let snapshot: Self = serde_json::from_str(&text).ok()?;
        (snapshot.format == MANIFEST_FORMAT).then_some(snapshot)
    }

    /// 按 modid 找提供它的文件
    ///
    /// 只认已回填过 modid 的条目；同一 modid 被多个文件提供时取第一个。
    #[must_use]
    pub fn file_providing(&self, modid: &str) -> Option<&SnapshotFile> {
        self.files
            .iter()
            .find(|file| file.modids.iter().any(|id| id == modid))
    }

    /// 按路径找条目
    #[must_use]
    pub fn file_at(&self, path: &str) -> Option<&SnapshotFile> {
        self.files.iter().find(|file| file.path == path)
    }

    /// 按绝对文件名找条目
    ///
    /// 磁盘上只看到 `mods/satin-2.0.0.jar` 这样的文件名，用它反查清单条目，
    /// 从而拿到项目 / 版本 ID。
    #[must_use]
    pub fn file_named(&self, filename: &str) -> Option<&SnapshotFile> {
        self.files
            .iter()
            .find(|file| file.path.rsplit('/').next() == Some(filename))
    }

    /// 把某个文件的 modid 回填进去（装好读 jar 之后调用）
    ///
    /// 返回是否有变化：没变化时调用方不必重写文件。
    pub fn record_modids(
        &mut self,
        path: &str,
        modids: Vec<String>,
        mod_version: Option<String>,
    ) -> bool {
        let Some(file) = self.files.iter_mut().find(|file| file.path == path) else {
            return false;
        };
        if file.modids == modids && file.mod_version == mod_version {
            return false;
        }
        file.modids = modids;
        file.mod_version = mod_version;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::server::modpack::{FileEnv, ModpackDependencies, ModpackHashes};

    fn file(path: &str, url: &str, server: EnvSupport) -> ModpackFile {
        ModpackFile::new(
            path.to_owned(),
            vec![url.to_owned()],
            10,
            ModpackHashes {
                sha1: Some("a".repeat(40)),
                sha512: None,
            },
            Some(FileEnv {
                client: EnvSupport::Required,
                server,
            }),
        )
    }

    fn index() -> ModpackIndex {
        let mut raw = BTreeMap::new();
        raw.insert("minecraft".to_owned(), "1.21.1".to_owned());
        raw.insert("fabric-loader".to_owned(), "0.20.1".to_owned());
        ModpackIndex::new(
            1,
            "minecraft",
            "4.0.1",
            "Aged",
            ModpackDependencies { raw },
            vec![
                file(
                    "mods/environmentz-2.2.0.jar",
                    "https://cdn.modrinth.com/data/TggYmc6t/versions/l7EZhw1K/environmentz-2.2.0.jar",
                    EnvSupport::Required,
                ),
                file(
                    "mods/satin-2.0.0.jar",
                    "https://cdn.modrinth.com/data/fRbqPLg4/versions/Tq9qJzQz/satin-2.0.0.jar",
                    EnvSupport::Required,
                ),
                file(
                    "mods/sodium-extra.jar",
                    "https://cdn.modrinth.com/data/PtjYWJkn/versions/AAA/sodium-extra.jar",
                    EnvSupport::Unsupported,
                ),
            ],
        )
        .expect("清单合法")
    }

    fn snapshot() -> ModpackManifestSnapshot {
        ModpackManifestSnapshot::from_index(
            "modrinth",
            "i4XHCd7Q",
            "kaxASXpi",
            "4.0.1",
            &index(),
            1_700_000_000,
        )
    }

    #[test]
    fn snapshot_keeps_project_and_version_ids_from_download_urls() {
        let snapshot = snapshot();
        let satin = snapshot
            .file_at("mods/satin-2.0.0.jar")
            .expect("satin 在清单里");
        assert_eq!(satin.project_id.as_deref(), Some("fRbqPLg4"));
        assert_eq!(satin.version_id.as_deref(), Some("Tq9qJzQz"));
    }

    #[test]
    fn snapshot_keeps_entries_the_server_does_not_install() {
        // 整合包更新要按项目 ID 比对完整集合，仅客户端条目也必须留住
        let snapshot = snapshot();
        assert_eq!(snapshot.files.len(), 3);
        let unsupported = snapshot
            .file_at("mods/sodium-extra.jar")
            .expect("仅客户端条目也在快照里");
        assert_eq!(unsupported.server.as_deref(), Some("unsupported"));
    }

    #[test]
    fn snapshot_records_pack_loader_and_minecraft() {
        let snapshot = snapshot();
        assert_eq!(snapshot.minecraft, "1.21.1");
        let loader = snapshot.loader.expect("有加载器");
        assert_eq!(loader.name, "fabric");
        assert_eq!(loader.version, "0.20.1");
        assert_eq!(snapshot.project_id, "i4XHCd7Q");
        assert_eq!(snapshot.version_id, "kaxASXpi");
    }

    #[test]
    fn modid_index_answers_which_project_provides_a_modid() {
        let mut snapshot = snapshot();
        // 装好 satin 之后读 jar 回填：modid 为 satin，来自项目 fRbqPLg4
        assert!(snapshot.record_modids(
            "mods/satin-2.0.0.jar",
            vec!["satin".to_owned()],
            Some("2.0.0".to_owned())
        ));
        let file = snapshot.file_providing("satin").expect("能找到 satin");
        assert_eq!(file.project_id.as_deref(), Some("fRbqPLg4"));
        assert_eq!(file.version_id.as_deref(), Some("Tq9qJzQz"));
        // 回填同样的值不产生变化，避免无谓重写
        assert!(!snapshot.record_modids(
            "mods/satin-2.0.0.jar",
            vec!["satin".to_owned()],
            Some("2.0.0".to_owned())
        ));
    }

    #[test]
    fn file_named_matches_by_basename() {
        let snapshot = snapshot();
        let file = snapshot
            .file_named("satin-2.0.0.jar")
            .expect("按文件名命中");
        assert_eq!(file.path, "mods/satin-2.0.0.jar");
        assert!(snapshot.file_named("absent.jar").is_none());
    }

    #[test]
    fn snapshot_roundtrips_through_json() {
        let snapshot = snapshot();
        let text = serde_json::to_string(&snapshot).expect("可序列化");
        let back: ModpackManifestSnapshot = serde_json::from_str(&text).expect("可反序列化");
        assert_eq!(back, snapshot);
    }

    #[tokio::test]
    async fn save_then_load_roundtrips() {
        let dir = std::env::temp_dir().join(format!("mcsup-snap-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.expect("建目录");
        let snapshot = snapshot();
        snapshot.save(&dir).await.expect("可写入");
        let back = ModpackManifestSnapshot::load(&dir).await.expect("可读回");
        assert_eq!(back, snapshot);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn load_returns_none_when_absent_or_corrupt() {
        let dir = std::env::temp_dir().join(format!("mcsup-snap-no-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.expect("建目录");
        // 没有快照
        assert!(ModpackManifestSnapshot::load(&dir).await.is_none());
        // 损坏的 JSON 不能被当成快照
        let path = ModpackManifestSnapshot::path_in(&dir);
        tokio::fs::create_dir_all(path.parent().expect("有父目录"))
            .await
            .expect("建目录");
        tokio::fs::write(&path, b"{not json")
            .await
            .expect("写坏文件");
        assert!(ModpackManifestSnapshot::load(&dir).await.is_none());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn version_mismatch_is_treated_as_absent() {
        let dir = std::env::temp_dir().join(format!("mcsup-snap-ver-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.expect("建目录");
        let mut snapshot = snapshot();
        snapshot.format = MANIFEST_FORMAT + 1;
        snapshot.save(&dir).await.expect("可写入");
        assert!(
            ModpackManifestSnapshot::load(&dir).await.is_none(),
            "格式版本不符时不能冒充快照"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
