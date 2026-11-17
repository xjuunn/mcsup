//! 备份任务：建快照、列快照、恢复、校验、删除、按保留策略清理
//!
//! 运行中备份的时序（`save-off` → `save-all flush` → 扫描 → `save-on` 兜底）
//! 全在 `service::BackupService`，kit 只传默认值与取消令牌。恢复同样：
//! 「原地恢复必须已停机」由领域层判定，kit 不复制这条规则，只把 `RestoreTarget`
//! 原样递交。

use super::Kit;
use super::config::KitConfig;
use super::error::KitError;
use super::options::RunOptions;
use domain::backup::{BackupScope, Page, RestoreTarget, RetentionInput};
use domain::server::meta::ServerName;
use service::{
    CreateOptions, CreateOutcome, DeleteOutcome, PruneOutcome, RestoreOutcome, SnapshotPage,
    VerifyOutcome,
};
use std::time::Duration;

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

/// 快照列表的分页参数
///
/// ```no_run
/// use mcsup::kit::ListBackups;
///
/// let first_page = ListBackups::default();
/// let third = ListBackups::default().page(3).size(50);
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct ListBackups {
    page: Option<usize>,
    size: Option<usize>,
}

impl ListBackups {
    /// 页码，从 1 开始
    #[must_use]
    pub fn page(mut self, page: usize) -> Self {
        self.page = Some(page);
        self
    }

    /// 每页条数
    #[must_use]
    pub fn size(mut self, size: usize) -> Self {
        self.size = Some(size);
        self
    }

    /// 翻成一页（`Page::new` 负责夹紧非法值）
    fn resolve(self) -> Page {
        Page::new(
            self.page.unwrap_or(1),
            self.size.unwrap_or(Page::DEFAULT_SIZE),
        )
    }
}

/// 恢复任务的单次覆盖
///
/// 默认**[`RestoreTarget::InPlace`]**：覆盖原服务器目录，且要求该服务器已停机
/// （由 `service` 判定）。恢复到新服务器用 [`Self::to_new_server`]。
#[derive(Clone, Debug, Default)]
pub struct RestoreBackup {
    target: Option<RestoreTarget>,
    run: RunOptions,
}

impl RestoreBackup {
    /// 原地恢复（默认行为）
    #[must_use]
    pub fn in_place(mut self) -> Self {
        self.target = Some(RestoreTarget::InPlace);
        self
    }

    /// 恢复到新服务器目录（原服务器不动）
    ///
    /// 名称用 [`ServerName`] 而不是裸文本：名字会拼进 `servers/<name>`，
    /// 裸文本能让 `..` 逃出工作区。
    #[must_use]
    pub fn to_new_server(mut self, name: ServerName) -> Self {
        self.target = Some(RestoreTarget::NewServer { name });
        self
    }

    /// 取消令牌（写盘大目录时靠它协作取消）
    #[must_use]
    pub fn run(mut self, run: RunOptions) -> Self {
        self.run = run;
        self
    }

    /// 与默认策略合并：恢复目标没有实例级默认，缺省即原地恢复
    fn resolve(self) -> (RestoreTarget, RunOptions) {
        (self.target.unwrap_or(RestoreTarget::InPlace), self.run)
    }
}

/// 按保留策略清理快照的单次覆盖
///
/// 两个策略都不给时只做 GC，不删任何快照；两个同时给由 `service` 判为冲突
/// 并拒绝——kit 不抢先择一执行。
#[derive(Clone, Copy, Debug, Default)]
pub struct PruneBackups {
    keep: Option<usize>,
    older_than: Option<Duration>,
    dry_run: bool,
}

impl PruneBackups {
    /// 只保留最近 N 份
    #[must_use]
    pub fn keep(mut self, keep: usize) -> Self {
        self.keep = Some(keep);
        self
    }

    /// 只删除早于此时长的快照
    #[must_use]
    pub fn older_than(mut self, age: Duration) -> Self {
        self.older_than = Some(age);
        self
    }

    /// 只统计不删盘
    #[must_use]
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// 与默认策略合并（纯计算）
    fn resolve(self) -> (RetentionInput, bool) {
        let retention = RetentionInput {
            keep: self.keep,
            older_than_secs: self.older_than.map(|age| age.as_secs()),
        };
        (retention, self.dry_run)
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

    /// 列快照（按时间倒序，最新在前）
    ///
    /// # Errors
    ///
    /// 服务器不存在、索引读取失败或服务器位于不可达远端时返回 [`KitError`]
    pub fn list_backups(&self, name: &str, options: ListBackups) -> Result<SnapshotPage, KitError> {
        Ok(self.core().backup.list(name, options.resolve())?)
    }

    /// 从快照恢复
    ///
    /// 原地恢复要求服务器已停机：运行中的 JVM 持有世界文件句柄，换掉文件后
    /// 它继续写旧 inode。恢复前会自动建一份安全快照，恢复错了还能回去。
    ///
    /// # Errors
    ///
    /// 快照不存在 / 不可恢复、原地恢复且服务器在运行、目标服务名已占用、
    /// 落点越界、CAS 对象缺失或写盘失败时返回 [`KitError`]
    pub fn restore_backup(
        &self,
        name: &str,
        snapshot_id: &str,
        options: RestoreBackup,
    ) -> Result<RestoreOutcome, KitError> {
        let (target, run) = options.resolve();
        let token = run.token();
        Ok(self
            .core()
            .backup
            .restore(name, snapshot_id, &target, &token)?)
    }

    /// 校验快照引用的 CAS 对象是否完好
    ///
    /// # Errors
    ///
    /// 快照不存在、索引 / CAS 读取失败时返回 [`KitError`]；对象缺失或损坏
    /// 不报错，而是出现在 [`VerifyOutcome::broken`] 里
    pub fn verify_backup(&self, name: &str, snapshot_id: &str) -> Result<VerifyOutcome, KitError> {
        Ok(self
            .core()
            .backup
            .verify(name, snapshot_id, &mut |_, _| {})?)
    }

    /// 删除一份快照（引用计数归零的 CAS 对象留给 prune 回收）
    ///
    /// # Errors
    ///
    /// 快照不存在、索引写入失败或服务器位于不可达远端时返回 [`KitError`]
    pub fn delete_backup(&self, name: &str, snapshot_id: &str) -> Result<DeleteOutcome, KitError> {
        Ok(self.core().backup.delete(name, snapshot_id)?)
    }

    /// 按保留策略清理快照并回收无引用的 CAS 对象
    ///
    /// # Errors
    ///
    /// 保留策略冲突（同时给了 `keep` 与 `older_than`）、服务器不存在、索引
    /// 读写失败或对象删除失败时返回 [`KitError`]
    pub fn prune_backups(
        &self,
        name: &str,
        options: PruneBackups,
    ) -> Result<PruneOutcome, KitError> {
        let (retention, dry_run) = options.resolve();
        Ok(self.core().backup.prune(name, retention, dry_run)?)
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

    /// 分页参数缺省为第一页、默认条数
    #[test]
    fn list_defaults_to_first_page() {
        let page = ListBackups::default().resolve();
        assert_eq!(page.number, 1);
        assert_eq!(page.size, Page::DEFAULT_SIZE);
    }

    /// 分页非法值由 `Page::new` 夹紧
    #[test]
    fn list_clamps_invalid_pages() {
        let page = ListBackups::default().page(0).size(0).resolve();
        assert_eq!(page.number, 1);
        assert_eq!(page.size, 1);
    }

    /// 恢复缺省原地；显式指定新服务器时原样带上名字
    #[test]
    fn restore_defaults_to_in_place() {
        let (target, _run) = RestoreBackup::default().resolve();
        assert!(matches!(target, RestoreTarget::InPlace));

        let name = ServerName::parse("survival-copy").expect("name");
        let (target, _run) = RestoreBackup::default().to_new_server(name).resolve();
        assert!(matches!(target, RestoreTarget::NewServer { .. }));
    }

    /// 保留策略不抢先择一：两个都给时原样递交，由 service 判冲突
    #[test]
    fn prune_passes_conflicting_retention_to_service() {
        let (retention, dry_run) = PruneBackups::default()
            .keep(3)
            .older_than(Duration::from_secs(86_400))
            .dry_run(true)
            .resolve();
        assert_eq!(retention.keep, Some(3));
        assert_eq!(retention.older_than_secs, Some(86_400));
        assert!(dry_run);
    }
}
