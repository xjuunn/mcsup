use crate::bootstrap;
use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use crate::prompts;
use anyhow::{Context, Result};
use comfy_table::{Cell, Color, Table};
use mcsup::domain::server::status::ServerStatus;
use mcsup::domain::server::version::{ServerVariant, VersionId};
use mcsup::domain::workspace::WorkspaceId;
use mcsup::protocol::cli::{CreateRow, RemoveJson, ServerRow};
use mcsup::service::{ServerRegistry, ServerRegistryError, WorkspaceError, WorkspaceService};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

use super::install::{download_jar, pick_version};

/// 表格状态列着色用，不进 JSON（JSON 行类型在 `protocol::cli`）
#[derive(Serialize)]
struct ServerRowView<'a> {
    #[serde(flatten)]
    row: ServerRow<'a>,
    /// 是否进程尚存
    #[serde(skip)]
    running: bool,
}

pub(crate) fn open_registry() -> Result<std::sync::Arc<ServerRegistry>> {
    bootstrap::open_registry()
}

/// 缺省工作区选择器：flag > `MCSUP_WORKSPACE` 环境变量 > 当前工作区
///
/// 环境变量在入口统一解析成显式选择器传给 service，service 只认一种口径。
#[must_use]
pub(crate) fn workspace_selector(flag: Option<&str>) -> Option<String> {
    flag.map(str::to_owned).or_else(|| {
        std::env::var("MCSUP_WORKSPACE")
            .ok()
            .filter(|s| !s.trim().is_empty())
    })
}

fn workspace_names() -> Result<HashMap<WorkspaceId, String>> {
    let store = bootstrap::open_store()?;
    let svc = WorkspaceService::new(store.clone(), store);
    Ok(svc
        .list()?
        .into_iter()
        .map(|w| (w.id, w.name.as_str().to_owned()))
        .collect())
}

pub(crate) fn list(printer: Printer) -> Result<()> {
    let reg = open_registry()?;
    let all = reg.list().context("列出服务器失败")?;
    let ws_names = workspace_names()?;
    let statuses = super::lifecycle::lifecycle_service()?.statuses()?;
    let rows: Vec<ServerRowView<'_>> = all
        .iter()
        .map(|srv| {
            // 单台探测失败记 unknown，不拖死整个列表
            let probe = statuses.get(srv.meta.name.as_str());
            let view = probe.and_then(|p| p.as_ref().ok()).and_then(Option::as_ref);
            let unreachable = probe.is_some_and(Result::is_err);
            ServerRowView {
                row: ServerRow {
                    name: srv.meta.name.as_str(),
                    workspace: ws_names
                        .get(&srv.meta.workspace_id)
                        .cloned()
                        .unwrap_or_else(|| srv.meta.workspace_id.as_str().to_owned()),
                    version: srv.meta.version.as_str(),
                    variant: srv.meta.variant.as_str(),
                    path: srv.dir.display().to_string(),
                    jar: srv.meta.jar.as_ref().map(|j| j.path.display().to_string()),
                    linked: srv.linked,
                    status: if unreachable {
                        "unknown".to_owned()
                    } else {
                        view.map_or_else(
                            || ServerStatus::Stopped.as_str().to_owned(),
                            |s| s.status.as_str().to_owned(),
                        )
                    },
                },
                running: view.is_some_and(|s| s.status.is_alive()),
            }
        })
        .collect();
    printer.print_rows(
        &rows,
        "尚无服务器，先运行 mcsup server create",
        || {
            let mut table = Table::new();
            table.set_header(["名称", "工作区", "版本", "变体", "状态", "路径"]);
            for row in &rows {
                let status = if row.running {
                    Cell::new("运行中").fg(Color::Green)
                } else {
                    Cell::new(&row.row.status)
                };
                table.add_row([
                    Cell::new(row.row.name),
                    Cell::new(&row.row.workspace),
                    Cell::new(row.row.version),
                    Cell::new(row.row.variant),
                    status,
                    Cell::new(&row.row.path),
                ]);
            }
            table
        },
    )
}

/// `server create`：非交互走参数直通（缺必选参数 → 退出码 2），
/// 交互且缺参进入向导（工作区兜底 → 名称 → 变体 → 版本 → 下载）
///
/// # Errors
///
/// 用法错误（2）、工作区 / 服务器冲突（3 / 4）、来源未支持、下载失败时返回错误
pub(crate) async fn create(
    name: Option<&str>,
    version: Option<&str>,
    variant: Option<&str>,
    workspace: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let interactive = printer.is_interactive();

    // 1. 用法闸（先于落库与联网，快速失败）
    if name.is_none() && !interactive {
        return Err(AppDiagnostic::usage(
            "server create 缺少服务器名称",
            Some("非交互模式：mcsup server create <name> --version <VER>".to_owned()),
        )
        .into());
    }
    if version.is_none() && !interactive {
        return Err(AppDiagnostic::usage(
            "server create 缺少 --version",
            Some("非交互模式必填；交互终端下可省略后从版本列表选择".to_owned()),
        )
        .into());
    }

    // 2. 变体能力：flag / 非交互默认先解析（显式未支持 → 退出码 2）
    let available = bootstrap::available_variants()?;
    let variant = match variant {
        Some(raw) => Some(resolve_flag_variant(raw, &available)?),
        None if !interactive => Some(default_variant(&available)?),
        None => None,
    };

    // 3. 工作区：flag > 环境变量 > 当前；无当前且交互 → 复用 init 向导
    let selector = workspace_selector(workspace);
    if !ensure_workspace(selector.as_deref(), interactive, printer)? {
        return Ok(());
    }

    let reg = open_registry()?;

    // 4. 名称（交互重输直到不重复）
    let name = if let Some(name) = name {
        (*name).to_owned()
    } else {
        prompts::step(printer, 1, 3, "服务器名称");
        prompt_unique_name(&reg, printer)?
    };

    // 5. 变体（交互只列可用；单可用项直接采纳）
    let variant = match variant {
        Some(variant) => variant,
        None => pick_variant(&available, printer)?,
    };

    // 6. 版本（交互拉列表选择；来源随变体能力匹配）
    let source = bootstrap::source_for(variant)?;
    let version_id = if let Some(version) = version {
        VersionId::new(version)
    } else {
        prompts::step(printer, 3, 3, "服务端版本");
        pick_version(source.as_ref(), variant, printer).await?
    };

    // Java 版本要求不在此预取：下载核心时 `install_jar` 会随元数据一并回写，
    // 预取会重复打一遍 manifest/details 网络请求
    let srv = reg
        .create(selector.as_deref(), &name, &version_id, variant, None)
        .with_context(|| format!("创建服务器 {name} 失败"))?;

    // 7. 下载核心：失败保留已注册结果，提示用 install 重试
    if let Err(err) = download_jar(&name, &version_id, source, printer).await {
        printer.print_warning(&format!(
            "服务器已创建但核心下载失败，可执行 mcsup server install {name} 重试"
        ));
        return Err(err);
    }

    printer.print_outcome(
        &format!(
            "已创建: {} {} → {}",
            srv.meta.name,
            srv.meta.version,
            srv.dir.display()
        ),
        &CreateRow {
            name: srv.meta.name.as_str(),
            version: srv.meta.version.as_str(),
            variant: srv.meta.variant.as_str(),
            path: srv.dir.display().to_string(),
            linked: None,
        },
    )?;
    printer.print_section(&format!("完成。运行 mcsup server start {name} 启动"));
    Ok(())
}

/// 显式 `--variant`：解析 + 能力校验；未知 / 未支持都判用法错误（退出码 2）
fn resolve_flag_variant(raw: &str, available: &[ServerVariant]) -> Result<ServerVariant> {
    let parsed = ServerVariant::parse(raw).map_err(|_| {
        anyhow::Error::from(AppDiagnostic::usage(
            format!("未知变体: {raw}"),
            Some(format!("可用变体：{}", bootstrap::join_variants(available))),
        ))
    })?;
    if !available.contains(&parsed) {
        return Err(AppDiagnostic::usage(
            format!("暂未支持变体 {raw}"),
            Some(format!("当前可用：{}", bootstrap::join_variants(available))),
        )
        .into());
    }
    Ok(parsed)
}

/// 非交互缺省变体：优先 vanilla（历史缺省值），否则取首个可用
fn default_variant(available: &[ServerVariant]) -> Result<ServerVariant> {
    available
        .iter()
        .copied()
        .find(|variant| *variant == ServerVariant::Vanilla)
        .or_else(|| available.first().copied())
        .ok_or_else(|| anyhow::anyhow!("没有可用的服务端来源（provider 未注册）"))
}

/// 交互变体选择：仅列可用；单可用项免提问（不打步骤回显）
fn pick_variant(available: &[ServerVariant], printer: Printer) -> Result<ServerVariant> {
    if let [only] = available {
        return Ok(*only);
    }
    prompts::step(printer, 2, 3, "服务端变体");
    let labels: Vec<String> = available.iter().map(|v| v.as_str().to_owned()).collect();
    let picked = prompts::select_filter("服务端变体", &labels)?;
    variant_from_label(&picked, available).ok_or_else(|| anyhow::anyhow!("无效变体: {picked}"))
}

/// 选择标签 → 变体（标签即 `as_str`，按全名回映）
fn variant_from_label(picked: &str, available: &[ServerVariant]) -> Option<ServerVariant> {
    available
        .iter()
        .find(|variant| variant.as_str() == picked)
        .copied()
}

/// 名称重输：语法由 prompts 校验，重复（注册表已有）时警示后重问
fn prompt_unique_name(reg: &ServerRegistry, printer: Printer) -> Result<String> {
    loop {
        let candidate = prompts::input_name("服务器名称", "server")?;
        match reg.get(&candidate) {
            Err(ServerRegistryError::NotFound(_)) => return Ok(candidate),
            Err(err) => return Err(err.into()),
            Ok(_) => printer.print_warning(&format!("名称 {candidate} 已存在，换一个")),
        }
    }
}

/// 工作区就绪：显式选择器早校验；无当前且交互 → 复用 `workspace init` 向导
///
/// 返回 `false` 表示向导未完成（取消 / 未注册），调用方应静默中止（已出提示）。
fn ensure_workspace(selector: Option<&str>, interactive: bool, printer: Printer) -> Result<bool> {
    let store = bootstrap::open_store()?;
    let svc = WorkspaceService::new(store.clone(), store);
    if let Some(sel) = selector {
        svc.resolve(sel)?;
        return Ok(true);
    }
    match svc.current_workspace() {
        Ok(_) => Ok(true),
        Err(WorkspaceError::NoCurrent) if interactive => {
            printer.print_section("尚未有工作区，先创建工作区");
            super::workspace::init(&svc, None, None, printer)?;
            if svc.current_workspace().is_ok() {
                Ok(true)
            } else {
                printer.print_section(
                    "工作区未就绪，创建中止（可重新执行 mcsup workspace init / use 后再创建）",
                );
                Ok(false)
            }
        }
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn link(
    path: &Path,
    name: Option<&str>,
    workspace: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let reg = open_registry()?;
    let selector = workspace_selector(workspace);
    let out = reg
        .link(path, name, selector.as_deref())
        .with_context(|| format!("注册服务器目录 {} 失败", path.display()))?;
    // 工作区内目录走就地注册（linked=false），外部目录才建符号链接
    printer.print_outcome(
        &if out.server.linked {
            format!(
                "已链接: {} → {}（目标 {}）",
                out.server.meta.name,
                out.server.dir.display(),
                out.target.display()
            )
        } else {
            format!(
                "已注册工作区内目录: {} → {}（未建符号链接）",
                out.server.meta.name,
                out.server.dir.display()
            )
        },
        &CreateRow {
            name: out.server.meta.name.as_str(),
            version: out.server.meta.version.as_str(),
            variant: out.server.meta.variant.as_str(),
            path: out.server.dir.display().to_string(),
            linked: Some(out.server.linked),
        },
    )
}

pub(crate) fn remove(name: &str, purge: bool, printer: Printer) -> Result<()> {
    let reg = open_registry()?;
    // 先看一眼：链接服务器 + purge 会被 service 拒绝，这里提前给出更友好的确认
    let srv = reg
        .get(name)
        .with_context(|| format!("服务器 {name} 不存在"))?;
    if purge && srv.linked {
        printer.print_warning(&format!(
            "{name} 是链接服务器，--purge 会拒绝执行（目标在工作区外，无法安全删除）"
        ));
        // 仍传给 service，由其拒绝并给出统一错误
    }
    // 删了不可恢复：可交互时问一句，默认不删；`--purge` 是显式指令，不再问
    let purge = if purge || !printer.is_interactive() {
        purge
    } else {
        let confirmed = prompts::confirm_yes(
            &format!("删除 {name} ({}) 的磁盘数据?", srv.dir.display()),
            false,
        )
        .context("已取消注销")?;
        if confirmed {
            // 同意删盘后再上一道闸：红色警示 + 手输服务器名称，防误删存档与备份
            printer.print_warning(&format!(
                "警告: 将永久删除 {name} ({}) 中的所有存档、备份等文件，此操作无法恢复",
                srv.dir.display()
            ));
            prompts::confirm_by_name(&format!("输入服务器名称 {name} 以确认删除"), name)?;
        }
        confirmed
    };
    // 链接服务器不会真删目录，不必退出来
    if purge && !srv.linked {
        super::chdir_out_of(&srv.dir);
    }
    // 删除经 lifecycle 编排：运行中拒绝（存档保护）、cwd 在目录内拒绝，
    // 判定收敛在 service
    let cwd = std::env::current_dir().context("获取当前工作目录失败")?;
    let out = super::lifecycle::lifecycle_service()?.remove_server(name, purge, &cwd)?;
    let srv = &out.server;
    let disk_purged = out.disk_purged();
    printer.print_outcome(
        &if disk_purged {
            format!("已删除并注销: {}", srv.meta.name)
        } else if srv.linked {
            format!("已移除链接并注销: {}（目标目录保留）", srv.meta.name)
        } else if purge {
            // 请求了删盘但目录本就不在：不能说「已删除」，用户会以为删掉了存档
            format!("已注销（磁盘数据本就不存在）: {}", srv.meta.name)
        } else {
            format!("已注销（磁盘数据保留）: {}", srv.meta.name)
        },
        &RemoveJson {
            name: srv.meta.name.as_str().to_owned(),
            path: srv.dir.display().to_string(),
            purged: purge,
            disk_purged,
            linked: srv.linked,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_flag_variant_gates_by_capability_with_usage_exit() {
        let available = vec![ServerVariant::Vanilla];
        assert_eq!(
            resolve_flag_variant("vanilla", &available).expect("可用变体"),
            ServerVariant::Vanilla
        );

        // 拼写未知 → 用法错误（2），并列出可用变体（help 经 miette 渲染携带）
        let unknown = resolve_flag_variant("paperx", &available).expect_err("未知变体");
        assert_eq!(Printer::exit_code(&unknown), 2);
        let msg = unknown.to_string();
        assert!(msg.contains("未知变体"), "{msg}");
        let rendered = format!(
            "{:?}",
            miette::Report::new(unknown.downcast::<AppDiagnostic>().expect("usage 诊断"))
        );
        assert!(rendered.contains("vanilla"), "{rendered}");

        // 能解析但未注册 provider → 同判用法错误（2），文案区分
        let unsupported = resolve_flag_variant("paper", &available).expect_err("未支持");
        assert_eq!(Printer::exit_code(&unsupported), 2);
        let msg = unsupported.to_string();
        assert!(msg.contains("暂未支持"), "{msg}");
        let rendered = format!(
            "{:?}",
            miette::Report::new(unsupported.downcast::<AppDiagnostic>().expect("usage 诊断"))
        );
        assert!(rendered.contains("vanilla"), "{rendered}");
    }

    #[test]
    fn default_variant_prefers_vanilla_then_first_available() {
        assert_eq!(
            default_variant(&[ServerVariant::Vanilla, ServerVariant::Paper]).expect("默认"),
            ServerVariant::Vanilla
        );
        assert_eq!(
            default_variant(&[ServerVariant::Paper, ServerVariant::Fabric]).expect("首个"),
            ServerVariant::Paper
        );
        assert!(
            default_variant(&[])
                .expect_err("空能力集必须报错")
                .to_string()
                .contains("provider")
        );
    }

    /// 单可用项免提问；标签（`as_str`）回映变体，未命中返回 None
    #[test]
    fn variant_from_label_maps_back_by_full_name() {
        let available = vec![ServerVariant::Vanilla, ServerVariant::Fabric];
        assert_eq!(
            variant_from_label("fabric", &available),
            Some(ServerVariant::Fabric)
        );
        assert_eq!(
            variant_from_label("VANILLA", &available),
            None,
            "全名匹配区分大小写，选择器已归一到注册名"
        );
        assert_eq!(variant_from_label("paper", &available), None);
    }
}
