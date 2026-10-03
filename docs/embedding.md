# 外部嵌入与分发设计

第三方启动器集成方式，以及作为 Rust 库发布到 crates.io 的分发形态。

本文记录**目标架构**，当前处于未实施状态：仓库只有 `crates/domain`。
`docs/architecture.md` 的 Workspace 架构一节描述的是当前多 crate 布局，
迁移时需同步更新；`scripts/check-architecture.sh` 的成员与命名校验届时改为
模块 `use` 图检查。

## 目标

同时满足两个目标，且不能互相妥协：

1. MCSUP 自己有功能完整的 CLI。
2. 第三方启动器（Modrinth App、PrismLauncher、HMCL 等）能直接嵌入 MCSUP
   的代码，从而获得服务器管理能力，而不 fork MCSUP。

结论是**双轨**：CLI 和第三方集成共享同一套领域逻辑，两个出口不重复实现。

## 库优先

CLI 不是主入口，库才是主入口。CLI 只是库的一个消费者。

理由：CLI 形态对第三方最不友好（要 spawn 进程、要解析输出、要管生命周期），
而第三方要的是调用能力。如果先把逻辑写在 CLI 里，之后抽库等于重写。

CLI 模块内部因此分两层：命令语义和渲染分离。第三方 Rust 用户可以复用
命令语义，只换自己的渲染层，行为兼容性由结构保证。

## Layer 结构与依赖方向

单 package，feature 门控。模块分层如下：

```
crates/mcsup/               package mcsup（库，唯一发布单元）
├── domain/                 领域模型、值对象、状态机、领域错误
├── port/                   能力 trait：Backend / ProcessSpawner /
│                           JavaResolver / ServerRepository / ConsoleSink /
│                           EventSink / Clock
├── service/                用例编排：start / stop / status / backup
├── protocol/               消息和结果类型（唯一契约来源）
├── cli/                    CLI 命令语义和渲染（feature = "cli"）
└── 实现层（feature 门控）：
    ├── backend/            Local / Remote 执行位置选择（实现 port::Backend）
    ├── runtime/            Minecraft / JVM 进程运行（实现 port）
    ├── java/               Java 检测与版本管理（实现 port）
    ├── storage/            元数据持久化（实现 port::ServerRepository）
    ├── file/               文件系统和文件传输
    ├── backup/             CAS / Snapshot / Restore
    ├── provider/           外部资源和第三方 API
    ├── modkit/             Mod / Modpack
    ├── transport/          RPC 通信（RemoteBackend 使用）
    ├── api/                REST DTO / OpenAPI（apps/server 使用）
    └── auth/               认证与授权（apps/server 使用）
```

依赖方向：入口只依赖 `service`；`service` 只依赖 `port` 和 `domain`；
实现层依赖 `port`（实现其 trait）和 `domain`。所有依赖最终指向 `domain`，
`domain` 不指向任何模块，实现层之间不互相依赖：

```
  apps → service → port → domain
                            ↑
                            │ 实现
      backend / runtime / java / storage / file / backup /
      provider / modkit / transport / api / auth
```

硬规则：

- `domain` 不依赖本仓库任何其他模块，也不依赖任何 IO / 并发 / 存储类
  crate。允许纯过程的依赖（如 `thiserror` 派生错误类型），因为它不引入
  平台能力，也不让领域类型泄漏实现细节。
- `port` 只定义 trait，实现交给调用方或 `service` 的组合根。
- `service` 不得依赖任何具体实现模块（runtime / storage / java / …）。
  它只通过 `port` 的 trait 工作。
- CLI 不得绕过 `service` 直接调用实现层。

分层规则将纳入 `scripts/check-architecture.sh`。当前脚本只校验包命名、
app 互依赖和 `domain` 不依赖其他 workspace 包；上表的分层和依赖方向
校验要在迁移到单 package、模块边界固定后才能实施。

## Port / Adapter

启动器已经拥有实例管理和进程管理。让启动器能管理服务器，价值最低的部分是
进程启停——它自己做得更成熟。

启动器真正缺的是：世界备份、跨机器远程管理、版本生态管理、崩溃诊断、
自动更新调度。这些才是 MCSUP 的价值所在。

推论：**启动器会主动注入自己的进程管理**。MCSUP 不和宿主抢进程所有权，
而是通过 port 把控制权交出去。

```rust
pub struct McsupBuilder { /* 每个字段都是 Option */ }

impl McsupBuilder {
    pub fn process_spawner(self, s: Arc<dyn ProcessSpawner>) -> Self;
    pub fn java_resolver(self, r: Arc<dyn JavaResolver>) -> Self;
    pub fn server_repository(self, r: Option<Arc<dyn ServerRepository>>) -> Self;
    pub fn event_sink(self, s: Arc<dyn EventSink>) -> Self;
    pub fn build(self) -> Result<Mcsup, McsupError>;
}
```

每个字段都是 `Option`：不注入就用内置实现，注入是为了让宿主接管。

公开 API 还必须提供同步入口。大量启动器使用同步模型（Qt 主线程、Swing），
只给 async API 会把采用门槛抬高一个数量级：

```rust
impl Mcsup {
    pub fn start_blocking(&self, spec: ServerSpec) -> Result<ServerHandle, McsupError>;
    pub async fn start(&self, spec: ServerSpec) -> Result<ServerHandle, McsupError>;
}
```

## 公开 API 约束

以下约束针对 crates.io 发布后的稳定性承诺：

- 公开 API 不出现 tokio 类型（`mpsc::Receiver` 等）。控制台流用
  `futures_core::Stream`，避免与宿主 tokio 版本冲突。
- 无全局状态、无 lazy_static、无 once_cell 单例。实例由调用方持有。
- 库不读配置文件、不读环境变量。配置由 app 层传入。
- 库不 spawn 不可取消的后台任务。长驻进程有 handle，调用方能停能查。
- 公开枚举加 `#[non_exhaustive]`。
- 需要宿主实现的 trait 不加 sealed。
- 不使用 glob 重导出（`pub use crate::runtime::*` 会把内部结构变成公共 API）。

## 对外契约

机器接口（CLI JSON、API 的 REST 与 WebSocket、MCP）共享同一套类型，来自
`protocol` 模块。TUI 直接经 `service` 工作，不另立契约；Web 作为独立项目消费
REST / WebSocket。

JSON 输出格式、REST body、WebSocket 消息都是这批类型的 serde 形式。
改一处，所有入口同步。破坏性变更走新增字段 + `schema` 版本号，
不改旧字段。

`protocol` 是唯一真相来源，禁止各入口自行定义 DTO。

## CLI 契约

CLI 既是人类工具，也是第三方（非 Rust）的编程接口。两者必须并存。

- `--output json` 显式开关，不用 TTY 自动检测。
- stdout 只出数据，stderr 只出日志。
- 退出码稳定映射：0 成功 / 1 通用失败 / 2 用法错误 / 3 未找到 /
  4 状态冲突 / 5 后端失败。
- `--follow` 输出 JSONL（一行一个事件对象），用于实时日志和状态推送。
- `server send <id> <command>` 支持把宿主输入接到服务器 stdin。
- 无交互：不依赖 TTY 输入，不等待确认，需要确认的走显式 `--yes`。
- 幂等：同样命令重复执行结果可预期。

这些必须在实现前定死。事后从"人类可用"改成"机器可用"要重写输出层。

## 分发形态

### 发布单元

只发布一个包：`mcsup`（lib）。

```
crates.io
└── mcsup              lib，第三方嵌入，feature 选择能力

仓库，不发布
├── crates/mcsup/      被上面那个包包含
└── apps/cli/ ...      bin，装出 mcsup / mcsup-agent / mcsup-server
```

`apps/` 不发布到 crates.io。它们通过 `--path` 或 `--git` 安装：

```
cargo install --path apps/cli        → mcsup.exe
cargo install --git <repo> cli       → mcsup.exe
```

Cargo 禁止同一 workspace 内出现同名 package，所以发布库叫 `mcsup` 时，
`apps/` 下的包不能用这个名字。但由于 apps 的 package 名本就是单词名
（`cli` / `agent` / `server`），不构成冲突。

不把库拆成 `mcsup-domain` / `mcsup-service` / … 的原因：

- crates.io 上 `domain` / `service` / `port` / `sdk` 已被占用
  （实测 2026-10），单词名无法发布。
- 多包意味着发布顺序管理，且存在版本错配风险（宿主同时依赖旧版
  `mcsup-domain` 与新版 `mcsup` 时类型不兼容）。
- 多包会让内部 crate 泄漏成公共 API，防守成本高。

### Feature 矩阵

`default = []`，强制显式选择。不设默认是因为默认 feature 会让
"只选部分能力"名存实亡：写了 `features = ["backup"]` 仍会带上默认的
`core` + `service`，除非调用方记得写 `default-features = false`，
而大部分调用方不会记得。

命名组合给不确定选哪些的调用方：

```
minimal = ["domain", "service"]
full    = ["domain", "service", "protocol", "backend", "runtime", "backup",
           "file", "modkit", "java", "provider", "storage", "transport"]
```

能力组（模块用 cfg 门控，外部依赖用 `dep:` 门控）：

```
domain    = ["dep:thiserror"]
port      = []
service   = ["domain", "port"]
protocol  = []
backend   = ["domain", "port"]
runtime   = ["domain", "port", "dep:tokio"]
backup    = ["domain", "dep:sha2"]
file      = ["dep:walkdir", "dep:fs2"]
storage   = ["dep:rusqlite"]
java      = ["dep:which"]
provider  = ["dep:reqwest"]
modkit    = ["dep:zip"]
transport = ["protocol", "dep:tonic"]
api       = ["protocol"]
auth      = []
cli       = ["service", "dep:clap"]
```

`full` 面向库嵌入者，含远程管理与备份，但不含 `api` / `auth`——
它们是 `apps/server` 专用的 HTTP 契约与鉴权，库嵌入者不需要。

`thiserror` 是 domain 错误类型的派生宏，只在构建期生成代码，不引入
运行时依赖，因此 `domain` feature 必须带上它。

feature 语义必须机器校验。如果 `backup` 硬依赖 `storage`，那
`features = ["backup"]` 会把 `storage` 拉进来，这个 feature 就是假的。

该检查尚未实现。迁移到单 package 后，`scripts/check-architecture.sh` 应
对每个 feature 单独构建并断言实际拉入的依赖集合等于允许集合。

### CLI 与库的解耦

Cargo 不会为依赖构建 bin target。第三方写 `mcsup = { version = "0.1" }`
时，`[[bin]]` 不会被编译，无论里面写了什么。

所以"不需要安装 CLI 功能"是自动的。真正要 gate 的只有 CLI 的库代码
（参数解析、渲染），目的是不让 `clap` 进入依赖图。

## 第三方集成形态

### Rust 第三方

```toml
[dependencies]
mcsup = { version = "0.1", default-features = false, features = ["full"] }
```

### 非 Rust 第三方（Prism / HMCL / ATLauncher）

两个阶段：

- 早期：`mcsup --output json` + `--follow`（JSONL 实时事件）。
  不需要常驻进程，无生命周期负担。
- 成熟：`mcsup-server` 在 127.0.0.1 暴露同一套 `protocol`，
  覆盖实时推送和远程机器。

两种粒度不同：Rust 第三方是 feature 粒度，非 Rust 是二进制粒度
（跑的是 `mcsup.exe`，无法只装 backup）。粒度差异必须提前说明，
不能让第三方误以为 feature 对所有消费者都生效。

## 明确不做

- **FFI / JNI**：panic 穿越 C ABI 是 UB；结构体成员变更即 ABI 破坏；
  每个消费者要自己配链接器和交叉编译。成本高于省下的一个子进程。
  等有明确的大启动器要求 in-process 时再评估。
- **glob 重导出**：见公开 API 约束一节。
- **默认 feature**：见分发形态一节。
- **入口内业务逻辑**：CLI / API / MCP 的判定逻辑一律收敛到
  `domain::ServerStatus::transition`，入口只做错误码映射。规则见
  `AGENTS.md` 的判定逻辑收敛一节。

## 落地顺序

依赖拓扑序，每步结束都是可用状态：

```
1  domain      状态机 + 值对象
2  port        trait 签名冻结
3  service     用例编排，测试用内存 fake
4  protocol    命令 + 结果类型冻结
5  cli         命令语义 + 渲染 + JSON
6  apps/cli    CLI 入口，bin 名为 mcsup
7  runtime     第一个真实 ProcessSpawner
8  storage     第一个真实 Repository
9  sidecar     protocol 的 HTTP 绑定
```

第 1-6 步做完，CLI 已完整，同时非 Rust 第三方已有编程接口。
