//! Minecraft / JVM 进程运行：supervisor 拉起、IPC、控制台与日志
//!
//! 结构：
//!
//! - [`LocalSupervisor`] 实现 `port::SupervisorPort`（CLI / service 侧）
//! - [`run_supervisor`] 是 `mcsup __supervise` 的入口（守护侧主循环）
//! - IPC 帧契约在 `protocol` crate（版本化 JSON-line），runtime 只做收发
//!
//! supervisor 以独立进程组运行并忽略 SIGHUP：CLI 退出、终端关闭都不影响
//! 已拉起的服务端；停机走 stdin 注入 `stop`，宽限到期且 `force` 才强杀。

mod client;
#[cfg(test)]
mod console_encoding_tests;
#[cfg(test)]
mod e2e_tests;
mod endpoint;
mod launch;
mod local;
mod process;
mod supervisor;

pub use launch::MinecraftLaunch;
pub use local::LocalSupervisor;
pub use supervisor::run_supervisor;
