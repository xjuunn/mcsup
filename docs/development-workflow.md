# MCSUP 开发流程与质量标准

来自 `AGENTS.md`。

规划一次开发任务、判断任务是否完成时查阅。

---

# 任务交付流程

每次用户提出新的功能或修复 bug，按以下顺序交付：

1. 在新建 worktree 中完成开发（除非用户明确要求，否则不在当前 worktree 操作）。
2. 完成后运行 `scripts/check.sh`，测试必须完全通过。
3. 经用户明确同意后，才提交并合入 master；合入前如有冲突，先解决冲突。

---

# 代码质量门禁

每次完成一个功能后执行：

```
scripts/check.sh
```

它依次运行 architecture / fmt / check / clippy / test / doc / feature
powerset / deny。fmt 只检查不修改（`cargo fmt --all -- --check`）；
deny 缺 cargo-deny 时静默跳过（本地绿灯 ≠ CI 绿灯，建议安装）；其余
任一失败即中止并报出步骤名。

提交时 `.githooks/pre-commit` 先对本次提交的 Rust 文件运行 `cargo fmt` 并重新
暂存，再运行本脚本，保证进入提交的代码已格式化。CI 相反：fmt job 只运行
`cargo fmt --all -- --check`，未通过直接报错，不自动格式化。

按需追加：性能相关 `cargo bench`，API 相关时 integration test。

# 开发流程

任何开发任务必须按以下顺序执行，禁止收到需求就直接生成大量代码。

1. 理解：需求、现有代码、架构、依赖、调用链、测试，先分析后编码。
2. 定边界：确定代码属于哪个模块（见 `AGENTS.md` 的「代码归属与依赖方向」
   一节）；无法判断就先重新分析。
3. 设计：明确数据结构、API、Trait、Error、State、并发与生命周期。
4. 设计测试：实现前定好 Happy Path / Boundary / Error / Failure /
   Concurrency / Cancellation / Timeout / Recovery。
5. 实现：一次只做一个逻辑单元，小、聚焦、可测试、可组合。
6. 测试：立即 `cargo test`，失败必须当场解决，不留到最后。
7. 静态检查：`cargo fmt` / `cargo check` / `cargo clippy`。
8. 性能：高性能路径做 Benchmark / Profile / 内存分析，至少确认无明显退化。
9. Review：架构、正确性、安全、性能、并发、错误处理、测试、可维护性。
10. 最终验证：再次运行 `scripts/check.sh`。

# 完成标准

一个任务只有在以下全部满足后才算完成：

```
[ ] 功能与架构边界正确，依赖方向正确
[ ] 没有重复业务逻辑，没有绕过 Service / Domain
[ ] API 设计合理，错误处理完整，日志与错误上下文合理
[ ] 安全边界正确，无敏感信息泄漏
[ ] 生命周期、并发、Cancellation、Timeout、资源释放正确
[ ] 大文件 Streaming，无无意义 Clone 与明显内存浪费
[ ] 无明显性能退化，必要时有 Benchmark
[ ] Unit / Integration / Failure Path / Edge Case 测试完整
[ ] 没有无意义重构，没有破坏已有功能
[ ] scripts/check.sh 通过
```

最终目标不是「能运行的 Minecraft Server 工具」，而是清晰架构、强类型、安全、
可测试、高性能、跨平台、可长期维护的专业级 Minecraft Server 基础设施平台。

始终按此优先级取舍：

```
正确性 → 架构 → 安全 → 测试 → 性能 → 可维护性 → 扩展性
```

简单的地方保持简单；复杂的地方明确建模；变化的地方抽象；稳定的地方保持简单；
高性能路径测量；高风险路径测试。跨模块行为经 Service 协调，跨进程行为经 RPC
协调，跨平台行为经明确抽象处理。不要为追求「高级架构」增加不必要的复杂度。

# 提交信息规范

- 提交信息简单、中文、明确。
- 只需标题时，首行写简单中文标题，如 `feat: 新增服务器备份功能`。
- 必须写大量信息的提交：首行标题 + 空行 + 正文；正文说明做了什么、为什么。
- 正文明确、简练；除非用户明确要求，否则不写正文。
