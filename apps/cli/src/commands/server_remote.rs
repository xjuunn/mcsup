//! `server` 子命令里「对端一侧」的处理：`--origin <remote>` 分支
//!
//! 与本地路径分开成独立模块，只为把两种执行位置的差异收在一处：本文件里的
//! 每个函数都在和对端 RPC 打交道（登记、装核心、装整合包、挂链接、注销），
//! 而 `server_ops` 只管本机磁盘与交互。混在同一个文件里时，改本地逻辑要在一
//! 屏远端分支里翻，改远端逻辑又容易顺手动到本地判定。
//!
//! 这里**不承载判定**：该不该路由到远端、对端能力如何取舍，都由
//! `remote_create_target` / service 与 backend 决定；本模块只负责把 CLI 的
//! 交互输入拼成一次远端调用，并把结果渲染出来。

use anyhow::{Context, Result};
use mcsup::domain::server::meta::ServerName;
use mcsup::domain::server::version::ServerVariant;
use mcsup::port::TaskStage;
use mcsup::protocol::cli::{CreateRow, RemoveJson};
use mcsup::service::WorkspaceError;
use std::path::Path;

use crate::bootstrap;
use crate::config::ConfigFile;
use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use crate::progress::{InstallSession, TaskSpinner};
use crate::prompts;

use super::install::select_version;
use super::server_ops::{is_modpack_flag, pick_variant, resolve_flag_variant};

/// 显式 `--origin <remote>` 时确定远端工作区名
///
/// - 给了 `--workspace`：去掉可选的来源前缀，来源必须与 `--origin` 一致；
/// - 没给：优先本机 current（若恰好在该远端），否则交给对端自己的 current。
///
/// 仍然**不写**对端 current；`--origin` 是用户显式指定的来源，未点名工作区时
/// 使用对端默认落点是既有语义。
pub(super) fn remote_workspace_for_origin(
    origin: &mcsup::port::Origin,
    workspace: Option<&str>,
    printer: Printer,
) -> Result<Option<String>> {
    if let Some(raw) = workspace {
        let selector = mcsup::service::Selector::parse(raw);
        if let Some(given) = selector.origin()
            && given != origin
        {
            return Err(AppDiagnostic::usage(
                crate::i18n::pick(
                    printer.lang(),
                    format!(
                        "--workspace 的来源 {given} 与 --origin {} 不一致",
                        origin.as_str()
                    ),
                    format!(
                        "--workspace origin {given} does not match --origin {}",
                        origin.as_str()
                    ),
                ),
                None,
            )
            .into());
        }
        return Ok(Some(selector.name().to_owned()));
    }
    let svc = bootstrap::workspace_service_with_remotes()?;
    match svc.current_sourced() {
        Ok(item) if &item.origin == origin => Ok(Some(item.value.name.as_str().to_owned())),
        // 没有本机 current（或不在该远端）：让对端用它自己的 current
        Ok(_)
        | Err(WorkspaceError::NoCurrent | WorkspaceError::CurrentOriginUnavailable { .. }) => {
            Ok(None)
        }
        Err(err) => Err(err.into()),
    }
}

/// 未显式 `--origin` 时，`--workspace` 或本机 current 指向远端 → 返回远端落点
///
/// 返回 `None` 表示按本地路径处理（本地工作区 / 新建 / 交互 init）。
pub(super) fn remote_create_target(
    workspace: Option<&str>,
    workspace_name: Option<&str>,
    workspace_dir: Option<&Path>,
) -> Result<Option<(mcsup::port::Origin, String)>> {
    // 带新建参数 = 在本机新建工作区，不路由远端
    if workspace_name.is_some() || workspace_dir.is_some() {
        return Ok(None);
    }
    let svc = bootstrap::workspace_service_with_remotes()?;
    if let Some(raw) = workspace {
        return match svc.resolve_sourced(raw) {
            Ok(item) if !item.origin.is_local() => {
                Ok(Some((item.origin, item.value.name.as_str().to_owned())))
            }
            // 本地命中 / 找不到：交给本地新建 / 交互 init 的既有路径
            Ok(_) | Err(WorkspaceError::NotFound(_)) => Ok(None),
            Err(err) => Err(err.into()),
        };
    }
    match svc.current_sourced() {
        Ok(item) if !item.origin.is_local() => {
            Ok(Some((item.origin, item.value.name.as_str().to_owned())))
        }
        Ok(_) | Err(WorkspaceError::NoCurrent) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// `server create --origin <remote>`：在对端登记服务器
///
/// - 普通变体：对端登记 + 下载核心（`ServerCreate` + `ServerInstall`），
///   客户端不下载、不拼远端路径。
/// - 整合包：对端读清单、装加载器、铺 overrides（`ServerCreateModpack`），
///   一次调用完成「登记 + 布置」。
#[allow(clippy::too_many_lines)]
pub(super) fn create_remote(
    origin: &mcsup::port::Origin,
    name: Option<&str>,
    version: Option<&str>,
    variant: Option<&str>,
    workspace: Option<&str>,
    modpack: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let cfg = ConfigFile::locate(None).load()?;
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
    // 路径的口径完全一致。进度用与本地同一个 InstallSession 渲染。
    let session = InstallSession::new(
        crate::i18n::pick(
            lang,
            format!("在 {} 安装 {} 的核心", member.name, meta.name),
            format!("installing the core of {} on {}", meta.name, member.name),
        ),
        printer.is_human(),
    );
    let installed = member
        .servers
        .install_with(meta.name.as_str(), &version, &session);
    session.finish();
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
    // 与本地 `server create --modpack` 同一个回显装配：阶段 / 字节进度由对端
    // 经 Task 帧回传，这里只负责画
    let session = InstallSession::new(TaskStage::Probing.message(), printer.is_human());
    let result = member
        .servers
        .create_modpack(workspace, name, project, version, &session);
    session.finish();
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

/// `server link --origin <remote>`：在对端注册一个已存在的目录
///
/// 符号链接、marker 与入库都在对端完成；`path` 是**目标机器**上的路径。
///
/// # Errors
///
/// 远端未就绪、对端拒绝（目标不存在 / 名称冲突 / 目标在工作区内…）时返回错误
pub(super) fn link_remote(
    origin: &mcsup::port::Origin,
    path: &Path,
    name: Option<&str>,
    workspace: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let cfg = ConfigFile::locate(None).load()?;
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

/// `server remove --origin <remote>`：注销对端服务器（可选删盘）
pub(crate) fn remove_remote(
    origin: &mcsup::port::Origin,
    name: &str,
    purge: bool,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let cfg = ConfigFile::locate(None).load()?;
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
