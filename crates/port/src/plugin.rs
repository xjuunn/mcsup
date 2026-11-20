//! 插件宿主的能力端口
//!
//! `crates/plugin` 需要「列服务器、发控制台、建备份」，但不能依赖 `service`
//! 或其它实现层。方向在这里反转：宿主只依赖这些窄 trait，由 `service` /
//! `storage` / `provider` 实现，门面 `bootstrap` 负责装配。
//!
//! # 为什么按能力拆开
//!
//! 一个 `HostApi` God Trait 会让任何实现方都被迫实现全部方法，测试也无法
//! 逐能力替换。拆开后：
//!
//! - 每个 trait 对应一项 [`domain::plugin::capability::Capability`]；
//! - 未实现的阶段用默认方法返回 `unsupported`，而不是静默成功；
//! - mock 只需要实现被调用的那一小块。
//!
//! # 错误
//!
//! 全部方法返回 [`HostError`]：它已经带上稳定错误码与 [`ErrorCategory`]，
//! 入口只做映射。`[禁止]` 在实现里把「能力未授予」降级成成功。
//!
//! # 限额
//!
//! 限额在 `docs/plugins.md` 第 9.2 节定义，常量集中在本模块。实现必须
//! **拒绝**越界请求（`limit_exceeded`），不得截断后返回成功。

use crate::StopOutcome;
use crate::error::ErrorCategory;
use domain::java::{JavaMajor, JavaRuntime};
use domain::plugin::capability::Capability;
use domain::plugin::error::PluginErrorCode;
use domain::plugin::manifest::Manifest;
use domain::plugin::revocation::RevocationList;
use domain::plugin::state::PluginState;
use domain::plugin::{PluginId, PluginScope};
use domain::server::artifact::Checksum;
use domain::server::content::{ContentIdentity, ContentKind};
use domain::server::meta::ServerName;
use domain::server::status::ServerStatus;
use domain::server::version::VersionId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use domain::plugin::error::PluginErrorCode as ErrorCode;

// ---- 限额（docs/plugins.md 9.2 / 5.4） ----

/// `kv` 键最大字节数
pub const MAX_KV_KEY_BYTES: usize = 256;

/// `kv` 单值最大字节数
pub const MAX_KV_VALUE_BYTES: usize = 256 * 1024;

/// 单插件 `kv` 总量最大字节数
pub const MAX_KV_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// `kv` 单次列举最大键数
pub const MAX_KV_LIST: usize = 1000;

/// 密钥键最大字节数
pub const MAX_SECRET_KEY_BYTES: usize = 128;

/// 密钥值最大字节数
pub const MAX_SECRET_VALUE_BYTES: usize = 8 * 1024;

/// 控制台单次读取行数上限
pub const MAX_CONSOLE_LINES: usize = 500;

/// 控制台注入单行最大字节数
pub const MAX_CONSOLE_LINE_BYTES: usize = 4096;

/// 文件单次读 / 写最大字节数
pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;

/// 出站响应最大字节数
pub const MAX_NET_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// 出站重定向上限
pub const MAX_NET_REDIRECTS: u8 = 3;

/// 流式总字节上限
pub const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;

/// 宿主错误：稳定码 + 分类 + 消息 + 可重试
///
/// 分类与可重试由构造时按 [`PluginErrorCode`] 推导；调用方可以覆盖可重试
/// （例如同样的 `conflict`，在状态修正后可重试），但不该改分类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    /// 稳定错误码
    pub code: PluginErrorCode,
    /// 失败分类
    pub category: ErrorCategory,
    /// 可重试
    pub retryable: bool,
    /// 人类可读消息；不得包含密钥
    pub message: String,
}

impl HostError {
    /// 按错误码构造
    #[must_use]
    pub fn new(code: PluginErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            category: category_of(code),
            retryable: code.retryable(),
            message: message.into(),
        }
    }

    /// 用法错误
    #[must_use]
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::InvalidInput, message)
    }

    /// 资源不存在
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::NotFound, message)
    }

    /// 能力未授予
    #[must_use]
    pub fn denied(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::Denied, message)
    }

    /// 状态冲突
    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::Conflict, message)
    }

    /// 当前阶段不支持
    #[must_use]
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::Unsupported, message)
    }

    /// 超过限额
    #[must_use]
    pub fn limit_exceeded(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::LimitExceeded, message)
    }

    /// 内部错误
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::Internal, message)
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for HostError {}

/// 错误码 → 分类
///
/// 与 `protocol::plugin::category_of` 的取值一致。`port` 不能依赖 `protocol`
/// （方向是 `protocol -> port`），因此两边各写一次映射；测试断言两者一致。
#[must_use]
pub const fn category_of(code: PluginErrorCode) -> ErrorCategory {
    use PluginErrorCode as C;
    match code {
        C::InvalidInput | C::LimitExceeded | C::Unsupported => ErrorCategory::Usage,
        C::NotFound => ErrorCategory::NotFound,
        C::Denied => ErrorCategory::Unauthorized,
        C::Conflict => ErrorCategory::Conflict,
        C::Cancelled => ErrorCategory::Cancelled,
        C::Timeout | C::Unavailable | C::Crashed => ErrorCategory::Backend,
        _ => ErrorCategory::Failure,
    }
}

// ---- 只读查询 ----

/// 列表项：服务器名、实时状态与版本
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSummary {
    /// 服务器名
    pub name: ServerName,
    /// 实时状态
    pub status: ServerStatus,
    /// 已安装核心版本；未安装为 `None`
    pub version: Option<VersionId>,
}

/// 单台服务器的详细视图
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatusView {
    /// 实时状态
    pub status: ServerStatus,
    /// 已安装核心版本
    pub version: Option<VersionId>,
    /// 在线玩家数；不可得时为 `None`
    pub players: Option<u32>,
    /// 已运行时长；不可得时为 `None`
    pub uptime: Option<Duration>,
}

/// `server:read`：列服务器、状态、版本、属性
pub trait ServerRead: Send + Sync {
    /// 列出全部服务器（`server` 作用域实现只返回自身）
    ///
    /// # Errors
    ///
    /// 记录读取失败时返回 [`HostError`]
    fn list(&self) -> Result<Vec<ServerSummary>, HostError>;

    /// 单台状态
    ///
    /// # Errors
    ///
    /// 不存在返回 `not_found`
    fn status(&self, server: &ServerName) -> Result<ServerStatusView, HostError>;

    /// 已安装核心版本
    ///
    /// # Errors
    ///
    /// 不存在返回 `not_found`
    fn version(&self, server: &ServerName) -> Result<Option<VersionId>, HostError>;

    /// 服务端属性
    ///
    /// # Errors
    ///
    /// 不存在返回 `not_found`
    fn properties(&self, server: &ServerName) -> Result<BTreeMap<String, String>, HostError>;
}

/// 一行控制台输出
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleLine {
    /// 游标：单调递增，用于去重
    pub cursor: u64,
    /// 行文本（不含换行）
    pub text: String,
}

/// 控制台分页
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolePage {
    /// 本页行
    pub lines: Vec<ConsoleLine>,
    /// 下一页游标；无新行时与请求游标相同
    pub next: u64,
}

/// `console:read` / `console:write`：控制台游标读取与注入
pub trait ConsoleIo: Send + Sync {
    /// 读取 `cursor` 之后的至多 `max_lines` 行；`cursor=None` 从当前尾部开始
    ///
    /// # Errors
    ///
    /// `max_lines` 越界或服务器不存在时返回 [`HostError`]
    fn tail(
        &self,
        server: &ServerName,
        cursor: Option<u64>,
        max_lines: usize,
    ) -> Result<ConsolePage, HostError>;

    /// 注入一行命令
    ///
    /// # Errors
    ///
    /// 服务器未运行返回 `conflict`；行超限返回 `limit_exceeded`
    fn send_line(&self, server: &ServerName, line: &str) -> Result<(), HostError>;
}

// ---- 生命周期 ----

/// `server:lifecycle`：启停重启
pub trait ServerLifecycle: Send + Sync {
    /// 启动
    ///
    /// # Errors
    ///
    /// 已在运行返回 `conflict`；实现未开放返回 `unsupported`
    fn start(&self, _server: &ServerName) -> Result<(), HostError> {
        Err(HostError::unsupported("server:lifecycle is not available"))
    }

    /// 停机
    ///
    /// # Errors
    ///
    /// 同 [`Self::start`]
    fn stop(&self, _server: &ServerName, _grace: Duration) -> Result<StopOutcome, HostError> {
        Err(HostError::unsupported("server:lifecycle is not available"))
    }

    /// 重启
    ///
    /// # Errors
    ///
    /// 同 [`Self::start`]
    fn restart(&self, _server: &ServerName, _grace: Duration) -> Result<(), HostError> {
        Err(HostError::unsupported("server:lifecycle is not available"))
    }
}

// ---- 备份 ----

/// 快照摘要
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupEntry {
    /// 快照 id
    pub id: String,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 新增对象字节数
    pub added_bytes: u64,
}

/// `backup`：创建 / 列表 / 恢复 / 校验 / 清理
pub trait BackupOps: Send + Sync {
    /// 创建备份，返回快照 id
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`；服务器未运行且实现要求在线时返回 `conflict`
    fn create(&self, _server: &ServerName, _label: Option<&str>) -> Result<String, HostError> {
        Err(HostError::unsupported("backup is not available"))
    }

    /// 列出快照，按创建时间降序
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`
    fn list(&self, _server: &ServerName) -> Result<Vec<BackupEntry>, HostError> {
        Err(HostError::unsupported("backup is not available"))
    }

    /// 恢复快照
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`
    fn restore(&self, _server: &ServerName, _snapshot: &str) -> Result<(), HostError> {
        Err(HostError::unsupported("backup is not available"))
    }

    /// 校验快照完整性
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`
    fn verify(&self, _server: &ServerName, _snapshot: &str) -> Result<bool, HostError> {
        Err(HostError::unsupported("backup is not available"))
    }

    /// 只保留最近 `keep` 个快照，返回删除数
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`
    fn prune(&self, _server: &ServerName, _keep: usize) -> Result<usize, HostError> {
        Err(HostError::unsupported("backup is not available"))
    }

    /// 删除单个快照
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`
    fn delete(&self, _server: &ServerName, _snapshot: &str) -> Result<bool, HostError> {
        Err(HostError::unsupported("backup is not available"))
    }
}

// ---- 内容 ----

/// 已安装内容条目
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentEntry {
    /// 内容身份；未联网识别时为 `None`
    pub identity: Option<ContentIdentity>,
    /// 文件名
    pub file_name: String,
    /// 字节数
    pub size: u64,
}

/// 内容安装请求：项目 + 可选精确版本 + 类别
///
/// 框架不凭「候选」猜项目：候选来自某次检索，可能属于另一个项目；
/// 安装必须由调用方明确给出项目标识与类别。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentInstallRequest {
    /// 上游项目 ID 或 slug
    pub project: String,
    /// 精确版本 ID；`None` 按服务端条件挑最优候选
    pub version: Option<String>,
    /// 内容类别
    pub kind: ContentKind,
}

/// `content`：查询 / 安装 / 移除服务端内容
pub trait ContentOps: Send + Sync {
    /// 列出已安装内容
    ///
    /// `identify=true` 时会计算哈希并访问上游反查来源与版本；失败项按
    /// 「身份未知」返回，不让整次列表失败。
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`
    fn list(
        &self,
        _server: &ServerName,
        _kind: ContentKind,
        _identify: bool,
    ) -> Result<Vec<ContentEntry>, HostError> {
        Err(HostError::unsupported("content is not available"))
    }

    /// 安装内容
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`；服务器运行中返回 `conflict`
    fn install(
        &self,
        _server: &ServerName,
        _request: &ContentInstallRequest,
    ) -> Result<ContentEntry, HostError> {
        Err(HostError::unsupported("content is not available"))
    }

    /// 移除内容；选择器为文件名或 modid
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`；未命中返回 `not_found`
    fn remove(
        &self,
        _server: &ServerName,
        _selector: &str,
        _kind: ContentKind,
    ) -> Result<usize, HostError> {
        Err(HostError::unsupported("content is not available"))
    }
}

// ---- Java ----

/// `java`：托管 JDK 列表 / 安装 / 卸载
pub trait JavaOps: Send + Sync {
    /// 列出已登记运行时
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`
    fn list(&self) -> Result<Vec<JavaRuntime>, HostError> {
        Err(HostError::unsupported("java is not available"))
    }

    /// 安装指定大版本
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`
    fn install(&self, _major: JavaMajor) -> Result<JavaRuntime, HostError> {
        Err(HostError::unsupported("java is not available"))
    }

    /// 卸载托管运行时
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`
    fn remove(&self, _path: &Path) -> Result<bool, HostError> {
        Err(HostError::unsupported("java is not available"))
    }
}

// ---- 文件 ----

/// 目录项
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// 相对服务端目录的路径
    pub path: PathBuf,
    /// 是否目录
    pub is_dir: bool,
    /// 文件字节数（目录为 0）
    pub size: u64,
}

/// `files:read` / `files:write`：服务端目录内的受限读写
///
/// 路径参数一律相对；实现必须完成规范化与前缀比较，拒绝 `..`、绝对路径、
/// 盘符、UNC 与符号链接逃逸。`[禁止]` 把校验交给调用方。
pub trait FileOps: Send + Sync {
    /// 读文件
    ///
    /// # Errors
    ///
    /// 越界路径返回 `denied`；超过 [`MAX_FILE_BYTES`] 返回 `limit_exceeded`
    fn read(&self, _server: &ServerName, _rel: &Path) -> Result<Vec<u8>, HostError> {
        Err(HostError::unsupported("files are not available"))
    }

    /// 写文件；`overwrite=false` 时目标已存在返回 `conflict`
    ///
    /// # Errors
    ///
    /// 同 [`Self::read`]；实现未开放返回 `unsupported`
    fn write(
        &self,
        _server: &ServerName,
        _rel: &Path,
        _bytes: &[u8],
        _overwrite: bool,
    ) -> Result<(), HostError> {
        Err(HostError::unsupported("files are not available"))
    }

    /// 列目录
    ///
    /// # Errors
    ///
    /// 同 [`Self::read`]
    fn list(&self, _server: &ServerName, _rel_dir: &Path) -> Result<Vec<FileEntry>, HostError> {
        Err(HostError::unsupported("files are not available"))
    }

    /// 删除文件或空目录
    ///
    /// # Errors
    ///
    /// 同 [`Self::read`]
    fn remove(&self, _server: &ServerName, _rel: &Path) -> Result<bool, HostError> {
        Err(HostError::unsupported("files are not available"))
    }
}

// ---- 键值与密钥 ----

/// `storage:kv`：插件自己的键值空间
pub trait KvStore: Send + Sync {
    /// 读取；不存在返回 `Ok(None)`
    ///
    /// # Errors
    ///
    /// 键非法返回 `invalid_input`
    fn get(&self, plugin: &PluginId, key: &str) -> Result<Option<Vec<u8>>, HostError>;

    /// 写入（覆盖）
    ///
    /// # Errors
    ///
    /// 键 / 值越界或总量超限返回 `limit_exceeded`
    fn set(&self, plugin: &PluginId, key: &str, value: &[u8]) -> Result<(), HostError>;

    /// 删除；返回是否曾存在
    ///
    /// # Errors
    ///
    /// 键非法返回 `invalid_input`
    fn delete(&self, plugin: &PluginId, key: &str) -> Result<bool, HostError>;

    /// 按前缀列举，按键升序，最多 [`MAX_KV_LIST`] 个
    ///
    /// # Errors
    ///
    /// 前缀非法返回 `invalid_input`
    fn list(&self, plugin: &PluginId, prefix: &str) -> Result<Vec<String>, HostError>;
}

/// 密钥值
///
/// `Debug` 固定输出 `***`，防止日志与审计意外泄露明文；要取出内容必须显式
/// 调用 [`SecretValue::expose`]。
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(Vec<u8>);

impl SecretValue {
    /// 从字节构造
    #[must_use]
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// 从文本构造
    #[must_use]
    pub fn from_text(text: impl Into<String>) -> Self {
        Self(text.into().into_bytes())
    }

    /// 显式取出内容
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// 字节数
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 是否为空
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(***)")
    }
}

/// `secret:rw`：插件命名空间的凭据
///
/// 写接口接收明文；读接口同样返回 [`SecretValue`]，由调用方决定用途。
/// `[禁止]` 把明文写进审计、日志或 `plugin status` 输出。
pub trait SecretStore: Send + Sync {
    /// 读取
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`
    fn get(&self, _plugin: &PluginId, _key: &str) -> Result<Option<SecretValue>, HostError> {
        Err(HostError::unsupported("secret:rw is not available"))
    }

    /// 写入
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`
    fn set(&self, _plugin: &PluginId, _key: &str, _value: &SecretValue) -> Result<(), HostError> {
        Err(HostError::unsupported("secret:rw is not available"))
    }

    /// 删除
    ///
    /// # Errors
    ///
    /// 未开放返回 `unsupported`
    fn delete(&self, _plugin: &PluginId, _key: &str) -> Result<bool, HostError> {
        Err(HostError::unsupported("secret:rw is not available"))
    }
}

// ---- 出站网络 ----

/// 宿主代发的出站请求
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetRequest {
    /// HTTP 方法，大写
    pub method: String,
    /// 绝对 URL，仅 http / https
    pub url: String,
    /// 请求头
    pub headers: BTreeMap<String, String>,
    /// 请求体
    pub body: Option<Vec<u8>>,
    /// 超时
    pub timeout: Duration,
}

/// 出站响应
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetResponse {
    /// 状态码
    pub status: u16,
    /// 响应头
    pub headers: BTreeMap<String, String>,
    /// 响应体
    pub body: Vec<u8>,
}

/// `net:outbound`：宿主代发的出站请求
///
/// 实现必须按 `manifest.net.allow` 做主机名后缀精确匹配，默认拒绝回环、
/// 私网、链路本地与云元数据地址。白名单为空时拒绝所有请求。
pub trait NetProxy: Send + Sync {
    /// 发起一次请求
    ///
    /// # Errors
    ///
    /// 目标不在白名单（含 `manifest.net.allow` 为空）返回 `denied`；
    /// 非 http/https 返回 `unsupported`
    fn request(&self, _plugin: &PluginId, _request: NetRequest) -> Result<NetResponse, HostError> {
        Err(HostError::unsupported("net:outbound is not available"))
    }
}

/// 出站域名策略
///
/// `NetProxy` 只拿到 [`PluginId`]，不允许采信请求里自报的身份；白名单必须
/// 从安装记录读取。实现放在 `service`（有 `PluginStore`），由门面注入
/// `provider` 的 HTTP 代理。
pub trait NetPolicy: Send + Sync {
    /// 该插件 `manifest.net.allow` 声明的允许主机 / 后缀
    ///
    /// # Errors
    ///
    /// 记录不存在或读取失败返回对应 [`HostError`]
    fn allowed_hosts(&self, plugin: &PluginId) -> Result<Vec<String>, HostError>;
}

/// 一次契约调用
#[derive(Debug, Clone)]
pub struct ContractCall {
    /// 调用方；由宿主会话身份决定，不采信插件输入
    pub caller: PluginId,
    /// 契约标识（含 `@1` 版本后缀）
    pub contract: String,
    /// 单播目标；广播契约必须为 `None`
    pub target: Option<PluginId>,
    /// 调用输入
    pub input: Value,
    /// 宿主在根调用分配的调用链令牌；提供方嵌套调用时原样带回
    pub trace: Option<String>,
}

/// 宿主内契约路由
///
/// 实现在 `crates/plugin`：它拥有会话表，能拉起提供方并把调用送过去。
/// 循环、深度、扇出与目标选择的判定在 `domain::plugin::contract`。
pub trait ContractInvoker: Send + Sync {
    /// 路由一次契约调用
    ///
    /// # Errors
    ///
    /// 契约未声明、没有提供方、循环、超深、超扇出或全部提供方失败时返回
    /// 对应 [`HostError`]
    fn invoke(&self, call: ContractCall) -> Result<Value, HostError>;
}

// ---- 安装记录 ----

/// 安装记录：期望状态与静态声明
///
/// 实时状态不在这里：`enabled` 是用户期望，进程跑没跑由 [`PluginSupervisor`]
/// 回答。记录里的 [`Self::last_state`] 只用于宿主不在时的离线展示。
///
/// 四个布尔字段互相独立，组合都有意义：`enabled`（期望状态）、`signed`
/// （来源可信度）、`blocked`（本机拉黑）、`quarantined`（取证隔离）。刻意
/// 不合并成状态机：拉黑后仍保留原 `last_state`，解除后能回到用户上一份
/// 期望状态。
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct PluginRecord {
    /// 插件 id
    pub id: PluginId,
    /// 当前活动版本（SemVer）
    pub version: String,
    /// 插件 API 主版本
    pub api: u32,
    /// 作用域
    pub scope: PluginScope,
    /// `server` 作用域下的服务器
    pub server: Option<ServerName>,
    /// 用户期望：是否允许运行
    pub enabled: bool,
    /// 用户确认过的能力
    pub approved: BTreeSet<Capability>,
    /// 原始 manifest
    pub manifest: Manifest,
    /// 产物根目录
    pub artifact_dir: PathBuf,
    /// 安装时间（Unix 秒）
    pub installed_at: i64,
    /// 最后更新时间（Unix 秒）
    pub updated_at: i64,
    /// 离线展示用的最后状态
    pub last_state: PluginState,
    /// 最近一次失败原因
    pub last_error: Option<String>,
    /// 已安装但未激活的版本（回滚用）
    pub previous_version: Option<String>,
    /// 安装来源摘要
    pub source: Option<String>,
    /// 产物摘要
    pub sha256: Option<String>,
    /// 是否通过签名校验
    pub signed: bool,
    /// 本机拉黑：立即停用且不再自动启动
    pub blocked: bool,
    /// 隔离：保留产物但禁止运行，用于取证
    pub quarantined: bool,
}

impl PluginRecord {
    /// 是否存在待确认能力（安装 / 升级后）
    #[must_use]
    pub fn needs_approval(&self) -> bool {
        self.manifest
            .capabilities
            .iter()
            .filter_map(|raw| Capability::parse(raw).ok())
            .any(|cap| cap.risk().needs_approval() && !self.approved.contains(&cap))
    }

    /// manifest 声明的能力集合（闭集内）
    #[must_use]
    pub fn declared_capabilities(&self) -> BTreeSet<Capability> {
        self.manifest
            .capabilities
            .iter()
            .filter_map(|raw| Capability::parse(raw).ok())
            .collect()
    }
}

/// 撤销列表的缓存读写
///
/// 判定（命中哪个版本、是否为空表）在 `domain::plugin::revocation`；这里只
/// 负责把列表落盘与读回，实现层决定权限与原子性。
///
/// `load` 在缓存不存在时返回空列表；缓存存在但读不懂时必须报错——撤销列表
/// 是安全设施，读不懂不能当放行。
pub trait PluginRevocations: Send + Sync {
    /// 读取列表；缓存不存在时返回空列表
    ///
    /// # Errors
    ///
    /// 缓存非法、权限异常或读取失败时返回 [`HostError`]
    fn load(&self) -> Result<RevocationList, HostError>;

    /// 覆盖写入列表
    ///
    /// # Errors
    ///
    /// 写盘失败时返回 [`HostError`]
    fn store(&self, list: &RevocationList) -> Result<(), HostError>;
}

/// 安装记录存储
pub trait PluginStore: Send + Sync {
    /// 全部记录，按 id 升序
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`crate::RepositoryError`]
    fn list(&self) -> Result<Vec<PluginRecord>, crate::RepositoryError>;

    /// 按 id 读取
    ///
    /// # Errors
    ///
    /// 不存在返回 [`crate::RepositoryError::NotFound`]
    fn get(&self, id: &PluginId) -> Result<PluginRecord, crate::RepositoryError>;

    /// 写入或覆盖
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`crate::RepositoryError::Storage`]
    fn put(&self, record: &PluginRecord) -> Result<(), crate::RepositoryError>;

    /// 删除记录（不删产物与数据）
    ///
    /// # Errors
    ///
    /// 不存在返回 [`crate::RepositoryError::NotFound`]
    fn remove(&self, id: &PluginId) -> Result<(), crate::RepositoryError>;
}

/// 一份已落盘的配置
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoredConfig {
    /// 单调递增的修订号
    pub revision: u64,
    /// 明文配置值
    pub values: Value,
}

/// 配置存储
///
/// 与安装记录分开：记录是声明与期望状态，配置是用户数据，二者写入频率与
/// 权限要求不同（配置可能含密钥，落盘为 `0600`）。
pub trait PluginConfigStore: Send + Sync {
    /// 读取；不存在返回 `Ok(None)`
    ///
    /// # Errors
    ///
    /// 底层失败或文件损坏返回 [`crate::RepositoryError`]
    fn get(&self, id: &PluginId) -> Result<Option<StoredConfig>, crate::RepositoryError>;

    /// 写入
    ///
    /// # Errors
    ///
    /// 底层失败返回 [`crate::RepositoryError::Storage`]
    fn put(&self, id: &PluginId, config: &StoredConfig) -> Result<(), crate::RepositoryError>;
}

// ---- 安装来源与校验 ----

/// 安装来源
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactOrigin {
    /// 本地目录或压缩包
    Local(PathBuf),
    /// 远程压缩包
    Url(String),
    /// git 仓库，可固定 revision
    Git {
        /// 仓库地址
        url: String,
        /// 分支 / tag / commit
        rev: Option<String>,
    },
}

/// 校验通过的插件产物
#[derive(Debug, Clone)]
pub struct PluginArtifact {
    /// 解包后的根目录
    pub root: PathBuf,
    /// 校验过的 manifest
    pub manifest: Manifest,
    /// descriptor 原始字节（校验由调用方完成，保证错误顺序可控）
    pub descriptor: Vec<u8>,
    /// 产物摘要（小写 hex）
    pub sha256: String,
    /// 是否带有效签名
    pub signed: bool,
    /// 是否被信任（签名有效；未签名产物即使经 `--allow-unsigned` 放行也保持
    /// `false`，由安装记录标记为 `untrusted`）
    pub trusted: bool,
}

/// `ArtifactSource`：把来源解析成解包后的产物
pub trait ArtifactSource: Send + Sync {
    /// 拉取并解包到 `dest_root`
    ///
    /// 实现负责压缩包上限（128 MiB / 解包 512 MiB）与路径逃逸检查；manifest
    /// 与 descriptor 的语义校验由 `service` 调用 `domain` 完成。
    ///
    /// # Errors
    ///
    /// 下载、解包、摘要失败返回 [`HostError`]
    fn fetch(&self, origin: &ArtifactOrigin, dest_root: &Path)
    -> Result<PluginArtifact, HostError>;
}

/// `ArtifactVerifier`：摘要与元数据复查
///
/// 签名在来源拉取阶段完成（签名对象是压缩包，解包后已不可恢复）；本 trait
/// 只复核来源摘要与解包目录，并原样保留 `signed` / `trusted`。
pub trait ArtifactVerifier: Send + Sync {
    /// 复查已拉取产物
    ///
    /// # Errors
    ///
    /// 来源摘要不匹配或元数据不可读时返回 `invalid_input`
    fn verify(
        &self,
        artifact: &PluginArtifact,
        expected_sha256: Option<&Checksum>,
    ) -> Result<PluginArtifact, HostError>;
}

// ---- 审计 ----

/// 审计结论
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditResult {
    /// 允许并完成
    Allowed,
    /// 被能力门拒绝
    Denied,
    /// 已授权但执行失败
    Failed,
}

/// 一条审计记录
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Unix 秒
    pub at: i64,
    /// 插件
    pub plugin: String,
    /// 动作（op id / `capability:<name>` / `lifecycle:<verb>`）
    pub action: String,
    /// 涉及能力
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<Capability>,
    /// 结论
    pub result: AuditResult,
    /// 细节；不得包含密钥
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 审计出口
///
/// 与 [`crate::EventSink`] 同一条纪律：`record` 必须不阻塞业务、不把失败
/// 传播给调用方。实现自行记录写入失败。
pub trait AuditSink: Send + Sync {
    /// 记录一条审计
    fn record(&self, _event: &AuditEvent) {}
}

/// 丢弃全部审计的默认出口
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopAuditSink;

impl AuditSink for NoopAuditSink {}

// ---- 宿主运行时 ----

/// 会话摘要
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// 会话 id
    pub session_id: String,
    /// 实际授予的能力
    pub granted: BTreeSet<Capability>,
    /// 插件进程 PID
    pub pid: u32,
    /// 会话开始时间（Unix 秒）
    pub started_at: i64,
}

/// 宿主视角的实时状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRuntimeStatus {
    /// 插件
    pub id: PluginId,
    /// 实际状态
    pub state: PluginState,
    /// 活动会话
    pub session: Option<SessionInfo>,
    /// 最近一次错误
    pub last_error: Option<String>,
}

/// 流式分块的接收方
pub trait StreamSink: Send {
    /// 收到一块；`done=true` 表示流正常结束
    ///
    /// # Errors
    ///
    /// 接收方决定放弃（客户端断开、写失败）时返回错误，宿主据此发 `cancel`
    fn on_chunk(&mut self, chunk: &Value, done: bool) -> Result<(), HostError>;
}

/// 插件宿主运行时：启停、状态、调用、控制通道
///
/// 由 `crates/plugin` 实现，`service` 只编排。`[禁止]` 让入口直接持有进程
/// 句柄或自行拼帧。
pub trait PluginSupervisor: Send + Sync {
    /// 拉起插件并等待握手完成
    ///
    /// # Errors
    ///
    /// 未启用返回 `conflict`；握手失败返回 `crashed`
    fn start(&self, id: &PluginId) -> Result<SessionInfo, HostError>;

    /// 请求停机
    ///
    /// # Errors
    ///
    /// 未运行返回 `conflict`
    fn stop(&self, id: &PluginId, grace: Duration) -> Result<(), HostError>;

    /// 实时状态
    ///
    /// # Errors
    ///
    /// 记录不存在返回 `not_found`
    fn status(&self, id: &PluginId) -> Result<PluginRuntimeStatus, HostError>;

    /// 调用插件 op（惰性启动）
    ///
    /// # Errors
    ///
    /// 能力未授予返回 `denied`；超时返回 `timeout`
    fn call(&self, id: &PluginId, op: &str, input: Value) -> Result<Value, HostError>;

    /// 订阅插件 stream（惰性启动）
    ///
    /// # Errors
    ///
    /// 同 [`Self::call`]；超过总字节上限返回 `limit_exceeded`
    fn stream(
        &self,
        id: &PluginId,
        op: &str,
        input: Value,
        sink: &mut dyn StreamSink,
    ) -> Result<(), HostError>;

    /// 让宿主重新读取安装记录（install / remove 后）
    ///
    /// # Errors
    ///
    /// 记录读取失败返回 `internal`
    fn reload(&self) -> Result<(), HostError>;

    /// 撤销一项已授予能力；立即生效，不重启会话
    ///
    /// 宿主不在时默认成功：下次握手会按记录重新计算授予集合。
    ///
    /// # Errors
    ///
    /// 会话状态损坏时返回 `internal`
    fn revoke(&self, _id: &PluginId, _capability: Capability) -> Result<(), HostError> {
        Ok(())
    }

    /// 游标读取插件日志（控制通道）
    ///
    /// # Errors
    ///
    /// 宿主未运行时返回 `unavailable`
    fn logs(&self, _id: &PluginId, _offset: Option<u64>, _max: usize) -> Result<Value, HostError> {
        Err(HostError::unsupported("plugin logs are not available"))
    }

    /// 把一组已校验的配置下发给运行中的插件
    ///
    /// 会话不在时返回成功：配置已落盘，下次握手会经 `welcome.config` 下发。
    /// `values` 是明文；入口读取必须走 [`ConfigOps`] 的打码接口。
    ///
    /// # Errors
    ///
    /// 插件回 `ok:false` 或超时返回对应错误
    fn apply_config(
        &self,
        _id: &PluginId,
        _revision: u64,
        _values: &Value,
    ) -> Result<(), HostError> {
        Err(HostError::unsupported(
            "config changes are not supported by this host",
        ))
    }
}

/// 插件配置的读取与写入
///
/// 由 `service` 实现：校验 schema、合并、递增版本、落盘、下发。入口与宿主
/// 控制通道共用同一份实现，`[禁止]` 在 CLI 或端点里重写「怎么校验配置」。
pub trait ConfigOps: Send + Sync {
    /// 读取配置（密钥字段已打码）
    ///
    /// # Errors
    ///
    /// 记录不存在返回 `not_found`；descriptor 非法返回 `internal`
    fn get(&self, id: &PluginId) -> Result<Value, HostError>;

    /// 写入配置：`values` 与当前值做顶层合并后整体校验
    ///
    /// # Errors
    ///
    /// schema 校验失败返回 `invalid_input`；插件应用失败返回对应错误，
    /// 且上一版本保持不变
    fn set(&self, id: &PluginId, values: &Value) -> Result<Value, HostError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_mapping_covers_every_code() {
        for code in PluginErrorCode::ALL {
            let category = category_of(code);
            assert_eq!(category, HostError::new(code, "").category, "{code}");
        }
        assert_eq!(
            category_of(PluginErrorCode::Denied),
            ErrorCategory::Unauthorized
        );
        assert_eq!(
            category_of(PluginErrorCode::Timeout),
            ErrorCategory::Backend
        );
        assert_eq!(
            category_of(PluginErrorCode::InvalidInput),
            ErrorCategory::Usage
        );
    }

    #[test]
    fn retryability_follows_code() {
        assert!(HostError::new(PluginErrorCode::Timeout, "").retryable);
        assert!(!HostError::new(PluginErrorCode::Denied, "").retryable);
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = SecretValue::from_text("hunter2");
        assert_eq!(format!("{secret:?}"), "SecretValue(***)");
        assert_eq!(secret.expose(), b"hunter2");
        assert_eq!(secret.len(), 7);
    }

    #[test]
    fn advanced_capabilities_default_to_unsupported() {
        struct Empty;
        impl ServerLifecycle for Empty {}
        impl BackupOps for Empty {}
        impl ContentOps for Empty {}
        impl JavaOps for Empty {}
        impl FileOps for Empty {}
        impl SecretStore for Empty {}
        impl NetProxy for Empty {}

        let server = ServerName::parse("survival").expect("name");
        assert!(matches!(
            Empty.start(&server).map_err(|err| err.code),
            Err(PluginErrorCode::Unsupported)
        ));
        let plugin = PluginId::parse("demo").expect("id");
        assert!(matches!(
            Empty
                .request(
                    &plugin,
                    NetRequest {
                        method: "GET".to_owned(),
                        url: "https://example.com".to_owned(),
                        headers: BTreeMap::new(),
                        body: None,
                        timeout: Duration::from_secs(1),
                    }
                )
                .map_err(|err| err.code),
            Err(PluginErrorCode::Unsupported)
        ));
    }

    #[test]
    fn display_includes_code() {
        let err = HostError::denied("console:write 未授予");
        assert_eq!(err.to_string(), "denied: console:write 未授予");
        assert_eq!(err.category, ErrorCategory::Unauthorized);
    }
}
