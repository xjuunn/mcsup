//! 撤销列表的判定规则
//!
//! 列表来自外部（市场 / 上游）发布的一份 JSON，实现层负责缓存与权限检查，
//! 判定只看「这条记录命不命中当前版本」。
//!
//! 设计取舍：
//!
//! - **按 id 精确匹配，不按前缀**：`demo` 的撤销不能牵连 `demo-extra`。
//! - **`versions` 为空 = 全部版本**：作者下架整个插件比逐个列版本更常见，
//!   空数组不能被读成「没有命中」。
//! - **非法条目整体拒绝**：撤销列表是安全设施，不能因为一条坏数据就把整份
//!   列表当作空表放行。宁可报错并保留上一次的缓存。
//! - **列表不可用不阻断已安装插件**：解析失败只记录、继续运行，见
//!   `docs/plugins.md` §16.4。
//!
//! 这里不读文件、不做权限检查：那些归实现层与 `port::plugin::PluginRevocations`。

use serde::{Deserialize, Serialize};

/// 列表结构版本
pub const REVOCATION_SCHEMA: u32 = 1;

/// 单个列表文件上限
pub const MAX_REVOCATION_BYTES: u64 = 4 * 1024 * 1024;

/// 一条撤销记录
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevocationEntry {
    /// 插件 id
    pub id: String,
    /// 受影响版本；空表示全部版本
    #[serde(default)]
    pub versions: Vec<String>,
    /// 原因，用于提示
    #[serde(default)]
    pub reason: Option<String>,
}

/// 撤销列表
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevocationList {
    /// 结构版本，当前固定 1
    pub schema: u32,
    /// 更新时间（Unix 秒），仅用于展示
    #[serde(default)]
    pub updated_at: i64,
    /// 撤销记录
    #[serde(default)]
    pub entries: Vec<RevocationEntry>,
}

impl RevocationList {
    /// 解析 JSON
    ///
    /// # Errors
    ///
    /// 结构不符、`schema` 不是 [`REVOCATION_SCHEMA`]、条目字段不合法时返回
    /// 说明文本
    pub fn from_json(bytes: &[u8]) -> Result<Self, RevocationListError> {
        let list: Self = serde_json::from_slice(bytes)
            .map_err(|err| RevocationListError(format!("invalid revocation list: {err}")))?;
        if list.schema != REVOCATION_SCHEMA {
            return Err(RevocationListError(format!(
                "unsupported revocation list schema {}",
                list.schema
            )));
        }
        for entry in &list.entries {
            entry.validate()?;
        }
        Ok(list)
    }

    /// 序列化为 JSON
    ///
    /// # Errors
    ///
    /// 序列化失败时返回说明文本（结构本身总是可序列化，仅防意外）
    pub fn to_json(&self) -> Result<Vec<u8>, RevocationListError> {
        serde_json::to_vec_pretty(self)
            .map_err(|err| RevocationListError(format!("serialize revocation list: {err}")))
    }

    /// 插件版本是否被撤销
    ///
    /// 返回命中的原因，未命中返回 `None`。
    #[must_use]
    pub fn hit(&self, id: &str, version: &str) -> Option<&RevocationEntry> {
        self.entries.iter().find(|entry| {
            entry.id == id
                && (entry.versions.is_empty() || entry.versions.iter().any(|v| v == version))
        })
    }

    /// 是否为空列表
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl RevocationEntry {
    fn validate(&self) -> Result<(), RevocationListError> {
        if self.id.trim().is_empty() {
            return Err(RevocationListError(
                "revocation entry has an empty plugin id".to_owned(),
            ));
        }
        for version in &self.versions {
            if version.trim().is_empty() {
                return Err(RevocationListError(format!(
                    "revocation entry for {} has an empty version",
                    self.id
                )));
            }
        }
        Ok(())
    }
}

/// 撤销列表解析失败
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RevocationListError(pub String);

/// 拒绝启动时的提示文本
#[must_use]
pub fn revoked_message(entry: &RevocationEntry, version: &str) -> String {
    match &entry.reason {
        Some(reason) => format!(
            "plugin {} {version} is revoked: {reason}; upgrade to a newer version",
            entry.id
        ),
        None => format!(
            "plugin {} {version} is revoked; upgrade to a newer version",
            entry.id
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(json: &str) -> RevocationList {
        RevocationList::from_json(json.as_bytes()).expect("parse")
    }

    #[test]
    fn exact_version_hit() {
        let list = list(r#"{"schema":1,"entries":[{"id":"demo","versions":["0.1.0"]}]}"#);
        assert!(list.hit("demo", "0.1.0").is_some());
        assert!(list.hit("demo", "0.1.1").is_none());
    }

    #[test]
    fn empty_versions_revokes_everything() {
        let list = list(r#"{"schema":1,"entries":[{"id":"demo","versions":[]}]}"#);
        assert!(list.hit("demo", "0.1.0").is_some());
        assert!(list.hit("demo", "9.9.9").is_some());
    }

    #[test]
    fn id_match_is_exact() {
        let list = list(r#"{"schema":1,"entries":[{"id":"demo","versions":[]}]}"#);
        assert!(list.hit("demo-extra", "0.1.0").is_none());
    }

    #[test]
    fn unknown_schema_is_rejected() {
        let err = RevocationList::from_json(br#"{"schema":2,"entries":[]}"#).expect_err("schema");
        assert!(err.to_string().contains("schema"));
    }

    #[test]
    fn empty_id_is_rejected() {
        let err = RevocationList::from_json(br#"{"schema":1,"entries":[{"id":"","versions":[]}]}"#)
            .expect_err("empty id");
        assert!(err.to_string().contains("empty plugin id"));
    }

    #[test]
    fn empty_version_is_rejected() {
        let err =
            RevocationList::from_json(br#"{"schema":1,"entries":[{"id":"demo","versions":[""]}]}"#)
                .expect_err("empty version");
        assert!(err.to_string().contains("empty version"));
    }

    #[test]
    fn round_trip_keeps_entries() {
        let list = list(
            r#"{"schema":1,"updated_at":7,"entries":[{"id":"demo","versions":["1.0.0"],"reason":"malware"}]}"#,
        );
        let bytes = list.to_json().expect("serialize");
        assert_eq!(RevocationList::from_json(&bytes).expect("parse"), list);
    }

    #[test]
    fn reason_appears_in_message() {
        let entry = RevocationEntry {
            id: "demo".to_owned(),
            versions: Vec::new(),
            reason: Some("malware".to_owned()),
        };
        let message = revoked_message(&entry, "0.1.0");
        assert!(message.contains("malware"));
        assert!(message.contains("0.1.0"));
    }
}
