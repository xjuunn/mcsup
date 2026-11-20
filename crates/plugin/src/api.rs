//! 插件 → 宿主调用的分发
//!
//! 协议只给 `capability` + `method` 字符串；本模块把它们映射到
//! [`port::plugin`] 的窄 trait。判定顺序固定：
//!
//! 1. 能力名是否在闭集内（未知即 `invalid_input`）；
//! 2. 本次会话是否已授予（未授予即 `denied`，不降级成功）；
//! 3. `server` 作用域是否越界（越界即 `denied`）；
//! 4. 参数形状与限额（不符合即 `invalid_input` / `limit_exceeded`）。
//!
//! 入口与插件都不重复这套顺序。

use crate::base64;
use domain::java::{JavaMajor, JavaRuntime};
use domain::plugin::PluginId;
use domain::plugin::capability::Capability;
use domain::server::content::{ContentIdentity, ContentKind};
use domain::server::meta::ServerName;
use port::StopOutcome;
use port::plugin::{
    BackupOps, ConsoleIo, ContentInstallRequest, ContentOps, ContractInvoker, FileOps, HostError,
    JavaOps, KvStore, NetProxy, NetRequest, SecretStore, SecretValue, ServerLifecycle, ServerRead,
    ServerStatusView,
};
use protocol::plugin::{Call, CallRoute};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// 宿主能力集合
///
/// 每项都可以缺省：门面按编译 feature 与阶段装配。缺省时调用返回
/// `unsupported`，而不是在分发处 panic 或静默成功。
#[derive(Clone, Default)]
pub struct HostServices {
    /// `server:read`
    pub server_read: Option<Arc<dyn ServerRead>>,
    /// `console:read` / `console:write`
    pub console: Option<Arc<dyn ConsoleIo>>,
    /// `server:lifecycle`
    pub lifecycle: Option<Arc<dyn ServerLifecycle>>,
    /// `backup`
    pub backup: Option<Arc<dyn BackupOps>>,
    /// `content`
    pub content: Option<Arc<dyn ContentOps>>,
    /// `java`
    pub java: Option<Arc<dyn JavaOps>>,
    /// `files:read` / `files:write`
    pub files: Option<Arc<dyn FileOps>>,
    /// `storage:kv`
    pub kv: Option<Arc<dyn KvStore>>,
    /// `secret:rw`
    pub secrets: Option<Arc<dyn SecretStore>>,
    /// `net:outbound`
    pub net: Option<Arc<dyn NetProxy>>,
    /// 插件间契约路由；由宿主构造时注入
    pub contracts: Option<Arc<dyn ContractInvoker>>,
}

impl std::fmt::Debug for HostServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut set = f.debug_set();
        for (name, present) in [
            ("server:read", self.server_read.is_some()),
            ("console", self.console.is_some()),
            ("server:lifecycle", self.lifecycle.is_some()),
            ("backup", self.backup.is_some()),
            ("content", self.content.is_some()),
            ("java", self.java.is_some()),
            ("files", self.files.is_some()),
            ("storage:kv", self.kv.is_some()),
            ("secret:rw", self.secrets.is_some()),
            ("net:outbound", self.net.is_some()),
            ("contracts", self.contracts.is_some()),
        ] {
            if present {
                set.entry(&name);
            }
        }
        set.finish()
    }
}

/// 一次分发的上下文
#[derive(Clone, Copy)]
pub struct DispatchContext<'a> {
    /// 本次会话的插件身份；键值与密钥命名空间由宿主决定，不采信输入
    pub plugin: &'a PluginId,
    /// 本次会话实际授予的能力
    pub granted: &'a BTreeSet<Capability>,
    /// `server` 作用域绑定的服务器；`machine` 作用域为 `None`
    pub scope_server: Option<&'a ServerName>,
    /// 契约路由；宿主尚未装配时为 `None`，契约调用返回 `unsupported`
    pub contracts: Option<&'a dyn ContractInvoker>,
}

impl std::fmt::Debug for DispatchContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchContext")
            .field("plugin", &self.plugin)
            .field("granted", &self.granted)
            .field("scope_server", &self.scope_server)
            .field("contracts", &self.contracts.is_some())
            .finish()
    }
}

/// 分发一次插件 → 宿主调用
///
/// # Errors
///
/// 未知能力 / 参数、未授予、越界、限额或实现未装配时返回 [`HostError`]
pub fn dispatch(
    services: &HostServices,
    call: &Call,
    ctx: DispatchContext<'_>,
) -> Result<Value, HostError> {
    match call
        .route()
        .map_err(|err| HostError::invalid_input(err.to_string()))?
    {
        CallRoute::Contract {
            contract,
            target,
            trace,
        } => {
            let invoker = ctx.contracts.ok_or_else(|| {
                HostError::unsupported("contract routing is not available in this host build")
            })?;
            let target = target
                .map(PluginId::parse)
                .transpose()
                .map_err(|err| HostError::invalid_input(format!("invalid target: {err}")))?;
            invoker.invoke(port::plugin::ContractCall {
                caller: ctx.plugin.clone(),
                contract: contract.to_owned(),
                target,
                input: call.input.clone(),
                trace: trace.map(str::to_owned),
            })
        }
        CallRoute::Host { .. } => host_call(services, call, ctx),
        _ => Err(HostError::invalid_input(
            "host call must use capability + method, or contract",
        )),
    }
}

fn host_call(
    services: &HostServices,
    call: &Call,
    ctx: DispatchContext<'_>,
) -> Result<Value, HostError> {
    let capability_name = call
        .capability
        .as_deref()
        .ok_or_else(|| HostError::invalid_input("host call requires capability"))?;
    let capability = Capability::parse(capability_name)
        .map_err(|_| HostError::invalid_input(format!("unknown capability: {capability_name}")))?;
    if !ctx.granted.contains(&capability) {
        return Err(HostError::denied(format!(
            "{capability} is not granted (granted: {})",
            granted_text(ctx.granted)
        )));
    }
    let method = call
        .method
        .as_deref()
        .ok_or_else(|| HostError::invalid_input("host call requires method"))?;
    let input = &call.input;
    let bound = ctx.scope_server;
    match capability {
        Capability::ServerRead => server_read(services, method, input, bound),
        Capability::ConsoleRead => console_read(services, method, input, bound),
        Capability::ConsoleWrite => console_write(services, method, input, bound),
        Capability::StorageKv => kv(services, method, input, ctx.plugin),
        Capability::SecretRw => secrets(services, method, input, ctx.plugin),
        Capability::NetOutbound => net(services, method, input, ctx.plugin),
        Capability::FilesRead => files_read(services, method, input, bound),
        Capability::FilesWrite => files_write(services, method, input, bound),
        Capability::ServerLifecycle => lifecycle(services, method, input, bound),
        Capability::Backup => backup(services, method, input, bound),
        Capability::Content => content(services, method, input, bound),
        Capability::Java => java(services, method, input),
        Capability::Events | Capability::ProcessSpawn | Capability::McClient => {
            Err(HostError::unsupported(format!(
                "{capability} host methods are not available in this host build"
            )))
        }
        // 非穷尽枚举：宿主版本比能力目录新时，未知能力拒绝而不是放行。
        _ => Err(HostError::unsupported(format!(
            "{capability} is not supported by this host"
        ))),
    }
}

fn granted_text(granted: &BTreeSet<Capability>) -> String {
    if granted.is_empty() {
        "none".to_owned()
    } else {
        granted
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn server_read(
    services: &HostServices,
    method: &str,
    input: &Value,
    bound: Option<&ServerName>,
) -> Result<Value, HostError> {
    let service = services
        .server_read
        .as_ref()
        .ok_or_else(|| HostError::unsupported("server:read is not available"))?;
    match method {
        "list" => {
            let items: Vec<Value> = service
                .list()?
                .into_iter()
                .filter(|summary| bound.is_none_or(|bound| &summary.name == bound))
                .map(|summary| {
                    json!({
                        "name": summary.name.as_str(),
                        "status": summary.status.as_str(),
                        "version": summary.version.as_ref().map(ToString::to_string),
                    })
                })
                .collect();
            Ok(json!({ "servers": items }))
        }
        "status" => {
            let server = server_arg(input, bound)?;
            let view = service.status(&server)?;
            Ok(status_json(&view))
        }
        "version" => {
            let server = server_arg(input, bound)?;
            Ok(json!({ "version": service.version(&server)?.map(|v| v.to_string()) }))
        }
        "properties" => {
            let server = server_arg(input, bound)?;
            Ok(json!({ "properties": service.properties(&server)? }))
        }
        other => Err(unknown_method("server:read", other)),
    }
}

fn status_json(view: &ServerStatusView) -> Value {
    json!({
        "status": view.status.as_str(),
        "version": view.version.as_ref().map(ToString::to_string),
        "players": view.players,
        "uptime_secs": view.uptime.map(|d| d.as_secs()),
    })
}

fn console_read(
    services: &HostServices,
    method: &str,
    input: &Value,
    bound: Option<&ServerName>,
) -> Result<Value, HostError> {
    let service = services
        .console
        .as_ref()
        .ok_or_else(|| HostError::unsupported("console:read is not available"))?;
    match method {
        "tail" => {
            let server = server_arg(input, bound)?;
            let cursor = input.get("cursor").and_then(Value::as_u64);
            let max_lines = input
                .get("max_lines")
                .and_then(Value::as_u64)
                .map_or(200, |n| usize::try_from(n).unwrap_or(usize::MAX));
            let page = service.tail(&server, cursor, max_lines)?;
            let lines: Vec<Value> = page
                .lines
                .into_iter()
                .map(|line| json!({ "cursor": line.cursor, "text": line.text }))
                .collect();
            Ok(json!({ "lines": lines, "next": page.next }))
        }
        other => Err(unknown_method("console:read", other)),
    }
}

fn console_write(
    services: &HostServices,
    method: &str,
    input: &Value,
    bound: Option<&ServerName>,
) -> Result<Value, HostError> {
    let service = services
        .console
        .as_ref()
        .ok_or_else(|| HostError::unsupported("console:write is not available"))?;
    match method {
        "send_line" => {
            let server = server_arg(input, bound)?;
            let line = string_arg(input, "line")?;
            service.send_line(&server, line)?;
            Ok(json!({ "ok": true }))
        }
        other => Err(unknown_method("console:write", other)),
    }
}

fn kv(
    services: &HostServices,
    method: &str,
    input: &Value,
    plugin: &PluginId,
) -> Result<Value, HostError> {
    let service = services
        .kv
        .as_ref()
        .ok_or_else(|| HostError::unsupported("storage:kv is not available"))?;
    match method {
        "get" => {
            let key = string_arg(input, "key")?;
            Ok(json!({
                "value": service.get(plugin, key)?.map(|bytes| base64::encode(&bytes))
            }))
        }
        "set" => {
            let key = string_arg(input, "key")?;
            let value = base64_arg(input, "value")?;
            service.set(plugin, key, &value)?;
            Ok(json!({ "ok": true }))
        }
        "delete" => {
            let key = string_arg(input, "key")?;
            Ok(json!({ "deleted": service.delete(plugin, key)? }))
        }
        "list" => {
            let prefix = input.get("prefix").and_then(Value::as_str).unwrap_or("");
            Ok(json!({ "keys": service.list(plugin, prefix)? }))
        }
        other => Err(unknown_method("storage:kv", other)),
    }
}

fn secrets(
    services: &HostServices,
    method: &str,
    input: &Value,
    plugin: &PluginId,
) -> Result<Value, HostError> {
    let service = services
        .secrets
        .as_ref()
        .ok_or_else(|| HostError::unsupported("secret:rw is not available"))?;
    match method {
        "get" => {
            let key = string_arg(input, "key")?;
            Ok(json!({
                "value": service
                    .get(plugin, key)?
                    .map(|secret| base64::encode(secret.expose()))
            }))
        }
        "set" => {
            let key = string_arg(input, "key")?;
            let value = SecretValue::from_bytes(base64_arg(input, "value")?);
            service.set(plugin, key, &value)?;
            Ok(json!({ "ok": true }))
        }
        "delete" => {
            let key = string_arg(input, "key")?;
            Ok(json!({ "deleted": service.delete(plugin, key)? }))
        }
        other => Err(unknown_method("secret:rw", other)),
    }
}

fn net(
    services: &HostServices,
    method: &str,
    input: &Value,
    plugin: &PluginId,
) -> Result<Value, HostError> {
    let service = services
        .net
        .as_ref()
        .ok_or_else(|| HostError::unsupported("net:outbound is not available"))?;
    match method {
        "request" => {
            let url = string_arg(input, "url")?;
            let http_method = input
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_ascii_uppercase();
            let mut headers = BTreeMap::new();
            if let Some(map) = input.get("headers") {
                let map = map
                    .as_object()
                    .ok_or_else(|| HostError::invalid_input("headers must be an object"))?;
                for (name, value) in map {
                    let value = value.as_str().ok_or_else(|| {
                        HostError::invalid_input(format!("header {name:?} must be a string"))
                    })?;
                    headers.insert(name.clone(), value.to_owned());
                }
            }
            let body = match input.get("body") {
                Some(Value::Null) | None => None,
                Some(_) => Some(base64_arg(input, "body")?),
            };
            let timeout_ms = input.get("timeout_ms").and_then(Value::as_u64);
            let timeout = timeout_ms.map_or(Duration::ZERO, Duration::from_millis);
            let response = service.request(
                plugin,
                NetRequest {
                    method: http_method,
                    url: url.to_owned(),
                    headers,
                    body,
                    timeout,
                },
            )?;
            Ok(json!({
                "status": response.status,
                "headers": response.headers,
                "body": base64::encode(&response.body),
                "body_len": response.body.len(),
            }))
        }
        other => Err(unknown_method("net:outbound", other)),
    }
}

fn files_read(
    services: &HostServices,
    method: &str,
    input: &Value,
    bound: Option<&ServerName>,
) -> Result<Value, HostError> {
    let service = services
        .files
        .as_ref()
        .ok_or_else(|| HostError::unsupported("files:read is not available"))?;
    match method {
        "read" => {
            let server = server_arg(input, bound)?;
            let path = path_arg(input, "path")?;
            let bytes = service.read(&server, &path)?;
            Ok(json!({
                "path": path_text(&path),
                "size": bytes.len(),
                "data": base64::encode(&bytes),
            }))
        }
        "list" => {
            let server = server_arg(input, bound)?;
            let path = optional_path_arg(input)?;
            let entries = service
                .list(&server, &path)?
                .into_iter()
                .map(|entry| {
                    json!({
                        "path": path_text(&entry.path),
                        "is_dir": entry.is_dir,
                        "size": entry.size,
                    })
                })
                .collect::<Vec<_>>();
            Ok(json!({ "entries": entries }))
        }
        other => Err(unknown_method("files:read", other)),
    }
}

fn files_write(
    services: &HostServices,
    method: &str,
    input: &Value,
    bound: Option<&ServerName>,
) -> Result<Value, HostError> {
    let service = services
        .files
        .as_ref()
        .ok_or_else(|| HostError::unsupported("files:write is not available"))?;
    match method {
        "write" => {
            let server = server_arg(input, bound)?;
            let path = path_arg(input, "path")?;
            let bytes = base64_arg(input, "data")?;
            let overwrite = match input.get("overwrite") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(value)) => *value,
                Some(_) => {
                    return Err(HostError::invalid_input("overwrite must be a boolean"));
                }
            };
            service.write(&server, &path, &bytes, overwrite)?;
            Ok(json!({ "ok": true, "size": bytes.len() }))
        }
        "remove" => {
            let server = server_arg(input, bound)?;
            let path = path_arg(input, "path")?;
            Ok(json!({ "removed": service.remove(&server, &path)? }))
        }
        other => Err(unknown_method("files:write", other)),
    }
}

fn lifecycle(
    services: &HostServices,
    method: &str,
    input: &Value,
    bound: Option<&ServerName>,
) -> Result<Value, HostError> {
    let service = services
        .lifecycle
        .as_ref()
        .ok_or_else(|| HostError::unsupported("server:lifecycle is not available"))?;
    match method {
        "start" => {
            let server = server_arg(input, bound)?;
            service.start(&server)?;
            Ok(json!({ "ok": true }))
        }
        "stop" => {
            let server = server_arg(input, bound)?;
            let grace = Duration::from_secs(grace_secs_arg(input)?);
            let outcome = service.stop(&server, grace)?;
            Ok(match outcome {
                StopOutcome::Stopped { elapsed } => json!({
                    "outcome": "stopped",
                    "elapsed_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                }),
                StopOutcome::AlreadyStopped => json!({ "outcome": "already_stopped" }),
                StopOutcome::TimedOut { waited } => json!({
                    "outcome": "timed_out",
                    "waited_ms": u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                }),
            })
        }
        "restart" => {
            let server = server_arg(input, bound)?;
            let grace = Duration::from_secs(grace_secs_arg(input)?);
            service.restart(&server, grace)?;
            Ok(json!({ "ok": true }))
        }
        other => Err(unknown_method("server:lifecycle", other)),
    }
}

fn grace_secs_arg(input: &Value) -> Result<u64, HostError> {
    match input.get("grace_secs") {
        None | Some(Value::Null) => Ok(30),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| HostError::invalid_input("grace_secs must be a non-negative integer")),
    }
}

fn backup(
    services: &HostServices,
    method: &str,
    input: &Value,
    bound: Option<&ServerName>,
) -> Result<Value, HostError> {
    let service = services
        .backup
        .as_ref()
        .ok_or_else(|| HostError::unsupported("backup is not available"))?;
    match method {
        "create" => {
            let server = server_arg(input, bound)?;
            let label = optional_string_arg(input, "label")?;
            let snapshot = service.create(&server, label.as_deref())?;
            Ok(json!({ "snapshot": snapshot }))
        }
        "list" => {
            let server = server_arg(input, bound)?;
            let snapshots = service
                .list(&server)?
                .into_iter()
                .map(|entry| {
                    json!({
                        "id": entry.id,
                        "created_at": entry.created_at,
                        "added_bytes": entry.added_bytes,
                    })
                })
                .collect::<Vec<_>>();
            Ok(json!({ "snapshots": snapshots }))
        }
        "restore" => {
            let server = server_arg(input, bound)?;
            let snapshot = string_arg(input, "snapshot")?;
            service.restore(&server, snapshot)?;
            Ok(json!({ "ok": true }))
        }
        "verify" => {
            let server = server_arg(input, bound)?;
            let snapshot = string_arg(input, "snapshot")?;
            Ok(json!({ "healthy": service.verify(&server, snapshot)? }))
        }
        "prune" => {
            let server = server_arg(input, bound)?;
            let keep = usize_arg(input, "keep")?;
            Ok(json!({ "removed": service.prune(&server, keep)? }))
        }
        "delete" => {
            let server = server_arg(input, bound)?;
            let snapshot = string_arg(input, "snapshot")?;
            Ok(json!({ "deleted": service.delete(&server, snapshot)? }))
        }
        other => Err(unknown_method("backup", other)),
    }
}

fn optional_string_arg(input: &Value, key: &str) -> Result<Option<String>, HostError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(raw)) => Ok(Some(raw.clone())),
        Some(_) => Err(HostError::invalid_input(format!("{key} must be a string"))),
    }
}

fn usize_arg(input: &Value, key: &str) -> Result<usize, HostError> {
    input
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| HostError::invalid_input(format!("{key} must be a non-negative integer")))
}

fn content(
    services: &HostServices,
    method: &str,
    input: &Value,
    bound: Option<&ServerName>,
) -> Result<Value, HostError> {
    let service = services
        .content
        .as_ref()
        .ok_or_else(|| HostError::unsupported("content is not available"))?;
    match method {
        "list" => {
            let server = server_arg(input, bound)?;
            let kind = content_kind_arg(input)?;
            let identify = match input.get("identify") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(value)) => *value,
                Some(_) => return Err(HostError::invalid_input("identify must be a boolean")),
            };
            let items = service
                .list(&server, kind, identify)?
                .into_iter()
                .map(|entry| {
                    let mut item = json!({
                        "file": entry.file_name,
                        "size": entry.size,
                    });
                    if let Some(identity) = entry.identity {
                        item["identity"] = content_identity_json(&identity);
                    }
                    item
                })
                .collect::<Vec<_>>();
            Ok(json!({ "items": items }))
        }
        "install" => {
            let server = server_arg(input, bound)?;
            let project = string_arg(input, "project")?;
            let version = optional_string_arg(input, "version")?;
            let kind = content_kind_arg(input)?;
            let entry = service.install(
                &server,
                &ContentInstallRequest {
                    project: project.to_owned(),
                    version: version.clone(),
                    kind,
                },
            )?;
            Ok(json!({
                "file": entry.file_name,
                "size": entry.size,
                "project": project,
                "version": version,
            }))
        }
        "remove" => {
            let server = server_arg(input, bound)?;
            let selector = string_arg(input, "selector")?;
            let kind = content_kind_arg(input)?;
            Ok(json!({ "removed": service.remove(&server, selector, kind)? }))
        }
        other => Err(unknown_method("content", other)),
    }
}

fn java(services: &HostServices, method: &str, input: &Value) -> Result<Value, HostError> {
    let service = services
        .java
        .as_ref()
        .ok_or_else(|| HostError::unsupported("java is not available"))?;
    match method {
        "list" => {
            let runtimes = service
                .list()?
                .iter()
                .map(java_runtime_json)
                .collect::<Vec<_>>();
            Ok(json!({ "runtimes": runtimes }))
        }
        "install" => {
            let major = java_major_arg(input)?;
            let runtime = service.install(major)?;
            Ok(json!({ "runtime": java_runtime_json(&runtime) }))
        }
        "remove" => {
            let path = path_arg(input, "path")?;
            Ok(json!({ "removed": service.remove(&path)? }))
        }
        other => Err(unknown_method("java", other)),
    }
}

fn java_major_arg(input: &Value) -> Result<JavaMajor, HostError> {
    match input.get("major") {
        Some(Value::Number(value)) => value
            .as_i64()
            .ok_or_else(|| HostError::invalid_input("major must be an integer"))
            .and_then(|raw| {
                JavaMajor::parse(raw)
                    .map_err(|err| HostError::invalid_input(format!("invalid java major: {err}")))
            }),
        Some(Value::String(raw)) => JavaMajor::parse_str(raw)
            .map_err(|err| HostError::invalid_input(format!("invalid java major: {err}"))),
        _ => Err(HostError::invalid_input("missing integer field major")),
    }
}

fn java_runtime_json(runtime: &JavaRuntime) -> Value {
    json!({
        "id": runtime.id,
        "path": runtime.path.to_string_lossy(),
        "major": runtime.major.as_i16(),
        "version": runtime.version,
        "vendor": runtime.vendor.as_str(),
        "managed": runtime.managed,
        "created_at": runtime.created_at,
        "updated_at": runtime.updated_at,
    })
}

fn content_kind_arg(input: &Value) -> Result<ContentKind, HostError> {
    match optional_string_arg(input, "kind")? {
        None => Ok(ContentKind::Mod),
        Some(raw) => ContentKind::parse(&raw)
            .map_err(|err| HostError::invalid_input(format!("invalid content kind: {err}"))),
    }
}

fn content_identity_json(identity: &ContentIdentity) -> Value {
    json!({
        "hash": identity.hash,
        "project_id": identity.project_id,
        "slug": identity.slug,
        "title": identity.title,
        "version_id": identity.version_id,
        "version_number": identity.version_number,
        "channel": identity.channel,
        "date_published": identity.date_published,
        "kind": identity.kind.map(ContentKind::as_str),
    })
}

fn path_arg(input: &Value, key: &str) -> Result<PathBuf, HostError> {
    Ok(PathBuf::from(string_arg(input, key)?))
}

fn optional_path_arg(input: &Value) -> Result<PathBuf, HostError> {
    match input.get("path") {
        None | Some(Value::Null) => Ok(PathBuf::new()),
        Some(Value::String(raw)) => Ok(PathBuf::from(raw)),
        Some(_) => Err(HostError::invalid_input("path must be a string")),
    }
}

/// 协议里的路径一律 POSIX 文本；内部 `PathBuf` 在 Windows 上带 `\`
fn path_text(path: &Path) -> String {
    let mut text = String::new();
    for component in path.components() {
        if let std::path::Component::Normal(part) = component {
            if !text.is_empty() {
                text.push('/');
            }
            text.push_str(&part.to_string_lossy());
        }
    }
    text
}

fn server_arg(input: &Value, bound: Option<&ServerName>) -> Result<ServerName, HostError> {
    let raw = string_arg(input, "server")?;
    let server = ServerName::parse(raw)
        .map_err(|_| HostError::invalid_input(format!("invalid server name: {raw:?}")))?;
    if let Some(bound) = bound
        && &server != bound
    {
        return Err(HostError::denied(format!(
            "plugin is scoped to server {bound}, not {server}"
        )));
    }
    Ok(server)
}

fn string_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str, HostError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::invalid_input(format!("missing string field {key}")))
}

fn base64_arg(input: &Value, key: &str) -> Result<Vec<u8>, HostError> {
    let raw = string_arg(input, key)?;
    base64::decode(raw).map_err(HostError::invalid_input)
}

fn unknown_method(capability: &str, method: &str) -> HostError {
    HostError::invalid_input(format!(
        "unknown method {method:?} for capability {capability}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::java::{JavaMajor, JavaVendor};
    use domain::plugin::PluginId;
    use port::plugin::{
        BackupEntry, ConsoleLine, ConsolePage, ContentEntry, ContentInstallRequest, FileEntry,
        ServerSummary,
    };
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::time::Duration;

    struct ReadOnly {
        status: Mutex<u32>,
    }

    impl ServerRead for ReadOnly {
        fn list(&self) -> Result<Vec<ServerSummary>, HostError> {
            Ok(vec![ServerSummary {
                name: ServerName::parse("survival").expect("name"),
                status: domain::server::status::ServerStatus::Running,
                version: None,
            }])
        }
        fn status(&self, _server: &ServerName) -> Result<ServerStatusView, HostError> {
            *self.status.lock().expect("lock") += 1;
            Ok(ServerStatusView {
                status: domain::server::status::ServerStatus::Running,
                version: None,
                players: Some(3),
                uptime: Some(Duration::from_secs(60)),
            })
        }
        fn version(
            &self,
            _server: &ServerName,
        ) -> Result<Option<domain::server::version::VersionId>, HostError> {
            Ok(None)
        }
        fn properties(&self, _server: &ServerName) -> Result<BTreeMap<String, String>, HostError> {
            Ok(BTreeMap::new())
        }
    }

    struct Console;

    impl ConsoleIo for Console {
        fn tail(
            &self,
            _server: &ServerName,
            cursor: Option<u64>,
            _max_lines: usize,
        ) -> Result<ConsolePage, HostError> {
            Ok(ConsolePage {
                lines: vec![ConsoleLine {
                    cursor: cursor.unwrap_or(0) + 1,
                    text: "hello".to_owned(),
                }],
                next: cursor.unwrap_or(0) + 1,
            })
        }
        fn send_line(&self, _server: &ServerName, _line: &str) -> Result<(), HostError> {
            Ok(())
        }
    }

    /// 记录生命周期调用的替身
    #[derive(Default)]
    struct Lifecycle {
        calls: Mutex<Vec<(&'static str, String, Option<Duration>)>>,
    }

    impl Lifecycle {
        fn record(&self, action: &'static str, server: &ServerName, grace: Option<Duration>) {
            self.calls
                .lock()
                .expect("lock")
                .push((action, server.as_str().to_owned(), grace));
        }
    }

    impl ServerLifecycle for Lifecycle {
        fn start(&self, server: &ServerName) -> Result<(), HostError> {
            self.record("start", server, None);
            Ok(())
        }

        fn stop(&self, server: &ServerName, grace: Duration) -> Result<StopOutcome, HostError> {
            self.record("stop", server, Some(grace));
            Ok(StopOutcome::Stopped {
                elapsed: Duration::from_millis(5),
            })
        }

        fn restart(&self, server: &ServerName, grace: Duration) -> Result<(), HostError> {
            self.record("restart", server, Some(grace));
            Ok(())
        }
    }

    fn lifecycle_services(lifecycle: &Arc<Lifecycle>) -> HostServices {
        HostServices {
            lifecycle: Some(Arc::clone(lifecycle) as Arc<dyn ServerLifecycle>),
            ..HostServices::default()
        }
    }

    /// 记录备份调用的替身
    #[derive(Default)]
    struct Backup {
        calls: Mutex<Vec<(String, String, Option<String>)>>,
    }

    impl Backup {
        fn record(&self, action: &str, server: &ServerName, extra: Option<&str>) {
            self.calls.lock().expect("lock").push((
                action.to_owned(),
                server.as_str().to_owned(),
                extra.map(str::to_owned),
            ));
        }
    }

    impl BackupOps for Backup {
        fn create(&self, server: &ServerName, label: Option<&str>) -> Result<String, HostError> {
            self.record("create", server, label);
            Ok("snap-1".to_owned())
        }

        fn list(&self, server: &ServerName) -> Result<Vec<BackupEntry>, HostError> {
            self.record("list", server, None);
            Ok(vec![BackupEntry {
                id: "snap-1".to_owned(),
                created_at: 1_700_000_000,
                added_bytes: 4096,
            }])
        }

        fn restore(&self, server: &ServerName, snapshot: &str) -> Result<(), HostError> {
            self.record("restore", server, Some(snapshot));
            Ok(())
        }

        fn verify(&self, server: &ServerName, snapshot: &str) -> Result<bool, HostError> {
            self.record("verify", server, Some(snapshot));
            Ok(true)
        }

        fn prune(&self, server: &ServerName, keep: usize) -> Result<usize, HostError> {
            self.record("prune", server, Some(&keep.to_string()));
            Ok(2)
        }

        fn delete(&self, server: &ServerName, snapshot: &str) -> Result<bool, HostError> {
            self.record("delete", server, Some(snapshot));
            Ok(true)
        }
    }

    fn backup_services(backup: &Arc<Backup>) -> HostServices {
        HostServices {
            backup: Some(Arc::clone(backup) as Arc<dyn BackupOps>),
            ..HostServices::default()
        }
    }

    /// 记录内容调用参数的替身
    #[derive(Default)]
    struct Content {
        calls: Mutex<Vec<(String, String, String)>>,
    }

    impl Content {
        fn record(&self, action: &str, server: &ServerName, extra: String) {
            self.calls.lock().expect("lock").push((
                action.to_owned(),
                server.as_str().to_owned(),
                extra,
            ));
        }
    }

    impl ContentOps for Content {
        fn list(
            &self,
            server: &ServerName,
            kind: ContentKind,
            identify: bool,
        ) -> Result<Vec<ContentEntry>, HostError> {
            self.record("list", server, format!("{kind}:{identify}"));
            Ok(vec![ContentEntry {
                identity: Some(ContentIdentity {
                    hash: "sha1".to_owned(),
                    project_id: "jei".to_owned(),
                    slug: Some("jei".to_owned()),
                    title: Some("JEI".to_owned()),
                    version_id: "v1".to_owned(),
                    version_number: "1.0".to_owned(),
                    channel: "release".to_owned(),
                    date_published: "2026-01-01T00:00:00Z".to_owned(),
                    kind: Some(ContentKind::Mod),
                }),
                file_name: "jei.jar".to_owned(),
                size: 1234,
            }])
        }

        fn install(
            &self,
            server: &ServerName,
            request: &ContentInstallRequest,
        ) -> Result<ContentEntry, HostError> {
            self.record(
                "install",
                server,
                format!(
                    "{}@{}:{}",
                    request.project,
                    request.version.as_deref().unwrap_or("-"),
                    request.kind
                ),
            );
            Ok(ContentEntry {
                identity: None,
                file_name: "jei.jar".to_owned(),
                size: 1234,
            })
        }

        fn remove(
            &self,
            server: &ServerName,
            selector: &str,
            kind: ContentKind,
        ) -> Result<usize, HostError> {
            self.record("remove", server, format!("{selector}:{kind}"));
            Ok(1)
        }
    }

    fn services() -> HostServices {
        HostServices {
            server_read: Some(Arc::new(ReadOnly {
                status: Mutex::new(0),
            })),
            console: Some(Arc::new(Console)),
            ..HostServices::default()
        }
    }

    fn granted(caps: &[Capability]) -> BTreeSet<Capability> {
        caps.iter().copied().collect()
    }

    fn ctx(granted_caps: &BTreeSet<Capability>) -> DispatchContext<'_> {
        DispatchContext {
            plugin: plugin(),
            granted: granted_caps,
            scope_server: None,
            contracts: None,
        }
    }

    fn plugin() -> &'static PluginId {
        use std::sync::OnceLock;
        static PLUGIN: OnceLock<PluginId> = OnceLock::new();
        PLUGIN.get_or_init(|| PluginId::parse("demo").expect("id"))
    }

    #[test]
    fn ungranted_capability_is_denied() {
        let call = Call::host(1, "server:read", "list", json!({}));
        let err = dispatch(&services(), &call, ctx(&granted(&[]))).expect_err("denied");
        assert_eq!(err.code, port::plugin::ErrorCode::Denied);
        assert_eq!(err.category, port::ErrorCategory::Unauthorized);
    }

    #[test]
    fn unknown_capability_and_method_are_usage_errors() {
        let granted_caps = granted(&[Capability::ServerRead]);
        let call = Call::host(1, "sandbox", "x", json!({}));
        assert_eq!(
            dispatch(&services(), &call, ctx(&granted_caps))
                .expect_err("unknown")
                .code,
            port::plugin::ErrorCode::InvalidInput
        );
        let call = Call::host(1, "server:read", "nope", json!({}));
        assert_eq!(
            dispatch(&services(), &call, ctx(&granted_caps))
                .expect_err("method")
                .code,
            port::plugin::ErrorCode::InvalidInput
        );
    }

    #[test]
    fn server_status_is_projected() {
        let call = Call::host(1, "server:read", "status", json!({ "server": "survival" }));
        let value =
            dispatch(&services(), &call, ctx(&granted(&[Capability::ServerRead]))).expect("status");
        assert_eq!(value["status"], "running");
        assert_eq!(value["players"], 3);
        assert_eq!(value["uptime_secs"], 60);
    }

    #[test]
    fn server_scope_rejects_other_servers() {
        let bound = ServerName::parse("survival").expect("name");
        let call = Call::host(1, "server:read", "status", json!({ "server": "creative" }));
        let caps = granted(&[Capability::ServerRead]);
        let err = dispatch(
            &services(),
            &call,
            DispatchContext {
                plugin: plugin(),
                granted: &caps,
                scope_server: Some(&bound),
                contracts: None,
            },
        )
        .expect_err("scoped");
        assert_eq!(err.code, port::plugin::ErrorCode::Denied);
    }

    #[test]
    fn console_tail_uses_cursor() {
        let call = Call::host(
            1,
            "console:read",
            "tail",
            json!({ "server": "survival", "cursor": 4, "max_lines": 1 }),
        );
        let value = dispatch(
            &services(),
            &call,
            ctx(&granted(&[Capability::ConsoleRead])),
        )
        .expect("tail");
        assert_eq!(value["next"], 5);
        assert_eq!(value["lines"][0]["text"], "hello");
    }

    struct Files {
        seen: Mutex<Option<(String, PathBuf, Option<bool>)>>,
    }

    impl Files {
        fn new() -> Self {
            Self {
                seen: Mutex::new(None),
            }
        }
    }

    impl FileOps for Files {
        fn read(&self, server: &ServerName, rel: &Path) -> Result<Vec<u8>, HostError> {
            *self.seen.lock().expect("lock") =
                Some((server.as_str().to_owned(), rel.to_path_buf(), None));
            Ok(b"hello".to_vec())
        }

        fn write(
            &self,
            server: &ServerName,
            rel: &Path,
            bytes: &[u8],
            overwrite: bool,
        ) -> Result<(), HostError> {
            assert_eq!(bytes, b"payload");
            *self.seen.lock().expect("lock") = Some((
                server.as_str().to_owned(),
                rel.to_path_buf(),
                Some(overwrite),
            ));
            Ok(())
        }

        fn list(&self, server: &ServerName, rel_dir: &Path) -> Result<Vec<FileEntry>, HostError> {
            *self.seen.lock().expect("lock") =
                Some((server.as_str().to_owned(), rel_dir.to_path_buf(), None));
            Ok(vec![FileEntry {
                path: Path::new("world").join("level.dat"),
                is_dir: false,
                size: 42,
            }])
        }

        fn remove(&self, server: &ServerName, rel: &Path) -> Result<bool, HostError> {
            *self.seen.lock().expect("lock") =
                Some((server.as_str().to_owned(), rel.to_path_buf(), None));
            Ok(true)
        }
    }

    fn files_services(files: &Arc<Files>) -> HostServices {
        HostServices {
            files: Some(Arc::clone(files) as Arc<dyn FileOps>),
            ..HostServices::default()
        }
    }

    #[test]
    fn files_read_returns_base64_and_uses_bound_server() {
        let files = Arc::new(Files::new());
        let bound = ServerName::parse("survival").expect("name");
        let call = Call::host(
            1,
            "files:read",
            "read",
            json!({ "server": "survival", "path": "world/level.dat" }),
        );
        let caps = granted(&[Capability::FilesRead]);
        let value = dispatch(
            &files_services(&files),
            &call,
            DispatchContext {
                plugin: plugin(),
                granted: &caps,
                scope_server: Some(&bound),
                contracts: None,
            },
        )
        .expect("read");
        assert_eq!(value["path"], "world/level.dat");
        assert_eq!(value["size"], 5);
        assert_eq!(value["data"], base64::encode(b"hello"));
        let seen = files.seen.lock().expect("lock").take().expect("called");
        assert_eq!(seen.0, "survival");
        assert_eq!(seen.1, PathBuf::from("world/level.dat"));
    }

    #[test]
    fn files_write_decodes_base64_and_forwards_overwrite() {
        let files = Arc::new(Files::new());
        let call = Call::host(
            1,
            "files:write",
            "write",
            json!({
                "server": "survival",
                "path": "plugins/demo.jar",
                "data": base64::encode(b"payload"),
                "overwrite": true,
            }),
        );
        let caps = granted(&[Capability::FilesWrite]);
        let value = dispatch(&files_services(&files), &call, ctx(&caps)).expect("write");
        assert_eq!(value["ok"], true);
        let seen = files.seen.lock().expect("lock").take().expect("called");
        assert_eq!(seen.2, Some(true));
    }

    #[test]
    fn files_list_projects_posix_paths_and_remove_reports_result() {
        let files = Arc::new(Files::new());
        let caps = granted(&[Capability::FilesRead, Capability::FilesWrite]);
        let call = Call::host(1, "files:read", "list", json!({ "server": "survival" }));
        let value = dispatch(&files_services(&files), &call, ctx(&caps)).expect("list");
        assert_eq!(value["entries"][0]["path"], "world/level.dat");
        assert_eq!(value["entries"][0]["size"], 42);

        let call = Call::host(
            2,
            "files:write",
            "remove",
            json!({ "server": "survival", "path": "plugins/old.jar" }),
        );
        let value = dispatch(&files_services(&files), &call, ctx(&caps)).expect("remove");
        assert_eq!(value["removed"], true);
    }

    #[test]
    fn files_methods_stay_inside_their_capability() {
        let files = Arc::new(Files::new());
        let call = Call::host(
            1,
            "files:read",
            "write",
            json!({ "server": "survival", "path": "a", "data": "" }),
        );
        let caps = granted(&[Capability::FilesRead]);
        assert_eq!(
            dispatch(&files_services(&files), &call, ctx(&caps))
                .expect_err("read cannot write")
                .code,
            port::plugin::ErrorCode::InvalidInput
        );
    }

    #[test]
    fn files_without_service_is_unsupported() {
        let call = Call::host(
            1,
            "files:read",
            "read",
            json!({ "server": "survival", "path": "a" }),
        );
        let caps = granted(&[Capability::FilesRead]);
        assert_eq!(
            dispatch(&HostServices::default(), &call, ctx(&caps))
                .expect_err("unsupported")
                .code,
            port::plugin::ErrorCode::Unsupported
        );
    }

    // ---- 生命周期与备份（API-LIFE / API-BAK） ----

    #[test]
    fn lifecycle_start_stop_restart_forward_bound_server_and_grace() {
        let lifecycle = Arc::new(Lifecycle::default());
        let bound = ServerName::parse("survival").expect("name");
        let caps = granted(&[Capability::ServerLifecycle]);
        let scope = DispatchContext {
            plugin: plugin(),
            granted: &caps,
            scope_server: Some(&bound),
            contracts: None,
        };
        let services = lifecycle_services(&lifecycle);

        let call = Call::host(
            1,
            "server:lifecycle",
            "start",
            json!({ "server": "survival" }),
        );
        assert_eq!(
            dispatch(&services, &call, scope).expect("start")["ok"],
            true
        );

        let call = Call::host(
            2,
            "server:lifecycle",
            "stop",
            json!({ "server": "survival", "grace_secs": 7 }),
        );
        let value = dispatch(&services, &call, scope).expect("stop");
        assert_eq!(value["outcome"], "stopped");
        assert_eq!(value["elapsed_ms"], 5);

        let call = Call::host(
            3,
            "server:lifecycle",
            "restart",
            json!({ "server": "survival" }),
        );
        dispatch(&services, &call, scope).expect("restart");

        let calls = lifecycle.calls.lock().expect("lock").clone();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], ("start", "survival".to_owned(), None));
        assert_eq!(
            calls[1],
            ("stop", "survival".to_owned(), Some(Duration::from_secs(7)))
        );
        assert_eq!(
            calls[2],
            (
                "restart",
                "survival".to_owned(),
                Some(Duration::from_secs(30))
            )
        );
    }

    #[test]
    fn lifecycle_scoped_plugin_cannot_target_another_server() {
        let lifecycle = Arc::new(Lifecycle::default());
        let bound = ServerName::parse("survival").expect("name");
        let caps = granted(&[Capability::ServerLifecycle]);
        let call = Call::host(
            1,
            "server:lifecycle",
            "start",
            json!({ "server": "creative" }),
        );
        let err = dispatch(
            &lifecycle_services(&lifecycle),
            &call,
            DispatchContext {
                plugin: plugin(),
                granted: &caps,
                scope_server: Some(&bound),
                contracts: None,
            },
        )
        .expect_err("scope");
        assert_eq!(err.code, port::plugin::ErrorCode::Denied);
    }

    #[test]
    fn backup_frames_round_trip_all_methods() {
        let backup = Arc::new(Backup::default());
        let services = backup_services(&backup);
        let caps = granted(&[Capability::Backup]);
        let ctx = ctx(&caps);

        let call = Call::host(
            1,
            "backup",
            "create",
            json!({ "server": "survival", "label": "before-upgrade" }),
        );
        assert_eq!(
            dispatch(&services, &call, ctx).expect("create")["snapshot"],
            "snap-1"
        );

        let call = Call::host(2, "backup", "list", json!({ "server": "survival" }));
        let value = dispatch(&services, &call, ctx).expect("list");
        assert_eq!(value["snapshots"][0]["id"], "snap-1");
        assert_eq!(value["snapshots"][0]["created_at"], 1_700_000_000);
        assert_eq!(value["snapshots"][0]["added_bytes"], 4096);

        let call = Call::host(
            3,
            "backup",
            "restore",
            json!({ "server": "survival", "snapshot": "snap-1" }),
        );
        assert_eq!(
            dispatch(&services, &call, ctx).expect("restore")["ok"],
            true
        );

        let call = Call::host(
            4,
            "backup",
            "verify",
            json!({ "server": "survival", "snapshot": "snap-1" }),
        );
        assert_eq!(
            dispatch(&services, &call, ctx).expect("verify")["healthy"],
            true
        );

        let call = Call::host(
            5,
            "backup",
            "prune",
            json!({ "server": "survival", "keep": 3 }),
        );
        assert_eq!(
            dispatch(&services, &call, ctx).expect("prune")["removed"],
            2
        );

        let call = Call::host(
            6,
            "backup",
            "delete",
            json!({ "server": "survival", "snapshot": "snap-1" }),
        );
        assert_eq!(
            dispatch(&services, &call, ctx).expect("delete")["deleted"],
            true
        );

        let calls = backup.calls.lock().expect("lock").clone();
        assert_eq!(calls[0].0, "create");
        assert_eq!(calls[0].2.as_deref(), Some("before-upgrade"));
        assert_eq!(calls[4].2.as_deref(), Some("3"));
    }

    #[test]
    fn backup_rejects_missing_arguments_and_methods() {
        let backup = Arc::new(Backup::default());
        let caps = granted(&[Capability::Backup]);
        let call = Call::host(1, "backup", "list", json!({ "server": "survival" }));
        let value = dispatch(&backup_services(&backup), &call, ctx(&caps)).expect("list");
        assert_eq!(value["snapshots"][0]["added_bytes"], 4096);

        let call = Call::host(1, "backup", "prune", json!({ "server": "survival" }));
        assert_eq!(
            dispatch(&backup_services(&backup), &call, ctx(&caps))
                .expect_err("missing keep")
                .code,
            port::plugin::ErrorCode::InvalidInput
        );

        let call = Call::host(
            1,
            "backup",
            "restore",
            json!({ "server": "survival", "snapshot": "snap-1" }),
        );
        assert_eq!(
            dispatch(&backup_services(&backup), &call, ctx(&granted(&[])))
                .expect_err("ungranted")
                .code,
            port::plugin::ErrorCode::Denied
        );
    }

    /// 记录 Java 调用参数的替身
    #[derive(Default)]
    struct Java {
        calls: Mutex<Vec<String>>,
    }

    impl Java {
        fn record(&self, call: String) {
            self.calls.lock().expect("lock").push(call);
        }
    }

    fn fake_runtime(major: JavaMajor) -> JavaRuntime {
        JavaRuntime {
            id: "runtime-1".to_owned(),
            path: PathBuf::from("/opt/java/17"),
            major,
            version: "17.0.9+9".to_owned(),
            vendor: JavaVendor::Temurin,
            managed: true,
            created_at: 1,
            updated_at: 2,
        }
    }

    impl JavaOps for Java {
        fn list(&self) -> Result<Vec<JavaRuntime>, HostError> {
            self.record("list".to_owned());
            Ok(vec![fake_runtime(JavaMajor::parse(17).expect("major"))])
        }

        fn install(&self, major: JavaMajor) -> Result<JavaRuntime, HostError> {
            self.record(format!("install:{major}"));
            Ok(fake_runtime(major))
        }

        fn remove(&self, path: &Path) -> Result<bool, HostError> {
            self.record(format!("remove:{}", path.display()));
            Ok(true)
        }
    }

    // ---- Java（API-JAVA） ----

    #[test]
    fn java_frames_forward_major_and_path() {
        let java = Arc::new(Java::default());
        let services = HostServices {
            java: Some(Arc::clone(&java) as Arc<dyn JavaOps>),
            ..HostServices::default()
        };
        let caps = granted(&[Capability::Java]);
        let ctx = ctx(&caps);

        let value =
            dispatch(&services, &Call::host(1, "java", "list", json!({})), ctx).expect("list");
        assert_eq!(value["runtimes"][0]["id"], "runtime-1");
        assert_eq!(value["runtimes"][0]["major"], 17);
        assert_eq!(value["runtimes"][0]["vendor"], "temurin");
        assert_eq!(value["runtimes"][0]["managed"], true);

        let value = dispatch(
            &services,
            &Call::host(2, "java", "install", json!({ "major": 21 })),
            ctx,
        )
        .expect("install");
        assert_eq!(value["runtime"]["major"], 21);

        let value = dispatch(
            &services,
            &Call::host(3, "java", "remove", json!({ "path": "/opt/java/17" })),
            ctx,
        )
        .expect("remove");
        assert_eq!(value["removed"], true);

        let calls = java.calls.lock().expect("lock").clone();
        assert_eq!(
            calls,
            [
                "list".to_owned(),
                "install:21".to_owned(),
                "remove:/opt/java/17".to_owned()
            ]
        );
    }

    #[test]
    fn java_requires_grant_and_service() {
        let call = Call::host(1, "java", "list", json!({}));
        assert_eq!(
            dispatch(&services(), &call, ctx(&granted(&[])))
                .expect_err("ungranted")
                .code,
            port::plugin::ErrorCode::Denied
        );

        let caps = granted(&[Capability::Java]);
        assert_eq!(
            dispatch(&HostServices::default(), &call, ctx(&caps))
                .expect_err("unsupported")
                .code,
            port::plugin::ErrorCode::Unsupported
        );
    }

    #[test]
    fn java_rejects_invalid_major() {
        let java = Arc::new(Java::default());
        let services = HostServices {
            java: Some(Arc::clone(&java) as Arc<dyn JavaOps>),
            ..HostServices::default()
        };
        let caps = granted(&[Capability::Java]);
        for input in [json!({ "major": 7 }), json!({ "major": "x" }), json!({})] {
            assert_eq!(
                dispatch(
                    &services,
                    &Call::host(1, "java", "install", input),
                    ctx(&caps)
                )
                .expect_err("invalid major")
                .code,
                port::plugin::ErrorCode::InvalidInput
            );
        }
        assert!(java.calls.lock().expect("lock").is_empty());
    }

    // ---- 内容（API-CNT） ----

    #[test]
    fn content_frames_forward_kind_identify_and_arguments() {
        let content = Arc::new(Content::default());
        let services = HostServices {
            content: Some(Arc::clone(&content) as Arc<dyn ContentOps>),
            ..HostServices::default()
        };
        let caps = granted(&[Capability::Content]);
        let ctx = ctx(&caps);

        let call = Call::host(
            1,
            "content",
            "list",
            json!({ "server": "survival", "kind": "plugin", "identify": true }),
        );
        let value = dispatch(&services, &call, ctx).expect("list");
        assert_eq!(value["items"][0]["file"], "jei.jar");
        assert_eq!(value["items"][0]["size"], 1234);
        assert_eq!(value["items"][0]["identity"]["project_id"], "jei");

        let call = Call::host(
            2,
            "content",
            "install",
            json!({ "server": "survival", "project": "jei", "version": "v1", "kind": "plugin" }),
        );
        let value = dispatch(&services, &call, ctx).expect("install");
        assert_eq!(value["file"], "jei.jar");
        assert_eq!(value["project"], "jei");
        assert_eq!(value["version"], "v1");

        let call = Call::host(
            3,
            "content",
            "remove",
            json!({ "server": "survival", "selector": "jei.jar", "kind": "plugin" }),
        );
        assert_eq!(
            dispatch(&services, &call, ctx).expect("remove")["removed"],
            1
        );

        let calls = content.calls.lock().expect("lock").clone();
        assert_eq!(
            calls[0],
            (
                "list".to_owned(),
                "survival".to_owned(),
                "plugin:true".to_owned()
            )
        );
        assert_eq!(
            calls[1],
            (
                "install".to_owned(),
                "survival".to_owned(),
                "jei@v1:plugin".to_owned()
            )
        );
        assert_eq!(
            calls[2],
            (
                "remove".to_owned(),
                "survival".to_owned(),
                "jei.jar:plugin".to_owned()
            )
        );
    }

    #[test]
    fn content_defaults_to_mod_and_rejects_unknown_kind() {
        let content = Arc::new(Content::default());
        let services = HostServices {
            content: Some(Arc::clone(&content) as Arc<dyn ContentOps>),
            ..HostServices::default()
        };
        let caps = granted(&[Capability::Content]);

        let call = Call::host(1, "content", "list", json!({ "server": "survival" }));
        dispatch(&services, &call, ctx(&caps)).expect("list");

        let call = Call::host(
            1,
            "content",
            "list",
            json!({ "server": "survival", "kind": "shaderpack" }),
        );
        assert_eq!(
            dispatch(&services, &call, ctx(&caps))
                .expect_err("unknown kind")
                .code,
            port::plugin::ErrorCode::InvalidInput
        );

        let calls = content.calls.lock().expect("lock").clone();
        assert_eq!(
            calls[0],
            (
                "list".to_owned(),
                "survival".to_owned(),
                "mod:false".to_owned()
            )
        );
    }

    #[test]
    fn content_requires_grant_and_service() {
        let content = Arc::new(Content::default());
        let services = HostServices {
            content: Some(Arc::clone(&content) as Arc<dyn ContentOps>),
            ..HostServices::default()
        };
        let call = Call::host(
            1,
            "content",
            "remove",
            json!({ "server": "survival", "selector": "jei.jar" }),
        );
        assert_eq!(
            dispatch(&services, &call, ctx(&granted(&[])))
                .expect_err("ungranted")
                .code,
            port::plugin::ErrorCode::Denied
        );

        let call = Call::host(1, "content", "list", json!({ "server": "survival" }));
        let caps = granted(&[Capability::Content]);
        assert_eq!(
            dispatch(&HostServices::default(), &call, ctx(&caps))
                .expect_err("unsupported")
                .code,
            port::plugin::ErrorCode::Unsupported
        );
    }

    #[test]
    fn missing_host_service_is_unsupported() {
        let call = Call::host(
            1,
            "storage:kv",
            "get",
            json!({ "plugin": "demo", "key": "k" }),
        );
        let caps = granted(&[Capability::StorageKv]);
        let err = dispatch(&HostServices::default(), &call, ctx(&caps)).expect_err("unsupported");
        assert_eq!(err.code, port::plugin::ErrorCode::Unsupported);
    }

    #[test]
    fn later_stage_capabilities_are_explicitly_unsupported() {
        let call = Call::host(1, "backup", "list", json!({ "server": "survival" }));
        let caps = granted(&[Capability::Backup]);
        let err = dispatch(&services(), &call, ctx(&caps)).expect_err("unsupported");
        assert_eq!(err.code, port::plugin::ErrorCode::Unsupported);
    }

    #[test]
    fn plugin_namespace_is_taken_from_context_not_input() {
        struct Kv;
        impl KvStore for Kv {
            fn get(&self, plugin: &PluginId, _key: &str) -> Result<Option<Vec<u8>>, HostError> {
                Ok(Some(plugin.as_str().as_bytes().to_vec()))
            }
            fn set(&self, _plugin: &PluginId, _key: &str, _value: &[u8]) -> Result<(), HostError> {
                Ok(())
            }
            fn delete(&self, _plugin: &PluginId, _key: &str) -> Result<bool, HostError> {
                Ok(false)
            }
            fn list(&self, _plugin: &PluginId, _prefix: &str) -> Result<Vec<String>, HostError> {
                Ok(Vec::new())
            }
        }
        let services = HostServices {
            kv: Some(Arc::new(Kv)),
            ..HostServices::default()
        };
        let call = Call::host(
            1,
            "storage:kv",
            "get",
            json!({ "plugin": "other", "key": "k" }),
        );
        let caps = granted(&[Capability::StorageKv]);
        let value = dispatch(
            &services,
            &call,
            DispatchContext {
                plugin: plugin(),
                granted: &caps,
                scope_server: None,
                contracts: None,
            },
        )
        .expect("kv get");
        assert_eq!(value["value"], base64::encode(b"demo"));
    }

    struct RecordingNet {
        seen: Mutex<Option<(PluginId, NetRequest)>>,
    }

    impl NetProxy for RecordingNet {
        fn request(
            &self,
            plugin: &PluginId,
            request: NetRequest,
        ) -> Result<port::plugin::NetResponse, HostError> {
            *self.seen.lock().expect("lock") = Some((plugin.clone(), request));
            Ok(port::plugin::NetResponse {
                status: 201,
                headers: BTreeMap::from([("x-test".to_owned(), "yes".to_owned())]),
                body: b"ok".to_vec(),
            })
        }
    }

    fn net_services(net: &Arc<RecordingNet>) -> HostServices {
        HostServices {
            net: Some(Arc::clone(net) as Arc<dyn NetProxy>),
            ..HostServices::default()
        }
    }

    #[test]
    fn net_request_decodes_base64_and_uses_host_identity() {
        let net = Arc::new(RecordingNet {
            seen: Mutex::new(None),
        });
        let call = Call::host(
            1,
            "net:outbound",
            "request",
            json!({
                "method": "post",
                "url": "https://api.example.com/hook",
                "headers": { "content-type": "text/plain" },
                "body": base64::encode(b"hello"),
                "timeout_ms": 1500,
            }),
        );
        let caps = granted(&[Capability::NetOutbound]);
        let value = dispatch(&net_services(&net), &call, ctx(&caps)).expect("net");
        assert_eq!(value["status"], 201);
        assert_eq!(value["headers"]["x-test"], "yes");
        assert_eq!(value["body"], base64::encode(b"ok"));
        assert_eq!(value["body_len"], 2);
        let (seen_plugin, seen) = net.seen.lock().expect("lock").take().expect("called");
        assert_eq!(&seen_plugin, plugin());
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.url, "https://api.example.com/hook");
        assert_eq!(
            seen.headers.get("content-type").map(String::as_str),
            Some("text/plain")
        );
        assert_eq!(seen.body.as_deref(), Some(&b"hello"[..]));
        assert_eq!(seen.timeout, Duration::from_millis(1500));
    }

    #[test]
    fn net_null_body_is_absent_and_bad_headers_are_invalid() {
        let net = Arc::new(RecordingNet {
            seen: Mutex::new(None),
        });
        let services = net_services(&net);
        let caps = granted(&[Capability::NetOutbound]);
        let call = Call::host(
            1,
            "net:outbound",
            "request",
            json!({ "url": "https://api.example.com", "body": null }),
        );
        dispatch(&services, &call, ctx(&caps)).expect("net");
        assert!(
            net.seen
                .lock()
                .expect("lock")
                .as_ref()
                .expect("called")
                .1
                .body
                .is_none()
        );

        let call = Call::host(
            2,
            "net:outbound",
            "request",
            json!({ "url": "https://api.example.com", "headers": { "x": 1 } }),
        );
        assert_eq!(
            dispatch(&services, &call, ctx(&caps))
                .expect_err("header")
                .code,
            port::plugin::ErrorCode::InvalidInput
        );
    }

    #[test]
    fn net_without_proxy_is_unsupported() {
        let call = Call::host(1, "net:outbound", "request", json!({ "url": "https://x" }));
        let caps = granted(&[Capability::NetOutbound]);
        assert_eq!(
            dispatch(&HostServices::default(), &call, ctx(&caps))
                .expect_err("no proxy")
                .code,
            port::plugin::ErrorCode::Unsupported
        );
    }

    struct RecordingInvoker {
        seen: Mutex<Option<port::plugin::ContractCall>>,
    }

    impl ContractInvoker for RecordingInvoker {
        fn invoke(&self, call: port::plugin::ContractCall) -> Result<Value, HostError> {
            *self.seen.lock().expect("lock") = Some(call);
            Ok(json!({ "delivered": true }))
        }
    }

    #[test]
    fn contract_call_carries_host_identity_and_trace() {
        let invoker = Arc::new(RecordingInvoker {
            seen: Mutex::new(None),
        });
        let services = HostServices {
            contracts: Some(Arc::clone(&invoker) as Arc<dyn ContractInvoker>),
            ..HostServices::default()
        };
        let caps = BTreeSet::new();
        let call = Call::contract(
            1,
            "chat.publish@1",
            Some("other".to_owned()),
            Some("t-1".to_owned()),
            json!({ "text": "hi" }),
        );
        let value = dispatch(
            &services,
            &call,
            DispatchContext {
                plugin: plugin(),
                granted: &caps,
                scope_server: None,
                contracts: services.contracts.as_deref(),
            },
        )
        .expect("contract");
        assert_eq!(value["delivered"], true);
        let seen = invoker.seen.lock().expect("lock").take().expect("called");
        assert_eq!(&seen.caller, plugin());
        assert_eq!(seen.contract, "chat.publish@1");
        assert_eq!(seen.target.as_ref().map(PluginId::as_str), Some("other"));
        assert_eq!(seen.trace.as_deref(), Some("t-1"));
        assert_eq!(seen.input["text"], "hi");
    }

    #[test]
    fn contract_without_router_is_unsupported() {
        let caps = BTreeSet::new();
        let call = Call::contract(1, "chat.publish@1", None, None, json!({}));
        assert_eq!(
            dispatch(&HostServices::default(), &call, ctx(&caps))
                .expect_err("no router")
                .code,
            port::plugin::ErrorCode::Unsupported
        );
    }
}
