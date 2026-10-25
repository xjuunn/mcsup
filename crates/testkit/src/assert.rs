//! 断言辅助：失败信息必须自证
//!
//! 给 AI 写的测试有一个特殊要求：失败时打印的信息必须**足够支持一次不猜的修复**，
//! 否则模型只能在报错上反复试探。每条消息统一包含四段：
//!
//! ```text
//! 不变量：<被违反的性质>
//! 期望：<expected>
//! 实际：<actual>
//! 现场：<路径 / 目录内容 / 其它上下文>
//! ```
//!
//! 用 [`assert_invariant`] 表达「必须恒成立」的性质，用 [`Failure`] 组装消息。
//! 第二个参数是闭包：条件成立时不构造消息，失败路径才付格式化成本。

use std::fmt::Display;
use std::path::Path;

/// 一条带上下文的失败描述
#[derive(Debug)]
pub struct Failure {
    message: String,
}

impl Failure {
    /// 以「被违反的不变量」起头
    #[must_use]
    pub fn invariant(invariant: &str) -> Self {
        Self {
            message: format!("不变量：{invariant}"),
        }
    }

    /// 追加一行「期望」
    #[must_use]
    pub fn expected(mut self, expected: impl Display) -> Self {
        self.push("期望", expected);
        self
    }

    /// 追加一行「实际」
    #[must_use]
    pub fn actual(mut self, actual: impl Display) -> Self {
        self.push("实际", actual);
        self
    }

    /// 追加一行「现场」（路径、命令、计数等）
    #[must_use]
    pub fn context(mut self, label: &str, value: impl Display) -> Self {
        self.push(label, value);
        self
    }

    /// 追加一个磁盘路径，并把该目录下的文件列表一并带出
    ///
    /// 失败现场里最常缺的就是「磁盘上到底有什么」。目录不存在时也会写明，
    /// 因为「不存在」本身往往就是被判定的错。
    #[must_use]
    pub fn tree(mut self, path: &Path) -> Self {
        self.push("路径", path.display());
        self.push("目录内容", list_tree(path));
        self
    }

    /// 立即失败
    pub fn panic(&self) -> ! {
        panic!("{}", self.message);
    }

    fn push(&mut self, label: &str, value: impl Display) {
        self.message.push('\n');
        self.message.push_str(label);
        self.message.push('：');
        self.message.push_str(&value.to_string());
    }
}

/// 断言一个恒成立的性质成立，否则按统一格式失败
pub fn assert_invariant(holds: bool, failure: impl FnOnce() -> Failure) {
    if !holds {
        failure().panic();
    }
}

/// 相对路径列表（排序后），最多 40 条
///
/// 限制条数：失败信息本身不能把日志撑爆，大到看不清的现场等于没有现场。
#[must_use]
pub fn list_tree(root: &Path) -> String {
    const MAX_TREE_ENTRIES: usize = 40;
    let mut out = Vec::new();
    collect(root, root, &mut out);
    out.sort();
    let total = out.len();
    if out.is_empty() {
        return "<空或缺席>".to_owned();
    }
    let shown = out
        .into_iter()
        .take(MAX_TREE_ENTRIES)
        .collect::<Vec<_>>()
        .join("\n  ");
    let mut text = format!("\n  {shown}");
    if total > MAX_TREE_ENTRIES {
        let last = MAX_TREE_ENTRIES;
        text = format!("{text}\n  …（共 {total} 项，只显示前 {last} 项）");
    }
    text
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let rel = rel.to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            out.push(format!("{rel}/"));
            collect(root, &path, out);
        } else {
            let size = entry.metadata().map_or(0, |meta| meta.len());
            out.push(format!("{rel} ({size} B)"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 失败消息四段齐全：少任何一段，修复方就得回头猜
    #[test]
    fn failure_message_carries_every_section() {
        let failure = Failure::invariant("save-on 必须在扫描失败后发出")
            .expected("[save-off, save-all flush, save-on]")
            .actual("[save-off, save-all flush]")
            .context("服务器", "alpha");
        assert_invariant(failure.message.contains("不变量：save-on"), || {
            Failure::invariant("消息应含不变量段")
        });
        assert_invariant(
            failure.message.contains("期望：") && failure.message.contains("实际："),
            || Failure::invariant("消息应含期望与实际段"),
        );
        assert_invariant(failure.message.contains("服务器：alpha"), || {
            Failure::invariant("消息应含自定义现场段")
        });
    }

    #[test]
    fn tree_lists_relative_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("world")).expect("mkdir");
        std::fs::write(dir.path().join("world/level.dat"), b"abc").expect("write");
        let listing = list_tree(dir.path());
        assert_invariant(listing.contains("world/level.dat (3 B)"), || {
            Failure::invariant("目录列表应含相对路径与大小").actual(listing.clone())
        });
        assert_invariant(listing.contains("world/"), || {
            Failure::invariant("目录列表应含子目录").actual(listing.clone())
        });
    }

    #[test]
    fn missing_path_is_reported_explicitly() {
        let missing = tempfile::tempdir()
            .expect("tempdir")
            .path()
            .join("not-there");
        assert_invariant(list_tree(&missing) == "<空或缺席>", || {
            Failure::invariant("缺席目录应显式说明")
        });
    }
}
