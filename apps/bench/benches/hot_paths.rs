//! 高成本路径的基准：CAS 写入 / 读取、目录扫描、快照元数据序列化
//!
//! 对应 `docs/testing.md` 的性能测试一节：这些模块必须先有数字，任何「性能
//! 优化」才有依据。跑法：`cargo bench -p bench`。

// 基准夹具构造失败必须立刻可见（同 `bench` lib 的豁免说明）。
#![allow(clippy::expect_used, reason = "基准样本构造失败必须立刻中止")]

use std::io::Cursor;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mcsup::backup::CasStore;
use mcsup::domain::backup::RelPath;
use mcsup::port::{ContentStore, Scanner};

/// CAS 写入：内容寻址的哈希 + 落盘
fn bench_cas_put(c: &mut Criterion) {
    let mut group = c.benchmark_group("cas_put");
    for size in [64 * 1024usize, 1024 * 1024] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = CasStore::new(dir.path());
            let payload = vec![0x5au8; size];
            let hint = RelPath::parse("world/region/r.0.0.mca").expect("rel");
            b.iter(|| {
                let mut cursor = Cursor::new(payload.as_slice());
                store.put(&mut cursor, &hint).expect("put");
            });
        });
    }
    group.finish();
}

/// CAS 读取：把对象写回目标路径（恢复的热路径）
fn bench_cas_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("cas_get");
    let size = 1024 * 1024usize;
    group.throughput(Throughput::Bytes(size as u64));
    let dir = tempfile::tempdir().expect("tempdir");
    let store = CasStore::new(dir.path());
    let payload = vec![0x5au8; size];
    let hint = RelPath::parse("world/region/r.0.0.mca").expect("rel");
    let (hash, _) = store
        .put(&mut Cursor::new(payload.as_slice()), &hint)
        .expect("put");
    let out_dir = tempfile::tempdir().expect("out");
    group.bench_function("1MiB", |b| {
        b.iter(|| {
            let dest = out_dir.path().join("restored.mca");
            store.get(&hash, &dest).expect("get");
        });
    });
    group.finish();
}

/// 目录扫描：把树转成「路径 → 哈希」并写入 CAS
fn bench_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("scan_tree");
    for (dirs, files) in [(4usize, 8usize), (16, 16)] {
        let count = dirs * files;
        group.throughput(Throughput::Elements(count as u64));
        let root = tempfile::tempdir().expect("root");
        // 只关心树的结构，字节数由 `Throughput` 另行表达
        let _bytes = bench::write_tree(root.path(), dirs, files, 4096);
        let store_dir = tempfile::tempdir().expect("store");
        let store = CasStore::new(store_dir.path());
        let scanner = mcsup::backup::DirectoryScanner;
        group.bench_function(BenchmarkId::new("files", count), |b| {
            b.iter(|| {
                let rules = mcsup::port::ScanRules::default();
                let cancel = mcsup::port::CancellationToken::new();
                scanner
                    .scan(root.path(), &store, &rules, &mut |_| {}, &cancel)
                    .expect("scan");
            });
        });
    }
    group.finish();
}

/// 快照元数据序列化：文件数线性增长时的编解码开销
fn bench_manifest(c: &mut Criterion) {
    let mut group = c.benchmark_group("manifest_json");
    for paths in [100usize, 1000] {
        let json = bench::manifest_json(paths);
        group.throughput(Throughput::Bytes(json.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(paths), &json, |b, json| {
            b.iter(|| {
                let parsed: serde_json::Value = serde_json::from_str(json).expect("parse manifest");
                serde_json::to_string(&parsed).expect("encode manifest");
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_cas_put,
    bench_cas_get,
    bench_scan,
    bench_manifest
);
criterion_main!(benches);
