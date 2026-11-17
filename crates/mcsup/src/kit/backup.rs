//! 备份任务：一行建快照
//!
//! 运行中备份的时序（`save-off` → `save-all flush` → 扫描 → `save-on` 兜底）
//! 全在 `service::BackupService`，kit 只传默认值与取消令牌。

use super::Kit;
use super::config::KitConfig;
use super::error::KitError;
use super::options::RunOptions;
use domain::backup::BackupScope;
use service::{CreateOptions, CreateOutcome};

/// 建快照的单次覆盖
///
/// ```no_run
/// use mcsup::kit::BackupNow;
///
/// // 全默认（范围取服务属性，运行中会先关盘再落盘）
/// let plain = BackupNow::default();
/// // 只加一个标签
/// let tagged = BackupNow::default().label("before-upgrade");
/// ```
#[derive(Clone, Debug, Default)]
pub struct BackupNow {
    label: Option<String>,
    scope: Option<BackupScope>,
    no_reuse: Option<bool>,
    flush_wait_ms: Option<u64>,
    run: RunOptions,
}

impl BackupNow {
    /// 用户标签（如 `before-upgrade`），进快照摘要与列表
    #[must_use]
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// 覆盖备份范围；缺省取服务属性里的范围
    #[must_use]
    pub fn scope(mut self, scope: BackupScope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// 跳过父快照复用提示（强制全量重读）
    #[must_use]
    pub fn no_reuse(mut self, no_reuse: bool) -> Self {
        self.no_reuse = Some(no_reuse);
        self
    }

    /// 运行中等待 `save-all` 落盘的时长（毫秒）
    #[must_use]
    pub fn flush_wait_ms(mut self, millis: u64) -> Self {
        self.flush_wait_ms = Some(millis);
        self
    }

    /// 取消令牌（扫描大目录时靠它协作取消）
    #[must_use]
    pub fn run(mut self, run: RunOptions) -> Self {
        self.run = run;
        self
    }

    /// 与默认策略合并（纯计算，不碰 IO）
    ///
    /// 消费 `self`：options 只服务一次调用，调用方不必再持有它。
    fn resolve(self, config: &KitConfig) -> (CreateOptions, RunOptions) {
        let create = CreateOptions {
            label: self.label,
            scope: self.scope,
            no_reuse: self.no_reuse.unwrap_or(config.no_reuse),
            flush_wait_ms: Some(self.flush_wait_ms.unwrap_or(config.flush_wait_ms)),
        };
        (create, self.run)
    }
}

impl Kit<'_> {
    /// 一行建快照
    ///
    /// 服务器运行中也能备份：service 先 `save-off` 再 `save-all flush`，扫描
    /// 期间不会有不一致写入，结束时由守卫补 `save-on`。本方法不替调用方判断
    /// 该不该备份——范围与保留策略都是服务属性 / 领域规则的事。
    ///
    /// 这是**阻塞**调用（扫描 + CAS 写盘）。在异步运行时里调用请自行
    /// `spawn_blocking`；本仓库不把 `block_on` 塞进库里。
    ///
    /// # Errors
    ///
    /// 服务器不存在、范围为空、CAS / 扫描 / 记录写入失败，或注入控制台命令
    /// 失败时返回 [`KitError`]
    pub fn backup_now(&self, name: &str, options: BackupNow) -> Result<CreateOutcome, KitError> {
        let (resolved, run) = options.resolve(self.config());
        let token = run.token();
        Ok(self.core().backup.create(name, &resolved, &token)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未设置的项由 `KitConfig` 补齐
    #[test]
    fn backup_merges_call_over_config() {
        let config = KitConfig {
            no_reuse: true,
            flush_wait_ms: 500,
            ..KitConfig::default()
        };
        let (resolved, _run) = BackupNow::default().resolve(&config);
        assert!(resolved.no_reuse);
        assert_eq!(resolved.flush_wait_ms, Some(500));
        assert!(resolved.label.is_none());
        assert!(resolved.scope.is_none());
    }

    /// 单次设置优先于配置
    #[test]
    fn backup_call_overrides_config() {
        let config = KitConfig {
            no_reuse: true,
            ..KitConfig::default()
        };
        let (resolved, _run) = BackupNow::default()
            .label("before-upgrade")
            .no_reuse(false)
            .flush_wait_ms(100)
            .resolve(&config);
        assert_eq!(resolved.label.as_deref(), Some("before-upgrade"));
        assert!(!resolved.no_reuse);
        assert_eq!(resolved.flush_wait_ms, Some(100));
    }
}
