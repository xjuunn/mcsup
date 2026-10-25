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
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
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
    Init {
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    /// 已注册工作目录
    List,
    /// 切换当前工作区；不带名字时交互选择
    Use { name: Option<String> },
    /// 注销工作目录
    Remove {
        name: String,
        #[arg(long)]
        purge: bool,
    },
    /// 检查工作区与注册表一致性
    #[command(name = "doctor")]
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
    List,
    /// 创建服务；缺名称 / 版本时交互补齐（非交互必填，见全局交互约定）
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
    Install {
        name: String,
        #[arg(long, value_name = "VER")]
        version: Option<String>,
    },
    /// 启动服务
    Start {
        name: String,
        /// 同意 Mojang EULA（写 eula=true）
        #[arg(long)]
        accept_eula: bool,
    },
    /// 停止服务
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
    Status { name: String },
    /// 附加交互控制台
    Console { name: String },
    /// 注入单条命令
    Send { name: String, command: String },
    /// 查看日志
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
    Props { name: String, kv: Vec<String> },
    /// 设置服务属性
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
    Remove {
        name: String,
        #[arg(long)]
        purge: bool,
    },
    /// 链接外部服务器目录到工作区
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
        #[arg(long, value_name = "WS")]
        workspace: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum JavaAction {
    /// 列出已注册的 Java 运行时（未扫描过则自动扫描）
    List,
    /// 重新扫描本机 Java
    Scan,
    /// 从远端下载安装 Java
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
    #[command(arg_required_else_help = true)]
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
