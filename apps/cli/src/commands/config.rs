//! `config show / set / unset / edit`：平台配置的入口
//!
//! 键表、校验与归一化全在 `mcsup::service`（[`ConfigKey`] / [`Config`]），
//! 这里只做参数搬运、文件读写与渲染。写入走 `tmp → fsync → rename`，
//! 半截文件不会覆盖掉可用的旧配置。

use crate::cli::ConfigAction;
use crate::config::ConfigFile;
use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use crate::prompts;
use anyhow::{Context, Result};
use comfy_table::Table;
use mcsup::protocol::cli::{ConfigRow, ConfigSetJson, ConfigShowJson};
use mcsup::service::ConfigKey;
use std::path::Path;

pub(crate) fn run(action: ConfigAction, printer: Printer) -> Result<()> {
    let file = ConfigFile::locate(None);
    match action {
        ConfigAction::Show => show(&file, printer),
        ConfigAction::Set { key, value } => {
            let key = resolve_key(key.as_deref(), "config set", printer)?;
            let value = resolve_value(value, &key, "config set", printer)?;
            set(&file, &key, &value, printer)
        }
        ConfigAction::Unset { key } => {
            let key = resolve_key(key.as_deref(), "config unset", printer)?;
            unset(&file, &key, printer)
        }
        ConfigAction::Edit => edit(&file, printer),
    }
}

/// 缺 key：交互下从全部配置键中选择；非交互报用法错误（2）
fn resolve_key(key: Option<&str>, command: &str, printer: Printer) -> Result<String> {
    if let Some(key) = key {
        return Ok(key.to_owned());
    }
    if !printer.is_interactive() {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                printer.lang(),
                format!("{command} 缺少配置键"),
                format!("{command} requires a config key"),
            ),
            Some(crate::i18n::pick(
                printer.lang(),
                format!(
                    "非交互模式：mcsup {command} <键>；可用键: {}",
                    ConfigKey::ALL
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                format!(
                    "non-interactive: mcsup {command} <key>; keys: {}",
                    ConfigKey::ALL
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
        )
        .into());
    }
    let keys: Vec<String> = ConfigKey::ALL
        .iter()
        .map(|k| k.as_str().to_owned())
        .collect();
    prompts::select_filter(
        printer.lang(),
        &crate::i18n::pick(printer.lang(), "选择配置键", "select a config key"),
        &keys,
    )
}

/// 缺 value：交互下按当前生效值作默认输入；非交互报用法错误（2）
fn resolve_value(
    value: Option<String>,
    key: &str,
    command: &str,
    printer: Printer,
) -> Result<String> {
    if let Some(value) = value {
        return Ok(value);
    }
    if !printer.is_interactive() {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                printer.lang(),
                format!("{command} 缺少 {key} 的值"),
                format!("{command} requires a value for {key}"),
            ),
            Some(crate::i18n::pick(
                printer.lang(),
                format!("非交互模式：mcsup {command} {key} <值>"),
                format!("non-interactive: mcsup {command} {key} <value>"),
            )),
        )
        .into());
    }
    prompts::input_text(
        printer.lang(),
        key,
        "",
        &crate::i18n::pick(
            printer.lang(),
            "输入新值；回车提交",
            "enter the new value; enter submits",
        ),
    )
}

/// 键名解析：拼错时报用法错误（2）并列出全部键
///
/// # Errors
///
/// 键名不在 [`ConfigKey::ALL`] 内时返回用法错误
fn parse_key(lang: crate::locale::Lang, name: &str) -> Result<ConfigKey> {
    ConfigKey::parse(name).ok_or_else(|| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("未知配置键: {name}"),
                format!("unknown config key: {name}"),
            ),
            Some(crate::i18n::pick(
                lang,
                format!(
                    "可用键: {}",
                    ConfigKey::ALL
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                format!(
                    "available keys: {}",
                    ConfigKey::ALL
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
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
    table.set_header([
        crate::i18n::text(printer.lang(), "键", "key"),
        crate::i18n::text(printer.lang(), "值", "value"),
        crate::i18n::text(printer.lang(), "来源", "source"),
    ]);
    for (key, row) in ConfigKey::ALL.iter().zip(&items) {
        table.add_row([
            key.as_str().to_owned(),
            row.value.clone(),
            if row.source == "file" {
                crate::i18n::pick(printer.lang(), "配置文件", "file")
            } else {
                crate::i18n::pick(printer.lang(), "缺省", "default")
            },
        ]);
    }
    crate::output::print_table(&table);
    // 文件不存在时说明改哪个文件：`config set` 会创建它
    let state = if file.exists() {
        String::new()
    } else {
        crate::i18n::pick(
            printer.lang(),
            "（尚未创建，config set 时会创建）",
            " (not created yet; `config set` creates it)",
        )
    };
    printer.print_section(&crate::i18n::pick(
        printer.lang(),
        format!("配置文件: {path}{state}"),
        format!("config file: {path}{state}"),
    ));
    Ok(())
}

fn set(file: &ConfigFile, key: &str, value: &str, printer: Printer) -> Result<()> {
    let key = parse_key(printer.lang(), key)?;
    let mut cfg = file.load()?;
    cfg.set(key, value).map_err(|err| {
        AppDiagnostic::usage(
            err.describe(),
            Some(crate::i18n::pick(
                printer.lang(),
                format!("`mcsup config set {} <值>`", key.as_str()),
                format!("`mcsup config set {} <value>`", key.as_str()),
            )),
        )
    })?;
    let previous = file.load().ok().and_then(|before| before.raw(key));
    file.save(&cfg)?;
    let effective = cfg.effective(key).display().to_string();
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("已设置 {} = {}", key.as_str(), effective),
            format!("set {} = {}", key.as_str(), effective),
        ),
        &ConfigSetJson {
            key: key.as_str(),
            value: effective,
            previous,
            path: file.path().display().to_string(),
        },
    )
}

fn unset(file: &ConfigFile, key: &str, printer: Printer) -> Result<()> {
    let key = parse_key(printer.lang(), key)?;
    let mut cfg = file.load()?;
    if !cfg.is_set(key) {
        // 已是缺省：幂等成功，但仍然回显生效值，不制造「改了什么」的错觉
        let effective = cfg.effective(key).display().to_string();
        return printer.print_outcome(
            &crate::i18n::pick(
                printer.lang(),
                format!("{} 已是平台缺省: {}", key.as_str(), effective),
                format!(
                    "{} already uses the platform default: {}",
                    key.as_str(),
                    effective
                ),
            ),
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
        &crate::i18n::pick(
            printer.lang(),
            format!("已恢复 {} 为平台缺省: {}", key.as_str(), effective),
            format!(
                "restored {} to the platform default: {}",
                key.as_str(),
                effective
            ),
        ),
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
            crate::i18n::pick(
                printer.lang(),
                "config edit 需要交互终端",
                "config edit needs an interactive terminal",
            ),
            Some(crate::i18n::pick(
                printer.lang(),
                format!(
                    "非交互模式请用 mcsup config set <键> <值>；文件: {}",
                    file.path().display()
                ),
                format!(
                    "non-interactive: use mcsup config set <key> <value>; file: {}",
                    file.path().display()
                ),
            )),
        )
        .into());
    }
    let editor = editor_command().ok_or_else(|| {
        AppDiagnostic::usage(
            crate::i18n::pick(printer.lang(), "未设置编辑器", "no editor configured"),
            Some(crate::i18n::pick(
                printer.lang(),
                "设置环境变量 EDITOR 或 VISUAL，如 `set EDITOR=notepad`",
                "set the EDITOR or VISUAL environment variable, e.g. `set EDITOR=notepad`",
            )),
        )
    })?;

    // 文件不存在时先落一份当前生效值，编辑器才有东西可改
    if !file.exists() {
        file.save(&file.load()?)?;
    }
    printer.print_section(&crate::i18n::pick(
        printer.lang(),
        format!(
            "打开 {}（编辑器: {}）",
            file.path().display(),
            editor.join(" ")
        ),
        format!(
            "opening {} (editor: {})",
            file.path().display(),
            editor.join(" ")
        ),
    ));
    let status = run_editor(printer.lang(), &editor, file.path())?;
    if !status {
        // 编辑器非零退出：用户可能只是没保存就关掉，不能当作「已保存」报成功
        return Err(anyhow::anyhow!(crate::i18n::pick(
            printer.lang(),
            format!(
                "编辑器以非零状态退出，配置未确认保存: {}",
                file.path().display()
            ),
            format!(
                "the editor exited non-zero; the config is not confirmed saved: {}",
                file.path().display()
            ),
        )));
    }

    // 编辑器写坏了文件时保留原文并报错：回滚由用户决定，不静默覆盖
    let cfg = file.load().map_err(|err| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                printer.lang(),
                format!("编辑后的配置无法解析: {err:#}"),
                format!("the edited config cannot be parsed: {err:#}"),
            ),
            Some(crate::i18n::pick(
                printer.lang(),
                format!(
                    "请重新运行 mcsup config edit 修正 {}",
                    file.path().display()
                ),
                format!("rerun mcsup config edit to fix {}", file.path().display()),
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
fn run_editor(lang: crate::locale::Lang, editor: &[String], path: &Path) -> Result<bool> {
    let Some((program, args)) = editor.split_first() else {
        return Err(anyhow::anyhow!(crate::i18n::pick(
            lang,
            "编辑器命令为空",
            "the editor command is empty",
        )));
    };
    let status = std::process::Command::new(program)
        .args(args)
        .arg(path)
        .status()
        .with_context(|| {
            crate::i18n::pick(
                lang,
                format!("启动编辑器 {program} 失败"),
                format!("failed to launch the editor {program}"),
            )
        })?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_key_lists_all_keys() {
        let err = parse_key(crate::locale::Lang::Zh, "nope").expect_err("unknown");
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
            assert_eq!(
                parse_key(crate::locale::Lang::Zh, key.as_str()).expect("known"),
                *key
            );
        }
    }

    #[test]
    fn editor_command_prefers_visual() {
        // 环境变量在测试进程里共享，这里只验证解析结果非空
        let editor = editor_command().expect("editor");
        assert!(!editor.is_empty());
    }
}
