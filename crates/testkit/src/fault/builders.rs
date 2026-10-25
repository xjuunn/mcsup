//! 按名字与序号构造 `FakeSupervisor`
//!
//! 与 [`supervisor`](super::supervisor) 的分工：那边是实现，这边是「一行拿到
//! 你要的那台假服务器」，让 service / runtime 的测试少写样板。

use super::supervisor::{CommandLog, FakeSupervisor};

/// 服务器不运行的假 supervisor
#[must_use]
pub fn stopped_supervisor() -> FakeSupervisor {
    FakeSupervisor::new(CommandLog::stopped())
}

/// 服务器运行中的假 supervisor
#[must_use]
pub fn running_supervisor() -> FakeSupervisor {
    FakeSupervisor::new(CommandLog::running())
}

/// 第 N 次 `send_line` 失败的假 supervisor
#[must_use]
pub fn supervisor_failing_send(attempt: usize) -> FakeSupervisor {
    FakeSupervisor::new(CommandLog::running().failing_send_on(attempt))
}

/// 第 N 次 `status` 失败的假 supervisor
#[must_use]
pub fn supervisor_failing_status(attempt: usize) -> FakeSupervisor {
    FakeSupervisor::new(CommandLog::stopped().failing_status_on(attempt))
}
