//! 来源（origin）过滤与选择器解析
//!
//! 远端是**资源属性**，不是全局模式（见 `docs/remote.md` 设计原则 1）。因此
//! [`Origin`] 只活在查询与路由里，**不进 `domain`**：领域规则（服务器状态机、
//! 备份口径、路径校验）与「这台机器是谁」无关，把来源塞进 `domain` 会让每条
//! 领域判定都被迫多一个与业务无关的维度。
//!
//! 类型本体（[`Origin`] / [`RemoteName`] / [`OriginError`]）放在 `port`：组合
//! 仓库（本地 + 多台远端合成一个列表）需要标注来源，而实现层不能依赖
//! `service`。本模块只放**入口语义**——过滤取值、限定名解析、跨来源唯一性：
//!
//! - [`OriginFilter`] 的解析与匹配（`all` / `local` / `remote` / `<name>`）
//! - [`Selector`] 的解析（`origin/name` 限定名）与跨来源唯一性解析
//!
//! 这两件事在 CLI / API / MCP 三个入口、以及本地与远端两侧的语义必须完全
//! 一致，所以只能有一份实现（AGENTS.md「判定逻辑收敛到领域层」）。入口只负责
//! 把 [`ResolveError`] 渲染成各自的提示与退出码。

use port::{LOCAL_NAME, Origin, OriginError, RemoteName};
use std::fmt;
use thiserror::Error;

/// 列表按来源过滤的取值
///
/// 缺省 [`OriginFilter::All`]：本地 + 全部已配对远端。过滤只影响「返回哪些
/// 条目」，不改变命令语义——过滤后为空是成功，不是错误。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum OriginFilter {
    /// 本地 + 全部远端
    #[default]
    All,
    /// 仅本地
    LocalOnly,
    /// 全部远端，不含本地
    RemoteOnly,
    /// 指定远端
    Named(RemoteName),
}

impl OriginFilter {
    /// 缺省过滤（全部）
    #[must_use]
    pub fn all() -> Self {
        Self::All
    }

    /// 按文本解析：`all` / `local` / `remote` / `<远端名>`
    ///
    /// # Errors
    ///
    /// 远端名非法时返回 [`OriginError`]
    pub fn parse(raw: &str) -> Result<Self, OriginError> {
        match raw {
            "all" => Ok(Self::All),
            "local" => Ok(Self::LocalOnly),
            "remote" => Ok(Self::RemoteOnly),
            other => RemoteName::parse(other).map(Self::Named),
        }
    }

    /// 该来源是否在过滤范围内
    #[must_use]
    pub fn matches(&self, origin: &Origin) -> bool {
        match self {
            Self::All => true,
            Self::LocalOnly => origin.is_local(),
            Self::RemoteOnly => !origin.is_local(),
            Self::Named(want) => matches!(origin, Origin::Remote(got) if want == got),
        }
    }

    /// 过滤范围是否包含某个远端（用于「该不该向这台远端发请求」）
    #[must_use]
    pub fn includes_remote(&self, name: &RemoteName) -> bool {
        self.matches(&Origin::Remote(name.clone()))
    }
}

impl fmt::Display for OriginFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All => f.write_str("all"),
            Self::LocalOnly => f.write_str(LOCAL_NAME),
            Self::RemoteOnly => f.write_str("remote"),
            Self::Named(name) => fmt::Display::fmt(name, f),
        }
    }
}

/// 资源选择器：可选来源限定 + 资源名
///
/// 资源名在本地与所有远端之间可能重名，`origin/name` 用来消除歧义。资源名
/// 本身不允许含 `/`（`ServerName` / `WorkspaceName` 的字符集已排除它），
/// 因此按第一个 `/` 切分是无歧义的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    origin: Option<Origin>,
    name: String,
}

impl Selector {
    /// 解析：含 `/` 时前半段是来源，其余是资源名；否则只有资源名
    ///
    /// 前半段不是合法来源时**不报错**，整串当资源名——资源名合法字符集里
    /// 没有 `/`，所以这种情况只可能出现在用户笔误里，交给后续「找不到」
    /// 统一处理，错误信息比「来源名非法」更有用。
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.split_once('/') {
            Some((origin, name)) if !name.is_empty() => match Origin::parse(origin) {
                Ok(origin) => Self {
                    origin: Some(origin),
                    name: name.to_owned(),
                },
                Err(_) => Self {
                    origin: None,
                    name: raw.to_owned(),
                },
            },
            _ => Self {
                origin: None,
                name: raw.to_owned(),
            },
        }
    }

    /// 直接构造（无来源限定）
    #[must_use]
    pub fn bare(name: impl Into<String>) -> Self {
        Self {
            origin: None,
            name: name.into(),
        }
    }

    /// 显式来源限定
    #[must_use]
    pub fn with_origin(origin: Origin, name: impl Into<String>) -> Self {
        Self {
            origin: Some(origin),
            name: name.into(),
        }
    }

    /// 限定的来源；`None` = 未限定
    #[must_use]
    pub fn origin(&self) -> Option<&Origin> {
        self.origin.as_ref()
    }

    /// 资源名（不含来源前缀）
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.origin {
            Some(origin) => write!(f, "{}", origin.qualify(&self.name)),
            None => f.write_str(&self.name),
        }
    }
}

/// 跨来源解析失败
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResolveError {
    /// 限定名点名的来源没有配对
    #[error("origin '{0}' is not paired")]
    OriginNotPaired(String),
    /// 过滤后找不到该名字
    #[error("'{0}' not found")]
    NotFound(String),
    /// 同名资源出现在多个来源，且选择器未限定
    #[error("'{selector}' exists in multiple origins")]
    Ambiguous {
        /// 用户输入的原始选择器
        selector: String,
        /// 候选的限定名（稳定升序，供入口逐条列出）
        candidates: Vec<String>,
    },
}

/// 在候选表里按选择器 + 过滤解析出唯一一条，返回下标
///
/// 语义（与 `docs/remote.md`「限定名」一节一致）：
///
/// 1. 选择器带来源限定 → 只在该来源里精确匹配名字；
/// 2. 不带限定 → 在过滤后的候选里精确匹配名字，唯一才通过；
/// 3. 同名多条 → [`ResolveError::Ambiguous`]，候选按限定名升序。
///
/// 过滤先于解析：被 `--origin local` 排除掉的远端条目不会参与唯一性判定，
/// 否则「过滤到只剩一条」反而会报歧义。
///
/// 精确匹配，不做前缀补全：前缀语义由各资源自己的 service 定义（工作区支持、
/// 服务器不支持），跨来源这一层不该再叠一套。
///
/// # Errors
///
/// 来源未配对 / 找不到 / 歧义时返回 [`ResolveError`]
pub fn resolve_index(
    candidates: &[(Origin, &str)],
    selector: &Selector,
    filter: &OriginFilter,
) -> Result<usize, ResolveError> {
    let scoped: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, (origin, _))| filter.matches(origin))
        .map(|(index, _)| index)
        .collect();

    if let Some(want) = selector.origin() {
        // 限定名：来源必须存在（否则是「没配对」而不是「没找到」）
        if !scoped.iter().any(|&index| &candidates[index].0 == want) {
            return Err(ResolveError::OriginNotPaired(want.as_str().to_owned()));
        }
        return scoped
            .into_iter()
            .find(|&index| &candidates[index].0 == want && candidates[index].1 == selector.name())
            .ok_or_else(|| ResolveError::NotFound(selector.to_string()));
    }

    let hits: Vec<usize> = scoped
        .into_iter()
        .filter(|&index| candidates[index].1 == selector.name())
        .collect();
    match hits.as_slice() {
        [] => Err(ResolveError::NotFound(selector.name().to_owned())),
        [one] => Ok(*one),
        many => {
            let mut names: Vec<String> = many
                .iter()
                .map(|&index| candidates[index].0.qualify(candidates[index].1))
                .collect();
            names.sort();
            Err(ResolveError::Ambiguous {
                selector: selector.name().to_owned(),
                candidates: names,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(raw: &str) -> RemoteName {
        RemoteName::parse(raw).expect("valid remote name")
    }

    #[test]
    fn filter_parses_and_matches() {
        assert_eq!(OriginFilter::parse("all"), Ok(OriginFilter::All));
        assert_eq!(OriginFilter::parse("local"), Ok(OriginFilter::LocalOnly));
        assert_eq!(OriginFilter::parse("remote"), Ok(OriginFilter::RemoteOnly));
        assert_eq!(
            OriginFilter::parse("prod-box"),
            Ok(OriginFilter::Named(name("prod-box")))
        );
        assert!(OriginFilter::parse("a/b").is_err());

        let local = Origin::Local;
        let prod = Origin::Remote(name("prod-box"));
        let stage = Origin::Remote(name("stage"));

        assert!(OriginFilter::All.matches(&local) && OriginFilter::All.matches(&prod));
        assert!(OriginFilter::LocalOnly.matches(&local));
        assert!(!OriginFilter::LocalOnly.matches(&prod));
        assert!(OriginFilter::RemoteOnly.matches(&prod));
        assert!(!OriginFilter::RemoteOnly.matches(&local));
        assert!(OriginFilter::Named(name("prod-box")).matches(&prod));
        assert!(!OriginFilter::Named(name("prod-box")).matches(&stage));
        assert!(!OriginFilter::Named(name("prod-box")).matches(&local));
        assert!(OriginFilter::Named(name("prod-box")).includes_remote(&name("prod-box")));
        assert!(!OriginFilter::LocalOnly.includes_remote(&name("prod-box")));
    }

    #[test]
    fn selector_splits_qualified_names() {
        let plain = Selector::parse("web");
        assert_eq!(plain.origin(), None);
        assert_eq!(plain.name(), "web");

        let scoped = Selector::parse("prod-box/web");
        assert_eq!(scoped.origin(), Some(&Origin::Remote(name("prod-box"))));
        assert_eq!(scoped.name(), "web");

        let local = Selector::parse("local/web");
        assert_eq!(local.origin(), Some(&Origin::Local));
        assert_eq!(local.name(), "web");

        // 来源名非法时不切分：整串当资源名，交给「找不到」
        let bogus = Selector::parse("not a name/web");
        assert_eq!(bogus.origin(), None);
        assert_eq!(bogus.name(), "not a name/web");

        // 以 `/` 结尾不切分
        let trailing = Selector::parse("prod-box/");
        assert_eq!(trailing.origin(), None);
    }

    #[test]
    fn unique_name_resolves_without_qualifier() {
        let all = [
            (Origin::Local, "web"),
            (Origin::Remote(name("prod-box")), "survival"),
        ];
        let index = resolve_index(&all, &Selector::parse("web"), &OriginFilter::All).expect("唯一");
        assert_eq!(index, 0);
    }

    #[test]
    fn duplicate_name_is_ambiguous_and_lists_candidates_in_order() {
        let all = [
            (Origin::Remote(name("prod-box")), "web"),
            (Origin::Local, "web"),
        ];
        let err = resolve_index(&all, &Selector::parse("web"), &OriginFilter::All)
            .expect_err("同名必须报歧义");
        assert_eq!(
            err,
            ResolveError::Ambiguous {
                selector: "web".to_owned(),
                candidates: vec!["local/web".to_owned(), "prod-box/web".to_owned()],
            }
        );
    }

    #[test]
    fn qualifier_disambiguates() {
        let all = [
            (Origin::Local, "web"),
            (Origin::Remote(name("prod-box")), "web"),
        ];
        let local = resolve_index(&all, &Selector::parse("local/web"), &OriginFilter::All)
            .expect("本地命中");
        assert_eq!(local, 0);
        let remote = resolve_index(&all, &Selector::parse("prod-box/web"), &OriginFilter::All)
            .expect("远端命中");
        assert_eq!(remote, 1);
    }

    /// 过滤先于唯一性判定：过滤到只剩一条时不该报歧义
    #[test]
    fn filter_narrows_before_uniqueness() {
        let all = [
            (Origin::Local, "web"),
            (Origin::Remote(name("prod-box")), "web"),
        ];
        let index = resolve_index(&all, &Selector::parse("web"), &OriginFilter::LocalOnly)
            .expect("过滤后唯一");
        assert_eq!(index, 0);
        let index = resolve_index(
            &all,
            &Selector::parse("web"),
            &OriginFilter::Named(name("prod-box")),
        )
        .expect("过滤后唯一");
        assert_eq!(index, 1);
    }

    /// 限定名点到没配对的来源是「未配对」，不是「找不到」
    #[test]
    fn qualifier_on_unpaired_origin_is_reported_separately() {
        let all = [(Origin::Local, "web")];
        let err = resolve_index(&all, &Selector::parse("stage/web"), &OriginFilter::All)
            .expect_err("没配对");
        assert_eq!(err, ResolveError::OriginNotPaired("stage".to_owned()));
    }

    #[test]
    fn missing_name_is_not_found() {
        let all = [(Origin::Local, "web")];
        let err =
            resolve_index(&all, &Selector::parse("nope"), &OriginFilter::All).expect_err("找不到");
        assert_eq!(err, ResolveError::NotFound("nope".to_owned()));
    }

    /// 精确匹配，不做前缀补全（前缀语义归各资源自己的 service）
    #[test]
    fn resolution_is_exact_not_prefix() {
        let all = [(Origin::Local, "web-01")];
        assert!(resolve_index(&all, &Selector::parse("web"), &OriginFilter::All).is_err());
    }

    #[test]
    fn qualify_joins_with_slash() {
        assert_eq!(Origin::Local.qualify("web"), "local/web");
        assert_eq!(
            Origin::Remote(name("prod-box")).qualify("web"),
            "prod-box/web"
        );
        assert_eq!(Origin::Remote(name("prod-box")).to_string(), "prod-box");
    }
}
