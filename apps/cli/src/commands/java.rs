use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use crate::progress::{InstallSession, ScanProgress};
use anyhow::{Context, Result, anyhow};
use comfy_table::{ContentArrangement, Table};
use inquire::Select;
use mcsup::domain::java::{JavaMajor, JavaPackageType, JavaRuntime, JavaVendor};
use mcsup::service::{InstallStage, JavaService, UninstallOutcome};
use std::path::{Path, PathBuf};
use tracing::info;

use crate::bootstrap;

/// `java list` 单行（JSON 模式序列化用）
#[derive(serde::Serialize)]
struct JavaRow<'a> {
    major: i64,
    version: &'a str,
    vendor: &'a str,
    managed: bool,
    path: String,
}

pub(crate) fn list(printer: Printer) -> Result<()> {
    let service = bootstrap::open_java_service()?;
    let mut progress = ScanProgress::new(printer.is_human());
    let mut first_scan = false;
    let runtimes = service
        .list_or_scan(
            &mut || {
                first_scan = true;
                printer.print_section("正在扫描本机 Java…");
            },
            &mut |found| {
                progress.on_found(&found.path, &found.version);
            },
        )
        .context("列出 Java 失败")?;
    progress.finish();
    if first_scan {
        printer.print_section("扫描完成");
    }
    let rows: Vec<JavaRow<'_>> = runtimes
        .iter()
        .map(|rt| JavaRow {
            major: rt.major.as_i64(),
            version: rt.version.as_str(),
            vendor: rt.vendor.as_str(),
            managed: rt.managed,
            path: rt.path.display().to_string(),
        })
        .collect();
    printer.print_rows(
        &rows,
        "未发现本机 Java，可运行 mcsup java install 安装",
        || {
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
            table
        },
    )
}

pub(crate) fn scan(printer: Printer) -> Result<()> {
    let service = bootstrap::open_java_service()?;
    printer.print_section("正在扫描本机 Java…");
    let mut progress = ScanProgress::new(printer.is_human());
    let outcome = service
        .scan(true, &mut |found| {
            progress.on_found(&found.path, &found.version);
        })
        .context("扫描失败")?;
    progress.finish();
    printer.print_outcome(
        &format!(
            "扫描完成：新注册 {} 个，跳过 {} 个",
            outcome.registered, outcome.skipped
        ),
        &ScanRow {
            registered: outcome.registered,
            skipped: outcome.skipped,
        },
    )
}

/// `java scan` 结果（JSON 模式序列化用）
#[derive(serde::Serialize)]
struct ScanRow {
    registered: usize,
    skipped: usize,
}

/// `java install` 结果（JSON 模式序列化用）
#[derive(serde::Serialize)]
struct InstallJavaRow<'a> {
    major: i64,
    version: &'a str,
    vendor: &'a str,
    path: String,
}

pub(crate) async fn install(
    version: Option<&str>,
    jre: bool,
    vendor: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let enable = printer.is_human();
    let session = InstallSession::new(InstallStage::Probing.message(), enable);
    let service = bootstrap::open_java_service()?;
    let package_type = if jre {
        JavaPackageType::Jre
    } else {
        JavaPackageType::Jdk
    };
    let major = if let Some(raw) = version {
        parse_major(raw)?
    } else {
        // 交互提示要独占这一行：让出后必须恢复，否则后面所有阶段
        // （探测 / 查询版本 / 解析地址 / 下载）都没有回显
        session.pause_for_prompt();
        let selected = prompt_major(printer)?;
        session.resume_after_prompt();
        selected
    };
    let vendor_enum = vendor.map(JavaVendor::parse_fuzzy);

    info!(major = %major, jre, "java install start");
    session.set_download_message(format!("下载 Java {major} {}", package_type.as_str()));
    let outcome = service
        .install(
            major,
            package_type,
            vendor_enum,
            Some(&session),
            Some(&session),
            session.cancel(),
        )
        .await
        .context("安装 Java 失败")?;
    session.finish();

    // 取消落在解压尾段、重装场景已照常注册：安装其实已完成，仍按取消静默退出
    if session.cancel().is_cancelled() {
        return Err(AppDiagnostic::cancelled().into());
    }

    printer.print_outcome(
        &format!(
            "已安装: Java {} ({}) → {}",
            outcome.runtime.version,
            outcome.runtime.vendor,
            outcome.runtime.path.display()
        ),
        &InstallJavaRow {
            major: outcome.runtime.major.as_i64(),
            version: outcome.runtime.version.as_str(),
            vendor: outcome.runtime.vendor.as_str(),
            path: outcome.runtime.path.display().to_string(),
        },
    )
}

/// `java uninstall` 结果（JSON 模式序列化用）
#[derive(serde::Serialize)]
struct UninstallJavaRow {
    path: String,
    kind: &'static str,
}

pub(crate) fn uninstall(target: Option<&str>, dir: Option<&Path>, printer: Printer) -> Result<()> {
    let service = bootstrap::open_java_service()?;
    let path = resolve_target(&service, target, dir, printer)?;
    let outcome = service.uninstall(&path).context("卸载失败")?;
    let (message, row) = match outcome {
        UninstallOutcome::RemovedManaged { path } => (
            format!("已删除 mcsup 管理的 Java: {}", path.display()),
            UninstallJavaRow {
                path: path.display().to_string(),
                kind: "managed",
            },
        ),
        UninstallOutcome::UnregisteredSystem { path } => (
            format!("已从注册表移除（文件保留）: {}", path.display()),
            UninstallJavaRow {
                path: path.display().to_string(),
                kind: "system",
            },
        ),
    };
    printer.print_outcome(&message, &row)
}

fn parse_major(raw: &str) -> Result<JavaMajor> {
    let trimmed = raw.trim().trim_start_matches("java-");
    JavaMajor::parse_str(trimmed)
        .map_err(|_| anyhow!("无法识别的 Java 大版本: {raw}（示例: 17 / 21）"))
}

fn prompt_major(printer: Printer) -> Result<JavaMajor> {
    // JSON 输出或管道下提问会卡死脚本，统一走 Printer 口径
    if !printer.is_interactive() {
        return Err(anyhow!("非交互环境请指定版本，如 mcsup java install 21"));
    }
    // 已发布大版本倒序（新版本在前）；inquire Select 自带滚动。
    // 可选项上界与 LTS 标注是领域知识，判定在 domain::java，不在此分叉
    let mut choices = Vec::new();
    for major in (JavaMajor::MIN..=JavaMajor::RELEASED_MAX).rev() {
        let label = if JavaMajor::LTS_MAJORS.contains(&major) {
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
    printer: Printer,
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
            _ if printer.is_interactive() => prompt_runtime(major, &matches),
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
