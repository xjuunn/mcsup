use crate::diagnostic::AppDiagnostic;
use comfy_table::{Cell, Table};
use domain::server::version::VersionId;
use owo_colors::OwoColorize;
use port::DownloadedArtifact;
use serde::Serialize;
use std::fmt::{self};
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

/// 渲染器
#[derive(Debug, Clone, Copy)]
pub(crate) struct Printer {
    format: OutputFormat,
}

impl Printer {
    #[must_use]
    pub(crate) const fn new(format: OutputFormat) -> Self {
        Self { format }
    }

    /// 一段粗体标题，仅人类模式，JSON 模式不输出非数据行
    pub(crate) fn print_section(self, text: &str) {
        if self.format == OutputFormat::Human {
            print_styled(text);
        }
    }

    /// 安装成功结果： JSON 模式输出单行数据对象
    ///
    /// # Errors
    ///
    /// 序列化结果失败时返回底层错误
    pub(crate) fn print_installed(
        self,
        version: &VersionId,
        artifact: &DownloadedArtifact,
        destination: &Path,
    ) -> anyhow::Result<()> {
        match self.format {
            OutputFormat::Human => {
                let mut table = Table::new();
                table.set_header(["项", "值"]);
                table.add_row([Cell::new("版本"), Cell::new(version.to_string())]);
                table.add_row([
                    Cell::new("文件"),
                    Cell::new(destination.display().to_string()),
                ]);
                table.add_row([Cell::new("大小"), Cell::new(human_bytes(artifact.size))]);
                print_table(&table);
                print_success(&format!("安装完成: {}", destination.display()));
                Ok(())
            }
            OutputFormat::Json => {
                let payload = InstalledJson {
                    version: version.to_string(),
                    path: destination.display().to_string(),
                    size: artifact.size,
                };
                let line = serde_json::to_string(&payload)?;
                print_json_line(&line);
                Ok(())
            }
        }
    }

    /// 错误退出码 0成功 / 1通用 / 2用法 / 3未找到 / 4冲突 / 5后端失败
    pub(crate) fn exit_code(err: &anyhow::Error) -> u8 {
        if let Some(diag) = err.downcast_ref::<AppDiagnostic>() {
            return diag.exit_code;
        }
        for cause in err.chain() {
            if let Some(install) = cause.downcast_ref::<service::InstallError>() {
                return match install {
                    service::InstallError::Source(source) => source_exit_code(source),
                    service::InstallError::Download(_) => 5,
                    service::InstallError::Status(_) => 4,
                    _ => 1,
                };
            }
            if let Some(source) = cause.downcast_ref::<port::SourceError>() {
                return source_exit_code(source);
            }
            if cause.downcast_ref::<port::DownloadError>().is_some() {
                return 5;
            }
        }
        1
    }

    /// 用 miette 渲染错误到 stderr
    pub(crate) fn render_error(err: anyhow::Error) {
        match err.downcast::<AppDiagnostic>() {
            Ok(diag) => print_error_line(&format!("{}", miette::Report::new(diag))),
            Err(err) => print_error_line(&format!("mcsup: {err:#}")),
        }
    }

    #[must_use]
    pub(crate) const fn is_human(self) -> bool {
        matches!(self.format, OutputFormat::Human)
    }
}

fn source_exit_code(source: &port::SourceError) -> u8 {
    match source {
        port::SourceError::NotFound(_) => 3,
        port::SourceError::Network(_)
        | port::SourceError::Status { .. }
        | port::SourceError::Init(_) => 5,
        _ => 1,
    }
}

impl Default for Printer {
    fn default() -> Self {
        Self::new(OutputFormat::Human)
    }
}

#[derive(Debug, Serialize)]
struct InstalledJson {
    version: String,
    path: String,
    size: u64,
}

#[allow(clippy::cast_precision_loss)]
fn human_bytes(n: u64) -> String {
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    if n >= GIB {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.2} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.2} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}
#[allow(clippy::print_stdout)]
fn print_table(table: &Table) {
    println!("{table}");
}

#[allow(clippy::print_stdout)]
fn print_styled(text: &str) {
    println!("{}", text.bold());
}

#[allow(clippy::print_stdout)]
fn print_success(text: &str) {
    let mark = "+".green();
    println!("{mark} {text}");
}

#[allow(clippy::print_stdout)]
fn print_json_line(line: &str) {
    println!("{line}");
}

#[allow(clippy::print_stderr)]
fn print_error_line(text: &str) {
    eprintln!("{text}");
}

#[cfg(test)]
mod tests {
    use super::Printer;
    use anyhow::Context as _;
    use domain::server::version::VersionId;
    use port::SourceError;

    #[test]
    fn backend_failure_maps_to_exit_code_5() {
        let err = Err::<(), _>(service::InstallError::from(SourceError::Network(
            "boom".into(),
        )))
        .context("安装服务端失败")
        .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 5);
    }

    #[test]
    fn not_found_maps_to_exit_code_3() {
        let err = Err::<(), _>(SourceError::NotFound(VersionId::new("1.21.1")))
            .context("安装服务端失败")
            .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 3);
    }
}
