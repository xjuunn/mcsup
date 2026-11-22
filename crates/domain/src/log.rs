//! 日志文件的窗口读取
//!
//! 控制台日志与插件日志都是追加写的普通文件，可能长到几百 MB。调用方要的
//! 只有两种：末尾 N 行、从游标开始的下一批。整文件 `read` 会把进程内存打满
//! （一个不换行的几 GB 行就能做到），所以这里只按窗口读：窗口起点由调用方
//! 定，窗口长度有上界（一般取 [`crate::limits::MAX_LOG_TAIL_BYTES`]）。
//!
//! 游标一律是**文件字节偏移**：它可以跨进程、跨重启复用，不依赖额外状态
//! 文件；按行号做游标则会在日志轮转后指着完全不同的内容。

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// 文件内容的一个窗口
#[derive(Debug, Default)]
pub struct Window {
    bytes: Vec<u8>,
    start: u64,
    len: u64,
}

impl Window {
    /// 读取 `path` 末尾最多 `want` 字节
    ///
    /// 文件不存在按空窗口处理（日志还没写出来不是错误）。
    ///
    /// # Errors
    ///
    /// 打开、定位或读取失败时返回 [`io::Error`]
    pub fn tail(path: &Path, want: usize) -> io::Result<Self> {
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err),
        };
        let len = file.metadata()?.len();
        let want = u64::try_from(want).unwrap_or(u64::MAX);
        let start = len.saturating_sub(want);
        file.seek(SeekFrom::Start(start))?;
        read_window(&mut file, start, want, len)
    }

    /// 从 `offset` 起读取最多 `want` 字节
    ///
    /// 游标超过文件长度（日志轮转、被手工清空）时退回文件末尾，返回空窗口：
    /// 调用方按「没有新内容」处理，不会重播旧日志。
    ///
    /// # Errors
    ///
    /// 打开、定位或读取失败时返回 [`io::Error`]
    pub fn from(path: &Path, offset: u64, want: usize) -> io::Result<Self> {
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err),
        };
        let len = file.metadata()?.len();
        let start = offset.min(len);
        file.seek(SeekFrom::Start(start))?;
        read_window(
            &mut file,
            start,
            u64::try_from(want).unwrap_or(u64::MAX),
            len,
        )
    }

    /// 窗口起点的文件偏移
    #[must_use]
    pub fn start(&self) -> u64 {
        self.start
    }

    /// 窗口之前还有内容：首行可能是被截断的半行
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.start > 0
    }

    /// 窗口结束处的文件偏移（下一轮续读的游标）
    ///
    /// 窗口被长度上界截断时它小于 [`Window::file_len`]：剩下的内容下一轮
    /// 再读，游标不会跳过任何字节。
    #[must_use]
    pub fn end(&self) -> u64 {
        self.start + self.bytes.len() as u64
    }

    /// 打开时文件的字节长度
    ///
    /// 与 [`Window::end`] 区分：窗口可能只覆盖文件的一段，而调用方常常要
    /// 把「文件总长」回给上层做进度显示。
    #[must_use]
    pub fn file_len(&self) -> u64 {
        self.len
    }

    /// 窗口是否为空
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// 按行切分：`(行文本, 行尾的文件偏移)`
    ///
    /// 最后一段没有换行时也作为一行返回（游标是窗口末尾）：日志是追加写的，
    /// 这半行下一轮会以完整形态出现，调用方按游标续读不会重复。
    ///
    /// 文本按 UTF-8 lossy 解码：控制台日志可能夹着服务端输出的原始字节，
    /// 但游标始终按**原始字节**计数，不会因为替换字符而错位。
    #[must_use]
    pub fn lines(&self) -> Vec<(String, u64)> {
        let mut lines = Vec::new();
        let mut begin = 0;
        for (index, byte) in self.bytes.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let text = String::from_utf8_lossy(&self.bytes[begin..index]);
            lines.push((
                text.trim_end_matches('\r').to_owned(),
                self.start + index as u64 + 1,
            ));
            begin = index + 1;
        }
        if begin < self.bytes.len() {
            let text = String::from_utf8_lossy(&self.bytes[begin..]);
            lines.push((text.trim_end_matches('\r').to_owned(), self.end()));
        }
        lines
    }
}

fn read_window(file: &mut std::fs::File, start: u64, want: u64, len: u64) -> io::Result<Window> {
    let mut bytes = Vec::new();
    file.take(want).read_to_end(&mut bytes)?;
    Ok(Window { bytes, start, len })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "测试夹具，失败直接 panic 才有用")]

    use super::*;
    use std::io::Write as _;

    fn file_with(lines: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut file = std::fs::File::create(dir.path().join("app.log")).expect("create");
        for line in lines {
            writeln!(file, "{line}").expect("write");
        }
        dir
    }

    #[test]
    fn tail_returns_only_the_last_bytes() {
        let dir = file_with(&["one", "two", "three"]);
        let window = Window::tail(&dir.path().join("app.log"), 8).expect("tail");
        assert!(window.truncated(), "窗口从中间开始");
        assert_eq!(window.end(), 14);
        // 只保留窗口内的完整行，首行是半行（`e\n`）
        assert_eq!(
            window.lines().last().map(|(text, _)| text.as_str()),
            Some("three")
        );
    }

    #[test]
    fn tail_of_a_missing_file_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let window = Window::tail(&dir.path().join("nope.log"), 1024).expect("tail");
        assert!(window.is_empty());
        assert_eq!(window.start(), 0);
        assert_eq!(window.end(), 0);
    }

    #[test]
    fn lines_carry_the_byte_offset_after_each_line() {
        let dir = file_with(&["a", "bb"]);
        let window = Window::tail(&dir.path().join("app.log"), 1024).expect("tail");
        assert!(!window.truncated(), "整文件都读进来了");
        assert_eq!(
            window.lines(),
            vec![("a".to_owned(), 2), ("bb".to_owned(), 5)]
        );
    }

    /// 末尾没有换行的半行也算一行，游标落在窗口末尾
    #[test]
    fn a_trailing_partial_line_still_reports_its_cursor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.log");
        std::fs::write(&path, b"done\nhalf").expect("write");
        let window = Window::tail(&path, 1024).expect("tail");
        assert_eq!(
            window.lines(),
            vec![("done".to_owned(), 5), ("half".to_owned(), 9)]
        );
    }

    /// 游标续读：第二批不会重复第一批的行
    #[test]
    fn from_continues_after_the_cursor() {
        let dir = file_with(&["one", "two"]);
        let path = dir.path().join("app.log");
        let first = Window::from(&path, 0, 1024).expect("from");
        let cursor = first.lines().first().expect("line").1;
        let second = Window::from(&path, cursor, 1024).expect("from");
        let texts = second
            .lines()
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["two".to_owned()]);
    }

    /// 游标超过文件长度（轮转 / 清空）时退回末尾，不重播
    #[test]
    fn a_cursor_past_the_end_yields_nothing() {
        let dir = file_with(&["one"]);
        let window = Window::from(&dir.path().join("app.log"), 100, 1024).expect("from");
        assert!(window.is_empty());
        assert_eq!(window.start(), 4);
    }

    /// 窗口被长度上界截断时 `end` 停在窗口末尾，`file_len` 仍是文件全长
    #[test]
    fn a_bounded_window_still_reports_the_whole_file_length() {
        let dir = file_with(&["aaaaaaaa", "bbbbbbbb"]);
        let window = Window::from(&dir.path().join("app.log"), 0, 4).expect("from");
        assert_eq!(window.file_len(), 18);
        assert_eq!(window.end(), 4);
    }
}
