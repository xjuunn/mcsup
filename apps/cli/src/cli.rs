use crate::output::OutputFormat;
use crate::verbosity::ZhLevel;
use clap::{Parser, Subcommand};
use clap_verbosity_flag::Verbosity;
use std::path::PathBuf;

/// 日志默认回看行数：`logs --tail` 缺省值，`console` 附加时回放的历史行数
pub(crate) const LOG_TAIL_DEFAULT: usize = 30;

/// 备份历史每页条数缺省值（`backup ls --page-size`）
pub(crate) const BACKUP_PAGE_SIZE: usize = 20;

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
    #[arg(
        long,
        global = true,
        short = 'y',
        help = crate::meta::yes_arg()
    )]
    pub(crate) yes: bool,
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
        /// 单 JAR 启动：核心 JAR 相对路径（与服务端目录拼绝对）
        #[arg(long, value_name = "PATH", help = crate::meta::supervise_jar_arg())]
        jar: Option<PathBuf>,
        /// 参数文件启动（Forge / NeoForge）：依次 `@` 展开的相对路径，可重复
        #[arg(long = "arg-file", value_name = "PATH")]
        arg_file: Vec<PathBuf>,
        /// 追加给服务端的程序参数（参数文件之后，可重复；默认 `nogui`）
        #[arg(long = "server-arg", value_name = "ARG")]
        server_arg: Vec<String>,
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
        #[arg(long, value_name = "ORIGIN", help = crate::meta::origin_target_arg())]
        origin: Option<String>,
    },
    #[command(about = crate::meta::workspace_list())]
    #[command(visible_alias = "ls")]
    List {
        #[arg(long, value_name = "ORIGIN", help = crate::meta::origin_filter_arg())]
        origin: Option<String>,
    },
    #[command(about = crate::meta::workspace_use())]
    #[command(visible_alias = "sw")]
    Use {
        name: Option<String>,
        #[arg(long, value_name = "ORIGIN", help = crate::meta::origin_target_arg())]
        origin: Option<String>,
    },
    #[command(about = crate::meta::workspace_remove())]
    #[command(visible_alias = "rm")]
    Remove {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(long)]
        purge: bool,
        #[arg(long, value_name = "ORIGIN", help = crate::meta::origin_target_arg())]
        origin: Option<String>,
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
    List {
        #[arg(long, value_name = "ORIGIN", help = crate::meta::origin_filter_arg())]
        origin: Option<String>,
    },
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
        #[arg(
            long,
            value_name = "WS",
            help = crate::meta::server_workspace_arg()
        )]
        workspace: Option<String>,
        #[arg(
            long = "workspace-name",
            value_name = "NAME",
            help = crate::meta::server_workspace_name_arg()
        )]
        workspace_name: Option<String>,
        #[arg(
            long = "workspace-dir",
            value_name = "PATH",
            help = crate::meta::server_workspace_dir_arg()
        )]
        workspace_dir: Option<PathBuf>,
        #[arg(
            long = "modpack",
            value_name = "ID",
            help = crate::meta::server_create_modpack_arg()
        )]
        modpack: Option<String>,
        #[arg(long, value_name = "ORIGIN", help = crate::meta::origin_target_arg())]
        origin: Option<String>,
    },
    #[command(about = crate::meta::server_install())]
    #[command(visible_alias = "in")]
    Install {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(long, value_name = "VER")]
        version: Option<String>,
    },
    #[command(about = crate::meta::server_start())]
    #[command(visible_alias = "up")]
    Start {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(long, help = crate::meta::server_accept_eula_arg())]
        accept_eula: bool,
    },
    #[command(about = crate::meta::server_stop())]
    #[command(visible_alias = "down")]
    Stop {
        #[arg(value_name = "NAME")]
        name: Option<String>,
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
        #[arg(value_name = "NAME")]
        name: Option<String>,
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
    Status {
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    #[command(about = crate::meta::server_console())]
    #[command(visible_alias = "con")]
    Console {
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    #[command(about = crate::meta::server_send())]
    #[command(visible_alias = "snd")]
    Send {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(value_name = "COMMAND")]
        command: Option<String>,
    },
    #[command(about = crate::meta::server_logs())]
    #[command(visible_alias = "log")]
    Logs {
        #[arg(value_name = "NAME")]
        name: Option<String>,
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
    Props {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(value_name = "KV")]
        kv: Vec<String>,
    },
    #[command(about = crate::meta::server_set())]
    #[command(visible_alias = "cfg")]
    Set {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(value_name = "KEY", help = crate::meta::server_set_key_arg())]
        key: Option<String>,
        #[arg(value_name = "VALUE", help = crate::meta::server_set_value_arg())]
        value: Option<String>,
    },
    #[command(about = crate::meta::server_remove())]
    #[command(visible_alias = "rm")]
    Remove {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(long)]
        purge: bool,
        #[arg(long, value_name = "ORIGIN", help = crate::meta::origin_target_arg())]
        origin: Option<String>,
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
    Version {
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
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
    #[command(about = crate::meta::backup_create())]
    #[command(visible_alias = "cr")]
    Create {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(long, help = crate::meta::backup_label_arg())]
        label: Option<String>,
        #[arg(long, value_name = "ITEMS", help = crate::meta::backup_scope_arg())]
        scope: Option<String>,
        #[arg(long, help = crate::meta::backup_no_reuse_arg())]
        no_reuse: bool,
    },
    #[command(about = crate::meta::backup_list())]
    #[command(visible_alias = "ls")]
    List {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(long, value_name = "N", default_value_t = 1, help = crate::meta::backup_page_arg())]
        page: usize,
        #[arg(
            long = "page-size",
            value_name = "N",
            default_value_t = BACKUP_PAGE_SIZE,
            help = crate::meta::backup_page_size_arg()
        )]
        page_size: usize,
    },
    #[command(about = crate::meta::backup_restore())]
    #[command(visible_alias = "rt")]
    Restore {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(help = crate::meta::backup_id_arg())]
        id: Option<String>,
        #[arg(long, value_name = "NAME", help = crate::meta::backup_to_arg())]
        to: Option<String>,
    },
    #[command(about = crate::meta::backup_delete())]
    #[command(visible_alias = "rm")]
    Delete {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(help = crate::meta::backup_id_arg())]
        id: Option<String>,
    },
    #[command(about = crate::meta::backup_verify())]
    #[command(visible_alias = "vf")]
    Verify {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(help = crate::meta::backup_id_arg())]
        id: Option<String>,
    },
    #[command(about = crate::meta::backup_prune())]
    #[command(visible_alias = "pr")]
    Prune {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(long, value_name = "N", help = crate::meta::backup_keep_arg())]
        keep: Option<usize>,
        #[arg(long, value_name = "AGE", help = crate::meta::backup_older_than_arg())]
        older_than: Option<String>,
        #[arg(long, help = crate::meta::backup_dry_run_arg())]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ModAction {
    #[command(visible_alias = "ls")]
    List {
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    #[command(visible_alias = "in")]
    Install {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(value_name = "SPEC")]
        spec: Option<String>,
    },
    #[command(visible_alias = "rm")]
    Remove {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(value_name = "MOD")]
        m: Option<String>,
    },
    #[command(visible_alias = "up")]
    Update {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(value_name = "MOD")]
        m: Option<String>,
    },
    #[command(visible_alias = "od")]
    Outdated {
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum JavaAction {
    #[command(about = crate::meta::java_list())]
    #[command(visible_alias = "ls")]
    List {
        #[arg(long, value_name = "ORIGIN", help = crate::meta::origin_filter_arg())]
        origin: Option<String>,
    },
    #[command(about = crate::meta::java_scan())]
    #[command(visible_alias = "sc")]
    Scan {
        #[arg(long, value_name = "ORIGIN", help = crate::meta::origin_target_arg())]
        origin: Option<String>,
    },
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
        #[arg(long, value_name = "ORIGIN", help = crate::meta::origin_target_arg())]
        origin: Option<String>,
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
        #[arg(long, value_name = "ORIGIN", help = crate::meta::origin_target_arg())]
        origin: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum RemoteAction {
    #[command(about = crate::meta::remote_serve())]
    #[command(visible_alias = "sv")]
    Serve {
        #[arg(
            long,
            value_name = "ADDR",
            default_value = "0.0.0.0:7681",
            help = crate::meta::remote_listen_arg()
        )]
        listen: String,
        #[arg(long, value_name = "NAME", help = crate::meta::remote_name_arg())]
        name: Option<String>,
        /// 停止本机服务（向正在运行的 agent 发 Shutdown）
        #[arg(long, conflicts_with_all = ["name", "status"])]
        stop: bool,
        /// 查看本机服务状态（身份、指纹、已配对客户端）
        #[arg(long, conflicts_with = "name")]
        status: bool,
    },
    #[command(about = crate::meta::remote_connect())]
    #[command(visible_alias = "con")]
    Connect {
        /// `user@host`（复用 SSH 信任）或 `host[:port]`（直连 + 配对码）
        target: Option<String>,
        #[arg(long, value_name = "NAME", help = crate::meta::remote_name_arg())]
        name: Option<String>,
        #[arg(long, help = crate::meta::remote_read_only_arg())]
        read_only: bool,
        /// 配对码；非交互下必填，交互下缺省提示输入
        #[arg(long, value_name = "CODE")]
        code: Option<String>,
    },
    #[command(about = crate::meta::remote_reconnect())]
    #[command(visible_alias = "rc")]
    Reconnect { name: String },
    #[command(about = crate::meta::remote_list())]
    #[command(visible_alias = "ls")]
    List,
    #[command(about = crate::meta::remote_status())]
    #[command(visible_alias = "st")]
    Status { name: Option<String> },
    #[command(about = crate::meta::remote_ping())]
    #[command(visible_alias = "pg")]
    Ping { name: String },
    #[command(about = crate::meta::remote_disconnect())]
    #[command(visible_alias = "dc")]
    Disconnect { name: String },
    #[command(about = crate::meta::remote_revoke())]
    #[command(visible_alias = "rv")]
    Revoke {
        /// 客户端 id 或名称
        client: String,
        #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:7681")]
        listen: String,
    },
    /// SSH 引导入口（隐藏）：前台起 agent，把握手 JSON 写 stdout
    #[command(name = "__serve-ssh", hide = true)]
    ServeSsh {
        /// 数据目录（远端机器上的路径）
        #[arg(long, value_name = "PATH")]
        data_dir: Option<PathBuf>,
        /// 本机名
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigAction {
    #[command(about = crate::meta::config_show())]
    #[command(visible_alias = "ls")]
    Show,
    #[command(about = crate::meta::config_set())]
    #[command(visible_alias = "st")]
    Set {
        #[arg(value_name = "KEY")]
        key: Option<String>,
        #[arg(value_name = "VALUE")]
        value: Option<String>,
    },
    #[command(about = crate::meta::config_unset())]
    #[command(visible_alias = "us")]
    Unset {
        #[arg(value_name = "KEY")]
        key: Option<String>,
    },
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
        let cases: [(&[&str], &[&str]); 6] = [
            (&["ws", "ls"], &["workspace", "list"]),
            (
                &["srv", "bak", "ls", "alpha"],
                &["server", "backup", "list", "alpha"],
            ),
            (
                &["srv", "md", "od", "alpha"],
                &["server", "mod", "outdated", "alpha"],
            ),
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

    /// `--origin` 在三类列表命令上都能解析，缺省为 `None`
    #[test]
    fn origin_filter_parses_on_list_commands() {
        for args in [
            vec!["mcsup", "workspace", "ls", "--origin", "prod-box"],
            vec!["mcsup", "server", "ls", "--origin", "remote"],
            vec!["mcsup", "java", "ls", "--origin", "local"],
        ] {
            let cli = Cli::try_parse_from(&args).expect("parse");
            let origin = match cli.command {
                Command::Workspace {
                    action: WorkspaceAction::List { origin },
                }
                | Command::Server {
                    action: ServerAction::List { origin },
                }
                | Command::Java {
                    action: JavaAction::List { origin },
                } => origin,
                other => panic!("unexpected: {other:?}"),
            };
            assert!(origin.is_some(), "{args:?}");
        }
        let cli = Cli::try_parse_from(["mcsup", "workspace", "ls"]).expect("parse");
        match cli.command {
            Command::Workspace {
                action: WorkspaceAction::List { origin },
            } => assert!(origin.is_none()),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// 创建类命令接受 `--origin`，缺省为 `None`
    #[test]
    fn origin_target_parses_on_create_commands() {
        let cli = Cli::try_parse_from([
            "mcsup",
            "server",
            "create",
            "web",
            "--version",
            "1.21.1",
            "--variant",
            "vanilla",
            "--origin",
            "prod-box",
        ])
        .expect("parse");
        match cli.command {
            Command::Server {
                action: ServerAction::Create { origin, .. },
            } => assert_eq!(origin.as_deref(), Some("prod-box")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `remote` 的新子命令都能解析
    #[test]
    fn remote_subcommands_parse() {
        for args in [
            vec!["mcsup", "remote", "serve", "--listen", "127.0.0.1:0"],
            vec!["mcsup", "remote", "serve", "--stop"],
            vec!["mcsup", "remote", "serve", "--status"],
            vec![
                "mcsup",
                "remote",
                "connect",
                "10.0.0.5",
                "--code",
                "1234-5678",
            ],
            vec!["mcsup", "remote", "reconnect", "prod-box"],
            vec!["mcsup", "remote", "list"],
            vec!["mcsup", "remote", "status", "prod-box"],
            vec!["mcsup", "remote", "ping", "prod-box"],
            vec!["mcsup", "remote", "disconnect", "prod-box"],
            vec!["mcsup", "remote", "revoke", "laptop"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
    }

    /// `serve --stop` 与 `--status` 互斥，避免一次做两件事
    #[test]
    fn serve_stop_and_status_conflict() {
        let err = Cli::try_parse_from(["mcsup", "remote", "serve", "--stop", "--status"])
            .expect_err("conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
