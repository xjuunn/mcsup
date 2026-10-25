//! 命令行元数据的本地化文案
//!
//! clap 的 `about` / `help` 来自文档注释时在编译期固化为字面量，运行时无法按
//! 语言切换；改成 `about = meta::xxx()` 形式的表达式后，`Cli::command()` 每次
//! 调用都会重新求值，语言切换才生效。
//!
//! 命名规则：函数名即命令路径，`_` 分隔。参数说明加 `_arg` 后缀，避免与同名
//! 子命令冲突。
//!
//! 语言取自 [`crate::locale::current`]。这是进程级设置：一次运行一种语言，
//! 写入后只读，没有并发改写的可能。

use crate::locale::{self, Lang};

/// 二元文案：取当前进程语言
fn t(zh: &'static str, en: &'static str) -> &'static str {
    match locale::current() {
        Lang::Zh => zh,
        Lang::En => en,
    }
}

/// 顶层 `about`
#[must_use]
pub(crate) fn about() -> &'static str {
    t(
        "一站式 Minecraft 服务器管理工具",
        "all-in-one Minecraft server management tool",
    )
}

// ---- 全局参数 ----

#[must_use]
pub(crate) fn verbose_arg() -> &'static str {
    t(
        "提升日志级别（可重复）",
        "increase logging verbosity (repeatable)",
    )
}

#[must_use]
pub(crate) fn quiet_arg() -> &'static str {
    t(
        "降低日志级别（可重复）",
        "decrease logging verbosity (repeatable)",
    )
}

#[must_use]
pub(crate) fn output_arg() -> &'static str {
    t(
        "输出格式：human默认 或者json",
        "output format: human (default) or json",
    )
}

#[must_use]
pub(crate) fn config_arg() -> &'static str {
    t(
        "配置文件路径，缺省用系统配置目录下的 mcsup/config.toml",
        "config file path; defaults to mcsup/config.toml under the system config directory",
    )
}

#[must_use]
pub(crate) fn lang_arg() -> &'static str {
    t(
        "界面语言：auto按环境自动判断 zh中文 en英文（只影响提示文案，--output json 不受影响）",
        "interface language: auto detects from the environment, zh Chinese, en English (affects messages only; --output json is unaffected)",
    )
}

// ---- 顶层子命令 ----

#[must_use]
pub(crate) fn workspace() -> &'static str {
    t(
        "工作目录：创建 / 注册表 / 默认切换",
        "workspaces: create, list, switch",
    )
}

#[must_use]
pub(crate) fn server() -> &'static str {
    t("服务端生命周期与运维", "server lifecycle and operations")
}

#[must_use]
pub(crate) fn modpack() -> &'static str {
    t("整合包", "modpacks")
}

#[must_use]
pub(crate) fn java() -> &'static str {
    t("JVM 运行时", "JVM runtimes")
}

#[must_use]
pub(crate) fn remote() -> &'static str {
    t("远程执行位置", "remote execution targets")
}

#[must_use]
pub(crate) fn doctor() -> &'static str {
    t("环境自检", "environment self-check")
}

#[must_use]
pub(crate) fn doctor_only_arg() -> &'static str {
    t(
        "只跑指定检查项（kebab-case，逗号分隔）；缺省全跑",
        "run only the given checks (kebab-case, comma separated); all by default",
    )
}

#[must_use]
pub(crate) fn doctor_strict_arg() -> &'static str {
    t(
        "warning 也视为失败（CI 用）",
        "treat warnings as failures (for CI)",
    )
}

#[must_use]
pub(crate) fn doctor_offline_arg() -> &'static str {
    t(
        "跳过联网检查（来源连通性）",
        "skip network checks (source reachability)",
    )
}

#[must_use]
pub(crate) fn config() -> &'static str {
    t("查看 / 编辑平台配置", "view or edit platform configuration")
}

#[must_use]
pub(crate) fn update() -> &'static str {
    t("升级自身", "upgrade mcsup itself")
}

#[must_use]
pub(crate) fn completions() -> &'static str {
    t("生成 shell 补全脚本", "generate shell completion scripts")
}

#[must_use]
pub(crate) fn version() -> &'static str {
    t("版本信息", "version information")
}

#[must_use]
pub(crate) fn supervise() -> &'static str {
    t(
        "supervisor 进程入口（server start 自动拉起，不面向用户）",
        "supervisor process entry (spawned by server start, not for direct use)",
    )
}

#[must_use]
pub(crate) fn supervise_dir_arg() -> &'static str {
    t("服务器目录", "server directory")
}

#[must_use]
pub(crate) fn supervise_java_arg() -> &'static str {
    t("java 可执行文件", "java executable")
}

#[must_use]
pub(crate) fn supervise_jar_arg() -> &'static str {
    t("服务端核心 JAR", "server core JAR")
}

#[must_use]
pub(crate) fn supervise_ram_arg() -> &'static str {
    t("堆上限（MiB）", "heap limit (MiB)")
}

// ---- workspace ----

#[must_use]
pub(crate) fn workspace_init() -> &'static str {
    t("创建并注册工作目录", "create and register a workspace")
}

#[must_use]
pub(crate) fn workspace_list() -> &'static str {
    t("已注册工作目录", "registered workspaces")
}

#[must_use]
pub(crate) fn workspace_use() -> &'static str {
    t(
        "切换当前工作区；不带名字时交互选择",
        "switch the current workspace; prompts when no name is given",
    )
}

#[must_use]
pub(crate) fn workspace_remove() -> &'static str {
    t("注销工作目录", "unregister a workspace")
}

#[must_use]
pub(crate) fn workspace_doctor() -> &'static str {
    t(
        "检查工作区与注册表一致性",
        "check consistency between workspaces and the registry",
    )
}

#[must_use]
pub(crate) fn workspace_doctor_only_arg() -> &'static str {
    t(
        "只跑指定检查项（kebab-case，逗号分隔）；缺省全跑",
        "run only the given checks (kebab-case, comma separated); all by default",
    )
}

#[must_use]
pub(crate) fn workspace_doctor_strict_arg() -> &'static str {
    t(
        "warning 也视为失败（CI 用）",
        "treat warnings as failures (for CI)",
    )
}

// ---- server ----

#[must_use]
pub(crate) fn server_list() -> &'static str {
    t("列出所有服务", "list all servers")
}

#[must_use]
pub(crate) fn server_create() -> &'static str {
    t(
        "创建服务；缺名称 / 版本时交互补齐（非交互必填，见全局交互约定）",
        "create a server; missing name or version is prompted interactively (required when non-interactive)",
    )
}

#[must_use]
pub(crate) fn server_create_name_arg() -> &'static str {
    t(
        "服务器名；缺省且可交互时提示输入",
        "server name; prompted when omitted and interactive",
    )
}

#[must_use]
pub(crate) fn server_create_version_arg() -> &'static str {
    t(
        "服务端版本；缺省且可交互时从版本列表选择",
        "server version; picked from a list when omitted and interactive",
    )
}

#[must_use]
pub(crate) fn server_install() -> &'static str {
    t(
        "安装 / 更换服务端核心",
        "install or replace the server core",
    )
}

#[must_use]
pub(crate) fn server_start() -> &'static str {
    t("启动服务", "start a server")
}

#[must_use]
pub(crate) fn server_accept_eula_arg() -> &'static str {
    t(
        "同意 Mojang EULA（写 eula=true）",
        "accept the Mojang EULA (writes eula=true)",
    )
}

#[must_use]
pub(crate) fn server_stop() -> &'static str {
    t("停止服务", "stop a server")
}

#[must_use]
pub(crate) fn server_grace_arg() -> &'static str {
    t(
        "优雅停机宽限秒数",
        "graceful shutdown grace period in seconds",
    )
}

#[must_use]
pub(crate) fn server_force_arg() -> &'static str {
    t(
        "宽限到期后强杀",
        "force kill after the grace period expires",
    )
}

#[must_use]
pub(crate) fn server_restart() -> &'static str {
    t(
        "重启服务（先停后启）",
        "restart a server (stop, then start)",
    )
}

#[must_use]
pub(crate) fn server_status() -> &'static str {
    t("服务状态", "server status")
}

#[must_use]
pub(crate) fn server_console() -> &'static str {
    t("附加交互控制台", "attach an interactive console")
}

#[must_use]
pub(crate) fn server_send() -> &'static str {
    t("注入单条命令", "send a single command")
}

#[must_use]
pub(crate) fn server_logs() -> &'static str {
    t("查看日志", "view logs")
}

#[must_use]
pub(crate) fn server_logs_tail_arg() -> &'static str {
    t("显示末尾行数", "number of trailing lines to show")
}

#[must_use]
pub(crate) fn server_logs_follow_arg() -> &'static str {
    t(
        "持续跟踪新增（JSON 模式输出 JSONL）",
        "follow new lines (JSONL in JSON mode)",
    )
}

#[must_use]
pub(crate) fn server_props() -> &'static str {
    t("读写 server.properties", "read or write server.properties")
}

#[must_use]
pub(crate) fn server_set() -> &'static str {
    t("设置服务属性", "set a server property")
}

#[must_use]
pub(crate) fn server_set_key_arg() -> &'static str {
    t(
        "属性键；缺省且可交互时从键列表选择",
        "property key; picked from a list when omitted and interactive",
    )
}

#[must_use]
pub(crate) fn server_set_value_arg() -> &'static str {
    t(
        "属性值；缺省且可交互时按键类型输入或选择",
        "property value; typed or picked by key type when omitted and interactive",
    )
}

#[must_use]
pub(crate) fn server_remove() -> &'static str {
    t("删除服务", "remove a server")
}

#[must_use]
pub(crate) fn server_link() -> &'static str {
    t(
        "链接外部服务器目录到工作区",
        "link an external server directory into a workspace",
    )
}

#[must_use]
pub(crate) fn server_link_path_arg() -> &'static str {
    t("外部服务器目录", "external server directory")
}

#[must_use]
pub(crate) fn server_link_name_arg() -> &'static str {
    t(
        "注册名，缺省取目录名或 marker 名",
        "registered name; defaults to the directory or marker name",
    )
}

#[must_use]
pub(crate) fn server_workspace_arg() -> &'static str {
    t(
        "目标工作区，缺省当前工作区",
        "target workspace; defaults to the current one",
    )
}

#[must_use]
pub(crate) fn server_version() -> &'static str {
    t("服务版本信息", "server version information")
}

#[must_use]
pub(crate) fn server_backup() -> &'static str {
    t("备份", "backups")
}

#[must_use]
pub(crate) fn backup_create() -> &'static str {
    t(
        "创建备份（默认全量，有历史时增量复用未改动文件）",
        "create a backup (full by default; reuses unchanged files from history)",
    )
}

#[must_use]
pub(crate) fn backup_list() -> &'static str {
    t(
        "列出备份历史（支持分页）",
        "list backup history (paginated)",
    )
}

#[must_use]
pub(crate) fn backup_restore() -> &'static str {
    t(
        "从备份恢复（原地恢复需先停服；--to 恢复到新服务）",
        "restore from a backup (stop the server for in-place; --to restores into a new server)",
    )
}

#[must_use]
pub(crate) fn backup_delete() -> &'static str {
    t(
        "删除某个备份（对象交给 prune 回收）",
        "delete a backup (objects reclaimed by prune)",
    )
}

#[must_use]
pub(crate) fn backup_verify() -> &'static str {
    t(
        "校验备份对象完整性与哈希",
        "verify backup object integrity and hashes",
    )
}

#[must_use]
pub(crate) fn backup_prune() -> &'static str {
    t(
        "按保留策略清理备份并回收无引用对象（--dry-run 只预览）",
        "prune backups by retention policy and reclaim unreferenced objects (--dry-run previews)",
    )
}

#[must_use]
pub(crate) fn backup_label_arg() -> &'static str {
    t(
        "快照标签，如 before-upgrade",
        "snapshot label, e.g. before-upgrade",
    )
}

#[must_use]
pub(crate) fn backup_id_arg() -> &'static str {
    t("快照 id（可用 ls 查看）", "snapshot id (see ls)")
}

#[must_use]
pub(crate) fn backup_to_arg() -> &'static str {
    t(
        "恢复到新服务名（原服务保持不动）；缺省为原地恢复",
        "restore into a new server name (original untouched); default is in-place",
    )
}

#[must_use]
pub(crate) fn backup_scope_arg() -> &'static str {
    t(
        "备份范围：逗号分隔的顶层目录（如 world,mods）；缺省用服务属性 backup-scope，再缺省为全部",
        "backup scope: comma-separated top-level entries (e.g. world,mods); falls back to the backup-scope setting, then to everything",
    )
}

#[must_use]
pub(crate) fn backup_no_reuse_arg() -> &'static str {
    t(
        "禁用增量复用，强制重读全部文件（怀疑元数据不可靠时）",
        "disable incremental reuse and re-read every file",
    )
}

#[must_use]
pub(crate) fn backup_page_arg() -> &'static str {
    t("页码（1 基）", "page number (1-based)")
}

#[must_use]
pub(crate) fn backup_page_size_arg() -> &'static str {
    t("每页条数", "entries per page")
}

#[must_use]
pub(crate) fn backup_keep_arg() -> &'static str {
    t(
        "只保留最近 N 份快照",
        "keep only the most recent N snapshots",
    )
}

#[must_use]
pub(crate) fn backup_older_than_arg() -> &'static str {
    t(
        "删除早于该时长的快照（如 7d、24h、30m）",
        "delete snapshots older than this age (e.g. 7d, 24h, 30m)",
    )
}

#[must_use]
pub(crate) fn backup_dry_run_arg() -> &'static str {
    t(
        "只预览将删除的内容，不实际修改",
        "preview what would be removed without changing anything",
    )
}

#[must_use]
pub(crate) fn server_mod() -> &'static str {
    t("服务端 Mod", "server mods")
}

// ---- java ----

#[must_use]
pub(crate) fn java_list() -> &'static str {
    t(
        "列出已注册的 Java 运行时（未扫描过则自动扫描）",
        "list registered JVM runtimes (scans automatically if never scanned)",
    )
}

#[must_use]
pub(crate) fn java_scan() -> &'static str {
    t("重新扫描本机 Java", "rescan the machine for Java")
}

#[must_use]
pub(crate) fn java_install() -> &'static str {
    t(
        "从远端下载安装 Java",
        "download and install Java from a remote source",
    )
}

#[must_use]
pub(crate) fn java_install_version_arg() -> &'static str {
    t(
        "大版本号，如 17 / 21；缺省时交互选择",
        "major version, e.g. 17 or 21; prompted when omitted",
    )
}

#[must_use]
pub(crate) fn java_install_jre_arg() -> &'static str {
    t(
        "只装 JRE，不装完整 JDK",
        "install only the JRE, not a full JDK",
    )
}

#[must_use]
pub(crate) fn java_install_vendor_arg() -> &'static str {
    t(
        "发行版：temurin / zulu / liberica / corretto",
        "distribution: temurin, zulu, liberica or corretto",
    )
}

#[must_use]
pub(crate) fn java_uninstall() -> &'static str {
    t(
        "卸载 Java（系统 Java 仅移除注册记录）",
        "uninstall Java (system Java only loses its registry record)",
    )
}

#[must_use]
pub(crate) fn java_uninstall_target_arg() -> &'static str {
    t("大版本号或运行时路径", "major version or runtime path")
}

#[must_use]
pub(crate) fn java_uninstall_dir_arg() -> &'static str {
    t(
        "运行时路径（路径含空格时推荐，如 \"…/Application Support/…\"）",
        "runtime path (preferred when it contains spaces, e.g. \"…/Application Support/…\")",
    )
}

// ---- config ----

#[must_use]
pub(crate) fn config_show() -> &'static str {
    t("显示平台配置", "show platform configuration")
}

#[must_use]
pub(crate) fn config_set() -> &'static str {
    t("设置配置项", "set a configuration entry")
}

#[must_use]
pub(crate) fn config_unset() -> &'static str {
    t("恢复为平台缺省", "restore the platform default")
}

#[must_use]
pub(crate) fn config_edit() -> &'static str {
    t("打开编辑器", "open the editor")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::locale::Lang;

    /// 每条文案在两种语言下都必须非空，且中文语言下确实含中文
    #[test]
    fn both_languages_are_populated() {
        let all: [(&'static str, &'static str); 4] = [
            (about(), about()),
            (server(), server()),
            (java_install(), java_install()),
            (workspace_doctor_only_arg(), workspace_doctor_only_arg()),
        ];
        for (zh, en) in all {
            assert!(!zh.is_empty() && !en.is_empty(), "{zh} / {en}");
        }
        locale::set(Lang::Zh);
        let zh = server_create();
        locale::set(Lang::En);
        let en = server_create();
        assert_ne!(zh, en);
        assert!(!zh.is_ascii(), "中文文案应含非 ASCII: {zh}");
        assert!(en.is_ascii(), "英文文案应为 ASCII: {en}");
    }
}
