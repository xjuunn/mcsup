//! `server.properties` 领域模型：解析 / 渲染 / 修改（纯规则，IO 在 service）
//!
//! Minecraft 的属性文件是有序的 `key=value` 行集，夹杂 `#` 注释与服务器
//! 自加的标题行。改写时必须保留注释与既有顺序——只替换目标键的值、新键
//! 追加到末尾；整体重排会让用户的手工注释与 diff 全部丢失。

use super::error::{InvalidPropertyKey, InvalidPropertyValue, PropertySetError};
use std::fmt::Write as _;

/// 服务端属性文件名（约定在服务器目录下）
pub const PROPERTIES_FILE: &str = "server.properties";

/// 文件里的一行：键值行保留键与值，其余（注释 / 空行 / 无法识别）原样保留
#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    /// 注释或无法识别的原始行（原样往返）
    Raw(String),
    /// `key=value`
    Entry { key: String, value: String },
}

/// `server.properties` 内容：有序行 + 键值视图
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Properties {
    lines: Vec<Line>,
}

impl Properties {
    /// 解析文本；不含 `=` 的行按原始行保留
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let lines = text
            .lines()
            .map(|line| {
                let trimmed = line.trim_end();
                match split_entry(trimmed) {
                    Some((key, value)) => Line::Entry {
                        key: key.to_owned(),
                        value: value.to_owned(),
                    },
                    None => Line::Raw(trimmed.to_owned()),
                }
            })
            .collect();
        Self { lines }
    }

    /// 渲染为文件文本（每行以 `\n` 结尾）
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            match line {
                Line::Raw(raw) => {
                    let _ = writeln!(out, "{raw}");
                }
                Line::Entry { key, value } => {
                    let _ = writeln!(out, "{key}={value}");
                }
            }
        }
        out
    }

    /// 键值对迭代，按文件内出现顺序（注释 / 空行不计入）
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.lines.iter().filter_map(|line| match line {
            Line::Entry { key, value } => Some((key.as_str(), value.as_str())),
            Line::Raw(_) => None,
        })
    }

    /// 键的当前值；不存在该键时返回 `None`
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries()
            .find(|(k, _)| *k == key)
            .map(|(_, value)| value)
    }

    /// 键的数量
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries().count()
    }

    /// 是否没有任何键（文件缺失 / 只有注释）
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 写入键值：已存在替换其值（原位），不存在追加到末尾
    ///
    /// # Errors
    ///
    /// 键为空 / 含 `=` 或换行，或值含换行时返回 [`PropertySetError`]，文件视图不变
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), PropertySetError> {
        validate_key(key)?;
        validate_value(key, value)?;
        match self
            .lines
            .iter_mut()
            .find(|line| matches!(line, Line::Entry { key: k, .. } if k == key))
        {
            Some(Line::Entry { value: slot, .. }) => value.clone_into(slot),
            _ => self.lines.push(Line::Entry {
                key: key.to_owned(),
                value: value.to_owned(),
            }),
        }
        Ok(())
    }
}

fn validate_key(key: &str) -> Result<(), PropertySetError> {
    if key.is_empty() || key.contains(['=', '\n', '\r']) {
        return Err(InvalidPropertyKey {
            raw: key.to_owned(),
        }
        .into());
    }
    Ok(())
}

fn validate_value(key: &str, value: &str) -> Result<(), PropertySetError> {
    if value.contains(['\n', '\r']) {
        return Err(InvalidPropertyValue {
            key: key.to_owned(),
            value: value.to_owned(),
        }
        .into());
    }
    Ok(())
}

/// 按首个 `=` 切分键值；键裁剪两侧空白，值保留原文（行尾空白已去）
fn split_entry(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    Some((key, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_preserves_comments_blanks_and_order() {
        let text = "#MC server properties\nmotd=A server\ngamemode=survival\n\nmax-players=20\n";
        let props = Properties::parse(text);
        assert_eq!(props.len(), 3);
        assert_eq!(props.get("motd"), Some("A server"));
        assert_eq!(props.get("max-players"), Some("20"));
        // 注释 / 空行 / 顺序原样往返
        assert_eq!(props.render(), text);
    }

    #[test]
    fn set_replaces_in_place_and_appends_new_keys() {
        let mut props = Properties::parse("# header\nmotd=old\n");
        props.set("motd", "new").expect("replace");
        props.set("pvp", "true").expect("append");
        assert_eq!(props.render(), "# header\nmotd=new\npvp=true\n");
    }

    #[test]
    fn set_only_touches_first_duplicate_key() {
        // 重复键只改首个，其后副本原样保留，交给服务器自己裁决
        let mut props = Properties::parse("motd=one\nmotd=two\n");
        props.set("motd", "three").expect("set");
        assert_eq!(props.get("motd"), Some("three"));
        assert_eq!(props.render(), "motd=three\nmotd=two\n");
    }

    #[test]
    fn set_rejects_invalid_key_and_value() {
        let mut props = Properties::parse("motd=old\n");
        assert!(props.set("bad=key", "v").is_err());
        assert!(props.set("", "v").is_err());
        assert!(props.set("key", "a\nb").is_err());
        // 失败后文件视图不变
        assert_eq!(props.render(), "motd=old\n");
    }

    #[test]
    fn empty_key_line_is_kept_raw() {
        let props = Properties::parse("=orphan\nmotd=ok\n");
        assert_eq!(props.len(), 1);
        assert_eq!(props.render(), "=orphan\nmotd=ok\n");
    }
}
