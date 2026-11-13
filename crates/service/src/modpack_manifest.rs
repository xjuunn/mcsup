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

    /// 按文件名形态找可能提供某个 modid 的文件
    ///
    /// [`Self::file_providing`] 只认已回填的 modid，而整合包按
    /// `server: unsupported` 跳过的文件从未下载过，`modids` 永远是空的。
    /// 这些文件在清单里却记着项目 ID 与版本 ID（藏在下载地址里），是补依赖
    /// 最权威的线索。文件名与 modid 的对应关系在整合包里高度稳定：
    ///
    /// | 清单路径 | 缺失的 modid |
    /// | --- | --- |
    /// | `mods/SimplyTooltips-fabric-0.1.5-1.20.1.jar` | `simplytooltips` |
    /// | `mods/antique-atlas-3.1.2+1.20.jar` | `antique_atlas` |
    ///
    /// 归一化后按非字母数字切段、逐段累加：遇到以数字开头的段（版本号）即
    /// 停止，累加长度超过目标即放弃。这样 `antique-atlas` 会命中而
    /// `antiqueatlastweaks` 不会，也不会被 `fabric` / `forge` 之类的加载器
    /// 后缀带偏。
    ///
    /// 只作候选，不是判定：命中的文件仍要经 `expect_modid` 与安装检查，
    /// 作者把文件命名成别的 mod 时会被 `WrongProvider` 拒下。多条命中说明
    /// 文件名不足以区分，返回 `None` 交回上游检索，不猜。
    #[must_use]
    pub fn file_by_name_hint(&self, modid: &str) -> Option<&SnapshotFile> {
        let want = normalize_modid(modid);
        if want.is_empty() {
            return None;
        }
        let mut hit: Option<&SnapshotFile> = None;
        for file in &self.files {
            // 已回填 modid 的条目走 [`Self::file_providing`]：那是权威结论，
            // 不能让文件名形态反过来覆盖它
            if !file.modids.is_empty() {
                continue;
            }
            if name_hints_modid(&file.path, &want) {
                if hit.is_some() {
                    return None;
                }
                hit = Some(file);
            }
        }
        hit
    }

    /// 找整合包为某个 modid 指定的文件：权威索引优先，文件名形态兜底
    ///
    /// 补依赖的两条路径（`plan_fix` 与补装队列的依赖展开）必须走同一个
    /// 入口，否则一处认已回填的 modid、另一处只认文件名，判定就会分叉成
    /// 「计划里能补、执行时补不上」。
    ///
    /// 顺序有意义：已回填的 modid 来自读过 jar 的结论，是权威数据；文件名
    /// 只是形态线索，不能反过来覆盖它。
    #[must_use]
    pub fn provider_for(&self, modid: &str) -> Option<&SnapshotFile> {
        self.file_providing(modid)
            .or_else(|| self.file_by_name_hint(modid))
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

/// modid 的归一化形式：只留 ASCII 字母数字，小写
fn normalize_modid(modid: &str) -> String {
    modid
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

/// 文件名里紧跟 modid 之后、不改变 mod 身份的修饰段
///
/// 加载器与平台标记常被拼进文件名（`SimplyTooltips-fabric-0.1.5.jar`），
/// 它们出现在 modid 之后不代表文件名还有另一段语义。不在表里的非数字段
/// 则说明这是另一个 mod 的名字：`sodium-extra` 不是 `sodium`。
const NAME_SUFFIX_NOISE: [&str; 7] = [
    "fabric",
    "forge",
    "neoforge",
    "quilt",
    "mc",
    "universal",
    "common",
];

/// 清单路径的文件名主干是否以「归一化 modid」开头
///
/// 按非字母数字切段后逐段累加，遇到以数字开头的段（版本号）即停止：
/// `antique-atlas-3.1.2+1.20` 累加到 `antiqueatlas` 时命中，而
/// `antiqueatlastweaks-1.0.0` 的第一段就已经超过目标长度，不会误配。
/// 后续段只有让累加值变长，因此超长后可以直接放弃。
///
/// 命中后还要求紧随的下一段是版本号或加载器修饰，否则文件名里还有另一段
/// 有意义的词，说明它不是本体的文件名（`sodium-extra.jar` 不能被拿去补
/// `sodium`）。
fn name_hints_modid(path: &str, want: &str) -> bool {
    let Some(name) = path.rsplit('/').next() else {
        return false;
    };
    let stem = name.strip_suffix(".jar").unwrap_or(name);
    let mut segments = stem
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|segment| !segment.is_empty());
    let mut acc = String::with_capacity(want.len());
    while let Some(segment) = segments.next() {
        if segment.starts_with(|ch: char| ch.is_ascii_digit()) {
            break;
        }
        for ch in segment.chars() {
            if ch.is_ascii_alphanumeric() {
                acc.push(ch.to_ascii_lowercase());
            }
        }
        if acc.len() > want.len() {
            return false;
        }
        if acc == want {
            return match segments.next() {
                None => true,
                Some(next) => {
                    next.starts_with(|ch: char| ch.is_ascii_digit())
                        || NAME_SUFFIX_NOISE.contains(&next.to_ascii_lowercase().as_str())
                }
            };
        }
    }
    false
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

    /// 现场：Rising Legends 3.1 把两个 `server: unsupported` 的库跳过了，
    /// 它们的 `modids` 永远是空的。缺 `simplytooltips` / `antique_atlas` 时
    /// 必须能从清单里认出作者指定的那个文件，而不是报「上游找不到提供方」。
    #[test]
    fn name_hint_finds_skipped_client_side_libraries() {
        let pack = ModpackIndex::new(
            1,
            "minecraft",
            "3.1",
            "Rising Legends",
            ModpackDependencies {
                raw: BTreeMap::new(),
            },
            vec![
                file(
                    "mods/SimplyTooltips-fabric-0.1.5-1.20.1.jar",
                    "https://cdn.modrinth.com/data/6avVoBVB/versions/MAwQRftH/SimplyTooltips-fabric-0.1.5-1.20.1.jar",
                    EnvSupport::Unsupported,
                ),
                file(
                    "mods/antique-atlas-3.1.2+1.20.jar",
                    "https://cdn.modrinth.com/data/Y5Ve4Ui4/versions/yIV6zMoy/antique-atlas-3.1.2%2B1.20.jar",
                    EnvSupport::Unsupported,
                ),
                // 真实整合包里同名的另一个 mod：提供 antique_atlas_item，
                // 不能因为名字前段相同就把它当成 antique_atlas 的候选
                file(
                    "mods/antiqueatlastweaks-1.0.0.jar",
                    "https://cdn.modrinth.com/data/ljpY4NNp/versions/rsNs3JVm/antiqueatlastweaks-1.0.0.jar",
                    EnvSupport::Required,
                ),
            ],
        )
        .expect("清单合法");
        let snapshot = ModpackManifestSnapshot::from_index(
            "modrinth",
            "Qx4KOI2G",
            "i6zyHTyK",
            "3.1",
            &pack,
            1_794_455_524,
        );

        let tooltips = snapshot
            .file_by_name_hint("simplytooltips")
            .expect("SimplyTooltips 命中");
        assert_eq!(tooltips.project_id.as_deref(), Some("6avVoBVB"));
        assert_eq!(tooltips.version_id.as_deref(), Some("MAwQRftH"));

        let atlas = snapshot
            .file_by_name_hint("antique_atlas")
            .expect("antique-atlas 命中");
        assert_eq!(atlas.project_id.as_deref(), Some("Y5Ve4Ui4"));
        assert_eq!(atlas.version_id.as_deref(), Some("yIV6zMoy"));
    }

    /// 名字相近但不是本体的文件不能被当成提供方：`sodium-extra` 与
    /// `environmentz` 各自是独立的 mod，不能拿去补 `sodium` / `environment`。
    ///
    /// 反过来，`antiqueatlastweaks` 提供的是 `antique_atlas_item`，其文件名
    /// 形态与这个 modid 对不上（名字里还有 `tweaks` 一段），此时不猜：交给
    /// 已回填的权威索引或上游检索。
    #[test]
    fn name_hint_does_not_match_longer_or_different_names() {
        let snapshot = snapshot();
        assert!(snapshot.file_by_name_hint("sodium").is_none());
        assert_eq!(
            snapshot
                .file_by_name_hint("sodiumextra")
                .map(|file| file.path.as_str()),
            Some("mods/sodium-extra.jar")
        );
        assert!(snapshot.file_by_name_hint("environment").is_none());

        let mut tweaks = snapshot.clone();
        tweaks.files.push(SnapshotFile::from_manifest(&file(
            "mods/antiqueatlastweaks-1.0.0.jar",
            "https://cdn.modrinth.com/data/ljpY4NNp/versions/rsNs3JVm/antiqueatlastweaks-1.0.0.jar",
            EnvSupport::Required,
        )));
        assert!(tweaks.file_by_name_hint("antique_atlas").is_none());
        assert!(tweaks.file_by_name_hint("antique_atlas_item").is_none());
    }

    /// 多条命中说明文件名不足以区分，交回检索而不是猜一个
    #[test]
    fn name_hint_returns_none_when_ambiguous() {
        let mut snapshot = snapshot();
        snapshot.files.push(SnapshotFile::from_manifest(&file(
            "mods/sodium-extra-1.0.jar",
            "https://cdn.modrinth.com/data/OTHER/versions/V/sodium-extra-1.0.jar",
            EnvSupport::Unsupported,
        )));
        assert!(snapshot.file_by_name_hint("sodium").is_none());
    }

    /// 已回填 modid 的条目由权威索引负责，文件名形态不能覆盖它
    #[test]
    fn name_hint_skips_entries_with_recorded_modids() {
        let mut snapshot = snapshot();
        assert!(snapshot.record_modids(
            "mods/sodium-extra.jar",
            vec!["unrelated".to_owned()],
            None
        ));
        assert!(snapshot.file_by_name_hint("sodium").is_none());
        assert!(snapshot.file_providing("unrelated").is_some());
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
