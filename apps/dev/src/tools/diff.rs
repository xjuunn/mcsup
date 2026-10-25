//! `get_diff` 工具：读取当前工作树的改动摘要。
//!
//! 只读，不修改任何状态。给 agent 自查改动用，输出按文件聚合，
//! 避免把整段 diff 塞进上下文。

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ToolError;
use super::workspace;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// 单个文件 diff 的上限，超出只给统计。
const MAX_PATCH_CHARS: usize = 8000;

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GetDiffRequest {
    /// 要包含哪些改动来源，默认全部。
    #[serde(default)]
    pub(crate) scope: DiffScope,
    /// 只看这些路径（相对工程根）；省略看全部。
    #[serde(default)]
    pub(crate) paths: Option<Vec<String>>,
    /// 返回完整 patch；默认只返回每个文件的增删行数。
    #[serde(default)]
    pub(crate) include_patch: bool,
}

/// 改动来源。默认三项全含，与 `git status` 的直觉一致。
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub(crate) struct DiffScope {
    /// 包含已暂存的改动。
    #[serde(default = "default_true")]
    pub(crate) staged: bool,
    /// 包含未暂存的改动。
    #[serde(default = "default_true")]
    pub(crate) unstaged: bool,
    /// 包含未跟踪文件。
    #[serde(default = "default_true")]
    pub(crate) untracked: bool,
}

impl Default for DiffScope {
    fn default() -> Self {
        Self {
            staged: true,
            unstaged: true,
            untracked: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub(crate) struct FileChange {
    /// 相对工程根的路径。
    path: String,
    /// 新增行数；二进制文件为 null。
    additions: Option<usize>,
    /// 删除行数；二进制文件为 null。
    deletions: Option<usize>,
    /// 是否未跟踪。
    untracked: bool,
    /// 完整 patch（仅 `include_patch` 时出现）。
    #[serde(skip_serializing_if = "Option::is_none")]
    patch: Option<String>,
    /// patch 是否被截断。
    #[serde(skip_serializing_if = "Option::is_none")]
    patch_truncated: Option<bool>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GetDiffResponse {
    /// 当前分支名；detached HEAD 时为 null。
    branch: Option<String>,
    /// worktree 根目录的绝对路径，方便调用方确认自己在哪个 worktree。
    worktree: String,
    files: Vec<FileChange>,
    /// 是否有任何改动。
    dirty: bool,
}

pub(crate) async fn get_diff(
    Parameters(request): Parameters<GetDiffRequest>,
) -> Result<String, String> {
    get_diff_inner(request).await.map_err(|e| e.render())
}

async fn get_diff_inner(request: GetDiffRequest) -> Result<String, ToolError> {
    let root = workspace::workspace_root()?;
    let paths = request.paths.unwrap_or_default();

    let branch = current_branch(&root).await;
    let mut files: Vec<FileChange> = Vec::new();

    if request.scope.staged {
        collect(
            &root,
            &["diff", "--cached", "--numstat"],
            &paths,
            false,
            &mut files,
        )
        .await?;
    }
    if request.scope.unstaged {
        collect(&root, &["diff", "--numstat"], &paths, false, &mut files).await?;
    }
    if request.scope.untracked {
        collect_untracked(&root, &paths, &mut files).await?;
    }

    merge_duplicates(&mut files);

    if request.include_patch {
        for file in &mut files {
            let patch = if file.untracked {
                read_untracked(&root, &file.path)
            } else {
                let mut args = vec!["diff".to_owned(), "HEAD".to_owned(), "--".to_owned()];
                args.push(file.path.clone());
                workspace::run("git", &args, Some(&root), GIT_TIMEOUT)
                    .await
                    .ok()
                    .map(|output| output.stdout)
            };
            if let Some(patch) = patch {
                let truncated = patch.chars().count() > MAX_PATCH_CHARS;
                file.patch = Some(if truncated {
                    patch.chars().take(MAX_PATCH_CHARS).collect::<String>()
                } else {
                    patch
                });
                file.patch_truncated = truncated.then_some(true);
            }
        }
    }

    let response = GetDiffResponse {
        branch,
        worktree: root.display().to_string(),
        dirty: !files.is_empty(),
        files,
    };
    serde_json::to_string_pretty(&response)
        .map_err(|error| ToolError::Failed(format!("序列化结果失败: {error}")))
}

async fn current_branch(root: &std::path::Path) -> Option<String> {
    let output = workspace::run(
        "git",
        &[
            "rev-parse".to_owned(),
            "--abbrev-ref".to_owned(),
            "HEAD".to_owned(),
        ],
        Some(root),
        GIT_TIMEOUT,
    )
    .await
    .ok()?;
    if !output.success {
        return None;
    }
    let name = output.stdout.trim();
    // detached HEAD 时 git 返回字面量 HEAD，此时没有分支名。
    (!name.is_empty() && name != "HEAD").then(|| name.to_owned())
}

/// 执行一次 `--numstat` 并累加到 `files`。
async fn collect(
    root: &std::path::Path,
    base_args: &[&str],
    paths: &[String],
    untracked: bool,
    files: &mut Vec<FileChange>,
) -> Result<(), ToolError> {
    let mut args: Vec<String> = base_args.iter().map(|s| (*s).to_owned()).collect();
    if !paths.is_empty() {
        args.push("--".to_owned());
        args.extend(paths.iter().cloned());
    }

    let output = workspace::run("git", &args, Some(root), GIT_TIMEOUT).await?;
    if !output.success {
        return Err(ToolError::Failed(format!(
            "git {} 失败：{}",
            base_args.join(" "),
            output.stderr.trim()
        )));
    }

    for line in output.stdout.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(additions), Some(deletions), Some(path)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        files.push(FileChange {
            path: path.to_owned(),
            // 二进制文件在 numstat 里显示为 `-`。
            additions: additions.parse().ok(),
            deletions: deletions.parse().ok(),
            untracked,
            patch: None,
            patch_truncated: None,
        });
    }
    Ok(())
}

async fn collect_untracked(
    root: &std::path::Path,
    paths: &[String],
    files: &mut Vec<FileChange>,
) -> Result<(), ToolError> {
    let mut args = vec![
        "ls-files".to_owned(),
        "--others".to_owned(),
        "--exclude-standard".to_owned(),
    ];
    if !paths.is_empty() {
        args.push("--".to_owned());
        args.extend(paths.iter().cloned());
    }
    let output = workspace::run("git", &args, Some(root), GIT_TIMEOUT).await?;
    if !output.success {
        return Err(ToolError::Failed(format!(
            "git ls-files 失败：{}",
            output.stderr.trim()
        )));
    }
    for path in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
        files.push(FileChange {
            path: path.to_owned(),
            additions: None,
            deletions: None,
            untracked: true,
            patch: None,
            patch_truncated: None,
        });
    }
    Ok(())
}

fn read_untracked(root: &std::path::Path, relative: &str) -> Option<String> {
    let path = root.join(relative);
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    // 只读文本文件；大文件直接放弃，避免把二进制内容塞进上下文。
    if metadata.len() > (MAX_PATCH_CHARS as u64) * 4 {
        return Some(format!("（未跟踪文件 {} 字节，未展开）", metadata.len()));
    }
    std::fs::read_to_string(&path).ok()
}

/// 同一文件可能同时出现在暂存区和工作区，按路径合并并累加行数。
fn merge_duplicates(files: &mut Vec<FileChange>) {
    let mut merged: Vec<FileChange> = Vec::with_capacity(files.len());
    for file in files.drain(..) {
        match merged
            .iter_mut()
            .find(|existing| existing.path == file.path)
        {
            Some(existing) => {
                existing.additions = match (existing.additions, file.additions) {
                    (Some(a), Some(b)) => Some(a + b),
                    _ => None,
                };
                existing.deletions = match (existing.deletions, file.deletions) {
                    (Some(a), Some(b)) => Some(a + b),
                    _ => None,
                };
            }
            None => merged.push(file),
        }
    }
    merged.sort_by(|a, b| a.path.cmp(&b.path));
    *files = merged;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, additions: Option<usize>, deletions: Option<usize>) -> FileChange {
        FileChange {
            path: path.to_owned(),
            additions,
            deletions,
            untracked: false,
            patch: None,
            patch_truncated: None,
        }
    }

    #[test]
    fn merges_same_path_and_sums_lines() {
        let mut files = vec![
            change("a.rs", Some(1), Some(2)),
            change("a.rs", Some(3), Some(4)),
        ];
        merge_duplicates(&mut files);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].additions, Some(4));
        assert_eq!(files[0].deletions, Some(6));
    }

    #[test]
    fn binary_file_stays_unknown() {
        let mut files = vec![
            change("a.png", None, None),
            change("a.png", Some(1), Some(1)),
        ];
        merge_duplicates(&mut files);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].additions, None);
    }

    #[test]
    fn sorts_by_path() {
        let mut files = vec![change("b", Some(1), Some(1)), change("a", Some(1), Some(1))];
        merge_duplicates(&mut files);
        assert_eq!(files[0].path, "a");
    }
}
