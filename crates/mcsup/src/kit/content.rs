//! 内容任务：装一个 mod、更新已装的 mod、移除文件
//!
//! 两阶段语义不拆：先 `plan_install` 拿到具体版本与落点，再 `install` 下载、
//! 暂存、检查、原子换入。`dry_run` 只停在计划阶段，返回的计划本身就足以展示
//! 「要装哪个版本、落到哪个文件、会替换什么」。
//!
//! 更新同理：`plan_update` 逐项确认「有没有新版本」，`apply_update` 才落盘。
//! 任一项失败时 `service` 返回 `UpdateAborted`（含已完成项数），kit 不吞掉
//! 这个中间状态。

use super::config::KitConfig;
use super::error::KitError;
use super::options::RunOptions;
use domain::server::content::ContentKind;
use port::ProgressSink;
use service::{
    ApplyOptions, InstallPlan, InstallSpec, ListOptions, ModInstalled, ModRemoved, UpdateApplied,
    UpdatePlan,
};

/// 安装内容的单次覆盖
///
/// `project` 是来源（默认 Modrinth）里的项目标识或别名，由
/// [`Content::install`](crate::kit::Content::install) 的参数给出，不在本类型里。
#[derive(Clone, Debug, Default)]
pub struct InstallMod {
    version: Option<String>,
    source: Option<String>,
    kind: Option<ContentKind>,
    allow_prerelease: Option<bool>,
    force: Option<bool>,
    dry_run: Option<bool>,
    run: RunOptions,
}

impl InstallMod {
    /// 精确版本 ID；缺省按服务端条件挑最优候选
    #[must_use]
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// 来源名（如 `modrinth`）；缺省用内容服务注册的默认来源
    #[must_use]
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// 内容类别；缺省用 [`KitConfig::content_kind`]
    #[must_use]
    pub fn kind(mut self, kind: ContentKind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// 允许测试版 / 内测版
    #[must_use]
    pub fn allow_prerelease(mut self, allow: bool) -> Self {
        self.allow_prerelease = Some(allow);
        self
    }

    /// 检查判定不可用于服务端时仍然落盘（风险与依据照样返回）
    #[must_use]
    pub fn force(mut self, force: bool) -> Self {
        self.force = Some(force);
        self
    }

    /// 只出计划、不写盘
    #[must_use]
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = Some(dry_run);
        self
    }

    /// 进度观察者与取消令牌
    #[must_use]
    pub fn run(mut self, run: RunOptions) -> Self {
        self.run = run;
        self
    }

    /// 与默认策略合并（纯计算，不碰 IO）
    fn resolve(&self, config: &KitConfig) -> ResolvedMod<'_> {
        ResolvedMod {
            version: self.version.as_deref(),
            source: self.source.as_deref(),
            kind: self.kind.unwrap_or(config.content_kind),
            allow_prerelease: self.allow_prerelease.unwrap_or(false),
            force: self.force.unwrap_or(false),
            dry_run: self.dry_run.unwrap_or(false),
        }
    }
}

/// 合并后的安装参数
struct ResolvedMod<'a> {
    version: Option<&'a str>,
    source: Option<&'a str>,
    kind: ContentKind,
    allow_prerelease: bool,
    force: bool,
    dry_run: bool,
}

/// 安装结果
///
/// `applied` 为 `None` 表示 `dry_run`：计划已给出，磁盘没有变化。
#[derive(Debug)]
#[non_exhaustive]
pub struct InstallModOutcome {
    /// 计划：具体版本、落点与是否替换
    pub plan: InstallPlan,
    /// 实际落盘结果；`dry_run` 时为 `None`
    pub applied: Option<ModInstalled>,
}

/// 移除内容的单次覆盖
///
/// 选择器先按文件名精确匹配，再按 modid 匹配；命中多个时 `service` 拒绝猜测，
/// 把候选列出来。
#[derive(Clone, Copy, Debug, Default)]
pub struct RemoveMod {
    kind: Option<ContentKind>,
    dry_run: Option<bool>,
}

impl RemoveMod {
    /// 内容类别；缺省用 [`KitConfig::content_kind`]
    #[must_use]
    pub fn kind(mut self, kind: ContentKind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// 只列出会删哪些文件
    #[must_use]
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = Some(dry_run);
        self
    }

    fn resolve(self, config: &KitConfig) -> (ContentKind, bool) {
        (
            self.kind.unwrap_or(config.content_kind),
            self.dry_run.unwrap_or(false),
        )
    }
}

/// 更新已装内容的单次覆盖
///
/// 不写 `selector` 时检查全部；写了只在匹配的文件里挑。`dry_run` 只出计划。
#[derive(Clone, Debug, Default)]
pub struct UpdateMods {
    selector: Option<String>,
    source: Option<String>,
    kind: Option<ContentKind>,
    force: Option<bool>,
    dry_run: Option<bool>,
    run: RunOptions,
}

impl UpdateMods {
    /// 只更新匹配该选择器的文件（文件名或 modid）
    #[must_use]
    pub fn selector(mut self, selector: impl Into<String>) -> Self {
        self.selector = Some(selector.into());
        self
    }

    /// 识别与查询更新用的来源名；缺省用默认来源
    #[must_use]
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// 内容类别；缺省用 [`KitConfig::content_kind`]
    #[must_use]
    pub fn kind(mut self, kind: ContentKind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// 检查判定不可用于服务端时仍然落盘
    #[must_use]
    pub fn force(mut self, force: bool) -> Self {
        self.force = Some(force);
        self
    }

    /// 只出计划、不写盘
    #[must_use]
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = Some(dry_run);
        self
    }

    /// 进度观察者与取消令牌
    #[must_use]
    pub fn run(mut self, run: RunOptions) -> Self {
        self.run = run;
        self
    }

    /// 与默认策略合并（纯计算，不碰 IO）
    fn resolve(&self, config: &KitConfig) -> ResolvedUpdate<'_> {
        ResolvedUpdate {
            selector: self.selector.as_deref(),
            source: self.source.as_deref(),
            kind: self.kind.unwrap_or(config.content_kind),
            force: self.force.unwrap_or(false),
            dry_run: self.dry_run.unwrap_or(false),
        }
    }
}

/// 合并后的更新参数
struct ResolvedUpdate<'a> {
    selector: Option<&'a str>,
    source: Option<&'a str>,
    kind: ContentKind,
    force: bool,
    dry_run: bool,
}

/// 更新结果
///
/// `applied` 为 `None` 表示 `dry_run`：计划已给出，磁盘没有变化。
#[derive(Debug)]
#[non_exhaustive]
pub struct UpdateModsOutcome {
    /// 计划：每项的原版本 / 目标版本与落点
    pub plan: UpdatePlan,
    /// 实际落盘结果；`dry_run` 时为 `None`
    pub applied: Option<UpdateApplied>,
}

/// 内容任务句柄
///
/// 由 [`Kit::content`](crate::kit::Kit::content) 取得。
#[derive(Clone, Copy, Debug)]
pub struct Content<'a> {
    core: &'a crate::bootstrap::Mcsup,
    config: &'a KitConfig,
}

impl<'a> Content<'a> {
    pub(super) fn new(core: &'a crate::bootstrap::Mcsup, config: &'a KitConfig) -> Self {
        Self { core, config }
    }
}

impl Content<'_> {
    /// 一行装一个 mod / 插件
    ///
    /// 服务端运行中会被拒绝（加载器正在读这些文件）；判定来自 `Backend` 的
    /// 实时状态，与 `server start` 同源，kit 不另做猜测。
    ///
    /// # Errors
    ///
    /// 服务器不存在 / 不在本机 / 正在运行、没有可安装版本、下载或检查失败时
    /// 返回 [`KitError`]
    pub async fn install(
        &self,
        name: &str,
        project: &str,
        options: InstallMod,
    ) -> Result<InstallModOutcome, KitError> {
        let resolved = options.resolve(self.config);
        let observer = options.run.observer_ref();
        let token = options.run.token();

        let plan = self
            .core
            .mods
            .plan_install(
                name,
                project,
                resolved.kind,
                InstallSpec {
                    version: resolved.version,
                    source: resolved.source,
                    allow_prerelease: resolved.allow_prerelease,
                },
            )
            .await?;
        if resolved.dry_run {
            return Ok(InstallModOutcome {
                plan,
                applied: None,
            });
        }

        let applied = self
            .core
            .mods
            .install(
                name,
                &plan,
                ApplyOptions {
                    allow_blocking: resolved.force,
                    progress: observer.map(|observer| observer as &dyn ProgressSink),
                    cancel: &token,
                },
            )
            .await?;
        Ok(InstallModOutcome {
            plan,
            applied: Some(applied),
        })
    }

    /// 检查并更新已装内容
    ///
    /// 先 `plan_update` 逐项问上游，再 `apply_update` 落盘。`dry_run` 只出计划：
    /// 适合在入口里先展示「哪些会升级」再让用户确认。
    ///
    /// 更新中途失败时 `service` 返回 `UpdateAborted`，错误里带着「停在哪一项、
    /// 已完成几项」——前面几项已经落盘，kit 不会谎报「什么都没变」。
    ///
    /// # Errors
    ///
    /// 服务器不存在 / 不在本机 / 正在运行、上游查询失败、下载或检查失败，
    /// 或更新中途失败时返回 [`KitError`]
    pub async fn update(
        &self,
        name: &str,
        options: UpdateMods,
    ) -> Result<UpdateModsOutcome, KitError> {
        let resolved = options.resolve(self.config);
        let observer = options.run.observer_ref();
        let token = options.run.token();

        let plan = self
            .core
            .mods
            .plan_update(
                name,
                resolved.selector,
                ListOptions {
                    kind: resolved.kind,
                    identify: true,
                    source: resolved.source,
                    // 计划阶段必须带更新判定，否则拿到的全是「未知」
                    updates: true,
                },
            )
            .await?;
        if resolved.dry_run {
            return Ok(UpdateModsOutcome {
                plan,
                applied: None,
            });
        }

        let applied = self
            .core
            .mods
            .apply_update(
                name,
                &plan,
                ApplyOptions {
                    allow_blocking: resolved.force,
                    progress: observer.map(|observer| observer as &dyn ProgressSink),
                    cancel: &token,
                },
            )
            .await?;
        Ok(UpdateModsOutcome {
            plan,
            applied: Some(applied),
        })
    }

    /// 移除一个 mod / 插件文件
    ///
    /// `dry_run` 只列会删哪些文件；真正删除要求服务端已停机（模型层判定）。
    /// 移除表达「不要了」；「先留着但别加载」用 `ModService::disable`（经
    /// [`Kit::core`](crate::kit::Kit::core)）。
    ///
    /// # Errors
    ///
    /// 服务器不存在 / 不在本机 / 正在运行、选择器未命中或命中多个、删除失败
    /// 时返回 [`KitError`]
    pub fn remove(
        &self,
        name: &str,
        selector: &str,
        options: RemoveMod,
    ) -> Result<ModRemoved, KitError> {
        let (kind, dry_run) = options.resolve(self.config);
        Ok(self.core.mods.remove(name, selector, kind, dry_run)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 类别与开关默认走配置 / 关闭，版本与来源默认交给服务端条件
    #[test]
    fn install_defaults_come_from_config() {
        let config = KitConfig {
            content_kind: ContentKind::Plugin,
            ..KitConfig::default()
        };
        let options = InstallMod::default();
        let resolved = options.resolve(&config);
        assert_eq!(resolved.kind, ContentKind::Plugin);
        assert!(resolved.version.is_none());
        assert!(resolved.source.is_none());
        assert!(!resolved.allow_prerelease);
        assert!(!resolved.force);
        assert!(!resolved.dry_run);
    }

    /// 单次设置覆盖配置
    #[test]
    fn install_call_overrides_config() {
        let options = InstallMod::default()
            .kind(ContentKind::Mod)
            .version("1.0.0")
            .source("modrinth")
            .allow_prerelease(true)
            .force(true)
            .dry_run(true);
        let resolved = options.resolve(&KitConfig::default());
        assert_eq!(resolved.kind, ContentKind::Mod);
        assert_eq!(resolved.version, Some("1.0.0"));
        assert_eq!(resolved.source, Some("modrinth"));
        assert!(resolved.allow_prerelease);
        assert!(resolved.force);
        assert!(resolved.dry_run);
    }

    /// 移除沿用配置里的类别；预览开关默认关闭
    #[test]
    fn remove_defaults_come_from_config() {
        let config = KitConfig {
            content_kind: ContentKind::Plugin,
            ..KitConfig::default()
        };
        let (kind, dry_run) = RemoveMod::default().resolve(&config);
        assert_eq!(kind, ContentKind::Plugin);
        assert!(!dry_run);
    }

    /// 更新不写选择器 = 全部；默认来源交给服务层
    #[test]
    fn update_defaults_are_all_files() {
        let options = UpdateMods::default();
        let resolved = options.resolve(&KitConfig::default());
        assert!(resolved.selector.is_none());
        assert!(resolved.source.is_none());
        assert_eq!(resolved.kind, ContentKind::Mod);
        assert!(!resolved.dry_run);
        assert!(!resolved.force);
    }

    /// 单次设置覆盖更新的默认值
    #[test]
    fn update_call_overrides_config() {
        let options = UpdateMods::default()
            .selector("fabric-api")
            .source("modrinth")
            .kind(ContentKind::Plugin)
            .force(true)
            .dry_run(true);
        let resolved = options.resolve(&KitConfig::default());
        assert_eq!(resolved.selector, Some("fabric-api"));
        assert_eq!(resolved.source, Some("modrinth"));
        assert!(resolved.force);
        assert!(resolved.dry_run);
    }
}
