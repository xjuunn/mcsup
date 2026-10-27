//! `server set` / `server version`：服务属性与核心信息
//!
//! `set` 缺 key / value 时在 TTY 下交互补齐（键列表选择、按类型输入或
//! 选择），非交互缺失即用法错误（2）；校验与持久化在 service。
//! `version` 只读注册元数据与核心文件信息。

use crate::bootstrap;
use crate::diagnostic::AppDiagnostic;
use crate::locale::Lang;
use crate::output::{self, Printer};
use crate::progress::{ScanProgress, TaskSpinner};
use crate::prompts;
use anyhow::{Context, Result, anyhow};
use comfy_table::{ContentArrangement, Table};
use mcsup::domain::server::settings::{BACKUP_SCOPE_ALL, ServerSettings, SettingKey};
use mcsup::port::JavaRuntimeRepository as _;
use mcsup::protocol::cli::{JarJson, SetJson, VersionJson};
use mcsup::service::{DEFAULT_RAM_MIB, JavaService, ServerSettingsService, SettingChange};

fn settings_service() -> Result<ServerSettingsService> {
    Ok(ServerSettingsService::new(
        bootstrap::open_registry()?,
        bootstrap::open_java_service()?,
    ))
}

/// 设置目标：本地走 service，远端走帧
///
/// 远端交互默认值直接取对端注册记录里的属性；判定与持久化仍在对端 service，
/// 客户端只搬键值。
enum Target {
    /// 本机：service 直接持久化
    Local(ServerSettingsService),
    /// 远端：属性在对端
    ///
    /// `RemoteTarget` 带着整条注册记录，直接内联会让枚举按最大变体分配
    /// （每次栈上 ~400 字节）；装箱后两个变体都只有一个指针大小。
    Remote(Box<super::origin::RemoteTarget>),
}

impl Target {
    fn open(name: &str, lang: Lang) -> Result<Self> {
        if let Some(remote) = super::origin::remote_target(name, lang)? {
            return Ok(Self::Remote(Box::new(remote)));
        }
        Ok(Self::Local(settings_service()?))
    }

    /// 当前属性（交互默认值）
    fn current(&self, name: &str) -> Result<ServerSettings> {
        match self {
            Self::Local(service) => Ok(service.read(name)?.1),
            Self::Remote(remote) => Ok(remote.meta.settings.clone()),
        }
    }

    /// 写入并返回规范化后的结果
    fn set(
        &self,
        name: &str,
        key: SettingKey,
        value: &str,
        printer: Printer,
    ) -> Result<SettingChange> {
        let spinner = TaskSpinner::new(
            crate::i18n::pick(
                printer.lang(),
                format!("正在设置 {name} {key}…"),
                format!("setting {name} {key}…"),
            ),
            printer.is_human(),
        );
        match self {
            Self::Local(service) => {
                let result = service.set(
                    name,
                    key,
                    value,
                    &mut || {
                        spinner.set_message(crate::i18n::pick(
                            printer.lang(),
                            "正在扫描本机 Java…",
                            "scanning the host for Java…",
                        ));
                    },
                    &mut |_| {},
                );
                spinner.finish();
                Ok(result?)
            }
            Self::Remote(remote) => {
                let result =
                    remote
                        .member
                        .servers
                        .set_setting(&remote.server_id, key.as_str(), value);
                spinner.finish();
                let (settings, value) = result.with_context(|| {
                    crate::i18n::pick(
                        printer.lang(),
                        format!("在 {} 设置 {name} {key} 失败", remote.member.name),
                        format!("failed to set {name} {key} on {}", remote.member.name),
                    )
                })?;
                Ok(SettingChange {
                    key,
                    value,
                    settings,
                })
            }
        }
    }

    /// 已注册 Java 运行时（`java-version` 交互选择用）
    fn runtimes(&self, printer: Printer) -> Result<Vec<mcsup::domain::java::JavaRuntime>> {
        match self {
            Self::Local(_) => list_runtimes(&bootstrap::open_java_service()?, printer),
            Self::Remote(remote) => remote.member.java.list().with_context(|| {
                crate::i18n::pick(
                    printer.lang(),
                    format!("列出 {} 上的 Java 失败", remote.member.name),
                    format!("failed to list Java runtimes on {}", remote.member.name),
                )
            }),
        }
    }
}

/// `server set`：键 / 值缺失时交互补齐（非交互 → 退出码 2），校验后持久化
///
/// # Errors
///
/// 服务器不存在（3）、键或值非法（2）、Java 大版本未注册（3）时返回错误
pub(crate) fn set(
    name: &str,
    key: Option<&str>,
    value: Option<&str>,
    printer: Printer,
) -> Result<()> {
    let target = Target::open(name, printer.lang())?;
    let interactive = printer.is_interactive();

    // 1. 键：显式解析（未知键 → 用法错误并列出全部键）；缺省交互选择
    let key = match key {
        Some(raw) => parse_key(printer.lang(), raw)?,
        None if interactive => prompt_key(printer)?,
        None => {
            return Err(AppDiagnostic::usage(
                crate::i18n::pick(
                    printer.lang(),
                    "server set 缺少属性键",
                    "server set requires a property key",
                ),
                Some(crate::i18n::pick(
                    printer.lang(),
                    format!(
                        "非交互模式：mcsup server set <name> <key> <value>；可用键：{}",
                        join_keys()
                    ),
                    format!(
                        "non-interactive: mcsup server set <name> <key> <value>; keys: {}",
                        join_keys()
                    ),
                )),
            )
            .into());
        }
    };

    // 2. 值：显式直通；缺省按键类型交互补齐
    let raw_value = match value {
        Some(raw) => raw.to_owned(),
        None if interactive => prompt_value(&target, name, key, printer)?,
        None => {
            return Err(AppDiagnostic::usage(
                crate::i18n::pick(
                    printer.lang(),
                    format!("server set 缺少 {key} 的值"),
                    format!("server set requires a value for {key}"),
                ),
                Some(value_hint(printer.lang(), key)),
            )
            .into());
        }
    };

    // 3. 校验 + 持久化（java-version 可能触发首次 Java 扫描，保持回显）
    let change: SettingChange = target.set(name, key, &raw_value, printer)?;

    printer.print_outcome(
        &crate::i18n::pick(
            printer.lang(),
            format!("已设置 {name} {key} = {}", change.value),
            format!("set {name} {key} = {}", change.value),
        ),
        &SetJson {
            name: name.to_owned(),
            key: key.as_str().to_owned(),
            value: change.value,
            settings: change.settings.into(),
        },
    )?;
    // autostart 目前只持久化、无调度器消费：如实告知，不假装生效
    if key == SettingKey::Autostart {
        printer.print_section(&crate::i18n::pick(
            printer.lang(),
            "注意: autostart 已记录但尚未生效（自动拉起调度器待实现）",
            "note: autostart is recorded but not enforced yet (the boot scheduler is not implemented)",
        ));
    }
    Ok(())
}

/// `server version`：注册版本 + 核心来源 / 大小 / 校验和
///
/// # Errors
///
/// 服务器不存在时返回错误
pub(crate) fn version(name: &str, printer: Printer) -> Result<()> {
    let reg = bootstrap::open_registry_with_remotes()?;
    let srv = reg.get(name).with_context(|| {
        crate::i18n::pick(
            printer.lang(),
            format!("服务器 {name} 不存在"),
            format!("server {name} does not exist"),
        )
    })?;
    let unknown_source = crate::i18n::pick(printer.lang(), "未知", "unknown");
    let source = bootstrap::pick_source(&srv.meta.variant, &bootstrap::jar_sources()?)
        .map_or(unknown_source.as_str(), |source| source.name());
    let jar = srv.meta.jar.as_ref().map(|jar| JarJson {
        path: jar.path.display().to_string(),
        size: jar.size,
        checksum_algo: jar.checksum.algorithm().as_str(),
        checksum: jar.checksum.hex().to_owned(),
        verified: jar.checksum.is_verified(),
    });
    let payload = VersionJson {
        name: srv.meta.name.as_str(),
        version: srv.meta.version.as_str(),
        variant: srv.meta.variant.as_str(),
        source,
        jar,
    };

    if printer.is_human() {
        let mut table = Table::new();
        table.set_content_arrangement(ContentArrangement::Dynamic);
        let l = printer.lang();
        table.set_header([
            crate::i18n::text(l, "项", "field"),
            crate::i18n::text(l, "值", "value"),
        ]);
        table.add_row([crate::i18n::text(l, "名称", "name"), payload.name]);
        table.add_row([crate::i18n::text(l, "版本", "version"), payload.version]);
        table.add_row([crate::i18n::text(l, "变体", "variant"), payload.variant]);
        table.add_row([crate::i18n::text(l, "来源", "source"), payload.source]);
        match payload.jar.as_ref() {
            Some(jar) => {
                table.add_row([crate::i18n::text(l, "核心", "core"), jar.path.as_str()]);
                table.add_row([
                    crate::i18n::text(l, "大小", "size"),
                    output::human_bytes(jar.size).as_str(),
                ]);
                table.add_row([
                    crate::i18n::text(l, "校验和", "checksum"),
                    checksum_label(l, jar).as_str(),
                ]);
            }
            None => {
                table.add_row([
                    crate::i18n::text(l, "核心", "core"),
                    crate::i18n::text(l, "尚未安装", "not installed"),
                ]);
            }
        }
        output::print_table(&table);
        if payload.jar.is_none() {
            printer.print_section(&crate::i18n::pick(
                printer.lang(),
                format!("可执行 mcsup server install {name} 安装核心"),
                format!("run mcsup server install {name} to install the core"),
            ));
        }
        Ok(())
    } else {
        let line = serde_json::to_string(&payload)?;
        output::print_json_line(&line);
        Ok(())
    }
}

/// 校验和的展示文案
///
/// 上游未提供哈希（如 Fabric 服务端 JAR）时如实说明，不显示残缺的 `sha1:`；
/// 有哈希时统一为 `算法:十六进制`。
fn checksum_label(lang: Lang, jar: &JarJson) -> String {
    if jar.verified {
        format!("{}:{}", jar.checksum_algo, jar.checksum)
    } else {
        crate::i18n::pick(
            lang,
            "未提供（上游无校验和）",
            "not provided (upstream has no checksum)",
        )
    }
}

/// 键解析：未知键报用法错误（2）并附全部可用键
fn parse_key(lang: Lang, raw: &str) -> Result<SettingKey> {
    SettingKey::parse(raw).map_err(|err| {
        AppDiagnostic::usage(
            crate::i18n::pick(
                lang,
                format!("未知服务属性键: {}", err.raw),
                format!("unknown server property key: {}", err.raw),
            ),
            Some(crate::i18n::pick(
                lang,
                format!("可用键：{}", join_keys()),
                format!("available keys: {}", join_keys()),
            )),
        )
        .into()
    })
}

fn join_keys() -> String {
    SettingKey::ALL
        .iter()
        .map(|key| key.as_str())
        .collect::<Vec<_>>()
        .join("、")
}

/// 键的简短说明（交互列表与用法提示共用同一口径）
///
/// 标签是 `key — 说明`：前缀始终是稳定的 `key`，回映不依赖说明的语种。
fn key_label(lang: Lang, key: SettingKey) -> String {
    format!("{key} — {}", key_help(lang, key))
}

fn key_help(lang: Lang, key: SettingKey) -> &'static str {
    match (lang, key) {
        (Lang::Zh, SettingKey::JavaVersion) => "绑定 JVM 大版本",
        (Lang::En, SettingKey::JavaVersion) => "bind a JVM major version",
        (Lang::Zh, SettingKey::Ram) => "启动内存上限",
        (Lang::En, SettingKey::Ram) => "startup heap limit",
        (Lang::Zh, SettingKey::Autostart) => "开机自动拉起",
        (Lang::En, SettingKey::Autostart) => "start automatically on boot",
        (Lang::Zh, SettingKey::BackupScope) => "备份范围（逗号分隔的顶层目录；all 表示全部）",
        (Lang::En, SettingKey::BackupScope) => {
            "backup scope (comma separated top-level entries; all means everything)"
        }
    }
}

/// 交互键选择：标签带说明，按前缀回映到键
fn prompt_key(printer: Printer) -> Result<SettingKey> {
    prompts::step(
        printer,
        1,
        2,
        &crate::i18n::pick(printer.lang(), "服务属性", "server property"),
    );
    let labels: Vec<String> = SettingKey::ALL
        .iter()
        .copied()
        .map(|key| key_label(printer.lang(), key))
        .collect();
    let picked = prompts::select_filter(
        printer.lang(),
        &crate::i18n::pick(printer.lang(), "服务属性", "server property"),
        &labels,
    )?;
    key_from_label(&picked).ok_or_else(|| {
        anyhow!(crate::i18n::pick(
            printer.lang(),
            format!("无效属性键: {picked}"),
            format!("invalid property key: {picked}")
        ))
    })
}

/// 标签（`key（说明）`）→ 键；按 `key（` 前缀匹配
fn key_from_label(label: &str) -> Option<SettingKey> {
    SettingKey::ALL
        .iter()
        .copied()
        .find(|key| label.starts_with(&format!("{} — ", key.as_str())))
}

/// 交互值补齐：java-version 从已注册大版本选择，ram 输入（带当前默认），
/// autostart 二选一
fn prompt_value(target: &Target, name: &str, key: SettingKey, printer: Printer) -> Result<String> {
    prompts::step(
        printer,
        2,
        2,
        &crate::i18n::pick(
            printer.lang(),
            format!("{key} 的值"),
            format!("value for {key}"),
        ),
    );
    let current = target.current(name)?;
    match key {
        SettingKey::JavaVersion => prompt_java_major(target, printer),
        SettingKey::Ram => {
            let default = current
                .value_of(SettingKey::Ram)
                .unwrap_or_else(|| format!("{DEFAULT_RAM_MIB}M"));
            prompts::input_text(
                printer.lang(),
                &crate::i18n::pick(printer.lang(), "启动内存上限", "heap limit"),
                &default,
                &crate::i18n::pick(
                    printer.lang(),
                    "如 2G / 1024M；回车保持当前值",
                    "e.g. 2G / 1024M; enter keeps the current value",
                ),
            )
        }
        SettingKey::Autostart => {
            let default = current.autostart;
            let picked = prompts::select_bool(
                printer.lang(),
                &crate::i18n::pick(printer.lang(), "开机自动拉起", "start on boot"),
                default,
            )?;
            Ok(picked.to_string())
        }
        SettingKey::BackupScope => {
            let default = current
                .value_of(SettingKey::BackupScope)
                .unwrap_or_else(|| BACKUP_SCOPE_ALL.to_owned());
            prompts::input_text(
                printer.lang(),
                &crate::i18n::pick(printer.lang(), "备份范围", "backup scope"),
                &default,
                &crate::i18n::pick(
                    printer.lang(),
                    "逗号分隔的顶层目录，如 world,mods；all 表示整个服务器目录",
                    "comma separated top-level entries, e.g. world,mods; all means the whole server directory",
                ),
            )
        }
    }
}

/// 从已注册 Java 运行时的大版本里选择（首次扫描带回显）
fn prompt_java_major(target: &Target, printer: Printer) -> Result<String> {
    let runtimes = target.runtimes(printer)?;
    let mut majors: Vec<i64> = runtimes.iter().map(|rt| rt.major.as_i64()).collect();
    majors.sort_unstable();
    majors.dedup();
    if majors.is_empty() {
        return Err(anyhow!(crate::i18n::pick(
            printer.lang(),
            "未发现已登记的 Java，可先运行 mcsup java install（远端加 --origin）",
            "no registered Java found; run `mcsup java install` first (add --origin for a remote)",
        )));
    }
    let labels: Vec<String> = majors.iter().map(ToString::to_string).collect();
    prompts::select_filter(
        printer.lang(),
        &crate::i18n::pick(printer.lang(), "JVM 大版本", "JVM major version"),
        &labels,
    )
}

/// 已注册运行时；未扫描过则扫描（与 `java list` 同一套回显口径）
fn list_runtimes(
    java: &JavaService,
    printer: Printer,
) -> Result<Vec<mcsup::domain::java::JavaRuntime>> {
    let mut progress = ScanProgress::new(printer.is_human(), printer.lang());
    let runtimes = java
        .list_or_scan(
            &mut || {
                printer.print_section(&crate::i18n::pick(
                    printer.lang(),
                    "正在扫描本机 Java…",
                    "scanning the host for Java…",
                ));
            },
            &mut |found| {
                progress.on_found(&found.path, &found.version);
            },
        )
        .context(crate::i18n::pick(
            printer.lang(),
            "列出 Java 失败",
            "failed to list Java runtimes",
        ))?;
    progress.finish();
    Ok(runtimes)
}

/// 非交互缺值时的按键用法提示
fn value_hint(lang: Lang, key: SettingKey) -> String {
    let example = match key {
        SettingKey::JavaVersion => "21".to_owned(),
        SettingKey::Ram => "2G".to_owned(),
        SettingKey::Autostart => "true".to_owned(),
        SettingKey::BackupScope => "world,mods".to_owned(),
    };
    crate::i18n::pick(
        lang,
        format!("非交互模式：mcsup server set <name> {key} <value>（如 {example}）"),
        format!("non-interactive: mcsup server set <name> {key} <value> (e.g. {example})"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_key_accepts_all_documented_keys() {
        assert_eq!(
            parse_key(Lang::Zh, "java-version").expect("java"),
            SettingKey::JavaVersion
        );
        assert_eq!(parse_key(Lang::Zh, "ram").expect("ram"), SettingKey::Ram);
        assert_eq!(
            parse_key(Lang::Zh, "autostart").expect("autostart"),
            SettingKey::Autostart
        );
    }

    #[test]
    fn parse_key_unknown_lists_available_keys() {
        let err = parse_key(Lang::Zh, "max-players").expect_err("unknown");
        assert_eq!(Printer::exit_code(&err), 2);
        let msg = err.to_string();
        assert!(msg.contains("max-players"), "{msg}");
        let rendered = format!(
            "{:?}",
            miette::Report::new(err.downcast::<AppDiagnostic>().expect("usage 诊断"))
        );
        assert!(rendered.contains("java-version"), "{rendered}");
    }

    /// 标签必须以 `key（` 开头，回映按同一前缀
    #[test]
    fn key_label_roundtrips_through_prefix() {
        for key in SettingKey::ALL {
            assert_eq!(key_from_label(&key_label(Lang::Zh, key)), Some(key));
        }
        assert_eq!(key_from_label("ram"), None);
        assert_eq!(key_from_label("其他（说明）"), None);
    }

    fn jar(verified: bool) -> JarJson {
        JarJson {
            path: "server.jar".to_owned(),
            size: 1,
            checksum_algo: "sha1",
            checksum: if verified {
                "abc".to_owned()
            } else {
                String::new()
            },
            verified,
        }
    }

    /// 有真实哈希时显示 `算法:十六进制`
    #[test]
    fn checksum_label_shows_algo_and_hex_when_verified() {
        assert_eq!(checksum_label(Lang::Zh, &jar(true)), "sha1:abc");
    }

    /// 上游无哈希（Fabric）时如实说明，不能显示残缺的 `sha1:`
    #[test]
    fn checksum_label_explains_missing_checksum() {
        let label = checksum_label(Lang::Zh, &jar(false));
        assert_eq!(label, "未提供（上游无校验和）");
        assert!(!label.starts_with("sha1:"), "{label}");
    }
}
