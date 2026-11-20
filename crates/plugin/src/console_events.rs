//! 控制台行 → 领域事件
//!
//! 常驻宿主进程按服务器跟随 `console.log`，把每一行做成两类事实：
//!
//! - `console:line`：原始行，给日志转发类插件；
//! - `chat:message` / `player:joined` / `player:left`：解析后的结构化事件。
//!
//! 跟随只在宿主进程里做。`mcsup server start` 拉起 supervisor 后立刻退出，
//! 在 CLI 进程里跟随日志的线程会随进程一起消失；把泵放在常驻宿主才与
//! 「事件只用于触发、不补发」的语义一致。
//!
//! 游标用 [`ConsoleIo::tail`] 的字节偏移：宿主重启后从头读并不是灾难，
//! 记录在内存里的游标也不会与文件长度脱节。首次看到某台服务器时先跳到
//! 文件末尾，避免把历史日志当成新事件重放。

use crate::event::EventBus;
use crate::host::PluginHost;
use crate::rfc3339;
use domain::plugin::event::parse_console_line;
use domain::server::event::DomainEvent;
use domain::server::meta::ServerName;
use port::plugin::ConsoleIo;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// 轮询间隔
///
/// 控制台行本身没有推送通道，只能轮询文件；200 ms 在「聊天延迟可感知」与
/// 「不必为繁忙服务器每秒唤醒十几次」之间取中。每轮只读新增行，空闲时是
/// 一次 `read` 加一次 `metadata` 的成本。
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 单轮单服务器读取上限
///
/// 服务器刚启动时会瞬间刷出大量行；设上限避免一次轮询把所有订阅者堵住，
/// 剩下的留到下一轮。
const MAX_LINES_PER_POLL: usize = 256;

/// 各服务器的读取游标（字节偏移）
#[derive(Default)]
struct Cursors {
    map: Mutex<BTreeMap<ServerName, u64>>,
}

/// 启动后台跟随线程
///
/// 线程持有宿主的弱引用：宿主被丢弃或进入关停流程时线程自行退出，不延长
/// 宿主生命周期，也不需要在 `Drop` 里等待。
pub fn spawn(host: &Arc<PluginHost>) {
    let weak = Arc::downgrade(host);
    std::thread::spawn(move || {
        let cursors = Cursors::default();
        loop {
            std::thread::sleep(POLL_INTERVAL);
            let Some(host) = weak.upgrade() else {
                return;
            };
            if host.is_shutting_down() {
                return;
            }
            cursors.poll(&host);
        }
    });
}

impl Cursors {
    fn poll(&self, host: &PluginHost) {
        let Some(services) = host.services() else {
            return;
        };
        let (Some(server_read), Some(console)) = (&services.server_read, &services.console) else {
            return;
        };
        let Ok(servers) = server_read.list() else {
            // 读取注册表失败是瞬时故障：保留游标，下一轮再试。
            return;
        };
        let mut cursors = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        for summary in servers {
            let name = summary.name.clone();
            if summary.status.is_alive() {
                let cursor = match cursors.get(&name) {
                    Some(cursor) => *cursor,
                    // 第一次看到运行中的服务器：从末尾开始，历史日志不是事件。
                    None => match console.tail(&name, None, 1) {
                        Ok(page) => {
                            cursors.insert(name.clone(), page.next);
                            page.next
                        }
                        Err(_) => continue,
                    },
                };
                read_new(host.bus(), console.as_ref(), &name, cursor, &mut cursors);
            } else if cursors.contains_key(&name) {
                // 进程已进入终态：把停机输出读完再丢弃游标，否则最后几行
                // （往往正是崩溃原因）永远发不出去。
                let cursor = cursors.get(&name).copied().unwrap_or_default();
                read_new(host.bus(), console.as_ref(), &name, cursor, &mut cursors);
                cursors.remove(&name);
            }
        }
    }
}

fn read_new(
    bus: &EventBus,
    console: &dyn ConsoleIo,
    server: &ServerName,
    cursor: u64,
    cursors: &mut BTreeMap<ServerName, u64>,
) {
    let Ok(page) = console.tail(server, Some(cursor), MAX_LINES_PER_POLL) else {
        return;
    };
    cursors.insert(server.clone(), page.next);
    for line in page.lines {
        publish(bus, server.as_str(), &line.text);
    }
}

/// 一行控制台输出 → 原始事件 + 解析出的结构化事件
///
/// 两件事都可能没有订阅者，`EventBus::publish` 自己会丢弃；这里不做投递
/// 前判定，否则「谁订阅了什么」会渗进解析路径。
fn publish(bus: &EventBus, server: &str, line: &str) {
    let at = rfc3339::now();
    if let Some(event) = parse_console_line(server, &at, line) {
        bus.publish(event);
    }
    bus.publish(DomainEvent::ConsoleLine {
        server: server.to_owned(),
        line: line.to_owned(),
        at,
    });
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "测试夹具，失败直接 panic 才有用"
    )]

    use super::*;
    use domain::plugin::PluginId;
    use domain::plugin::event::Subscription;
    use serde_json::Value;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    type Frame = (Subscription, Option<String>, Value);

    #[derive(Default)]
    struct Collect {
        frames: Mutex<Vec<Frame>>,
    }

    impl crate::event::EventDelivery for Collect {
        fn deliver(&self, kind: Subscription, _at: &str, server: Option<&str>, data: Value) {
            self.frames
                .lock()
                .expect("lock")
                .push((kind, server.map(ToOwned::to_owned), data));
        }
    }

    fn bus_with(subs: &[Subscription]) -> (EventBus, Arc<Collect>) {
        let bus = EventBus::new();
        let target = Arc::new(Collect::default());
        bus.subscribe(
            PluginId::parse("demo").expect("id"),
            subs.iter().copied().collect::<BTreeSet<_>>(),
            target.clone(),
        );
        (bus, target)
    }

    #[test]
    fn chat_line_produces_chat_and_console_events() {
        let (bus, target) = bus_with(&[Subscription::ChatMessage, Subscription::ConsoleLine]);
        publish(
            &bus,
            "survival",
            "[09:30:00] [Server thread/INFO]: <Steve> 有人吗",
        );
        let frames = target.frames.lock().expect("lock");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0, Subscription::ChatMessage);
        assert_eq!(frames[0].1.as_deref(), Some("survival"));
        assert_eq!(frames[0].2["from"], "Steve");
        assert_eq!(frames[0].2["text"], "有人吗");
        assert_eq!(frames[1].0, Subscription::ConsoleLine);
        assert_eq!(
            frames[1].2["line"],
            "[09:30:00] [Server thread/INFO]: <Steve> 有人吗"
        );
    }

    #[test]
    fn join_and_leave_lines_are_structured() {
        let (bus, target) = bus_with(&[Subscription::PlayerJoined, Subscription::PlayerLeft]);
        publish(
            &bus,
            "survival",
            "[09:30:00] [Server thread/INFO]: Steve joined the game",
        );
        publish(
            &bus,
            "survival",
            "[09:31:00] [Server thread/INFO]: Steve left the game",
        );
        let frames = target.frames.lock().expect("lock");
        assert_eq!(frames[0].0, Subscription::PlayerJoined);
        assert_eq!(frames[0].2["player"], "Steve");
        assert_eq!(frames[1].0, Subscription::PlayerLeft);
        assert_eq!(frames[1].2["player"], "Steve");
    }

    #[test]
    fn plain_lines_only_produce_console_events() {
        let (bus, target) = bus_with(&[Subscription::ChatMessage, Subscription::ConsoleLine]);
        publish(
            &bus,
            "survival",
            "[09:30:00] [Server thread/INFO]: Done (1.2s)!",
        );
        let frames = target.frames.lock().expect("lock");
        assert_eq!(frames.len(), 1, "非聊天行不应伪造 chat 事件");
        assert_eq!(frames[0].0, Subscription::ConsoleLine);
    }

    #[test]
    fn console_line_is_not_delivered_to_chat_only_subscribers() {
        let (bus, target) = bus_with(&[Subscription::ChatMessage]);
        publish(
            &bus,
            "survival",
            "[09:30:00] [Server thread/INFO]: Done (1.2s)!",
        );
        assert!(target.frames.lock().expect("lock").is_empty());
    }
}
