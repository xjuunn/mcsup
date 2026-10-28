//! `remote`：接入、状态与撤销
//!
//! 所有判定都在库里：配对码 / token / 角色在 `auth`，证书指纹与握手在
//! `transport`，资源列表在 `service`。这里只做参数搬运、终端交互与渲染。
//!
//! 接入分两步：`probe` 只建连并捕获对端证书指纹（不认证），用户确认指纹后
//! 才 `enroll` 换 token。把「确认」放在两步之间，是因为 TOFU 的信任锚就是
//! 用户看到的那个指纹——先配对再展示等于让用户事后追认。

use crate::cli::RemoteAction;
use crate::config::ConfigFile;
use crate::diagnostic::AppDiagnostic;
use crate::locale::Lang;
use crate::output::Printer;
use crate::prompts;
use anyhow::{Context, Result};
use comfy_table::Table;
use mcsup::auth::{ClientId, ClientRegistry, CredentialStore, CredentialVault, StoredCredential};
use mcsup::port::{Origin, RemoteName};
use mcsup::protocol::cli::{
    RemoteConnectJson, RemoteDisconnectJson, RemoteListJson, RemotePingJson, RemoteRevokeJson,
    RemoteRow, RemoteServeJson,
};
use mcsup::protocol::remote::{Capability, Request, Response};
use mcsup::service::{RemoteRecord, RemoteVia};
use mcsup::transport::{Client, Fingerprint, Probe, TransportError};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// 缺省端口：与 `mcsup-agent` 的缺省监听一致
const DEFAULT_PORT: u16 = 7681;

/// agent pid 文件名（`{data_dir}/remote/agent.pid`）
const AGENT_PID: &str = "agent.pid";

pub(crate) fn run(action: RemoteAction, printer: Printer) -> Result<()> {
    let file = ConfigFile::locate(None);
    match action {
        RemoteAction::ServeSsh { data_dir, name } => {
            let dir = data_dir.unwrap_or_else(crate::config::platform_data_dir);
            let name = name.unwrap_or_else(host_name);
            mcsup::remote::serve_ssh_main(&dir, &name).with_context(|| {
                crate::i18n::pick(printer.lang(), "SSH 引导 agent", "serving over ssh")
            })
        }
        RemoteAction::Serve {
            listen,
            name,
            stop,
            status,
        } => serve(&listen, name.as_deref(), stop, status, printer),
        RemoteAction::Connect {
            target,
            name,
            read_only,
            code,
        } => connect(
            &file,
            target.as_deref(),
            name.as_deref(),
            read_only,
            code,
            printer,
        ),
        RemoteAction::Reconnect { name } => reconnect(&file, &name, printer),
        RemoteAction::List => list(&file, printer),
        RemoteAction::Status { name } => status(&file, name.as_deref(), printer),
        RemoteAction::Ping { name } => ping(&file, &name, printer),
        RemoteAction::Disconnect { name } => disconnect(&file, &name, printer),
        RemoteAction::Revoke { client, listen } => revoke(&client, &listen, printer),
    }
}

/// `remote serve`：前台运行 agent，或 `--stop` / `--status`
fn serve(
    listen: &str,
    name: Option<&str>,
    stop: bool,
    status: bool,
    printer: Printer,
) -> Result<()> {
    let data_dir = crate::config::platform_data_dir();
    if status {
        return serve_status(&data_dir, printer);
    }
    if stop {
        return serve_stop(&data_dir, printer);
    }

    let name = name.map_or_else(host_name, str::to_owned);
    let agent = Arc::new(
        mcsup::remote::Agent::open(&data_dir, &name).with_context(|| {
            crate::i18n::pick(printer.lang(), "启动 agent", "starting the agent")
        })?,
    );
    let code = agent.issue_pairing_code().context(crate::i18n::pick(
        printer.lang(),
        "生成配对码",
        "generating a pairing code",
    ))?;
    let server = agent
        .bind(listen)
        .with_context(|| crate::i18n::pick(printer.lang(), "绑定监听", "binding the listener"))?;
    let addr = server.local_addr().context(crate::i18n::pick(
        printer.lang(),
        "读取监听地址",
        "reading the listen address",
    ))?;

    write_pid(&data_dir, std::process::id())?;
    report_serve(&agent, &name, &addr.to_string(), &code, printer);

    // 码自然过期时轮换：屏幕上始终有当前有效码。JSON 模式下每次变化打一行
    // JSONL，stdout 仍是纯数据。
    let watch = mcsup::remote::Agent::watch_pairing_code(&agent, move |code| {
        report_pairing_code(code, printer);
    });
    let result = server.run();
    drop(watch);
    remove_pid(&data_dir);
    result.context(crate::i18n::pick(
        printer.lang(),
        "agent 退出",
        "the agent exited",
    ))?;
    Ok(())
}

fn report_serve(
    agent: &mcsup::remote::Agent,
    name: &str,
    addr: &str,
    code: &mcsup::auth::PairingCode,
    printer: Printer,
) {
    let lang = printer.lang();
    let payload = RemoteServeJson {
        name,
        listen: addr,
        fingerprint: agent.fingerprint().as_str(),
        pairing_code: Some(&code.display_form()),
    };
    if printer.is_human() {
        printer.print_section(&crate::i18n::pick(
            lang,
            format!(
                "mcsup 已开始监听\n  名称    {name}\n  地址    {addr}\n  指纹    {}",
                agent.fingerprint()
            ),
            format!(
                "mcsup is listening\n  name        {name}\n  address     {addr}\n  fingerprint {}",
                agent.fingerprint()
            ),
        ));
        printer.print_section(&crate::i18n::pick(
            lang,
            format!(
                "  配对码  {}\n          10 分钟内有效，成功配对后自动更换",
                code.display_form()
            ),
            format!(
                "  pairing code {}\n               valid for 10 minutes, rotated after each pairing",
                code.display_form()
            ),
        ));
    } else {
        // JSON 模式：启动信息一行，后续配对码变化各一行 JSONL
        let _ = printer.print_outcome("", &payload);
    }
}

fn report_pairing_code(code: &mcsup::auth::PairingCode, printer: Printer) {
    if printer.is_human() {
        printer.print_section(&crate::i18n::pick(
            printer.lang(),
            format!("配对码  {}", code.display_form()),
            format!("pairing code {}", code.display_form()),
        ));
    } else {
        let payload = serde_json::json!({ "pairing_code": code.display_form() });
        let _ = printer.print_outcome("", &payload);
    }
}

/// `remote serve --status`：离线读身份、名册与 pid
fn serve_status(data_dir: &Path, printer: Printer) -> Result<()> {
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
    let pid = read_pid(data_dir);
    let running = pid.is_some_and(is_alive);

    if printer.is_human() {
        printer.print_section(&crate::i18n::pick(
            lang,
            format!(
                "本机服务\n  指纹    {}\n  进程    {}\n  客户端  {}",
                identity.fingerprint(),
                pid.map_or_else(|| "未运行".to_owned(), |pid| pid.to_string()),
                records.len()
            ),
            format!(
                "local service\n  fingerprint {}\n  process     {}\n  clients     {}",
                identity.fingerprint(),
                pid.map_or_else(|| "not running".to_owned(), |pid| pid.to_string()),
                records.len()
            ),
        ));
        if !records.is_empty() {
            let mut table = Table::new();
            table.set_header([
                crate::i18n::text(lang, "客户端", "client"),
                crate::i18n::text(lang, "名称", "name"),
                crate::i18n::text(lang, "角色", "role"),
            ]);
            for record in &records {
                table.add_row([
                    record.client_id.as_str().to_owned(),
                    record.name.clone(),
                    record.role.to_string(),
                ]);
            }
            printer.print_table(&table);
        }
        if !running && pid.is_some() {
            printer.print_warning(&crate::i18n::pick(
                lang,
                "pid 文件存在但进程已不在，可运行 remote serve --stop 清理",
                "the pid file is stale; run `remote serve --stop` to clean it up",
            ));
        }
    } else {
        let payload = serde_json::json!({
            "fingerprint": identity.fingerprint().as_str(),
            "running": running,
            "pid": pid,
            "clients": records.iter().map(|record| serde_json::json!({
                "client_id": record.client_id.as_str(),
                "name": record.name,
                "role": record.role.to_string(),
            })).collect::<Vec<_>>(),
        });
        printer.print_outcome("", &payload)?;
    }
    Ok(())
}

/// `remote serve --stop`：按 pid 文件终止前台 agent
fn serve_stop(data_dir: &Path, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    let Some(pid) = read_pid(data_dir) else {
        return Err(AppDiagnostic::failure(
            crate::i18n::pick(
                lang,
                "本机没有正在运行的 agent（无 pid 文件）",
                "no agent is running on this machine (no pid file)",
            ),
            None,
        )
        .into());
    };
    if !is_alive(pid) {
        remove_pid(data_dir);
        return Err(AppDiagnostic::failure(
            crate::i18n::pick(
                lang,
                format!("agent 进程 {pid} 已不在，已清理陈旧 pid 文件"),
                format!("agent process {pid} is gone; removed the stale pid file"),
            ),
            None,
        )
        .into());
    }
    terminate(pid).with_context(|| {
        crate::i18n::pick(
            lang,
            format!("终止 agent 进程 {pid}"),
            format!("terminating agent process {pid}"),
        )
    })?;
    remove_pid(data_dir);
    printer.print_outcome(
        &crate::i18n::pick(
            lang,
            format!("已停止 agent（pid {pid}）"),
            format!("stopped the agent (pid {pid})"),
        ),
        &serde_json::json!({ "stopped": true, "pid": pid }),
    )
}

/// `remote connect`：probe → 确认指纹 → enroll → 落盘
fn connect(
    file: &ConfigFile,
    target: Option<&str>,
    name: Option<&str>,
    read_only: bool,
    code: Option<String>,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let Some(raw) = target else {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                "局域网自动发现尚未实现；请用 mcsup remote connect <host[:port]>",
                "LAN discovery is not implemented yet; use `mcsup remote connect <host[:port]>`",
            ),
            None,
        )
        .into());
    };
    if raw.contains('@') {
        return connect_ssh(file, raw, name, read_only, printer);
    }

    let target = normalize_target(raw);
    let probe = Client::probe(&target).with_context(|| {
        crate::i18n::pick(
            lang,
            format!("连接 {target} 失败"),
            format!("failed to connect to {target}"),
        )
    })?;
    let fingerprint = confirm_fingerprint(&probe, printer)?;
    let remote_name = resolve_remote_name(name, file, &target, printer)?;
    let code = resolve_pairing_code(code, &probe, printer)?;
    let client_name = client_display_name();

    let client = Client::new(&target, fingerprint.clone());
    let enrolled = client
        .enroll(&code, &client_name, read_only)
        .with_context(|| crate::i18n::pick(lang, "配对失败", "pairing failed"))?;

    let credentials = credential_store();
    credentials
        .put(
            remote_name.as_str(),
            &StoredCredential {
                token: enrolled.token.clone(),
                client_id: enrolled.client_id.as_str().to_owned(),
                role: enrolled.role.to_string(),
            },
        )
        .context(crate::i18n::pick(lang, "保存凭证", "saving the credential"))?;

    let mut cfg = file.load()?;
    cfg.put_remote(
        &remote_name,
        RemoteRecord {
            target: target.clone(),
            via: RemoteVia::Direct,
            read_only,
            added_at: unix_now(),
            fingerprint: Some(fingerprint.as_str().to_owned()),
            credential: Some(credentials.describe()),
        },
    );
    file.save(&cfg)?;

    report_connected(
        &remote_name,
        &target,
        &target,
        &fingerprint,
        &enrolled.role.to_string(),
        printer,
    )
}

/// SSH 引导配对：复用已有 SSH 信任，不要求用户抄配对码
///
/// 流程：SSH 起远端 `__serve-ssh` → 读回 `{addr, fingerprint, pairing_code}` →
/// 建本地隧道 → 走与直连完全相同的 probe / enroll / 落盘路径。记录里的
/// `via` 是 `ssh`、`target` 是原始 `user@host`；后续命令由装配层按需重建隧道。
///
/// # Errors
///
/// SSH 不可用、远端缺 mcsup、隧道失败、配对被拒时返回错误
fn connect_ssh(
    file: &ConfigFile,
    target: &str,
    name: Option<&str>,
    read_only: bool,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let (user, host) = crate::ssh::parse_user_host(target, lang)?;
    let ssh_target = format!("{user}@{host}");
    // 建立会话（引导 + 隧道）后，握手里的配对码直接用来配对：用户不必再从
    // 对端屏幕上抄一次。会话登记在进程内，后面 report_connected 复用同一条。
    let (local, handshake) = crate::ssh::session(&ssh_target, lang)?;

    let probe = Client::probe(&local).with_context(|| {
        crate::i18n::pick(
            lang,
            format!("通过 SSH 隧道连接 {ssh_target} 失败"),
            format!("failed to connect to {ssh_target} over ssh"),
        )
    })?;
    // 指纹必须与远端自报的一致（probe 已校验），用户仍需确认——SSH 只证明
    // 「能登上去」，不证明「agent 是同一台机器上的那个」
    let fingerprint = confirm_fingerprint(&probe, printer)?;
    let remote_name = resolve_remote_name(name, file, &ssh_target, printer)?;
    let client_name = client_display_name();
    let client = Client::new(&local, fingerprint.clone());
    let enrolled = client
        .enroll(&handshake.pairing_code, &client_name, read_only)
        .with_context(|| crate::i18n::pick(lang, "SSH 配对失败", "ssh pairing failed"))?;

    let credentials = credential_store();
    credentials
        .put(
            remote_name.as_str(),
            &StoredCredential {
                token: enrolled.token.clone(),
                client_id: enrolled.client_id.as_str().to_owned(),
                role: enrolled.role.to_string(),
            },
        )
        .context(crate::i18n::pick(lang, "保存凭证", "saving the credential"))?;
    let mut cfg = file.load()?;
    cfg.put_remote(
        &remote_name,
        RemoteRecord {
            // 原始 user@host：后续每条命令都按原样重放这条接入路径
            target: ssh_target.clone(),
            via: RemoteVia::Ssh,
            read_only,
            added_at: unix_now(),
            fingerprint: Some(fingerprint.as_str().to_owned()),
            credential: Some(credentials.describe()),
        },
    );
    file.save(&cfg)?;

    // 摘要走隧道地址：此时的 agent 还在本次会话里，不能用 user@host 当 TCP 地址
    report_connected(
        &remote_name,
        &ssh_target,
        &local,
        &fingerprint,
        &enrolled.role.to_string(),
        printer,
    )
}

/// `remote reconnect`：复用记录，校验指纹后重新认证
fn reconnect(file: &ConfigFile, name: &str, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    let remote_name = crate::commands::origin::remote_name(name, lang)?;
    let cfg = file.load()?;
    let record = cfg.remote(&remote_name).ok_or_else(|| {
        AppDiagnostic::not_found(
            crate::i18n::pick(
                lang,
                format!("远端 {name} 未配对"),
                format!("remote {name} is not paired"),
            ),
            None,
        )
    })?;
    let Some(fingerprint_text) = record.fingerprint.as_deref() else {
        return Err(AppDiagnostic::unauthorized(
            crate::i18n::pick(
                lang,
                format!("远端 {name} 缺少指纹，请重新 connect"),
                format!("remote {name} has no pinned fingerprint; run `connect` again"),
            ),
            None,
        )
        .into());
    };
    let fingerprint = Fingerprint::parse(fingerprint_text).context(crate::i18n::pick(
        lang,
        "解析已存指纹",
        "parsing the stored fingerprint",
    ))?;
    let credential = credential_store()
        .get(remote_name.as_str())
        .context(crate::i18n::pick(
            lang,
            "读取凭证",
            "reading the credential",
        ))?;

    // 先解析实际可连地址（SSH 记录要先建隧道），再 probe 确认对端仍是同一张
    // 证书，最后认证：指纹变化必须显式重新配对
    let endpoint = crate::ssh::endpoint(record, lang)?;
    let probe = Client::probe(&endpoint)?;
    if probe.fingerprint != fingerprint {
        return Err(AppDiagnostic::unauthorized(
            crate::i18n::pick(
                lang,
                format!(
                    "远端 {name} 的证书指纹已变化（{} → {}）；请重新运行 remote connect",
                    fingerprint, probe.fingerprint
                ),
                format!(
                    "the certificate fingerprint of {name} changed ({} → {}); run `remote connect` again",
                    fingerprint, probe.fingerprint
                ),
            ),
            None,
        )
        .into());
    }
    let client = Client::new(&endpoint, fingerprint.clone()).with_token(credential.token);
    client.call(&Request::Ping).context(crate::i18n::pick(
        lang,
        "认证失败",
        "authentication failed",
    ))?;

    report_connected(
        &remote_name,
        &record.target,
        &endpoint,
        &fingerprint,
        &credential.role,
        printer,
    )
}

/// `remote list`：每台远端做一次短探测
fn list(file: &ConfigFile, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    let cfg = file.load()?;
    let credentials = credential_store();
    let rows: Vec<RemoteRow> = cfg
        .remotes
        .iter()
        .map(|(name, record)| probe_row(name, record, &credentials, lang))
        .collect();
    if !printer.is_human() {
        return printer.print_outcome("", &RemoteListJson { items: rows });
    }
    printer.print_rows(
        &rows,
        &crate::i18n::pick(
            lang,
            "尚未配对任何远端，运行 mcsup remote connect",
            "no remotes paired; run `mcsup remote connect`",
        ),
        || {
            let mut table = Table::new();
            table.set_header([
                crate::i18n::text(lang, "名称", "name"),
                crate::i18n::text(lang, "目标", "target"),
                crate::i18n::text(lang, "状态", "state"),
                crate::i18n::text(lang, "角色", "role"),
                crate::i18n::text(lang, "版本", "version"),
            ]);
            for row in &rows {
                table.add_row([
                    row.name.clone(),
                    row.target.clone(),
                    row.state.clone(),
                    row.role.clone().unwrap_or_else(|| "-".to_owned()),
                    row.agent_version.clone().unwrap_or_else(|| "-".to_owned()),
                ]);
            }
            table
        },
    )
}

/// `remote status`：单台详情（缺名时列全部）
fn status(file: &ConfigFile, name: Option<&str>, printer: Printer) -> Result<()> {
    let Some(name) = name else {
        return list(file, printer);
    };
    let lang = printer.lang();
    let remote_name = crate::commands::origin::remote_name(name, lang)?;
    let cfg = file.load()?;
    let record = cfg.remote(&remote_name).ok_or_else(|| {
        AppDiagnostic::not_found(
            crate::i18n::pick(
                lang,
                format!("远端 {name} 未配对"),
                format!("remote {name} is not paired"),
            ),
            None,
        )
    })?;
    let row = probe_row(remote_name.as_str(), record, &credential_store(), lang);
    if printer.is_human() {
        printer.print_section(&crate::i18n::pick(
            lang,
            format!(
                "远端 {}\n  目标    {}\n  接入    {}\n  状态    {}\n  角色    {}\n  版本    {}\n  指纹    {}",
                row.name,
                row.target,
                row.via,
                row.state,
                row.role.as_deref().unwrap_or("-"),
                row.agent_version.as_deref().unwrap_or("-"),
                row.fingerprint.as_deref().unwrap_or("-")
            ),
            format!(
                "remote {}\n  target      {}\n  via         {}\n  state       {}\n  role        {}\n  version     {}\n  fingerprint {}",
                row.name,
                row.target,
                row.via,
                row.state,
                row.role.as_deref().unwrap_or("-"),
                row.agent_version.as_deref().unwrap_or("-"),
                row.fingerprint.as_deref().unwrap_or("-")
            ),
        ));
        Ok(())
    } else {
        printer.print_outcome("", &row)
    }
}

/// `remote ping`：probe 拿能力，认证后 Ping 拿版本 / uptime
fn ping(file: &ConfigFile, name: &str, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    let remote_name = crate::commands::origin::remote_name(name, lang)?;
    let cfg = file.load()?;
    let record = cfg.remote(&remote_name).ok_or_else(|| {
        AppDiagnostic::not_found(
            crate::i18n::pick(
                lang,
                format!("远端 {name} 未配对"),
                format!("remote {name} is not paired"),
            ),
            None,
        )
    })?;
    let fingerprint_text = record.fingerprint.as_deref().ok_or_else(|| {
        AppDiagnostic::unauthorized(
            crate::i18n::pick(
                lang,
                format!("远端 {name} 缺少指纹"),
                format!("remote {name} has no pinned fingerprint"),
            ),
            None,
        )
    })?;
    let fingerprint = Fingerprint::parse(fingerprint_text).context(crate::i18n::pick(
        lang,
        "解析已存指纹",
        "parsing the stored fingerprint",
    ))?;
    let credential = credential_store()
        .get(remote_name.as_str())
        .context(crate::i18n::pick(
            lang,
            "读取凭证",
            "reading the credential",
        ))?;

    let endpoint = crate::ssh::endpoint(record, lang)?;
    let probe = Client::probe(&endpoint)?;
    let client = Client::new(&endpoint, fingerprint).with_token(credential.token);
    let pong =
        client
            .call(&Request::Ping)
            .context(crate::i18n::pick(lang, "ping 失败", "ping failed"))?;
    let Response::Pong {
        agent_version,
        uptime_secs,
    } = pong
    else {
        return Err(anyhow::anyhow!(
            "{}",
            crate::i18n::pick(
                lang,
                "对端返回了非预期帧",
                "the peer returned an unexpected frame"
            )
        ));
    };
    let capabilities: Vec<&str> = Capability::ALL
        .iter()
        .filter(|cap| probe.greeting.capabilities.has(**cap))
        .map(|cap| capability_name(*cap))
        .collect();
    let payload = RemotePingJson {
        name: remote_name.as_str(),
        agent_version: &agent_version,
        protocol_version: mcsup::protocol::remote::REMOTE_PROTOCOL_VERSION,
        uptime_secs,
        capabilities,
    };
    if printer.is_human() {
        printer.print_section(&crate::i18n::pick(
            lang,
            format!(
                "远端 {} 可达\n  agent   {}\n  运行    {} 秒\n  能力    {}",
                remote_name,
                agent_version,
                uptime_secs,
                payload.capabilities.join(", ")
            ),
            format!(
                "remote {} is reachable\n  agent       {}\n  uptime      {}s\n  capabilities {}",
                remote_name,
                agent_version,
                uptime_secs,
                payload.capabilities.join(", ")
            ),
        ));
        Ok(())
    } else {
        printer.print_outcome("", &payload)
    }
}

/// `remote disconnect`：删本地配对与凭证
fn disconnect(file: &ConfigFile, name: &str, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    let remote_name = crate::commands::origin::remote_name(name, lang)?;
    let mut cfg = file.load()?;
    let removed = cfg.remove_remote(&remote_name).is_some();
    if removed {
        file.save(&cfg)?;
    }
    credential_store()
        .remove(remote_name.as_str())
        .context(crate::i18n::pick(
            lang,
            "删除凭证",
            "removing the credential",
        ))?;
    printer.print_outcome(
        &crate::i18n::pick(
            lang,
            format!("已断开 {name}（远端服务未受影响）"),
            format!("disconnected {name} (the remote service is unaffected)"),
        ),
        &RemoteDisconnectJson {
            name: remote_name.as_str(),
            removed,
        },
    )
}

/// `remote revoke`：在 serve 端离线删除客户端记录
fn revoke(client: &str, listen: &str, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    // `listen` 只用于提示当前实现是本地离线操作；保留参数让未来能切到 RPC
    let _ = listen;
    let data_dir = crate::config::platform_data_dir();
    let registry = ClientRegistry::new(mcsup::remote::clients_path(&data_dir));
    let records = registry.load().context(crate::i18n::pick(
        lang,
        "读取客户端名册",
        "reading the client registry",
    ))?;
    let target = records
        .iter()
        .find(|record| record.client_id.as_str() == client || record.name == client)
        .ok_or_else(|| {
            AppDiagnostic::not_found(
                crate::i18n::pick(
                    lang,
                    format!("没有匹配的客户端: {client}"),
                    format!("no matching client: {client}"),
                ),
                None,
            )
        })?;
    let client_id = ClientId::parse(target.client_id.as_str()).context(crate::i18n::pick(
        lang,
        "解析客户端身份",
        "parsing the client id",
    ))?;
    let removed = registry.revoke(&client_id).context(crate::i18n::pick(
        lang,
        "撤销客户端",
        "revoking the client",
    ))?;
    printer.print_outcome(
        &crate::i18n::pick(
            lang,
            format!("已撤销客户端 {}（{}）", target.name, target.client_id),
            format!("revoked client {} ({})", target.name, target.client_id),
        ),
        &RemoteRevokeJson {
            client: target.client_id.as_str(),
            removed,
        },
    )
}

/// 连接成功后的摘要：同步远端元数据并报数
///
/// `target` 是**用户看到的**目标（`host:port` 或 `user@host`），`endpoint` 是
/// 本次实际可连的地址。两者对 SSH 接入不同：隧道端口是临时分配的实现细节，
/// 写进输出会让人以为下次还能用。
fn report_connected(
    name: &RemoteName,
    target: &str,
    endpoint: &str,
    fingerprint: &Fingerprint,
    role: &str,
    printer: Printer,
) -> Result<()> {
    let lang = printer.lang();
    let credential = credential_store().get(name.as_str())?;
    let client = Client::new(endpoint, fingerprint.clone()).with_token(credential.token);
    let workspaces = count(&client, &Request::WorkspaceList)?;
    let servers = count(&client, &Request::ServerList)?;
    let java = count(&client, &Request::JavaList)?;

    let payload = RemoteConnectJson {
        name: name.as_str(),
        target,
        fingerprint: fingerprint.as_str(),
        role,
        workspaces,
        servers,
        java,
    };
    if printer.is_human() {
        printer.print_section(&crate::i18n::pick(
            lang,
            format!(
                "已连接 {name}\n  同步 {workspaces} 个工作区、{servers} 台服务器、{java} 个 Java 运行时\n  指纹 {fingerprint}（已保存）"
            ),
            format!(
                "connected to {name}\n  synced {workspaces} workspaces, {servers} servers, {java} Java runtimes\n  fingerprint {fingerprint} (saved)"
            ),
        ));
        Ok(())
    } else {
        printer.print_outcome("", &payload)
    }
}

/// 数一下某类资源有多少条
fn count(client: &Client, request: &Request) -> Result<usize> {
    let response = client.call(request)?;
    Ok(match response {
        Response::Workspaces { items } => items.len(),
        Response::Servers { items } => items.len(),
        Response::Javas { items } => items.len(),
        other => {
            return Err(anyhow::anyhow!(
                "unexpected response for {}: {}",
                request.operation(),
                response_label(&other)
            ));
        }
    })
}

/// 一台远端的探测结果
fn probe_row(
    name: &str,
    record: &RemoteRecord,
    credentials: &dyn CredentialStore,
    lang: Lang,
) -> RemoteRow {
    let credential = credentials.get(name).ok();
    let (state, agent_version) = probe_state(record, credential.as_ref(), lang);
    RemoteRow {
        name: name.to_owned(),
        target: record.target.clone(),
        via: record.via.as_str().to_owned(),
        state: state.to_owned(),
        role: credential.as_ref().map(|cred| cred.role.clone()),
        agent_version,
        fingerprint: record.fingerprint.clone(),
        read_only: record.read_only,
    }
}

/// 状态探测：`ok` / `unreachable` / `unauthorized`
///
/// SSH 记录的 `unreachable` 包含「隧道建不起来」（SSH 不可用、远端没装
/// mcsup）：对列表来说两者是同一件事——这台远端现在用不了。
fn probe_state(
    record: &RemoteRecord,
    credential: Option<&StoredCredential>,
    lang: Lang,
) -> (&'static str, Option<String>) {
    let Some(fingerprint_text) = record.fingerprint.as_deref() else {
        return ("unauthorized", None);
    };
    let Ok(fingerprint) = Fingerprint::parse(fingerprint_text) else {
        return ("unauthorized", None);
    };
    let Some(credential) = credential else {
        return ("unauthorized", None);
    };
    let endpoint = match crate::ssh::endpoint(record, lang) {
        Ok(endpoint) => endpoint,
        Err(err) => {
            tracing::warn!(remote = %record.target, %err, "ssh session failed; reporting unreachable");
            return ("unreachable", None);
        }
    };
    let client = Client::new(&endpoint, fingerprint).with_token(credential.token.clone());
    match client.call(&Request::Ping) {
        Ok(Response::Pong { agent_version, .. }) => ("ok", Some(agent_version)),
        Ok(_) => ("ok", None),
        Err(TransportError::Unauthorized(_)) => ("unauthorized", None),
        Err(TransportError::Remote { kind, .. })
            if kind == mcsup::protocol::remote::ErrorKind::UNAUTHORIZED =>
        {
            ("unauthorized", None)
        }
        Err(_) => ("unreachable", None),
    }
}

/// 确认指纹：交互下让用户核对，非交互下要求 `--yes` 或直接拒绝
fn confirm_fingerprint(probe: &Probe, printer: Printer) -> Result<Fingerprint> {
    let lang = printer.lang();
    if !printer.is_interactive() {
        if printer.assumes_yes() {
            return Ok(probe.fingerprint.clone());
        }
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("首次连接需要确认证书指纹：{}", probe.fingerprint),
                format!(
                    "first connection requires confirming the certificate fingerprint: {}",
                    probe.fingerprint
                ),
            ),
            Some(crate::i18n::pick(
                lang,
                "非交互下加 --yes 表示接受该指纹",
                "pass --yes to accept this fingerprint in non-interactive mode",
            )),
        )
        .into());
    }
    let message = crate::i18n::pick(
        lang,
        format!(
            "首次连接 {}（{}），指纹 {}。确认信任这台机器？",
            probe.greeting.server_name, probe.greeting.agent_version, probe.fingerprint
        ),
        format!(
            "connect to {} ({}) with fingerprint {}?",
            probe.greeting.server_name, probe.greeting.agent_version, probe.fingerprint
        ),
    );
    if prompts::confirm_yes(lang, &message, false)? {
        Ok(probe.fingerprint.clone())
    } else {
        Err(AppDiagnostic::usage(
            crate::i18n::pick(lang, "已取消配对", "pairing cancelled"),
            None,
        )
        .into())
    }
}

/// 配对码：显式给就用；否则交互输入；非交互报用法错误
fn resolve_pairing_code(code: Option<String>, probe: &Probe, printer: Printer) -> Result<String> {
    if let Some(code) = code {
        return Ok(code);
    }
    let lang = printer.lang();
    if !printer.is_interactive() {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                "非交互配对需要 --code <配对码>",
                "non-interactive pairing requires --code <pairing-code>",
            ),
            Some(crate::i18n::pick(
                lang,
                "配对码显示在对端屏幕上（mcsup remote serve）",
                "the pairing code is shown on the peer's screen (`mcsup remote serve`)",
            )),
        )
        .into());
    }
    let _ = probe;
    prompts::input_text(
        lang,
        &crate::i18n::pick(lang, "配对码", "pairing code"),
        "",
        &crate::i18n::pick(
            lang,
            "输入对端屏幕上显示的配对码",
            "enter the code shown on the peer",
        ),
    )
}

/// 远端名：显式 > 交互命名 > 默认 `r1` / `r2` 递增
///
/// 显式 `--name` 与非交互（JSON / 非 TTY）都不提问。默认名取当前配置里
/// 最小的空闲 `rN`；同一 target 已有记录时沿用其名字，保证 `connect` 幂等。
/// 交互下用户可直接回车；若输入的名字已配对到另一台机器，警示后重问，
/// 避免一次回车静默覆盖别的远端。
fn resolve_remote_name(
    name: Option<&str>,
    file: &ConfigFile,
    target: &str,
    printer: Printer,
) -> Result<RemoteName> {
    let lang = printer.lang();
    if let Some(name) = name {
        return crate::commands::origin::remote_name(name, lang);
    }
    let cfg = file.load()?;
    let candidate = cfg
        .remotes
        .iter()
        .find(|(_, record)| record.target == target)
        .map_or_else(
            || next_remote_name(cfg.remote_names()),
            |(name, _)| name.clone(),
        );
    if !printer.is_interactive() {
        return crate::commands::origin::remote_name(&candidate, lang);
    }
    loop {
        let entered = prompts::input_remote_name(
            lang,
            &crate::i18n::pick(lang, "为远程设备命名", "name this remote"),
            &candidate,
        )?;
        let name = crate::commands::origin::remote_name(&entered, lang)?;
        match cfg.remote(&name) {
            Some(record) if record.target != target => {
                printer.print_warning(&crate::i18n::pick(
                    lang,
                    format!(
                        "远端名 {} 已用于 {}；换一个，或先 remote disconnect {}",
                        name, record.target, name
                    ),
                    format!(
                        "remote name {} is already used by {}; pick another or run `remote disconnect {}` first",
                        name, record.target, name
                    ),
                ));
            }
            _ => return Ok(name),
        }
    }
}

/// 默认远端名：最小的空闲 `rN`（`r1`、`r2` …）
///
/// 只把形如 `r<数字>` 的名字计入占用；用户自定义的其它名字不影响序号。
fn next_remote_name<'a>(existing: impl Iterator<Item = &'a str>) -> String {
    let used: HashSet<usize> = existing
        .filter_map(|name| name.strip_prefix('r'))
        .filter_map(|digits| digits.parse().ok())
        .collect();
    let mut n = 1;
    while used.contains(&n) {
        n += 1;
    }
    format!("r{n}")
}

/// 目标归一：无端口补 `:7681`
fn normalize_target(raw: &str) -> String {
    let raw = raw.trim();
    // IPv6 字面量 `[::1]` / `[::1]:7681` 不再拆；其余按最后一个 `:` 判端口
    if raw.starts_with('[') {
        return if raw.contains("]:") {
            raw.to_owned()
        } else {
            format!("{raw}:{DEFAULT_PORT}")
        };
    }
    match raw.rsplit_once(':') {
        Some((_, port)) if port.parse::<u16>().is_ok() => raw.to_owned(),
        _ => format!("{raw}:{DEFAULT_PORT}"),
    }
}

/// 客户端自称：主机名，失败用 `mcsup`
fn client_display_name() -> String {
    host_name()
}

/// 本机名
fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "mcsup".to_owned())
}

/// 客户端凭证存储：优先系统钥匙串，无钥匙串时退到 `{data_dir}/credentials.toml`
fn credential_store() -> CredentialVault {
    CredentialVault::open(crate::config::platform_data_dir().join("credentials.toml"))
}

fn capability_name(capability: Capability) -> &'static str {
    match capability {
        Capability::Backup => "backup",
        Capability::File => "file",
        Capability::Java => "java",
        Capability::Streaming => "streaming",
        Capability::Installer => "installer",
        _ => "unknown",
    }
}

fn response_label(response: &Response) -> &'static str {
    match response {
        Response::Hello { .. } => "hello",
        Response::Enrolled { .. } => "enrolled",
        Response::Authenticated { .. } => "authenticated",
        Response::Pong { .. } => "pong",
        Response::Ok => "ok",
        Response::Workspaces { .. } => "workspaces",
        Response::Servers { .. } => "servers",
        Response::Javas { .. } => "javas",
        Response::Clients { .. } => "clients",
        Response::Statuses { .. } => "statuses",
        Response::Started { .. } => "started",
        Response::Stopped { .. } => "stopped",
        Response::LogLines { .. } => "log_lines",
        Response::Removed { .. } => "removed",
        Response::Linked { .. } => "linked",
        Response::Variants { .. } => "variants",
        Response::Versions { .. } => "versions",
        Response::SnapshotCreated { .. } => "snapshot_created",
        Response::Snapshots { .. } => "snapshots",
        Response::Restored { .. } => "restored",
        Response::Props { .. } => "props",
        Response::SettingsChanged { .. } => "settings_changed",
        Response::ModpackApplied { .. } => "modpack_applied",
        Response::JavaUninstalled { .. } => "java_uninstalled",
        Response::SnapshotDeleted { .. } => "snapshot_deleted",
        Response::SnapshotVerified { .. } => "snapshot_verified",
        Response::Pruned { .. } => "pruned",
        Response::Error { .. } => "error",
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs().cast_signed())
}

/// pid 文件路径
fn pid_path(data_dir: &Path) -> PathBuf {
    data_dir.join("remote").join(AGENT_PID)
}

fn write_pid(data_dir: &Path, pid: u32) -> Result<()> {
    let path = pid_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录 {}", parent.display()))?;
    }
    std::fs::write(&path, pid.to_string())
        .with_context(|| format!("写入 pid 文件 {}", path.display()))
}

fn read_pid(data_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_path(data_dir))
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

fn remove_pid(data_dir: &Path) {
    let _ = std::fs::remove_file(pid_path(data_dir));
}

/// 进程是否存活（`kill -0` / `tasklist`，不引平台库）
fn is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// 终止进程
fn terminate(pid: u32) -> Result<()> {
    #[cfg(unix)]
    let status = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
    #[cfg(windows)]
    let status = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status();
    #[cfg(not(any(unix, windows)))]
    let status: std::io::Result<std::process::ExitStatus> = Err(std::io::Error::other(
        "terminating a process is not supported on this platform",
    ));
    let status = status?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("terminate {pid} exited with {status}"))
    }
}

/// 供 `--origin` 解析使用的来源文本（测试与提示共用）
#[allow(dead_code)]
fn origin_label(origin: &Origin) -> String {
    origin.as_str().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_gets_default_port_when_missing() {
        assert_eq!(normalize_target("10.0.0.5"), "10.0.0.5:7681");
        assert_eq!(normalize_target("10.0.0.5:9000"), "10.0.0.5:9000");
        assert_eq!(normalize_target("prod-box"), "prod-box:7681");
        assert_eq!(normalize_target("[::1]"), "[::1]:7681");
        assert_eq!(normalize_target("[::1]:9000"), "[::1]:9000");
    }

    #[test]
    fn default_remote_names_increment_from_r1() {
        assert_eq!(next_remote_name(std::iter::empty()), "r1");
        assert_eq!(next_remote_name(["r1"].into_iter()), "r2");
        assert_eq!(next_remote_name(["r1", "r2"].into_iter()), "r3");
        assert_eq!(next_remote_name(["r2"].into_iter()), "r1");
        assert_eq!(next_remote_name(["r1", "r3"].into_iter()), "r2");
        assert_eq!(next_remote_name(["prod-box", "r1"].into_iter()), "r2");
        assert_eq!(next_remote_name(["rX", "r2"].into_iter()), "r1");
    }

    #[test]
    fn pid_file_roundtrip_and_cleanup() {
        let dir = tempfile::tempdir().expect("tmp");
        assert_eq!(read_pid(dir.path()), None);
        write_pid(dir.path(), 4242).expect("write");
        assert_eq!(read_pid(dir.path()), Some(4242));
        remove_pid(dir.path());
        assert_eq!(read_pid(dir.path()), None);
    }

    #[test]
    fn capability_names_are_stable() {
        assert_eq!(capability_name(Capability::Backup), "backup");
        assert_eq!(capability_name(Capability::Streaming), "streaming");
        assert_eq!(capability_name(Capability::Installer), "installer");
    }

    #[test]
    fn origin_label_matches_display() {
        assert_eq!(origin_label(&Origin::Local), "local");
        assert_eq!(
            origin_label(&Origin::Remote(
                RemoteName::parse("prod-box").expect("name")
            )),
            "prod-box"
        );
    }

    fn record(target: &str, fingerprint: Option<&str>) -> RemoteRecord {
        RemoteRecord {
            target: target.to_owned(),
            via: RemoteVia::Direct,
            read_only: false,
            added_at: 1,
            fingerprint: fingerprint.map(str::to_owned),
            credential: None,
        }
    }

    #[test]
    fn probe_state_is_unauthorized_without_fingerprint_or_credential() {
        let no_fp = record("127.0.0.1:1", None);
        assert_eq!(probe_state(&no_fp, None, Lang::Zh).0, "unauthorized");

        let with_fp = record("127.0.0.1:1", Some(&Fingerprint::of_der(b"x").to_string()));
        assert_eq!(probe_state(&with_fp, None, Lang::Zh).0, "unauthorized");
    }

    #[test]
    fn probe_state_is_unreachable_when_nothing_listens() {
        let fp = Fingerprint::of_der(b"x");
        let rec = record("127.0.0.1:1", Some(fp.as_str()));
        let cred = StoredCredential {
            token: "a".repeat(64),
            client_id: "0f0e0d0c-0b0a-49d8-a716-446655440000".to_owned(),
            role: "operator".to_owned(),
        };
        assert_eq!(probe_state(&rec, Some(&cred), Lang::Zh).0, "unreachable");
    }

    #[test]
    fn probe_row_carries_config_fields() {
        let fp = Fingerprint::of_der(b"x");
        let rec = record("10.0.0.5:7681", Some(fp.as_str()));
        let cred = StoredCredential {
            token: "a".repeat(64),
            client_id: "0f0e0d0c-0b0a-49d8-a716-446655440000".to_owned(),
            role: "viewer".to_owned(),
        };
        let store = MemCreds(Some(cred));
        let row = probe_row("prod-box", &rec, &store, Lang::Zh);
        assert_eq!(row.name, "prod-box");
        assert_eq!(row.target, "10.0.0.5:7681");
        assert_eq!(row.via, "direct");
        assert_eq!(row.role.as_deref(), Some("viewer"));
        assert_eq!(row.fingerprint.as_deref(), Some(fp.as_str()));
        assert!(!row.read_only);
    }

    struct MemCreds(Option<StoredCredential>);

    impl CredentialStore for MemCreds {
        fn put(&self, _: &str, _: &StoredCredential) -> Result<(), mcsup::auth::CredentialError> {
            unreachable!()
        }
        fn get(&self, name: &str) -> Result<StoredCredential, mcsup::auth::CredentialError> {
            self.0
                .clone()
                .ok_or_else(|| mcsup::auth::CredentialError::NotFound(name.to_owned()))
        }
        fn remove(&self, _: &str) -> Result<(), mcsup::auth::CredentialError> {
            unreachable!()
        }
        fn names(&self) -> Result<Vec<String>, mcsup::auth::CredentialError> {
            Ok(Vec::new())
        }
    }
}
