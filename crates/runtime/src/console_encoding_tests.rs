//! Windows 端到端：控制台日志必须保留子进程的原始字节
//!
//! 服务端（Java）在 Windows 下按系统代码页（GBK）输出中文。若 supervisor 的
//! `pump` 先用 `String::from_utf8_lossy` 解码再写 `console.log`，GBK 字节会被
//! 替换成 U+FFFD（`EF BF BD`），日志在写入端就永久损坏，`server console`
//! 回放时表现为乱码。这里驱动真实子进程吐非 UTF-8 字节，断言日志保真。
//!
//! 只在 Windows 上跑：依赖 PowerShell 直接写标准输出原始字节（Unix 侧由
//! `e2e_tests::console_log_keeps_non_utf8_bytes` 覆盖）。
#![cfg(windows)]

use crate::supervisor::run_with;
use domain::server::layout::console_log_path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// PowerShell 先把 GBK 字节直接写到 stdout，再输出就绪标记并等待停机
const SCRIPT: &str = "\
$b=[byte[]](0xD5,0xFD,0xD4,0xDA); \
$s=[Console]::OpenStandardOutput(); \
$s.Write($b,0,$b.Length); $s.Flush(); \
[Console]::Out.WriteLine('Done (0.1s)!'); \
Start-Sleep -Seconds 5";

/// GBK 的「正在」，不是合法 UTF-8
const GBK: [u8; 4] = [0xD5, 0xFD, 0xD4, 0xDA];

#[test]
fn console_log_keeps_gbk_bytes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();
    let spawned = dir.clone();
    let handle = thread::spawn(move || {
        run_with(&spawned, move || {
            Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        })
    });
    crate::client::wait_up(&dir, Duration::from_secs(30)).expect("supervisor ready");

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let bytes = std::fs::read(console_log_path(&dir)).unwrap_or_default();
        if bytes.windows(GBK.len()).any(|w| w == GBK) {
            break;
        }
        assert!(Instant::now() < deadline, "console.log 未保留 GBK 字节");
        thread::sleep(Duration::from_millis(50));
    }

    let bytes = std::fs::read(console_log_path(&dir)).expect("read console.log");
    assert!(
        !bytes.windows(3).any(|w| w == [0xEF, 0xBB, 0xBF]),
        "日志不应出现 U+FFFD 替换符：{bytes:02X?}"
    );

    let _ = crate::client::rpc(
        &dir,
        protocol::Request::Stop {
            grace_secs: 0,
            force: true,
        },
        Duration::from_secs(5),
    );
    let _ = crate::client::rpc(&dir, protocol::Request::Shutdown, Duration::from_secs(5));
    let _ = handle.join();
}
