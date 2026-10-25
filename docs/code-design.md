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

## 平台专属代码

条件编译的代码只在目标平台编译，因此本机能过不代表 CI 能过：

- 一个类型 / 常量 / 函数只在某个平台的分支里被构造或调用时，其他平台的
  `dead_code` 会把它们当垃圾。用 `#[cfg_attr(<其他平台>, allow(dead_code, reason = "..."))]`
  显式标注，不要改成 `#[allow(dead_code)]`——那会在真正产生无用代码时也闭嘴。
- 被 `cfg` 分走的 `async fn` 若在某个平台没有 `await`，会触发
  `clippy::unused_async`。给每个平台写各自的函数体，不要在一个函数里用
  `cfg` 块拼分支。
- `cfg` 分支里的返回值形状必须一致：一侧 `async`、一侧同步时，调用点无法
  用同一份代码同时等待，最终只能两边都写成 `async`。
- 本地编译不到的分支（macOS / Linux 文件）改完后，在 CI 或交叉编译前不要
  假定通过；至少逐行核对被调函数的签名与同步性。
