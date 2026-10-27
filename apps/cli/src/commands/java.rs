use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use crate::progress::{InstallSession, ScanProgress};
use anyhow::{Context, Result, anyhow};
use comfy_table::{ContentArrangement, Table};
use inquire::Select;
use mcsup::domain::java::{JavaMajor, JavaPackageType, JavaRuntime, JavaVendor};
use mcsup::protocol::cli::{JavaInstallJson, JavaRow, JavaScanJson, JavaUninstallJson};
use mcsup::service::{InstallStage, JavaService, UninstallOutcome};
use std::path::{Path, PathBuf};
use tracing::info;

use crate::bootstrap;
use crate::locale::Lang;

pub(crate) fn list(filter: &mcsup::service::OriginFilter, printer: Printer) -> Result<()> {
    let service = bootstrap::open_java_service()?;
    let mut progress = ScanProgress::new(printer.is_human(), printer.lang());
    let mut first_scan = false;
    let sourced = service
        .list_sourced(
            &mut || {
                first_scan = true;
                printer.print_section(&crate::i18n::pick(
                    printer.lang(),
                    "正在扫描本机 Java…",
                    "scanning the host for Java…",
                ));
            },
            &mut |found| {
                progress.on_found(&found.path, &found.version);
            },
        )
        .context(crate::i18n::pick(
            printer.lang(),
            "列出 Java 失败",
            "failed to list Java runtimes",
        ))?;
    progress.finish();
    if first_scan {
        printer.print_section(&crate::i18n::pick(
            printer.lang(),
            "扫描完成",
            "scan complete",
        ));
    }
    let runtimes: Vec<_> = sourced
        .into_iter()
        .filter(|item| filter.matches(&item.origin))
        .collect();
    let rows: Vec<JavaRow<'_>> = runtimes
        .iter()
        .map(|item| {
            let rt = &item.value;
            JavaRow {
                origin: super::origin::label(&item.origin),
                major: rt.major.as_i64(),
                version: rt.version.as_str(),
                vendor: rt.vendor.as_str(),
                managed: rt.managed,
                path: rt.path.display().to_string(),
            }
        })
        .collect();
    printer.print_rows(
        &rows,
        &crate::i18n::pick(
            printer.lang(),
            "未发现本机 Java，可运行 mcsup java install 安装",
            "no Java found on this machine; run mcsup java install",
        ),
        || {
            let mut table = Table::new();
            table.set_content_arrangement(ContentArrangement::Dynamic);
            let l = printer.lang();
            table.set_header([
                crate::i18n::text(l, "大版本", "major"),
                crate::i18n::text(l, "来源", "origin"),
                crate::i18n::text(l, "完整版本", "version"),
                crate::i18n::text(l, "厂商", "vendor"),
                crate::i18n::text(l, "管理", "managed"),
                crate::i18n::text(l, "路径", "path"),
            ]);
            for item in &runtimes {
                let rt = &item.value;
                table.add_row([
                    rt.major.to_string(),
                    super::origin::label(&item.origin),
                    rt.version.clone(),
                    rt.vendor.as_str().to_owned(),
                    if rt.managed {
                        "mcsup".to_owned()
                    } else {
                        crate::i18n::pick(printer.lang(), "系统", "system")
                    },
                    rt.path.display().to_string(),
                ]);
            }
            table
        },
    )
}

pub(crate) fn scan(printer: Printer) -> Result<()> {
    let service = bootstrap::open_java_service()?;
    printer.print_section(&crate::i18n::pick(
        printer.lang(),
        "正在扫描本机 Java…",
        "scanning the host for Java…",
    ));
    let mut progress = ScanProgress::new(printer.is_human(), printer.lang());
    let outcome = service
        .scan(true, &mut |found| {
            progress.on_found(&found.path, &found.version);
        })
        .context(crate::i18n::pick(printer.lang(), "扫描失败", "scan failed"))?;
    progress.finish();
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!(
                "扫描完成：新注册 {} 个，跳过 {} 个",
                outcome.registered, outcome.skipped
            ),
            format!(
                "scan complete: {} newly registered, {} skipped",
                outcome.registered, outcome.skipped
            ),
        ),
        &JavaScanJson {
            registered: outcome.registered,
            skipped: outcome.skipped,
        },
    )
}

/// `java scan --origin <remote>`：让对端扫描并登记
pub(crate) fn scan_remote(origin: &mcsup::port::Origin, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    let cfg = crate::config::ConfigFile::locate(None).load()?;
    let members = bootstrap::remote_members(&cfg)?;
    let member = super::origin::member(&members, origin, lang)?.ok_or_else(|| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                "本地来源不需要远端扫描",
                "local origin does not need a remote scan",
            ),
            None,
        )
    })?;
    let runtimes = member
        .java
        .scan()
        .with_context(|| crate::i18n::pick(lang, "远端扫描失败", "remote scan failed"))?;
    printer.print_outcome(
        &crate::i18n::pick(
            lang,
            format!(
                "{} 扫描完成：已登记 {} 个 Java 运行时",
                member.name,
                runtimes.len()
            ),
            format!(
                "scan on {} complete: {} Java runtimes registered",
                member.name,
                runtimes.len()
            ),
        ),
        &JavaScanJson {
            registered: runtimes.len(),
            skipped: 0,
        },
    )
}

/// 安装 Java 并返回结果（供 server start 调用）
pub(crate) async fn install_with_outcome(
    version: Option<&str>,
    jre: bool,
    vendor: Option<&str>,
    printer: Printer,
) -> Result<mcsup::service::InstallOutcome> {
    let enable = printer.is_human();
    let session = InstallSession::new(InstallStage::Probing.message(), enable);
    let service = bootstrap::open_java_service()?;
    let package_type = if jre {
        JavaPackageType::Jre
    } else {
        JavaPackageType::Jdk
    };
    let major = if let Some(raw) = version {
        parse_major(printer.lang(), raw)?
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
    session.set_download_message(crate::i18n::pick(
        printer.lang(),
        format!("下载 Java {major} {}", package_type.as_str()),
        format!("downloading Java {major} {}", package_type.as_str()),
    ));
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
        .context(crate::i18n::pick(
            printer.lang(),
            "安装 Java 失败",
            "failed to install Java",
        ))?;
    session.finish();

    // 取消落在解压尾段、重装场景已照常注册：安装其实已完成，仍按取消静默退出
    if session.cancel().is_cancelled() {
        return Err(AppDiagnostic::cancelled().into());
    }

    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!(
                "已安装: Java {} ({}) → {}",
                outcome.runtime.version,
                outcome.runtime.vendor,
                outcome.runtime.path.display()
            ),
            format!(
                "installed: Java {} ({}) → {}",
                outcome.runtime.version,
                outcome.runtime.vendor,
                outcome.runtime.path.display()
            ),
        ),
        &JavaInstallJson {
            major: outcome.runtime.major.as_i64(),
            version: outcome.runtime.version.as_str(),
            vendor: outcome.runtime.vendor.as_str(),
            path: outcome.runtime.path.display().to_string(),
        },
    )?;

    Ok(outcome)
}

pub(crate) async fn install(
    version: Option<&str>,
    jre: bool,
    vendor: Option<&str>,
    printer: Printer,
) -> Result<()> {
    install_with_outcome(version, jre, vendor, printer).await?;
    Ok(())
}

pub(crate) fn uninstall(target: Option<&str>, dir: Option<&Path>, printer: Printer) -> Result<()> {
    let service = bootstrap::open_java_service()?;
    let path = resolve_target(&service, target, dir, printer)?;
    let outcome = service.uninstall(&path).context(crate::i18n::pick(
        printer.lang(),
        "卸载失败",
        "uninstall failed",
    ))?;
    let (message, row) = match outcome {
        UninstallOutcome::RemovedManaged { path } => (
            crate::i18n::pick(
                printer.lang(),
                format!("已删除 mcsup 管理的 Java: {}", path.display()),
                format!("deleted the mcsup-managed Java: {}", path.display()),
            ),
            JavaUninstallJson {
                path: path.display().to_string(),
                kind: "managed",
            },
        ),
        UninstallOutcome::UnregisteredSystem { path } => (
            crate::i18n::pick(
                printer.lang(),
                format!("已从注册表移除（文件保留）: {}", path.display()),
                format!("unregistered (files kept): {}", path.display()),
            ),
            JavaUninstallJson {
                path: path.display().to_string(),
                kind: "system",
            },
        ),
    };
    printer.print_outcome(&message, &row)
}

fn parse_major(lang: Lang, raw: &str) -> Result<JavaMajor> {
    let trimmed = raw.trim().trim_start_matches("java-");
    JavaMajor::parse_str(trimmed).map_err(|_| {
        anyhow!(crate::i18n::pick(
            lang,
            format!("无法识别的 Java 大版本: {raw}（示例: 17 / 21）"),
            format!("unrecognized Java major version: {raw} (e.g. 17 / 21)"),
        ))
    })
}

fn prompt_major(printer: Printer) -> Result<JavaMajor> {
    // JSON 输出或管道下提问会卡死脚本，统一走 Printer 口径
    if !printer.is_interactive() {
        return Err(anyhow!(crate::i18n::pick(
            printer.lang(),
            "非交互环境请指定版本，如 mcsup java install 21",
            "non-interactive: pass a version, e.g. mcsup java install 21",
        )));
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
    let selected = Select::new(
        &crate::i18n::pick(
            printer.lang(),
            "选择 Java 大版本",
            "select a Java major version",
        ),
        choices,
    )
    .with_page_size(12)
    .prompt()
    .context(crate::i18n::pick(
        printer.lang(),
        "选择版本取消",
        "version selection cancelled",
    ))?;
    let major_raw = selected.split_whitespace().next().unwrap_or("21");
    parse_major(printer.lang(), major_raw)
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
        // 缺必填目标：交互下从已注册运行时中选一个；非交互报用法错误（2）
        if printer.is_interactive() {
            return prompt_registered_runtime(service, printer.lang());
        }
        return Err(anyhow!(crate::i18n::pick(
            printer.lang(),
            "请指定大版本号或 --dir <路径>，如 mcsup java uninstall 17 或 mcsup java uninstall --dir \"…/Contents/Home\"",
            "pass a major version or --dir <path>, e.g. mcsup java uninstall 17 or mcsup java uninstall --dir \"…/Contents/Home\"",
        )));
    };
    if let Ok(major) = parse_major(printer.lang(), target) {
        let matches = service.find_by_major(major).context(crate::i18n::pick(
            printer.lang(),
            "查询运行时失败",
            "failed to query runtimes",
        ))?;
        return match matches.len() {
            0 => Err(anyhow!(crate::i18n::pick(
                printer.lang(),
                format!("未注册的大版本 Java: {major}"),
                format!("no registered Java for major version: {major}"),
            ))),
            1 => Ok(matches[0].path.clone()),
            _ if printer.is_interactive() => prompt_runtime(printer.lang(), major, &matches),
            _ => Err(anyhow!(crate::i18n::pick(
                printer.lang(),
                format!(
                    "大版本 {major} 有多个运行时，请用 --dir 指定路径，如:\n  mcsup java uninstall --dir \"{}\"",
                    matches[0].path.display()
                ),
                format!(
                    "major version {major} has several runtimes; pass --dir, e.g.:\n  mcsup java uninstall --dir \"{}\"",
                    matches[0].path.display()
                ),
            ))),
        };
    }
    let as_path = Path::new(target);
    if as_path.exists() || target.contains('/') || target.contains('\\') {
        return Ok(as_path.to_path_buf());
    }
    Err(anyhow!(crate::i18n::pick(
        printer.lang(),
        format!("无法识别的目标: {target}（大版本号或 --dir 路径）"),
        format!("unrecognized target: {target} (a major version or a --dir path)"),
    )))
}

/// 缺目标时：列出已注册运行时供选择，返回其路径
fn prompt_registered_runtime(service: &JavaService, lang: Lang) -> Result<PathBuf> {
    let runtimes = service.list().context(crate::i18n::pick(
        lang,
        "列出 Java 失败",
        "failed to list Java runtimes",
    ))?;
    if runtimes.is_empty() {
        return Err(anyhow!(crate::i18n::pick(
            lang,
            "尚无已注册的 Java 运行时，可先运行 mcsup java scan",
            "no registered Java runtimes; run mcsup java scan first",
        )));
    }
    let choices: Vec<String> = runtimes
        .iter()
        .map(|rt| {
            format!(
                "{} · {} · {} ({})",
                rt.major,
                rt.version,
                rt.path.display(),
                if rt.managed {
                    "mcsup".to_owned()
                } else {
                    crate::i18n::pick(lang, "系统", "system")
                }
            )
        })
        .collect();
    let selected = Select::new(
        &crate::i18n::pick(
            lang,
            "选择要卸载的 Java 运行时",
            "select the Java runtime to uninstall",
        ),
        choices,
    )
    .with_page_size(12)
    .prompt()
    .context(crate::i18n::pick(lang, "选择取消", "selection cancelled"))?;
    runtimes
        .iter()
        .find(|rt| selected.starts_with(&format!("{} · ", rt.major)))
        .map(|rt| rt.path.clone())
        .ok_or_else(|| {
            anyhow!(crate::i18n::pick(
                lang,
                "未匹配到所选运行时",
                "the selected runtime was not matched",
            ))
        })
}

/// 多个运行时命中大版本时，TTY 下交互选择
fn prompt_runtime(lang: Lang, major: JavaMajor, matches: &[JavaRuntime]) -> Result<PathBuf> {
    let choices: Vec<String> = matches
        .iter()
        .map(|rt| {
            format!(
                "{} ({}) {}",
                rt.path.display(),
                rt.vendor,
                if rt.managed {
                    "mcsup".to_owned()
                } else {
                    crate::i18n::pick(lang, "系统", "system")
                }
            )
        })
        .collect();
    let label = crate::i18n::pick(
        lang,
        format!("大版本 {major} 有多个运行时，选择要卸载的"),
        format!("major version {major} has several runtimes; pick one to uninstall"),
    );
    let selected = Select::new(&label, choices)
        .prompt()
        .context(crate::i18n::pick(lang, "选择取消", "selection cancelled"))?;
    // 第一个匹配即用户选中的（label 与 matches 一一对应）
    matches
        .iter()
        .find(|rt| selected.starts_with(&rt.path.display().to_string()))
        .map(|rt| rt.path.clone())
        .ok_or_else(|| {
            anyhow!(crate::i18n::pick(
                lang,
                "未匹配到所选运行时",
                "the selected runtime was not matched",
            ))
        })
}
