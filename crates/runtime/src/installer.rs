//! 运行服务端安装器（`Forge` / `NeoForge`）
//!
//! 安装器的约定（实测 `Forge` `1.21.1-52.0.1`）：
//!
//! - `java -jar forge-...-installer.jar --installServer <dir>`
//! - 产出 `libraries/`、`run.sh` / `run.bat`、`user_jvm_args.txt`
//! - 成功时输出 `The server installed successfully`，退出码 0
//!
//! 安装器联网下载原版与 libraries，耗时可达数十秒，因此输出不进入 supervisor
//! 控制台，而是收集尾部若干行供失败时排查；成功即丢弃。

use port::{InstallerError, InstallerRunner};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 失败时保留的输出尾部行数
const TAIL_LINES: usize = 20;

/// 安装器成功后在 `dir` 中应存在的参数文件（任一即算成功产物）
///
/// `Forge` 会生成 `run.sh` / `run.bat` 与 `user_jvm_args.txt`；用参数文件判断
/// 比输出文案更可靠（文案可能随版本变化）。
const EXPECTED_MARKERS: &[&str] = &["user_jvm_args.txt", "run.sh", "run.bat"];

/// 本机安装器执行器：直接 spawn `java`
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalInstallerRunner;

impl LocalInstallerRunner {
    /// 新建执行器
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl InstallerRunner for LocalInstallerRunner {
    fn run_installer(
        &self,
        java_bin: &Path,
        installer: &Path,
        dir: &Path,
    ) -> Result<(), InstallerError> {
        let output = Command::new(java_bin)
            .arg("-jar")
            .arg(installer)
            .arg("--installServer")
            .arg(dir)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(InstallerError::Spawn)?;

        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            return Err(InstallerError::Failed {
                code,
                tail: tail_of(&output.stdout, &output.stderr),
            });
        }

        // 退出码为 0 不代表产物齐全：某些镜像会「成功」但漏生成参数文件
        if !has_expected_marker(dir) {
            return Err(InstallerError::NoOutput {
                dir: dir.to_path_buf(),
            });
        }
        Ok(())
    }
}

/// 安装目录下是否存在任一预期产物文件
fn has_expected_marker(dir: &Path) -> bool {
    EXPECTED_MARKERS.iter().any(|name| dir.join(name).is_file())
}

/// 取 stdout + stderr 的末尾若干行，合并为一条可读文本
fn tail_of(stdout: &[u8], stderr: &[u8]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for stream in [stdout, stderr] {
        let text = String::from_utf8_lossy(stream);
        for line in text.lines() {
            let trimmed = line.trim_end();
            if !trimmed.is_empty() {
                lines.push(trimmed.to_owned());
            }
        }
    }
    let start = lines.len().saturating_sub(TAIL_LINES);
    lines[start..].join("\n")
}

/// 安装器产出的参数文件相对路径（供启动使用）
#[must_use]
pub fn args_files_for(build: &str) -> Vec<PathBuf> {
    let platform_args = if cfg!(windows) {
        format!("libraries/net/minecraftforge/forge/{build}/win_args.txt")
    } else {
        format!("libraries/net/minecraftforge/forge/{build}/unix_args.txt")
    };
    vec![
        PathBuf::from("user_jvm_args.txt"),
        PathBuf::from(platform_args),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_of_joins_last_lines_of_both_streams() {
        let stdout = b"line1\nline2\n";
        let stderr = b"warn\n";
        let tail = tail_of(stdout, stderr);
        assert_eq!(tail, "line1\nline2\nwarn");
    }

    #[test]
    fn tail_of_keeps_only_last_n_lines() {
        let mut stdout = Vec::new();
        for i in 0..50 {
            stdout.extend_from_slice(format!("l{i}\n").as_bytes());
        }
        let tail = tail_of(&stdout, b"");
        let count = tail.lines().count();
        assert_eq!(count, TAIL_LINES);
        assert!(tail.contains("l49"), "{tail}");
        assert!(!tail.contains("l0\n"), "{tail}");
    }

    #[test]
    fn tail_of_empty_is_empty() {
        assert_eq!(tail_of(b"", b""), "");
        assert_eq!(tail_of(b"\n\n", b"  \n"), "");
    }

    #[test]
    fn marker_detection_requires_any_expected_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!has_expected_marker(dir.path()));
        std::fs::write(dir.path().join("user_jvm_args.txt"), b"# args").expect("write");
        assert!(has_expected_marker(dir.path()));
    }

    #[test]
    fn args_files_order_user_before_platform() {
        let files = args_files_for("1.21.1-52.0.1");
        assert_eq!(files[0], PathBuf::from("user_jvm_args.txt"));
        assert!(files[1].to_string_lossy().contains("1.21.1-52.0.1"));
        assert!(files[1].to_string_lossy().contains("_args.txt"));
    }
}
