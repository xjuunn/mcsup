//! 一帧 `Request` → 一次 `service` 调用 → 一帧 `Response`
//!
//! 这里**只做转接**：参数解包、错误分类、结果封帧。任何「能不能做」的判定
//! 都不在这里，而在 `service` / `domain`——agent 与本地 CLI 跑的是同一份业务，
//! 差异只在传输。
//!
//! 错误分类（[`ErrorKind`]）是协议的一部分：客户端按 `kind` 决定退出码与提示，
//! 不解析 `message`。因此映射按**领域判定**（各 `kind()`）来做，不看错误文案。

use super::Agent;
use domain::java::{JavaMajor, JavaPackageType, JavaVendor};
use domain::server::settings::SettingKey;
use domain::server::version::{ServerVariant, ServerVersion, VersionId};
use file::FileCoreInspector;
use port::TaskObserver;
use protocol::remote::wire::{
    WireModBlocked, WirePropUpdate, WireServer, WireSettings, WireSnapshot, WireStatusEntry,
    WireVersion,
};
use protocol::remote::{ErrorKind, Request, Response};
use service::{
    InitOutcome, InstallJarError, InstallerContext, JavaError, LifecycleError, ModBlocked,
    ModBlocker, ModpackError, ModpackInstallOptions, PropsError, ServerInstaller,
    ServerPropsService, ServerRegistryError, ServerRegistryErrorKind, ServerSettingsService,
    SettingsError, StartOptions, UninstallOutcome, WorkspaceError, WorkspaceErrorKind,
};
use transport::{RemoteFailure, SessionInfo};

impl Agent {
    /// 执行一个已认证的操作，并把长任务的阶段 / 进度 / 输出交给 `progress`
    ///
    /// `progress` 由传输层提供：它把每条事件写成 `Response::Task` 中间帧，客户端
    /// 用**同一个**观察者渲染——远端与本地共用一套进度代码，差别只在传输。
    ///
    /// `session` 是已认证的连接身份。绝大多数帧不需要它（目标由帧参数指定），
    /// 只有「作用于调用方自己」的帧才读——目前是
    /// [`Request::ClientSelfRevoke`]。让它走参数而不是让帧自带 selector，
    /// 是为了让「我是谁」只有一个来源：认证结果。客户端传什么都不能改变它。
    pub(crate) fn dispatch_with(
        &self,
        session: &SessionInfo,
        request: &Request,
        progress: &dyn TaskObserver,
    ) -> Response {
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
            Request::ServerList
            | Request::ServerCreate { .. }
            | Request::ServerCreateModpack { .. }
            | Request::ServerRemove { .. }
            | Request::ServerInstall { .. }
            | Request::ServerLink { .. }
            | Request::ServerVariants
            | Request::ServerVersions { .. }
            | Request::PropsGet { .. }
            | Request::PropsSet { .. }
            | Request::SettingsSet { .. } => self.server_op(request, progress),
            Request::JavaList
            | Request::JavaScan
            | Request::JavaInstall { .. }
            | Request::JavaUninstall { .. } => self.java_op(request, progress),

            Request::ClientList => match self.clients.load() {
                Ok(records) => Response::Clients {
                    items: records.iter().map(Into::into).collect(),
                },
                Err(err) => failure(ErrorKind::IO, err),
            },
            Request::ClientRevoke { selector } => self.revoke(selector),
            Request::ClientSelfRevoke => self.self_revoke(session),

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
            Request::LogsFollow {
                server_id,
                lines,
                timeout_ms,
            } => self.logs_follow(server_id, *lines, *timeout_ms),

            Request::BackupCreate {
                server_id,
                label,
                scope,
                no_reuse,
                flush_wait_ms,
            } => self.backup_create(
                server_id,
                label.as_deref(),
                scope.as_ref(),
                *no_reuse,
                *flush_wait_ms,
            ),
            Request::BackupList {
                server_id,
                page,
                page_size,
            } => self.backup_list(server_id, *page, *page_size),
            Request::BackupRestore {
                server_id,
                snapshot,
                to_new,
            } => self.backup_restore(server_id, snapshot, to_new.as_deref()),
            Request::BackupDelete {
                server_id,
                snapshot,
            } => self.backup_delete(server_id, snapshot),
            Request::BackupVerify {
                server_id,
                snapshot,
            } => self.backup_verify(server_id, snapshot),
            Request::BackupPrune {
                server_id,
                keep,
                older_than_secs,
                dry_run,
            } => self.backup_prune(server_id, *keep, *older_than_secs, *dry_run),

            // 握手帧只在连接前两步出现；走到这里说明客户端在已认证连接上重放
            Request::Hello { .. } | Request::Enroll { .. } | Request::Authenticate { .. } => {
                RemoteFailure::new(
                    ErrorKind::PROTOCOL,
                    format!("{} is a handshake frame", request.operation()),
                )
                .into_response()
            }
            // 协议新增帧而本端不认识：显式报协议错误，不能静默当成功。
            // `Request` 是 `#[non_exhaustive]`，对端可能比本端新。
            other => unreachable_frame(other),
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
    fn server_op(&self, request: &Request, progress: &dyn TaskObserver) -> Response {
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
            Request::ServerInstall { name, version } => {
                self.server_install(name, version, progress)
            }
            Request::ServerLink {
                target,
                name,
                workspace,
            } => match self.core.registry.link(
                std::path::Path::new(target),
                name.as_deref(),
                workspace.as_deref(),
            ) {
                Ok(outcome) => match WireServer::try_from(&outcome.server.meta) {
                    Ok(server) => Response::Linked {
                        server,
                        target: outcome.target.to_string_lossy().into_owned(),
                        linked: outcome.server.linked,
                    },
                    Err(err) => failure(ErrorKind::IO, err),
                },
                Err(err) => registry_failure(&err),
            },
            Request::ServerVariants => Response::Variants {
                items: available_variants(&self.core.jar_sources),
            },
            Request::ServerVersions { variant } => self.server_versions(variant),
            Request::ServerCreateModpack {
                workspace,
                name,
                project,
                version,
            } => self.server_create_modpack(
                workspace.as_deref(),
                name,
                project,
                version.as_deref(),
                progress,
            ),
            Request::PropsGet { server_id } => self.props_get(server_id),
            Request::PropsSet { server_id, updates } => self.props_set(server_id, updates),
            Request::SettingsSet {
                server_id,
                key,
                value,
            } => self.settings_set(server_id, key, value),
            other => unreachable_frame(other),
        }
    }

    /// Java 帧
    fn java_op(&self, request: &Request, progress: &dyn TaskObserver) -> Response {
        match request {
            Request::JavaList => self.java_list(),
            // 扫描结果直接回列表：跨机器没有「扫到几个」的渲染位置，
            // 客户端要的是「现在有哪些」
            Request::JavaScan => match self.core.java.scan(true, &mut |_| {}) {
                Ok(_) => self.java_list(),
                Err(err) => java_failure(&err),
            },
            Request::JavaInstall { major, jre, vendor } => {
                self.java_install(*major, *jre, vendor.as_deref(), progress)
            }
            Request::JavaUninstall { path } => self.java_uninstall(path),
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

    /// 删除调用方自己在名册里的记录（`remote disconnect` 的服务端一半）
    ///
    /// 只认认证结果里的 `client_id`，不接受任何参数：能删的永远只有「你自己」。
    /// 记录不存在视为成功——客户端可能重试，或名册已被管理员手工清理过；
    /// 报「找不到」只会让本地断开卡在一个已经达成的目标上。
    fn self_revoke(&self, session: &SessionInfo) -> Response {
        let client_id = session.client_id.clone();
        let revoked = match self.registry_lock.lock() {
            Ok(_guard) => self.clients.revoke(&client_id),
            Err(_) => {
                return RemoteFailure::new(ErrorKind::IO, "client registry lock is poisoned")
                    .into_response();
            }
        };
        match revoked {
            Ok(_) => {
                tracing::info!(client = %client_id, "remote client removed itself");
                Response::Ok
            }
            Err(err) => failure(ErrorKind::IO, err),
        }
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
        let revoked = match self.registry_lock.lock() {
            Ok(_guard) => self.clients.revoke(&client_id),
            Err(_) => {
                return RemoteFailure::new(ErrorKind::IO, "client registry lock is poisoned")
                    .into_response();
            }
        };
        match revoked {
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
                items.push(WireStatusEntry::failed_kind(
                    ErrorKind::NOT_FOUND,
                    format!("server not found: {id}"),
                ));
                continue;
            };
            let name = server.meta.name.as_str();
            items.push(match statuses.get(name) {
                Some(Ok(status)) => WireStatusEntry::ok(status.as_ref()),
                Some(Err(err)) => WireStatusEntry::failed(err.to_string()),
                None => WireStatusEntry::failed_kind(
                    ErrorKind::NOT_FOUND,
                    format!("no status for {name}"),
                ),
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
            // 远端帧没有这个开关；对端的 mods/ 由对端预检（同样有读取器）
            skip_mod_check: false,
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
            // 宽限来自远端帧，先按领域上限夹紧再交给 service
            grace: domain::limits::stop_grace(grace_secs),
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

    /// 日志跟随（长轮询）：等到有新行或超时
    ///
    /// 历史只回一次（`lines > 0` 时），之后客户端传 0 只拿增量；等待期间没有
    /// 新行返回空列表，客户端自行决定下一轮。断线即停：transport 层连接断开，
    /// 这个阻塞调用随线程结束，没有需要清理的订阅状态。
    fn logs_follow(&self, server_id: &str, lines: usize, timeout_ms: u64) -> Response {
        let name = match self.name_of(server_id) {
            Ok(name) => name,
            Err(response) => return response,
        };
        // 历史只在第一条请求里回：open_tail 的文件长度快照与返回的历史对齐，
        // 之后的增量由 read_new 提供，不重不漏
        let (mut watch, mut history) = if lines > 0 {
            match self.core.lifecycle.log_watch_tail(&name, lines) {
                Ok((tail, watch)) => (watch, tail),
                Err(err) => return lifecycle_failure(&err),
            }
        } else {
            match self.core.lifecycle.log_watch(&name) {
                Ok(watch) => (watch, Vec::new()),
                Err(err) => return lifecycle_failure(&err),
            }
        };
        // 等待上限由领域层夹紧：`u64::MAX` 毫秒会让 `Instant + Duration` 溢出
        let deadline = std::time::Instant::now() + domain::limits::log_wait(timeout_ms);
        loop {
            match watch.read_new() {
                Ok(batch) if !batch.is_empty() => {
                    history.extend(batch);
                    if history.len() > domain::limits::MAX_LOG_LINES {
                        history.drain(..history.len() - domain::limits::MAX_LOG_LINES);
                    }
                    return Response::LogLines { lines: history };
                }
                Ok(_) => {}
                Err(err) => return lifecycle_failure(&err),
            }
            if std::time::Instant::now() >= deadline {
                return Response::LogLines { lines: history };
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    /// 身份 → 名字：`service` 的写路径按名字寻址，远端帧按稳定身份寻址
    #[allow(clippy::result_large_err)] // 错误即 Response 帧，按帧返回是分发器的统一口径
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

    /// `server install`：在对端下载 / 安装核心
    ///
    /// 编排与本地 CLI 走同一份 `install_jar_checked`：下载、校验、（安装器类
    /// 变体的）安装器执行都在对端，客户端只拿到更新后的注册记录。
    fn server_install(&self, name: &str, version: &str, progress: &dyn TaskObserver) -> Response {
        let variant = match self.core.registry.get(name) {
            Ok(srv) => srv.meta.variant,
            Err(err) => return registry_failure(&err),
        };
        let installed = with_dispatch_runtime(|runtime| {
            runtime.block_on(self.install_core_async(name, version, &variant, progress))
        });
        match installed {
            Ok(Ok(item)) => Response::Servers { items: vec![item] },
            Ok(Err(response)) => *response,
            Err(err) => failure(ErrorKind::IO, err),
        }
    }

    /// 对端某个变体的版本列表（`server create --origin` 交互向导用）
    ///
    /// 与本地 `pick_version` 走同一份 `ServerJarSource::versions`，排序也复用
    /// 领域层的 [`ServerVersion::sort_display_order`]：两端顺序一致，用户在
    /// 远端向导里看到的同样是「新在前」。
    fn server_versions(&self, variant: &str) -> Response {
        let variant = match ServerVariant::from_id(variant.to_owned()) {
            Ok(variant) => variant,
            Err(err) => return failure(ErrorKind::INVALID, err),
        };
        let Some(source) = pick_source(&self.core.jar_sources, &variant) else {
            return RemoteFailure::new(
                ErrorKind::INVALID,
                format!("no server jar source for variant {}", variant.as_str()),
            )
            .into_response();
        };
        match with_dispatch_runtime(|runtime| runtime.block_on(source.versions())) {
            Ok(Ok(mut versions)) => {
                ServerVersion::sort_display_order(&mut versions);
                Response::Versions {
                    items: versions.iter().map(WireVersion::from).collect(),
                }
            }
            Ok(Err(err)) => failure(ErrorKind::IO, err),
            Err(err) => failure(ErrorKind::IO, err),
        }
    }

    /// 安装 / 更换对端核心（async：下载走 HTTP）
    ///
    /// `server install` 与 `server create --modpack` 共用；失败直接返回错误帧，
    /// 调用方不需要再翻译一次。
    async fn install_core_async(
        &self,
        name: &str,
        version: &str,
        variant: &ServerVariant,
        progress: &dyn TaskObserver,
    ) -> Result<WireServer, Box<Response>> {
        let Some(source) = pick_source(&self.core.jar_sources, variant) else {
            return Err(Box::new(
                RemoteFailure::new(
                    ErrorKind::INVALID,
                    format!("no server jar source for variant {}", variant.as_str()),
                )
                .into_response(),
            ));
        };
        let installer = ServerInstaller::new(source, self.core.downloader.clone());
        match self
            .core
            .lifecycle
            .install_jar_checked(
                name,
                &VersionId::new(version.to_owned()),
                &installer,
                &FileCoreInspector,
                installer_ctx(self, name, variant).as_ref(),
                Some(progress),
                &port::CancellationToken::new(),
            )
            .await
        {
            Ok(outcome) => WireServer::try_from(&outcome.server.meta)
                .map_err(|err| Box::new(failure(ErrorKind::IO, err))),
            Err(LifecycleError::InstallJar(err)) => Err(Box::new(install_failure(&err))),
            Err(err) => Err(Box::new(lifecycle_failure(&err))),
        }
    }

    /// `server create --modpack`：对端读清单 → 登记 → 装加载器核心 → 布置
    ///
    /// 与本地 `server create` 的整合包路径同一条编排（`prepare` → 登记 →
    /// 装加载器 → `apply`）。清单必须在对端读：MC 版本与加载器变体由整合包
    /// 元数据决定，客户端拿不到。暂存目录用系统临时目录：登记前还不知道
    /// 服务器目录，而清单下载只需要一个可写落点。
    fn server_create_modpack(
        &self,
        workspace: Option<&str>,
        name: &str,
        project: &str,
        version: Option<&str>,
        progress: &dyn TaskObserver,
    ) -> Response {
        match with_dispatch_runtime(|runtime| {
            runtime.block_on(async {
                // 每次创建用唯一暂存目录：多台远端命令可能并发创建整合包，
                // 共享 `temp/mcsup-modpack-staging` 会互相删暂存文件
                let staging = unique_staging_dir();
                if let Err(err) = std::fs::create_dir_all(&staging) {
                    return failure(ErrorKind::IO, err);
                }
                // 守卫保证任何提前返回都会清理暂存目录
                let _staging_guard = StagingGuard(staging.clone());
                let mut options = ModpackInstallOptions::new(&staging)
                    .progress(progress as &dyn port::ProgressSink);
                if let Some(version) = version {
                    options = options.version(version);
                }
                let cancel = port::CancellationToken::new();
                let prepared = match self.core.modpack.prepare(project, options, &cancel).await {
                    Ok(prepared) => prepared,
                    Err(err) => return modpack_failure(&err),
                };
                // 加载器与 MC 版本必须在登记前确定：注册表存加载器对应的变体，
                // 而不是 `modrinth-modpack`（后者只是「走整合包流程」的标记，
                // 没有可下载的核心来源）
                let variant = match prepared.require_loader() {
                    Ok(variant) => variant,
                    Err(err) => return modpack_failure(&err),
                };
                let minecraft = match prepared.minecraft_version() {
                    Ok(version) => version.to_owned(),
                    Err(err) => return modpack_failure(&err),
                };
                let dir = match self.core.registry.create(
                    workspace,
                    name,
                    &VersionId::new(minecraft.clone()),
                    variant.clone(),
                    None,
                ) {
                    Ok(srv) => srv.dir,
                    Err(err) => return registry_failure(&err),
                };
                // 先装核心再铺文件：失败留在「已登记」状态，重试入口是 `server install`
                if let Err(response) = self
                    .install_core_async(name, &minecraft, &variant, progress)
                    .await
                {
                    return *response;
                }
                let applied = match self
                    .core
                    .modpack
                    .apply(
                        &prepared,
                        &dir,
                        Some(progress as &dyn port::ProgressSink),
                        &cancel,
                    )
                    .await
                {
                    Ok(applied) => applied,
                    Err(err) => return modpack_failure(&err),
                };
                // `.mrpack` 暂存目录由 `StagingGuard` 在作用域结束时删除
                let updated = match self.core.registry.get(name) {
                    Ok(srv) => srv,
                    Err(err) => return registry_failure(&err),
                };
                match WireServer::try_from(&updated.meta) {
                    Ok(server) => Response::ModpackApplied {
                        server,
                        project_id: prepared.resolved.project_id.clone(),
                        version_id: prepared.resolved.version_id.clone(),
                        version_number: prepared.resolved.version_number.clone(),
                        minecraft,
                        loader: prepared
                            .index
                            .loader()
                            .map(|(loader, version)| format!("{loader} {version}")),
                        files: applied.file_count,
                        downloaded_bytes: applied.downloaded_bytes,
                        reused: applied.reused,
                        skipped_client_only: applied.skipped_client_only,
                        skipped_bytes: applied.skipped_bytes,
                        stale_hashes: applied.stale_hashes.len(),
                        blocked: applied.blocked.iter().map(wire_mod_blocked).collect(),
                    },
                    Err(err) => failure(ErrorKind::IO, err),
                }
            })
        }) {
            Ok(response) => response,
            Err(err) => failure(ErrorKind::IO, err),
        }
    }

    /// 读取对端 `server.properties` 原文
    ///
    /// 传原文而不是键值表：注释与顺序属于用户文件内容，两端用同一份
    /// `Properties::parse` 得到一致的视图。
    fn props_get(&self, server_id: &str) -> Response {
        let name = match self.name_of(server_id) {
            Ok(name) => name,
            Err(response) => return response,
        };
        let service = ServerPropsService::new(self.core.registry.clone());
        match service.read(&name) {
            Ok(report) => Response::Props {
                path: report.path.display().to_string(),
                text: report.text,
            },
            Err(err) => props_failure(&err),
        }
    }

    /// 写入对端 `server.properties` 的若干键值
    fn props_set(&self, server_id: &str, updates: &[WirePropUpdate]) -> Response {
        let name = match self.name_of(server_id) {
            Ok(name) => name,
            Err(response) => return response,
        };
        let updates: Vec<(String, String)> = updates
            .iter()
            .map(|update| (update.key.clone(), update.value.clone()))
            .collect();
        let service = ServerPropsService::new(self.core.registry.clone());
        match service.set(&name, &updates) {
            Ok(report) => Response::Props {
                path: report.path.display().to_string(),
                text: report.text,
            },
            Err(err) => props_failure(&err),
        }
    }

    /// 设置对端服务器的一项服务属性（内存 / Java / 开机自启 / 备份范围）
    fn settings_set(&self, server_id: &str, key: &str, value: &str) -> Response {
        let name = match self.name_of(server_id) {
            Ok(name) => name,
            Err(response) => return response,
        };
        let key = match SettingKey::parse(key) {
            Ok(key) => key,
            Err(err) => return failure(ErrorKind::INVALID, err),
        };
        let service =
            ServerSettingsService::new(self.core.registry.clone(), self.core.java.clone());
        match service.set(&name, key, value, &mut || {}, &mut |_| {}) {
            Ok(change) => Response::SettingsChanged {
                value: change.value,
                settings: WireSettings::from(&change.settings),
            },
            Err(err) => settings_failure(&err),
        }
    }

    /// 在对端安装 Java；返回对端登记的全部运行时
    fn java_install(
        &self,
        major: i64,
        jre: bool,
        vendor: Option<&str>,
        progress: &dyn TaskObserver,
    ) -> Response {
        let major = match JavaMajor::parse(major) {
            Ok(major) => major,
            Err(err) => return failure(ErrorKind::INVALID, err),
        };
        let package = if jre {
            JavaPackageType::Jre
        } else {
            JavaPackageType::Jdk
        };
        let vendor = vendor.map(JavaVendor::parse_fuzzy);
        let outcome = with_dispatch_runtime(|runtime| {
            runtime.block_on(self.core.java.install(
                major,
                package,
                vendor,
                Some(progress),
                &port::CancellationToken::new(),
            ))
        });
        match outcome {
            Ok(Ok(_)) => self.java_list(),
            Ok(Err(err)) => java_failure(&err),
            Err(err) => failure(ErrorKind::IO, err),
        }
    }

    /// 在对端卸载 Java（`path` 已由客户端按对端列表解析）
    fn java_uninstall(&self, path: &str) -> Response {
        match self.core.java.uninstall(std::path::Path::new(path)) {
            Ok(UninstallOutcome::RemovedManaged { path }) => Response::JavaUninstalled {
                path: path.display().to_string(),
                managed: true,
            },
            Ok(UninstallOutcome::UnregisteredSystem { path }) => Response::JavaUninstalled {
                path: path.display().to_string(),
                managed: false,
            },
            Err(err) => java_failure(&err),
        }
    }

    // ---- backup（阶段 9）：转发给与本地 CLI 同一份 BackupService ----

    fn backup_create(
        &self,
        server_id: &str,
        label: Option<&str>,
        scope: Option<&Vec<String>>,
        no_reuse: bool,
        flush_wait_ms: Option<u64>,
    ) -> Response {
        let name = match self.name_of(server_id) {
            Ok(name) => name,
            Err(response) => return response,
        };
        let options = service::CreateOptions {
            label: label.map(str::to_owned),
            scope: match scope {
                Some(entries) => {
                    match domain::backup::BackupScope::from_entries(entries.iter().cloned()) {
                        Ok(built) => Some(built),
                        Err(err) => {
                            return RemoteFailure::new(ErrorKind::INVALID, err.to_string())
                                .into_response();
                        }
                    }
                }
                None => None,
            },
            no_reuse,
            flush_wait_ms,
        };
        // RPC 请求自带超时，不由服务端令牌取消：这里给一个不会被取消的令牌，
        // 免得 agent 的备份被无关的本地取消信号打断
        match self
            .core
            .backup
            .create(&name, &options, &port::CancellationToken::new())
        {
            Ok(outcome) => {
                let snapshot = match WireSnapshot::try_from(&outcome.snapshot) {
                    Ok(wire) => wire,
                    Err(err) => return failure(ErrorKind::PROTOCOL, err),
                };
                Response::SnapshotCreated {
                    snapshot,
                    skipped: outcome.skipped,
                }
            }
            Err(err) => backup_failure(&err),
        }
    }

    fn backup_list(&self, server_id: &str, page: usize, page_size: usize) -> Response {
        let name = match self.name_of(server_id) {
            Ok(name) => name,
            Err(response) => return response,
        };
        match self
            .core
            .backup
            .list(&name, domain::backup::Page::new(page, page_size))
        {
            Ok(paged) => {
                let items = paged
                    .items
                    .iter()
                    .map(WireSnapshot::try_from)
                    .collect::<Result<Vec<_>, _>>();
                match items {
                    Ok(items) => Response::Snapshots {
                        items,
                        total: paged.total,
                        page: paged.page,
                        page_size: paged.page_size,
                    },
                    Err(err) => failure(ErrorKind::PROTOCOL, err),
                }
            }
            Err(err) => backup_failure(&err),
        }
    }

    fn backup_restore(&self, server_id: &str, snapshot: &str, to_new: Option<&str>) -> Response {
        let name = match self.name_of(server_id) {
            Ok(name) => name,
            Err(response) => return response,
        };
        let target = match to_new {
            // 名字直接拼进 `servers/<name>`：对端必须按领域规则校验，
            // 不能把 `..` / 绝对路径当成合法目标
            Some(new_name) => match domain::server::meta::ServerName::parse(new_name) {
                Ok(name) => domain::backup::RestoreTarget::NewServer { name },
                Err(err) => return failure(ErrorKind::INVALID, err),
            },
            None => domain::backup::RestoreTarget::InPlace,
        };
        match self
            .core
            .backup
            .restore(&name, snapshot, &target, &port::CancellationToken::new())
        {
            Ok(outcome) => {
                let restored = match WireSnapshot::try_from(&outcome.snapshot) {
                    Ok(wire) => wire,
                    Err(err) => return failure(ErrorKind::PROTOCOL, err),
                };
                Response::Restored {
                    snapshot: restored,
                    destination: outcome.destination.to_string_lossy().into_owned(),
                    files: outcome.files,
                    bytes: outcome.bytes,
                    safety_snapshot: outcome
                        .safety_snapshot
                        .as_ref()
                        .map(|id| id.as_str().to_owned()),
                    deleted: outcome.deleted,
                }
            }
            Err(err) => backup_failure(&err),
        }
    }

    fn backup_delete(&self, server_id: &str, snapshot: &str) -> Response {
        let name = match self.name_of(server_id) {
            Ok(name) => name,
            Err(response) => return response,
        };
        match self.core.backup.delete(&name, snapshot) {
            Ok(outcome) => {
                let deleted = match WireSnapshot::try_from(&outcome.snapshot) {
                    Ok(wire) => wire,
                    Err(err) => return failure(ErrorKind::PROTOCOL, err),
                };
                Response::SnapshotDeleted {
                    snapshot: deleted,
                    orphaned: outcome.orphaned,
                }
            }
            Err(err) => backup_failure(&err),
        }
    }

    fn backup_verify(&self, server_id: &str, snapshot: &str) -> Response {
        let name = match self.name_of(server_id) {
            Ok(name) => name,
            Err(response) => return response,
        };
        match self.core.backup.verify(&name, snapshot, &mut |_, _| ()) {
            Ok(outcome) => {
                let verified = match WireSnapshot::try_from(&outcome.snapshot) {
                    Ok(wire) => wire,
                    Err(err) => return failure(ErrorKind::PROTOCOL, err),
                };
                Response::SnapshotVerified {
                    snapshot: verified,
                    objects: outcome.objects,
                    bytes: outcome.bytes,
                    broken: outcome
                        .broken
                        .iter()
                        .map(|hash| hash.as_str().to_owned())
                        .collect(),
                }
            }
            Err(err) => backup_failure(&err),
        }
    }

    fn backup_prune(
        &self,
        server_id: &str,
        keep: Option<usize>,
        older_than_secs: Option<u64>,
        dry_run: bool,
    ) -> Response {
        let name = match self.name_of(server_id) {
            Ok(name) => name,
            Err(response) => return response,
        };
        // 只递交原始输入：冲突判定在 domain / service，不在这里择一
        let retention = domain::backup::RetentionInput {
            keep,
            older_than_secs,
        };
        match self.core.backup.prune(&name, retention, dry_run) {
            Ok(outcome) => {
                let removed = outcome
                    .removed
                    .iter()
                    .map(WireSnapshot::try_from)
                    .collect::<Result<Vec<_>, _>>();
                match removed {
                    Ok(removed) => Response::Pruned {
                        removed,
                        objects_reclaimed: outcome.objects_reclaimed,
                        bytes_reclaimed: outcome.bytes_reclaimed,
                        dry_run: outcome.dry_run,
                    },
                    Err(err) => failure(ErrorKind::PROTOCOL, err),
                }
            }
            Err(err) => backup_failure(&err),
        }
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
///
/// 消息拼**整条 source 链**：`RepositoryError::Storage` 之类只是
/// 「存储失败」的包装，真正的原因（marker 拒绝、IO 路径）在链尾。只取最外层
/// `Display`，对端就只剩一句 `storage error`，无从判断该改什么。
fn failure(kind: &'static str, err: impl std::error::Error + 'static) -> Response {
    RemoteFailure::new(kind, chain_text(&err)).into_response()
}

/// [`failure`] 的借用版本，供各类包装函数（`&T` 入参）使用
fn failure_ref(kind: &'static str, err: &(dyn std::error::Error + 'static)) -> Response {
    RemoteFailure::new(kind, chain_text(err)).into_response()
}

/// 把错误链渲染成一行：`外层: 内层: 更内层`
///
/// 各层 `Display` 都不重复内层文本（`thiserror` 的约定），因此直接拼接即可，
/// 不需要去重。
fn chain_text(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(link) = current {
        let text = link.to_string();
        if !text.is_empty() {
            parts.push(text);
        }
        current = link.source();
    }
    parts.join(": ")
}

/// 同步分发线程上跑 async 安装 / 整合包编排
///
/// `install_jar_checked` 与整合包取回都走 HTTP（async），而分发线程是同步的。
/// 远端命令是「一请求一连接一线程」，同一线程会串行处理该连接的每一条请求，
/// 因此 runtime 按线程复用一次而不是每请求重建：它不是业务状态，只是执行器
/// 资源（IO / 定时器驱动），不参与任何判定。transport 的 600s 读写超时与下载
/// 时长同一量级，不构成额外约束。
fn with_dispatch_runtime<T>(
    work: impl FnOnce(&tokio::runtime::Runtime) -> T,
) -> std::io::Result<T> {
    DISPATCH_RUNTIME.with(|cell| {
        if cell.get().is_none() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            // `set` 失败只可能是同一线程重入（不会发生）；保留首次创建的那个
            let _ = cell.set(runtime);
        }
        cell.get()
            .map(work)
            .ok_or_else(|| std::io::Error::other("dispatch runtime was not initialized"))
    })
}

thread_local! {
    /// 本线程的分发 runtime（首次使用时创建）
    static DISPATCH_RUNTIME: std::cell::OnceCell<tokio::runtime::Runtime> =
        const { std::cell::OnceCell::new() };
}

/// 一次整合包创建用的唯一暂存目录
fn unique_staging_dir() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    std::env::temp_dir().join(format!("mcsup-modpack-{}-{nanos}", std::process::id()))
}

/// 暂存目录的 Drop 守卫：提前返回也会清理
struct StagingGuard(std::path::PathBuf);

impl Drop for StagingGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 失败分类 → 线上错误词表
///
/// 分类由各层错误自己给出（`port::ErrorCategory`），这里只做词表投影，不重判：
/// 对端与本机跑同一套 `service`，两边各写一张 `match` 必然分叉——同一个错误
/// 本地给 4、经 agent 给 2，客户端无法判断该不该重试。
///
/// 线上词表比分类更细一档（`busy` / `already_exists` 都是冲突），需要区分时
/// 由调用方在投影前先摘出来。分类与词表一一对应，未知种类只在旧对端出现：
/// 旧客户端收到 `failure` / `cancelled` 会落 `Other`，其 `category()` 仍按
/// 后端失败处理，方向安全。本地 Ctrl-C 不经过这里（入口自己就能判 130），
/// 只有 agent 侧自查到取消才会走到这条路径。
fn wire_kind(category: port::ErrorCategory) -> &'static str {
    match category {
        port::ErrorCategory::NotFound => ErrorKind::NOT_FOUND,
        port::ErrorCategory::Conflict => ErrorKind::BUSY,
        port::ErrorCategory::Usage => ErrorKind::INVALID,
        port::ErrorCategory::Unauthorized => ErrorKind::UNAUTHORIZED,
        port::ErrorCategory::Failure => ErrorKind::FAILURE,
        port::ErrorCategory::Cancelled => ErrorKind::CANCELLED,
        port::ErrorCategory::Backend => ErrorKind::IO,
    }
}

fn workspace_failure(err: &WorkspaceError) -> Response {
    // 名字冲突在线上有专门取值：客户端据此提示「换个名字」，比笼统的 busy 更有用
    let kind = match err.kind() {
        Some(WorkspaceErrorKind::AlreadyExists { .. }) => ErrorKind::ALREADY_EXISTS,
        _ => wire_kind(err.category()),
    };
    failure_ref(kind, err)
}

/// 阻断原因转线格式
///
/// 只搬运事实，不搬运文案：`kind` 是稳定机器名，`detail` 是缺的 modid 或不
/// 匹配的维度说明。渲染成一句话由客户端按自己的语言做——协议层不知道也不该
/// 知道用户界面长什么样。
fn wire_mod_blocked(entry: &ModBlocked) -> WireModBlocked {
    let (kind, detail) = match &entry.blocker {
        ModBlocker::MissingDeps { mods } => ("missing_deps", mods.clone()),
        ModBlocker::ClientOnly => ("client_only", Vec::new()),
        ModBlocker::Mismatch { detail } => ("mismatch", vec![detail.clone()]),
    };
    WireModBlocked {
        file: entry.file.clone(),
        kind: kind.to_owned(),
        detail,
    }
}

fn registry_failure(err: &ServerRegistryError) -> Response {
    let kind = match err.kind() {
        Some(ServerRegistryErrorKind::AlreadyExists { .. }) => ErrorKind::ALREADY_EXISTS,
        _ => wire_kind(err.category()),
    };
    failure_ref(kind, err)
}

fn java_failure(err: &JavaError) -> Response {
    failure_ref(wire_kind(err.category()), err)
}

/// 属性读写失败 → 错误帧（按领域判定分类，不解析文案）
fn props_failure(err: &PropsError) -> Response {
    if let PropsError::Registry(inner) = err {
        return registry_failure(inner);
    }
    failure_ref(wire_kind(err.category()), err)
}

/// 服务属性写入失败 → 错误帧
fn settings_failure(err: &SettingsError) -> Response {
    if let SettingsError::Registry(inner) = err {
        return registry_failure(inner);
    }
    failure_ref(wire_kind(err.category()), err)
}

/// 整合包取回 / 布置失败 → 错误帧
fn modpack_failure(err: &ModpackError) -> Response {
    // 核心安装失败沿用生命周期分类（含 Java 选择 / 注册表判定）
    if let ModpackError::Lifecycle(inner) = err {
        return lifecycle_failure(inner);
    }
    failure_ref(wire_kind(err.category()), err)
}

fn lifecycle_failure(err: &LifecycleError) -> Response {
    failure_ref(wire_kind(err.category()), err)
}

/// 备份失败 → 错误帧：按领域判定分类，不解析文案
fn backup_failure(err: &service::BackupError) -> Response {
    failure_ref(wire_kind(err.category()), err)
}

/// 可用变体标识：来源 `variant()` 去重，保持注册顺序
///
/// 与本地 `bootstrap::available_variants` 的口径一致（去重、保序），但**不含**
/// 整合包变体：整合包没有 `ServerJarSource`，其版本必须走清单，远端向导把它
/// 留给显式的 `--modpack`，不在「变体 → 版本」两步里混入。
fn available_variants(sources: &[std::sync::Arc<dyn port::ServerJarSource>]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for source in sources {
        let id = source.variant().as_str().to_owned();
        if !seen.contains(&id) {
            seen.push(id.clone());
            out.push(id);
        }
    }
    out
}

/// 变体 → 来源；`Unknown` 兜底到 vanilla（与本地 CLI 同一规则）
fn pick_source(
    sources: &[std::sync::Arc<dyn port::ServerJarSource>],
    variant: &ServerVariant,
) -> Option<std::sync::Arc<dyn port::ServerJarSource>> {
    let target = if *variant == ServerVariant::Unknown {
        &ServerVariant::Vanilla
    } else {
        variant
    };
    sources
        .iter()
        .find(|source| &source.variant() == target)
        .cloned()
}

/// 安装器类变体需要的安装上下文（Java 选择判定在 `service`）
///
/// `installer_java_bin` 只对安装器类变体才有意义；直接可运行的来源不读上下文，
/// 因此这里只按变体判一次，不重复实现 Java 挑选规则。
fn installer_ctx(agent: &Agent, name: &str, variant: &ServerVariant) -> Option<InstallerContext> {
    if !matches!(variant, ServerVariant::Forge | ServerVariant::NeoForge) {
        return None;
    }
    let java_bin = agent
        .core
        .lifecycle
        .installer_java_bin(name, &mut || {}, &mut |_| {})
        .ok()?;
    Some(InstallerContext {
        java_bin,
        runner: std::sync::Arc::new(runtime::LocalInstallerRunner::new()),
    })
}

/// 安装失败 → 错误帧（按领域判定分类，不解析文案）
fn install_failure(err: &InstallJarError) -> Response {
    if let InstallJarError::Registry(inner) = err {
        return registry_failure(inner);
    }
    failure_ref(wire_kind(err.category()), err)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 投影后可原样还原分类：本地 4 与远端 4 必须是同一个结论
    ///
    /// 回归：投影表与 `category()` 各写一份时，同一个错误本地判 2、经 agent
    /// 判 4，客户端无从判断该不该重试。
    #[test]
    fn wire_kind_round_trips_to_the_same_category() {
        use port::{ErrorCategory, RemoteFailureKind};

        for category in [
            ErrorCategory::NotFound,
            ErrorCategory::Conflict,
            ErrorCategory::Usage,
            ErrorCategory::Unauthorized,
            ErrorCategory::Backend,
            ErrorCategory::Failure,
            ErrorCategory::Cancelled,
        ] {
            let wire = wire_kind(category);
            assert_eq!(
                RemoteFailureKind::from_wire(wire).category(),
                category,
                "{wire}"
            );
        }
    }

    /// 名字冲突单独摘出 `already_exists`，其余照分类投影
    ///
    /// 对端据此提示「换个名字」；若笼统落 `busy`，客户端只会说「有冲突」。
    #[test]
    fn registry_failure_keeps_already_exists_on_the_wire() {
        let err = ServerRegistryError::AlreadyExists("web".to_owned());
        match registry_failure(&err) {
            Response::Error { kind, .. } => assert_eq!(kind, ErrorKind::ALREADY_EXISTS),
            other => panic!("应为错误帧：{other:?}"),
        }

        let missing = ServerRegistryError::NotFound("web".to_owned());
        match registry_failure(&missing) {
            Response::Error { kind, .. } => assert_eq!(kind, ErrorKind::NOT_FOUND),
            other => panic!("应为错误帧：{other:?}"),
        }
    }

    /// 链尾原因必须带到对端，不能被外层包装吞掉
    ///
    /// 回归：只取最外层 `Display` 时，对端只收到一句 `storage error`，
    /// 真正的原因（marker 拒绝、IO 路径）全丢。
    #[test]
    fn chain_text_keeps_inner_reason() {
        let inner = std::io::Error::other("marker /ws/foo: refusing to purge");
        let outer = port::RepositoryError::Storage(Box::new(inner));
        let text = chain_text(&outer);
        assert!(text.contains("storage error"), "{text}");
        assert!(text.contains("refusing to purge"), "{text}");
        // 原因只出现一次：`Storage` 的 Display 不再内嵌内层文本
        assert_eq!(text.matches("refusing to purge").count(), 1, "{text}");
    }

    /// 多层链按 `外层: 内层` 拼接，顺序稳定
    #[test]
    fn chain_text_joins_layers_in_order() {
        #[derive(Debug, thiserror::Error)]
        #[error("outer layer")]
        struct Outer(#[source] std::io::Error);

        let err = Outer(std::io::Error::other("inner layer"));
        assert_eq!(chain_text(&err), "outer layer: inner layer");
    }
}
