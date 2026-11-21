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
use mcsup::domain::plugin::descriptor::{ResolvedSetupField, SetupKind};
use mcsup::domain::plugin::manifest::Manifest;
use mcsup::domain::plugin::revocation::RevocationList;
use mcsup::domain::server::artifact::{Checksum, ChecksumAlgorithm};
use mcsup::port::plugin::ArtifactVerifier as _;
use mcsup::port::plugin::{ArtifactOrigin, PluginRecord, SecretProtection, StreamSink};
use mcsup::service::PluginService;
use serde_json::{Map, Value, json};
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
            yes,
            no_setup,
        } => install(
            source, from, url, git, reference, sha256, yes, !no_setup, printer,
        ),
        PluginAction::Update {
            id,
            to,
            prune,
            yes,
            no_setup,
        } => update(&id, to, prune, yes, !no_setup, printer),
        PluginAction::Remove {
            id,
            purge,
            keep_secrets,
        } => remove(&id, purge, keep_secrets, printer),
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
            yes,
        } => call(&id, &op, input.as_deref(), stream, yes, printer, json),
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
    host.spawn_console_events();
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
        }));
    }
    printer.print_outcome("", &items)
}

/// 安装向导的输出：写入了哪些字段、还缺哪些必填项、是否应自动启动
#[derive(Debug, Default)]
struct SetupReport {
    /// 已写入的字段路径
    configured: Vec<String>,
    /// 仍缺失的必填字段路径
    missing: Vec<String>,
    /// descriptor 是否要求自动启动
    start: bool,
}

/// 安装/升级后仍需确认的能力
fn pending_of(record: &PluginRecord) -> Vec<Capability> {
    record
        .manifest
        .capabilities
        .iter()
        .filter_map(|raw| Capability::parse(raw).ok())
        .filter(|cap| cap.risk().needs_approval() && !record.approved.contains(cap))
        .collect()
}

/// 能力的中英说明：授权清单里让用户知道自己在同意什么
fn capability_summary(cap: Capability) -> (&'static str, &'static str) {
    match cap.as_str() {
        "events" => ("接收服务器事件", "receive server events"),
        "storage:kv" => (
            "读写插件私有键值存储",
            "read/write private key-value storage",
        ),
        "server:read" => ("读取服务器信息", "read server info"),
        "console:read" => ("读取控制台输出", "read console output"),
        "console:write" => ("向控制台执行命令", "run console commands"),
        "net:outbound" => ("访问网络", "access the network"),
        "secret:rw" => ("读写插件密钥", "read/write plugin secrets"),
        "backup" => ("创建与恢复备份", "create and restore backups"),
        "files:read" => ("读取服务器文件", "read server files"),
        "files:write" => ("写入服务器文件", "write server files"),
        "server:lifecycle" => ("启动与停止服务器", "start and stop servers"),
        "content" => ("管理模组与整合包内容", "manage mod and modpack content"),
        "java" => ("管理 Java 运行时", "manage Java runtimes"),
        "process:spawn" => ("启动子进程", "spawn subprocesses"),
        "mc:client" => ("以玩家身份连服", "connect as a player"),
        _ => ("其它能力", "another capability"),
    }
}

/// 列出待授权能力并一次确认；非交互模式下只有 `--yes` 才授予
fn approve_pending(
    service: &PluginService,
    id: &PluginId,
    pending: &[Capability],
    printer: Printer,
) -> Result<Vec<Capability>> {
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    if printer.is_human() {
        let lang = printer.lang();
        let mut lines = vec![crate::i18n::pick(
            lang,
            "插件申请以下权限：",
            "the plugin requests these capabilities:",
        )];
        for cap in pending {
            let (zh, en) = capability_summary(*cap);
            lines.push(format!("  - {cap}  {}", crate::i18n::pick(lang, zh, en)));
        }
        printer.print_section(&lines.join("\n"));
    }
    let confirmed = if printer.is_interactive() {
        crate::prompts::confirm_or_yes(
            printer,
            &crate::i18n::pick(
                printer.lang(),
                "授予以上权限？",
                "grant these capabilities?",
            ),
            true,
        )?
    } else {
        // 脚本模式不能静默授权：只有显式 `--yes` 才视为同意
        printer.assumes_yes()
    };
    if !confirmed {
        return Ok(Vec::new());
    }
    let mut granted = Vec::with_capacity(pending.len());
    for cap in pending {
        service.grant(id, *cap)?;
        granted.push(*cap);
    }
    Ok(granted)
}

/// 逐项补齐安装向导声明的配置
///
/// `fresh` 为真（全新安装）时询问全部字段：已有值作为输入默认值，密钥留空
/// 表示保持原值；升级只补仍为空的字段，避免每次升级重问一遍。
fn run_setup(
    service: &PluginService,
    id: &PluginId,
    printer: Printer,
    fresh: bool,
) -> Result<SetupReport> {
    let Ok(descriptor) = service.descriptor(id) else {
        return Ok(SetupReport::default());
    };
    let Some(setup) = descriptor.setup.as_ref() else {
        return Ok(SetupReport::default());
    };
    let current = service.config_get(id)?;
    let values = current.get("values").cloned().unwrap_or_else(|| json!({}));
    let interactive = printer.is_interactive();
    let lang = printer.lang();
    let mut patch = Map::new();
    let mut report = SetupReport {
        start: setup.start,
        ..SetupReport::default()
    };

    for field in fields_to_ask(setup, &values, fresh) {
        let existing = value_at(&values, &field.path).cloned();
        let missing = value_missing(&values, &field.path);
        if !interactive {
            if field.required && missing {
                report.missing.push(field.path.clone());
            }
            continue;
        }
        // 重装时留空表示沿用旧值，但旧值本身为空不算「沿用」
        let kept = existing
            .as_ref()
            .is_some_and(|value| !value_is_empty(value));
        let value = loop {
            let Some(value) = prompt_setup_field(field, printer, existing.as_ref())? else {
                break None;
            };
            if field.required && value_is_empty(&value) {
                if kept {
                    break None;
                }
                printer.print_section(&crate::i18n::pick(
                    lang,
                    format!("{} 为必填项，请重新输入", field.label),
                    format!("{} is required; please try again", field.label),
                ));
                continue;
            }
            match mcsup::domain::plugin::schema::validate_instance(
                &field.schema,
                &value,
                &field.path,
            ) {
                Ok(()) => break Some(value),
                Err(err) => printer.print_section(&crate::i18n::pick(
                    lang,
                    format!("{} 无效：{err}", field.label),
                    format!("{} is invalid: {err}", field.label),
                )),
            }
        };
        if let Some(value) = value {
            set_path(&mut patch, &field.path, value);
            report.configured.push(field.path.clone());
        } else if field.required && missing {
            report.missing.push(field.path.clone());
        }
    }

    if !patch.is_empty() {
        service.config_merge(id, &Value::Object(patch))?;
    }
    Ok(report)
}

/// 安装（`fresh`）问全部字段，升级只问仍为空的字段
fn fields_to_ask<'a>(
    setup: &'a mcsup::domain::plugin::descriptor::ResolvedSetup,
    values: &Value,
    fresh: bool,
) -> Vec<&'a ResolvedSetupField> {
    setup
        .fields
        .iter()
        .filter(|field| fresh || value_missing(values, &field.path))
        .collect()
}

/// 询问单个字段；返回 `None` 表示留空跳过或沿用已有值
///
/// 已有值不预填成输入默认值：否则一个回车就把旧值当成新输入提交，用户
/// 也分不清「默认值」与「上次填的值」。改为在帮助行标注「已设置，留空保持
/// 不变」。
fn prompt_setup_field(
    field: &ResolvedSetupField,
    printer: Printer,
    existing: Option<&Value>,
) -> Result<Option<Value>> {
    let lang = printer.lang();
    let keeps = existing.is_some_and(|value| !value_is_empty(value));
    let help = setup_help(field, keeps && is_free_form(&field.kind), lang);
    // 已有值时不预填；否则只把 schema 默认值作为提示
    let default_text = if keeps {
        String::new()
    } else {
        setup_default_text(field)
    };
    let value = match &field.kind {
        SetupKind::Boolean => {
            let default = existing
                .and_then(Value::as_bool)
                .or_else(|| field.default.as_ref().and_then(Value::as_bool))
                .unwrap_or(false);
            json!(crate::prompts::select_bool_with_help(
                lang,
                &field.label,
                default,
                &help
            )?)
        }
        SetupKind::Text { choices } if !choices.is_empty() => {
            let default = existing
                .and_then(Value::as_str)
                .or_else(|| field.default.as_ref().and_then(Value::as_str));
            let picked =
                crate::prompts::select_choice(lang, &field.label, choices, default, &help)?;
            json!(picked)
        }
        SetupKind::Text { .. } => {
            let input = if field.secret {
                crate::prompts::input_secret(lang, &field.label, &help)?
            } else {
                crate::prompts::input_text(lang, &field.label, &default_text, &help)?
            };
            if input.is_empty() {
                return Ok(None);
            }
            json!(input)
        }
        SetupKind::Integer => {
            let input = crate::prompts::input_text(lang, &field.label, &default_text, &help)?;
            if input.trim().is_empty() {
                return Ok(None);
            }
            // 解析失败也交给 schema 复校验，统一走「重输」分支
            match input.trim().parse::<i64>() {
                Ok(number) => json!(number),
                Err(_) => json!(input.trim()),
            }
        }
        SetupKind::Number => {
            let input = crate::prompts::input_text(lang, &field.label, &default_text, &help)?;
            if input.trim().is_empty() {
                return Ok(None);
            }
            match input.trim().parse::<f64>() {
                Ok(number) => json!(number),
                Err(_) => json!(input.trim()),
            }
        }
        SetupKind::StringList { choices } if !choices.is_empty() => {
            let current = existing
                .and_then(Value::as_array)
                .or_else(|| field.default.as_ref().and_then(Value::as_array))
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let picked = crate::prompts::select_many(
                lang,
                &field.label,
                &help,
                choices,
                &current,
                field.required,
            )?;
            if picked.is_empty() {
                return Ok(None);
            }
            json!(picked)
        }
        SetupKind::StringList { .. } => {
            let input = crate::prompts::input_text(lang, &field.label, &default_text, &help)?;
            let items = split_list(&input);
            if items.is_empty() {
                return Ok(None);
            }
            json!(items)
        }
    };
    Ok(Some(value))
}

/// 自由输入（相对于选择器 / 多选框）的字段：留空才有「保持原值」语义
fn is_free_form(kind: &SetupKind) -> bool {
    match kind {
        SetupKind::Text { choices } | SetupKind::StringList { choices } => choices.is_empty(),
        SetupKind::Integer | SetupKind::Number => true,
        SetupKind::Boolean => false,
    }
}

/// 帮助行：描述 + 多选操作提示 + 已有值提示
fn setup_help(field: &ResolvedSetupField, keeps: bool, lang: crate::locale::Lang) -> String {
    let mut parts = Vec::new();
    if let Some(description) = field.description.as_ref().filter(|text| !text.is_empty()) {
        parts.push(description.clone());
    }
    if matches!(&field.kind, SetupKind::StringList { choices } if !choices.is_empty()) {
        parts.push(
            crate::i18n::pick(lang, "空格切换、回车确认", "space toggles, enter confirms").clone(),
        );
    }
    if keeps {
        parts.push(
            crate::i18n::pick(
                lang,
                "已设置；留空保持不变",
                "already set; leave blank to keep it",
            )
            .clone(),
        );
    }
    parts.join("；")
}

/// schema 默认值渲染成输入框提示文本
fn setup_default_text(field: &ResolvedSetupField) -> String {
    match field.default.as_ref() {
        None => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        Some(other) => other.to_string(),
    }
}

/// 列表输入：逗号（中英文）或分号分隔
fn split_list(input: &str) -> Vec<String> {
    input
        .split([',', '，', ';', '；'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn value_is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        Value::Bool(_) | Value::Number(_) => false,
    }
}

fn value_missing(values: &Value, path: &str) -> bool {
    value_at(values, path).is_none_or(value_is_empty)
}

/// 按 `qq.appId` 这样的点分路径读取配置值
fn value_at<'a>(values: &'a Value, path: &str) -> Option<&'a Value> {
    let mut node = values;
    for segment in path.split('.') {
        node = node.get(segment)?;
    }
    Some(node)
}

/// 把 `qq.appId` 写进嵌套补丁对象
fn set_path(map: &mut Map<String, Value>, path: &str, value: Value) {
    let segments = path.split('.').collect::<Vec<_>>();
    set_path_at(map, &segments, value);
}

fn set_path_at(map: &mut Map<String, Value>, segments: &[&str], value: Value) {
    match segments {
        [] => {}
        [last] => {
            map.insert((*last).to_owned(), value);
        }
        [head, rest @ ..] => {
            let entry = map
                .entry((*head).to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            if let Some(child) = entry.as_object_mut() {
                set_path_at(child, rest, value);
            }
        }
    }
}

/// 启用并启动；已在运行时重启以加载新配置
fn activate(service: &PluginService, id: &PluginId) -> Result<()> {
    service.enable(id)?;
    ensure_host()?;
    if service.status(id)?.session.is_some() {
        service.restart(id, Duration::from_secs(10))?;
    } else {
        service.start(id)?;
    }
    Ok(())
}

/// 向导前半段：确认待授权能力，再按 descriptor 的 setup 逐项补配置
///
/// `fresh` 区分安装与升级：安装始终询问全部字段（已有值作默认），
/// 升级只补仍为空的字段。
fn approve_and_configure(
    service: &PluginService,
    id: &PluginId,
    pending: &[Capability],
    setup: bool,
    fresh: bool,
    printer: Printer,
) -> Result<(Vec<Capability>, Vec<Capability>, SetupReport)> {
    let granted = if setup {
        approve_pending(service, id, pending, printer)?
    } else {
        Vec::new()
    };
    let remaining = pending
        .iter()
        .copied()
        .filter(|cap| !granted.contains(cap))
        .collect::<Vec<_>>();
    let report = if setup {
        run_setup(service, id, printer, fresh)?
    } else {
        SetupReport::default()
    };
    Ok((granted, remaining, report))
}

/// 权限齐备且必填配置无缺口时才允许自动启动
fn should_start(report: &SetupReport, remaining: &[Capability]) -> bool {
    report.start && remaining.is_empty() && report.missing.is_empty()
}

/// 权限齐备才启动；缺权限或启动失败都只提示，不视为安装 / 升级失败
fn start_if_ready(
    service: &PluginService,
    id: &PluginId,
    remaining: &[Capability],
    printer: Printer,
    failure_zh: &str,
    failure_en: &str,
) -> bool {
    if !remaining.is_empty() {
        printer.print_section(&crate::i18n::pick(
            printer.lang(),
            format!(
                "仍有权限未授予，已跳过启动；运行 `mcsup plugin grant {id} <capability>` 后重试"
            ),
            format!(
                "some capabilities are still pending, so start was skipped; run `mcsup plugin grant {id} <capability>` and retry"
            ),
        ));
        return false;
    }
    match activate(service, id) {
        Ok(()) => true,
        Err(err) => {
            printer.print_section(&crate::i18n::pick(
                printer.lang(),
                format!("{failure_zh}：{err:#}\n运行 `mcsup plugin logs {id}` 查看原因"),
                format!("{failure_en}: {err:#}\nrun `mcsup plugin logs {id}` for details"),
            ));
            false
        }
    }
}

/// 必填配置仍未补齐时给出可直接复制的修复命令
fn print_missing(printer: Printer, id: &PluginId, missing: &[String]) {
    if missing.is_empty() {
        return;
    }
    printer.print_section(&crate::i18n::pick(
        printer.lang(),
        format!(
            "必填配置仍缺失：{}；运行 `mcsup plugin config set {id} <key> <value>` 补齐",
            missing.join(", ")
        ),
        format!(
            "required configuration is still missing: {}; run `mcsup plugin config set {id} <key> <value>` to fill it",
            missing.join(", ")
        ),
    ));
}

#[allow(clippy::too_many_arguments)]
fn install(
    source: Option<String>,
    from: Option<PathBuf>,
    url: Option<String>,
    git: Option<String>,
    reference: Option<String>,
    sha256: Option<String>,
    yes: bool,
    setup: bool,
    printer: Printer,
) -> Result<()> {
    let origin = resolve_origin(source, from, url, git, reference)?;
    let expected = sha256.map(|hex| Checksum::new(ChecksumAlgorithm::Sha256, hex));
    let service = open()?;
    // 安装前确认（docs §15.5）：`--yes` 或非交互模式直接执行。待确认能力
    // 在安装后展示，grant 仍需显式确认，这里只确认「装」这个动作本身。
    if printer.is_human() && !yes {
        let origin_text = match &origin {
            ArtifactOrigin::Local(path) => path.display().to_string(),
            ArtifactOrigin::Url(url) => url.clone(),
            ArtifactOrigin::Git { url, rev } => match rev {
                Some(rev) => format!("git {url}@{rev}"),
                None => format!("git {url}"),
            },
        };
        let confirmed = crate::prompts::confirm_or_yes(
            printer,
            &crate::i18n::pick(
                printer.lang(),
                format!("确认安装插件（来源：{origin_text}）？"),
                format!("install plugin from {origin_text}?"),
            ),
            false,
        )?;
        if !confirmed {
            return Ok(());
        }
    }
    let spinner = TaskSpinner::new(
        crate::i18n::pick(printer.lang(), "正在安装插件…", "installing plugin…"),
        printer.is_human(),
    );
    let outcome = service
        .install(&origin, expected.as_ref(), false)
        .map_err(anyhow::Error::from);
    spinner.finish();
    let outcome = outcome?;
    let record = outcome.record;
    let pending = outcome.pending.iter().copied().collect::<Vec<_>>();

    let (granted, remaining, report) =
        approve_and_configure(&service, &record.id, &pending, setup, true, printer)?;
    // 安装向导默认启动；`--no-setup` 时 report.start 为 false，保持旧的
    // 「装完不启动」行为。必填配置仍缺失时也不启动，避免带着坏配置反复崩溃。
    let started = if should_start(&report, &remaining) {
        start_if_ready(
            &service,
            &record.id,
            &remaining,
            printer,
            "配置已保存，但启动失败",
            "configuration saved, but start failed",
        )
    } else {
        false
    };
    print_missing(printer, &record.id, &report.missing);

    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!(
                "已安装{} {} {}",
                if started { "并启动" } else { "" },
                record.id,
                record.version
            ),
            format!(
                "installed{} {} {}",
                if started { " and started" } else { "" },
                record.id,
                record.version
            ),
        ),
        &json!({
            "id": record.id.to_string(),
            "version": record.version,
            "granted": granted.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "pending": remaining.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "configured": report.configured,
            "missing": report.missing,
            "started": started,
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

fn update(
    id: &str,
    to: Option<String>,
    prune: bool,
    yes: bool,
    setup: bool,
    printer: Printer,
) -> Result<()> {
    let id = parse_id(id)?;
    let service = open()?;
    let spinner_text = if to.is_some() {
        "正在切换插件版本…"
    } else {
        "正在升级插件…"
    };
    if printer.is_human() && !yes {
        let confirmed = crate::prompts::confirm_or_yes(
            printer,
            &crate::i18n::pick(
                printer.lang(),
                if to.is_some() {
                    format!(
                        "确认切换 {id} 到版本 {}？",
                        to.as_deref().unwrap_or_default()
                    )
                } else {
                    format!("确认升级 {id}？")
                },
                if to.is_some() {
                    format!(
                        "switch {id} to version {}?",
                        to.as_deref().unwrap_or_default()
                    )
                } else {
                    format!("update {id}?")
                },
            ),
            false,
        )?;
        if !confirmed {
            return Ok(());
        }
    }
    let spinner = TaskSpinner::new(
        crate::i18n::pick(printer.lang(), spinner_text, "updating plugin…"),
        printer.is_human(),
    );
    let result = if let Some(to) = to {
        // 目标版本已安装：直接切换 active version
        let to = to.trim().to_owned();
        service
            .activate_version(&id, &to, prune)
            .map(|record| (record.id, record.version))
            .map_err(anyhow::Error::from)
    } else {
        upgrade_from_source(&service, &id, prune)
    };
    spinner.finish();
    let (updated_id, version) = result?;
    let record = service.record(&updated_id)?;
    let pending = pending_of(&record);
    let (granted, remaining, report) =
        approve_and_configure(&service, &updated_id, &pending, setup, false, printer)?;

    // 升级沿用原有的启用状态：用户主动停用的插件不会被升级顺手拉起。
    // 必填配置仍缺失时同样跳过重启，先让用户补齐再手动启动。
    let started = if record.enabled && remaining.is_empty() && report.missing.is_empty() {
        start_if_ready(
            &service,
            &updated_id,
            &remaining,
            printer,
            "升级完成，但重启失败",
            "updated, but restart failed",
        )
    } else {
        false
    };
    print_missing(printer, &updated_id, &report.missing);

    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!(
                "已切换 {updated_id} 到 {version}{}",
                if started { "（已重启）" } else { "" }
            ),
            format!(
                "updated {updated_id} to {version}{}",
                if started { " (restarted)" } else { "" }
            ),
        ),
        &json!({
            "id": updated_id.to_string(),
            "version": version,
            "prune": prune,
            "granted": granted.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "pending": remaining.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "configured": report.configured,
            "missing": report.missing,
            "started": started,
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

/// 按记录里的安装来源重新拉取并升级
fn upgrade_from_source(
    service: &PluginService,
    id: &PluginId,
    prune: bool,
) -> Result<(PluginId, String)> {
    let record = service.record(id)?;
    let origin = origin_from_source(record.source.as_deref()).ok_or_else(|| {
        AppDiagnostic::usage(
            format!("插件 {id} 没有可复用的安装来源；用 `mcsup plugin install <source>` 升级"),
            None,
        )
    })?;
    let outcome = service.install(&origin, None, prune)?;
    Ok((outcome.record.id, outcome.record.version))
}

fn remove(id: &str, purge: bool, keep_secrets: bool, printer: Printer) -> Result<()> {
    let id = parse_id(id)?;
    let service = open()?;
    service.remove_with(&id, purge, keep_secrets)?;
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!(
                "已卸载 {id}{}{}",
                if purge { "（含数据）" } else { "" },
                if keep_secrets { "，保留密钥" } else { "" }
            ),
            format!(
                "removed {id}{}{}",
                if purge { " (with data)" } else { "" },
                if keep_secrets { ", secrets kept" } else { "" }
            ),
        ),
        &json!({ "id": id.to_string(), "purge": purge, "keep_secrets": keep_secrets }),
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
    yes: bool,
    printer: Printer,
    json: bool,
) -> Result<()> {
    let printer = json_printer(printer, json);
    let id = parse_id(id)?;
    let input = read_input(input)?;
    ensure_host()?;
    let service = open()?;
    // 风险确认：mutating / destructive 的 op 在动手前问一次。`--yes` 跳过
    // 提问；JSON 模式没有交互终端，直接执行（调用方是脚本，结果可读回）。
    let descriptor = service.descriptor(&id)?;
    let resolved = descriptor.ops.iter().find(|resolved| resolved.op.id == op);
    if let Some(resolved) = resolved
        && printer.is_human()
        && !yes
    {
        let required = mcsup::auth::Role::for_risk(resolved.op.risk.as_str());
        let needs_confirm = !matches!(required, mcsup::auth::Role::Viewer);
        if needs_confirm {
            let confirmed = crate::prompts::confirm_or_yes(
                printer,
                &crate::i18n::pick(
                    printer.lang(),
                    format!(
                        "调用 {} 的 op `{}`（风险：{}，需要 {} 角色），继续？",
                        id,
                        op,
                        resolved.op.risk.as_str(),
                        required
                    ),
                    format!(
                        "call op `{op}` of {id} (risk: {}, requires {} role); continue?",
                        resolved.op.risk.as_str(),
                        required
                    ),
                ),
                false,
            )?;
            if !confirmed {
                return Ok(());
            }
        }
    }
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

fn flag_text(record: &PluginRecord) -> &'static str {
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
fn revoked_finding(service: &PluginService, record: &PluginRecord, skip: bool) -> Option<Value> {
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
    // 压缩包 / 目录：重新走一遍来源校验；插件 id：按安装记录找到产物目录。
    let (target_path, id, recheck) = if let Ok(id) = PluginId::parse(target) {
        let record = service.record(&id)?;
        (record.artifact_dir, Some(record.id), false)
    } else {
        let path = PathBuf::from(target);
        if !path.exists() {
            return Err(
                AppDiagnostic::not_found(format!("找不到插件 id 或目录: {target}"), None).into(),
            );
        }
        (path.clone(), None, path.is_file())
    };
    let verifier = mcsup::provider::plugin::PluginArtifactVerifier::new();
    let artifact = if recheck {
        // 未安装的文件：先解包并计算来源摘要，再与旁路 `.sha256` 比对。
        let fetched = service
            .fetch_artifact(&ArtifactOrigin::Local(target_path))
            .map_err(anyhow::Error::from)?;
        verifier
            .verify(&fetched.artifact, expected.as_ref())
            .map_err(anyhow::Error::from)?
    } else {
        verifier
            .verify_installed(&target_path, expected.as_ref())
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

/// 检查单个插件的产物；撤销命中时不再继续看产物
fn scan_record(
    service: &PluginService,
    record: &PluginRecord,
    revocation_unavailable: bool,
) -> Vec<Value> {
    let id = record.id.to_string();
    let artifact = &record.artifact_dir;
    if !artifact.is_dir() {
        return vec![json!({
            "level": "error",
            "id": id,
            "message": format!("产物目录缺失: {}", artifact.display()),
        })];
    }
    if let Some(message) = revoked_finding(service, record, revocation_unavailable) {
        return vec![message];
    }
    let mut findings = Vec::new();
    if let Err(err) = std::fs::read(artifact.join("manifest.json"))
        .map_err(anyhow::Error::from)
        .and_then(|bytes| Manifest::from_json(&bytes).map_err(|err| anyhow!("{err}")))
    {
        findings.push(json!({
            "level": "error",
            "id": id,
            "message": format!("manifest 非法: {err}"),
        }));
    }
    if let Err(err) = std::fs::read(artifact.join("descriptor.json"))
        .map_err(anyhow::Error::from)
        .and_then(|bytes| Descriptor::from_json(&bytes, &record.id).map_err(|err| anyhow!("{err}")))
    {
        findings.push(json!({
            "level": "error",
            "id": id,
            "message": format!("descriptor 非法: {err}"),
        }));
    }
    if record.needs_approval() {
        findings.push(json!({
            "level": "warning",
            "id": id,
            "message": "存在待确认能力，插件不会启动",
        }));
    }
    findings
}

/// 机器密钥不可用时密钥只能靠 0600 文件权限保护。这是安全降级，必须让用户
/// 看见，不能因为「加密代码存在」就当作加密生效。
fn secret_protection_finding(protection: SecretProtection) -> Option<Value> {
    (protection == SecretProtection::FilePermissions).then(|| {
        json!({
            "level": "warning",
            "id": Value::Null,
            "message": "插件密钥未加密：系统钥匙串不可用，secrets.json 仅由 0600 文件权限保护",
        })
    })
}

fn doctor(printer: Printer, json: bool) -> Result<()> {
    let printer = json_printer(printer, json);
    // 密钥保护方式只有装配层知道（机器密钥来自钥匙串）；这里多取一次整体，
    // 避免 CLI 自己拼 `plugin-data` 路径去猜。
    let mc = bootstrap::open_mcsup()?;
    let protection = mc.plugin_api.secrets().protection();
    let service = mc.plugins;
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
    if let Some(finding) = secret_protection_finding(protection) {
        findings.push(finding);
    }
    for record in &records {
        findings.extend(scan_record(&service, record, revocation_error.is_some()));
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
fn artifact_root(record: &PluginRecord) -> &Path {
    &record.artifact_dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcsup::domain::plugin::descriptor::ResolvedSetup;
    use serde_json::json;

    fn report(start: bool, missing: &[&str]) -> SetupReport {
        SetupReport {
            configured: Vec::new(),
            missing: missing.iter().map(|path| (*path).to_owned()).collect(),
            start,
        }
    }

    /// 必填配置没补齐时绝不能自动启动：否则插件带着坏配置反复崩溃
    #[test]
    fn auto_start_requires_complete_required_config() {
        assert!(should_start(&report(true, &[]), &[]));
        assert!(!should_start(&report(true, &["qq.appId"]), &[]));
        assert!(!should_start(&report(false, &[]), &[]));
        let pending = [Capability::ConsoleWrite];
        assert!(!should_start(&report(true, &[]), &pending));
    }

    #[test]
    fn missing_paths_track_nested_empties() {
        let values = json!({ "platforms": ["qq"], "qq": { "appId": "a", "groups": [] } });
        assert!(!value_missing(&values, "platforms"));
        assert!(!value_missing(&values, "qq.appId"));
        assert!(value_missing(&values, "qq.clientSecret"));
        assert!(value_missing(&values, "qq.groups"));
        assert_eq!(value_at(&values, "qq.appId"), Some(&json!("a")));
    }

    /// 重装已有配置的插件时仍必须询问 `AppID` / `ClientSecret` 等必填项
    #[test]
    fn fresh_install_asks_every_field_even_when_configured() {
        let field = |path: &str| ResolvedSetupField {
            path: path.to_owned(),
            label: path.to_owned(),
            description: None,
            required: true,
            secret: false,
            default: None,
            kind: SetupKind::Text {
                choices: Vec::new(),
            },
            schema: json!({ "type": "string" }),
        };
        let setup = ResolvedSetup {
            start: true,
            fields: vec![field("qq.appId"), field("qq.groups")],
        };
        let values = json!({ "qq": { "appId": "a", "groups": [] } });

        assert_eq!(fields_to_ask(&setup, &values, true).len(), 2);
        let update = fields_to_ask(&setup, &values, false);
        assert_eq!(update.len(), 1);
        assert_eq!(update[0].path, "qq.groups");
    }

    #[test]
    fn set_path_builds_nested_patch_and_replaces_non_object() {
        let mut patch = Map::new();
        set_path(&mut patch, "qq.appId", json!("a"));
        set_path(&mut patch, "qq.clientSecret", json!("b"));
        assert_eq!(patch["qq"], json!({ "appId": "a", "clientSecret": "b" }));

        let mut odd = Map::new();
        odd.insert("qq".to_owned(), json!("not-an-object"));
        set_path(&mut odd, "qq.groups", json!(["g"]));
        assert_eq!(odd["qq"], json!({ "groups": ["g"] }));
    }
}
