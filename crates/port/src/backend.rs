//! 执行位置抽象：回答「这个操作在哪里执行」
//!
//! Backend 只承担 Local / Remote 的**执行位置**差异，不承担「能不能执行」
//! 的领域判定——后者在 `domain` / `service`。它把「按位置变化」的少数能力
//! 聚合成一个对象，`service` 持有 `Arc<dyn Backend>`，不出现
//! `if remote { ... } else { ... }`。
//!
//! 刻意保持**窄**：只收拢真正的执行位置能力（进程生命周期、安装器），
//! 不把存储 / 备份等数据能力塞进来。把它们放在一起会变成 God Trait，
//! 而且「存到哪里」与「在哪里跑」是两个不同的轴。数据能力见
//! [`crate::BackupStore`]。
//!
//! 本 trait 只返回 `port` 内的契约类型，不含任何实现层类型，因此
//! `backend` 实现 crate 可以组合 `runtime` / `storage` / `backup`，而
//! `service` 对其一无所知。

use crate::{InstallerRunner, SupervisorPort};

/// 执行位置：Local（本机）/ Remote（经 transport → RPC → agent）
///
/// 实现层负责按位置装配各能力；共享的业务判定仍只在 `domain` / `service`。
pub trait Backend: Send + Sync {
    /// 进程生命周期（启动 / 停机 / 状态 / 控制台注入）
    fn supervisor(&self) -> &dyn SupervisorPort;

    /// 服务端安装器（Forge / `NeoForge`）
    fn installer(&self) -> &dyn InstallerRunner;
}
