# MCSUP Rust 开发规范

来自 `AGENTS.md`。

写实现代码、定义错误类型、打日志时查阅。

---

# 设计模式

先解决真实问题，再选设计模式。可用 Strategy、Factory、Builder、Adapter、
Repository、Command、State、Observer、Facade、DI、Type State、Policy、Visitor
等。只有同时具备真实变化点、真实替换需求、真实测试收益、真实边界时才抽象。

禁止为用模式而用模式（如 FactoryFactory）、只有一个实现就抽象 5 层 Trait、以
「未来可能需要」为由提前构建复杂插件系统。

# 编码规范

遵循现代 Rust：ownership / borrowing / lifetimes / Result / Option / enum /
trait / 类型安全 / zero-copy / streaming。避免无意义 `clone()`，尤其
`large_string` / `large_vec` / `large_struct`；确需 Clone 时要理解成本。

# 错误处理

Library / Domain 用 `thiserror`，Application 可用 `anyhow`。错误必须可定位、
带上下文、有类型、可传播，可恢复时提供恢复路径。生产路径禁止无理由使用
`unwrap()` / `expect()` / `panic!()`；测试代码可按需使用。

## 判定与文案分离

面向用户的文案不进 `domain` / `service`。这些层的错误只承载判定结果与参数，
`Display` 用英文短句，仅服务日志与调试：

```rust
pub enum LifecycleError {
    #[error("server {name} has no core jar")]   // 只给日志
    NoJar { name: String },
}

impl LifecycleError {
    // 判定 + 参数，不带文案
    pub fn kind(&self) -> Option<LifecycleErrorKind<'_>> { ... }
}
```

入口拿到 `kind()` 后按语言渲染文案。这样三个入口共享同一份判定，语言差异
只落在渲染层，加语言不需要动业务代码。

判据：这个错误会被入口直接展示给用户吗？

- 会 → 必须提供 `kind()`，`Display` 写成英文短句
- 不会（纯内部错误、需逐层包装）→ 只写 `Display`，`kind()` 返回 `None`

`#[non_exhaustive]` 的枚举在下游 `match` 必须带兜底分支；兜底不得静默丢弃，
至少沿用英文措辞。

## 分类与文案分开

`kind()` 回答「哪个变体 + 什么参数」，供入口按语言渲染文案。它**不**回答
「这算哪一类失败」——那是另一件事，由 `category()` 返回
`port::ErrorCategory`（`Failure` / `Usage` / `NotFound` / `Conflict` /
`Backend` / `Unauthorized` / `Cancelled`）。

两层分开的理由：分类要跨入口共享（CLI 退出码、REST 状态码、MCP 工具错误），
而文案只有展示层关心。混成一个 `kind()` 会让「新增一种失败」变成「在三个
入口各改一次映射」，漏一处就是同一种失败三种结论。

```rust
impl LifecycleError {
    pub fn category(&self) -> port::ErrorCategory {
        match self {
            Self::Registry(err) => err.category(),      // 委托，不重判
            Self::EulaRequired => port::ErrorCategory::Usage,
            Self::AlreadyRunning => port::ErrorCategory::Conflict,
            Self::Io(_) => port::ErrorCategory::Backend,
        }
    }
}
```

强制约定：

- **包装变体必须委托**。`#[error(transparent)]` 包内层错误时返回内层
  `category()`，禁止返回 `None` 让调用方沿链重新猜：内层类型不在
  `anyhow` 的 `chain()` 里，猜不到。
- `match` 必须穷尽本层所有变体，不加兜底。新增变体时编译器强制表态。
- 分层单向：`port` 与 `service` 只给分类，**不知道**退出码与 HTTP 状态码；
  数字只在入口的映射表里出现一次（CLI 见 `apps/cli/src/output/error.rs`）。
- `ErrorCategory` 本身不加 `#[non_exhaustive]`，理由同上表「封闭判定」。
- 实现层错误要进 `RepositoryError::Storage` 这类装箱字段时，实现
  `port::StorageFailure`（`category()` 默认 `Backend`，有明确等级的自己覆写）。

### 哪些枚举加 `#[non_exhaustive]`

看「加变体时应不应该让下游编译失败」：

| 类型 | 加 | 理由 |
| --- | --- | --- |
| 跨进程 / 跨版本的线上契约（`protocol::Request` / `Response` 等） | 加 | 对端可能比本端新，未知帧要能显式报错而不是崩 |
| 会被第三方按 feature 依赖的对外错误（`CredentialError`、`ScanError` 等） | 加 | 加变体不应破坏下游编译 |
| 开放词表（`ServerVariant`、`Modrinth` 的 `ProjectType` / `SearchIndex` 等） | 加 | 上游会新增取值 |
| 封闭判定（`RestoreTarget`、`SettingKey`、`Permission`、`Role`、`TaskEvent`、`StopOutcome`、`ConfigKey`、`Severity`、各 `Picks` / `Wire*` 线格式） | **不加** | 穷尽 `match` 本身就是安全属性：新增变体时必须让每个分支重新审视，兜底会静默走错分支 |

判断顺序：先问「漏掉一个变体，静默兜底会不会导致错误行为」。会 → 不加，
让编译器拦住；不会（只是少一条提示 / 多一个未知值）→ 加。

## 进程级状态

界面相关的一次性设置（如界面语言）可以放进程级状态，但必须同时满足：
存在无法绕过的外部约束、写入点唯一且在启动早期完成、只影响界面表现。
规则见 `docs/code-design.md` 的「进程级状态」一节。

# 日志

统一用 `tracing` / `tracing-subscriber`，支持结构化日志，至少区分 Application
/ Minecraft / Agent / Audit / HTTP 五类。生产业务日志禁止用 `println!`。

# 代码风格

清晰、直接、可维护、类型安全、低耦合、高内聚；优先 enum / struct / trait /
Result / Option / Iterator / async。避免巨型函数、巨型 struct、万能 Service /
Manager / Utils，禁止建 `utils.rs` / `helpers.rs` / `common.rs` / `misc.rs`
回收不明确代码；属于明确领域的代码放对应模块。
