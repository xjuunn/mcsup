use crate::diagnostic::AppDiagnostic;
use crate::locale::Lang;
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
    lang: Lang,
}

impl Printer {
    #[must_use]
    pub(crate) const fn new(format: OutputFormat, lang: Lang) -> Self {
        Self { format, lang }
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
            if let Some(life) = cause.downcast_ref::<mcsup::service::LifecycleError>() {
                return lifecycle_exit_code(life);
            }
            if let Some(props) = cause.downcast_ref::<mcsup::service::PropsError>() {
                return props_exit_code(props);
            }
            if let Some(settings) = cause.downcast_ref::<mcsup::service::SettingsError>() {
                return settings_exit_code(settings);
            }
            if let Some(jar) = cause.downcast_ref::<mcsup::service::InstallJarError>() {
                return match jar.kind() {
                    // 世界与目标版本冲突：状态冲突，不重试同一版本
                    Some(mcsup::service::InstallJarErrorKind::Downgrade { .. }) => 4,
                    _ => match jar {
                        mcsup::service::InstallJarError::Registry(reg) => registry_exit_code(reg),
                        mcsup::service::InstallJarError::Install(install) => {
                            install_exit_code(install)
                        }
                        _ => 1,
                    },
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
            if let Some(doctor) = cause.downcast_ref::<mcsup::service::DoctorError>() {
                // 注册表都读不出来：不是「数据有问题」而是「后端不可用」，
                // 与体检发现 error 级问题的 1 区分开
                return match doctor {
                    mcsup::service::DoctorError::Repo(_) => 5,
                    _ => 1,
                };
            }
            if let Some(source) = cause.downcast_ref::<mcsup::port::SourceError>() {
                return source_exit_code(source);
            }
            if let Some(download) = cause.downcast_ref::<mcsup::port::DownloadError>() {
                return download_exit_code(download);
            }
        }
        1
    }

    /// 用 miette 渲染错误到 stderr
    pub(crate) fn render_error(self, err: anyhow::Error) {
        if let Some(message) = structured_message(&err, self.lang) {
            print_error_line(&format!("mcsup: {message}"));
            return;
        }
        match err.downcast::<AppDiagnostic>() {
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

    #[must_use]
    pub(crate) const fn is_human(self) -> bool {
        matches!(self.format, OutputFormat::Human)
    }
}

/// 结构化错误的用户提示：只认带 `kind()` 的那几类，其余（IO、注册表、
/// 下载源等）沿用它自己的 `Display`。文案集中在 [`crate::i18n`]。
fn structured_message(err: &anyhow::Error, lang: Lang) -> Option<String> {
    for cause in err.chain() {
        if let Some(life) = cause.downcast_ref::<mcsup::service::LifecycleError>() {
            let kind = life.kind()?;
            return Some(crate::i18n::lifecycle(&kind, lang));
        }
        if let Some(jar) = cause.downcast_ref::<mcsup::service::InstallJarError>() {
            let kind = jar.kind()?;
            return Some(crate::i18n::install_jar(&kind, lang));
        }
        if let Some(settings) = cause.downcast_ref::<mcsup::service::SettingsError>() {
            let kind = settings.kind()?;
            return Some(crate::i18n::settings(&kind, lang));
        }
    }
    None
}

/// 生命周期错误判级：状态冲突 4、缺资源 3、EULA 用法 2、后端 5、其余 1
fn lifecycle_exit_code(err: &mcsup::service::LifecycleError) -> u8 {
    use mcsup::service::LifecycleError as Life;
    match err {
        Life::Registry(reg) => registry_exit_code(reg),
        Life::Java(java) => java_exit_code(java),
        Life::Supervisor(_) => 5,
        Life::NoJar { .. } | Life::NoJava | Life::UnknownJavaMajor { .. } => 3,
        Life::EulaRequired => 2,
        // 运行中（含删除 / 换核心被拒）：状态冲突
        Life::AlreadyRunning | Life::NotRunning | Life::Busy { .. } => 4,
        // 换核心失败：注册表 / 来源 / 下载各按原判级，其余 1
        Life::InstallJar(jar) => match jar.kind() {
            Some(mcsup::service::InstallJarErrorKind::Downgrade { .. }) => 4,
            _ => match jar {
                mcsup::service::InstallJarError::Registry(reg) => registry_exit_code(reg),
                mcsup::service::InstallJarError::Install(install) => install_exit_code(install),
                _ => 1,
            },
        },
        // StopTimedOut / Io 与未来新增变体：通用 1
        _ => 1,
    }
}

/// 属性文件判级：键 / 值非法 2（用法）、缺服务器 3、IO 失败 1
fn props_exit_code(err: &mcsup::service::PropsError) -> u8 {
    use mcsup::service::PropsError as Props;
    match err {
        Props::Registry(reg) => registry_exit_code(reg),
        Props::InvalidSet(_) => 2,
        _ => 1,
    }
}

/// 服务属性判级：值非法 2（用法）、大版本未注册 / 缺服务器 3、其余 1
fn settings_exit_code(err: &mcsup::service::SettingsError) -> u8 {
    use mcsup::service::SettingsError as Set;
    match err {
        Set::Registry(reg) => registry_exit_code(reg),
        Set::Java(java) => java_exit_code(java),
        Set::InvalidValue(_) => 2,
        Set::UnknownJavaMajor { .. } => 3,
        _ => 1,
    }
}

/// 下载失败 → 5 后端失败；用户取消 → 130（128 + SIGINT），与 Ctrl-C 强制
/// 退出码一致，调用方凭码区分「取消」与「失败」
fn download_exit_code(err: &mcsup::port::DownloadError) -> u8 {
    match err {
        mcsup::port::DownloadError::Cancelled => 130,
        _ => 5,
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
        // 未找到 / 无当前工作区都是「缺省资源不存在」（契约：3 未找到）
        mcsup::service::WorkspaceError::NotFound(_) | mcsup::service::WorkspaceError::NoCurrent => {
            3
        }
        mcsup::service::WorkspaceError::AlreadyExists(_) => 4,
        mcsup::service::WorkspaceError::Repo(repo) => repo_exit_code(repo),
        _ => 1,
    }
}

fn java_exit_code(err: &mcsup::service::JavaError) -> u8 {
    match err {
        mcsup::service::JavaError::NotFound(_) => 3,
        mcsup::service::JavaError::Source(source) => source_exit_code(source),
        mcsup::service::JavaError::Download(download) => download_exit_code(download),
        mcsup::service::JavaError::Repo(repo) => repo_exit_code(repo),
        _ => 1,
    }
}

fn install_exit_code(err: &mcsup::service::InstallError) -> u8 {
    match err {
        mcsup::service::InstallError::Source(source) => source_exit_code(source),
        mcsup::service::InstallError::Download(download) => download_exit_code(download),
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
        Self::new(OutputFormat::Human, Lang::Zh)
    }
}

#[allow(clippy::cast_precision_loss)]
pub(crate) fn human_bytes(n: u64) -> String {
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
pub(crate) fn print_json_line(line: &str) {
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

    /// 世界降级：状态冲突，重试同一版本无意义
    #[test]
    fn install_jar_downgrade_maps_to_exit_code_4() {
        use mcsup::domain::server::world::DataVersion;
        use mcsup::service::InstallJarError;
        let err = Err::<(), _>(InstallJarError::Downgrade {
            world: DataVersion::new(4786),
            target: DataVersion::new(4554),
            world_version: "26.1".to_owned(),
            target_version: "1.21.9".to_owned(),
        })
        .context("安装服务端 x 失败")
        .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 4);
    }

    #[test]
    fn workspace_not_found_maps_to_exit_code_3() {
        let err = Err::<(), _>(mcsup::service::WorkspaceError::NotFound("nope".to_owned()))
            .context("切换失败")
            .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 3);
    }

    /// 无当前工作区：缺省落点资源不存在，同「未找到」判 3
    #[test]
    fn no_current_workspace_maps_to_exit_code_3() {
        let err = Err::<(), _>(mcsup::service::WorkspaceError::NoCurrent)
            .context("创建服务器失败")
            .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 3);
    }

    /// 用法错误（非交互缺必选参数 / 取值不合法）：退出码 2
    #[test]
    fn usage_error_maps_to_exit_code_2() {
        let err = anyhow::Error::from(crate::diagnostic::AppDiagnostic::usage(
            "server create 缺少 --version",
            Some("交互终端下可省略".to_owned()),
        ));
        assert_eq!(Printer::exit_code(&err), 2);
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

    // ---- 生命周期判级 ----

    /// 已在运行 / 未在运行：状态冲突 → 4
    #[test]
    fn lifecycle_state_conflict_maps_to_exit_code_4() {
        use mcsup::service::LifecycleError;
        for err in [LifecycleError::AlreadyRunning, LifecycleError::NotRunning] {
            let err = Err::<(), _>(err).context("启动失败").unwrap_err();
            assert_eq!(Printer::exit_code(&err), 4);
        }
    }

    /// EULA 未同意（非交互）：用法类 → 2
    #[test]
    fn lifecycle_eula_required_maps_to_exit_code_2() {
        let err = Err::<(), _>(mcsup::service::LifecycleError::EulaRequired)
            .context("启动失败")
            .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 2);
    }

    /// 缺核心 / 缺 Java：资源不存在 → 3
    #[test]
    fn lifecycle_missing_resources_map_to_exit_code_3() {
        use mcsup::service::LifecycleError;
        for err in [
            LifecycleError::NoJar {
                name: "alpha".to_owned(),
            },
            LifecycleError::NoJava,
        ] {
            let err = Err::<(), _>(err).context("启动失败").unwrap_err();
            assert_eq!(Printer::exit_code(&err), 3);
        }
    }

    /// supervisor 交互失败（spawn / IPC）：后端失败 → 5
    #[test]
    fn lifecycle_supervisor_error_maps_to_exit_code_5() {
        let err = Err::<(), _>(mcsup::service::LifecycleError::Supervisor(
            mcsup::port::SupervisorError::Unreachable("gone".to_owned()),
        ))
        .context("停止失败")
        .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 5);
    }

    /// 包装在 Registry 内层的未找到同样还原为 3
    #[test]
    fn lifecycle_registry_not_found_maps_to_exit_code_3() {
        use mcsup::service::{LifecycleError, ServerRegistryError};
        let err = Err::<(), _>(LifecycleError::Registry(ServerRegistryError::NotFound(
            "nope".to_owned(),
        )))
        .context("状态查询失败")
        .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 3);
    }

    /// 停机超时：通用失败 → 1（提示由错误文案携带）
    #[test]
    fn lifecycle_stop_timeout_maps_to_exit_code_1() {
        let err = Err::<(), _>(mcsup::service::LifecycleError::StopTimedOut {
            waited: std::time::Duration::from_secs(30),
        })
        .context("停止失败")
        .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 1);
    }

    /// 运行中的删除 / 换核心被拒（Busy）：状态冲突 → 4
    #[test]
    fn lifecycle_busy_maps_to_exit_code_4() {
        let err = Err::<(), _>(mcsup::service::LifecycleError::Busy {
            name: "alpha".to_owned(),
            op: mcsup::service::BusyOp::Remove,
        })
        .context("删除失败")
        .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 4);
    }

    /// 换核心的 Downgrade 经 `LifecycleError::InstallJar` 包装同样还原为 4
    #[test]
    fn lifecycle_install_jar_downgrade_maps_to_exit_code_4() {
        use mcsup::domain::server::world::DataVersion;
        use mcsup::service::{InstallJarError, LifecycleError};
        let err = Err::<(), _>(LifecycleError::InstallJar(InstallJarError::Downgrade {
            world: DataVersion::new(4786),
            target: DataVersion::new(4554),
            world_version: "26.1".to_owned(),
            target_version: "1.21.9".to_owned(),
        }))
        .context("安装服务端失败")
        .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 4);
    }

    /// 用户取消（Ctrl-C 协作取消）：130 = 128 + SIGINT，与后端失败 5 区分
    #[test]
    fn java_cancelled_maps_to_exit_code_130() {
        let err = Err::<(), _>(mcsup::service::JavaError::from(
            mcsup::port::DownloadError::Cancelled,
        ))
        .context("安装 Java 失败")
        .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 130);
    }

    /// 裸下载取消（不经 `JavaError` 包装，如服务端核心下载）同样判 130
    #[test]
    fn download_cancelled_maps_to_exit_code_130() {
        let err = Err::<(), _>(mcsup::port::DownloadError::Cancelled)
            .context("下载服务端失败")
            .unwrap_err();
        assert_eq!(Printer::exit_code(&err), 130);
    }
}
