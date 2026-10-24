# MCSUP 项目开发总规范

MCSUP 是 Minecraft Server 基础设施管理平台，使用 Rust Cargo Workspace 构建。

项目完整范围和各架构层职责见 `docs/architecture.md`。

本文件只保留每次开发都必须知道的内容，按需查阅 `docs/`，不要一次性全部读入。

# 最高优先级原则

开发过程中必须始终遵守以下原则。

## 1. 正确性优先

首先保证：

1. 行为正确
2. 数据安全
3. 状态一致
4. 错误可恢复
5. 测试覆盖
6. 再考虑性能优化
7. 最后考虑代码简洁性

禁止为了代码看起来简单而牺牲正确性。

## 2. 架构边界优先

任何新代码必须首先判断属于哪个架构层。不要因为当前实现方便，
就把代码放进错误的模块。

依赖只能单向向下：

```
入口 (apps/*) → service → port → domain
                                      ↑
   backend / runtime / storage / file / backup / java / provider / modkit
```

`domain` 不依赖任何模块。`service` 不依赖入口，实现层不互相依赖。完整依赖
模型见 `docs/architecture.md` 的核心依赖方向一节。

## 3. 判定逻辑收敛到领域层

项目有三个入口：CLI（含 TUI）、API（REST）、MCP。它们是消费者，不是实现者。
Web 暂不在本仓库，后续作为独立项目通过 API 接入。

业务逻辑只能写在 `domain` / `service`。所有判定必须收敛到领域层，
例如 `ServerStatus::transition`；入口只负责把领域错误映射成各自的
退出码 / HTTP 状态码 / UI 提示。

```
禁止 → 把判定逻辑写在 CLI，然后在 API 里重写一份
禁止 → Local 和 Remote 各写一套
```

判定逻辑一旦分散就会分叉，这是本项目最常见的架构腐化。

## 4. Thin Entry Point

以下模块只能负责入口和协议适配，禁止承载核心业务逻辑：

```
apps/cli    apps/server    apps/agent    apps/mcp
```

## 5. 禁止擅自操作 Git

禁止 → git add / commit / push / stash / reset / restore / checkout / clean
允许 → git status / diff / log
允许 → 报告状态，由用户决定下一步

任务完成不等于可以提交。发现暂存内容有问题时指出问题，不要修正。

开发任务（新功能 / bugfix）默认在新建 worktree 中完成，除非用户明确要求。
完成后运行 `scripts/check.sh`，测试必须完全通过；经用户明确同意后，才提交并
合入 master。合入前如有冲突，先解决冲突。提交信息规范见
`docs/development-workflow.md` 的「提交信息规范」。

## 6. AI 写作规范

简洁、直接、必要。不要前言后语，不加表情符号。保留原有条文和术语，不擅自改写。

回复直击重点，不铺陈。结论先行，细节按需展开。

## 7. AI 协助开发

本项目由人写代码、AI 协助。收到用户提问时先读 `docs/ai-collaboration.md`，
按其中的类型分派执行。规则只在该文件维护，此处不复制。

## 8. CLI 耗时操作必须回显

CLI 中任何可能超过数百毫秒的步骤（网络请求、扫描、下载、解压、数据库迁移）
必须有实时进度回显，禁止静默等待。用户看不到输出会认为程序卡死。

回显要求：

- 风格统一：spinner 或进度条，单行刷新，不刷屏
- 多阶段任务每阶段切换文案，让用户知道当前在做什么
- JSON 输出模式（`--output json`）下隐藏进度，stdout 保持纯数据
- 实现放在 `apps/cli/src/progress.rs`，复用 `TaskSpinner` / `BarSink`

# 命名与目录结构

`crates` 中的库名称一律使用单个单词，不使用 `mcsup-` 前缀：

```
domain port backend service runtime provider storage backup file java
modkit protocol transport api auth
```

这些是 workspace 内部的包名，不发布，因此不要求在 crates.io 上全局唯一。

`domain` 是领域层，包含领域模型和领域规则。禁止命名为 `core`。

`core` 是 Rust 内置 crate。任何 crate 依赖名为 `core` 的包之后，内置 `core`
会在该 crate 内被永久遮蔽。这个错误只在下游 crate 出现，因此问题会被推迟到
第一个下游 crate 编写时才暴露，且排查方向容易错误。

机理、完整禁止列表和原因见 `docs/architecture.md` 的 Workspace 架构一节。

`mod` 无法作为 crate 名称，Mod / Modpack 管理 crate 命名为 `modkit`。

`apps/` 下的包同样使用单词名，不带 `mcsup-` 前缀。它们不发布到
crates.io，不受全局命名空间约束。

可执行文件名是另一回事，必须带 `mcsup` 或 `mcsup-` 前缀，通过 `[[bin]]` 指定：

```toml
[package]
name = "cli"                 # package 名：单词名

[[bin]]
name = "mcsup"               # bin 名：用户命令
```

原因是 bin 名会进入用户 PATH，而 PATH 是全局扁平命名空间：

```
$ mcsup server start          # 用户敲 mcsup 就能 tab 补出整个家族
$ mcsup-agent --listen ...    # 看到 server / agent 无法猜到和 Minecraft 有关
```

`cargo install` 装的是 bin 名而非 package 名，所以前缀不影响安装体验。

三个标识符各有不同的唯一性范围：

| 标识符 | 唯一性范围 | 约束 |
| --- | --- | --- |
| 目录名 | 仓库内 | 单词名 |
| package 名 | Cargo workspace 内 | 单词名 |
| bin 名 | 用户 PATH | `mcsup` 或 `mcsup-*` |

完整目录结构见 `docs/architecture.md`。

当前与目标都是多 crate + `mcsup` 门面：`mcsup` 按 feature 级联重导出内层
crate，是第三方依赖的唯一入口与发布主体。内层 crate 现为 `publish = false`
的 path 依赖，单词名不发布、不占 crates.io；一旦要发布 `mcsup`，它 feature
拉入的每个内层 crate 必须先改名 `mcsup-*`（单词名已被占用，`mcsup-*` 已验证
可用），按 `domain → port → protocol → service / file / java / provider / storage / runtime → mcsup` 顺序发布，
版本以 `workspace.version` 锁步。门面 feature、级联与发布条件见
`docs/embedding.md` 的分发形态一节。

# 代码归属与依赖方向

新代码的归属：

| 职责 | 归属 |
| --- | --- |
| 领域模型 / 领域规则 | `domain` |
| 能力 trait / Port | `port` |
| 业务编排 | `service` |
| Local / Remote 执行位置抽象 | `backend` |
| 运行 Minecraft / Process / JVM | `runtime` |
| Java | `java` |
| 外部 API / 下载 / 第三方资源 | `provider` |
| 数据库 / 元数据 | `storage` |
| 文件系统 / 文件传输 | `file` |
| Backup / CAS / Snapshot / Restore | `backup` |
| Mod / Modpack | `modkit` |
| RPC Message | `protocol` |
| RPC / gRPC | `transport` |
| REST DTO / OpenAPI | `api` |
| Authentication / Authorization | `auth` |
| CLI | `apps/cli` |
| HTTP / REST / WebSocket Server | `apps/server` |
| Remote Agent | `apps/agent` |
| TUI（CLI 入口的一部分） | `apps/cli` |
| MCP | `apps/mcp` |
| Browser UI（独立项目，不在本仓库） | 通过 `apps/server` 的 API 接入 |
| 开发工具 | `apps/dev` |
| Benchmark | `apps/bench` |

依赖方向：

```
apps → service → port → domain
                     ↑
            实现层实现 port 并依赖 domain
```

`domain` 不得依赖本仓库任何其他模块。`apps` 是并行入口，不允许互相
Cargo 依赖。`service` 不直接依赖任何具体实现模块，实现层之间也不互相依赖。

Local / Remote 必须共享同一套业务逻辑，差异只体现在 Backend 实现里。
Backend 只回答「在哪里执行」，不回答「能不能执行」——后者属于领域层判定。

上表名称即 crate / 模块名，归属不随形态变化。多 crate 以 `mcsup` 为门面：
门面按 feature 级联重导出内层（`file` / `provider` → `port` → `domain`），
第三方经门面按 feature 选能力；`apps/cli` 已改走门面，装配统一经门面
`bootstrap` 组合根（`McsupBuilder`），CLI 不再手工拼装服务。
方案、代价与发布条件见 `docs/embedding.md`。

以下行为禁止：

```
为了方便 → 把业务逻辑放 CLI
为了方便 → HTTP Handler 直接访问数据库
为了方便 → 前端直接访问内部 API
为了方便 → Local / Remote 写两套业务逻辑
为了方便 → Agent 复制 Service
为了方便 → 创建全局状态
为了方便 → Arc<Mutex<Everything>>
为了方便 → unwrap()
为了方便 → clone 大量数据
```

# 详细规范索引

| 文件 | 内容 | 何时读 |
| --- | --- | --- |
| `docs/architecture.md` | 项目范围；目录结构；各架构层职责、边界和领域模型；Domain Event、状态机；依赖模型 | 判断某段代码属于哪个层、某个类型该放在哪个模块时 |
| `docs/embedding.md` | 第三方启动器集成方式；Port/Adapter 能力注入；公开 API 约束；CLI 机器契约；crates.io 分发形态和 Feature 矩阵 | 设计库对外接口、实现能力注入、处理发布与打包时 |
| `docs/development-workflow.md` | 开发流程、代码质量门禁、完成标准、任务交付流程、提交信息规范 | 规划一次开发任务、判断任务是否完成、规范提交信息时 |
| `docs/principles.md` | 最终开发原则、Agent 最终行为准则 | 判断某个设计取舍是否合理时 |
| `docs/rust-standards.md` | 设计模式、Rust 开发规范、错误处理、日志、代码风格 | 写实现代码、定义错误类型、打日志时 |
| `docs/testing.md` | 测试分层、测试矩阵、测试优先级、性能测试 | 设计测试、评估测试覆盖时 |
| `docs/concurrency-performance.md` | 并发设计、性能原则、资源生命周期、Cancellation、Timeout、网络设计 | 涉及并发、性能、IO、任务取消时 |
| `docs/reliability-security.md` | 安全、路径安全、数据库设计、配置设计、API Compatibility、向后兼容、可观测性 | 处理用户输入、密钥、配置、协议兼容时 |
| `docs/code-design.md` | God Object、God Trait、依赖管理、跨平台 | 设计模块接口、引入依赖、处理平台差异时 |
| `docs/engineering-discipline.md` | 创建模块的时机、无意义重构、发现架构问题时的处理、架构修改规则 | 新建模块、改动已有代码、遇到设计缺陷时 |
| `docs/ai-collaboration.md` | AI 协助开发细则：方案类 / 代码类 / 审查类的对应流程 | 收到用户提问时 |
| `docs/cli-commands.md` | CLI 子命令设计：`workspace`；`server` 及其 `backup` / `mod`；`modpack` / `java` / `remote` 用法表 | 设计或查询子命令时 |
| `docs/workspace.md` | 平台与工作目录布局、命名、身份与移动、init 判空与恢复、统一注册表语义 | 设计存储布局、实现 workspace / init、改动 create 落点、处理路径身份时 |

新增代码时的判断顺序：

```
1. 先读本文件的一 ~ 三
2. 用「代码归属与依赖方向」的归属表确定它属于哪个模块
3. 再按上表读取对应 docs/ 文件的细则
4. 完成后执行 scripts/check.sh
```

# 代码质量门禁

每次完成一个功能后执行：

```
scripts/check.sh
```

它依次运行 architecture / fmt / check / clippy / test / doc / feature powerset / deny。
fmt 只检查不修改；deny 缺 cargo-deny 时静默跳过；其余任一失败即中止并报出步骤名。

本地提交由 `.githooks/pre-commit` 先对本次提交的 Rust 文件运行 `cargo fmt`
并重新暂存，再执行 `scripts/check.sh`。CI 相反：格式化未通过直接报错，不自动
格式化。提交前检查表见 `docs/development-workflow.md`。
