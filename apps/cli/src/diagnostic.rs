use miette::Diagnostic;
use std::fmt::Display;

/// 入口边界错误
#[derive(Debug)]
pub(crate) struct AppDiagnostic {
    pub(crate) exit_code: u8,
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
    #[must_use]
    pub(crate) fn not_implemented(what: &str) -> Self {
        Self {
            exit_code: 1,
            message: format!("{what} 尚未实现"),
            help: Some("先在 service 层实现对应能力，再填充这里的入口".to_owned()),
        }
    }

    /// 普通失败（退出码 1）：业务拒绝、远端不可达等，不是「尚未实现」
    #[must_use]
    pub(crate) fn failure(message: impl Into<String>, help: Option<String>) -> Self {
        Self {
            exit_code: 1,
            message: message.into(),
            help,
        }
    }

    /// 目标不存在（退出码 3）
    #[must_use]
    pub(crate) fn not_found(message: impl Into<String>, help: Option<String>) -> Self {
        Self {
            exit_code: 3,
            message: message.into(),
            help,
        }
    }

    /// 未授权（退出码 6）：token 失效、权限不足、指纹变化
    #[must_use]
    pub(crate) fn unauthorized(message: impl Into<String>, help: Option<String>) -> Self {
        Self {
            exit_code: 6,
            message: message.into(),
            help,
        }
    }

    /// 用法错误（退出码 2，与 clap 的参数解析错误同级）：
    /// 非交互 / JSON 模式下缺必选参数、取值不合法时走这里，不进入交互兜底
    #[must_use]
    pub(crate) fn usage(message: impl Into<String>, help: Option<String>) -> Self {
        Self {
            exit_code: 2,
            message: message.into(),
            help,
        }
    }

    /// 用户取消（Ctrl-C 协作取消后走完清理路径）：130 = 128 + SIGINT，
    /// 与强制退出码一致，调用方凭退出码即可区分「取消」与「失败」；
    /// 入口层对 130 不渲染消息（静默退出）
    #[must_use]
    pub(crate) fn cancelled() -> Self {
        Self {
            exit_code: 130,
            message: "已取消".to_owned(),
            help: None,
        }
    }

    /// 体检发现错误级问题：数据要人处理，不是程序失败（退出码 1）
    #[must_use]
    pub(crate) fn findings(errors: usize) -> Self {
        Self {
            exit_code: 1,
            message: format!("发现 {errors} 个错误级问题，请按提示处理"),
            help: Some("修复后可再次运行 `mcsup workspace doctor` 复核".to_owned()),
        }
    }
}
