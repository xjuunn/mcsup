use crate::output::OutputFormat;
use clap::{Parser, Subcommand};
use clap_verbosity_flag::Verbosity;
use std::path::PathBuf;

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
    pub(crate) verbose: Verbosity,
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
    /// 服务端生命周期与运维
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
    /// 整合包
    Modpack {
        #[command(subcommand)]
        action: ModpackAction,
    },
    /// JVM 运行时
    Java {
        #[command(subcommand)]
        action: JavaAction,
    },
    /// 远程执行位置
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },
    /// 初始化默认配置与服务目录
    Init {
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
    },
    /// 环境自检
    Doctor,
    /// 查看 / 编辑平台配置
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// 升级自身
    Update,
    /// 生成 shell 补全脚本
    Completions { shell: clap_complete::Shell },
    /// 版本信息
    Version,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ServerAction {
    /// 列出所有服务
    List,
    /// 按指定版本 / 变体创建服务
    Create {
        name: String,
        #[arg(long, value_name = "VER")]
        version: String,
        #[arg(long, value_name = "VAR")]
        variant: Option<String>,
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
    },
    /// 安装 / 更换服务端核心
    Install {
        #[arg(value_name = "VERSION")]
        version: String,
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
    },
    /// 启动服务
    Start { name: String },
    /// 停止服务
    Stop { name: String },
    /// 重启服务
    Restart { name: String },
    /// 服务状态
    Status { name: String },
    /// 附加交互控制台
    Console { name: String },
    /// 注入单条命令
    Send { name: String, command: String },
    /// 查看日志
    Logs {
        name: String,
        #[arg(long)]
        tail: Option<usize>,
        #[arg(long)]
        follow: bool,
    },
    /// 读写 server.properties
    Props { name: String, kv: Vec<String> },
    /// 设置服务属性
    Set {
        name: String,
        key: String,
        value: String,
    },
    /// 删除服务
    Remove {
        name: String,
        #[arg(long)]
        purge: bool,
    },
    /// 服务版本信息
    Version { name: String },
    /// 备份
    Backup {
        #[command(subcommand)]
        action: BackupAction,
    },
    /// 服务端 Mod
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
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum JavaAction {
    List,
    Install {
        version: String,
        #[arg(long, value_name = "NAME")]
        vendor: Option<String>,
    },
    Uninstall {
        version: String,
    },
    Default {
        version: Option<String>,
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
    /// 显示平台配置
    Show,
    /// 设置配置项
    Set { key: String, value: String },
    /// 打开编辑器
    Edit,
}
