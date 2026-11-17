//! Java 任务：检测本机运行时、安装托管运行时
//!
//! 扫描与「装哪个包」的判定都在 `service::JavaService`：来源排序、平台匹配、
//! 解压后验 JVM home、注册都由它完成。kit 只补默认值与调用顺序。

use super::config::KitConfig;
use super::error::KitError;
use super::options::RunOptions;
use domain::java::{JavaMajor, JavaPackageType, JavaRuntime, JavaVendor};
use service::InstallOutcome;

/// 安装 Java 的单次覆盖
///
/// 大版本必须能从单次调用或 [`KitConfig::java_major`] 得到；都不给时返回
/// [`KitError::MissingJavaMajor`]，不猜一个默认版本。
///
/// ```no_run
/// use mcsup::domain::java::JavaMajor;
/// use mcsup::kit::InstallJava;
///
/// # fn opts() -> Result<InstallJava, Box<dyn std::error::Error>> {
/// // 装 21（JDK，来源自选）
/// let opts = InstallJava::default().major(JavaMajor::parse(21)?);
/// # Ok(opts)
/// # }
/// ```
#[derive(Clone, Debug, Default)]
pub struct InstallJava {
    major: Option<JavaMajor>,
    package_type: Option<JavaPackageType>,
    vendor: Option<JavaVendor>,
    run: RunOptions,
}

impl InstallJava {
    /// 目标大版本；外部输入先用 [`JavaMajor::parse`] 校验
    #[must_use]
    pub fn major(mut self, major: JavaMajor) -> Self {
        self.major = Some(major);
        self
    }

    /// 包类型；缺省用 [`KitConfig::java_package_type`]（JDK）
    #[must_use]
    pub fn package_type(mut self, package_type: JavaPackageType) -> Self {
        self.package_type = Some(package_type);
        self
    }

    /// 发行商；缺省用 [`KitConfig::java_vendor`]，再缺省由来源排序决定
    #[must_use]
    pub fn vendor(mut self, vendor: JavaVendor) -> Self {
        self.vendor = Some(vendor);
        self
    }

    /// 进度观察者与取消令牌
    #[must_use]
    pub fn run(mut self, run: RunOptions) -> Self {
        self.run = run;
        self
    }

    /// 与默认策略合并（纯计算，不碰 IO）
    fn resolve(self, config: &KitConfig) -> Result<ResolvedJava, KitError> {
        let major = self
            .major
            .or(config.java_major)
            .ok_or(KitError::MissingJavaMajor)?;
        Ok(ResolvedJava {
            major,
            package_type: self.package_type.unwrap_or(config.java_package_type),
            vendor: self.vendor.or(config.java_vendor),
            run: self.run,
        })
    }
}

/// 合并后的安装参数
#[derive(Debug)]
struct ResolvedJava {
    major: JavaMajor,
    package_type: JavaPackageType,
    vendor: Option<JavaVendor>,
    run: RunOptions,
}

/// Java 任务句柄
///
/// 由 [`Kit::java`](crate::kit::Kit::java) 取得。领域模型仍以
/// `domain::java` 的类型为准（[`JavaMajor`]、[`JavaVendor`] 等），本句柄只
/// 负责调用顺序与默认值。
#[derive(Clone, Copy, Debug)]
pub struct Java<'a> {
    core: &'a crate::bootstrap::Mcsup,
    config: &'a KitConfig,
}

impl<'a> Java<'a> {
    pub(super) fn new(core: &'a crate::bootstrap::Mcsup, config: &'a KitConfig) -> Self {
        Self { core, config }
    }
}

impl Java<'_> {
    /// 一行装 Java 运行时
    ///
    /// 下载 → 解压 → 验出 JVM home → 注册，全程走 `JavaService`；宿主用
    /// `McsupBuilder::java_registry` / `extractor` / `downloader` 换实现。
    ///
    /// # Errors
    ///
    /// 没有大版本（用法类），或来源不可达、下载 / 解压 / 注册失败时返回
    /// [`KitError`]
    pub async fn install(&self, options: InstallJava) -> Result<InstallOutcome, KitError> {
        let resolved = options.resolve(self.config)?;
        Ok(self
            .core
            .java
            .install(
                resolved.major,
                resolved.package_type,
                resolved.vendor,
                resolved.run.observer_ref(),
                &resolved.run.token(),
            )
            .await?)
    }

    /// 列出本机可用 Java（没扫描过时先扫一遍）
    ///
    /// 扫描可能耗时数秒（要看每个候选目录的 `java -version`），入口应配进度
    /// 回显；异步宿主请自行 `spawn_blocking`。
    ///
    /// # Errors
    ///
    /// 扫描或运行时注册表读取失败时返回 [`KitError`]
    pub fn runtimes(&self) -> Result<Vec<JavaRuntime>, KitError> {
        Ok(self.core.java.list_or_scan(&mut || {}, &mut |_| {})?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不给大版本、配置里也没有：用法错误，不猜
    #[test]
    fn install_java_requires_major() {
        let err = InstallJava::default()
            .resolve(&KitConfig::default())
            .expect_err("缺大版本");
        assert!(matches!(err, KitError::MissingJavaMajor));
        assert_eq!(err.category(), port::ErrorCategory::Usage);
    }

    /// 单次给的大版本优先于配置
    #[test]
    fn install_java_call_overrides_config() {
        let config = KitConfig {
            java_major: Some(JavaMajor::parse(17).expect("17")),
            ..KitConfig::default()
        };
        let resolved = InstallJava::default()
            .major(JavaMajor::parse(21).expect("21"))
            .package_type(JavaPackageType::Jre)
            .resolve(&config)
            .expect("resolve");
        assert_eq!(resolved.major, JavaMajor::parse(21).expect("21"));
        assert_eq!(resolved.package_type, JavaPackageType::Jre);
        assert!(resolved.vendor.is_none());
    }

    /// 没单次设置时用配置里的包类型与发行商
    #[test]
    fn install_java_defaults_come_from_config() {
        let config = KitConfig {
            java_major: Some(JavaMajor::parse(21).expect("21")),
            java_package_type: JavaPackageType::Jre,
            java_vendor: Some(JavaVendor::Temurin),
            ..KitConfig::default()
        };
        let resolved = InstallJava::default().resolve(&config).expect("resolve");
        assert_eq!(resolved.major, JavaMajor::parse(21).expect("21"));
        assert_eq!(resolved.package_type, JavaPackageType::Jre);
        assert_eq!(resolved.vendor, Some(JavaVendor::Temurin));
    }
}
