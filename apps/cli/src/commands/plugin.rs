//! `plugin` 子命令与隐藏的 `__plugin-host`
//!
//! 入口只做参数解析、进度回显、结果渲染与错误映射。安装记录与授权在
//! [`mcsup::service::PluginService`]；进程、帧与会话在宿主进程；两者经控制
//! 通道相连。管理命令在本进程完成文件与记录操作后通知宿主 `reload`。

use crate::bootstrap;
use crate::cli::{PluginAction, PluginConfigAction, PluginRevocationAction};
use crate::config;
use crate::diagnostic::AppDiagnostic;
use crate::output::{OutputFormat, Printer};
use crate::progress::TaskSpinner;
use anyhow::{Context, Result, anyhow};
use comfy_table::Table;
use mcsup::domain::plugin::PluginId;
use mcsup::domain::plugin::capability::Capability;
use mcsup::domain::plugin::descriptor::Descriptor;
use mcsup::domain::plugin::manifest::Manifest;
use mcsup::domain::plugin::revocation::RevocationList;
use mcsup::domain::server::artifact::{Checksum, ChecksumAlgorithm};
use mcsup::port::plugin::ArtifactVerifier as _;
use mcsup::port::plugin::{ArtifactOrigin, StreamSink};
use mcsup::service::PluginService;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub(crate) fn run(action: PluginAction, printer: Printer) -> Result<()> {
    match action {
        PluginAction::List { json } => list(printer, json),
        PluginAction::Install {
            source,
            from,
            url,
            git,
            reference,
            sha256,
            allow_unsigned,
            dev,
        } => install(
            source,
            from,
            url,
            git,
            reference,
            sha256,
            allow_unsigned || dev,
            printer,
        ),
        PluginAction::Update { id, to, prune } => update(&id, to, prune, printer),
        PluginAction::Remove { id, purge } => remove(&id, purge, printer),
        PluginAction::Enable { id } => set_enabled(&id, true, printer),
        PluginAction::Disable { id } => set_enabled(&id, false, printer),
        PluginAction::Start { id } => start(&id, printer),
        PluginAction::Stop { id, grace } => stop(&id, grace, printer),
        PluginAction::Restart { id } => restart(&id, printer),
        PluginAction::Status { id, json } => status(&id, printer, json),
        PluginAction::Ops { id, json } => ops(&id, printer, json),
        PluginAction::Call {
            id,
            op,
            input,
            json,
            stream,
        } => call(&id, &op, input.as_deref(), stream, printer, json),
        PluginAction::Config { action } => config_cmd(&action, printer),
        PluginAction::Grant { id, capability } => grant(&id, &capability, printer),
        PluginAction::Revoke { id, capability } => revoke(&id, &capability, printer),
        PluginAction::Block { id } => set_flag(&id, FlagAction::Block, printer),
        PluginAction::Unblock { id } => set_flag(&id, FlagAction::Unblock, printer),
        PluginAction::Quarantine { id } => set_flag(&id, FlagAction::Quarantine, printer),
        PluginAction::Release { id } => set_flag(&id, FlagAction::Release, printer),
        PluginAction::Logs { id, follow, tail } => logs(&id, follow, tail, printer),
        PluginAction::Verify { target } => verify(&target, printer),
        PluginAction::Rollback { id } => rollback(&id, printer),
        PluginAction::Revocations { action } => revocations(action, printer),
        PluginAction::Doctor { json } => doctor(printer, json),
    }
}

/// 隐藏入口：运行宿主直到收到 `shutdown`
pub(crate) fn run_host(data_dir: Option<PathBuf>, idle_timeout: u64) -> Result<()> {
    let data_dir = data_dir.unwrap_or_else(config::platform_data_dir);
    let mcsup = Arc::new(bootstrap::open_mcsup().context("装配插件宿主")?);
    let host = mcsup
        .plugin_host(&data_dir)
        .with_idle_after(Duration::from_secs(idle_timeout.max(10)));
    let server = mcsup::plugin::ControlServer::bind(&data_dir).map_err(|err| match err {
        mcsup::plugin::control::ControlError::AlreadyRunning => {
            AppDiagnostic::conflict("另一个插件宿主已在运行", None)
        }
        other => AppDiagnostic::failure(other.to_string(), None),
    })?;
    host.spawn_idle_reaper();
    let control = Arc::new(mcsup::plugin::HostControl::new(
        Arc::clone(&host) as Arc<dyn mcsup::port::plugin::PluginSupervisor>,
        Arc::clone(&mcsup.plugin_store),
        mcsup.plugins.config(),
        data_dir.join("logs").join("plugins"),
        Arc::clone(&mcsup.plugin_audit),
    ));
    server.serve(control).context("运行插件宿主控制端点")?;
    host.shutdown_all(Duration::from_secs(5));
    Ok(())
}

fn open() -> Result<PluginService> {
    Ok(bootstrap::open_mcsup()?.plugins)
}

fn parse_id(raw: &str) -> Result<PluginId> {
    PluginId::parse(raw).map_err(|err| {
        AppDiagnostic::usage(format!("插件 id 非法: {raw}"), Some(err.to_string())).into()
    })
}

fn parse_capability(raw: &str) -> Result<Capability> {
    Capability::parse(raw).map_err(|err| {
        AppDiagnostic::usage(format!("能力名非法: {raw}"), Some(err.to_string())).into()
    })
}

fn json_printer(printer: Printer, json: bool) -> Printer {
    if json {
        printer.with_format(OutputFormat::Json)
    } else {
        printer
    }
}

/// 宿主不在时拉起 `__plugin-host` 并等待可连接
fn ensure_host() -> Result<()> {
    let data_dir = config::platform_data_dir();
    let client = mcsup::plugin::ControlClient::new(&data_dir);
    if client.is_available() {
        return Ok(());
    }
    let exe = std::env::current_exe().context("定位 mcsup 可执行文件")?;
    std::process::Command::new(exe)
        .arg("__plugin-host")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("拉起插件宿主")?;
    for _ in 0..100 {
        if client.is_available() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(AppDiagnostic::failure(
        "插件宿主未能在 10 秒内就绪",
        Some("查看 `mcsup plugin doctor` 或宿主日志".to_owned()),
    )
    .into())
}

fn list(printer: Printer, json: bool) -> Result<()> {
    let printer = json_printer(printer, json);
    let service = open()?;
    let records = service.list()?;
    if printer.is_human() {
        if records.is_empty() {
            printer.print_section(&crate::i18n::pick(
                printer.lang(),
                "还没有安装插件",
                "no plugins installed",
            ));
            return Ok(());
        }
        let mut table = Table::new();
        table.set_header(["ID", "版本", "状态", "启用", "待确认", "安全"]);
        for record in &records {
            let state = service.status(&record.id).map_or_else(
                |_| record.last_state.as_str().to_owned(),
                |status| status.state.as_str().to_owned(),
            );
            table.add_row([
                record.id.to_string(),
                record.version.clone(),
                state,
                record.enabled.to_string(),
                record.needs_approval().to_string(),
                flag_text(record).to_owned(),
            ]);
        }
        printer.print_table(&table);
        return Ok(());
    }
    let mut items = Vec::with_capacity(records.len());
    for record in records {
        let state = service.status(&record.id).map_or_else(
            |_| record.last_state.as_str().to_owned(),
            |status| status.state.as_str().to_owned(),
        );
        items.push(json!({
            "id": record.id.to_string(),
            "version": record.version,
            "enabled": record.enabled,
            "state": state,
            "needs_approval": record.needs_approval(),
            "blocked": record.blocked,
            "quarantined": record.quarantined,
            "signed": record.signed,
        }));
    }
    printer.print_outcome("", &items)
}

#[allow(clippy::too_many_arguments)]
fn install(
    source: Option<String>,
    from: Option<PathBuf>,
    url: Option<String>,
    git: Option<String>,
    reference: Option<String>,
    sha256: Option<String>,
    allow_unsigned: bool,
    printer: Printer,
) -> Result<()> {
    let origin = resolve_origin(source, from, url, git, reference)?;
    let expected = sha256.map(|hex| Checksum::new(ChecksumAlgorithm::Sha256, hex));
    let service = open()?;
    let spinner = TaskSpinner::new(
        crate::i18n::pick(printer.lang(), "正在安装插件…", "installing plugin…"),
        printer.is_human(),
    );
    let outcome = service
        .install(&origin, expected.as_ref(), allow_unsigned)
        .map_err(anyhow::Error::from);
    spinner.finish();
    let outcome = outcome?;
    let record = outcome.record;
    if printer.is_human() && !outcome.pending.is_empty() {
        printer.print_section(&crate::i18n::pick(
            printer.lang(),
            format!(
                "安装完成，仍待确认: {}；运行 `mcsup plugin grant {} <capability>`",
                outcome
                    .pending
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                record.id
            ),
            format!(
                "installed; pending approval: {}; run `mcsup plugin grant {} <capability>`",
                outcome
                    .pending
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                record.id
            ),
        ));
    }
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("已安装 {} {}", record.id, record.version),
            format!("installed {} {}", record.id, record.version),
        ),
        &json!({
            "id": record.id.to_string(),
            "version": record.version,
            "pending": outcome.pending.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "signed": record.signed,
        }),
    )
}

fn resolve_origin(
    source: Option<String>,
    from: Option<PathBuf>,
    url: Option<String>,
    git: Option<String>,
    reference: Option<String>,
) -> Result<ArtifactOrigin> {
    let supplied = [
        source.is_some(),
        from.is_some(),
        url.is_some(),
        git.is_some(),
    ]
    .into_iter()
    .filter(|given| *given)
    .count();
    if supplied != 1 {
        return Err(AppDiagnostic::usage(
            "必须且只能给出一个来源：<source> / --from / --url / --git",
            None,
        )
        .into());
    }
    if let Some(git) = git {
        return Ok(ArtifactOrigin::Git {
            url: git,
            rev: reference,
        });
    }
    if reference.is_some() {
        return Err(AppDiagnostic::usage("--ref 只能与 --git 一起使用", None).into());
    }
    if let Some(url) = url {
        return Ok(ArtifactOrigin::Url(url));
    }
    if let Some(path) = from {
        return Ok(ArtifactOrigin::Local(path));
    }
    let Some(raw) = source else {
        return Err(AppDiagnostic::usage("缺少插件来源", None).into());
    };
    if raw.starts_with("http://") || raw.starts_with("https://") {
        Ok(ArtifactOrigin::Url(raw))
    } else {
        Ok(ArtifactOrigin::Local(PathBuf::from(raw)))
    }
}

fn update(id: &str, to: Option<String>, _prune: bool, printer: Printer) -> Result<()> {
    let id = parse_id(id)?;
    let service = open()?;
    let record = service.record(&id)?;
    if let Some(to) = to
        && to != record.version
    {
        return Err(AppDiagnostic::usage(
            format!(
                "当前实现按来源中的最新产物安装，不能单独指定 --to {to}；请安装包含该版本的来源"
            ),
            None,
        )
        .into());
    }
    let origin = origin_from_source(record.source.as_deref()).ok_or_else(|| {
        AppDiagnostic::usage(
            format!("插件 {id} 没有可复用的安装来源；用 `mcsup plugin install <source>` 升级"),
            None,
        )
    })?;
    let spinner = TaskSpinner::new(
        crate::i18n::pick(printer.lang(), "正在升级插件…", "updating plugin…"),
        printer.is_human(),
    );
    // 升级沿用原产物的信任姿态：原产物未签名才允许继续安装未签名产物。
    let allow_unsigned = !record.signed;
    let result = service
        .install(&origin, None, allow_unsigned)
        .map_err(anyhow::Error::from);
    spinner.finish();
    let outcome = result?;
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("已升级 {} 到 {}", outcome.record.id, outcome.record.version),
            format!(
                "updated {} to {}",
                outcome.record.id, outcome.record.version
            ),
        ),
        &json!({
            "id": outcome.record.id.to_string(),
            "version": outcome.record.version,
        }),
    )
}

fn origin_from_source(source: Option<&str>) -> Option<ArtifactOrigin> {
    let source = source?;
    if let Some(path) = source.strip_prefix("local:") {
        return Some(ArtifactOrigin::Local(PathBuf::from(path)));
    }
    if let Some(url) = source.strip_prefix("url:") {
        return Some(ArtifactOrigin::Url(url.to_owned()));
    }
    if let Some(rest) = source.strip_prefix("git:") {
        let (url, rev) = match rest.split_once('@') {
            Some((url, rev)) => (url.to_owned(), Some(rev.to_owned())),
            None => (rest.to_owned(), None),
        };
        return Some(ArtifactOrigin::Git { url, rev });
    }
    None
}

fn remove(id: &str, purge: bool, printer: Printer) -> Result<()> {
    let id = parse_id(id)?;
    let service = open()?;
    service.remove(&id, purge)?;
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("已卸载 {id}{}", if purge { "（含数据）" } else { "" }),
            format!("removed {id}{}", if purge { " (with data)" } else { "" }),
        ),
        &json!({ "id": id.to_string(), "purge": purge }),
    )
}

fn set_enabled(id: &str, enable: bool, printer: Printer) -> Result<()> {
    let id = parse_id(id)?;
    let service = open()?;
    let record = if enable {
        service.enable(&id)?
    } else {
        service.disable(&id)?
    };
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("{} {}", if enable { "已启用" } else { "已停用" }, record.id),
            format!(
                "{} {}",
                if enable { "enabled" } else { "disabled" },
                record.id
            ),
        ),
        &json!({ "id": record.id.to_string(), "enabled": record.enabled }),
    )
}

fn start(id: &str, printer: Printer) -> Result<()> {
    let id = parse_id(id)?;
    ensure_host()?;
    let service = open()?;
    let info = service.start(&id)?;
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("已启动 {}（pid {}）", id, info.pid),
            format!("started {} (pid {})", id, info.pid),
        ),
        &json!({
            "id": id.to_string(),
            "session": {
                "session_id": info.session_id,
                "pid": info.pid,
                "granted": info.granted.iter().map(ToString::to_string).collect::<Vec<_>>(),
            }
        }),
    )
}

fn stop(id: &str, grace: u64, printer: Printer) -> Result<()> {
    let id = parse_id(id)?;
    let service = open()?;
    service.stop(&id, Duration::from_secs(grace.min(300)))?;
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("已停止 {id}"),
            format!("stopped {id}"),
        ),
        &json!({ "id": id.to_string(), "stopped": true }),
    )
}

fn restart(id: &str, printer: Printer) -> Result<()> {
    let id = parse_id(id)?;
    ensure_host()?;
    let service = open()?;
    let info = service.restart(&id, Duration::from_secs(10))?;
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("已重启 {}（pid {}）", id, info.pid),
            format!("restarted {} (pid {})", id, info.pid),
        ),
        &json!({ "id": id.to_string(), "pid": info.pid }),
    )
}

fn status(id: &str, printer: Printer, json: bool) -> Result<()> {
    let printer = json_printer(printer, json);
    let id = parse_id(id)?;
    let service = open()?;
    let record = service.record(&id)?;
    let status = service.status(&id)?;
    if printer.is_human() {
        let mut table = Table::new();
        table.set_header(["项", "值"]);
        let mut row = |name: &str, value: String| {
            table.add_row([comfy_table::Cell::new(name), comfy_table::Cell::new(value)]);
        };
        row("ID", record.id.to_string());
        row("版本", record.version.clone());
        row("作用域", record.scope.as_str().to_owned());
        row("启用", record.enabled.to_string());
        row("状态", status.state.as_str().to_owned());
        row("待确认", record.needs_approval().to_string());
        if record.blocked {
            row("拉黑", "是".to_owned());
        }
        if record.quarantined {
            row("隔离", "是".to_owned());
        }
        row(
            "能力",
            record
                .approved
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        );
        if let Some(session) = &status.session {
            row("会话", session.session_id.clone());
            row("PID", session.pid.to_string());
        }
        if let Some(error) = &status.last_error {
            row("最近错误", error.clone());
        }
        printer.print_table(&table);
        return Ok(());
    }
    printer.print_outcome(
        "",
        &json!({
            "id": record.id.to_string(),
            "version": record.version,
            "scope": record.scope.as_str(),
            "enabled": record.enabled,
            "state": status.state.as_str(),
            "needs_approval": record.needs_approval(),
            "blocked": record.blocked,
            "quarantined": record.quarantined,
            "approved": record.approved.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "capabilities": record.declared_capabilities().iter().map(ToString::to_string).collect::<Vec<_>>(),
            "session": status.session.as_ref().map(|session| json!({
                "session_id": session.session_id,
                "pid": session.pid,
                "granted": session.granted.iter().map(ToString::to_string).collect::<Vec<_>>(),
            })),
            "last_error": status.last_error,
        }),
    )
}

fn ops(id: &str, printer: Printer, json: bool) -> Result<()> {
    let printer = json_printer(printer, json);
    let id = parse_id(id)?;
    let service = open()?;
    let descriptor = service.descriptor(&id)?;
    if printer.is_human() {
        if descriptor.ops.is_empty() {
            printer.print_section(&crate::i18n::pick(
                printer.lang(),
                "该插件没有声明 op",
                "this plugin declares no ops",
            ));
            return Ok(());
        }
        let mut table = Table::new();
        table.set_header(["op", "类型", "展示", "风险", "标题"]);
        for resolved in &descriptor.ops {
            table.add_row([
                resolved.op.id.clone(),
                resolved.op.kind.as_str().to_owned(),
                resolved.display.type_name().to_owned(),
                resolved.op.risk.as_str().to_owned(),
                resolved.op.title.clone(),
            ]);
        }
        printer.print_table(&table);
        return Ok(());
    }
    let items: Vec<Value> = descriptor
        .ops
        .iter()
        .map(|resolved| {
            json!({
                "id": resolved.op.id,
                "kind": resolved.op.kind.as_str(),
                "risk": resolved.op.risk.as_str(),
                "title": resolved.op.title,
                "display": resolved.display.type_name(),
                "timeout_ms": resolved.op.timeout_ms,
            })
        })
        .collect();
    printer.print_outcome("", &items)
}

fn read_input(raw: Option<&str>) -> Result<Value> {
    let text = match raw {
        None => return Ok(json!({})),
        Some("-") => {
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .context("读取 stdin")?;
            buffer
        }
        Some(value) if value.starts_with('@') => {
            let path = &value[1..];
            std::fs::read_to_string(path).with_context(|| format!("读取 {path}"))?
        }
        Some(value) => value.to_owned(),
    };
    serde_json::from_str(&text)
        .map_err(|err| AppDiagnostic::usage("--input 不是合法 JSON", Some(err.to_string())).into())
}

fn call(
    id: &str,
    op: &str,
    input: Option<&str>,
    stream: bool,
    printer: Printer,
    json: bool,
) -> Result<()> {
    let printer = json_printer(printer, json);
    let id = parse_id(id)?;
    let input = read_input(input)?;
    ensure_host()?;
    let service = open()?;
    if stream {
        let mut sink = PrintSink::new(printer);
        service.stream(&id, op, input, &mut sink)?;
        return Ok(());
    }
    let output = service.call(&id, op, input)?;
    printer.print_outcome(&serde_json::to_string_pretty(&output)?, &output)
}

/// 把 stream 分块输出；`done` 只作为 JSON 载荷的一部分，不额外造行
struct PrintSink {
    printer: Printer,
}

impl PrintSink {
    fn new(printer: Printer) -> Self {
        Self { printer }
    }
}

impl StreamSink for PrintSink {
    fn on_chunk(
        &mut self,
        chunk: &Value,
        done: bool,
    ) -> Result<(), mcsup::port::plugin::HostError> {
        if self.printer.is_human() {
            match chunk {
                Value::String(text) => crate::output::print_line(text),
                other => crate::output::print_line(&other.to_string()),
            }
        } else {
            crate::output::print_json_line(
                &serde_json::to_string(&json!({ "chunk": chunk, "done": done }))
                    .unwrap_or_else(|_| "{}".to_owned()),
            );
        }
        Ok(())
    }
}

fn config_cmd(action: &PluginConfigAction, printer: Printer) -> Result<()> {
    match action {
        PluginConfigAction::Get { id, key, json } => {
            let printer = json_printer(printer, *json);
            let id = parse_id(id)?;
            let service = open()?;
            let config = service.config_get(&id)?;
            let revision = config.get("revision").and_then(Value::as_u64).unwrap_or(0);
            let values = config.get("values").cloned().unwrap_or_else(|| json!({}));
            if let Some(key) = key {
                let value = values.get(key).cloned().ok_or_else(|| {
                    AppDiagnostic::not_found(
                        format!("插件 {id} 的配置里没有键 {key}"),
                        Some(format!("运行 `mcsup plugin config get {id}` 查看已有键")),
                    )
                })?;
                if printer.is_human() {
                    match &value {
                        Value::String(text) => printer.print_section(text),
                        other => printer.print_section(&serde_json::to_string_pretty(other)?),
                    }
                    return Ok(());
                }
                return printer.print_outcome(
                    "",
                    &json!({ "id": id.to_string(), "key": key, "value": value, "revision": revision }),
                );
            }
            if printer.is_human() {
                let Some(object) = values.as_object() else {
                    printer.print_section(&values.to_string());
                    return Ok(());
                };
                if object.is_empty() {
                    printer.print_section(&crate::i18n::pick(
                        printer.lang(),
                        "该插件没有配置项",
                        "this plugin has no configuration",
                    ));
                    return Ok(());
                }
                let mut table = Table::new();
                table.set_header(["键", "值"]);
                for (name, value) in object {
                    let rendered = match value {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    table.add_row([name.clone(), rendered]);
                }
                printer.print_table(&table);
                return Ok(());
            }
            printer.print_outcome(
                "",
                &json!({ "id": id.to_string(), "revision": revision, "values": values }),
            )
        }
        PluginConfigAction::Set {
            id,
            key,
            value,
            secret,
        } => {
            let id = parse_id(id)?;
            let value = config_value(value.as_deref(), *secret)?;
            let service = open()?;
            let config = service.config_set(&id, &json!({ key.clone(): value }))?;
            let revision = config.get("revision").and_then(Value::as_u64).unwrap_or(0);
            printer.print_outcome(
                &crate::i18n::pick(
                    printer.lang(),
                    format!("已更新 {id} 的配置（revision {revision}）"),
                    format!("updated config for {id} (revision {revision})"),
                ),
                &json!({ "id": id.to_string(), "key": key, "revision": revision }),
            )
        }
    }
}

/// 解析 `config set` 的值
///
/// `--secret` 时一律按字符串处理（从 stdin 读时只去掉行尾换行），避免把密钥
/// 当作 JSON 解析失败后打出一段无意义提示。普通值先按 JSON 解析，失败才按
/// 字符串，保证整数 / 布尔 / 数组可以直接写。
fn config_value(raw: Option<&str>, secret: bool) -> Result<Value> {
    if secret {
        let text = if let Some(text) = raw {
            text.to_owned()
        } else {
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .context("从 stdin 读取密钥")?;
            buffer
        };
        return Ok(Value::String(
            text.trim_end_matches(['\r', '\n']).to_owned(),
        ));
    }
    let Some(raw) = raw else {
        return Err(AppDiagnostic::usage("缺少配置值；或用 --secret 从 stdin 读取", None).into());
    };
    Ok(serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned())))
}

#[derive(Clone, Copy)]
enum FlagAction {
    Block,
    Unblock,
    Quarantine,
    Release,
}

fn set_flag(raw: &str, action: FlagAction, printer: Printer) -> Result<()> {
    let id = parse_id(raw)?;
    let service = open()?;
    let record = match action {
        FlagAction::Block => service.block(&id)?,
        FlagAction::Unblock => service.unblock(&id)?,
        FlagAction::Quarantine => service.quarantine(&id)?,
        FlagAction::Release => service.release(&id)?,
    };
    let (human, verb) = match action {
        FlagAction::Block => (format!("已拉黑 {id}，已停止并禁用"), "blocked".to_owned()),
        FlagAction::Unblock => (format!("已解除拉黑 {id}"), "unblocked".to_owned()),
        FlagAction::Quarantine => (
            format!("已隔离 {id}，产物保留但禁止运行"),
            "quarantined".to_owned(),
        ),
        FlagAction::Release => (format!("已解除隔离 {id}"), "released".to_owned()),
    };
    printer.print_outcome(
        &crate::i18n::pick(printer.lang(), human, format!("{verb} {id}")),
        &json!({
            "id": record.id.to_string(),
            "blocked": record.blocked,
            "quarantined": record.quarantined,
            "enabled": record.enabled,
        }),
    )
}

fn revocations(action: Option<PluginRevocationAction>, printer: Printer) -> Result<()> {
    let service = open()?;
    let Some(action) = action else {
        return print_revocations(&service.list_revocations()?, printer);
    };
    let max = mcsup::domain::plugin::revocation::MAX_REVOCATION_BYTES;
    let bytes = match action {
        PluginRevocationAction::Import { file } => {
            let path = PathBuf::from(&file);
            let metadata =
                std::fs::metadata(&path).with_context(|| format!("read revocation list {file}"))?;
            if metadata.len() > max {
                return Err(
                    AppDiagnostic::usage(format!("撤销列表超过 {max} 字节"), Some(file)).into(),
                );
            }
            std::fs::read(&path).with_context(|| format!("read revocation list {file}"))?
        }
        PluginRevocationAction::Fetch { url, sha256 } => {
            let spinner = TaskSpinner::new(
                crate::i18n::pick(
                    printer.lang(),
                    "正在拉取撤销列表…",
                    "fetching revocation list…",
                ),
                printer.is_human(),
            );
            let result = fetch_revocation_list(&url, sha256.as_deref(), max);
            spinner.finish();
            result?
        }
    };
    let list = RevocationList::from_json(&bytes)
        .map_err(|err| AppDiagnostic::usage(format!("撤销列表非法: {err}"), None))?;
    service.store_revocations(&list)?;
    print_revocations(&list, printer)
}

/// 十六进制 sha256
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// 拉取撤销列表：只接受 HTTPS，响应带上限
fn fetch_revocation_list(url: &str, sha256: Option<&str>, max: u64) -> Result<Vec<u8>> {
    use std::io::Read as _;

    let parsed = reqwest::Url::parse(url)
        .map_err(|err| AppDiagnostic::usage(format!("URL 非法: {url}"), Some(err.to_string())))?;
    if parsed.scheme() != "https" {
        return Err(AppDiagnostic::usage(format!("撤销列表只接受 https URL: {url}"), None).into());
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|err| anyhow!("build http client: {err}"))?;
    let response = client
        .get(parsed)
        .send()
        .map_err(|err| anyhow!("fetch revocation list: {err}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("fetch revocation list: HTTP {}", status.as_u16()));
    }
    if response.content_length().is_some_and(|len| len > max) {
        return Err(anyhow!("revocation list exceeds {max} bytes"));
    }
    let mut bytes = Vec::new();
    response
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| anyhow!("read revocation list: {err}"))?;
    if bytes.len() as u64 > max {
        return Err(anyhow!("revocation list exceeds {max} bytes"));
    }
    if let Some(expected) = sha256 {
        let actual = sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            return Err(AppDiagnostic::conflict(
                format!("撤销列表摘要不匹配: 期望 {expected}，实际 {actual}"),
                None,
            )
            .into());
        }
    }
    Ok(bytes)
}

fn print_revocations(list: &RevocationList, printer: Printer) -> Result<()> {
    if printer.is_human() {
        if list.entries.is_empty() {
            printer.print_section(&crate::i18n::pick(
                printer.lang(),
                "当前没有撤销记录",
                "no revoked plugins",
            ));
        } else {
            let mut table = Table::new();
            table.set_header(["插件", "版本", "原因"]);
            for entry in &list.entries {
                table.add_row([
                    entry.id.clone(),
                    if entry.versions.is_empty() {
                        "*".to_owned()
                    } else {
                        entry.versions.join(", ")
                    },
                    entry.reason.clone().unwrap_or_default(),
                ]);
            }
            printer.print_table(&table);
        }
    }
    printer.print_outcome(
        "",
        &json!({
            "updated_at": list.updated_at,
            "entries": list.entries.iter().map(|entry| json!({
                "id": entry.id,
                "versions": entry.versions,
                "reason": entry.reason,
            })).collect::<Vec<_>>(),
        }),
    )
}

fn flag_text(record: &mcsup::port::plugin::PluginRecord) -> &'static str {
    match (record.blocked, record.quarantined) {
        (true, true) => "blocked+quarantined",
        (true, false) => "blocked",
        (false, true) => "quarantined",
        (false, false) => "",
    }
}

fn grant(id: &str, capability: &str, printer: Printer) -> Result<()> {
    let id = parse_id(id)?;
    let capability = parse_capability(capability)?;
    let service = open()?;
    let record = service.grant(&id, capability)?;
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("已授予 {} {capability}", record.id),
            format!("granted {capability} to {}", record.id),
        ),
        &json!({
            "id": record.id.to_string(),
            "capability": capability.to_string(),
            "approved": record.approved.iter().map(ToString::to_string).collect::<Vec<_>>(),
        }),
    )
}

fn revoke(id: &str, capability: &str, printer: Printer) -> Result<()> {
    let id = parse_id(id)?;
    let capability = parse_capability(capability)?;
    let service = open()?;
    let record = service.revoke(&id, capability)?;
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("已撤销 {} {capability}", record.id),
            format!("revoked {capability} from {}", record.id),
        ),
        &json!({
            "id": record.id.to_string(),
            "capability": capability.to_string(),
            "approved": record.approved.iter().map(ToString::to_string).collect::<Vec<_>>(),
        }),
    )
}

fn logs(id: &str, follow: bool, tail: usize, printer: Printer) -> Result<()> {
    let id = parse_id(id)?;
    ensure_host()?;
    let service = open()?;
    let mut offset: Option<u64> = None;
    loop {
        let payload = service.logs(&id, offset, tail.clamp(1, 5000))?;
        if let Some(lines) = payload.get("lines").and_then(Value::as_array) {
            for line in lines {
                match line {
                    Value::String(text) => crate::output::print_line(text),
                    other => crate::output::print_line(&other.to_string()),
                }
            }
        }
        let next = payload.get("next").and_then(Value::as_u64).or(offset);
        offset = next;
        if !follow {
            return Ok(());
        }
        let _ = printer;
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// 撤销命中时给出拒绝启动的诊断；列表不可用时跳过
fn revoked_finding(
    service: &PluginService,
    record: &mcsup::port::plugin::PluginRecord,
    skip: bool,
) -> Option<Value> {
    if skip {
        return None;
    }
    let message = service.revocation(record).ok().flatten()?;
    Some(json!({
        "level": "error",
        "id": record.id.to_string(),
        "message": format!("{message}（拒绝启动；升级或卸载后重试）"),
    }))
}

fn verify(target: &str, printer: Printer) -> Result<()> {
    let service = open()?;
    let expected = sha256_hint(target);
    // 本地文件直接校验文件本体：目录里没有 `.minisig`，`.zip.minisig` 的签名
    // 只对压缩包字节有意义。插件 id 与目录走安装记录 + 目录摘要复查。
    let local_file = PluginId::parse(target)
        .is_err()
        .then(|| PathBuf::from(target));
    let local_file = local_file
        .filter(|path| path.is_file() && path.extension().is_none_or(|ext| ext != "minisig"));
    let (target_path, id, signed, recheck) = if let Some(path) = local_file {
        (path, None, false, true)
    } else if let Ok(id) = PluginId::parse(target) {
        let record = service.record(&id)?;
        (record.artifact_dir, Some(record.id), record.signed, false)
    } else {
        let path = PathBuf::from(target);
        if !path.exists() {
            return Err(
                AppDiagnostic::not_found(format!("找不到插件 id 或目录: {target}"), None).into(),
            );
        }
        (path, None, false, false)
    };
    let verifier = mcsup::provider::plugin::PluginArtifactVerifier::new();
    let artifact = if recheck {
        // 未安装的文件：重新走一遍来源校验（含签名），失败即拒绝。来源摘要
        // 在这里就是文件本身的摘要，再与旁路 `.sha256` 比对。
        let fetched = service
            .fetch_artifact(&ArtifactOrigin::Local(target_path.clone()))
            .map_err(anyhow::Error::from)?;
        verifier
            .verify(&fetched.artifact, expected.as_ref())
            .map_err(anyhow::Error::from)?
    } else {
        verifier
            .verify_installed(&target_path, expected.as_ref(), signed)
            .map_err(anyhow::Error::from)?
    };
    let manifest = Manifest::from_json(
        &serde_json::to_vec(&artifact.manifest).map_err(|err| anyhow!("{err}"))?,
    )
    .map_err(|err| AppDiagnostic::failure(format!("manifest 非法: {err}"), None))?;
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("校验通过：{} {}", manifest.id, manifest.raw.version),
            format!("verified {} {}", manifest.id, manifest.raw.version),
        ),
        &json!({
            "id": id.map_or_else(|| manifest.id.to_string(), |id| id.to_string()),
            "sha256": artifact.sha256,
            "signed": artifact.signed,
            "trusted": artifact.trusted,
        }),
    )
}

/// 旁路摘要：`<path>.sha256` 的首个十六进制串
///
/// 已安装插件不用它：安装记录里的摘要是解包目录摘要，与压缩包摘要不同。
fn sha256_hint(target: &str) -> Option<Checksum> {
    let path = PathBuf::from(target);
    let mut name = path.file_name()?.to_os_string();
    name.push(".sha256");
    let text = std::fs::read_to_string(path.with_file_name(&name)).ok()?;
    let hex = text
        .split_whitespace()
        .find(|token| token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))?;
    Some(Checksum::new(ChecksumAlgorithm::Sha256, hex))
}

fn rollback(id: &str, printer: Printer) -> Result<()> {
    let id = parse_id(id)?;
    let service = open()?;
    let record = service.rollback(&id)?;
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("已回滚 {} 到 {}", record.id, record.version),
            format!("rolled {} back to {}", record.id, record.version),
        ),
        &json!({ "id": record.id.to_string(), "version": record.version }),
    )
}

fn doctor(printer: Printer, json: bool) -> Result<()> {
    let printer = json_printer(printer, json);
    let service = open()?;
    let records = service.list()?;
    // 撤销列表读不懂只报一次，不逐插件重复：它是全局设施。
    let revocation_error = match service.revocation_list_check() {
        Ok(()) => None,
        Err(err) => Some(err.to_string()),
    };
    let mut findings: Vec<Value> = Vec::new();
    if let Some(message) = &revocation_error {
        findings.push(json!({
            "level": "warning",
            "id": Value::Null,
            "message": format!("撤销列表不可用，已跳过撤销检查: {message}"),
        }));
    }
    for record in &records {
        let artifact = record.artifact_dir.clone();
        if !artifact.is_dir() {
            findings.push(json!({
                "level": "error",
                "id": record.id.to_string(),
                "message": format!("产物目录缺失: {}", artifact.display()),
            }));
            continue;
        }
        if let Some(message) = revoked_finding(&service, record, revocation_error.is_some()) {
            findings.push(message);
            continue;
        }
        let manifest_path = artifact.join("manifest.json");
        if let Err(err) = std::fs::read(&manifest_path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Manifest::from_json(&bytes).map_err(|err| anyhow!("{err}")))
        {
            findings.push(json!({
                "level": "error",
                "id": record.id.to_string(),
                "message": format!("manifest 非法: {err}"),
            }));
        }
        let descriptor_path = artifact.join("descriptor.json");
        if let Err(err) = std::fs::read(&descriptor_path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| {
                Descriptor::from_json(&bytes, &record.id).map_err(|err| anyhow!("{err}"))
            })
        {
            findings.push(json!({
                "level": "error",
                "id": record.id.to_string(),
                "message": format!("descriptor 非法: {err}"),
            }));
        }
        if record.needs_approval() {
            findings.push(json!({
                "level": "warning",
                "id": record.id.to_string(),
                "message": "存在待确认能力，插件不会启动",
            }));
        }
    }
    let host = mcsup::plugin::ControlClient::new(&config::platform_data_dir()).is_available();
    if !host {
        findings.push(json!({
            "level": "info",
            "id": Value::Null,
            "message": "插件宿主未运行；start / call 会自动拉起",
        }));
    }
    let errors = findings
        .iter()
        .filter(|finding| finding["level"] == "error")
        .count();
    if printer.is_human() {
        if findings.is_empty() {
            printer.print_section(&crate::i18n::pick(
                printer.lang(),
                "插件检查通过",
                "plugin check passed",
            ));
        } else {
            let mut table = Table::new();
            table.set_header(["级别", "插件", "说明"]);
            for finding in &findings {
                table.add_row([
                    finding["level"].as_str().unwrap_or("info").to_owned(),
                    finding["id"].as_str().unwrap_or("-").to_owned(),
                    finding["message"].as_str().unwrap_or_default().to_owned(),
                ]);
            }
            printer.print_table(&table);
        }
    } else {
        printer.print_outcome("", &findings)?;
    }
    if errors > 0 {
        return Err(AppDiagnostic::findings(errors).into());
    }
    Ok(())
}

/// 测试与诊断用：产物根
#[allow(dead_code)]
fn artifact_root(record: &mcsup::port::plugin::PluginRecord) -> &Path {
    &record.artifact_dir
}
