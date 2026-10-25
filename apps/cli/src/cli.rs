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
    about = "一站式 Minecraft 服务器管理工具",
    // propagate_version = true,
    arg_required_else_help = true
)]
pub(crate) struct Cli {
    /// 日志级别：-v 提升、-q 降低
    #[command(flatten)]
    pub(crate) verbose: Verbosity<ZhLevel>,
    /// 输出格式：human默认 或者json
    #[arg(long, global = true, value_enum, default_value = "human")]
    pub(crate) output: OutputFormat,
    /// 配置文件路径，缺省用系统配置目录下的 mcsup/config.toml
    #[arg(long, global = true, value_name = "PATH")]
    pub(crate) config: Option<PathBuf>,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// 工作目录：创建 / 注册表 / 默认切换
    #[command(visible_alias = "ws")]
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// 服务端生命周期与运维
    #[command(visible_alias = "srv")]
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
    /// 整合包
    #[command(visible_alias = "mp")]
    Modpack {
        #[command(subcommand)]
        action: ModpackAction,
    },
    /// JVM 运行时
    #[command(visible_alias = "jv")]
    Java {
        #[command(subcommand)]
        action: JavaAction,
    },
    /// 远程执行位置
    #[command(visible_alias = "rmt")]
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },
    /// 环境自检
    #[command(visible_alias = "doc")]
    Doctor {
        /// 只跑指定检查项（kebab-case，逗号分隔）；缺省全跑
        #[arg(long, value_name = "KIND", value_delimiter = ',')]
        only: Vec<String>,
        /// warning 也视为失败（CI 用）
        #[arg(long)]
        strict: bool,
        /// 跳过联网检查（来源连通性）
        #[arg(long)]
        offline: bool,
    },
    /// 查看 / 编辑平台配置
    #[command(visible_alias = "cfg")]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// 升级自身
    #[command(visible_alias = "up")]
    Update,
    /// 生成 shell 补全脚本
    #[command(visible_alias = "comp")]
    Completions { shell: clap_complete::Shell },
    /// 版本信息
    #[command(visible_alias = "ver")]
    Version,
    /// supervisor 进程入口（server start 自动拉起，不面向用户）
    #[command(name = "__supervise", hide = true)]
    Supervise {
        /// 服务器目录
        #[arg(long, value_name = "PATH")]
        dir: PathBuf,
        /// java 可执行文件
        #[arg(long, value_name = "PATH")]
        java: PathBuf,
        /// 服务端核心 JAR
        #[arg(long, value_name = "PATH")]
        jar: PathBuf,
        /// 堆上限（MiB）
        #[arg(long, value_name = "MIB")]
        ram: Option<u32>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum WorkspaceAction {
    /// 创建并注册工作目录
    #[command(visible_alias = "in", visible_alias = "new")]
    Init {
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    /// 已注册工作目录
    #[command(visible_alias = "ls")]
    List,
    /// 切换当前工作区；不带名字时交互选择
    #[command(visible_alias = "sw")]
    Use { name: Option<String> },
    /// 注销工作目录
    #[command(visible_alias = "rm")]
    Remove {
        name: String,
        #[arg(long)]
        purge: bool,
    },
    /// 检查工作区与注册表一致性
    #[command(name = "doctor", visible_alias = "doc")]
    Doctor {
        /// 只跑指定检查项（kebab-case，逗号分隔）；缺省全跑
        #[arg(long, value_name = "KIND", value_delimiter = ',')]
        only: Vec<String>,
        /// warning 也视为失败（CI 用）
        #[arg(long)]
        strict: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ServerAction {
    /// 列出所有服务
    #[command(visible_alias = "ls")]
    List,
    /// 创建服务；缺名称 / 版本时交互补齐（非交互必填，见全局交互约定）
    #[command(visible_alias = "cr")]
    Create {
        /// 服务器名；缺省且可交互时提示输入
        #[arg(value_name = "NAME")]
        name: Option<String>,
        /// 服务端版本；缺省且可交互时从版本列表选择
        #[arg(long, value_name = "VER")]
        version: Option<String>,
        #[arg(long, value_name = "VAR")]
        variant: Option<String>,
        #[arg(long, value_name = "WS")]
        workspace: Option<String>,
    },
    /// 安装 / 更换服务端核心
    #[command(visible_alias = "in")]
    Install {
        name: String,
        #[arg(long, value_name = "VER")]
        version: Option<String>,
    },
    /// 启动服务
    #[command(visible_alias = "up")]
    Start {
        name: String,
        /// 同意 Mojang EULA（写 eula=true）
        #[arg(long)]
        accept_eula: bool,
    },
    /// 停止服务
    #[command(visible_alias = "down")]
    Stop {
        name: String,
        /// 优雅停机宽限秒数
        #[arg(long, value_name = "SEC", default_value_t = 30)]
        grace: u64,
        /// 宽限到期后强杀
        #[arg(long)]
        force: bool,
    },
    /// 重启服务（先停后启）
    #[command(visible_alias = "rs")]
    Restart {
        name: String,
        /// 同意 Mojang EULA（写 eula=true）
        #[arg(long)]
        accept_eula: bool,
        /// 优雅停机宽限秒数
        #[arg(long, value_name = "SEC", default_value_t = 30)]
        grace: u64,
        /// 宽限到期后强杀
        #[arg(long)]
        force: bool,
    },
    /// 服务状态
    #[command(visible_alias = "st")]
    Status { name: String },
    /// 附加交互控制台
    #[command(visible_alias = "con")]
    Console { name: String },
    /// 注入单条命令
    #[command(visible_alias = "snd")]
    Send { name: String, command: String },
    /// 查看日志
    #[command(visible_alias = "log")]
    Logs {
        name: String,
        /// 显示末尾行数
        #[arg(long, value_name = "N", default_value_t = LOG_TAIL_DEFAULT)]
        tail: usize,
        /// 持续跟踪新增（JSON 模式输出 JSONL）
        #[arg(long)]
        follow: bool,
    },
    /// 读写 server.properties
    #[command(visible_alias = "prop")]
    Props { name: String, kv: Vec<String> },
    /// 设置服务属性
    #[command(visible_alias = "cfg")]
    Set {
        name: String,
        /// 属性键；缺省且可交互时从键列表选择
        #[arg(value_name = "KEY")]
        key: Option<String>,
        /// 属性值；缺省且可交互时按键类型输入或选择
        #[arg(value_name = "VALUE")]
        value: Option<String>,
    },
    /// 删除服务
    #[command(visible_alias = "rm")]
    Remove {
        name: String,
        #[arg(long)]
        purge: bool,
    },
    /// 链接外部服务器目录到工作区
    #[command(visible_alias = "ln")]
    Link {
        /// 外部服务器目录
        path: PathBuf,
        /// 注册名，缺省取目录名或 marker 名
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// 目标工作区，缺省当前工作区
        #[arg(long, value_name = "WS")]
        workspace: Option<String>,
    },
    /// 服务版本信息
    #[command(visible_alias = "ver")]
    Version { name: String },
    /// 备份
    #[command(visible_alias = "bak")]
    Backup {
        #[command(subcommand)]
        action: BackupAction,
    },
    /// 服务端 Mod
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
    /// 列出已注册的 Java 运行时（未扫描过则自动扫描）
    #[command(visible_alias = "ls")]
    List,
    /// 重新扫描本机 Java
    #[command(visible_alias = "sc")]
    Scan,
    /// 从远端下载安装 Java
    #[command(visible_alias = "in")]
    Install {
        /// 大版本号，如 17 / 21；缺省时交互选择
        version: Option<String>,
        /// 只装 JRE，不装完整 JDK
        #[arg(long)]
        jre: bool,
        /// 发行版：temurin / zulu / liberica / corretto
        #[arg(long, value_name = "VENDOR")]
        vendor: Option<String>,
    },
    /// 卸载 Java（系统 Java 仅移除注册记录）
    #[command(
        arg_required_else_help = true,
        visible_alias = "un",
        visible_alias = "rm"
    )]
    Uninstall {
        /// 大版本号或运行时路径
        target: Option<String>,
        /// 运行时路径（路径含空格时推荐，如 "…/Application Support/…"）
        #[arg(long, value_name = "PATH")]
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
    /// 显示平台配置
    #[command(visible_alias = "ls")]
    Show,
    /// 设置配置项
    #[command(visible_alias = "st")]
    Set { key: String, value: String },
    /// 恢复为平台缺省
    #[command(visible_alias = "us")]
    Unset { key: String },
    /// 打开编辑器
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
