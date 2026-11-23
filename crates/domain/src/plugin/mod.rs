//! 平台插件领域模型：标识、能力、状态、manifest、descriptor 与契约
//!
//! 本模块只放**判定**：能力三方交集、状态迁移、manifest / descriptor 校验、
//! 契约闭集与事件目录。进程、端点、帧编解码都在实现层，入口只映射错误。
//!
//! 设计来源见 `docs/plugins.md`：这里的每个常量、枚举与限值都必须与该文档
//! 及 `docs/plugins-testing.md` 的用例 ID 对齐；改数值必须三处同步。
//!
//! # 版本
//!
//! 四个版本各自独立递增（[`PLUGIN_API_VERSION`]、manifest schema、descriptor
//! schema、协议版本）。`[禁止]` 从一个版本推导另一个：它们变更的理由不同，
//! 推导会把「新增一个 op 字段」变成「所有插件都得重装」。

pub mod capability;
pub mod contract;
pub mod descriptor;
pub mod error;
pub mod event;
pub mod manifest;
pub mod revocation;
pub mod schema;
pub mod source;
pub mod state;

use std::fmt;

/// 插件 API 主版本：SDK 主版本与它相等
pub const PLUGIN_API_VERSION: u32 = 1;

/// manifest schema 版本
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// descriptor schema 版本
///
/// 2 起支持 `setup` 安装向导，3 起支持 `bind` 服务器绑定，4 起支持 setup
/// 字段条件（`setup.fields[].when`）；校验同时接受更旧的 1 / 2 / 3（旧产物）。
pub const DESCRIPTOR_SCHEMA_VERSION: u32 = 4;

/// 当前平台标识：`darwin-arm64` / `linux-x64` / `win32-x64`
///
/// manifest 的 `entry.platforms` 键与 SDK 构建产物的平台目录共用这一套命名。
/// 安装（`provider`）与运行（`plugin`）都要选平台产物，放在领域层才能保证
/// 两边不会在新增平台时分叉。
#[must_use]
pub fn platform_key() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        other => other,
    };
    format!("{os}-{arch}")
}

/// 插件进程退出码（`plugins.md` §6.7）
///
/// SDK 与宿主共用这一份定义：Rust SDK 直接引用，TypeScript / Python 由
/// `protocol::plugin::contracts` 生成，避免各自手写。
pub mod exit_code {
    /// 正常退出
    pub const OK: i32 = 0;
    /// 未分类失败
    pub const FAILURE: i32 = 1;
    /// 协议或握手错误
    pub const PROTOCOL: i32 = 2;
    /// 配置无效
    pub const CONFIG: i32 = 3;
    /// API 版本不支持
    pub const API_MISMATCH: i32 = 4;

    /// 全部退出码与线上名（生成器按此顺序输出）
    pub const ALL: [(&str, i32); 5] = [
        ("ok", OK),
        ("failure", FAILURE),
        ("protocol", PROTOCOL),
        ("config", CONFIG),
        ("api_mismatch", API_MISMATCH),
    ];
}

/// 插件 id 最大字节数（含）
pub const MAX_PLUGIN_ID_LEN: usize = 64;

/// 插件 id 最小字节数（含）
pub const MIN_PLUGIN_ID_LEN: usize = 2;

/// 插件 id：全局唯一的小写标识，同时用作目录名与端点哈希输入
///
/// 规则：`^[a-z][a-z0-9-]{1,63}$`。首字符必须是字母，避免生成以数字或 `-`
/// 开头的目录名；不允许 `_` / `.` / 大写，避免大小写不敏感文件系统与不同
/// 平台之间出现「同一 id 两个目录」。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PluginId(String);

impl PluginId {
    /// 保留名：与平台自身模块同名会让日志、CLI 与目录归属产生歧义
    pub const RESERVED: &'static [&'static str] = &["mcsup", "core", "mod"];

    /// 校验并构造
    ///
    /// # Errors
    ///
    /// 长度越界、字符集不符或以保留名出现时返回 [`InvalidPluginId`]
    pub fn parse(raw: &str) -> Result<Self, InvalidPluginId> {
        let valid = raw.len() >= MIN_PLUGIN_ID_LEN
            && raw.len() <= MAX_PLUGIN_ID_LEN
            && raw.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
            && raw
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !valid {
            return Err(InvalidPluginId {
                raw: raw.to_owned(),
                reason: IdReject::Format,
            });
        }
        if Self::RESERVED.contains(&raw) {
            return Err(InvalidPluginId {
                raw: raw.to_owned(),
                reason: IdReject::Reserved,
            });
        }
        Ok(Self(raw.to_owned()))
    }

    /// 文本形式
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 是否为保留名（不做格式校验）
    #[must_use]
    pub fn is_reserved(raw: &str) -> bool {
        Self::RESERVED.contains(&raw)
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 插件作用域：决定事件可见范围与是否带 `--server`
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum PluginScope {
    /// 机器级：可见所有服务器的事件，端点按插件 id 区分
    #[default]
    Machine,
    /// 服务器级：只可见单服事件，端点还要带上服务器
    Server,
}

impl PluginScope {
    /// 稳定小写文本（manifest / 输出 / 端点哈希口径）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Machine => "machine",
            Self::Server => "server",
        }
    }

    /// 从 manifest 字段解析；缺省为 `machine`
    ///
    /// # Errors
    ///
    /// 未知取值返回 [`InvalidPluginId`] 之外的 [`UnknownScope`]
    pub fn parse(raw: &str) -> Result<Self, UnknownScope> {
        match raw {
            "machine" => Ok(Self::Machine),
            "server" => Ok(Self::Server),
            other => Err(UnknownScope {
                raw: other.to_owned(),
            }),
        }
    }
}

/// 插件 id 非法
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid plugin id {raw:?}: {reason}")]
pub struct InvalidPluginId {
    /// 原始输入
    pub raw: String,
    /// 拒绝原因
    pub reason: IdReject,
}

/// 插件 id 被拒的具体原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdReject {
    /// 长度或字符集不符
    Format,
    /// 与平台保留名冲突
    Reserved,
}

impl fmt::Display for IdReject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format => {
                f.write_str("must match ^[a-z][a-z0-9-]{1,63}$ (lowercase, digits and hyphen only)")
            }
            Self::Reserved => f.write_str("reserved by the platform"),
        }
    }
}

/// 无法识别的作用域
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown plugin scope: {raw}")]
pub struct UnknownScope {
    /// 原始输入
    pub raw: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_id_accepts_lowercase_kebab() {
        for raw in ["ab", "chat-bridge", "a1", "x-9"] {
            let id = PluginId::parse(raw).expect(raw);
            assert_eq!(id.as_str(), raw);
        }
    }

    #[test]
    fn plugin_id_rejects_format_violations() {
        for raw in [
            "",
            "a",
            "Chat",
            "chat_bridge",
            "1chat",
            "-chat",
            "chat.bridge",
        ] {
            let err = PluginId::parse(raw).expect_err(raw);
            assert_eq!(err.reason, IdReject::Format, "{raw}");
        }
        let too_long = "a".repeat(MAX_PLUGIN_ID_LEN + 1);
        assert_eq!(
            PluginId::parse(&too_long).expect_err("long").reason,
            IdReject::Format
        );
    }

    #[test]
    fn plugin_id_rejects_reserved_names() {
        for raw in PluginId::RESERVED {
            let err = PluginId::parse(raw).expect_err(raw);
            assert_eq!(err.reason, IdReject::Reserved);
        }
    }

    #[test]
    fn scope_roundtrips_wire_text() {
        for scope in [PluginScope::Machine, PluginScope::Server] {
            assert_eq!(PluginScope::parse(scope.as_str()), Ok(scope));
        }
        assert!(PluginScope::parse("cluster").is_err());
        assert_eq!(PluginScope::default(), PluginScope::Machine);
    }
}
