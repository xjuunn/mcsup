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
}
