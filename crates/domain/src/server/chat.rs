//! 控制台行解析：把原版服务端的稳定格式转成领域事件
//!
//! 解析放在 `domain` 而不是宿主：CLI、远端 agent、插件宿主都会追随同一份
//! 控制台，各写一套正则会迅速分叉。模组改写的行不被识别，订阅者仍可通过
//! `console:line` 拿到原文。
//!
//! 解析器是纯函数，不 panic；时间与来源服务器由调用方提供，因为日志行本身
//! 不带可靠的 UTC 时间与服务器身份。

use crate::server::event::DomainEvent;

/// 解析一行控制台输出
///
/// `server` 是来源服务器名，`at` 是 RFC3339 UTC 时间（由宿主在读取时打上）。
/// 识别原版聊天、玩家加入、玩家离开；其余返回 `None`。
#[must_use]
pub fn parse_line(server: &str, at: &str, line: &str) -> Option<DomainEvent> {
    let line = strip_log_prefix(line);
    if let Some((from, text)) = parse_chat(line) {
        return Some(DomainEvent::ChatMessage {
            server: server.to_owned(),
            from: from.to_owned(),
            text: text.to_owned(),
            at: at.to_owned(),
        });
    }
    if let Some(player) = line
        .strip_suffix(" joined the game")
        .filter(|name| is_player_name(name))
    {
        return Some(DomainEvent::PlayerJoined {
            server: server.to_owned(),
            player: player.to_owned(),
        });
    }
    if let Some(player) = line
        .strip_suffix(" left the game")
        .filter(|name| is_player_name(name))
    {
        return Some(DomainEvent::PlayerLeft {
            server: server.to_owned(),
            player: player.to_owned(),
        });
    }
    None
}

/// 去掉 `[12:34:56] [Server thread/INFO]: ` 一类前缀
fn strip_log_prefix(line: &str) -> &str {
    match line.rfind("]: ") {
        Some(idx) => &line[idx + 3..],
        None => line,
    }
}

/// `<Steve> 文本` → `(Steve, 文本)`
fn parse_chat(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('<')?;
    let (name, text) = rest.split_once("> ")?;
    (is_player_name(name) && !text.is_empty()).then_some((name, text))
}

/// 原版玩家名：1..=16 位字母、数字、下划线
fn is_player_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 16
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: &str = "2026-11-18T09:30:00Z";

    #[test]
    fn parses_vanilla_chat_lines() {
        let event = parse_line(
            "survival",
            AT,
            "[09:30:00] [Server thread/INFO]: <Steve> 有人吗",
        )
        .expect("chat");
        assert_eq!(
            event,
            DomainEvent::ChatMessage {
                server: "survival".to_owned(),
                from: "Steve".to_owned(),
                text: "有人吗".to_owned(),
                at: AT.to_owned(),
            }
        );
        assert!(parse_line("survival", AT, "<Steve> hi").is_some());
    }

    #[test]
    fn parses_join_and_leave_lines() {
        assert_eq!(
            parse_line(
                "survival",
                AT,
                "[09:30:00] [Server thread/INFO]: Steve joined the game"
            ),
            Some(DomainEvent::PlayerJoined {
                server: "survival".to_owned(),
                player: "Steve".to_owned(),
            })
        );
        assert_eq!(
            parse_line("survival", AT, "Alex left the game"),
            Some(DomainEvent::PlayerLeft {
                server: "survival".to_owned(),
                player: "Alex".to_owned(),
            })
        );
    }

    #[test]
    fn malformed_lines_do_not_parse_as_chat() {
        for line in [
            "<> hi",
            "<Steve with space> hi",
            "<Steve>",
            "<Steve> ",
            "Steve joined",
            "[09:30:00] [Server thread/INFO]: Done (1.2s)!",
        ] {
            let event = parse_line("survival", AT, line);
            assert!(
                !matches!(event, Some(DomainEvent::ChatMessage { .. })),
                "{line}"
            );
        }
    }

    #[test]
    fn section_signs_survive_parsing() {
        match parse_line("survival", AT, "<Steve> §a绿色") {
            Some(DomainEvent::ChatMessage { text, .. }) => assert_eq!(text, "§a绿色"),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
