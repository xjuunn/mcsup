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
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 停止插件的宽限：网络类插件需要时间收尾
const STOP_GRACE: Duration = Duration::from_secs(10);

/// 常驻宿主检查绑定服务器的间隔
///
/// 与 `console_events` 的 200 ms 轮询不同，这里只探 supervisor 状态；1 秒足以
/// 让「服务器停了、桥还挂着」的窗口短到用户感知不到，又不会为每台服务器每秒
/// 发多次 IPC。
const WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// 新会话的启动宽限
///
/// `server start` 先拉插件再拉服务器，中间那几秒「插件在跑、服务器还没进
/// `starting`」与「服务器已经停了」在探测结果里长得一模一样。会话身份能区分
/// 大部分情况（见 [`IdleWatch`]），但插件也可能是上一次启动留下的；宽限就是
/// 给这段窗口的余量。宽限过后服务器仍没起来，仍旧收掉：不留空转的桥。
const STARTUP_GRACE: Duration = Duration::from_secs(90);

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
            let result = plugin_host::ensure(printer).and_then(|()| {
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

/// 宿主被换成当前构建之后，按启动门禁恢复它原本在跑的插件
///
/// 换宿主是升级的必要动作，但插件是被换宿主牵连停掉的，用户没要求停。恢复
/// 必须重新过一遍门禁：绑定服务器都已经停了的插件不该借着这次重启被拉起来，
/// 否则服务器没跑、桥却空转——与 `plugin start` 同一口径。
pub(crate) fn restore_after_restart(ids: &[PluginId], printer: Printer) {
    if ids.is_empty() {
        return;
    }
    let mcsup = match crate::bootstrap::open_mcsup() {
        Ok(mcsup) => mcsup,
        Err(err) => {
            printer.print_warning(&format!("读取插件绑定失败，未恢复插件：{err}"));
            return;
        }
    };
    let bound = BoundPlugins::new(&mcsup);
    for id in ids {
        if let StartGate::AllStopped { servers } = bound.enforce_gate(id, printer) {
            printer.print_note(&format!(
                "插件 {id} 未恢复：绑定的服务器都已停止（{}）",
                servers.join("、")
            ));
            continue;
        }
        match mcsup.plugins.start(id) {
            Ok(info) => printer.print_note(&format!("已恢复插件 {id}（pid {}）", info.pid)),
            Err(err) => printer.print_warning(&format!("恢复插件 {id} 失败：{err}")),
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
            let sessions = running_sessions(host.as_ref(), &bindings);
            for id in watch.next(&bindings, &alive, &sessions) {
                stop_if_running(host.as_ref(), &id);
            }
        }
    });
}

/// 「绑定服务器全停」的连续观察
///
/// 观察单位是 `(插件, 会话)`，不是插件 id：
///
/// 1. `server stop` 收掉插件、`server start` 再拉起一个新进程，中间那一秒里
///    「插件在跑、服务器还没起来」和「服务器已经停了」在探测结果里完全一样。
///    只有会话身份能把两者分开——旧会话的观察不能替新会话背书；
/// 2. 必须在更早的轮次里真的见过它绑定的服务器在运行（`armed`），否则一次
///    启动窗口就会被当成停机；
/// 3. 同一结论要连续两轮（`idle`），避开状态探测的抖动；
/// 4. 没见过服务器在跑的会话先给启动宽限（`grace`），宽限过后仍然全停就收，
///    不让桥空转。
#[derive(Debug)]
struct IdleWatch {
    /// 见过绑定服务器运行的插件 → 当时的会话
    armed: BTreeMap<PluginId, String>,
    /// 上一轮所有绑定服务器都没在运行的插件 → 当时的会话
    idle: BTreeMap<PluginId, String>,
    /// 会话第一次被判全停的时刻，用于启动宽限
    off_since: BTreeMap<PluginId, Instant>,
    /// 启动宽限
    grace: Duration,
}

impl Default for IdleWatch {
    fn default() -> Self {
        Self {
            armed: BTreeMap::new(),
            idle: BTreeMap::new(),
            off_since: BTreeMap::new(),
            grace: STARTUP_GRACE,
        }
    }
}

impl IdleWatch {
    /// 本轮应当停止的插件；同时滚动观察状态
    ///
    /// `sessions` 是这批插件当前的活跃会话（`插件 → 会话 id`）；没有会话的插件
    /// 没有可停的东西，观察一并清零。
    fn next(
        &mut self,
        bindings: &PluginBindings,
        alive: &BTreeSet<String>,
        sessions: &BTreeMap<PluginId, String>,
    ) -> Vec<PluginId> {
        // 插件被卸载后不会再来：清掉残留的 id，长驻宿主的观察集合不无限增长
        self.armed
            .retain(|id, _| bindings.active.iter().any(|binding| &binding.id == id));
        self.off_since
            .retain(|id, _| bindings.active.iter().any(|binding| &binding.id == id));
        let mut idle = BTreeMap::new();
        for binding in &bindings.active {
            let Some(session) = sessions.get(&binding.id) else {
                self.armed.remove(&binding.id);
                self.off_since.remove(&binding.id);
                continue;
            };
            if binding.servers.iter().any(|name| alive.contains(name)) {
                self.armed.insert(binding.id.clone(), session.clone());
                self.off_since.remove(&binding.id);
                continue;
            }
            if self.armed.get(&binding.id) == Some(session) {
                self.off_since.remove(&binding.id);
            } else {
                // 这个会话还没服务过任何绑定服务器：可能是启动窗口
                let since = *self
                    .off_since
                    .entry(binding.id.clone())
                    .or_insert_with(Instant::now);
                if since.elapsed() < self.grace {
                    continue;
                }
            }
            idle.insert(binding.id.clone(), session.clone());
        }
        let stop = idle
            .iter()
            .filter(|(id, session)| self.idle.get(*id) == Some(*session))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        self.idle = idle;
        stop
    }
}

/// 这批插件当前的活跃会话
///
/// 状态读不出来按「没有会话」处理：下一轮再试，不在崩溃退避期误判。
fn running_sessions(
    host: &dyn PluginSupervisor,
    bindings: &PluginBindings,
) -> BTreeMap<PluginId, String> {
    bindings
        .active
        .iter()
        .filter_map(|binding| {
            let session = host.status(&binding.id).ok()?.session?;
            Some((binding.id.clone(), session.session_id))
        })
        .collect()
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

    /// 活跃会话：`(插件, 会话 id)` 对
    fn sessions(pairs: &[(&str, &str)]) -> BTreeMap<PluginId, String> {
        pairs
            .iter()
            .map(|(id, session)| (PluginId::parse(id).expect("id"), (*session).to_owned()))
            .collect()
    }

    /// 先见过服务器在跑，再连续两轮全停才收；单轮抖动不动手
    #[test]
    fn stops_after_seeing_the_server_alive_and_two_idle_rounds() {
        let (mut watch, bindings) = watch(vec![binding("chat-bridge", &["test1"])]);
        let live = sessions(&[("chat-bridge", "s1")]);
        assert!(watch.next(&bindings, &names(&["test1"]), &live).is_empty());
        assert!(watch.next(&bindings, &names(&["test1"]), &live).is_empty());
        // 服务器刚进入终态：第一轮只记下空闲
        assert!(watch.next(&bindings, &names(&[]), &live).is_empty());
        // 第二轮仍然是全停 → 收插件
        assert_eq!(
            watch.next(&bindings, &names(&[]), &live),
            vec![PluginId::parse("chat-bridge").expect("id")]
        );
        // 状态保持空闲时每轮都会返回：调用方按「是否真有会话」幂等，瞬时
        // stop 失败下一轮还会重试
        assert_eq!(watch.next(&bindings, &names(&[]), &live).len(), 1);
    }

    /// 从没见过服务器在跑（`server start` 的启动窗口）不算停机
    #[test]
    fn never_stops_a_plugin_that_has_not_served_a_live_server() {
        let (mut watch, bindings) = watch(vec![binding("chat-bridge", &["test1"])]);
        let live = sessions(&[("chat-bridge", "s1")]);
        for _ in 0..4 {
            assert!(watch.next(&bindings, &names(&[]), &live).is_empty());
        }
    }

    /// 宽限过后服务器仍没起来：照样收掉，不留空转的桥
    #[test]
    fn stops_an_unserved_plugin_once_the_startup_grace_expires() {
        let (mut watch, bindings) = watch(vec![binding("chat-bridge", &["test1"])]);
        watch.grace = Duration::ZERO;
        let live = sessions(&[("chat-bridge", "s1")]);
        assert!(watch.next(&bindings, &names(&[]), &live).is_empty());
        assert_eq!(watch.next(&bindings, &names(&[]), &live).len(), 1);
    }

    /// 多服务器绑定：任一台还在跑就不收
    #[test]
    fn keeps_the_plugin_while_any_bound_server_runs() {
        let (mut watch, bindings) = watch(vec![binding("chat-bridge", &["test1", "test2"])]);
        let live = sessions(&[("chat-bridge", "s1")]);
        assert!(watch.next(&bindings, &names(&["test2"]), &live).is_empty());
        assert!(
            watch
                .next(&bindings, &names(&["test1", "test2"]), &live)
                .is_empty()
        );
        for _ in 0..3 {
            assert!(watch.next(&bindings, &names(&["test2"]), &live).is_empty());
        }
    }

    /// 服务器又起来了：连续空闲被打断，重新计数
    #[test]
    fn a_returning_server_resets_the_idle_streak() {
        let (mut watch, bindings) = watch(vec![binding("chat-bridge", &["test1"])]);
        let live = sessions(&[("chat-bridge", "s1")]);
        assert!(watch.next(&bindings, &names(&["test1"]), &live).is_empty());
        assert!(watch.next(&bindings, &names(&[]), &live).is_empty());
        assert!(watch.next(&bindings, &names(&["test1"]), &live).is_empty());
        assert!(watch.next(&bindings, &names(&[]), &live).is_empty());
        assert_eq!(watch.next(&bindings, &names(&[]), &live).len(), 1);
    }

    /// `server stop` 收掉插件后 `server start` 拉起的新会话不能被旧观察误杀
    ///
    /// 回归：观察集合只记插件 id 时，停机留下的「已武装」会替新会话背书，
    /// `server start` 拉起的插件在服务器进入 `starting` 之前就被收掉。
    #[test]
    fn a_new_session_after_stop_is_not_killed_by_the_previous_observation() {
        let (mut watch, bindings) = watch(vec![binding("chat-bridge", &["test1"])]);
        let first = sessions(&[("chat-bridge", "s1")]);
        assert!(watch.next(&bindings, &names(&["test1"]), &first).is_empty());
        assert!(watch.next(&bindings, &names(&[]), &first).is_empty());
        assert_eq!(watch.next(&bindings, &names(&[]), &first).len(), 1);

        // 插件收掉：没有会话，观察清零
        assert!(
            watch
                .next(&bindings, &names(&[]), &sessions(&[]))
                .is_empty()
        );
        // 服务器启动前插件已被拉起：新会话处在启动宽限里，不能收
        let second = sessions(&[("chat-bridge", "s2")]);
        assert!(watch.next(&bindings, &names(&[]), &second).is_empty());
        // 服务器起来了：新会话被武装
        assert!(
            watch
                .next(&bindings, &names(&["test1"]), &second)
                .is_empty()
        );
        assert!(watch.next(&bindings, &names(&[]), &second).is_empty());
        assert_eq!(watch.next(&bindings, &names(&[]), &second).len(), 1);
    }
}
