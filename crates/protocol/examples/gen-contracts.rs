//! 生成 SDK 契约文件（`plugins.md` §14.4）
//!
//! 用法：
//!
//! ```text
//! cargo run -q -p mcsup-protocol --example gen-contracts -- <target> <out>
//! ```
//!
//! `<target>` 目前只有 `typescript`；`<out>` 是写入路径。生成物带固定头
//! `// GENERATED FROM protocol::plugin. DO NOT EDIT.`，门禁重新生成后
//! `git status --porcelain`，漂移即失败。
//!
//! 用 example 而不是 bin：example 不随 `cargo install` 安装，读者不会在
//! `PATH` 里看到它，也不需要为它加 feature 开关。

// 工作区禁 `print_stderr`（入口不得直连终端），但本 example 就是命令行
// 工具，诊断只能写 stderr——stdout 留给管道。
#![allow(clippy::print_stderr)]

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(target_raw) = args.next() else {
        eprintln!("usage: gen-contracts <typescript> <out>");
        std::process::exit(2);
    };
    let Some(out) = args.next() else {
        eprintln!("usage: gen-contracts <typescript> <out>");
        std::process::exit(2);
    };
    let Some(target) = protocol::plugin::contracts::Target::parse(&target_raw) else {
        eprintln!("unknown contract target: {target_raw}");
        std::process::exit(2);
    };
    let text = protocol::plugin::contracts::render(target);
    let path = std::path::Path::new(&out);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!("create {}: {err}", parent.display());
        std::process::exit(1);
    }
    if let Err(err) = std::fs::write(path, text) {
        eprintln!("write {}: {err}", path.display());
        std::process::exit(1);
    }
}
