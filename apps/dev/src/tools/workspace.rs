//! 工程根定位与子进程执行。
//!
//! MCP 服务由客户端启动，工作目录不一定是仓库根，因此所有涉及仓库路径的工具
//! 都经 [`workspace_root`] 解析，不直接使用相对路径。

use std::path::{Path, PathBuf};

use crate::tools::ToolError;

/// 定位工程根目录。
///
/// 依次尝试：
/// 1. `MCSUP_DEV_TOOLS_ROOT` 环境变量（显式指定，优先）
/// 2. 当前工作目录及其祖先中含 `Cargo.toml` + `scripts/check.sh` 的目录
/// 3. 可执行文件所在目录的祖先（`target/debug/` 上溯到仓库根）
///
/// 都找不到时返回错误，不猜测。
pub(crate) fn workspace_root() -> Result<PathBuf, ToolError> {
    if let Some(explicit) = std::env::var_os("MCSUP_DEV_TOOLS_ROOT") {
        let dir = PathBuf::from(explicit);
        if is_workspace(&dir) {
            return Ok(dir);
        }
        return Err(ToolError::InvalidInput(format!(
            "MCSUP_DEV_TOOLS_ROOT 指向的不是 mcsup 工程根：{}",
            dir.display()
        )));
    }

    let mut candidates = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        candidates.push(parent.to_owned());
    }

    for start in candidates {
        for ancestor in start.ancestors() {
            if is_workspace(ancestor) {
                return Ok(ancestor.to_owned());
            }
        }
    }

    Err(ToolError::NotFound(
        "找不到 mcsup 工程根（含 Cargo.toml 与 scripts/check.sh 的目录），\
         可用 MCSUP_DEV_TOOLS_ROOT 显式指定"
            .to_owned(),
    ))
}

/// 工程根的判定标准：同时存在 `Cargo.toml` 与 `scripts/check.sh`。
///
/// 只看 `Cargo.toml` 会把 `crates/*` 子 crate 误判为根。
fn is_workspace(dir: &Path) -> bool {
    dir.join("Cargo.toml").is_file() && dir.join("scripts").join("check.sh").is_file()
}

/// 把可能是相对路径的用户输入解析成绝对路径。
///
/// 相对路径相对于工程根解释，而不是服务进程的 cwd——后者由客户端决定。
pub(crate) fn resolve_in_workspace(raw: &str) -> Result<PathBuf, ToolError> {
    if raw.trim().is_empty() {
        return Err(ToolError::InvalidInput("path 不能为空".to_owned()));
    }
    let candidate = PathBuf::from(raw);
    if candidate.is_absolute() {
        return Ok(candidate);
    }
    Ok(workspace_root()?.join(candidate))
}

/// 一次子进程执行的结果。
#[derive(Debug)]
pub(crate) struct CommandOutput {
    pub(crate) success: bool,
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    /// 是否因超时被终止。
    pub(crate) timed_out: bool,
}

/// 执行子进程并捕获输出。
///
/// 超时会杀掉子进程并返回 `timed_out = true`，而不是无限等待。
/// stdin 一律置空：工具不接受交互式输入，否则子进程会卡在读取上。
pub(crate) async fn run(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    timeout: std::time::Duration,
) -> Result<CommandOutput, ToolError> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    #[cfg(target_os = "windows")]
    {
        // 工具不应在用户桌面上闪出控制台窗口。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let child = command.spawn().map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            ToolError::NotFound(format!("找不到可执行程序 {program}"))
        } else {
            ToolError::Io(format!("启动 {program} 失败: {source}"))
        }
    })?;

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(CommandOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            timed_out: false,
        }),
        Ok(Err(source)) => Err(ToolError::Io(format!("等待 {program} 失败: {source}"))),
        Err(_) => Ok(CommandOutput {
            success: false,
            code: None,
            stdout: String::new(),
            stderr: format!("{program} 超过 {}s 未结束，已终止", timeout.as_secs()),
            timed_out: true,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::is_workspace;

    #[test]
    fn workspace_root_requires_both_markers() {
        let temp = std::env::temp_dir().join("mcsup-dev-tools-ws-test");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join("scripts")).expect("create temp dir");

        std::fs::write(temp.join("Cargo.toml"), b"").expect("write Cargo.toml");
        assert!(!is_workspace(&temp), "只有 Cargo.toml 不算工程根");

        std::fs::write(temp.join("scripts").join("check.sh"), b"").expect("write check.sh");
        assert!(is_workspace(&temp), "两个标记齐备才算工程根");

        let _ = std::fs::remove_dir_all(&temp);
    }
}
