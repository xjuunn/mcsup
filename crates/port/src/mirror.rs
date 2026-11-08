//! 下载源的选源与持久化端口
//!
//! 三个能力各管一件事，互不重叠：
//!
//! - [`SourceProbe`]：**测**。给一条地址，回答「多久能拿到首字节」。实现方是
//!   真正会发 HTTP 的那一层（`provider`），本层只定口径。
//! - [`SourceRouteStore`]：**记**。记住每个上游面选中的源，让「第一次探测、
//!   之后一直用」跨进程成立。实现方是持久化层（`storage`）。
//! - 目录（[`domain::mirror::Catalog`]）不设 trait：它是纯数据，构造后不需要
//!   替换实现，组合根直接注入具体值。
//!
//! 选源策略本身不在本模块：判定收敛在 `domain::mirror`（纯函数、可单测），
//! 这里只有「怎么测」「怎么记」两个 IO 边界。

use crate::{RepositoryError, SourceError};
use async_trait::async_trait;
use domain::mirror::SourceId;
use std::time::Duration;

/// 探测一条地址的首字节往返耗时
///
/// 语义是**可用性 + 延迟**，不是吞吐：探测只发一次轻量请求（`HEAD`，服务端
/// 不支持时退化为取一字节），拿到响应头即返回。按吞吐排序需要真下几兆字节，
/// 在「每个上游面只探一次」的场景里不划算。
#[async_trait]
pub trait SourceProbe: Send + Sync {
    /// 探测；不可达或状态码非成功时返回错误
    ///
    /// # Errors
    ///
    /// 传输失败返回 [`SourceError::Network`]，非成功状态码返回
    /// [`SourceError::Status`] / [`SourceError::Upstream`]
    async fn probe(&self, location: &str) -> Result<Duration, SourceError>;
}

/// 选源结果的持久化
///
/// 键是上游面的键（`domain::mirror::Upstream::key`），值是选中的源。缺一条
/// 记录就表示「这个上游面还没探过」，与「探过但选中的是官方」是两种状态：
/// 前者下次要探，后者不该再探。
pub trait SourceRouteStore: Send + Sync {
    /// 读取某个上游面已选中的源
    ///
    /// # Errors
    ///
    /// 状态文件不可读或损坏时返回 [`RepositoryError`]
    fn route(&self, upstream: &str) -> Result<Option<SourceId>, RepositoryError>;

    /// 记住某个上游面选中的源（覆盖旧值）
    ///
    /// # Errors
    ///
    /// 写入失败时返回 [`RepositoryError`]
    fn remember(&self, upstream: &str, source: &SourceId) -> Result<(), RepositoryError>;

    /// 忘记某个上游面的选择
    ///
    /// 下载失败后调用：只清记录不立刻重探，重探发生在下一次真正需要选源时，
    /// 免得失败的下载路径上再叠一次全源探测。
    ///
    /// # Errors
    ///
    /// 写入失败时返回 [`RepositoryError`]
    fn forget(&self, upstream: &str) -> Result<(), RepositoryError>;
}

/// 内存实现：测试与「不持久化」场景
///
/// 每次 run 都当首次，行为仍然正确（只是每次重新探测）。
#[derive(Debug, Default)]
pub struct MemoryRouteStore {
    routes: std::sync::Mutex<std::collections::BTreeMap<String, SourceId>>,
}

impl MemoryRouteStore {
    /// 空存储
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SourceRouteStore for MemoryRouteStore {
    fn route(&self, upstream: &str) -> Result<Option<SourceId>, RepositoryError> {
        let routes = self
            .routes
            .lock()
            .map_err(|_| RepositoryError::Corrupt("route store poisoned".to_owned()))?;
        Ok(routes.get(upstream).cloned())
    }

    fn remember(&self, upstream: &str, source: &SourceId) -> Result<(), RepositoryError> {
        let mut routes = self
            .routes
            .lock()
            .map_err(|_| RepositoryError::Corrupt("route store poisoned".to_owned()))?;
        routes.insert(upstream.to_owned(), source.clone());
        Ok(())
    }

    fn forget(&self, upstream: &str) -> Result<(), RepositoryError> {
        let mut routes = self
            .routes
            .lock()
            .map_err(|_| RepositoryError::Corrupt("route store poisoned".to_owned()))?;
        routes.remove(upstream);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trip() {
        let store = MemoryRouteStore::new();
        assert!(store.route("forge-maven").expect("read").is_none());
        store
            .remember("forge-maven", &SourceId::new("bmclapi").expect("id"))
            .expect("write");
        assert_eq!(
            store.route("forge-maven").expect("read"),
            Some(SourceId::new("bmclapi").expect("id"))
        );
        store.forget("forge-maven").expect("forget");
        assert!(store.route("forge-maven").expect("read").is_none());
    }
}
