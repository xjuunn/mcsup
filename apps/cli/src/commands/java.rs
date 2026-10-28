use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use crate::progress::{InstallSession, ScanProgress, TaskSpinner};
use anyhow::{Context, Result, anyhow};
use comfy_table::{ContentArrangement, Table};
use inquire::Select;
use mcsup::domain::java::{JavaMajor, JavaPackageType, JavaRuntime, JavaVendor};
use mcsup::port::JavaRuntimeRepository as _;
use mcsup::protocol::cli::{JavaInstallJson, JavaRow, JavaScanJson, JavaUninstallJson};
use mcsup::service::{InstallStage, UninstallOutcome};
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
    let members = bootstrap::remote_members(&cfg);
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
    origin: Option<&str>,
    printer: Printer,
) -> Result<()> {
    if let Some(origin) = remote_origin(origin, printer.lang())? {
        return install_on(&origin, version, jre, vendor, printer);
    }
    install_with_outcome(version, jre, vendor, printer).await?;
    Ok(())
}

/// 解析 `--origin`：本地来源（或没给）返回 `None`
fn remote_origin(raw: Option<&str>, lang: Lang) -> Result<Option<mcsup::port::Origin>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let origin = mcsup::port::Origin::parse(raw).map_err(|err| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("--origin 取值非法: {raw}"),
                format!("invalid --origin value: {raw}"),
            ),
            Some(err.to_string()),
        )
    })?;
    Ok((!origin.is_local()).then_some(origin))
}

/// 在对端安装 Java：查询来源、下载、解压、登记全部发生在对端
///
/// 客户端只搬大版本 / 包类型 / 厂商，结果取对端回来列表里刚登记的那一项。
pub(crate) fn install_on(
    origin: &mcsup::port::Origin,
    version: Option<&str>,
    jre: bool,
    vendor: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let cfg = crate::config::ConfigFile::locate(None).load()?;
    let members = bootstrap::remote_members(&cfg);
    let member = super::origin::member(&members, origin, lang)?.ok_or_else(|| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("远端 {} 未就绪（缺凭证或指纹）", origin.as_str()),
                format!(
                    "remote {} is not ready (missing credential or fingerprint)",
                    origin.as_str()
                ),
            ),
            None,
        )
    })?;
    // 大版本列表是领域知识，两端共用；交互选择在本机完成，无需对端往返
    let major = if let Some(raw) = version {
        parse_major(lang, raw)?
    } else {
        prompt_major(printer)?
    };
    let spinner = TaskSpinner::new(
        crate::i18n::pick(
            lang,
            format!("在 {} 安装 Java {major}…", member.name),
            format!("installing Java {major} on {}…", member.name),
        ),
        printer.is_human(),
    );
    let result = member.java.install(major.as_i64(), jre, vendor);
    spinner.finish();
    let runtimes = result.with_context(|| {
        crate::i18n::pick(
            lang,
            format!("在 {} 安装 Java 失败", member.name),
            format!("failed to install Java on {}", member.name),
        )
    })?;
    let installed = pick_installed(&runtimes, major).ok_or_else(|| {
        anyhow!(crate::i18n::pick(
            lang,
            format!("{} 报告安装成功但没有 Java {major} 的登记记录", member.name),
            format!(
                "{} reported success but has no Java {major} registration",
                member.name
            ),
        ))
    })?;
    printer.print_outcome(
        &crate::i18n::pick(
            lang,
            format!(
                "已在 {} 安装: Java {} ({}) → {}",
                member.name,
                installed.version,
                installed.vendor,
                installed.path.display()
            ),
            format!(
                "installed on {}: Java {} ({}) → {}",
                member.name,
                installed.version,
                installed.vendor,
                installed.path.display()
            ),
        ),
        &JavaInstallJson {
            major: installed.major.as_i64(),
            version: installed.version.as_str(),
            vendor: installed.vendor.as_str(),
            path: installed.path.display().to_string(),
        },
    )
}

/// 安装结果里挑刚登记的那项：managed 优先，其次同大版本的第一项
fn pick_installed(runtimes: &[JavaRuntime], major: JavaMajor) -> Option<&JavaRuntime> {
    runtimes
        .iter()
        .find(|rt| rt.major == major && rt.managed)
        .or_else(|| runtimes.iter().find(|rt| rt.major == major))
}

pub(crate) fn uninstall(
    target: Option<&str>,
    dir: Option<&Path>,
    origin: Option<&str>,
    printer: Printer,
) -> Result<()> {
    if let Some(origin) = remote_origin(origin, printer.lang())? {
        return uninstall_on(&origin, target, dir, printer);
    }
    let service = bootstrap::open_java_service()?;
    let runtimes = service.list().context(crate::i18n::pick(
        printer.lang(),
        "列出 Java 失败",
        "failed to list Java runtimes",
    ))?;
    let path = resolve_target(&runtimes, target, dir, printer)?;
    let outcome = service.uninstall(&path).context(crate::i18n::pick(
        printer.lang(),
        "卸载失败",
        "uninstall failed",
    ))?;
    let (message, row) = report_uninstall(&outcome, printer.lang());
    printer.print_outcome(&message, &row)
}

/// 在对端卸载 Java：登记表与文件都在对端，客户端只搬目标路径
///
/// 目标解析用对端返回的运行时列表（同一份 [`resolve_target`]），因此
/// `--dir`、大版本、裸路径三种写法在本地与远端行为一致。
fn uninstall_on(
    origin: &mcsup::port::Origin,
    target: Option<&str>,
    dir: Option<&Path>,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let cfg = crate::config::ConfigFile::locate(None).load()?;
    let members = bootstrap::remote_members(&cfg);
    let member = super::origin::member(&members, origin, lang)?.ok_or_else(|| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                "本地来源不需要远端卸载",
                "local origin does not need a remote uninstall",
            ),
            None,
        )
    })?;
    let runtimes = member.java.list().with_context(|| {
        crate::i18n::pick(
            lang,
            format!("列出 {} 上的 Java 失败", member.name),
            format!("failed to list Java runtimes on {}", member.name),
        )
    })?;
    let path = resolve_target(&runtimes, target, dir, printer)?;
    let spinner = TaskSpinner::new(
        crate::i18n::pick(
            lang,
            format!("正在从 {} 卸载 {}…", member.name, path.display()),
            format!("uninstalling {} from {}…", path.display(), member.name),
        ),
        printer.is_human(),
    );
    let result = member.java.uninstall(&path);
    spinner.finish();
    let (path, managed) = result.with_context(|| {
        crate::i18n::pick(
            lang,
            format!("在 {} 卸载 Java 失败", member.name),
            format!("failed to uninstall Java on {}", member.name),
        )
    })?;
    let (message, row) = if managed {
        (
            crate::i18n::pick(
                lang,
                format!(
                    "已在 {} 删除 mcsup 管理的 Java: {}",
                    member.name,
                    path.display()
                ),
                format!(
                    "deleted the mcsup-managed Java on {}: {}",
                    member.name,
                    path.display()
                ),
            ),
            JavaUninstallJson {
                path: path.display().to_string(),
                kind: "managed",
            },
        )
    } else {
        (
            crate::i18n::pick(
                lang,
                format!(
                    "已在 {} 从注册表移除（文件保留）: {}",
                    member.name,
                    path.display()
                ),
                format!(
                    "unregistered on {} (files kept): {}",
                    member.name,
                    path.display()
                ),
            ),
            JavaUninstallJson {
                path: path.display().to_string(),
                kind: "system",
            },
        )
    };
    printer.print_outcome(&message, &row)
}

/// 卸载结果 → 文案与 JSON（本地 / 远端共用同一套措辞）
fn report_uninstall(outcome: &UninstallOutcome, lang: Lang) -> (String, JavaUninstallJson) {
    match outcome {
        UninstallOutcome::RemovedManaged { path } => (
            crate::i18n::pick(
                lang,
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
                lang,
                format!("已从注册表移除（文件保留）: {}", path.display()),
                format!("unregistered (files kept): {}", path.display()),
            ),
            JavaUninstallJson {
                path: path.display().to_string(),
                kind: "system",
            },
        ),
    }
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
    runtimes: &[JavaRuntime],
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
            return prompt_registered_runtime(runtimes, printer.lang());
        }
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                printer.lang(),
                "java uninstall 缺少目标，请指定大版本号或 --dir <路径>",
                "java uninstall requires a target: pass a major version or --dir <path>",
            ),
            Some(crate::i18n::pick(
                printer.lang(),
                "例如：mcsup java uninstall 17 或 mcsup java uninstall --dir \"…/Contents/Home\"",
                "e.g. mcsup java uninstall 17 or mcsup java uninstall --dir \"…/Contents/Home\"",
            )),
        )
        .into());
    };
    if let Ok(major) = parse_major(printer.lang(), target) {
        let matches: Vec<_> = runtimes.iter().filter(|rt| rt.major == major).collect();
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
fn prompt_registered_runtime(runtimes: &[JavaRuntime], lang: Lang) -> Result<PathBuf> {
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
        choices.clone(),
    )
    .with_page_size(12)
    .prompt()
    .context(crate::i18n::pick(lang, "选择取消", "selection cancelled"))?;
    pick_selected(runtimes, &choices, &selected)
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
fn prompt_runtime(lang: Lang, major: JavaMajor, matches: &[&JavaRuntime]) -> Result<PathBuf> {
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
    let selected = Select::new(&label, choices.clone())
        .prompt()
        .context(crate::i18n::pick(lang, "选择取消", "selection cancelled"))?;
    // 标签与 matches 一一对应：按**完整标签**取回同一位置，不能用路径前缀
    // 反查——`/java/17` 会遮住 `/java/17.0.9`。
    pick_selected(matches, &choices, &selected)
        .map(|rt| rt.path.clone())
        .ok_or_else(|| {
            anyhow!(crate::i18n::pick(
                lang,
                "未匹配到所选运行时",
                "the selected runtime was not matched",
            ))
        })
}

/// 标签与数据一一对应时，按选中标签取回同一位置的数据
///
/// 不能用 `starts_with` 反查：同大版本（`17 · …`）或路径前缀
/// （`/java/17` 与 `/java/17.0.9`）都会把选择映射到前一条。
fn pick_selected<'a, T>(items: &'a [T], labels: &[String], selected: &str) -> Option<&'a T> {
    labels
        .iter()
        .position(|label| label == selected)
        .and_then(|index| items.get(index))
}

#[cfg(test)]
mod tests {
    use super::pick_selected;

    /// 同大版本的两条运行时必须按完整标签映射，不能都落到第一条
    #[test]
    fn selected_runtime_maps_by_exact_label_not_major_prefix() {
        let items = ["system-17", "managed-17"];
        let labels = vec![
            "17 · 17.0.9 · /java/17 (系统)".to_owned(),
            "17 · 17.0.11 · /java/17.0.11 (mcsup)".to_owned(),
        ];
        assert_eq!(
            pick_selected(&items, &labels, &labels[1]),
            Some(&"managed-17")
        );
    }

    /// 路径前缀不能遮住更长的路径（`/java/17` 与 `/java/17.0.9`）
    #[test]
    fn selected_path_prefix_does_not_shadow_longer_path() {
        let items = ["/java/17", "/java/17.0.9"];
        let labels = vec![
            "/java/17 · 17.0.9 (系统)".to_owned(),
            "/java/17.0.9 · 17.0.9 (mcsup)".to_owned(),
        ];
        assert_eq!(
            pick_selected(&items, &labels, &labels[1]),
            Some(&"/java/17.0.9")
        );
    }
}
