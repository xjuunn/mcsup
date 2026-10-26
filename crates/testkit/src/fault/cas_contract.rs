//! `ContentStore` 契约：「任何 CAS 实现都必须满足」的可复用断言组
//!
//! 契约测试的价值在于**它不随实现走**。实现层换了（文件系统 CAS 换成分块
//! 存储、将来换成远端对象存储），契约不动，实现方必须自己满足；反过来，
//! 如果有人改了实现又把测试跟着改，契约测试会立刻发现语义已经漂移。
//!
//! 调用方式（实现 crate 的 `tests/` 里换成自己的实现，这里用 [`InMemoryCas`]）：
//!
//! ```
//! use std::sync::Arc;
//! use testkit::{cas_contract, InMemoryCas};
//!
//! cas_contract::run(|_root| Arc::new(InMemoryCas::default()));
//! ```
//!
//! 每一条契约都对应 `port::backup::ContentStore` 文档里的一句话。

use domain::backup::{ContentHash, RelPath};
use port::{CasError, ContentStore};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::assert::{Failure, assert_invariant};

/// 内存 CAS：`testkit` 自测与故障注入的底座
///
/// 不是产品实现，但必须满足同一份契约——否则故障注入包装器就没有可靠的
/// 对照组，注入测试的结论也就不可信。
#[derive(Debug, Default)]
pub struct InMemoryCas {
    objects: Mutex<BTreeMap<ContentHash, Vec<u8>>>,
}

impl InMemoryCas {
    fn with<T>(&self, f: impl FnOnce(&mut BTreeMap<ContentHash, Vec<u8>>) -> T) -> T {
        let mut objects = self.objects.lock().expect("lock");
        f(&mut objects)
    }
}

impl ContentStore for InMemoryCas {
    fn put(
        &self,
        reader: &mut dyn std::io::Read,
        _hint: &RelPath,
    ) -> Result<(ContentHash, u64), CasError> {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|err| CasError::io(Path::new("<memory>"), err))?;
        let hash = content_key(&bytes);
        let size = bytes.len() as u64;
        self.with(|objects| {
            objects.entry(hash.clone()).or_insert(bytes);
        });
        Ok((hash, size))
    }

    fn get(&self, hash: &ContentHash, dest: &Path) -> Result<u64, CasError> {
        let bytes = self
            .with(|objects| objects.get(hash).cloned())
            .ok_or_else(|| CasError::Missing(hash.clone()))?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|err| CasError::io(parent, err))?;
        }
        std::fs::write(dest, &bytes).map_err(|err| CasError::io(dest, err))?;
        Ok(bytes.len() as u64)
    }

    fn stat(&self, hash: &ContentHash) -> Result<Option<u64>, CasError> {
        Ok(self.with(|objects| objects.get(hash).map(|bytes| bytes.len() as u64)))
    }

    fn verify(
        &self,
        hashes: &[ContentHash],
        on_progress: &mut dyn FnMut(usize, usize),
    ) -> Result<Vec<ContentHash>, CasError> {
        let total = hashes.len();
        let mut broken = Vec::new();
        for (index, hash) in hashes.iter().enumerate() {
            let ok = self
                .with(|objects| objects.get(hash).cloned())
                .is_some_and(|bytes| content_key(&bytes) == *hash);
            if !ok {
                broken.push(hash.clone());
            }
            on_progress(index + 1, total);
        }
        Ok(broken)
    }

    fn list(&self) -> Result<Vec<ContentHash>, CasError> {
        Ok(self.with(|objects| objects.keys().cloned().collect()))
    }

    fn remove(&self, hash: &ContentHash) -> Result<(), CasError> {
        self.with(|objects| {
            objects.remove(hash);
        });
        Ok(())
    }
}

/// 内容 → 键：用 FNV-1a 展开到 64 个十六进制字符
///
/// 契约只要求「同内容同键、异内容异键、键格式合法」，不要求与生产用的
/// SHA-256 一致；不引 sha2 是为了让测试底座无额外依赖。
fn content_key(bytes: &[u8]) -> ContentHash {
    let mut state: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let hex = format!("{state:016x}").repeat(4);
    ContentHash::parse(&hex).expect("64 个十六进制字符")
}

fn rel(path: &str) -> RelPath {
    RelPath::parse(path).expect("相对路径")
}

fn put_bytes(store: &dyn ContentStore, bytes: &[u8], hint: &str) -> (ContentHash, u64) {
    store
        .put(&mut Cursor::new(bytes.to_vec()), &rel(hint))
        .expect("put 必须成功")
}

/// 跑完整契约：`factory` 在给定根目录上构造一个空存储
///
/// 每一条独立成函数，实现方也可以只跑关心的那几条。
pub fn run(factory: impl Fn(&Path) -> Arc<dyn ContentStore>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = factory(dir.path());
    let root = dir.path();

    put_get_round_trip(store.as_ref(), root);
    write_is_idempotent(store.as_ref());
    dedup_by_content(store.as_ref());
    empty_object_is_valid(store.as_ref());
    missing_object_is_reported(store.as_ref(), root);
    remove_is_idempotent(store.as_ref());
    verify_detects_missing(store.as_ref(), root);
    verify_accepts_intact(store.as_ref());
    list_reflects_written_objects(store.as_ref());
}

/// 契约：写入的字节必须能原样取回，大小一致
pub fn put_get_round_trip(store: &dyn ContentStore, root: &Path) {
    let payload = b"hello world".as_slice();
    let (hash, size) = put_bytes(store, payload, "world/level.dat");
    let dest = root.join("out/level.dat");
    let restored = store.get(&hash, &dest).expect("get");
    assert_invariant(
        restored == size && std::fs::read(&dest).expect("read") == payload,
        || {
            Failure::invariant("put 的字节必须能原样 get 回来")
                .expected(format!("{size} B"))
                .actual(format!("{restored} B"))
                .tree(root)
        },
    );
}

/// 契约：同一内容重复写入不报错，且得到同一哈希
pub fn write_is_idempotent(store: &dyn ContentStore) {
    let first = put_bytes(store, b"same bytes", "a");
    let second = put_bytes(store, b"same bytes", "b");
    assert_invariant(first == second, || {
        Failure::invariant("同内容两次 put 必须得到同一哈希")
            .expected(first.0.to_string())
            .actual(second.0.to_string())
    });
}

/// 契约：键只由内容决定——相同内容共用一个键，不同内容不能共用
pub fn dedup_by_content(store: &dyn ContentStore) {
    let a = put_bytes(store, b"dup", "a");
    let b = put_bytes(store, b"dup", "b");
    let c = put_bytes(store, b"other", "c");
    assert_invariant(a.0 == b.0 && a.0 != c.0, || {
        Failure::invariant("去重键必须只由内容决定")
            .expected("a == b 且 a != c")
            .actual(format!("a={} b={} c={}", a.0, b.0, c.0))
    });
}

/// 契约：空文件是合法对象（存档里有大量空文件）
pub fn empty_object_is_valid(store: &dyn ContentStore) {
    let (hash, size) = put_bytes(store, b"", "empty");
    assert_invariant(
        size == 0 && store.stat(&hash).expect("stat") == Some(0),
        || Failure::invariant("空文件必须是合法 CAS 对象").actual(size),
    );
}

/// 契约：不存在的对象必须报 `Missing`，不能报成功
pub fn missing_object_is_reported(store: &dyn ContentStore, root: &Path) {
    let hash = ContentHash::parse(&"0".repeat(64)).expect("hash");
    let err = store.get(&hash, &root.join("nope")).expect_err("must fail");
    assert_invariant(matches!(err, CasError::Missing(_)), || {
        Failure::invariant("缺失对象必须报 CasError::Missing")
            .expected("CasError::Missing")
            .actual(err.to_string())
            .tree(root)
    });
}

/// 契约：删除已不存在的对象视为成功
pub fn remove_is_idempotent(store: &dyn ContentStore) {
    let (hash, _) = put_bytes(store, b"x", "a");
    store.remove(&hash).expect("first remove");
    store.remove(&hash).expect("second remove");
    assert_invariant(store.stat(&hash).expect("stat").is_none(), || {
        Failure::invariant("remove 之后 stat 必须为 None")
    });
}

/// 契约：缺失的对象必须出现在 `verify` 的返回值里
///
/// 这一条决定「备份可验证」是否成立：漏报意味着 `verify` 只是安慰剂。
pub fn verify_detects_missing(store: &dyn ContentStore, root: &Path) {
    let (hash, _) = put_bytes(store, b"payload", "a");
    let (other, _) = put_bytes(store, b"payload2", "b");
    store.remove(&hash).expect("remove one");
    let mut steps = 0;
    let broken = store
        .verify(&[hash.clone(), other.clone()], &mut |done, total| {
            steps += 1;
            assert_invariant(done <= total && total == 2, || {
                Failure::invariant("进度回调必须给出 已做/总数")
            });
        })
        .expect("verify");
    assert_invariant(
        steps == 2 && broken.contains(&hash) && !broken.contains(&other),
        || {
            Failure::invariant("verify 必须只报出缺失或损坏的对象")
                .expected("恰好 [missing]")
                .actual(format!("{broken:?}"))
                .tree(root)
        },
    );
}

/// 契约：完好对象不报损坏
pub fn verify_accepts_intact(store: &dyn ContentStore) {
    let (hash, _) = put_bytes(store, b"ok", "a");
    let broken = store.verify(&[hash], &mut |_, _| {}).expect("verify");
    assert_invariant(broken.is_empty(), || {
        Failure::invariant("完好对象不得被 verify 报成损坏").actual(format!("{broken:?}"))
    });
}

/// 契约：`list` 与写入的对象集合一致
pub fn list_reflects_written_objects(store: &dyn ContentStore) {
    let (a, _) = put_bytes(store, b"list-a", "a");
    let (b, _) = put_bytes(store, b"list-b", "b");
    let listed = store.list().expect("list");
    assert_invariant(listed.contains(&a) && listed.contains(&b), || {
        Failure::invariant("list 必须包含写入的对象")
            .expected(format!("{a} 与 {b} 都在 list 里"))
            .actual(format!("{listed:?}"))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内存实现必须满足契约，否则它不能用作故障注入的对照组
    #[test]
    fn in_memory_cas_satisfies_contract() {
        run(|_root| Arc::new(InMemoryCas::default()));
    }

    /// 契约是有效的：一个「哈希与内容无关」的坏实现必须被捕获
    ///
    /// 没有这条自检，契约可能整体失效却一路绿灯——那比没有契约更危险。
    #[test]
    fn collapsing_store_fails_the_contract() {
        #[derive(Debug)]
        struct CollapsingCas;

        impl ContentStore for CollapsingCas {
            fn put(
                &self,
                _reader: &mut dyn std::io::Read,
                _hint: &RelPath,
            ) -> Result<(ContentHash, u64), CasError> {
                // 忽略内容，永远返回同一个哈希
                Ok((ContentHash::parse(&"1".repeat(64)).expect("hash"), 0))
            }
            fn get(&self, hash: &ContentHash, _dest: &Path) -> Result<u64, CasError> {
                Err(CasError::Missing(hash.clone()))
            }
            fn stat(&self, _hash: &ContentHash) -> Result<Option<u64>, CasError> {
                Ok(None)
            }
            fn verify(
                &self,
                _hashes: &[ContentHash],
                _on_progress: &mut dyn FnMut(usize, usize),
            ) -> Result<Vec<ContentHash>, CasError> {
                Ok(Vec::new())
            }
            fn list(&self) -> Result<Vec<ContentHash>, CasError> {
                Ok(Vec::new())
            }
            fn remove(&self, _hash: &ContentHash) -> Result<(), CasError> {
                Ok(())
            }
        }

        let outcome = std::panic::catch_unwind(|| {
            run(|_root| Arc::new(CollapsingCas));
        });
        assert_invariant(outcome.is_err(), || {
            Failure::invariant("坏实现必须让契约失败；否则契约本身失效")
                .expected("panic")
                .actual("契约通过了")
        });
    }
}
