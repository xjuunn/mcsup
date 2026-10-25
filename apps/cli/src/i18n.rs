//! 结构化错误 → 面向用户的提示文本
//!
//! `domain` / `service` 的错误只携带判定与参数（`kind()`），文案在这里集中渲染。
//! 入口据此输出中文提示；将来接入多语言时只改这一处，判定逻辑不受影响。
//!
//! 各错误的 `Display` 仍是英文短句，只用于日志与调试，不是用户口径。

use mcsup::service::{
    BusyOp, CheckKind, InstallJarErrorKind, LifecycleErrorKind, SettingsErrorKind,
};
use std::time::Duration;

/// 生命周期错误提示
#[must_use]
pub(crate) fn lifecycle(kind: &LifecycleErrorKind<'_>) -> String {
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
                busy_op(*op)
            ),
            BusyOp::WorkspaceRemove => {
                format!("工作区内有服务器在运行（{name}），请先 mcsup server stop {name}")
            }
            _ => format!("服务器 {name} 正在运行，请先 mcsup server stop {name}"),
        },
        LifecycleErrorKind::StopTimedOut { waited } => {
            format!(
                "停机超时（已等待 {}），可加 --force 强杀",
                fmt_duration(*waited)
            )
        }
        // non_exhaustive：新增判定在此补文案前先从英文短句兜底
        _ => String::new(),
    }
}

/// 运行中被拒绝的操作
#[must_use]
pub(crate) fn busy_op(op: BusyOp) -> &'static str {
    match op {
        BusyOp::Remove => "删除",
        BusyOp::InstallJar => "更换核心",
        BusyOp::WorkspaceRemove => "注销工作区",
        _ => "该操作",
    }
}

/// 核心安装错误提示
#[must_use]
pub(crate) fn install_jar(kind: &InstallJarErrorKind<'_>) -> String {
    match kind {
        InstallJarErrorKind::Inspect => "无法检查世界或目标核心的兼容性".to_owned(),
        InstallJarErrorKind::Downgrade {
            world,
            target,
            world_version,
            target_version,
        } => format!(
            "世界由 {world_version}（DataVersion {world}）创建，目标版本 {target_version}（DataVersion {target}）更低；Minecraft 不支持降级世界。请改用 {world_version} 或更高版本，或备份并移走世界目录后重试"
        ),
        _ => String::new(),
    }
}

/// 服务属性错误提示
#[must_use]
pub(crate) fn settings(kind: &SettingsErrorKind<'_>) -> String {
    match kind {
        SettingsErrorKind::UnknownJavaMajor { major, .. } => {
            format!("未注册的大版本 Java {major}，可执行 mcsup java install {major} 安装")
        }
        _ => String::new(),
    }
}

/// doctor 发现的问题描述
#[must_use]
pub(crate) fn check(kind: &CheckKind) -> String {
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
        _ => String::new(),
    }
}

/// doctor 发现问题的定位行
#[must_use]
pub(crate) fn check_location(kind: &CheckKind) -> String {
    if matches!(kind, CheckKind::NoCurrentWorkspace) {
        return "运行 mcsup workspace use <name> 指定当前工作区".to_owned();
    }
    kind.locate()
}

/// doctor 发现问题的修复建议：只给动作，doctor 本身不写任何东西
#[must_use]
pub(crate) fn check_fix(kind: &CheckKind) -> String {
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
        _ => String::new(),
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
    use mcsup::service::{InstallJarError, LifecycleError, SettingsError};

    #[test]
    fn lifecycle_variants_render_chinese() {
        let no_jar = LifecycleError::NoJar {
            name: "alpha".to_owned(),
        };
        assert_eq!(
            lifecycle(&no_jar.kind().expect("kind")),
            "服务器 alpha 未安装核心，可执行 mcsup server install alpha"
        );
        assert_eq!(
            lifecycle(&LifecycleError::EulaRequired.kind().expect("kind")),
            "EULA 未同意（需 eula=true），可加 --accept-eula 重试"
        );
        assert_eq!(
            lifecycle(&LifecycleError::AlreadyRunning.kind().expect("kind")),
            "服务器已在运行"
        );
        let timeout = LifecycleError::StopTimedOut {
            waited: Duration::from_secs(90),
        };
        assert_eq!(
            lifecycle(&timeout.kind().expect("kind")),
            "停机超时（已等待 1m30s），可加 --force 强杀"
        );
    }

    /// 运行中被拒：删除 / 换核心按操作名，工作区级拦截点名区内第一台活着的服务器
    #[test]
    fn busy_renders_per_operation() {
        let remove = LifecycleError::Busy {
            name: "alpha".to_owned(),
            op: BusyOp::Remove,
        };
        assert_eq!(
            lifecycle(&remove.kind().expect("kind")),
            "服务器 alpha 正在运行（删除），请先 mcsup server stop alpha"
        );
        let install = LifecycleError::Busy {
            name: "alpha".to_owned(),
            op: BusyOp::InstallJar,
        };
        assert_eq!(
            lifecycle(&install.kind().expect("kind")),
            "服务器 alpha 正在运行（更换核心），请先 mcsup server stop alpha"
        );
        let ws = LifecycleError::Busy {
            name: "alpha".to_owned(),
            op: BusyOp::WorkspaceRemove,
        };
        assert_eq!(
            lifecycle(&ws.kind().expect("kind")),
            "工作区内有服务器在运行（alpha），请先 mcsup server stop alpha"
        );
    }

    #[test]
    fn install_jar_downgrade_renders_chinese() {
        let err = InstallJarError::Downgrade {
            world: DataVersion::new(4786),
            target: DataVersion::new(4554),
            world_version: "26.1".to_owned(),
            target_version: "1.21.9".to_owned(),
        };
        let text = install_jar(&err.kind().expect("kind"));
        assert!(text.contains("世界由 26.1"), "{text}");
        assert!(text.contains("Minecraft 不支持降级世界"), "{text}");
        assert!(!text.contains("not supported"), "{text}");
        assert_eq!(
            install_jar(&InstallJarErrorKind::Inspect),
            "无法检查世界或目标核心的兼容性"
        );
    }

    #[test]
    fn settings_unknown_major_renders_chinese() {
        let err = SettingsError::UnknownJavaMajor {
            name: "alpha".to_owned(),
            major: JavaMajor::parse(17).expect("17"),
        };
        assert_eq!(
            settings(&err.kind().expect("kind")),
            "未注册的大版本 Java 17，可执行 mcsup java install 17 安装"
        );
    }
}
