use domain::server::version::VersionId;
use port::SourceError;
use service::InstallError;
use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

mod install;

/// 命令失败类型
enum Failure {
    /// 用法错误
    Usage,
    /// 安装任务失败
    Install(InstallError),
}

impl Failure {
    /// 0 成功 / 1 通用失败 / 2 用法 / 3 未找到
    #[must_use]
    const fn exit_code(&self) -> u8 {
        match self {
            Self::Usage => 2,
            Self::Install(InstallError::Source(SourceError::NotFound(_))) => 3,
            Self::Install(_) => 1,
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Usage => "usage: mcsup server install <version> [--dir <path>]".to_owned(),
            Self::Install(err) => err.to_string(),
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(path) => {
            print_path(&path);
            ExitCode::SUCCESS
        }
        Err(failure) => {
            print_error(&failure.message());
            ExitCode::from(failure.exit_code())
        }
    }
}

async fn run() -> Result<PathBuf, Failure> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("server") => server(&args[1..]).await,
        _ => Err(Failure::Usage),
    }
}

async fn server(args: &[String]) -> Result<PathBuf, Failure> {
    match args.first().map(String::as_str) {
        Some("install") => install(&args[1..]).await,
        _ => Err(Failure::Usage),
    }
}

async fn install(args: &[String]) -> Result<PathBuf, Failure> {
    let Some(version) = args.first() else {
        return Err(Failure::Usage);
    };
    let version = VersionId::new(version.clone());
    let mut dir = PathBuf::from(".");
    if let Some(rest) = args.get(1..) {
        if rest.len() != 2 || rest[0] != "--dir" {
            return Err(Failure::Usage);
        }
        dir = PathBuf::from(&rest[1]);
    }
    let runner = install::InstallRunner::assemble().map_err(Failure::Install)?;
    runner
        .install(&version, dir)
        .await
        .map_err(Failure::Install)
}

#[allow(clippy::print_stdout)]
fn print_path(path: &Path) {
    println!("{}", path.display());
}

#[allow(clippy::print_stderr)]
fn print_error(message: &str) {
    eprintln!("mcsup: {message}");
}
