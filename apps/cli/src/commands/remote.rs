//! `remote`：接入、状态与撤销
//!
//! 所有判定都在库里：配对码 / token / 角色在 `auth`，证书指纹与握手在
//! `transport`，资源列表在 `service`。这里只做参数搬运、终端交互与渲染。
//!
//! 接入分两步：`probe` 只建连并捕获对端证书指纹（不认证），用户确认指纹后
//! 才 `enroll` 换 token。把「确认」放在两步之间，是因为 TOFU 的信任锚就是
//! 用户看到的那个指纹——先配对再展示等于让用户事后追认。

use crate::cli::{RemoteAction, ServeAction};
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
    RemoteRow,
};
use mcsup::protocol::remote::{Capability, Request, Response};
use mcsup::service::{RemoteRecord, RemoteVia};
use mcsup::transport::{Client, Fingerprint, Probe, TransportError};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

mod agent_files;
mod peer;
mod serve;

use peer::{
    capability_name, client_display_name, confirm_fingerprint, credential_store, host_name,
    normalize_target, resolve_pairing_code, resolve_remote_name, response_label, unix_now,
};
use serve::{code, serve};

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
        RemoteAction::Serve { action } => serve(action, printer),
        RemoteAction::Code {
            ttl,
            clear,
            data_dir,
        } => {
            let dir = data_dir.unwrap_or_else(crate::config::platform_data_dir);
            code(&dir, ttl, clear, printer)
        }
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
        RemoteAction::Revoke { client } => revoke(&client, printer),
    }
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

    // 显式给了名字就先校验：「名字非法 / 是保留字」是纯本地用法错误，
    // 不该先花一次 TLS 探测再报。探测失败时会盖掉真正的错因，用户看到的
    // 是「连不上」而不知道 `--name all` 本身就不合法。
    if let Some(explicit) = name {
        crate::commands::origin::remote_name(explicit, lang)?;
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

/// `remote disconnect`：删本地配对与凭证，并通知远端删掉自己那条名册记录
fn disconnect(file: &ConfigFile, name: &str, printer: Printer) -> Result<()> {
    let lang = printer.lang();
    let remote_name = crate::commands::origin::remote_name(name, lang)?;
    let mut cfg = file.load()?;
    // 先在**还持有凭证**的时候通知远端删记录：token 一旦删掉就发不出这个请求了。
    // 结果只影响提示措辞，不影响本地断开——本地这次清理是用户的明确意图，
    // 不能因为对端暂时连不上就整条命令失败，把人卡在「断不掉」的状态里。
    let revocation = revoke_self_remote(&cfg, &remote_name, lang);
    let removed = cfg.remove_remote(&remote_name).is_some();
    // 先删凭证（安全优先：宁可留下一条缺凭证的配置记录，也不要留下有效 token）
    credential_store()
        .remove(remote_name.as_str())
        .context(crate::i18n::pick(
            lang,
            "删除凭证",
            "removing the credential",
        ))?;
    if removed {
        file.save(&cfg)?;
    }
    // 缓存里是对端的服务器名、路径、版本与 Java 清单。断开后这些既是数据残留，
    // 也会让下一次 `connect` 到同名远端时先渲染出过期的旧内容——一并删掉。
    // 删不掉不算失败：配对与凭证已经清了，缓存只是可重建的副本。
    if let Err(err) = std::fs::remove_dir_all(crate::remote_cache::cache_for(&remote_name).dir())
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(remote = remote_name.as_str(), %err, "删除远端缓存失败");
    }
    // 当前选择可能正指向这台远端。断开后指针留在远端，之后每条缺省落点命令
    // 都会报「来源不可用」，所以要当场撤回本地最后使用的工作区
    let restored = detach_workspace(&remote_name, lang)?;
    let (restored_name, cleared) = match &restored {
        mcsup::service::DetachOutcome::Restored(ws) => (Some(ws), false),
        mcsup::service::DetachOutcome::Cleared => (None, true),
        mcsup::service::DetachOutcome::Untouched => (None, false),
    };
    let mut message = crate::i18n::pick(
        lang,
        format!("已断开 {name}（远端服务未受影响）"),
        format!("disconnected {name} (the remote service is unaffected)"),
    );
    // 远端那条名册记录的结果如实写出来。不能只说「已断开」——用户下次
    // `remote serve status` 看到列表里还挂着一条，会以为断开没生效。
    match &revocation {
        SelfRevoke::Revoked => message.push_str(&crate::i18n::pick(
            lang,
            "\n远端名册中的本机记录已删除",
            "\nthe record for this client was removed from the remote registry",
        )),
        SelfRevoke::AlreadyGone => message.push_str(&crate::i18n::pick(
            lang,
            "\n远端名册中已无本机记录",
            "\nthe remote registry had no record for this client",
        )),
        SelfRevoke::Skipped(reason) => message.push_str(&crate::i18n::pick(
            lang,
            format!(
                "\n远端记录未删除：{reason}\n如需清理，在对端运行 mcsup remote revoke <client>"
            ),
            format!(
                "\nthe remote record was not removed: {reason}\n\
                 to clean it up, run `mcsup remote revoke <client>` on the peer"
            ),
        )),
    }
    if let Some(ws) = restored_name {
        message.push_str(&crate::i18n::pick(
            lang,
            format!(
                "\n当前工作区已切回本地 {}（{}）",
                ws.name,
                ws.path.display()
            ),
            format!(
                "\ncurrent workspace switched back to local {} ({})",
                ws.name,
                ws.path.display()
            ),
        ));
    } else if cleared {
        message.push_str(&crate::i18n::pick(
            lang,
            "\n当前工作区已清空：本地没有可回退的工作区，先运行 mcsup workspace init".to_owned(),
            "\ncurrent workspace cleared: no local workspace to fall back to; run `mcsup workspace init`".to_owned(),
        ));
    }
    printer.print_outcome(
        &message,
        &RemoteDisconnectJson {
            name: remote_name.as_str(),
            removed,
            // 目标都是「远端已无这条记录」，对用户是同一件事
            remote_revoked: Some(!matches!(revocation, SelfRevoke::Skipped(_))),
            restored_workspace: restored_name.map(|ws| ws.name.as_str()),
            workspace_cleared: cleared,
        },
    )
}

/// 远端自撤销的结果
///
/// 三态而不是 `bool`：`Skipped` 要带原因（连不上 / 缺凭证 / 对端拒绝），
/// 这些原因决定了用户下一步是「不用管」还是「去对端手动清理」。
enum SelfRevoke {
    /// 远端已删除本机记录
    Revoked,
    /// 远端本来就没有本机记录（幂等）
    AlreadyGone,
    /// 没能通知到远端，附带原因
    Skipped(String),
}

/// 通知远端：删掉本机在它名册里的那条记录
///
/// **必须在删本地凭证之前调用**——token 是这次请求的唯一凭据。做法是先复制
/// 出 token，删凭证之后才发请求是不可行的。
///
/// 所有失败都收敛成 [`SelfRevoke::Skipped`]：本地断开的意图不该被网络状况
/// 否决。用户明确要断开，就必须断开；远端留一条不能用的孤儿记录是可以事后
/// 清理的次要问题，而「命令失败、什么都没变」会让用户反复重试。
fn revoke_self_remote(
    cfg: &mcsup::service::Config,
    remote_name: &RemoteName,
    lang: Lang,
) -> SelfRevoke {
    let Some(record) = cfg.remote(remote_name) else {
        return SelfRevoke::AlreadyGone;
    };
    let Some(fingerprint_text) = record.fingerprint.as_deref() else {
        // 没有固定指纹就还没真正配对过，远端不可能有名册记录
        return SelfRevoke::AlreadyGone;
    };
    let Ok(fingerprint) = Fingerprint::parse(fingerprint_text) else {
        return SelfRevoke::Skipped(crate::i18n::pick(
            lang,
            "已存指纹无法解析".to_owned(),
            "the pinned fingerprint cannot be parsed".to_owned(),
        ));
    };
    let credential = match credential_store().get(remote_name.as_str()) {
        Ok(credential) => credential,
        Err(err) => {
            return SelfRevoke::Skipped(crate::i18n::pick(
                lang,
                format!("读取凭证失败：{err:#}"),
                format!("reading the credential failed: {err:#}"),
            ));
        }
    };
    let endpoint = match crate::ssh::endpoint(record, lang) {
        Ok(endpoint) => endpoint,
        Err(err) => {
            return SelfRevoke::Skipped(crate::i18n::pick(
                lang,
                format!("建立连接失败：{err:#}"),
                format!("connecting failed: {err:#}"),
            ));
        }
    };

    let client = Client::new(&endpoint, fingerprint).with_token(credential.token);
    match client.call(&Request::ClientSelfRevoke) {
        Ok(_) => SelfRevoke::Revoked,
        // 记录已经不在（重试、或管理员手工清理过）：目标已达成，不算失败
        Err(TransportError::Remote { kind, .. })
            if kind == mcsup::protocol::remote::ErrorKind::NOT_FOUND =>
        {
            SelfRevoke::AlreadyGone
        }
        Err(err) => SelfRevoke::Skipped(crate::i18n::pick(
            lang,
            format!("{err:#}"),
            format!("{err:#}"),
        )),
    }
}

/// 断开后把本机 current 从该来源撤回本地
///
/// 装配的仓库**只含本地**：断开路径不该再向对端发起任何请求，也没必要为一次
/// 指针改写去读缓存。
fn detach_workspace(remote: &RemoteName, lang: Lang) -> Result<mcsup::service::DetachOutcome> {
    let store = crate::bootstrap::open_store()?;
    let workspaces = mcsup::service::WorkspaceService::new(store.clone(), store);
    workspaces
        .detach_origin(&Origin::Remote(remote.clone()))
        .context(crate::i18n::pick(
            lang,
            "撤回当前工作区",
            "restoring the current workspace",
        ))
}

/// `remote revoke`：在 serve 端本机离线删除客户端记录
fn revoke(client: &str, printer: Printer) -> Result<()> {
    let lang = printer.lang();
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
    // 连接成功正好是「刚问过对端」的时刻：顺手把四份列表落进缓存，
    // 接下来的 `ls` 立刻有内容，不必等第一次后台刷新
    seed_cache(name, endpoint, fingerprint)?;

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

/// 把刚连上的远端整份写进缓存（失败只记日志）
///
/// 连接已经成功，缓存没写上不该让 `remote connect` 报错——下一次读命令会
/// 再拉起后台刷新补上。
fn seed_cache(name: &RemoteName, endpoint: &str, fingerprint: &Fingerprint) -> Result<()> {
    let cache = crate::remote_cache::cache_for(name);
    let credential = credential_store().get(name.as_str())?;
    let link =
        mcsup::backend::remote::RemoteLink::new(endpoint, fingerprint.clone(), &credential.token);
    if let Err(err) = mcsup::backend::remote::refresh::refresh(&link, &cache) {
        tracing::warn!(remote = name.as_str(), %err, "failed to seed the remote cache");
    }
    Ok(())
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

/// 供 `--origin` 解析使用的来源文本（测试与提示共用）
#[allow(dead_code)]
fn origin_label(origin: &Origin) -> String {
    origin.as_str().to_owned()
}

#[cfg(test)]
mod tests;
