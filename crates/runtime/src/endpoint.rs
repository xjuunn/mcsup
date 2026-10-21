//! supervisor 端点：socket / PID / 自身日志的落点
///
/// 两端（supervisor 与客户端）都从 `server_dir` 推导同一端点，禁止各自拼。
use domain::server::layout::run_dir;
use std::path::{Path, PathBuf};

/// Unix socket 文件名（Windows 走命名管道，不用此文件）
const SOCKET_FILE: &str = "supervisor.sock";
/// supervisor 自身的 PID 文件名
const PID_FILE: &str = "supervisor.pid";
/// supervisor 启动失败时的 stderr 落点
pub(crate) const SUPERVISOR_LOG: &str = "supervisor.log";

/// `{server_dir}/run/supervisor.sock`
#[must_use]
pub(crate) fn socket_path(server_dir: &Path) -> PathBuf {
    run_dir(server_dir).join(SOCKET_FILE)
}

/// `{server_dir}/run/supervisor.pid`
#[must_use]
pub(crate) fn pid_path(server_dir: &Path) -> PathBuf {
    run_dir(server_dir).join(PID_FILE)
}

/// `{server_dir}/run/supervisor.log`
#[must_use]
pub(crate) fn supervisor_log_path(server_dir: &Path) -> PathBuf {
    run_dir(server_dir).join(SUPERVISOR_LOG)
}

/// Windows 命名管道名：路径进不了管道命名空间，按目录路径做稳定哈希
///
/// 两端都持有 `server_dir`，同一目录推出同一名字。
#[cfg(any(windows, test))]
#[must_use]
pub(crate) fn pipe_name(server_dir: &Path) -> String {
    format!(
        r"\\.\pipe\mcsup-supervisor-{hash}",
        hash = fnv1a(server_dir)
    )
}

/// FNV-1a（64 位）：无依赖的稳定哈希，仅用于生成管道名
#[cfg(any(windows, test))]
fn fnv1a(path: &Path) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 解析当前平台的 IPC 端点名
///
/// # Errors
///
/// 名称在当前平台不受支持（如把路径型名字用在只支持命名空间的平台）时返回错误
pub(crate) fn endpoint_name(
    server_dir: &Path,
) -> std::io::Result<interprocess::local_socket::Name<'static>> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::ToFsName as _;
        let socket = socket_path(server_dir);
        socket
            .to_fs_name::<interprocess::local_socket::GenericFilePath>()
            .map(interprocess::local_socket::Name::into_owned)
            .map_err(std::io::Error::other)
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::ToFsName as _;
        use interprocess::os::windows::local_socket::NamedPipe;
        pipe_name(server_dir)
            .to_fs_name::<NamedPipe>()
            .map_err(std::io::Error::other)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = server_dir;
        Err(std::io::Error::other(
            "unsupported platform for local sockets",
        ))
    }
}

/// 启动前清理上一次崩溃残留的端点文件（活着的 supervisor 由调用方先探测）
pub(crate) fn cleanup_stale(server_dir: &Path) {
    #[cfg(unix)]
    {
        let socket = socket_path(server_dir);
        if socket.exists() {
            let _ = std::fs::remove_file(socket);
        }
    }
    let pid = pid_path(server_dir);
    if pid.exists() {
        let _ = std::fs::remove_file(pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_paths_sit_under_run_dir() {
        let dir = Path::new("/srv/mc/a");
        assert_eq!(socket_path(dir), Path::new("/srv/mc/a/run/supervisor.sock"));
        assert_eq!(pid_path(dir), Path::new("/srv/mc/a/run/supervisor.pid"));
        assert_eq!(
            supervisor_log_path(dir),
            Path::new("/srv/mc/a/run/supervisor.log")
        );
    }

    #[test]
    fn pipe_name_is_stable_and_dir_sensitive() {
        let a = pipe_name(Path::new("/srv/mc/a"));
        assert_eq!(a, pipe_name(Path::new("/srv/mc/a")));
        assert_ne!(a, pipe_name(Path::new("/srv/mc/b")));
        assert!(a.starts_with(r"\\.\pipe\mcsup-supervisor-"));
    }

    #[test]
    fn endpoint_name_resolves_for_current_platform() {
        let name = endpoint_name(Path::new("/tmp/mcsup-e2e")).expect("endpoint name");
        // 同目录推导两次必须一致（两端握手的前提）
        let again = endpoint_name(Path::new("/tmp/mcsup-e2e")).expect("endpoint name");
        assert_eq!(name, again);
    }

    #[test]
    fn cleanup_removes_stale_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = run_dir(dir.path());
        std::fs::create_dir_all(&run).expect("run dir");
        std::fs::write(pid_path(dir.path()), b"1").expect("pid");
        cleanup_stale(dir.path());
        assert!(!pid_path(dir.path()).exists());
    }
}
