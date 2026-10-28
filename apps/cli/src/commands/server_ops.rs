use crate::bootstrap;
use crate::diagnostic::AppDiagnostic;
use crate::locale::Lang;
use crate::output::Printer;
use crate::progress::TaskSpinner;
use crate::prompts;
use anyhow::{Context, Result};
use comfy_table::{Cell, Color, Table};
use mcsup::domain::server::meta::ServerName;
use mcsup::domain::server::status::ServerStatus;
use mcsup::domain::server::version::{ServerVariant, VersionId};
use mcsup::domain::workspace::{WorkspaceId, WorkspaceName};
use mcsup::port::ServerJarSource;
use mcsup::protocol::cli::{CreateRow, RemoveJson, ServerRow};
use mcsup::service::{
    InitOutcome, ServerRegistry, ServerRegistryError, WorkspaceError, WorkspaceService,
    absolute_path, resolve_selector,
};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::install::{download_jar, pick_version, select_version};

/// 表格状态列着色用，不进 JSON（JSON 行类型在 `protocol::cli`）
#[derive(Serialize)]
struct ServerRowView<'a> {
    #[serde(flatten)]
    row: ServerRow<'a>,
    /// 是否进程尚存
    #[serde(skip)]
    running: bool,
}

pub(crate) fn open_registry() -> Result<Arc<ServerRegistry>> {
    bootstrap::open_registry_with_remotes()
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

/// 缺必填服务器名：交互下从已注册服务器中选择；否则报用法错误（退出码 2）
///
/// 全局交互约定：必填参数缺失只在 human + TTY 下补参，非交互 / JSON 下不等待
/// 输入。`--yes` 不跳过这里——它只跳过确认式提问。
pub(crate) fn require_server_name(
    name: Option<&str>,
    command: &str,
    printer: Printer,
) -> Result<String> {
    if let Some(name) = name {
        return super::origin::resolve_server_name(name, printer);
    }
    if !printer.is_interactive() {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                printer.lang(),
                format!("{command} 缺少服务器名称"),
                format!("{command} requires a server name"),
            ),
            Some(crate::i18n::pick(
                printer.lang(),
                format!("非交互模式：mcsup {command} <名称>"),
                format!("non-interactive: mcsup {command} <name>"),
            )),
        )
        .into());
    }
    let reg = open_registry()?;
    let servers = reg.list().context(crate::i18n::pick(
        printer.lang(),
        "列出服务器失败",
        "failed to list servers",
    ))?;
    if servers.is_empty() {
        return Err(anyhow::anyhow!(crate::i18n::pick(
            printer.lang(),
            "尚无服务器，先运行 mcsup server create 创建",
            "no servers yet; run `mcsup server create` first"
        )));
    }
    let names: Vec<String> = servers
        .iter()
        .map(|srv| srv.meta.name.as_str().to_owned())
        .collect();
    let picked = prompts::select_filter(
        printer.lang(),
        &crate::i18n::pick(printer.lang(), "选择服务器", "select a server"),
        &names,
    )?;
    Ok(picked)
}

/// 缺必填文本参数（如 `server send` 的命令体）：交互下输入，否则用法错误
pub(crate) fn require_text(
    value: Option<String>,
    missing_message: &str,
    usage_hint: &str,
    printer: Printer,
) -> Result<String> {
    if let Some(value) = value {
        return Ok(value);
    }
    if !printer.is_interactive() {
        return Err(AppDiagnostic::usage(missing_message, Some(usage_hint.to_owned())).into());
    }
    prompts::input_text(
        printer.lang(),
        &crate::i18n::pick(printer.lang(), "输入内容", "input"),
        "",
        &crate::i18n::pick(
            printer.lang(),
            "回车提交；Ctrl+C 取消",
            "enter submits; Ctrl+C cancels",
        ),
    )
}

pub(crate) fn list(filter: &mcsup::service::OriginFilter, printer: Printer) -> Result<()> {
    let reg = bootstrap::open_registry_with_remotes()?;
    let all = reg.list_sourced().context(crate::i18n::pick(
        printer.lang(),
        "列出服务器失败",
        "failed to list servers",
    ))?;
    let all: Vec<_> = all
        .into_iter()
        .filter(|sourced| filter.matches(&sourced.origin))
        .collect();
    let ws_names = workspace_names()?;
    let statuses = super::lifecycle::lifecycle_service()?.statuses()?;
    let rows: Vec<ServerRowView<'_>> = all
        .iter()
        .map(|sourced| {
            let srv = &sourced.value;
            // 单台探测失败记 unknown，不拖死整个列表
            let probe = statuses.get(srv.meta.name.as_str());
            let view = probe.and_then(|p| p.as_ref().ok()).and_then(Option::as_ref);
            let unreachable = probe.is_some_and(Result::is_err);
            ServerRowView {
                row: ServerRow {
                    origin: super::origin::label(&sourced.origin),
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
    let lang = printer.lang();
    printer.print_rows(
        &rows,
        crate::i18n::text(
            lang,
            "尚无服务器，先运行 mcsup server create",
            "no servers yet; run `mcsup server create` first",
        ),
        || {
            let mut table = Table::new();
            table.set_header([
                crate::i18n::text(lang, "名称", "name"),
                crate::i18n::text(lang, "来源", "origin"),
                crate::i18n::text(lang, "工作区", "workspace"),
                crate::i18n::text(lang, "版本", "version"),
                crate::i18n::text(lang, "变体", "variant"),
                crate::i18n::text(lang, "状态", "status"),
                crate::i18n::text(lang, "路径", "path"),
            ]);
            for row in &rows {
                let status = if row.running {
                    Cell::new(crate::i18n::text(lang, "运行中", "running")).fg(Color::Green)
                } else {
                    Cell::new(&row.row.status)
                };
                table.add_row([
                    Cell::new(row.row.name),
                    Cell::new(&row.row.origin),
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

/// `server create` 的入参：必填项都是 `Option`，缺哪项补哪项
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CreateArgs<'a> {
    /// 服务器名（位置参数）
    pub(crate) name: Option<&'a str>,
    /// 目标来源：`local`（缺省）或远端名
    pub(crate) origin: Option<&'a str>,
    /// 服务端版本（`--version`，接受 `latest` / `lts` / `latest-snapshot`）
    pub(crate) version: Option<&'a str>,
    /// 服务端变体（`--variant`）
    pub(crate) variant: Option<&'a str>,
    /// 目标工作区（`--workspace`，名称或路径都接受）
    pub(crate) workspace: Option<&'a str>,
    /// 与 `--workspace` 配合：要新建的工作区名称
    pub(crate) workspace_name: Option<&'a str>,
    /// 与 `--workspace` 配合：要新建的工作区目录
    pub(crate) workspace_dir: Option<&'a Path>,
    /// 整合包项目 ID / slug（`--modpack`，隐含 `--variant modrinth-modpack`）
    pub(crate) modpack: Option<&'a str>,
}

/// 空选择器等价于「未指定工作区」
fn non_empty_selector(selector: String) -> Option<String> {
    (!selector.is_empty()).then_some(selector)
}

/// `--variant` 是否指向整合包变体（含 `modrinth` 短名）
fn is_modpack_flag(raw: &str) -> bool {
    raw.eq_ignore_ascii_case("modrinth-modpack") || raw.eq_ignore_ascii_case("modrinth")
}

/// `server create`：非交互走参数直通（缺必选参数 → 退出码 2），
/// 交互且缺参进入向导（工作区兜底 → 名称 → 变体 → 版本 → 下载）
///
/// # Errors
///
/// 用法错误（2）、工作区 / 服务器冲突（3 / 4）、来源未支持、下载失败时返回错误
#[allow(clippy::too_many_lines)]
pub(crate) async fn create(args: CreateArgs<'_>, printer: Printer) -> Result<()> {
    let CreateArgs {
        name,
        origin,
        version,
        variant,
        workspace,
        workspace_name,
        workspace_dir,
        modpack,
    } = args;
    // 远端落点：远端只做「登记 + 核心安装由对端负责」，本地向导不参与
    if let Some(origin) = origin {
        let origin = mcsup::port::Origin::parse(origin).map_err(|err| {
            AppDiagnostic::usage(
                crate::i18n::pick(
                    printer.lang(),
                    format!("--origin 取值非法: {origin}"),
                    format!("invalid --origin value: {origin}"),
                ),
                Some(err.to_string()),
            )
        })?;
        if !origin.is_local() {
            return create_remote(&origin, name, version, variant, workspace, modpack, printer);
        }
    }
    let interactive = printer.is_interactive();
    // `--modpack` 隐含整合包变体，避免用户同时写两个参数还写错
    let variant = modpack.is_some().then_some("modrinth-modpack").or(variant);

    // 1. 用法闸（先于落库与联网，快速失败）
    if name.is_none() && !interactive {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                printer.lang(),
                "server create 缺少服务器名称",
                "server create requires a server name",
            ),
            Some(crate::i18n::pick(
                printer.lang(),
                "非交互模式：mcsup server create <name> --version <VER>",
                "non-interactive: mcsup server create <name> --version <VER>",
            )),
        )
        .into());
    }
    // 整合包变体的 `--version` 是整合包版本 ID，可省略（取最新正式版），
    // 因此不套用「非交互必须给版本」的闸门
    let modpack_mode = variant.is_some_and(is_modpack_flag);
    if version.is_none() && !interactive && !modpack_mode {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                printer.lang(),
                "server create 缺少 --version",
                "server create requires --version",
            ),
            Some(crate::i18n::pick(
                printer.lang(),
                "非交互模式必填；交互终端下可省略后从版本列表选择",
                "required when non-interactive; in a terminal it is picked from a list",
            )),
        )
        .into());
    }

    // 版本别名：`lts` 等价于 `latest`；显式版本原样保留（含 `latest-snapshot`）
    let version = version.map(crate::commands::install::normalize_version_alias);

    // 2. 变体能力：flag / 非交互默认先解析（显式未支持 → 退出码 2）
    let available = bootstrap::available_variants()?;
    let variant = match variant {
        Some(raw) => Some(resolve_flag_variant(raw, &available, printer.lang())?),
        None if !interactive => Some(default_variant(&available, printer.lang())?),
        None => None,
    };

    // 3. 工作区：--workspace 接受名称或路径；配合 --workspace-name / --workspace-dir
    //    时不存在就新建。无当前且交互 → 复用 init 向导
    // 向导被取消 / 未就绪时返回 `None`：已给出提示，静默中止
    let Some(selector) = pick_create_workspace(
        workspace,
        workspace_name,
        workspace_dir,
        interactive,
        printer,
    )?
    else {
        return Ok(());
    };
    let selector = non_empty_selector(selector);

    let reg = open_registry()?;

    // 交互预检只查本地注册表：名称那一格是「早提示」，不该为一台离线远端
    // 付建连等待（远端读是实时查询，见 docs/remote.md）。全局查重仍由
    // `reg.create` 在登记时做——唯一判定点不搬家。
    let local_reg = bootstrap::open_registry()?;

    // 4-6. 名称 / 变体 / 版本：给定即用，缺哪项补哪项
    let inputs = finish_create_inputs(
        &local_reg, &available, name, variant, version, modpack, printer,
    )
    .await?;
    let CreateInputs {
        name,
        variant,
        version_id,
        source,
        modpack,
    } = inputs;

    // Java 版本要求不在此预取：下载核心时 `install_jar` 会随元数据一并回写，
    // 预取会重复打一遍 manifest/details 网络请求
    //
    // 登记前要查全量名称（含已配对远端），离线远端会等一次建连超时：这步
    // 可能明显可见，挂 spinner 而不是让终端静止。
    let srv = {
        let _spinner = TaskSpinner::new(
            crate::i18n::pick(printer.lang(), "登记服务器", "registering server"),
            printer.is_human(),
        );
        reg.create(
            selector.as_deref(),
            &name,
            &version_id,
            variant.clone(),
            None,
        )
        .with_context(|| {
            crate::i18n::pick(
                printer.lang(),
                format!("创建服务器 {name} 失败"),
                format!("failed to create server {name}"),
            )
        })?
    };

    // 7. 布置服务端（核心 / 整合包），失败保留已注册结果
    setup_server(
        &name,
        &srv.dir,
        &version_id,
        source,
        &variant,
        modpack,
        printer,
    )
    .await?;
    report_created(&srv, printer)
}

/// `server create --origin <remote>`：在对端登记服务器
///
/// - 普通变体：对端登记 + 下载核心（`ServerCreate` + `ServerInstall`），
///   客户端不下载、不拼远端路径。
/// - 整合包：对端读清单、装加载器、铺 overrides（`ServerCreateModpack`），
///   一次调用完成「登记 + 布置」。
#[allow(clippy::too_many_lines)]
fn create_remote(
    origin: &mcsup::port::Origin,
    name: Option<&str>,
    version: Option<&str>,
    variant: Option<&str>,
    workspace: Option<&str>,
    modpack: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let cfg = crate::config::ConfigFile::locate(None).load()?;
    let members = bootstrap::remote_members(&cfg);
    let member = super::origin::member(&members, origin, lang)?.ok_or_else(|| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                "本地来源不需要远端创建",
                "local origin does not need remote create",
            ),
            None,
        )
    })?;
    let interactive = printer.is_interactive();
    // 名称：给定即用；交互缺省则提示输入；非交互缺省是用法错误
    let name = match name {
        Some(name) => name.to_owned(),
        None if interactive => prompt_remote_name(printer)?,
        None => {
            return Err(AppDiagnostic::usage(
                crate::i18n::pick(
                    lang,
                    "server create 缺少服务器名称",
                    "server create requires a server name",
                ),
                Some(crate::i18n::pick(
                    lang,
                    "非交互模式：mcsup server create <name> --version <VER> --variant <V>",
                    "non-interactive: mcsup server create <name> --version <VER> --variant <V>",
                )),
            )
            .into());
        }
    };
    // 整合包变体既可由 `--modpack` 隐含，也可显式写 `--variant modrinth-modpack`
    if let Some(project) = modpack {
        return create_remote_modpack(member, &name, project, version, variant, workspace, printer);
    }
    if variant.is_some_and(is_modpack_flag) {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                "整合包变体需要 --modpack <项目 ID 或 slug>",
                "the modpack variant requires --modpack <project id or slug>",
            ),
            Some(crate::i18n::pick(
                lang,
                "例如：mcsup server create myserver --origin prod --modpack fabulously-optimized",
                "e.g. mcsup server create myserver --origin prod --modpack fabulously-optimized",
            )),
        )
        .into());
    }
    // 变体：显式给出就按对端的可用变体校验；交互缺省则从对端列表选
    let variant = match variant {
        Some(raw) => {
            let available = remote_variants(member, printer)?;
            resolve_flag_variant(raw, &available, lang)?
        }
        None if interactive => {
            let available = remote_variants(member, printer)?;
            pick_variant(&available, printer)?
        }
        None => {
            return Err(AppDiagnostic::usage(
                crate::i18n::pick(
                    lang,
                    "远端 server create 需要 --variant",
                    "remote `server create` requires --variant",
                ),
                Some(crate::i18n::pick(
                    lang,
                    "交互终端下可省略后从对端变体列表选择",
                    "in a terminal it is picked from the peer's variant list",
                )),
            )
            .into());
        }
    };
    // 版本：显式给出即用；交互缺省则从对端版本列表选
    let version = match version {
        Some(version) => version.to_owned(),
        None if interactive => pick_remote_version(member, &variant, printer)?,
        None => {
            return Err(AppDiagnostic::usage(
                crate::i18n::pick(
                    lang,
                    "远端 server create 需要 --version",
                    "remote `server create` requires --version",
                ),
                Some(crate::i18n::pick(
                    lang,
                    "交互终端下可省略后从对端版本列表选择",
                    "in a terminal it is picked from the peer's version list",
                )),
            )
            .into());
        }
    };
    let meta = member
        .servers
        .create(workspace, &name, &version, variant.as_str(), None)
        .with_context(|| {
            crate::i18n::pick(lang, "在远端创建服务器", "creating the server on the peer")
        })?;
    // 登记成功即装核心：与本地 `server create` 同一条语义（建站 = 登记 + 下载核心）。
    // 安装失败保留登记，提示对端重试——「已登记、可重试」与本地 setup_server 失败
    // 路径的口径完全一致。
    let installed = member.servers.install(meta.name.as_str(), &version);
    let (jar_version, path) = match installed {
        Ok(updated) => match updated.jar.as_ref() {
            Some(_) => (
                Some(updated.version.as_str().to_owned()),
                Some(server_path(member, &updated)),
            ),
            None => (None, None),
        },
        Err(err) => {
            printer.print_warning(&crate::i18n::pick(
                lang,
                format!(
                    "服务器已在 {} 登记，但核心安装失败，可重试 mcsup server install {}/{}",
                    member.name, member.name, meta.name
                ),
                format!(
                    "the server is registered on {} but the core install failed; retry with `mcsup server install {}/{}'",
                    member.name, member.name, meta.name
                ),
            ));
            return Err(err).with_context(|| {
                crate::i18n::pick(lang, "远端核心安装失败", "remote core install failed")
            });
        }
    };
    printer.print_outcome(
        &crate::i18n::pick(
            lang,
            format!(
                "已在 {} 创建服务器 {} {}（{}）",
                member.name,
                meta.name,
                meta.version,
                meta.variant.as_str()
            ),
            format!(
                "created server {} {} ({}) on {}",
                meta.name,
                meta.version,
                meta.variant.as_str(),
                member.name
            ),
        ),
        &CreateRow {
            name: meta.name.as_str(),
            version: meta.version.as_str(),
            variant: meta.variant.as_str(),
            path: path.unwrap_or_default(),
            linked: None,
        },
    )?;
    if let Some(version) = jar_version {
        printer.print_section(&crate::i18n::pick(
            lang,
            format!("核心版本 {version} 已在对端安装完成"),
            format!("core {version} installed on the peer"),
        ));
    }
    Ok(())
}

/// 远端整合包创建：对端读清单 → 登记 → 装加载器 → 铺文件
///
/// 远端创建向导：交互输入服务器名（非空且合法）
fn prompt_remote_name(printer: Printer) -> Result<String> {
    let lang = printer.lang();
    prompts::step(
        printer,
        1,
        3,
        &crate::i18n::pick(lang, "服务器名称", "server name"),
    );
    loop {
        let input = prompts::input_name(
            lang,
            &crate::i18n::pick(lang, "服务器名称", "server name"),
            "",
        )?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            continue;
        }
        match ServerName::parse(trimmed) {
            Ok(name) => return Ok(name.as_str().to_owned()),
            Err(_) => printer.print_warning(&crate::i18n::pick(
                lang,
                "名称只能包含字母、数字、-、_，最长 64 字符",
                "names may only contain letters, digits, - or _, up to 64 characters",
            )),
        }
    }
}

/// 对端可用变体（解析失败的对端 id 忽略，不让一个未知变体拖垮整张列表）
fn remote_variants(
    member: &mcsup::backend::remote::RemoteMember,
    printer: Printer,
) -> Result<Vec<ServerVariant>> {
    let lang = printer.lang();
    let ids = member.servers.variants().with_context(|| {
        crate::i18n::pick(lang, "获取对端变体列表", "fetching the peer's variant list")
    })?;
    let available: Vec<ServerVariant> = ids
        .into_iter()
        .filter_map(|id| ServerVariant::from_id(id).ok())
        .collect();
    if available.is_empty() {
        return Err(anyhow::anyhow!(crate::i18n::pick(
            lang,
            "对端没有可用的服务端来源（provider 未注册）",
            "the peer has no server jar source available (no provider registered)",
        )));
    }
    Ok(available)
}

/// 远端创建向导：从对端版本列表交互选择版本
fn pick_remote_version(
    member: &mcsup::backend::remote::RemoteMember,
    variant: &ServerVariant,
    printer: Printer,
) -> Result<String> {
    let lang = printer.lang();
    prompts::step(
        printer,
        3,
        3,
        &crate::i18n::pick(lang, "服务端版本", "server version"),
    );
    let spinner = TaskSpinner::new(
        crate::i18n::pick(
            lang,
            format!("获取 {} 版本列表", variant.as_str()),
            format!("fetching the {} version list", variant.as_str()),
        ),
        printer.is_human(),
    );
    let versions = member.servers.versions(variant.as_str()).with_context(|| {
        crate::i18n::pick(
            lang,
            "获取对端版本列表失败",
            "failed to fetch the peer's version list",
        )
    })?;
    spinner.finish();
    Ok(select_version(versions, variant.as_str(), printer)?
        .as_str()
        .to_owned())
}

/// `--version` 是整合包版本（可省略取最新正式版），不是 Minecraft 版本；
/// 清单声明的 MC 版本与加载器变体由对端回传，客户端只做展示。
fn create_remote_modpack(
    member: &mcsup::backend::remote::RemoteMember,
    name: &str,
    project: &str,
    version: Option<&str>,
    variant: Option<&str>,
    workspace: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    if let Some(variant) = variant.filter(|raw| !is_modpack_flag(raw)) {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("--modpack 不能与 --variant {variant} 同时使用：加载器由整合包清单决定"),
                format!(
                    "--modpack cannot be combined with --variant {variant}: the modpack manifest decides the loader"
                ),
            ),
            None,
        )
        .into());
    }
    let spinner = TaskSpinner::new(
        crate::i18n::pick(
            lang,
            format!("正在 {} 按整合包 {project} 创建 {name}…", member.name),
            format!("creating {name} from modpack {project} on {}…", member.name),
        ),
        printer.is_human(),
    );
    let result = member
        .servers
        .create_modpack(workspace, name, project, version);
    spinner.finish();
    let result = result.with_context(|| {
        crate::i18n::pick(
            lang,
            format!(
                "在 {} 按整合包创建服务器失败（登记可能已完成，可用 server install 重试）",
                member.name
            ),
            format!(
                "failed to create the server from the modpack on {} (it may already be registered; retry with `server install`)",
                member.name
            ),
        )
    })?;
    let path = server_path(member, &result.meta);
    super::modpack::report_remote_applied(&result, path.clone(), member.name.as_str(), printer)?;
    printer.print_outcome(
        &crate::i18n::pick(
            lang,
            format!(
                "已在 {} 创建服务器 {} {}（{}）",
                member.name,
                result.meta.name,
                result.meta.version,
                result.meta.variant.as_str()
            ),
            format!(
                "created server {} {} ({}) on {}",
                result.meta.name,
                result.meta.version,
                result.meta.variant.as_str(),
                member.name
            ),
        ),
        &CreateRow {
            name: result.meta.name.as_str(),
            version: result.meta.version.as_str(),
            variant: result.meta.variant.as_str(),
            path,
            linked: None,
        },
    )?;
    printer.print_section(&crate::i18n::pick(
        lang,
        format!(
            "完成。运行 mcsup server start {}/{} 启动",
            member.name, result.meta.name
        ),
        format!(
            "done. run `mcsup server start {}/{}` to start it",
            member.name, result.meta.name
        ),
    ));
    Ok(())
}

/// 远端服务器的目录展示口径：对端的 `{workspace}/servers/{rel_dir}`
fn server_path(
    member: &mcsup::backend::remote::RemoteMember,
    meta: &mcsup::domain::server::meta::ServerMeta,
) -> String {
    use mcsup::port::WorkspaceRepository as _;
    member
        .workspaces
        .list()
        .ok()
        .and_then(|workspaces| {
            workspaces
                .iter()
                .find(|ws| ws.id.as_str() == meta.workspace_id.as_str())
                .map(|ws| {
                    ws.path
                        .join("servers")
                        .join(meta.rel_dir.clone())
                        .display()
                        .to_string()
                })
        })
        .unwrap_or_else(|| {
            format!(
                "{}/{}/{}",
                member.name,
                meta.workspace_id.as_str(),
                meta.rel_dir.display()
            )
        })
}

/// 创建服务器之后的服务端布置
///
/// - 整合包：装加载器核心 → 铺 overrides → 下载清单文件
/// - 其余变体：下载核心 JAR
///
/// 失败时给出一条「已创建、可重试」的提示并把错误原样返回：服务器已经落库，
/// 不存在「回滚到没创建」的语义，重试入口是 `server install`。
///
/// # Errors
///
/// 核心下载失败、整合包取回 / 布置失败，或变体既非整合包又没有核心来源时返回错误
async fn setup_server(
    name: &str,
    dir: &Path,
    version_id: &VersionId,
    source: Option<Arc<dyn ServerJarSource>>,
    variant: &ServerVariant,
    modpack: Option<super::modpack::PickedModpack>,
    printer: Printer,
) -> Result<()> {
    let outcome = match (modpack, source) {
        (Some(modpack), _) => super::modpack::install_modpack(name, dir, &modpack, printer).await,
        (None, Some(source)) => download_jar(name, version_id, source, variant.clone(), printer)
            .await
            .map(|_| ()),
        // 变体既不是整合包、又没有核心来源：前面已按能力校验过，走到这里说明
        // 注册表与变体集合不一致，如实报错而不是静默成功
        (None, None) => Err(anyhow::anyhow!(
            "{}",
            crate::i18n::pick(
                printer.lang(),
                format!("变体 {} 没有可用的服务端来源", variant.as_str()),
                format!("variant {} has no server source", variant.as_str()),
            )
        )),
    };
    if let Err(err) = outcome {
        printer.print_warning(&crate::i18n::pick(
            printer.lang(),
            format!("服务器已创建但服务端布置失败，可执行 mcsup server install {name} 重试"),
            format!(
                "server created but the server setup failed; run `mcsup server install {name}` to retry"
            ),
        ));
        return Err(err);
    }
    Ok(())
}

/// `server create` 的名称 / 变体 / 版本补齐：参数给定则跳过对应步骤
/// `server create` 补齐后的输入
struct CreateInputs {
    name: String,
    variant: ServerVariant,
    version_id: VersionId,
    /// 核心来源；整合包由清单自行装加载器，因此为 `None`
    source: Option<Arc<dyn ServerJarSource>>,
    /// 已选整合包；非整合包变体为 `None`
    modpack: Option<super::modpack::PickedModpack>,
}

async fn finish_create_inputs(
    local_reg: &ServerRegistry,
    available: &[ServerVariant],
    name: Option<&str>,
    variant: Option<ServerVariant>,
    version: Option<&str>,
    modpack: Option<&str>,
    printer: Printer,
) -> Result<CreateInputs> {
    let name = if let Some(name) = name {
        name.to_owned()
    } else {
        prompts::step(
            printer,
            1,
            3,
            &crate::i18n::pick(printer.lang(), "服务器名称", "server name"),
        );
        prompt_unique_name(local_reg, printer)?
    };

    let variant = match variant {
        Some(variant) => variant,
        None => pick_variant(available, printer)?,
    };

    // 整合包：MC 版本由清单声明，`pick_modpack` 先读清单再返回可落库的版本
    if bootstrap::is_modpack(&variant) {
        let picked = super::modpack::pick_modpack(modpack, version, printer).await?;
        return Ok(CreateInputs {
            name,
            // 落库用清单加载器对应的变体（fabric / forge / ...），而不是
            // `modrinth-modpack`：后者只是「走整合包流程」的标记，没有核心来源
            variant: picked.variant.clone(),
            version_id: VersionId::new(picked.minecraft.clone()),
            source: None,
            modpack: Some(picked),
        });
    }

    let source = bootstrap::source_for(&variant, printer.lang())?;
    let version_id = if let Some(version) = version {
        VersionId::new(version)
    } else {
        prompts::step(
            printer,
            3,
            3,
            &crate::i18n::pick(printer.lang(), "服务端版本", "server version"),
        );
        pick_version(source.as_ref(), &variant, printer).await?
    };
    Ok(CreateInputs {
        name,
        variant,
        version_id,
        source: Some(source),
        modpack: None,
    })
}

/// `server create` 的结果输出：human 一行 + 后续提示，JSON 单个对象
fn report_created(srv: &mcsup::service::RegisteredServer, printer: Printer) -> Result<()> {
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!(
                "已创建: {} {} → {}",
                srv.meta.name,
                srv.meta.version,
                srv.dir.display()
            ),
            format!(
                "created: {} {} → {}",
                srv.meta.name,
                srv.meta.version,
                srv.dir.display()
            ),
        ),
        &CreateRow {
            name: srv.meta.name.as_str(),
            version: srv.meta.version.as_str(),
            variant: srv.meta.variant.as_str(),
            path: srv.dir.display().to_string(),
            linked: None,
        },
    )?;
    printer.print_section(&crate::i18n::pick(
        printer.lang(),
        format!("完成。运行 mcsup server start {} 启动", srv.meta.name),
        format!(
            "done. run `mcsup server start {}` to start it",
            srv.meta.name
        ),
    ));
    Ok(())
}

/// 显式 `--variant`：名字归一 + 能力校验；未知 / 未支持都判用法错误（退出码 2）
///
/// 两级判定，顺序不能反：
///
/// 1. **名字合法**（[`ServerVariant::from_id`]）：空串、控制字符、路径分隔符
///    直接拒。内置名在这里归一成内置项，其余名字成为第三方变体。
/// 2. **有来源**（`available`）：名字合法但组合根没注册对应来源时，才报
///    「暂未支持」。
///
/// 第 1 步**不能**用 [`ServerVariant::parse`] 只认内置集合：第三方宿主注册了
/// `Custom` 变体的来源后，CLI 必须能通过 `--variant` 指名它，否则那个变体
/// 在 CLI 里永远无法创建——注册表说了支持，入口却拒绝。
fn resolve_flag_variant(
    raw: &str,
    available: &[ServerVariant],
    lang: Lang,
) -> Result<ServerVariant> {
    // 用户向的短名：`--variant modrinth` 等价于整合包变体。存储与展示仍用
    // `modrinth-modpack`，别名只降低输入成本，不改变持久化口径。
    let raw = if raw.eq_ignore_ascii_case("modrinth") {
        ServerVariant::ModrinthModpack.as_str()
    } else {
        raw
    };
    let parsed = ServerVariant::from_id(raw).map_err(|_| {
        anyhow::Error::from(AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("未知变体: {raw}"),
                format!("unknown variant: {raw}"),
            ),
            Some(crate::i18n::pick(
                lang,
                format!("可用变体：{}", bootstrap::join_variants(available, lang)),
                format!(
                    "available variants: {}",
                    bootstrap::join_variants(available, lang)
                ),
            )),
        ))
    })?;
    if !available.contains(&parsed) {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("暂未支持变体 {raw}"),
                format!("variant not supported yet: {raw}"),
            ),
            Some(crate::i18n::pick(
                lang,
                format!("当前可用：{}", bootstrap::join_variants(available, lang)),
                format!(
                    "currently available: {}",
                    bootstrap::join_variants(available, lang)
                ),
            )),
        )
        .into());
    }
    Ok(parsed)
}

/// 非交互缺省变体：优先 vanilla（历史缺省值），否则取首个可用
fn default_variant(available: &[ServerVariant], lang: Lang) -> Result<ServerVariant> {
    available
        .iter()
        .find(|variant| **variant == ServerVariant::Vanilla)
        .or_else(|| available.first())
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(crate::i18n::pick(
                lang,
                "没有可用的服务端来源（provider 未注册）".to_owned(),
                "no server jar source available (no provider registered)".to_owned(),
            ))
        })
}

/// 交互变体选择：仅列可用；单可用项免提问（不打步骤回显）
fn pick_variant(available: &[ServerVariant], printer: Printer) -> Result<ServerVariant> {
    if let [only] = available {
        return Ok(only.clone());
    }
    prompts::step(
        printer,
        2,
        3,
        &crate::i18n::pick(printer.lang(), "服务端类型", "server type"),
    );
    // 标签由 `bootstrap::variant_label` 统一给出，候选与变体一一对应
    let labels: Vec<String> = available.iter().map(bootstrap::variant_label).collect();
    let picked = prompts::select_filter(
        printer.lang(),
        &crate::i18n::pick(printer.lang(), "服务端类型", "server type"),
        &labels,
    )?;
    bootstrap::variant_from_label(&picked, available)
        .ok_or_else(|| anyhow::anyhow!("无效变体: {picked}"))
}

/// 名称重输：语法由 prompts 校验，重复（本地已有）时警示后重问
///
/// 只查本地注册表：这格是交互预检，候选列表与提示必须立刻出现，不能为一台
/// 离线远端付建连等待。远端 / 全局重名由登记阶段（`ServerRegistry::create`）
/// 兜底——那里才是名称唯一性的判定点。
fn prompt_unique_name(local_reg: &ServerRegistry, printer: Printer) -> Result<String> {
    loop {
        let candidate = prompts::input_name(
            printer.lang(),
            &crate::i18n::pick(printer.lang(), "服务器名称", "server name"),
            "server",
        )?;
        match local_reg.get(&candidate) {
            Err(ServerRegistryError::NotFound(_)) => return Ok(candidate),
            Err(err) => return Err(err.into()),
            Ok(_) => printer.print_warning(&crate::i18n::pick(
                printer.lang(),
                format!("名称 {candidate} 已存在，换一个"),
                format!("name {candidate} is taken; pick another"),
            )),
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
            printer.print_section(&crate::i18n::pick(
                printer.lang(),
                "尚未有工作区，先创建工作区",
                "no workspace yet; create one first",
            ));
            super::workspace::init(&svc, None, None, printer)?;
            if svc.current_workspace().is_ok() {
                Ok(true)
            } else {
                printer.print_section(&crate::i18n::pick(
                    printer.lang(),
                    "工作区未就绪，创建中止（可重新执行 mcsup workspace init / use 后再创建）",
                    "workspace is not ready; aborting (rerun mcsup workspace init / use and retry)",
                ));
                Ok(false)
            }
        }
        Err(err) => Err(err.into()),
    }
}

/// `server create` 的工作区就绪：返回 `None` 表示已提示并中止
fn pick_create_workspace(
    workspace: Option<&str>,
    workspace_name: Option<&str>,
    workspace_dir: Option<&Path>,
    interactive: bool,
    printer: Printer,
) -> Result<Option<String>> {
    match resolve_create_workspace(
        workspace,
        workspace_name,
        workspace_dir,
        interactive,
        printer,
    )? {
        Some(CreateWorkspace::Aborted) => Ok(None),
        Some(CreateWorkspace::Ready { selector, created }) => {
            if created && !ensure_workspace(selector.as_deref(), interactive, printer)? {
                return Ok(None);
            }
            Ok(Some(selector.unwrap_or_default()))
        }
        None => {
            if !ensure_workspace(None, interactive, printer)? {
                return Ok(None);
            }
            // 空串代表「按当前工作区落点」，避免再包一层 Option
            Ok(Some(String::new()))
        }
    }
}

/// `server create` 的工作区解析结果
#[derive(Debug, Clone, PartialEq, Eq)]
enum CreateWorkspace {
    /// 解析完成：`selector` 为空时按当前工作区落点
    Ready {
        /// 传给注册表的选择器（工作区名称）
        selector: Option<String>,
        /// 是否本次新建的（需要再做一次就绪校验）
        created: bool,
    },
    /// 交互向导被取消 / 工作区未就绪
    Aborted,
}

/// `server create` 的工作区解析：名称 / 路径都能定位，不存在就新建
///
/// 规则（用户口径）：
///
/// - `--workspace <名或路径>` 命中已有工作区 → 直接用
/// - 没命中，但给了 `--workspace-dir` / `--workspace-name` → 新建并落进去
/// - 名称命中但路径不一致（或路径命中但名称不一致）→ 用法错误，不静默接管
/// - 非交互下既没命中也没有目录参数 → 用法错误（2），不等待输入
///
/// 返回 `Ok(None)` 表示没给 `--workspace`，按当前工作区处理；
/// [`CreateWorkspace::Ready`] 给出选择器（工作区名称）。
fn resolve_create_workspace(
    workspace: Option<&str>,
    workspace_name: Option<&str>,
    workspace_dir: Option<&Path>,
    interactive: bool,
    printer: Printer,
) -> Result<Option<CreateWorkspace>> {
    // 没给 --workspace 也没给新建参数：完全走既有口径（环境变量 > 当前工作区）
    if workspace.is_none() && workspace_name.is_none() && workspace_dir.is_none() {
        return Ok(None);
    }
    let store = bootstrap::open_store()?;
    let svc = WorkspaceService::new(store.clone(), store);
    let all = svc.list()?;

    let by_name = match workspace {
        Some(sel) => match resolve_selector(&all, sel) {
            Ok(ws) => Some(ws),
            Err(WorkspaceError::NotFound(_)) => None,
            Err(err) => return Err(err.into()),
        },
        None => None,
    };
    let by_dir = match workspace_dir {
        Some(dir) => {
            let dir = absolute_path(dir);
            all.iter().find(|ws| ws.path == dir).cloned()
        }
        None => None,
    };

    check_workspace_identity(by_name.as_ref(), by_dir.as_ref(), workspace_dir, printer)?;

    if let Some(ws) = by_name.or(by_dir) {
        if let Some(name) = workspace_name
            && name != ws.name.as_str()
        {
            return Err(AppDiagnostic::usage(
                crate::i18n::pick(
                    printer.lang(),
                    format!(
                        "工作区 {} 已存在，与 --workspace-name {name} 不一致",
                        ws.name.as_str()
                    ),
                    format!(
                        "workspace {} already exists and does not match --workspace-name {name}",
                        ws.name.as_str()
                    ),
                ),
                Some(crate::i18n::pick(
                    printer.lang(),
                    "要用已有工作区就只传 --workspace / --workspace-dir；要新建就换一个名称",
                    "to reuse a workspace pass only --workspace / --workspace-dir; to create one pick another name",
                )),
            )
            .into());
        }
        return Ok(Some(CreateWorkspace::Ready {
            selector: Some(ws.name.as_str().to_owned()),
            created: false,
        }));
    }

    // `--workspace <名> --workspace-dir <路径>` 未命中时，把 `--workspace` 当作名称
    let name = if workspace_name.is_some() {
        workspace_name
    } else if workspace_dir.is_some() {
        workspace.filter(|sel| !sel.contains(['/', '\\']))
    } else {
        None
    };

    // 未命中已有工作区：没有目录 / 名称只能交互建，否则按显式参数新建
    if workspace_dir.is_none() && name.is_none() {
        if !interactive {
            return Err(AppDiagnostic::usage(
                crate::i18n::pick(
                    printer.lang(),
                    format!("工作区 {} 不存在", workspace.unwrap_or("(未指定)")),
                    format!(
                        "workspace {} does not exist",
                        workspace.unwrap_or("(unspecified)")
                    ),
                ),
                Some(crate::i18n::pick(
                    printer.lang(),
                    "非交互模式：带 --workspace-dir <路径> 新建，或先运行 mcsup workspace init",
                    "non-interactive: pass --workspace-dir <path> to create it, or run mcsup workspace init first",
                )),
            )
            .into());
        }
        printer.print_section(&crate::i18n::pick(
            printer.lang(),
            "尚未有可用的工作区，先创建工作区",
            "no workspace yet; create one first",
        ));
        super::workspace::init(&svc, None, None, printer)?;
        return Ok(if let Ok(ws) = svc.current_workspace() {
            Some(CreateWorkspace::Ready {
                selector: Some(ws.name.as_str().to_owned()),
                created: true,
            })
        } else {
            printer.print_section(&crate::i18n::pick(
                printer.lang(),
                "工作区未就绪，创建中止（可重新执行 mcsup workspace init / use 后再创建）",
                "workspace is not ready; aborting (rerun mcsup workspace init / use and retry)",
            ));
            Some(CreateWorkspace::Aborted)
        });
    }

    create_workspace(&svc, workspace_dir, workspace, name, printer)
}

/// 新建并注册工作区，返回按名称定位的选择器
fn create_workspace(
    svc: &WorkspaceService,
    workspace_dir: Option<&Path>,
    workspace: Option<&str>,
    name: Option<&str>,
    printer: Printer,
) -> Result<Option<CreateWorkspace>> {
    let dir = resolve_new_workspace_dir(printer.lang(), workspace_dir, workspace, name)?;
    let ws = match svc.init(&dir, name)? {
        InitOutcome::Created(ws)
        | InitOutcome::Recovered(ws)
        | InitOutcome::MarkerRestored(ws)
        | InitOutcome::AlreadyRegistered(ws) => ws,
    };
    printer.print_section(&crate::i18n::pick(
        printer.lang(),
        format!("已就绪工作区 {} ({})", ws.name.as_str(), ws.path.display()),
        format!(
            "workspace ready: {} ({})",
            ws.name.as_str(),
            ws.path.display()
        ),
    ));
    Ok(Some(CreateWorkspace::Ready {
        selector: Some(ws.name.as_str().to_owned()),
        created: true,
    }))
}

/// 名称与路径必须指向同一个已注册工作区，否则是用户笔误，直接报用法错误
fn check_workspace_identity(
    by_name: Option<&mcsup::domain::workspace::Workspace>,
    by_dir: Option<&mcsup::domain::workspace::Workspace>,
    workspace_dir: Option<&Path>,
    printer: Printer,
) -> Result<()> {
    if let Some(name_hit) = by_name
        && let Some(dir) = workspace_dir
        && !same_path(&name_hit.path, dir)
    {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                printer.lang(),
                format!(
                    "工作区 {} 已注册在 {}，与 --workspace-dir {} 不一致",
                    name_hit.name.as_str(),
                    name_hit.path.display(),
                    absolute_path(dir).display()
                ),
                format!(
                    "workspace {} is registered at {}, which does not match --workspace-dir {}",
                    name_hit.name.as_str(),
                    name_hit.path.display(),
                    absolute_path(dir).display()
                ),
            ),
            Some(crate::i18n::pick(
                printer.lang(),
                "名称与路径必须指向同一个工作区；只保留其中一个再重试",
                "the name and the path must point to the same workspace; keep only one and retry",
            )),
        )
        .into());
    }
    if let (Some(name_hit), Some(dir_hit)) = (by_name, by_dir)
        && name_hit.id != dir_hit.id
    {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                printer.lang(),
                format!(
                    "工作区名称与路径指向不同工作区：{} 在 {}，而 --workspace-dir 指向 {}",
                    name_hit.name.as_str(),
                    name_hit.path.display(),
                    dir_hit.path.display()
                ),
                format!(
                    "the workspace name and --workspace-dir point to different workspaces: {} at {}, but --workspace-dir is {}",
                    name_hit.name.as_str(),
                    name_hit.path.display(),
                    dir_hit.path.display()
                ),
            ),
            Some(crate::i18n::pick(
                printer.lang(),
                "名称与路径必须指向同一个工作区；只保留其中一个再重试",
                "the name and the path must point to the same workspace; keep only one and retry",
            )),
        )
        .into());
    }
    Ok(())
}

/// 两个目录是否指向同一位置（都尽量取物理路径比较）
fn same_path(left: &Path, right: &Path) -> bool {
    let normalize =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| absolute_path(path));
    normalize(left) == normalize(right)
}

/// 新建工作区的目录：`--workspace-dir` > `--workspace` 本身是路径 > 数据目录/名称
fn resolve_new_workspace_dir(
    lang: Lang,
    workspace_dir: Option<&Path>,
    workspace: Option<&str>,
    workspace_name: Option<&str>,
) -> Result<PathBuf> {
    if let Some(dir) = workspace_dir {
        return Ok(absolute_path(dir));
    }
    if let Some(sel) = workspace
        && sel.contains(['/', '\\'])
    {
        return Ok(absolute_path(Path::new(sel)));
    }
    let name = workspace_name.or(workspace).ok_or_else(|| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                "新建工作区缺少名称或目录",
                "creating a workspace needs a name or a directory",
            ),
            Some(crate::i18n::pick(
                lang,
                "用 --workspace-dir <路径> 或 --workspace-name <名称> 指定",
                "pass --workspace-dir <path> or --workspace-name <name>",
            )),
        )
    })?;
    // 名称非法时交给 service 的 WorkspaceName::parse 报错，口径只留一份
    let _ = WorkspaceName::parse(name)?;
    Ok(crate::config::platform_data_dir()
        .join("workspaces")
        .join(name))
}

pub(crate) fn link(
    path: &Path,
    name: Option<&str>,
    workspace: Option<&str>,
    origin: Option<&str>,
    printer: Printer,
) -> Result<()> {
    if let Some(origin) = origin {
        let origin = mcsup::port::Origin::parse(origin).map_err(|err| {
            AppDiagnostic::usage(
                crate::i18n::pick(
                    printer.lang(),
                    format!("--origin 取值非法: {origin}"),
                    format!("invalid --origin value: {origin}"),
                ),
                Some(err.to_string()),
            )
        })?;
        if !origin.is_local() {
            return link_remote(&origin, path, name, workspace, printer);
        }
    }
    let reg = open_registry()?;
    let selector = workspace_selector(workspace);
    let out = reg.link(path, name, selector.as_deref()).with_context(|| {
        crate::i18n::pick(
            printer.lang(),
            format!("注册服务器目录 {} 失败", path.display()),
            format!("failed to register server directory {}", path.display()),
        )
    })?;
    // 工作区内目录走就地注册（linked=false），外部目录才建符号链接
    printer.print_outcome(
        &if out.server.linked {
            crate::i18n::pick(
                printer.lang(),
                format!(
                    "已链接: {} → {}（目标 {}）",
                    out.server.meta.name,
                    out.server.dir.display(),
                    out.target.display()
                ),
                format!(
                    "linked: {} → {} (target {})",
                    out.server.meta.name,
                    out.server.dir.display(),
                    out.target.display()
                ),
            )
        } else {
            crate::i18n::pick(
                printer.lang(),
                format!(
                    "已注册工作区内目录: {} → {}（未建符号链接）",
                    out.server.meta.name,
                    out.server.dir.display()
                ),
                format!(
                    "registered an in-workspace directory: {} → {} (no symlink)",
                    out.server.meta.name,
                    out.server.dir.display()
                ),
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

/// `server link --origin <remote>`：在对端注册一个已存在的目录
///
/// 符号链接、marker 与入库都在对端完成；`path` 是**目标机器**上的路径。
///
/// # Errors
///
/// 远端未就绪、对端拒绝（目标不存在 / 名称冲突 / 目标在工作区内…）时返回错误
fn link_remote(
    origin: &mcsup::port::Origin,
    path: &Path,
    name: Option<&str>,
    workspace: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let cfg = crate::config::ConfigFile::locate(None).load()?;
    let members = bootstrap::remote_members(&cfg);
    let member = super::origin::member(&members, origin, lang)?.ok_or_else(|| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                "本地来源不需要远端注册",
                "local origin does not need a remote link",
            ),
            None,
        )
    })?;
    let target = path.to_string_lossy().into_owned();
    let out = member
        .servers
        .link_directory(&target, name, workspace)
        .with_context(|| {
            crate::i18n::pick(
                lang,
                format!("在远端注册目录 {} 失败", path.display()),
                format!("failed to register {} on the peer", path.display()),
            )
        })?;
    printer.print_outcome(
        &if out.linked {
            crate::i18n::pick(
                lang,
                format!(
                    "已在 {} 链接: {} → {}（目标 {}）",
                    member.name, out.meta.name, out.target, target
                ),
                format!(
                    "linked on {}: {} → {} (target {})",
                    member.name, out.meta.name, out.target, target
                ),
            )
        } else {
            crate::i18n::pick(
                lang,
                format!(
                    "已在 {} 注册工作区内目录: {} → {}（未建符号链接）",
                    member.name, out.meta.name, out.target
                ),
                format!(
                    "registered an in-workspace directory on {}: {} → {} (no symlink)",
                    member.name, out.meta.name, out.target
                ),
            )
        },
        &CreateRow {
            name: out.meta.name.as_str(),
            version: out.meta.version.as_str(),
            variant: out.meta.variant.as_str(),
            path: out.target,
            linked: Some(out.linked),
        },
    )
}

pub(crate) fn remove(name: &str, purge: bool, printer: Printer) -> Result<()> {
    let reg = open_registry()?;
    // 先看一眼：链接服务器 + purge 会被 service 拒绝，这里提前给出更友好的确认
    let srv = reg.get(name).with_context(|| {
        crate::i18n::pick(
            printer.lang(),
            format!("服务器 {name} 不存在"),
            format!("server {name} does not exist"),
        )
    })?;
    if purge && srv.linked {
        printer.print_warning(&crate::i18n::pick(
            printer.lang(),
            format!("{name} 是链接服务器，--purge 会拒绝执行（目标在工作区外，无法安全删除）"),
            format!(
                "{name} is a linked server; --purge will be refused (its target is outside the workspace and cannot be deleted safely)"
            ),
        ));
        // 仍传给 service，由其拒绝并给出统一错误
    }
    // 删了不可恢复：可交互时问一句，默认不删；`--purge` 是显式指令，不再问；
    // `--yes` 对「是否删盘」「手输名称」两个确认都答「是」
    let purge = if purge || !printer.is_interactive() {
        purge
    } else {
        // 普通注销不删目录，先问「是否连磁盘数据一起删」；回答是后再按删盘的前置校验
        // 拦一次（与 --purge 同一道闸），用户此时尚未确认名称
        let confirmed = prompts::confirm_or_yes(
            printer,
            &crate::i18n::pick(
                printer.lang(),
                format!("删除 {name} ({}) 的磁盘数据?", srv.dir.display()),
                format!(
                    "also delete the on-disk data of {name} ({})?",
                    srv.dir.display()
                ),
            ),
            false,
        )
        .context(crate::i18n::pick(
            printer.lang(),
            "已取消注销",
            "unregister cancelled",
        ))?;
        if confirmed {
            let cwd = std::env::current_dir().context(crate::i18n::pick(
                printer.lang(),
                "获取当前工作目录失败",
                "cannot read the current directory",
            ))?;
            super::lifecycle::lifecycle_service()?.ensure_server_purgeable(name, &cwd)?;
            // 同意删盘后再上一道闸：红色警示 + 手输服务器名称，防误删存档与备份
            printer.print_warning(&crate::i18n::pick(
                printer.lang(),
                format!(
                    "警告: 将永久删除 {name} ({}) 中的所有存档、备份等文件，此操作无法恢复",
                    srv.dir.display()
                ),
                format!(
                    "warning: this permanently deletes every world, backup and file under {name} ({}); it cannot be undone",
                    srv.dir.display()
                ),
            ));
            if !printer.assumes_yes() {
                prompts::confirm_by_name(
                    printer.lang(),
                    &crate::i18n::pick(
                        printer.lang(),
                        format!("输入服务器名称 {name} 以确认删除"),
                        format!("type the server name {name} to confirm deletion"),
                    ),
                    name,
                )?;
            }
        }
        confirmed
    };
    // 终端的工作目录：service 据此拒绝「删掉调用方所在目录」
    let cwd = std::env::current_dir().context(crate::i18n::pick(
        printer.lang(),
        "获取当前工作目录失败",
        "cannot read the current directory",
    ))?;
    if purge {
        // 删盘前置条件先过一次：`--purge` 是显式指令，条件不满足直接报错，
        // 不必等 service 在删除那一刻才拒
        super::lifecycle::lifecycle_service()?.ensure_server_purgeable(name, &cwd)?;
        // 链接服务器不会真删目录，不必退出来
        if !srv.linked {
            super::chdir_out_of(&srv.dir);
        }
    }
    // 删除经 lifecycle 编排：运行中拒绝（存档保护）、cwd 在目录内拒绝，
    // 判定收敛在 service
    let out = super::lifecycle::lifecycle_service()?.remove_server(name, purge, &cwd)?;
    report_removed(&out, purge, printer)
}

/// `server remove` 的结果输出：按「删盘 / 链接 / 请求删盘但目录不在 / 仅注销」分文案
fn report_removed(
    out: &mcsup::service::ServerRemoveOutcome,
    purge: bool,
    printer: Printer,
) -> Result<()> {
    let srv = &out.server;
    let disk_purged = out.disk_purged();
    let human = if disk_purged {
        crate::i18n::pick(
            printer.lang(),
            format!("已删除并注销: {}", srv.meta.name),
            format!("deleted and unregistered: {}", srv.meta.name),
        )
    } else if srv.linked {
        crate::i18n::pick(
            printer.lang(),
            format!("已移除链接并注销: {}（目标目录保留）", srv.meta.name),
            format!(
                "link removed and unregistered: {} (target directory kept)",
                srv.meta.name
            ),
        )
    } else if purge {
        // 请求了删盘但目录本就不在：不能说「已删除」，用户会以为删掉了存档
        crate::i18n::pick(
            printer.lang(),
            format!("已注销（磁盘数据本就不存在）: {}", srv.meta.name),
            format!("unregistered (no on-disk data existed): {}", srv.meta.name),
        )
    } else {
        crate::i18n::pick(
            printer.lang(),
            format!("已注销（磁盘数据保留）: {}", srv.meta.name),
            format!("unregistered (on-disk data kept): {}", srv.meta.name),
        )
    };
    printer.print_outcome(
        &human,
        &RemoveJson {
            name: srv.meta.name.as_str().to_owned(),
            path: srv.dir.display().to_string(),
            purged: purge,
            disk_purged,
            linked: srv.linked,
        },
    )
}

/// `server remove --origin <remote>`：注销对端服务器（可选删盘）
pub(crate) fn remove_remote(
    origin: &mcsup::port::Origin,
    name: &str,
    purge: bool,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let cfg = crate::config::ConfigFile::locate(None).load()?;
    let members = bootstrap::remote_members(&cfg);
    let member = super::origin::member(&members, origin, lang)?.ok_or_else(|| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                "本地来源不需要远端删除",
                "local origin does not need remote removal",
            ),
            None,
        )
    })?;
    let disk_purged = member.servers.remove(name, purge).with_context(|| {
        crate::i18n::pick(lang, "在远端删除服务器", "removing the server on the peer")
    })?;
    printer.print_outcome(
        &crate::i18n::pick(
            lang,
            format!("已在 {} 注销服务器 {name}", member.name),
            format!("removed server {name} on {}", member.name),
        ),
        &RemoveJson {
            name: name.to_owned(),
            path: String::new(),
            purged: purge,
            disk_purged,
            linked: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 新建工作区目录：显式目录优先；`--workspace` 是路径时按路径；
    /// 否则落在数据目录下的 workspaces/<名称>
    #[test]
    fn new_workspace_dir_prefers_explicit_dir_then_path_then_name() {
        let explicit = Path::new("/tmp/ws-explicit");
        assert_eq!(
            resolve_new_workspace_dir(Lang::Zh, Some(explicit), Some("other"), Some("named"))
                .expect("explicit"),
            absolute_path(explicit)
        );
        assert_eq!(
            resolve_new_workspace_dir(Lang::Zh, None, Some("/tmp/ws-path"), None).expect("path"),
            absolute_path(Path::new("/tmp/ws-path"))
        );
        assert_eq!(
            resolve_new_workspace_dir(Lang::Zh, None, None, Some("named")).expect("name"),
            crate::config::platform_data_dir()
                .join("workspaces")
                .join("named")
        );
    }

    /// 无目录、无名称时无法推导新建位置，必须报用法错误（2）
    #[test]
    fn new_workspace_dir_without_input_is_usage_error() {
        let err = resolve_new_workspace_dir(Lang::Zh, None, None, None).expect_err("缺名称/目录");
        assert_eq!(Printer::exit_code(&err), 2);
    }

    #[test]
    fn resolve_flag_variant_gates_by_capability_with_usage_exit() {
        let available = vec![ServerVariant::Vanilla];
        assert_eq!(
            resolve_flag_variant("vanilla", &available, Lang::Zh).expect("可用变体"),
            ServerVariant::Vanilla
        );

        // 名字非法（含路径分隔符）→ 用法错误（2），并列出可用变体（help 经
        // miette 渲染携带）
        let unknown = resolve_flag_variant("../evil", &available, Lang::Zh).expect_err("未知变体");
        assert_eq!(Printer::exit_code(&unknown), 2);
        let msg = unknown.to_string();
        assert!(msg.contains("未知变体"), "{msg}");
        let rendered = format!(
            "{:?}",
            miette::Report::new(unknown.downcast::<AppDiagnostic>().expect("usage 诊断"))
        );
        assert!(rendered.contains("vanilla"), "{rendered}");

        // 名字合法但未注册 provider → 同判用法错误（2），文案区分。
        // `paper` 是内置名，`paperx` 是合法第三方名：两者都走这一分支
        for raw in ["paper", "paperx"] {
            let unsupported = resolve_flag_variant(raw, &available, Lang::Zh).expect_err("未支持");
            assert_eq!(Printer::exit_code(&unsupported), 2);
            let msg = unsupported.to_string();
            assert!(msg.contains("暂未支持"), "{msg}");
            let rendered = format!(
                "{:?}",
                miette::Report::new(unsupported.downcast::<AppDiagnostic>().expect("usage 诊断"))
            );
            assert!(rendered.contains("vanilla"), "{rendered}");
        }
    }

    /// 第三方变体：名字合法且已注册来源时可以通过 `--variant` 指定
    ///
    /// 这是 `Custom` 变体在设计上的唯一用途——宿主注册了来源，入口就必须能
    /// 指名它。用 `parse`（只认内置名）做第一级判定时这里会失败。
    #[test]
    fn resolve_flag_variant_accepts_registered_custom_variant() {
        let custom = ServerVariant::from_id("acme-server").expect("合法第三方名");
        let available = vec![ServerVariant::Vanilla, custom.clone()];
        assert_eq!(
            resolve_flag_variant("acme-server", &available, Lang::Zh).expect("已注册"),
            custom
        );
    }

    /// 空名与首尾空白都在领域层被拒（`VariantId` 不隐式 trim）
    ///
    /// 静默 trim 会让 `--variant " paper"` 与 `paper` 变成同一个变体的两种写法，
    /// 存储里读回来就分不清用户原本敲了什么。内部空格不受影响。
    #[test]
    fn resolve_flag_variant_does_not_trim_names() {
        let available = vec![ServerVariant::Vanilla];
        for raw in ["", " ", "\t", "vanilla ", " vanilla"] {
            let err = resolve_flag_variant(raw, &available, Lang::Zh).expect_err("应被拒");
            assert!(err.to_string().contains("未知变体"), "{raw:?}: {err}");
        }
        // 内部空格是名字的一部分，不是首尾空白
        let err = resolve_flag_variant("acme server", &available, Lang::Zh).expect_err("未注册");
        assert!(err.to_string().contains("暂未支持"), "{err}");
    }

    #[test]
    fn default_variant_prefers_vanilla_then_first_available() {
        assert_eq!(
            default_variant(&[ServerVariant::Vanilla, ServerVariant::Paper], Lang::Zh)
                .expect("默认"),
            ServerVariant::Vanilla
        );
        assert_eq!(
            default_variant(&[ServerVariant::Paper, ServerVariant::Fabric], Lang::Zh)
                .expect("首个"),
            ServerVariant::Paper
        );
        assert!(
            default_variant(&[], Lang::Zh)
                .expect_err("空能力集必须报错")
                .to_string()
                .contains("provider")
        );
    }

    /// `--variant modrinth` 是整合包的短名别名，存储仍用 `modrinth-modpack`
    #[test]
    fn modrinth_alias_resolves_to_modpack_variant() {
        let available = vec![ServerVariant::Vanilla, ServerVariant::ModrinthModpack];
        for raw in ["modrinth", "MODRINTH", "modrinth-modpack"] {
            assert_eq!(
                resolve_flag_variant(raw, &available, Lang::Zh).expect(raw),
                ServerVariant::ModrinthModpack
            );
        }
        // 未注册时别名同样走「暂未支持」，不静默失败
        assert!(resolve_flag_variant("modrinth", &[], Lang::Zh).is_err());
    }

    /// 展示标签由 `bootstrap::variant_label` 统一给出，并能回映变体
    #[test]
    fn variant_label_round_trips_and_unknown_is_none() {
        let available = vec![ServerVariant::Vanilla, ServerVariant::Fabric];
        let fabric = bootstrap::variant_label(&ServerVariant::Fabric);
        assert_eq!(
            bootstrap::variant_from_label(&fabric, &available),
            Some(ServerVariant::Fabric)
        );
        assert_eq!(
            bootstrap::variant_from_label("fabric", &available),
            Some(ServerVariant::Fabric),
            "展示标签就是名字本身，`as_str` 直接命中"
        );
        assert_eq!(bootstrap::variant_from_label("paper", &available), None);
    }
}
