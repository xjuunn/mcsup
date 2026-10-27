//! 路由后端：按服务器来源把 supervisor 调用分派到本地或对应远端
//!
//! `service` 只持有一个 `Arc<dyn Backend>`，不感知执行位置（架构边界）。
//! 「这条调用该去哪台机器」因此必须在这一层回答：路由器持有一个本地
//! supervisor 与全部远端的 [`RemoteSupervisor`]，按服务器 id 在组合服务器
//! 仓库里查来源（[`port::ServerRepository::list_sourced`] 的远端标注），
//! 再转发给对应一侧。
//!
//! # 判定归属
//!
//! 这里只做「在哪里执行」的查表分派；「能不能执行」（状态机、权限）仍在
//! `domain` / `service`——被路由到的一侧各自跑同一套判定，路由器不重复。
//!
//! # 失败语义
//!
//! 目标 id 不在任何一侧的注册表里 → [`SupervisorError::Unreachable`]（调用
//! 方把它当「服务器不存在」渲染）；远端不可达 → 该次调用失败，本地不代跑——
//! 「本地帮远端查状态」会返回错误的事实（本机当然没在跑），比报「连不上」
//! 更有误导性。

use crate::remote::supervisor::RemoteSupervisor;
use domain::server::meta::ServerId;
use port::{
    Origin, RemoteName, ServerRef, StopOutcome, StopSpec, SupervisorError, SupervisorPort,
    SupervisorStartOutcome, SupervisorStartRequest, SupervisorStatus,
};
use std::collections::HashMap;
use std::sync::Arc;

/// 本地 + 远端的 supervisor 路由表
pub struct RoutingSupervisor {
    local: Arc<dyn SupervisorPort>,
    /// 远端名 → 该远端的 supervisor
    remotes: HashMap<RemoteName, RemoteSupervisor>,
    /// 组合服务器仓库：id → 来源的唯一真相源
    servers: Arc<dyn port::ServerRepository>,
}

impl std::fmt::Debug for RoutingSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingSupervisor")
            .field("remotes", &self.remotes.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl RoutingSupervisor {
    /// 装配：本地 supervisor + 远端表 + 组合服务器仓库（查来源用）
    ///
    /// 服务器列表必须注入**组合**仓库（含远端镜像），否则查不到远端条目、
    /// 全部落到本地——错位比缺位更难排查，装配侧务必用与 `service` 同一个
    /// 仓库实例。
    #[must_use]
    pub fn new(
        local: Arc<dyn SupervisorPort>,
        remotes: HashMap<RemoteName, RemoteSupervisor>,
        servers: Arc<dyn port::ServerRepository>,
    ) -> Self {
        Self {
            local,
            remotes,
            servers,
        }
    }

    /// 按 id 解析目标：`Err` 表示该 id 不属于任何已接入一侧
    fn resolve(&self, id: &ServerId) -> Result<&dyn SupervisorPort, SupervisorError> {
        let sourced = self
            .servers
            .list_sourced()
            .map_err(|err| SupervisorError::Unreachable(err.to_string()))?;
        let origin = sourced
            .into_iter()
            .find(|entry| entry.value.id == *id)
            .map(|entry| entry.origin)
            .ok_or_else(|| {
                SupervisorError::Unreachable(format!("no such server: {}", id.as_str()))
            })?;
        match origin {
            Origin::Local => Ok(self.local.as_ref()),
            Origin::Remote(name) => self
                .remotes
                .get(&name)
                .map(|supervisor| supervisor as &dyn SupervisorPort)
                .ok_or_else(|| {
                    SupervisorError::Unreachable(format!(
                        "server {} belongs to remote '{}', which is not reachable",
                        id.as_str(),
                        name.as_str()
                    ))
                }),
        }
    }
}

impl SupervisorPort for RoutingSupervisor {
    fn start(
        &self,
        request: SupervisorStartRequest,
    ) -> Result<SupervisorStartOutcome, SupervisorError> {
        self.resolve(&request.server_id)?.start(request)
    }

    fn stop(&self, server: &ServerRef<'_>, spec: StopSpec) -> Result<StopOutcome, SupervisorError> {
        self.resolve(server.id)?.stop(server, spec)
    }

    fn status(&self, server: &ServerRef<'_>) -> Result<Option<SupervisorStatus>, SupervisorError> {
        self.resolve(server.id)?.status(server)
    }

    fn status_many(
        &self,
        servers: &[ServerRef<'_>],
    ) -> Vec<Result<Option<SupervisorStatus>, SupervisorError>> {
        // 先查一次来源，把请求按目标分桶；每桶一次调用，远端仍是单次批量 RPC
        let sourced = match self.servers.list_sourced() {
            Ok(sourced) => sourced,
            Err(err) => {
                return servers
                    .iter()
                    .map(|_| Err(SupervisorError::Unreachable(err.to_string())))
                    .collect();
            }
        };

        // 桶键即来源：本地 / 某台远端
        let mut buckets: Vec<(Origin, Vec<usize>)> = Vec::new();
        // 解析失败的位置可能不止一个：每个槽都要拿到「不存在」而不是
        // 兜底的 `Protocol("missing slot")`
        let mut resolve_errs: Vec<(usize, SupervisorError)> = Vec::new();
        for (index, server) in servers.iter().enumerate() {
            let origin = sourced
                .iter()
                .find(|entry| entry.value.id == *server.id)
                .map(|entry| entry.origin.clone());
            match origin {
                Some(origin) => {
                    // 同目标的并入同一桶
                    if let Some((_, indexes)) =
                        buckets
                            .iter_mut()
                            .find(|(existing, _)| match (existing, &origin) {
                                (Origin::Local, Origin::Local) => true,
                                (Origin::Remote(a), Origin::Remote(b)) => a == b,
                                _ => false,
                            })
                    {
                        indexes.push(index);
                    } else {
                        buckets.push((origin, vec![index]));
                    }
                }
                None => {
                    resolve_errs.push((
                        index,
                        SupervisorError::Unreachable(format!(
                            "no such server: {}",
                            server.id.as_str()
                        )),
                    ));
                }
            }
        }

        let mut out: Vec<Option<Result<Option<SupervisorStatus>, SupervisorError>>> =
            (0..servers.len()).map(|_| None).collect();
        for (origin, indexes) in &buckets {
            let refs: Vec<ServerRef<'_>> = indexes.iter().map(|&i| servers[i]).collect();
            let results = match origin {
                Origin::Local => self.local.status_many(&refs),
                Origin::Remote(name) => match self.remotes.get(name) {
                    Some(supervisor) => supervisor.status_many(&refs),
                    None => refs
                        .iter()
                        .map(|_| {
                            Err(SupervisorError::Unreachable(format!(
                                "remote '{}' is not reachable",
                                name.as_str()
                            )))
                        })
                        .collect(),
                },
            };
            for (position, result) in results.into_iter().enumerate() {
                out[indexes[position]] = Some(result);
            }
        }
        for (index, err) in resolve_errs {
            out[index] = Some(Err(err));
        }
        out.into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| Err(SupervisorError::Protocol("missing slot".to_owned())))
            })
            .collect()
    }

    fn send_line(&self, server: &ServerRef<'_>, line: &str) -> Result<(), SupervisorError> {
        self.resolve(server.id)?.send_line(server, line)
    }

    fn shutdown(&self, server: &ServerRef<'_>) -> Result<(), SupervisorError> {
        self.resolve(server.id)?.shutdown(server)
    }
}
