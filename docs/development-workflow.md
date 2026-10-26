# MCSUP 开发流程与质量标准

来自 `AGENTS.md`。

规划一次开发任务、判断任务是否完成时查阅。

---

# 任务交付流程

任务的边界是「提交」而不是「合入」：已提交即一个任务结束。

worktree 的建立与复用：

- 当前没有独立的 worktree → 从 master 新建，之后所有改动都在其中完成；
  禁止在主工作区改动代码。
- 当前 worktree 已有提交 → 该任务已结束，删除它，下一个任务重新从 master 新建。
  不复用已提交的旧 worktree——它的基线已落后于 master，在上面继续开发会把
  后续改动建立在过期的点上。
- 当前 worktree 尚未提交 → 继续在其中做，不管这次的改动是不是新功能；一个
  worktree 可以承载多次改动，提交一次即关闭。

按以下顺序交付：

1. 按上述规则确定在哪个 worktree 工作。确定前先执行「开工前必须执行的判定」
   （见 `AGENTS.md`），并在回复里写明当前所在的 worktree 与分支。
2. 开工后先 `git fetch`，并在做任何改动前把 `origin/master` 并入 worktree。
3. 完成后运行 `scripts/check.sh`，测试必须完全通过。
4. 经用户明确同意后，才提交并合入 master；合入前如有冲突，先解决冲突。
5. 任务结束时调用 `dev-tools` 的 `send_notification` 通知用户。title 写当前
   worktree 的功能名，内容写为什么通知（已完成 / 需要用户确认 / 受阻待决策），
   title 与内容都简短。调用失败时在回复中说明。
6. 用户同意合入后：在主工作区执行合入，合入成功后立即删除该 worktree 与已合入
   分支，不要把它留作「下次接着用」。

```
git -C <主工作区> merge <分支>
git worktree remove <worktree 路径>
git branch -d <分支>
```

删除是合入的必要收尾：每个 worktree 自带一份 `target`，单个 1–4 GB，不删会持续
占用磁盘。顺序不可颠倒——合入成功之后才删，合入失败或有冲突时先解决冲突；
删除前确认 `git status` 无未提交改动，有则如实报告，不强行删除。

## 为什么主工作区不是开工的地方

主工作区（`git worktree list` 的第一条）是所有 worktree 共享 `.git` 的那一份，
它随时可能承载**用户的**未提交改动或进行中的合并。agent 在主工作区写文件，
后果不是「多了一处改动」，而是：

- 与用户正在进行的工作混在一起，无法分辨谁改了什么；
- 用户在自己不知情的情况下进入一个半成品状态；
- 任务结束时无法干净地回滚，只能人工逐文件甄别。

因此约束是「**任何改动都先有独立 worktree**」，与改动大小、类型、紧急程度无关。
这条规则没有任何例外分支，也不需要用户每次重申。

## 先合并上游，再跑门禁

```
开工时  → git fetch && git merge origin/master
提交前  → git fetch；若 origin/master 前进，再 merge 一次
```

原因：冲突的代价随改动量增长。等到任务结束才合并，撞上的是「整个任务的改动」
对「master 上所有并行改动」，冲突一次爆发且分散在多个文件里；开场就对齐，撞上的
只是「本次零改动」对「上游变化」，无从冲突。

`apps/cli/src/cli.rs` 与 `docs/cli-commands.md` 几乎每个任务都会碰，是冲突高发
点，务必在开工时对齐，而不是留到最后。

## CI 只能手动触发

`.github/workflows/test.yml` **不监听 `push` 和 `pull_request`**，只保留
`workflow_dispatch`。原因是 GitHub Actions 额度有限，自动触发会让每次提交都跑满
os × locale 矩阵，额度很快用光。

需要 CI 信号时手动跑一次，指向要验证的分支：

```
gh workflow run test.yml --ref <分支名>
gh run watch
```

或在 GitHub 的 Actions 页面选择 Test → Run workflow。分支必须先推到
`origin`，`workflow_dispatch` 只能引用远端已有的 ref：

```
git push -u origin HEAD:refs/heads/<功能分支名>
```

CI 是唯一能跑满 os × locale 矩阵的地方（见「代码质量门禁」）。本地 `check.sh`
通过不等于 CI 通过；需要确认时手动触发一次，合入前看一眼结果。

`.githooks/pre-push` 在推送前检查两件事：当前分支是否已落后 `origin/master`
（落后就提示先合并）、HEAD 与 `origin/master` 是否可干净合并。跳过用
`MCSUP_SKIP_PREPUSH=1`。

# worktree 回收

worktree 各自带一份 `target`，单个 1–4 GB，只有删掉 worktree 才回收得回来。

不建议把所有 worktree 指到同一个 `CARGO_TARGET_DIR`：多个 agent 同时跑
`check.sh` 时会争用同一份 target 目录里的锁与构建产物，并行退化成本地排队，
而且不同分支对同一 `target` 的交替写入会互相触发重编译。体积问题是「没回收」
而不是「没共享」，所以这里的做法是及时删 worktree：

```
sh scripts/worktree-status.sh        # 每个 worktree 的状态、落后数、target 体积
sh scripts/clean-worktrees.sh        # 预览哪些可删
sh scripts/clean-worktrees.sh --yes  # 真正删除（拒绝删有未跟踪文件的）
```

判定与上面「worktree 的建立与复用」一致：无未提交改动且已全部合入 = 应删。
交付流程的第 6 步已要求合入后立即删除，这里的脚本用于兜底清理历史遗留的
worktree，不能代替合入后的即时删除。

需要跨 worktree 复用依赖编译结果时，用 `sccache`（`RUSTC_WRAPPER=sccache`）
而不是共享 target 目录：它只共享编译结果，不共享构建状态与锁。

---

# 代码质量门禁

每次完成一个功能后执行：

```
scripts/check.sh
```

它依次运行 architecture / fmt / check / clippy / test / e2e / doc / feature
powerset / deny。fmt 只检查不修改（`cargo fmt --all -- --check`）；
deny 缺 cargo-deny 时静默跳过（本地绿灯 ≠ CI 绿灯，建议安装）；其余
任一失败即中止并报出步骤名。

`e2e` 单独成步（`scripts/check-e2e.sh`），跑 `apps/cli/tests/cli_e2e.rs`：真实
二进制 + 真实临时目录，断言命令串起来之后注册表与磁盘是否一致。只改文档时可
`MCSUP_SKIP_E2E=1` 跳过，其余情况不许跳。

`test` 与 `e2e` 的分工：前者是单元 / 契约测试，证明单个判定与输出形状正确；
后者证明多条命令串起来后状态仍正确。check.sh 通过只证明「测过的部分对」，
不证明业务逻辑对。

提交时 `.githooks/pre-commit` 先对本次提交的 Rust 文件运行 `cargo fmt` 并重新
暂存，再运行本脚本，保证进入提交的代码已格式化。CI 相反：fmt job 只运行
`cargo fmt --all -- --check`，未通过直接报错，不自动格式化。

hooks 只有执行过 `scripts/init.sh` 才生效（它配置 `core.hooksPath=.githooks`）。
`core.hooksPath` 是 repository 级配置，在主仓库配一次，所有 worktree 共享。
没配时 `git commit` 不会跑门禁——这时必须手工跑 `scripts/check.sh`，不能因为
「没报错」就认为门禁过了。

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
[ ] 新增或改动的用户可见路径有端到端覆盖（`apps/cli/tests/cli_e2e.rs`）
[ ] 没有无意义重构，没有破坏已有功能
[ ] 已与 origin/master 对齐（冲突在开工时消掉，不是合入时）
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
