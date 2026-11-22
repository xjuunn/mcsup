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
use mcsup::plugin::PluginHost;
use mcsup::port::plugin::PluginSupervisor;
use mcsup::service::{PluginBindings, PluginService, ServerLifecycleService};
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// 停止插件的宽限：网络类插件需要时间收尾
const STOP_GRACE: Duration = Duration::from_secs(10);

/// 常驻宿主检查绑定服务器的间隔
///
/// 与 `console_events` 的 200 ms 轮询不同，这里只探 supervisor 状态；1 秒足以
/// 让「服务器停了、桥还挂着」的窗口短到用户感知不到，又不会为每台服务器每秒
/// 发多次 IPC。
const WATCH_INTERVAL: Duration = Duration::from_secs(1);

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

/// 常驻宿主的绑定看门狗：绑定服务器全停后收掉插件
///
/// `server start/stop/restart` 的编排只在 CLI 命令里执行；控制台 `stop`、游戏
/// 内 `/stop`、服务端崩溃退出都不经过 CLI，插件宿主是唯一常驻的观察者。不补
/// 这一步，桥就会在服务器停掉后一直空转（见 §18.1）。
///
/// 线程持有宿主的弱引用：宿主关停即自行退出，不在 `Drop` 里等待。
pub(crate) fn spawn_watch(
    host: &Arc<PluginHost>,
    plugins: PluginService,
    lifecycle: ServerLifecycleService,
) {
    let weak = Arc::downgrade(host);
    std::thread::spawn(move || {
        let mut watch = IdleWatch::default();
        loop {
            std::thread::sleep(WATCH_INTERVAL);
            let Some(host) = weak.upgrade() else {
                return;
            };
            if host.is_shutting_down() {
                return;
            }
            let Ok(bindings) = plugins.bindings() else {
                // 读不出绑定清单是瞬时故障：保留观察状态，下一轮再试
                continue;
            };
            let alive = alive_bound(&lifecycle, &bindings);
            for id in watch.next(&bindings, &alive) {
                stop_if_running(host.as_ref(), &id);
            }
        }
    });
}

/// 「绑定服务器全停」的连续观察
///
/// 两道闸：
///
/// 1. 必须在更早的轮次里真的见过它绑定的服务器在运行（`armed`）：`server
///    start` 先拉插件再拉服务器，启动窗口里「插件在跑、服务器还是 stopped」
///    不是停机；
/// 2. 同一结论要连续两轮（`idle`），避开状态探测的抖动。
#[derive(Debug, Default)]
struct IdleWatch {
    /// 见过绑定服务器运行的插件
    armed: BTreeSet<PluginId>,
    /// 上一轮所有绑定服务器都没在运行的插件
    idle: BTreeSet<PluginId>,
}

impl IdleWatch {
    /// 本轮应当停止的插件；同时滚动观察状态
    fn next(&mut self, bindings: &PluginBindings, alive: &BTreeSet<String>) -> Vec<PluginId> {
        // 插件被卸载后不会再来：清掉残留的 id，长驻宿主的观察集合不无限增长
        self.armed
            .retain(|id| bindings.active.iter().any(|binding| &binding.id == id));
        let mut idle = BTreeSet::new();
        for binding in &bindings.active {
            if binding.servers.iter().any(|name| alive.contains(name)) {
                self.armed.insert(binding.id.clone());
            } else if self.armed.contains(&binding.id) {
                idle.insert(binding.id.clone());
            }
        }
        let stop = idle.intersection(&self.idle).cloned().collect::<Vec<_>>();
        self.idle = idle;
        stop
    }
}

/// 绑定集合里当前在运行的服务器
///
/// 一次批量探测取全部状态；探测失败按「仍在运行」处理，宁可晚一点收桥，也不
/// 在服务器还活着时停掉插件。已注销的绑定服务器不在结果里，按停止处理。
fn alive_bound(lifecycle: &ServerLifecycleService, bindings: &PluginBindings) -> BTreeSet<String> {
    let names = bindings
        .active
        .iter()
        .flat_map(|binding| binding.servers.iter().cloned())
        .collect::<HashSet<_>>();
    if names.is_empty() {
        return BTreeSet::new();
    }
    let Ok(statuses) = lifecycle.statuses_for(&names) else {
        return names.into_iter().collect();
    };
    statuses
        .into_iter()
        .filter_map(|(name, result)| match result {
            Ok(Some(status)) if !status.status.is_alive() => None,
            Ok(None) => None,
            // 单台探测报错：按仍在运行处理，不误收
            Ok(Some(_)) | Err(_) => Some(name),
        })
        .collect()
}

/// 只在插件确实有会话时下发 stop；状态读不出来就等下一轮
fn stop_if_running(host: &dyn PluginSupervisor, id: &PluginId) {
    let running = host.status(id).is_ok_and(|status| status.session.is_some());
    if !running {
        return;
    }
    if let Err(err) = host.stop(id, STOP_GRACE) {
        tracing::warn!(plugin = %id, error = %err, "stop plugin after its bound servers exited");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "测试夹具，失败直接 panic 才有用")]

    use super::*;
    use mcsup::service::PluginBinding;

    fn binding(id: &str, servers: &[&str]) -> PluginBinding {
        PluginBinding {
            id: PluginId::parse(id).expect("id"),
            servers: servers.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    fn watch(bindings: Vec<PluginBinding>) -> (IdleWatch, PluginBindings) {
        (
            IdleWatch::default(),
            PluginBindings {
                active: bindings,
                skipped: Vec::new(),
            },
        )
    }

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// 先见过服务器在跑，再连续两轮全停才收；单轮抖动不动手
    #[test]
    fn stops_after_seeing_the_server_alive_and_two_idle_rounds() {
        let (mut watch, bindings) = watch(vec![binding("chat-bridge", &["test1"])]);
        assert!(watch.next(&bindings, &names(&["test1"])).is_empty());
        assert!(watch.next(&bindings, &names(&["test1"])).is_empty());
        // 服务器刚进入终态：第一轮只记下空闲
        assert!(watch.next(&bindings, &names(&[])).is_empty());
        // 第二轮仍然是全停 → 收插件
        assert_eq!(
            watch.next(&bindings, &names(&[])),
            vec![PluginId::parse("chat-bridge").expect("id")]
        );
        // 状态保持空闲时每轮都会返回：调用方按「是否真有会话」幂等，瞬时
        // stop 失败下一轮还会重试
        assert_eq!(watch.next(&bindings, &names(&[])).len(), 1);
    }

    /// 从没见过服务器在跑（`server start` 的启动窗口）不算停机
    #[test]
    fn never_stops_a_plugin_that_has_not_served_a_live_server() {
        let (mut watch, bindings) = watch(vec![binding("chat-bridge", &["test1"])]);
        for _ in 0..4 {
            assert!(watch.next(&bindings, &names(&[])).is_empty());
        }
    }

    /// 多服务器绑定：任一台还在跑就不收
    #[test]
    fn keeps_the_plugin_while_any_bound_server_runs() {
        let (mut watch, bindings) = watch(vec![binding("chat-bridge", &["test1", "test2"])]);
        assert!(watch.next(&bindings, &names(&["test2"])).is_empty());
        assert!(
            watch
                .next(&bindings, &names(&["test1", "test2"]))
                .is_empty()
        );
        for _ in 0..3 {
            assert!(watch.next(&bindings, &names(&["test2"])).is_empty());
        }
    }

    /// 服务器又起来了：连续空闲被打断，重新计数
    #[test]
    fn a_returning_server_resets_the_idle_streak() {
        let (mut watch, bindings) = watch(vec![binding("chat-bridge", &["test1"])]);
        assert!(watch.next(&bindings, &names(&["test1"])).is_empty());
        assert!(watch.next(&bindings, &names(&[])).is_empty());
        assert!(watch.next(&bindings, &names(&["test1"])).is_empty());
        assert!(watch.next(&bindings, &names(&[])).is_empty());
        assert_eq!(watch.next(&bindings, &names(&[])).len(), 1);
    }
}
