use crate::config;
use crate::output::Printer;
use crate::progress::{BarSink, TaskSpinner};
use anyhow::{Context, Result, anyhow};
use comfy_table::{ContentArrangement, Table};
use inquire::Select;
use mcsup::domain::java::{JavaMajor, JavaPackageType, JavaRuntime, JavaVendor};
use mcsup::file::FileDownloader;
use mcsup::java::HostJavaScanner;
use mcsup::port::CancellationToken;
use mcsup::provider::http::HttpByteSource;
use mcsup::provider::java::{JavaSourceRegistry, default_request, rank_sources};
use mcsup::service::{
    InstallStage, JavaService, JavaSourceRegistryPort, StageSink, UninstallOutcome,
};
use mcsup::storage::SqliteStore;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::info;

/// 把 provider 的 [`JavaSourceRegistry`] 适配为 service 端口
struct RegistryAdapter(JavaSourceRegistry);

#[async_trait::async_trait]
impl JavaSourceRegistryPort for RegistryAdapter {
    fn source_names(&self) -> Vec<&'static str> {
        self.0.source_names()
    }

    async fn list_packages(
        &self,
        major: JavaMajor,
        package_type: JavaPackageType,
    ) -> Result<Vec<mcsup::port::JavaPackage>, mcsup::port::SourceError> {
        let request = default_request(major, package_type);
        self.0.list_packages(&request).await
    }

    async fn resolve_binary(
        &self,
        package: &mcsup::port::JavaPackage,
    ) -> Result<mcsup::port::ResolvedJava, mcsup::port::SourceError> {
        self.0.resolve_binary(package).await
    }
}

/// 把 service 的 [`StageSink`] 转发到 spinner；进入下载阶段时收掉 spinner，让进度条独占
struct StageAdapter<'a> {
    spinner: &'a TaskSpinner,
    downloading: std::sync::atomic::AtomicBool,
}

impl StageAdapter<'_> {
    fn new(spinner: &TaskSpinner) -> StageAdapter<'_> {
        StageAdapter {
            spinner,
            downloading: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl StageSink for StageAdapter<'_> {
    fn on_stage(&self, stage: InstallStage) {
        if matches!(stage, InstallStage::Downloading) {
            // 下载阶段：清掉 spinner，BarSink 独占 stderr
            self.downloading
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.spinner.finish();
        } else if self.downloading.load(std::sync::atomic::Ordering::Relaxed) {
            // 下载后的阶段（解压等）：spinner 已 finish，换文案会重新绘制
            self.spinner.set_message(stage.message());
        } else {
            self.spinner.set_message(stage.message());
        }
    }
}

/// 本地命令（list / scan / uninstall）：不需要源探测，避免每次启动打网络
fn open_local_service() -> Result<JavaService> {
    let path = config::database_path()?;
    let store = Arc::new(
        SqliteStore::open(&path).with_context(|| format!("打开元数据库 {}", path.display()))?,
    );
    let scanner = Arc::new(HostJavaScanner::new());
    let registry = Arc::new(RegistryAdapter(JavaSourceRegistry::new(Vec::new())));
    let bytes = Arc::new(HttpByteSource::new().context("初始化 HTTP 传输")?);
    let downloader = Arc::new(FileDownloader::new(bytes.clone()).with_range_source(bytes));
    let java_root = config::platform_data_dir()?.join("java");
    std::fs::create_dir_all(&java_root)
        .with_context(|| format!("创建 Java 目录 {}", java_root.display()))?;
    Ok(JavaService::new(
        store.clone(),
        scanner,
        registry,
        downloader,
        java_root,
    ))
}

/// install：并发探测源延迟并排序；`spinner` 负责探测阶段回显
async fn open_install_service(spinner: &TaskSpinner) -> Result<JavaService> {
    spinner.set_message(InstallStage::Probing.message());
    let path = config::database_path()?;
    let store = Arc::new(
        SqliteStore::open(&path).with_context(|| format!("打开元数据库 {}", path.display()))?,
    );
    let scanner = Arc::new(HostJavaScanner::new());
    let registry = Arc::new(RegistryAdapter(rank_sources().await));
    let bytes = Arc::new(HttpByteSource::new().context("初始化 HTTP 传输")?);
    let downloader = Arc::new(FileDownloader::new(bytes.clone()).with_range_source(bytes));
    let java_root = config::platform_data_dir()?.join("java");
    std::fs::create_dir_all(&java_root)
        .with_context(|| format!("创建 Java 目录 {}", java_root.display()))?;
    Ok(JavaService::new(
        store.clone(),
        scanner,
        registry,
        downloader,
        java_root,
    ))
}

/// 单行刷新扫描进度：`\r` 覆盖当前行，非 TTY 时退化为逐行
struct ScanProgress {
    interactive: bool,
    last_len: usize,
}

impl ScanProgress {
    fn new(enable: bool) -> Self {
        Self {
            interactive: enable && std::io::stderr().is_terminal(),
            last_len: 0,
        }
    }

    fn on_found(&mut self, path: &Path, version: &str) {
        #[allow(clippy::print_stderr)]
        {
            if self.interactive {
                let line = format!("  扫描到: {} ({})", path.display(), version);
                // 清掉上一行残留（路径长短不一时用空格补齐）
                let pad = self.last_len.saturating_sub(line.len());
                eprint!("\r{line}{}", " ".repeat(pad));
                let _ = std::io::stderr().flush();
                self.last_len = line.len();
            } else {
                eprintln!("  扫描到: {} ({})", path.display(), version);
            }
        }
    }

    fn finish(&mut self) {
        if self.interactive && self.last_len > 0 {
            #[allow(clippy::print_stderr)]
            {
                eprintln!();
            }
            self.last_len = 0;
        }
    }
}

pub(crate) fn list(printer: Printer) -> Result<()> {
    let service = open_local_service()?;
    // 未扫描过时带进度回调触发一次
    if !service.has_scanned().context("读取扫描状态")? {
        printer.print_section("首次使用，正在扫描本机 Java…");
        let mut progress = ScanProgress::new(printer.is_human());
        service
            .scan(false, &mut |found| {
                progress.on_found(&found.path, &found.version);
            })
            .context("扫描失败")?;
        progress.finish();
        printer.print_section("扫描完成");
    }
    let runtimes = service.list().context("列出 Java 失败")?;
    if runtimes.is_empty() {
        printer.print_section("未发现本机 Java，可运行 mcsup java install 安装");
        return Ok(());
    }
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(["大版本", "完整版本", "厂商", "管理", "路径"]);
    for rt in &runtimes {
        table.add_row([
            rt.major.to_string(),
            rt.version.clone(),
            rt.vendor.as_str().to_owned(),
            if rt.managed { "mcsup" } else { "系统" }.to_owned(),
            rt.path.display().to_string(),
        ]);
    }
    crate::output::print_table(&table);
    Ok(())
}

pub(crate) fn scan(printer: Printer) -> Result<()> {
    let service = open_local_service()?;
    printer.print_section("正在扫描本机 Java…");
    let mut progress = ScanProgress::new(printer.is_human());
    let outcome = service
        .scan(true, &mut |found| {
            progress.on_found(&found.path, &found.version);
        })
        .context("扫描失败")?;
    progress.finish();
    printer.print_section(&format!("扫描完成：新注册 {} 个", outcome.registered));
    Ok(())
}

pub(crate) async fn install(
    version: Option<&str>,
    jre: bool,
    vendor: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let enable = printer.is_human();
    let spinner = TaskSpinner::new(InstallStage::Probing.message(), enable);
    let service = open_install_service(&spinner).await?;
    let package_type = if jre {
        JavaPackageType::Jre
    } else {
        JavaPackageType::Jdk
    };
    let major = if let Some(raw) = version {
        parse_major(raw)?
    } else {
        spinner.finish();
        prompt_major(enable)?
    };
    let vendor_enum = vendor.map(JavaVendor::parse_fuzzy);

    info!(major = %major, jre, "java install start");
    let sink = BarSink::new(
        format!("下载 Java {major} {}", package_type.as_str()),
        enable,
    );
    let cancel = CancellationToken::new();
    let stage = StageAdapter::new(&spinner);
    let outcome = service
        .install(
            major,
            package_type,
            vendor_enum,
            Some(&stage),
            Some(&sink),
            &cancel,
        )
        .await
        .context("安装 Java 失败")?;
    sink.finish();
    spinner.finish();

    printer.print_section(&format!(
        "已安装: Java {} ({}) → {}",
        outcome.runtime.version,
        outcome.runtime.vendor,
        outcome.runtime.path.display()
    ));
    Ok(())
}

pub(crate) fn uninstall(target: Option<&str>, dir: Option<&Path>, printer: Printer) -> Result<()> {
    let service = open_local_service()?;
    let path = resolve_target(&service, target, dir)?;
    let outcome = service.uninstall(&path).context("卸载失败")?;
    match outcome {
        UninstallOutcome::RemovedManaged { path } => {
            printer.print_section(&format!("已删除 mcsup 管理的 Java: {}", path.display()));
        }
        UninstallOutcome::UnregisteredSystem { path } => {
            printer.print_section(&format!("已从注册表移除（文件保留）: {}", path.display()));
        }
    }
    Ok(())
}

fn parse_major(raw: &str) -> Result<JavaMajor> {
    let trimmed = raw.trim().trim_start_matches("java-");
    trimmed
        .parse::<i64>()
        .ok()
        .and_then(|n| JavaMajor::parse(n).ok())
        .ok_or_else(|| anyhow!("无法识别的 Java 大版本: {raw}（示例: 17 / 21）"))
}

/// LTS 大版本集合，用于交互列表标注
const LTS_MAJORS: &[i64] = &[8, 11, 17, 21, 25];

/// 交互列表的最高大版本：只列已发布版本，避免出现还不存在的选择
///
/// 能力上界是 [`JavaMajor::MAX`]，更新的版本用 `mcsup java install <大版本>` 指定。
const INSTALL_MENU_MAX: i64 = 27;

fn prompt_major(enable: bool) -> Result<JavaMajor> {
    if !enable || !std::io::stdin().is_terminal() {
        return Err(anyhow!("非交互环境请指定版本，如 mcsup java install 21"));
    }
    // 已发布大版本倒序（新版本在前）；inquire Select 自带滚动
    let mut choices = Vec::new();
    for major in (JavaMajor::MIN..=INSTALL_MENU_MAX).rev() {
        let label = if LTS_MAJORS.contains(&major) {
            format!("{major} (LTS)")
        } else {
            major.to_string()
        };
        choices.push(label);
    }
    let selected = Select::new("选择 Java 大版本", choices)
        .with_page_size(12)
        .prompt()
        .context("选择版本取消")?;
    let major_raw = selected.split_whitespace().next().unwrap_or("21");
    parse_major(major_raw)
}

fn resolve_target(
    service: &JavaService,
    target: Option<&str>,
    dir: Option<&Path>,
) -> Result<PathBuf> {
    // --dir 优先：路径含空格时避免 shell 拆分问题
    if let Some(dir) = dir {
        return Ok(dir.to_path_buf());
    }
    let Some(target) = target else {
        return Err(anyhow!(
            "请指定大版本号或 --dir <路径>，如 mcsup java uninstall 17 或 mcsup java uninstall --dir \"…/Contents/Home\""
        ));
    };
    if let Ok(major) = parse_major(target) {
        let matches = service.find_by_major(major).context("查询运行时失败")?;
        return match matches.len() {
            0 => Err(anyhow!("未注册的大版本 Java: {major}")),
            1 => Ok(matches[0].path.clone()),
            _ if std::io::stdin().is_terminal() => prompt_runtime(major, &matches),
            _ => Err(anyhow!(
                "大版本 {major} 有多个运行时，请用 --dir 指定路径，如:\n  mcsup java uninstall --dir \"{}\"",
                matches[0].path.display()
            )),
        };
    }
    let as_path = Path::new(target);
    if as_path.exists() || target.contains('/') || target.contains('\\') {
        return Ok(as_path.to_path_buf());
    }
    Err(anyhow!("无法识别的目标: {target}（大版本号或 --dir 路径）"))
}

/// 多个运行时命中大版本时，TTY 下交互选择
fn prompt_runtime(major: JavaMajor, matches: &[JavaRuntime]) -> Result<PathBuf> {
    let choices: Vec<String> = matches
        .iter()
        .map(|rt| {
            format!(
                "{} ({}) {}",
                rt.path.display(),
                rt.vendor,
                if rt.managed { "mcsup" } else { "系统" }
            )
        })
        .collect();
    let selected = Select::new(
        &format!("大版本 {major} 有多个运行时，选择要卸载的"),
        choices,
    )
    .prompt()
    .context("选择取消")?;
    // 第一个匹配即用户选中的（label 与 matches 一一对应）
    matches
        .iter()
        .find(|rt| selected.starts_with(&rt.path.display().to_string()))
        .map(|rt| rt.path.clone())
        .ok_or_else(|| anyhow!("未匹配到所选运行时"))
}
