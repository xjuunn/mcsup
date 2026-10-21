use crate::server::error::InvalidStatusTransition;
use std::fmt;

/// 服务器生命周期状态
///
/// 生命周期必须用 enum 显式建模：多个 bool 会出现
/// `running` 与 `starting` 同时为真的非法组合。判定收敛在
/// [`ServerStatus::transition`]，入口只把领域错误映射成各自的表现形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServerStatus {
    /// 未运行（含 supervisor 不在）
    Stopped,
    /// 已拉起，尚未就绪（未见 Done 行）
    Starting,
    /// 就绪运行中
    Running,
    /// 已请求停机，进程尚存
    Stopping,
    /// 意外退出（非停机请求下的非零退出）
    Crashed,
}

impl ServerStatus {
    /// 判定一次状态转换是否合法
    ///
    /// 合法后继：
    ///
    /// | 当前 | 可达 |
    /// | --- | --- |
    /// | `Stopped` / `Crashed` | `Starting` |
    /// | `Starting` | `Running` / `Stopping` / `Stopped` / `Crashed` |
    /// | `Running` | `Stopping` / `Stopped` / `Crashed` |
    /// | `Stopping` | `Stopped` / `Crashed` |
    ///
    /// 同状态与回退（如 `Running → Starting`、`Stopped → Stopping`）由调用方
    /// 先行判定（幂等不重复触发），非法流转一律返回
    /// [`InvalidStatusTransition`]。
    ///
    /// # Errors
    ///
    /// `next` 不是 `self` 的合法后继时返回 [`InvalidStatusTransition`]
    pub fn transition(self, next: Self) -> Result<Self, InvalidStatusTransition> {
        use ServerStatus::{Crashed, Running, Starting, Stopped, Stopping};
        let allowed = matches!(
            (self, next),
            (Stopped | Crashed, Starting)
                | (Starting, Running | Stopping | Stopped | Crashed)
                | (Running, Stopping | Stopped | Crashed)
                | (Stopping, Stopped | Crashed)
        );
        if allowed {
            Ok(next)
        } else {
            Err(InvalidStatusTransition {
                from: self,
                to: next,
            })
        }
    }

    /// 进程已退出（`Stopped` / `Crashed`）
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped | Self::Crashed)
    }

    /// 进程尚存（`Starting` / `Running` / `Stopping`）
    #[must_use]
    pub const fn is_alive(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }

    /// 稳定小写文本（输出与 JSON 口径）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Crashed => "crashed",
        }
    }
}

impl fmt::Display for ServerStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_reaches_running_then_stops() {
        assert_eq!(
            ServerStatus::Stopped.transition(ServerStatus::Starting),
            Ok(ServerStatus::Starting)
        );
        assert_eq!(
            ServerStatus::Starting.transition(ServerStatus::Running),
            Ok(ServerStatus::Running)
        );
        assert_eq!(
            ServerStatus::Running.transition(ServerStatus::Stopping),
            Ok(ServerStatus::Stopping)
        );
        assert_eq!(
            ServerStatus::Stopping.transition(ServerStatus::Stopped),
            Ok(ServerStatus::Stopped)
        );
    }

    #[test]
    fn crash_is_reachable_from_alive_states_and_restart_from_crashed() {
        assert_eq!(
            ServerStatus::Starting.transition(ServerStatus::Crashed),
            Ok(ServerStatus::Crashed)
        );
        assert_eq!(
            ServerStatus::Running.transition(ServerStatus::Crashed),
            Ok(ServerStatus::Crashed)
        );
        assert_eq!(
            ServerStatus::Stopping.transition(ServerStatus::Crashed),
            Ok(ServerStatus::Crashed)
        );
        // 崩溃后可再次启动
        assert_eq!(
            ServerStatus::Crashed.transition(ServerStatus::Starting),
            Ok(ServerStatus::Starting)
        );
    }

    #[test]
    fn stop_during_boot_is_legal() {
        assert_eq!(
            ServerStatus::Starting.transition(ServerStatus::Stopping),
            Ok(ServerStatus::Stopping)
        );
        assert_eq!(
            ServerStatus::Starting.transition(ServerStatus::Stopped),
            Ok(ServerStatus::Stopped)
        );
    }

    #[test]
    fn illegal_and_backward_transitions_are_rejected() {
        // 回退 / 同状态 / 越级
        assert!(
            ServerStatus::Running
                .transition(ServerStatus::Starting)
                .is_err()
        );
        assert!(
            ServerStatus::Stopped
                .transition(ServerStatus::Running)
                .is_err()
        );
        assert!(
            ServerStatus::Stopped
                .transition(ServerStatus::Stopped)
                .is_err()
        );
        assert!(
            ServerStatus::Stopped
                .transition(ServerStatus::Crashed)
                .is_err()
        );
        assert!(
            ServerStatus::Crashed
                .transition(ServerStatus::Running)
                .is_err()
        );
        assert!(
            ServerStatus::Stopping
                .transition(ServerStatus::Running)
                .is_err()
        );
    }

    #[test]
    fn terminal_alive_flags_and_wire_text() {
        assert!(ServerStatus::Stopped.is_terminal());
        assert!(ServerStatus::Crashed.is_terminal());
        assert!(!ServerStatus::Running.is_terminal());
        assert!(ServerStatus::Running.is_alive());
        assert!(!ServerStatus::Stopped.is_alive());
        assert_eq!(ServerStatus::Starting.as_str(), "starting");
        assert_eq!(ServerStatus::Stopping.to_string(), "stopping");
    }
}
