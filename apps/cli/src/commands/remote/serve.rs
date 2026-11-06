//! `remote serve` / `remote code`：本机作为被连接端的守护进程管理

use super::agent_files::{
    is_alive, is_mcsup_process, read_fingerprint, read_listen_addr, read_pid, remember_listen,
    remove_listen_addr, remove_pid, resolve_listen, terminate, write_listen_addr, write_pid,
};
use super::{
    AppDiagnostic, Arc, ClientRegistry, Context, Path, Printer, Result, ServeAction, Table,
    host_name,
};

/// `remote serve start|stop|status`：后台常驻 agent 的生命周期
///
/// 三个动作都是本机操作（不经网络）。后台形态用**按需**发码策略：启动不持有
/// 任何配对码，需要配对时另跑 `remote code`（见 [`code`]）。
pub(super) fn serve(action: ServeAction, printer: Printer) -> Result<()> {
    match action {
        ServeAction::Start {
            listen,
            name,
            foreground,
            data_dir,
        } => {
            let dir = data_dir.unwrap_or_else(crate::config::platform_data_dir);
            // 没给 `--listen` 就沿用上次用过的地址，而不是每次回落到回环。
            //
            // 回落回环是「安全默认」，但对**已经配好端口映射**的机器是陷阱：
            // 映射指向的端口在公网可用，agent 却只绑 127.0.0.1，外部流量到
            // 内网地址后被拒，客户端只看到 Connection refused，与「agent 没
            // 启动」无从区分。管理员停一次、不带参数起一次，对外服务就悄悄
            // 退化成只在本机可用。
            let listen = resolve_listen(&dir, listen);
            if foreground {
                return serve_foreground(&dir, &listen, name.as_deref(), printer);
            }
            serve_start(&dir, &listen, name.as_deref(), printer)
        }
        ServeAction::Stop { force, data_dir } => {
            let dir = data_dir.unwrap_or_else(crate::config::platform_data_dir);
            serve_stop(&dir, force, printer)
        }
        ServeAction::Status { data_dir } => {
            let dir = data_dir.unwrap_or_else(crate::config::platform_data_dir);
            serve_status(&dir, printer)
        }
    }
}

/// `remote serve start`：拉起后台 agent 等它就绪
///
/// 单实例靠锁判定，不是「pid 文件存在」：先看锁，没被持有才拉起。子进程自己
/// 也会再取一次锁（`--foreground` 路径），所以即使两个 `start` 同时跑，也只有
/// 一个能真正监听。
pub(super) fn serve_start(
    data_dir: &Path,
    listen: &str,
    name: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    if crate::daemon::InstanceLock::is_held(data_dir).unwrap_or(false) {
        return Err(AppDiagnostic::conflict(
            crate::i18n::pick(
                lang,
                "本机 agent 已在运行；先 remote serve stop 再启动，或用 remote serve status 查看",
                "the agent is already running; run `remote serve stop` first, or `remote serve status`",
            ),
            None,
        )
        .into());
    }

    // 清掉上一轮可能残留的地址文件：它现在是唯一的就绪信号，留着旧内容会
    // 让 `start` 误判「已就绪」而立刻返回一个根本没起来的 agent。
    // 进程崩溃 / 被强杀时走不到 `serve_foreground` 的清理，所以这里也要清。
    remove_listen_addr(data_dir);

    // 子进程真正 bind 成功后会再写一次（写入实际地址，`--listen :0` 时要靠它
    // 才知道端口）。这里先落盘用户的意图，崩溃在半路也不丢「上次用哪个地址」。
    remember_listen(data_dir, listen);

    let exe = std::env::current_exe().context(crate::i18n::pick(
        lang,
        "定位 mcsup 可执行文件",
        "locating the mcsup executable",
    ))?;
    // 就绪 = 地址文件已落盘。
    //
    // `serve_foreground` 的顺序是「取锁 → 生成身份 → bind → 写 pid → 写地址」，
    // 地址文件最后写，它存在即说明前面每一步都已完成，所以单看它就够。
    //
    // **不能**在就绪判定里探实例锁。`InstanceLock::is_held` 是靠 `try_lock`
    // 实现的——它为了「能不能拿到」必须真的去拿一次。于是父进程每 50ms 的探测
    // 都会短暂独占那把锁，而子进程此时正在 `acquire`：一旦撞上，子进程看到
    // 「已被别人持有」就直接退出，`start` 随后在超时里报出一句
    // 「本机 agent 已在运行」，而实际上并没有任何 agent 在跑。探针与被探测者
    // 争同一把锁，这个竞态是结构性的，不是调小概率就能解决。
    let pid = crate::daemon::spawn_background(&exe, data_dir, listen, name, |dir| {
        read_listen_addr(dir).is_some()
    })
    .context(crate::i18n::pick(
        lang,
        "启动后台 agent",
        "starting the agent",
    ))?;

    let addr = read_listen_addr(data_dir).unwrap_or_else(|| listen.to_owned());
    let fingerprint = read_fingerprint(data_dir);
    let payload = serde_json::json!({
        "running": true,
        "pid": pid,
        "listen": addr,
        "fingerprint": fingerprint,
        "log": crate::daemon::log_path(data_dir).display().to_string(),
        "pairing_code": null,
    });
    if printer.is_human() {
        let mut lines = vec![
            "mcsup 已在后台监听".to_owned(),
            format!("  名称    {}", name.map_or_else(host_name, str::to_owned)),
            format!("  地址    {addr}"),
            format!("  进程    {pid}"),
            format!("  日志    {}", crate::daemon::log_path(data_dir).display()),
        ];
        if let Some(fingerprint) = &fingerprint {
            lines.insert(3, format!("  指纹    {fingerprint}"));
        }
        printer.print_section(&lines.join("\n"));
        printer.print_section(&crate::i18n::pick(
            lang,
            "  配对码  未生成（按需发放）\n          需要配对时运行：mcsup remote code",
            "  pairing code none yet (on demand)\n              run `mcsup remote code` when you need one",
        ));
        if let Ok(parsed) = addr.parse() {
            warn_exposure(&parsed, printer);
            warn_loopback_on_mapped_host(&parsed, printer);
        }
    } else {
        printer.print_outcome("", &payload)?;
    }
    Ok(())
}

/// `remote serve start --foreground`：前台跑 agent（调试用）
///
/// 这是后台形态的**本体**：`serve start` 用同一个可执行文件拉起这个入口。
/// 它自己取单实例锁，因此无论从哪条路径进来都只有一个在监听。
pub(super) fn serve_foreground(
    data_dir: &Path,
    listen: &str,
    name: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let Some(_lock) = crate::daemon::InstanceLock::acquire(data_dir)? else {
        return Err(AppDiagnostic::conflict(
            crate::i18n::pick(
                lang,
                "本机 agent 已在运行（另一个进程持有实例锁）",
                "the agent is already running (another process holds the instance lock)",
            ),
            None,
        )
        .into());
    };

    let name = name.map_or_else(host_name, str::to_owned);
    let agent = Arc::new(
        mcsup::remote::Agent::open_with(data_dir, &name, mcsup::remote::PairingPolicy::OnDemand)
            .with_context(|| crate::i18n::pick(lang, "启动 agent", "starting the agent"))?,
    );
    let server = agent
        .bind(listen)
        .with_context(|| crate::i18n::pick(lang, "绑定监听", "binding the listener"))?;
    let addr = server.local_addr().context(crate::i18n::pick(
        lang,
        "读取监听地址",
        "reading the listen address",
    ))?;

    write_pid(data_dir, std::process::id())?;
    write_listen_addr(data_dir, &addr.to_string());
    report_serve(&agent, &name, addr, printer);
    warn_exposure(&addr, printer);

    // 不启动轮换线程：按需策略下码由 `remote code` 生成，agent 自动补码会
    // 让"不需要时无码"永远做不到（见 `PairingPolicy::keeps_code_on_screen`）。
    let result = server.run();
    remove_pid(data_dir);
    remove_listen_addr(data_dir);
    result.context(crate::i18n::pick(lang, "agent 退出", "the agent exited"))?;
    Ok(())
}

/// 监听回环时提醒「这台机器可能并不是只在本地用」
///
/// 只监听本机是安全默认，但它与「已经配好端口映射」的用法直接冲突：映射把
/// 公网流量转到本机网卡地址，回环绑定一律拒绝。此时外部只能看到
/// Connection refused，与被防火墙挡住、agent 没启动都无从区分——本次排查就
/// 卡在这里。提示一次比让人对着 refused 猜一小时划算。
pub(super) fn warn_loopback_on_mapped_host(addr: &std::net::SocketAddr, printer: Printer) {
    if !printer.is_human() || !addr.ip().is_loopback() {
        return;
    }
    printer.print_section(&crate::i18n::pick(
        printer.lang(),
        format!(
            "  提示    只监听本机；其他机器要连请 --listen 0.0.0.0:{} 并放行防火墙\n           \
             （配了端口映射的机器尤其注意：映射转发到内网后到达的是本机网卡地址，\n           \
             只绑回环会让外部一律 Connection refused）",
            addr.port()
        ),
        format!(
            "  hint    loopback only; use --listen 0.0.0.0:{} and open the firewall to accept other machines\n           \
             (this matters on a port-mapped host: the mapping forwards to this machine's NIC\n           \
             address, so a loopback bind turns every external attempt into a Connection refused)",
            addr.port()
        ),
    ));
}

/// 监听所有网卡时提醒暴露面
pub(super) fn warn_exposure(addr: &std::net::SocketAddr, printer: Printer) {
    if !printer.is_human() {
        return;
    }
    let parsed = *addr;
    if !parsed.ip().is_unspecified() {
        return;
    }
    printer.print_warning(&crate::i18n::pick(
        printer.lang(),
        format!(
            "正在监听所有网卡（{addr}）；端口长期开放，仅配对码保护。请用防火墙限制可达范围，或改用 --listen 127.0.0.1:{} 只开本机（跨机请优先用 SSH 隧道或 VPN）",
            parsed.port()
        ),
        format!(
            "listening on all interfaces ({addr}); the port stays open and is protected only by the pairing code. Restrict it with a firewall, or bind loopback with `--listen 127.0.0.1:{}` (prefer an SSH tunnel or VPN across machines)",
            parsed.port()
        ),
    ));
}

pub(super) fn report_serve(
    agent: &mcsup::remote::Agent,
    name: &str,
    addr: std::net::SocketAddr,
    printer: Printer,
) {
    let lang = printer.lang();
    if printer.is_human() {
        printer.print_section(&crate::i18n::pick(
            lang,
            format!(
                "mcsup 已开始监听\n  名称    {name}\n  地址    {addr}\n  指纹    {}\n  数据    {}",
                agent.fingerprint(),
                crate::config::platform_data_dir().display()
            ),
            format!(
                "mcsup is listening\n  name        {name}\n  address     {addr}\n  fingerprint {}\n  data        {}",
                agent.fingerprint(),
                crate::config::platform_data_dir().display()
            ),
        ));
        printer.print_section(&crate::i18n::pick(
            lang,
            "  配对码  未生成（按需发放）\n          需要配对时运行：mcsup remote code",
            "  pairing code none yet (on demand)\n              run `mcsup remote code` when you need one",
        ));
        warn_loopback_on_mapped_host(&addr, printer);
    } else {
        let payload = serde_json::json!({
            "name": name,
            "listen": addr.to_string(),
            "fingerprint": agent.fingerprint().as_str(),
            "pairing_code": null,
        });
        let _ = printer.print_outcome("", &payload);
    }
}

/// `remote code`：在本机生成一枚一次性配对码
///
/// 码写到 `{data_dir}/remote/grant.toml`（**只存摘要**），agent 在兑换时读它。
/// 因此本命令不需要 agent 在跑，也不需要与它通信——这是选落盘方案而非本地
/// IPC 的全部理由（见 `auth::grant` 的模块注释）。
///
/// 明文码只在这里出现一次：丢掉就重发，旧码自动失效。
pub(super) fn code(data_dir: &Path, ttl: u64, clear: bool, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    let grant = mcsup::auth::PairingGrant::new(data_dir, mcsup::auth::PairingWindow::default());
    if clear {
        grant.clear().context(crate::i18n::pick(
            lang,
            "作废配对码",
            "clearing the pairing code",
        ))?;
        return printer.print_outcome(
            &crate::i18n::pick(lang, "已作废当前配对码", "the pairing code was cleared"),
            &serde_json::json!({ "cleared": true, "pairing_code": null }),
        );
    }

    let code = grant
        .issue(mcsup::remote::now_secs(), ttl)
        .context(crate::i18n::pick(
            lang,
            "生成配对码（--ttl 需在 30..=3600 秒内）",
            "minting a pairing code (--ttl must be 30..=3600 seconds)",
        ))?;
    let running = crate::daemon::InstanceLock::is_held(data_dir).unwrap_or(false);
    if printer.is_human() {
        printer.print_section(&crate::i18n::pick(
            lang,
            format!(
                "配对码  {}\n        {} 分钟内有效，只能用一次；本命令只显示这一次",
                code.display_form(),
                ttl / 60
            ),
            format!(
                "pairing code {}\n             valid for {} minutes, single use; shown only once",
                code.display_form(),
                ttl / 60
            ),
        ));
        if !running {
            printer.print_warning(&crate::i18n::pick(
                lang,
                "本机 agent 未在运行；码已保存，启动 agent 后仍可用（到期即失效）",
                "the agent is not running; the code is stored and stays valid once it starts (until it expires)",
            ));
        }
    } else {
        printer.print_outcome(
            "",
            &serde_json::json!({
                "pairing_code": code.display_form(),
                "expires_in": ttl,
            }),
        )?;
    }
    Ok(())
}

/// `remote serve status`：离线读身份、名册、实例锁与配对码 TTL
///
/// 全程不连网络：状态查询不该因为 agent 卡住而卡住。
pub(super) fn serve_status(data_dir: &Path, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    let identity_dir = mcsup::remote::identity_dir(data_dir);
    if !identity_dir.join("cert.pem").exists() {
        return Err(AppDiagnostic::failure(
            crate::i18n::pick(
                lang,
                "本机尚未运行过 remote serve（没有身份文件）",
                "this machine has never run `remote serve` (no identity)",
            ),
            None,
        )
        .into());
    }
    let identity = mcsup::transport::ServerIdentity::load(&identity_dir).context(
        crate::i18n::pick(lang, "读取本机身份", "reading the local identity"),
    )?;
    let clients = ClientRegistry::new(mcsup::remote::clients_path(data_dir));
    let records = clients.load().context(crate::i18n::pick(
        lang,
        "读取客户端名册",
        "reading the client registry",
    ))?;
    // 存活判定看实例锁，不看 pid：pid 会被系统复用（`is_mcsup_process` 只能兜底）
    let running = crate::daemon::InstanceLock::is_held(data_dir).unwrap_or(false);
    let pid = read_pid(data_dir);
    let addr = read_listen_addr(data_dir);
    let grant = mcsup::auth::PairingGrant::new(data_dir, mcsup::auth::PairingWindow::default());
    let status = grant
        .status(mcsup::remote::now_secs())
        .context(crate::i18n::pick(
            lang,
            "读取配对码状态",
            "reading the pairing state",
        ))?;

    if printer.is_human() {
        render_serve_status(
            &identity,
            &records,
            running,
            pid,
            addr.as_deref(),
            status,
            printer,
        );
    } else {
        let payload = serde_json::json!({
            "running": running,
            "address": addr,
            "fingerprint": identity.fingerprint().as_str(),
            "pid": pid,
            "clients": records.iter().map(|record| serde_json::json!({
                "client_id": record.client_id.as_str(),
                "name": record.name,
                "role": record.role.to_string(),
            })).collect::<Vec<_>>(),
            "pairing": status.map(|status| serde_json::json!({
                "expires_in": status.remaining_secs,
                "cooling_down": status.cooling_down,
            })),
        });
        printer.print_outcome("", &payload)?;
    }
    Ok(())
}

/// `remote serve stop`：结束后台 agent
///
/// 三步：先按 pid 发信号，再等实例锁释放（真正的退出判据），最后清理 pid 文件。
/// 不把「信号发出去了」当成功：进程可能还在收尾在跑的任务。
pub(super) fn serve_stop(data_dir: &Path, force: bool, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    let held = crate::daemon::InstanceLock::is_held(data_dir).unwrap_or(false);
    let Some(pid) = read_pid(data_dir) else {
        // 没有 pid 文件但锁被持有：状态不一致，如实说清而不是假装成功
        if held {
            return Err(AppDiagnostic::failure(
                crate::i18n::pick(
                    lang,
                    "实例锁被持有但没有 pid 文件；无法安全定位进程，请手动检查",
                    "the instance lock is held but there is no pid file; find the process manually",
                ),
                None,
            )
            .into());
        }
        // 已在停止状态：幂等成功，与 `server stop` 同口径
        return printer.print_outcome(
            &crate::i18n::pick(lang, "本机 agent 未在运行", "the agent is not running"),
            &serde_json::json!({ "stopped": false, "pid": null }),
        );
    };
    if !is_alive(pid) {
        remove_pid(data_dir);
        return printer.print_outcome(
            &crate::i18n::pick(
                lang,
                format!("agent 进程 {pid} 已不在，已清理陈旧 pid 文件"),
                format!("agent process {pid} is gone; removed the stale pid file"),
            ),
            &serde_json::json!({ "stopped": false, "pid": pid }),
        );
    }
    // pid 会被系统复用：文件里的数字活着，不代表还是当初那个 agent
    if !is_mcsup_process(pid) {
        remove_pid(data_dir);
        return Err(AppDiagnostic::failure(
            crate::i18n::pick(
                lang,
                format!("pid {pid} 已被别的程序复用（镜像不是 mcsup），已清理陈旧 pid 文件"),
                format!("pid {pid} now belongs to a non-mcsup process; removed the stale pid file"),
            ),
            None,
        )
        .into());
    }

    terminate(pid, force).with_context(|| {
        crate::i18n::pick(
            lang,
            format!("终止 agent 进程 {pid}"),
            format!("terminating agent process {pid}"),
        )
    })?;

    // 等锁释放才是「真的停了」：SIGTERM 之后进程还要收尾
    let deadline = std::time::Instant::now() + STOP_WAIT;
    while std::time::Instant::now() < deadline {
        if !crate::daemon::InstanceLock::is_held(data_dir).unwrap_or(false) {
            remove_pid(data_dir);
            return printer.print_outcome(
                &crate::i18n::pick(
                    lang,
                    format!("已停止 agent（pid {pid}）"),
                    format!("stopped the agent (pid {pid})"),
                ),
                &serde_json::json!({ "stopped": true, "pid": pid }),
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Err(AppDiagnostic::failure(
        crate::i18n::pick(
            lang,
            format!("agent（pid {pid}）在 {STOP_WAIT:?} 内没有退出；用 --force 强杀"),
            format!("the agent (pid {pid}) did not exit within {STOP_WAIT:?}; retry with --force"),
        ),
        None,
    )
    .into())
}

/// 等 agent 自行退出的上限
pub(super) const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// `serve status` 的人类可读渲染
///
/// 抽出来只为让 `serve_status` 保持线性；两段文案（中 / 英）本来就长。
pub(super) fn render_serve_status(
    identity: &mcsup::transport::ServerIdentity,
    records: &[mcsup::auth::ClientRecord],
    running: bool,
    pid: Option<u32>,
    addr: Option<&str>,
    status: Option<mcsup::auth::GrantStatus>,
    printer: Printer,
) {
    let lang = printer.lang();
    let code_line = match status {
        Some(status) => crate::i18n::pick(
            lang,
            format!(
                "{} 分 {} 秒后过期{}",
                status.remaining_secs / 60,
                status.remaining_secs % 60,
                if status.cooling_down {
                    "（猜码冷却中）"
                } else {
                    ""
                }
            ),
            format!(
                "expires in {}m {}s{}",
                status.remaining_secs / 60,
                status.remaining_secs % 60,
                if status.cooling_down {
                    " (cooling down)"
                } else {
                    ""
                }
            ),
        ),
        None => crate::i18n::pick(lang, "无".to_owned(), "none".to_owned()),
    };
    printer.print_section(&crate::i18n::pick(
        lang,
        format!(
            "本机服务\n  状态    {}\n  地址    {}\n  指纹    {}\n  进程    {}\n  客户端  {}\n  配对码  {}",
            if running { "运行中" } else { "未运行" },
            addr.unwrap_or("-"),
            identity.fingerprint(),
            pid.map_or_else(|| "-".to_owned(), |pid| pid.to_string()),
            records.len(),
            code_line
        ),
        format!(
            "local service\n  state       {}\n  address     {}\n  fingerprint {}\n  process     {}\n  clients     {}\n  pairing     {}",
            if running { "running" } else { "stopped" },
            addr.unwrap_or("-"),
            identity.fingerprint(),
            pid.map_or_else(|| "-".to_owned(), |pid| pid.to_string()),
            records.len(),
            code_line
        ),
    ));
    if records.is_empty() {
        return;
    }
    let mut table = Table::new();
    table.set_header([
        crate::i18n::text(lang, "客户端", "client"),
        crate::i18n::text(lang, "名称", "name"),
        crate::i18n::text(lang, "角色", "role"),
    ]);
    for record in records {
        table.add_row([
            record.client_id.as_str().to_owned(),
            record.name.clone(),
            record.role.to_string(),
        ]);
    }
    printer.print_table(&table);
}

/// agent pid 文件名（`{data_dir}/remote/agent.pid`）
pub(super) const AGENT_PID: &str = "agent.pid";
