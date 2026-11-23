//! 敏感文件的权限检查
//!
//! 撤销列表、密钥这类文件只有本用户可读写才有意义：同机其他用户能改一份
//! 「撤销列表」或「密钥」，等于绕开整条判定链。检查按「权限位比要求更宽就
//! 拒绝」做，不静默收紧——静默改权限会让用户看不到自己配错了。

use port::plugin::HostError;
use std::path::Path;

/// 检查文件或目录权限不比 `0600` / `0700` 更宽
///
/// 非 Unix 平台没有等价语义（ACL 不体现在 mode 上），直接放行。
///
/// # Errors
///
/// 权限过宽时返回 `denied`。
#[cfg(unix)]
pub(crate) fn check_permissions(
    path: &Path,
    metadata: &std::fs::Metadata,
    is_dir: bool,
) -> Result<(), HostError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    let too_open = if is_dir {
        mode & 0o077 != 0
    } else {
        mode & 0o177 != 0
    };
    if too_open {
        return Err(HostError::denied(format!(
            "{} has permissions {mode:04o}; expected {}",
            path.display(),
            if is_dir { "0700" } else { "0600" }
        )));
    }
    Ok(())
}

/// 非 Unix：无 mode 位，放行
///
/// 返回 `Result` 而不是 `()`：调用方统一用 `?` 传播，不必按平台分叉写法。
///
/// # Errors
///
/// 不会返回错误。
// 保持与 Unix 分支同签名是为了让调用方只有一份写法：这个 `Result` 在非 Unix
// 上确实恒为 `Ok`，clippy 因此会报 `unnecessary_wraps`——只在非 Unix 分支上
// 放行，Unix 分支（CI 走的那条）仍照常受检。
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn check_permissions(
    _path: &Path,
    _metadata: &std::fs::Metadata,
    _is_dir: bool,
) -> Result<(), HostError> {
    Ok(())
}
