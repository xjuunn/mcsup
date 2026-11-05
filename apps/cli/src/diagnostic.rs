use mcsup::port::ErrorCategory;
use miette::Diagnostic;
use std::fmt::Display;

/// 入口边界错误
///
/// 分类用 [`ErrorCategory`] 而不是直接写退出码：CLI 是三个入口之一，退出码是
/// CLI 的概念。分类在库侧定、退出码在 [`crate::output::error`] 定，同一件事
/// 不在两处各写一份。
#[derive(Debug)]
pub(crate) struct AppDiagnostic {
    category: ErrorCategory,
    message: String,
    help: Option<String>,
}

impl Display for AppDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AppDiagnostic {}

impl Diagnostic for AppDiagnostic {
    fn severity(&self) -> Option<miette::Severity> {
        Some(miette::Severity::Error)
    }

    fn help<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        self.help
            .as_deref()
            .map(|help| Box::new(help) as Box<dyn Display + '_>)
    }
}

impl AppDiagnostic {
    /// 失败分类（退出码由入口按分类映射，见 [`crate::output::error`]）
    #[must_use]
    pub(crate) const fn category(&self) -> ErrorCategory {
        self.category
    }

    #[must_use]
    pub(crate) fn not_implemented(what: &str) -> Self {
        Self {
            category: ErrorCategory::Failure,
            message: format!("{what} 尚未实现"),
            help: Some("先在 service 层实现对应能力，再填充这里的入口".to_owned()),
        }
    }

    /// 普通失败：业务拒绝、远端不可达等，不是「尚未实现」
    #[must_use]
    pub(crate) fn failure(message: impl Into<String>, help: Option<String>) -> Self {
        Self {
            category: ErrorCategory::Failure,
            message: message.into(),
            help,
        }
    }

    /// 状态冲突：要做的动作与当前状态矛盾（如已在运行）
    #[must_use]
    pub(crate) fn conflict(message: impl Into<String>, help: Option<String>) -> Self {
        Self {
            category: ErrorCategory::Conflict,
            message: message.into(),
            help,
        }
    }

    /// 目标不存在
    #[must_use]
    pub(crate) fn not_found(message: impl Into<String>, help: Option<String>) -> Self {
        Self {
            category: ErrorCategory::NotFound,
            message: message.into(),
            help,
        }
    }

    /// 未授权：token 失效、权限不足、指纹变化
    #[must_use]
    pub(crate) fn unauthorized(message: impl Into<String>, help: Option<String>) -> Self {
        Self {
            category: ErrorCategory::Unauthorized,
            message: message.into(),
            help,
        }
    }

    /// 用法错误（与 clap 的参数解析错误同级）：
    /// 非交互 / JSON 模式下缺必选参数、取值不合法时走这里，不进入交互兜底
    #[must_use]
    pub(crate) fn usage(message: impl Into<String>, help: Option<String>) -> Self {
        Self {
            category: ErrorCategory::Usage,
            message: message.into(),
            help,
        }
    }

    /// 用户取消（Ctrl-C 协作取消后走完清理路径）
    ///
    /// 入口把 [`ErrorCategory::Cancelled`] 映射成 130（= 128 + `SIGINT`），
    /// 并对它**不渲染消息**：用户自己按的 Ctrl-C 不是错误。
    #[must_use]
    pub(crate) fn cancelled() -> Self {
        Self {
            category: ErrorCategory::Cancelled,
            message: "已取消".to_owned(),
            help: None,
        }
    }

    /// 体检发现错误级问题：数据要人处理，不是程序失败
    #[must_use]
    pub(crate) fn findings(errors: usize) -> Self {
        Self {
            category: ErrorCategory::Failure,
            message: format!("发现 {errors} 个错误级问题，请按提示处理"),
            help: Some("修复后可再次运行 `mcsup workspace doctor` 复核".to_owned()),
        }
    }
}
