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
/// 缺省只监听本机：`mcsup-agent` 一旦被暴露到网络，配对码就是唯一门槛，
/// 而默认值不应该替用户做「对外开端口」这个决定。需要别的机器连进来时
/// 显式 `--listen 0.0.0.0:7681`，并由用户自己处理防火墙。
const DEFAULT_LISTEN: &str = "127.0.0.1:7681";

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
///
/// 常驻进程用**按需**发码策略：启动时不持有任何配对码，端口开着也没有码可猜。
/// 需要配对时在本机跑 `mcsup remote code` 生成一枚（`docs/remote.md`「安全」）。
fn run() -> anyhow::Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    let agent = std::sync::Arc::new(mcsup::remote::Agent::open_with(
        &args.data_dir,
        &args.name,
        mcsup::remote::PairingPolicy::OnDemand,
    )?);
    let server = agent.bind(&args.listen)?;
    let addr = server.local_addr()?;

    announce(&agent, &args, addr, &code_hint(&args));
    let result = server.run();
    result?;
    Ok(())
}

/// 启动信息：用户第一次看到它就要能照抄出 `remote connect`
#[allow(clippy::print_stdout)]
fn announce(
    agent: &mcsup::remote::Agent,
    args: &Args,
    addr: std::net::SocketAddr,
    code_hint: &str,
) {
    println!("mcsup 已开始监听");
    println!("  名称    {}", args.name);
    println!("  地址    {addr}");
    println!("  指纹    {}", agent.fingerprint());
    println!("  数据    {}", args.data_dir.display());
    println!();
    println!("  配对码  未生成（按需发放，启动时不持有）");
    println!("          需要配对时在本机运行：{code_hint}");
    if addr.ip().is_loopback() {
        println!("  提示    只监听本机；其他机器连进来需 --listen 0.0.0.0:7681 并放行防火墙");
    }
}

/// 配对码提示：**不打印码**，只说明怎么取
///
/// 按需策略下码不在这个进程里：它由 `remote code` 写到同一个数据目录的
/// `remote/grant.toml`（只存摘要），agent 兑换时读那份文件。
#[allow(clippy::print_stdout)]
fn code_hint(args: &Args) -> String {
    // 只在数据目录不是默认值时提示，避免让用户以为必须带参数
    if args.data_dir == mcsup::service::Config::default_data_dir() {
        "mcsup remote code".to_owned()
    } else {
        format!("mcsup remote code --data-dir {}", args.data_dir.display())
    }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::Args;

    /// 只给 `--name` 时其余取缺省：`--listen` 用内置地址，`--data-dir` 用平台目录
    #[test]
    fn parse_keeps_defaults_for_unspecified_flags() {
        let args = Args::parse(["--name", "node-a"]).expect("parse");
        assert_eq!(args.name, "node-a");
        assert_eq!(args.listen, super::DEFAULT_LISTEN);
        assert_eq!(args.data_dir, mcsup::service::Config::default_data_dir());
    }

    /// 三个参数都能被覆盖，且 `--data-dir` 转成程序路径
    #[test]
    fn parse_accepts_every_flag() {
        let args = Args::parse([
            "--listen",
            "0.0.0.0:9443",
            "--data-dir",
            "/tmp/agent-data",
            "--name",
            "node-b",
        ])
        .expect("parse");
        assert_eq!(args.listen, "0.0.0.0:9443");
        assert_eq!(args.data_dir, std::path::PathBuf::from("/tmp/agent-data"));
        assert_eq!(args.name, "node-b");
    }

    /// 空名字必须拒绝：它会成为客户端的缺省远端名，空值无法被引用
    #[test]
    fn parse_rejects_blank_name() {
        let err = Args::parse(["--name", "   "]).expect_err("must reject");
        assert!(err.to_string().contains("--name"), "{err}");
    }

    /// 未知参数报用法错误，而不是被静默忽略
    #[test]
    fn parse_rejects_unknown_flag() {
        let err = Args::parse(["--nope", "x"]).expect_err("must reject");
        assert!(err.to_string().contains("--nope"), "{err}");
    }

    /// 缺取值的尾随参数要指出是哪一个
    #[test]
    fn parse_rejects_missing_value() {
        let err = Args::parse(["--listen"]).expect_err("must reject");
        assert!(err.to_string().contains("--listen"), "{err}");
    }

    /// 多次给出同一参数时后者生效（与常见 CLI 一致，不报错）
    #[test]
    fn parse_last_value_wins() {
        let args = Args::parse(["--name", "first", "--name", "second"]).expect("parse");
        assert_eq!(args.name, "second");
    }
}
