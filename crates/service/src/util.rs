use crate::FsError;
use std::path::Path;

/// 当前 Unix 时间戳（秒）；系统时钟早于 epoch 时归零，不让坏时钟炸掉写入
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed())
}

/// 同目录临时文件写满后 rename 覆盖
///
/// marker（`mcsup.json` / `workspace.toml`）被截断就等于身份丢失：下次读取
/// 按「损坏」拒绝，`server link` / `workspace use` 全部失败。直接
/// `fs::write` 会在「打开截断」与「写完」之间留下空文件窗口，崩溃或断电正好
/// 落在这里就不可恢复；先写临时文件再原子替换，正式文件只会有旧内容或新内容。
///
/// 临时文件名带 pid 与进程内序号：两个进程 / 线程同时写同一个 marker 时
/// 不会互相踩内容（最终的 rename 由文件系统保证原子）。
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<(), FsError> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let tmp = path.with_file_name(format!(".{name}.{}.{seq}.tmp", std::process::id()));
    if let Err(err) = std::fs::write(&tmp, text) {
        let _ = std::fs::remove_file(&tmp);
        return Err(FsError::io(&tmp, err));
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(FsError::io(path, err));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_content_without_leaving_temp() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mcsup.json");
        std::fs::write(&path, "old").expect("seed");

        write_atomic(&path, "{\"schema\":1}").expect("write");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "{\"schema\":1}"
        );

        // 临时文件不能留在目录里：备份与 doctor 会把它们当业务文件
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");
    }

    #[test]
    fn atomic_write_creates_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("workspace.toml");
        write_atomic(&path, "schema = 1\n").expect("write");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "schema = 1\n"
        );
    }
}
