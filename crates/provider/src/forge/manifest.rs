//! Forge 官方 Maven 元数据模型
//!
//! 只声明本项目用到的字段；上游新增字段不影响解析。端点清单（均已实测可达）：
//!
//! - `GET {files}/net/minecraftforge/forge/maven-metadata.json`
//!   —— MC 版本 → build 列表，**主选**。key 是纯 MC 版本号（如 `1.21.1`），
//!   value 是该 MC 下的完整 `MC-Forge` 串数组。
//! - `GET {files}/net/minecraftforge/forge/promotions_slim.json`
//!   —— 官方推荐的 `latest` / `recommended` 版本，用于挑选默认 build。
//!
//! 两处都由 `files.minecraftforge.net` 提供；镜像只需替换根地址。
//!
//! # 顺序陷阱
//!
//! `maven-metadata.json` 的数组按「旧版本在前」排列，**末项才是最新 build**，
//! 与 Fabric Meta 的「新版本在前」相反。任何选取逻辑都必须显式取末项，不能
//! 假设首项最新。

use serde::Deserialize;
use std::collections::BTreeMap;

/// `maven-metadata.json` 的结构：MC 版本 → 该版本下的 build 串列表
///
/// 例如 `{"1.21.1": ["1.21.1-52.0.0", ..., "1.21.1-52.1.20"]}`。
/// `BTreeMap` 保证遍历顺序稳定（版本号字典序），便于测试与展示。
pub(crate) type MavenMetadata = BTreeMap<String, Vec<String>>;

/// `promotions_slim.json` 的响应
#[derive(Debug, Deserialize)]
pub(crate) struct Promotions {
    /// `promos` 表：key 形如 `1.21.1-latest` / `1.21.1-recommended`
    #[serde(default)]
    pub(crate) promos: BTreeMap<String, String>,
}

impl Promotions {
    /// 某 MC 版本官方推荐的**完整 build 串**（`{mc}-{forge}`）
    ///
    /// 官方文档把 `recommended` 视为生产可用版本，`latest` 是尝鲜版；
    /// 两者都没有时返回 `None`，由调用方退回 maven-metadata 的末项。
    ///
    /// 注意：`promos` 里存的是**短版本**（如 `52.1.20`），需要拼上 `{mc}-`
    /// 才能与 maven-metadata 的完整串（`1.21.1-52.1.20`）对上。
    #[must_use]
    pub(crate) fn preferred(&self, mc: &str) -> Option<String> {
        self.promos
            .get(&format!("{mc}-recommended"))
            .or_else(|| self.promos.get(&format!("{mc}-latest")))
            .map(|forge| format!("{mc}-{forge}"))
    }
}

/// 一个 Forge build 的三个坐标分量
///
/// Forge 的版本串统一为 `{mc}-{forge}`（如 `1.21.1-52.0.1`），据此拼出
/// Maven 路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForgeBuild<'a> {
    /// 完整串，如 `1.21.1-52.0.1`
    pub(crate) full: &'a str,
}

impl ForgeBuild<'_> {
    /// 是否为合法 build 串（会被拼进 URL 与文件名，必须严查）
    ///
    /// 形如 `{mc}-{forge}`，两段都只允许 ASCII 数字、字母与 `.` `_` `+`。
    /// 这段字符串最终进入 `maven_dir()` / `installer_file()` 拼 URL 与本地路径，
    /// 只挡 `-` 是不够的：`1.21.1-../../evil` 能一路穿到 URL 与文件系统。
    #[must_use]
    pub(crate) fn is_valid(&self) -> bool {
        let Some((mc, forge)) = self.full.split_once('-') else {
            return false;
        };
        !mc.is_empty()
            && !forge.is_empty()
            && mc.chars().all(is_safe_build_char)
            && forge.chars().all(is_safe_build_char)
    }

    /// Maven 相对路径：`net/minecraftforge/forge/{full}`
    #[must_use]
    pub(crate) fn maven_dir(&self) -> String {
        format!("net/minecraftforge/forge/{}", self.full)
    }

    /// installer JAR 文件名：`forge-{full}-installer.jar`
    #[must_use]
    pub(crate) fn installer_file(&self) -> String {
        format!("forge-{}-installer.jar", self.full)
    }
}

/// build 串里允许出现的字符
///
/// 覆盖真实版本号用到的字符（`1.21.1`、`52.0.1`、`1.7.10-10.13.4.1614`），
/// 同时把 `/` `\` `:` 与空白挡在外面。
fn is_safe_build_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn promos(pairs: &[(&str, &str)]) -> Promotions {
        Promotions {
            promos: pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn promotions_prefers_recommended_over_latest() {
        let p = promos(&[
            ("1.21.1-latest", "52.1.20"),
            ("1.21.1-recommended", "52.0.1"),
        ]);
        assert_eq!(p.preferred("1.21.1").as_deref(), Some("1.21.1-52.0.1"));
    }

    #[test]
    fn promotions_falls_back_to_latest() {
        let p = promos(&[("1.21.1-latest", "52.1.20")]);
        assert_eq!(p.preferred("1.21.1").as_deref(), Some("1.21.1-52.1.20"));
    }

    #[test]
    fn promotions_missing_returns_none() {
        let p = promos(&[]);
        assert_eq!(p.preferred("1.21.1"), None);
    }

    #[test]
    fn build_validity_requires_two_parts() {
        assert!(
            ForgeBuild {
                full: "1.21.1-52.0.1"
            }
            .is_valid()
        );
        assert!(!ForgeBuild { full: "1.21.1" }.is_valid());
        assert!(!ForgeBuild { full: "-52.0.1" }.is_valid());
        assert!(!ForgeBuild { full: "1.21.1-" }.is_valid());
    }

    /// build 串会被拼进 URL 与文件名：路径分隔符与空白一律非法
    #[test]
    fn build_validity_rejects_path_injection() {
        for bad in [
            "1.21.1-../../evil",
            "1.21.1-52.0.1/../..",
            "../1.21.1-52.0.1",
            "1.21.1-52 0.1",
            "1.21.1-52.0.1\t",
            "1.21.1-52\\0.1",
            "1.21.1:52.0.1",
        ] {
            assert!(!ForgeBuild { full: bad }.is_valid(), "应拒绝 {bad}");
        }
        // 真实存在的各种形态仍然通过
        for good in ["1.21.1-52.0.1", "1.7.10-10.13.4.1614", "1.16.5-36.2.39"] {
            assert!(ForgeBuild { full: good }.is_valid(), "应接受 {good}");
        }
    }

    #[test]
    fn build_maven_paths_are_consistent() {
        let b = ForgeBuild {
            full: "1.21.1-52.0.1",
        };
        assert_eq!(b.maven_dir(), "net/minecraftforge/forge/1.21.1-52.0.1");
        assert_eq!(b.installer_file(), "forge-1.21.1-52.0.1-installer.jar");
    }

    #[test]
    fn maven_metadata_deserializes_mc_keyed_map() {
        let raw = r#"{"1.21.1":["1.21.1-52.0.0","1.21.1-52.1.20"],"1.20.1":["1.20.1-47.0.0"]}"#;
        let meta: MavenMetadata = serde_json::from_str(raw).expect("parse");
        assert_eq!(meta.len(), 2);
        assert_eq!(
            meta.get("1.21.1").map(Vec::as_slice),
            Some(["1.21.1-52.0.0".to_owned(), "1.21.1-52.1.20".to_owned()].as_slice())
        );
    }

    #[test]
    fn promotions_deserialize_ignores_extra_keys() {
        let raw = r#"{"homepage":"x","promos":{"1.21.1-latest":"52.1.20"}}"#;
        let p: Promotions = serde_json::from_str(raw).expect("parse");
        assert_eq!(p.promos.len(), 1);
        assert_eq!(p.preferred("1.21.1").as_deref(), Some("1.21.1-52.1.20"));
    }

    #[test]
    fn empty_promos_is_accepted() {
        let p: Promotions = serde_json::from_str("{}").expect("parse");
        assert!(p.promos.is_empty());
    }

    #[test]
    fn btreemap_keeps_versions_sorted() {
        let meta: MavenMetadata =
            serde_json::from_str(r#"{"1.20.1":[],"1.21.1":[],"1.19.2":[]}"#).expect("parse");
        let keys: Vec<_> = meta.keys().map(String::as_str).collect();
        assert_eq!(keys, ["1.19.2", "1.20.1", "1.21.1"]);
        let _: BTreeMap<String, String> = BTreeMap::new();
    }
}
