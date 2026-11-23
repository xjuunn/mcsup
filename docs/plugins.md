# 平台插件规范

来自 `AGENTS.md`。

本文是 **平台插件（Platform Plugin）的最终方案与实现规范**。插件宿主、SDK、
CLI / API / MCP 投影、Web 渲染、安装与市场审核都以本文为准。

设计或实现插件、宿主、SDK、插件界面时查阅本文。

---

# 阅读约定

| 标记 | 含义 |
| --- | --- |
| `[规范]` | 已定，实现必须遵守；偏离即为缺陷 |
| `[待实现]` | 设计已定、代码尚未落地；实现时不得改变语义 |
| `[禁止]` | 不得以「更方便」为理由绕过 |

冲突处理顺序：

1. 分层、命名、依赖方向以 `AGENTS.md` 与 `docs/architecture.md` 为准。
2. 插件专属语义以本文为准；本文与其它文档冲突时，先改文档再写代码。
3. 本文没有写到的行为，默认取「更保守、更可观测、更容易回滚」的一侧，
   并把结论补回本文，不允许在代码里形成未记录的约定。

规范中的数值（超时、上限、版本号）是契约的一部分。改动必须同时改本文、
`sdks/conformance/` 的用例与测试，三处不一致视为未完成。

---

# 1. 定位与范围

## 1.1 插件扩展的是平台，不是服务端

MCSUP 里同时存在三种容易混淆的「插件」：

| 概念 | 运行位置 | 生命周期由谁管 | 本文是否涉及 |
| --- | --- | --- | --- |
| **平台插件** | 独立进程，连接宿主本地端点 | 平台（`crates/plugin` + 宿主进程） | 是，全文 |
| **服务端插件** | Minecraft 服务端进程内（Bukkit / Paper 等） | Minecraft / 加载器 | 否，已由 `server mod --kind plugin` 覆盖 |
| **数据包（datapack）** | 世界存档内 | Minecraft | 否，已由内容管理覆盖 |

平台插件不能修改 Minecraft 进程，只能经宿主暴露的能力与其交互。这个边界
必须写进产品文案与市场分类，否则用户会以为「装个平台插件就能改变服务端行为」。

## 1.2 平台插件能做什么

- 监听平台与服务端事件（启动、崩溃、备份完成、聊天、玩家进出、控制台）。
- 经宿主读取服务器状态、属性、版本。
- 经宿主注入控制台命令、启停服务器、创建与恢复备份、管理内容。
- 持有自己的配置、键值数据与凭据。
- 声明操作（op），被 CLI / Web / MCP / REST / 其它插件以同一套语义调用。
- 与其它插件组合：聊天桥 + AI agent + 崩溃助手 + 监控可以拼成一条完整的
  「发现异常 → 询问 AI → 重启 → 通告」链路。

## 1.3 设计目标

| 目标 | 具体承诺 |
| --- | --- |
| 作者心智负担低 | 一个插件 = 一个二进制 + 一份构建时生成的 descriptor；不写前端、不写路由、不写协议 |
| 自动适配 Web | descriptor 是唯一界面来源，六种固定展示类型覆盖绝大多数插件 |
| 可组合 | 宿主中介的事件广播与闭集契约调用，禁止插件直连 |
| 可审计 | 每次能力调用、契约调用、授权变更都有记录 |
| 可恢复 | 崩溃退避、升级失败回滚、授权扩张需重新确认 |
| 跨平台 | macOS / Linux / Windows 同一套语义；端点差异封装在实现层 |
| 可扩展 | API 版本独立于宿主版本；新能力按阶段追加，旧插件不失效 |

## 1.4 顶层不做

以下不是「以后再说」，而是架构层面的排除项，理由见第 22 节：

- 不做原生动态库（`.so` / `.dll` / `.dylib`）插件。
- 不做安装时执行插件代码（包括 `build.rs`、`postinstall`、`pip install`）。
- 不做插件自带 HTML / CSS / JavaScript。
- 不做插件之间的直接连接。
- 不做热替换 / 热重载。
- 不把插件进程描述成「沙箱」。

---

# 2. 术语

| 术语 | 定义 |
| --- | --- |
| **宿主（host）** | 长驻进程，拥有插件进程、端点、能力门、事件扇出与契约路由 |
| **插件进程** | 插件作者交付的可执行文件，由宿主拉起，经本地端点通信 |
| **控制通道** | CLI / API / agent 与宿主之间的本地连接，用于管理、调用、订阅 |
| **会话（session）** | 一次插件进程连接。重连即新会话，旧会话上的 inflight 全部作废 |
| **能力（capability）** | 宿主代做的动作的开关，如 `console:write` |
| **订阅（subscription）** | 事件类别，如 `chat:message`，决定投递哪类事件 |
| **契约（contract）** | 插件之间经宿主中转的固定接口，如 `chat.publish@1` |
| **op** | 插件声明的操作，分 `query` / `action` / `stream` 三类 |
| **descriptor** | 构建时生成的机器描述：配置 schema、op 表、展示类型 |
| **实例（instance）** | 一个插件的运行单位。默认机器级单实例，可声明为每服一实例 |
| **resident** | 常驻标记。为真时宿主不因空闲回收该插件，且宿主随之存活 |
| **产物（artifact）** | 可安装的压缩包：manifest + descriptor + 二进制 + 许可证 |
| **可信产物** | 摘要匹配、通过解包安全检查与 manifest / descriptor 校验的产物；当前不校验签名 |

---

# 3. 硬规则

以下条款是可被 review 直接引用的编号。违反任一条即为实现缺陷，不接受
「当前实现方便」作为理由。

| 编号 | 规则 |
| --- | --- |
| H1 | 插件只能是外部进程。`[禁止]` 动态库、WASM（v1）、脚本内嵌宿主 |
| H2 | 插件与宿主之间只有一条本地端点 + JSONL 帧；`[禁止]` 插件直接访问宿主数据库或内部文件 |
| H3 | `[禁止]` 安装阶段执行插件代码。安装只做下载、校验、解包、登记 |
| H4 | 能力授予 = manifest 声明 ∩ hello 申请 ∩ 用户确认，三者缺一即不授予 |
| H5 | `[禁止]` 把插件进程称为沙箱。文档、CLI、市场文案只能承诺契约、审计、审核 |
| H6 | 判定逻辑写在 `domain` / `service`。`crates/plugin`、`apps/*` 只做执行与协议适配 |
| H7 | `[禁止]` 插件与插件直连。组合只能经宿主的事件广播或闭集契约 |
| H8 | 契约集合闭合。`[禁止]` 插件自定义契约、通用 workflow 引擎、动态 RPC |
| H9 | descriptor 必须构建时生成。`[禁止]` 运行时生成、手写与代码并存 |
| H10 | 运行时输入校验由宿主完成。插件 SDK 不重复实现 schema 校验 |
| H11 | 单帧上限 24 MiB；超限即协议错误并终止会话（`result` 帧超限改回 `limit_exceeded`） |
| H12 | `[禁止]` 热替换。升级、改配置、改授权一律经明确状态迁移 |
| H13 | 升级若新增能力，授权状态回到 `PendingApproval`，不沿用旧授权 |
| H14 | 一台机器同一插件同一作用域只有一个运行实例；`[禁止]` 多实例并存 |
| H15 | CLI / Web / MCP / REST 的 op 调用必须走同一张 op 表与同一条能力门 |
| H16 | 插件日志只走 stdout / stderr；`[禁止]` 把日志写进协议端点 |
| H17 | 密钥只写不读。读取接口返回 `***`，日志与审计不得出现明文 |
| H18 | 卸载默认连同配置与数据一起删；保留数据必须显式 `--keep-data` |
| H19 | 协议版本、API 版本、manifest schema、descriptor schema 各自独立递增 |
| H20 | 所有耗时步骤必须有进度回显；JSON 模式下 stdout 只含数据 |
| H21 | `[禁止]` 插件写前端资源。界面只能由 descriptor 投影生成 |
| H22 | 插件产物必须可离线校验：摘要 + 平台匹配必备；当前不校验签名，安装记录如实保存摘要 |
| H23 | 每条硬规则与关键小节必须在 `docs/plugins-testing.md` 有测试 ID；没有对应用例的条款视为未实现 |

# 4. 架构落位

## 4.1 总览

```
apps/cli ─┐
apps/server（目标）─┼─→ mcsup 门面（feature = plugin）
apps/mcp（目标）─┤        │
apps/agent ─┘        │
                            ├─→ plugin   （宿主运行时，实现层）
                            ├─→ service  （编排 + HostApi 实现）
                            ├─→ storage  （PluginStore 实现）
                            ├─→ provider （ArtifactSource 实现）
                            └─→ protocol / port / domain

plugin ─→ port ─→ domain
   │         ↑
   └→ protocol（帧集）─→ auth / domain / port
```

`crates/plugin` 是**实现层**，与 `runtime` / `file` / `backup` 同级：

| 约束 | 内容 |
| --- | --- |
| 允许依赖 | `domain` / `port` / `protocol` |
| 禁止依赖 | `service` / 其它实现层 / `apps/*` |
| 反向依赖 | 宿主需要的平台能力经 `port::plugin::*` 注入，由 `service` 实现 |
| 对外暴露 | 仅经门面 feature `plugin`；第三方不得直接依赖 `mcsup-plugin` |

`[禁止]` 把宿主逻辑写进 `apps/cli`。CLI 只保留：

- `mcsup plugin ...` 的参数解析、输出渲染、退出码映射；
- 隐藏子命令 `mcsup __plugin-host` 的外壳（装配 + 事件注入 + 等待信号）。

## 4.2 为什么 SDK 不在 `crates/`

`crates/<dir>` 的命名断言是 package `mcsup-<dir>`、`[lib] name = <dir>`。SDK 若
放进 `crates/`，只能叫 `mcsup-sdk` / `use sdk::`：名字泛化、与 npm / PyPI 命名
不一致，且会被误认为宿主的一部分。

SDK 放在独立 workspace：

```
sdks/
├── conformance/        语言无关的用例（唯一规范）
├── rust/               package + lib = mcsup-plugin-sdk
└── typescript/         npm package = mcsup-plugin
```

`sdks/*` 不是 Cargo workspace 成员，不受 `check-architecture.sh` 的 `crates/*`
规则约束；但必须由 `scripts/check-sdk.sh` 单独把门，CI 与 `scripts/check.sh`
并列执行。

## 4.3 三个注册表里的同名问题

| 名字 | 注册表 | 实际内容 | 谁依赖 |
| --- | --- | --- | --- |
| `mcsup-plugin` | crates.io | **宿主运行时**（`crates/plugin`） | 只有门面；第三方不直接依赖 |
| `mcsup-plugin-sdk` | crates.io | **Rust 插件 SDK**（`sdks/rust`） | 插件作者 |
| `mcsup-plugin` | npm | **TypeScript 插件 SDK**（`sdks/typescript`） | 插件作者 |
| `mcsup-plugin` | PyPI（目标） | Python 插件 SDK | 插件作者 |

crates.io 与 npm 同名但语义不同，这是用户已确认的命名。发布说明、市场页面、
SDK README 必须显式区分，避免作者把宿主 crate 写进插件依赖。

## 4.4 新增模块的落位

### `domain::plugin` `[规范]`

纯词汇与纯规则，不依赖本仓库任何模块：

| 类型 | 职责 |
| --- | --- |
| `PluginId` / `PluginVersion` / `ApiVersion` | 标识与格式校验 |
| `Capability` / `CapabilitySet` | 能力枚举、解析、集合运算 |
| `Subscription` | 事件类别枚举 |
| `OpRisk` | `read` / `mutating` / `destructive` |
| `DisplayType` | 六种展示类型 |
| `Manifest` | 安装与运行所需的声明 |
| `Descriptor` / `Op` / `Display` | 机器描述 |
| `PluginState` | 生命周期状态与 `transition` |
| `ContractId` | 契约标识与版本解析 |
| `Grant` | 授予结果（能力集合 + 来源） |
| `PluginError` | 纯校验错误（字段名 + 原因） |

规则必须放在这里，不得下沉到 `crates/plugin`：

- manifest / descriptor 的结构与限值校验；
- 能力三方交集的集合运算；
- 状态迁移是否合法；
- 展示类型与 op 形状的静态一致性检查；
- 契约标识解析（`chat.publish@1`）。

### `port::plugin` `[规范]`

| trait | 作用 | 实现方 |
| --- | --- | --- |
| `ServerRead` | 列服务器、状态、版本、属性 | `service` |
| `ConsoleIo` | 控制台游标读取、注入一行 | `service` |
| `ServerLifecycle` | 启停重启 | `service` |
| `BackupOps` | 备份创建 / 列表 / 恢复 / 校验 / 清理 | `service` |
| `ContentOps` | 已装内容查询与增删 | `service` |
| `JavaOps` | 托管 JDK 列表 / 安装 / 卸载 | `service` |
| `FileOps` | 服务端目录内受限读写 | `service` + `file` |
| `SecretStore` | 插件命名空间的凭据 | `storage` |
| `KvStore` | 插件键值 | `storage` |
| `NetProxy` | 宿主代发的出站请求（可选路径） | `provider` |
| `PluginStore` | 安装记录、当前版本、授权状态 | `storage` |
| `ArtifactSource` | GitHub / url 解析到产物字节 | `provider` |
| `ArtifactVerifier` | 来源摘要与解包目录复查 | `plugin` |
| `AuditSink` | 审计记录 | `storage` |
| `PluginSupervisor` | 启停 / 状态 / 控制通道客户端 | `plugin` |

`port` 不得依赖 `protocol`（现有断言 `protocol -> port`，反向会成环）。因此
宿主错误码定义在 `port::plugin::PluginErrorCode`，`protocol` 负责其线格式。

### `protocol::plugin` `[规范]`

| 模块 | 内容 |
| --- | --- |
| `frame` | `PluginFrame` 枚举、`PLUGIN_PROTOCOL_VERSION = 1`、读写函数 |
| `event` | 事件信封、kind 常量、`DomainEvent` 映射 |
| `control` | 控制通道帧、`PLUGIN_CONTROL_VERSION = 1` |
| `contracts` | 六个契约的载荷类型 |
| `access` | `OpRisk` → `auth::Role` / `Permission` 映射 |

帧编解码复用 `protocol` 现有的 JSONL 纪律，但**版本常量独立**：

```rust
pub const PLUGIN_PROTOCOL_VERSION: u32 = 1;
pub const PLUGIN_CONTROL_VERSION: u32 = 1;
// domain::plugin
pub const PLUGIN_API_VERSION: u32 = 1;
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const DESCRIPTOR_SCHEMA_VERSION: u32 = 4;   // 2 起 setup，3 起 bind，4 起 setup.when
```

`[禁止]` 把插件帧塞进现有的 `Request` / `Response`：那两者是「一连接一请求」
的 supervisor IPC 语义，插件是长连接多路复用，混用会让两条路径的兼容策略
互相拖累。

## 4.5 方向反转：HostApi

`crates/plugin` 需要「列服务器、发控制台、建备份」，但脚本断言
`service -> domain / port`、实现层之间互不依赖，所以 `plugin -> service` 违规。

正确方向：

```
plugin::PluginHost ──调用──→ dyn port::plugin::ServerRead / ConsoleIo / ...
                                      ↑ 实现
                                  service::plugin::HostApiImpl
```

`service` 依赖 `port` 是既有方向，实现这些 trait 合规。装配发生在门面
`bootstrap`：它同时持有 `plugin` 与 `service`，是唯一允许把两边接起来的位置。

`[禁止]` 为图省事把 `HostApi` 做成一个 God Trait。按能力拆成小 trait 的原因：

- 不同实现方（service / storage / provider）各自只实现自己那部分；
- 门面可以按 feature 组合，不需要为了一个方法拉入整套依赖；
- 测试可以逐能力替换，不必构造巨型 mock。

## 4.6 事件注入点

宿主进程用门面 `bootstrap` 装配服务，并向 `McsupBuilder::events` 注入一个
`EventSink` 适配器，把 `DomainEvent` 转成插件事件：

```rust
// apps/cli/src/plugin_host.rs（外壳，无判定）
let bus = plugin::EventBus::new(/* ... */);
let mcsup = mcsup::bootstrap::McsupBuilder::new()
    .events(Arc::new(plugin::EventSinkAdapter::new(bus.clone())))
    .build(&data_dir)?;
```

这是**既有注入点**，不新增全局状态。`EventSink::publish` 的语义是旁路通知：
投递失败（队列满、插件崩溃）只记日志与指标，不影响业务本身。

`console:line` 与聊天事件不来自 `DomainEvent` 的轮询，而来自控制台追随：

- `port::plugin::ConsoleIo::tail(server, cursor, max_lines)` 提供游标分页；
- 宿主按订阅的服务器每 500 ms 拉取一次，去重后转成事件；
- 聊天解析在 `domain::server::chat`（纯函数），由 `service` 在追随路径上完成，
  产出 `DomainEvent::ChatMessage`；
- `[禁止]` 在 `crates/plugin` 里解析 Minecraft 日志格式。

M1 必须先补三个领域事件变体与聊天解析器：

| 变体 | 来源 | 备注 |
| --- | --- | --- |
| `ChatMessage { server, from, text, at }` | 控制台追随 | 需要解析器，格式差异集中在 `domain` |
| `PlayerJoined { server, player }` | 控制台追随 | 登录行 |
| `PlayerLeft { server, player }` | 控制台追随 | 登出行 |

领域事件用 `server` 而不是其它事件里的 `name`，因为 `ChatMessage` 同时有
`from` 与 `text`，`name` 会产生歧义；线格式同样用 `server`。这是有意的不一致，
实现时不得改名。

## 4.7 宿主进程与控制通道

插件必须活得比一次 CLI 命令久，因此宿主是独立的长驻进程：

```
mcsup __plugin-host --data-dir <path> --idle-timeout 300
```

| 属性 | 规则 |
| --- | --- |
| 触发 | 首个需要插件的命令按需拉起；已有实例则直接连接 |
| 单实例 | 锁文件 `{data_dir}/mcsup/run/plugin-host.lock`，由内核持有 |
| 控制端点 | Unix socket / Windows 命名管道 / 回环 TCP 回退 |
| 构建自愈 | 连上后先取 `identity` 核对宿主构建标识（版本 / 路径 / 大小 / mtime），不是当前可执行文件就换掉并恢复原先在跑的插件 |
| 生命周期 | 收到 `shutdown` 帧才退出；空闲只回收非 resident 插件的会话，宿主本身常驻 |
| 事件投递 | 只在宿主存活期间有效；事件驱动插件必须 `resident: true` |
| 日志 | `{data_dir}/mcsup/logs/plugin-host.log`，按大小轮转 |

宿主是常驻进程，二进制被替换（重新编译、`cargo install`、升级安装包）不会换掉
已经在跑的那一个：它继续执行启动时那份镜像里的代码，所以「端点连得上」不等于
「跑的是当前构建」。`ensure()` 因此每次连上后都做一次 `identity` 握手，三类不
一致分别对应三种成因——编译版本不同（`VersionMismatch`）、同一个版本但不是同
一个可执行文件（`DifferentExe`）、同一路径上的文件在宿主启动之后被替换
（`BinaryReplaced`）；元数据缺失时一律判「一致」，宁可不杀宿主也不误杀。换宿主
会连带停掉插件，所以重启后按 §11.9 的绑定门禁恢复：绑定服务器都停着的插件不会
被拉起来。

`apps/agent` 与未来的 `apps/server` 不重复实现宿主：它们连接同一个控制端点。
远程机器上由该机器的宿主负责本机插件，跨机不共享插件进程。

## 4.8 新增内层 crate 的同步清单

这一节是给实现 agent 的硬性清单。漏一项的表现写在最后一列。

| # | 位置 | 改动 | 漏掉的后果 |
| --- | --- | --- | --- |
| 1 | 根 `Cargo.toml` | `members` 加 `"crates/plugin"` | 包不编译、不测试，CI 仍全绿 |
| 2 | `scripts/check-architecture.sh` | `impl_layers` 加 `plugin` | 依赖方向不再受检 |
| 3 | 同上 | 新增一行 `check_module_layer plugin "plugin\|protocol\|$core_modules"` | 模块级引用 `protocol` 被误判或漏判 |
| 4 | 同上 | `facade_allow` 加 `plugin) printf '%s' 'domain port protocol plugin' ;;` | feature 语义检查失败 |
| 5 | 同上 | `bootstrap` / `kit` / `remote` 的允许集合各追加 `plugin` | 组合 feature 拉入集合与允许集合不一致，检查失败 |
| 6 | `crates/mcsup/Cargo.toml` | 可选依赖 `plugin = { package = "mcsup-plugin", ... }`、feature `plugin = ["dep:plugin", "port", "protocol"]`、三个组合 feature 追加 `"plugin"` | 门面不可用 |
| 7 | `crates/mcsup/src/lib.rs` | `#[cfg(feature = "plugin")] pub use plugin;` | feature 与模块对应断言失败 |
| 8 | `.github/workflows/publish.yml` | 在 `mcsup-protocol` 之后、`mcsup-service` 之前插入 `mcsup-plugin` | 发布时才失败 |
| 9 | `scripts/bump-version.sh` | 同步 `crates/plugin/Cargo.toml` 的 `version` 字面量 | 版本漂移 |

**不要**把 `plugin` 加进 `check-architecture.sh` 第 7 节的
`for layer in backend runtime java storage file backup provider modkit` 循环：
那个循环只允许 core 与自身，而 `plugin` 需要 `protocol`。必须像 `transport`
一样单独一行。

`backend` 允许组合实现层，但 `[禁止]` 让 `backend` 依赖 `plugin`：执行位置
抽象不负责插件运行时。`impl_layers` 加 `plugin` 后脚本会放行这条边，因此必须在
`backend` 的允许集合里显式排除 `plugin`，并在 review 中作为违规处理。

# 5. 运行模型

## 5.1 启动

宿主拉起插件进程时只传两样东西：

```
<entry.cmd> --endpoint <endpoint> [--server <name>] [--log-level <level>]
环境变量：MCSUP_PLUGIN_TOKEN=<一次性令牌>
          MCSUP_PLUGIN_ID=<id>
          MCSUP_PLUGIN_SCOPE=<machine|server>
```

规则：

- `--endpoint` 必填，由宿主在 spawn 时给出；不使用环境变量传端点（便于排错）。
- 令牌**只经环境变量**传递。`[禁止]` 放在 argv：`ps` / 进程列表会泄露。
- 令牌是一次性的 32 字节随机值，仅本次会话有效，断开即作废。
- 作用域为 `server` 时额外传 `--server`，事件与能力调用都被限制在该服务器。
- 插件不得自行重连。连接断开即宿主侧会话结束，插件应尽快退出。
- 插件必须在 10 s 内连接并发出 `hello`，否则宿主杀掉进程并记 `handshake_timeout`。

## 5.2 端点

| 平台 | 首选 | 回退 |
| --- | --- | --- |
| Linux / macOS | Unix domain socket | 回环 TCP |
| Windows | 命名管道 `\\.\pipe\mcsup-plugin-<hash>` | 回环 TCP |

端点放在 `{data_dir}/mcsup/run/` 下，文件名带 64 位 FNV-1a 哈希：

```
plugin-<hash>.sock
```

哈希输入是 `(data_dir, plugin_id, scope, server)` 的拼接，避免不同实例撞名。

macOS 的 Unix socket 路径上限约 104 字节。端点解析必须：

1. 计算完整路径长度；
2. 超过 100 字节时改用 `$TMPDIR/mcsup-<uid>/pl-<hash>.sock`；
3. 仍超限则回退回环 TCP。

回环 TCP 必须绑定 `127.0.0.1`，端口由内核分配，端点串形如
`tcp://127.0.0.1:<port>`，令牌校验在 `hello` 帧完成。

Unix socket 权限 `0600`；Windows 管道使用当前用户 DACL。`[禁止]` 监听
`0.0.0.0` 或局域网地址。

## 5.3 通道划分

| 通道 | 方向 | 内容 | 是否走协议 |
| --- | --- | --- | --- |
| 端点 | 双向 | 帧（握手、事件、调用、心跳） | 是 |
| stdout | 插件 → 宿主 | 人类可读日志 | 否，逐行采集 |
| stderr | 插件 → 宿主 | 错误与告警 | 否，逐行采集 |
| 文件 | 插件 → 自己 | 插件私有数据 | 否，只能写自己的数据目录 |

`[禁止]` 把日志写进端点，也禁止插件把协议帧写到 stdout。两者混用会让宿主
收到半截 JSON，排错成本极高。

日志采集规则：

- 单行超过 8 KiB 截断并标注 `[truncated]`；
- 每插件限速 200 行/秒，超出丢弃并每 10 s 汇总一条 `dropped N lines`；
- 宿主加前缀 `[<plugin_id>]`，写入 `logs/plugins/<id>.log`；
- 已知密钥值在采集时做精确匹配替换为 `***`（尽力而为，不承诺覆盖所有编码）。

## 5.4 并发与限额

| 项 | 默认 | 上限 | 超出行为 |
| --- | --- | --- | --- |
| 单帧字节 | 24 MiB | 24 MiB | 协议错误，终止会话 |
| 单帧 JSON 嵌套深度 | 32 | 32 | 协议错误，终止会话 |
| 出站 inflight（宿主→插件） | 8 | 32 | 排队；队列满返回 `limit_exceeded` |
| 插件→宿主 inflight | 8 | 32 | 同上 |
| 事件队列 | 1024 | 4096 | 丢最旧，`seq` 出现缺口，记指标 |
| 日志 | 200 行/秒 | 固定 | 丢行并汇总 |
| op 超时 | 30 s | 300 s | 返回 `timeout`，发 `cancel` |
| 流式总字节 | 16 MiB | 64 MiB | 终止流并返回 `limit_exceeded` |
| 流式空闲 | 60 s | 固定 | 终止流 |

插件进程的 CPU / 内存不由宿主强制限制。`manifest.resources` 是声明值，用于
市场展示与告警建议，不是配额。`[禁止]` 在文档中把它描述成「资源限制生效」。

## 5.5 实例、作用域与空闲回收

| 维度 | 规则 |
| --- | --- |
| 作用域 | `machine`（默认）或 `server`（manifest 显式声明） |
| 实例数 | 每个 `(plugin_id, scope, server)` 至多一个运行进程 |
| 惰性启动 | 首次调用 op、首次需要投递事件、或用户显式 `start` 时才拉起 |
| 空闲回收 | 非 resident 插件在最后一次活动后 120 s 停止进程，保留安装记录 |
| resident | 不随空闲停止；有 resident 插件运行时宿主不退出 |
| 事件丢失 | 宿主未运行时事件不排队、不补发。事件驱动插件应 `resident: true` |

依赖集中：machine 作用域的插件被所有工作区与服务器共享，避免「每台服务器
起一个聊天桥」。需要按服隔离的插件（例如只连某个服的 RCON）声明
`"scope": "server"`。

## 5.6 崩溃与重启

| 情况 | 处理 |
| --- | --- |
| 握手前退出 | 记录退出码与最后 20 行 stderr，状态 `failed`，不自动重试 |
| 运行中崩溃 | 指数退避重启：1 s、2 s、4 s、8 s、16 s |
| 10 分钟内第 5 次崩溃 | 状态转 `failed`，停止自动重启，保留证据 |
| 用户手动 `start` | 清零退避计数 |
| 宿主退出 | 先发 `shutdown`，宽限 5 s，超时强杀 |
| 插件拒绝退出 | 记录 `kill` 审计，强杀并标记 `stopped` |

自动重启不得跨越能力授权变更或版本变更；升级后必须重新握手并重新计算授予。

## 5.7 平台差异

| 项 | macOS | Linux | Windows |
| --- | --- | --- | --- |
| 端点 | Unix socket | Unix socket | 命名管道 |
| 进程树终止 | 进程组 | 进程组 | Job Object（若不可用则逐 PID 终止） |
| 控制台归属 | 不适用 | 不适用 | 宿主自带（detached），插件进程自取无窗口控制台，不继承启动者 |
| 可执行位 | 校验 `0o111` | 校验 `0o111` | 校验扩展名与 PE 头 |
| 路径大小写 | 保留 | 保留 | 不敏感，比较前归一化 |
| 长路径 | 受端点限制 | 受端点限制 | 启用 `\\?\` 前缀 |

`crates/plugin` 内部按 `cfg` 分支实现，但对外语义必须一致。测试矩阵必须覆盖
三条平台的端点建立与超时路径；无法在 CI 覆盖的平台用 `testkit` 的
`FakeEndpoint` 走同一套帧。

Windows 的控制台是**继承**来的：宿主是常驻进程，若它继承的是拉起它的那个 CLI
的控制台，终端（含无窗口的伪控制台）一销毁，宿主手里就留下一个已经死掉的控制台
句柄。此后它再创建控制台程序（`.cmd` / `.bat` 入口要经 `cmd.exe`），子进程会在
控制台初始化阶段直接失败，退出码 `0xC0000142`（`STATUS_DLL_INIT_FAILED`），而且
一行输出都没有。所以宿主以 detached 方式拉起（脱离终端 + 独立进程组），插件进程
再用 `CREATE_NO_WINDOW` 自取一个无窗口控制台——两者都不依赖上一代进程的状态。
启动失败的原因文本会带上 NTSTATUS 符号名，便于一眼定性。

---

# 6. 帧协议

## 6.1 信封

所有帧是一行 JSON，字段 `snake_case`，末尾 `\n`，写完立即 flush：

```json
{"v":1,"type":"event","seq":42,"kind":"chat:message","at":"2026-11-18T09:30:00Z","server":"survival","data":{...}}
```

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `v` | u32 | 必须是 `PLUGIN_PROTOCOL_VERSION` |
| `type` | string | 帧类型 |
| 其余 | 依类型 | 见下 |

未知字段必须忽略；未知 `type` 必须忽略并记一行日志（不能断开）。版本不匹配
必须断开，不做猜测兼容。

解码前先做两项检查，避免解析器被构造输入拖垮：单行字节数 ≤ 24 MiB，JSON 嵌套
深度 ≤ 32。任一超出即按协议错误终止会话，不进入反序列化。

24 MiB 不是随手取的数：`files:read` / `files:write` 允许单文件 16 MiB 内联
传输，base64 后膨胀到 `ceil(16 MiB / 3) * 4 ≈ 21.3 MiB`，再加 JSON 字段与
转义开销。上限低于它，读一个几 MiB 的 jar 就会被判成「单帧超限」并断开整条
连接——调用本身完全合法。写出侧同样按此上限检查（[`write_frame`]），任一侧
都不会写出对端必然拒绝的帧。

## 6.2 帧类型

| 类型 | 方向 | 作用 |
| --- | --- | --- |
| `hello` | 插件 → 宿主 | 声明身份、API 版本、申请能力与订阅 |
| `welcome` | 宿主 → 插件 | 授予结果、配置、限额、作用域 |
| `reject` | 宿主 → 插件 | 拒绝握手并给出原因 |
| `event` | 宿主 → 插件 | 事件投递 |
| `call` | 双向 | 调用 op / 调用宿主能力 |
| `result` | 双向 | 调用的成功或失败 |
| `stream` | 插件 → 宿主 | 流式分块 |
| `cancel` | 双向 | 取消进行中的调用或流 |
| `config` | 宿主 → 插件 | 配置变更（带 id，需回 `result`） |
| `ping` | 宿主 → 插件 | 心跳 |
| `pong` | 插件 → 宿主 | 心跳回应 |
| `shutdown` | 宿主 → 插件 | 优雅退出 |

心跳每 15 s 一次，连续 3 次未回（45 s）判定失联，终止会话并按崩溃处理。

## 6.3 握手

```json
{"v":1,"type":"hello","token":"<64 hex>","api":1,"plugin_id":"chat-bridge",
 "plugin_version":"0.3.1","platform":"darwin-arm64","pid":41234,
 "requested":["console:read"],"subscriptions":["chat:message"]}
```

```json
{"v":1,"type":"welcome","session_id":"s-7f3a9c","host_version":"0.1.0",
 "granted":["console:read"],
 "limits":{"inflight":8,"frame_bytes":25165824,"event_queue":1024,"log_lines_per_sec":200},
 "config":{"revision":3,"values":{"token":"***","channels":["global"]}},
 "scope":{"kind":"machine"}}
```

```json
{"v":1,"type":"reject","code":"api_mismatch","message":"plugin requires api 2, host supports 1",
 "details":{"plugin_api":2,"host_api":1}}
```

握手步骤（宿主侧）：

1. 接受连接，10 s 内必须收到 `hello`，否则关闭并杀进程。
2. 校验令牌、`plugin_id`、`plugin_version` 与安装记录一致。
3. 校验 `api`：`plugin.api` 必须等于 `PLUGIN_API_VERSION`；不等即 `reject`。
4. 校验 `requested ⊆ manifest.capabilities`；越界项**不授予**并写审计。
5. 校验 `subscriptions ⊆ manifest.subscriptions`，且每项所需能力已授予。
6. 计算 `granted = manifest ∩ requested ∩ approved`。
7. 下发 `welcome`，写入会话表，开始心跳。
8. `reject` 后宿主等待 5 s，插件未退出则强杀。

插件侧：必须先用 `hello`，在收到 `welcome` 前不得发 `call` 或 `event`；
收到 `reject` 后必须退出，退出码 4。

## 6.4 调用

宿主调用插件 op：

```json
{"v":1,"type":"call","id":7,"op":"send","input":{"channel":"global","text":"hi"}}
```

插件调用宿主能力：

```json
{"v":1,"type":"call","id":7,"capability":"console:write","method":"send_line",
 "input":{"server":"survival","line":"say hi"}}
```

插件调用另一个插件提供的契约（`target` 仅单播可用，`trace` 由宿主分配）：

```json
{"v":1,"type":"call","id":8,"contract":"chat.publish@1",
 "input":{"server":"survival","text":"hi"}}
```

根调用不带 `trace`。宿主在接受根调用时分配一个不透明令牌并随调用链登记；
提供方再次发起契约调用时必须把收到的 `trace` 原样带回，宿主据此做循环、
深度与扇出判定。提供方无法伪造链路：未知或过期的 `trace` 返回 `conflict`。

宿主向提供方投递契约时使用 op `contract.call`，调用链令牌在 `input.trace`：

```json
{"v":1,"type":"call","id":9,"op":"contract.call",
 "input":{"contract":"chat.publish@1","caller":"monitor","trace":"<64 hex>",
          "input":{"server":"survival","text":"hi"}}}
```

`net:outbound` 的宿主方法为 `request`：

```json
{"v":1,"type":"call","id":10,"capability":"net:outbound","method":"request",
 "input":{"method":"GET","url":"https://api.example.com/status",
          "headers":{"accept":"application/json"},"body":null,"timeout_ms":5000}}
```

请求体与响应体都用 base64；响应为
`{"status":200,"headers":{...},"body":"<base64>","body_len":123}`。

`id` 由发起方分配，在**同一方向**内唯一。两个方向各自维护 id 空间，`result`
按方向归属，不会混淆。

结果：

```json
{"v":1,"type":"result","id":7,"ok":true,"output":{"ok":true,"message_id":"m-1"}}
{"v":1,"type":"result","id":7,"ok":false,
 "error":{"code":"denied","category":"unauthorized","retryable":false,
          "message":"console:write 未授予（当前：console:read）"}}
```

错误码与分类：

| `code` | `category` | 可重试 | 含义 |
| --- | --- | --- | --- |
| `invalid_input` | `usage` | 否 | 输入不符合 descriptor |
| `not_found` | `not_found` | 否 | 服务器 / 资源不存在 |
| `denied` | `unauthorized` | 否 | 能力未授予或越权 |
| `conflict` | `conflict` | 视情况 | 状态不允许（已在运行等） |
| `timeout` | `backend` | 是 | 超时 |
| `cancelled` | `cancelled` | 否 | 被取消 |
| `unavailable` | `backend` | 是 | 插件未运行 / 正在重启 |
| `crashed` | `backend` | 是 | 插件进程崩溃 |
| `limit_exceeded` | `usage` | 否 | 超过帧 / 流 / 队列限额 |
| `unsupported` | `usage` | 否 | 当前平台或版本不支持 |
| `internal` | `failure` | 否 | 插件内部错误 |

`[禁止]` 插件把未授予的能力调用「降级成成功」。未发出就是未发出，必须回报
`denied`，否则用户会以为消息已送达。

## 6.5 流

```json
{"v":1,"type":"stream","id":9,"seq":0,"chunk":{"delta":"你"},"done":false}
{"v":1,"type":"stream","id":9,"seq":1,"chunk":{"delta":"好"},"done":false}
{"v":1,"type":"stream","id":9,"seq":2,"chunk":{"delta":""},"done":true}
```

规则：

- `seq` 从 0 连续递增；宿主检测缺口即终止流。
- `chunk` 是 descriptor 声明的输出 fragment schema，不是任意结构。
- `done: true` 之后不得再发同 id 的 `stream`。
- 取消：宿主发 `{"type":"cancel","id":9,"reason":"timeout"}`，插件在 1 s 内停止
  并发 `result`（`code: cancelled`）。插件不得忽略取消继续写流。

## 6.6 配置变更

```json
{"v":1,"type":"config","id":11,"revision":4,"values":{"token":"***","channels":["global"]}}
```

- 宿主先按新 schema 校验；校验失败不发帧，直接返回用户错误。
- 插件应用成功后回 `result{id:11,ok:true}`；失败回 `ok:false`。
- 失败时宿主恢复上一版本，状态转 `degraded` 并在 `plugin status` 中展示原因。
- `revision` 单调递增，插件可据此丢弃过期变更。

## 6.7 关闭

| 触发 | 顺序 |
| --- | --- |
| 用户 `stop` | `shutdown{grace_ms:5000}` → 等退出 → 超时强杀 |
| 宿主退出 | 对所有会话发 `shutdown`，并发等待，超时强杀 |
| 插件主动退出 | 关闭端点即视为退出；宿主按退出码记录 |
| 协议错误 | 立即关闭端点 + 强杀，记 `protocol_error` |
| 心跳超时 | 同上，记 `heartbeat_timeout` |

插件收到 `shutdown` 后必须：停止接受新调用 → 完成或取消 inflight → flush 数据
→ 退出码 0。退出码约定：

| 退出码 | 含义 |
| --- | --- |
| 0 | 正常退出 |
| 1 | 未分类失败 |
| 2 | 协议或握手错误 |
| 3 | 配置无效 |
| 4 | API 版本不支持 |

## 6.8 一次调用的完整序列

```
宿主 → 插件   call{id:7, op:"send", input:{...}}
插件 → 宿主   call{id:1, capability:"console:write", method:"send_line", ...}
宿主 → 插件   result{id:1, ok:true}
插件 → 宿主   result{id:7, ok:true, output:{...}}
```

重入是允许的：插件处理 `call` 期间可以发起宿主调用。宿主必须支持同一会话上
的嵌套调用，不能串行化到死锁。

## 6.9 控制通道

CLI / `apps/server` / `apps/agent` 与宿主之间的连接使用独立的控制端点，
版本为 `PLUGIN_CONTROL_VERSION`，帧形状复用 `call` / `result` / `stream` /
`event` / `ping` / `pong`，但 `call` 里的字段是 `method` 而不是 `capability`：

```json
{"v":1,"type":"call","id":3,"method":"status","input":{"id":"chat-bridge"}}
{"v":1,"type":"result","id":3,"ok":true,"output":{"state":"running","version":"0.3.1"}}
```

| `method` | 作用 |
| --- | --- |
| `list` | 列出安装记录与实时状态 |
| `status` | 单插件状态、能力、最近错误 |
| `start` / `stop` / `restart` | 进程控制 |
| `call` | 调用插件 op（`input.op` + `input.input`） |
| `stream` | 订阅插件 stream，返回 `stream` 帧 |
| `logs` | 游标读取插件日志 |
| `config_get` / `config_set` | 配置读取（密钥打码）/ 写入 |
| `config_apply` | 只下发已由管理端校验并落盘的配置，不重复写盘 |
| `grant` / `revoke` | 授权变更 |
| `reload` | 安装 / 卸载后让宿主重读记录 |
| `identity` | 宿主自身的构建标识（启动时快照），供客户端判断宿主是否过期 |
| `shutdown` | 宿主退出 |

`identity` 返回 `{"version":…,"exe":…,"len":…,"mtime_secs":…}`：`version` 是编译
进二进制的版本，其余是宿主**启动时**读到的自身可执行文件信息。快照在
`HostControl::new` 取一次，之后即使磁盘上的文件被替换，旧宿主报告的仍是它启动
时的那份——这正是判定「宿主过期」需要的基准。客户端拿自己的同一组值比较，缺字
段（旧宿主、元数据读不到）按「一致」处理。

构建早于该方法的宿主会回 `unknown control method`：这本身就是「旧构建」的证据，
客户端据此换掉它。

认证：

- Unix socket / 命名管道靠文件权限（当前用户）认证，不需要额外令牌。
- 回环 TCP 回退必须在 `run/control-token`（0600）写入一次性令牌，连接后第一帧
  携带。
- `[禁止]` 控制端点监听非回环地址。

管理命令（install / remove / rollback）在 CLI 进程内完成文件与记录操作，然后
调用 `reload` 通知宿主；`[禁止]` 由宿主代为下载或解包。

---

# 7. 生命周期

## 7.1 期望状态与实际状态分开

| 概念 | 存储 | 含义 |
| --- | --- | --- |
| `enabled` | `db/plugins/<id>.json` | 用户期望：是否允许运行 |
| `approved` | 同上 | 用户确认过的能力集合 |
| `state` | 运行时会话表 | 实际：`stopped` / `starting` / `running` / ... |

`enabled=false` 时任何 `start` 都返回 `conflict`。`enabled=true` 不代表进程一定
在跑：惰性启动下它是 `stopped`，首次调用才转 `starting`。记录里的 `last_state`
只用于宿主不在时的离线展示，读取时必须与会话表合并，不能当作实时状态。

## 7.2 状态

| 状态 | 含义 | 允许的下一步 |
| --- | --- | --- |
| `pending_approval` | 已安装，存在待确认能力，`enabled=false` | `disabled` |
| `disabled` | 未启用（安装后的初始状态，或用户停用），`enabled=false` | `stopped`（enable） |
| `starting` | 进程已拉起，握手未完成 | `running` / `failed` |
| `running` | 握手完成，可调用 | `stopping` / `degraded` / `failed` |
| `degraded` | 部分能力不可用或配置应用失败 | `running` / `stopping` / `failed` |
| `stopping` | 正在优雅退出 | `stopped` / `failed` |
| `stopped` | 已启用但进程不在，可再次启动 | `starting` / `disabled` |
| `failed` | 崩溃超限或握手失败 | `starting`（手动）/ `disabled` |
| `updating` | 版本切换中 | `stopped` / `failed` |

`PluginState::transition(from, to)` 在 `domain::plugin`，返回 `Result`。所有入口
只映射错误，不自行判断「这个状态能不能停」。

## 7.3 迁移表

| 从 | 事件 | 到 | 副作用 |
| --- | --- | --- | --- |
| （无） | 安装完成且无需确认 | `disabled` | 写记录，`enabled=false` |
| （无） | 安装完成且有中高风险能力 | `pending_approval` | 展示待确认 |
| `pending_approval` | 用户确认 | `disabled` | 写 `approved` |
| `disabled` | `enable` | `stopped` | 只改 `enabled=true` |
| `stopped` | 首次调用 / `start` | `starting` | 拉起进程 |
| `starting` | `welcome` | `running` | 建立会话 |
| `starting` | 超时 / `reject` / 退出 | `failed` | 记录原因 |
| `running` | `stop` | `stopping` | 发 `shutdown` |
| `running` | 崩溃 | `starting`（退避）或 `failed` | 计数 |
| `running` | 配置应用失败 | `degraded` | 保留旧配置 |
| `degraded` | 配置修正成功 | `running` | 清错误 |
| `stopping` | 进程退出 | `stopped` | 清会话 |
| `stopping` | 超时 | `failed` | 强杀 |
| `failed` | 手动 `start` | `starting` | 清退避计数 |
| `stopped` / `failed` | `disable` | `disabled` | `enabled=false` |
| 任意 | `update` | `updating` | 先停进程 |
| `updating` | 切换成功 | `stopped` | 更新 `active_version` |
| `updating` | 校验失败 | `failed` | 保留旧版本 |

卸载不是状态迁移：它删除记录与产物，必须先 `disabled` 且进程已退出。

## 7.4 失败恢复

| 失败 | 恢复动作 |
| --- | --- |
| 握手超时 | `plugin logs` 看最后 20 行；检查 entry 是否可执行、平台是否匹配 |
| 能力被拒 | `plugin grant <id> <cap>` 后重启；不要改 manifest 绕过 |
| 配置无效 | `plugin config set` 修正；宿主保留上一版本 |
| 崩溃循环 | `failed` 后看日志与退出码；修复后 `start` 清零计数 |
| 升级失败 | `plugin rollback <id>` 切回上一版本 |

# 8. 能力模型

## 8.1 三方交集

```
granted = manifest.capabilities
        ∩ hello.requested
        ∩ user.approved
```

- `manifest` 是作者声明，安装时展示给用户。
- `hello.requested` 是本次会话实际申请；可以少于 manifest（例如用户没配网络时
  不申请 `net:outbound`）。
- `user.approved` 是用户在安装 / 升级 / `grant` 时确认的集合。

任一为空则该能力不授予。`requested` 超出 manifest 的部分被忽略并写审计，
说明插件在越界申请。`[禁止]` 在运行中扩大 `manifest` 以获得新能力。

## 8.2 能力目录

| 能力 | 解锁的宿主方法 | 风险 | 默认 | 批准 | 阶段 |
| --- | --- | --- | --- | --- | --- |
| `events` | 订阅领域事件 | 低 | 允许 | 安装即得 | M1 |
| `storage:kv` | 自己的键值空间 | 低 | 允许 | 安装即得 | M1 |
| `server:read` | 列服务器、状态、版本、属性 | 低 | 允许 | 安装即得 | M1 |
| `console:read` | 控制台尾部、聊天事件 | 中 | 询问 | 安装确认 | M1 |
| `console:write` | 注入任意命令（等价 OP） | 高 | 拒绝 | 显式 `grant` | M2 |
| `net:outbound` | 出站 HTTP / WS（可经宿主代发） | 中 | 询问 | 安装确认 + 域名白名单 | M2 |
| `secret:rw` | 自己命名空间的凭据 | 中 | 询问 | 安装确认 | M2 |
| `backup` | 创建 / 恢复 / 清理 / 校验 | 高 | 拒绝 | 显式 `grant` | M3 |
| `files:read` | 服务端目录内读 | 中 | 询问 | 安装确认 | M3 |
| `files:write` | 服务端目录内写（禁逃逸） | 高 | 拒绝 | 显式 `grant` | M3 |
| `server:lifecycle` | 启停重启 | 高 | 拒绝 | 显式 `grant` | M3 |
| `content` | 搜索 / 安装 / 更新 / 移除内容 | 中高 | 拒绝 | 显式 `grant` | M3 |
| `java` | 安装 / 卸载托管 JDK | 中 | 拒绝 | 显式 `grant` | M4 |
| `process:spawn` | 拉起子进程 | 极高 | 不开放 | 需要 OS 沙箱 | 不开放 |
| `mc:client` | 以玩家身份连服 | 极高 | 不开放 | 需要账号绑定与沙箱 | 不开放 |

`console:read` 与 `console:write` 必须是两项。合并会让最常见的聊天桥直接拿到
OP，风险分级失效。

## 8.3 风险与角色

映射放在 `protocol::plugin::access`（`protocol` 依赖 `auth`，方向合规）：

| `risk` | 所需角色 | 权限 | 额外要求 |
| --- | --- | --- | --- |
| `read` | Viewer | `Read` / `Logs` | 无 |
| `mutating` | Operator | `Control` / `Write` | 无 |
| `destructive` | Admin | `Manage` | 二次确认（CLI `--yes` 不跳过） |

`destructive` 用 `Manage` 表达「只有 Admin」，而不是新造权限枚举。理由：
`auth::Permission` 是最小集合，新增一项会扩散到远端与 API 的权限表；风险是
插件 op 的属性，不该改变通用 RBAC 词汇。

## 8.4 拒绝与审计

```json
{"v":1,"type":"result","id":7,"ok":false,
 "error":{"code":"denied","category":"unauthorized","retryable":false,
          "message":"console:write 未授予（当前：console:read）"}}
```

三处联动：

| 位置 | 行为 |
| --- | --- |
| 插件 | 降级运行，日志说明缺少哪项；不重试、不假装成功 |
| 审计 | `plugin=chat-bridge capability=console:write result=denied` |
| 入口 | CLI 退出码 6 / Web 显示锁图标与「申请权限」 |

## 8.5 授权生命周期

| 时机 | 行为 |
| --- | --- |
| 安装 | 低风险自动通过；中 / 高风险列成清单一次确认（默认 `y`，拒绝则留 `pending_approval`）；`--yes` 视为同意 |
| 启用 | `approved` 写入记录，宿主据此计算 `granted` |
| 升级 | 新增能力 → `pending_approval`；减少能力直接生效 |
| 撤销 | 立即生效；下一次调用即被拒，不需要重启进程 |
| 禁用 / 卸载 | 授权记录一并清理，不留磁盘 |

升级的能力扩张是供应链攻击最常见形态。用户确认的是「这次授权」，不是
「永远授权」。

## 8.6 不是沙箱

| 授予能拦住 | 授予拦不住 |
| --- | --- |
| 宿主代做的动作：启停、注入命令、恢复备份、读写服务端文件 | 插件自己 `std::fs` 读任意文件、自己开 socket、自己 spawn 进程 |
| 有明确的拒绝与审计记录 | 进程内资源滥用（需要 OS 限额） |

`[禁止]` 在文档、CLI 帮助、市场页面写「sandboxed」「隔离运行」。正确表述是
「能力受控、可审计、安装前需确认」。高风险能力是契约层面唯一能提前劝退的
地方，因此必须逐项显式确认。

---

# 9. HostApi 契约

## 9.1 签名风格

- 全部为同步 trait：宿主在自己的线程池里调用，必要时 `spawn_blocking`。
- 每个方法都接受显式的 `PluginId` 或由实现持有；禁止全局状态。
- 返回 `Result<T, HostError>`；`HostError` 已带 `PluginErrorCode` 与
  `ErrorCategory`，入口只做映射。
- 所有路径参数是相对于服务端目录的相对路径，且必须经统一路径校验。

```rust
// crates/port/src/plugin/api.rs（示意，最终签名以实现为准）
pub struct HostError {
    pub code: PluginErrorCode,
    pub category: ErrorCategory,
    pub message: String,
    pub retryable: bool,
}

pub trait ServerRead: Send + Sync {
    fn list(&self) -> Result<Vec<ServerSummary>, HostError>;
    fn status(&self, server: &ServerName) -> Result<ServerStatusView, HostError>;
    fn version(&self, server: &ServerName) -> Result<Option<String>, HostError>;
    fn properties(&self, server: &ServerName) -> Result<BTreeMap<String, String>, HostError>;
}

pub trait ConsoleIo: Send + Sync {
    /// 游标分页；`cursor=None` 表示从当前尾部开始
    fn tail(&self, server: &ServerName, cursor: Option<u64>, max_lines: usize)
        -> Result<ConsolePage, HostError>;
    fn send_line(&self, server: &ServerName, line: &str) -> Result<(), HostError>;
}

pub trait ServerLifecycle: Send + Sync {
    fn start(&self, server: &ServerName) -> Result<(), HostError>;
    fn stop(&self, server: &ServerName, grace: Duration) -> Result<StopOutcome, HostError>;
    fn restart(&self, server: &ServerName, grace: Duration) -> Result<(), HostError>;
}

pub trait KvStore: Send + Sync {
    fn get(&self, plugin: &PluginId, key: &str) -> Result<Option<Vec<u8>>, HostError>;
    fn set(&self, plugin: &PluginId, key: &str, value: &[u8]) -> Result<(), HostError>;
    fn delete(&self, plugin: &PluginId, key: &str) -> Result<bool, HostError>;
    fn list(&self, plugin: &PluginId, prefix: &str) -> Result<Vec<String>, HostError>;
}

pub trait SecretStore: Send + Sync {
    fn get(&self, plugin: &PluginId, key: &str) -> Result<Option<SecretValue>, HostError>;
    fn set(&self, plugin: &PluginId, key: &str, value: &SecretValue) -> Result<(), HostError>;
    fn delete(&self, plugin: &PluginId, key: &str) -> Result<bool, HostError>;
}

pub trait NetProxy: Send + Sync {
    fn request(&self, plugin: &PluginId, req: NetRequest) -> Result<NetResponse, HostError>;
}

pub trait FileOps: Send + Sync {
    fn read(&self, server: &ServerName, rel: &Path) -> Result<Vec<u8>, HostError>;
    fn write(&self, server: &ServerName, rel: &Path, bytes: &[u8], overwrite: bool)
        -> Result<(), HostError>;
    fn list(&self, server: &ServerName, rel_dir: &Path) -> Result<Vec<FileEntry>, HostError>;
    fn remove(&self, server: &ServerName, rel: &Path) -> Result<bool, HostError>;
}
```

`BackupOps` / `ContentOps` / `JavaOps` / `FileOps` 按同样风格定义，方法集合与
现有 `service` 的公开方法一一对应，不新增业务语义。

生命周期能力的帧方法：

| 能力 | `method` | 输入 | 输出 |
| --- | --- | --- | --- |
| `server:lifecycle` | `start` | `{server}` | `{ok: true}` |
| `server:lifecycle` | `stop` | `{server, grace_secs?}`；默认 30 秒 | `{outcome: "stopped", elapsed_ms}` / `{outcome: "already_stopped"}` |
| `server:lifecycle` | `restart` | `{server, grace_secs?}` | `{ok: true}` |

插件触发的启动不接受 EULA 条款：`eula.txt` 未同意时返回 `conflict`，必须由
管理员在 CLI 显式确认。停机宽限到期仍未退出返回 `timeout`，服务器保持运行。

备份能力的帧方法：

| 能力 | `method` | 输入 | 输出 |
| --- | --- | --- | --- |
| `backup` | `create` | `{server, label?}` | `{snapshot}` |
| `backup` | `list` | `{server}` | `{snapshots:[{id, created_at, added_bytes}]}` |
| `backup` | `restore` | `{server, snapshot}` | `{ok: true}` |
| `backup` | `verify` | `{server, snapshot}` | `{healthy}` |
| `backup` | `prune` | `{server, keep}` | `{removed}` |
| `backup` | `delete` | `{server, snapshot}` | `{deleted}` |

`restore` 只执行原地恢复；服务器运行中返回 `conflict`。快照不存在统一返回
`not_found`，磁盘 / 远端不可用返回 `unavailable`（可重试）。

内容能力的帧方法：

| 能力 | `method` | 输入 | 输出 |
| --- | --- | --- | --- |
| `content` | `list` | `{server, kind?, identify?}`；`kind` 默认 `mod` | `{items:[{file, size, identity?}]}` |
| `content` | `install` | `{server, project, version?, kind?}` | `{file, size, project, version}` |
| `content` | `remove` | `{server, selector, kind?}` | `{removed}` |

`kind` 只接受 `mod` / `plugin` / `resourcepack` / `datapack` / `shader` / `modpack`。
`identify=true` 需要联网反查来源与版本，失败项按「身份未知」返回而不让整次
列表失败。安装沿用服务端条件挑版本，服务器运行中返回 `conflict`，上游不可用
返回 `unavailable`（可重试），该类内容为纯客户端时返回 `invalid_input`。移除的
`selector` 可以是文件名或 modid，未命中返回 `not_found`，命中多个返回
`invalid_input` 并列候选。

文件能力的帧方法：

| 能力 | `method` | 输入 | 输出 |
| --- | --- | --- | --- |
| `files:read` | `read` | `{server, path}` | `{path, size, data}`（`data` 为 base64） |
| `files:read` | `list` | `{server, path?}`；省略 / `""` / `"."` 表示根 | `{entries:[{path, is_dir, size}]}` |
| `files:write` | `write` | `{server, path, data, overwrite?}`；`overwrite` 默认 `false` | `{ok, size}` |
| `files:write` | `remove` | `{server, path}` | `{removed}` |

`write` 覆盖已存在文件时必须显式 `overwrite:true`；目标存在且未允许覆盖返回
`conflict`。`mcsup.json` 与运行中的启动文件（JAR / 参数文件）禁止改写与删除：
前者返回 `denied`，后者返回 `conflict`。

Java 能力的帧方法：

| 能力 | `method` | 输入 | 输出 |
| --- | --- | --- | --- |
| `java` | `list` | `{}` | `{runtimes:[{id, path, major, version, vendor, managed, created_at, updated_at}]}` |
| `java` | `install` | `{major}`；十进制整数或字符串 | `{runtime}` |
| `java` | `remove` | `{path}`；绝对路径，需与已登记项一致 | `{removed}` |

安装固定 `jdk` 包类型，厂商交给来源排序挑最优，与 CLI `java install` 的默认
路径一致；未登记的路径返回 `not_found`，上游 / 下载 / 磁盘失败返回
`unavailable`。大版本不在 8–99 区间返回 `invalid_input`。

## 9.2 限额

| 接口 | 限额 |
| --- | --- |
| `KvStore` 键 | ≤ 256 字节，`[a-zA-Z0-9:_-]` |
| `KvStore` 值 | ≤ 256 KiB |
| `KvStore` 单插件总量 | ≤ 64 MiB |
| `KvStore::list` | 单次 ≤ 1000 个键，必须分页 |
| `SecretStore` 键 | ≤ 128 字节；值 ≤ 8 KiB |
| `ConsoleIo::tail` | `max_lines` ∈ [1, 500] |
| `ConsoleIo::send_line` | ≤ 4096 字节，禁止换行 |
| `FileOps` 读 | ≤ 16 MiB / 文件 |
| `FileOps` 写 | ≤ 16 MiB / 文件，禁止覆盖 `mcsup.json` 与运行中 jar |
| `NetProxy` 响应 | ≤ 8 MiB，重定向 ≤ 3，仅 http/https |

`[禁止]` 把限额实现为「截断后成功」。超限必须返回 `limit_exceeded`，并说明
实际值与上限。

## 9.3 路径与 SSRF

- 路径必须是相对路径；拒绝 `..`、绝对路径、盘符、UNC、符号链接逃逸。
- 解析后必须仍在服务端目录内（`canonicalize` 后前缀比较，注意 Windows 大小写）。
- 文件能力在创建父目录前先校验最深的已存在祖先，避免先写后查；符号链接
  指向目录内部允许，指向目录外一律 `denied`。
- `mcsup.json` 与运行中的启动文件是保留名：写 / 删分别返回 `denied` /
  `conflict`；判定对大小写不敏感，覆盖 Windows 的大小写不敏感文件系统。
- `NetProxy` 默认拒绝回环、私网、链路本地地址与云元数据地址；`manifest.net.allow`
  为空时拒绝所有请求并返回 `denied`。
- `[禁止]` 用字符串包含判断域名；必须解析后按主机名后缀精确匹配。

## 9.4 实现落位

| 实现 | 位置 | 说明 |
| --- | --- | --- |
| `ServerRead` / `ConsoleIo` / `ServerLifecycle` / `BackupOps` / `ContentOps` / `JavaOps` | `service::plugin::HostApiImpl` | 复用现有 service，不复制业务逻辑 |
| `FileOps` | `service::plugin::HostApiImpl` + `domain::path` 的路径校验 | service 内部经 `crate::fs` 入口 |
| `KvStore` / `SecretStore` / `PluginStore` / `AuditSink` | `storage` | 一条记录一个 JSON；审计用追加 JSONL |
| `ArtifactSource` | `provider` | GitHub / url 解析与下载 |
| `ArtifactVerifier` / `PluginSupervisor` | `plugin` | 摘要、进程、端点、帧 |
| `NetProxy` | `provider` | 复用现有 HTTP 栈与代理配置 |

`service` 的 `HostApiImpl` 由门面 `bootstrap` 构造并注入 `plugin::PluginHost`。
`[禁止]` 让 `crates/plugin` 直接 `use service::...`。

---

# 10. manifest

## 10.1 字段

| 字段 | 类型 | 必填 | 约束 |
| --- | --- | --- | --- |
| `schema` | u32 | 是 | `1..=MANIFEST_SCHEMA_VERSION`（当前 1）；宿主接受更旧的产物，新增字段必须能按默认值退化（见 14.3） |
| `id` | string | 是 | `^[a-z][a-z0-9-]{1,63}$`，全局唯一，非保留名 |
| `name` | string | 是 | ≤ 64 字符 |
| `description` | string | 否 | ≤ 512 字符 |
| `version` | string | 是 | SemVer，与产物目录、安装记录一致 |
| `api` | u32 | 是 | 必须是 `PLUGIN_API_VERSION`（1） |
| `entry` | object | 是 | `{type:"process", cmd, platforms?}` |
| `scope` | string | 否 | `machine`（默认）/ `server` |
| `resident` | bool | 否 | 默认 `false` |
| `capabilities` | string[] | 是 | 允许的能力名，去重，≤ 16 项 |
| `net` | object | 否 | `{allow: string[]}`，仅 `net:outbound` 时有效 |
| `subscriptions` | string[] | 否 | 事件类别，≤ 32 项 |
| `provides` | string[] | 否 | 契约标识，必须属于闭集 |
| `consumes` | string[] | 否 | 契约标识，必须属于闭集 |
| `resources` | object | 否 | `{recommended_memory_mb, recommended_cpu_percent}`，仅声明 |
| `publisher` | object | 否 | `{name, url}`，市场审核用 |
| `license` | string | 是 | SPDX 标识 |
| `source` | object | 安装时写入 | 来源、commit、摘要 |

## 10.2 示例

```json
{
  "schema": 1,
  "id": "chat-bridge",
  "name": "Chat Bridge",
  "description": "把游戏内聊天桥接到外部聊天平台",
  "version": "0.3.1",
  "api": 1,
  "entry": {
    "type": "process",
    "cmd": "bin/darwin-arm64/chat-bridge",
    "platforms": {
      "linux-x64": "bin/linux-x64/chat-bridge",
      "win32-x64": "bin/win32-x64/chat-bridge.exe"
    }
  },
  "scope": "machine",
  "resident": true,
  "capabilities": ["events", "storage:kv", "server:read", "console:read",
                   "console:write", "net:outbound", "secret:rw"],
  "net": { "allow": ["api.telegram.org"] },
  "subscriptions": ["chat:message", "server:crashed"],
  "provides": ["chat.publish@1", "chat.ask@1"],
  "consumes": ["incident.report@1"],
  "resources": { "recommended_memory_mb": 128, "recommended_cpu_percent": 10 },
  "publisher": { "name": "Example", "url": "https://example.com" },
  "license": "MIT"
}
```

## 10.3 校验规则

安装时逐条检查，任一失败即拒绝安装并指出字段：

1. `id` 格式、保留名、与已安装记录冲突。
2. `version` 是合法 SemVer，且不是已安装版本（除非 `--force`）。
3. `api` 等于宿主支持的主版本；高于宿主 → `unsupported_api`。
4. `entry.cmd` 是相对路径，无 `..`、无绝对路径、无符号链接逃逸。
5. `entry` 解析到当前平台后文件存在；Unix 校验可执行位，Windows 允许 `.exe`
   （校验 PE 头 `MZ`）与 `.cmd` / `.bat`（宿主经 `cmd.exe /C` 启动）。
6. `capabilities` 每项属于能力目录；未知能力 → 拒绝（不忽略）。
7. `subscriptions` 每项属于事件目录；`console:line` 需要 `console:read`。
8. `subscriptions` 非空时必须有 `events`。
9. `provides` / `consumes` 每项属于第 12 节的闭集，版本后缀合法。
10. `provides` 所需能力必须在 `capabilities` 中（见 12.4）。
11. `net.allow` 非空时必须有 `net:outbound`；条目必须是主机名或后缀。
12. `resources` 只做范围检查（1..=65536 MB），不强制。
13. `license` 是合法 SPDX；市场来源必须有。
14. `scope=server` 时不得 `provides` 需要跨服语义的契约（见 12.4）。
15. 未知顶层字段拒绝（防止旧宿主忽略新语义）。

`[禁止]` 忽略未知能力或未知契约。旧宿主遇到新能力必须拒绝安装，而不是
装上一个永远不可用的插件。

---

# 11. descriptor

## 11.1 结构

descriptor 由 SDK 在构建时生成，与二进制一起打包。宿主只读它，不执行插件
代码就能知道界面与输入输出形状。

校验时机：安装时按 11.2–11.7 完整校验一次；启动插件前再校验一次（同一份
产物不重复解析，按「路径 + 大小 + mtime」缓存结果），不合规在启动这一步就
报可读错误，而不是等每个 op 调用各自失败。运行期的输入校验与超时判定都取自
这同一份已校验结果。

```json
{
  "schema": 3,
  "plugin_id": "chat-bridge",
  "api": 1,
  "config": { "$ref": "#/schemas/config" },
  "setup": {
    "start": true,
    "fields": [
      { "path": "platforms", "required": true, "choices": ["qq", "log"] },
      { "path": "qq.appId", "required": true, "label": "AppID" },
      { "path": "qq.clientSecret", "required": true, "secret": true },
      { "path": "qq.groups" }
    ]
  },
  "bind": { "servers": ["qq.server"] },
  "ops": [
    {
      "id": "history",
      "kind": "query",
      "title": "聊天记录",
      "risk": "read",
      "display": { "type": "chat", "send": "send", "stream": "messages" },
      "input": { "$ref": "#/schemas/history_in" },
      "output": { "$ref": "#/schemas/history_out" },
      "timeout_ms": 10000,
      "pagination": { "cursor_field": "next", "limit_field": "limit",
                      "default_limit": 50, "max_limit": 200 }
    },
    {
      "id": "send",
      "kind": "action",
      "title": "发送消息",
      "risk": "mutating",
      "display": { "type": "form" },
      "input": { "$ref": "#/schemas/send_in" },
      "output": { "$ref": "#/schemas/send_out" }
    },
    {
      "id": "messages",
      "kind": "stream",
      "title": "实时消息",
      "risk": "read",
      "display": { "type": "log" },
      "input": { "type": "object", "properties": {}, "additionalProperties": false },
      "output": { "$ref": "#/schemas/messages_out" }
    }
  ],
  "schemas": {
    "config": { "type": "object", "additionalProperties": false, "properties": {} },
    "history_in": { "type": "object", "additionalProperties": false,
                    "required": ["channel"],
                    "properties": { "channel": { "type": "string" },
                                    "limit": { "type": "integer", "default": 50 },
                                    "cursor": { "type": "string" } } },
    "history_out": { "type": "object", "additionalProperties": false,
                     "required": ["items"],
                     "properties": {
                       "items": { "type": "array", "items": { "$ref": "#/schemas/message" } },
                       "next": { "type": "string" } } },
    "message": { "type": "object", "additionalProperties": false,
                 "required": ["id", "at", "from", "text"],
                 "properties": {
                   "id": { "type": "string" },
                   "at": { "type": "string", "format": "date-time" },
                   "from": { "type": "string" },
                   "text": { "type": "string", "x-mcsup-format": "minecraft-text" } } },
    "send_in": { "type": "object", "additionalProperties": false,
                 "required": ["channel", "text"],
                 "properties": { "channel": { "type": "string" },
                                 "text": { "type": "string", "maxLength": 4000 } } },
    "send_out": { "type": "object", "additionalProperties": false,
                  "required": ["ok"],
                  "properties": { "ok": { "type": "boolean" } } },
    "messages_out": { "type": "object", "additionalProperties": false,
                      "required": ["channel", "from", "text", "at"],
                      "properties": { "channel": { "type": "string" },
                                      "from": { "type": "string" },
                                      "text": { "type": "string" },
                                      "at": { "type": "string", "format": "date-time" } } }
  }
}
```

注意 `schemas` 内部也出现了 `$ref`。规则是：

- descriptor 顶层的 `input` / `output` / `config` 可以是 `{"$ref":"#/schemas/<name>"}`；
- `schemas` 内部的 `$ref` **只允许**指向 `#/schemas/<name>`，且加载时一次性
  内联；
- 内联后若仍有 `$ref`、或出现循环，安装失败；
- 归一化后的 schema 不得包含 `$defs` / `definitions`。

## 11.2 op 字段

| 字段 | 必填 | 约束 |
| --- | --- | --- |
| `id` | 是 | `^[a-z][a-z0-9_]{0,63}$`，同一插件内唯一 |
| `kind` | 是 | `query` / `action` / `stream` |
| `title` | 是 | ≤ 64 字符，界面标题 |
| `description` | 否 | ≤ 512 字符 |
| `risk` | 是 | `read` / `mutating` / `destructive`；作者未声明时按 kind 取缺省：query / stream → `read`，action → `mutating`（有副作用的 op 不得缺省成只读） |
| `display` | 是 | 见 11.3；缺省按 11.4 推断 |
| `input` / `output` | 是 | schema 或 `$ref` |
| `timeout_ms` | 否 | 1000..=300000，默认 30000 |
| `pagination` | 否 | 列表类 query 必填 |

`output` 在 descriptor 里恒为必填，但作者**不必**声明：SDK 未拿到
`.output()` 时落一个空 object（`{"type":"object","additionalProperties":false}`），
并记下「作者没声明」。这种情况下运行期**不校验**返回值——空 object 占位不是
作者的意图，拿它去校验只会把正常返回判成 `internal: … returned an invalid
output`。返回 `undefined` 按 `{}` 回传。作者一旦声明了 `output`，返回值与
分块两侧都照常校验。

## 11.3 六种展示类型

| `display.type` | 渲染 | 适用 | 额外字段 |
| --- | --- | --- | --- |
| `form` | 表单 + 提交 | 有副作用的 action | 无 |
| `table` | 表格 + 分页 | 数组查询 | `columns?` |
| `detail` | 键值面板 | 单对象查询 | `fields?` |
| `chat` | 消息列表 + 输入框 + 实时追加 | 会话 | `send`、`stream`、`text_field?`、`from_field?`、`at_field?` |
| `log` | 追加行 + 自动滚动 | 流式文本 | `text_field?`、`level_field?`、`at_field?` |
| `metrics` | 数值 + 趋势 | 时间序列 | `value_field?`、`at_field?`、`unit?` |

形状要求（安装时静态检查，字段缺失即失败）：

| 类型 | 输出形状 |
| --- | --- |
| `table` | `{items: [object], next?: string}` |
| `chat` | `{items: [{id, at, from, text}], next?: string}`；`send` 必须是已声明的 action，`stream` 必须是已声明的 stream |
| `log` | `{lines: [{at, text}], next?: string}`，或 stream 输出 `{at, text}` |
| `metrics` | `{points: [{at, value}], unit?}`，或 stream 输出 `{at, value}` |
| `detail` | 顶层 object |
| `form` | 顶层 object 输入；输出可为空 object |

字段名缺省值（descriptor 未写时取这些）：

| 字段 | 缺省 |
| --- | --- |
| `text_field` | `text` |
| `from_field` | `from` |
| `at_field` | `at` |
| `level_field` | `level` |
| `value_field` | `value` |

渲染器只按字段名取值，不做语义猜测。字段不存在时显示空值并记录一次 schema
不匹配告警，不 panic、不白屏。

实现落位：形状与引用校验在 `domain::plugin::descriptor`
（`validate_display_shape` / `validate_display_refs`），CLI / Web / MCP 只投影
`ResolvedOp::display`，不各判一套。CLI 的 `plugin ops` 在人类表格列出「展示」
列、在 JSON 里给出 `display` 字段，取值即 [`Display::type_name`]。

## 11.4 默认推断

- `kind=action` 且无 `display` → `form`。
- `kind=query` 且输出含 `items` 数组 → `table`。
- `kind=query` 且输出是 object → `detail`。
- `kind=stream` 且无 `display` → `log`。
- 其它情况必须显式声明，否则安装失败。

推断只影响外壳，内部仍按 schema 渲染。运行时形状不匹配时降级为
`table`（数组）或 `detail`（对象），并在界面标注「按通用视图渲染」。

## 11.5 schema 子集

只允许以下构造。`[禁止]` 超出子集的 schema，安装阶段直接拒绝。

| 允许 | 说明 | 渲染 |
| --- | --- | --- |
| `string` / `number` / `integer` / `boolean` | 基本类型 | 输入框 / 数字框 / 开关 |
| `object` + `properties` + `required` + `additionalProperties:false` | 扁平对象 | 字段组 |
| `array` + `items`（单个 schema） | 列表 | 多值输入 / 表格 |
| `enum`（字符串或整数） | 选项 | 下拉框 / 标签 |
| `default` | 默认值 | 预填 |
| `minimum` / `maximum` / `exclusiveMinimum` / `exclusiveMaximum` | 数值约束 | 即时校验 |
| `minLength` / `maxLength` / `pattern` | 字符串约束 | 即时校验 |
| `minItems` / `maxItems` | 数组约束 | 即时校验 |
| `format: date-time / duration / uri / uuid` | 标准注解 | 对应控件 |

`default` 是**字段级**注解：非必填对象（父级 `required` 未列出）的子字段即使写了
默认值，也不会让该对象自动出现。宿主用「默认值打底」读取配置时，未设置的平台
片段（如没启用 `qq` 时的 `qq`）保持缺席；一旦片段出现（用户写入或声明了
`default`），它的必填子字段就必须齐全。

| 禁止 | 原因 |
| --- | --- |
| `oneOf` / `anyOf` / `allOf` | 组合爆炸：语言 × 方向 × 渲染器 |
| `if` / `then` / `else` / `not` / `patternProperties` / `prefixItems` | 渲染器与校验器都要各写一遍 |
| 外部 `$ref`、递归引用 | 无法保证终止与可缓存 |
| `type:"null"` | 用「字段可选」表达，四语言映射才一致 |
| `transform` / `refine` / 自定义校验器 | 无法用 schema 表达；写在 handler 里 |

限值（安装时强制）：

| 项 | 上限 |
| --- | --- |
| schema 深度 | 6 |
| 单对象属性数 | 64 |
| `enum` 取值数 | 100 |
| schema 节点总数 | 512 |
| 单字符串 `maxLength` | 8192 |
| 单个 schema 序列化字节 | 64 KiB |
| 整个 descriptor | 256 KiB |

超限错误必须指名字段路径（如 `ops.history.output.properties.items`），不能只报
「schema 太大」。

## 11.6 扩展键

JSON Schema 没有「密钥」「单位」的标准表达，统一用 `x-mcsup-` 前缀：

| 键 | 值 | 作用 |
| --- | --- | --- |
| `x-mcsup-secret` | `true` | 输入打码、读取返回 `***`、日志脱敏 |
| `x-mcsup-label` | string | 字段标签（缺省用属性名） |
| `x-mcsup-format` | `plain` / `minecraft-text` / `markdown-sanitized` / `datetime` / `duration` / `bytes` | 文本渲染方式 |
| `x-mcsup-unit` | string | 单位展示（`MiB`、`ms`） |
| `x-mcsup-placeholder` | string | 输入占位提示 |

`x-mcsup-secret` 只允许出现在输入方向：op 的 `input`、`config`。出现在
`output`、流式 chunk、事件载荷或契约返回中，安装阶段直接拒绝
（`secret_on_output`）。原因是输出必然经过 Web、日志、审计三条路径，任何一条
都可能把它变成明文；密钥只能写入，不能回读。

标准 JSON Schema `format`（例如 zod `.datetime()` / `.uuid()` / `.url()` 生成的
`date-time` / `uuid` / `uri`）原样保留，不映射为 `x-mcsup-format`。两处都有的
`duration` 优先按自定义展示格式处理。无法识别的值在构建期报错，不静默丢弃。

`[禁止]` 用 `x-mcsup-format` 表达富文本以外的任意 HTML。`markdown-sanitized`
只允许白名单标签，禁止内联样式与事件属性。

## 11.7 归一化

所有语言 SDK 生成 descriptor 后必须走同一套归一化，否则四份产物无法逐字节
比对：

1. 内联 `$defs` / `definitions` / `#/schemas/*`；
2. 折叠 `anyOf: [X, null]` 为「非必填字段」；
3. 删除空 `title`、空 `description`、`$schema`、`$id`；
4. 对象键按字典序排序，数组保持声明顺序；
5. 强制限值检查，超限失败并指名字段；
6. 输出紧凑 JSON（无多余空格）+ 末尾换行。

## 11.8 安装向导 `setup`

`schema: 2` 起 descriptor 可声明可选 `setup`，宿主在安装 / 升级时据此逐项
询问配置，写完后按 `start` 启用并启动，做到「装完即用」。`schema: 1` 的
旧产物没有该块，宿主仍按旧流程处理。

`schema: 4` 起字段可声明 `when` 条件：只有条件成立时才问这一项，用来表达
「选了哪个平台才问谁的凭据」这类分支，避免问用户用不着的配置。

| 字段 | 必填 | 约束 |
| --- | --- | --- |
| `setup.start` | 否 | 布尔，默认 `true`；配置写完后是否自动启用并启动 |
| `setup.fields` | 否 | 数组，≤ 32 项，顺序即提问顺序 |
| `fields[].path` | 是 | 点号分隔，必须命中 config schema 的**叶子**属性；同一 descriptor 内唯一 |
| `fields[].required` | 否 | 布尔，默认 `false`；必填项留空会重问 |
| `fields[].secret` | 否 | 布尔，默认 `false`；输入遮蔽，字段类型必须是 `string` |
| `fields[].label` | 否 | ≤ 64 字符；省略时取 schema 的 `x-mcsup-label`，再退到 `path` |
| `fields[].description` | 否 | ≤ 512 字符；省略时取 schema 的 `description` |
| `fields[].default` | 否 | 提示默认值，必须符合该字段 schema；省略时取 schema 的 `default` |
| `fields[].choices` | 否 | 数组，≤ 32 项、每项 1–64 字符且不重复；仅用于字符串列表字段，声明后渲染成多选框 |
| `fields[].choicesFrom` | 否 | `servers`：选项由宿主枚举已注册服务器名。字符串字段渲染成单选，字符串列表渲染成多选；与 `choices` 互斥，不能用于 `secret` 或非字符串字段 |
| `fields[].when` | 否 | 提问条件（`schema: 4` 起）：`path` + `includes` 或 `equals` 二选一。`path` 必须是**本字段之前**声明过的 setup 字段；`includes` 判字符串列表包含（被依赖字段必须是字符串列表），`equals` 判值相等（值须符合被依赖字段的 schema）。条件不成立时既不提问也不计入 `missing`；依赖值还没收集到时视为不成立 |

宿主行为：

- 安装 / 升级后先列出待确认能力并**一次确认**（默认授予，`--yes` 视为同意）；
- 安装视为全新安装：**总是**逐项询问全部**条件成立**的 `setup.fields`；因此
  重装也能核对并更新 AppID 等必填项；
- 声明了 `when` 的字段只在条件成立时提问，条件依赖本次已填的答案（不是只看
  旧配置）：例如平台里没选 `qq`，`qq.appId` / `qq.clientSecret` 一个都不问，
  也不会把「缺 QQ 凭据」算进 `missing`；
- 已有值**不预填**到输入框（避免把旧值当成新输入一把回车提交）：留空即沿用，
  字符串列表会预勾选当前值；
- 每个提问只显示 label 与控件，不额外渲染描述或操作提示行；
- 字符串列表声明了 `choices` 时渲染成多选框：`■` / `□` 表示勾选状态，
  空格切换、回车确认；当前值不在 `choices` 里时会作为额外选项追加，不会被
  静默丢弃；
- 声明 `choicesFrom: "servers"` 的字段从已注册服务器中选：字符串字段是单选，
  字符串列表是多选；宿主没有可选项时退回自由输入；
- 必填项没有可沿用的旧值时，回车无法跳过：提示「不能留空」后重新提问，
  直到填写或按 `Ctrl+C` 取消；
- 密钥字段每输入一个字符显示一个 `*`（只暴露长度，不暴露内容）；
- 升级只补仍缺失的字段，用户已填过的值不会被重问或覆盖；
- 字段值通过深度合并写入：只改目标键，同级配置与已存密钥保持不变；
- 写入后 `enable` + `start`，已在运行则 `restart`；
- 用户拒绝授权或必填项未填时，安装仍然成功，但跳过启动并在结果里报告
  `pending` / `missing`；
- `--no-setup` 跳过整个向导（不授权、不补配置、不启动），与旧版行为一致；
- 非交互（`--json` / 非 TTY）不提问：只有 `--yes` 才授予能力，缺失必填项
  以 `missing` 返回，提示用 `mcsup plugin config set` 补齐。

## 11.9 服务器绑定 `bind`

`schema: 3` 起 descriptor 可声明可选 `bind`，把插件关联到一台或多台服务器。
宿主据此让插件跟随服务器启停（`server start / stop / restart`）：

- 启动服务器前先拉起绑定到它的插件，玩家进服时桥已经在收消息；
- 服务器停下后，停止已经没有任何绑定服务器在运行的插件；
- 插件绑定多台服务器时，任意一台在跑就保持运行，最后一台停下才停止。

| 字段 | 必填 | 约束 |
| --- | --- | --- |
| `bind.servers` | 否 | 配置路径数组，≤ 8 项；每项 1–128 字符，点号分隔且分段非空，不得重复 |
| `bind.servers[]` | 是 | 必须命中 config schema 的叶子，且类型是 `string` 或 `string[]` |

路径的值就是服务器名（`string[]` 用于多服务器场景）。运行时从插件当前配置
读取：路径缺失、类型不符或值为空时忽略该路径，结果 trim 后去重并保持有序。
`bind` 本身不做「服务器是否已注册」的判断——注册表属于宿主。

绑定不改变手动操作：`mcsup plugin start / stop` 仍可独立启停插件。插件启动
失败只告警、不阻止服务器启动；服务器启动失败时，本次刚由 `start` 拉起的插件
会被回滚（仅当它已没有绑定服务器在运行）。同一套门禁也用于宿主构建自愈
（§4.7）：换宿主后恢复原先在跑的插件时，绑定服务器都已停止的不恢复。kit、
远端 agent 与插件自身触发的服务器启停不参与该编排——插件宿主是本地进程。

# 12. 契约与组合

## 12.1 两类交互

| 交互 | 语义 | 方向 | 例子 |
| --- | --- | --- | --- |
| **事件广播** | 1:N，无返回值，可丢 | 宿主 → 插件 | 聊天消息、服务器崩溃 |
| **契约调用** | 1:1 或 1:N 请求/响应，有超时 | 插件 → 宿主 → 插件 | 发消息、问 AI、请求重启 |

`[禁止]` 插件直连插件（共享内存、socket、文件、消息队列都不行）。所有跨插件
交互必须经宿主，原因：权限、审计、超时、循环检测、版本协商都只有宿主能做。

## 12.2 闭集

v1 只允许以下六个契约，版本后缀固定为 `@1`：

| 契约 | 提供方 | 多重性 | 提供方所需能力 | 超时 |
| --- | --- | --- | --- | --- |
| `chat.publish@1` | 聊天桥 | 广播 | `console:write` | 10 s |
| `chat.ask@1` | 聊天桥 | 单播（多提供方需 `target`） | `console:read` + `console:write` | 60 s |
| `agent.converse@1` | AI agent | 单播 | `net:outbound` | 120 s |
| `metrics.sample@1` | 监控 | 广播 | `server:read` | 10 s |
| `incident.report@1` | 崩溃助手 / 通知 | 广播 | 无 | 10 s |
| `lifecycle.act@1` | 自愈 / 定时 | 单播（多提供方需 `target`） | `server:lifecycle` | 30 s |

`[禁止]` 新增契约而不改本文与 conformance 用例。`[禁止]` 通用 workflow、
动态方法名、插件自定义事件名。

## 12.3 载荷

### `chat.publish@1`

```json
{ "server": "survival", "text": "服务器将在 5 分钟后重启", "audience": "global" }
```

```json
{ "delivered": true, "message_id": "tg-123" }
```

`audience`：`global`（默认）/ `players` / `{ "players": ["Steve"] }`。

### `chat.ask@1`

```json
{ "server": "survival", "text": "谁在线？", "from": "Steve",
  "match": { "pattern": "^.*$" }, "timeout_ms": 30000 }
```

```json
{ "answers": [ { "from": "Alex", "text": "我在", "at": "2026-11-18T09:31:00Z" } ] }
```

语义：发送问题并等待匹配的聊天回复。超时返回空数组，不报错。`pattern` 是
RE2 子集，禁止回溯引用与环视。

### `agent.converse@1`

```json
{ "conversation": "c-42", "message": "服务器 TPS 低，可能是什么原因？" }
```

```json
{ "conversation": "c-42", "text": "先看实体数量与区块加载……" }
```

流式输出不走契约，走插件自己的 `stream` op（Web 直接消费）。

### `metrics.sample@1`

```json
{ "server": "survival", "names": ["tps", "players", "memory_mb"] }
```

```json
{ "samples": [ { "name": "tps", "value": 19.8, "unit": "tps",
                 "at": "2026-11-18T09:30:00Z" } ] }
```

### `incident.report@1`

```json
{ "severity": "error", "title": "survival 崩溃", "server": "survival",
  "detail": "exit code 1", "evidence": { "last_lines": ["..."] },
  "at": "2026-11-18T09:30:00Z" }
```

```json
{ "accepted": true, "incident_id": "i-7f3a" }
```

### `lifecycle.act@1`

```json
{ "server": "survival", "action": "restart", "reason": "崩溃自愈", "grace_ms": 30000 }
```

```json
{ "ok": true, "outcome": "restarted" }
```

## 12.4 调用规则

| 规则 | 内容 |
| --- | --- |
| 声明 | 调用方必须在 `consumes`，提供方必须在 `provides`，否则安装校验失败 |
| 能力 | 提供方仍受自身能力门约束；契约不是绕过能力的后门 |
| 路由 | 广播：所有提供方；单播：唯一提供方，多个时必须 `target` |
| 目标 | `target` 是 `plugin_id`，只允许在单播契约中使用 |
| 深度 | 调用链 ≤ 4 层 |
| 循环 | 同一 `(plugin_id, contract)` 在调用栈中出现两次即拒绝，返回 `conflict` |
| 扇出 | 单次请求内契约调用总数 ≤ 32；按实际投递的提供方次数计数 |
| 超时 | 每个契约有独立超时；父调用超时同时取消子调用 |
| 审计 | 每次调用记录 caller、provider、契约、结果、耗时 |
| 版本 | 调用方声明版本；提供方不支持即 `not_found`，不做降级猜测 |

根调用由宿主分配 `trace`，提供方嵌套调用原样带回；每层压入
`(caller, contract)`，循环判定与深度判定都基于这条显式调用链，不依赖宿主
线程栈。广播没有提供方时返回空 `responses` 与空 `failures`，不算失败。

广播的聚合结果：

```json
{ "responses": [ { "plugin_id": "chat-bridge", "output": { "delivered": true } } ],
  "failures": [ { "plugin_id": "other-bridge", "error": { "code": "timeout", "category": "backend",
                                                          "retryable": true, "message": "..." } } ] }
```

只要有一个成功，整体 `ok:true`；全部失败才 `ok:false`。`failures` 必须原样
展示给调用方，不允许静默吞掉。

## 12.5 版本演进

- `@1` 只允许新增**可选**输出字段与新增可选输入字段。
- 新增必填输入、改变字段语义、删除字段 → 新版本 `@2`，与 `@1` 并存。
- 宿主按调用方请求的版本路由；提供方可以同时声明 `provides` 两个版本。
- `[禁止]` 在没有提供方支持的情况下把 `@1` 改名为 `@2`。

## 12.6 组合示例

```
监控（metrics.sample 提供方）
  └─ 检测到 TPS 持续低于阈值
       └─ incident.report@1 → 崩溃助手
            ├─ agent.converse@1 → AI agent（分析原因）
            ├─ lifecycle.act@1 → 自愈插件（重启）
            └─ chat.publish@1 → 聊天桥（通告玩家）
```

这条链路里没有任何插件知道另一个插件的存在：全部是契约标识，由宿主解析。
新增一个「Discord 桥」只需提供 `chat.publish@1`，链路自动多一个出口。

---

# 13. Web 与入口投影

## 13.1 一份声明，五个消费者

| 消费者 | 来源 | 插件作者额外工作 |
| --- | --- | --- |
| CLI | `mcsup plugin call <id> <op>` | 0 |
| Web 面板 | descriptor → 六种展示类型 | 0 |
| 其他插件 | `provides` 中的契约 | 声明 `provides` |
| MCP / AI | op 表 → tools | 0 |
| REST 第三方 | `/api/v1/plugins/{id}/ops/{op}` | 0 |

插件作者不写 HTML / CSS / JS，也不写路由。界面是 descriptor 的投影。

## 13.2 渲染映射

| display | Web 组件 | 数据来源 |
| --- | --- | --- |
| `form` | 表单 + 提交按钮 | `input` schema |
| `table` | 表格 + 分页 | `output.items` + `next` |
| `detail` | 键值卡片 | `output` 对象 |
| `chat` | 消息气泡 + 输入框 + 实时追加 | `output.items`、`send`、`stream` |
| `log` | 等宽追加行 + 自动滚动 | `output.lines` 或 stream |
| `metrics` | 数值 + 迷你趋势 | `output.points` |

通用区域（所有插件都有，不需要声明）：启用 / 停用 / 重启、配置表单、日志、
权限与审计、健康状态。

## 13.3 渲染规则

| 规则 | 内容 |
| --- | --- |
| 转义 | 一切按文本渲染；`x-mcsup-format` 只决定格式，不产生 HTML |
| 密钥 | `x-mcsup-secret` 字段读取返回 `***`；写入后不可回显 |
| 风险 | `read` 对 Viewer 可见；`mutating` 需 Operator；`destructive` 需 Admin + 二次确认 |
| 分页 | 列表必须分页；`max_limit` 由 descriptor 给出，服务端再夹一次 |
| 流 | 通过 WebSocket 复用同一 op 表；断线用 cursor 重同步 |
| 背压 | 每会话限速；超出丢弃并提示「请刷新」 |
| 缓存 | `query` 可缓存，按 `plugin_id + op + input` 计算 ETag |
| 降级 | 形状不匹配 → 通用 table / detail；插件崩溃 → `Unavailable` 卡片 |
| 超时 | 按 op 的 `timeout_ms`；超时显示重试，不无限转圈 |

## 13.4 REST / WS 契约（目标）

`apps/server` 尚未落地，以下路径先冻结，避免 Web 端返工：

| 方法 | 路径 | 作用 |
| --- | --- | --- |
| GET | `/api/v1/plugins` | 列表：状态、版本、能力摘要、健康 |
| GET | `/api/v1/plugins/{id}` | 详情：manifest、授权、最近错误 |
| GET | `/api/v1/plugins/{id}/descriptor` | descriptor（可缓存，带 ETag） |
| GET | `/api/v1/plugins/{id}/ops` | op 表（CLI / MCP 同源） |
| POST | `/api/v1/plugins/{id}/ops/{op}` | 调用 query / action |
| GET | `/api/v1/plugins/{id}/ops/{op}/stream` | WS 订阅 stream |
| GET/PUT | `/api/v1/plugins/{id}/config` | 配置读取（密钥打码）/ 写入 |
| POST | `/api/v1/plugins/{id}/enable` | 启用 |
| POST | `/api/v1/plugins/{id}/disable` | 停用 |
| POST | `/api/v1/plugins/{id}/restart` | 重启 |
| POST | `/api/v1/plugins/{id}/grants` | 追加授权 |
| DELETE | `/api/v1/plugins/{id}/grants/{cap}` | 撤销授权 |
| GET | `/api/v1/plugins/{id}/logs` | 日志（游标分页） |

`api` crate 的 DTO 必须从 `domain::plugin` 的类型派生，`[禁止]` 重写一份
「API 版 descriptor」。Web 端只靠 descriptor 与这些路径渲染，不需要与本仓库
联调字段。

HTTP 状态映射：

| `ErrorCategory` | HTTP |
| --- | --- |
| `Usage` | 400 |
| `NotFound` | 404 |
| `Unauthorized` | 403 |
| `Conflict` | 409 |
| `Backend` | 502 / 503（不可用用 503，超时用 504） |
| `Cancelled` | 408（响应体标注 `cancelled`；不使用 nginx 私有的 499） |
| `Failure` | 500 |

## 13.5 明确不做

- 插件上传 HTML / CSS / JS / 字体 / 图片。
- 插件自定义布局 DSL（v1）。
- 插件自开 HTTP 端口供面板嵌入。
- Web 直连插件进程（绕过能力门与审计）。

---

# 14. SDK

> 风格与架构细化见 `docs/sdk-design.md`：单一内核与三层架构（内核 /
> Builder / Hooks）、类型安全支点、多语言策略与禁止项。本节只保留公开面、
> 版本规则与生成管线。

## 14.1 布局

```
sdks/
├── conformance/
│   ├── frames/               握手、调用、流、取消、错误
│   ├── schema/               同一组类型 → 期望 descriptor 片段
│   ├── cases.json            行为用例
│   └── README.md             如何新增用例
├── rust/
│   ├── Cargo.toml            [workspace]，package + lib = mcsup-plugin-sdk
│   ├── src/                  run / 宏 / Ctx / testing
│   └── tests/                conformance 驱动 + 真实进程用例
└── typescript/
    ├── package.json          name = "mcsup-plugin"
    ├── src/                  definePlugin / transport / dispatch / schema
    │   └── contracts/        由 Rust 契约生成，带生成头
    ├── bin/build.ts          descriptor 生成与打包
    └── test/                 vitest + conformance
```

生成物放在 `src/contracts/` 而不是包根的 `contracts/`：`tsc` 的 `rootDir`
限定在 `src`，并且 `dist/*.js` 里的相对导入必须与源码树一致；放到包根会让
`rootDir` 与运行期解析路径同时失配。

## 14.2 公开面

刻意保持小，因为 SDK 主版本必须等于 plugin API 主版本。

Rust：

| 项 | 作用 |
| --- | --- |
| `mcsup_plugin_sdk::run::<T>()` | 连接、握手、帧循环、退出 |
| `#[plugin]` / `#[op]` | 注册元数据，生成 descriptor |
| `Ctx` | 能力客户端：`server` / `console` / `kv` / `secret` / `net` / `log` / `stream` |
| `Config` / `Event` / `Error` / `Exit` | 基础类型 |
| `testing::MockHost` | 内存双工，不开 socket |

TypeScript：

| 项 | 作用 |
| --- | --- |
| `definePlugin({...})` | 注册元数据，入口只需 `export default` 它 |
| `s` | 字段 DSL：`plugins.md` §11.5 子集，内部归约为 zod |
| `z` | 重新导出的 zod（peer），超出子集的逃生口 |
| `run(plugin)` | 连接、握手、帧循环、退出；构建产物自动调用 |
| `PluginContext` | 能力客户端 |
| `PluginError` | 带 `code` / `retryable` 的错误 |
| `testing.createMockHost()` | 内存双工 |
| `utils` | 纯函数工具（文本 / 时间 / 重试 / Minecraft 命令转义 / KV 包装），不含第三方平台客户端 |

Builder 与 Hooks 是次版本可选 API（见 `sdk-design.md` §9），不算主版本承诺的
最小面。`Session` / `ExitCode` / descriptor 校验常量属内部实现，不从包入口导出。

`mcsup-plugin build` 生成固定名 `bin/entry.mjs`，由它导入插件默认导出并调用
`run()`；作者不写启动样板，`run()` 幂等以兼容作者自行调用。

`[禁止]` 在 SDK 里实现业务能力（内置 Telegram 客户端、内置 AI 客户端等）。
SDK 只做协议、类型、生命周期。

## 14.3 版本规则

```
SDK 主版本 == plugin API 主版本
```

- `mcsup-plugin-sdk@1.x` / `mcsup-plugin@1.x` 对应 manifest `api: 1`。
- SDK 的次版本可以新增可选 API；主版本只在 plugin API 升版时变。
- `protocol` 在 0.x 阶段每次发版可能破坏；市场开放前插件帧集必须冻结到
  `PLUGIN_PROTOCOL_VERSION = 1` 的稳定语义。
- 宿主对 `api` 只做相等校验，不做范围兼容。
- manifest 与 descriptor 的 `schema` 是**范围**校验（`1..=当前版本`）：
  只增字段时升 `schema`，旧产物继续装得上。新增字段必须能按默认值退化
  （缺省即「未声明」），做不到的变更只能升 `api` 主版本。
- 未知顶层字段一律拒绝：新字段配旧宿主必须**明确失败**，不许被静默忽略。
  两条合起来的意思是「新宿主认旧产物，旧宿主拒新产物」。
- 收紧 descriptor / manifest 校验规则视同破坏性变更：已装的旧产物会在启动
  或调用时才暴露。规则收紧必须同时接受旧产物（例如按 `schema` 分档），
  否则作为 `api` 主版本变更处理。

## 14.4 生成管线

方向必须分清：

```
生产方向（插件作者）：类型 → descriptor
   Rust schemars / TS zod / Python pydantic / Java victools
        └─ 归一化 → descriptor.json

消费方向（插件作者）：契约 → 类型
   protocol::plugin::contracts
        └─ 生成 → sdks/typescript/src/contracts/v1.ts
                  sdks/python/mcsup_contracts/
                  sdks/java/.../contracts/
```

规则：

- 生成物提交进版本库，带
  `// GENERATED FROM protocol::plugin. DO NOT EDIT.` 头。
- CI 重新生成后与生成器输出逐字节比较（`diff`），漂移即失败。
- `[禁止]` 手写契约类型。
- 生成命令：`npm run gen`（在 `sdks/typescript` 下），内部调用
  `cargo run -q -p mcsup-protocol --example gen-contracts -- typescript src/contracts/v1.ts`。
- 字段名、可选性与标量类型都取自真实 `serde` 序列化结果：`call` 的四条路由与
  `result` 的成败两种形态各取一次采样，字段集合取并集，只出现一次的字段记为
  可省略。`crates/protocol/src/plugin/contracts.rs` 的漂移测试对同一份采样做
  断言，字段改名或增删会先让 `cargo test` 失败。
- 协议版本与限额常量（帧限额、descriptor / schema 限额）同样由该模块生成，
  SDK 侧不手抄第二份：改 `domain::plugin` 的数字会让 `scripts/check-sdk.sh`
  的逐字节比对失败，而不是等装到宿主才发现「构建期放行、安装期被拒」。
- 插件二进制的 `--mcsup-dump-descriptor` 是 descriptor 的唯一来源：
  `mcsup-plugin build` 先构建，再运行该标志取 JSON，最后打包。
- `--mcsup-dump-descriptor` 与 `--mcsup-dump-manifest` 是保留标志，宿主永不传入。

## 14.5 一致性用例

`sdks/conformance/schema/` 是规范本体，文档只是解释。每种语言 SDK 必须对同一
组声明生成逐字节一致的归一化 schema：

| 用例 | 期望 |
| --- | --- |
| 必填字符串 + 长度上限 | `{"type":"string","maxLength":N}` |
| 非必填字段 | 不在 `required`，且不出现 `null` 类型 |
| 字符串枚举 | `{"type":"string","enum":[...]}` |
| 对象数组 | `{"type":"array","items":{"type":"object",...}}` |
| 默认值 | `default` 保留 |
| 密钥标记 | `x-mcsup-secret: true` |
| 文本格式 | `x-mcsup-format: "minecraft-text"` |
| 嵌套对象 | 深度 ≤ 6，键排序 |
| 禁止：union | 构建失败，错误指名字段 |
| 禁止：递归 | 构建失败 |
| 禁止：外部 `$ref` | 构建失败 |
| 禁止：超限 | 构建失败，报实际值与上限 |

行为用例（`cases.json`）覆盖：握手成功 / 拒绝、越界能力、未知帧、超长帧、
超时、取消、流式缺口、心跳丢失、配置回滚、重入调用。

## 14.6 门禁

`scripts/check-sdk.sh` 由 `scripts/check.sh` 的 `sdk` 步骤调用（§4.2），CI 上
再单独执行一次，避免本地未装 Node / Python 时漏检。

| 步骤 | 命令 | 失败含义 |
| --- | --- | --- |
| Rust | `cargo test --workspace`（`sdks/rust`） | 契约或帧循环错误 |
| TS 安装 | `npm ci` | 锁文件与 package.json 不一致 |
| 生成一致 | `npm run gen` 后与生成器输出逐字节比较（`diff`） | 契约漂移：生成物未随 `protocol::plugin::contracts` 更新 |
| TS 类型 | `tsc --noEmit` | 类型错误 |
| TS 单测 | `vitest run` | 行为错误 |
| conformance | `npm run conformance` | 与其它 SDK 偏离 |
| 打包 | `npm run build` | 产物不可发布 |

## 14.7 语言优先级

| 语言 | 阶段 | 说明 |
| --- | --- | --- |
| Rust | M1 | 参考实现，与宿主共享 `protocol` 类型，零漂移 |
| TypeScript | M1 | Web 生态与聊天桥 / AI agent 的主要语言 |
| Python | M2 | pydantic 归一化：`$defs` 内联、`Optional` 折叠 |
| Java | M3 | victools；命名管道不可用，SDK 内部回退 TCP |
| Go | M4 | 只提供 builder，不承诺自动推导 |

---

# 15. 安装与分发

## 15.1 来源

只支持三种来源，`[禁止]` npm / pip / cargo 安装插件：

| 来源 | 命令 | 解析方式 |
| --- | --- | --- |
| 本地产物 | `--from ./x.zip` | 直接读文件 |
| URL | `--url https://.../x.zip` | 下载；`--sha256` 或同名 `.sha256` |
| GitHub 仓库 | `install https://github.com/<owner>/<repo>` | 解析引用到 commit，再下载该 commit 的仓库归档 |

GitHub 归档解析步骤（不执行任何代码）：

1. 把 `<owner>/<repo>` 与可选引用（`--ref`，或链接里的 `/tree/<ref>`）解析成
   一个不可变的 commit SHA。引用本身就是完整 SHA 时跳过这一步。
2. 下载 `https://codeload.github.com/<owner>/<repo>/zip/<SHA>`。
3. 解包（归档顶层是 `<repo>-<SHA>/`，安装器会自动下钻一层）。
4. 校验 `manifest.json` / `descriptor.json`，登记。

安装**不构建**：仓库内容必须是成品产物，`manifest.json` 必须在仓库根或唯一一层
子目录里。作者把产物提交进默认分支时，用户装裸链接；把产物提交进独立的
`dist` 分支时，用户装 `https://github.com/<owner>/<repo>/tree/dist`。

解析引用优先问 `api.github.com`（未鉴权 60 次/小时）；不可达或被限流时回退到
`github.com/<owner>/<repo>/commits/<ref>.atom`（同主机、无速率限制）。两条都
失败才报 `unavailable`。私有仓库两条路都拿不到内容，请改用 `--from` 安装本地
构建。

`--ref` 只配 GitHub 链接：`[禁止]` 与 `--url` / `--from` 一起用。引用可以是
分支、tag 或 commit；缺省即默认分支。想钉死版本请显式 `--ref <tag|sha>`，
因为 `update` 会按记录的引用重新解析最新提交。

`--sha256` 比对的是**归档字节**的摘要，而 GitHub 重新打包同一 commit 时字节
可能变化（压缩实现、时间戳），所以它主要用于 `--url`。要固定内容请改用
commit（`--ref <sha>`）或 `--from`。

## 15.2 产物

```
chat-bridge-0.3.1/
├── manifest.json
├── descriptor.json
├── schemas/                 # 可选；descriptor 也可内联
├── bin/
│   ├── darwin-arm64/chat-bridge
│   ├── linux-x64/chat-bridge
│   └── win32-x64/chat-bridge.exe
├── dist/                    # Node / TS 插件：编译产物
├── node_modules/            # Node / TS 插件：运行时依赖，必须自备
├── LICENSE
└── README.md                # 可选
```

**产物必须自带运行时依赖。** 插件装到 `{data_dir}/mcsup/plugins/<id>/<version>/`
之后，作者的工程目录不在旁边，`node_modules` 解析不到任何东西：进程会以
`ERR_MODULE_NOT_FOUND` 直接退出，宿主只看到握手超时。TS SDK 的
`mcsup-plugin build` 因此把生产依赖（含 `mcsup-plugin` 自身与 `zod` 这类
peer）整棵 vendor 进 `<out>/node_modules`（`sdks/typescript` 的
`vendorRuntimeDependencies`），而不只写 `package.json`。

**产物里不得有符号链接。** 安装与 `plugin verify` 逐文件检查，遇到符号链接
直接拒绝（`symbolic link in artifact: …`）：链接可以指到产物外，等于绕过
「不可变产物目录」这层约束。npm 装出来的 `node_modules/.bin/*` 与
`file:` 依赖都是链接，所以 vendor 时必须展开成实体文件。依赖包自己的
`node_modules` 也不照抄：那是它装过的依赖，由 SDK 按 `package.json` 重新解析
并铺到产物顶层，照抄会把整棵开发依赖（`typescript`、`vitest`）塞进产物。

**GitHub 归档会原样保留仓库里的符号链接。** 也就是说仓库里提交了符号链接
（常见于 `node_modules/.bin/*`、`file:` 依赖、指向仓库外的相对链接）时，装出来
的归档会被 `symbolic link in artifact` 拒绝。发布前请用 `mcsup-plugin build`
产出实体文件，不要把 `node_modules` 直接提交进仓库。

压缩格式：zip（默认）或 tar.zst。限制：

| 项 | 上限 |
| --- | --- |
| 压缩包 | 128 MiB |
| 解包后 | 512 MiB |
| 文件数 | 5000 |
| 单文件 | 256 MiB |
| 路径深度 | 8 |

解包必须防 Zip Slip：拒绝绝对路径、`..`、符号链接、硬链接、设备文件。
Windows 还需拒绝保留名（`CON`、`NUL` 等）与尾随点 / 空格。

GitHub 归档额外多一层 `<repo>-<SHA>/` 顶层目录；128 MiB 的压缩包上限也适用，
产物里塞了大体积资源时请改用 `--from` / `--url` 分发。

## 15.3 签名（暂不实现）

当前不做签名校验：任何人都可以构建并发布插件，安装只校验**摘要**与**包
安全**。这是明确的产品决定，不是遗漏。

| 场景 | 当前行为 |
| --- | --- |
| 市场来源 | 与自建源、本地来源相同：只校验摘要与包安全 |
| 自建源 | 同上；`plugins/trusted-keys/` 目录不再创建 |
| 本地 `--from` | 直接安装，无需额外开关 |
| 远端 URL | 只校验摘要（`--sha256` 或同名 `.sha256`），不要求签名 |
| 签名文件 | 忽略：`.minisig` / `.sigstore` 不影响安装结果 |

保留的校验：

- 压缩包上限、解包安全（Zip Slip / 符号链接 / 大小与数量上限）、
  manifest 与 descriptor 校验。
- 摘要：安装记录保存解包目录摘要；`plugin verify` 重算并比对。摘要能发现
  下载损坏与传输截断，但不提供身份保证——它防不了「有人发布一个恶意插件」，
  只能保证「装到的东西与校验过的东西一致」。

已移除：minisign / ed25519 验签、信任库（`plugins/trusted-keys/`）、
`--allow-unsigned` / `--dev` 开关、安装记录里的 `signed` / `trusted` 字段、
`plugin verify` 的签名结论。

将来若接入签名，计划是市场来源用 Sigstore（cosign）绑定仓库与 commit、
自建源用 minisign；届时需要先解决证书链解析与信任根。当前用**能力授权 +
撤销列表 + 审计**承担运行时风险，而不是产物来源。

## 15.4 安装流水线

```
解析来源 → 下载 → 校验摘要 → 解包到临时目录
→ 校验 manifest → 校验 descriptor（含 setup 解析）→ 计算能力清单
→ 原子移动到 plugins/<id>/<version>/
→ 写安装记录 → 状态 disabled / pending_approval
→ （交互）列出能力并确认授权 → 逐项填写 setup → enable + start
```

关键约束：

- 临时目录与目标同一文件系统，移动是原子的。临时目录名带 pid 与自增序号
  （`.install-<tag>-<pid>-<secs>-<seq>`），崩溃残留按名字里的创建时刻超过
  一小时后在下一次安装时清理。
- 安装期间不启动插件进程。
- 任一步失败清理临时目录，已安装版本不受影响。
- 同一 `id` 的安装串行化（`plugins/.<id>.install.lock`，内核文件锁），避免并发
  写坏记录。抢锁在 manifest / descriptor 校验之后——那些步骤只读，不必串行。
- 待确认能力与 `enabled` 互斥：`pending` 非空时记录强制 `enabled=false`，升级
  不会带着未审批的能力去启动（`pending_approval` 只能迁到 `disabled`）。
- 已运行旧版本时，升级先停旧进程，等新版本启动（含握手）成功后由服务层重新
  拉起；失败自动切回旧版本并重新启动。CLI 的 `plugin install` / `plugin update`
  因此不会再多重启一次。

## 15.5 授权展示

安装完成后、启动之前，宿主机集中展示待确认能力并要求一次确认：

```
插件申请以下权限：
  - events          接收服务器事件
  - console:read    读取控制台输出
  - console:write   向控制台执行命令
  - server:read     读取服务器信息
授予以上权限？[Y/n]
```

- 默认 `Y`；输入 `n` 则保留待授权状态并跳过启动，之后可用
  `mcsup plugin grant <id> <capability>` 逐项补齐。
- `--yes`（脚本模式）视为同意；非交互且未给 `--yes` 时不静默授权。
- 低风险能力在安装阶段已自动通过，不会出现在清单里。
- 随后按 descriptor 的 `setup.fields` 逐项询问配置；安装时询问全部字段，
  升级只补仍缺失的项。
- 声明了 `when` 条件的字段只在条件成立时提问（`schema: 4` 起）：条件看本次
  已填的答案，不成立的字段既不问也不计入 `missing`。例如平台里没选 `qq`，
  就不会问 `qq.appId` / `qq.clientSecret`。
- 已有值**不预填**到输入框：自由输入字段留空即沿用，界面不另加提示行。
  避免一个回车把旧值当成新输入提交。
- 每个提问只显示 label 与控件；descriptor 的 `description` 不再渲染成帮助行。
- 字符串列表声明了 `choices` 时渲染成多选框：`■` / `□` 表示勾选状态，
  空格切换、回车确认，并预勾选当前值。
- 声明 `choicesFrom: "servers"` 的字段从已注册服务器中选（字符串 → 单选，
  字符串列表 → 多选）；没有任何服务器时退回自由输入。
- 必填项没有旧值可沿用时不能回车跳过：提示后重新提问，直到填写或取消。
- 布尔字段用「是 / 否」选择器，初始光标由当前值 / schema 默认值决定；
  密钥字段每输入一个字符显示一个 `*`（只暴露长度），内容不写进审计日志。
- 已安装插件要重跑向导时用 `mcsup plugin setup <id>`：不必卸载重装，同样
  逐项询问（现有值留空即沿用），沿用原启用状态，只在原本运行中时重启一次。

## 15.6 升级、回滚、卸载

| 操作 | 规则 |
| --- | --- |
| 升级 | 新版本并存；切换 `active_version` 后重启；保留上一版本用于回滚 |
| 回滚 | `plugin rollback <id>` 切回上一版本并重启；不重新确认未变化的授权 |
| 降级 | 允许，但若目标版本能力更少，直接生效；更多则 `pending_approval` |
| 卸载 | 必须先停用且进程退出：启用中的卸载由 CLI 先 `disable` 再卸（一遍操作，不用手动两步）；默认连 `plugin-data/<id>/`（配置、密钥、键值）与日志一起删 |
| 保留数据 | `--keep-data` 保留 `plugin-data/<id>/` 与日志，只删产物与记录；其中 `secrets.json` 仍默认删除——凭据不随同名插件重装继承 |
| 保留密钥 | `--keep-secrets` 保留密钥（隐含 `--keep-data`），用于重装后继续使用同一凭据 |
| 确认 | 交互下问一次「是否禁用并移除 <id>？」（已停用则「是否移除 <id>？」），默认答「否」；无法弹出确认框（管道 / JSON）时必须显式 `--yes`，不因「没人回答」而当同意 |
| 版本保留 | 始终保留当前版本与回滚目标（`previous_version`）；`--prune` 只清理更旧的目录 |

升级失败（校验、启动、握手、配置）必须自动回滚到旧版本，并把失败原因写入
记录。`[禁止]` 让用户在「旧版本已删、新版本起不来」的状态里手工恢复。

## 15.7 离线

- 已安装插件在无网络时正常工作。
- `install --from` 与 `install --url` 的本地缓存可复用。
- 远程机器安装时，产物在本机校验后再分发；`[禁止]` 信任远端已校验的结论。

# 16. 安全与市场审核

## 16.1 威胁模型

| 威胁 | 现有措施 | 残余风险 |
| --- | --- | --- |
| 插件执行任意命令 | 能力门只拦「宿主代做」；插件自身进程可做任何 OS 允许的事 | 需要 OS 级沙箱才能消除；文案不得承诺 |
| 供应链投毒 | 固定 commit、摘要、审核 | 作者账号被盗；靠撤销与举报缓解（当前不做产物签名） |
| 能力扩张 | 升级重新确认授权 | 用户习惯性点确认 |
| 密钥泄露 | 只写不读、日志脱敏、文件权限 0600 | 插件自身可读明文（它需要用） |
| SSRF | `NetProxy` 默认拒绝私网与元数据地址 | 插件自己开 socket 不受限 |
| XSS | 插件不能提供 HTML；一切文本转义 | `markdown-sanitized` 的白名单实现漏洞 |
| 拒绝服务 | 帧 / 队列 / 日志限额 | 插件可耗尽 CPU / 内存 |
| 路径逃逸 | 统一路径校验 + 符号链接拒绝 | 插件自身文件操作不受限 |
| 审计缺失 | 能力调用与契约调用全记录 | 插件内部行为不可见 |

## 16.2 审核清单

自动检查（安装 / 上架时）：

1. manifest 与 descriptor 全部通过第 10、11 节校验。
2. 能力与实现的一致性：声明 `console:write` 但没有任何写控制台的 op 或契约，
   标记「能力冗余」要求解释。
3. 契约闭集与能力匹配（12.2）。
4. 产物无符号链接、无超出平台目录的路径、无可执行脚本文件（`.sh` / `.ps1`）。
5. 摘要与安装记录一致。
6. 二进制平台覆盖与 manifest 一致。
7. descriptor 中不得出现 `x-mcsup-*` 以外的扩展键（未知扩展忽略但记录）。
8. 包内不得包含 `.git` 与构建缓存；`node_modules` 只允许 `mcsup-plugin build`
   vendor 出来的实体文件（符号链接一律拒绝，见 §15.2）。

人工审核（市场来源）：

| 项 | 关注点 |
| --- | --- |
| 目的与能力匹配 | 聊天桥不应该要 `server:lifecycle` |
| 出站域名 | `net.allow` 是否是最小集合 |
| 密钥使用 | 是否只存必要凭据；是否写入日志 |
| 破坏性操作 | 是否有二次确认与可回滚路径 |
| 文档 | 是否说明失败行为与权限用途 |
| 许可证 | 是否允许分发 |

## 16.3 密钥处理

- 存储在 `plugin-data/<id>/secrets.json`，文件权限 0600，内容用机器密钥加密：
  AES-256-GCM，一条数据一个随机 nonce，插件 id 作为 AAD（密文复制到别的插件
  目录解不开）。文件是 `v` / `nonce` / `data` 信封；旧版明文文件仍可读，下次
  写入时迁移为密文。
- 机器密钥由 `auth` 的凭据设施提供，只存系统钥匙串，不在磁盘上再落一份：
  密钥与密文同目录时，拿到目录副本的人同时拿到密钥，加密等于没有。
- 钥匙串不可用时退化为 `0600` 明文，并由 `mcsup plugin doctor` 明确报告；
  已有密文在密钥缺失时读取报 `denied`，不静默当作不存在。
- 读取接口永不返回明文给入口；插件在 `welcome.config` 中收到明文（它需要使用）。
- 日志采集对已知密钥值做精确替换：宿主在拉起进程前把该插件的密钥值并入
  打码表（只收 UTF-8、长度 ≥ 8 字节、最多 64 条）。过短的值会在日志里命中
  无关文本，替换后日志不可读，反而掩盖问题；`[禁止]` 把密钥写入审计、错误
  消息、URL。
- 卸载默认删除密钥；`--keep-data` 保留配置与键值（重装后接着用），
  `--keep-secrets` 连密钥一起留。

## 16.4 撤销与举报

| 机制 | 行为 |
| --- | --- |
| 撤销列表 | 宿主定期拉取；命中的版本拒绝启动并提示升级。当前实现由 `mcsup plugin revocations import` / `fetch` 写入 `plugins/revoked.json`，尚无内置定时拉取 |
| 本地拉黑 | `plugin block <id>` 立即停用且不再自动启动 |
| 举报 | CLI / Web 生成带诊断包的举报；不包含密钥与聊天内容 |
| 隔离 | `plugin quarantine <id>` 保留产物但禁止运行，用于取证 |

撤销列表不可用时不得阻断已安装插件，只记录「无法检查撤销状态」。

---

# 17. 数据与目录布局

## 17.1 平台数据目录

沿用 `docs/workspace.md` 的 `{data_dir}/mcsup/`：

```
{data_dir}/mcsup/
├── config.toml
├── db/
│   ├── plugins/
│   │   ├── state.json                 # 全局开关、当前工作区无关的指针
│   │   └── <id>.json                  # 一条安装记录
│   └── audit/
│       └── plugins/
│           └── 2026-11.jsonl          # 追加写，按月轮转
├── plugins/
│   ├── <id>/
│   │   ├── 0.3.0/                     # 不可变产物
│   │   └── 0.3.1/
├── plugin-data/
│   └── <id>/
│       ├── kv/                        # 键值，一键一文件
│       ├── config.json                # 配置（0600，可能含密钥字段）
│       └── secrets.json               # 加密凭据
├── logs/
│   ├── plugin-host.log
│   └── plugins/<id>.log
└── run/
    ├── plugin-host.lock
    └── plugin-host.sock
```

规则：

- `plugins/<id>/<version>/` 安装后不可变；升级新增目录，不原地覆盖。
- 当前版本记录在 `db/plugins/<id>.json` 的 `active_version`，不另设指针文件，
  避免双真相源。
- `plugin-data/<id>/` 是插件唯一可写目录；宿主只在卸载时清理——默认连同
  目录与日志一起删，`--keep-data` 保留。例外是 `secrets.json`：保留目录时
  仍默认删除，`--keep-secrets` 才留。
- `plugin-data/<id>/config.json` 与 `secrets.json` 同为 `0600`：配置可能含
  `x-mcsup-secret` 字段，读取接口必须打码，卸载删目录时随目录删除。
- 审计用 JSONL 而不是「一条记录一个 JSON」：记录不可变、写入高频、按时间
  追加；这是对既有存储约定的有意例外，必须在实现中注释原因。
- 所有写入先写临时文件再原子重命名；跨进程写用 `db/.lock`。

## 17.2 安装记录

```json
{
  "schema": 1,
  "id": "chat-bridge",
  "active_version": "0.3.1",
  "enabled": true,
  "blocked": false,
  "quarantined": false,
  "approved": ["events", "storage:kv", "server:read", "console:read"],
  "last_state": "running",
  "installed_at": "2026-11-18T09:00:00Z",
  "updated_at": "2026-11-18T09:30:00Z",
  "source": {
    "type": "git",
    "url": "https://github.com/example/chat-bridge",
    "commit": "3f9c2a1d4e5f60718293a4b5c6d7e8f901234567",
    "artifact_sha256": "9f2c..."
  },
  "versions": ["0.3.0", "0.3.1"],
  "last_error": null
}
```

`last_state` 是最近一次已知实际状态；进程实时状态以宿主会话表为准，读取时合并。
`last_error` 记录最近一次失败的结构化原因（code + message + at）。

`blocked` / `quarantined` 是只对本机生效的安全标记：写入时立即停止进程并禁用，
读取时优先于 `enabled`；解除后不自动启用，由用户显式 `enable`。

## 17.3 锁

| 锁 | 粒度 | 持有者 |
| --- | --- | --- |
| `run/plugin-host.lock` | 机器 | 宿主进程 |
| `db/.lock` | 写事务 | 任意写记录的进程 |
| `plugins/.<id>.install.lock` | 单插件 | 安装 / 升级 / 卸载 |

锁由内核持有（`flock` / Windows 文件锁），进程退出自动释放。`[禁止]`
用「文件存在即加锁」的实现，崩溃后会留下永久锁。

锁文件落在插件 id 目录**之外**（`plugins/.<id>.install.lock`）并且永不删除：
放在 `plugins/<id>/` 里会被 `plugin remove` 连同产物一起删掉，而删锁文件会让
等待者在旧 inode 上继续等、新来者在新 inode 上立刻拿到锁，互斥当场失效。

---

# 18. CLI 契约

## 18.1 命令

```
mcsup plugin list [--json]
mcsup plugin install <source> [--from|--url] [--ref <tag|sha|branch>] [--sha256 <hex>]
                              [--yes] [--no-setup] [--json]
mcsup plugin update <id> [--to <version>] [--prune] [--yes] [--no-setup]
mcsup plugin setup <id>                  # 已安装插件重跑安装向导（沿用现有值）
mcsup plugin remove <id> [--keep-data [--keep-secrets]] [--yes]
mcsup plugin enable <id>
mcsup plugin disable <id> [--yes]
mcsup plugin start <id>
mcsup plugin stop <id> [--grace <secs>]
mcsup plugin restart <id>
mcsup plugin status <id> [--json]
mcsup plugin ops <id> [--json]
mcsup plugin call <id> <op> [--input <json|@file|->] [--json] [--stream]
mcsup plugin config get <id> [key] [--json]
mcsup plugin config set <id> <key> [value] [--secret | --file <path>]
mcsup plugin grant <id> <capability>
mcsup plugin revoke <id> <capability>
mcsup plugin block <id>
mcsup plugin unblock <id>
mcsup plugin quarantine <id>
mcsup plugin release <id>
mcsup plugin logs <id> [--follow] [--tail <n>]
mcsup plugin verify <path|id>            # 重算摘要并复查 manifest / descriptor
mcsup plugin rollback <id>
mcsup plugin revocations [import <file>|fetch <url> [--sha256 <hex>]]
mcsup plugin doctor [--json]
mcsup __plugin-host --data-dir <path> [--idle-timeout <secs>]   # 隐藏
```

约定：

- `install` 的 `<source>` 可省略，由 `--from` / `--url` 指定；三者互斥。
- `call` 的 `--input` 缺省为 `{}`；`-` 表示从 stdin 读 JSON。
- `--output json` 时 stdout 只含一行 JSON；进度与提示走 stderr。
- `--yes` 跳过确认式提问，不跳过必填参数补参；安装向导里视为同意能力授权。
- `plugin remove` 默认连配置与数据一起删；交互下先问一次（默认「否」），无法
  弹出确认框（管道 / `--output json`）时必须显式 `--yes`，否则退出码 2——
  删除不可恢复，不能因为「没人回答」就当同意。
- `install` / `update` 完成时不解释「绑定的服务器都没在运行」：绑定插件的生命
  周期归服务器，装完不启动、等 `server start` 拉起是常态；显式 `plugin start`
  / `restart` 仍会说明为什么没起来。
- `--no-setup` 跳过安装向导：不授权、不补配置、不自动启动（脚本与旧版行为）。
- `plugin stop --grace` 上限 3600 秒（与 `server stop` 共用领域常量）：超限按用法
  错误拒绝，不静默夹紧——夹紧会让用户以为 `--grace 600` 真的生效了。
- `plugin update` 不带 `--to` 时按记录里的来源重新拉取，产物 id 必须仍是同一个；
  本地来源在安装时已规范成绝对路径，`update` 不依赖当前工作目录。GitHub 来源
  记录的是**引用**（分支 / tag / commit）而不是解析出来的 commit，所以裸链接
  装上的插件 `update` 会跟到默认分支的最新提交，`.../tree/dist` 装上的会跟到
  `dist` 分支最新提交；想钉死版本用 `--ref <tag|sha>`。
- 非交互缺必填参数：退出码 2，stderr 说明缺什么；配置必填项缺失时以
  `missing` 字段返回并在下一轮 `config set` 后补上。
- `config set` / `config get` 的 `<key>` 支持点号路径（`qq.appId`）：写入
  按深度合并只改目标叶子，读取定位到对应值；单段键仍按顶层写入。
  同组必填字段（如 `qq.appId` 与 `qq.clientSecret`）必须**一次写完**：单写
  一个会因缺另一个被整份校验拒绝。点号路径适合在必填项已齐后修单个值。
- `config get` 返回「schema 默认值打底 + 已存值」的视图：可选对象片段（父级
  `required` 未列出）不会因子字段有默认值而出现，没启用的平台片段保持缺席；
  写入校验同样以这份视图为底，所以没填凭据的平台不会阻塞其它配置。
- `config set --file <path>` 从文件读 JSON 值（自动剥掉 UTF-8 BOM），与位置
  参数、`--secret` 互斥。数组与对象在 Windows PowerShell 5.1 下送不到程序里
  （`["log"]` 会被拆成 `[log]`，`--%`、`cmd /c`、`Start-Process` 都不行），
  写文件再传是最稳的：
  `mcsup plugin config set chat-bridge platforms --file platforms.json`。
  文件不是合法 JSON 时以用法错误 2 退出，不写配置。
- `server start` / `stop` / `restart` 会顺带编排绑定到该服务器的插件
  （descriptor `bind.servers`）：启动前先起插件，停机后停止已无绑定服务器在
  跑的插件。插件启停失败只告警，不影响服务器本身；kit、远端 agent 与插件
  自触发的服务器启停不参与该编排。
- 控制台 `stop`、游戏内 `/stop`、崩溃退出都不经过 CLI，因此常驻
  `__plugin-host` 每秒检查一次绑定服务器的实时状态：绑定集合全部停下（连续
  两轮确认，且此前确实见过其中一台在跑）后由宿主自行停止插件，效果与
  `server stop` 的编排一致。插件刚拉起、服务器还没进入 Starting 的启动窗口
  不算停机。
- 绑定插件的生命周期归服务器，手动命令不能绕开：绑定的服务器全部处于停止
  状态时，`plugin install` / `update` / `setup` / `start` / `restart` 不会把
  插件拉起来，还会顺手收掉残留的运行进程。被挡下时插件仍保持「已启用」但
  进程不跑，等服务器启动时再自动拉起。没有绑定、绑定服务器至少一台在跑，或
  读不出绑定清单（descriptor / 配置损坏）时放行；读不出时放行是为了不让诊断
  失败挡住用户。
- 「运行 `mcsup server start <name>` 会自动拉起」这句只在显式 `plugin start`
  / `plugin restart` / `plugin setup` 被挡下时提示：那是用户主动要它跑。安装
  与升级不解释——装完不启动是绑定插件的常态，说一遍反而像出了问题。
- 本机状态先于绑定门禁：插件被拉黑、隔离、停用或还有待授权能力时，
  `plugin start` / `restart` 直接报出真实原因与应执行的命令
  （`unblock` / `release` / `enable` / `setup`），不再被「绑定的服务器都没
  在运行」盖成一次 exit 0 的「已跳过启动」。

## 18.2 输出示例

```
$ mcsup plugin list
插件          版本    状态     能力                               RSS
chat-bridge   0.3.1   running  events, storage:kv, server:read   38 MB
watchdog      0.1.0   stopped  events, server:read                 -
```

```
$ mcsup plugin call watchdog health --json
{"servers":3,"running":1,"uptime_secs":412}
```

```
$ mcsup plugin install https://github.com/example/chat-bridge
确认安装插件（来源：https://github.com/example/chat-bridge）？[Y/n] y
正在安装插件…
插件申请以下权限：
  - events          接收服务器事件
  - console:read    读取控制台输出
  - console:write   向控制台执行命令
  - server:read     读取服务器信息
授予以上权限？[Y/n] y
AppID: 1906491720
ClientSecret: ••••••••
群 openid（留空表示不限制群）:
已安装并启动 chat-bridge 0.3.1
```

## 18.3 退出码

沿用 CLI 既有机器契约：

| `ErrorCategory` | 退出码 |
| --- | --- |
| `Failure` | 1 |
| `Usage` | 2 |
| `NotFound` | 3 |
| `Conflict` | 4 |
| `Backend` | 5 |
| `Unauthorized` | 6 |
| `Cancelled` | 130 |

插件特有场景：

| 场景 | 分类 | 退出码 |
| --- | --- | --- |
| 插件不存在 | `NotFound` | 3 |
| 能力未授予 | `Unauthorized` | 6 |
| 插件未运行 / 崩溃 | `Backend` | 5 |
| 状态不允许（已启用、正在运行） | `Conflict` | 4 |
| descriptor 不合法 | `Usage` | 2 |
| 用户取消 | `Cancelled` | 130 |

## 18.4 进度回显

所有可能超过数百毫秒的步骤必须回显，使用 `apps/cli/src/progress.rs` 的
`TaskSpinner` / `BarSink`：

| 步骤 | 文案 |
| --- | --- |
| 解析来源 | `正在解析来源…` |
| 下载 | `正在下载产物…` + 字节进度 |
| 校验 | `正在校验摘要与包安全…` |
| 解包 | `正在解包…` |
| 校验 descriptor | `正在校验插件描述…` |
| 启动 | `正在启动 <id>…` |
| 握手 | `等待插件就绪…` |

`[禁止]` 静默等待。用户看不到输出会认为程序卡死。

---

# 19. 可观测性与审计

## 19.1 审计事件

| 事件 | 字段 |
| --- | --- |
| `plugin.install` | id, version, source, sha256, actor |
| `plugin.remove` | id, data（`delete` / `keep`）, actor |
| `plugin.enable` / `disable` | id, actor |
| `plugin.start` / `stop` / `restart` | id, reason, actor |
| `plugin.crash` | id, exit_code, restarts, last_lines |
| `plugin.call` | caller, plugin, op, risk, result, duration_ms |
| `plugin.capability` | plugin, capability, method, result |
| `plugin.grant` / `revoke` | id, capability, actor |
| `plugin.contract` | caller, provider, contract, result, duration_ms |
| `plugin.config` | id, revision, changed_keys（不含值） |
| `plugin.update` | id, from, to, result |
| `plugin.denied` | plugin, capability, method, reason |

记录格式（JSONL）：

```json
{"at":"2026-11-18T09:31:02Z","event":"plugin.call","actor":"cli:local",
 "plugin":"chat-bridge","op":"send","risk":"mutating","result":"ok","duration_ms":42}
```

规则：

- 参数只记摘要（键名与长度），不记明文；`x-mcsup-secret` 字段完全省略。
- `actor` 形如 `cli:local` / `remote:<name>` / `plugin:<id>` / `system`。
- 审计写入失败不得影响业务，但必须记入宿主日志并计入指标。
- 保留期默认 180 天，按大小与时间双阈值轮转。

## 19.2 日志

| 日志 | 位置 | 内容 |
| --- | --- | --- |
| 宿主 | `logs/plugin-host.log` | 启动、会话、崩溃、限额、错误 |
| 插件 | `logs/plugins/<id>.log` | 插件 stdout / stderr，带前缀 |
| 审计 | `db/audit/plugins/YYYY-MM.jsonl` | 结构化事件 |

日志级别：`error` / `warn` / `info` / `debug` / `trace`。默认 `info`；
`--log-level` 可按插件覆盖，重启生效。

## 19.3 指标

| 指标 | 类型 | 标签 |
| --- | --- | --- |
| `plugin_ops_total` | counter | plugin, op, result |
| `plugin_op_duration_seconds` | histogram | plugin, op |
| `plugin_restarts_total` | counter | plugin, reason |
| `plugin_denied_total` | counter | plugin, capability |
| `plugin_event_dropped_total` | counter | plugin, kind |
| `plugin_process_rss_bytes` | gauge | plugin |
| `plugin_sessions` | gauge | state |

指标只在宿主进程内聚合；没有 metrics 后端时通过 `plugin status --json` 暴露
基础值，不新增全局状态。

## 19.4 排查顺序

| 现象 | 检查 |
| --- | --- |
| `unavailable` | `plugin status`：进程是否在跑；`plugin logs` 看握手错误 |
| `denied` | `plugin status --json` 看 `granted` 与 `approved` 差异 |
| 启动即退出 | 退出码 + 最后 20 行 stderr；entry 是否匹配平台 |
| 事件不来 | `resident` 是否为真；订阅是否正确；宿主是否存活 |
| Web 无界面 | descriptor 是否存在且通过校验；`plugin ops` 是否为空 |
| 配置不生效 | `revision` 是否推进；插件是否回了 `result` 错误 |

# 20. 测试要求

完整用例清单、ID 分组、需求追踪矩阵、conformance 文件格式、覆盖率门禁与里程
碑验收条件见 `docs/plugins-testing.md`。本节只给出必测范围；两处冲突时以测试
规范为准，并先同步修订本文。

## 20.0 条款追踪

每条硬规则与关键小节必须有测试 ID。没有 ID 的条款视为未实现，不得在 PR 描述
里声称完成：

| 规则 | 要求 |
| --- | --- |
| 新增 / 修改条款 | 同一 PR 内在 `docs/plugins-testing.md` 的追踪矩阵补 ID，并登记 `sdks/conformance/cases.json` |
| 新增错误码 / 能力 / 契约 / 展示类型 | 逐项补正向与失败路径用例 |
| 新增数值（超时、上限） | 设计文档、常量、conformance 三处同步 |
| 删除用例 | 只能因为条款被删除；条款还在而删用例视为回归 |
| 临时跳过 | 必须带 `reason` 与到期日期，里程碑验收前清零 |

## 20.1 分层

| 层 | 范围 | 位置 |
| --- | --- | --- |
| 单元 | manifest / descriptor 校验、能力交集、状态迁移、契约解析 | `domain` 与 `plugin` 的 `tests` |
| 契约 | 帧编解码、握手、错误码、流、取消 | `protocol` + `sdks/conformance` |
| 集成 | 真实插件进程：拉起、握手、调用、崩溃、重启、限流 | `crates/plugin/tests` |
| 端到端 | CLI 全流程；Web / MCP 在各自入口 | `apps/cli/tests` + 各入口 |

`[禁止]` 只测 mock 不测真实进程。协议与生命周期的缺陷几乎都出现在进程边界。

## 20.2 必测用例

| 用例 | 期望 |
| --- | --- |
| 握手成功 | `welcome.granted` 等于三方交集 |
| 越界能力 | 不授予，写审计，插件仍可运行其余能力 |
| API 版本不匹配 | `reject{code:"api_mismatch"}`，进程退出码 4 |
| 令牌错误 | 拒绝握手，不泄露有效信息 |
| 超长帧 | 终止会话，状态 `failed`，原因 `protocol_error` |
| 未知帧 | 忽略，连接保持 |
| 未知字段 | 忽略，连接保持 |
| 心跳丢失 | 45 s 后终止，按崩溃处理 |
| 崩溃退避 | 1/2/4/8/16 s；第 5 次转 `failed` |
| 手动 `start` | 清零退避并成功 |
| 空闲回收 | 非 resident 120 s 后停止；resident 不停止 |
| 配置回滚 | 插件回错误后恢复旧 revision，状态 `degraded` |
| 取消 | `cancel` 后 1 s 内停止发流 |
| 流式缺口 | 宿主检测到 `seq` 跳号即终止 |
| 重入调用 | 插件在 handler 内调宿主，不死锁 |
| 能力拒绝 | 返回 `denied`，不重试，不假装成功 |
| 契约广播 | 部分失败仍 `ok:true`，`failures` 完整 |
| 契约循环 | 第二次同 `(plugin, contract)` 被拒 |
| 契约深度 | 第 5 层被拒 |
| 安装不执行代码 | 用带陷阱的产物（含 `preinstall` 脚本）验证未被运行 |
| 路径逃逸 | 含 `../`、符号链接的产物被拒 |
| 卸载清数据 | 默认 `plugin-data` 与日志消失；`--keep-data` 后仍在 |

## 20.3 testkit

`crates/testkit` 增加（`[待实现]`）：

| 设施 | 作用 |
| --- | --- |
| `FakeEndpoint` | 内存双工，不开真实 socket，供 Windows CI 使用 |
| `FixturePlugin` | 编译期 fixture 二进制，可配置延迟、崩溃、超长帧、忽略取消 |
| `MockHostApi` | 逐能力可编程返回，验证宿主调用 |
| `DescriptorFixture` | 一组合法 / 非法 descriptor，供 domain 与各 SDK 共用 |

`FixturePlugin` 必须是真实可执行进程，不能是 trait 对象：只有真实进程能覆盖
端点、退出码、stdout 混入、信号处理。

## 20.4 门禁

| 门禁 | 内容 |
| --- | --- |
| `scripts/check.sh` | 既有步骤；新增 crate 后自动覆盖 `crates/plugin` |
| `scripts/check-sdk.sh` | 四个 SDK + conformance + 生成一致 |
| 架构 | `check-architecture.sh` 的 crate / 模块 / feature 断言全绿 |
| 平台 | macOS / Linux / Windows 各跑集成用例；Windows 必须覆盖 TCP 回退 |
| 文档 | `cargo doc` 无警告；本文与常量一致 |

新增一个数值（超时、上限）必须同时出现在：本文、`protocol` 常量或
`domain` 常量、conformance 用例。三处缺一视为未完成。

## 20.5 性能验收

| 指标 | 目标 |
| --- | --- |
| 本机握手（含进程启动） | p95 < 500 ms（冷启动）、< 150 ms（热启动） |
| op 调用额外开销 | p95 < 5 ms（不含业务） |
| 事件扇出 | 单宿主 5000 事件/秒，不丢且 `seq` 连续 |
| 空闲内存 | 宿主 < 40 MiB；单插件由插件自身决定，只记录 |
| 100 个非 resident 插件 | 安装与列表 < 1 s，无进程常驻 |

性能测试放在 `apps/bench`，不进入 `check.sh`，但 M1 起每次里程碑跑一次并把
结果写进 `docs/plugins-progress.md`（与 `remote-progress.md` 同构）。

---

# 21. 实施阶段

## M0：契约冻结（无运行时）

交付：

- `domain::plugin` 全部纯类型与校验规则；
- `port::plugin` 全部 trait 与错误码；
- `protocol::plugin` 帧集、控制通道、六个契约、`PLUGIN_PROTOCOL_VERSION = 1`；
- `sdks/conformance/` 首批用例（schema 8 条 + 行为 12 条）；
- 本文件与 `AGENTS.md` 索引；
- `check-architecture.sh` 的 crate / 模块 / feature 表更新；
- 根 `Cargo.toml` members、门面 feature、发布顺序、版本脚本。

验收：

- `scripts/check.sh` 全绿；
- descriptor fixture 全部通过 `domain` 校验；
- 帧编解码对同一组用例的字节输出与 conformance 一致；
- 没有任何插件进程被拉起。

## M1：宿主与最小能力

交付：

- `crates/plugin`：进程、端点、握手、心跳、帧循环、退避、空闲回收；
- `service::plugin::HostApiImpl` 实现 `ServerRead` / `ConsoleIo`；
- `storage` 实现 `PluginStore` / `KvStore` / `AuditSink`；
- `provider` 实现 `ArtifactSource`（`--from` / `--url` / GitHub 仓库归档）；
- CLI：`list` / `install` / `enable` / `disable` / `status` / `call` / `logs`；
- 能力：`events` / `storage:kv` / `server:read` / `console:read`；
- 补 `domain` 的 `ChatMessage` / `PlayerJoined` / `PlayerLeft` 与聊天解析器；
- Rust SDK：`run` + `#[plugin]` / `#[op]` + `--mcsup-dump-descriptor`；
- TS SDK：`definePlugin` + `mcsup-plugin build`。

验收：

- 一个最小插件装完即可 `list` / `enable` / `call` / `logs`；
- 聊天记录插件能在控制台打印游戏聊天；
- 越界能力被拒且审计可见；
- 崩溃与退避用例全绿；
- 非 resident 插件空闲后进程消失。

## M2：写能力、出站、契约与配置

交付：

- 能力：`console:write` / `net:outbound` / `secret:rw`；
- 契约路由（广播 / 单播）、循环与深度限制、审计；
- `config` 帧、回滚、`degraded`；
- `NetProxy`（域名白名单 + SSRF 防护）；
- CLI：`config get/set` / `grant` / `revoke` / `restart` / `rollback`；
- TS SDK 生成契约类型；Rust SDK 复用 `protocol`。

验收：

- 聊天桥可发送消息；AI agent 可对话；两者经契约组合出「问 AI → 回聊天」；
- 配置写错能回滚，界面显示错误；
- 未授予 `console:write` 的插件调用被拒；
- 契约循环在 4 层内被拦截。

## M3：平台写操作与六类展示

交付：

- 能力：`backup` / `files:read` / `files:write` / `server:lifecycle` / `content`；
- 六种 display 全部在 `descriptor` 校验与 CLI 投影中落地；
- REST / WS 契约冻结（`api` crate 的 DTO 从 `domain` 派生）；
- MCP 复用同一 op 表；
- `plugin doctor`。

验收：

- 备份上云、定时重启、白名单同步三个示例插件只用声明就得到可用界面；
- CLI / MCP 对同一 op 的输入输出一致；
- 六种 display 的 fixture 在渲染测试中全部通过；
- 破坏性 op 需要 Admin + 二次确认。

## M4：Java、撤销与市场

交付：

- 能力：`java`；
- 撤销列表、`quarantine`；
- 市场审核清单自动化（后续）；
- `docs/plugins-progress.md` 记录里程碑与实测性能。

验收：

- 撤销列表命中时拒绝启动；
- 升级失败自动回滚，用户不需要手工恢复。

签名、provenance 与 SDK 当前不做，见 `docs/plugins-progress.md`「明确不做」。

## 阶段依赖

```
M0 ─→ M1 ─→ M2 ─→ M3 ─→ M4
```

`[禁止]` 跳过 M0 直接写宿主：帧与 descriptor 未冻结时，宿主与 SDK 会各自
长出方言，后期无法收敛。

---

# 22. 明确不做

| 不做 | 原因 |
| --- | --- |
| 原生动态库插件 | ABI 不稳、崩溃拖垮宿主、跨语言成本高、无法审计 |
| WASM（v1） | 需要额外运行时与能力映射；等进程模型稳定后再评估 |
| npm / pip / cargo 安装插件 | 安装即执行任意代码；与审核、回滚全部冲突 |
| 安装时构建源码 | 同上；产物无法复用、无法校验摘要 |
| 插件自带前端 | XSS、CSP、审核、版本漂移；且与「作者不写前端」矛盾 |
| 任意布局 DSL（v1） | 把界面设计责任推给作者，负担反而上升 |
| 插件直连插件 | 绕过能力门、审计、超时、循环检测 |
| 通用 workflow 引擎 | 会把平台变成编排框架，契约无法闭合 |
| 插件自定义契约 | 同上；新需求走新增闭集契约 |
| 热替换 / 热重载 | 状态与授权难以保持一致；升级必须显式重启 |
| 声明「沙箱」 | 没有 OS 隔离就不成立；错误承诺比没有承诺更危险 |
| 插件自开 HTTP 端口供面板嵌入 | 鉴权、跨域、生命周期全部要重做 |
| 跨机共享插件进程 | 权限与数据边界会糊掉；每台机器各自运行 |
| 由插件决定授权 | 授权只能由用户确认，插件只能申请 |

# 附录 A：事件目录

订阅名在 `manifest.subscriptions` 与 `hello.subscriptions` 中声明。事件帧统一为：

```json
{"v":1,"type":"event","seq":42,"kind":"chat:message","at":"2026-11-18T09:30:00Z",
 "server":"survival","data":{"from":"Steve","text":"有人吗"}}
```

| `kind` | 所需能力 | `data` |
| --- | --- | --- |
| `chat:message` | `console:read` | `{from, text, at}` |
| `player:joined` | `console:read` | `{player, at}` |
| `player:left` | `console:read` | `{player, at}` |
| `console:line` | `console:read` | `{line, at}`，默认不订阅（高频） |
| `server:starting` | `events` | `{name}` |
| `server:started` | `events` | `{name}` |
| `server:stopped` | `events` | `{name}` |
| `server:crashed` | `events` | `{name, exit_code}` |
| `backup:completed` | `events` | `{name, snapshot, added_bytes}` |
| `backup:failed` | `events` | `{name, reason}` |

规则：

- `server` 字段在 machine 作用域下标识来源服务器；server 作用域下恒为自身。
- `seq` 按会话单调递增；缺口表示丢弃，插件应据此重同步（若支持）。
- `[禁止]` 把 `console:line` 当默认订阅：一个繁忙服务器每秒可产生数百行。
- 事件不持久化、不补发。需要可靠性的插件用 `query` 拉取状态，事件只做触发。

# 附录 B：常量速查

| 常量 | 值 | 位置 |
| --- | --- | --- |
| `PLUGIN_API_VERSION` | 1 | `domain::plugin` |
| `MANIFEST_SCHEMA_VERSION` | 1 | `domain::plugin` |
| `DESCRIPTOR_SCHEMA_VERSION` | 4（接受更旧的 1 / 2 / 3） | `domain::plugin` |
| `PLUGIN_PROTOCOL_VERSION` | 1 | `protocol::plugin` |
| `PLUGIN_CONTROL_VERSION` | 1 | `protocol::plugin` |
| 单帧上限 | 24 MiB | `protocol::plugin` |
| 握手超时 | 10 s | `plugin` |
| 心跳 | 15 s × 3 | `plugin` |
| 非 resident 空闲回收 | 120 s | `plugin` |
| 宿主空闲退出 | 300 s | `apps/cli` 外壳 |
| 崩溃退避 | 1/2/4/8/16 s，10 分钟 5 次 | `plugin` |
| 默认 op 超时 | 30 s（上限 300 s） | `domain` / `plugin` |
| 流式总字节 | 16 MiB（上限 64 MiB） | `plugin` |
| 事件队列 | 1024（上限 4096） | `plugin` |
| 日志限速 | 200 行/秒 | `plugin` |
| schema 深度 / 属性 / enum | 6 / 64 / 100 | `domain` |
| descriptor 上限 | 256 KiB | `domain` |
| 压缩包 / 解包 | 128 MiB / 512 MiB | `service` + `plugin` |

# 附录 C：最小 Rust 插件（SDK 形态）

以下代码是 SDK 的目标接口，不是现有实现；`[待实现]`。

```rust
use mcsup_plugin_sdk::{plugin, Ctx, Event, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Config {
    /// 巡检间隔（秒）
    #[serde(default = "default_interval")]
    #[schemars(range(min = 5, max = 3600))]
    pub interval_secs: u64,
}

fn default_interval() -> u64 { 60 }

#[derive(Debug, Serialize, JsonSchema)]
pub struct Health {
    /// 服务器总数
    pub servers: usize,
    /// 正在运行
    pub running: usize,
    /// 插件运行时长（秒）
    pub uptime_secs: u64,
}

pub struct Watchdog { interval_secs: u64 }

#[plugin(id = "watchdog", name = "Watchdog", version = "0.1.0", api = "1")]
impl Watchdog {
    fn from_config(cfg: Config) -> Self {
        Self { interval_secs: cfg.interval_secs }
    }

    #[op(kind = "query", display = "detail", risk = "read", title = "状态")]
    async fn health(&self, ctx: &Ctx) -> Result<Health> {
        let servers = ctx.server.list().await?;
        Ok(Health {
            servers: servers.len(),
            running: servers.iter().filter(|s| s.running).count(),
            uptime_secs: ctx.uptime().as_secs(),
        })
    }

    #[event]
    async fn on_event(&self, event: Event, ctx: &Ctx) -> Result<()> {
        if let Event::ServerCrashed { name, exit_code } = event {
            ctx.log.warn(format!("{name} 崩溃，退出码 {exit_code:?}"));
        }
        Ok(())
    }
}

fn main() -> mcsup_plugin_sdk::Exit {
    mcsup_plugin_sdk::run::<Watchdog>()
}
```

作者只写这些。SDK 负责：读 `--endpoint` 与令牌、连接、握手、注册 op、帧循环、
心跳、取消、退出码、`--mcsup-dump-descriptor`。

# 附录 D：最小 TypeScript 插件（SDK 形态）

```ts
import { action, definePlugin, query, s } from "mcsup-plugin";

// config schema 先提出来：Builder 要用同一个对象绑 `.config(config)`，
// `run` 里的 `ctx.config` 才有推导类型
const config = s.object({
  token: s.string({ min: 1, secret: true, label: "Bot Token" }),
  channels: s.array(s.string(), { label: "频道" }),
});

export default definePlugin({
  id: "chat-bridge",
  title: "聊天桥",
  version: "0.1.0",
  license: "MIT",
  api: 1,
  provides: ["chat.publish@1"],
  capabilities: ["console:read", "console:write", "events"],
  subscriptions: ["chat:message"],

  config,

  queries: {
    history: query("history")
      .title("聊天记录")
      .config(config)
      .input(s.object({
        channel: s.string(),
        cursor: s.optional(s.integer()),
        limit: s.integer({ max: 100, default: 50 }),
      }))
      .output(s.object({
        items: s.array(s.object({
          id: s.string(),
          at: s.datetime(),
          from: s.string(),
          text: s.string({ format: "minecraft-text" }),
          direction: s.enum(["in", "out"]),
        })),
        next: s.optional(s.integer()),
      }))
      .pagination({ cursorField: "next", limitField: "limit" })
      .run(async (input, ctx) => {
        const page = await ctx["console:read"].tail(input.channel, input.cursor ?? null, input.limit);
        return {
          items: page.lines.map((text, index) => ({
            id: `${page.cursor}-${index}`,
            at: new Date().toISOString(),
            from: "console",
            text,
            direction: "in" as const,
          })),
          next: page.cursor,
        };
      }),
  },

  actions: {
    send: action("send")
      .title("发送消息")
      .input(s.object({ text: s.string({ min: 1, max: 4000 }) }))
      .output(s.object({ ok: s.boolean() }))
      .run(async (input, ctx) => {
        await ctx["console:write"].sendLine("survival", `say ${input.text}`);
        return { ok: true };
      }),
  },

  onEvent(data, ctx) {
    ctx.log.info(`event: ${JSON.stringify(data)}`);
  },
});
```

作者只写这些。`s.*` 覆盖 §11.5 的 schema 子集，SDK 内部归约为 zod，仍同时得到
编译期类型、运行期校验与构造期 JSON Schema；超出子集时可直接写 `z`（逃生口），
两者可混用。启动样板也不需要写：`mcsup-plugin build` 生成 `bin/entry.mjs`，导入
默认导出后调用 `run()`。

`mcsup-plugin build` 做四件事：`tsc` 编译、运行 `--mcsup-dump-descriptor` 取
descriptor、把运行时依赖 vendor 进产物（§15.2「产物必须自带运行时依赖」、
符号链接展开成实体文件）、打包 zip。作者不接触端点、帧、路由与界面。

# 附录 E：GitHub 仓库归档

GitHub 来源**不需要**任何索引文件。仓库内容就是产物：

```
<repo root>/
├── manifest.json          # 必需，或唯一一层子目录里的 manifest.json
├── descriptor.json        # 必需
├── dist/                  # Node / TS 插件：编译产物
└── node_modules/          # Node / TS 插件：运行时依赖，必须自备
```

两种发布姿势：

| 姿势 | 作者 | 用户命令 |
| --- | --- | --- |
| 默认分支携带产物 | 把 `mcsup-plugin build --out .` 的产物提交进默认分支 | `install https://github.com/<owner>/<repo>` |
| 独立 `dist` 分支 | 产物提交进 `dist` 分支，代码留在默认分支 | `install https://github.com/<owner>/<repo>/tree/dist` |

`mcsup-plugin publish` 封装第二种：构建后把产物写成一次提交推到 `dist` 分支，
不碰工作区，也不需要用户手工切分支。

规则：

- `manifest.json` 必须在归档的解包根或唯一一层子目录里（GitHub 归档自带一层
  `<repo>-<SHA>/`，安装器会自动下钻）。缺失 → `invalid input`。
- 归档里的符号链接会被原样保留，安装时直接拒绝（`symbolic link in artifact`）。
  不要把 `node_modules` 直接提交进仓库——那是整棵链接树；用
  `mcsup-plugin build` 产出实体文件。
- 压缩包 128 MiB 上限适用于 GitHub 归档本身。产物里带大体积资源时改用
  `--url` / `--from`。
- 私有仓库拿不到内容（`codeload` 返回 404，两条解析路都失败），报
  `unavailable` 并提示改用 `--from` 安装本地构建。
- `[禁止]` 假设作者有构建步骤：安装流水线**不执行**仓库里的任何代码。

# 附录 F：实现顺序清单

给后续 agent 的最小正确顺序：

1. 读 `AGENTS.md`、`docs/architecture.md` 的分层与依赖方向。
2. 读本文第 4、6、10、11 节，冻结 `domain::plugin` / `port::plugin` /
   `protocol::plugin`。
3. 写 conformance 用例（schema 与行为），再写实现。
4. 按第 4.8 节改 9 处，先让 `check-architecture.sh` 通过。
5. 实现 `crates/plugin` 的进程与帧；用 `FixturePlugin` 做集成测试。
6. 实现 `HostApiImpl` 与 `PluginStore` / `KvStore` / `AuditSink`。
7. 接 CLI 的 `plugin` 命令与 `__plugin-host` 外壳。
8. 写 Rust SDK，再写 TS SDK；两边过同一份 conformance。
9. 补第 20.2 节用例；跑 `scripts/check.sh` 与 `scripts/check-sdk.sh`。
10. 更新 `docs/plugins-progress.md` 记录完成项与实测值。

`[禁止]` 在第 2 步之前写宿主：帧与 descriptor 未冻结时，后续所有代码都会
围绕错误契约返工。







