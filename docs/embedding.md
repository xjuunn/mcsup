# 外部嵌入与分发设计

第三方启动器集成方式，以及作为 Rust 库发布到 crates.io 的分发形态。

本文记录已实施的**门面架构**：多 crate 保留，`mcsup` 作为门面在
`crates/mcsup` 内按 feature 重导出内层 crate。`docs/architecture.md` 的
Workspace 架构一节与本方案一致；`scripts/check-architecture.sh` 校验成员、
命名、apps 互依赖与门面 feature ↔ `pub use` 对应。

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

多 crate + 门面：能力层保持独立 crate（`crates/<name>`），`mcsup` 按
feature 重导出。分层如下：

```
crates/mcsup/               门面 package mcsup（唯一入口与发布主体）
│                           src/lib.rs：feature → pub use 重导出
├── 已落地（内层 crate + 门面同名 feature）：
│   ├── domain/             领域模型、值对象、状态机、领域错误（无依赖）
│   ├── port/               能力 trait（29 个，随能力分组在 `port::backup` /
│   │                       `port::content` / `port::modpack` / `port::task` /
│   │                       `port::origin` / `port::backend` 与 `lib.rs`），
│   │                       依赖 domain
│   ├── protocol/           消息和结果类型（唯一契约来源，依赖 domain / port / auth）
│   ├── service/            用例编排：start / stop / status / backup / content / modpack
│   ├── file/               文件系统和文件传输
│   ├── java/               Java 检测与版本管理（实现 port）
│   ├── provider/           外部资源和第三方 API
│   ├── storage/            元数据持久化（实现 port::ServerRepository）
│   ├── runtime/            Minecraft / JVM 进程运行（实现 port，依赖 protocol）
│   ├── backup/             CAS / Snapshot / Restore（实现 port::ContentStore 等）
│   ├── transport/          RPC 通信：TLS + 长度前缀帧（client / server / handler）
│   └── backend/            Local 执行位置与备份数据（实现 port::Backend 与
│                           port::BackupStore；唯一允许组合实现层的 crate）
└── 待建（同样建 crate 并在门面加 feature）：
    ├── modkit/             Mod / Modpack
    └── api/                REST DTO / OpenAPI（apps/server 使用）

apps/cli 不是门面内模块：CLI 命令语义与渲染在 `apps/cli`，通过门面
`bootstrap` 组合根取服务，不额外建 `cli` crate。
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
- `port` 只定义 trait 与契约类型，实现交给调用方或 `service` 的组合根。
- `service` 不得依赖任何具体实现模块（runtime / storage / java / …）。
  它只通过 `port` 的 trait 工作。
- CLI 不得绕过 `service` 直接调用实现层。
- 错误分类必须类型化：跨层的「种类」用枚举（如 `port::RemoteFailureKind`），
  不在协议线上按字符串比较。

分层在多 crate 形态下由 Cargo manifest 直接表达（实现层只声明 domain / port
等依赖）。`scripts/check-architecture.sh` 校验包命名、app 互依赖、
`domain` 不依赖其他 workspace 包、门面 feature ↔ `pub use` 对应，以及
crate 级与模块级依赖方向（实现层之间不互相依赖）。

## Port / Adapter

启动器已经拥有实例管理和进程管理。让启动器能管理服务器，价值最低的部分是
进程启停——它自己做得更成熟。

启动器真正缺的是：世界备份、跨机器远程管理、版本生态管理、崩溃诊断、
自动更新调度。这些才是 MCSUP 的价值所在。

推论：**启动器会主动注入自己的进程管理**。MCSUP 不和宿主抢进程所有权，
而是通过 port 把控制权交出去。

已实施（`crates/mcsup/src/bootstrap.rs`，feature = "bootstrap"）：门面
提供 `McsupBuilder` + `Mcsup` 组合根，注入面与当初设计一致——
`supervisor` / `downloader` / `scanner` / `java_registry` / `extractor` /
`store`（具体连接）/ `server_repository` + `workspace_repository`（按
trait 换仓库实现）/ `jar_sources`，每个字段都是 `Option`，不注入就用
内置实现。CLI 与库侧共用同一个组合根（`apps/cli` 已改走
`mcsup::bootstrap`），不再各拼一套。待补：同步入口（`start_blocking`）
与事件注入（EventSink）。

默认 supervisor 约束：`LocalSupervisor` 经 mcsup CLI 的 `__supervise`
子命令拉起子进程，`server start` 必须在 `StartOptions::supervisor_exe`
传入 mcsup CLI 路径（CLI 传 `current_exe()`，成立）；第三方宿主没有
这个子命令，应注入自己的 `SupervisorPort`。exe 缺失在 spawn 前报
指名错误，不留 5 秒超时的模糊失败。

```rust
pub struct McsupBuilder { /* 每个字段都是 Option */ }

impl McsupBuilder {
    pub fn supervisor(self, s: Arc<dyn SupervisorPort>) -> Self;
    pub fn downloader(self, d: Arc<dyn ArtifactDownloader>) -> Self;
    pub fn scanner(self, s: Arc<dyn JavaScanner>) -> Self;
    pub fn extractor(self, e: Arc<dyn ArchiveExtractor>) -> Self;
    pub fn java_registry(self, r: Arc<dyn JavaSourceRegistryPort>) -> Self;
    pub fn store(self, s: Arc<JsonStore>) -> Self;
    pub fn server_repository(self, r: Arc<dyn ServerRepository>) -> Self;
    pub fn workspace_repository(self, r: Arc<dyn WorkspaceRepository>) -> Self;
    pub fn jar_sources(self, s: Vec<Arc<dyn ServerJarSource>>) -> Self;
    pub fn build(self, data_dir: &Path) -> Result<Mcsup, BootstrapError>;
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

已实施：CLI `--output json` 的全部行类型收敛在
`crates/protocol/src/cli.rs`（`protocol::cli`），`apps/cli` 不再自定义
DTO；行类型字段名已用测试钉住形状。未来的 REST / 桌面端直接复用
`protocol::cli` 这批类型。

JSON 输出格式、REST body、WebSocket 消息都是这批类型的 serde 形式。
改一处，所有入口同步。破坏性变更走新增字段 + `schema` 版本号，
不改旧字段。

`protocol` 是唯一真相来源，禁止各入口自行定义 DTO。

## CLI 契约

CLI 既是人类工具，也是第三方（非 Rust）的编程接口。两者必须并存。

- `--output json` 显式开关，不用 TTY 自动检测。
- stdout 只出数据，stderr 只出日志。
- 退出码稳定映射：0 成功 / 1 通用失败 / 2 用法错误 / 3 未找到 /
  4 状态冲突 / 5 后端失败 / 6 未授权（token 失效、权限不足、证书指纹变化）。
- `--follow` 输出 JSONL（一行一个事件对象），用于实时日志和状态推送。
- `server send <id> <command>` 支持把宿主输入接到服务器 stdin。
- 无交互：不依赖 TTY 输入，不等待确认，需要确认的走显式 `--yes`。
- 交互兜底：参数优先级 `flag > MCSUP_WORKSPACE 环境变量 > 交互提示 / 默认值`；
  必选参数缺失只在 human 输出 + TTY 下进交互补齐，`--output json` 或非 TTY
  下不等待输入，stderr 提示用法并以退出码 2 结束；专用确认开关（如
  `--accept-eula`）与 `--yes` 同此规则。
- `-y` / `--yes` 只作用于**确认式提问**：是 / 否选择、危险操作二次确认一律按
  「是」处理；它不参与必填参数的补参（名称、版本、变体、快照 id 等），缺这些
  参数时仍按上一条进交互或退出 2。这样 `--yes` 不会让机器在不给参数的情况下
  随机挑一个值，也不会改变 stdout 的数据形状。
- 幂等：同样命令重复执行结果可预期。
- 语言无关：`--output json` 的 stdout、错误分类与退出码不随界面语言变化，
  同一输入在任何 locale 下逐字节相同。面向人的文案跟随 `--lang` >
  `MCSUP_LANG` > 平台 locale，详见 `docs/cli-commands.md` 的界面语言一节。
  解析器禁止把语言信息写进 stdout 数据。

这些必须在实现前定死。事后从"人类可用"改成"机器可用"要重写输出层。

## 分发形态

### 发布单元

门面 `mcsup` 是唯一入口与发布主体；它 feature 拉入的每个内层 crate 必须
随之发布（crates.io 不接受 path-only / `publish = false` 依赖）：

```
crates.io
├── mcsup                  门面 lib，feature 选择能力
└── mcsup-domain 等         内层（发布时由单词名改名，见下）

仓库，不发布
├── crates/mcsup/           门面；内层现为 publish = false 的 path 依赖
└── apps/cli/ ...           bin，装出 mcsup / mcsup-agent / mcsup-server
```

`apps/` 不发布到 crates.io。它们通过 `--path` 或 `--git` 安装：

```
cargo install --path apps/cli        → mcsup.exe
cargo install --git <repo> cli       → mcsup.exe
```

Cargo 禁止同一 workspace 内出现同名 package，所以发布库叫 `mcsup` 时，
`apps/` 下的包不能用这个名字。但由于 apps 的 package 名本就是单词名
（`cli` / `agent` / `server`），不构成冲突。

### 为什么是多 crate + 门面

保留多 crate 而不并成单 package 的原因：

- 增量编译与构建并行：crate 边界即缓存边界，改 `file` 不重编 `domain`。
- 内层可被独立依赖与演进（如只想用 `domain` 做校验的第三方）。
- 门面把多包成本收口：第三方只面对 `mcsup`，feature 级联保证「开什么得
  什么」且类型同源，不必手拼多个 crate 的版本组合。

发布时接受的代价：

- 单词名 `domain` / `port` / `service` / `file` / `provider` 在 crates.io
  均被占用（实测 2026-10）；内层发布必须改名 `mcsup-*`（已验证可用）。
- 发布顺序：`mcsup-domain → mcsup-port → mcsup-service / mcsup-file /
  mcsup-java / mcsup-provider / mcsup-storage / mcsup-runtime → mcsup`。
- 版本锁步：内层与门面共用 `workspace.version` 一起 bump，避免同名类型
  跨版本不兼容。
- 内层一经发布即成公共 API，跨层重构受 semver 约束。
- 只开发不发布时无需改名：内层 `publish = false`，`apps/cli` 与测试方走
  path 依赖；未改名前 `cargo publish -p mcsup` 必然失败。

### Feature 矩阵

`default = []`，强制显式选择。不设默认是因为默认 feature 会让
"只选部分能力"名存实亡：写了 `features = ["file"]` 仍会带上默认的
`service` + `provider`，除非调用方记得写 `default-features = false`，
而大部分调用方不会记得。

已实施（`crates/mcsup/Cargo.toml`，feature 切可选依赖并按名重导出）：

```
domain   = ["dep:domain"]
port     = ["dep:port", "domain"]
protocol = ["dep:protocol", "domain"]
service  = ["dep:service", "port"]
file     = ["dep:file", "port"]
java     = ["dep:java", "port"]
provider = ["dep:provider", "port"]
storage  = ["dep:storage", "port"]
runtime  = ["dep:runtime", "port", "protocol"]
```

级联规则：feature 引用同级 feature，被引用者带出其可选依赖——只要
`mcsup::port` 可命名，`mcsup::domain` 必可命名。每新增一个内层 crate，
同步三处：根 `[workspace.dependencies]` 的互依声明（`path + version`
唯一携带处）、门面可选依赖（`workspace = true` + `optional`）与
feature 级联、`lib.rs` 的 `pub use`。内层 manifest 一律
`xxx.workspace = true` 继承，禁止再写版本字面量。

组合 feature（`minimal` / `full`）尚未声明：`check-architecture.sh` 的
feature_bundles 目前只豁免 `default`，新增组合 feature 时同步豁免。
已声明的组合 feature 是 `bootstrap`（默认组合根，见下），允许集合已
加入 `facade_allow`。

feature 语义必须机器校验。如果 `file` 硬依赖 `provider`，那
`features = ["file"]` 会把 `reqwest` 拉进来，这个 feature 就是假的。

该检查已由 `scripts/check-architecture.sh` 第 9 节实现：`cargo tree -p
mcsup --no-default-features --features <f>` 实际拉入的内层 crate 集合等于
`facade_allow` 允许集合；新增内层 crate 或 feature 时同步该表。

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

依赖拓扑序，每步结束都是可用状态。已落地：domain、port、protocol、
service、file、java、provider、storage、runtime（内层 crate + 门面 feature
同步）、apps/cli（workspace / java / server create·install·link·remove 与生命周期
start·stop·restart·status·console·send·logs 已接通，`workspace doctor` 体检已接通，
其余命令为桩）、
门面 `bootstrap` 组合根（`McsupBuilder`，CLI 已改走）、CLI JSON 行类型
收敛（`protocol::cli`，apps/cli 全部命令已切换）。
后续按序：

```
1  cli         库侧命令语义 + 渲染 + JSON（渲染入薄层，语义下沉门面）
2  sidecar     protocol 的 HTTP 绑定
```

第 1 步做完，CLI 库侧语义完整；第 2 步后非 Rust 第三方已有网络编程接口。
