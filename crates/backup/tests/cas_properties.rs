//! 文件系统 CAS 的属性测试：任意字节、任意写入顺序都必须保持语义
//!
//! 契约测试（`cas_contract.rs`）钉住的是「列出的每一条都成立」，属性测试补的
//! 是「任意输入下都成立」——尤其是「同内容只存一份」这种在随机数据下才容易
//! 暴露的问题（碰撞、截断、缓冲边界）。
//!
//! 失败反例会写进 `proptest-regressions/`：那个文件是**测试资产**，必须随
//! 修复一起提交（门禁 G3）。
//!
//! 每个 case 都要真实落盘（CAS 写入带 `sync_all`），单次 put 约 4ms。默认
//! 256 个 case 下本文件光同步写就二十秒，而它断言的是**去重与哈希语义**，
//! 与「落盘多少次」无关。32 个 case 已足够覆盖这些性质：真正需要大样本的是
//! 输入空间巨大的解析类判定（见 `domain/tests/backup_properties.rs`，那里保持
//! 默认值），这里的输入空间是「几次写同样/不同的字节」。

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
    // 见文件头：本文件每个 case 都真实落盘，压小样本量换运行时间
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

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

#[cfg(test)]
mod strategy_self_check {
    use super::*;

    /// 属性用的字节策略确实会产出跨缓冲区的大样本
    ///
    /// 只查大端：`0..4096` 的均匀采样几乎不会产出空内容，用「采到过空」当
    /// 判据会变成偶发失败。空内容由下面那条单独的用例直接钉住。
    ///
    /// 没有这条自检，策略若被改小，上面的属性就会在小数据上空转通过——
    /// 哈希与去重恰恰在跨缓冲区边界时才容易出错。
    #[test]
    fn byte_strategy_reaches_across_buffer_boundaries() {
        let mut runner = proptest::test_runner::TestRunner::deterministic();
        // 闭包是 `Fn`，用 `Cell` 累计命中情况
        let saw_large = std::cell::Cell::new(false);
        runner
            .run(&proptest::collection::vec(any::<u8>(), 0..4096), |bytes| {
                saw_large.set(saw_large.get() || bytes.len() > 2048);
                Ok(())
            })
            .expect("策略必须有效");
        assert!(saw_large.get(), "策略没有覆盖跨缓冲区的较大输入");
    }

    /// 空内容是独立边界：采样覆盖不到，必须直接测
    ///
    /// 空文件是真实存在的（新工作区的空 `data/`、被截断的配置），且是哈希与
    /// 去重最容易出错的一端——它没有分块、大小字段为 0。
    #[test]
    fn empty_content_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cas = CasStore::new(dir.path());
        let (hash, size) = put(&cas, b"");
        assert_eq!(size, 0, "空内容大小必须是 0");
        let dest = dir.path().join("empty.out");
        cas.get(&hash, &dest).expect("get");
        assert_eq!(std::fs::read(&dest).expect("read"), Vec::<u8>::new());
    }
}
