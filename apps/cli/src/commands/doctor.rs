//! `mcsup doctor`：平台环境自检的入口
//!
//! 检查清单与级别在 `mcsup::service::PlatformDoctor`；这里只做
//! 「采集输入 → 跑 → 渲染 → 映射退出码」。输入采集失败（配置读不出来、
//! 注册表打不开）本身也是发现，不让 doctor 因为一个子系统坏掉就整个崩掉。

use crate::bootstrap;
use crate::config::ConfigFile;
use crate::diagnostic::AppDiagnostic;
use crate::output::Printer;
use crate::progress::TaskSpinner;
use anyhow::Result;
use mcsup::protocol::cli::{PlatformDoctorJson, PlatformFindingJson};
use mcsup::service::{PlatformCheck, PlatformDoctor, PlatformFinding, Severity};
use serde_json::Value;

/// 全部检查项标识（`PlatformCheck::KNOWN_KINDS` 是唯一来源）
const KNOWN_KINDS: &[&str] = PlatformCheck::KNOWN_KINDS;

pub(crate) async fn run(
    file: &ConfigFile,
    only: &[String],
    strict: bool,
    offline: bool,
    printer: Printer,
) -> Result<()> {
    validate_only(only)?;
    let doctor = collect(file, offline);
    // 来源连通性是真请求：没有回显时断网会像卡死
    let spinner = TaskSpinner::new("检查运行环境", printer.is_human() && !offline);
    let report = doctor.run().await;
    spinner.finish();

    // --only 过滤只影响展示与退出码判定，检查本身全跑：过滤在 service 里做
    // 会让「不关心」变成「不检查」，掩盖连带问题
    let findings: Vec<&PlatformFinding> = report
        .findings()
        .iter()
        .filter(|f| only.is_empty() || only.iter().any(|k| k == f.check.as_str()))
        .collect();
    let errors = findings
        .iter()
        .filter(|f| f.severity() == Severity::Error)
        .count();
    let warnings = findings.len() - errors;
    let online = report.is_online();

    if printer.is_human() {
        render_human(&findings, online, printer);
    }

    let payload = PlatformDoctorJson {
        schema: 1,
        online,
        has_errors: errors > 0,
        errors,
        warnings,
        findings: findings
            .iter()
            .map(|f| PlatformFindingJson {
                level: f.severity().as_str(),
                kind: f.check.as_str(),
                detail: detail_json(&f.check),
                message: f.check.describe(),
                fix: f.check.fix_hint(),
            })
            .collect(),
    };
    let summary = if findings.is_empty() {
        "检查通过".to_owned()
    } else {
        format!(
            "发现 {} 个问题（{errors} error, {warnings} warning）",
            findings.len()
        )
    };
    printer.print_outcome(&summary, &payload)?;

    if errors > 0 || (strict && warnings > 0) {
        return Err(AppDiagnostic::findings(errors).into());
    }
    Ok(())
}

/// `--only` 拼错 → 用法错误（2），列出可用检查项
fn validate_only(only: &[String]) -> Result<()> {
    for kind in only {
        if !KNOWN_KINDS.contains(&kind.as_str()) {
            return Err(AppDiagnostic::usage(
                format!("未知检查项: {kind}"),
                Some(format!("可用检查项: {}", KNOWN_KINDS.join(", "))),
            )
            .into());
        }
    }
    Ok(())
}

/// 采集 doctor 的输入
///
/// 配置读不出来、组合根装不起来、注册表打不开都不让本命令失败：
/// 每一处都降级成一条发现，让报告本身说清是哪一层坏了。
fn collect(file: &ConfigFile, offline: bool) -> PlatformDoctor {
    let (cfg, config_error) = match file.load() {
        Ok(cfg) => (cfg, None),
        Err(err) => (mcsup::service::Config::default(), Some(format!("{err:#}"))),
    };
    let data_dir = cfg.resolved_data_dir();
    let mcsup = bootstrap::open_mcsup().ok();

    let java = mcsup.as_ref().and_then(|m| m.java.list().ok());
    let java_scanned = mcsup
        .as_ref()
        .is_some_and(|m| m.java.has_scanned().unwrap_or(false));
    let sources = mcsup
        .as_ref()
        .map_or_else(Vec::new, |m| m.jar_sources.clone());
    let available: Vec<&'static str> = mcsup
        .as_ref()
        .map_or_else(Vec::new, |m| m.java.source_names());

    let mut doctor = PlatformDoctor::new(
        cfg,
        file.path().to_path_buf(),
        data_dir,
        java,
        java_scanned,
        sources,
        available,
    );
    if let Some(reason) = config_error {
        doctor = doctor.with_config_error(reason);
    }
    if offline {
        doctor = doctor.offline();
    }
    doctor
}

fn render_human(findings: &[&PlatformFinding], online: bool, printer: Printer) {
    if findings.is_empty() {
        printer.print_section("检查通过：运行环境可用");
    } else {
        for f in findings {
            let tag = match f.severity() {
                Severity::Error => "ERROR",
                Severity::Warning => "WARN",
            };
            let line = format!(
                "[{tag}] {}\n        {}\n        修复：{}",
                f.check.describe(),
                f.check.locate(),
                f.check.fix_hint()
            );
            match f.severity() {
                Severity::Error => printer.print_warning(&line),
                Severity::Warning => printer.print_section(&line),
            }
        }
    }
    if !online {
        printer.print_section("已跳过联网检查（--offline）");
    }
}

/// 结构化字段：与 `PlatformCheck` 同源，kind 与 detail 不可能不一致
fn detail_json(check: &PlatformCheck) -> Value {
    match check {
        PlatformCheck::DataDirMissing { path } => {
            serde_json::json!({ "kind": check.as_str(), "path": path })
        }
        PlatformCheck::DataDirNotWritable { path, reason }
        | PlatformCheck::ConfigInvalid { path, reason } => {
            serde_json::json!({ "kind": check.as_str(), "path": path, "reason": reason })
        }
        PlatformCheck::ConfigSourceUnknown {
            key,
            value,
            choices,
        } => serde_json::json!({
            "kind": check.as_str(),
            "key": key,
            "value": value,
            "choices": choices,
        }),
        PlatformCheck::JavaMissing { path, version } => {
            serde_json::json!({ "kind": check.as_str(), "path": path, "version": version })
        }
        PlatformCheck::SourceUnreachable { source, reason } => {
            serde_json::json!({ "kind": check.as_str(), "source": source, "reason": reason })
        }
        PlatformCheck::SourceEmpty { source } => {
            serde_json::json!({ "kind": check.as_str(), "source": source })
        }
        // 无额外字段的类型：保留最小形状
        _ => serde_json::json!({ "kind": check.as_str() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_only_kind_is_usage_error() {
        let err = validate_only(&["nope".to_owned()]).expect_err("unknown");
        let diag = err
            .downcast_ref::<AppDiagnostic>()
            .expect("usage diagnostic");
        assert_eq!(diag.exit_code, 2);
    }

    #[test]
    fn known_kinds_pass_validation() {
        let all: Vec<String> = KNOWN_KINDS.iter().map(|k| (*k).to_owned()).collect();
        validate_only(&all).expect("全部已知检查项");
    }

    /// 带字段的类型必须把路径放进 detail，而不是只留一个 kind
    #[test]
    fn detail_carries_addressable_fields() {
        let check = PlatformCheck::JavaMissing {
            path: std::path::PathBuf::from("/jdk"),
            version: "21.0.1".to_owned(),
        };
        let detail = detail_json(&check);
        assert_eq!(detail["kind"], "java-missing");
        assert_eq!(detail["version"], "21.0.1");
        assert!(detail["path"].is_string());
    }
}
