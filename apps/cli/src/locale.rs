//! 界面语言解析
//!
//! 优先级：`--lang` > `MCSUP_LANG` > 平台 locale（[`sys_locale`]）> 英文。
//!
//! 语言只影响面向人的文案；机器可读的输出（`--output json` 的 stdout、
//! 错误分类、退出码）始终与语言无关。
//!
//! 终端不能正确显示非 ASCII 时回退英文：否则中文提示会变成乱码，比英文更难读。

use std::io::IsTerminal;

/// 界面语言
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lang {
    /// 中文（简体）
    Zh,
    /// 英文
    En,
}

impl Lang {
    /// 解析语言标签：`zh` / `zh-CN` / `zh_CN.UTF-8` 等一律视为中文，其余英文
    ///
    /// 只取主语言子标签，`zh-Hant` 也按中文处理——目前没有繁体目录，回退到
    /// 同语言总比回退到英文更接近用户预期。
    #[must_use]
    pub(crate) fn parse(tag: &str) -> Option<Self> {
        let tag = tag.trim();
        if tag.is_empty() {
            return None;
        }
        // BCP 47 用 `-`，POSIX locale 用 `_` 且可带 `.codeset`
        let primary = tag
            .split(['-', '_'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match primary.as_str() {
            "zh" => Some(Self::Zh),
            "en" => Some(Self::En),
            // 认不出的语言标签不当作「用户指定了英文」，交给下一级继续解析
            _ => None,
        }
    }

    /// 切换文案用
    #[must_use]
    pub(crate) const fn is_zh(self) -> bool {
        matches!(self, Self::Zh)
    }
}

/// `--lang` 的取值
///
/// `auto` 走自动解析；`zh` / `en` 强制指定。`--lang en` 在中文系统上也要
/// 生效，所以显式值不经过终端编码回退。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LangChoice {
    /// 按环境自动解析
    Auto,
    /// 强制中文
    Zh,
    /// 强制英文
    En,
}

impl LangChoice {
    /// 解析命令行取值；值由 clap 的 `value_parser` 限定，不会走到兜底
    #[must_use]
    pub(crate) fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "zh" => Self::Zh,
            "en" => Self::En,
            _ => Self::Auto,
        }
    }
}

/// 解析结果：语言 + 是否由用户显式指定
///
/// 「显式」指 `--lang` 或 `MCSUP_LANG` 点名了某种语言。终端码页回退只作用于
/// 非显式场景，见 [`detect`]。
#[derive(Debug, Clone, Copy)]
struct Resolved {
    /// 最终语言
    lang: Lang,
    /// 是否来自用户显式指定
    explicit: bool,
}

/// 解析最终语言
///
/// 自动路径的每一级都用 [`Lang::parse`] 解析，认不出的标签继续向下一级回退，
/// 最终落到 [`Lang::En`]。
#[must_use]
fn resolve(choice: LangChoice, env_lang: Option<&str>, system: Option<&str>) -> Resolved {
    // 用户有没有表态：--lang 优先，其次 MCSUP_LANG；认不出的标签继续往下
    let pinned = match choice {
        LangChoice::Zh => Some(Lang::Zh),
        LangChoice::En => Some(Lang::En),
        LangChoice::Auto => env_lang.and_then(Lang::parse),
    };
    if let Some(lang) = pinned {
        return Resolved {
            lang,
            explicit: true,
        };
    }
    // 没人表态：平台 locale 只是默认值，不算用户指定
    Resolved {
        lang: system.and_then(Lang::parse).unwrap_or(Lang::En),
        explicit: false,
    }
}

/// 按真实环境解析语言
///
/// 终端不能显示 UTF-8 时回退英文：此时中文提示只会是乱码。用户显式点名中文
/// 的情形例外——那时回退等于无视指令，宁可让用户看到乱码再自行调整终端。
#[must_use]
pub(crate) fn detect(choice: LangChoice) -> Lang {
    let env_lang = std::env::var("MCSUP_LANG").ok();
    let system = sys_locale::get_locale();
    let resolved = resolve(choice, env_lang.as_deref(), system.as_deref());
    if !resolved.explicit && resolved.lang.is_zh() && !stdout_supports_utf8() {
        return Lang::En;
    }
    resolved.lang
}

/// 进程级界面语言
///
/// clap 的 `about` / `help` 参数在 `Cli::command()` 构建命令树时求值，那条路径
/// 拿不到 `Cli`，只能从进程状态读。写入只发生在启动时一次，之后全是读，
/// 不存在并发改写；测试里也用它切换断言语言。
static CURRENT: std::sync::OnceLock<std::sync::RwLock<Lang>> = std::sync::OnceLock::new();

fn cell() -> &'static std::sync::RwLock<Lang> {
    CURRENT.get_or_init(|| std::sync::RwLock::new(Lang::En))
}

/// 当前进程语言；未设置时英文
#[must_use]
pub(crate) fn current() -> Lang {
    // 锁中毒只会让语言停在旧值，不影响正确性，故用 unwrap_or_else 兜底
    *cell()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 设置进程语言
pub(crate) fn set(lang: Lang) {
    *cell()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = lang;
}

/// stdout 能否正确显示非 ASCII
///
/// 非终端（管道 / 重定向到文件）不做编码判断：此时没有「显示」这回事，
/// 按用户环境语言输出即可。
#[must_use]
fn stdout_supports_utf8() -> bool {
    if !std::io::stdout().is_terminal() {
        return true;
    }
    legacy_console()
}

/// 从原始命令行里预扫 `--lang`
///
/// 帮助渲染发生在 clap 解析失败之后，那时拿不到 `Cli` 里的 `--lang`，只能
/// 自己扫一遍。只认 `--lang` 与 `--lang=VALUE` 两种写法，`zh` / `en` 直接
/// 生效，`auto` 与未给出都走自动解析。
#[must_use]
fn prescan_lang(args: &[String]) -> LangChoice {
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--lang=") {
            return LangChoice::parse(value);
        }
        if arg == "--lang" {
            return iter
                .next()
                .map_or(LangChoice::Auto, |value| LangChoice::parse(value));
        }
    }
    LangChoice::Auto
}

impl Lang {
    /// 帮助 / 版本渲染用：遵循 `--lang`、`MCSUP_LANG` 与平台 locale
    #[must_use]
    pub(crate) fn detect_help() -> Self {
        detect(prescan_lang(&std::env::args().collect::<Vec<_>>()))
    }
}

#[cfg(windows)]
fn legacy_console() -> bool {
    // 能否显示中文取决于控制台输出代码页是否为 65001，而 workspace 禁止
    // unsafe，读不到 GetConsoleOutputCP。退而看字符集环境提示：用户显式
    // 声明了非 UTF-8 的 locale 时才判定为不能显示；未声明按 UTF-8 处理。
    !charset_declared_non_utf8()
}

/// 环境是否显式声明了非 UTF-8 字符集
///
/// 只在 locale 形如 `zh_CN.GBK` 这类带非 UTF 代码集时判定为真；`C` / `POSIX`
/// 与只有语言部分的取值都不算声明。中文文案混着问号比英文更难读，命中时回退英文。
#[cfg(windows)]
fn charset_declared_non_utf8() -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"].into_iter().any(|key| {
        let Ok(value) = std::env::var(key) else {
            return false;
        };
        let value = value.to_ascii_lowercase();
        if value.is_empty() || value == "c" || value == "posix" {
            return false;
        }
        value
            .split_once('.')
            .is_some_and(|(_, codeset)| !codeset.contains("utf"))
    })
}

#[cfg(not(windows))]
fn legacy_console() -> bool {
    // Unix 终端按字节透传，UTF-8 由终端模拟器解码
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_bcp47_and_posix() {
        assert_eq!(Lang::parse("zh-CN"), Some(Lang::Zh));
        assert_eq!(Lang::parse("zh_CN.UTF-8"), Some(Lang::Zh));
        assert_eq!(Lang::parse("ZH"), Some(Lang::Zh));
        assert_eq!(Lang::parse("zh-Hant-TW"), Some(Lang::Zh));
        assert_eq!(Lang::parse("en-US"), Some(Lang::En));
        assert_eq!(Lang::parse("en_US.UTF-8"), Some(Lang::En));
        assert_eq!(Lang::parse("ja-JP"), None);
        assert_eq!(Lang::parse(""), None);
        assert_eq!(Lang::parse("   "), None);
    }

    /// 显式 `--lang` 压过环境变量与系统 locale
    #[test]
    fn explicit_choice_wins() {
        let en = resolve(LangChoice::En, Some("zh_CN.UTF-8"), Some("zh-CN"));
        assert_eq!(en.lang, Lang::En);
        assert!(en.explicit);
        let zh = resolve(LangChoice::Zh, Some("en_US.UTF-8"), Some("en-US"));
        assert_eq!(zh.lang, Lang::Zh);
        assert!(zh.explicit);
    }

    #[test]
    fn env_lang_wins_over_system() {
        let en = resolve(LangChoice::Auto, Some("en_US.UTF-8"), Some("zh-CN"));
        assert_eq!(en.lang, Lang::En);
        assert!(en.explicit, "MCSUP_LANG 是用户表态");
        let zh = resolve(LangChoice::Auto, Some("zh_CN.UTF-8"), Some("en-US"));
        assert_eq!(zh.lang, Lang::Zh);
        assert!(zh.explicit);
    }

    /// 环境变量认不出时继续看系统 locale，而不是直接英文
    ///
    /// 认不出的 `MCSUP_LANG` 不算表态：语言来自平台 locale，故不是 explicit。
    #[test]
    fn unknown_env_falls_back_to_system() {
        let r = resolve(LangChoice::Auto, Some("ja_JP.UTF-8"), Some("zh-CN"));
        assert_eq!(r.lang, Lang::Zh);
        assert!(!r.explicit, "认不出的 MCSUP_LANG 不构成用户指定");
    }

    /// 两级都认不出（或都没有）才回退英文
    #[test]
    fn unresolvable_falls_back_to_english() {
        assert_eq!(resolve(LangChoice::Auto, None, None).lang, Lang::En);
        assert_eq!(
            resolve(LangChoice::Auto, None, Some("C.UTF-8")).lang,
            Lang::En
        );
        assert_eq!(
            resolve(LangChoice::Auto, Some(""), Some("POSIX")).lang,
            Lang::En
        );
    }

    #[test]
    fn choice_parse_defaults_to_auto() {
        assert_eq!(LangChoice::parse("zh"), LangChoice::Zh);
        assert_eq!(LangChoice::parse("EN"), LangChoice::En);
        assert_eq!(LangChoice::parse("auto"), LangChoice::Auto);
        assert_eq!(LangChoice::parse(""), LangChoice::Auto);
    }
}
