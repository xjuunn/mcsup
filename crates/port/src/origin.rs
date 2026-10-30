//! 资源来源：一条记录属于哪台机器
//!
//! 远端是**资源属性**，不是全局模式（`docs/remote.md` 设计原则 1）。来源标签
//! 落在契约层而不是 `domain`：领域规则（状态机、备份口径、路径校验）与「这台
//! 机器是谁」无关，把它塞进 `domain` 会让每条判定都多一个无关维度。
//!
//! 放在 `port` 而不是 `service`，是因为**组合仓库**（把本地与多台远端合成
//! 一个列表）需要标注来源，而实现层不能依赖 `service`。过滤与选择器解析
//! 仍是 `service` 的事（那是入口语义，不是契约）。

use domain::workspace::WorkspaceId;
use std::fmt;
use thiserror::Error;

/// 本地来源的固定名称
pub const LOCAL_NAME: &str = "local";

/// 保留字：不能用作远端名
///
/// 它们已经是 `--origin` 的取值，再拿来当远端名会让 `--origin remote` 产生
/// 歧义（「全部远端」还是某台叫 `remote` 的机器）。
pub const RESERVED_NAMES: &[&str] = &["all", "local", "remote"];

/// 远端名字符数上限（与 `ServerName` 同量级，够长且不会撑爆表格）
pub const MAX_NAME_LEN: usize = 64;

/// 来源名非法
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OriginError {
    /// 空名字
    #[error("origin name is empty")]
    Empty,
    /// 超长
    #[error("origin name is too long: {0} (max {MAX_NAME_LEN})")]
    TooLong(String),
    /// 含路径分隔符（限定名用 `/` 分隔，名字里不能有）
    #[error("origin name must not contain '/': {0}")]
    HasSlash(String),
    /// 含 `[A-Za-z0-9_.-]` 以外的字符
    #[error("origin name has invalid character: {0}")]
    InvalidChar(String),
    /// 与保留字冲突
    #[error("'{0}' is a reserved origin keyword")]
    Reserved(String),
    /// 把 `all` / `remote` 当具体来源解析
    #[error("'{0}' is a filter keyword, not a concrete origin")]
    NotConcrete(String),
}

/// 远端名：已校验，可直接用作配置键与展示名
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemoteName(String);

impl RemoteName {
    /// 校验并构造
    ///
    /// 规则：非空、≤ [`MAX_NAME_LEN`] 字符、仅 `[A-Za-z0-9_.-]`、不是保留字。
    /// 允许 `.` 与 `-` 是为了能直接拿主机名 / `prod-box.local` 当默认名。
    ///
    /// # Errors
    ///
    /// 违反上述任一规则时返回 [`OriginError`]
    pub fn parse(raw: &str) -> Result<Self, OriginError> {
        if raw.is_empty() {
            return Err(OriginError::Empty);
        }
        if raw.chars().count() > MAX_NAME_LEN {
            return Err(OriginError::TooLong(raw.to_owned()));
        }
        if raw.contains('/') {
            return Err(OriginError::HasSlash(raw.to_owned()));
        }
        if !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            return Err(OriginError::InvalidChar(raw.to_owned()));
        }
        if RESERVED_NAMES.contains(&raw) {
            return Err(OriginError::Reserved(raw.to_owned()));
        }
        Ok(Self(raw.to_owned()))
    }

    /// 原始文本
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RemoteName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 资源来源
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Origin {
    /// 本机
    Local,
    /// 某个已配对的远端
    Remote(RemoteName),
}

impl Origin {
    /// 按名解析具体来源：`local` → [`Origin::Local`]，其余按 [`RemoteName`] 校验
    ///
    /// `all` / `remote` 是过滤词，不是具体来源，解析失败。
    ///
    /// # Errors
    ///
    /// 名字非法或是过滤词时返回 [`OriginError`]
    pub fn parse(raw: &str) -> Result<Self, OriginError> {
        if raw == LOCAL_NAME {
            return Ok(Self::Local);
        }
        if matches!(raw, "all" | "remote") {
            return Err(OriginError::NotConcrete(raw.to_owned()));
        }
        RemoteName::parse(raw).map(Self::Remote)
    }

    /// 稳定文本（配置键、JSON `origin` 字段、限定名前缀共用）
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Local => LOCAL_NAME,
            Self::Remote(name) => name.as_str(),
        }
    }

    /// 是否本地
    #[must_use]
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    /// 把资源名拼成限定名（`origin/name`）
    #[must_use]
    pub fn qualify(&self, name: &str) -> String {
        format!("{}/{name}", self.as_str())
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 带来源的一条资源
///
/// 组合仓库（本地 + 多台远端）用它在合并列表里标注每一条的归属；单来源仓库
/// 直接返回裸值。这样「来源」不会污染领域对象本身——`ServerMeta` 仍只描述
/// 一台服务器，不知道也不关心自己在哪台机器上。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sourced<T> {
    /// 来源
    pub origin: Origin,
    /// 资源本体
    pub value: T,
    /// 本条是否只是本地缓存的旧值（来源当前不可达）
    ///
    /// 与 `origin` 分开：来源仍是那台机器（过滤、限定名、写路由都不变），
    /// 变的只是「这份数据是刚问来的还是上次留下的」。入口据此渲染
    /// `(offline)`，而不是把条目藏起来——藏起来会让用户以为资源没了。
    pub offline: bool,
}

/// 客户端视角的「当前工作区」选择
///
/// 「当前」是**查看它的那台机器**的状态，不是资源自身的属性：本机 `use`
/// 某个远端工作区时只改这个本地指针，绝不去改对端的 current（对端机器上
/// 直接跑 mcsup 的人仍保留自己的 current）。
///
/// `origin` 用远端名而不是证书指纹：与 `--origin` / 限定名的口径一致；
/// 此时改远端名会让选择悬空，按「无当前」安全失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSelection {
    /// 工作区所在来源
    pub origin: Origin,
    /// 工作区稳定身份
    pub workspace: WorkspaceId,
}

impl<T> Sourced<T> {
    /// 本地来源
    #[must_use]
    pub fn local(value: T) -> Self {
        Self {
            origin: Origin::Local,
            value,
            offline: false,
        }
    }

    /// 指定来源
    #[must_use]
    pub fn new(origin: Origin, value: T) -> Self {
        Self {
            origin,
            value,
            offline: false,
        }
    }

    /// 换掉值，保留来源
    #[must_use]
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Sourced<U> {
        Sourced {
            origin: self.origin,
            value: f(self.value),
            offline: self.offline,
        }
    }

    /// 标成「来自离线来源的缓存值」
    #[must_use]
    pub const fn offline(mut self) -> Self {
        self.offline = true;
        self
    }

    /// 是否是离线缓存值
    #[must_use]
    pub const fn is_offline(&self) -> bool {
        self.offline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_words_are_not_remote_names() {
        for word in RESERVED_NAMES {
            assert_eq!(
                RemoteName::parse(word),
                Err(OriginError::Reserved((*word).to_owned())),
                "{word} 是保留字"
            );
        }
    }

    #[test]
    fn remote_names_reject_unsafe_input() {
        assert_eq!(RemoteName::parse(""), Err(OriginError::Empty));
        assert!(matches!(
            RemoteName::parse("a/b"),
            Err(OriginError::HasSlash(_))
        ));
        assert!(matches!(
            RemoteName::parse("prod box"),
            Err(OriginError::InvalidChar(_))
        ));
        assert!(matches!(
            RemoteName::parse("prod\u{4e2d}"),
            Err(OriginError::InvalidChar(_))
        ));
        assert!(matches!(
            RemoteName::parse(&"a".repeat(MAX_NAME_LEN + 1)),
            Err(OriginError::TooLong(_))
        ));
        assert!(RemoteName::parse("prod-box.local_2").is_ok());
    }

    #[test]
    fn origin_parse_separates_concrete_from_filter_words() {
        assert_eq!(Origin::parse("local"), Ok(Origin::Local));
        assert_eq!(
            Origin::parse("prod-box"),
            Ok(Origin::Remote(RemoteName::parse("prod-box").expect("name")))
        );
        assert_eq!(
            Origin::parse("all"),
            Err(OriginError::NotConcrete("all".to_owned()))
        );
        assert_eq!(
            Origin::parse("remote"),
            Err(OriginError::NotConcrete("remote".to_owned()))
        );
    }

    #[test]
    fn qualify_joins_with_slash() {
        assert_eq!(Origin::Local.qualify("web"), "local/web");
        let remote = Origin::Remote(RemoteName::parse("prod-box").expect("name"));
        assert_eq!(remote.qualify("web"), "prod-box/web");
        assert_eq!(remote.to_string(), "prod-box");
    }

    #[test]
    fn sourced_keeps_origin_across_map() {
        let remote = Origin::Remote(RemoteName::parse("prod-box").expect("name"));
        let sourced = Sourced::new(remote.clone(), 1u8).map(|value| value + 1);
        assert_eq!(sourced.origin, remote);
        assert_eq!(sourced.value, 2);
        assert!(!sourced.is_offline());
        assert!(Sourced::local(1u8).offline().is_offline());
        assert_eq!(Sourced::local(7u8).origin, Origin::Local);
    }
}
