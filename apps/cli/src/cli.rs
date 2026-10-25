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
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    #[command(about = crate::meta::server())]
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
    #[command(about = crate::meta::modpack())]
    Modpack {
        #[command(subcommand)]
        action: ModpackAction,
    },
    #[command(about = crate::meta::java())]
    Java {
        #[command(subcommand)]
        action: JavaAction,
    },
    #[command(about = crate::meta::remote())]
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },
    #[command(about = crate::meta::doctor())]
    Doctor,
    #[command(about = crate::meta::config())]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    #[command(about = crate::meta::update())]
    Update,
    #[command(about = crate::meta::completions())]
    Completions { shell: clap_complete::Shell },
    #[command(about = crate::meta::version())]
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
    Init {
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    #[command(about = crate::meta::workspace_list())]
    List,
    #[command(about = crate::meta::workspace_use())]
    Use { name: Option<String> },
    #[command(about = crate::meta::workspace_remove())]
    Remove {
        name: String,
        #[arg(long)]
        purge: bool,
    },
    #[command(name = "doctor", about = crate::meta::workspace_doctor())]
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
    List,
    #[command(about = crate::meta::server_create())]
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
    Install {
        name: String,
        #[arg(long, value_name = "VER")]
        version: Option<String>,
    },
    #[command(about = crate::meta::server_start())]
    Start {
        name: String,
        #[arg(long, help = crate::meta::server_accept_eula_arg())]
        accept_eula: bool,
    },
    #[command(about = crate::meta::server_stop())]
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
    Status { name: String },
    #[command(about = crate::meta::server_console())]
    Console { name: String },
    #[command(about = crate::meta::server_send())]
    Send { name: String, command: String },
    #[command(about = crate::meta::server_logs())]
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
    Props { name: String, kv: Vec<String> },
    #[command(about = crate::meta::server_set())]
    Set {
        name: String,
        #[arg(value_name = "KEY", help = crate::meta::server_set_key_arg())]
        key: Option<String>,
        #[arg(value_name = "VALUE", help = crate::meta::server_set_value_arg())]
        value: Option<String>,
    },
    #[command(about = crate::meta::server_remove())]
    Remove {
        name: String,
        #[arg(long)]
        purge: bool,
    },
    #[command(about = crate::meta::server_link())]
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
    Version { name: String },
    #[command(about = crate::meta::server_backup())]
    Backup {
        #[command(subcommand)]
        action: BackupAction,
    },
    #[command(about = crate::meta::server_mod())]
    Mod {
        #[command(subcommand)]
        action: ModAction,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum BackupAction {
    Create {
        name: String,
        #[arg(long)]
        label: Option<String>,
    },
    List {
        name: String,
    },
    Restore {
        name: String,
        id: String,
        #[arg(long, value_name = "NAME")]
        to: Option<String>,
    },
    Verify {
        name: String,
        id: String,
    },
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
    List { name: String },
    Install { name: String, spec: String },
    Remove { name: String, m: String },
    Update { name: String, m: Option<String> },
    Outdated { name: String },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ModpackAction {
    List {
        query: Option<String>,
    },
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
    List,
    #[command(about = crate::meta::java_scan())]
    Scan,
    #[command(about = crate::meta::java_install())]
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
    Uninstall {
        #[arg(help = crate::meta::java_uninstall_target_arg())]
        target: Option<String>,
        #[arg(long, value_name = "PATH", help = crate::meta::java_uninstall_dir_arg())]
        dir: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum RemoteAction {
    List,
    Deploy {
        name: String,
        host: String,
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
    },
    Ping {
        name: String,
    },
    Remove {
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigAction {
    #[command(about = crate::meta::config_show())]
    Show,
    #[command(about = crate::meta::config_set())]
    Set { key: String, value: String },
    #[command(about = crate::meta::config_edit())]
    Edit,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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
}
