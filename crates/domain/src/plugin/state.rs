//! 插件生命周期状态机
//!
//! 期望状态（`enabled`）与实际状态（`state`）分开存：`enabled=true` 不代表
//! 进程在跑，惰性启动下它是 `stopped`。迁移只在这里判定，入口只映射错误，
//! `[禁止]` 在 CLI 或端点处理里重写一份「能不能停」。

use std::fmt;

/// 插件实际状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PluginState {
    /// 已安装，存在待确认能力
    PendingApproval,
    /// 未启用（安装后的初始状态或用户停用）
    Disabled,
    /// 进程已拉起，握手未完成
    Starting,
    /// 握手完成，可调用
    Running,
    /// 部分能力不可用或配置应用失败
    Degraded,
    /// 正在优雅退出
    Stopping,
    /// 已启用但进程不在
    Stopped,
    /// 崩溃超限或握手失败
    Failed,
    /// 版本切换中
    Updating,
}

impl PluginState {
    /// 稳定小写文本（输出 / 记录 / 审计口径）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PendingApproval => "pending_approval",
            Self::Disabled => "disabled",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Degraded => "degraded",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Updating => "updating",
        }
    }

    /// 进程是否存活（`starting` / `running` / `degraded` / `stopping`）
    #[must_use]
    pub const fn is_alive(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Running | Self::Degraded | Self::Stopping
        )
    }

    /// 是否允许接受调用
    #[must_use]
    pub const fn accepts_calls(self) -> bool {
        matches!(self, Self::Running | Self::Degraded)
    }

    /// 卸载前置条件：已禁用且进程已退出
    #[must_use]
    pub const fn can_uninstall(self) -> bool {
        matches!(self, Self::Disabled | Self::PendingApproval)
    }
}

impl fmt::Display for PluginState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 触发迁移的事件
///
/// 事件是**已发生的事实**或明确的用户意图；不含「检查当前状态」这种查询。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PluginEvent {
    /// 安装完成，无需确认
    Installed,
    /// 安装完成，存在待确认能力
    InstalledNeedsApproval,
    /// 用户确认了待定能力
    Approved,
    /// 用户启用
    Enabled,
    /// 用户或首次调用请求启动
    StartRequested,
    /// 握手完成
    Welcome,
    /// 握手超时、被拒或进程提前退出
    HandshakeFailed,
    /// 用户请求停止
    StopRequested,
    /// 进程崩溃；`will_retry` 由退避策略决定
    Crashed {
        /// 是否还有重试额度
        will_retry: bool,
    },
    /// 配置应用失败
    ConfigFailed,
    /// 配置修正成功
    ConfigRecovered,
    /// 进程在 `stopping` 后正常退出
    Exited,
    /// 停止超时，已被强杀
    StopTimedOut,
    /// 用户停用
    Disabled,
    /// 开始版本切换（先停进程）
    UpdateStarted,
    /// 版本切换成功
    UpdateSucceeded,
    /// 版本切换失败
    UpdateFailed,
}

/// 非法迁移
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid plugin state transition: {from} + {event:?}")]
pub struct InvalidPluginTransition {
    /// 迁移前状态
    pub from: PluginState,
    /// 触发事件
    pub event: PluginEvent,
}

impl PluginState {
    /// 判定一次迁移
    ///
    /// 规则与 `docs/plugins.md` 的迁移表一一对应。幂等事件（已停时再 `stop`、
    /// 已禁用时再 `disable`）返回原状态，调用方不必先查一次再决定发不发。
    ///
    /// # Errors
    ///
    /// 迁移不在表中时返回 [`InvalidPluginTransition`]
    pub fn transition(self, event: PluginEvent) -> Result<Self, InvalidPluginTransition> {
        use PluginEvent as E;
        use PluginState as S;
        // 同一目标状态的边合并在一个分支里：它们是同一类迁移，展开只会让
        // 「哪些状态能到这里」变得难读。
        let next = match (self, event) {
            (S::PendingApproval, E::Approved | E::Disabled)
            | (S::Stopped | S::Failed | S::Disabled, E::Disabled) => S::Disabled,
            (S::Disabled, E::Enabled)
            | (S::Stopping, E::Exited)
            | (S::Updating, E::UpdateSucceeded)
            | (S::Stopped, E::StopRequested) => S::Stopped,
            (S::Stopped | S::Failed, E::StartRequested)
            | (S::Running | S::Degraded, E::Crashed { will_retry: true }) => S::Starting,
            (S::Starting, E::Welcome) | (S::Degraded, E::ConfigRecovered) => S::Running,
            (S::Starting, E::HandshakeFailed | E::Crashed { .. })
            | (S::Running | S::Degraded, E::Crashed { will_retry: false })
            | (S::Stopping, E::StopTimedOut)
            | (S::Updating, E::UpdateFailed) => S::Failed,
            (S::Running | S::Degraded | S::Stopping, E::StopRequested) => S::Stopping,
            (S::Running, E::ConfigFailed) => S::Degraded,
            (from, E::UpdateStarted) if from != S::Updating => S::Updating,
            (from, event) => return Err(InvalidPluginTransition { from, event }),
        };
        Ok(next)
    }

    /// 授权后的展示状态
    ///
    /// 授权只影响「待确认 → 可用」这一条边：仍存在待确认能力时状态不变；
    /// 没有待确认项且当前卡在 `pending_approval` 时回到 `disabled`（等待用户
    /// enable，而不是自动启动）。其余状态一律不动——授权不会启动进程。
    #[must_use]
    pub const fn after_grant(self, needs_approval: bool) -> Self {
        if !needs_approval && matches!(self, Self::PendingApproval) {
            Self::Disabled
        } else {
            self
        }
    }

    /// 撤销后的展示状态
    ///
    /// 撤销可能让插件重新出现待确认能力：此时回到 `pending_approval`；
    /// 其余状态不变——撤销不会停止正在运行的进程，能力集合在会话内即时收缩。
    #[must_use]
    pub const fn after_revoke(self, needs_approval: bool) -> Self {
        if needs_approval {
            Self::PendingApproval
        } else {
            self
        }
    }
}

impl PluginState {
    /// 安装完成后的初始状态
    ///
    /// 低风险能力自动通过 → `disabled`；存在中高风险 → `pending_approval`。
    #[must_use]
    pub const fn after_install(needs_approval: bool) -> Self {
        if needs_approval {
            Self::PendingApproval
        } else {
            Self::Disabled
        }
    }
}

impl PluginEvent {
    /// 安装事件
    #[must_use]
    pub const fn installed(needs_approval: bool) -> Self {
        if needs_approval {
            Self::InstalledNeedsApproval
        } else {
            Self::Installed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STATES: [PluginState; 9] = [
        PluginState::PendingApproval,
        PluginState::Disabled,
        PluginState::Starting,
        PluginState::Running,
        PluginState::Degraded,
        PluginState::Stopping,
        PluginState::Stopped,
        PluginState::Failed,
        PluginState::Updating,
    ];

    const ALL_EVENTS: [PluginEvent; 16] = [
        PluginEvent::Installed,
        PluginEvent::InstalledNeedsApproval,
        PluginEvent::Approved,
        PluginEvent::Enabled,
        PluginEvent::StartRequested,
        PluginEvent::Welcome,
        PluginEvent::HandshakeFailed,
        PluginEvent::StopRequested,
        PluginEvent::Crashed { will_retry: true },
        PluginEvent::Crashed { will_retry: false },
        PluginEvent::ConfigFailed,
        PluginEvent::ConfigRecovered,
        PluginEvent::Exited,
        PluginEvent::StopTimedOut,
        PluginEvent::Disabled,
        PluginEvent::UpdateStarted,
    ];

    #[test]
    fn every_state_has_stable_wire_text() {
        for state in ALL_STATES {
            assert!(!state.as_str().is_empty());
            assert_eq!(state.as_str(), state.as_str().to_lowercase());
        }
    }

    #[test]
    fn happy_path_migrates() {
        let s = PluginState::PendingApproval
            .transition(PluginEvent::Approved)
            .and_then(|s| s.transition(PluginEvent::Enabled))
            .and_then(|s| s.transition(PluginEvent::StartRequested))
            .and_then(|s| s.transition(PluginEvent::Welcome))
            .and_then(|s| s.transition(PluginEvent::StopRequested))
            .and_then(|s| s.transition(PluginEvent::Exited));
        assert_eq!(s, Ok(PluginState::Stopped));
    }

    #[test]
    fn full_matrix_never_panics_and_reports_from_and_event() {
        for from in ALL_STATES {
            for event in ALL_EVENTS {
                match from.transition(event) {
                    Ok(next) => {
                        assert_ne!(
                            next,
                            PluginState::PendingApproval,
                            "{from} + {event:?} 不应回到待确认"
                        );
                    }
                    Err(err) => {
                        assert_eq!(err.from, from);
                        assert_eq!(err.event, event);
                    }
                }
            }
        }
    }

    #[test]
    fn disabled_gate_rejects_start() {
        assert!(!PluginState::Disabled.is_alive());
        assert!(matches!(
            PluginState::Disabled
                .transition(PluginEvent::StartRequested)
                .map_err(|e| e.from),
            Err(PluginState::Disabled)
        ));
        // 未启用时的 start 是用法/状态冲突，不是迁移
    }

    #[test]
    fn stop_is_idempotent() {
        assert_eq!(
            PluginState::Stopped.transition(PluginEvent::StopRequested),
            Ok(PluginState::Stopped)
        );
        assert_eq!(
            PluginState::Stopping.transition(PluginEvent::StopRequested),
            Ok(PluginState::Stopping)
        );
        assert_eq!(
            PluginState::Disabled.transition(PluginEvent::Disabled),
            Ok(PluginState::Disabled)
        );
    }

    #[test]
    fn crash_without_retry_ends_failed() {
        assert_eq!(
            PluginState::Running.transition(PluginEvent::Crashed { will_retry: false }),
            Ok(PluginState::Failed)
        );
        assert_eq!(
            PluginState::Running.transition(PluginEvent::Crashed { will_retry: true }),
            Ok(PluginState::Starting)
        );
        assert_eq!(
            PluginState::Starting.transition(PluginEvent::Crashed { will_retry: true }),
            Ok(PluginState::Failed)
        );
    }

    #[test]
    fn stop_timeout_is_failed_not_stopped() {
        assert_eq!(
            PluginState::Stopping.transition(PluginEvent::StopTimedOut),
            Ok(PluginState::Failed)
        );
    }

    #[test]
    fn install_picks_initial_state() {
        assert_eq!(PluginState::after_install(false), PluginState::Disabled);
        assert_eq!(
            PluginState::after_install(true),
            PluginState::PendingApproval
        );
        assert_eq!(
            PluginEvent::installed(true),
            PluginEvent::InstalledNeedsApproval
        );
    }

    #[test]
    fn uninstall_requires_disabled() {
        for state in ALL_STATES {
            assert_eq!(
                state.can_uninstall(),
                matches!(state, PluginState::Disabled | PluginState::PendingApproval)
            );
        }
    }

    #[test]
    fn update_cannot_nest() {
        for from in ALL_STATES {
            let expect_ok = from != PluginState::Updating;
            assert_eq!(
                from.transition(PluginEvent::UpdateStarted).is_ok(),
                expect_ok,
                "{from}"
            );
        }
    }
}
