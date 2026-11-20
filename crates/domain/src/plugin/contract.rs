//! 契约闭集：六个跨插件契约的标识、多重性、能力与超时
//!
//! 插件之间不允许直连。组合只有两条路：事件广播（1:N，无返回值）与契约
//! 调用（1:1 或 1:N，有超时）。这里登记 v1 允许的全部契约；未知名字必须让
//! 安装失败，`[禁止]` 插件自定义契约或动态方法名。

use crate::plugin::PluginId;
use crate::plugin::capability::Capability;
use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

/// v1 契约的固定版本后缀
pub const CONTRACT_VERSION: u32 = 1;

/// 调用链最大深度
pub const MAX_CONTRACT_DEPTH: usize = 4;

/// 单次请求内契约调用总数上限
pub const MAX_CONTRACT_FANOUT: u32 = 32;

/// 调用链中的一层：哪个插件调用了哪个契约
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallStep {
    /// 调用方插件
    pub plugin: PluginId,
    /// 被调用的契约（不含版本）
    pub contract: Contract,
}

/// 契约调用链判定失败
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallChainError {
    /// 同一 `(plugin, contract)` 在链上出现第二次
    #[error("contract call cycle: {plugin} already called {contract}")]
    Cycle {
        /// 插件
        plugin: PluginId,
        /// 契约
        contract: Contract,
    },
    /// 超过最大深度
    #[error("contract call depth exceeds {MAX_CONTRACT_DEPTH}")]
    TooDeep,
    /// 超过单次请求的调用总数
    #[error("contract call fanout exceeds {MAX_CONTRACT_FANOUT}")]
    TooBroad,
}

/// 一次请求的调用链
///
/// 只保存判定所需的最小信息：调用方身份与契约。宿主在进入提供方之前把本层
/// 压入链，提供方再次发起契约调用时带回来，循环与深度因此不依赖调用栈猜测。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallChain {
    steps: Vec<CallStep>,
}

impl CallChain {
    /// 空链（根调用）
    #[must_use]
    pub const fn new() -> Self {
        Self { steps: Vec::new() }
    }

    /// 已记录的层
    #[must_use]
    pub fn steps(&self) -> &[CallStep] {
        &self.steps
    }

    /// 层数
    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// 是否为空
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// 压入一层；重复 `(plugin, contract)` 视为循环
    ///
    /// # Errors
    ///
    /// 循环、超过 [`MAX_CONTRACT_DEPTH`] 时返回 [`CallChainError`]
    pub fn push(&mut self, plugin: PluginId, contract: Contract) -> Result<(), CallChainError> {
        if self
            .steps
            .iter()
            .any(|step| step.plugin == plugin && step.contract == contract)
        {
            return Err(CallChainError::Cycle { plugin, contract });
        }
        if self.steps.len() >= MAX_CONTRACT_DEPTH {
            return Err(CallChainError::TooDeep);
        }
        self.steps.push(CallStep { plugin, contract });
        Ok(())
    }
}

/// 记入本次请求的调用数并检查扇出
///
/// # Errors
///
/// 总数超过 [`MAX_CONTRACT_FANOUT`] 或溢出时返回
/// [`CallChainError::TooBroad`]
pub fn ensure_fanout(current: u32, additional: u32) -> Result<u32, CallChainError> {
    let total = current
        .checked_add(additional)
        .ok_or(CallChainError::TooBroad)?;
    if total > MAX_CONTRACT_FANOUT {
        return Err(CallChainError::TooBroad);
    }
    Ok(total)
}

/// 闭集中的一个契约
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Contract {
    /// 向游戏内广播消息
    ChatPublish,
    /// 提问并等待匹配的聊天回复
    ChatAsk,
    /// 与 AI agent 对话
    AgentConverse,
    /// 采样指标
    MetricsSample,
    /// 上报故障
    IncidentReport,
    /// 请求启停
    LifecycleAct,
}

impl Contract {
    /// 全部六个契约
    pub const ALL: [Self; 6] = [
        Self::ChatPublish,
        Self::ChatAsk,
        Self::AgentConverse,
        Self::MetricsSample,
        Self::IncidentReport,
        Self::LifecycleAct,
    ];

    /// 契约名（不含版本后缀）
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ChatPublish => "chat.publish",
            Self::ChatAsk => "chat.ask",
            Self::AgentConverse => "agent.converse",
            Self::MetricsSample => "metrics.sample",
            Self::IncidentReport => "incident.report",
            Self::LifecycleAct => "lifecycle.act",
        }
    }

    /// 多重性
    #[must_use]
    pub const fn multiplicity(self) -> Multiplicity {
        match self {
            Self::ChatAsk | Self::AgentConverse | Self::LifecycleAct => Multiplicity::Unicast,
            Self::ChatPublish | Self::MetricsSample | Self::IncidentReport => {
                Multiplicity::Broadcast
            }
        }
    }

    /// 提供方需要的能力（全部满足才允许 `provides`）
    #[must_use]
    pub const fn required_capabilities(self) -> &'static [Capability] {
        match self {
            Self::ChatPublish => &[Capability::ConsoleWrite],
            Self::ChatAsk => &[Capability::ConsoleRead, Capability::ConsoleWrite],
            Self::AgentConverse => &[Capability::NetOutbound],
            Self::MetricsSample => &[Capability::ServerRead],
            Self::IncidentReport => &[],
            Self::LifecycleAct => &[Capability::ServerLifecycle],
        }
    }

    /// 单次调用的默认超时
    #[must_use]
    pub const fn timeout(self) -> Duration {
        match self {
            Self::ChatPublish | Self::MetricsSample | Self::IncidentReport => {
                Duration::from_secs(10)
            }
            Self::ChatAsk => Duration::from_secs(60),
            Self::AgentConverse => Duration::from_secs(120),
            Self::LifecycleAct => Duration::from_secs(30),
        }
    }

    /// 是否要求 `machine` 作用域
    ///
    /// `agent.converse` 的会话可以跨越多个服务器（一次对话里查询所有服），
    /// `server` 作用域的插件只能看到单服，提供它必然给出残缺答案。其余契约
    /// 的载荷都带 `server`，单服插件可以合法承担。
    #[must_use]
    pub const fn requires_machine_scope(self) -> bool {
        matches!(self, Self::AgentConverse)
    }

    /// 解析契约名（不含版本）
    ///
    /// # Errors
    ///
    /// 不在闭集内时返回 [`UnknownContract`]
    pub fn parse_name(raw: &str) -> Result<Self, UnknownContract> {
        Self::ALL
            .into_iter()
            .find(|contract| contract.name() == raw)
            .ok_or_else(|| UnknownContract {
                raw: raw.to_owned(),
            })
    }
}

impl fmt::Display for Contract {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// 契约多重性
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Multiplicity {
    /// 广播：所有提供方，部分失败仍整体成功
    Broadcast,
    /// 单播：唯一提供方；多个时必须带 `target`
    Unicast,
}

impl Multiplicity {
    /// 是否允许 `target` 字段
    #[must_use]
    pub const fn allows_target(self) -> bool {
        matches!(self, Self::Unicast)
    }
}

/// 带版本后缀的契约标识，如 `chat.publish@1`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContractId {
    contract: Contract,
    version: u32,
}

impl ContractId {
    /// 构造闭集内某版本的标识
    ///
    /// # Errors
    ///
    /// v1 只存在 `@1`：其它版本一律按未知契约处理，不做兼容猜测
    pub fn new(contract: Contract, version: u32) -> Result<Self, UnknownContractVersion> {
        if version == CONTRACT_VERSION {
            Ok(Self { contract, version })
        } else {
            Err(UnknownContractVersion {
                name: contract.name().to_owned(),
                version,
            })
        }
    }

    /// 解析 `name@version`
    ///
    /// # Errors
    ///
    /// 缺少 `@`、版本非数字、名字不在闭集或版本未知时返回 [`ContractError`]
    pub fn parse(raw: &str) -> Result<Self, ContractError> {
        let (name, version) = raw
            .rsplit_once('@')
            .ok_or_else(|| ContractError::Malformed {
                raw: raw.to_owned(),
            })?;
        if name.is_empty() || version.is_empty() {
            return Err(ContractError::Malformed {
                raw: raw.to_owned(),
            });
        }
        let version: u32 = version.parse().map_err(|_| ContractError::Malformed {
            raw: raw.to_owned(),
        })?;
        let contract = Contract::parse_name(name)?;
        let id = Self::new(contract, version)?;
        Ok(id)
    }

    /// 契约本体
    #[must_use]
    pub const fn contract(self) -> Contract {
        self.contract
    }

    /// 版本后缀
    #[must_use]
    pub const fn version(self) -> u32 {
        self.version
    }

    /// 规范文本
    #[must_use]
    pub fn to_wire(self) -> String {
        format!("{}@{}", self.contract.name(), self.version)
    }
}

impl fmt::Display for ContractId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.contract.name(), self.version)
    }
}

/// 未知契约
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown contract: {raw}")]
pub struct UnknownContract {
    /// 原始输入
    pub raw: String,
}

/// 未知契约版本
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown contract version: {name}@{version}")]
pub struct UnknownContractVersion {
    /// 契约名
    pub name: String,
    /// 请求的版本
    pub version: u32,
}

/// 契约标识解析失败
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContractError {
    /// 缺少 `@` 或版本不是数字
    #[error("malformed contract id: {raw:?}")]
    Malformed {
        /// 原始输入
        raw: String,
    },
    /// 名字不在闭集
    #[error(transparent)]
    Unknown(#[from] UnknownContract),
    /// 版本未知
    #[error(transparent)]
    Version(#[from] UnknownContractVersion),
}

/// 解析契约列表并去重
///
/// # Errors
///
/// 任一项非法时返回该项错误；不跳过非法项
pub fn parse_all(raw: &[String]) -> Result<BTreeSet<ContractId>, ContractError> {
    raw.iter()
        .map(|item| ContractId::parse(item))
        .collect::<Result<BTreeSet<_>, _>>()
}

/// `provides` 的能力校验：返回缺失能力
#[must_use]
pub fn missing_capabilities(
    contract: Contract,
    declared: &BTreeSet<Capability>,
) -> Vec<Capability> {
    contract
        .required_capabilities()
        .iter()
        .copied()
        .filter(|cap| !declared.contains(cap))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_set_has_six_contracts() {
        assert_eq!(Contract::ALL.len(), 6);
        let names: BTreeSet<&str> = Contract::ALL.iter().map(|c| c.name()).collect();
        assert_eq!(names.len(), 6);
        for contract in Contract::ALL {
            assert_eq!(Contract::parse_name(contract.name()), Ok(contract));
        }
    }

    #[test]
    fn contract_ids_roundtrip() {
        for contract in Contract::ALL {
            let id = ContractId::new(contract, 1).expect("v1");
            assert_eq!(ContractId::parse(&id.to_wire()), Ok(id));
            assert_eq!(id.to_string(), format!("{}@1", contract.name()));
        }
    }

    #[test]
    fn malformed_ids_are_rejected() {
        for raw in [
            "chat.publish",
            "chat.publish@",
            "@1",
            "chat.publish@x",
            "chat.publish@0",
            "chat.publish@999",
            "unknown@1",
            "Chat.Publish@1",
        ] {
            assert!(ContractId::parse(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn version_two_is_not_accepted_by_v1_host() {
        assert!(ContractId::new(Contract::ChatPublish, 2).is_err());
    }

    #[test]
    fn unicast_only_contracts_allow_target() {
        for contract in Contract::ALL {
            assert_eq!(
                contract.multiplicity().allows_target(),
                matches!(
                    contract,
                    Contract::ChatAsk | Contract::AgentConverse | Contract::LifecycleAct
                )
            );
        }
    }

    #[test]
    fn required_capabilities_match_catalog() {
        let declared: BTreeSet<Capability> = [
            Capability::ConsoleWrite,
            Capability::ConsoleRead,
            Capability::NetOutbound,
            Capability::ServerRead,
            Capability::ServerLifecycle,
        ]
        .into_iter()
        .collect();
        for contract in Contract::ALL {
            assert!(
                missing_capabilities(contract, &declared).is_empty(),
                "{contract}"
            );
        }
        let none = BTreeSet::new();
        assert_eq!(
            missing_capabilities(Contract::ChatAsk, &none),
            vec![Capability::ConsoleRead, Capability::ConsoleWrite]
        );
        assert!(missing_capabilities(Contract::IncidentReport, &none).is_empty());
    }

    #[test]
    fn agent_converse_requires_machine_scope() {
        for contract in Contract::ALL {
            assert_eq!(
                contract.requires_machine_scope(),
                contract == Contract::AgentConverse
            );
        }
    }

    #[test]
    fn timeouts_are_bounded_and_distinct() {
        assert_eq!(Contract::ChatPublish.timeout(), Duration::from_secs(10));
        assert_eq!(Contract::AgentConverse.timeout(), Duration::from_secs(120));
        assert_eq!(Contract::LifecycleAct.timeout(), Duration::from_secs(30));
    }

    #[test]
    fn call_chain_rejects_cycles_and_depth() {
        let alpha = PluginId::parse("alpha").expect("id");
        let beta = PluginId::parse("beta").expect("id");
        let mut chain = CallChain::new();
        chain
            .push(alpha.clone(), Contract::IncidentReport)
            .expect("first");
        assert_eq!(chain.len(), 1);
        assert_eq!(
            chain.push(alpha.clone(), Contract::IncidentReport),
            Err(CallChainError::Cycle {
                plugin: alpha.clone(),
                contract: Contract::IncidentReport,
            })
        );
        chain.push(beta, Contract::MetricsSample).expect("second");
        chain
            .push(alpha.clone(), Contract::ChatPublish)
            .expect("third");
        chain.push(alpha, Contract::ChatAsk).expect("fourth");
        assert_eq!(
            chain.push(
                PluginId::parse("gamma").expect("id"),
                Contract::LifecycleAct
            ),
            Err(CallChainError::TooDeep)
        );
    }

    #[test]
    fn fanout_is_capped() {
        assert_eq!(ensure_fanout(0, 32), Ok(32));
        assert_eq!(ensure_fanout(32, 1), Err(CallChainError::TooBroad));
        assert_eq!(ensure_fanout(u32::MAX, 1), Err(CallChainError::TooBroad));
    }
}
