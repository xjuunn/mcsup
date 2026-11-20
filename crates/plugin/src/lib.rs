//! 插件宿主运行时
//!
//! 只做执行与协议适配：进程拉起、端点、帧、会话、心跳、能力分发。判定在
//! `domain::plugin`，业务编排在 `service::plugin`，入口只映射错误。
//!
//! # 依赖方向
//!
//! `plugin` 允许依赖 `domain` / `port` / `protocol`，`[禁止]` 依赖 `service`
//! 或其它实现层。宿主需要的业务能力经 [`api::HostServices`] 注入，装配只在
//! 门面 `bootstrap`。
//!
//! # 长驻
//!
//! [`host::PluginHost`] 必须活得比一次 CLI 命令久：控制端点、单实例锁与空闲
//! 退出在 `apps/cli` 的 `__plugin-host` 外壳里，进程内不建全局单例。

pub mod api;
pub mod base64;
pub mod conn;
pub mod console_events;
pub mod contract;
pub mod control;
pub mod event;
pub mod host;
#[cfg(windows)]
mod job;
pub mod process;
pub mod rfc3339;
pub mod session;

pub mod endpoint;

pub use api::{DispatchContext, HostServices, dispatch};
pub use control::{ControlClient, ControlServer, HostControl};
pub use event::{EventBus, EventDelivery, EventSinkAdapter};
pub use host::PluginHost;
pub use session::Heartbeat;
