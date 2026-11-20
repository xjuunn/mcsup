//! 事件目录：10 个订阅名、所需能力与领域事件映射
//!
//! 事件只用于「触发」：不持久化、不补发。需要可靠性的插件用 query 拉状态。
//! `console:line` 默认不订阅——繁忙服务器每秒数百行，默认订阅会把宿主日志
//! 管道打满。

use crate::plugin::capability::Capability;
use crate::server::event::DomainEvent;
use std::collections::BTreeSet;
use std::fmt;

/// 事件类别（订阅名）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Subscription {
    /// 游戏内聊天
    ChatMessage,
    /// 玩家加入
    PlayerJoined,
    /// 玩家离开
    PlayerLeft,
    /// 原始控制台行（默认不订阅）
    ConsoleLine,
    /// 服务器开始启动
    ServerStarting,
    /// 服务器就绪
    ServerStarted,
    /// 服务器停机
    ServerStopped,
    /// 服务器崩溃
    ServerCrashed,
    /// 备份完成
    BackupCompleted,
    /// 备份失败
    BackupFailed,
}

impl Subscription {
    /// 全部 10 项
    pub const ALL: [Self; 10] = [
        Self::ChatMessage,
        Self::PlayerJoined,
        Self::PlayerLeft,
        Self::ConsoleLine,
        Self::ServerStarting,
        Self::ServerStarted,
        Self::ServerStopped,
        Self::ServerCrashed,
        Self::BackupCompleted,
        Self::BackupFailed,
    ];

    /// 线上 `kind` 文本
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChatMessage => "chat:message",
            Self::PlayerJoined => "player:joined",
            Self::PlayerLeft => "player:left",
            Self::ConsoleLine => "console:line",
            Self::ServerStarting => "server:starting",
            Self::ServerStarted => "server:started",
            Self::ServerStopped => "server:stopped",
            Self::ServerCrashed => "server:crashed",
            Self::BackupCompleted => "backup:completed",
            Self::BackupFailed => "backup:failed",
        }
    }

    /// 解析订阅名
    ///
    /// # Errors
    ///
    /// 未知名字返回 [`UnknownSubscription`]
    pub fn parse(raw: &str) -> Result<Self, UnknownSubscription> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == raw)
            .ok_or_else(|| UnknownSubscription {
                raw: raw.to_owned(),
            })
    }

    /// 投递该事件需要的能力
    #[must_use]
    pub const fn required_capability(self) -> Capability {
        match self {
            Self::ChatMessage | Self::PlayerJoined | Self::PlayerLeft | Self::ConsoleLine => {
                Capability::ConsoleRead
            }
            Self::ServerStarting
            | Self::ServerStarted
            | Self::ServerStopped
            | Self::ServerCrashed
            | Self::BackupCompleted
            | Self::BackupFailed => Capability::Events,
        }
    }

    /// 是否默认订阅
    #[must_use]
    pub const fn default_enabled(self) -> bool {
        !matches!(self, Self::ConsoleLine)
    }
}

impl fmt::Display for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 未知订阅名
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown subscription: {raw}")]
pub struct UnknownSubscription {
    /// 原始输入
    pub raw: String,
}

/// 解析订阅列表并去重
///
/// # Errors
///
/// 任一项未知时返回该项错误
pub fn parse_all(raw: &[String]) -> Result<BTreeSet<Subscription>, UnknownSubscription> {
    raw.iter()
        .map(|item| Subscription::parse(item))
        .collect::<Result<BTreeSet<_>, _>>()
}

/// 领域事件 → 订阅名
#[must_use]
pub fn subscription_of(event: &DomainEvent) -> Subscription {
    match event {
        DomainEvent::ChatMessage { .. } => Subscription::ChatMessage,
        DomainEvent::PlayerJoined { .. } => Subscription::PlayerJoined,
        DomainEvent::PlayerLeft { .. } => Subscription::PlayerLeft,
        DomainEvent::ServerStarting { .. } => Subscription::ServerStarting,
        DomainEvent::ServerStarted { .. } => Subscription::ServerStarted,
        DomainEvent::ServerStopped { .. } => Subscription::ServerStopped,
        DomainEvent::ServerCrashed { .. } => Subscription::ServerCrashed,
        DomainEvent::BackupCompleted { .. } => Subscription::BackupCompleted,
        DomainEvent::BackupFailed { .. } => Subscription::BackupFailed,
        // 下载事件不在插件事件目录里：插件无法对「平台自己的 jar 下载」做
        // 任何有意义的反应，投递只会放大噪声。
        DomainEvent::JarDownloadCompleted { .. } | DomainEvent::JarDownloadFailed { .. } => {
            Subscription::ConsoleLine
        }
        DomainEvent::ConsoleLine { .. } => Subscription::ConsoleLine,
    }
}

/// 该事件是否属于插件可订阅目录
///
/// 返回 `false` 时宿主不投递；调用方不得把下载事件硬塞给插件。
#[must_use]
pub fn is_subscribable(event: &DomainEvent) -> bool {
    !matches!(
        event,
        DomainEvent::JarDownloadCompleted { .. } | DomainEvent::JarDownloadFailed { .. }
    )
}

/// 控制台行解析入口
///
/// 实现是 [`crate::server::chat::parse_line`]：解析规则属于服务端控制台，
/// 不因为「第一个消费者是插件」就搬进插件模块。这里只是给宿主转发路径一个
/// 稳定的调用点。
#[must_use]
pub fn parse_console_line(server: &str, at: &str, line: &str) -> Option<DomainEvent> {
    crate::server::chat::parse_line(server, at, line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_ten_unique_subscriptions() {
        assert_eq!(Subscription::ALL.len(), 10);
        let names: BTreeSet<&str> = Subscription::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(names.len(), 10);
        for kind in Subscription::ALL {
            assert_eq!(Subscription::parse(kind.as_str()), Ok(kind));
        }
        assert!(Subscription::parse("chat").is_err());
        assert!(Subscription::parse("Chat:Message").is_err());
    }

    #[test]
    fn console_requires_console_read_and_events_requires_events() {
        assert_eq!(
            Subscription::ChatMessage.required_capability(),
            Capability::ConsoleRead
        );
        assert_eq!(
            Subscription::ServerCrashed.required_capability(),
            Capability::Events
        );
        assert!(!Subscription::ConsoleLine.default_enabled());
        assert!(Subscription::ChatMessage.default_enabled());
    }

    #[test]
    fn domain_events_map_to_subscriptions() {
        let cases = [
            (
                DomainEvent::ServerStarted {
                    name: "survival".to_owned(),
                },
                Subscription::ServerStarted,
            ),
            (
                DomainEvent::ServerCrashed {
                    name: "survival".to_owned(),
                    exit_code: Some(1),
                },
                Subscription::ServerCrashed,
            ),
            (
                DomainEvent::BackupCompleted {
                    name: "survival".to_owned(),
                    snapshot: "s-1".to_owned(),
                    added_bytes: 10,
                },
                Subscription::BackupCompleted,
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(subscription_of(&event), expected);
            assert!(is_subscribable(&event));
        }
        assert!(!is_subscribable(&DomainEvent::JarDownloadCompleted {
            version: crate::server::version::VersionId::new("1.21.1"),
        }));
    }

    #[test]
    fn console_parsing_delegates_to_server_chat() {
        let event = parse_console_line(
            "survival",
            "2026-11-18T09:30:00Z",
            "[09:30:00] [Server thread/INFO]: <Steve> 有人吗",
        )
        .expect("chat");
        assert_eq!(subscription_of(&event), Subscription::ChatMessage);
        assert!(matches!(
            event,
            DomainEvent::ChatMessage {
                server,
                from,
                text,
                ..
            } if server == "survival" && from == "Steve" && text == "有人吗"
        ));
    }
}
