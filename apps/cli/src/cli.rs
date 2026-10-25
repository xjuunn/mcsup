use crate::output::OutputFormat;
use crate::verbosity::ZhLevel;
use clap::{Parser, Subcommand};
use clap_verbosity_flag::Verbosity;
use std::path::PathBuf;

/// 日志默认回看行数：`logs --tail` 缺省值，`console` 附加时回放的历史行数
pub(crate) const LOG_TAIL_DEFAULT: usize = 30;

#[derive(Debug, Parser)]
#[command(
    name = "mcsup",
    version,
    about = crate::meta::about(),
    // propagate_version = true,
    arg_required_else_help = true
)]
pub(crate) struct Cli {
    #[command(flatten)]
    pub(crate) verbose: Verbosity<ZhLevel>,
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "human",
        help = crate::meta::output_arg()
    )]
    pub(crate) output: OutputFormat,
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = crate::meta::config_arg()
    )]
    pub(crate) config: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        value_name = "LANG",
        default_value = "auto",
        value_parser = ["auto", "zh", "en"],
        help = crate::meta::lang_arg()
    )]
    pub(crate) lang: String,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    #[command(about = crate::meta::workspace())]
    #[command(visible_alias = "ws")]
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    #[command(about = crate::meta::server())]
    #[command(visible_alias = "srv")]
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
    #[command(about = crate::meta::modpack())]
    #[command(visible_alias = "mp")]
    Modpack {
        #[command(subcommand)]
        action: ModpackAction,
    },
    #[command(about = crate::meta::java())]
    #[command(visible_alias = "jv")]
    Java {
        #[command(subcommand)]
        action: JavaAction,
    },
    #[command(about = crate::meta::remote())]
    #[command(visible_alias = "rmt")]
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },
    #[command(about = crate::meta::doctor())]
    #[command(visible_alias = "doc")]
    Doctor {
        #[arg(
            long,
            value_name = "KIND",
            value_delimiter = ',',
            help = crate::meta::doctor_only_arg()
        )]
        only: Vec<String>,
        #[arg(long, help = crate::meta::doctor_strict_arg())]
        strict: bool,
        #[arg(long, help = crate::meta::doctor_offline_arg())]
        offline: bool,
    },
    #[command(about = crate::meta::config())]
    #[command(visible_alias = "cfg")]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    #[command(about = crate::meta::update())]
    #[command(visible_alias = "up")]
    Update,
    #[command(about = crate::meta::completions())]
    #[command(visible_alias = "comp")]
    Completions { shell: clap_complete::Shell },
    #[command(about = crate::meta::version())]
    #[command(visible_alias = "ver")]
    Version,
    #[command(name = "__supervise", hide = true, about = crate::meta::supervise())]
    Supervise {
        #[arg(long, value_name = "PATH", help = crate::meta::supervise_dir_arg())]
        dir: PathBuf,
        #[arg(long, value_name = "PATH", help = crate::meta::supervise_java_arg())]
        java: PathBuf,
        #[arg(long, value_name = "PATH", help = crate::meta::supervise_jar_arg())]
        jar: PathBuf,
        #[arg(long, value_name = "MIB", help = crate::meta::supervise_ram_arg())]
        ram: Option<u32>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum WorkspaceAction {
    #[command(about = crate::meta::workspace_init())]
    #[command(visible_alias = "in", visible_alias = "new")]
    Init {
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    #[command(about = crate::meta::workspace_list())]
    #[command(visible_alias = "ls")]
    List,
    #[command(about = crate::meta::workspace_use())]
    #[command(visible_alias = "sw")]
    Use { name: Option<String> },
    #[command(about = crate::meta::workspace_remove())]
    #[command(visible_alias = "rm")]
    Remove {
        name: String,
        #[arg(long)]
        purge: bool,
    },
    #[command(name = "doctor", about = crate::meta::workspace_doctor())]
    #[command(visible_alias = "doc")]
    Doctor {
        #[arg(
            long,
            value_name = "KIND",
            value_delimiter = ',',
            help = crate::meta::workspace_doctor_only_arg()
        )]
        only: Vec<String>,
        #[arg(long, help = crate::meta::workspace_doctor_strict_arg())]
        strict: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ServerAction {
    #[command(about = crate::meta::server_list())]
    #[command(visible_alias = "ls")]
    List,
    #[command(about = crate::meta::server_create())]
    #[command(visible_alias = "cr")]
    Create {
        #[arg(value_name = "NAME", help = crate::meta::server_create_name_arg())]
        name: Option<String>,
        #[arg(
            long,
            value_name = "VER",
            help = crate::meta::server_create_version_arg()
        )]
        version: Option<String>,
        #[arg(long, value_name = "VAR")]
        variant: Option<String>,
        #[arg(long, value_name = "WS")]
        workspace: Option<String>,
    },
    #[command(about = crate::meta::server_install())]
    #[command(visible_alias = "in")]
    Install {
        name: String,
        #[arg(long, value_name = "VER")]
        version: Option<String>,
    },
    #[command(about = crate::meta::server_start())]
    #[command(visible_alias = "up")]
    Start {
        name: String,
        #[arg(long, help = crate::meta::server_accept_eula_arg())]
        accept_eula: bool,
    },
    #[command(about = crate::meta::server_stop())]
    #[command(visible_alias = "down")]
    Stop {
        name: String,
        #[arg(
            long,
            value_name = "SEC",
            default_value_t = 30,
            help = crate::meta::server_grace_arg()
        )]
        grace: u64,
        #[arg(long, help = crate::meta::server_force_arg())]
        force: bool,
    },
    #[command(about = crate::meta::server_restart())]
    #[command(visible_alias = "rs")]
    Restart {
        name: String,
        #[arg(long, help = crate::meta::server_accept_eula_arg())]
        accept_eula: bool,
        #[arg(
            long,
            value_name = "SEC",
            default_value_t = 30,
            help = crate::meta::server_grace_arg()
        )]
        grace: u64,
        #[arg(long, help = crate::meta::server_force_arg())]
        force: bool,
    },
    #[command(about = crate::meta::server_status())]
    #[command(visible_alias = "st")]
    Status { name: String },
    #[command(about = crate::meta::server_console())]
    #[command(visible_alias = "con")]
    Console { name: String },
    #[command(about = crate::meta::server_send())]
    #[command(visible_alias = "snd")]
    Send { name: String, command: String },
    #[command(about = crate::meta::server_logs())]
    #[command(visible_alias = "log")]
    Logs {
        name: String,
        #[arg(
            long,
            value_name = "N",
            default_value_t = LOG_TAIL_DEFAULT,
            help = crate::meta::server_logs_tail_arg()
        )]
        tail: usize,
        #[arg(long, help = crate::meta::server_logs_follow_arg())]
        follow: bool,
    },
    #[command(about = crate::meta::server_props())]
    #[command(visible_alias = "prop")]
    Props { name: String, kv: Vec<String> },
    #[command(about = crate::meta::server_set())]
    #[command(visible_alias = "cfg")]
    Set {
        name: String,
        #[arg(value_name = "KEY", help = crate::meta::server_set_key_arg())]
        key: Option<String>,
        #[arg(value_name = "VALUE", help = crate::meta::server_set_value_arg())]
        value: Option<String>,
    },
    #[command(about = crate::meta::server_remove())]
    #[command(visible_alias = "rm")]
    Remove {
        name: String,
        #[arg(long)]
        purge: bool,
    },
    #[command(about = crate::meta::server_link())]
    #[command(visible_alias = "ln")]
    Link {
        #[arg(help = crate::meta::server_link_path_arg())]
        path: PathBuf,
        #[arg(
            long,
            value_name = "NAME",
            help = crate::meta::server_link_name_arg()
        )]
        name: Option<String>,
        #[arg(
            long,
            value_name = "WS",
            help = crate::meta::server_workspace_arg()
        )]
        workspace: Option<String>,
    },
    #[command(about = crate::meta::server_version())]
    #[command(visible_alias = "ver")]
    Version { name: String },
    #[command(about = crate::meta::server_backup())]
    #[command(visible_alias = "bak")]
    Backup {
        #[command(subcommand)]
        action: BackupAction,
    },
    #[command(about = crate::meta::server_mod())]
    #[command(visible_alias = "md")]
    Mod {
        #[command(subcommand)]
        action: ModAction,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum BackupAction {
    #[command(visible_alias = "cr")]
    Create {
        name: String,
        #[arg(long)]
        label: Option<String>,
    },
    #[command(visible_alias = "ls")]
    List { name: String },
    #[command(visible_alias = "rt")]
    Restore {
        name: String,
        id: String,
        #[arg(long, value_name = "NAME")]
        to: Option<String>,
    },
    #[command(visible_alias = "vf")]
    Verify { name: String, id: String },
    #[command(visible_alias = "pr")]
    Prune {
        name: String,
        #[arg(long, value_name = "N")]
        keep: Option<usize>,
        #[arg(long, value_name = "AGE")]
        older_than: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ModAction {
    #[command(visible_alias = "ls")]
    List { name: String },
    #[command(visible_alias = "in")]
    Install { name: String, spec: String },
    #[command(visible_alias = "rm")]
    Remove { name: String, m: String },
    #[command(visible_alias = "up")]
    Update { name: String, m: Option<String> },
    #[command(visible_alias = "od")]
    Outdated { name: String },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ModpackAction {
    #[command(visible_alias = "ls")]
    List { query: Option<String> },
    #[command(visible_alias = "in")]
    Install {
        pack: String,
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        #[arg(long, value_name = "WS")]
        workspace: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum JavaAction {
    #[command(about = crate::meta::java_list())]
    #[command(visible_alias = "ls")]
    List,
    #[command(about = crate::meta::java_scan())]
    #[command(visible_alias = "sc")]
    Scan,
    #[command(about = crate::meta::java_install())]
    #[command(visible_alias = "in")]
    Install {
        #[arg(help = crate::meta::java_install_version_arg())]
        version: Option<String>,
        #[arg(long, help = crate::meta::java_install_jre_arg())]
        jre: bool,
        #[arg(
            long,
            value_name = "VENDOR",
            help = crate::meta::java_install_vendor_arg()
        )]
        vendor: Option<String>,
    },
    #[command(
        arg_required_else_help = true,
        about = crate::meta::java_uninstall()
    )]
    #[command(visible_alias = "un", visible_alias = "rm")]
    Uninstall {
        #[arg(help = crate::meta::java_uninstall_target_arg())]
        target: Option<String>,
        #[arg(long, value_name = "PATH", help = crate::meta::java_uninstall_dir_arg())]
        dir: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum RemoteAction {
    #[command(visible_alias = "ls")]
    List,
    #[command(visible_alias = "dp")]
    Deploy {
        name: String,
        host: String,
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
    },
    #[command(visible_alias = "pg")]
    Ping { name: String },
    #[command(visible_alias = "rm")]
    Remove { name: String },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigAction {
    #[command(about = crate::meta::config_show())]
    #[command(visible_alias = "ls")]
    Show,
    #[command(about = crate::meta::config_set())]
    #[command(visible_alias = "st")]
    Set { key: String, value: String },
    #[command(about = crate::meta::config_unset())]
    #[command(visible_alias = "us")]
    Unset { key: String },
    #[command(about = crate::meta::config_edit())]
    #[command(visible_alias = "ed")]
    Edit,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    /// `logs` 缺省回看 30 行，与 `console` 附加回放行数一致
    #[test]
    fn logs_tail_defaults_to_console_history_lines() {
        let cli = Cli::try_parse_from(["mcsup", "server", "logs", "alpha"]).expect("parse");
        let tail = match cli.command {
            Command::Server {
                action: ServerAction::Logs { tail, .. },
            } => tail,
            other => panic!("expected server logs, got {other:?}"),
        };
        assert_eq!(tail, 30);
        assert_eq!(tail, LOG_TAIL_DEFAULT);
    }

    /// 子命令简写最少两个字母：单字母简称在 shell 里极易误触，一律不提供。
    /// 递归遍历整棵命令树（别名手写维护，这条防线防止后续加进 `a` 这类别名）。
    #[test]
    fn aliases_are_at_least_two_letters() {
        fn walk(cmd: &clap::Command, path: &str) {
            for sub in cmd.get_subcommands() {
                let name = sub.get_name();
                let here = if path.is_empty() {
                    name.to_owned()
                } else {
                    format!("{path} {name}")
                };
                for alias in sub.get_all_aliases() {
                    assert!(
                        alias.chars().count() >= 2,
                        "{here} 的简写 `{alias}` 不足两个字母"
                    );
                    assert!(
                        alias.chars().all(|c| c.is_ascii_lowercase()),
                        "{here} 的简写 `{alias}` 必须是纯小写字母"
                    );
                }
                walk(sub, &here);
            }
        }
        walk(&Cli::command(), "");
    }

    /// 同一层内简写不得重复，否则 clap 会在运行期 panic
    #[test]
    fn aliases_are_unique_within_a_level() {
        fn walk(cmd: &clap::Command, path: &str) {
            let mut seen: Vec<(&str, &str)> = Vec::new();
            for sub in cmd.get_subcommands() {
                for alias in sub.get_all_aliases() {
                    if let Some((owner, _)) = seen.iter().find(|(a, _)| *a == alias) {
                        panic!(
                            "`{path}` 层的简写 `{alias}` 被 `{owner}` 和 `{}` 同时占用",
                            sub.get_name()
                        );
                    }
                    seen.push((alias, sub.get_name()));
                }
            }
            for sub in cmd.get_subcommands() {
                let here = if path.is_empty() {
                    sub.get_name().to_owned()
                } else {
                    format!("{path} {}", sub.get_name())
                };
                walk(sub, &here);
            }
        }
        walk(&Cli::command(), "");
    }

    /// 一级与二级简写与全拼解析到同一命令
    #[test]
    fn shorthand_matches_long_form() {
        let cases: [(&[&str], &[&str]); 7] = [
            (&["ws", "ls"], &["workspace", "list"]),
            (
                &["srv", "bak", "ls", "alpha"],
                &["server", "backup", "list", "alpha"],
            ),
            (
                &["srv", "md", "od", "alpha"],
                &["server", "mod", "outdated", "alpha"],
            ),
            (&["mp", "in", "atm9"], &["modpack", "install", "atm9"]),
            (&["jv", "un", "17"], &["java", "uninstall", "17"]),
            (&["rmt", "ls"], &["remote", "list"]),
            (&["cfg", "st", "a", "b"], &["config", "set", "a", "b"]),
        ];
        for (short, long) in cases {
            let mut short_args = vec!["mcsup"];
            short_args.extend_from_slice(short);
            let mut long_args = vec!["mcsup"];
            long_args.extend_from_slice(long);
            let from_short = Cli::try_parse_from(&short_args)
                .unwrap_or_else(|e| panic!("{short:?} 解析失败: {e}"));
            let from_long = Cli::try_parse_from(&long_args)
                .unwrap_or_else(|e| panic!("{long:?} 解析失败: {e}"));
            assert_eq!(
                format!("{:?}", from_short.command),
                format!("{:?}", from_long.command),
                "{short:?} 与 {long:?} 应解析到同一命令"
            );
        }
    }

    /// 单字母不是简写，必须报错而不是被推断成某个子命令
    #[test]
    fn single_letter_is_not_accepted() {
        for arg in ["s", "w", "l"] {
            let err = Cli::try_parse_from(["mcsup", arg]).expect_err("单字母不应被接受");
            assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
        }
        // 二级同理：`server l` 不推断为 list / link / logs 之一
        let err = Cli::try_parse_from(["mcsup", "server", "l"]).expect_err("`server l` 不应被接受");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}
