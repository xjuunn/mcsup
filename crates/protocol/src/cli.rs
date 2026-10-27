//! CLI JSON 输出契约：`--output json` 单行数据的唯一真相来源
//!
//! 这里只放「机器可读的行类型」：字段名与形状一经发布即是对第三方的
//! 承诺，破坏性变更只加字段、不改旧字段（见 docs/embedding.md 的
//! CLI 契约一节）。REST / WebSocket / 桌面客户端复用同一批类型，
//! 禁止各入口自行定义 DTO。
//!
//! 渲染（表格、着色、进度）不在此层：那是入口的职责，payload 保持
//! 与展示无关。每条 payload 都是 `--output json` 下 stdout 的一行；
//! `--follow` 流式行是 JSONL（一行一个对象）。

use domain::server::version::ServerVariant;
use serde::Serialize;

/// `server list` 单行
#[derive(Debug, Serialize)]
pub struct ServerRow<'a> {
    /// 来源：`local` 或远端名
    pub origin: String,
    /// 服务器名
    pub name: &'a str,
    /// 工作区显示名
    pub workspace: String,
    /// 服务端版本
    pub version: &'a str,
    /// 变体标识（`as_str` 口径）
    pub variant: &'a str,
    /// 服务器目录绝对路径
    pub path: String,
    /// 已安装核心的相对路径；未安装为 `null`
    pub jar: Option<String>,
    /// 是否链接服务器（外部目录经符号链接注册）
    pub linked: bool,
    /// 领域状态口径；无 supervisor 时为 `stopped`
    pub status: String,
}

/// `server create` / `server link` 结果
#[derive(Debug, Serialize)]
pub struct CreateRow<'a> {
    /// 服务器名
    pub name: &'a str,
    /// 服务端版本
    pub version: &'a str,
    /// 变体标识
    pub variant: &'a str,
    /// 服务器目录绝对路径
    pub path: String,
    /// `server link` 时为 `Some`；`server create` 省略该字段
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked: Option<bool>,
}

/// `server start` / `server restart` 结果
#[derive(Debug, Serialize)]
pub struct StartJson<'a> {
    /// 服务器名
    pub name: &'a str,
    /// supervisor 进程 PID
    pub supervisor_pid: u32,
}

/// `server stop` 结果
#[derive(Debug, Serialize)]
pub struct StopJson<'a> {
    /// 服务器名
    pub name: &'a str,
    /// 停机结果：`stopped` / `already_stopped`
    pub outcome: &'a str,
    /// 停机耗时秒数；本来就没跑时为 `null`
    pub elapsed_secs: Option<u64>,
}

/// `server status` 结果
#[derive(Debug, Serialize)]
pub struct StatusJson<'a> {
    /// 服务器名
    pub name: &'a str,
    /// 服务端版本
    pub version: &'a str,
    /// 变体标识
    pub variant: &'a str,
    /// 进程是否存活（Starting / Running / Stopping 为真）
    pub running: bool,
    /// 领域状态口径；无 supervisor 时为 `stopped`
    pub status: &'a str,
    /// 服务端进程 PID；未运行为 `null`
    pub pid: Option<u32>,
    /// 已运行秒数；未运行为 `null`
    pub uptime_secs: Option<u64>,
    /// 退出码；进程已退出时为 `Some`
    pub exit_code: Option<i32>,
}

/// `server send` 结果
#[derive(Debug, Serialize)]
pub struct SendJson<'a> {
    /// 服务器名
    pub name: &'a str,
    /// 注入的命令行
    pub command: &'a str,
}

/// `server logs --tail` 结果（不含 follow 增量）
#[derive(Debug, Serialize)]
pub struct LogsTailJson<'a> {
    /// 服务器名
    pub name: &'a str,
    /// 末尾若干行原样输出
    pub lines: Vec<String>,
}

/// `server logs --follow` 的流式单行（JSONL）
#[derive(Debug, Serialize)]
pub struct LogLineJson<'a> {
    /// 一行日志原文
    pub line: &'a str,
}

/// `server remove` 结果
#[derive(Debug, Serialize)]
pub struct RemoveJson {
    /// 服务器名
    pub name: String,
    /// 服务器目录绝对路径
    pub path: String,
    /// 是否请求了连磁盘数据一并删除（即 `--purge`）
    pub purged: bool,
    /// 目录是否真的被本次调用删掉了
    ///
    /// 与 `purged` 不同：未请求删除、链接服务器（只删链接）、或目录本就不存在
    /// 时为 `false`。消费者据此区分「删成功」与「无事可做」。
    pub disk_purged: bool,
    /// 是否链接服务器
    pub linked: bool,
}

/// `server install` / `java install` 的已安装产物
#[derive(Debug, Serialize)]
pub struct InstalledJson {
    /// 版本标识
    pub version: String,
    /// 落盘路径
    pub path: String,
    /// 字节数
    pub size: u64,
}

/// `server props` 列表行
#[derive(Debug, Serialize)]
pub struct PropRow<'a> {
    /// 属性键
    pub key: &'a str,
    /// 属性值
    pub value: &'a str,
}

/// `server props key=value` 写入结果
#[derive(Debug, Serialize)]
pub struct PropsWriteJson {
    /// 服务器名
    pub name: String,
    /// server.properties 路径
    pub path: String,
    /// 本次写入的键值
    pub updated: std::collections::BTreeMap<String, String>,
}

/// `server set` 结果
#[derive(Debug, Serialize)]
pub struct SetJson {
    /// 服务器名
    pub name: String,
    /// 属性键
    pub key: String,
    /// 写入值
    pub value: String,
    /// 写入后的完整属性集
    pub settings: SettingsJson,
}

/// 属性集的机器视图（`server set` / `server version` 共用）
#[derive(Debug, Serialize)]
pub struct SettingsJson {
    /// 绑定的 Java 大版本；未绑定为 `null`
    pub java_version: Option<i64>,
    /// 堆内存上限（MiB）；未设置为 `null`
    pub ram_mib: Option<u32>,
    /// 是否开机自动拉起
    pub autostart: bool,
}

impl From<domain::server::settings::ServerSettings> for SettingsJson {
    fn from(settings: domain::server::settings::ServerSettings) -> Self {
        Self {
            java_version: settings.java_major.map(domain::java::JavaMajor::as_i64),
            ram_mib: settings.ram_mib,
            autostart: settings.autostart,
        }
    }
}

/// `server version` 里已安装核心的信息
#[derive(Debug, Serialize)]
pub struct JarJson {
    /// 核心文件路径
    pub path: String,
    /// 字节数
    pub size: u64,
    /// 校验算法（如 `sha1`）
    pub checksum_algo: &'static str,
    /// 校验和十六进制；上游未提供校验和时为空串
    pub checksum: String,
    /// 是否有真实校验和；`false` 时 `checksum` 为空，不得作为校验依据
    pub verified: bool,
}

/// `server version` 结果
#[derive(Debug, Serialize)]
pub struct VersionJson<'a> {
    /// 服务器名
    pub name: &'a str,
    /// 注册的版本
    pub version: &'a str,
    /// 变体标识
    pub variant: &'a str,
    /// 核心来源名（按变体取自已注册来源）；无匹配来源时为 `未知`
    pub source: &'a str,
    /// 已安装核心；尚未安装为 `null`
    pub jar: Option<JarJson>,
}

/// `workspace list` 单行
#[derive(Debug, Serialize)]
pub struct WorkspaceRow<'a> {
    /// 来源：`local` 或远端名
    pub origin: String,
    /// 工作区名
    pub name: &'a str,
    /// 目录绝对路径
    pub path: String,
    /// 区内服务器数
    pub servers: usize,
    /// 是否当前工作区
    pub current: bool,
}

/// `workspace init` 结果
#[derive(Debug, Serialize)]
pub struct WorkspaceInitRow<'a> {
    /// 工作区名
    pub name: &'a str,
    /// 目录绝对路径
    pub path: String,
    /// 结果：`created` / `recovered` / `already`
    pub status: &'a str,
}

/// `workspace use` 结果
#[derive(Debug, Serialize)]
pub struct WorkspaceUseJson<'a> {
    /// 工作区名
    pub name: &'a str,
    /// 目录绝对路径
    pub path: String,
}

/// `workspace remove` 结果
#[derive(Debug, Serialize)]
pub struct WorkspaceRemoveJson<'a> {
    /// 工作区名
    pub name: &'a str,
    /// 目录绝对路径
    pub path: String,
    /// 是否请求了连磁盘数据一并删除（即 `--purge` / 交互确认）
    pub purged: bool,
    /// 磁盘目录是否真的被删掉了
    ///
    /// 与 `purged` 不同：未请求删除，或目标目录本就不存在时为 `false`。
    /// 消费者据此区分「删成功」与「无事可做」，不要用 `purged` 推断磁盘状态。
    pub disk_purged: bool,
}

/// `workspace doctor` 发现项
///
/// `kind` 是稳定标识（`service::CheckKind::as_str`），`detail` 携带该类型的
/// 可寻址字段。`message` 保留一个版本周期以兼容既有消费者，新代码只用
/// `kind` + `detail`。
#[derive(Debug, Serialize)]
pub struct DoctorFindingJson<'a> {
    /// 级别：`error` / `warning`
    pub level: &'a str,
    /// 发现类型标识（kebab-case）
    pub kind: &'a str,
    /// 所属工作区；全局发现为 `null`
    pub workspace: Option<&'a str>,
    /// 该类型的结构化字段
    pub detail: serde_json::Value,
    /// 问题描述（兼容字段）
    pub message: String,
}

/// `workspace doctor` 结果
#[derive(Debug, Serialize)]
pub struct DoctorJson<'a> {
    /// 行格式版本
    pub schema: u32,
    /// 是否存在 error 级发现
    pub has_errors: bool,
    /// error 级数量
    pub errors: usize,
    /// warning 级数量
    pub warnings: usize,
    /// 全部发现项
    pub findings: Vec<DoctorFindingJson<'a>>,
}

/// `java list` 单行
#[derive(Debug, Serialize)]
pub struct JavaRow<'a> {
    /// 来源：`local` 或远端名
    pub origin: String,
    /// Java 大版本
    pub major: i64,
    /// 完整版本串
    pub version: &'a str,
    /// 厂商标识
    pub vendor: &'a str,
    /// 是否 mcsup 安装管理
    pub managed: bool,
    /// JVM home 路径
    pub path: String,
}

/// `java scan` 结果
#[derive(Debug, Serialize)]
pub struct JavaScanJson {
    /// 新注册数
    pub registered: usize,
    /// 跳过数
    pub skipped: usize,
}

/// `java install` 结果
#[derive(Debug, Serialize)]
pub struct JavaInstallJson<'a> {
    /// Java 大版本
    pub major: i64,
    /// 完整版本串
    pub version: &'a str,
    /// 厂商标识
    pub vendor: &'a str,
    /// JVM home 路径
    pub path: String,
}

/// `java uninstall` 结果
#[derive(Debug, Serialize)]
pub struct JavaUninstallJson {
    /// JVM home 路径
    pub path: String,
    /// 卸载类型：`managed`（已删盘）/ `system`（仅移除登记）
    pub kind: &'static str,
}

/// 变体名到展示名：保持与 `ServerVariant::as_str` 一致，供渲染层复用
#[must_use]
pub fn variant_str(variant: &ServerVariant) -> &str {
    variant.as_str()
}

/// `config show` 的单行
///
/// `value` 永远是可用的生效值；`source` 说明它来自哪里，调用方不必自己去
/// 比对缺省值。
#[derive(Debug, Serialize)]
pub struct ConfigRow<'a> {
    /// 配置键（kebab-case）
    pub key: &'a str,
    /// 生效值
    pub value: String,
    /// 来源：`file`（配置文件中显式设置）/ `default`（平台缺省）
    pub source: &'a str,
}

/// `config show` 结果
#[derive(Debug, Serialize)]
pub struct ConfigShowJson<'a> {
    /// 行格式版本
    pub schema: u32,
    /// 配置文件路径
    pub path: String,
    /// 配置文件是否存在
    pub exists: bool,
    /// 全部配置键的生效值
    pub items: Vec<ConfigRow<'a>>,
}

/// `config set` / `config unset` 结果
#[derive(Debug, Serialize)]
pub struct ConfigSetJson<'a> {
    /// 被修改的键
    pub key: &'a str,
    /// 修改后的生效值
    pub value: String,
    /// `set` 时为 `Some`；`unset` 时为 `null`
    pub previous: Option<String>,
    /// 配置文件路径
    pub path: String,
}

/// `doctor` 的一条发现
#[derive(Debug, Serialize)]
pub struct PlatformFindingJson<'a> {
    /// 级别：`error` / `warning`
    pub level: &'a str,
    /// 发现类型标识（kebab-case）
    pub kind: &'a str,
    /// 结构化字段（含可寻址路径 / 名称）
    pub detail: serde_json::Value,
    /// 问题描述
    pub message: String,
    /// 修复建议
    pub fix: String,
}

/// `doctor` 结果
#[derive(Debug, Serialize)]
pub struct PlatformDoctorJson<'a> {
    /// 行格式版本
    pub schema: u32,
    /// 是否存在 error 级发现
    pub has_errors: bool,
    /// error 级数量
    pub errors: usize,
    /// warning 级数量
    pub warnings: usize,
    /// 全部发现项
    pub findings: Vec<PlatformFindingJson<'a>>,
}

/// `version` 结果
#[derive(Debug, Serialize)]
pub struct VersionInfoJson<'a> {
    /// 程序名
    pub name: &'a str,
    /// 版本号（与 `--version` 一致）
    pub version: &'a str,
    /// 构建目标三元组
    pub target: &'a str,
    /// 编译配置：`debug` / `release`
    pub profile: &'a str,
    /// supervisor IPC 协议版本
    pub protocol_version: u32,
}

/// `remote list` / `remote status` 单行
#[derive(Debug, Serialize)]
pub struct RemoteRow {
    /// 远端名（同时是 `--origin` 的取值）
    pub name: String,
    /// 用户输入的原始 target
    pub target: String,
    /// 接入方式：`direct` / `ssh`
    pub via: String,
    /// 连接状态：`ok` / `unreachable` / `unauthorized`
    pub state: String,
    /// 角色：`viewer` / `operator` / `admin`；未知时为 `null`
    pub role: Option<String>,
    /// 对端 agent 版本；不可达时为 `null`
    pub agent_version: Option<String>,
    /// 证书指纹
    pub fingerprint: Option<String>,
    /// 是否只读配对
    pub read_only: bool,
}

/// `remote list` 信封
#[derive(Debug, Serialize)]
pub struct RemoteListJson {
    /// 全部远端
    pub items: Vec<RemoteRow>,
}

/// `remote ping` 结果
#[derive(Debug, Serialize)]
pub struct RemotePingJson<'a> {
    /// 远端名
    pub name: &'a str,
    /// 对端 agent 版本
    pub agent_version: &'a str,
    /// 远端协议版本
    pub protocol_version: u32,
    /// 已运行秒数
    pub uptime_secs: u64,
    /// 能力名（稳定升序）
    pub capabilities: Vec<&'a str>,
}

/// `remote connect` / `reconnect` 结果
#[derive(Debug, Serialize)]
pub struct RemoteConnectJson<'a> {
    /// 远端名
    pub name: &'a str,
    /// 连接目标
    pub target: &'a str,
    /// 证书指纹（已固定）
    pub fingerprint: &'a str,
    /// 角色
    pub role: &'a str,
    /// 同步到的工作区数
    pub workspaces: usize,
    /// 同步到的服务器数
    pub servers: usize,
    /// 同步到的 Java 运行时数
    pub java: usize,
}

/// `remote disconnect` 结果
#[derive(Debug, Serialize)]
pub struct RemoteDisconnectJson<'a> {
    /// 远端名
    pub name: &'a str,
    /// 是否删除了本地配对记录（幂等：本来就没有时为 `false`）
    pub removed: bool,
}

/// `remote serve` 结果（常驻进程启动时打印一次）
#[derive(Debug, Serialize)]
pub struct RemoteServeJson<'a> {
    /// 本机名
    pub name: &'a str,
    /// 监听地址
    pub listen: &'a str,
    /// 证书指纹
    pub fingerprint: &'a str,
    /// 当前配对码；无可用码时为 `null`
    pub pairing_code: Option<&'a str>,
}

/// `remote revoke` 结果
#[derive(Debug, Serialize)]
pub struct RemoteRevokeJson<'a> {
    /// 被撤销的客户端
    pub client: &'a str,
    /// 是否真的删除了记录（幂等）
    pub removed: bool,
}

/// `backup list` 单行
///
/// 字段口径与 `service` 层的快照类型对齐，但只暴露稳定、机器可读的
/// 部分：`scope` 是顶层项列表，`created_at` 是 Unix 秒。
#[derive(Debug, Serialize)]
pub struct BackupRow<'a> {
    /// 快照 id
    pub id: &'a str,
    /// 所属服务器名
    pub server: &'a str,
    /// 用户标签；无标签为 `null`
    pub label: Option<&'a str>,
    /// 父快照 id（增量基线）；首份为 `null`
    pub parent: Option<&'a str>,
    /// 状态：`in_progress` / `complete` / `failed`
    pub status: &'a str,
    /// 备份范围（顶层项）；空数组 = 整个服务器目录
    pub scope: Vec<String>,
    /// 条目数
    pub files: usize,
    /// 本次新建的 CAS 对象数
    pub added: usize,
    /// 复用已存在对象的文件数
    pub reused: usize,
    /// 逻辑总字节数
    pub total_bytes: u64,
    /// 本次净增字节数
    pub added_bytes: u64,
    /// 快照创建时服务器是否在运行
    pub server_online: bool,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
}

/// `backup list` 分页信封
#[derive(Debug, Serialize)]
pub struct BackupListJson<'a> {
    /// 本页快照（按时间倒序）
    pub items: Vec<BackupRow<'a>>,
    /// 该服务器快照总数
    pub total: usize,
    /// 当前页码（1 基）
    pub page: usize,
    /// 每页条数
    pub page_size: usize,
    /// 总页数
    pub pages: usize,
}

/// `backup create` 结果
#[derive(Debug, Serialize)]
pub struct BackupCreateJson<'a> {
    /// 新建的快照
    pub snapshot: BackupRow<'a>,
    /// 扫描期跳过的条目数（运行期文件、范围外、符号链接）
    pub skipped: usize,
}

/// `backup restore` 结果
#[derive(Debug, Serialize)]
pub struct BackupRestoreJson<'a> {
    /// 被恢复的快照 id
    pub snapshot: &'a str,
    /// 恢复目标目录绝对路径
    pub destination: String,
    /// 写回的文件数
    pub files: usize,
    /// 写回的字节数
    pub bytes: u64,
    /// 原地恢复前自动生成的安全快照 id；`--to` 恢复为 `null`
    pub safety_snapshot: Option<&'a str>,
    /// 按墓碑 / 快照外文件删除的路径数
    pub deleted: usize,
}

/// `backup delete` 结果
#[derive(Debug, Serialize)]
pub struct BackupDeleteJson<'a> {
    /// 被删除的快照 id
    pub id: &'a str,
    /// 该快照释放的 CAS 对象数（可被 prune 回收）
    pub orphaned: usize,
}

/// `backup verify` 结果
#[derive(Debug, Serialize)]
pub struct BackupVerifyJson<'a> {
    /// 被校验的快照 id
    pub id: &'a str,
    /// 是否完好
    pub healthy: bool,
    /// 被引用的对象数（去重后）
    pub objects: usize,
    /// 校验通过的字节数
    pub bytes: u64,
    /// 损坏或缺失的对象哈希
    pub broken: Vec<&'a str>,
}

/// `backup prune` 结果
#[derive(Debug, Serialize)]
pub struct BackupPruneJson<'a> {
    /// 被删除的快照 id 列表
    pub removed: Vec<&'a str>,
    /// 回收的 CAS 对象数
    pub objects_reclaimed: usize,
    /// 回收的字节数
    pub bytes_reclaimed: u64,
    /// 是否只是试运行
    pub dry_run: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 字段名是公开契约：改名即破坏第三方，测试钉住形状
    #[test]
    fn server_row_field_names_are_stable() {
        let row = ServerRow {
            origin: "local".to_owned(),
            name: "alpha",
            workspace: "ws".to_owned(),
            version: "1.21.1",
            variant: "vanilla",
            path: "/srv/alpha".to_owned(),
            jar: None,
            linked: false,
            status: "stopped".to_owned(),
        };
        let json = serde_json::to_string(&row).expect("serialize");
        for field in [
            "\"origin\":\"local\"",
            "\"name\":\"alpha\"",
            "\"workspace\":\"ws\"",
            "\"version\":\"1.21.1\"",
            "\"variant\":\"vanilla\"",
            "\"path\":\"/srv/alpha\"",
            "\"linked\":false",
            "\"status\":\"stopped\"",
        ] {
            assert!(json.contains(field), "missing {field} in {json}");
        }
    }

    /// `CreateRow::linked` 为 None 时省略，Some 时保留
    #[test]
    fn create_row_omits_none_linked() {
        let create = CreateRow {
            name: "alpha",
            version: "1.21.1",
            variant: "vanilla",
            path: "/srv".to_owned(),
            linked: None,
        };
        let json = serde_json::to_string(&create).expect("serialize");
        assert!(!json.contains("linked"), "{json}");
    }

    #[test]
    fn variant_str_matches_domain() {
        assert_eq!(variant_str(&ServerVariant::Vanilla), "vanilla");
    }

    /// 备份分页信封与单行字段名是公开契约，钉住形状
    #[test]
    fn backup_list_shape_is_stable() {
        let row = BackupRow {
            id: "20260101t000000-abcdef",
            server: "alpha",
            label: Some("before-upgrade"),
            parent: None,
            status: "complete",
            scope: vec!["world".to_owned()],
            files: 3,
            added: 2,
            reused: 1,
            total_bytes: 100,
            added_bytes: 40,
            server_online: true,
            created_at: 1_700_000_000,
        };
        let page = BackupListJson {
            items: vec![row],
            total: 1,
            page: 1,
            page_size: 10,
            pages: 1,
        };
        let json = serde_json::to_string(&page).expect("serialize");
        for field in [
            "\"total\":1",
            "\"page\":1",
            "\"page_size\":10",
            "\"pages\":1",
            "\"id\":\"20260101t000000-abcdef\"",
            "\"label\":\"before-upgrade\"",
            "\"parent\":null",
            "\"status\":\"complete\"",
            "\"scope\":[\"world\"]",
            "\"server_online\":true",
        ] {
            assert!(json.contains(field), "missing {field}: {json}");
        }
    }

    /// 恢复结果的 `safety_snapshot` 为 `null` 时也要出现（`--to` 语义）
    #[test]
    fn backup_restore_reports_null_safety_snapshot() {
        let restore = BackupRestoreJson {
            snapshot: "20260101t000000-abcdef",
            destination: "/srv/copy".to_owned(),
            files: 2,
            bytes: 12,
            safety_snapshot: None,
            deleted: 0,
        };
        let json = serde_json::to_string(&restore).expect("serialize");
        assert!(json.contains("\"safety_snapshot\":null"), "{json}");
        assert!(json.contains("\"destination\":\"/srv/copy\""), "{json}");
    }
}
