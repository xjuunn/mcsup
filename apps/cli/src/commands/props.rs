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
use mcsup::domain::server::props::Properties;
use mcsup::protocol::cli::{PropRow, PropsWriteJson};
use mcsup::service::ServerPropsService;

/// 属性文件的一份快照
///
/// 本地与远端都归一到同一形态：路径用于回显，`Properties` 供交互与渲染。
struct PropsSnapshot {
    path: String,
    properties: Properties,
}

/// 属性读写后端：本地走 service，远端走帧
///
/// 远端帧传原文、两端各自解析：注释与顺序属于用户文件内容，键值表无法
/// 无损表达。入口只关心「读到了什么、写成功没有」，不感知传输。
enum Backend {
    /// 本机：service 直接读写磁盘
    Local(ServerPropsService),
    /// 远端：文件在对端，只能经帧读写
    Remote {
        member: mcsup::backend::remote::RemoteMember,
        server_id: String,
    },
}

impl Backend {
    /// 按名字选后端：来源判定统一走 [`super::origin::remote_target`]
    fn open(name: &str, lang: crate::locale::Lang) -> Result<Self> {
        if let Some(target) = super::origin::remote_target(name, lang)? {
            return Ok(Self::Remote {
                member: target.member,
                server_id: target.server_id,
            });
        }
        Ok(Self::Local(ServerPropsService::new(
            bootstrap::open_registry()?,
        )))
    }

    /// 读当前内容
    fn read(&self, name: &str) -> Result<PropsSnapshot> {
        match self {
            Self::Local(service) => {
                let report = service.read(name)?;
                Ok(PropsSnapshot {
                    path: report.path.display().to_string(),
                    properties: report.properties,
                })
            }
            Self::Remote { member, server_id } => {
                let (path, text) = member.servers.props(server_id)?;
                Ok(PropsSnapshot {
                    path,
                    properties: Properties::parse(&text),
                })
            }
        }
    }

    /// 写入若干键值，返回写入后的快照
    fn set(&self, name: &str, updates: &[(String, String)]) -> Result<PropsSnapshot> {
        match self {
            Self::Local(service) => {
                let report = service.set(name, updates)?;
                Ok(PropsSnapshot {
                    path: report.path.display().to_string(),
                    properties: report.properties,
                })
            }
            Self::Remote { member, server_id } => {
                let (path, text) = member.servers.set_props(server_id, updates)?;
                Ok(PropsSnapshot {
                    path,
                    properties: Properties::parse(&text),
                })
            }
        }
    }
}

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
    let backend = Backend::open(name, printer.lang())?;
    match classify(kv, printer.lang())? {
        Args::List => list(&backend, name, printer),
        Args::KeyOnly(key) => set_key_only(&backend, name, key, printer),
        Args::Direct(updates) => write(&backend, name, &updates, printer),
    }
}

/// 位置参数分类：全 `key=value` → 直接写入；单个裸 `key` → 缺值交互；
/// 其余组合（如两个裸 key）一律用法错误
fn classify(kv: &[String], lang: crate::locale::Lang) -> Result<Args> {
    match kv {
        [] => Ok(Args::List),
        [single] if !single.contains('=') => Ok(Args::KeyOnly(single.clone())),
        _ => {
            let mut updates = Vec::with_capacity(kv.len());
            for item in kv {
                let (key, value) = item.split_once('=').ok_or_else(|| {
                    AppDiagnostic::usage(
                        crate::i18n::pick(
                            lang,
                            format!("参数 {item} 不是 key=value 形式"),
                            format!("argument {item} is not in key=value form"),
                        ),
                        Some(crate::i18n::pick(
                            lang,
                            "用法：mcsup server props <name> [key=value ...]；只给 key 时单独传一个参数",
                            "usage: mcsup server props <name> [key=value ...]; pass a single argument when only giving a key",
                        )),
                    )
                })?;
                updates.push((key.to_owned(), value.to_owned()));
            }
            Ok(Args::Direct(updates))
        }
    }
}

/// 列出全部键值；TTY 下进入交互修改循环
fn list(backend: &Backend, name: &str, printer: Printer) -> Result<()> {
    let report = backend.read(name)?;
    print_props(&report, printer)?;
    if !printer.is_interactive() {
        return Ok(());
    }
    edit_loop(backend, name, printer)
}

/// 交互循环：选键 → 输入新值（回车保持当前值）→ 落盘；Esc / Ctrl+C 退出
fn edit_loop(backend: &Backend, name: &str, printer: Printer) -> Result<()> {
    loop {
        // 每轮重读：默认值要反映上一轮写入后的真实文件
        let report = backend.read(name)?;
        if report.properties.is_empty() {
            printer.print_section(&crate::i18n::pick(
                printer.lang(),
                "尚无可修改项，先启动一次服务器生成默认配置",
                "nothing to edit yet; start the server once so it writes the default config",
            ));
            return Ok(());
        }
        let keys: Vec<String> = report
            .properties
            .entries()
            .map(|(key, _)| key.to_owned())
            .collect();
        let key = match prompts::select_filter(
            printer.lang(),
            &crate::i18n::pick(
                printer.lang(),
                "要修改的项（Esc 退出）",
                "property to edit (Esc to quit)",
            ),
            &keys,
        ) {
            Ok(key) => key,
            Err(err) if prompts::is_cancelled(&err) => return Ok(()),
            Err(err) => return Err(err),
        };
        let current = report.properties.get(&key).unwrap_or_default();
        let value = match prompts::input_text(
            printer.lang(),
            &key,
            current,
            &crate::i18n::pick(
                printer.lang(),
                "回车保持当前值",
                "enter keeps the current value",
            ),
        ) {
            Ok(value) => value,
            Err(err) if prompts::is_cancelled(&err) => return Ok(()),
            Err(err) => return Err(err),
        };
        if value == current {
            printer.print_section(&crate::i18n::pick(
                printer.lang(),
                format!("{key} 未变化，跳过"),
                format!("{key} unchanged; skipping"),
            ));
            continue;
        }
        write(backend, name, &[(key.clone(), value)], printer)?;
    }
}

/// 单个 `key` 缺值：交互输入；非交互报用法错误
fn set_key_only(backend: &Backend, name: &str, key: String, printer: Printer) -> Result<()> {
    let report = backend.read(name)?;
    if !printer.is_interactive() {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                printer.lang(),
                format!("server props 缺少 {key} 的值"),
                format!("server props requires a value for {key}"),
            ),
            Some(crate::i18n::pick(
                printer.lang(),
                format!("非交互模式：mcsup server props {name} {key}=<value>"),
                format!("non-interactive: mcsup server props {name} {key}=<value>"),
            )),
        )
        .into());
    }
    let current = report.properties.get(&key).unwrap_or_default();
    let value = prompts::input_text(
        printer.lang(),
        &key,
        current,
        &crate::i18n::pick(
            printer.lang(),
            "回车保持当前值",
            "enter keeps the current value",
        ),
    )?;
    if value == current {
        printer.print_section(&crate::i18n::pick(
            printer.lang(),
            format!("{key} 未变化，未写入"),
            format!("{key} unchanged; nothing written"),
        ));
        return Ok(());
    }
    write(backend, name, &[(key, value)], printer)
}

/// 写入并报告（human 一行 + 重启提醒；JSON 单对象）
fn write(
    backend: &Backend,
    name: &str,
    updates: &[(String, String)],
    printer: Printer,
) -> Result<()> {
    let report = backend.set(name, updates).with_context(|| {
        crate::i18n::pick(
            printer.lang(),
            format!("写入 {name} 的 server.properties 失败"),
            format!("failed to write {name}'s server.properties"),
        )
    })?;
    let summary = updates
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut updated_map = std::collections::BTreeMap::new();
    for (key, value) in updates {
        updated_map.insert(key.clone(), value.clone());
    }
    let payload = PropsWriteJson {
        name: name.to_owned(),
        path: report.path,
        updated: updated_map,
    };
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("已写入 {summary}（重启生效）"),
            format!("wrote {summary} (takes effect after a restart)"),
        ),
        &payload,
    )?;
    Ok(())
}

/// 列表渲染：human 表格（键值按文件顺序），JSON 键值数组
///
/// # Errors
///
/// 序列化结果失败时返回底层错误
fn print_props(report: &PropsSnapshot, printer: Printer) -> Result<()> {
    let rows: Vec<PropRow<'_>> = report
        .properties
        .entries()
        .map(|(key, value)| PropRow { key, value })
        .collect();
    printer.print_rows(
        &rows,
        &crate::i18n::pick(
            printer.lang(),
            "尚无 server.properties（服务器首次启动后由服务端生成）",
            "no server.properties yet (the server writes it on first start)",
        ),
        || {
            let mut table = Table::new();
            table.set_header([
                crate::i18n::text(printer.lang(), "键", "key"),
                crate::i18n::text(printer.lang(), "值", "value"),
            ]);
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
        assert_eq!(
            classify(&[], crate::locale::Lang::Zh).expect("list"),
            Args::List
        );
    }

    #[test]
    fn classify_single_bare_key_is_key_only() {
        assert_eq!(
            classify(&args(&["motd"]), crate::locale::Lang::Zh).expect("key only"),
            Args::KeyOnly("motd".to_owned())
        );
    }

    #[test]
    fn classify_key_value_pairs_are_direct() {
        let parsed =
            classify(&args(&["motd=hello", "pvp=true"]), crate::locale::Lang::Zh).expect("direct");
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
            classify(&args(&["motd="]), crate::locale::Lang::Zh).expect("empty value"),
            Args::Direct(vec![("motd".to_owned(), String::new())])
        );
    }

    /// 两个裸 key 组合形态无法确定意图 → 用法错误（2）
    #[test]
    fn classify_rejects_mixed_bare_keys() {
        let err =
            classify(&args(&["motd", "pvp"]), crate::locale::Lang::Zh).expect_err("ambiguous");
        assert_eq!(Printer::exit_code(&err), 2);
    }
}
