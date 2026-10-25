# MCSUP 代码设计规范

来自 `AGENTS.md`。

设计模块接口、引入依赖、处理平台差异时查阅。

---

# 避免 God Object / God Trait

禁止一个 `ServerManager` 同时负责 Server、Java、Backup、File、Mod、Database、
Network、Auth；应拆成 ServerService、Runtime、Backup、File、Mod、Storage、Auth
等，Service 只编排，不吞掉所有职责。

不要创建 `trait EverythingManager`；Trait 应描述稳定的能力边界，如 Runtime、
Provider、Storage、Backend、FileStore。

# 进程级状态

`AGENTS.md` 禁止「为了方便创建全局状态」。禁止的是承载业务判定的可变全局，
不是所有进程级数据。允许的进程级状态必须同时满足：

1. 存在无法绕过的外部约束，使参数传递不可行；
2. 写入点唯一，且发生在启动早期，之后只读；
3. 只影响界面表现，不参与任何业务判定；
4. 约束与理由写在定义处的文档注释里。

当前唯一实例是 `apps/cli/src/locale.rs` 的界面语言：clap 在
`Cli::command()` 构建命令树时求值 `about` / `help`，那条路径拿不到 `Cli`，
语言只能从进程状态读；`help::parse()` 在 `try_parse` 前写一次，其余全是读。

新增这类状态前先证明第 1 条：能靠参数、`Builder` 或注入解决的问题，一律
不许用进程级状态绕开。

# 依赖管理

增加依赖前先问为什么需要，并检查：现有依赖能否完成、是否真的降低复杂度、是否
增加编译时间与安全风险、是否维护活跃、是否跨平台。禁止为几十行代码引入大型
依赖。

# 跨平台

目标平台是 Windows、macOS、Linux，尤其关注 Path、Process、Permission、Signal、
Shell、Java、Filesystem、Terminal。禁止默认假设 Unix / Linux / bash / systemd；
Windows 必须是一等公民。
