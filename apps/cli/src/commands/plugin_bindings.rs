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
        let mut names = BTreeSet::new();
        for binding in &bindings.active {
            for name in &binding.servers {
                if name != exclude {
                    names.insert(name.clone());
                }
            }
        }
        names.retain(|name| match self.lifecycle.status(name) {
            Ok(Some(status)) => status.status.is_alive(),
            Ok(None) => false,
            Err(_) => true,
        });
        names
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
