//! 目标操作系统与架构标识，对齐各下载 API 的取值

/// 当前平台的 API 操作系统标识
#[must_use]
pub(crate) fn current_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

/// 当前平台的 API 架构标识
#[must_use]
pub(crate) fn current_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x64"
    }
}
