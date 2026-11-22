//! 插件 ↔ 服务器绑定
//!
//! descriptor 的 `bind.servers` 声明「去哪些配置路径读服务器名」；这里把它与
//! 插件当前配置合起来，回答两个问题：
//!
//! - 某台服务器启动时，哪些插件应当先起来；
//! - 某台服务器停止后，哪些插件已经没有任何服务器可以服务、该收掉。
//!
//! 只有纯判定，不含副作用：启停动作由调用方（CLI 组合根）执行。service 层
//! 不知道插件宿主是否可达，也不该替调用方决定失败时是回滚还是继续。

use crate::plugin::{PluginError, PluginService};
use domain::plugin::PluginId;
use std::collections::BTreeSet;

/// 一个插件绑定的服务器集合（可多台，任一台在跑就应保持运行）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginBinding {
    /// 插件 id
    pub id: PluginId,
    /// 绑定的服务器名
    pub servers: BTreeSet<String>,
}

impl PluginBinding {
    /// 是否绑定到某台服务器
    #[must_use]
    pub fn is_bound_to(&self, server: &str) -> bool {
        self.servers.contains(server)
    }
}

/// 所有启用插件的绑定清单
///
/// `skipped` 记录算不出绑定的插件（descriptor 或配置坏了）：它们不能参与
/// 服务器启停编排，但也不该让整台服务器起不来，调用方可以据此告警。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginBindings {
    /// 绑定非空的插件
    pub active: Vec<PluginBinding>,
    /// 跳过计算的插件及原因
    pub skipped: Vec<(PluginId, String)>,
}

impl PluginBindings {
    /// 绑定到某台服务器的插件
    pub fn bound_to<'a>(&'a self, server: &'a str) -> impl Iterator<Item = &'a PluginBinding> {
        self.active.iter().filter(move |b| b.is_bound_to(server))
    }

    /// 插件绑定的服务器：一台都没在运行则返回它们，否则 `None`
    ///
    /// 用于「手动启动前」的门禁：绑定插件的生命周期归服务器，绑定的服务器都
    /// 停着时不该被 `plugin start` / `plugin install` 拉起来。判定只看传入的
    /// 清单；调用方用 [`PluginService::binding_of`] 构造单插件清单，因此不受
    /// 「已启用」状态影响。
    #[must_use]
    pub fn idle_servers(&self, id: &PluginId, running: &BTreeSet<String>) -> Option<Vec<String>> {
        let binding = self.active.iter().find(|binding| &binding.id == id)?;
        if binding.servers.iter().any(|name| running.contains(name)) {
            return None;
        }
        Some(binding.servers.iter().cloned().collect())
    }

    /// 某台服务器停止后应当停止的插件
    ///
    /// 条件：绑定了 `stopped`，且绑定的其它服务器都没有在运行。`running` 里
    /// 出现 `stopped` 也不影响判定——调用方在停机后采样，顺序不该改变结论。
    #[must_use]
    pub fn stop_candidates(&self, stopped: &str, running: &BTreeSet<String>) -> Vec<PluginId> {
        self.active
            .iter()
            .filter(|binding| binding.is_bound_to(stopped))
            .filter(|binding| {
                binding
                    .servers
                    .iter()
                    .all(|name| name == stopped || !running.contains(name))
            })
            .map(|binding| binding.id.clone())
            .collect()
    }
}

impl PluginService {
    /// 单个插件的绑定清单，**不看启用 / 待授权状态**
    ///
    /// 手动启动路径（`plugin install` / `start` 等）会先把未启用的插件启用再
    /// 拉起，所以门禁不能只读 [`Self::bindings`]：那份清单故意排除了未启用
    /// 的插件，会让全新安装的插件在「绑定服务器已停」时漏过门禁。
    ///
    /// # Errors
    ///
    /// 记录不存在、descriptor 非法或配置读取失败时返回 [`PluginError`]
    pub fn binding_of(&self, id: &PluginId) -> Result<PluginBindings, PluginError> {
        let servers = self.bound_servers(id)?;
        let mut out = PluginBindings::default();
        if !servers.is_empty() {
            out.active.push(PluginBinding {
                id: id.clone(),
                servers,
            });
        }
        Ok(out)
    }

    /// 插件绑定到哪些服务器
    ///
    /// 服务器级插件（manifest `scope: server`）天然只绑定它自己的服务器；
    /// 机器级插件按 descriptor 的 `bind.servers` 路径从配置里读。
    ///
    /// # Errors
    ///
    /// 记录不存在、descriptor 非法或配置读取失败时返回 [`PluginError`]
    pub fn bound_servers(&self, id: &PluginId) -> Result<BTreeSet<String>, PluginError> {
        let record = self.record(id)?;
        if let Some(server) = &record.server {
            return Ok(BTreeSet::from([server.to_string()]));
        }
        let descriptor = self.descriptor(id)?;
        let config = self.config_get(id)?;
        let values = config.get("values").cloned().unwrap_or_default();
        Ok(descriptor.bound_servers(&values))
    }

    /// 所有启用插件的绑定清单
    ///
    /// 只收集「可以启动」的插件：禁用、拉黑、隔离与待授权的插件不会随服务器
    /// 自动启停，待授权的记入 `skipped` 便于提示。
    ///
    /// 例外是「待授权但**已经在跑**」：`plugin revoke` 会把记录打到
    /// `needs_approval`，进程却还活着。这类必须留在 `active` 里，否则绑定
    /// 的服务器全停之后没有任何路径会去收掉它，桥进程继续占端口。
    ///
    /// # Errors
    ///
    /// 插件记录列表读取失败时返回 [`PluginError`]
    pub fn bindings(&self) -> Result<PluginBindings, PluginError> {
        let mut out = PluginBindings::default();
        for record in self.list()? {
            if !record.enabled || record.blocked || record.quarantined {
                continue;
            }
            if record.needs_approval() && !self.is_running(&record.id) {
                out.skipped.push((record.id, "needs approval".to_owned()));
                continue;
            }
            match self.bound_servers(&record.id) {
                Ok(servers) if !servers.is_empty() => out.active.push(PluginBinding {
                    id: record.id,
                    servers,
                }),
                Ok(_) => {}
                Err(err) => out.skipped.push((record.id, err.to_string())),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn binding(id: &str, servers: &[&str]) -> PluginBinding {
        PluginBinding {
            id: PluginId::parse(id).expect("id"),
            servers: servers.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn id(raw: &str) -> PluginId {
        PluginId::parse(raw).expect("id")
    }

    #[test]
    fn single_server_binding_stops_when_that_server_stops() {
        let bindings = PluginBindings {
            active: vec![binding("chat-bridge", &["test1"])],
            skipped: Vec::new(),
        };
        assert_eq!(
            bindings.stop_candidates("test1", &BTreeSet::new()),
            vec![id("chat-bridge")]
        );
        // 没绑定这台服务器的插件不受影响
        assert!(
            bindings
                .stop_candidates("other", &BTreeSet::new())
                .is_empty()
        );
    }

    #[test]
    fn multi_server_binding_survives_while_any_bound_server_runs() {
        let bindings = PluginBindings {
            active: vec![binding("chat-bridge", &["test1", "test2"])],
            skipped: Vec::new(),
        };
        let running: BTreeSet<String> = ["test2".to_owned()].into();
        assert!(bindings.stop_candidates("test1", &running).is_empty());

        // 最后一台也停了才收插件
        assert_eq!(
            bindings.stop_candidates("test2", &BTreeSet::new()),
            vec![id("chat-bridge")]
        );
    }

    #[test]
    fn stopped_server_in_the_running_set_does_not_keep_the_plugin() {
        let bindings = PluginBindings {
            active: vec![binding("chat-bridge", &["test1", "test2"])],
            skipped: Vec::new(),
        };
        let running: BTreeSet<String> = ["test1".to_owned(), "test2".to_owned()].into();
        assert!(bindings.stop_candidates("test1", &running).is_empty());
        // 只剩 stopped 自己在 running 里：绑定集合里没有别的活服务器 → 停
        let only_stopped: BTreeSet<String> = ["test1".to_owned()].into();
        assert_eq!(bindings.stop_candidates("test1", &only_stopped).len(), 1);
    }

    #[test]
    fn bound_to_filters_by_server() {
        let bindings = PluginBindings {
            active: vec![
                binding("chat-bridge", &["test1"]),
                binding("other", &["test2"]),
            ],
            skipped: Vec::new(),
        };
        let ids = bindings
            .bound_to("test1")
            .map(|b| b.id.to_string())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["chat-bridge"]);
    }

    #[test]
    fn idle_servers_reports_bindings_without_a_running_server() {
        let bindings = PluginBindings {
            active: vec![binding("chat-bridge", &["test1", "test2"])],
            skipped: Vec::new(),
        };
        // 两台都没在跑：插件不该被手动拉起
        assert_eq!(
            bindings.idle_servers(&id("chat-bridge"), &BTreeSet::new()),
            Some(vec!["test1".to_owned(), "test2".to_owned()])
        );
        // 任一台在跑就允许启动
        let running: BTreeSet<String> = ["test2".to_owned()].into();
        assert_eq!(bindings.idle_servers(&id("chat-bridge"), &running), None);
        // 没有绑定的插件不受服务器生命周期约束
        assert_eq!(bindings.idle_servers(&id("other"), &BTreeSet::new()), None);
    }
}
