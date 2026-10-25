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

# 日志

统一用 `tracing` / `tracing-subscriber`，支持结构化日志，至少区分 Application
/ Minecraft / Agent / Audit / HTTP 五类。生产业务日志禁止用 `println!`。

# 代码风格

清晰、直接、可维护、类型安全、低耦合、高内聚；优先 enum / struct / trait /
Result / Option / Iterator / async。避免巨型函数、巨型 struct、万能 Service /
Manager / Utils，禁止建 `utils.rs` / `helpers.rs` / `common.rs` / `misc.rs`
回收不明确代码；属于明确领域的代码放对应模块。
