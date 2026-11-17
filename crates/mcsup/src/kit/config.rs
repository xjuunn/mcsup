//! 任务级默认策略：`KitConfig`
//!
//! 配置分三层，优先级从高到低：
//!
//! ```text
//! 单次调用显式设置（CreateServer::version 等）
//!   ↓ 没设置
//! KitConfig（本类型；实例级默认）
//!   ↓ 没有默认
//! 内置默认（本文件的 Default 实现）
//! ```
//!
//! 单次调用只写「这次想改的」，其余字段为 `None`，由本类型补齐——这与
//! `..Default::default()` 是同一种语义，区别只是补齐发生在运行时而不是结构体
//! 字面量里，因此同一份默认策略可以被多次调用复用。

use domain::java::{JavaMajor, JavaPackageType, JavaVendor};
use domain::server::content::ContentKind;
use domain::server::variant::ServerVariant;
use service::DEFAULT_FLUSH_WAIT_MS;
use std::ffi::OsString;
use std::path::PathBuf;

/// 任务级默认策略
///
/// 只放**跨任务复用**的默认值；「这次要装哪个版本 / 哪个项目」这类一次性参数
/// 属于各任务的 options，不在这里。
///
/// 构造方式有两种，语义相同：
///
/// ```no_run
/// use mcsup::kit::KitConfig;
///
/// // 全默认
/// let all = KitConfig::default();
/// // 只改个别项，其余仍是默认
/// let some = KitConfig { ram_mib: Some(8192), accept_eula: true, ..all };
/// ```
///
/// `accept_eula` 默认 `false`：代用户接受 Mojang EULA 是调用方的决定，库不
/// 替它做。需要「建完直接起服」时显式打开，否则起服步骤会报
/// [`super::KitError::EulaNotAccepted`]。
// 四个 bool 是彼此独立的策略开关（同意 EULA / 跳过预检 / 强制全量 / 自动起服），
// 不构成状态机；拆成枚举只会让「默认关」变得难写。
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug)]
pub struct KitConfig {
    /// 默认工作目录选择器（名称或路径）；`None` = 当前工作目录
    ///
    /// 只用于定位**已存在**的工作目录。要让 kit 顺手建一个，用
    /// [`super::CreateServer::workspace_dir`]。
    pub workspace: Option<String>,
    /// 默认服务端变体
    pub variant: ServerVariant,
    /// 默认版本；`latest` 是来源保留别名，表示其最新稳定版
    pub default_version: String,
    /// 起服时是否代为同意 EULA
    pub accept_eula: bool,
    /// 起服时是否跳过 mod 预检
    pub skip_mod_check: bool,
    /// 默认堆上限（MiB）；`None` = 服务属性优先，其次平台默认
    pub ram_mib: Option<u32>,
    /// 备份等待 `save-all` 落盘的时长（毫秒）
    pub flush_wait_ms: u64,
    /// 备份默认是否跳过父快照复用提示（强制全量重读）
    pub no_reuse: bool,
    /// 默认内容类别（`mod` / `plugin` 等）
    pub content_kind: ContentKind,
    /// 默认安装的 Java 大版本；`None` = 必须由单次调用指定
    ///
    /// 不给默认值是刻意的：装哪个大版本取决于要跑的服务端，猜错会白下一个
    /// 几百 MB 的运行时。
    pub java_major: Option<JavaMajor>,
    /// 默认 Java 包类型（完整 JDK 或精简 JRE）
    pub java_package_type: JavaPackageType,
    /// 默认 Java 发行商；`None` = 来源按自身优先级挑选
    pub java_vendor: Option<JavaVendor>,
    /// 拉起 supervisor 的可执行文件
    ///
    /// 只有默认 supervisor（`LocalSupervisor`）需要它：它靠拉起
    /// `mcsup __supervise` 子进程工作。宿主经 `McsupBuilder::supervisor`
    /// （或 `backend`）注入自己的进程管理时，本字段不会被读取。
    pub supervisor_exe: Option<PathBuf>,
    /// supervisor 可执行文件的前置子命令（如 `["__supervise"]`）
    pub supervisor_args: Vec<OsString>,
    /// 建服后是否自动起服
    pub start_after_create: bool,
}

impl Default for KitConfig {
    fn default() -> Self {
        Self {
            workspace: None,
            variant: ServerVariant::Vanilla,
            default_version: "latest".to_owned(),
            // EULA 必须由调用方显式接受
            accept_eula: false,
            skip_mod_check: false,
            ram_mib: None,
            flush_wait_ms: DEFAULT_FLUSH_WAIT_MS,
            no_reuse: false,
            content_kind: ContentKind::Mod,
            java_major: None,
            java_package_type: JavaPackageType::Jdk,
            java_vendor: None,
            supervisor_exe: None,
            supervisor_args: Vec::new(),
            start_after_create: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认值必须安全：不接受 EULA、不起服、不跳过预检
    #[test]
    fn defaults_are_conservative() {
        let config = KitConfig::default();
        assert!(!config.accept_eula, "默认不得代用户接受 EULA");
        assert!(!config.start_after_create, "默认不得自动起服");
        assert!(!config.skip_mod_check, "默认不得跳过 mod 预检");
        assert_eq!(config.default_version, "latest");
        assert_eq!(config.variant, ServerVariant::Vanilla);
        assert_eq!(config.flush_wait_ms, DEFAULT_FLUSH_WAIT_MS);
        assert!(config.java_major.is_none(), "装 Java 必须显式给大版本");
        assert_eq!(config.java_package_type, JavaPackageType::Jdk);
    }

    /// `..Default::default()` 只改指定字段，其余保持默认
    #[test]
    fn struct_update_keeps_other_defaults() {
        let config = KitConfig {
            ram_mib: Some(8192),
            ..KitConfig::default()
        };
        assert_eq!(config.ram_mib, Some(8192));
        assert_eq!(config.content_kind, ContentKind::Mod);
        assert!(!config.accept_eula);
    }
}
