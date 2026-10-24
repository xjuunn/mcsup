//! `server props`：读写 `server.properties`
//!
//! 无参：列出全部键值（human 表格 / JSON 数组）；TTY 下随后进入交互，
//! 选键改值、逐项落盘。带参：`key=value` 直接写入；单独 `key` 缺值时
//! 交互输入（非交互报用法错误）。判定与 IO 在 service，这里只做参数
//! 搬运、交互补齐与结果渲染。

use crate::bootstrap;
use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use crate::prompts;
use anyhow::{Context, Result};
use comfy_table::Table;
use mcsup::protocol::cli::{PropRow, PropsWriteJson};
use mcsup::service::{PropsReport, ServerPropsService};

/// 位置参数的三种形态
#[derive(Debug, PartialEq, Eq)]
enum Args {
    /// 无参：列出（交互终端下进入修改循环）
    List,
    /// 单个 `key`（缺值）：交互输入或用法错误
    KeyOnly(String),
    /// 若干 `key=value`：直接写入
    Direct(Vec<(String, String)>),
}

/// `server props` 入口
///
/// # Errors
///
/// 服务器不存在（3）、参数形态非法（2）、键 / 值非法（2）或 IO 失败（1）时返回错误
pub(crate) fn run(name: &str, kv: &[String], printer: Printer) -> Result<()> {
    let svc = ServerPropsService::new(bootstrap::open_registry()?);
    match classify(kv)? {
        Args::List => list(&svc, name, printer),
        Args::KeyOnly(key) => set_key_only(&svc, name, key, printer),
        Args::Direct(updates) => write(&svc, name, &updates, printer),
    }
}

/// 位置参数分类：全 `key=value` → 直接写入；单个裸 `key` → 缺值交互；
/// 其余组合（如两个裸 key）一律用法错误
fn classify(kv: &[String]) -> Result<Args> {
    match kv {
        [] => Ok(Args::List),
        [single] if !single.contains('=') => Ok(Args::KeyOnly(single.clone())),
        _ => {
            let mut updates = Vec::with_capacity(kv.len());
            for item in kv {
                let (key, value) = item.split_once('=').ok_or_else(|| {
                    AppDiagnostic::usage(
                        format!("参数 {item} 不是 key=value 形式"),
                        Some("用法：mcsup server props <name> [key=value ...]；只给 key 时单独传一个参数".to_owned()),
                    )
                })?;
                updates.push((key.to_owned(), value.to_owned()));
            }
            Ok(Args::Direct(updates))
        }
    }
}

/// 列出全部键值；TTY 下进入交互修改循环
fn list(svc: &ServerPropsService, name: &str, printer: Printer) -> Result<()> {
    let report = svc.read(name)?;
    print_props(&report, printer)?;
    if !printer.is_interactive() {
        return Ok(());
    }
    edit_loop(svc, name, printer)
}

/// 交互循环：选键 → 输入新值（回车保持当前值）→ 落盘；Esc / Ctrl+C 退出
fn edit_loop(svc: &ServerPropsService, name: &str, printer: Printer) -> Result<()> {
    loop {
        // 每轮重读：默认值要反映上一轮写入后的真实文件
        let report = svc.read(name)?;
        if report.properties.is_empty() {
            printer.print_section("尚无可修改项，先启动一次服务器生成默认配置");
            return Ok(());
        }
        let keys: Vec<String> = report
            .properties
            .entries()
            .map(|(key, _)| key.to_owned())
            .collect();
        let key = match prompts::select_filter("要修改的项（Esc 退出）", &keys) {
            Ok(key) => key,
            Err(err) if prompts::is_cancelled(&err) => return Ok(()),
            Err(err) => return Err(err),
        };
        let current = report.properties.get(&key).unwrap_or_default();
        let value = match prompts::input_text(&key, current, "回车保持当前值") {
            Ok(value) => value,
            Err(err) if prompts::is_cancelled(&err) => return Ok(()),
            Err(err) => return Err(err),
        };
        if value == current {
            printer.print_section(&format!("{key} 未变化，跳过"));
            continue;
        }
        write(svc, name, &[(key.clone(), value)], printer)?;
    }
}

/// 单个 `key` 缺值：交互输入；非交互报用法错误
fn set_key_only(svc: &ServerPropsService, name: &str, key: String, printer: Printer) -> Result<()> {
    let report = svc.read(name)?;
    if !printer.is_interactive() {
        return Err(AppDiagnostic::usage(
            format!("server props 缺少 {key} 的值"),
            Some(format!(
                "非交互模式：mcsup server props {name} {key}=<value>"
            )),
        )
        .into());
    }
    let current = report.properties.get(&key).unwrap_or_default();
    let value = prompts::input_text(&key, current, "回车保持当前值")?;
    if value == current {
        printer.print_section(&format!("{key} 未变化，未写入"));
        return Ok(());
    }
    write(svc, name, &[(key, value)], printer)
}

/// 写入并报告（human 一行 + 重启提醒；JSON 单对象）
fn write(
    svc: &ServerPropsService,
    name: &str,
    updates: &[(String, String)],
    printer: Printer,
) -> Result<()> {
    let report = svc
        .set(name, updates)
        .with_context(|| format!("写入 {name} 的 server.properties 失败"))?;
    let summary = updates
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("、");
    let mut updated_map = std::collections::BTreeMap::new();
    for (key, value) in updates {
        updated_map.insert(key.clone(), value.clone());
    }
    let payload = PropsWriteJson {
        name: name.to_owned(),
        path: report.path.display().to_string(),
        updated: updated_map,
    };
    printer.print_outcome(&format!("已写入 {summary}（重启生效）"), &payload)?;
    Ok(())
}

/// 列表渲染：human 表格（键值按文件顺序），JSON 键值数组
///
/// # Errors
///
/// 序列化结果失败时返回底层错误
fn print_props(report: &PropsReport, printer: Printer) -> Result<()> {
    let rows: Vec<PropRow<'_>> = report
        .properties
        .entries()
        .map(|(key, value)| PropRow { key, value })
        .collect();
    printer.print_rows(
        &rows,
        "尚无 server.properties（服务器首次启动后由服务端生成）",
        || {
            let mut table = Table::new();
            table.set_header(["键", "值"]);
            for (key, value) in report.properties.entries() {
                table.add_row([key.to_owned(), value.to_owned()]);
            }
            table
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn classify_empty_is_list() {
        assert_eq!(classify(&[]).expect("list"), Args::List);
    }

    #[test]
    fn classify_single_bare_key_is_key_only() {
        assert_eq!(
            classify(&args(&["motd"])).expect("key only"),
            Args::KeyOnly("motd".to_owned())
        );
    }

    #[test]
    fn classify_key_value_pairs_are_direct() {
        let parsed = classify(&args(&["motd=hello", "pvp=true"])).expect("direct");
        assert_eq!(
            parsed,
            Args::Direct(vec![
                ("motd".to_owned(), "hello".to_owned()),
                ("pvp".to_owned(), "true".to_owned())
            ])
        );
    }

    /// 空值（`key=`）合法：清空该项
    #[test]
    fn classify_allows_empty_value() {
        assert_eq!(
            classify(&args(&["motd="])).expect("empty value"),
            Args::Direct(vec![("motd".to_owned(), String::new())])
        );
    }

    /// 两个裸 key 组合形态无法确定意图 → 用法错误（2）
    #[test]
    fn classify_rejects_mixed_bare_keys() {
        let err = classify(&args(&["motd", "pvp"])).expect_err("ambiguous");
        assert_eq!(Printer::exit_code(&err), 2);
    }
}
