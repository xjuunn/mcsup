//! 共享交互组件
//!
//! `workspace init` / `server create` / `server remove` / `server set` 等
//! 入口共用这里的步骤提示、选择器与校验重输。组件只做 UI 与输入校验，
//! 业务判定仍在 service / domain；调用方必须先经
//! `printer.is_interactive()` 守卫，非交互（JSON / 非 TTY）绝不进入提示分支。

use crate::output::Printer;
use anyhow::{Context, Result};
use inquire::autocompletion::Replacement;
use inquire::validator::{ErrorMessage, Validation};
use inquire::{Autocomplete, Confirm, CustomUserError, Select, Text};
use mcsup::domain::workspace::WorkspaceName;
use mcsup::service::{MARKER_FILE, NOISE_ENTRIES, absolute_path};
use std::path::{Path, PathBuf};

mod scoped;

pub(crate) use scoped::{ScopeHint, ScopedChoice, select_filter_scoped};

/// 空输入的校验文案（可过滤选择的两个组件共用）
pub(crate) const ERR_EMPTY_INPUT: &str = "请输入关键字或从建议中选择";

/// 非候选全名的校验文案（同上）
pub(crate) const ERR_NOT_A_CHOICE: &str = "请从建议中选择一个选项";

/// 步骤标题：`[n/N] 标题`（多阶段任务的统一回显风格）
pub(crate) fn step(printer: Printer, step: usize, total: usize, title: &str) {
    printer.print_section(&format!("[{step}/{total}] {title}"));
}

/// 通用确认（默认值随调用方）
///
/// # Errors
///
/// 读取终端输入失败时返回底层错误
pub(crate) fn confirm_yes(message: &str, default: bool) -> Result<bool> {
    Confirm::new(message)
        .with_default(default)
        .prompt()
        .context("读取确认输入")
}

/// 危险操作的第二道闸：逐字输入目标名称确认
///
/// # Errors
///
/// 读取终端输入失败时返回底层错误
pub(crate) fn confirm_by_name(message: &str, expected: &str) -> Result<()> {
    let expected = expected.to_owned();
    let _: String = Text::new(message)
        .with_validator(move |input: &str| Ok(name_matches(input, &expected)))
        .prompt()
        .context("已取消")?;
    Ok(())
}

/// 名称逐字匹配（`confirm_by_name` 的校验内核）
#[must_use]
pub(crate) fn name_matches(input: &str, expected: &str) -> Validation {
    if input.trim() == expected {
        Validation::Valid
    } else {
        Validation::Invalid(ErrorMessage::Custom(
            "名称不匹配，请重新输入（Ctrl+C 取消）".to_owned(),
        ))
    }
}

/// `workspace init` 无 `--dir` 时的目录位置三选项
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkspaceLocation {
    /// 数据目录 `{data_dir}/mcsup/workspaces/<名称>`（名称确定后才合成路径）
    DataDir,
    /// 当前工作目录
    CurrentDir,
    /// 自定义目录（已按入库口径绝对化）
    Custom(PathBuf),
}

#[derive(Debug, Clone, Copy)]
enum LocationKind {
    Data,
    Current,
    Custom,
}

#[derive(Debug, Clone)]
struct LocationChoice {
    label: String,
    kind: LocationKind,
}

impl std::fmt::Display for LocationChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// 目录位置三选项；自定义路径走带校验的输入，交互下重输而不是终止
///
/// # Errors
///
/// 读取终端输入或解析数据目录失败时返回底层错误
pub(crate) fn pick_workspace_location() -> Result<WorkspaceLocation> {
    let data_root = crate::config::platform_data_dir()?.join("workspaces");
    let cwd = std::env::current_dir().context("获取当前工作目录失败")?;
    let choices = vec![
        LocationChoice {
            label: format!("mcsup 数据目录 ({}/<名称>)", data_root.display()),
            kind: LocationKind::Data,
        },
        LocationChoice {
            label: format!("当前目录 ({})", cwd.display()),
            kind: LocationKind::Current,
        },
        LocationChoice {
            label: "自定义目录".to_owned(),
            kind: LocationKind::Custom,
        },
    ];
    let picked = Select::new("工作区目录", choices)
        .prompt()
        .context("读取目录选择")?;
    match picked.kind {
        LocationKind::Data => Ok(WorkspaceLocation::DataDir),
        LocationKind::Current => Ok(WorkspaceLocation::CurrentDir),
        LocationKind::Custom => Ok(WorkspaceLocation::Custom(input_custom_path()?)),
    }
}

fn input_custom_path() -> Result<PathBuf> {
    let input: String = Text::new("自定义目录")
        .with_default(".")
        .with_help_message("回车接受默认值；不存在的目录会自动创建")
        .with_validator(|input: &str| Ok(validate_dir_input(input)))
        .prompt()
        .context("读取自定义目录")?;
    Ok(absolute_path(Path::new(input.trim())))
}

/// 目录输入校验：`dir_issue` 判据映射为 inquire 校验（非空且非工作区 /
/// 指向文件 / 最近存在的祖先不是目录 → 重输）
#[must_use]
pub(crate) fn validate_dir_input(input: &str) -> Validation {
    match dir_issue(Path::new(input.trim())) {
        None => Validation::Valid,
        Some(message) => Validation::Invalid(ErrorMessage::Custom(message)),
    }
}

/// 目录不可用的原因；`None` 表示可以交给 `workspace init`
///
/// - 不存在：最近存在的祖先必须是目录（`file/sub` 必然建不出来）
/// - 是文件：不可用
/// - 非空且缺 `workspace.toml`：既不是工作区也不是空目录，拒绝覆盖
/// - 非空但有 marker：中断恢复场景，放行（由 service 补注册）
/// - 仅噪声文件（`.DS_Store` 等）：按空目录放行，口径与 service 判空一致
#[must_use]
pub(crate) fn dir_issue(raw: &Path) -> Option<String> {
    let abs = absolute_path(raw);
    if !abs.exists() {
        return match nearest_existing(&abs) {
            Some(existing) if existing.is_dir() => None,
            Some(existing) => Some(format!("{} 不是目录", existing.display())),
            None => None,
        };
    }
    if !abs.is_dir() {
        return Some(format!("{} 是一个文件", abs.display()));
    }
    match std::fs::read_dir(&abs) {
        Err(err) => Some(format!("无法读取 {}: {err}", abs.display())),
        Ok(entries) => {
            let foreign = entries.flatten().any(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                !NOISE_ENTRIES.contains(&name.as_ref())
            });
            if foreign && !abs.join(MARKER_FILE).exists() {
                return Some(format!(
                    "{} 非空且不是工作区（缺少 {MARKER_FILE}）",
                    abs.display()
                ));
            }
            None
        }
    }
}

/// 自身或最近存在的祖先
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    loop {
        if current.exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

/// 名称输入：回车取默认值，非法即时重输
///
/// # Errors
///
/// 读取终端输入失败时返回底层错误
pub(crate) fn input_name(label: &str, default: &str) -> Result<String> {
    let input: String = Text::new(label)
        .with_default(default)
        .with_help_message("字母、数字、-、_，最长 64；回车取默认值")
        .with_validator(|input: &str| Ok(validate_name(input)))
        .prompt()
        .context("读取名称输入")?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(default.to_owned())
    } else {
        Ok(trimmed.to_owned())
    }
}

/// 名称校验：空 = 取默认；其余按 domain 规则
#[must_use]
pub(crate) fn validate_name(input: &str) -> Validation {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Validation::Valid;
    }
    match WorkspaceName::parse(trimmed) {
        Ok(_) => Validation::Valid,
        Err(_) => Validation::Invalid(ErrorMessage::Custom(
            "名称只能包含字母、数字、-、_，最长 64 字符".to_owned(),
        )),
    }
}

/// 通用文本输入：回车取默认值（默认值常为该项当前值）
///
/// # Errors
///
/// 读取终端输入失败时返回底层错误
pub(crate) fn input_text(label: &str, default: &str, help: &str) -> Result<String> {
    let input: String = Text::new(label)
        .with_default(default)
        .with_help_message(help)
        .prompt()
        .context("读取输入")?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(default.to_owned())
    } else {
        Ok(trimmed.to_owned())
    }
}

/// 布尔选择（`server set autostart` 等）：默认项置顶，回车即取默认
///
/// # Errors
///
/// 读取终端输入失败时返回底层错误
pub(crate) fn select_bool(label: &str, default: bool) -> Result<bool> {
    let choices = vec!["true", "false"];
    let picked = Select::new(label, choices)
        .with_starting_cursor(usize::from(!default))
        .prompt()
        .context("读取选择")?;
    Ok(picked == "true")
}

/// 提示是否被 Esc / Ctrl+C 取消：交互循环据此静默退出而非报错
#[must_use]
pub(crate) fn is_cancelled(err: &anyhow::Error) -> bool {
    matches!(
        err.root_cause().downcast_ref::<inquire::InquireError>(),
        Some(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted
        )
    )
}

/// 目录名可作默认工作区名时返回它；不合法由调用方退回序号
#[must_use]
pub(crate) fn default_name_for_dir(dir: &Path) -> Option<String> {
    let file = dir.file_name()?.to_str()?;
    WorkspaceName::parse(file).ok()?;
    Some(file.to_owned())
}

/// 可过滤名称选择：仅一个选项直接返回；否则输入关键字、从建议中选
///
/// # Errors
///
/// 无可选项、读取终端输入失败或结果无法归一到候选名时返回错误
pub(crate) fn select_filter(label: &str, options: &[String]) -> Result<String> {
    match options {
        [] => return Err(anyhow::anyhow!("没有可选项")),
        [only] => return Ok((*only).clone()),
        _ => {}
    }
    let choices = options.to_vec();
    let validator_choices = choices.clone();
    let input: String = Text::new(label)
        .with_help_message("输入关键字过滤；从建议中选择后回车")
        .with_autocomplete(FilterComplete { choices })
        .with_validator(move |input: &str| Ok(validate_choice(input, &validator_choices)))
        .prompt()
        .context("读取选择")?;
    canonical_choice(input.trim(), options)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("无效选择: {input}"))
}

/// 大小写不敏感的关键字过滤（空关键字 = 全部候选）
pub(crate) fn filter_choices<'a>(choices: &'a [String], input: &str) -> Vec<&'a String> {
    let key = input.trim().to_lowercase();
    if key.is_empty() {
        return choices.iter().collect();
    }
    choices
        .iter()
        .filter(|choice| choice.to_lowercase().contains(&key))
        .collect()
}

/// 大小写不敏感的全名匹配（把用户输入归一到候选名；部分匹配不算）
pub(crate) fn canonical_choice<'a>(input: &str, choices: &'a [String]) -> Option<&'a String> {
    choices
        .iter()
        .find(|choice| choice.eq_ignore_ascii_case(input))
}

/// 选择校验：必须是某个候选的全名（大小写不敏感），空输入无效
#[must_use]
pub(crate) fn validate_choice(input: &str, choices: &[String]) -> Validation {
    let input = input.trim();
    if input.is_empty() {
        return Validation::Invalid(ErrorMessage::Custom(ERR_EMPTY_INPUT.to_owned()));
    }
    if canonical_choice(input, choices).is_some() {
        Validation::Valid
    } else {
        Validation::Invalid(ErrorMessage::Custom(ERR_NOT_A_CHOICE.to_owned()))
    }
}

/// 输入关键字 → 过滤后的建议列表
#[derive(Debug, Clone)]
struct FilterComplete {
    choices: Vec<String>,
}

impl Autocomplete for FilterComplete {
    fn get_suggestions(&mut self, input: &str) -> Result<Vec<String>, CustomUserError> {
        Ok(filter_choices(&self.choices, input)
            .into_iter()
            .take(10)
            .cloned()
            .collect())
    }

    fn get_completion(
        &mut self,
        input: &str,
        highlighted_suggestion: Option<String>,
    ) -> Result<Replacement, CustomUserError> {
        // 用户显式高亮的建议优先；不合法的高亮一律拒绝
        if let Some(highlighted) = highlighted_suggestion {
            return if self.choices.contains(&highlighted) {
                Ok(Some(highlighted))
            } else {
                Ok(None)
            };
        }
        let input = input.trim();
        if input.is_empty() {
            return Ok(None);
        }
        // 输入已是全名 → 归一后采纳
        if let Some(exact) = canonical_choice(input, &self.choices) {
            return Ok(Some(exact.clone()));
        }
        // 单一匹配 → 自动补全
        if let [only] = filter_choices(&self.choices, input)[..] {
            return Ok(Some((*only).clone()));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn default_name_prefers_valid_dirname() {
        assert_eq!(
            default_name_for_dir(Path::new("/tmp/my-ws")).as_deref(),
            Some("my-ws")
        );
        assert_eq!(
            default_name_for_dir(Path::new("/tmp/ws_1")).as_deref(),
            Some("ws_1")
        );
        assert!(default_name_for_dir(Path::new("/tmp/带空格")).is_none());
        assert!(default_name_for_dir(Path::new("/tmp/a b")).is_none());
        assert!(default_name_for_dir(Path::new("/")).is_none());
        assert_eq!(
            default_name_for_dir(Path::new("relative-name")).as_deref(),
            Some("relative-name")
        );
    }

    #[test]
    fn name_validation_follows_domain_rules() {
        assert!(matches!(validate_name("ws-01"), Validation::Valid));
        assert!(matches!(validate_name("ws_1"), Validation::Valid));
        assert!(matches!(validate_name("  "), Validation::Valid));
        assert!(matches!(validate_name("bad name"), Validation::Invalid(_)));
        assert!(matches!(validate_name("a/b"), Validation::Invalid(_)));
        assert!(matches!(validate_name(&"x".repeat(64)), Validation::Valid));
        assert!(matches!(
            validate_name(&"x".repeat(65)),
            Validation::Invalid(_)
        ));
    }

    #[test]
    fn dir_issue_accepts_fresh_noise_and_workspace_dirs() {
        let tmp = tempdir().expect("tempdir");
        let fresh = tmp.path().join("fresh");
        assert!(dir_issue(&fresh).is_none(), "不存在的目录可创建");

        fs::create_dir(&fresh).expect("mkdir");
        assert!(dir_issue(&fresh).is_none(), "空目录可作工作区");

        fs::write(fresh.join(".DS_Store"), "").expect("noise");
        assert!(dir_issue(&fresh).is_none(), "噪声文件按空处理");

        fs::write(fresh.join(MARKER_FILE), "schema = 1").expect("marker");
        fs::write(fresh.join("world"), "data").expect("content");
        assert!(dir_issue(&fresh).is_none(), "有 marker 走中断恢复");

        assert!(dir_issue(&tmp.path().join("new/deep")).is_none());
    }

    #[test]
    fn dir_issue_rejects_files_and_foreign_dirs() {
        let tmp = tempdir().expect("tempdir");
        let file = tmp.path().join("f.txt");
        fs::write(&file, "x").expect("file");
        let issue = dir_issue(&file).expect("文件应不可用");
        assert!(issue.contains("文件"), "{issue}");

        let under_file = file.join("sub");
        let issue = dir_issue(&under_file).expect("文件之下应不可用");
        assert!(issue.contains("不是目录"), "{issue}");

        let foreign = tmp.path().join("foreign");
        fs::create_dir(&foreign).expect("mkdir");
        fs::write(foreign.join("data.txt"), "x").expect("content");
        let issue = dir_issue(&foreign).expect("非空目录应不可用");
        assert!(issue.contains("非空"), "{issue}");
    }

    #[test]
    fn validate_dir_input_maps_issue_to_validation() {
        let tmp = tempdir().expect("tempdir");
        let file = tmp.path().join("f.txt");
        fs::write(&file, "x").expect("file");
        assert!(matches!(
            validate_dir_input(file.to_str().expect("utf8")),
            Validation::Invalid(_)
        ));
        let fresh = tmp.path().join("fresh-ok");
        assert!(matches!(
            validate_dir_input(fresh.to_str().expect("utf8")),
            Validation::Valid
        ));
    }

    #[test]
    fn name_matches_requires_exact_trimmed_input() {
        assert!(matches!(name_matches("ws-01", "ws-01"), Validation::Valid));
        assert!(matches!(
            name_matches("  ws-01  ", "ws-01"),
            Validation::Valid
        ));
        assert!(matches!(
            name_matches("ws-02", "ws-01"),
            Validation::Invalid(_)
        ));
        assert!(matches!(name_matches("", "ws-01"), Validation::Invalid(_)));
    }

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn filter_and_canonical_match_case_insensitively() {
        let opts = owned(&["Survival-1", "creative"]);
        assert_eq!(filter_choices(&opts, "surv"), vec![&opts[0]]);
        assert_eq!(filter_choices(&opts, "  SURV "), vec![&opts[0]]);
        assert_eq!(filter_choices(&opts, ""), vec![&opts[0], &opts[1]]);
        assert!(filter_choices(&opts, "zzz").is_empty());

        assert_eq!(canonical_choice("SURVIVAL-1", &opts), Some(&opts[0]));
        assert_eq!(canonical_choice("surv", &opts), None);
        assert_eq!(canonical_choice("survival", &opts), None);
    }

    #[test]
    fn choice_validation_requires_full_name() {
        let opts = owned(&["a", "b"]);
        assert!(matches!(validate_choice("a", &opts), Validation::Valid));
        assert!(matches!(validate_choice("A", &opts), Validation::Valid));
        assert!(matches!(validate_choice("", &opts), Validation::Invalid(_)));
        assert!(matches!(
            validate_choice("  ", &opts),
            Validation::Invalid(_)
        ));
        assert!(matches!(
            validate_choice("ab", &opts),
            Validation::Invalid(_)
        ));
    }

    /// 补全语义：显式高亮 > 输入全名 > 单一匹配；非法输入不替换
    #[test]
    fn autocomplete_resolves_suggestions_to_completion() {
        let opts = owned(&["Survival-1", "creative"]);
        let mut ac = FilterComplete {
            choices: opts.clone(),
        };

        let suggestions = Autocomplete::get_suggestions(&mut ac, "surv").expect("suggestions");
        assert_eq!(suggestions, vec!["Survival-1".to_owned()]);

        // 单一匹配 → 自动补全
        let completion = Autocomplete::get_completion(&mut ac, "surv", None).expect("completion");
        assert_eq!(completion, Some("Survival-1".to_owned()));

        // 输入已是全名 → 归一采纳（大小写不敏感）
        let completion =
            Autocomplete::get_completion(&mut ac, "CREATIVE", None).expect("completion");
        assert_eq!(completion, Some("creative".to_owned()));

        // 高亮是合法选项 → 显式选择优先
        let completion = Autocomplete::get_completion(&mut ac, "", Some("creative".to_owned()))
            .expect("completion");
        assert_eq!(completion, Some("creative".to_owned()));

        // 高亮不合法 → 拒绝
        let completion =
            Autocomplete::get_completion(&mut ac, "", Some("evil".to_owned())).expect("completion");
        assert_eq!(completion, None);

        // 多匹配且无高亮 → 交由校验拒绝部分匹配
        let completion = Autocomplete::get_completion(&mut ac, "r", None).expect("completion");
        assert_eq!(completion, None);

        // 空输入无高亮 → 不替换
        let completion = Autocomplete::get_completion(&mut ac, "", None).expect("completion");
        assert_eq!(completion, None);
    }
}
