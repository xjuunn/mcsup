//! 能力目录与授予规则
//!
//! 能力是宿主代做动作的许可，不是沙箱。判定的唯一入口是
//! [`granted`]：`manifest ∩ requested ∩ approved`，三者缺一即不授予。
//! 入口（CLI / Web / MCP）不得自行判断「这个能力够不够」，只能展示结果。

use std::collections::BTreeSet;
use std::fmt;

/// 能力风险：决定安装时的确认强度
///
/// 与 op 的 [`crate::plugin::descriptor::OpRisk`] 是两个维度：能力回答
/// 「插件能请求宿主做什么」，op 风险回答「用户点这一下会发生什么」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CapabilityRisk {
    /// 低：安装即得
    Low,
    /// 中：安装时一行确认
    Medium,
    /// 高：需要显式 `grant`
    High,
    /// 极高：v1 不开放
    Critical,
}

impl CapabilityRisk {
    /// 稳定文本
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    /// 安装时是否必须由用户逐项确认
    #[must_use]
    pub const fn needs_approval(self) -> bool {
        matches!(self, Self::Medium | Self::High | Self::Critical)
    }
}

/// 默认授予策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefaultPolicy {
    /// 安装即授予
    Allow,
    /// 安装时询问一次
    Ask,
    /// 默认拒绝，必须显式 `grant`
    Deny,
    /// v1 不开放，声明即拒绝安装
    Closed,
}

impl DefaultPolicy {
    /// 稳定文本
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
            Self::Closed => "closed",
        }
    }
}

/// 能力开放的里程碑
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityStage {
    /// M1：事件、键值、只读查询
    M1,
    /// M2：控制台写、出站、密钥
    M2,
    /// M3：备份、文件、生命周期、内容
    M3,
    /// M4：Java
    M4,
    /// 不开放
    Closed,
}

impl CapabilityStage {
    /// 稳定文本
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::M1 => "m1",
            Self::M2 => "m2",
            Self::M3 => "m3",
            Self::M4 => "m4",
            Self::Closed => "closed",
        }
    }

    /// 当前版本是否可授予
    #[must_use]
    pub const fn is_open(self) -> bool {
        !matches!(self, Self::Closed)
    }
}

/// 能力标识：v1 的闭集，共 15 项
///
/// 未知能力不能在运行时被忽略：旧宿主装上「永远不可用」的插件比拒绝安装更
/// 糟，因此解析失败必须让安装失败（见 [`Capability::parse`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Capability {
    /// 订阅领域事件
    Events,
    /// 自己的键值空间
    StorageKv,
    /// 列服务器、状态、版本、属性
    ServerRead,
    /// 控制台尾部与聊天事件
    ConsoleRead,
    /// 注入命令（等价 OP）
    ConsoleWrite,
    /// 出站 HTTP / WS
    NetOutbound,
    /// 自己的凭据
    SecretRw,
    /// 备份创建 / 恢复 / 清理 / 校验
    Backup,
    /// 服务端目录内读
    FilesRead,
    /// 服务端目录内写
    FilesWrite,
    /// 启停重启
    ServerLifecycle,
    /// 内容搜索 / 安装 / 更新 / 移除
    Content,
    /// 托管 JDK 安装 / 卸载
    Java,
    /// 拉起子进程（v1 不开放）
    ProcessSpawn,
    /// 以玩家身份连服（v1 不开放）
    McClient,
}

impl Capability {
    /// 全部 15 项，顺序与能力目录一致
    pub const ALL: [Self; 15] = [
        Self::Events,
        Self::StorageKv,
        Self::ServerRead,
        Self::ConsoleRead,
        Self::ConsoleWrite,
        Self::NetOutbound,
        Self::SecretRw,
        Self::Backup,
        Self::FilesRead,
        Self::FilesWrite,
        Self::ServerLifecycle,
        Self::Content,
        Self::Java,
        Self::ProcessSpawn,
        Self::McClient,
    ];

    /// 稳定文本（manifest / 审计 / CLI 输出口径）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Events => "events",
            Self::StorageKv => "storage:kv",
            Self::ServerRead => "server:read",
            Self::ConsoleRead => "console:read",
            Self::ConsoleWrite => "console:write",
            Self::NetOutbound => "net:outbound",
            Self::SecretRw => "secret:rw",
            Self::Backup => "backup",
            Self::FilesRead => "files:read",
            Self::FilesWrite => "files:write",
            Self::ServerLifecycle => "server:lifecycle",
            Self::Content => "content",
            Self::Java => "java",
            Self::ProcessSpawn => "process:spawn",
            Self::McClient => "mc:client",
        }
    }

    /// 解析能力名
    ///
    /// # Errors
    ///
    /// 未知名字返回 [`UnknownCapability`]；调用方必须拒绝安装，不得忽略
    pub fn parse(raw: &str) -> Result<Self, UnknownCapability> {
        Self::ALL
            .into_iter()
            .find(|cap| cap.as_str() == raw)
            .ok_or_else(|| UnknownCapability {
                raw: raw.to_owned(),
            })
    }

    /// 风险等级
    #[must_use]
    pub const fn risk(self) -> CapabilityRisk {
        match self {
            Self::Events | Self::StorageKv | Self::ServerRead => CapabilityRisk::Low,
            Self::ConsoleRead | Self::NetOutbound | Self::SecretRw | Self::FilesRead => {
                CapabilityRisk::Medium
            }
            Self::ConsoleWrite
            | Self::Backup
            | Self::FilesWrite
            | Self::ServerLifecycle
            | Self::Content
            | Self::Java => CapabilityRisk::High,
            Self::ProcessSpawn | Self::McClient => CapabilityRisk::Critical,
        }
    }

    /// 安装缺省策略
    #[must_use]
    pub const fn default_policy(self) -> DefaultPolicy {
        match self.risk() {
            CapabilityRisk::Low => DefaultPolicy::Allow,
            CapabilityRisk::Medium => DefaultPolicy::Ask,
            CapabilityRisk::High => DefaultPolicy::Deny,
            CapabilityRisk::Critical => DefaultPolicy::Closed,
        }
    }

    /// 开放的里程碑
    #[must_use]
    pub const fn stage(self) -> CapabilityStage {
        match self {
            Self::Events | Self::StorageKv | Self::ServerRead | Self::ConsoleRead => {
                CapabilityStage::M1
            }
            Self::ConsoleWrite | Self::NetOutbound | Self::SecretRw => CapabilityStage::M2,
            Self::Backup
            | Self::FilesRead
            | Self::FilesWrite
            | Self::ServerLifecycle
            | Self::Content => CapabilityStage::M3,
            Self::Java => CapabilityStage::M4,
            Self::ProcessSpawn | Self::McClient => CapabilityStage::Closed,
        }
    }

    /// v1 是否可授予
    #[must_use]
    pub const fn is_open(self) -> bool {
        self.stage().is_open()
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl serde::Serialize for Capability {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for Capability {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// 未知能力名
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown capability: {raw}")]
pub struct UnknownCapability {
    /// 原始输入
    pub raw: String,
}

/// 解析并去重能力列表，保持稳定排序
///
/// # Errors
///
/// 任一项未知时返回该项的 [`UnknownCapability`]；不做「跳过未知项」的兜底
pub fn parse_all(raw: &[String]) -> Result<BTreeSet<Capability>, UnknownCapability> {
    raw.iter()
        .map(|item| Capability::parse(item))
        .collect::<Result<BTreeSet<_>, _>>()
}

/// 计算授予集合：`manifest ∩ requested ∩ approved`
///
/// 三个集合都是 [`BTreeSet`]，结果天然去重且顺序稳定。任一为空即空集。
#[must_use]
pub fn granted(
    manifest: &BTreeSet<Capability>,
    requested: &BTreeSet<Capability>,
    approved: &BTreeSet<Capability>,
) -> BTreeSet<Capability> {
    manifest
        .intersection(requested)
        .filter(|cap| approved.contains(cap))
        .copied()
        .collect()
}

/// 授权判定的错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    /// 能力不在 manifest 声明内
    NotDeclared(Capability),
    /// 能力在当前版本不可用（closed stage）
    NotOpen(Capability),
    /// 能力尚未授予，无法撤销
    NotGranted(Capability),
}

impl fmt::Display for GrantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotDeclared(cap) => write!(f, "capability {cap} is not declared"),
            Self::NotOpen(cap) => {
                write!(f, "capability {cap} is not available in this version")
            }
            Self::NotGranted(cap) => write!(f, "capability {cap} is not granted"),
        }
    }
}

/// 授权前判定：能力必须已声明且当前版本可用
///
/// 不改任何状态；是否修改授予集合与状态迁移由调用方按返回值处理。
///
/// # Errors
///
/// 未声明返回 [`GrantError::NotDeclared`]，closed stage 返回
/// [`GrantError::NotOpen`]
pub fn check_grantable(
    declared: &BTreeSet<Capability>,
    capability: Capability,
) -> Result<(), GrantError> {
    if !declared.contains(&capability) {
        return Err(GrantError::NotDeclared(capability));
    }
    if !capability.is_open() {
        return Err(GrantError::NotOpen(capability));
    }
    Ok(())
}

/// 未开放能力的声明检查
///
/// 返回声明了但 v1 不开放的能力，供安装直接拒绝。
#[must_use]
pub fn closed_declared(declared: &BTreeSet<Capability>) -> Vec<Capability> {
    declared
        .iter()
        .copied()
        .filter(|cap| !cap.is_open())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[Capability]) -> BTreeSet<Capability> {
        items.iter().copied().collect()
    }

    #[test]
    fn catalog_has_fifteen_unique_names() {
        assert_eq!(Capability::ALL.len(), 15);
        let names: BTreeSet<&str> = Capability::ALL.iter().map(|cap| cap.as_str()).collect();
        assert_eq!(names.len(), 15);
        for cap in Capability::ALL {
            assert_eq!(Capability::parse(cap.as_str()), Ok(cap));
        }
    }

    #[test]
    fn unknown_capability_is_rejected() {
        for raw in ["sandbox", "console", "Console:read", ""] {
            assert!(Capability::parse(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn granted_is_three_way_intersection() {
        let manifest = set(&[
            Capability::Events,
            Capability::ConsoleRead,
            Capability::ConsoleWrite,
        ]);
        let requested = set(&[Capability::ConsoleRead, Capability::ConsoleWrite]);
        let approved = set(&[Capability::Events, Capability::ConsoleRead]);
        assert_eq!(
            granted(&manifest, &requested, &approved),
            set(&[Capability::ConsoleRead])
        );
        assert!(granted(&manifest, &requested, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn granted_is_subset_of_all_three() {
        let manifest = set(&[Capability::Events, Capability::Backup]);
        let requested = set(&[Capability::Events, Capability::Backup]);
        let approved = set(&[Capability::Backup]);
        let result = granted(&manifest, &requested, &approved);
        assert!(result.iter().all(|cap| manifest.contains(cap)));
        assert!(result.iter().all(|cap| requested.contains(cap)));
        assert!(result.iter().all(|cap| approved.contains(cap)));
    }

    #[test]
    fn closed_capabilities_are_reported() {
        let declared = set(&[Capability::Events, Capability::ProcessSpawn]);
        assert_eq!(closed_declared(&declared), vec![Capability::ProcessSpawn]);
        assert!(!Capability::McClient.is_open());
        assert!(Capability::Java.is_open());
    }

    #[test]
    fn risk_to_policy_mapping_is_total() {
        for cap in Capability::ALL {
            let policy = cap.default_policy();
            match cap.risk() {
                CapabilityRisk::Low => assert_eq!(policy, DefaultPolicy::Allow),
                CapabilityRisk::Medium => assert_eq!(policy, DefaultPolicy::Ask),
                CapabilityRisk::High => assert_eq!(policy, DefaultPolicy::Deny),
                CapabilityRisk::Critical => assert_eq!(policy, DefaultPolicy::Closed),
            }
        }
    }

    #[test]
    fn medium_and_above_require_approval() {
        assert!(!CapabilityRisk::Low.needs_approval());
        assert!(CapabilityRisk::Medium.needs_approval());
        assert!(CapabilityRisk::High.needs_approval());
    }
}
