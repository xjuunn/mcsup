use crate::diagnostic::AppDiagnostic;
use comfy_table::{Cell, Table};
use mcsup::domain::server::version::VersionId;
use owo_colors::OwoColorize;
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

    /// 一行红色警示，仅人类模式：不可恢复的危险操作前的最后提示
    #[allow(clippy::print_stdout)]
    pub(crate) fn print_warning(self, text: &str) {
        if self.format == OutputFormat::Human {
            println!("{}", text.red());
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
                table.set_header(["项", "值"]);
                table.add_row([Cell::new("版本"), Cell::new(version.to_string())]);
                table.add_row([
                    Cell::new("文件"),
                    Cell::new(destination.display().to_string()),
                ]);
                table.add_row([Cell::new("大小"), Cell::new(human_bytes(size))]);
                print_table(&table);
                print_success(&format!("安装完成: {}", destination.display()));
                Ok(())
            }
            OutputFormat::Json => {
                let payload = InstalledJson {
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

    /// 错误退出码 0成功 / 1通用 / 2用法 / 3未找到 / 4冲突 / 5后端失败
    ///
    /// `#[error(transparent)]` 包装（[`mcsup::service::InstallJarError`] 等）会把
    /// 内层错误从 `chain()` 里跳过——内层类型 downcast 不到，必须对包装类型
    /// match 其内层字段判级。
    pub(crate) fn exit_code(err: &anyhow::Error) -> u8 {
        if let Some(diag) = err.downcast_ref::<AppDiagnostic>() {
            return diag.exit_code;
        }
        for cause in err.chain() {
            if let Some(jar) = cause.downcast_ref::<mcsup::service::InstallJarError>() {
                return match jar {
                    mcsup::service::InstallJarError::Registry(reg) => registry_exit_code(reg),
                    mcsup::service::InstallJarError::Install(install) => install_exit_code(install),
                    _ => 1,
                };
            }
            if let Some(reg) = cause.downcast_ref::<mcsup::service::ServerRegistryError>() {
                return registry_exit_code(reg);
            }
            if let Some(ws) = cause.downcast_ref::<mcsup::service::WorkspaceError>() {
                return workspace_exit_code(ws);
            }
            if let Some(java) = cause.downcast_ref::<mcsup::service::JavaError>() {
                return java_exit_code(java);
            }
            if let Some(install) = cause.downcast_ref::<mcsup::service::InstallError>() {
                return install_exit_code(install);
            }
            if let Some(source) = cause.downcast_ref::<mcsup::port::SourceError>() {
                return source_exit_code(source);
            }
            if cause.downcast_ref::<mcsup::port::DownloadError>().is_some() {
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

fn source_exit_code(source: &mcsup::port::SourceError) -> u8 {
    match source {
        mcsup::port::SourceError::NotFound(_) => 3,
        mcsup::port::SourceError::Network(_)
        | mcsup::port::SourceError::Status { .. }
        | mcsup::port::SourceError::Init(_) => 5,
        _ => 1,
    }
}

fn registry_exit_code(err: &mcsup::service::ServerRegistryError) -> u8 {
    match err {
        mcsup::service::ServerRegistryError::NotFound(_) => 3,
        mcsup::service::ServerRegistryError::AlreadyExists(_) => 4,
        mcsup::service::ServerRegistryError::Workspace(ws) => workspace_exit_code(ws),
        mcsup::service::ServerRegistryError::Repo(repo) => repo_exit_code(repo),
        _ => 1,
    }
}

fn workspace_exit_code(err: &mcsup::service::WorkspaceError) -> u8 {
    match err {
        mcsup::service::WorkspaceError::NotFound(_) => 3,
        mcsup::service::WorkspaceError::AlreadyExists(_) => 4,
        mcsup::service::WorkspaceError::Repo(repo) => repo_exit_code(repo),
        _ => 1,
    }
}

fn java_exit_code(err: &mcsup::service::JavaError) -> u8 {
    match err {
        mcsup::service::JavaError::NotFound(_) => 3,
        mcsup::service::JavaError::Source(source) => source_exit_code(source),
        mcsup::service::JavaError::Download(_) => 5,
        mcsup::service::JavaError::Repo(repo) => repo_exit_code(repo),
        _ => 1,
    }
}

fn install_exit_code(err: &mcsup::service::InstallError) -> u8 {
    match err {
        mcsup::service::InstallError::Source(source) => source_exit_code(source),
        mcsup::service::InstallError::Download(_) => 5,
        mcsup::service::InstallError::Status(_) => 4,
        _ => 1,
    }
}

fn repo_exit_code(err: &mcsup::port::RepositoryError) -> u8 {
    match err {
        mcsup::port::RepositoryError::NotFound(_) => 3,
        mcsup::port::RepositoryError::AlreadyExists(_) => 4,
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
pub(crate) fn print_table(table: &Table) {
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
    use mcsup::domain::server::version::VersionId;
    use mcsup::port::SourceError;

    #[test]
    fn backend_failure_maps_to_exit_code_5() {
        let err = Err::<(), _>(mcsup::service::InstallError::from(SourceError::Network(
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

    /// 服务器不存在：`InstallJarError` 是 transparent 包装，内层
    /// `ServerRegistryError` 不在 `chain()` 里，判级必须落在包装类型上
    #[test]
    fn install_jar_missing_server_maps_to_exit_code_3() {
        use mcsup::service::{InstallJarError, ServerRegistryError};
        let err = Err::<(), _>(InstallJarError::from(ServerRegistryError::NotFound(
            "nope".to_owned(),
        )))
        .context("安装服务端 nope 失败")
        .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 3);
    }

    /// 版本不存在：同上，经 `InstallJarError::Install` 包装也要还原成 3 / 5
    #[test]
    fn install_jar_missing_version_maps_to_exit_code_3() {
        use mcsup::service::{InstallError, InstallJarError};
        let err = Err::<(), _>(InstallJarError::from(InstallError::from(
            SourceError::NotFound(VersionId::new("0.0.1")),
        )))
        .context("安装服务端 x 失败")
        .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 3);
    }

    #[test]
    fn install_jar_backend_failure_maps_to_exit_code_5() {
        use mcsup::service::{InstallError, InstallJarError};
        let err = Err::<(), _>(InstallJarError::from(InstallError::from(
            SourceError::Network("boom".into()),
        )))
        .context("安装服务端 x 失败")
        .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 5);
    }

    #[test]
    fn workspace_not_found_maps_to_exit_code_3() {
        let err = Err::<(), _>(mcsup::service::WorkspaceError::NotFound("nope".to_owned()))
            .context("切换失败")
            .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 3);
    }

    #[test]
    fn java_not_found_maps_to_exit_code_3() {
        let err = Err::<(), _>(mcsup::service::JavaError::NotFound("/nope".to_owned()))
            .context("卸载失败")
            .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 3);
    }

    #[test]
    fn java_backend_failure_maps_to_exit_code_5() {
        let err = Err::<(), _>(mcsup::service::JavaError::from(SourceError::Network(
            "boom".into(),
        )))
        .context("安装 Java 失败")
        .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 5);
    }
}
