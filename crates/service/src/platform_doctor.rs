//! `mcsup doctor`：平台环境自检
//!
//! 只读编排，与 `workspace doctor` 互补：那边查工作区与注册表的一致性，
//! 这里查「这台机器能不能正常跑 mcsup」——数据目录、Java、下载源、
//! 远端执行位置。
//!
//! 判定收敛在这里：检查项枚举化并带结构字段，CLI / 未来的 REST 只做映射；
//! 描述、定位与修复建议与 [`PlatformCheck`] 同源，入口不复制检查清单。
//!
//! 与 `workspace doctor` 的区别是要打网络：来源连通性必须实际请求一次，
//! 否则「能配通」与「能下载」会分叉。网络检查由调用方决定是否启用
//! （[`PlatformDoctor::offline`]），离线时该组直接跳过，不伪造通过。

use crate::platform_config::{Config, ConfigKey};
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
    /// 配置里引用的来源不存在
    ConfigSourceUnknown {
        /// 键名
        key: &'static str,
        /// 记录的值
        value: String,
        /// 当前可用取值
        choices: Vec<String>,
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
    /// 服务端来源不可达（离线时不出此项）
    SourceUnreachable {
        /// 来源名
        source: &'static str,
        /// 底层原因
        reason: String,
    },
    /// 服务端来源返回空版本列表
    SourceEmpty {
        /// 来源名
        source: &'static str,
    },
}

impl PlatformCheck {
    /// 全部检查项标识，顺序即机器契约的稳定顺序
    pub const KNOWN_KINDS: &'static [&'static str] = &[
        "data-dir-missing",
        "data-dir-not-writable",
        "config-invalid",
        "config-source-unknown",
        "no-java",
        "no-java-scanned",
        "java-missing",
        "no-jar-source",
        "source-unreachable",
        "source-empty",
    ];

    /// 稳定 kebab-case 标识（JSON 的 `kind`、`--skip` 的取值口径）
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::DataDirMissing { .. } => "data-dir-missing",
            Self::DataDirNotWritable { .. } => "data-dir-not-writable",
            Self::ConfigInvalid { .. } => "config-invalid",
            Self::ConfigSourceUnknown { .. } => "config-source-unknown",
            Self::NoJava | Self::NoJavaScanned => "no-java",
            Self::JavaMissing { .. } => "java-missing",
            Self::NoJarSource => "no-jar-source",
            Self::SourceUnreachable { .. } => "source-unreachable",
            Self::SourceEmpty { .. } => "source-empty",
        }
    }

    /// 严重程度由类型决定：同一类型的级别永远一致
    #[must_use]
    pub const fn severity(&self) -> Severity {
        match self {
            Self::DataDirMissing { .. } | Self::NoJavaScanned | Self::SourceEmpty { .. } => {
                Severity::Warning
            }
            Self::DataDirNotWritable { .. }
            | Self::ConfigInvalid { .. }
            | Self::ConfigSourceUnknown { .. }
            | Self::NoJava
            | Self::JavaMissing { .. }
            | Self::NoJarSource
            | Self::SourceUnreachable { .. } => Severity::Error,
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
            Self::ConfigSourceUnknown { key, value, .. } => {
                format!("配置项 {key} 的取值 {value} 不存在")
            }
            Self::NoJava => "没有可用的 Java 运行时".to_owned(),
            Self::NoJavaScanned => "已扫描过本机但未发现 Java".to_owned(),
            Self::JavaMissing { .. } => "登记的 Java 运行时已不在磁盘上".to_owned(),
            Self::NoJarSource => "没有可用的服务端下载源".to_owned(),
            Self::SourceUnreachable { source, reason } => {
                format!("服务端来源 {source} 不可达（{reason}）")
            }
            Self::SourceEmpty { source } => {
                format!("服务端来源 {source} 返回了空版本列表")
            }
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
            Self::ConfigSourceUnknown {
                key,
                value,
                choices,
            } => {
                format!("{key} = {value}（可用: {}）", choices.join(", "))
            }
            Self::NoJava => "运行 `mcsup java install` 或安装系统 Java".to_owned(),
            Self::NoJavaScanned => "运行 `mcsup java scan`".to_owned(),
            Self::JavaMissing { path, version } => {
                format!("{version} → {}", path.display())
            }
            Self::NoJarSource => "内置来源装配失败".to_owned(),
            Self::SourceUnreachable { source, .. } | Self::SourceEmpty { source } => {
                (*source).to_owned()
            }
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
            Self::ConfigSourceUnknown { key, .. } => {
                format!("`mcsup config set {key} <可用取值>`，或 `mcsup config unset {key}`")
            }
            Self::NoJava | Self::NoJavaScanned => "`mcsup java install 21` 下载一个 JVM".to_owned(),
            Self::JavaMissing { .. } => {
                "`mcsup java scan` 重新登记，或 `mcsup java uninstall --dir <路径>` 清理".to_owned()
            }
            Self::NoJarSource => "重新安装 mcsup（内置来源装配失败）".to_owned(),
            Self::SourceUnreachable { .. } => {
                "检查网络 / 代理后重试；离线时可用 `--offline` 跳过联网检查".to_owned()
            }
            Self::SourceEmpty { .. } => "稍后重试；持续为空时检查上游服务状态".to_owned(),
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
    /// 是否跑了联网检查
    online: bool,
}

impl PlatformReport {
    /// 全部发现
    #[must_use]
    pub fn findings(&self) -> &[PlatformFinding] {
        &self.findings
    }

    /// 是否跑了联网检查（人类输出据此说明「已跳过」而不是伪装成通过）
    #[must_use]
    pub const fn is_online(&self) -> bool {
        self.online
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
    config: Config,
    /// 配置文件路径
    config_path: PathBuf,
    /// 配置读取失败的原因；`None` 表示配置可用
    config_error: Option<String>,
    /// 生效的数据目录
    data_dir: PathBuf,
    java: Option<Vec<JavaRuntime>>,
    java_scanned: bool,
    jar_sources: Vec<Arc<dyn ServerJarSource>>,
    available_sources: Vec<&'static str>,
    online: bool,
}

impl std::fmt::Debug for PlatformDoctor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformDoctor")
            .field("data_dir", &self.data_dir)
            .field("online", &self.online)
            .finish_non_exhaustive()
    }
}

impl PlatformDoctor {
    /// 组合输入；`java` 为 `None` 表示注册表读不出来（按 error 报）
    #[must_use]
    pub fn new(
        config: Config,
        config_path: PathBuf,
        data_dir: PathBuf,
        java: Option<Vec<JavaRuntime>>,
        java_scanned: bool,
        jar_sources: Vec<Arc<dyn ServerJarSource>>,
        available_sources: Vec<&'static str>,
    ) -> Self {
        Self {
            config,
            config_path,
            config_error: None,
            data_dir,
            java,
            java_scanned,
            jar_sources,
            available_sources,
            online: true,
        }
    }

    /// 配置文件解析失败的情况：保留错误供报告，配置本身按缺省处理
    #[must_use]
    pub fn with_config_error(mut self, reason: String) -> Self {
        self.config_error = Some(reason);
        self
    }

    /// 离线：跳过所有联网检查（`--offline`）
    #[must_use]
    pub const fn offline(mut self) -> Self {
        self.online = false;
        self
    }

    /// 跑一遍自检
    ///
    /// 检查项：数据目录存在 / 可写、配置可解析且取值已注册、有 Java、
    /// 登记的 Java 仍在盘上、有服务端来源、来源可达且非空。
    ///
    /// 单项失败不会被吞成「无问题」：读不出来就各自成为一条发现，
    /// 否则诊断工具会把故障报成通过。
    pub async fn run(&self) -> PlatformReport {
        // 三项本地检查都是阻塞文件 IO（探写权限、stat Java 路径）：
        // 放到阻塞线程池，不占 async 执行器。
        let data_dir = self.data_dir.clone();
        let config = self.config.clone();
        let config_path = self.config_path.clone();
        let config_error = self.config_error.clone();
        let java = self.java.clone();
        let java_scanned = self.java_scanned;
        let available_sources = self.available_sources.clone();
        let mut findings = tokio::task::spawn_blocking(move || {
            let mut findings = Vec::new();
            check_data_dir(&data_dir, &mut findings);
            check_config(
                &config,
                &config_path,
                config_error.as_deref(),
                &available_sources,
                &mut findings,
            );
            check_java(java.as_deref(), java_scanned, &mut findings);
            findings
        })
        .await
        .unwrap_or_default();

        self.check_sources(&mut findings).await;
        findings.sort_by(|a, b| {
            a.severity()
                .cmp(&b.severity())
                .then_with(|| a.check.as_str().cmp(b.check.as_str()))
        });
        PlatformReport {
            findings,
            online: self.online,
        }
    }

    async fn check_sources(&self, out: &mut Vec<PlatformFinding>) {
        if self.jar_sources.is_empty() {
            out.push(PlatformFinding {
                check: PlatformCheck::NoJarSource,
            });
            return;
        }
        if !self.online {
            return;
        }
        for source in &self.jar_sources {
            match source.versions().await {
                Ok(versions) if versions.is_empty() => out.push(PlatformFinding {
                    check: PlatformCheck::SourceEmpty {
                        source: source.name(),
                    },
                }),
                Ok(_) => {}
                Err(err) => out.push(PlatformFinding {
                    check: PlatformCheck::SourceUnreachable {
                        source: source.name(),
                        reason: err.to_string(),
                    },
                }),
            }
        }
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

fn check_config(
    config: &Config,
    config_path: &Path,
    config_error: Option<&str>,
    available_sources: &[&'static str],
    out: &mut Vec<PlatformFinding>,
) {
    if let Some(reason) = config_error {
        out.push(PlatformFinding {
            check: PlatformCheck::ConfigInvalid {
                path: config_path.to_path_buf(),
                reason: reason.to_owned(),
            },
        });
        return;
    }
    for key in ConfigKey::ALL {
        if !config.is_set(*key) {
            continue;
        }
        // 只有 Java 源有可枚举的注册表；其余键的校验在写入时已完成
        let ConfigKey::JavaSource = key else {
            continue;
        };
        let value = config.effective(*key).display();
        if !available_sources.iter().any(|c| *c == value) {
            out.push(PlatformFinding {
                check: PlatformCheck::ConfigSourceUnknown {
                    key: key.as_str(),
                    value,
                    choices: available_sources.iter().map(|c| (*c).to_owned()).collect(),
                },
            });
        }
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
}
