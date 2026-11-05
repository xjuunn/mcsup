//! 失败分类：跨入口共用的稳定词汇表
//!
//! `domain` / `service` / 实现层的错误各有自己的类型，但入口需要的只有一件事：
//! **这是哪一类失败**。CLI 把它映射成退出码，REST 映射成 HTTP 状态码，
//! MCP 映射成工具错误——映射表各入口自己维护，分类本身只有一份。
//!
//! 分类放在 `port`（契约 crate）而不是 `domain`：它描述的是「失败属于哪一类」，
//! 是入口与实现之间共享的词汇，不参与任何业务判定；`service` 与实现层都依赖
//! `port`，因此各错误类型可以直接返回它。
//!
//! # 判定只写一次
//!
//! 每个错误类型在自己的 `category()` 里 `match` 一次：包装变体（`#[error(transparent)]`
//! 包内层错误的那种）**委托**给内层，而不是返回 `None` 让调用方沿错误链重新猜。
//! 好处有两条，都是编译期保证：
//!
//! - 新增变体时 `category()` 必须补分支，不会漏；
//! - 入口不再需要「拆透明包装」的代码——那类代码是错误分类漂移的源头，
//!   同一层包装在退出码和文案两侧各判一次，漏一处就是同一种失败两个结果。
//!
//! 与 `kind()` 的分工：`category()` 只回答等级（用于映射），`kind()` 回答
//! 「哪个变体 + 什么参数」（用于入口按语言渲染文案）。

/// 失败分类
///
/// 七个取值与 CLI 契约的退出码一一对应（见 `docs/embedding.md` 的 CLI 契约一节：
/// 0 成功 / 1 通用失败 / 2 用法错误 / 3 未找到 / 4 状态冲突 / 5 后端失败 /
/// 6 未授权 / 130 用户取消）。REST 入口按同一份分类映射 HTTP 状态码。
///
/// 刻意不加 `#[non_exhaustive]`：新增分类必须让每个入口的映射表重新审视，
/// 静默兜底会把新分类压成「通用失败」，届时脚本无法区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCategory {
    /// 通用失败：确实出错，但没有更具体的分类
    Failure,
    /// 用法错误：调用方给的参数或输入不合法，改参数即可成功
    Usage,
    /// 未找到：请求的服务器 / 工作区 / 版本 / 快照等资源不存在
    NotFound,
    /// 状态冲突：请求与当前状态矛盾，处理状态后可重试
    Conflict,
    /// 后端失败：磁盘、网络、上游、远端进程不可用或拒绝服务
    Backend,
    /// 未授权：token 失效 / 权限不足 / 证书指纹变化
    Unauthorized,
    /// 用户主动取消（协作取消走完清理路径后上报）
    Cancelled,
}

impl ErrorCategory {
    /// 是否为「调用方可直接修正输入」的失败
    ///
    /// 供入口决定要不要附带用法提示；与退出码无关。
    #[must_use]
    pub const fn is_callers_fault(self) -> bool {
        matches!(self, Self::Usage | Self::NotFound | Self::Conflict)
    }
}

/// 存储层失败的原因需要自带等级
///
/// [`crate::RepositoryError::Storage`] 与 [`crate::SnapshotStoreError::Storage`] 包的是
/// 「某个实现层的失败」，入口要区分的是**它的**等级：远端仓库把对端的
/// 传输失败原样装进 `Storage`，若这一层不给等级，入口只能靠 `downcast_ref`
/// 猜类型——那是本文件开头说的分类漂移。
///
/// 实现约定：有明确等级的类型自己实现（`TransportError` 的未授权就是 6），
/// 其余一律用默认值 [`ErrorCategory::Backend`]：存储失败默认是后端问题。
pub trait StorageFailure: std::error::Error + Send + Sync + 'static {
    /// 失败等级；默认视为后端失败
    fn category(&self) -> ErrorCategory {
        ErrorCategory::Backend
    }
}

impl StorageFailure for std::io::Error {}

/// 装箱后仍按内层回答等级
///
/// `Box<dyn StorageFailure>` 不是 `Sized`，`Error` 的既有实现覆盖不到它；
/// 只补 `Error` 不补 `Display` / `Debug`——那两个已由 `alloc` 的
/// `impl<T: ?Sized + Display> Display for Box<T>` 提供，重写会冲突。
impl std::error::Error for Box<dyn StorageFailure> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        <dyn StorageFailure as std::error::Error>::source(&**self)
    }
}
