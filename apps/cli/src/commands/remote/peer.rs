//! 对端探测与命名：指纹确认、配对码、名称解析与展示

use super::{
    AppDiagnostic, Capability, ConfigFile, CredentialVault, Fingerprint, HashSet, Printer, Probe,
    RemoteName, Response, Result, SystemTime, UNIX_EPOCH, prompts,
};

/// 确认指纹：交互下让用户核对，非交互下要求 `--yes` 或直接拒绝
pub(super) fn confirm_fingerprint(probe: &Probe, printer: Printer) -> Result<Fingerprint> {
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
pub(super) fn resolve_pairing_code(
    code: Option<String>,
    probe: &Probe,
    printer: Printer,
) -> Result<String> {
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
pub(super) fn resolve_remote_name(
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
pub(super) fn next_remote_name<'a>(existing: impl Iterator<Item = &'a str>) -> String {
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
pub(super) fn normalize_target(raw: &str) -> String {
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
pub(super) fn client_display_name() -> String {
    host_name()
}

/// 本机名
pub(super) fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "mcsup".to_owned())
}

/// 客户端凭证存储：优先系统钥匙串，无钥匙串时退到 `{data_dir}/credentials.toml`
pub(super) fn credential_store() -> CredentialVault {
    crate::bootstrap::open_credential_store()
}

pub(super) fn capability_name(capability: Capability) -> &'static str {
    match capability {
        Capability::Backup => "backup",
        Capability::File => "file",
        Capability::Java => "java",
        Capability::Streaming => "streaming",
        Capability::Installer => "installer",
        _ => "unknown",
    }
}

pub(super) fn response_label(response: &Response) -> &'static str {
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
        Response::Task { .. } => "task",
        Response::Error { .. } => "error",
        // 对端可能比本端新：未知帧只用于日志
        _ => "unknown",
    }
}

pub(super) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs().cast_signed())
}

/// 缺省端口：与 `mcsup-agent` 的缺省监听一致
pub(super) const DEFAULT_PORT: u16 = 7681;
