use clap_verbosity_flag::Verbosity;
use tracing::level_filters::LevelFilter;

/// 把 -v / -q 映射到 tracing 过滤级别，日志全部 stderr
pub(crate) fn init(verbose: Verbosity) {
    let default: LevelFilter = verbose.tracing_level_filter();
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(default.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
