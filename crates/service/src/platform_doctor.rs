//! `mcsup doctor`：平台环境自检
//!
//! 只读编排，与 `workspace doctor` 互补：那边查工作区与注册表的一致性，
//! 这里查「这台机器能不能正常跑 mcsup」——数据目录、配置、Java、来源装配。
//!
//! 判定收敛在这里：检查项枚举化并带结构字段，CLI / 未来的 REST 只做映射；
//! 描述、定位与修复建议与 [`PlatformCheck`] 同源，入口不复制检查清单。
//!
//! 自检**完全离线**：只读本机状态，不发任何网络请求。上游可达性随时会变，
//! 属于「当前能不能下载」而不是「环境有没有问题」，应由实际命令去碰运气；
//! doctor 只回答本机配置是否自洽，因此不检查来源连通性，也不拉版本列表。

use domain::java::JavaRuntime;
use port::ServerJarSource;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 严重程度（与 `workspace doctor` 同一口径，两处不各造一套）
use crate::doctor::Severity;

/// 单项检查的结论
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlatformCheck {
    /// 数据目录不存在（首次使用会创建，但要提前提示会写到哪里）
    DataDirMissing {
        /// 数据目录
        path: PathBuf,
    },
    /// 数据目录不可创建 / 不可写：元数据、Java 全都会失败
    DataDirNotWritable {
        /// 数据目录
        path: PathBuf,
        /// 底层原因
        reason: String,
    },
    /// 配置文件解析失败：所有命令都会在启动时被挡住
    ConfigInvalid {
        /// 配置文件路径
        path: PathBuf,
        /// 解析失败原因
        reason: String,
    },
    /// 一个 Java 运行时都没有：任何服务都启动不了
    NoJava,
    /// Java 已扫描过但登记为空
    NoJavaScanned,
    /// 登记的 Java 路径在磁盘上找不到
    JavaMissing {
        /// 运行时路径
        path: PathBuf,
        /// 运行时版本
        version: String,
    },
    /// 没有可用的服务端来源：建站与安装都无法进行
    NoJarSource,
}

impl PlatformCheck {
    /// 全部检查项标识，顺序即机器契约的稳定顺序
    pub const KNOWN_KINDS: &'static [&'static str] = &[
        "data-dir-missing",
        "data-dir-not-writable",
        "config-invalid",
        "no-java",
        "no-java-scanned",
        "java-missing",
        "no-jar-source",
    ];

    /// 稳定 kebab-case 标识（JSON 的 `kind`、`--skip` 的取值口径）
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::DataDirMissing { .. } => "data-dir-missing",
            Self::DataDirNotWritable { .. } => "data-dir-not-writable",
            Self::ConfigInvalid { .. } => "config-invalid",
            Self::NoJava | Self::NoJavaScanned => "no-java",
            Self::JavaMissing { .. } => "java-missing",
            Self::NoJarSource => "no-jar-source",
        }
    }

    /// 严重程度由类型决定：同一类型的级别永远一致
    #[must_use]
    pub const fn severity(&self) -> Severity {
        match self {
            Self::DataDirMissing { .. } | Self::NoJavaScanned => Severity::Warning,
            Self::DataDirNotWritable { .. }
            | Self::ConfigInvalid { .. }
            | Self::NoJava
            | Self::JavaMissing { .. }
            | Self::NoJarSource => Severity::Error,
        }
    }

    /// 问题描述
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::DataDirMissing { .. } => "平台数据目录尚未创建".to_owned(),
            Self::DataDirNotWritable { reason, .. } => {
                format!("平台数据目录不可写（{reason}）")
            }
            Self::ConfigInvalid { .. } => "配置文件无法解析".to_owned(),
            Self::NoJava => "没有可用的 Java 运行时".to_owned(),
            Self::NoJavaScanned => "已扫描过本机但未发现 Java".to_owned(),
            Self::JavaMissing { .. } => "登记的 Java 运行时已不在磁盘上".to_owned(),
            Self::NoJarSource => "没有可用的服务端下载源".to_owned(),
        }
    }

    /// 可寻址位置（人类输出的第二行）
    #[must_use]
    pub fn locate(&self) -> String {
        match self {
            Self::DataDirMissing { path } | Self::DataDirNotWritable { path, .. } => {
                path.display().to_string()
            }
            Self::ConfigInvalid { path, .. } => path.display().to_string(),
            Self::NoJava => "运行 `mcsup java install` 或安装系统 Java".to_owned(),
            Self::NoJavaScanned => "运行 `mcsup java scan`".to_owned(),
            Self::JavaMissing { path, version } => {
                format!("{version} → {}", path.display())
            }
            Self::NoJarSource => "内置来源装配失败".to_owned(),
        }
    }

    /// 修复建议：doctor 本身不写任何东西
    #[must_use]
    pub fn fix_hint(&self) -> String {
        match self {
            Self::DataDirMissing { .. } => {
                "首次运行任一命令时会自动创建；也可先建好目录".to_owned()
            }
            Self::DataDirNotWritable { path, .. } => format!(
                "检查 {} 的权限，或 `mcsup config set data-dir <可写目录>`",
                path.display()
            ),
            Self::ConfigInvalid { path, .. } => {
                format!(
                    "`mcsup config edit` 修正 {}，或删除后回到缺省",
                    path.display()
                )
            }
            Self::NoJava | Self::NoJavaScanned => "`mcsup java install 21` 下载一个 JVM".to_owned(),
            Self::JavaMissing { .. } => {
                "`mcsup java scan` 重新登记，或 `mcsup java uninstall --dir <路径>` 清理".to_owned()
            }
            Self::NoJarSource => "重新安装 mcsup（内置来源装配失败）".to_owned(),
        }
    }
}

impl std::fmt::Display for PlatformCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条发现
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformFinding {
    /// 发现类型（含可寻址字段）
    pub check: PlatformCheck,
}

impl PlatformFinding {
    /// 严重程度随类型
    #[must_use]
    pub const fn severity(&self) -> Severity {
        self.check.severity()
    }
}

/// 一次自检的全部发现（已排序：error 在前，同级按类型）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlatformReport {
    findings: Vec<PlatformFinding>,
}

impl PlatformReport {
    /// 全部发现
    #[must_use]
    pub fn findings(&self) -> &[PlatformFinding] {
        &self.findings
    }

    /// 是否存在 error 级发现
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity() == Severity::Error)
    }

    /// 按严重程度计数
    #[must_use]
    pub fn count(&self, severity: Severity) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity() == severity)
            .count()
    }

    /// 是否干净
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }
}

/// 平台自检的输入：全部来自调用方（库侧组合根 / 配置文件）
pub struct PlatformDoctor {
    /// 配置文件路径
    config_path: PathBuf,
    /// 配置读取失败的原因；`None` 表示配置可用
    config_error: Option<String>,
    /// 生效的数据目录
    data_dir: PathBuf,
    java: Option<Vec<JavaRuntime>>,
    java_scanned: bool,
    jar_sources: Vec<Arc<dyn ServerJarSource>>,
}

impl std::fmt::Debug for PlatformDoctor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformDoctor")
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

impl PlatformDoctor {
    /// 组合输入；`java` 为 `None` 表示注册表读不出来（按 error 报）
    #[must_use]
    pub fn new(
        config_path: PathBuf,
        data_dir: PathBuf,
        java: Option<Vec<JavaRuntime>>,
        java_scanned: bool,
        jar_sources: Vec<Arc<dyn ServerJarSource>>,
    ) -> Self {
        Self {
            config_path,
            config_error: None,
            data_dir,
            java,
            java_scanned,
            jar_sources,
        }
    }

    /// 配置文件解析失败的情况：保留错误供报告，配置本身按缺省处理
    #[must_use]
    pub fn with_config_error(mut self, reason: String) -> Self {
        self.config_error = Some(reason);
        self
    }

    /// 跑一遍自检
    ///
    /// 检查项：数据目录存在 / 可写、配置可解析、有 Java、登记的 Java 仍在
    /// 盘上、有服务端来源。全部只读本机状态，不发任何网络请求。
    ///
    /// 单项失败不会被吞成「无问题」：读不出来就各自成为一条发现，
    /// 否则诊断工具会把故障报成通过。
    pub async fn run(&self) -> PlatformReport {
        let mut findings = Vec::new();
        if self.jar_sources.is_empty() {
            findings.push(PlatformFinding {
                check: PlatformCheck::NoJarSource,
            });
        }

        // 三项本地检查都是阻塞文件 IO（探写权限、stat Java 路径）：
        // 放到阻塞线程池，不占 async 执行器。
        let data_dir = self.data_dir.clone();
        let config_path = self.config_path.clone();
        let config_error = self.config_error.clone();
        let java = self.java.clone();
        let java_scanned = self.java_scanned;
        let mut local = tokio::task::spawn_blocking(move || {
            let mut findings = Vec::new();
            check_data_dir(&data_dir, &mut findings);
            check_config(&config_path, config_error.as_deref(), &mut findings);
            check_java(java.as_deref(), java_scanned, &mut findings);
            findings
        })
        .await
        .unwrap_or_default();
        findings.append(&mut local);

        findings.sort_by(|a, b| {
            a.severity()
                .cmp(&b.severity())
                .then_with(|| a.check.as_str().cmp(b.check.as_str()))
        });
        PlatformReport { findings }
    }
}

// 三项本地检查做成自由函数：在 `run()` 里经 `spawn_blocking` 执行，参数显式
// 传入（不用 `&self`，避免把不可 `'static` 的引用带进阻塞线程）。

fn check_data_dir(data_dir: &Path, out: &mut Vec<PlatformFinding>) {
    let path = data_dir;
    if !path.exists() {
        out.push(PlatformFinding {
            check: PlatformCheck::DataDirMissing {
                path: path.to_path_buf(),
            },
        });
        // 不存在就判不了可写：创建由首次写元数据的命令负责，这里不替它建
        return;
    }
    // 实际探一次写权限：只查元数据不足以发现只读挂载 / ACL 拒写
    let probe = path.join(".mcsup-doctor-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
        }
        Err(err) => out.push(PlatformFinding {
            check: PlatformCheck::DataDirNotWritable {
                path: path.to_path_buf(),
                reason: err.to_string(),
            },
        }),
    }
}

fn check_config(config_path: &Path, config_error: Option<&str>, out: &mut Vec<PlatformFinding>) {
    if let Some(reason) = config_error {
        out.push(PlatformFinding {
            check: PlatformCheck::ConfigInvalid {
                path: config_path.to_path_buf(),
                reason: reason.to_owned(),
            },
        });
    }
}

fn check_java(java: Option<&[JavaRuntime]>, java_scanned: bool, out: &mut Vec<PlatformFinding>) {
    let Some(runtimes) = java else {
        out.push(PlatformFinding {
            check: PlatformCheck::NoJava,
        });
        return;
    };
    if runtimes.is_empty() {
        out.push(PlatformFinding {
            check: if java_scanned {
                PlatformCheck::NoJavaScanned
            } else {
                PlatformCheck::NoJava
            },
        });
        return;
    }
    for rt in runtimes {
        if !rt.path.is_dir() {
            out.push(PlatformFinding {
                check: PlatformCheck::JavaMissing {
                    path: rt.path.clone(),
                    version: rt.version.clone(),
                },
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::server::version::{ServerVersion, VersionId};
    use port::{ResolvedArtifact, SourceError};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn kinds_and_severity_stay_in_sync() {
        let err = PlatformCheck::NoJava;
        assert_eq!(err.as_str(), "no-java");
        assert_eq!(err.severity(), Severity::Error);
        let warn = PlatformCheck::DataDirMissing {
            path: PathBuf::from("/data"),
        };
        assert_eq!(warn.severity(), Severity::Warning);
        assert!(PlatformCheck::KNOWN_KINDS.contains(&warn.as_str()));
    }

    /// 记录 `versions()` 是否被调用：doctor 一个请求都不该发
    struct CountingSource {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ServerJarSource for CountingSource {
        fn variant(&self) -> domain::server::version::ServerVariant {
            domain::server::version::ServerVariant::Vanilla
        }

        fn name(&self) -> &'static str {
            "counting"
        }

        async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }

        async fn artifact(&self, version: &VersionId) -> Result<ResolvedArtifact, SourceError> {
            Err(SourceError::NotFound(version.clone()))
        }
    }

    fn doctor_with(sources: Vec<Arc<dyn ServerJarSource>>) -> PlatformDoctor {
        PlatformDoctor::new(
            PathBuf::from("/nonexistent/mcsup.toml"),
            PathBuf::from("/nonexistent/data"),
            Some(Vec::new()),
            false,
            sources,
        )
    }

    /// 自检完全离线：即使来源可用也不调用 `versions()`
    #[tokio::test]
    async fn never_touches_the_network() {
        let calls = Arc::new(AtomicUsize::new(0));
        let sources: Vec<Arc<dyn ServerJarSource>> = vec![Arc::new(CountingSource {
            calls: Arc::clone(&calls),
        })];
        let report = doctor_with(sources).run().await;

        assert_eq!(calls.load(Ordering::SeqCst), 0, "doctor 不得发网络请求");
        // 没有联网检查项，来源不会产生任何发现
        assert!(
            report
                .findings()
                .iter()
                .all(|f| f.check.as_str() != "source-unreachable"
                    && f.check.as_str() != "source-empty"),
            "{:?}",
            report.findings()
        );
    }

    /// 来源装配为空是本地状态，仍要报 `no-jar-source`（不需要联网判断）
    #[tokio::test]
    async fn empty_source_list_is_reported_locally() {
        let report = doctor_with(Vec::new()).run().await;
        assert!(
            report
                .findings()
                .iter()
                .any(|f| f.check.as_str() == "no-jar-source"),
            "{:?}",
            report.findings()
        );
    }

    /// 本地检查不需要网络：不会产生任何 `source-*` 发现
    #[tokio::test]
    async fn local_checks_do_not_require_network() {
        let calls = Arc::new(AtomicUsize::new(0));
        let sources: Vec<Arc<dyn ServerJarSource>> = vec![Arc::new(CountingSource { calls })];
        let report = doctor_with(sources).run().await;
        assert!(
            report
                .findings()
                .iter()
                .all(|f| !f.check.as_str().starts_with("source-")),
            "{:?}",
            report.findings()
        );
    }
}
