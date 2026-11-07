//! Minecraft 版本发布时间表：所有服务端共用的排序基准
//!
//! 各服务端的版本标识几乎都是 MC 版本本身（`1.21.11` / `24w14a`），或以它为
//! 前缀（`Sponge` 的 `26.3-21.0.0`）。但这些标识的**发布时间**没有任何一路
//! 上游给出权威数值：`Fabric` / `Quilt` Meta 只有 `stable` 标记，`Paper` /
//! `Purpur` 只按「新在前」排列，`Forge` / `NeoForge` 的 Maven 元数据按字典序。
//! 唯一权威的时间戳在 `Mojang` 的版本清单里（`releaseTime`）。
//!
//! 于是把清单整理成「版本 id → 发布时间」，供各来源给
//! [`domain::server::version::ServerVersion::released`] 填值。先后仍由
//! [`domain::server::version::ServerVersion::sort_display_order`] 统一判定，
//! 来源只提供数据、不各自比较——否则各写一套排序必然分叉。
//!
//! # 为什么放在 provider
//!
//! `domain` 不做 I/O；清单是网络请求，属于实现层。
//!
//! # 覆盖不到的 id
//!
//! `1.16_combat-3`、`1.18_experimental-snapshot-1` 这类实验版在清单里没有条目
//! （`Mojang` 不发布它们的清单记录）。它们得不到时间，退回版本号口径排序，
//! 不做猜测。

use crate::http::get_json;
use crate::mojang::manifest::VersionManifest;
use port::SourceError;
use reqwest::Client;
use std::collections::HashMap;

/// `Mojang` 官方版本清单：发布时间的唯一权威来源
///
/// 与 [`crate::mojang::MojangSource`] 用同一份数据，但那份是「清单 + 详情」
/// 的来源实现，这里是给**其他**来源补时间用的纯数据。
pub(crate) const MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest.json";

/// 版本 id → 发布时间（RFC 3339 UTC 原文）
///
/// 键是清单里的原始 id，值是该版本的 `releaseTime` 原文。字典序即时间序的
/// 前提由清单保证（定长、统一 `+00:00`）；本类型不做解析，只做传递。
#[derive(Debug, Clone, Default)]
pub(crate) struct ReleaseCalendar {
    times: HashMap<String, String>,
}

impl ReleaseCalendar {
    /// 拉取并整理 `Mojang` 清单
    ///
    /// # Errors
    ///
    /// 清单不可达、返回非成功状态码或响应无法解析时返回 [`SourceError`]
    pub(crate) async fn fetch(http: &Client) -> Result<Self, SourceError> {
        let manifest: VersionManifest = get_json(http, MANIFEST_URL, None).await?;
        Ok(Self::from_manifest(manifest))
    }

    /// 由已解析的清单构造
    ///
    /// 清单里缺 `releaseTime` 的条目被跳过（解析层已把它当可选字段），
    /// 这类版本之后退回版本号排序。
    #[must_use]
    pub(crate) fn from_manifest(manifest: VersionManifest) -> Self {
        let mut times = HashMap::with_capacity(manifest.versions.len());
        for item in manifest.versions {
            if let Some(released) = item.released {
                times.insert(item.id, released);
            }
        }
        Self { times }
    }

    /// 某版本的发布时间；清单不含该 id 时返回 `None`
    ///
    /// 先按原样查，再按去掉平台附加后缀的 id 查：`Fabric` 会为同一版本另发
    /// `1.21.11_unobfuscated`、`24w14potato_original` 这类变体，它们与基础版本
    /// 同时发布，时间应取其基础版本的。
    #[must_use]
    pub(crate) fn released(&self, id: &str) -> Option<&str> {
        self.times.get(id).map(String::as_str).or_else(|| {
            self.times
                .get(strip_variant_suffix(id)?)
                .map(String::as_str)
        })
    }
}

/// 去掉平台附加后缀：`1.21.11_unobfuscated` → `1.21.11`
///
/// 只认已知的两个后缀，其余原样返回 `None`——不做「先按 `_` 切一刀」这种
/// 猜测：`1.18_experimental-snapshot-1` 切出来的 `1.18` 是另一个版本的
/// 发布时间，张冠李戴比没有时间更糟。
fn strip_variant_suffix(id: &str) -> Option<&str> {
    id.strip_suffix("_unobfuscated")
        .or_else(|| id.strip_suffix("_original"))
}

/// 拉取时间表；失败时返回空表并记一条警告
///
/// 时间只决定**顺序**，不决定「能不能装」。为了排序而让 `Fabric` / `Paper`
/// 的版本列表在 `Mojang` 不可达时整体失败，是把装饰性问题升级成功能故障；
/// 降级成版本号口径更符合可用性。
pub(crate) async fn fetch_or_empty(http: &Client) -> ReleaseCalendar {
    match ReleaseCalendar::fetch(http).await {
        Ok(calendar) => calendar,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "Minecraft release calendar unavailable; \
                 version list falls back to version-number order"
            );
            ReleaseCalendar::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = include_str!("../tests/fixtures/version_manifest.json");

    fn manifest() -> VersionManifest {
        serde_json::from_str(MANIFEST).expect("version_manifest fixture")
    }

    /// 清单每条都带时间：id → 原文直接可用，且字典序即时间序
    #[test]
    fn manifest_maps_ids_to_release_time() {
        let calendar = ReleaseCalendar::from_manifest(manifest());
        assert_eq!(
            calendar.released("1.21.1"),
            Some("2024-08-08T00:00:00+00:00")
        );
        assert_eq!(
            calendar.released("24w33a"),
            Some("2024-08-15T00:00:00+00:00")
        );
        // 字典序即时间序：快照晚于正式版，字符串比较直接成立
        assert!(calendar.released("24w33a") > calendar.released("1.21.1"));
    }

    /// 清单里没有的 id 返回 `None`，由排序退回版本号口径
    #[test]
    fn unknown_id_has_no_time() {
        let calendar = ReleaseCalendar::from_manifest(manifest());
        assert_eq!(calendar.released("1.99.999"), None);
        assert_eq!(calendar.released("26.4-snapshot-7"), None);
    }

    /// `Fabric` 的平台附加后缀取基础版本的发布时间
    #[test]
    fn platform_suffix_falls_back_to_base_version() {
        let calendar = ReleaseCalendar::from_manifest(manifest());
        assert_eq!(
            calendar.released("1.21.1_unobfuscated"),
            calendar.released("1.21.1")
        );
        assert_eq!(
            calendar.released("24w33a_unobfuscated"),
            calendar.released("24w33a")
        );
        // `_original` 同理（`24w14potato_original` 取 `24w14potato`）
        assert_eq!(
            calendar.released("1.21.1_original"),
            calendar.released("1.21.1")
        );
    }

    /// 不认识的形状不猜：`1.18_experimental-snapshot-1` 不能借用 `1.18` 的时间
    #[test]
    fn unrecognized_suffix_is_not_guessed() {
        let manifest = VersionManifest {
            latest: crate::mojang::manifest::LatestVersions {
                release: "1.21.1".to_owned(),
                snapshot: "24w33a".to_owned(),
            },
            versions: vec![crate::mojang::manifest::VersionManifestItem {
                id: "1.18".to_owned(),
                kind: crate::mojang::manifest::VersionKind::Release,
                released: Some("2021-11-30T00:00:00+00:00".to_owned()),
                url: "https://example.invalid/1.18.json".to_owned(),
            }],
        };
        let calendar = ReleaseCalendar::from_manifest(manifest);
        assert_eq!(
            calendar.released("1.18_experimental-snapshot-1"),
            None,
            "实验版不能借用正式版的时间"
        );
    }

    /// 缺 `releaseTime` 的条目被跳过，不影响其余条目
    #[test]
    fn missing_release_time_is_skipped() {
        let manifest = VersionManifest {
            latest: crate::mojang::manifest::LatestVersions {
                release: "1.21.1".to_owned(),
                snapshot: "24w33a".to_owned(),
            },
            versions: vec![
                crate::mojang::manifest::VersionManifestItem {
                    id: "1.21.1".to_owned(),
                    kind: crate::mojang::manifest::VersionKind::Release,
                    released: None,
                    url: "https://example.invalid/a.json".to_owned(),
                },
                crate::mojang::manifest::VersionManifestItem {
                    id: "24w33a".to_owned(),
                    kind: crate::mojang::manifest::VersionKind::Snapshot,
                    released: Some("2024-08-15T00:00:00+00:00".to_owned()),
                    url: "https://example.invalid/b.json".to_owned(),
                },
            ],
        };
        let calendar = ReleaseCalendar::from_manifest(manifest);
        assert_eq!(calendar.released("1.21.1"), None);
        assert_eq!(
            calendar.released("24w33a"),
            Some("2024-08-15T00:00:00+00:00")
        );
    }

    /// 空日历：降级路径的判定
    #[test]
    fn default_calendar_is_empty_and_knows_nothing() {
        let calendar = ReleaseCalendar::default();
        assert_eq!(calendar.released("1.21.1"), None);
        assert_eq!(calendar.released("1.21.1_unobfuscated"), None);
    }
}
