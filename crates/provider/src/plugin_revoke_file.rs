//! 撤销列表缓存：`plugins/revoked.json`
//!
//! 文件权限按最小权限检查（`0600`），写入走临时文件 + `rename`。判定规则
//! 在 `domain::plugin::revocation`，本类型只满足 `port::plugin::PluginRevocations`。

use domain::plugin::revocation::RevocationList;
use port::plugin::{HostError, PluginRevocations};
use std::path::{Path, PathBuf};

/// 基于文件的撤销列表
#[derive(Debug, Clone)]
pub struct FileRevocations {
    path: PathBuf,
}

impl FileRevocations {
    /// 指定缓存路径
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 缓存路径
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl PluginRevocations for FileRevocations {
    fn load(&self) -> Result<RevocationList, HostError> {
        Ok(crate::plugin_revoke::load_revocation_list(&self.path)?.unwrap_or_default())
    }

    fn store(&self, list: &RevocationList) -> Result<(), HostError> {
        crate::plugin_revoke::store_revocation_list(&self.path, list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_loads_as_empty_list() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = FileRevocations::new(dir.path().join("revoked.json"));
        let list = store.load().expect("load");
        assert!(list.is_empty());
    }

    #[test]
    fn store_is_visible_to_load() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = FileRevocations::new(dir.path().join("revoked.json"));
        let list = RevocationList::from_json(
            br#"{"schema":1,"entries":[{"id":"demo","versions":["0.1.0"]}]}"#,
        )
        .expect("parse");
        store.store(&list).expect("store");
        assert_eq!(store.load().expect("load"), list);
    }
}
