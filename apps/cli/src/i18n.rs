//! 结构化错误 → 面向用户的提示文本
//!
//! `domain` / `service` 的错误只携带判定与参数（`kind()`），文案在这里集中渲染。
//! 判定与语言无关，同一份 `kind()` 按 [`Lang`] 出不同文案。
//!
//! 语言由 [`crate::locale`] 解析（`--lang` > `MCSUP_LANG` > 平台 locale），
//! 显式传入而非全局状态：入口拿到的 `Lang` 一路往下传，测试可以直接断言两种语言。
//!
//! 各错误的 `Display` 仍是英文短句，只用于日志与调试，不是用户口径。

use crate::locale::Lang;
use mcsup::service::{
    BackupErrorKind, BusyOp, CheckKind, InstallJarErrorKind, LifecycleErrorKind,
    ServerRegistryErrorKind, SettingsErrorKind, WorkspaceErrorKind,
};
use std::time::Duration;

/// 二元短文案：没有参数要插时才用这个，避免为零散界面词各建一个函数
#[must_use]
pub(crate) const fn text(lang: Lang, zh: &'static str, en: &'static str) -> &'static str {
    match lang {
        Lang::Zh => zh,
        Lang::En => en,
    }
}

/// 二元长文案：两侧已渲染好（含插值）时用这个
#[must_use]
pub(crate) fn pick(lang: Lang, zh: String, en: String) -> String {
    match lang {
        Lang::Zh => zh,
        Lang::En => en,
    }
}

/// 生命周期错误提示
#[must_use]
pub(crate) fn lifecycle(kind: &LifecycleErrorKind<'_>, lang: Lang) -> String {
    if !lang.is_zh() {
        return lifecycle_en(kind);
    }
    match kind {
        LifecycleErrorKind::NoJar { name } => {
            format!("服务器 {name} 未安装核心，可执行 mcsup server install {name}")
        }
        LifecycleErrorKind::EulaRequired => {
            "EULA 未同意（需 eula=true），可加 --accept-eula 重试".to_owned()
        }
        LifecycleErrorKind::NoJava => {
            "未发现可用 Java 运行时，可执行 mcsup java install 安装".to_owned()
        }
        LifecycleErrorKind::UnknownJavaMajor { name, major } => format!(
            "服务器 {name} 绑定的 Java 大版本 {major} 无已注册运行时，可执行 mcsup java install {major} 安装"
        ),
        LifecycleErrorKind::AlreadyRunning => "服务器已在运行".to_owned(),
        LifecycleErrorKind::NotRunning => "服务器未在运行".to_owned(),
        LifecycleErrorKind::Busy { name, op } => match op {
            BusyOp::Remove | BusyOp::InstallJar => format!(
                "服务器 {name} 正在运行（{}），请先 mcsup server stop {name}",
                busy_op(*op, lang)
            ),
            BusyOp::WorkspaceRemove => {
                format!("工作区内有服务器在运行（{name}），请先 mcsup server stop {name}")
            }
            _ => format!("服务器 {name} 正在运行，请先 mcsup server stop {name}"),
        },
        LifecycleErrorKind::StopTimedOut { waited } => format!(
            "停机超时（已等待 {}），可加 --force 强杀",
            fmt_duration(*waited)
        ),
        // non_exhaustive：新增判定在补文案前沿用英文措辞，不漏掉提示
        _ => lifecycle_en(kind),
    }
}

fn lifecycle_en(kind: &LifecycleErrorKind<'_>) -> String {
    match kind {
        LifecycleErrorKind::NoJar { name } => {
            format!("server {name} has no core jar; run `mcsup server install {name}`")
        }
        LifecycleErrorKind::EulaRequired => {
            "the Mojang EULA is not accepted; retry with --accept-eula".to_owned()
        }
        LifecycleErrorKind::NoJava => {
            "no usable Java runtime found; run `mcsup java install`".to_owned()
        }
        LifecycleErrorKind::UnknownJavaMajor { name, major } => format!(
            "server {name} is pinned to Java {major}, which is not registered; run `mcsup java install {major}`"
        ),
        LifecycleErrorKind::AlreadyRunning => "the server is already running".to_owned(),
        LifecycleErrorKind::NotRunning => "the server is not running".to_owned(),
        LifecycleErrorKind::Busy { name, op } => match op {
            BusyOp::WorkspaceRemove => format!(
                "a server in this workspace is still running ({name}); run `mcsup server stop {name}` first"
            ),
            other => format!(
                "server {name} is running; refusing to {} — run `mcsup server stop {name}` first",
                busy_op_en(*other)
            ),
        },
        LifecycleErrorKind::StopTimedOut { waited } => format!(
            "stop timed out after {}; add --force to kill it",
            fmt_duration(*waited)
        ),
        _ => "operation failed".to_owned(),
    }
}

/// 运行中被拒绝的操作
#[must_use]
pub(crate) fn busy_op(op: BusyOp, lang: Lang) -> &'static str {
    if !lang.is_zh() {
        return busy_op_en(op);
    }
    match op {
        BusyOp::Remove => "删除",
        BusyOp::InstallJar => "更换核心",
        BusyOp::WorkspaceRemove => "注销工作区",
        _ => "该操作",
    }
}

fn busy_op_en(op: BusyOp) -> &'static str {
    match op {
        BusyOp::Remove => "remove it",
        BusyOp::InstallJar => "replace its core",
        BusyOp::WorkspaceRemove => "remove the workspace",
        _ => "proceed",
    }
}

/// 核心安装错误提示
#[must_use]
pub(crate) fn install_jar(kind: &InstallJarErrorKind<'_>, lang: Lang) -> String {
    match kind {
        InstallJarErrorKind::Inspect => match lang {
            Lang::Zh => "无法检查世界或目标核心的兼容性".to_owned(),
            Lang::En => "failed to inspect the world or the target core".to_owned(),
        },
        InstallJarErrorKind::Downgrade {
            world,
            target,
            world_version,
            target_version,
        } => match lang {
            Lang::Zh => format!(
                "世界由 {world_version}（DataVersion {world}）创建，目标版本 {target_version}（DataVersion {target}）更低；Minecraft 不支持降级世界。请改用 {world_version} 或更高版本，或备份并移走世界目录后重试"
            ),
            Lang::En => format!(
                "the world was created by {world_version} (DataVersion {world}), which is newer than the target {target_version} (DataVersion {target}); Minecraft cannot downgrade a world. Use {world_version} or newer, or back up and move the world directory away before retrying"
            ),
        },
        // non_exhaustive：新增判定在补文案前沿用英文措辞
        _ => "failed to install the core".to_owned(),
    }
}

/// 服务属性错误提示
#[must_use]
pub(crate) fn settings(kind: &SettingsErrorKind<'_>, lang: Lang) -> String {
    match kind {
        SettingsErrorKind::UnknownJavaMajor { major, .. } => match lang {
            Lang::Zh => {
                format!("未注册的大版本 Java {major}，可执行 mcsup java install {major} 安装")
            }
            Lang::En => format!("Java {major} is not registered; run `mcsup java install {major}`"),
        },
        _ => "failed to update server settings".to_owned(),
    }
}

/// 备份错误提示
#[must_use]
pub(crate) fn backup(kind: &BackupErrorKind<'_>, lang: Lang) -> String {
    match kind {
        BackupErrorKind::SnapshotNotFound { id } => pick(
            lang,
            format!("快照 {id} 不存在，可用 mcsup server backup ls <name> 查看"),
            format!("snapshot {id} does not exist; run `mcsup server backup ls <name>`"),
        ),
        BackupErrorKind::NotRestorable { id, status } => pick(
            lang,
            format!("快照 {id} 不可恢复（状态: {status}），只能从完整快照恢复"),
            format!("snapshot {id} is not restorable (status: {status})"),
        ),
        BackupErrorKind::RestoreWhileRunning { name } => pick(
            lang,
            format!("服务器 {name} 正在运行，原地恢复前需先 mcsup server stop {name}"),
            format!(
                "server {name} is running; run `mcsup server stop {name}` before restoring in place"
            ),
        ),
        BackupErrorKind::TargetExists { name } => pick(
            lang,
            format!("目标服务器 {name} 已存在，请换一个 --to 名称"),
            format!("target server {name} already exists; choose another --to name"),
        ),
        BackupErrorKind::EmptyScope => pick(
            lang,
            "备份范围内没有任何文件，无可备份内容".to_owned(),
            "nothing to back up in the configured scope".to_owned(),
        ),
        BackupErrorKind::VerifyFailed { id, broken } => pick(
            lang,
            format!("快照 {id} 校验失败：{} 个对象损坏或缺失", broken.len()),
            format!(
                "snapshot {id} is broken: {} corrupt or missing object(s)",
                broken.len()
            ),
        ),
        BackupErrorKind::InvalidLabel { raw } => pick(
            lang,
            format!("快照标签非法: {raw}（不能包含控制字符或换行，长度有上限）"),
            format!("invalid snapshot label: {raw}"),
        ),
        BackupErrorKind::ConflictingRetention => pick(
            lang,
            "--keep 与 --older-than 只能二选一".to_owned(),
            "--keep and --older-than are mutually exclusive".to_owned(),
        ),
        _ => "backup operation failed".to_owned(),
    }
}

/// doctor 发现的问题描述
#[must_use]
pub(crate) fn check(kind: &CheckKind, lang: Lang) -> String {
    if !lang.is_zh() {
        return check_en(kind);
    }
    match kind {
        CheckKind::NoCurrentWorkspace => "没有任何当前工作区".to_owned(),
        CheckKind::WorkspaceDirMissing { .. } => "工作目录在磁盘上不存在".to_owned(),
        CheckKind::WorkspaceMarkerMissing { .. } => {
            "工作目录缺少 workspace.toml，身份无法确认".to_owned()
        }
        CheckKind::WorkspaceMarkerMismatch { .. } => {
            "workspace.toml 的身份与注册记录不一致".to_owned()
        }
        CheckKind::ServersDirUnreadable { reason, .. } => {
            format!("servers/ 无法列举（{reason}），该工作区的服务器盘点不可信")
        }
        CheckKind::DanglingLink { .. } => "符号链接指向的目标不存在".to_owned(),
        CheckKind::OnDiskNotRegistered { .. } => "磁盘上有未登记的服务器目录".to_owned(),
        CheckKind::RegisteredButMissing { .. } => "服务器已登记但磁盘上找不到".to_owned(),
        CheckKind::RelDirNotDirectChild { .. } => {
            "服务器记录的目录不是 servers/ 的直接子目录".to_owned()
        }
        CheckKind::OrphanServerRecord { .. } => "服务器记录指向的工作区已不存在".to_owned(),
        _ => check_en(kind),
    }
}

fn check_en(kind: &CheckKind) -> String {
    match kind {
        CheckKind::NoCurrentWorkspace => "no current workspace".to_owned(),
        CheckKind::WorkspaceDirMissing { .. } => "the workspace directory is missing".to_owned(),
        CheckKind::WorkspaceMarkerMissing { .. } => {
            "the workspace directory has no workspace.toml, so its identity is unknown".to_owned()
        }
        CheckKind::WorkspaceMarkerMismatch { .. } => {
            "workspace.toml does not match the registry record".to_owned()
        }
        CheckKind::ServersDirUnreadable { reason, .. } => format!(
            "servers/ cannot be listed ({reason}); the server inventory of this workspace is unreliable"
        ),
        CheckKind::DanglingLink { .. } => "a symlink points to a missing target".to_owned(),
        CheckKind::OnDiskNotRegistered { .. } => {
            "a server directory on disk is not registered".to_owned()
        }
        CheckKind::RegisteredButMissing { .. } => {
            "a registered server cannot be found on disk".to_owned()
        }
        CheckKind::RelDirNotDirectChild { .. } => {
            "a server record points outside the direct children of servers/".to_owned()
        }
        CheckKind::OrphanServerRecord { .. } => {
            "a server record points to a workspace that no longer exists".to_owned()
        }
        _ => "unknown finding".to_owned(),
    }
}

/// doctor 发现问题的定位行
#[must_use]
pub(crate) fn check_location(kind: &CheckKind, lang: Lang) -> String {
    if matches!(kind, CheckKind::NoCurrentWorkspace) {
        return match lang {
            Lang::Zh => "运行 mcsup workspace use <name> 指定当前工作区".to_owned(),
            Lang::En => "run `mcsup workspace use <name>` to pick a workspace".to_owned(),
        };
    }
    kind.locate()
}

/// doctor 发现问题的修复建议：只给动作，doctor 本身不写任何东西
#[must_use]
pub(crate) fn check_fix(kind: &CheckKind, lang: Lang) -> String {
    if !lang.is_zh() {
        return check_fix_en(kind);
    }
    match kind {
        CheckKind::NoCurrentWorkspace => "`mcsup workspace use <name>`".to_owned(),
        CheckKind::WorkspaceDirMissing { .. } => {
            "把目录移回原位，或 `mcsup workspace remove <name>` 注销".to_owned()
        }
        CheckKind::WorkspaceMarkerMissing { path } => format!(
            "确认目录归属后重建 workspace.toml（id 需与记录一致），或注销后重新 `workspace init {}`",
            path.display()
        ),
        CheckKind::WorkspaceMarkerMismatch { .. } => {
            "该目录已不是这条记录的工作区：确认后注销记录，或恢复原 marker".to_owned()
        }
        CheckKind::ServersDirUnreadable { .. } => "检查目录权限".to_owned(),
        CheckKind::DanglingLink { path } => format!(
            "`mcsup server remove <name>` 移除链接，或恢复目标后重试（链接：{}）",
            path.display()
        ),
        CheckKind::OnDiskNotRegistered { path } => format!(
            "`mcsup server link {}` 重新登记，或删除该目录",
            path.display()
        ),
        CheckKind::RegisteredButMissing { name, .. } => {
            format!("`mcsup server remove {name}`，或把目录恢复到原位")
        }
        CheckKind::RelDirNotDirectChild { name, .. } => {
            format!("`mcsup server remove {name}` 后重新 `server link`")
        }
        CheckKind::OrphanServerRecord { name, .. } => {
            format!("`mcsup server remove {name}` 清理孤儿记录")
        }
        _ => check_fix_en(kind),
    }
}

fn check_fix_en(kind: &CheckKind) -> String {
    match kind {
        CheckKind::NoCurrentWorkspace => "`mcsup workspace use <name>`".to_owned(),
        CheckKind::WorkspaceDirMissing { .. } => {
            "move the directory back, or unregister it with `mcsup workspace remove <name>`".to_owned()
        }
        CheckKind::WorkspaceMarkerMissing { path } => format!(
            "rebuild workspace.toml once the ownership is confirmed (id must match the record), or unregister and run `workspace init {}` again",
            path.display()
        ),
        CheckKind::WorkspaceMarkerMismatch { .. } => {
            "this directory is no longer the workspace of this record: unregister the record, or restore the original marker".to_owned()
        }
        CheckKind::ServersDirUnreadable { .. } => "check directory permissions".to_owned(),
        CheckKind::DanglingLink { path } => format!(
            "run `mcsup server remove <name>` to drop the link, or restore the target and retry (link: {})",
            path.display()
        ),
        CheckKind::OnDiskNotRegistered { path } => format!(
            "run `mcsup server link {}` to register it again, or delete the directory",
            path.display()
        ),
        CheckKind::RegisteredButMissing { name, .. } => {
            format!("run `mcsup server remove {name}`, or restore the directory to its original place")
        }
        CheckKind::RelDirNotDirectChild { name, .. } => {
            format!("run `mcsup server remove {name}` and then `server link` again")
        }
        CheckKind::OrphanServerRecord { name, .. } => {
            format!("run `mcsup server remove {name}` to clear the orphan record")
        }
        _ => "inspect and fix the finding manually".to_owned(),
    }
}

/// 服务器注册表错误提示
#[must_use]
pub(crate) fn registry(kind: &ServerRegistryErrorKind<'_>, lang: Lang) -> String {
    match kind {
        ServerRegistryErrorKind::AlreadyExists { name } => match lang {
            Lang::Zh => format!("服务器 {name} 已存在"),
            Lang::En => format!("server {name} already exists"),
        },
        ServerRegistryErrorKind::NotFound { name } => match lang {
            Lang::Zh => format!("服务器 {name} 不存在"),
            Lang::En => format!("server {name} not found"),
        },
        ServerRegistryErrorKind::TargetInsideWorkspace { path } => match lang {
            Lang::Zh => format!(
                "目标位于某个工作区的 servers/ 内，不能链接：{}",
                path.display()
            ),
            Lang::En => format!(
                "the target is inside a workspace servers/ directory and cannot be linked: {}",
                path.display()
            ),
        },
        ServerRegistryErrorKind::WorkspaceMismatch { owner, selected } => match lang {
            Lang::Zh => format!("该目录属于工作区 `{owner}`，但 --workspace 解析到 `{selected}`"),
            Lang::En => format!(
                "the target belongs to workspace `{owner}`, but --workspace resolved to `{selected}`"
            ),
        },
        _ => "server registry operation failed".to_owned(),
    }
}

/// 工作区错误提示
#[must_use]
pub(crate) fn workspace(kind: &WorkspaceErrorKind<'_>, lang: Lang) -> String {
    match kind {
        WorkspaceErrorKind::NotEmpty { path } => match lang {
            Lang::Zh => format!("目录非空：{}", path.display()),
            Lang::En => format!("directory is not empty: {}", path.display()),
        },
        WorkspaceErrorKind::NotAWorkspace { path } => match lang {
            Lang::Zh => format!("不是工作区（缺少 workspace.toml）：{}", path.display()),
            Lang::En => format!(
                "not a workspace (missing workspace.toml): {}",
                path.display()
            ),
        },
        WorkspaceErrorKind::AlreadyExists { name } => match lang {
            Lang::Zh => format!("已存在：{name}"),
            Lang::En => format!("already exists: {name}"),
        },
        WorkspaceErrorKind::NotFound { name } => match lang {
            Lang::Zh => format!("工作区不存在：{name}"),
            Lang::En => format!("workspace not found: {name}"),
        },
        WorkspaceErrorKind::Ambiguous {
            selector,
            candidates,
        } => match lang {
            Lang::Zh => format!(
                "工作区选择器 {selector:?} 匹配到多个候选：{}",
                candidates.join(", ")
            ),
            Lang::En => format!(
                "ambiguous workspace selector {selector:?}; candidates: {}",
                candidates.join(", ")
            ),
        },
        WorkspaceErrorKind::CwdInsideTarget { cwd, target } => match lang {
            Lang::Zh => format!(
                "当前工作目录在待删目录内（{} 位于 {}），先 cd 出去再重试",
                cwd.display(),
                target.display()
            ),
            Lang::En => format!(
                "current working directory {} is inside {}; move out of it first",
                cwd.display(),
                target.display()
            ),
        },
        WorkspaceErrorKind::NoCurrent => match lang {
            Lang::Zh => {
                "尚无当前工作区，先执行 mcsup workspace init 或 mcsup workspace use".to_owned()
            }
            Lang::En => {
                "no current workspace; run `mcsup workspace init` or `mcsup workspace use` first"
                    .to_owned()
            }
        },
        _ => "workspace operation failed".to_owned(),
    }
}

/// 时长：`30s` / `1m30s`
fn fmt_duration(waited: Duration) -> String {
    let secs = waited.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcsup::domain::java::JavaMajor;
    use mcsup::domain::server::world::DataVersion;
    use mcsup::service::{
        InstallJarError, LifecycleError, ServerRegistryError, SettingsError, WorkspaceError,
    };

    #[test]
    fn lifecycle_variants_render_chinese() {
        let no_jar = LifecycleError::NoJar {
            name: "alpha".to_owned(),
        };
        assert_eq!(
            lifecycle(&no_jar.kind().expect("kind"), Lang::Zh),
            "服务器 alpha 未安装核心，可执行 mcsup server install alpha"
        );
        assert_eq!(
            lifecycle(
                &LifecycleError::EulaRequired.kind().expect("kind"),
                Lang::Zh
            ),
            "EULA 未同意（需 eula=true），可加 --accept-eula 重试"
        );
        assert_eq!(
            lifecycle(
                &LifecycleError::AlreadyRunning.kind().expect("kind"),
                Lang::Zh
            ),
            "服务器已在运行"
        );
        let timeout = LifecycleError::StopTimedOut {
            waited: Duration::from_secs(90),
        };
        assert_eq!(
            lifecycle(&timeout.kind().expect("kind"), Lang::Zh),
            "停机超时（已等待 1m30s），可加 --force 强杀"
        );
    }

    /// 同一份判定在英文下必须给出英文，且不残留中文
    #[test]
    fn lifecycle_variants_render_english() {
        let no_jar = LifecycleError::NoJar {
            name: "alpha".to_owned(),
        };
        let text = lifecycle(&no_jar.kind().expect("kind"), Lang::En);
        assert!(text.contains("alpha"), "{text}");
        assert!(text.contains("no core jar"), "{text}");
        assert!(!text.contains("服务器"), "{text}");

        let busy = LifecycleError::Busy {
            name: "alpha".to_owned(),
            op: BusyOp::Remove,
        };
        let text = lifecycle(&busy.kind().expect("kind"), Lang::En);
        assert!(text.contains("is running"), "{text}");
        assert!(text.contains("remove it"), "{text}");
        assert!(!text.contains("正在运行"), "{text}");
    }

    /// 运行中被拒：删除 / 换核心按操作名，工作区级拦截点名区内第一台活着的服务器
    #[test]
    fn busy_renders_per_operation() {
        let remove = LifecycleError::Busy {
            name: "alpha".to_owned(),
            op: BusyOp::Remove,
        };
        assert_eq!(
            lifecycle(&remove.kind().expect("kind"), Lang::Zh),
            "服务器 alpha 正在运行（删除），请先 mcsup server stop alpha"
        );
        let install = LifecycleError::Busy {
            name: "alpha".to_owned(),
            op: BusyOp::InstallJar,
        };
        assert_eq!(
            lifecycle(&install.kind().expect("kind"), Lang::Zh),
            "服务器 alpha 正在运行（更换核心），请先 mcsup server stop alpha"
        );
        let ws = LifecycleError::Busy {
            name: "alpha".to_owned(),
            op: BusyOp::WorkspaceRemove,
        };
        assert_eq!(
            lifecycle(&ws.kind().expect("kind"), Lang::Zh),
            "工作区内有服务器在运行（alpha），请先 mcsup server stop alpha"
        );
    }

    fn downgrade() -> InstallJarError {
        InstallJarError::Downgrade {
            world: DataVersion::new(4786),
            target: DataVersion::new(4554),
            world_version: "26.1".to_owned(),
            target_version: "1.21.9".to_owned(),
        }
    }

    #[test]
    fn install_jar_downgrade_renders_both_languages() {
        let err = downgrade();
        let zh = install_jar(&err.kind().expect("kind"), Lang::Zh);
        assert!(zh.contains("世界由 26.1"), "{zh}");
        assert!(zh.contains("Minecraft 不支持降级世界"), "{zh}");

        let en = install_jar(&err.kind().expect("kind"), Lang::En);
        assert!(en.contains("cannot downgrade a world"), "{en}");
        assert!(!en.contains("世界"), "{en}");

        assert_eq!(
            install_jar(&InstallJarErrorKind::Inspect, Lang::Zh),
            "无法检查世界或目标核心的兼容性"
        );
        assert_eq!(
            install_jar(&InstallJarErrorKind::Inspect, Lang::En),
            "failed to inspect the world or the target core"
        );
    }

    #[test]
    fn settings_unknown_major_renders_both_languages() {
        let err = SettingsError::UnknownJavaMajor {
            name: "alpha".to_owned(),
            major: JavaMajor::parse(17).expect("17"),
        };
        assert_eq!(
            settings(&err.kind().expect("kind"), Lang::Zh),
            "未注册的大版本 Java 17，可执行 mcsup java install 17 安装"
        );
        assert_eq!(
            settings(&err.kind().expect("kind"), Lang::En),
            "Java 17 is not registered; run `mcsup java install 17`"
        );
    }

    #[test]
    fn registry_renders_both_languages() {
        let err = ServerRegistryError::NotFound("alpha".to_owned());
        assert_eq!(
            registry(&err.kind().expect("kind"), Lang::Zh),
            "服务器 alpha 不存在"
        );
        assert_eq!(
            registry(&err.kind().expect("kind"), Lang::En),
            "server alpha not found"
        );
    }

    #[test]
    fn workspace_renders_both_languages() {
        let err = WorkspaceError::NoCurrent;
        assert_eq!(
            workspace(&err.kind().expect("kind"), Lang::Zh),
            "尚无当前工作区，先执行 mcsup workspace init 或 mcsup workspace use"
        );
        assert_eq!(
            workspace(&err.kind().expect("kind"), Lang::En),
            "no current workspace; run `mcsup workspace init` or `mcsup workspace use` first"
        );
        let missing = WorkspaceError::NotFound("ws".to_owned());
        assert_eq!(
            workspace(&missing.kind().expect("kind"), Lang::Zh),
            "工作区不存在：ws"
        );
        // 包装变体不下钻：返回 None 让入口沿错误链继续找
        let wrapped = WorkspaceError::AlreadyExists("ws".to_owned());
        assert!(matches!(
            wrapped.kind(),
            Some(WorkspaceErrorKind::AlreadyExists { .. })
        ));
    }

    #[test]
    fn check_renders_both_languages() {
        let kind = CheckKind::NoCurrentWorkspace;
        assert_eq!(check(&kind, Lang::Zh), "没有任何当前工作区");
        assert_eq!(check(&kind, Lang::En), "no current workspace");
        assert_eq!(check_fix(&kind, Lang::Zh), "`mcsup workspace use <name>`");
        assert_eq!(
            check_location(&kind, Lang::En),
            "run `mcsup workspace use <name>` to pick a workspace"
        );
    }
}
