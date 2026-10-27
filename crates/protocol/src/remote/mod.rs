//! 远端 RPC：版本协商、认证帧与带类型的操作帧
//!
//! 远端协议与本地 supervisor IPC 共用 [`crate::Frame`] 的「版本 + JSON-line」
//! 纪律，但**版本号独立**：本地 IPC 只在同一次进程代际内对话，远端要对跨机器、
//! 跨版本的 agent 负责，两者的演进节奏不同，共用一个常量会让一侧的破坏性变更
//! 强制另一侧跟着升版。
//!
//! 帧集是**操作**而不是「仓库 CRUD」。原因不是偷懒，而是本地写路径必然伴随
//! 文件系统动作（`workspace init` 建目录与 marker、`server create` 建目录、
//! `server start` 读 `eula.txt`）：把 `port` 的仓库方法逐条搬到线上，客户端仍
//! 要在本机做这些 IO，而本机没有那些目录。因此写操作整条在 agent 侧执行同一套
//! `service`，客户端只搬运参数——「判定逻辑收敛到领域层」在跨机器场景下的
//! 唯一正确形态是「同一份 service 在对端运行」。
//!
//! 读操作同理：`Remote*Repository` 从 agent 拉回元数据填本地只读镜像，供
//! `--origin all` 合并列表使用（`docs/remote.md`「数据与同步」）。
//!
//! 连接生命周期：
//!
//! ```text
//! TLS 握手（客户端固定服务端指纹）→ Hello → Enroll（仅首次）→ Authenticate
//!   → 一串操作帧（连接内已认证，不再逐帧带 token）
//! ```

pub mod wire;

use crate::Frame;
use auth::{ClientId, Permission, Role};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};
use wire::{
    WireClient, WireJava, WireServer, WireSnapshot, WireStartOutcome, WireStatusEntry, WireStop,
    WireWorkspace,
};

/// 远端协议版本；不匹配直接拒绝，禁止猜测兼容
pub const REMOTE_PROTOCOL_VERSION: u32 = 1;

/// 单帧字节上限
///
/// 帧是「一次操作的参数或结果」，不是文件内容：备份与日志走流式帧（阶段 9），
/// 单帧超过这个量级说明对端在灌无界数据或协议被误用。设上限是防止一个恶意
/// 对端用一条没有换行的巨行把客户端内存打满。
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// agent 声明的能力
///
/// 用位掩码而不是「一堆布尔字段 + 字符串数组」：拼错的字符串会被静默忽略，
/// 能力探测就变成「有时生效」；而布尔字段每加一项都是破坏性变更。位掩码让
/// 旧客户端读新 agent 时自动忽略不认识的位，新增能力只需加一个
/// [`Capability`] 常量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Capabilities(u32);

/// 单个能力
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Capability {
    /// 备份（创建 / 恢复 / 校验）
    Backup,
    /// 文件读写与内容安装
    File,
    /// Java 扫描与安装
    Java,
    /// 流式日志与进度
    Streaming,
    /// `Forge` / `NeoForge` 安装器
    Installer,
}

impl Capability {
    /// 全部能力
    pub const ALL: &'static [Self] = &[
        Self::Backup,
        Self::File,
        Self::Java,
        Self::Streaming,
        Self::Installer,
    ];

    /// 对应的位
    #[must_use]
    pub const fn bit(self) -> u32 {
        match self {
            Self::Backup => 1 << 0,
            Self::File => 1 << 1,
            Self::Java => 1 << 2,
            Self::Streaming => 1 << 3,
            Self::Installer => 1 << 4,
        }
    }
}

impl Capabilities {
    /// 什么都不支持（握手失败前的占位）
    #[must_use]
    pub const fn none() -> Self {
        Self(0)
    }

    /// 本版本 agent 提供的全部能力
    #[must_use]
    pub const fn all() -> Self {
        let mut bits = 0;
        let mut i = 0;
        while i < Capability::ALL.len() {
            bits |= Capability::ALL[i].bit();
            i += 1;
        }
        Self(bits)
    }

    /// 是否具备某项能力
    #[must_use]
    pub const fn has(self, capability: Capability) -> bool {
        self.0 & capability.bit() != 0
    }

    /// 是否全部具备
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl From<Capability> for Capabilities {
    fn from(capability: Capability) -> Self {
        Self(capability.bit())
    }
}

/// 客户端 → agent 的一帧
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// 首帧：声明协议版本与客户端版本
    Hello {
        /// 客户端实现的远端协议版本
        protocol_version: u32,
        /// 客户端程序版本（仅用于展示与审计）
        client_version: String,
    },
    /// 用配对码换 token（首次信任）
    Enroll {
        /// 被连接端屏幕上显示的配对码
        pairing_code: String,
        /// 客户端自称（展示在 agent 的已配对列表里）
        client_name: String,
        /// 以只读角色配对（`viewer`）；缺省 `false` = `operator`
        #[serde(default)]
        read_only: bool,
    },
    /// 出示 token 认证本连接
    Authenticate {
        /// 明文 token（TLS 之上）
        token: String,
    },
    /// 连通性 / 版本 / 能力探测
    Ping,
    /// 通知 agent 在空载后退出（`remote serve --stop` 用）
    Shutdown,

    /// 列出工作目录
    WorkspaceList,
    /// 创建工作目录（`dir` 是**目标机器**上的路径）
    WorkspaceInit {
        /// 目标机器上的目录
        dir: String,
        /// 工作目录名；`None` = 取目录名
        name: Option<String>,
    },
    /// 切换当前工作区
    WorkspaceUse {
        /// 工作目录选择器（精确名或唯一前缀）
        selector: String,
    },
    /// 注销工作目录
    WorkspaceRemove {
        /// 工作目录选择器
        selector: String,
        /// 同时删除磁盘目录
        purge: bool,
    },

    /// 列出服务器
    ServerList,
    /// 创建服务器登记（不下载核心）
    ServerCreate {
        /// 工作目录选择器；`None` = 当前工作区
        workspace: Option<String>,
        /// 服务器名
        name: String,
        /// 服务端版本
        version: String,
        /// 变体标识
        variant: String,
        /// 该版本要求的 Java 大版本
        java_major: Option<i64>,
    },
    /// 注销服务器
    ServerRemove {
        /// 服务器名
        name: String,
        /// 同时删除磁盘目录
        purge: bool,
    },
    /// 在对端安装 / 更换服务端核心
    ///
    /// 下载、校验、（安装器类变体的）安装器执行全部发生在对端；客户端只
    /// 搬名字与版本。`version` 传 `latest` 由对端来源解析为最新稳定版。
    ServerInstall {
        /// 服务器名
        name: String,
        /// 服务端版本（接受 `latest` 别名）
        version: String,
    },

    /// 列出 Java 运行时
    JavaList,
    /// 扫描本机 JVM 并登记
    JavaScan,

    /// 列出已配对客户端（`remote status` / `remote revoke` 的候选来源）
    ClientList,
    /// 撤销一个客户端（立即失效其 token）
    ClientRevoke {
        /// 客户端身份（UUID 文本）或名称
        selector: String,
    },

    /// 批量实时状态
    StatusMany {
        /// 服务器身份（UUID 文本）
        ids: Vec<String>,
    },
    /// 拉起 supervisor
    ///
    /// 刻意不带启动方式：`LaunchSpec` 是对端的事实，客户端的副本只是缓存。
    /// 让客户端指定「用哪个文件启动」等于让它选远端执行的文件，与「路径由
    /// 对端按自身规则解析」冲突。
    Start {
        /// 服务器身份
        server_id: String,
        /// 堆上限（MiB）
        ram_mib: Option<u32>,
        /// 同意 EULA（缺失 / `false` 时由对端写入）
        accept_eula: bool,
    },
    /// 请求停机
    Stop {
        /// 服务器身份
        server_id: String,
        /// 优雅停机宽限秒数
        grace_secs: u64,
        /// 宽限到期是否强杀
        force: bool,
    },
    /// 控制台注入一行
    SendLine {
        /// 服务器身份
        server_id: String,
        /// 注入内容
        line: String,
    },
    /// 读取控制台日志尾部
    LogsTail {
        /// 服务器身份
        server_id: String,
        /// 回看行数
        lines: usize,
    },
    /// 日志跟随（长轮询）
    ///
    /// 返回「上次以来新增的行」；没有新行时对端最多等 `timeout_ms` 再返回空。
    /// 客户端循环发送直到用户中断——断线即取消订阅，无会话状态。
    /// `lines` 只在第一条生效（回看历史），后续调用传 0 只拿增量。
    LogsFollow {
        /// 服务器身份
        server_id: String,
        /// 回看行数；0 = 只拿增量
        lines: usize,
        /// 无新行时的等待上限（毫秒）
        timeout_ms: u64,
    },

    // ---- backup（阶段 9）：全部走元数据帧，CAS 对象留在对端 ----
    /// 创建快照（对端扫描 + 写对端 CAS）
    BackupCreate {
        /// 服务器身份
        server_id: String,
        /// 用户标签
        label: Option<String>,
        /// 备份范围（顶层项）；`None` = 用对端服务器属性里的范围
        scope: Option<Vec<String>>,
        /// 禁用增量复用（强制全量）
        no_reuse: bool,
        /// 等待 `save-all` 落盘的毫秒数
        flush_wait_ms: Option<u64>,
    },
    /// 快照列表（分页，按时间倒序）
    BackupList {
        /// 服务器身份
        server_id: String,
        /// 页码（1 基）
        page: usize,
        /// 每页条数
        page_size: usize,
    },
    /// 恢复快照
    BackupRestore {
        /// 服务器身份
        server_id: String,
        /// 快照 id
        snapshot: String,
        /// 恢复目标：`None` = 原地；`Some(name)` = 恢复为新服务
        to_new: Option<String>,
    },
    /// 删除快照记录
    BackupDelete {
        /// 服务器身份
        server_id: String,
        /// 快照 id
        snapshot: String,
    },
    /// 校验快照引用的 CAS 对象
    BackupVerify {
        /// 服务器身份
        server_id: String,
        /// 快照 id
        snapshot: String,
    },
    /// 按保留策略清理
    BackupPrune {
        /// 服务器身份
        server_id: String,
        /// 保留最近 N 份；与 `older_than_secs` 二选一
        keep: Option<usize>,
        /// 删除早于该秒数的快照
        older_than_secs: Option<u64>,
        /// 只报告不执行
        dry_run: bool,
    },
}

impl Request {
    /// 该操作要求的权限；`None` = 无需认证
    ///
    /// 映射放这里而不是 agent：它是协议的一部分，客户端可以据此在发请求前
    /// 就给出「当前角色无权执行」的提示，而不是让对端拒绝一次。
    #[must_use]
    pub fn required_permission(&self) -> Option<Permission> {
        match self {
            Self::Hello { .. } | Self::Enroll { .. } | Self::Authenticate { .. } => None,
            Self::Ping
            | Self::WorkspaceList
            | Self::ServerList
            | Self::JavaList
            | Self::StatusMany { .. } => Some(Permission::Read),
            // 控制台输出可能含玩家名、IP、管理员敲下的命令，比元数据更敏感，
            // 单列 `Logs` 而不是并进 `Read`
            Self::LogsTail { .. } | Self::LogsFollow { .. } => Some(Permission::Logs),
            Self::WorkspaceInit { .. }
            | Self::WorkspaceUse { .. }
            | Self::WorkspaceRemove { .. }
            | Self::ServerCreate { .. }
            | Self::ServerRemove { .. }
            | Self::ServerInstall { .. }
            | Self::JavaScan => Some(Permission::Write),
            Self::Start { .. } | Self::Stop { .. } | Self::SendLine { .. } => {
                Some(Permission::Control)
            }
            // 备份写对端磁盘并可能覆盖服务器目录，比 Write 更重，单列 Backup
            Self::BackupCreate { .. }
            | Self::BackupList { .. }
            | Self::BackupRestore { .. }
            | Self::BackupDelete { .. }
            | Self::BackupVerify { .. }
            | Self::BackupPrune { .. } => Some(Permission::Backup),
            // 客户端名册是访问控制数据：能看就能做社会工程，按 Manage 处理
            Self::Shutdown | Self::ClientList | Self::ClientRevoke { .. } => {
                Some(Permission::Manage)
            }
        }
    }

    /// 操作名（审计日志、错误提示共用）
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::Enroll { .. } => "enroll",
            Self::Authenticate { .. } => "authenticate",
            Self::Ping => "ping",
            Self::Shutdown => "shutdown",
            Self::WorkspaceList => "workspace.list",
            Self::WorkspaceInit { .. } => "workspace.init",
            Self::WorkspaceUse { .. } => "workspace.use",
            Self::WorkspaceRemove { .. } => "workspace.remove",
            Self::ServerList => "server.list",
            Self::ServerCreate { .. } => "server.create",
            Self::ServerRemove { .. } => "server.remove",
            Self::ServerInstall { .. } => "server.install",
            Self::JavaList => "java.list",
            Self::JavaScan => "java.scan",
            Self::ClientList => "client.list",
            Self::ClientRevoke { .. } => "client.revoke",
            Self::StatusMany { .. } => "status.many",
            Self::Start { .. } => "server.start",
            Self::Stop { .. } => "server.stop",
            Self::SendLine { .. } => "server.send",
            Self::LogsTail { .. } | Self::LogsFollow { .. } => "server.logs",
            Self::BackupCreate { .. } => "backup.create",
            Self::BackupList { .. } => "backup.list",
            Self::BackupRestore { .. } => "backup.restore",
            Self::BackupDelete { .. } => "backup.delete",
            Self::BackupVerify { .. } => "backup.verify",
            Self::BackupPrune { .. } => "backup.prune",
        }
    }
}

/// agent → 客户端的一帧
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Response {
    /// 握手结果
    Hello {
        /// agent 实现的远端协议版本
        protocol_version: u32,
        /// agent 程序版本
        agent_version: String,
        /// 本机名（`remote connect` 的缺省远端名）
        server_name: String,
        /// 能力
        capabilities: Capabilities,
        /// TLS 证书指纹（`sha256:…`）
        fingerprint: String,
        /// 本机是否已有配对客户端（`false` 时只能先 `Enroll`）
        has_clients: bool,
    },
    /// 配对成功
    Enrolled {
        /// 明文 token；客户端立刻写入凭证存储
        token: String,
        /// 服务端分配的客户端身份
        client_id: String,
        /// 角色
        role: Role,
    },
    /// 认证成功
    Authenticated {
        /// 客户端身份
        client_id: String,
        /// 角色
        role: Role,
    },
    /// 连通性探测结果
    Pong {
        /// agent 版本
        agent_version: String,
        /// agent 已运行秒数
        uptime_secs: u64,
    },
    /// 通用成功（无载荷）
    Ok,
    /// 工作目录列表 / 单条结果
    Workspaces {
        /// 工作目录
        items: Vec<WireWorkspace>,
    },
    /// 服务器列表 / 单条结果
    Servers {
        /// 服务器
        items: Vec<WireServer>,
    },
    /// Java 运行时列表 / 单条结果
    Javas {
        /// 运行时
        items: Vec<WireJava>,
    },
    /// 已配对客户端列表
    Clients {
        /// 客户端
        items: Vec<WireClient>,
    },
    /// 批量状态（顺序与请求一致）
    Statuses {
        /// 每台一项
        items: Vec<WireStatusEntry>,
    },
    /// 拉起结果
    Started {
        /// supervisor 进程 PID
        outcome: WireStartOutcome,
    },
    /// 停机结果
    Stopped {
        /// 停机结果
        outcome: WireStop,
    },
    /// 日志尾部（已按行切分）
    LogLines {
        /// 行
        lines: Vec<String>,
    },
    /// 注销结果
    Removed {
        /// 磁盘目录是否被本次调用实际删除
        disk_purged: bool,
    },

    // ---- backup（阶段 9） ----
    /// 快照创建结果
    SnapshotCreated {
        /// 新建的快照
        snapshot: WireSnapshot,
        /// 扫描期跳过的条目数
        skipped: usize,
    },
    /// 快照列表一页（顺序与请求一致）
    Snapshots {
        /// 本页快照
        items: Vec<WireSnapshot>,
        /// 该服务器的快照总数
        total: usize,
        /// 当前页码
        page: usize,
        /// 每页条数
        page_size: usize,
    },
    /// 恢复结果
    Restored {
        /// 被恢复的快照
        snapshot: WireSnapshot,
        /// 对端上的恢复目标目录
        destination: String,
        /// 写回的文件数
        files: usize,
        /// 写回的字节数
        bytes: u64,
        /// 原地恢复前自动生成的安全快照 id
        safety_snapshot: Option<String>,
        /// 按墓碑 / 快照外文件删除的路径数
        deleted: usize,
    },
    /// 删除结果
    SnapshotDeleted {
        /// 被删除的快照
        snapshot: WireSnapshot,
        /// 该快照释放的 CAS 引用计数缺口（待 prune 回收）
        orphaned: usize,
    },
    /// 校验结果
    SnapshotVerified {
        /// 被校验的快照
        snapshot: WireSnapshot,
        /// 被引用的对象数（去重后）
        objects: usize,
        /// 校验通过的字节数
        bytes: u64,
        /// 损坏或缺失的对象哈希
        broken: Vec<String>,
    },
    /// 清理结果
    Pruned {
        /// 被删除的快照
        removed: Vec<WireSnapshot>,
        /// 回收的 CAS 对象数
        objects_reclaimed: usize,
        /// 回收的字节数
        bytes_reclaimed: u64,
        /// 是否只是试运行
        dry_run: bool,
    },
    /// 业务 / 协议错误
    Error {
        /// 错误种类（`unauthorized` / `not_found` / `busy` / `protocol` / `io` …）
        kind: String,
        /// 人类可读消息
        message: String,
    },
}

/// 错误种类常量：客户端按此决定退出码与提示，不解析 `message`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorKind;

impl ErrorKind {
    /// 未认证 / token 失效 / 权限不足
    pub const UNAUTHORIZED: &'static str = "unauthorized";
    /// 目标不存在
    pub const NOT_FOUND: &'static str = "not_found";
    /// 名称冲突
    pub const ALREADY_EXISTS: &'static str = "already_exists";
    /// 状态冲突（如服务器在运行）
    pub const BUSY: &'static str = "busy";
    /// 协议 / 版本不匹配
    pub const PROTOCOL: &'static str = "protocol";
    /// 对端 IO / 存储失败
    pub const IO: &'static str = "io";
    /// 参数非法
    pub const INVALID: &'static str = "invalid";
}

/// 写出一帧（版本 + JSON + 换行 + flush）
///
/// # Errors
///
/// 序列化失败（不可达）或写入失败时返回底层 IO 错误
pub fn write_request<W: Write>(w: &mut W, request: &Request) -> io::Result<()> {
    write_remote(w, request)
}

/// 读入一帧请求
///
/// # Errors
///
/// 读取失败、超长、JSON 非法或协议版本不匹配时返回错误
pub fn read_request<R: BufRead>(r: &mut R) -> io::Result<Request> {
    read_remote(r)
}

/// 写出一帧响应
///
/// # Errors
///
/// 序列化失败（不可达）或写入失败时返回底层 IO 错误
pub fn write_response<W: Write>(w: &mut W, response: &Response) -> io::Result<()> {
    write_remote(w, response)
}

/// 读入一帧响应
///
/// # Errors
///
/// 读取失败、超长、JSON 非法或协议版本不匹配时返回错误
pub fn read_response<R: BufRead>(r: &mut R) -> io::Result<Response> {
    read_remote(r)
}

fn write_remote<W: Write, T: Serialize>(w: &mut W, body: &T) -> io::Result<()> {
    let frame = Frame {
        v: REMOTE_PROTOCOL_VERSION,
        body,
    };
    serde_json::to_writer(&mut *w, &frame)?;
    w.write_all(b"\n")?;
    w.flush()
}

fn read_remote<R: BufRead, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let line = read_bounded_line(r)?;
    let frame: Frame<T> = serde_json::from_slice(&line)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if frame.v != REMOTE_PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "remote protocol version mismatch: peer {peer}, local {local}",
                peer = frame.v,
                local = REMOTE_PROTOCOL_VERSION
            ),
        ));
    }
    Ok(frame.body)
}

/// 读一行，但先设上限
///
/// 不能用 `read_line` / `read_until`：它们会一直读到换行，对端发一条没有换行
/// 的巨行就能把内存打满。这里按 `fill_buf` 分块累加，超过 [`MAX_FRAME_BYTES`]
/// 立即报错，剩下的字节留在缓冲区里不影响连接状态。
fn read_bounded_line<R: BufRead>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let available = match r.fill_buf() {
            Ok(buf) => buf,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if available.is_empty() {
            return if line.is_empty() {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed before a frame arrived",
                ))
            } else {
                // 末行没有换行：按一帧处理，让 JSON 解析去判定它是否完整
                Ok(line)
            };
        }
        if let Some(pos) = available.iter().position(|byte| *byte == b'\n') {
            line.extend_from_slice(&available[..=pos]);
            r.consume(pos + 1);
            return Ok(line);
        }
        let len = available.len();
        line.extend_from_slice(available);
        r.consume(len);
        if line.len() > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame exceeds {MAX_FRAME_BYTES} bytes"),
            ));
        }
    }
}

/// 校验对端声明的协议版本
///
/// # Errors
///
/// 版本不一致时返回 [`ErrorKind::PROTOCOL`] 的说明
pub fn check_protocol_version(peer: u32) -> Result<(), String> {
    if peer == REMOTE_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(format!(
            "remote protocol version mismatch: peer {peer}, local {REMOTE_PROTOCOL_VERSION}"
        ))
    }
}

/// 已认证连接的上下文
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// 客户端身份
    pub client_id: ClientId,
    /// 角色
    pub role: Role,
}

impl Session {
    /// 该角色能否执行某个请求
    #[must_use]
    pub fn permits(&self, request: &Request) -> bool {
        request
            .required_permission()
            .is_none_or(|permission| self.role.permits(permission))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip(request: &Request) -> Request {
        let mut buf = Vec::new();
        write_request(&mut buf, request).expect("write");
        let mut cursor = Cursor::new(buf);
        read_request(&mut cursor).expect("read")
    }

    fn roundtrip_response(response: &Response) -> Response {
        let mut buf = Vec::new();
        write_response(&mut buf, response).expect("write");
        let mut cursor = Cursor::new(buf);
        read_response(&mut cursor).expect("read")
    }

    /// 一条最小合法的 `WireSnapshot`（完整内容清单，验编解码不丢字段）
    fn snapshot_wire() -> WireSnapshot {
        WireSnapshot {
            id: "20260101t000000-abc123".to_owned(),
            workspace_id: "survival".to_owned(),
            server: "web".to_owned(),
            label: Some("before-upgrade".to_owned()),
            parent: None,
            scope: vec!["world".to_owned()],
            status: "complete".to_owned(),
            entries: vec![("world/level.dat".to_owned(), "a".repeat(64))],
            deleted: vec!["logs/old.log".to_owned()],
            stamps: vec![("world/level.dat".to_owned(), 4096, 1_700_000_000)],
            added: 1,
            reused: 0,
            total_bytes: 4096,
            added_bytes: 4096,
            server_online: false,
            created_at: 1_700_000_000,
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // 帧样本清单本身就是长列表，拆分反而难对账
    fn every_request_roundtrips_with_version() {
        let requests = [
            Request::Hello {
                protocol_version: REMOTE_PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            },
            Request::Enroll {
                pairing_code: "4821-9375".to_owned(),
                client_name: "laptop".to_owned(),
                read_only: true,
            },
            Request::Authenticate {
                token: "a".repeat(64),
            },
            Request::Ping,
            Request::Shutdown,
            Request::WorkspaceList,
            Request::WorkspaceInit {
                dir: "/srv/mc".to_owned(),
                name: Some("w1".to_owned()),
            },
            Request::WorkspaceUse {
                selector: "w1".to_owned(),
            },
            Request::WorkspaceRemove {
                selector: "w1".to_owned(),
                purge: true,
            },
            Request::ServerList,
            Request::ServerCreate {
                workspace: Some("w1".to_owned()),
                name: "web".to_owned(),
                version: "1.21.1".to_owned(),
                variant: "paper".to_owned(),
                java_major: Some(21),
            },
            Request::ServerRemove {
                name: "web".to_owned(),
                purge: false,
            },
            Request::JavaList,
            Request::JavaScan,
            Request::ClientList,
            Request::ClientRevoke {
                selector: "laptop".to_owned(),
            },
            Request::StatusMany {
                ids: vec!["550e8400-e29b-41d4-a716-446655440000".to_owned()],
            },
            Request::Start {
                server_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
                ram_mib: Some(2048),
                accept_eula: true,
            },
            Request::Stop {
                server_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
                grace_secs: 30,
                force: false,
            },
            Request::SendLine {
                server_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
                line: "list".to_owned(),
            },
            Request::LogsTail {
                server_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
                lines: 30,
            },
            Request::BackupCreate {
                server_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
                label: Some("before-upgrade".to_owned()),
                scope: Some(vec!["world".to_owned()]),
                no_reuse: false,
                flush_wait_ms: Some(2000),
            },
            Request::BackupList {
                server_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
                page: 1,
                page_size: 20,
            },
            Request::BackupRestore {
                server_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
                snapshot: "20260101t000000-abc123".to_owned(),
                to_new: Some("web-restored".to_owned()),
            },
            Request::BackupDelete {
                server_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
                snapshot: "20260101t000000-abc123".to_owned(),
            },
            Request::BackupVerify {
                server_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
                snapshot: "20260101t000000-abc123".to_owned(),
            },
            Request::BackupPrune {
                server_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
                keep: Some(3),
                older_than_secs: None,
                dry_run: true,
            },
        ];
        for request in requests {
            let back = roundtrip(&request);
            assert_eq!(
                serde_json::to_value(&back).expect("json"),
                serde_json::to_value(&request).expect("json"),
                "{} 往返不一致",
                request.operation()
            );
        }
    }

    #[test]
    fn responses_roundtrip() {
        let responses = [
            Response::Hello {
                protocol_version: REMOTE_PROTOCOL_VERSION,
                agent_version: "0.1.0".to_owned(),
                server_name: "prod-box".to_owned(),
                capabilities: Capabilities::all(),
                fingerprint: "sha256:ab".to_owned(),
                has_clients: false,
            },
            Response::Enrolled {
                token: "b".repeat(64),
                client_id: "6f1a1d3e-0b7c-4a51-9c8e-2b0f3a4d5e60".to_owned(),
                role: Role::Operator,
            },
            Response::Authenticated {
                client_id: "6f1a1d3e-0b7c-4a51-9c8e-2b0f3a4d5e60".to_owned(),
                role: Role::Viewer,
            },
            Response::Pong {
                agent_version: "0.1.0".to_owned(),
                uptime_secs: 12,
            },
            Response::Ok,
            Response::Workspaces { items: Vec::new() },
            Response::Servers { items: Vec::new() },
            Response::Javas { items: Vec::new() },
            Response::Clients { items: Vec::new() },
            Response::Statuses { items: Vec::new() },
            Response::Started {
                outcome: WireStartOutcome { supervisor_pid: 7 },
            },
            Response::Stopped {
                outcome: WireStop::AlreadyStopped,
            },
            Response::LogLines {
                lines: vec!["[12:00:00] done".to_owned()],
            },
            Response::Removed { disk_purged: true },
            Response::SnapshotCreated {
                snapshot: snapshot_wire(),
                skipped: 2,
            },
            Response::Snapshots {
                items: vec![snapshot_wire()],
                total: 1,
                page: 1,
                page_size: 20,
            },
            Response::Restored {
                snapshot: snapshot_wire(),
                destination: "/srv/mc/web".to_owned(),
                files: 10,
                bytes: 4096,
                safety_snapshot: Some("20260101t000100-def456".to_owned()),
                deleted: 1,
            },
            Response::SnapshotDeleted {
                snapshot: snapshot_wire(),
                orphaned: 3,
            },
            Response::SnapshotVerified {
                snapshot: snapshot_wire(),
                objects: 10,
                bytes: 4096,
                broken: Vec::new(),
            },
            Response::Pruned {
                removed: vec![snapshot_wire()],
                objects_reclaimed: 3,
                bytes_reclaimed: 12_288,
                dry_run: false,
            },
            Response::Error {
                kind: ErrorKind::NOT_FOUND.to_owned(),
                message: "server web not found".to_owned(),
            },
        ];
        for response in responses {
            let back = roundtrip_response(&response);
            assert_eq!(
                serde_json::to_value(&back).expect("json"),
                serde_json::to_value(&response).expect("json")
            );
        }
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let mut cursor = Cursor::new(b"{\"v\":99,\"op\":\"ping\"}\n".to_vec());
        let err = read_request(&mut cursor).expect_err("v99");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("version mismatch"), "{err}");
        assert!(check_protocol_version(99).is_err());
        assert!(check_protocol_version(REMOTE_PROTOCOL_VERSION).is_ok());
    }

    #[test]
    fn oversized_and_garbage_frames_are_rejected() {
        let mut cursor = Cursor::new(b"not json\n".to_vec());
        let err = read_request(&mut cursor).expect_err("garbage");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let mut cursor = Cursor::new(Vec::new());
        let err = read_request(&mut cursor).expect_err("eof");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

        // 没有换行的超长行必须在读到上限时停下并报错
        let mut huge = Vec::new();
        huge.extend_from_slice(b"{\"v\":1,\"op\":\"");
        huge.resize(MAX_FRAME_BYTES + 16, b'a');
        let mut cursor = Cursor::new(huge);
        let err = read_request(&mut cursor).expect_err("oversized");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    /// 权限映射：读操作只读，写操作要 Write，控制要 Control，管理要 Manage
    #[test]
    fn permission_mapping_matches_operation_risk() {
        assert_eq!(Request::Ping.required_permission(), Some(Permission::Read));
        assert_eq!(
            Request::WorkspaceInit {
                dir: "/srv/mc".to_owned(),
                name: None
            }
            .required_permission(),
            Some(Permission::Write)
        );
        assert_eq!(
            Request::Start {
                server_id: "id".to_owned(),
                ram_mib: None,
                accept_eula: false
            }
            .required_permission(),
            Some(Permission::Control)
        );
        assert_eq!(
            Request::Shutdown.required_permission(),
            Some(Permission::Manage)
        );
        // 握手与认证本身不需要权限（否则永远无法完成认证）
        assert_eq!(
            Request::Hello {
                protocol_version: 1,
                client_version: "0.1.0".to_owned()
            }
            .required_permission(),
            None
        );
        assert_eq!(
            Request::Authenticate {
                token: "a".repeat(64)
            }
            .required_permission(),
            None
        );
    }

    #[test]
    fn viewer_cannot_write_and_operator_cannot_manage() {
        let viewer = Session {
            client_id: ClientId::parse("6f1a1d3e-0b7c-4a51-9c8e-2b0f3a4d5e60").expect("id"),
            role: Role::Viewer,
        };
        assert!(viewer.permits(&Request::Ping));
        assert!(viewer.permits(&Request::ServerList));
        assert!(!viewer.permits(&Request::JavaScan));
        assert!(!viewer.permits(&Request::Shutdown));

        let operator = Session {
            client_id: ClientId::generate(),
            role: Role::Operator,
        };
        assert!(operator.permits(&Request::JavaScan));
        assert!(operator.permits(&Request::Start {
            server_id: "id".to_owned(),
            ram_mib: None,
            accept_eula: false
        }));
        assert!(!operator.permits(&Request::Shutdown));
    }

    /// 操作名唯一：审计日志与错误提示都按它归类
    #[test]
    fn operation_names_are_unique() {
        let names = [
            Request::Ping.operation(),
            Request::WorkspaceList.operation(),
            Request::ServerList.operation(),
            Request::JavaList.operation(),
        ];
        let mut seen = Vec::new();
        for name in names {
            assert!(!seen.contains(&name), "{name} 重复");
            seen.push(name);
        }
    }
}
