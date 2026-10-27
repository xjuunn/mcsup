//! 假 ssh：SSH 引导与隧道的测试替身（由 `cli_remote_ssh` 用 rustc 现场编译）
//!
//! 测试环境里没有 sshd，也没有装好 mcsup 的第二台机器，但 CLI 只用 `ssh`
//! 的**命令行契约**，不使用 SSH 库：
//!
//! ```text
//! 引导  ssh <target> <remote-bin> remote __serve-ssh
//! 隧道  ssh -N -o ExitOnForwardFailure=yes -L <local>:<host>:<remote> <target>
//! ```
//!
//! 本程序实现这两条契约，其余交给环境变量描述「远端」：
//!
//! | 变量 | 含义 |
//! | --- | --- |
//! | `FAKE_SSH_AGENT_ADDR` | 远端 agent 的监听地址（测试进程里跑着的真 agent） |
//! | `FAKE_SSH_FINGERPRINT` | 该 agent 的证书指纹 |
//! | `FAKE_SSH_PAIRING_CODE` | 引导时给出的配对码 |
//! | `FAKE_SSH_SERVER_NAME` | 对端自报的主机名 |
//! | `FAKE_SSH_LOG` | 追加一行调用记录（断言 CLI 到底跑了什么） |
//! | `FAKE_SSH_FAIL` | 置任意值则模拟 SSH 失败（stderr + 非零退出） |
//!
//! 为什么不用脚本：隧道模式必须做真正的 TCP 转发，否则测不出「命令走隧道
//! 连上了 agent」这件事；`.cmd` / `.sh` 都写不出可靠的跨平台字节转发。
//!
//! 为什么由测试用 rustc 现场编译而不是做成 `examples/`：`cargo test` 不构建
//! examples（`--test <name>` 更是只构建选中的目标），而在测试里再跑一次
//! `cargo build` 会按不同的 feature 组合重建整棵依赖树。这个文件只用标准库，
//! 单独 rustc 编译不到一秒，且与 cargo 的 feature 决议完全无关。
//!
//! 本文件不是 cargo 的目标（`tests/` 下只有顶层 `.rs` 才是测试目标），因此
//! 不参与 workspace 的 clippy / doc 检查。

use std::error::Error;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let _ = writeln!(std::io::stderr(), "fake-ssh: {err}");
            ExitCode::from(3)
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    record_invocation(&args)?;

    if std::env::var_os("FAKE_SSH_FAIL").is_some() {
        return Err("simulated ssh failure".into());
    }
    if args.iter().any(|arg| arg == "__serve-ssh") {
        return serve_handshake();
    }
    if let Some(spec) = args
        .iter()
        .position(|arg| arg == "-L")
        .and_then(|index| args.get(index + 1))
    {
        return forward(spec);
    }
    Err(format!("unsupported invocation: {args:?}").into())
}

/// 追加一行调用记录：测试据此断言 CLI 用的是引导还是隧道（以及目标）
fn record_invocation(args: &[String]) -> std::io::Result<()> {
    let Some(path) = std::env::var_os("FAKE_SSH_LOG") else {
        return Ok(());
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", args.join(" "))
}

/// 引导模式：打一行握手 JSON，然后活到 stdin 关闭
///
/// 与真实现同一份寿命语义：本地 CLI 一退出，它持有的管道关闭，这里读到
/// EOF 就收摊。
fn serve_handshake() -> Result<(), Box<dyn Error>> {
    let addr = required("FAKE_SSH_AGENT_ADDR")?;
    let fingerprint = required("FAKE_SSH_FINGERPRINT")?;
    let pairing_code = required("FAKE_SSH_PAIRING_CODE")?;
    let server_name =
        std::env::var("FAKE_SSH_SERVER_NAME").unwrap_or_else(|_| "fake-ssh-box".to_owned());
    let handshake = format!(
        r#"{{"addr":"{}","fingerprint":"{}","pairing_code":"{}","server_name":"{}"}}"#,
        escape(&addr),
        escape(&fingerprint),
        escape(&pairing_code),
        escape(&server_name)
    );
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{handshake}")?;
    stdout.flush()?;

    // 读不到 EOF 就一直跑：CLI 侧子进程被 kill 时也会走到这里
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 256];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => return Ok(()),
            Ok(_) => {}
        }
    }
}

/// 隧道模式：在本地端口监听，把每个连接转发到远端端口
fn forward(spec: &str) -> Result<(), Box<dyn Error>> {
    let parts: Vec<&str> = spec.split(':').collect();
    if parts.len() != 4 {
        return Err(format!("unsupported -L spec: {spec}").into());
    }
    let local_port: u16 = parts[1].parse()?;
    let remote_port: u16 = parts[3].parse()?;
    let listener = TcpListener::bind(("127.0.0.1", local_port))?;
    for inbound in listener.incoming() {
        let Ok(inbound) = inbound else { continue };
        std::thread::spawn(move || {
            let _ = pipe(inbound, remote_port);
        });
    }
    Ok(())
}

/// 双向转发一条连接
fn pipe(mut inbound: TcpStream, remote_port: u16) -> std::io::Result<()> {
    let mut outbound = TcpStream::connect(("127.0.0.1", remote_port))?;
    let mut inbound_reader = inbound.try_clone()?;
    let mut outbound_writer = outbound.try_clone()?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut inbound_reader, &mut outbound_writer);
    });
    std::io::copy(&mut outbound, &mut inbound)?;
    Ok(())
}

/// 必填环境变量
fn required(name: &str) -> Result<String, Box<dyn Error>> {
    std::env::var(name).map_err(|_| format!("{name} is not set").into())
}

/// JSON 字符串转义（握手字段全是标识符，但仍然按规矩转义）
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}
