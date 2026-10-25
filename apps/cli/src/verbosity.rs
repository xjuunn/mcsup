use clap_verbosity_flag::{LogLevel, Verbosity, VerbosityFilter};
use tracing::level_filters::LevelFilter;

/// `-v` / `-q` 的中文说明；默认级别与 clap 的 `ErrorLevel` 一致（只报 error）
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ZhLevel;

impl LogLevel for ZhLevel {
    fn default_filter() -> VerbosityFilter {
        VerbosityFilter::Error
    }

    fn verbose_help() -> Option<&'static str> {
        Some("提升日志级别（可重复）")
    }

    fn quiet_help() -> Option<&'static str> {
        Some("降低日志级别（可重复）")
    }
}

/// 把 -v / -q 映射到 tracing 过滤级别，日志全部 stderr
pub(crate) fn init(verbose: Verbosity<ZhLevel>) {
    let default: LevelFilter = verbose.tracing_level_filter();
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(default.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
