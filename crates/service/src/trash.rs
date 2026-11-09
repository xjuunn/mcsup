//! 文件回收：把「判定为不该装」的文件移出原位而不销毁
//!
//! 移除是工具的推断，不是用户的决定：判定可能出错（实测 `side="CLIENT"` 的
//! `CreativeCore`、依赖全写 `side="CLIENT"` 的 `embeddiumplus` 都能在
//! dedicated server 上正常装载）。直接删掉就把用户的文件连同「判错了怎么办」
//! 一起消灭了。移到服务端目录下的回收目录，用户随时能取回。
//!
//! 两条使用路径共用这里：[`crate::ModService`] 的修复计划与
//! [`crate::ModpackService`] 的安装末判定。判定口径可以不同，落盘动作必须
//! 只有一份——否则一处改名字、另一处漏改，回收站里就会出现两套布局。

use std::path::{Path, PathBuf};

use crate::FsError;

/// 移除文件的暂存目录名（相对服务端目录）
///
/// 与 `mods/` 同级而不是放在里面：放进 `mods/` 会被加载器当成待加载的 mod。
/// 以 `.` 开头的名字同时避开备份作用域里的常见过滤口径。
pub const TRASH_DIR: &str = ".mcsup-trash";

/// 把文件移进服务端目录下的回收站
///
/// 同名冲突时加序号，不覆盖：回收站里已有的同名文件同样可能是唯一副本。
/// 回收站不是备份，会在下次成功启动后由用户自行清理——这里只保证「不销毁」。
///
/// # Errors
///
/// 回收目录建不出来、或文件移不过去时返回 [`FsError`]
pub(crate) fn trash_file(server_dir: &Path, path: &Path) -> Result<PathBuf, FsError> {
    let trash = server_dir.join(TRASH_DIR);
    crate::fs::create_dir_all(&trash).map_err(|err| FsError::io(&trash, err))?;
    let name = path.file_name().map_or_else(
        || std::ffi::OsString::from("unnamed"),
        std::ffi::OsStr::to_os_string,
    );
    let mut target = trash.join(&name);
    let mut seq = 1u32;
    // 冲突时在扩展名前加序号：`a.jar` → `a-1.jar`，保持可识别
    let stem = Path::new(&name)
        .file_stem()
        .unwrap_or_else(|| std::ffi::OsStr::new("unnamed"));
    let ext = Path::new(&name)
        .extension()
        .map(std::ffi::OsStr::to_os_string);
    while target.exists() {
        let mut cand = stem.to_os_string();
        cand.push(format!("-{seq}"));
        if let Some(ext) = &ext {
            cand.push(".");
            cand.push(ext);
        }
        target = trash.join(cand);
        seq += 1;
    }
    crate::fs::rename(path, &target).map_err(|err| FsError::io(path, err))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_file_and_keeps_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = dir.path();
        let file = server.join("a.jar");
        std::fs::write(&file, b"x").expect("write");

        let trashed = trash_file(server, &file).expect("trash");
        assert_eq!(trashed, server.join(TRASH_DIR).join("a.jar"));
        assert!(!file.exists(), "原位不再有文件");
        assert!(trashed.exists(), "回收站里有文件");
    }

    #[test]
    fn repeated_trash_does_not_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = dir.path();
        let trash = server.join(TRASH_DIR);
        std::fs::create_dir_all(&trash).expect("trash dir");
        std::fs::write(trash.join("a.jar"), b"old").expect("seed");

        let file = server.join("a.jar");
        std::fs::write(&file, b"new").expect("write");
        let trashed = trash_file(server, &file).expect("trash");

        assert_eq!(trashed, trash.join("a-1.jar"), "冲突时加序号，不覆盖");
        assert_eq!(std::fs::read(trash.join("a.jar")).expect("read"), b"old");
        assert_eq!(std::fs::read(&trashed).expect("read"), b"new");
    }
}
