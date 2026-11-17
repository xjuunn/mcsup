//! 内容安装任务：一行装一个 mod / 插件
//!
//! 两阶段语义不拆：先 `plan_install` 拿到具体版本与落点，再 `install` 下载、
//! 暂存、检查、原子换入。`dry_run` 只停在第一阶段，返回的计划本身就足以展示
//! 「要装哪个版本、落到哪个文件、会替换什么」。

use super::Kit;
use super::config::KitConfig;
use super::error::KitError;
use super::options::RunOptions;
use domain::server::content::ContentKind;
use port::ProgressSink;
use service::{ApplyOptions, InstallPlan, InstallSpec, ModInstalled};

/// 安装内容的单次覆盖
///
/// `project` 是来源（默认 Modrinth）里的项目标识或别名，由
/// [`Kit::install_mod`] 的参数给出，不在本类型里。
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

impl Kit<'_> {
    /// 一行装一个 mod / 插件
    ///
    /// 服务端运行中会被拒绝（加载器正在读这些文件）；判定来自 `Backend` 的
    /// 实时状态，与 `server start` 同源，kit 不另做猜测。
    ///
    /// # Errors
    ///
    /// 服务器不存在 / 不在本机 / 正在运行、没有可安装版本、下载或检查失败时
    /// 返回 [`KitError`]
    pub async fn install_mod(
        &self,
        name: &str,
        project: &str,
        options: InstallMod,
    ) -> Result<InstallModOutcome, KitError> {
        let resolved = options.resolve(self.config());
        let observer = options.run.observer_ref();
        let token = options.run.token();

        let plan = self
            .core()
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
            .core()
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
}
