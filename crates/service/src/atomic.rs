//! 原子写入与时间戳：`service` 内所有落盘 marker 都经这里
//!
//! 模块名说明「这里保证什么」（原子性），而不是「这里是杂物间」。新东西若要
//! 放进来，先回答它是否与「一次写入要么全成要么全不成」有关——无关的就不该
//! 进这个模块。

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
    if let Err(err) = crate::fs::write(&tmp, text) {
        let _ = crate::fs::remove_file(&tmp);
        return Err(FsError::io(&tmp, err));
    }
    if let Err(err) = crate::fs::rename(&tmp, path) {
        let _ = crate::fs::remove_file(&tmp);
        return Err(FsError::io(path, err));
    }
    Ok(())
}

/// 用已写好的暂存文件替换目标文件（目标可以已存在）
///
/// 用于「下载到暂存目录 → 检查 → 换入正式位置」（mod / 资源包）。文件内容由
/// 下载器写好并已校验，这里只剩「换入」这一步，因此与 [`write_atomic`] 的差别
/// 是：源文件不是本模块写的，目标可能是**已存在**的旧版本文件。
///
/// Windows 上 `rename` 不会覆盖已存在的目标，所以旧文件先改名挪开：
///
/// 1. 旧文件 → `.<name>.<pid>.<seq>.old`（同目录，保证同一卷）
/// 2. 暂存文件 → 目标位置
/// 3. 删除 `.old`
///
/// 第 2 步失败时把旧文件放回原位：换入失败不能连用户原有的 mod 一起丢掉。
/// 挪开的临时名不以 `.jar` 结尾，目录扫描（[`crate::collect_jars`]）不会把
/// 残留的半成品当成已装内容。
pub(crate) fn replace_file(staged: &Path, target: &Path) -> Result<(), FsError> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    let backup = if target.exists() {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let name = target
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let backup = target.with_file_name(format!(".{name}.{}.{seq}.old", std::process::id()));
        crate::fs::rename(target, &backup).map_err(|err| FsError::io(target, err))?;
        Some(backup)
    } else {
        None
    };

    if let Err(err) = crate::fs::rename(staged, target) {
        if let Some(backup) = &backup
            && let Err(restore) = crate::fs::rename(backup, target)
        {
            return Err(FsError::marker(
                backup,
                format!(
                    "替换 {} 失败（{err}），且旧文件回滚失败（{restore}）；旧文件留在 {}",
                    target.display(),
                    backup.display()
                ),
            ));
        }
        return Err(FsError::io(target, err));
    }

    if let Some(backup) = &backup {
        let _ = crate::fs::remove_file(backup);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_file_moves_staged_into_place_when_target_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let staged = dir.path().join("staged.jar");
        let target = dir.path().join("mods").join("a.jar");
        crate::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
        crate::fs::write(&staged, b"new").expect("seed");

        replace_file(&staged, &target).expect("replace");

        assert_eq!(crate::fs::read(&target).expect("read"), b"new");
        assert!(!staged.exists(), "暂存文件必须被移走");
    }

    /// 目标已存在时内容被替换，且不残留 `.old` 备份
    #[test]
    fn replace_file_overwrites_existing_target_and_leaves_no_backup() {
        let dir = tempfile::tempdir().expect("temp dir");
        let staged = dir.path().join("staged.jar");
        let target = dir.path().join("a.jar");
        crate::fs::write(&staged, b"new").expect("seed staged");
        crate::fs::write(&target, b"old").expect("seed target");

        replace_file(&staged, &target).expect("replace");

        assert_eq!(crate::fs::read(&target).expect("read"), b"new");
        let leftovers: Vec<String> = crate::fs::read_dir(dir.path())
            .expect("readdir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.to_ascii_lowercase().ends_with(".old"))
            .collect();
        assert!(leftovers.is_empty(), "残留备份：{leftovers:?}");
    }

    /// 暂存文件不存在时换入失败，但旧文件必须还在原位
    #[test]
    fn replace_file_keeps_target_when_staged_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let staged = dir.path().join("missing.jar");
        let target = dir.path().join("a.jar");
        crate::fs::write(&target, b"old").expect("seed target");

        let result = replace_file(&staged, &target);

        assert!(result.is_err(), "暂存文件不存在时必须失败");
        assert_eq!(crate::fs::read(&target).expect("read"), b"old");
    }

    #[test]
    fn atomic_write_replaces_content_without_leaving_temp() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mcsup.json");
        crate::fs::write(&path, "old").expect("seed");

        write_atomic(&path, "{\"schema\":1}").expect("write");
        assert_eq!(
            crate::fs::read_to_string(&path).expect("read"),
            "{\"schema\":1}"
        );

        // 临时文件不能留在目录里：备份与 doctor 会把它们当业务文件
        let leftovers: Vec<_> = crate::fs::read_dir(dir.path())
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
            crate::fs::read_to_string(&path).expect("read"),
            "schema = 1\n"
        );
    }
}
