//! 基准测试共用的样本构造
//!
//! 单独成 lib 是为了让 `benches/*.rs` 能复用同一份样本生成逻辑：每个 bench
//! 文件各写一遍会让「同一份数据」的假设悄悄分叉，测出来的数字就不可比。
//!
//! 本 crate 不发布、不被其他包依赖。

// 基准数据造不出来就无从测量，因此这里允许 `expect`：失败必须立刻可见，
// 而不是让基准在残缺输入上跑出一组好看但无意义的数字。
#![allow(clippy::expect_used, reason = "基准样本构造失败必须立刻中止")]

use std::path::Path;

/// 生成一棵有 `dirs` 个目录、每目录 `files` 个文件、每文件 `size` 字节的树
///
/// 返回写入的总字节数。用于扫描 / 哈希 / 备份路径的基准输入。
///
/// # Panics
///
/// 目录或文件创建失败时 panic：基准数据造不出来就无从测量，继续跑只会得到
/// 误导性的数字。
#[must_use]
pub fn write_tree(root: &Path, dirs: usize, files: usize, size: usize) -> u64 {
    let payload = vec![0x5au8; size];
    let mut total = 0u64;
    for d in 0..dirs {
        let dir = root.join(format!("world/region{d}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        for f in 0..files {
            let path = dir.join(format!("r.{d}.{f}.mca"));
            std::fs::write(&path, &payload).expect("write");
            total += size as u64;
        }
    }
    total
}

/// 一个典型快照清单的 JSON 文本（`paths` 条记录）
///
/// 用于序列化 / 反序列化基准：快照元数据的大小随文件数线性增长。
#[must_use]
pub fn manifest_json(paths: usize) -> String {
    let entries: Vec<String> = (0..paths)
        .map(|i| {
            format!(
                r#"{{"path":"world/region/r.{i}.mca","hash":"{:064x}","size":{}}}"#,
                i, 4096
            )
        })
        .collect();
    format!(r#"{{"entries":[{}]}}"#, entries.join(","))
}
