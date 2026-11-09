//! 停用目录：把「这台服务端装不了」的文件从加载范围里挪开，但不销毁
//!
//! 与 [`crate::trash`] 的区别是**语义**，不是动作：回收站表达「工具认为
//! 这个文件不该存在」，停用目录表达「用户决定暂时不用它」。前者是推断，
//! 后者是决定，因此停用目录必须放在用户看得见、认得出的位置，且保留原
//! 文件名——用户要能自己搬回来。
//!
//! # 为什么是 `mods/disabled/` 而不是改名
//!
//! 项目里另有一套「加 `.disabled` 后缀」的口径（见
//! [`domain::server::modpack::ModpackFile::installs_on_server`]）。两种做法
//! 加载器都不加载，差别在可读性：
//!
//! - 改名：`PuzzlesLib-v8.1.33.jar.disabled`，文件名被改动，与清单、与
//!   上游对不上，用户得靠字符串比对认文件。
//! - 子目录：`disabled/PuzzlesLib-v8.1.33.jar`，原名不变，且一屏列得完。
//!
//! 子目录不被扫描这一点对四个加载器都成立：Fabric / Quilt 的
//! `DirectoryModCandidateFinder` 用 `Files.list`（不递归），Forge /
//! `NeoForge` 的 `FolderRepositorySource` 用 `Files.walk(path, 1)`（深度
//! 1）。整合包作者自己也常用 `mods/<mc版本>/` 子目录装不启用的文件。
//! 本仓库的 [`crate::mod_inspect::collect_jars`] 同样只看直接子项，两边
//! 口径一致。

use std::path::{Path, PathBuf};

use crate::FsError;

/// 停用目录（相对服务端目录）
///
/// 放在 `mods/` 里面而不是与服务端目录同级：用户排查「为什么这个 mod
/// 没生效」时会先看 `mods/`，停用的东西就该在那儿。
pub const DISABLED_DIR: &str = "mods/disabled";

/// 把一个 mod 文件移进停用目录
///
/// 同名冲突时加序号，不覆盖：停用目录里已有的同名文件可能是另一份版本，
/// 覆盖就等于销毁。返回新路径，供调用方回显。
///
/// # Errors
///
/// 停用目录建不出来、或文件移不过去时返回 [`FsError`]
pub(crate) fn disable_file(server_dir: &Path, path: &Path) -> Result<PathBuf, FsError> {
    let disabled = server_dir.join(DISABLED_DIR);
    crate::fs::create_dir_all(&disabled).map_err(|err| FsError::io(&disabled, err))?;
    let name = path.file_name().map_or_else(
        || std::ffi::OsString::from("unnamed"),
        std::ffi::OsStr::to_os_string,
    );
    let mut target = disabled.join(&name);
    let stem = Path::new(&name)
        .file_stem()
        .unwrap_or_else(|| std::ffi::OsStr::new("unnamed"));
    let ext = Path::new(&name)
        .extension()
        .map(std::ffi::OsStr::to_os_string);
    let mut seq = 1u32;
    while target.exists() {
        let mut cand = stem.to_os_string();
        cand.push(format!("-{seq}"));
        if let Some(ext) = &ext {
            cand.push(".");
            cand.push(ext);
        }
        target = disabled.join(cand);
        seq += 1;
    }
    crate::fs::rename(path, &target).map_err(|err| FsError::io(path, err))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mcsup-disabled-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(dir.join("mods")).expect("dir");
        dir
    }

    #[test]
    fn moves_file_and_keeps_name() {
        let server = scratch("move");
        let file = server.join("mods/a.jar");
        std::fs::write(&file, b"x").expect("write");

        let moved = disable_file(&server, &file).expect("disable");

        assert_eq!(moved, server.join(DISABLED_DIR).join("a.jar"));
        assert!(moved.is_file(), "原名保留，用户能认出来");
        assert!(!file.exists(), "原位置不能留副本，否则加载器照样加载");
        std::fs::remove_dir_all(&server).ok();
    }

    #[test]
    fn repeated_disable_does_not_overwrite() {
        let server = scratch("dup");
        let disabled = server.join(DISABLED_DIR);
        std::fs::create_dir_all(&disabled).expect("dir");
        std::fs::write(disabled.join("a.jar"), b"first").expect("write");

        let file = server.join("mods/a.jar");
        std::fs::write(&file, b"second").expect("write");
        let moved = disable_file(&server, &file).expect("disable");

        assert_eq!(moved, disabled.join("a-1.jar"), "冲突加序号，不覆盖");
        assert_eq!(
            std::fs::read(disabled.join("a.jar")).expect("read"),
            b"first"
        );
        std::fs::remove_dir_all(&server).ok();
    }

    #[test]
    fn creates_disabled_dir_on_demand() {
        let server = scratch("create");
        let file = server.join("mods/a.jar");
        std::fs::write(&file, b"x").expect("write");

        assert!(!server.join(DISABLED_DIR).exists());
        disable_file(&server, &file).expect("disable");

        assert!(server.join(DISABLED_DIR).is_dir());
        std::fs::remove_dir_all(&server).ok();
    }
}
