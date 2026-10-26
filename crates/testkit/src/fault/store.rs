//! 装饰任意 [`ContentStore`] 并注入失败
//!
//! 用法是「包住真实的 CAS」，而不是替换它（示例用 [`InMemoryCas`](crate::InMemoryCas)
//! 代替真实实现）：
//!
//! ```
//! use std::sync::Arc;
//! use testkit::{CasOp, FailingContentStore, InMemoryCas};
//!
//! let store = FailingContentStore::new(Arc::new(InMemoryCas::default()))
//!     .failing_on(CasOp::Put, 1);
//! ```
//!
//! 失败注入跑在真实实现之上——被验证的是「实现出错时上层怎么收场」，
//! 而不是「桩自己怎么出错」。

use domain::backup::{ContentHash, RelPath};
use port::{CasError, ContentStore};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// 被注入故障的操作
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CasOp {
    /// 写入对象
    Put,
    /// 读取对象
    Get,
    /// 查询对象
    Stat,
    /// 校验对象
    Verify,
    /// 列举对象
    List,
    /// 删除对象
    Remove,
}

#[derive(Debug, Default)]
struct Inner {
    /// 每类操作已发生的调用次数
    attempts: HashMap<CasOp, usize>,
    /// 每类操作的注入点（第几次调用失败）
    fail_on: HashMap<CasOp, usize>,
}

/// 可注入失败的 `ContentStore` 装饰器
#[derive(Clone)]
pub struct FailingContentStore {
    inner: Arc<dyn ContentStore>,
    state: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for FailingContentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FailingContentStore")
            .finish_non_exhaustive()
    }
}

impl FailingContentStore {
    /// 包住一个真实存储
    #[must_use]
    pub fn new(inner: Arc<dyn ContentStore>) -> Self {
        Self {
            inner,
            state: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// 让 `op` 的第 `attempt` 次调用（1 基）失败
    #[must_use]
    pub fn failing_on(self, op: CasOp, attempt: usize) -> Self {
        self.state.lock().expect("lock").fail_on.insert(op, attempt);
        self
    }

    /// `op` 已被调用的次数
    #[must_use]
    pub fn attempts(&self, op: CasOp) -> usize {
        self.state
            .lock()
            .expect("lock")
            .attempts
            .get(&op)
            .copied()
            .unwrap_or(0)
    }

    /// 记录一次调用；命中注入点则返回错误
    fn tick(&self, op: CasOp) -> Result<(), CasError> {
        let mut state = self.state.lock().expect("lock");
        let count = {
            let counter = state.attempts.entry(op).or_insert(0);
            *counter += 1;
            *counter
        };
        let fail_on = state.fail_on.get(&op).copied();
        drop(state);
        if fail_on == Some(count) {
            return Err(CasError::io(
                Path::new("<injected>"),
                std::io::Error::other(format!("injected {op:?} failure #{count}")),
            ));
        }
        Ok(())
    }
}

impl ContentStore for FailingContentStore {
    fn put(
        &self,
        reader: &mut dyn std::io::Read,
        hint: &RelPath,
    ) -> Result<(ContentHash, u64), CasError> {
        self.tick(CasOp::Put)?;
        self.inner.put(reader, hint)
    }

    fn get(&self, hash: &ContentHash, dest: &Path) -> Result<u64, CasError> {
        self.tick(CasOp::Get)?;
        self.inner.get(hash, dest)
    }

    fn stat(&self, hash: &ContentHash) -> Result<Option<u64>, CasError> {
        self.tick(CasOp::Stat)?;
        self.inner.stat(hash)
    }

    fn verify(
        &self,
        hashes: &[ContentHash],
        on_progress: &mut dyn FnMut(usize, usize),
    ) -> Result<Vec<ContentHash>, CasError> {
        self.tick(CasOp::Verify)?;
        self.inner.verify(hashes, on_progress)
    }

    fn list(&self) -> Result<Vec<ContentHash>, CasError> {
        self.tick(CasOp::List)?;
        self.inner.list()
    }

    fn remove(&self, hash: &ContentHash) -> Result<(), CasError> {
        self.tick(CasOp::Remove)?;
        self.inner.remove(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assert::{Failure, assert_invariant};
    use crate::fault::cas_contract::InMemoryCas;

    fn rel() -> RelPath {
        RelPath::parse("a").expect("rel")
    }

    #[test]
    fn injection_hits_only_named_attempt() {
        let store =
            FailingContentStore::new(Arc::new(InMemoryCas::default())).failing_on(CasOp::Put, 2);
        store.put(&mut &b"one"[..], &rel()).expect("first ok");
        assert_invariant(store.put(&mut &b"two"[..], &rel()).is_err(), || {
            Failure::invariant("第 2 次 put 必须失败")
        });
        store.put(&mut &b"three"[..], &rel()).expect("third ok");
        assert_invariant(store.attempts(CasOp::Put) == 3, || {
            Failure::invariant("调用计数必须包含失败的那次").actual(store.attempts(CasOp::Put))
        });
    }

    #[test]
    fn non_injected_ops_pass_through() {
        let store = FailingContentStore::new(Arc::new(InMemoryCas::default()));
        let (hash, size) = store.put(&mut &b"payload"[..], &rel()).expect("put");
        assert_invariant(store.stat(&hash).expect("stat") == Some(size), || {
            Failure::invariant("未注入的操作必须直通")
        });
    }
}
