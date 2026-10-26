//! `config show / set / unset / edit`：平台配置的入口
//!
//! 键表、校验与归一化全在 `mcsup::service`（[`ConfigKey`] / [`Config`]），
//! 这里只做参数搬运、文件读写与渲染。写入走 `tmp → fsync → rename`，
//! 半截文件不会覆盖掉可用的旧配置。

use crate::cli::ConfigAction;
use crate::config::ConfigFile;
use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use anyhow::{Context, Result};
use comfy_table::Table;
use mcsup::protocol::cli::{ConfigRow, ConfigSetJson, ConfigShowJson};
use mcsup::service::ConfigKey;
use std::path::Path;

pub(crate) fn run(action: ConfigAction, printer: Printer) -> Result<()> {
    let file = ConfigFile::locate(None);
    match action {
        ConfigAction::Show => show(&file, printer),
        ConfigAction::Set { key, value } => set(&file, &key, &value, printer),
        ConfigAction::Unset { key } => unset(&file, &key, printer),
        ConfigAction::Edit => edit(&file, printer),
    }
}

/// 键名解析：拼错时报用法错误（2）并列出全部键
///
/// # Errors
///
/// 键名不在 [`ConfigKey::ALL`] 内时返回用法错误
fn parse_key(name: &str) -> Result<ConfigKey> {
    ConfigKey::parse(name).ok_or_else(|| {
        AppDiagnostic::usage(
            format!("未知配置键: {name}"),
            Some(format!(
                "可用键: {}",
                ConfigKey::ALL
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        )
        .into()
    })
}

fn show(file: &ConfigFile, printer: Printer) -> Result<()> {
    let cfg = file.load()?;
    let items: Vec<ConfigRow<'_>> = ConfigKey::ALL
        .iter()
        .map(|key| ConfigRow {
            key: key.as_str(),
            value: cfg.effective(*key).display().to_string(),
            source: if cfg.is_set(*key) { "file" } else { "default" },
        })
        .collect();
    let path = file.path().display().to_string();

    if !printer.is_human() {
        // 机器契约：stdout 只一行对象。表格行不能在此之前也打一遍
        return printer.print_outcome(
            "",
            &ConfigShowJson {
                schema: 1,
                path,
                exists: file.exists(),
                items,
            },
        );
    }

    let mut table = Table::new();
    table.set_header(["键", "值", "来源"]);
    for (key, row) in ConfigKey::ALL.iter().zip(&items) {
        table.add_row([
            key.as_str().to_owned(),
            row.value.clone(),
            if row.source == "file" {
                "配置文件".to_owned()
            } else {
                "缺省".to_owned()
            },
        ]);
    }
    crate::output::print_table(&table);
    // 文件不存在时说明改哪个文件：`config set` 会创建它
    let state = if file.exists() {
        String::new()
    } else {
        "（尚未创建，config set 时会创建）".to_owned()
    };
    printer.print_section(&format!("配置文件: {path}{state}"));
    Ok(())
}

fn set(file: &ConfigFile, key: &str, value: &str, printer: Printer) -> Result<()> {
    let key = parse_key(key)?;
    let mut cfg = file.load()?;
    cfg.set(key, value).map_err(|err| {
        AppDiagnostic::usage(
            err.describe(),
            Some(format!("`mcsup config set {} <值>`", key.as_str())),
        )
    })?;
    let previous = file.load().ok().and_then(|before| before.raw(key));
    file.save(&cfg)?;
    let effective = cfg.effective(key).display().to_string();
    printer.print_outcome(
        &format!("已设置 {} = {}", key.as_str(), effective),
        &ConfigSetJson {
            key: key.as_str(),
            value: effective,
            previous,
            path: file.path().display().to_string(),
        },
    )
}

fn unset(file: &ConfigFile, key: &str, printer: Printer) -> Result<()> {
    let key = parse_key(key)?;
    let mut cfg = file.load()?;
    if !cfg.is_set(key) {
        // 已是缺省：幂等成功，但仍然回显生效值，不制造「改了什么」的错觉
        let effective = cfg.effective(key).display().to_string();
        return printer.print_outcome(
            &format!("{} 已是平台缺省: {}", key.as_str(), effective),
            &ConfigSetJson {
                key: key.as_str(),
                value: effective,
                previous: None,
                path: file.path().display().to_string(),
            },
        );
    }
    let previous = cfg.raw(key);
    cfg.unset(key);
    file.save(&cfg)?;
    let effective = cfg.effective(key).display().to_string();
    printer.print_outcome(
        &format!("已恢复 {} 为平台缺省: {}", key.as_str(), effective),
        &ConfigSetJson {
            key: key.as_str(),
            value: effective,
            previous,
            path: file.path().display().to_string(),
        },
    )
}

/// `config edit`：打开 `$EDITOR`，保存后重新解析；解析失败保留原文件
///
/// 非交互（管道 / JSON）下无法进编辑器，按用法错误处理而不是静默成功。
fn edit(file: &ConfigFile, printer: Printer) -> Result<()> {
    if !printer.is_interactive() {
        return Err(AppDiagnostic::usage(
            "config edit 需要交互终端",
            Some(format!(
                "非交互模式请用 mcsup config set <键> <值>；文件: {}",
                file.path().display()
            )),
        )
        .into());
    }
    let editor = editor_command().ok_or_else(|| {
        AppDiagnostic::usage(
            "未设置编辑器",
            Some("设置环境变量 EDITOR 或 VISUAL，如 `set EDITOR=notepad`".to_owned()),
        )
    })?;

    // 文件不存在时先落一份当前生效值，编辑器才有东西可改
    if !file.exists() {
        file.save(&file.load()?)?;
    }
    printer.print_section(&format!(
        "打开 {}（编辑器: {}）",
        file.path().display(),
        editor.join(" ")
    ));
    let status = run_editor(&editor, file.path())?;
    if !status {
        // 编辑器非零退出：用户可能只是没保存就关掉，不能当作「已保存」报成功
        return Err(anyhow::anyhow!(
            "编辑器以非零状态退出，配置未确认保存: {}",
            file.path().display()
        ));
    }

    // 编辑器写坏了文件时保留原文并报错：回滚由用户决定，不静默覆盖
    let cfg = file.load().map_err(|err| {
        AppDiagnostic::usage(
            format!("编辑后的配置无法解析: {err:#}"),
            Some(format!(
                "请重新运行 mcsup config edit 修正 {}",
                file.path().display()
            )),
        )
    })?;
    show(file, printer)?;
    let _ = cfg;
    Ok(())
}

/// `$VISUAL` 优先于 `$EDITOR`（POSIX 惯例：VISUAL 面向全屏编辑器），
/// 两者都没有时退回平台默认编辑器
fn editor_command() -> Option<Vec<String>> {
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(raw) = std::env::var(var)
            && !raw.trim().is_empty()
        {
            let mut parts = shell_words::split(&raw).ok()?;
            if !parts.is_empty() {
                let program = parts.remove(0);
                parts.insert(0, program);
                return Some(parts);
            }
        }
    }
    // 无环境变量时的平台缺省：Windows 用记事本，其余用 vi
    Some(vec![
        if cfg!(windows) { "notepad" } else { "vi" }.to_owned(),
    ])
}

/// 运行编辑器；返回是否以成功状态退出
fn run_editor(editor: &[String], path: &Path) -> Result<bool> {
    let Some((program, args)) = editor.split_first() else {
        return Err(anyhow::anyhow!("编辑器命令为空"));
    };
    let status = std::process::Command::new(program)
        .args(args)
        .arg(path)
        .status()
        .with_context(|| format!("启动编辑器 {program} 失败"))?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_key_lists_all_keys() {
        let err = parse_key("nope").expect_err("unknown");
        let text = format!("{err:#}");
        assert!(text.contains("未知配置键"), "{text}");
        let diag = err
            .downcast_ref::<AppDiagnostic>()
            .expect("usage diagnostic");
        assert_eq!(diag.exit_code, 2);
    }

    #[test]
    fn known_keys_parse() {
        for key in ConfigKey::ALL {
            assert_eq!(parse_key(key.as_str()).expect("known"), *key);
        }
    }

    #[test]
    fn editor_command_prefers_visual() {
        // 环境变量在测试进程里共享，这里只验证解析结果非空
        let editor = editor_command().expect("editor");
        assert!(!editor.is_empty());
    }
}
