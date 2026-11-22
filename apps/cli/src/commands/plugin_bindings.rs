//! 服务器启停时插件的跟随编排
//!
//! 「谁绑定哪台服务器」「最后一台停止才收插件」的判定在 service 层
//! （[`PluginBindings`]）；这里只做宿主拉起、启停副作用、提示与失败回滚。
//!
//! 插件起不来不阻止服务器启动：桥接坏掉时，服务器本身仍应能跑起来。

use crate::commands::plugin_host;
use crate::output::Printer;
use mcsup::bootstrap::Mcsup;
use mcsup::domain::plugin::PluginId;
use mcsup::service::{PluginBindings, PluginService, ServerLifecycleService};
use std::collections::BTreeSet;
use std::time::Duration;

/// 停止插件的宽限：网络类插件需要时间收尾
const STOP_GRACE: Duration = Duration::from_secs(10);

/// 手动启动前对绑定插件的门禁
///
/// 绑定插件的生命周期归服务器：`plugin start` / `plugin install` 这类命令
/// 不能把绑定的服务器都停着的插件拉起来，否则服务器没跑、桥却空转。
pub(crate) enum StartGate {
    /// 没有绑定，或至少有一台绑定服务器在运行
    Allowed,
    /// 有绑定，但绑定服务器一台都没在运行
    AllStopped { servers: Vec<String> },
}

impl StartGate {
    /// 是不是「绑定服务器都停着」
    pub(crate) fn is_blocked(&self) -> bool {
        matches!(self, Self::AllStopped { .. })
    }
}

/// 与一组插件绑定的服务器生命周期联动
///
/// 由 `server start / stop / restart` 在组合根处创建。绑定关系每次调用都
/// 重新读取，因此两条命令之间改过配置也能立即生效。
pub(crate) struct BoundPlugins {
    plugins: PluginService,
    lifecycle: ServerLifecycleService,
}

impl BoundPlugins {
    /// 从组合根取插件与生命周期服务
    pub(crate) fn new(mcsup: &Mcsup) -> Self {
        Self {
            plugins: mcsup.plugins.clone(),
            lifecycle: mcsup.lifecycle.clone(),
        }
    }

    /// 启动服务器前先拉起绑定到它的插件，返回本次真正拉起的插件
    ///
    /// 单个插件失败只告警并继续：返回的列表只含成功启动的，供回滚使用。
    /// 返回顺序按 id 排序，便于日志对照与测试。
    pub(crate) fn start_bound(&self, server: &str, printer: Printer) -> Vec<PluginId> {
        let Some(bindings) = self.bindings(printer) else {
            return Vec::new();
        };
        let mut targets = bindings
            .bound_to(server)
            .map(|binding| binding.id.clone())
            .collect::<Vec<_>>();
        targets.sort();

        let mut started = Vec::new();
        for id in targets {
            if self.is_running(&id) {
                continue;
            }
            let result = plugin_host::ensure().and_then(|()| {
                self.plugins
                    .start(&id)
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            });
            match result {
                Ok(()) => {
                    printer.print_note(&format!("已启动插件 {id}（绑定服务器 {server}）"));
                    started.push(id);
                }
                Err(err) => {
                    printer.print_warning(&format!("插件 {id} 启动失败，服务器继续启动：{err}"));
                }
            }
        }
        started
    }

    /// 服务器启动失败：收掉本次刚拉起、且已无服务器可服务的插件
    ///
    /// 只动 [`Self::start_bound`] 返回的插件：启动前就在跑的插件不是这次的
    /// 副作用，不该替用户停掉。
    pub(crate) fn rollback(&self, server: &str, started: &[PluginId], printer: Printer) {
        if started.is_empty() {
            return;
        }
        let Some(bindings) = self.bindings(printer) else {
            return;
        };
        let running = self.running_servers(&bindings, server);
        for id in bindings.stop_candidates(server, &running) {
            if started.contains(&id) {
                self.stop_one(&id, printer);
            }
        }
    }

    /// 服务器停下后，收掉已经没有服务器可服务的插件
    pub(crate) fn stop_unused(&self, server: &str, printer: Printer) {
        let Some(bindings) = self.bindings(printer) else {
            return;
        };
        let running = self.running_servers(&bindings, server);
        for id in bindings.stop_candidates(server, &running) {
            // 宿主没起来或本来就没跑：没有可停的东西
            if self.is_running(&id) {
                self.stop_one(&id, printer);
            }
        }
    }

    /// 启动门禁：判定插件是否该运行，并收掉「绑定服务器都停着却还在跑」的残留
    ///
    /// 绑定关系按插件单独读取（[`PluginService::binding_of`]），不受启用状态
    /// 影响：全新安装的插件在向导结束前还没启用，但门禁仍要看得出它绑定到
    /// 哪台服务器。读不出绑定清单时放行：宁可让用户手动启动，也不因为一次
    /// 诊断失败挡住他。
    pub(crate) fn enforce_gate(&self, id: &PluginId, printer: Printer) -> StartGate {
        let bindings = match self.plugins.binding_of(id) {
            Ok(bindings) => bindings,
            // 插件根本不存在：让后续命令自己报 NotFound，不额外吓人
            Err(mcsup::service::PluginError::NotFound(_)) => return StartGate::Allowed,
            Err(err) => {
                printer.print_warning(&format!(
                    "读取插件 {id} 的服务器绑定失败，跳过启停门禁：{err}"
                ));
                return StartGate::Allowed;
            }
        };
        let bound = bindings
            .active
            .first()
            .map(|binding| binding.servers.clone())
            .unwrap_or_default();
        let Some(servers) = bindings.idle_servers(id, &self.alive_servers(&bound)) else {
            return StartGate::Allowed;
        };
        // 残留进程（例如升级时 service 恢复了运行状态）也要对齐：不留空转的桥
        if self.is_running(id) {
            self.stop_one(id, printer);
        }
        StartGate::AllStopped { servers }
    }

    /// 读取绑定清单；读不出来只告警，返回 `None` 让调用方跳过编排
    ///
    /// `skipped` 里的插件（待授权、descriptor 或配置损坏）不参与编排，
    /// 但要让用户知道它这次没被带上，否则只会在「桥怎么没起来」时才发现。
    fn bindings(&self, printer: Printer) -> Option<PluginBindings> {
        match self.plugins.bindings() {
            Ok(bindings) => {
                for (id, reason) in &bindings.skipped {
                    printer.print_warning(&format!("插件 {id} 未参与服务器启停：{reason}"));
                }
                Some(bindings)
            }
            Err(err) => {
                printer.print_warning(&format!("读取插件绑定失败，跳过插件编排：{err}"));
                None
            }
        }
    }

    /// 绑定集合里除 `exclude` 外仍在运行的服务器
    ///
    /// 查询失败按「仍在运行」处理：宁可让插件多跑一会儿，也不在服务器还活着
    /// 时把它的桥收掉。
    fn running_servers(&self, bindings: &PluginBindings, exclude: &str) -> BTreeSet<String> {
        let names = bindings
            .active
            .iter()
            .flat_map(|binding| binding.servers.iter())
            .filter(|name| name.as_str() != exclude)
            .cloned()
            .collect::<BTreeSet<_>>();
        self.alive_servers(&names)
    }

    /// 从候选服务器里筛出真正在运行的
    ///
    /// 查询失败同样按「仍在运行」处理，避免把还活着的服务器的桥收掉。
    fn alive_servers(&self, names: &BTreeSet<String>) -> BTreeSet<String> {
        names
            .iter()
            .filter(|name| match self.lifecycle.status(name) {
                Ok(Some(status)) => status.status.is_alive(),
                Ok(None) => false,
                Err(_) => true,
            })
            .cloned()
            .collect()
    }

    fn is_running(&self, id: &PluginId) -> bool {
        self.plugins
            .status(id)
            .is_ok_and(|status| status.session.is_some())
    }

    fn stop_one(&self, id: &PluginId, printer: Printer) {
        match self.plugins.stop(id, STOP_GRACE) {
            Ok(()) => printer.print_note(&format!("已停止插件 {id}（绑定的服务器都已停止）")),
            Err(err) => printer.print_warning(&format!("停止插件 {id} 失败：{err}")),
        }
    }
}
