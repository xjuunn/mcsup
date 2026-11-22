//! 事件扇出：`DomainEvent` → 订阅者事件帧
//!
//! 事件只用于触发，不持久化、不补发。宿主未运行时事件直接丢弃，因此事件
//! 驱动插件应声明 `resident: true`。
//!
//! 扇出按订阅过滤，投递失败（队列满、会话已死）只丢弃该条，不影响业务。
//! 序号由接收方（会话）维护，因为 `seq` 是「会话内」单调递增，不是全局。

use crate::rfc3339;
use domain::plugin::PluginId;
use domain::plugin::event::{self, Subscription};
use domain::server::event::DomainEvent;
use protocol::plugin::EventFrame;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, Weak};

/// 接收事件的一方（通常是插件会话）
pub trait EventDelivery: Send + Sync {
    /// 投递一条事件；`at` 为 RFC3339 UTC
    fn deliver(&self, kind: Subscription, at: &str, server: Option<&str>, data: Value);
}

struct Entry {
    plugin: PluginId,
    subscriptions: BTreeSet<Subscription>,
    target: Weak<dyn EventDelivery>,
}

/// 事件总线
///
/// 克隆共享同一份订阅表；`[禁止]` 把它做成全局单例，装配由门面完成。
#[derive(Clone, Default)]
pub struct EventBus {
    entries: Arc<Mutex<Vec<Entry>>>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.entries.lock().map_or(0, |e| e.len());
        f.debug_struct("EventBus")
            .field("subscribers", &count)
            .finish()
    }
}

impl EventBus {
    /// 新建空总线
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个订阅者
    ///
    /// 同一个插件重复注册（重启）时替换旧条目；`target` 用弱引用持有，
    /// 会话结束后自动失效。
    #[allow(clippy::needless_pass_by_value)] // 调用方交出订阅者所有权，总线只留弱引用
    pub fn subscribe(
        &self,
        plugin: PluginId,
        subscriptions: BTreeSet<Subscription>,
        target: Arc<dyn EventDelivery>,
    ) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|entry| entry.plugin != plugin);
            entries.push(Entry {
                plugin,
                subscriptions,
                target: Arc::downgrade(&target),
            });
        }
    }

    /// 取消订阅
    pub fn unsubscribe(&self, plugin: &PluginId) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|entry| &entry.plugin != plugin);
        }
    }

    /// 发布一条领域事件
    ///
    /// 不可订阅的事件（平台自己的 jar 下载）直接忽略。
    // 与 `port::EventSink::publish` 同口径：按值接收，内部按引用分发
    #[allow(clippy::needless_pass_by_value)]
    pub fn publish(&self, event: DomainEvent) {
        if !event::is_subscribable(&event) {
            return;
        }
        let kind = event::subscription_of(&event);
        let at = rfc3339::now();
        let at = event_at(&event).unwrap_or(&at).to_owned();
        let server = event_server(&event).map(ToOwned::to_owned);
        let data = event_payload(&event);
        let targets: Vec<Arc<dyn EventDelivery>> = {
            let Ok(mut entries) = self.entries.lock() else {
                return;
            };
            entries.retain(|entry| entry.target.strong_count() > 0);
            entries
                .iter()
                .filter(|entry| entry.subscriptions.contains(&kind))
                .filter_map(|entry| entry.target.upgrade())
                .collect()
        };
        // 投递不持有订阅表锁：一个慢插件（写超时前最多阻塞 10 s）不能
        // 把其他订阅者、订阅/退订一起冻住。
        for target in targets {
            target.deliver(kind, &at, server.as_deref(), data.clone());
        }
    }

    /// 当前订阅者数量（测试与指标用）
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.len())
    }
}

/// `port::EventSink` 适配器：把 `service` 发布的事实转成插件事件
#[derive(Debug, Clone)]
pub struct EventSinkAdapter {
    bus: EventBus,
}

impl EventSinkAdapter {
    /// 包装总线
    #[must_use]
    pub fn new(bus: EventBus) -> Self {
        Self { bus }
    }

    /// 底层总线（宿主注册订阅者用）
    #[must_use]
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }
}

impl port::EventSink for EventSinkAdapter {
    fn publish(&self, event: DomainEvent) {
        self.bus.publish(event);
    }
}

/// 事件信封里的 `server` 字段
#[must_use]
pub fn event_server(event: &DomainEvent) -> Option<&str> {
    match event {
        DomainEvent::ChatMessage { server, .. }
        | DomainEvent::PlayerJoined { server, .. }
        | DomainEvent::PlayerLeft { server, .. }
        | DomainEvent::ConsoleLine { server, .. } => Some(server),
        DomainEvent::ServerStarting { name }
        | DomainEvent::ServerStarted { name }
        | DomainEvent::ServerStopped { name }
        | DomainEvent::ServerCrashed { name, .. }
        | DomainEvent::BackupCompleted { name, .. }
        | DomainEvent::BackupFailed { name, .. } => Some(name),
        // 非穷尽枚举：jar 下载与未来变体都没有服务器归属，按「无 server」处理。
        _ => None,
    }
}

/// 聊天事件自带时间；其余用发布时刻
#[must_use]
pub fn event_at(event: &DomainEvent) -> Option<&str> {
    match event {
        DomainEvent::ChatMessage { at, .. } | DomainEvent::ConsoleLine { at, .. } => Some(at),
        _ => None,
    }
}

/// 事件数据（信封之外的部分）
///
/// 字段名按 `docs/plugins.md` 固定：聊天用 `from` / `text` / `at`，服务器
/// 事件用 `name`。线格式一旦发布就不再改名。
#[must_use]
pub fn event_payload(event: &DomainEvent) -> Value {
    match event {
        DomainEvent::ChatMessage { from, text, at, .. } => {
            json!({ "from": from, "text": text, "at": at })
        }
        DomainEvent::PlayerJoined { player, .. } | DomainEvent::PlayerLeft { player, .. } => {
            json!({ "player": player })
        }
        DomainEvent::ConsoleLine { line, at, .. } => json!({ "line": line, "at": at }),
        DomainEvent::ServerStarting { name }
        | DomainEvent::ServerStarted { name }
        | DomainEvent::ServerStopped { name } => json!({ "name": name }),
        DomainEvent::ServerCrashed { name, exit_code } => {
            json!({ "name": name, "exit_code": exit_code })
        }
        DomainEvent::BackupCompleted {
            name,
            snapshot,
            added_bytes,
        } => json!({ "name": name, "snapshot": snapshot, "added_bytes": added_bytes }),
        DomainEvent::BackupFailed { name, reason } => json!({ "name": name, "reason": reason }),
        DomainEvent::JarDownloadCompleted { .. } | DomainEvent::JarDownloadFailed { .. } => {
            Value::Null
        }
        // 非穷尽枚举：未登记的变体不向插件投影，避免把内部信息泄露出去。
        _ => Value::Null,
    }
}

/// 由事件常量生成事件帧（会话负责补 `seq`）
#[must_use]
pub fn event_frame(
    seq: u64,
    kind: Subscription,
    at: &str,
    server: Option<&str>,
    data: Value,
) -> EventFrame {
    EventFrame {
        seq,
        kind: kind.as_str().to_owned(),
        at: at.to_owned(),
        server: server.map(ToOwned::to_owned),
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use port::EventSink as _;
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    type Frame = (Subscription, String, Option<String>, Value);

    #[derive(Default)]
    struct Collect {
        frames: Mutex<Vec<Frame>>,
    }

    impl EventDelivery for Collect {
        fn deliver(&self, kind: Subscription, at: &str, server: Option<&str>, data: Value) {
            self.frames.lock().expect("lock").push((
                kind,
                at.to_owned(),
                server.map(ToOwned::to_owned),
                data,
            ));
        }
    }

    fn plugin() -> PluginId {
        PluginId::parse("demo").expect("id")
    }

    #[test]
    fn only_subscribed_events_are_delivered() {
        let bus = EventBus::new();
        let target = Arc::new(Collect::default());
        bus.subscribe(
            plugin(),
            [Subscription::ChatMessage].into_iter().collect(),
            target.clone(),
        );
        bus.publish(DomainEvent::ChatMessage {
            server: "survival".to_owned(),
            from: "Steve".to_owned(),
            text: "hi".to_owned(),
            at: "2026-11-18T09:30:00Z".to_owned(),
        });
        bus.publish(DomainEvent::ServerStarted {
            name: "survival".to_owned(),
        });
        bus.publish(DomainEvent::JarDownloadCompleted {
            version: domain::server::version::VersionId::new("1.21.1"),
        });
        let frames = target.frames.lock().expect("lock");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, Subscription::ChatMessage);
        assert_eq!(frames[0].1, "2026-11-18T09:30:00Z");
        assert_eq!(frames[0].2.as_deref(), Some("survival"));
        assert_eq!(frames[0].3["from"], "Steve");
    }

    #[test]
    fn resubscribe_replaces_old_target() {
        let bus = EventBus::new();
        let first = Arc::new(Collect::default());
        let second = Arc::new(Collect::default());
        let subs: BTreeSet<Subscription> = [Subscription::ServerStarted].into_iter().collect();
        bus.subscribe(plugin(), subs.clone(), first.clone());
        bus.subscribe(plugin(), subs, second.clone());
        assert_eq!(bus.subscriber_count(), 1);
        bus.publish(DomainEvent::ServerStarted {
            name: "survival".to_owned(),
        });
        assert_eq!(first.frames.lock().expect("lock").len(), 0);
        assert_eq!(second.frames.lock().expect("lock").len(), 1);
    }

    #[test]
    fn dropping_target_prunes_entry() {
        let bus = EventBus::new();
        let target = Arc::new(Collect::default());
        bus.subscribe(
            plugin(),
            [Subscription::ServerStarted].into_iter().collect(),
            target,
        );
        bus.publish(DomainEvent::ServerStarted {
            name: "survival".to_owned(),
        });
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn sink_adapter_forwards_events() {
        let bus = EventBus::new();
        let target = Arc::new(Collect::default());
        bus.subscribe(
            plugin(),
            [Subscription::PlayerJoined].into_iter().collect(),
            target.clone(),
        );
        let sink = EventSinkAdapter::new(bus);
        sink.publish(DomainEvent::PlayerJoined {
            server: "survival".to_owned(),
            player: "Alex".to_owned(),
        });
        let frames = target.frames.lock().expect("lock");
        assert_eq!(frames[0].3["player"], "Alex");
    }
}
