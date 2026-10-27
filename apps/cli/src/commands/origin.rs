//! `--origin` 过滤与来源解析：入口侧唯一分派点
//!
//! 过滤取值、限定名解析与跨来源唯一性都在 `service::origin`（判定只有一份），
//! 这里只做三件事：读参数 / 环境变量、把 `Origin` 渲染成展示文本、按来源取
//! 对应远端的成员句柄。命令层因此不需要各自写一遍 `if remote`。

use crate::diagnostic::AppDiagnostic;
use anyhow::Result;
use mcsup::backend::remote::RemoteMembers;
use mcsup::port::{Origin, RemoteName};
use mcsup::service::OriginFilter;

/// 环境变量：缺省来源过滤
pub(crate) const ORIGIN_ENV: &str = "MCSUP_ORIGIN";

/// 解析过滤：`flag > 环境变量 > all`
///
/// # Errors
///
/// 取值不是 `all` / `local` / `remote` / 合法远端名时返回用法错误（退出码 2）
pub(crate) fn filter(raw: Option<&str>, lang: crate::locale::Lang) -> Result<OriginFilter> {
    filter_from(raw, std::env::var(ORIGIN_ENV).ok(), lang)
}

/// [`filter`] 的纯逻辑内核：环境变量由调用方读入，便于测试
///
/// # Errors
///
/// 取值不是 `all` / `local` / `remote` / 合法远端名时返回用法错误（退出码 2）
pub(crate) fn filter_from(
    raw: Option<&str>,
    env: Option<String>,
    lang: crate::locale::Lang,
) -> Result<OriginFilter> {
    let value = raw
        .map(str::to_owned)
        .or(env)
        .filter(|value| !value.trim().is_empty());
    let Some(value) = value else {
        return Ok(OriginFilter::all());
    };
    OriginFilter::parse(value.trim()).map_err(|err| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("--origin 取值非法: {value}"),
                format!("invalid --origin value: {value}"),
            ),
            Some(crate::i18n::pick(
                lang,
                format!("可用取值：all / local / remote / <远端名>（{err}）"),
                format!("use all / local / remote / <remote-name> ({err})"),
            )),
        )
        .into()
    })
}

/// 来源的展示文本：本地固定 `local`，远端就是远端名
pub(crate) fn label(origin: &Origin) -> String {
    origin.as_str().to_owned()
}

/// 按来源取远端成员；本地来源返回 `None`
///
/// # Errors
///
/// 远端未在成员表里（配置有记录但凭证 / 指纹不可用）时返回用法错误
pub(crate) fn member<'a>(
    members: &'a RemoteMembers,
    origin: &Origin,
    lang: crate::locale::Lang,
) -> Result<Option<&'a mcsup::backend::remote::RemoteMember>> {
    let Origin::Remote(name) = origin else {
        return Ok(None);
    };
    members.get(name).map(Some).ok_or_else(|| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("远端 {name} 未就绪（缺凭证或指纹）"),
                format!("remote {name} is not ready (missing credential or fingerprint)"),
            ),
            Some(crate::i18n::pick(
                lang,
                "先运行 mcsup remote reconnect <name> 或 mcsup remote list 检查状态",
                "run `mcsup remote reconnect <name>` or `mcsup remote list` first",
            )),
        )
        .into()
    })
}

/// 把 `origin/name` 解析成可交给 service 的裸名
///
/// service 的 `get(name)` 不认识来源前缀，且组合仓库在重名时按「本地优先」
/// 返回第一条。因此：
///
/// - 未限定 → 原样返回；
/// - 限定本地，或该名字全局唯一 → 返回裸名（service 取到的就是同一条）；
/// - 限定远端且同名存在于多个来源 → 明确拒绝，避免把操作落到本地同名服务器。
///
/// 这是「限定名对所有命令生效」在入口侧的落点；真正的跨来源唯一性判定仍在
/// `service::origin::resolve_index`。
///
/// # Errors
///
/// 来源未配对、找不到或重名且无法用裸名唯一表达时返回用法 / 未找到错误
pub(crate) fn resolve_server_name(raw: &str, printer: crate::output::Printer) -> Result<String> {
    if !raw.contains('/') {
        return Ok(raw.to_owned());
    }
    let selector = mcsup::service::Selector::parse(raw);
    if selector.origin().is_none() {
        return Ok(raw.to_owned());
    }
    let reg = crate::bootstrap::open_registry_with_remotes()?;
    let all = reg.list_sourced()?;
    let candidates: Vec<(Origin, &str)> = all
        .iter()
        .map(|sourced| (sourced.origin.clone(), sourced.value.meta.name.as_str()))
        .collect();
    let index = mcsup::service::resolve_index(&candidates, &selector, &OriginFilter::All)
        .map_err(|err| AppDiagnostic::not_found(err.to_string(), None))?;
    let bare = selector.name();
    let unique = candidates.iter().filter(|(_, name)| *name == bare).count() == 1;
    if candidates[index].0.is_local() || unique {
        return Ok(bare.to_owned());
    }
    Err(AppDiagnostic::usage(
        crate::i18n::pick(
            printer.lang(),
            format!(
                "服务器名在多个来源重复，无法确定远端操作目标: {raw}；请让该名字全局唯一后再重试"
            ),
            format!(
                "the server name exists in more than one origin, so the remote target is ambiguous: {raw}"
            ),
        ),
        None,
    )
    .into())
}

/// 解析到远端的服务器目标
///
/// 入口需要「这台服务器现在在哪」才能选通道：`service` 的写路径只认裸名，
/// 远端写要走帧并带对端身份。这里一次解析出三样，避免各命令各查一遍。
#[derive(Debug, Clone)]
pub(crate) struct RemoteTarget {
    /// 该服务器所属的远端
    pub member: mcsup::backend::remote::RemoteMember,
    /// 对端上的稳定身份（帧按 id 寻址，不按名字）
    pub server_id: String,
    /// 对端此刻的注册记录（含服务属性，交互默认值直接用它）
    pub meta: mcsup::domain::server::meta::ServerMeta,
}

/// 组合列表里的目标判定结果
///
/// 纯判定、不做 IO：`remote_target` 只负责取列表与装配 [`RemoteTarget`]，
/// 「本地优先 / 唯一远端 / 重名歧义」这三条口径因此可以单测。
enum TargetPick<'a, T> {
    /// 本地命中（含显式 `local/name`）：交给本地路径
    Local,
    /// 唯一远端命中
    Remote(&'a T),
    /// 该名字不存在（由随后的 service / 镜像调用统一报）
    Missing,
    /// 多台远端重名，不替用户猜
    Ambiguous,
}

/// 在已按名字过滤前的完整列表里挑目标（本地优先）
fn pick_hit<'a, T>(
    items: &'a [T],
    bare: &str,
    selector: &mcsup::service::Selector,
    name_of: impl Fn(&T) -> &str,
    origin_of: impl Fn(&T) -> &Origin,
) -> TargetPick<'a, T> {
    let named: Vec<&T> = items.iter().filter(|item| name_of(item) == bare).collect();
    let hits: Vec<&T> = match selector.origin() {
        // 限定来源：只看该来源
        Some(origin) => named
            .into_iter()
            .filter(|item| origin_of(item) == origin)
            .collect(),
        // 未限定：本地命中即交给本地路径，不再考虑同名远端
        None if named.iter().any(|item| origin_of(item).is_local()) => {
            return TargetPick::Local;
        }
        None => named,
    };
    match hits.as_slice() {
        [] => TargetPick::Missing,
        [one] if origin_of(one).is_local() => TargetPick::Local,
        [one] => TargetPick::Remote(one),
        _ => TargetPick::Ambiguous,
    }
}

/// 目标服务器是否落在远端
///
/// `Some` = 该名字解析到远端服务器；`None` = 本地，或本地根本没有这个名字
/// （「不存在」由随后的 service / 镜像调用统一报，退出码一致）。
///
/// 解析口径与 `server list --origin` 同一份组合注册表：
///
/// - 未限定：本地有同名服务器就返回 `None`（本地优先，既有语义）；
/// - 未限定且只有一台远端有这个裸名：返回那台；
/// - 未限定而多台远端重名：报歧义，不替用户猜；
/// - 限定远端：只看该来源。
///
/// # Errors
///
/// 远端读取失败，或该远端在配置里有记录但凭证 / 指纹不可用时返回错误
pub(crate) fn remote_target(raw: &str, lang: crate::locale::Lang) -> Result<Option<RemoteTarget>> {
    let cfg = crate::config::ConfigFile::locate(None).load()?;
    let members = crate::bootstrap::remote_members(&cfg)?;
    if members.is_empty() {
        return Ok(None);
    }
    let selector = mcsup::service::Selector::parse(raw);
    let reg = crate::bootstrap::open_registry_with_remotes()?;
    let all = reg.list_sourced()?;
    let picked = pick_hit(
        &all,
        selector.name(),
        &selector,
        |sourced| sourced.value.meta.name.as_str(),
        |sourced| &sourced.origin,
    );
    let sourced = match picked {
        TargetPick::Local | TargetPick::Missing => return Ok(None),
        TargetPick::Ambiguous => {
            return Err(AppDiagnostic::usage(
                crate::i18n::pick(
                    lang,
                    format!("服务器名在多个远端重复，无法确定目标: {raw}；请用 <远端>/<名称> 限定"),
                    format!(
                        "the server name exists on more than one remote: {raw}; qualify it as <remote>/<name>"
                    ),
                ),
                None,
            )
            .into());
        }
        TargetPick::Remote(sourced) => sourced,
    };
    let Some(member) = member(&members, &sourced.origin, lang)? else {
        return Ok(None);
    };
    Ok(Some(RemoteTarget {
        member: member.clone(),
        server_id: sourced.value.meta.id.as_str().to_owned(),
        meta: sourced.value.meta.clone(),
    }))
}

/// 解析远端名（供 `remote` 子命令复用）
///
/// # Errors
///
/// 名字非法或是保留字时返回用法错误
pub(crate) fn remote_name(raw: &str, lang: crate::locale::Lang) -> Result<RemoteName> {
    RemoteName::parse(raw).map_err(|err| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("远端名非法: {raw}"),
                format!("invalid remote name: {raw}"),
            ),
            Some(err.to_string()),
        )
        .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::locale::Lang;

    fn name(raw: &str) -> RemoteName {
        RemoteName::parse(raw).expect("valid name")
    }

    #[test]
    fn filter_defaults_to_all_without_flag_or_env() {
        assert_eq!(
            filter_from(None, None, Lang::Zh).expect("default"),
            OriginFilter::All
        );
        assert_eq!(
            filter_from(None, Some("local".to_owned()), Lang::Zh).expect("env"),
            OriginFilter::LocalOnly
        );
        assert_eq!(
            filter_from(Some("prod-box"), Some("local".to_owned()), Lang::Zh).expect("flag wins"),
            OriginFilter::Named(name("prod-box"))
        );
        assert_eq!(
            filter_from(Some("remote"), None, Lang::Zh).expect("remote"),
            OriginFilter::RemoteOnly
        );
        // 空白环境变量视为未设置
        assert_eq!(
            filter_from(None, Some("   ".to_owned()), Lang::Zh).expect("blank"),
            OriginFilter::All
        );
    }

    #[test]
    fn invalid_filter_is_a_usage_error() {
        let err = filter_from(Some("a/b"), None, Lang::Zh).expect_err("slash");
        assert_eq!(
            err.downcast_ref::<AppDiagnostic>().map(|d| d.exit_code),
            Some(2)
        );
    }

    #[test]
    fn remote_name_rejects_reserved_and_invalid() {
        assert!(remote_name("prod-box", Lang::Zh).is_ok());
        for bad in ["", "a/b", "all", "local", "remote", "prod box"] {
            let err = remote_name(bad, Lang::Zh).expect_err(bad);
            assert_eq!(
                err.downcast_ref::<AppDiagnostic>().map(|d| d.exit_code),
                Some(2),
                "{bad}"
            );
        }
    }

    #[test]
    fn member_returns_none_for_local_and_errors_for_unpaired() {
        let members = RemoteMembers::empty();
        assert!(
            member(&members, &Origin::Local, Lang::Zh)
                .expect("local")
                .is_none()
        );
        let err =
            member(&members, &Origin::Remote(name("prod-box")), Lang::Zh).expect_err("unpaired");
        assert_eq!(
            err.downcast_ref::<AppDiagnostic>().map(|d| d.exit_code),
            Some(2)
        );
    }

    #[test]
    fn bare_server_name_is_passed_through() {
        let printer = crate::output::Printer::new(crate::output::OutputFormat::Human, Lang::Zh);
        assert_eq!(resolve_server_name("web", printer).expect("bare"), "web");
    }

    /// 组合列表的最小条目：只保留判定关心的两列
    struct Hit {
        name: &'static str,
        origin: Origin,
    }

    fn hits() -> Vec<Hit> {
        vec![
            Hit {
                name: "web",
                origin: Origin::Local,
            },
            Hit {
                name: "web",
                origin: Origin::Remote(name("prod-box")),
            },
            Hit {
                name: "survival",
                origin: Origin::Remote(name("prod-box")),
            },
            Hit {
                name: "survival",
                origin: Origin::Remote(name("lab")),
            },
        ]
    }

    fn pick<'a>(items: &'a [Hit], raw: &str) -> TargetPick<'a, Hit> {
        let selector = mcsup::service::Selector::parse(raw);
        pick_hit(
            items,
            selector.name(),
            &selector,
            |hit| hit.name,
            |hit| &hit.origin,
        )
    }

    #[test]
    fn pick_prefers_local_when_name_is_unqualified() {
        // web 本地有、prod-box 也有：未限定就是本地优先
        assert!(matches!(pick(&hits(), "web"), TargetPick::Local));
        // 显式 local/ 同样归本地
        assert!(matches!(pick(&hits(), "local/web"), TargetPick::Local));
    }

    #[test]
    fn pick_resolves_unique_remote_hit() {
        let items = hits();
        let TargetPick::Remote(found) = pick(&items, "prod-box/survival") else {
            panic!("expected a remote hit");
        };
        assert_eq!(found.origin, Origin::Remote(name("prod-box")));

        // 未限定且本地没有同名：唯一远端命中
        let only_remote = vec![Hit {
            name: "atm",
            origin: Origin::Remote(name("lab")),
        }];
        assert!(matches!(pick(&only_remote, "atm"), TargetPick::Remote(_)));
    }

    #[test]
    fn pick_reports_ambiguity_across_remotes() {
        assert!(matches!(pick(&hits(), "survival"), TargetPick::Ambiguous));
    }

    #[test]
    fn pick_reports_missing_for_unknown_or_wrong_origin() {
        assert!(matches!(pick(&hits(), "nope"), TargetPick::Missing));
        // 限定到没有这台服务器的远端：不回落、不猜
        assert!(matches!(pick(&hits(), "lab/web"), TargetPick::Missing));
    }
}
