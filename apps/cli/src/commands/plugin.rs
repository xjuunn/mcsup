//! `plugin` 子命令与隐藏的 `__plugin-host`
//!
//! 入口只做参数解析、进度回显、结果渲染与错误映射。安装记录与授权在
//! [`mcsup::service::PluginService`]；进程、帧与会话在宿主进程；两者经控制
//! 通道相连。管理命令在本进程完成文件与记录操作后通知宿主 `reload`。

use crate::bootstrap;
use crate::cli::{PluginAction, PluginConfigAction, PluginRevocationAction};
use crate::commands::plugin_bindings::{BoundPlugins, StartGate, spawn_watch};
use crate::commands::plugin_host;
use crate::config;
use crate::diagnostic::AppDiagnostic;
use crate::output::{OutputFormat, Printer};
use crate::progress::TaskSpinner;
use anyhow::{Context, Result, anyhow};
use comfy_table::Table;
use mcsup::domain::plugin::PluginId;
use mcsup::domain::plugin::capability::Capability;
use mcsup::domain::plugin::descriptor::Descriptor;
use mcsup::domain::plugin::descriptor::{ChoicesSource, ResolvedSetupField, SetupKind};
use mcsup::domain::plugin::manifest::Manifest;
use mcsup::domain::plugin::revocation::RevocationList;
use mcsup::domain::plugin::source::PluginSource;
use mcsup::domain::server::artifact::{Checksum, ChecksumAlgorithm};
use mcsup::port::plugin::ArtifactVerifier as _;
use mcsup::port::plugin::{ArtifactOrigin, PluginRecord, SecretProtection, StreamSink};
use mcsup::service::{PluginService, RemoveData};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Read};
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
            reference,
            sha256,
            yes,
            no_setup,
        } => install(
            source, from, url, reference, sha256, yes, !no_setup, printer,
        ),
        PluginAction::Update {
            id,
            to,
            prune,
            yes,
            no_setup,
        } => update(&id, to, prune, yes, !no_setup, printer),
        PluginAction::Setup { id } => setup(&id, printer),
        PluginAction::Remove {
            id,
            keep_data,
            keep_secrets,
        } => remove(&id, keep_data, keep_secrets, printer),
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
    // 控制台 / 游戏内的 stop 不经过 CLI 编排：由常驻宿主补上「服务器全停 →
    // 收插件」，否则桥会在服务器停掉后一直空转
    spawn_watch(&host, mcsup.plugins.clone(), mcsup.lifecycle.clone());
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

/// 插件服务 + 服务器绑定编排：安装 / 启动前要看绑定服务器是否在跑
fn open_bound() -> Result<(PluginService, BoundPlugins)> {
    let mcsup = bootstrap::open_mcsup()?;
    Ok((mcsup.plugins.clone(), BoundPlugins::new(&mcsup)))
}

fn parse_id(raw: &str, lang: crate::locale::Lang) -> Result<PluginId> {
    PluginId::parse(raw).map_err(|err| {
        // 单行、随 `--lang` 切换：不留「中文 + 英文」两行说同一件事。领域
        // 错误只用来区分「格式不符」与「保留名」，文案由这里按语言给。
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("插件 id 非法: {raw}"),
                format!("invalid plugin id: {raw}"),
            ),
            Some(match err.reason {
                mcsup::domain::plugin::IdReject::Format => crate::i18n::pick(
                    lang,
                    "id 只允许小写字母、数字与连字符，首字符必须是字母，长度 2..=64",
                    "ids match ^[a-z][a-z0-9-]{1,63}$: lowercase letters, digits and dashes only",
                ),
                mcsup::domain::plugin::IdReject::Reserved => crate::i18n::pick(
                    lang,
                    format!("{raw} 是平台保留名（mcsup / core / mod）"),
                    format!("{raw} is reserved by the platform (mcsup / core / mod)"),
                ),
            }),
        )
        .into()
    })
}

fn parse_capability(raw: &str, lang: crate::locale::Lang) -> Result<Capability> {
    Capability::parse(raw).map_err(|_| {
        let available = Capability::ALL.map(Capability::as_str).join(" / ");
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("能力名非法: {raw}"),
                format!("unknown capability: {raw}"),
            ),
            Some(crate::i18n::pick(
                lang,
                format!("可用能力：{available}"),
                format!("available capabilities: {available}"),
            )),
        )
        .into()
    })
}

fn json_printer(printer: Printer, json: bool) -> Printer {
    if json {
        printer.with_format(OutputFormat::Json)
    } else {
        printer
    }
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
#[allow(clippy::too_many_lines)] // 条件、交互式与普通输入共享同一段提问序列
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
    let record = service.record(id)?;
    let current = service.config_get(id)?;
    let values = current.get("values").cloned().unwrap_or_else(|| json!({}));
    let interactive = printer.is_interactive();
    let lang = printer.lang();
    let mut patch = Map::new();
    let mut report = SetupReport {
        start: setup.start,
        ..SetupReport::default()
    };
    // 只有声明了 `choicesFrom` 的字段才读服务器列表；非交互模式完全不碰。
    let servers = if interactive
        && setup
            .fields
            .iter()
            .any(|field| field.choices_from.is_some())
    {
        super::server_ops::server_names()
    } else {
        Vec::new()
    };

    // 已收集的答案：旧配置 + 本次刚填的值。条件字段靠它判定要不要问，
    // 所以每问完一项都要回填，不能只看初始配置。
    let mut answered = values.clone();
    // 本次向导已经拿到的路径：交互式 hook 可能一次回填多个字段，后面的
    // 同名项即使 `fresh` 也不再重复提问。
    let mut answered_now = BTreeSet::new();
    for field in &setup.fields {
        if !should_ask(field, &answered, fresh, &answered_now) {
            continue;
        }
        let existing = value_at(&answered, &field.path).cloned();
        let missing = value_missing(&answered, &field.path);
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
        // 交互式字段由插件自己的 setup hook 取值：宿主继承终端拉起插件，
        // 插件负责扫码 / 设备授权，结果经 0600 临时文件回传。一次登录可能
        // 同时产出多个配置项（如 token 与服务端给的基址），因此返回值是
        // 路径 → 值的映射，不是单个字符串。
        if field.interactive {
            let filled = prompt_interactive_field(
                &record,
                field,
                printer,
                existing.as_ref(),
                &setup.fields,
            )?;
            let Some(filled) = filled else {
                if field.required && missing {
                    report.missing.push(field.path.clone());
                }
                continue;
            };
            for (path, secret) in filled {
                let value = Value::String(secret);
                set_value_path(&mut answered, &path, value.clone());
                set_path(&mut patch, &path, value);
                report.configured.push(path.clone());
                answered_now.insert(path);
            }
            continue;
        }
        let value = loop {
            let Some(value) = prompt_setup_field(field, printer, existing.as_ref(), &servers)?
            else {
                // 必填项没有现有值可沿用：留空就是没填，必须重问，不能记一笔
                // `missing` 就滑过去（那等于回车跳过必填）。
                if field.required && !kept {
                    printer.print_section(&crate::i18n::pick(
                        lang,
                        format!("{} 为必填项，不能留空，请重新输入", field.label),
                        format!(
                            "{} is required and cannot be blank; please try again",
                            field.label
                        ),
                    ));
                    continue;
                }
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
            set_value_path(&mut answered, &field.path, value.clone());
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

/// 交互式 setup 字段：拉起插件自身的 setup hook（schema 5）
///
/// 宿主只负责开临时文件、继承终端与回读结果，平台流程（二维码、设备授权）
/// 全在插件里。一次登录可能同时产出多个配置项，hook 用 JSON 对象回传
/// 路径 → 值；宿主逐项按声明字段的 schema 复校验后再落盘。
///
/// 已有值默认保持，用户主动确认后才重新授权；hook 失败时让用户选择重试、
/// 手工粘贴或跳过，不把一次扫码失败变成安装失败。
fn prompt_interactive_field(
    record: &PluginRecord,
    field: &ResolvedSetupField,
    printer: Printer,
    existing: Option<&Value>,
    fields: &[ResolvedSetupField],
) -> Result<Option<BTreeMap<String, String>>> {
    let lang = printer.lang();
    let keeps = existing.is_some_and(|value| !value_is_empty(value));
    if keeps {
        let again = crate::prompts::confirm_or_yes(
            printer,
            &crate::i18n::pick(
                lang,
                format!("{} 已设置，要重新授权吗？", field.label),
                format!("{} is already set; authorize again?", field.label),
            ),
            false,
        )?;
        if !again {
            return Ok(None);
        }
    }
    // 告诉插件本次可以回填哪些交互式字段；旧插件忽略这个变量
    let interactive_paths = fields
        .iter()
        .filter(|item| item.interactive)
        .map(|item| item.path.clone())
        .collect::<Vec<_>>();
    let mut manual = false;
    loop {
        let attempt = if manual {
            let text = crate::prompts::input_secret(lang, &field.label, "")?;
            let text = text.trim().to_owned();
            if text.is_empty() {
                printer.print_section(&crate::i18n::pick(
                    lang,
                    "不能为空，请重新输入",
                    "cannot be blank; please try again",
                ));
                continue;
            }
            Ok(BTreeMap::from([(field.path.clone(), text)]))
        } else {
            printer.print_section(&crate::i18n::pick(
                lang,
                format!("正在启动插件完成 {} 登录…", field.label),
                format!("starting the plugin to complete {} login…", field.label),
            ));
            run_setup_hook(record, field, &interactive_paths, printer)
        };
        match attempt {
            Ok(values) => match validate_interactive_values(fields, field, &values) {
                Ok(()) => return Ok(Some(values)),
                Err(reason) => printer.print_section(&reason),
            },
            Err(err) => printer.print_section(&crate::i18n::pick(
                lang,
                format!("登录失败：{err}"),
                format!("login failed: {err}"),
            )),
        }
        let options = vec![
            crate::i18n::pick(lang, "重试", "retry"),
            crate::i18n::pick(lang, "手工粘贴", "paste manually"),
            crate::i18n::pick(lang, "跳过", "skip"),
        ];
        let picked = crate::prompts::select_choice(
            lang,
            &crate::i18n::pick(lang, "接下来怎么做？", "what next?"),
            &options,
            None,
            "",
        )?;
        if picked == options[0] {
            manual = false;
        } else if picked == options[1] {
            manual = true;
        } else {
            return Ok(None);
        }
    }
}

/// 校验一次登录回填的全部字段
///
/// 只接受安装向导里声明过的字符串字段：路径写错或类型不符说明插件与
/// descriptor 漂移，宁可报错重试，也不把未声明的键写进配置。
fn validate_interactive_values(
    fields: &[ResolvedSetupField],
    trigger: &ResolvedSetupField,
    values: &BTreeMap<String, String>,
) -> std::result::Result<(), String> {
    if !values.contains_key(&trigger.path) {
        return Err(format!("登录结果没有返回 {}", trigger.path));
    }
    for (path, text) in values {
        let Some(target) = fields.iter().find(|item| &item.path == path) else {
            return Err(format!("登录结果包含未声明的字段 {path}"));
        };
        if !matches!(&target.kind, SetupKind::Text { .. }) {
            return Err(format!("{path} 不是字符串字段，不能由登录结果回填"));
        }
        if text.trim().is_empty() {
            return Err(format!("{path} 为空"));
        }
        mcsup::domain::plugin::schema::validate_instance(
            &target.schema,
            &Value::String(text.clone()),
            path,
        )
        .map_err(|err| format!("{path} 无效：{err}"))?;
    }
    Ok(())
}

/// 解析密钥文件内容：JSON 对象表示一次回填多个字段，其余按单个字符串
fn parse_setup_payload(text: &str, trigger: &str) -> Result<BTreeMap<String, String>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("plugin setup hook returned an empty value"));
    }
    if trimmed.starts_with('{') {
        let parsed: Value =
            serde_json::from_str(trimmed).context("plugin setup hook returned invalid JSON")?;
        let Value::Object(map) = parsed else {
            return Err(anyhow!("plugin setup hook returned a non-object payload"));
        };
        let mut values = BTreeMap::new();
        for (path, value) in map {
            let Value::String(text) = value else {
                return Err(anyhow!("plugin setup hook returned a non-string value"));
            };
            let text = text.trim();
            if text.is_empty() {
                return Err(anyhow!("plugin setup hook returned an empty value"));
            }
            values.insert(path, text.to_owned());
        }
        if !values.contains_key(trigger) {
            return Err(anyhow!(
                "plugin setup hook did not return the requested field {trigger}"
            ));
        }
        return Ok(values);
    }
    Ok(BTreeMap::from([(trigger.to_owned(), trimmed.to_owned())]))
}

/// 执行插件的 setup hook：继承终端，密钥经 0600 临时文件回传
///
/// 临时目录与密钥文件用完即删；插件只拿到路径，密钥不进 argv、不经过 shell。
/// 二维码这类需要排版的输出走独立的 UI 文件：宿主边等边渲染，插件不必自己
/// 处理终端编码与宽度。
fn run_setup_hook(
    record: &PluginRecord,
    field: &ResolvedSetupField,
    interactive_paths: &[String],
    printer: Printer,
) -> Result<BTreeMap<String, String>> {
    let dir = tempfile::Builder::new()
        .prefix("mcsup-setup-")
        .tempdir()
        .context("create setup temp dir")?;
    let secret_file = dir.path().join("secret");
    let ui_file = dir.path().join("ui");
    let (program, args) =
        mcsup::plugin::process::entry_command(&record.artifact_dir, &record.manifest.entry, &[])
            .map_err(|err| anyhow!("{err}"))?;
    let fields = serde_json::to_string(interactive_paths)
        .context("serialize interactive setup field list")?;
    let mut child = std::process::Command::new(&program)
        .args(&args)
        .current_dir(&record.artifact_dir)
        .env("MCSUP_SETUP_FIELD", &field.path)
        .env("MCSUP_SETUP_SECRET_FILE", &secret_file)
        .env("MCSUP_SETUP_UI_FILE", &ui_file)
        .env("MCSUP_SETUP_FIELDS", fields)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| format!("run plugin setup hook {}", program.display()))?;
    let mut ui = SetupUi::new(&ui_file, printer);
    let status = loop {
        ui.drain();
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("wait for plugin setup hook {}", program.display()))?
        {
            break status;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    // 子进程退出前写入的最后一屏内容也要显示
    ui.drain();
    if !status.success() {
        let code = status
            .code()
            .map_or_else(|| status.to_string(), |code| code.to_string());
        return Err(anyhow!("plugin setup hook exited with {code}"));
    }
    let text = std::fs::read_to_string(&secret_file)
        .with_context(|| format!("read {}", secret_file.display()))?;
    parse_setup_payload(&text, &field.path)
}

/// 交互式 setup 的界面事件读取器
///
/// 插件按行追加 JSON：`{"type":"qr","content":"…"}` 由宿主编码并居中渲染，
/// `{"type":"status","text":"…"}` 原样作为进度行。未知事件与坏行忽略，
/// 不能因为一行排版内容让登录失败。
struct SetupUi<'a> {
    path: &'a Path,
    offset: usize,
    pending: Vec<u8>,
    printer: Printer,
}

impl<'a> SetupUi<'a> {
    fn new(path: &'a Path, printer: Printer) -> Self {
        Self {
            path,
            offset: 0,
            pending: Vec::new(),
            printer,
        }
    }

    /// 读取并渲染尚未处理的事件；文件还没创建时静默跳过
    fn drain(&mut self) {
        let Ok(bytes) = std::fs::read(self.path) else {
            return;
        };
        if bytes.len() <= self.offset {
            return;
        }
        self.pending.extend_from_slice(&bytes[self.offset..]);
        self.offset = bytes.len();
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line = self.pending.drain(..=end).collect::<Vec<_>>();
            let Ok(text) = std::str::from_utf8(&line) else {
                continue;
            };
            self.render(text.trim());
        }
    }

    #[allow(clippy::print_stdout)]
    fn render(&self, line: &str) {
        if line.is_empty() {
            return;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("qr") => {
                let Some(content) = event.get("content").and_then(Value::as_str) else {
                    return;
                };
                let content = content.trim();
                if content.is_empty() {
                    return;
                }
                let label = event.get("label").and_then(Value::as_str).map_or_else(
                    || {
                        crate::i18n::pick(
                            self.printer.lang(),
                            "用手机扫描二维码：",
                            "scan the QR code:",
                        )
                    },
                    str::to_owned,
                );
                self.printer.print_section(&label);
                self.printer.print_note(&format!("  {content}"));
                if std::io::stdout().is_terminal()
                    && let Some(qr) = crate::qr::render_centered(
                        content,
                        crossterm::terminal::size().map_or(80, |(width, _)| usize::from(width)),
                    )
                {
                    print!("{qr}");
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }
            }
            Some("status") => {
                if let Some(text) = event.get("text").and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    self.printer.print_note(text.trim());
                }
            }
            _ => {}
        }
    }
}

/// 该字段这次要不要问：条件成立（未声明条件视为成立），且没有被本次向导的
/// 交互式 hook 顺带回填，且全新安装或仍为空
///
/// 条件由插件的 `setup.fields[].when` 声明，例如「没选 qq 就不问 QQ 凭据」。
/// 条件不成立的字段既不问，也不计入 `missing`：它不是「没填」，而是用不着。
/// `answered_now` 记录本次已由 hook 填好的路径：一次扫码可能同时给出 token 与
/// 服务端基址，后面的同名项即使 `fresh` 也不能再问一遍。
fn should_ask(
    field: &ResolvedSetupField,
    values: &Value,
    fresh: bool,
    answered_now: &BTreeSet<String>,
) -> bool {
    if answered_now.contains(&field.path) {
        return false;
    }
    let active = field
        .when
        .as_ref()
        .is_none_or(|when| when.is_satisfied(values));
    active && (fresh || value_missing(values, &field.path))
}

/// 询问单个字段；返回 `None` 表示留空跳过或沿用已有值
///
/// 已有值不预填成输入默认值：否则一个回车就把旧值当成新输入提交，用户
/// 也分不清「默认值」与「上次填的值」。改为在帮助行标注「已设置，留空保持
/// 不变」。声明了 `choicesFrom` 的字段由宿主枚举候选（如已注册服务器），
/// 渲染成单选 / 多选，当前值即使不在候选里也会追加，保证看得见。
fn prompt_setup_field(
    field: &ResolvedSetupField,
    printer: Printer,
    existing: Option<&Value>,
    servers: &[String],
) -> Result<Option<Value>> {
    let lang = printer.lang();
    let offered = setup_options(field, servers);
    let select = !offered.is_empty();
    let keeps = existing.is_some_and(|value| !value_is_empty(value));
    // 提问下面不再渲染任何提示行：字段含义由 label 表达，向导保持一行一问。
    let default_text = if keeps {
        String::new()
    } else {
        setup_default_text(field)
    };
    match &field.kind {
        SetupKind::Boolean => {
            let default = existing
                .and_then(Value::as_bool)
                .or_else(|| field.default.as_ref().and_then(Value::as_bool))
                .unwrap_or(false);
            Ok(Some(json!(crate::prompts::select_bool_with_help(
                lang,
                &field.label,
                default,
                ""
            )?)))
        }
        SetupKind::Text { .. } if select => select_one(field, lang, existing, offered, ""),
        SetupKind::Text { .. } => prompt_text(field, lang, &default_text, ""),
        SetupKind::Integer => {
            let input = crate::prompts::input_text(lang, &field.label, &default_text, "")?;
            if input.trim().is_empty() {
                return Ok(None);
            }
            // 解析失败也交给 schema 复校验，统一走「重输」分支
            Ok(Some(match input.trim().parse::<i64>() {
                Ok(number) => json!(number),
                Err(_) => json!(input.trim()),
            }))
        }
        SetupKind::Number => {
            let input = crate::prompts::input_text(lang, &field.label, &default_text, "")?;
            if input.trim().is_empty() {
                return Ok(None);
            }
            Ok(Some(match input.trim().parse::<f64>() {
                Ok(number) => json!(number),
                Err(_) => json!(input.trim()),
            }))
        }
        SetupKind::StringList { .. } if select => select_list(field, lang, existing, offered, ""),
        SetupKind::StringList { .. } => prompt_list(field, lang, &default_text, ""),
    }
}

/// 字段的可选项：声明 `choicesFrom` 用宿主动态列表，否则用静态 `choices`
///
/// 动态来源没有可选项时退回静态声明（通常为空 → 自由输入），这样装机时
/// 还没有任何服务器也不会把安装卡死。
fn setup_options<'a>(field: &'a ResolvedSetupField, servers: &'a [String]) -> &'a [String] {
    match (&field.kind, field.choices_from) {
        (
            SetupKind::Text { choices } | SetupKind::StringList { choices },
            Some(ChoicesSource::Servers),
        ) => {
            if servers.is_empty() {
                choices
            } else {
                servers
            }
        }
        (SetupKind::Text { choices } | SetupKind::StringList { choices }, None) => choices,
        _ => &[],
    }
}

/// 单选：候选 + 当前值（即使已不在候选里）＋ schema 默认值定位光标
fn select_one(
    field: &ResolvedSetupField,
    lang: crate::locale::Lang,
    existing: Option<&Value>,
    offered: &[String],
    help: &str,
) -> Result<Option<Value>> {
    let current = existing
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned);
    let mut options = offered.to_vec();
    if let Some(value) = current.as_ref()
        && !options.contains(value)
    {
        options.push(value.clone());
    }
    let cursor = match current {
        Some(value) => Some(value),
        None => field
            .default
            .as_ref()
            .and_then(Value::as_str)
            .filter(|default| options.iter().any(|option| option.as_str() == *default))
            .map(ToOwned::to_owned),
    };
    let picked =
        crate::prompts::select_choice(lang, &field.label, &options, cursor.as_deref(), help)?;
    Ok(Some(json!(picked)))
}

/// 多选：候选 + 当前值（即使已不在候选里）
fn select_list(
    field: &ResolvedSetupField,
    lang: crate::locale::Lang,
    existing: Option<&Value>,
    offered: &[String],
    help: &str,
) -> Result<Option<Value>> {
    let current = existing
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect::<Vec<_>>()
        })
        .or_else(|| {
            field
                .default
                .as_ref()
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                        .collect::<Vec<_>>()
                })
        })
        .unwrap_or_default();
    let picked = crate::prompts::select_many(
        lang,
        &field.label,
        help,
        &choices_with_current(offered, &current),
        &current,
        field.required,
    )?;
    if picked.is_empty() {
        return Ok(None);
    }
    Ok(Some(json!(picked)))
}

/// 自由文本；留空表示跳过（必填项由调用方重问）
fn prompt_text(
    field: &ResolvedSetupField,
    lang: crate::locale::Lang,
    default_text: &str,
    help: &str,
) -> Result<Option<Value>> {
    let input = if field.secret {
        crate::prompts::input_secret(lang, &field.label, help)?
    } else {
        crate::prompts::input_text(lang, &field.label, default_text, help)?
    };
    if input.is_empty() {
        return Ok(None);
    }
    Ok(Some(json!(input)))
}

/// 逗号分隔的列表输入
fn prompt_list(
    field: &ResolvedSetupField,
    lang: crate::locale::Lang,
    default_text: &str,
    help: &str,
) -> Result<Option<Value>> {
    let input = crate::prompts::input_text(lang, &field.label, default_text, help)?;
    let items = split_list(&input);
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(json!(items)))
}

/// 多选选项 = 声明值 + 当前值里未声明的项
///
/// 少这一步，用户手写进配置、插件却没在 `choices` 里声明的值在向导里看不见，
/// 回车一提交就被静默丢掉。把它们追加成额外选项，至少看得见、可取消。
fn choices_with_current(choices: &[String], current: &[String]) -> Vec<String> {
    let mut offered = choices.to_vec();
    for value in current {
        if !offered.contains(value) {
            offered.push(value.clone());
        }
    }
    offered
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

/// 把单个答案写回整棵配置树（条件判定用）
fn set_value_path(values: &mut Value, path: &str, value: Value) {
    if !values.is_object() {
        *values = Value::Object(Map::new());
    }
    if let Value::Object(map) = values {
        set_path(map, path, value);
    }
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

/// 启用并保证在跑
///
/// 升级路径里服务层已经在新版本启动成功之后才算成功（`install_into` /
/// `activate_version` 都是这个次序），这里再 `restart` 会多出一整轮停机与
/// 重启审计事件——对启动慢的桥是实打实的额外中断。所以只在**没有会话**时
/// 启动。
fn activate(service: &PluginService, id: &PluginId, printer: Printer) -> Result<()> {
    let enabled = service
        .list()?
        .iter()
        .any(|record| &record.id == id && record.enabled);
    if !enabled {
        service.enable(id)?;
    }
    plugin_host::ensure(printer)?;
    if service.status(id)?.session.is_none() {
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

/// 门禁挡住启动时仍保证插件是「已启用」的期望状态
///
/// 绑定插件的运行与否由服务器决定：手动安装 / 启动被挡下时，插件应保持启用
/// 但进程不跑，这样之后 `server start` 的编排才会把它算进来（那份清单只收
/// 已启用插件）。启用失败只告警，用户仍可手动处理。
fn ensure_enabled(service: &PluginService, id: &PluginId, printer: Printer) {
    let already = service.list().is_ok_and(|records| {
        records
            .iter()
            .any(|record| &record.id == id && record.enabled)
    });
    if !already && let Err(err) = service.enable(id) {
        printer.print_warning(&format!("启用插件 {id} 失败：{err}"));
    }
}

/// 权限齐备且必填配置无缺口时才允许自动启动
fn should_start(report: &SetupReport, remaining: &[Capability]) -> bool {
    report.start && remaining.is_empty() && report.missing.is_empty()
}

/// 绑定服务器都没在跑时的统一提示（启动 / 重启被跳过）
fn print_gate_note(printer: Printer, id: &PluginId, servers: &[String], zh: &str, en: &str) {
    let lang = printer.lang();
    let first = servers.first().map_or("", String::as_str);
    let zh_list = servers.join("、");
    let en_list = servers.join(", ");
    printer.print_section(&crate::i18n::pick(
        lang,
        format!(
            "{id}：绑定的服务器（{zh_list}）都没在运行，{zh}；运行 `mcsup server start {first}` 会自动拉起"
        ),
        format!(
            "{id}: bound servers ({en_list}) are not running, so {en}; `mcsup server start {first}` starts it automatically"
        ),
    ));
}

/// 本机门禁：拉黑 / 隔离 / 停用 / 待授权时给出可操作的错误
///
/// 与宿主 `PluginHost::start` 的判定同源，但必须在绑定门禁**之前**先判一次：
/// 「绑定的服务器都没在运行」是成功返回（exit 0），会把真实原因盖掉，用户只
/// 看到「已跳过启动」，永远不知道该去 `unblock` / `release` / `enable`。
fn reject_unstartable(service: &PluginService, id: &PluginId, printer: Printer) -> Result<()> {
    let record = service.record(id)?;
    let lang = printer.lang();
    let (message, help) = if record.blocked {
        (
            format!("插件 {id} 已被本机拉黑，不会启动"),
            format!("`mcsup plugin unblock {id}`"),
        )
    } else if record.quarantined {
        (
            format!("插件 {id} 已隔离，不会启动"),
            format!("`mcsup plugin release {id}`"),
        )
    } else if !record.enabled {
        (
            format!("插件 {id} 已停用，不会启动"),
            format!("`mcsup plugin enable {id}`"),
        )
    } else if record.needs_approval() {
        (
            format!("插件 {id} 还有待授权能力，不会启动"),
            format!("`mcsup plugin setup {id}`"),
        )
    } else {
        return Ok(());
    };
    let en_message = format!("plugin {id} cannot start: {help}");
    Err(AppDiagnostic::conflict(
        crate::i18n::pick(lang, message, en_message),
        Some(crate::i18n::pick(
            lang,
            format!("先运行 {help}"),
            format!("run {help} first"),
        )),
    )
    .into())
}

/// 权限齐备才启动；缺权限、绑定服务器停着或启动失败都只提示，不视为安装 / 升级失败
///
/// 绑定服务器都停着时不解释——绑定插件的生命周期本来归服务器，装完不启动、等
/// `server start` 拉起是常态，说一句「已跳过启动」反而像出了问题。显式的
/// `plugin start` / `restart` / `setup` 仍会说明原因。
fn start_if_ready(
    bound: &BoundPlugins,
    service: &PluginService,
    id: &PluginId,
    remaining: &[Capability],
    printer: Printer,
    failure_zh: &str,
    failure_en: &str,
) -> bool {
    if let StartGate::AllStopped { .. } = bound.enforce_gate(id, printer) {
        ensure_enabled(service, id, printer);
        return false;
    }
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
    match activate(service, id, printer) {
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
    reference: Option<String>,
    sha256: Option<String>,
    yes: bool,
    setup: bool,
    printer: Printer,
) -> Result<()> {
    let origin = resolve_origin(source, from, url, reference)?;
    let expected = sha256.map(|hex| Checksum::new(ChecksumAlgorithm::Sha256, hex));
    let (service, bound) = open_bound()?;
    // 安装前确认（docs §15.5）：`--yes` 或非交互模式直接执行。待确认能力
    // 在安装后展示，grant 仍需显式确认，这里只确认「装」这个动作本身。
    if printer.is_human() && !yes {
        let origin_text = describe_origin(&origin);
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
            &bound,
            &service,
            &record.id,
            &remaining,
            printer,
            "配置已保存，但启动失败",
            "configuration saved, but start failed",
        )
    } else {
        // 升级时 service 会恢复「原本在跑」的状态；绑定服务器都停着的话
        // 这里把它收掉，别留下空转的桥。
        bound.enforce_gate(&record.id, printer);
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
    reference: Option<String>,
) -> Result<ArtifactOrigin> {
    let supplied = [source.is_some(), from.is_some(), url.is_some()]
        .into_iter()
        .filter(|given| *given)
        .count();
    if supplied != 1 {
        return Err(AppDiagnostic::usage(
            "必须且只能给出一个来源：<source> / --from / --url",
            None,
        )
        .into());
    }
    if let Some(url) = url {
        reject_ref_outside_github(reference.as_deref())?;
        return Ok(ArtifactOrigin::Url(url));
    }
    if let Some(path) = from {
        reject_ref_outside_github(reference.as_deref())?;
        return Ok(ArtifactOrigin::Local(absolute_local(path)?));
    }
    let Some(raw) = source else {
        return Err(AppDiagnostic::usage("缺少插件来源", None).into());
    };
    let parsed = PluginSource::parse_input(&raw)
        .map_err(|err| AppDiagnostic::usage(err.to_string(), None))?;
    match parsed {
        PluginSource::Github { owner, repo, rev } => {
            let rev = match (rev, reference) {
                (Some(_), Some(_)) => {
                    return Err(AppDiagnostic::usage(
                        "链接里已经带了版本（/tree/<ref>）；不要再给 --ref",
                        None,
                    )
                    .into());
                }
                (Some(rev), None) => Some(rev),
                (None, explicit) => explicit,
            };
            Ok(ArtifactOrigin::Github { owner, repo, rev })
        }
        other => {
            reject_ref_outside_github(reference.as_deref())?;
            origin_of(other)
        }
    }
}

/// `--ref` 只配 GitHub 仓库链接：`--url` / `--from` 没有「版本」这回事
fn reject_ref_outside_github(reference: Option<&str>) -> Result<()> {
    if reference.is_some() {
        return Err(AppDiagnostic::usage("--ref 只能与 GitHub 仓库链接一起使用", None).into());
    }
    Ok(())
}

/// 把领域来源转成端口来源；本地来源补成绝对路径
fn origin_of(source: PluginSource) -> Result<ArtifactOrigin> {
    Ok(match source {
        PluginSource::Local(path) => ArtifactOrigin::Local(absolute_local(PathBuf::from(path))?),
        PluginSource::Url(url) => ArtifactOrigin::Url(url),
        PluginSource::Github { owner, repo, rev } => ArtifactOrigin::Github { owner, repo, rev },
    })
}

/// 面向人的来源描述（确认提示用）
fn describe_origin(origin: &ArtifactOrigin) -> String {
    match origin {
        ArtifactOrigin::Local(path) => path.display().to_string(),
        ArtifactOrigin::Url(url) => url.clone(),
        ArtifactOrigin::Github { owner, repo, rev } => match rev {
            Some(rev) => format!("https://github.com/{owner}/{repo} ({rev})"),
            None => format!("https://github.com/{owner}/{repo}"),
        },
    }
}

/// 本地来源一律规范成绝对路径
///
/// 安装记录里存的是 `local:<path>`。相对路径会在**执行 `plugin update` 时的
/// 工作目录**下解析——同一个插件在别的目录里 `update` 会指向完全不同的位置，
/// 甚至恰好指向另一个插件。规范成绝对路径后记录与工作目录无关。
fn absolute_local(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    let cwd = std::env::current_dir()
        .map_err(|err| AppDiagnostic::usage(format!("无法确定当前工作目录: {err}"), None))?;
    Ok(cwd.join(path))
}

fn update(
    id: &str,
    to: Option<String>,
    prune: bool,
    yes: bool,
    setup: bool,
    printer: Printer,
) -> Result<()> {
    let id = parse_id(id, printer.lang())?;
    let (service, bound) = open_bound()?;
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
            &bound,
            &service,
            &updated_id,
            &remaining,
            printer,
            "升级完成，但重启失败",
            "updated, but restart failed",
        )
    } else {
        // 升级可能把「原本在跑」的进程恢复起来；绑定服务器停着就收掉
        bound.enforce_gate(&updated_id, printer);
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

/// 已安装插件重跑安装向导
///
/// 与 `install` 的区别：产物已就位，只确认待授权能力并按 `setup` 声明逐项
/// 询问配置。启用状态沿用原样（未启用的不会被顺手拉起），只在原本运行中
/// 时重启一次，让新配置生效。装载了 `choicesFrom` 的字段同样走服务器枚举。
fn setup(id: &str, printer: Printer) -> Result<()> {
    let id = parse_id(id, printer.lang())?;
    let (service, bound) = open_bound()?;
    let record = service.record(&id)?;
    let lang = printer.lang();
    let declares_setup = service
        .descriptor(&id)
        .is_ok_and(|descriptor| descriptor.setup.is_some());
    if !declares_setup {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("{id} 没有声明安装向导配置，无需运行 setup"),
                format!("{id} declares no setup fields, nothing to configure"),
            ),
            None,
        )
        .into());
    }
    // 只在原本运行中时重启：`setup` 是改配置，不是改变启用状态。
    let was_running = service
        .status(&id)
        .is_ok_and(|status| status.session.is_some());
    let pending = pending_of(&record);
    let (granted, remaining, report) =
        approve_and_configure(&service, &id, &pending, true, true, printer)?;
    // 配置改完后才判定：绑定服务器都停着时收掉残留进程并跳过重启，避免
    // 用户在向导里改配置的这段时间桥还在空转。
    let gate = bound.enforce_gate(&id, printer);
    if was_running && let StartGate::AllStopped { servers } = &gate {
        print_gate_note(
            printer,
            &id,
            servers,
            "已跳过重启",
            "the restart was skipped",
        );
    }
    let restarted = if was_running
        && !gate.is_blocked()
        && remaining.is_empty()
        && report.missing.is_empty()
    {
        let outcome = plugin_host::ensure(printer).and_then(|()| {
            service
                .restart(&id, Duration::from_secs(10))
                .map_err(anyhow::Error::from)
        });
        match outcome {
            Ok(_) => true,
            Err(err) => {
                printer.print_section(&crate::i18n::pick(
                    lang,
                    format!(
                        "配置已保存，但重启失败：{err:#}\n运行 `mcsup plugin logs {id}` 查看原因"
                    ),
                    format!(
                        "configuration saved, but restart failed: {err:#}\nrun `mcsup plugin logs {id}` for details"
                    ),
                ));
                false
            }
        }
    } else {
        false
    };
    print_missing(printer, &id, &report.missing);

    printer.print_outcome(
        &crate::i18n::pick(
            lang,
            format!(
                "已更新 {id} 的配置{}",
                if restarted { "（已重启）" } else { "" }
            ),
            format!(
                "updated the configuration of {id}{}",
                if restarted { " (restarted)" } else { "" }
            ),
        ),
        &json!({
            "id": id.to_string(),
            "granted": granted.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "pending": remaining.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "configured": report.configured,
            "missing": report.missing,
            "restarted": restarted,
        }),
    )
}

fn origin_from_source(source: Option<&str>) -> Option<ArtifactOrigin> {
    let parsed = PluginSource::parse_record(source?).ok()?;
    Some(match parsed {
        PluginSource::Local(path) => ArtifactOrigin::Local(PathBuf::from(path)),
        PluginSource::Url(url) => ArtifactOrigin::Url(url),
        PluginSource::Github { owner, repo, rev } => ArtifactOrigin::Github { owner, repo, rev },
    })
}

/// 按记录里的安装来源重新拉取并升级
fn upgrade_from_source(
    service: &PluginService,
    id: &PluginId,
    prune: bool,
) -> Result<(PluginId, String)> {
    let record = service.record(id)?;
    let Some(origin) = origin_from_source(record.source.as_deref()) else {
        // 记录里存的来源读不出来时，把原因带出去：旧版 `git:` 来源需要
        // 用户换 GitHub 链接重装，只说「没有来源」会让他无从下手。
        let detail = match record.source.as_deref().map(PluginSource::parse_record) {
            Some(Err(err)) => format!("：{err}"),
            _ => String::new(),
        };
        return Err(AppDiagnostic::usage(
            format!(
                "插件 {id} 没有可复用的安装来源{detail}；\
                 用 `mcsup plugin install <source>` 升级"
            ),
            None,
        )
        .into());
    };
    let outcome = service.install_as(id, &origin, prune)?;
    Ok((outcome.record.id, outcome.record.version))
}

/// 卸载：默认连配置与数据一起删
///
/// 「卸载」在这台机器上意味着「不再有这个名字」，所以产物、记录、日志与
/// `plugin-data/<id>/`（配置、密钥、键值）一起消失；要重装后接着用同一份配置
/// 与键值走 `--keep-data`。删除不可恢复，因此先问一次（默认不删）；无法弹出
/// 确认框时（管道 / JSON）必须显式 `--yes`，不能因为「没人回答」就当同意。
fn remove(id: &str, keep_data: bool, keep_secrets: bool, printer: Printer) -> Result<()> {
    let id = parse_id(id, printer.lang())?;
    let service = open()?;
    let record = service.record(&id)?;
    // 密钥就在数据目录里：要求保留密钥却删掉整个目录是矛盾指令，按用户意图
    // 当作「保留数据」处理，方向安全（多留不删）。
    let data = if keep_data || keep_secrets {
        RemoveData::Keep {
            secrets: keep_secrets,
        }
    } else {
        RemoveData::Delete
    };
    if !confirm_remove(&record, printer)? {
        return Ok(());
    }
    // 启用中的插件要走「禁用并移除」：`disable` 顺带停掉进程，否则 service 会
    // 以「还在跑」拒绝。已停用但残留会话（升级时恢复起来的）也要收掉。
    let was_enabled = record.enabled;
    let had_session = service
        .status(&id)
        .is_ok_and(|status| status.session.is_some());
    if was_enabled || had_session {
        service.disable(&id)?;
    }
    service.remove_with(&id, data)?;
    let data_kept = !data.deletes_data();
    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!(
                "{}{}{}",
                if was_enabled || had_session {
                    "已停用并卸载 "
                } else {
                    "已卸载 "
                },
                id,
                if data_kept {
                    if data.keeps_secrets() {
                        "（保留配置、数据与密钥）"
                    } else {
                        "（保留配置与数据）"
                    }
                } else {
                    "（含配置与数据）"
                }
            ),
            format!(
                "{} {id}{}",
                if was_enabled || had_session {
                    "disabled and removed"
                } else {
                    "removed"
                },
                if data_kept {
                    if data.keeps_secrets() {
                        " (configuration, data and secrets kept)"
                    } else {
                        " (configuration and data kept)"
                    }
                } else {
                    " (with configuration and data)"
                }
            ),
        ),
        &json!({
            "id": id.to_string(),
            "disabled": was_enabled || had_session,
            "data_kept": data_kept,
            "secrets_kept": data.keeps_secrets(),
        }),
    )
}

/// 卸载前的确认：启用中问「是否禁用并移除」，已停用问「是否移除」，默认不执行
///
/// 提示只问「做什么」，不括注「连同配置与数据」：那是 `--keep-data` 的差别，
/// 归帮助与结果输出说明，塞进确认框只会让一句话变长。返回 `false` 表示用户
/// 选择不删；无法交互又没有 `--yes` 时按用法错误拒绝。
fn confirm_remove(record: &PluginRecord, printer: Printer) -> Result<bool> {
    let id = &record.id;
    let lang = printer.lang();
    let message = if record.enabled {
        crate::i18n::pick(
            lang,
            format!("是否禁用并移除 {id}？"),
            format!("disable and remove {id}?"),
        )
    } else {
        crate::i18n::pick(lang, format!("是否移除 {id}？"), format!("remove {id}?"))
    };
    if printer.is_interactive() {
        let confirmed = crate::prompts::confirm_or_yes(printer, &message, false)?;
        if !confirmed {
            printer.print_section(&crate::i18n::pick(
                lang,
                format!("已取消移除 {id}"),
                format!("removal of {id} cancelled"),
            ));
        }
        return Ok(confirmed);
    }
    if printer.assumes_yes() {
        return Ok(true);
    }
    Err(AppDiagnostic::usage(
        crate::i18n::pick(
            lang,
            format!("非交互模式下卸载插件需要 --yes（会删除配置与数据）：`mcsup plugin remove {id} --yes`"),
            format!(
                "removing a plugin without a terminal requires --yes (it deletes configuration and data): `mcsup plugin remove {id} --yes`"
            ),
        ),
        Some(crate::i18n::pick(
            lang,
            "先停掉管道或用 `--yes` 明确同意",
            "stop piping, or pass `--yes` to agree explicitly",
        )),
    )
    .into())
}

fn set_enabled(id: &str, enable: bool, printer: Printer) -> Result<()> {
    let id = parse_id(id, printer.lang())?;
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
    let id = parse_id(id, printer.lang())?;
    let (service, bound) = open_bound()?;
    // 本机状态先判：拉黑 / 隔离 / 停用的插件根本不该启动，别让绑定门禁的
    // 「已跳过启动」把它盖成一次成功
    reject_unstartable(&service, &id, printer)?;
    // 绑定服务器都没在跑：不拉起（并收掉残留），启动服务器时会自动起
    if let StartGate::AllStopped { servers } = bound.enforce_gate(&id, printer) {
        print_gate_note(
            printer,
            &id,
            &servers,
            "已跳过启动",
            "the start was skipped",
        );
        if !printer.is_human() {
            printer.print_outcome(
                "",
                &json!({
                    "id": id.to_string(),
                    "started": false,
                    "bound_servers": servers,
                }),
            )?;
        }
        return Ok(());
    }
    plugin_host::ensure(printer)?;
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
    let id = parse_id(id, printer.lang())?;
    let service = open()?;
    // 上限用领域常量（与 `server stop` 同一份）：静默夹紧会让用户以为
    // 「`--grace 600`」真的生效了，实际只等了别的值。
    let max = mcsup::domain::limits::MAX_STOP_GRACE.as_secs();
    if grace > max {
        return Err(AppDiagnostic::usage(
            crate::i18n::pick(
                printer.lang(),
                format!("--grace 最大 {max}s，收到 {grace}"),
                format!("--grace must be at most {max}s, got {grace}"),
            ),
            Some(crate::i18n::pick(
                printer.lang(),
                "默认 10s",
                "the default is 10s",
            )),
        )
        .into());
    }
    service.stop(&id, Duration::from_secs(grace))?;
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
    let id = parse_id(id, printer.lang())?;
    let (service, bound) = open_bound()?;
    // 同 `start`：本机状态先判，避免被「已跳过重启」盖成成功
    reject_unstartable(&service, &id, printer)?;
    // 绑定服务器都没在跑：不重启（并收掉残留），等服务器启动时再拉起
    if let StartGate::AllStopped { servers } = bound.enforce_gate(&id, printer) {
        print_gate_note(
            printer,
            &id,
            &servers,
            "已跳过重启",
            "the restart was skipped",
        );
        if !printer.is_human() {
            printer.print_outcome(
                "",
                &json!({
                    "id": id.to_string(),
                    "restarted": false,
                    "bound_servers": servers,
                }),
            )?;
        }
        return Ok(());
    }
    plugin_host::ensure(printer)?;
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
    let id = parse_id(id, printer.lang())?;
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
    let id = parse_id(id, printer.lang())?;
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
    let id = parse_id(id, printer.lang())?;
    let input = read_input(input)?;
    plugin_host::ensure(printer)?;
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
            let id = parse_id(id, printer.lang())?;
            let service = open()?;
            let config = service.config_get(&id)?;
            let revision = config.get("revision").and_then(Value::as_u64).unwrap_or(0);
            let values = config.get("values").cloned().unwrap_or_else(|| json!({}));
            if let Some(key) = key {
                let value = value_at(&values, key).cloned().ok_or_else(|| {
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
            file,
            secret,
        } => {
            let id = parse_id(id, printer.lang())?;
            let value = config_value(value.as_deref(), file.as_deref(), *secret)?;
            let service = open()?;
            // 点号路径走深度合并，只改目标叶子；单段键沿用顶层写入。
            // 否则 `config set <id> qq.appId 1` 会被当成字面键 `qq.appId`，
            // schema 直接报 unknown field。
            let config = if key.contains('.') {
                let mut patch = Map::new();
                set_path(&mut patch, key, value);
                match service.config_merge(&id, &Value::Object(patch)) {
                    Ok(config) => config,
                    Err(err) => return Err(hint_required_siblings(err, &id, key, printer)),
                }
            } else {
                service.config_set(&id, &json!({ key.clone(): value }))?
            };
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

/// 整份配置校验拒绝「单写一个必填字段」时，给出可操作的提示
///
/// `qq.appId` 与 `qq.clientSecret` 这类同组必填字段，单独写一个必然缺另一个，
/// 校验会整份拒绝。真正要做的是把这一组一次性写入。
fn hint_required_siblings(
    err: mcsup::service::PluginError,
    id: &PluginId,
    key: &str,
    printer: Printer,
) -> anyhow::Error {
    let text = err.to_string();
    if text.contains("required field is missing") {
        let group = key.split('.').next().unwrap_or(key);
        printer.print_section(&crate::i18n::pick(
            printer.lang(),
            format!(
                "提示：{group} 下还有必填字段没填，单独写 {key} 会被整份校验拒绝；\
                 把这一组一次写完即可，例如 `mcsup plugin config set {id} {group} '{{\"…\":\"…\"}}'`"
            ),
            format!(
                "hint: `{group}` still has required fields missing, so writing {key} alone is \
                 rejected; write the whole group in one call, e.g. \
                 `mcsup plugin config set {id} {group} '{{\"…\":\"…\"}}'`"
            ),
        ));
    }
    err.into()
}

/// 解析 `config set` 的值
///
/// `--file` 时按严格 JSON 解析，报错直接抛给用户（文件里写错比静默存成字符串好）；
/// `--secret` 时一律按字符串处理（从 stdin 读时只去掉行尾换行），避免把密钥
/// 当作 JSON 解析失败后打出一段无意义提示。普通值先按 JSON 解析，失败才按
/// 字符串，保证整数 / 布尔 / 数组可以直接写。
fn config_value(raw: Option<&str>, file: Option<&Path>, secret: bool) -> Result<Value> {
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
    if let Some(path) = file {
        let text = std::fs::read_to_string(path).map_err(|err| {
            AppDiagnostic::usage(format!("读取 {} 失败：{err}", path.display()), None)
        })?;
        // Windows 记事本 / PowerShell 5.1 的 UTF-8 会带 BOM，serde_json 认不出来
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let value = serde_json::from_str(text).map_err(|err| {
            AppDiagnostic::usage(format!("{} 不是合法的 JSON：{err}", path.display()), None)
        })?;
        return Ok(value);
    }
    let Some(raw) = raw else {
        return Err(AppDiagnostic::usage(
            "缺少配置值；或用 --secret 从 stdin 读取，或用 --file 从文件读取 JSON",
            None,
        )
        .into());
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
    let id = parse_id(raw, printer.lang())?;
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
    let id = parse_id(id, printer.lang())?;
    let capability = parse_capability(capability, printer.lang())?;
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
    let id = parse_id(id, printer.lang())?;
    let capability = parse_capability(capability, printer.lang())?;
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
    let id = parse_id(id, printer.lang())?;
    plugin_host::ensure(printer)?;
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
    // 压缩包 / 目录：重跑来源校验，摘要取自旁路 `.sha256`。插件 id：按安装
    // 记录找到产物目录，摘要必须取自**记录**——旁路文件里是压缩包摘要，与
    // 解包目录树摘要不是一回事，混用会拿错误的期望值去比对，或者干脆不比对
    // 就报「校验通过」，放行被改过的产物。
    let (target_path, id, recheck) = if let Ok(id) = PluginId::parse(target) {
        let record = service.record(&id)?;
        let expected = record
            .sha256
            .as_deref()
            .map(|hex| Checksum::new(ChecksumAlgorithm::Sha256, hex));
        let verifier = mcsup::provider::plugin::PluginArtifactVerifier::new();
        let artifact = verifier
            .verify_installed(&record.artifact_dir, expected.as_ref())
            .map_err(anyhow::Error::from)?;
        return report_verified(&artifact, Some(record.id), printer);
    } else {
        let path = PathBuf::from(target);
        if !path.exists() {
            return Err(
                AppDiagnostic::not_found(format!("找不到插件 id 或目录: {target}"), None).into(),
            );
        }
        (path.clone(), None, path.is_file())
    };
    let expected = sha256_hint(target);
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
    report_verified(&artifact, id, printer)
}

fn report_verified(
    artifact: &mcsup::port::plugin::PluginArtifact,
    id: Option<PluginId>,
    printer: Printer,
) -> Result<()> {
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
    let id = parse_id(id, printer.lang())?;
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
    let host = plugin_host::available();
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
    use mcsup::domain::plugin::descriptor::ResolvedSetupWhen;
    use serde_json::json;

    /// 多选框不能吞掉配置里已有、但插件没声明的值
    #[test]
    fn multiselect_keeps_values_outside_choices() {
        let choices = vec!["qq".to_owned(), "log".to_owned()];
        let current = vec!["log".to_owned(), "custom".to_owned()];
        assert_eq!(
            choices_with_current(&choices, &current),
            vec!["qq".to_owned(), "log".to_owned(), "custom".to_owned()]
        );
        // 已在 choices 里的当前值不重复追加
        assert_eq!(choices_with_current(&choices, &choices), choices);
    }

    /// `choicesFrom` 没有可选项时退回自由输入，不能把安装卡死
    #[test]
    fn dynamic_options_fall_back_when_host_has_none() {
        let field = ResolvedSetupField {
            path: "qq.server".to_owned(),
            label: "入站服务器".to_owned(),
            description: None,
            required: true,
            secret: false,
            interactive: false,
            default: None,
            choices_from: Some(ChoicesSource::Servers),
            when: None,
            kind: SetupKind::Text {
                choices: Vec::new(),
            },
            schema: json!({ "type": "string" }),
        };
        let servers = vec!["survival".to_owned()];
        assert_eq!(setup_options(&field, &servers), &servers[..]);
        assert!(setup_options(&field, &[]).is_empty());
    }

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

    /// 重装已有配置的插件时仍必须询问 `AppID` / `ClientSecret` 等必填项，
    /// 但条件不成立的字段（没选 qq 时的 QQ 凭据）不问也不记缺失
    #[test]
    fn fresh_install_asks_every_field_even_when_configured() {
        let field = |path: &str, when: Option<ResolvedSetupWhen>| ResolvedSetupField {
            path: path.to_owned(),
            label: path.to_owned(),
            description: None,
            required: true,
            secret: false,
            interactive: false,
            default: None,
            choices_from: None,
            when,
            kind: SetupKind::Text {
                choices: Vec::new(),
            },
            schema: json!({ "type": "string" }),
        };
        let setup = ResolvedSetup {
            start: true,
            fields: vec![field("qq.appId", None), field("qq.groups", None)],
        };
        let values = json!({ "qq": { "appId": "a", "groups": [] } });

        let asked = |fields: &[ResolvedSetupField], values: &Value, fresh: bool| {
            let answered_now = BTreeSet::new();
            fields
                .iter()
                .filter(|field| should_ask(field, values, fresh, &answered_now))
                .map(|field| field.path.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(asked(&setup.fields, &values, true).len(), 2);
        assert_eq!(
            asked(&setup.fields, &values, false),
            vec!["qq.groups".to_owned()]
        );

        // 交互式 hook 刚填过的路径，后续不再重复提问
        let answered_now = BTreeSet::from(["qq.appId".to_owned()]);
        assert_eq!(
            setup
                .fields
                .iter()
                .filter(|field| should_ask(field, &values, true, &answered_now))
                .map(|field| field.path.clone())
                .collect::<Vec<_>>(),
            vec!["qq.groups".to_owned()]
        );

        // 没选 qq：QQ 凭据既不该问，也不该算「缺配置」
        let gated = vec![
            field("platforms", None),
            field(
                "qq.appId",
                Some(ResolvedSetupWhen {
                    path: "platforms".to_owned(),
                    includes: Some("qq".to_owned()),
                    equals: None,
                }),
            ),
        ];
        let telegram_only = json!({ "platforms": ["telegram"] });
        assert_eq!(
            asked(&gated, &telegram_only, true),
            vec!["platforms".to_owned()]
        );
        let with_qq = json!({ "platforms": ["qq", "telegram"] });
        assert_eq!(asked(&gated, &with_qq, true).len(), 2);
    }

    /// 多值回传：普通字符串保持旧协议，JSON 对象一次填多个字段
    #[test]
    fn setup_payload_accepts_single_and_multi_values() {
        assert_eq!(
            parse_setup_payload("  token-1 \n", "weixin.botToken").expect("single"),
            BTreeMap::from([("weixin.botToken".to_owned(), "token-1".to_owned())])
        );
        assert_eq!(
            parse_setup_payload(
                r#"{"weixin.botToken":" t ","weixin.apiBase":"https://api.example.com"}"#,
                "weixin.botToken",
            )
            .expect("multi"),
            BTreeMap::from([
                ("weixin.botToken".to_owned(), "t".to_owned()),
                (
                    "weixin.apiBase".to_owned(),
                    "https://api.example.com".to_owned(),
                ),
            ])
        );
        // 没有当前字段、空值、非字符串都不能当成一次成功的登录
        assert!(parse_setup_payload(r#"{"weixin.apiBase":"x"}"#, "weixin.botToken").is_err());
        assert!(parse_setup_payload(r#"{"weixin.botToken":""}"#, "weixin.botToken").is_err());
        assert!(parse_setup_payload(r#"{"weixin.botToken":1}"#, "weixin.botToken").is_err());
        assert!(parse_setup_payload("   ", "weixin.botToken").is_err());
    }

    /// 回填值必须命中声明过的字符串字段，并按各自 schema 复校验
    #[test]
    fn interactive_values_must_match_declared_fields() {
        let field = |path: &str, schema: Value| ResolvedSetupField {
            path: path.to_owned(),
            label: path.to_owned(),
            description: None,
            required: false,
            secret: false,
            interactive: false,
            default: None,
            choices_from: None,
            when: None,
            kind: SetupKind::Text {
                choices: Vec::new(),
            },
            schema,
        };
        let fields = vec![
            field("weixin.botToken", json!({ "type": "string" })),
            field(
                "weixin.apiBase",
                json!({ "type": "string", "maxLength": 20 }),
            ),
        ];
        let trigger = &fields[0];
        validate_interactive_values(
            &fields,
            trigger,
            &BTreeMap::from([
                ("weixin.botToken".to_owned(), "tok".to_owned()),
                ("weixin.apiBase".to_owned(), "https://a".to_owned()),
            ]),
        )
        .expect("declared fields pass");
        assert!(
            validate_interactive_values(
                &fields,
                trigger,
                &BTreeMap::from([("unknown".to_owned(), "x".to_owned())]),
            )
            .is_err()
        );
        assert!(
            validate_interactive_values(
                &fields,
                trigger,
                &BTreeMap::from([
                    ("weixin.botToken".to_owned(), "tok".to_owned()),
                    (
                        "weixin.apiBase".to_owned(),
                        "https://api.example.com/long/base".to_owned()
                    ),
                ]),
            )
            .is_err()
        );
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

    /// 旧版 `git:` 记录：GitHub 裸仓库映射成 GitHub 来源，其它一律读不出来
    ///
    /// `|` 与 `@` 两种旧分隔都要认（provider 曾写 `@`，service 曾写 `|`）。
    #[test]
    fn legacy_git_records_become_github_sources() {
        assert!(matches!(
            origin_from_source(Some("git:git@github.com:org/repo|v1.0.0")),
            Some(ArtifactOrigin::Github { owner, repo, rev: Some(rev) })
                if owner == "org" && repo == "repo" && rev == "v1.0.0"
        ));
        assert!(matches!(
            origin_from_source(Some("git:https://github.com/org/repo")),
            Some(ArtifactOrigin::Github { rev: None, .. })
        ));
        // 非 GitHub 的旧 git 来源已不再支持：读不出来，由 update 报错提示重装
        assert!(origin_from_source(Some("git:https://user@host/org/repo")).is_none());
    }

    /// 安装记录里存引用而不是 commit：`update` 才跟得动 `dist` 分支
    #[test]
    fn recorded_sources_round_trip_their_ref() {
        for text in [
            "local:/opt/plugins/demo",
            "url:https://example.com/plugin.zip",
            "github:org/repo",
            "github:org/repo@dist",
        ] {
            let origin = origin_from_source(Some(text)).expect(text);
            assert_eq!(origin.record_text(), text);
        }
    }

    /// `--ref` 只配 GitHub 仓库链接：直链与本地路径都没有「版本」这回事
    #[test]
    fn ref_is_only_accepted_with_a_github_link() {
        let err = resolve_origin(
            Some("https://example.com/plugin.zip".to_owned()),
            None,
            None,
            Some("v1".to_owned()),
        )
        .expect_err("url with ref");
        assert!(err.to_string().contains("--ref"), "{err}");

        let origin = resolve_origin(
            Some("https://github.com/org/repo".to_owned()),
            None,
            None,
            Some("v1".to_owned()),
        )
        .expect("github with ref");
        assert_eq!(
            origin,
            ArtifactOrigin::Github {
                owner: "org".to_owned(),
                repo: "repo".to_owned(),
                rev: Some("v1".to_owned()),
            }
        );
    }

    /// 链接里带 `/tree/<ref>` 与显式 `--ref` 是矛盾指令
    #[test]
    fn tree_link_conflicts_with_explicit_ref() {
        let err = resolve_origin(
            Some("https://github.com/org/repo/tree/dist".to_owned()),
            None,
            None,
            Some("v1".to_owned()),
        )
        .expect_err("both refs");
        assert!(err.to_string().contains("--ref"), "{err}");
    }

    /// 本地来源规范成绝对路径：`plugin update` 不该依赖当前工作目录
    #[test]
    fn relative_local_sources_become_absolute() {
        let relative = absolute_local(PathBuf::from("plugins").join("demo")).expect("absolute");
        assert!(relative.is_absolute(), "{relative:?}");
        assert!(relative.ends_with(Path::new("plugins").join("demo")));

        let base = std::env::temp_dir();
        assert_eq!(absolute_local(base.clone()).expect("absolute"), base);
    }
}
