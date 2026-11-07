//! 输出层：人类表格、JSON 数据行、错误渲染
//!
//! 分三块，边界是「谁改它」：
//!
//! - 本文件（[`Printer`]）：输出格式的选择，命令只与它打交道；
//! - [`render`]：往 stdout / stderr 写字节的原语，不认识任何业务类型；
//! - [`error`]：错误分级与本地化文案，是新增失败类型时**唯一**要改的地方。
//!
//! 分级与文案分开的原因见 [`error`] 的模块注释：曾经两者各有一张表，
//! 新增一种失败时要同时改两处。

mod error;
mod render;

pub(crate) use error::{exit_code_of, structured_message};
pub(crate) use render::{
    compact_table, human_bytes, print_error_line, print_json_line, print_styled, print_success,
    print_table, render_fields,
};

use crate::locale::Lang;
use comfy_table::{Cell, Table};
use mcsup::domain::server::version::VersionId;
use owo_colors::{OwoColorize, Stream};
use serde::Serialize;
use std::fmt::{self};
use std::io::IsTerminal;
use std::path::Path;

/// 输出格式，方便人类阅读
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum OutputFormat {
    #[default]
    Human,
    Json,
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Human => "human",
            Self::Json => "json",
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Printer {
    format: OutputFormat,
    lang: Lang,
    /// 全局 `--yes`：确认式提问一律按「是」处理
    yes: bool,
}

impl Printer {
    #[must_use]
    pub(crate) const fn new(format: OutputFormat, lang: Lang) -> Self {
        Self {
            format,
            lang,
            yes: false,
        }
    }

    /// 打开全局 `--yes`：只影响确认式提问，不影响补参交互
    #[must_use]
    pub(crate) const fn with_yes(mut self, yes: bool) -> Self {
        self.yes = yes;
        self
    }

    /// 是否已用 `--yes` 跳过确认
    #[must_use]
    pub(crate) const fn assumes_yes(self) -> bool {
        self.yes
    }

    /// 界面语言：命令自行拼提示文本时用
    #[must_use]
    pub(crate) const fn lang(self) -> Lang {
        self.lang
    }

    /// 一段粗体标题，仅人类模式，JSON 模式不输出非数据行
    pub(crate) fn print_section(self, text: &str) {
        if self.format == OutputFormat::Human {
            print_styled(text);
        }
    }

    /// 直接打印一张已构建的表格，仅人类模式
    ///
    /// JSON 模式下不输出任何内容：调用方负责在 JSON 分支自行序列化。
    pub(crate) fn print_table(self, table: &Table) {
        if self.format == OutputFormat::Human {
            print_table(table);
        }
    }

    /// 一行红色警示，仅人类模式：不可恢复的危险操作前的最后提示
    #[allow(clippy::print_stdout)]
    pub(crate) fn print_warning(self, text: &str) {
        if self.format == OutputFormat::Human {
            println!(
                "{}",
                text.if_supports_color(Stream::Stdout, OwoColorize::red)
            );
        }
    }

    /// 当前是否可提问：JSON 输出下交互会卡住脚本，管道输入下提问必然失败
    ///
    /// 所有 inquire 调用前统一问这一个口径，禁止在命令里各自判 TTY。
    #[must_use]
    pub(crate) fn is_interactive(self) -> bool {
        self.is_human() && std::io::stdin().is_terminal()
    }

    /// 列表输出：人类打表格，JSON 打对象数组（空列表输出 `[]`）
    ///
    /// # Errors
    ///
    /// 序列化结果失败时返回底层错误
    pub(crate) fn print_rows<T: Serialize>(
        self,
        rows: &[T],
        empty_human: &str,
        build_table: impl FnOnce() -> Table,
    ) -> anyhow::Result<()> {
        match self.format {
            OutputFormat::Human => {
                if rows.is_empty() {
                    print_styled(empty_human);
                } else {
                    print_table(&build_table());
                }
                Ok(())
            }
            OutputFormat::Json => {
                let line = serde_json::to_string(rows)?;
                print_json_line(&line);
                Ok(())
            }
        }
    }

    /// 带上下文的列表：人类打表格，JSON 打**一个对象**
    ///
    /// 与 [`Self::print_rows`] 的分工：那个只输出条目数组，适合「结果就是这一
    /// 批行」的命令；这里输出的是带 `server` / `skipped` 等上下文的完整载荷，
    /// 即使条目为空也是同一个对象——否则同一命令的 JSON 形状会随内容变化，
    /// 脚本就没法稳定解析。
    ///
    /// # Errors
    ///
    /// 序列化结果失败时返回底层错误
    pub(crate) fn print_table_or_payload<T: Serialize>(
        self,
        rows: &[T],
        empty_human: &str,
        build_table: impl FnOnce() -> Table,
        payload: &impl Serialize,
    ) -> anyhow::Result<()> {
        match self.format {
            OutputFormat::Human => {
                if rows.is_empty() {
                    print_styled(empty_human);
                } else {
                    print_table(&build_table());
                }
                Ok(())
            }
            OutputFormat::Json => {
                let line = serde_json::to_string(payload)?;
                print_json_line(&line);
                Ok(())
            }
        }
    }

    /// 单条结果：人类打标题行，JSON 打一个对象
    ///
    /// # Errors
    ///
    /// 序列化结果失败时返回底层错误
    pub(crate) fn print_outcome<T: Serialize>(
        self,
        human: &str,
        payload: &T,
    ) -> anyhow::Result<()> {
        match self.format {
            OutputFormat::Human => {
                print_styled(human);
                Ok(())
            }
            OutputFormat::Json => {
                let line = serde_json::to_string(payload)?;
                print_json_line(&line);
                Ok(())
            }
        }
    }

    /// 安装成功结果： JSON 模式输出单行数据对象
    ///
    /// 行类型来自 [`mcsup::protocol::cli::InstalledJson`]（唯一契约来源）。
    ///
    /// # Errors
    ///
    /// 序列化结果失败时返回底层错误
    pub(crate) fn print_installed(
        self,
        version: &VersionId,
        size: u64,
        destination: &Path,
    ) -> anyhow::Result<()> {
        match self.format {
            OutputFormat::Human => {
                let mut table = Table::new();
                table.set_header([
                    crate::i18n::pick(self.lang, "项", "field"),
                    crate::i18n::pick(self.lang, "值", "value"),
                ]);
                table.add_row([
                    Cell::new(crate::i18n::pick(self.lang, "版本", "version")),
                    Cell::new(version.to_string()),
                ]);
                table.add_row([
                    Cell::new(crate::i18n::pick(self.lang, "文件", "file")),
                    Cell::new(destination.display().to_string()),
                ]);
                table.add_row([
                    Cell::new(crate::i18n::pick(self.lang, "大小", "size")),
                    Cell::new(human_bytes(size)),
                ]);
                print_table(&table);
                print_success(&crate::i18n::pick(
                    self.lang,
                    format!("安装完成: {}", destination.display()),
                    format!("install finished: {}", destination.display()),
                ));
                Ok(())
            }
            OutputFormat::Json => {
                let payload = mcsup::protocol::cli::InstalledJson {
                    version: version.to_string(),
                    path: destination.display().to_string(),
                    size,
                };
                let line = serde_json::to_string(&payload)?;
                print_json_line(&line);
                Ok(())
            }
        }
    }

    #[must_use]
    pub(crate) const fn is_human(self) -> bool {
        matches!(self.format, OutputFormat::Human)
    }

    /// 渲染错误到 stderr
    pub(crate) fn render_error(self, err: anyhow::Error) {
        let code = exit_code_of(&err);
        // 130 只有「用户取消」一条来路：用户自己按的 Ctrl-C 不是错误，
        // 再打一行红字只是噪音
        if code == 130 {
            return;
        }
        if let Some(message) = structured_message(&err, self.lang) {
            print_error_line(&format!("mcsup: {message}"));
            return;
        }
        match err.downcast::<crate::diagnostic::AppDiagnostic>() {
            Ok(diag) => {
                use miette::Diagnostic as _;
                // miette 的 Display 不携带 help，用法错误必须把「怎么改」打出来
                let help = diag.help().map(|h| h.to_string());
                let mut text = format!("{}", miette::Report::new(diag));
                if let Some(help) = help {
                    text.push('\n');
                    text.push_str(&help);
                }
                print_error_line(&text);
            }
            Err(err) => print_error_line(&format!("mcsup: {err:#}")),
        }
    }
}

impl Default for Printer {
    fn default() -> Self {
        Self::new(OutputFormat::Human, Lang::Zh)
    }
}
