//! 一帧 `Request` → 一次 `service` 调用 → 一帧 `Response`
//!
//! 这里**只做转接**：参数解包、错误分类、结果封帧。任何「能不能做」的判定
//! 都不在这里，而在 `service` / `domain`——agent 与本地 CLI 跑的是同一份业务，
//! 差异只在传输。
//!
//! 错误分类（[`ErrorKind`]）是协议的一部分：客户端按 `kind` 决定退出码与提示，
//! 不解析 `message`。因此映射按**领域判定**（各 `kind()`）来做，不看错误文案。

use super::Agent;
use domain::java::JavaMajor;
use domain::server::version::{ServerVariant, VersionId};
use protocol::remote::wire::{WireServer, WireStatusEntry};
use protocol::remote::{ErrorKind, Request, Response};
use service::{
    InitOutcome, JavaError, LifecycleError, LifecycleErrorKind, ServerRegistryError,
    ServerRegistryErrorKind, StartOptions, WorkspaceError, WorkspaceErrorKind,
};
use transport::RemoteFailure;

impl Agent {
    /// 执行一个已认证的操作
    pub(crate) fn dispatch(&self, request: &Request) -> Response {
        match request {
            Request::Ping => Response::Pong {
                agent_version: env!("CARGO_PKG_VERSION").to_owned(),
                uptime_secs: self.started.elapsed().as_secs(),
            },
            Request::Shutdown => self.shutdown(),

            Request::WorkspaceList
            | Request::WorkspaceInit { .. }
            | Request::WorkspaceUse { .. }
            | Request::WorkspaceRemove { .. } => self.workspace_op(request),
            Request::ServerList | Request::ServerCreate { .. } | Request::ServerRemove { .. } => {
                self.server_op(request)
            }
            Request::JavaList | Request::JavaScan => self.java_op(request),

            Request::ClientList => match self.clients.load() {
                Ok(records) => Response::Clients {
                    items: records.iter().map(Into::into).collect(),
                },
                Err(err) => failure(ErrorKind::IO, err),
            },
            Request::ClientRevoke { selector } => self.revoke(selector),

            Request::StatusMany { ids } => self.status_many(ids),
            Request::Start {
                server_id,
                ram_mib,
                accept_eula,
            } => self.start(server_id, *ram_mib, *accept_eula),
            Request::Stop {
                server_id,
                grace_secs,
                force,
            } => self.stop(server_id, *grace_secs, *force),
            Request::SendLine { server_id, line } => self.send_line(server_id, line),
            Request::LogsTail { server_id, lines } => self.logs_tail(server_id, *lines),

            // 握手帧只在连接前两步出现；走到这里说明客户端在已认证连接上重放
            Request::Hello { .. } | Request::Enroll { .. } | Request::Authenticate { .. } => {
                RemoteFailure::new(
                    ErrorKind::PROTOCOL,
                    format!("{} is a handshake frame", request.operation()),
                )
                .into_response()
            }
        }
    }

    /// 工作目录帧
    fn workspace_op(&self, request: &Request) -> Response {
        match request {
            Request::WorkspaceList => match self.core.workspaces.list() {
                Ok(items) => Response::Workspaces {
                    items: items.iter().map(Into::into).collect(),
                },
                Err(err) => failure(ErrorKind::IO, err),
            },
            Request::WorkspaceInit { dir, name } => {
                match self
                    .core
                    .workspaces
                    .init(std::path::Path::new(dir), name.as_deref())
                {
                    Ok(outcome) => Response::Workspaces {
                        items: vec![init_workspace(&outcome).into()],
                    },
                    Err(err) => workspace_failure(&err),
                }
            }
            Request::WorkspaceUse { selector } => {
                match self.core.workspaces.set_current(selector) {
                    Ok(ws) => Response::Workspaces {
                        items: vec![(&ws).into()],
                    },
                    Err(err) => workspace_failure(&err),
                }
            }
            Request::WorkspaceRemove { selector, purge } => {
                match self.core.workspaces.remove(selector, *purge, &self.cwd) {
                    Ok(outcome) => Response::Removed {
                        disk_purged: outcome.disk_purged,
                    },
                    Err(err) => workspace_failure(&err),
                }
            }
            // 调用方只按上面四帧分派进来
            other => unreachable_frame(other),
        }
    }

    /// 服务器注册表帧
    fn server_op(&self, request: &Request) -> Response {
        match request {
            Request::ServerList => match self.core.registry.list() {
                Ok(items) => match items
                    .iter()
                    .map(|srv| WireServer::try_from(&srv.meta))
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(items) => Response::Servers { items },
                    Err(err) => failure(ErrorKind::IO, err),
                },
                Err(err) => registry_failure(&err),
            },
            Request::ServerCreate {
                workspace,
                name,
                version,
                variant,
                java_major,
            } => {
                let variant = match ServerVariant::parse(variant) {
                    Ok(variant) => variant,
                    Err(err) => return failure(ErrorKind::INVALID, err),
                };
                let java_major = match java_major.map(JavaMajor::parse) {
                    Some(Ok(major)) => Some(major),
                    Some(Err(err)) => return failure(ErrorKind::INVALID, err),
                    None => None,
                };
                match self.core.registry.create(
                    workspace.as_deref(),
                    name,
                    &VersionId::new(version.clone()),
                    variant,
                    java_major,
                ) {
                    Ok(srv) => match WireServer::try_from(&srv.meta) {
                        Ok(item) => Response::Servers { items: vec![item] },
                        Err(err) => failure(ErrorKind::IO, err),
                    },
                    Err(err) => registry_failure(&err),
                }
            }
            Request::ServerRemove { name, purge } => {
                match self.core.registry.remove(name, *purge, &self.cwd) {
                    Ok(outcome) => Response::Removed {
                        disk_purged: outcome.disk_purged,
                    },
                    Err(err) => registry_failure(&err),
                }
            }
            other => unreachable_frame(other),
        }
    }

    /// Java 帧
    fn java_op(&self, request: &Request) -> Response {
        match request {
            Request::JavaList => self.java_list(),
            // 扫描结果直接回列表：跨机器没有「扫到几个」的渲染位置，
            // 客户端要的是「现在有哪些」
            Request::JavaScan => match self.core.java.scan(true, &mut |_| {}) {
                Ok(_) => self.java_list(),
                Err(err) => java_failure(&err),
            },
            other => unreachable_frame(other),
        }
    }

    /// 本机 Java 运行时列表（未扫描过则先扫，与 `java ls` 同一条路径）
    fn java_list(&self) -> Response {
        match self.core.java.list_or_scan(&mut || {}, &mut |_| {}) {
            Ok(items) => Response::Javas {
                items: items.iter().map(Into::into).collect(),
            },
            Err(err) => java_failure(&err),
        }
    }

    /// `Shutdown`：置停止标志，由监听循环退出
    fn shutdown(&self) -> Response {
        let Some(handle) = self.stop.get() else {
            return RemoteFailure::new(ErrorKind::IO, "agent is not listening yet").into_response();
        };
        handle.request_stop();
        tracing::info!("remote shutdown requested");
        Response::Ok
    }

    /// 撤销客户端：先按身份精确匹配，再按名字匹配
    ///
    /// 名字可能重名，取第一个命中的：撤销是幂等的，用户看到结果后可以再撤一个。
    fn revoke(&self, selector: &str) -> Response {
        let records = match self.clients.load() {
            Ok(records) => records,
            Err(err) => return failure(ErrorKind::IO, err),
        };
        let target = records
            .iter()
            .find(|record| record.client_id.as_str() == selector)
            .or_else(|| records.iter().find(|record| record.name == selector))
            .map(|record| record.client_id.clone());
        let Some(client_id) = target else {
            return RemoteFailure::new(ErrorKind::NOT_FOUND, format!("no such client: {selector}"))
                .into_response();
        };
        match self.clients.revoke(&client_id) {
            // 撤销即删除：本来就不存在也算成功（幂等）
            Ok(_) => {
                tracing::info!(client = %client_id, "remote client revoked");
                Response::Ok
            }
            Err(err) => failure(ErrorKind::IO, err),
        }
    }

    /// 批量实时状态：一次 `statuses()` 取全量再按请求顺序挑
    ///
    /// 请求里可能有一台已被别处删掉：那个位置报「不存在」，不拖死整批——
    /// 列表页要把失败的那台渲染成 `unknown` 而不是整体报错。
    fn status_many(&self, ids: &[String]) -> Response {
        let servers = match self.core.registry.list() {
            Ok(servers) => servers,
            Err(err) => return registry_failure(&err),
        };
        let statuses = match self.core.lifecycle.statuses() {
            Ok(statuses) => statuses,
            Err(err) => return lifecycle_failure(&err),
        };
        let mut items = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(server) = servers.iter().find(|srv| srv.meta.id.as_str() == id) else {
                items.push(WireStatusEntry::failed(format!("server not found: {id}")));
                continue;
            };
            let name = server.meta.name.as_str();
            items.push(match statuses.get(name) {
                Some(Ok(status)) => WireStatusEntry::ok(status.as_ref()),
                Some(Err(err)) => WireStatusEntry::failed(err.to_string()),
                None => WireStatusEntry::failed(format!("no status for {name}")),
            });
        }
        Response::Statuses { items }
    }

    /// 拉起：启动方式由本机注册表决定（`Request::Start` 不带 `launch`）
    fn start(&self, server_id: &str, ram_mib: Option<u32>, accept_eula: bool) -> Response {
        let name = match self.name_of(server_id) {
            Ok(name) => name,
            Err(response) => return response,
        };
        let opts = StartOptions {
            accept_eula,
            ram_mib,
            supervisor_exe: self.supervisor_exe().to_path_buf(),
            supervisor_args: vec![std::ffi::OsString::from("__supervise")],
        };
        match self
            .core
            .lifecycle
            .start(&name, &opts, &mut || {}, &mut |_| {})
        {
            Ok(outcome) => Response::Started {
                outcome: (&outcome).into(),
            },
            Err(err) => lifecycle_failure(&err),
        }
    }

    /// 停机：不在运行视为幂等成功（`StopOutcome::AlreadyStopped`）
    fn stop(&self, server_id: &str, grace_secs: u64, force: bool) -> Response {
        let name = match self.name_of(server_id) {
            Ok(name) => name,
            Err(response) => return response,
        };
        let spec = port::StopSpec {
            grace: std::time::Duration::from_secs(grace_secs),
            force,
        };
        match self.core.lifecycle.stop(&name, spec) {
            Ok(outcome) => Response::Stopped {
                outcome: (&outcome).into(),
            },
            Err(err) => lifecycle_failure(&err),
        }
    }

    /// 向控制台写一行
    fn send_line(&self, server_id: &str, line: &str) -> Response {
        let name = match self.name_of(server_id) {
            Ok(name) => name,
            Err(response) => return response,
        };
        match self.core.lifecycle.send_line(&name, line) {
            Ok(()) => Response::Ok,
            Err(err) => lifecycle_failure(&err),
        }
    }

    /// 控制台日志尾部
    fn logs_tail(&self, server_id: &str, lines: usize) -> Response {
        let name = match self.name_of(server_id) {
            Ok(name) => name,
            Err(response) => return response,
        };
        match self.core.lifecycle.log_tail(&name, lines) {
            Ok(lines) => Response::LogLines { lines },
            Err(err) => lifecycle_failure(&err),
        }
    }

    /// 身份 → 名字：`service` 的写路径按名字寻址，远端帧按稳定身份寻址
    fn name_of(&self, server_id: &str) -> Result<String, Response> {
        let servers = self
            .core
            .registry
            .list()
            .map_err(|err| registry_failure(&err))?;
        servers
            .iter()
            .find(|srv| srv.meta.id.as_str() == server_id)
            .map(|srv| srv.meta.name.as_str().to_owned())
            .ok_or_else(|| {
                RemoteFailure::new(ErrorKind::NOT_FOUND, format!("no such server: {server_id}"))
                    .into_response()
            })
    }
}

/// `init` 的四种结局都只关心「落在哪个工作目录」
fn init_workspace(outcome: &InitOutcome) -> &domain::workspace::Workspace {
    match outcome {
        InitOutcome::Created(ws)
        | InitOutcome::Recovered(ws)
        | InitOutcome::AlreadyRegistered(ws)
        | InitOutcome::MarkerRestored(ws) => ws,
    }
}

/// 分派到子函数后，理论上不会出现的帧
///
/// 子函数按帧族拆分只为可读性，入口已把族外的帧分给别处。真走到这里说明
/// 分派表漏了一个分支——报协议错误而不是 panic：agent 不能因为一帧内部
/// 不一致就整进程退出。
fn unreachable_frame(request: &Request) -> Response {
    RemoteFailure::new(
        ErrorKind::PROTOCOL,
        format!("{} was routed to the wrong handler", request.operation()),
    )
    .into_response()
}

/// 通用失败帧（`Display` 只用于日志，客户端按 `kind` 分类）
fn failure(kind: &'static str, err: impl std::fmt::Display) -> Response {
    RemoteFailure::new(kind, err.to_string()).into_response()
}

fn workspace_failure(err: &WorkspaceError) -> Response {
    let kind = match err.kind() {
        Some(WorkspaceErrorKind::NotFound { .. } | WorkspaceErrorKind::NoCurrent) => {
            ErrorKind::NOT_FOUND
        }
        Some(WorkspaceErrorKind::AlreadyExists { .. }) => ErrorKind::ALREADY_EXISTS,
        Some(
            WorkspaceErrorKind::NotEmpty { .. }
            | WorkspaceErrorKind::NotAWorkspace { .. }
            | WorkspaceErrorKind::Ambiguous { .. }
            | WorkspaceErrorKind::CwdInsideTarget { .. },
        ) => ErrorKind::INVALID,
        // 结构化分类缺失（IO / 存储 / 名字非法）与未来新增变体：归到通用 IO
        Some(_) | None => ErrorKind::IO,
    };
    failure(kind, err)
}

fn registry_failure(err: &ServerRegistryError) -> Response {
    let kind = match err.kind() {
        Some(ServerRegistryErrorKind::NotFound { .. }) => ErrorKind::NOT_FOUND,
        Some(ServerRegistryErrorKind::AlreadyExists { .. }) => ErrorKind::ALREADY_EXISTS,
        Some(
            ServerRegistryErrorKind::TargetInsideWorkspace { .. }
            | ServerRegistryErrorKind::WorkspaceMismatch { .. },
        ) => ErrorKind::INVALID,
        Some(_) | None => ErrorKind::IO,
    };
    failure(kind, err)
}

fn java_failure(err: &JavaError) -> Response {
    let kind = match err {
        JavaError::NotFound(_) => ErrorKind::NOT_FOUND,
        _ => ErrorKind::IO,
    };
    failure(kind, err)
}

fn lifecycle_failure(err: &LifecycleError) -> Response {
    let kind = match err.kind() {
        Some(
            // 停机超时说明服务端仍在运行：状态冲突，客户端重试即可
            LifecycleErrorKind::AlreadyRunning
            | LifecycleErrorKind::NotRunning
            | LifecycleErrorKind::Busy { .. }
            | LifecycleErrorKind::StopTimedOut { .. },
        ) => ErrorKind::BUSY,
        Some(
            LifecycleErrorKind::NoJar { .. }
            | LifecycleErrorKind::EulaRequired
            | LifecycleErrorKind::NoJava
            | LifecycleErrorKind::UnknownJavaMajor { .. },
        ) => ErrorKind::INVALID,
        Some(_) | None => ErrorKind::IO,
    };
    failure(kind, err)
}
