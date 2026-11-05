//! 错误判级与用户文案
//!
//! 本模块只做两件事，且**都只做一次**：
//!
//! 1. 从错误链里取出 [`ErrorCategory`]（等级由各错误类型自己回答，见
//!    [`mcsup::port::ErrorCategory`]），再映射成 CLI 退出码；
//! 2. 按 `kind()` 渲染面向用户的本地化文案（文案表在 [`crate::i18n`]）。
//!
//! 分级与文案分开而不是各走一遍：曾经每个子命令在这里各有一个
//! `*_exit_code`，新增一种失败要同时改两处，漏一处就出现「同一种失败，
//! 退出码说冲突、提示文案说后端挂了」。

use crate::locale::Lang;
use mcsup::port::ErrorCategory;

/// 分类 → 退出码（CLI 机器契约，见 `docs/embedding.md`）
///
/// 这是**唯一**一处把分类翻成数字的地方；`match` 故意穷尽，新增分类时
/// 编译器会强制在这里表态，不会静默落进「通用失败」。
#[must_use]
pub(crate) const fn exit_code(category: ErrorCategory) -> u8 {
    match category {
        ErrorCategory::Failure => 1,
        ErrorCategory::Usage => 2,
        ErrorCategory::NotFound => 3,
        ErrorCategory::Conflict => 4,
        ErrorCategory::Backend => 5,
        ErrorCategory::Unauthorized => 6,
        // 128 + SIGINT：只由「Ctrl-C 协作取消」来，入口据此静默退出
        ErrorCategory::Cancelled => 130,
    }
}

/// 整条错误链的退出码
///
/// `anyhow` 的链只给 `&dyn Error`，取不回具体类型，因此这里逐个试；每个
/// 分支只做一件事——把等级问出来，判定逻辑一概不复制。命中第一个带分类的
/// 层就停：外层是离调用方最近的口径。
pub(crate) fn exit_code_of(err: &anyhow::Error) -> u8 {
    for cause in err.chain() {
        if let Some(category) = category_of(cause) {
            return exit_code(category);
        }
    }
    // 链上没有可分类的错误（如裸 `anyhow!("…")`）：通用失败
    1
}

/// 单层错误的分类
///
/// 每个分支都是 `类型 → 它自己说的等级`，不含任何判定。新增带分类的错误
/// 类型时在 `probe!` 里加一个即可，不会影响其它类型。
fn category_of(cause: &(dyn std::error::Error + 'static)) -> Option<ErrorCategory> {
    use mcsup::{port, service};

    // 入口自己造的诊断也是链上一员：补过 `.context()` 后同样要能取到分类
    if let Some(diag) = cause.downcast_ref::<crate::diagnostic::AppDiagnostic>() {
        return Some(diag.category());
    }

    /// 逐个尝试，命中即返回该类型自己给出的等级
    macro_rules! probe {
        ($($ty:ty),+ $(,)?) => {
            $(
                if let Some(err) = cause.downcast_ref::<$ty>() {
                    return Some(err.category());
                }
            )+
        };
    }

    probe!(
        service::LifecycleError,
        service::InstallJarError,
        service::ServerRegistryError,
        service::WorkspaceError,
        service::JavaError,
        service::InstallError,
        service::PropsError,
        service::SettingsError,
        service::BackupError,
        service::DoctorError,
        service::ModpackError,
        service::ContentError,
        service::ResolveError,
        service::ConfigValueError,
        service::ConfigDocumentError,
        service::FsError,
        port::SourceError,
        port::DownloadError,
        port::InstallerError,
        port::InspectError,
        port::RepositoryError,
        port::SupervisorError,
        port::CasError,
        port::SnapshotStoreError,
        port::ScanError,
        port::RemoteBackupError,
        port::OriginError,
        port::modpack::ModpackExtractError,
    );

    // 传输层把 `category()` 定义在 `StorageFailure` 上（存储层要靠它拿到
    // 内层等级），不是固有方法
    if let Some(err) = cause.downcast_ref::<mcsup::transport::TransportError>() {
        return Some(port::StorageFailure::category(err));
    }
    None
}

/// 结构化错误的用户提示：只认带 `kind()` 的那几类，其余（IO、
/// 下载源等）沿用它自己的 `Display`。文案集中在 [`crate::i18n`]。
///
/// 嵌套错误沿链路下钻：`LifecycleError::Registry` 之类是 transparent 包装，
/// 没有 `kind()`，要落到内层才能取到判定。
pub(crate) fn structured_message(err: &anyhow::Error, lang: Lang) -> Option<String> {
    for cause in err.chain() {
        if let Some(life) = cause.downcast_ref::<mcsup::service::LifecycleError>()
            && let Some(message) = lifecycle_message(life, lang)
        {
            return Some(message);
        }
        if let Some(jar) = cause.downcast_ref::<mcsup::service::InstallJarError>()
            && let Some(message) = install_jar_message(jar, lang)
        {
            return Some(message);
        }
        if let Some(settings) = cause.downcast_ref::<mcsup::service::SettingsError>()
            && let Some(kind) = settings.kind()
        {
            return Some(crate::i18n::settings(&kind, lang));
        }

        // `PropsError` / `BackupError` / `SettingsError` 的 `Registry` 变体是
        // transparent 包装：
        // 既没有 `kind()`，内层 `ServerRegistryError` 也不进 `chain()`。必须
        // 当场拆开，否则 `server props` / `server set` / `backup ls` 的
        // 「服务器不存在」会掉回硬编码英文 `Display`，`--lang zh` 下成为英文。
        for reg in [
            cause
                .downcast_ref::<mcsup::service::PropsError>()
                .and_then(|props| match props {
                    mcsup::service::PropsError::Registry(reg) => Some(reg),
                    _ => None,
                }),
            cause
                .downcast_ref::<mcsup::service::BackupError>()
                .and_then(|backup| match backup {
                    mcsup::service::BackupError::Registry(reg) => Some(reg),
                    _ => None,
                }),
            cause
                .downcast_ref::<mcsup::service::SettingsError>()
                .and_then(|settings| match settings {
                    mcsup::service::SettingsError::Registry(reg) => Some(reg),
                    _ => None,
                }),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(kind) = reg.kind() {
                return Some(crate::i18n::registry(&kind, lang));
            }
        }
        if let Some(backup) = cause.downcast_ref::<mcsup::service::BackupError>()
            && let Some(kind) = backup.kind()
        {
            return Some(crate::i18n::backup(&kind, lang));
        }
        // `SourceError::NotFound`（版本不存在）只有英文 `Display`：
        // `server create --version 9.99.99` 在中文下会混出
        // `version not found: 9.99.99`。它被 `InstallError` / `InstallJarError`
        // 两层 transparent 包着，不在 `chain()` 里，必须逐层拆。
        if let Some(version) = source_not_found(cause) {
            return Some(crate::i18n::pick(
                lang,
                format!("下载源没有版本 {version}"),
                format!("the source has no version {version}"),
            ));
        }
        if let Some(message) = wrapped_message(cause, lang) {
            return Some(message);
        }
        if let Some(reg) = cause.downcast_ref::<mcsup::service::ServerRegistryError>()
            && let Some(kind) = reg.kind()
        {
            return Some(crate::i18n::registry(&kind, lang));
        }
        if let Some(ws) = cause.downcast_ref::<mcsup::service::WorkspaceError>()
            && let Some(kind) = ws.kind()
        {
            return Some(crate::i18n::workspace(&kind, lang));
        }
        if let Some(transport) = cause.downcast_ref::<mcsup::transport::TransportError>()
            && let Some(message) = crate::i18n::transport(transport, lang)
        {
            return Some(message);
        }
    }
    None
}

/// 从透明包装里掏出 `SourceError::NotFound` 的版本号
///
/// `InstallJarError::Install(InstallError::Source(..))` 是两层 transparent，
/// 内层既不在 `chain()` 里，也没有能带出版本的 `kind()`。
fn source_not_found<'a>(cause: &'a (dyn std::error::Error + 'static)) -> Option<&'a str> {
    use mcsup::port::SourceError;
    use mcsup::service::{InstallError, InstallJarError, LifecycleError};

    /// `InstallJarError` 拆到 `SourceError::NotFound` 的版本号
    fn from_jar(jar: &InstallJarError) -> Option<&str> {
        match jar {
            InstallJarError::Install(InstallError::Source(SourceError::NotFound(version))) => {
                Some(version.as_str())
            }
            _ => None,
        }
    }

    if let Some(SourceError::NotFound(version)) = cause.downcast_ref::<SourceError>() {
        return Some(version.as_str());
    }
    if let Some(InstallError::Source(SourceError::NotFound(version))) =
        cause.downcast_ref::<InstallError>()
    {
        return Some(version.as_str());
    }
    if let Some(jar) = cause.downcast_ref::<InstallJarError>() {
        return from_jar(jar);
    }
    // `create` / `install` 走 `LifecycleError::InstallJar`，比上面多一层
    if let Some(LifecycleError::InstallJar(jar)) = cause.downcast_ref::<LifecycleError>() {
        return from_jar(jar);
    }
    None
}

/// 处理被 `#[error(transparent)]` 包装的错误
///
/// 这组错误在中文界面下都会混出英文，成因同一个：包装类型转发 `source()`，
/// 但内层既没有 `kind()`，也不会出现在 `anyhow::Error::chain()` 的其它层里。
/// 逐类型下钻是唯一可靠做法——每加一个 transparent 包装就要在这里补一条。
fn wrapped_message(cause: &(dyn std::error::Error + 'static), lang: Lang) -> Option<String> {
    // 属性键 / 值非法：被 `PropsError::InvalidSet` 或 `SettingsError::InvalidValue` 包着
    let invalid = cause
        .downcast_ref::<mcsup::service::PropsError>()
        .and_then(|props| match props {
            mcsup::service::PropsError::InvalidSet(invalid) => Some(invalid),
            _ => None,
        })
        .or_else(|| cause.downcast_ref::<mcsup::domain::server::error::PropertySetError>());
    if let Some(invalid) = invalid {
        return Some(crate::i18n::property_set(invalid, lang));
    }
    // `server set` 的值非法走 `SettingsError::InvalidValue`，载荷类型不同
    if let Some(invalid) = cause
        .downcast_ref::<mcsup::service::SettingsError>()
        .and_then(|settings| match settings {
            mcsup::service::SettingsError::InvalidValue(invalid) => Some(invalid),
            _ => None,
        })
    {
        return Some(crate::i18n::setting_invalid(invalid, lang));
    }

    // 文件系统失败：`io error on {path}` 是英文外壳
    let fs = cause.downcast_ref::<mcsup::service::FsError>().or_else(|| {
        match cause.downcast_ref::<mcsup::service::ServerRegistryError>() {
            Some(mcsup::service::ServerRegistryError::Fs(fs)) => Some(fs),
            _ => None,
        }
    });
    if let Some(mcsup::service::FsError::Io { path, source }) = fs {
        // 操作系统给的原因（如 `No such file`）保留原样：它比任何中文概括都精确
        return Some(crate::i18n::pick(
            lang,
            format!("文件操作失败: {}: {source}", path.display()),
            format!("file operation failed: {}: {source}", path.display()),
        ));
    }

    // 上游拒绝：`JavaError::Source` 也是 transparent 包装
    let upstream = cause
        .downcast_ref::<mcsup::port::SourceError>()
        .or_else(|| match cause.downcast_ref::<mcsup::service::JavaError>() {
            Some(mcsup::service::JavaError::Source(source)) => Some(source),
            _ => None,
        });
    if let Some(mcsup::port::SourceError::Upstream { status, message }) = upstream {
        // 上游原文保留，排障要用
        return Some(crate::i18n::pick(
            lang,
            format!("下载源拒绝请求（{status}）：{message}"),
            format!("the source rejected the request ({status}): {message}"),
        ));
    }

    // 版本不存在已在 `structured_message` 的主循环里处理（比这里更早，
    // 且覆盖更多包装层次），此处不再重复
    None
}

/// `LifecycleError` 的透明包装不把内层错误放进 `chain()`：必须按变体下钻
fn lifecycle_message(life: &mcsup::service::LifecycleError, lang: Lang) -> Option<String> {
    use mcsup::service::LifecycleError as Life;
    match life {
        Life::Registry(reg) => reg.kind().map(|kind| crate::i18n::registry(&kind, lang)),
        Life::InstallJar(jar) => install_jar_message(jar, lang),
        other => other.kind().map(|kind| crate::i18n::lifecycle(&kind, lang)),
    }
}

/// `InstallJarError` 同样是透明包装，Registry 变体要下钻一层才能拿到分类
fn install_jar_message(jar: &mcsup::service::InstallJarError, lang: Lang) -> Option<String> {
    use mcsup::service::InstallJarError as Jar;
    match jar {
        Jar::Registry(reg) => reg.kind().map(|kind| crate::i18n::registry(&kind, lang)),
        other => other
            .kind()
            .map(|kind| crate::i18n::install_jar(&kind, lang)),
    }
}

#[cfg(test)]
mod tests {
    use super::{exit_code, exit_code_of, structured_message};
    use crate::locale::Lang;
    use anyhow::Context as _;
    use mcsup::domain::server::version::VersionId;
    use mcsup::port::{ErrorCategory, SourceError};

    /// 分类 → 退出码是 CLI 机器契约，逐条钉死（见 `docs/embedding.md`）
    #[test]
    fn categories_map_to_contract_exit_codes() {
        for (category, code) in [
            (ErrorCategory::Failure, 1),
            (ErrorCategory::Usage, 2),
            (ErrorCategory::NotFound, 3),
            (ErrorCategory::Conflict, 4),
            (ErrorCategory::Backend, 5),
            (ErrorCategory::Unauthorized, 6),
            (ErrorCategory::Cancelled, 130),
        ] {
            assert_eq!(exit_code(category), code, "{category:?}");
        }
    }

    /// 链首带分类就停：外层是离调用方最近的口径
    #[test]
    fn context_wrapper_does_not_change_the_category() {
        let err = Err::<(), _>(mcsup::service::InstallError::from(SourceError::Network(
            "boom".into(),
        )))
        .context("安装服务端失败")
        .unwrap_err();
        assert_eq!(exit_code_of(&err), 5);
    }

    /// 上游用 404 / 400 / 410 表达「你要的东西不存在」：都算资源不存在
    ///
    /// 回归：只认 400，于是 `--modpack <不存在的 id>` 的 404 落成后端失败 5，
    /// 脚本会误以为对端挂了而重试，实际是 id 写错了。
    #[test]
    fn upstream_missing_resource_statuses_map_to_exit_code_3() {
        use mcsup::port::SourceError;

        for status in [400, 404, 410] {
            for err in [
                SourceError::Status { status },
                SourceError::Upstream {
                    status,
                    message: "not found".to_owned(),
                },
            ] {
                let err = Err::<(), _>(err).context("获取整合包详情失败").unwrap_err();
                assert_eq!(exit_code_of(&err), 3, "HTTP {status}");
            }
        }

        // 5xx / 429 是可重试或需运维介入的后端问题
        for status in [429, 500, 503] {
            let err = Err::<(), _>(SourceError::Status { status })
                .context("获取整合包详情失败")
                .unwrap_err();
            assert_eq!(exit_code_of(&err), 5, "HTTP {status}");
        }

        // 下载源不带用户凭据：401 / 403 是源侧配置问题，不是调用方的身份问题
        for status in [401, 403] {
            let err = Err::<(), _>(SourceError::Status { status })
                .context("获取整合包详情失败")
                .unwrap_err();
            assert_eq!(exit_code_of(&err), 5, "HTTP {status}");
        }
    }

    #[test]
    fn not_found_maps_to_exit_code_3() {
        let err = Err::<(), _>(SourceError::NotFound(VersionId::new("1.21.1")))
            .context("安装服务端失败")
            .unwrap_err();
        assert_eq!(exit_code_of(&err), 3);
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
        assert_eq!(exit_code_of(&err), 3);
    }

    /// 版本不存在：同上，经 `InstallJarError::Install` 包装也要还原成 3
    #[test]
    fn install_jar_missing_version_maps_to_exit_code_3() {
        use mcsup::service::{InstallError, InstallJarError};
        let err = Err::<(), _>(InstallJarError::from(InstallError::from(
            SourceError::NotFound(VersionId::new("0.0.1")),
        )))
        .context("安装服务端 x 失败")
        .unwrap_err();
        assert_eq!(exit_code_of(&err), 3);
    }

    #[test]
    fn install_jar_backend_failure_maps_to_exit_code_5() {
        use mcsup::service::{InstallError, InstallJarError};
        let err = Err::<(), _>(InstallJarError::from(InstallError::from(
            SourceError::Network("boom".into()),
        )))
        .context("安装服务端 x 失败")
        .unwrap_err();
        assert_eq!(exit_code_of(&err), 5);
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
        assert_eq!(exit_code_of(&err), 4);
    }

    #[test]
    fn workspace_not_found_maps_to_exit_code_3() {
        let err = Err::<(), _>(mcsup::service::WorkspaceError::NotFound("nope".to_owned()))
            .context("切换失败")
            .unwrap_err();
        assert_eq!(exit_code_of(&err), 3);
    }

    /// 无当前工作区：缺省落点资源不存在，同「未找到」判 3
    #[test]
    fn no_current_workspace_maps_to_exit_code_3() {
        let err = Err::<(), _>(mcsup::service::WorkspaceError::NoCurrent)
            .context("创建服务器失败")
            .unwrap_err();
        assert_eq!(exit_code_of(&err), 3);
    }

    /// 终端停在待删目录里：资源被占用，退出来即可重试，判 4
    #[test]
    fn cwd_inside_target_maps_to_exit_code_4() {
        use mcsup::service::{ServerRegistryError, WorkspaceError};
        let err = Err::<(), _>(WorkspaceError::CwdInsideTarget {
            cwd: std::path::PathBuf::from("/ws/servers/alpha"),
            target: std::path::PathBuf::from("/ws"),
        })
        .context("注销工作区失败")
        .unwrap_err();
        assert_eq!(exit_code_of(&err), 4);

        // `server remove` 经注册表包装后同样还原为 4
        let err = Err::<(), _>(ServerRegistryError::Workspace(
            WorkspaceError::CwdInsideTarget {
                cwd: std::path::PathBuf::from("/ws/servers/alpha"),
                target: std::path::PathBuf::from("/ws/servers/alpha"),
            },
        ))
        .context("删除服务器失败")
        .unwrap_err();
        assert_eq!(exit_code_of(&err), 4);
    }

    /// 用法错误（非交互缺必选参数 / 取值不合法）：退出码 2
    #[test]
    fn usage_error_maps_to_exit_code_2() {
        let err = anyhow::Error::from(crate::diagnostic::AppDiagnostic::usage(
            "server create 缺少 --version",
            Some("交互终端下可省略".to_owned()),
        ));
        assert_eq!(exit_code_of(&err), 2);
    }

    #[test]
    fn java_not_found_maps_to_exit_code_3() {
        let err = Err::<(), _>(mcsup::service::JavaError::NotFound("/nope".to_owned()))
            .context("卸载失败")
            .unwrap_err();
        assert_eq!(exit_code_of(&err), 3);
    }

    #[test]
    fn java_backend_failure_maps_to_exit_code_5() {
        let err = Err::<(), _>(mcsup::service::JavaError::from(SourceError::Network(
            "boom".into(),
        )))
        .context("安装 Java 失败")
        .unwrap_err();
        assert_eq!(exit_code_of(&err), 5);
    }

    // ---- 生命周期判级 ----

    /// 已在运行 / 未在运行：状态冲突 → 4
    #[test]
    fn lifecycle_state_conflict_maps_to_exit_code_4() {
        use mcsup::service::LifecycleError;
        for err in [LifecycleError::AlreadyRunning, LifecycleError::NotRunning] {
            let err = Err::<(), _>(err).context("启动失败").unwrap_err();
            assert_eq!(exit_code_of(&err), 4);
        }
    }

    /// EULA 未同意（非交互）：用法类 → 2
    #[test]
    fn lifecycle_eula_required_maps_to_exit_code_2() {
        let err = Err::<(), _>(mcsup::service::LifecycleError::EulaRequired)
            .context("启动失败")
            .unwrap_err();
        assert_eq!(exit_code_of(&err), 2);
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
            assert_eq!(exit_code_of(&err), 3);
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
        assert_eq!(exit_code_of(&err), 5);
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
        assert_eq!(exit_code_of(&err), 3);
    }

    /// 停机超时说明服务端仍在运行：状态冲突 → 4（原来落 1 是判级缺失）
    #[test]
    fn lifecycle_stop_timeout_maps_to_exit_code_4() {
        let err = Err::<(), _>(mcsup::service::LifecycleError::StopTimedOut {
            waited: std::time::Duration::from_secs(30),
        })
        .context("停止失败")
        .unwrap_err();
        assert_eq!(exit_code_of(&err), 4);
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
        assert_eq!(exit_code_of(&err), 4);
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
        assert_eq!(exit_code_of(&err), 4);
    }

    /// 用户取消（Ctrl-C 协作取消）：130 = 128 + SIGINT，与后端失败 5 区分
    #[test]
    fn java_cancelled_maps_to_exit_code_130() {
        let err = Err::<(), _>(mcsup::service::JavaError::from(
            mcsup::port::DownloadError::Cancelled,
        ))
        .context("安装 Java 失败")
        .unwrap_err();
        assert_eq!(exit_code_of(&err), 130);
    }

    /// 裸下载取消（不经 `JavaError` 包装，如服务端核心下载）同样判 130
    #[test]
    fn download_cancelled_maps_to_exit_code_130() {
        let err = Err::<(), _>(mcsup::port::DownloadError::Cancelled)
            .context("下载服务端失败")
            .unwrap_err();
        assert_eq!(exit_code_of(&err), 130);
    }

    /// 安装器被 Ctrl-C 终止：取消走 130，不能报成「安装器失败」
    #[test]
    fn installer_cancelled_maps_to_exit_code_130() {
        use mcsup::port::InstallerError;
        let err = Err::<(), _>(mcsup::service::InstallError::from(
            InstallerError::Cancelled,
        ))
        .context("运行 Forge 安装器失败")
        .unwrap_err();
        assert_eq!(exit_code_of(&err), 130);
    }

    /// 安装器非零退出：后端失败 5（原来是通用 1）
    #[test]
    fn installer_failure_maps_to_exit_code_5() {
        use mcsup::port::InstallerError;
        let err = Err::<(), _>(InstallerError::Failed {
            code: 1,
            tail: "boom".to_owned(),
        })
        .context("运行 Forge 安装器失败")
        .unwrap_err();
        assert_eq!(exit_code_of(&err), 5);
    }

    #[test]
    fn transport_unauthorized_maps_to_exit_code_6() {
        use mcsup::port::StorageFailure;
        use mcsup::transport::TransportError;

        let code = |err: &TransportError| exit_code(StorageFailure::category(err));
        assert_eq!(code(&TransportError::Unauthorized("revoked".to_owned())), 6);
        assert_eq!(
            code(&TransportError::FingerprintMismatch {
                expected: "a".to_owned(),
                actual: "b".to_owned(),
            }),
            6
        );
        assert_eq!(
            code(&TransportError::Remote {
                kind: mcsup::protocol::remote::ErrorKind::UNAUTHORIZED.to_owned(),
                message: "nope".to_owned(),
            }),
            6
        );
        // 对端说「没找到」按未找到判 3，不是未授权：脚本不该因此重新配对，
        // 也不该把整台后端判成不可用
        assert_eq!(
            code(&TransportError::Remote {
                kind: mcsup::protocol::remote::ErrorKind::NOT_FOUND.to_owned(),
                message: "gone".to_owned(),
            }),
            3
        );
        // 未登记的对端种类按后端失败兜底
        assert_eq!(
            code(&TransportError::Remote {
                kind: "something_new".to_owned(),
                message: "?".to_owned(),
            }),
            5
        );
    }

    /// 远端仓库把对端失败包在 `Storage` 里：等级靠内层自己回答
    ///
    /// 回归：只认 `NotFound` / `AlreadyExists`，**未授权**一律落 1，于是
    /// `server status <远端>/x` 在 token 被撤销时返回 5，而 `remote ping`
    /// 返回 6——同一种失败两个退出码，脚本无从判断要不要重新配对。
    #[test]
    fn repository_storage_delegates_to_inner_category() {
        use mcsup::port::RepositoryError;
        use mcsup::transport::TransportError;

        let unauthorized = RepositoryError::Storage(Box::new(TransportError::Remote {
            kind: mcsup::protocol::remote::ErrorKind::UNAUTHORIZED.to_owned(),
            message: "token is unknown or revoked".to_owned(),
        }));
        assert_eq!(exit_code(unauthorized.category()), 6);

        // 与本判级无关的存储失败仍是后端失败
        let io = RepositoryError::Storage(Box::new(std::io::Error::other("disk")));
        assert_eq!(exit_code(io.category()), 5);
    }

    /// 远端 supervisor 的业务拒绝同样按 kind 判级
    #[test]
    fn supervisor_remote_kind_maps_to_exit_code() {
        use mcsup::port::{RemoteFailureKind, SupervisorError};

        let make = |kind| SupervisorError::Remote {
            kind,
            message: "x".to_owned(),
        };
        assert_eq!(
            exit_code(make(RemoteFailureKind::Unauthorized).category()),
            6
        );
        assert_eq!(exit_code(make(RemoteFailureKind::NotFound).category()), 3);
        assert_eq!(exit_code(make(RemoteFailureKind::Busy).category()), 4);
        assert_eq!(exit_code(make(RemoteFailureKind::Io).category()), 5);
    }

    /// 诊断错误补过 `.context()` 后仍能取到分类
    #[test]
    fn context_wrapped_diagnostic_keeps_its_category() {
        let err = Err::<(), _>(crate::diagnostic::AppDiagnostic::not_found(
            "找不到",
            Some("检查名字".to_owned()),
        ))
        .context("读取服务器失败")
        .unwrap_err();
        assert_eq!(exit_code_of(&err), 3);
    }

    #[test]
    fn unauthorized_diagnostic_maps_to_exit_code_6() {
        let err = anyhow::Error::from(crate::diagnostic::AppDiagnostic::unauthorized(
            "token revoked",
            None,
        ));
        assert_eq!(exit_code_of(&err), 6);
    }

    #[test]
    fn cancelled_diagnostic_maps_to_exit_code_130() {
        let err = anyhow::Error::from(crate::diagnostic::AppDiagnostic::cancelled());
        assert_eq!(exit_code_of(&err), 130);
    }

    // ---- 文案 ----

    /// 包装变体没有 kind()：必须继续沿链下钻到内层 registry 错误，
    /// 不能 `?` 直接放弃整条链
    #[test]
    fn structured_message_drills_through_wrapper_without_kind() {
        use mcsup::service::{LifecycleError, ServerRegistryError};
        let err = anyhow::Error::from(LifecycleError::from(ServerRegistryError::NotFound(
            "alpha".to_owned(),
        )));
        assert_eq!(
            structured_message(&err, Lang::Zh).as_deref(),
            Some("服务器 alpha 不存在")
        );
    }

    /// `PropsError` / `BackupError` / `SettingsError` 的 `Registry` 变体
    /// 是 transparent 包装：内层 `ServerRegistryError` 既没有 `kind()` 也不会
    /// 出现在 `chain()` 里。回归：`server props nosuch --lang zh` 输出英文
    /// `server not found: nosuch`，而同目录的 `server status` 是中文。
    #[test]
    fn structured_message_drills_into_registry_wrapped_by_transparent_errors() {
        use mcsup::service::{BackupError, PropsError, ServerRegistryError, SettingsError};
        let missing = || ServerRegistryError::NotFound("alpha".to_owned());

        let cases: [(&str, anyhow::Error); 3] = [
            ("props", PropsError::from(missing()).into()),
            ("backup", BackupError::from(missing()).into()),
            ("settings", SettingsError::from(missing()).into()),
        ];
        for (label, err) in cases {
            assert_eq!(
                structured_message(&err, Lang::Zh).as_deref(),
                Some("服务器 alpha 不存在"),
                "{label} 中文提示应统一"
            );
            assert_eq!(
                structured_message(&err, Lang::En).as_deref(),
                Some("server alpha not found"),
                "{label} 英文提示应统一"
            );
        }
    }

    /// 下载源找不到版本：中文界面不得混出 `version not found`
    ///
    /// 回归：`server create --version 9.99.99 --lang zh` 输出
    /// `安装服务端 x 失败: version not found: 9.99.99`。错误被
    /// `LifecycleError::InstallJar(InstallJarError::Install(InstallError::Source))`
    /// 三层 transparent 包着，`chain()` 里只剩最外层，必须逐层拆。
    #[test]
    fn source_not_found_is_localized_through_three_wrappers() {
        use mcsup::port::SourceError;
        use mcsup::service::{InstallError, InstallJarError, LifecycleError};

        let src = SourceError::NotFound(VersionId::new("9.99.99"));
        let jar = InstallJarError::from(InstallError::from(src));

        // 直接给出 InstallJarError（`server install` 路径）
        let direct = anyhow::Error::from(jar).context("安装服务端 x 失败");
        assert_eq!(
            structured_message(&direct, Lang::Zh).as_deref(),
            Some("下载源没有版本 9.99.99")
        );

        // 经 LifecycleError 再包一层（`server create` 路径）
        let src = SourceError::NotFound(VersionId::new("1.2.3"));
        let jar = InstallJarError::from(InstallError::from(src));
        let wrapped = anyhow::Error::from(LifecycleError::from(jar)).context("安装服务端 x 失败");
        assert_eq!(
            structured_message(&wrapped, Lang::Zh).as_deref(),
            Some("下载源没有版本 1.2.3")
        );
        assert_eq!(
            structured_message(&wrapped, Lang::En).as_deref(),
            Some("the source has no version 1.2.3")
        );
    }

    /// transparent 包装的错误在中文界面下不得只剩英文
    ///
    /// 回归三例：`server props x 'bad key=v'` 混出 `invalid property key`；
    /// `java install 99` 混出 `upstream rejected the request`；
    /// `server link /tmp/nonexist` 混出 `io error on`。成因同一个——内层既无
    /// `kind()` 也不在 `chain()` 里。
    #[test]
    fn transparent_wrappers_are_localized() {
        use mcsup::domain::server::error::{InvalidPropertyKey, PropertySetError};
        use mcsup::service::{FsError, JavaError, PropsError, ServerRegistryError, SettingsError};

        // 属性键非法
        let props = anyhow::Error::from(PropsError::from(PropertySetError::from(
            InvalidPropertyKey {
                raw: "bad key".to_owned(),
            },
        )))
        .context("写入 x 的 server.properties 失败");
        let zh = structured_message(&props, Lang::Zh).expect("应有中文");
        assert!(zh.contains("属性键非法"), "{zh}");
        assert!(!zh.contains("invalid property key"), "{zh}");

        // 上游拒绝：原文保留
        let upstream = anyhow::Error::from(JavaError::from(SourceError::Upstream {
            status: 400,
            message: "not released yet".to_owned(),
        }))
        .context("安装 Java 失败");
        let zh = structured_message(&upstream, Lang::Zh).expect("应有中文");
        assert!(zh.contains("下载源拒绝请求"), "{zh}");
        assert!(zh.contains("not released yet"), "上游原文必须保留: {zh}");

        // 文件系统失败：操作系统原因保留
        let fs = anyhow::Error::from(ServerRegistryError::from(FsError::Io {
            path: std::path::PathBuf::from("/tmp/nonexist"),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        }))
        .context("注册服务器目录失败");
        let zh = structured_message(&fs, Lang::Zh).expect("应有中文");
        assert!(zh.contains("文件操作失败"), "{zh}");
        assert!(!zh.contains("io error on"), "{zh}");

        // `server set` 的值非法
        let invalid = mcsup::domain::server::settings::ServerSettings::default()
            .apply(mcsup::domain::server::settings::SettingKey::Ram, "abc")
            .expect_err("abc 不是合法 ram");
        let settings = anyhow::Error::from(SettingsError::from(invalid));
        let zh = structured_message(&settings, Lang::Zh).expect("应有中文");
        assert!(zh.contains("的值非法"), "{zh}");
        assert!(!zh.contains("invalid value from"), "{zh}");
    }
}
