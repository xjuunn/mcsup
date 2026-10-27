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
}
