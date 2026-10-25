//! 文件系统 CAS 的属性测试：任意字节、任意写入顺序都必须保持语义
//!
//! 契约测试（`cas_contract.rs`）钉住的是「列出的每一条都成立」，属性测试补的
//! 是「任意输入下都成立」——尤其是「同内容只存一份」这种在随机数据下才容易
//! 暴露的问题（碰撞、截断、缓冲边界）。
//!
//! 失败反例会写进 `proptest-regressions/`：那个文件是**测试资产**，必须随
//! 修复一起提交（门禁 G3）。

#![allow(clippy::expect_used)]

use backup::CasStore;
use domain::backup::RelPath;
use port::ContentStore;
use proptest::prelude::*;

fn rel() -> RelPath {
    RelPath::parse("probe.bin").expect("rel")
}

fn put(cas: &CasStore, bytes: &[u8]) -> (domain::backup::ContentHash, u64) {
    let mut cursor = std::io::Cursor::new(bytes.to_vec());
    cas.put(&mut cursor, &rel()).expect("put")
}

proptest! {
    /// 任意字节往返：取回来的内容与大小都与写入一致
    #[test]
    fn arbitrary_bytes_round_trip(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cas = CasStore::new(dir.path());
        let (hash, size) = put(&cas, &bytes);
        prop_assert_eq!(size, bytes.len() as u64);

        let dest = dir.path().join("out.bin");
        let restored = cas.get(&hash, &dest).expect("get");
        prop_assert_eq!(restored, size);
        prop_assert_eq!(std::fs::read(&dest).expect("read"), bytes);
    }

    /// 相同内容写 N 次只留一个对象；不同内容各自占一个
    #[test]
    fn duplicate_writes_collapse(
        copies in 1usize..5,
        distinct in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 1..64), 1..5),
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cas = CasStore::new(dir.path());

        // 去重后应有的对象数 = 不同内容的数量（内容可能偶然重复，按集合算）
        let mut expected: std::collections::BTreeSet<domain::backup::ContentHash> =
            std::collections::BTreeSet::new();
        for bytes in &distinct {
            expected.insert(put(&cas, bytes).0);
        }
        for _ in 0..copies {
            for bytes in &distinct {
                prop_assert_eq!(put(&cas, bytes).0, put(&cas, bytes).0);
            }
        }
        let listed = cas.list().expect("list");
        prop_assert_eq!(listed.len(), expected.len());
    }

    /// 哈希是内容的函数且只由内容决定：同一字节序列两次写入得到同一键
    #[test]
    fn hash_depends_only_on_content(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cas = CasStore::new(dir.path());
        let first = put(&cas, &bytes).0;
        let second = put(&cas, &bytes).0;
        prop_assert_eq!(first.as_str(), second.as_str());
        prop_assert_eq!(first.as_str().len(), domain::backup::ContentHash::HEX_LEN);
    }
}
