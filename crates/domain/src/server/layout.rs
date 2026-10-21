/// 运行期文件布局：纯路径构造，IO 由调用方执行
///
/// `console.log` 与 `run/` 由 supervisor 与服务编排共用同一口径，
/// 分散构造会让日志 / 停机读到错误位置。
use std::path::{Path, PathBuf};

/// 运行期目录名（socket / PID 落点）
pub const RUN_DIR: &str = "run";
/// 服务端控制台输出重定向文件名
pub const CONSOLE_LOG: &str = "console.log";

/// `{server_dir}/run`
#[must_use]
pub fn run_dir(server_dir: &Path) -> PathBuf {
    server_dir.join(RUN_DIR)
}

/// `{server_dir}/console.log`
#[must_use]
pub fn console_log_path(server_dir: &Path) -> PathBuf {
    server_dir.join(CONSOLE_LOG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_sit_under_server_dir() {
        let dir = Path::new("/srv/mc/survival");
        assert_eq!(run_dir(dir), Path::new("/srv/mc/survival/run"));
        assert_eq!(
            console_log_path(dir),
            Path::new("/srv/mc/survival/console.log")
        );
    }

    #[test]
    fn console_log_is_not_inside_run_dir() {
        // 日志要给用户直接看，不藏进运行期目录
        let log = console_log_path(Path::new("/srv"));
        assert!(!log.starts_with(run_dir(Path::new("/srv"))));
    }
}
