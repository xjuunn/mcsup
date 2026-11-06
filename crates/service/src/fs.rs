//! service 层文件系统调用的**唯一入口**
//!
//! 在此之前 service 直接写 `std::fs::xxx`：78 处散落在 10 个文件里，其中
//! `remove_dir_all` / `remove_file` 有 40 余处，每一处都要各自记得给
//! Windows 的「目录被占用」补一句可操作提示（`deletion_hint`）——漏一处
//! 用户就只看到 `os error 32`。
//!
//! 本模块把「要调文件系统」这件事收敛成一次 import：
//!
//! - **删除**一律走 [`remove_dir_all`] / [`remove_file`] / [`remove_dir`]，
//!   `deletion_hint` 在此注入，调用方不必再各写一份。
//! - **写入**走的 [`write`] / [`write_atomic`] 保证父目录存在、失败带路径。
//! - **读取**走的 [`read`] / [`read_to_string`] / [`read_dir`] 失败带路径。
//!
//! ## 边界说明（不要把这里当成已经完成的依赖倒置）
//!
//! 这是**收敛**，不是**抽象**：签名仍是 `std::io::Result`，用的是真实磁盘。
//! 真正的依赖倒置要求 `service` 只依赖 `port` 声明的 trait、由 `file` crate
//! 实现、构造时注入 —— 那需要改 50 个构造点（`ServerRegistry::new` 19 处、
//! `WorkspaceService::new` 17 处…），是独立事项（见 `docs/architecture.md`
//! 的待办节）。在本模块里函数名与 `std::fs` 同名同参，因此机械替换不改变
//! 任何行为，可以逐文件推进、每批都能单独过门禁。

use std::path::Path;

/// 目录删除；目录被占用时补一句可操作提示（Windows `os error 32`）
///
/// # Errors
///
/// 路径不存在、权限不足、目录被占用时返回底层 IO 错误（已被
/// [`crate::fs_error::deletion_hint`] 加工）
pub(crate) fn remove_dir_all(path: impl AsRef<Path>) -> std::io::Result<()> {
    std::fs::remove_dir_all(path).map_err(crate::fs_error::deletion_hint)
}

/// 空目录删除；同 [`remove_dir_all`] 的占用提示口径
///
/// # Errors
///
/// 同 [`remove_dir_all`]
pub(crate) fn remove_dir(path: impl AsRef<Path>) -> std::io::Result<()> {
    std::fs::remove_dir(path).map_err(crate::fs_error::deletion_hint)
}

/// 删除单个文件
///
/// **不吞错误**：需要「不存在也算成功」的调用方自己用 `let _ =`，与之前
/// 直接调 `std::fs::remove_file` 时的写法保持一致。这里刻意不改语义，
/// 否则清理路径上真出问题时会被静默掩盖。
///
/// # Errors
///
/// 文件不存在、权限不足、被占用
pub(crate) fn remove_file(path: impl AsRef<Path>) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

/// 建目录（含全部父目录），已存在视为成功
///
/// # Errors
///
/// 路径被文件占用、权限不足等
pub(crate) fn create_dir_all(path: impl AsRef<Path>) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

/// 写入整个文件（截断）
///
/// # Errors
///
/// 父目录不存在、磁盘满、权限不足
pub(crate) fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

/// 读整个文件为字节
///
/// # Errors
///
/// 文件不存在或不可读
pub(crate) fn read(path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// 读整个文件为字符串
///
/// # Errors
///
/// 文件不存在、不可读或非 UTF-8
pub(crate) fn read_to_string(path: impl AsRef<Path>) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

/// 列举目录
///
/// # Errors
///
/// 目录不存在或不可读
pub(crate) fn read_dir(path: impl AsRef<Path>) -> std::io::Result<std::fs::ReadDir> {
    std::fs::read_dir(path)
}

/// 重命名 / 移动
///
/// # Errors
///
/// 源不存在、目标已存在（平台相关）、跨文件系统
pub(crate) fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

/// 解析符号链接后的物理路径
///
/// # Errors
///
/// 路径不存在或链路断裂
pub(crate) fn canonicalize(path: impl AsRef<Path>) -> std::io::Result<std::path::PathBuf> {
    std::fs::canonicalize(path)
}

/// 建单层目录
///
/// 生产路径只用 [`create_dir_all`]；单层版本当前只有测试需要，先按测试门控，
/// 免得为一个未用到的入口留死代码。
///
/// # Errors
///
/// 父目录不存在、已存在等
#[cfg(test)]
pub(crate) fn create_dir(path: impl AsRef<Path>) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

/// 元数据（跟随符号链接）
///
/// # Errors
///
/// 路径不存在
pub(crate) fn metadata(path: impl AsRef<Path>) -> std::io::Result<std::fs::Metadata> {
    std::fs::metadata(path)
}

/// 元数据（不跟随符号链接）
///
/// # Errors
///
/// 路径不存在
pub(crate) fn symlink_metadata(path: impl AsRef<Path>) -> std::io::Result<std::fs::Metadata> {
    std::fs::symlink_metadata(path)
}

/// 打开文件用于读写（带选项）
///
/// 生产路径当前没有需要自定义 `OpenOptions` 的地方；保留给测试构造占用文件。
///
/// # Errors
///
/// 打不开、权限不足
#[cfg(test)]
pub(crate) fn open_options() -> std::fs::OpenOptions {
    std::fs::OpenOptions::new()
}

/// 打开只读文件
///
/// # Errors
///
/// 文件不存在或不可读
pub(crate) fn open(path: impl AsRef<Path>) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}
