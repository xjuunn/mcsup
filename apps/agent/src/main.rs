//! `mcsup-agent`：被连接端的常驻进程
//!
//! 薄入口：解析监听参数、打印配对信息、跑 [`mcsup::remote::Agent`]。业务在
//! 门面（`mcsup::remote`），这里只做「把命令行变成一次 `serve`」。
//!
//! 同时承载隐藏入口 `mcsup-agent __supervise …`：远端 `server start` 由 agent
//! 在**同一台机器**上拉起 supervisor 子进程，子进程必须能跑下去而不依赖 agent
//! 存活，所以它必须是独立可执行文件。复用本进程省掉第二个 bin。

use std::path::PathBuf;
use std::process::ExitCode;

/// 缺省监听地址
///
/// 监听全部网卡是「让别的机器连进来」这个功能的全部意义；`127.0.0.1` 只在
/// 本机自测时有用，用户还得再查一遍文档才知道要改。
const DEFAULT_LISTEN: &str = "0.0.0.0:7681";

fn main() -> ExitCode {
    // 隐藏入口先分流：它不初始化日志与网络，直接跑 supervisor
    if mcsup::remote::is_supervise_invocation(std::env::args()) {
        let code = mcsup::remote::supervise_main();
        return ExitCode::from(u8::try_from(code).unwrap_or(1));
    }

    init_tracing();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            note(&format!("{err:#}"));
            ExitCode::FAILURE
        }
    }
}

/// 解析参数并阻塞服务
fn run() -> anyhow::Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    let agent = std::sync::Arc::new(mcsup::remote::Agent::open(&args.data_dir, &args.name)?);

    // 先发码再绑定：绑定失败时用户也能看到「码已生成但没起来」之前的真正原因，
    // 而不是先看到一个没有码的启动失败
    let code = agent.issue_pairing_code()?;
    let server = agent.bind(&args.listen)?;
    let addr = server.local_addr()?;

    announce(&agent, &args, addr);
    report_pairing_code(&code);

    // 轮换线程只在码自然过期时换新：用掉或锁死都保持关闭（限速不能失效）
    let watch = mcsup::remote::Agent::watch_pairing_code(&agent, report_pairing_code);
    let result = server.run();
    drop(watch);
    result?;
    Ok(())
}

/// 启动信息：用户第一次看到它就要能照抄出 `remote connect`
#[allow(clippy::print_stdout)]
fn announce(agent: &mcsup::remote::Agent, args: &Args, addr: std::net::SocketAddr) {
    println!("mcsup 已开始监听");
    println!("  名称    {}", args.name);
    println!("  地址    {addr}");
    println!("  指纹    {}", agent.fingerprint());
    println!("  数据    {}", args.data_dir.display());
}

/// 配对码：带分组显示，并说明有效期
#[allow(clippy::print_stdout)]
fn report_pairing_code(code: &mcsup::auth::PairingCode) {
    println!();
    println!("  配对码  {}", code.display_form());
    println!("          10 分钟内有效，成功配对后自动更换");
}

/// 错误与提示写 stderr
fn note(message: &str) {
    let mut stderr = std::io::stderr();
    let _ = std::io::Write::write_all(&mut stderr, format!("mcsup-agent: {message}\n").as_bytes());
}

/// 日志只写 stderr：stdout 留给启动信息与配对码
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MCSUP_AGENT_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}

/// 命令行参数
#[derive(Debug, Clone)]
struct Args {
    /// 监听地址（`host:port`）
    listen: String,
    /// 数据目录（仓库、TLS 身份、客户端名册都在这下面）
    data_dir: PathBuf,
    /// 本机名（客户端 `remote connect` 的缺省远端名）
    name: String,
}

impl Args {
    /// 解析 `--listen` / `--data-dir` / `--name`
    ///
    /// 不用 clap：入口只有这三个参数，且 `mcsup-agent` 的分发主体是常驻服务，
    /// 多一套帮助/补全体系没有收益。
    fn parse<I, S>(args: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut listen = DEFAULT_LISTEN.to_owned();
        let mut data_dir: Option<PathBuf> = None;
        let mut name: Option<String> = None;

        let mut iter = args.into_iter().map(Into::into);
        while let Some(flag) = iter.next() {
            match flag.as_str() {
                "--listen" => listen = value(&mut iter, "--listen")?,
                "--data-dir" => data_dir = Some(PathBuf::from(value(&mut iter, "--data-dir")?)),
                "--name" => name = Some(value(&mut iter, "--name")?),
                "--help" | "-h" => {
                    print_usage();
                    std::process::exit(0);
                }
                other => anyhow::bail!("未知参数：{other}（--help 查看用法）"),
            }
        }

        let name = name.unwrap_or_else(host_name);
        if name.trim().is_empty() {
            anyhow::bail!("--name 不能为空");
        }
        Ok(Self {
            listen,
            data_dir: data_dir.unwrap_or_else(mcsup::service::Config::default_data_dir),
            name,
        })
    }
}

/// 取一个参数值
fn value(iter: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    iter.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} 缺少取值"))
}

/// 本机名：取系统环境里的主机名，取不到就用固定名
///
/// 只是展示用的缺省值，不参与任何判定，因此失败不必报错。
fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "mcsup-agent".to_owned())
}

/// 用法
#[allow(clippy::print_stdout)]
fn print_usage() {
    println!("mcsup-agent：被连接端（agent）");
    println!();
    println!("用法：mcsup-agent [--listen <ADDR>] [--data-dir <PATH>] [--name <NAME>]");
    println!();
    println!("  --listen <ADDR>     监听地址，缺省 {DEFAULT_LISTEN}");
    println!("  --data-dir <PATH>   数据目录，缺省为平台数据目录");
    println!("  --name <NAME>       本机名，缺省取系统主机名");
}
