# MCSUP 架构规范

来自 `AGENTS.md`。

包含项目范围、目录结构、每个架构层的职责与边界、领域模型、Domain Event、状态机与依赖模型。

判断某段代码属于哪个层、某个类型该放在哪个模块时查阅。

---

# 项目范围

MCSUP（Minecraft Server Utility Platform）是专业级 Minecraft Server 管理平台。

MCSUP 使用 Rust 构建，采用 Cargo Workspace + Monorepo 架构。

MCSUP 面向三个入口：CLI、API（REST / WebSocket，由 `apps/server`
承载）、MCP。支持本地与远程 Server、Remote Agent，以及 Java / Minecraft
Runtime、Provider、File、World、Backup、Mod、Player、Scheduler、Monitoring、
Auth、Audit、Automation 等能力。目标是在生产环境长期稳定运行，可跨平台、高
性能、可扩展。Web 前端暂不在本仓库，后续作为独立项目通过 API 接入。

所有入口经 Service 调用，禁止直接访问数据库、启动 Minecraft 或处理 Backup。

# Workspace 架构

> 本节描述的多 crate 布局**也是目标布局**：多 crate + `mcsup` 门面。
>
> `mcsup` 按 feature 级联重导出内层 crate，是第三方依赖的唯一入口与发布
> 主体；`apps/*` 是并行 bin 入口。feature 矩阵、级联与 crates.io 发布条件
> 见 `docs/embedding.md` 的分发形态一节。

项目使用 Rust Workspace。

推荐结构：

```
mcsup/
├── apps/
│   ├── cli/                        # [已落地] CLI 程序入口，bin 名为 mcsup
│   ├── agent/                      # [已落地] Remote Agent 程序入口，bin 名为 mcsup-agent
│   ├── server/                     # [目标] API（HTTP / REST / WebSocket）入口，bin 名为 mcsup-server
│   ├── mcp/                        # [目标] MCP Server 入口，bin 名为 mcsup-mcp
│   ├── dev/                        # [已落地] 开发辅助工具
│   └── bench/                      # [已落地] 性能基准（criterion，无 bin 目标）
│
├── crates/
│   ├── mcsup/                      # [已落地] 门面：feature 级联重导出内层，唯一发布单元
│   ├── domain/                     # [已落地] 领域模型、领域规则和领域事件
│   ├── port/                       # [已落地] 能力 trait / Port 定义
│   ├── service/                    # [已落地] 统一业务服务层和业务编排
│   ├── backend/                    # [已落地] Local / Remote 执行位置抽象
│   ├── runtime/                    # [已落地] Minecraft / Java Runtime 运行管理
│   ├── provider/                   # [已落地] 外部资源和第三方 API Provider
│   ├── storage/                    # [已落地] 元数据和持久化
│   ├── file/                       # [已落地] 文件系统和文件传输
│   ├── backup/                     # [已落地] CAS / Snapshot / Restore / GC
│   ├── modkit/                     # [已落地] Mod / Modpack 管理（jar 检测）
│   ├── java/                       # [已落地] Java 检测、安装和版本管理
│   ├── protocol/                   # [已落地] RPC 消息和协议定义
│   ├── transport/                  # [已落地] RPC 通信实现（TLS + 长度前缀帧）
│   ├── api/                        # [目标] REST DTO / API Contract / OpenAPI
│   └── auth/                       # [已落地] Authentication / Authorization
│
├── tests/                        # 跨模块集成测试（现有跨 crate 用例在各 crate 的 tests/）
├── docs/                         # 项目文档
├── scripts/                      # 构建、开发和发布脚本
│
├── .github/
│   └── workflows/
│       ├── test.yml              # CI（手动触发）
│       ├── release.yml           # Release CLI（手动触发）
│       └── publish.yml           # crates.io 发布（手动触发）
│
├── Cargo.toml                    # Workspace 配置
├── Cargo.lock                    # 依赖锁定
├── README.md                     # 项目说明
├── CONTRIBUTING.md               # 开发规范
└── LICENSE                       # License
```

注意：

`crates` 中的库名称一律使用单个单词，不使用 `mcsup-` 前缀，也没有例外
（清单见上方目录树）。禁止使用以下名称：

```
core     // 内置 crate，会被遮蔽
std      // 内置 crate，破坏 prelude，println! / Vec / Ok 全部不可用
mod      // Rust 关键字；因此 Mod / Modpack 管理 crate 命名为 modkit
crate    // Rust 关键字
self     // Rust 关键字
super    // Rust 关键字
```

依赖名为 `core` 的包会永久遮蔽内置 `core`，连 `::core::...` 也失败。该错误
只在第一个下游 crate 编译时暴露，排查方向容易错，因此领域层必须是
`domain`。`cargo metadata` 不校验这些名称，必须在 CI 中检查。

`apps/` 下的包同样使用单词名：`cli agent server mcp dev bench`。它们不发布到
crates.io，不受全局命名空间约束。TUI 属于 `apps/cli`，不单独建入口。

可执行文件名必须带前缀，通过 `[[bin]]` 指定：

```
mcsup  mcsup-agent  mcsup-server  mcsup-mcp
```

因为 bin 名会进入用户 PATH。`cargo install` 装的是 bin 名，所以前缀不影响
安装体验：`cargo install --path apps/cli` 装出的可执行文件名仍是 `mcsup`。

# 核心依赖方向

依赖必须保持单向。

推荐：

```
apps
  ↓
service
  ↓
port / domain
  ↑
backend / runtime / provider / storage / file / backup / modkit
```

实现层实现 `port` 的 trait 并依赖 `domain`，`service` 不直接依赖任何
具体实现模块。

远程通信：

```
service
  ↓
backend
  ↓
transport
  ↓
protocol
  ↓
agent
```

API：

```
HTTP
 ↓
API DTO
 ↓
Service
```

禁止：

```
domain → CLI
domain → HTTP
domain → Agent
domain → TUI
domain → MCP
```

判定逻辑必须收敛到领域层（例如 `ServerStatus::transition`），入口只做错误
映射。三个入口（CLI / API / MCP）一旦各写一份判定就会分叉。
规则见 `AGENTS.md` 的判定逻辑收敛一节。

禁止：

```
    apps/cli → apps/agent
    apps/cli → apps/server
    apps/agent → apps/cli
```

`apps` 是并行入口，不允许互相 Cargo 依赖。

# Domain

`domain` 是领域核心：Server / ServerId / ServerStatus / ServerConfig / World /
Player、生命周期与状态转换、领域规则与领域事件、核心领域错误。状态用枚举
显式表达（如 `Created → Starting → Running → Stopping → Stopped`）。

Workspace 也是领域对象：`Workspace` / `WorkspaceId` / `WorkspaceName`、
命名与默认标记、身份与移动规则。目录布局与存储规范见 `docs/workspace.md`。

Domain 不涉及 CLI、HTTP、REST、TUI、MCP、数据库、RPC / gRPC，必须尽可能
稳定、纯粹、可测试。

# Service

`service` 是统一业务入口，CLI（TUI 落地后同样如此）、API、MCP 都经它调用。它负责
业务流程、跨模块协调、权限检查入口、Domain Event，并统一 Local / Remote。

Service 不负责 CLI 输出、HTTP Router、HTML、TUI 渲染、MCP 协议细节。

**文件系统访问**：`service` 内部一切 `std::fs` 调用必须经 `crate::fs`（`crates/service/src/fs.rs`）
这一个入口，不要在业务模块里直接写 `std::fs::xxx`。理由不是洁癖，是删除提示：
`remove_dir_all` / `remove_dir` 在 Windows 上遇到目录被占用（`os error 32`）必须补一句
「终端工作目录在其中 / 其他程序打开着」的可操作提示，散落写会漏。

> 这是**收敛**不是**依赖倒置**：签名仍是 `std::io::Result`、仍操作真实磁盘。
> 真正的方向是把它变成 `port` 里的 trait、由 `file` crate 实现、构造时注入，
> 但那要改 50 个构造点（`ServerRegistry::new` 19 处、`WorkspaceService::new` 17 处…），
> 属于独立事项。`crate::fs` 的函数与 `std::fs` 同名同参，因此可以逐文件推进、
> 每批单独过门禁。

**同步 / 异步契约**：`service` 同时存在两种编排接口，边界按「有没有网络
IO」划分，不按模块划分：

| 接口类别 | 例子 | 约束 |
| --- | --- | --- |
| 同步（主流，140+ 个） | 注册表、生命周期、备份、属性、设置 | 不得在长驻异步运行时的请求任务里直接调用，调用方自行 `spawn_blocking` |
| async（12 个） | `java::install`、`content`、`modpack` 的取回 / 应用、平台自检 | 需要调用方持有 tokio 运行时；内部用 `spawn_blocking` 把重文件 IO 移出执行器 |

CLI 是单命令进程，阻塞即本任务的全部工作，两种都能直接调用。同步宿主
（Qt / Swing 等）需要同步形态的网络能力时，应经门面提供的阻塞包装，
不要在库内部 `block_on` 一个共享运行时。

> 早期版本把「`service` 的编排接口是同步的」写成绝对规则；实际落地时
> 网络编排（下载 / 解包 / 安装器）留在 async，硬改成同步会把 `block_on`
> 推进库内部。此表是当前约定，新增 async 接口时同步更新。

# Backend

Backend 回答「这个操作在哪里执行」，由 `port::Backend` 定义，抽象为
`LocalBackend` 与 `RemoteBackend`。Local 本机执行，Remote 经 transport →
RPC → agent。

`port::Backend` 刻意保持**窄**，只收拢真正的执行位置能力：`SupervisorPort`
与 `InstallerRunner`。它不把存储 / 备份塞进来——那是「存到哪里」，与「在哪里
跑」是两个不同的轴，混在一起会变成 God Trait。数据能力是独立的
`port::BackupStore`（`Scanner` + `ContentStore` + `SnapshotRepository`）。

`service` 分别持有 `Arc<dyn Backend>` 与 `Arc<dyn BackupStore>`，不依赖任何
实现 crate。`crates/backend` 提供 `LocalBackend`（本机进程 / 安装器）与
`LocalBackupStore`（本地文件系统 CAS / 快照），由门面 `bootstrap` 组合根构造。
`backend` 是唯一允许组合实现层的 crate（`scripts/check-architecture.sh` 断言
其余实现层互不依赖）。

Service 里不应出现大量 `if remote { ... } else { ... }`；出现即说明抽象边界
有问题。

# Runtime

Runtime 回答「Minecraft Server 应该如何运行」：Server Jar / Loader、启动命令、
JVM 参数、环境、进程生命周期、stdout / stderr / stdin、PID、优雅与强制停止、
重启、崩溃检测、日志流。

下载文件不归 Runtime，归 Provider。

# Provider

Provider 回答「外部资源从哪里获得」，对接 Mojang、Fabric、Forge、NeoForge、
Paper、Purpur、Modrinth 等：HTTP API、下载、版本 / Mod / Modpack 查询、
Artifact 获取。Provider 不承担核心业务流程。

已实现：Vanilla（Mojang version manifest）、Fabric（meta API）、Forge
（Maven `maven-metadata.json` + `promotions_slim.json`，下载 installer JAR 并校验
`.sha1` 侧车）、Modrinth（Labrinth v2）。

## 两类 Provider 契约

Provider 对外提供两个互不重叠的端口：

| 端口 | 回答的问题 | 粒度 |
| --- | --- | --- |
| `port::ServerJarSource` | 「这个变体的服务端核心从哪拿」 | 一个变体一个实现 |
| `port::content::ContentSource` | 「给已有服务端装内容，项目 / 版本 / 文件在哪」 | 一个来源可覆盖多种内容类别 |

后者只描述文件（地址、大小、哈希），不执行下载；字节传输统一走
`port::ArtifactDownloader`，与核心下载共用校验与断点续传设施。

## Modrinth（内容来源）

`provider::modrinth` 封装 Labrinth v2 的查询面：搜索、项目详情与批量、
版本列表与批量、按文件哈希反查（单个与批量）、依赖图、标签
（加载器 / 游戏版本 / 分类 / 项目类型）、团队成员。真正的字节下载不在这里。

- 响应模型（`modrinth::manifest`）保留上游字符串，未知字段忽略：上游新增
  类型不会让整条响应报废
- 类别映射（`modrinth::source`）收敛在一处：`project_type` 只有 mod /
  modpack / resourcepack / shader，`datapack` / `plugin` 是同一份 jar 的额外
  用途，体现在 `all_project_types`
- 换源：`ModrinthSource::builder().base_url(..)` 指向自建 Labrinth 或镜像，
  只接受 `https`；装配层用 `McsupBuilder::content_sources` 替换整套来源

内容安装的领域口径在 `domain::server::content`：类别、落点目录、候选排序
（加载器 → 游戏版本 → 正式版 → 精选 → 发布时间）、客户端资源拒绝安装。
编排在 `service::ContentService`：搜索、挑版本、下载、落盘、幂等复用。

```
入口 apps/cli ──→ service::ContentService ──→ port::content::ContentSource
                        │                              ↑
                        └──→ port::ArtifactDownloader   provider::modrinth
```

## 服务端 Mod 管理

`server mod` 是「某台服务端装了什么、该装什么」，与 `mod inspect`（「这个 jar
能不能进服务端」）互补。前者要有服务端记录（目录、加载器、MC 版本）与运行态，
后者只面对文件。

| 层 | 职责 |
| --- | --- |
| `domain::mod_` | 客/服归属、依赖闭包、风险等级、`InstallVerdict`（`Blocking` 到底拦不拦） |
| `domain::server::content` | 类别 / 落点 / 候选排序；`ContentIdentity`（哈希反查结果）、`decide_update`（版本 ID 比较） |
| `port::mod_::ModInspector` | 读一个 jar 的能力面；`port::content::ContentSource::by_hash` |
| `port::Backend` | 运行态从 supervisor 取，判定「现在能不能改 disk」 |
| `service::ModService` | list / plan_install / install / remove / plan_update / apply_update |
| `apps/cli` | 只解析参数、调服务、渲染 |

不变量：

- **状态从磁盘读，不建表。** `mods/` 目录是唯一权威：建「已装 mod」表必然
  出现「表说装了、盘上没有」，用户手工丢进去的文件也会漏掉。代价是每次列表都
  要读 zip，联网识别要现算 SHA1——所以识别与查更新是显式可选项（`--project-info`
  / `--outdated`）。
- **两阶段。** `plan_install` / `plan_update` 零写入，入口展示后确认才
  `install` / `apply_update`；`--dry-run` 停在第一步。计划里带版本、落点与
  是否替换，用户确认的是**具体动作**而非「大概装点什么」。
- **先暂存再换入。** 下载落到 `<server>/.mcsup-staging/<n>/mods/`，用
  `ZipModInspector` 检查，通过才 `service::atomic::replace_file` 换进正式位置
  （同卷 rename，旧文件先挪开、失败回滚）。半截文件与「装进去必然起不来」的
  文件不会留在 `mods/` 里。
- **运行中一律拒绝。** 服务端正在读这些文件时改动等于随机崩溃。判定问
  `Backend::supervisor()` 的实时状态（与 `server start` 同源），不猜「目录里
  有没有锁文件」；状态取不到按失败处理。
- **判定不可用于服务端时不落盘**，`--force` 才放行，且风险与依据照样输出——
  强制不等于静默。
- **更新靠哈希反查。** `by_hash` → 识别项目与版本 → `candidates` → `decide_update`。
  上游不认识的文件进「无法判断」，绝不当作「已是最新」。
- **换名更新删旧文件。** 新版本常换文件名（`mod-1.0.jar` → `mod-2.0.jar`），
  只装新的会让 `mods/` 里同时存在两份同 modid 的 jar，加载器直接拒绝启动。
  因此更新项记下原路径，落盘成功后删掉旧文件；路径相同则不动。
- **更新只往前不往后。** 两个坑都踩过，各留一条不变量：
  - `featured`（作者精选）不进打分。它只是网页置顶标记，与新旧无关；一旦算
    加分项，被精选过的旧版本会压过最新版（实测 Cobblemon `1.6.1` vs `1.8.4F`）。
  - 已装版本不在候选集时，候选发布时间必须晚于已装版本才算更新。请求条件
    可以把已装版本排除在外（装了测试版、而请求只收正式版），此时最优候选可能
    更旧（实测 JEI `19.70.0.460` beta vs 正式版 `19.64.0.455`）。
- **`--kind` 只到 `mod` / `plugin`。** 这两个类别由服务端从顶层目录装载，
  改动的就是 `mods/` 与 `plugins/`；数据包在 `world/datapacks/`、服务端资源包
  由 `server.properties` 下发，入口报用法错误而不是装进没用的目录。

```
入口 apps/cli ──→ service::ModService ──→ service::ContentService ──→ port::content
                        │                        └─→ port::ArtifactDownloader
                        ├─→ port::mod_::ModInspector（本地检查）
                        ├─→ port::Backend::supervisor（运行态）
                        └─→ service::ServerRegistry（目录 / 变体 / MC 版本）
```

暂不作为本次范围：依赖求解（`ContentSource::dependencies` + 领域侧闭包规划）、
远端 mod 管理（协议 `Mod*` 帧 + agent 侧落地）。二者都不改变上面的分层，
只是在上表里各加一格。

## Modrinth 整合包（开服变体）

整合包不是一种「服务端软件」，而是**一份服务端布置方案**。它作为内置变体
`ServerVariant::ModrinthModpack`（`modrinth-modpack`）参与 `server create`，
但**没有** `ServerJarSource`——加载器由清单决定，而不是由变体决定。

| 层 | 职责 |
| --- | --- |
| `domain::server::modpack` | `.mrpack` 清单模型、路径安全、`server: unsupported` 过滤、依赖键 → 加载器 |
| `port::modpack::{ModpackSource, ModpackExtractor}` | 搜索可开服整合包 / 详情 / 版本 / 解析下载目标；读归档清单与铺 overrides |
| `provider::modrinth::modpack` | Modrinth 实现（`server_side` facet 过滤）+ 直接解析 zip 读取归档（不依赖系统 `unzip`） |
| `service::ModpackService` | prepare（取回 + 读清单）→ apply（铺 overrides + 下载服务端文件） |

开服顺序由入口编排：`prepare` 拿到 MC 版本与加载器 → 注册表写入 MC 版本与
**加载器对应的变体**（`fabric` / `forge` / `neoforge` / `quilt` / `vanilla`，
而不是 `modrinth-modpack`，后者只是入口标记、没有核心来源）→ 用**既有**
`ServerJarSource` 装对应加载器（与 `server install` 同一套流程，Forge /
NeoForge 的安装器运行与 Java 选择不出现第二份实现）→ `apply` 落文件。

`.mrpack` 是外部格式，解析放在 Provider（领域层不引入 serde）；领域层只做
校验与判定，因此 `ModpackIndex` 由 Provider 反序列化后经 `ModpackIndex::new`
构造。

Modrinth 同一版本可以同时提供客户端与服务端文件，服务端文件通常不是
`primary`，只在文件名里带 `[server]` 标记。服务端安装优先取该文件；没有时
退回主文件。不指定版本时也优先选带服务端归档的正式版，避免最新版只上传了
客户端包。客户端包常把仅客户端模组标成 `server: required`，直接装到服务端
会在启动阶段崩溃。

清单的 `env` 字段同样不可全信。`apply` 会把 `mods/` 下 jar 的 sha1 交给
`port::ModpackSource::pack_member_verdicts`（默认实现退化为
`client_only_hashes`），由来源给出两份事实：哪些文件被上游标注
`server_side = unsupported`、哪些文件被包内其他成员必需依赖。

「谁该留下」由领域层算：`domain::server::modpack::PackMemberGraph::retained`
从服务端可用的成员出发，沿必需依赖迭代到不动点。一次统计是错的——被跳过的
成员不该为别人提供保护（`sodium` 被同为仅客户端的 `indium` / `iris` 依赖，
一次统计会把它误留下来）。查询失败即中止安装，不带着未知状态落盘。

装完还有一道按**文件本身**的复核（`ModpackService::audit_mods`）：对整台服务端
的 `mods/` 与 `plugins/` 跑一次本地检测，列出会让服务端起不来的条目——缺必需
依赖、仅客户端、与目标服务端不匹配（加载器 / MC 版本 / Java）。判定收敛在
`ModReport::blocker`，扫描收敛在 `ModInspectService::blockers_in_dir`，与
`server start` 的预检是同一份代码：两处各判一套迟早分叉成「装完说没事、启动却
被拦下」。

安装本身不动这些文件。处置是用户的决定，入口负责问：交互式终端打印无边框表格
（序号 / 文件 / 原因），收逗号分隔序号 / `A` / `N`，选中的移进
`<server>/mods/disabled/`（`service::DISABLED_DIR`）。加载器不扫描 `mods/` 的
子目录，原名不变，用户随时能搬回来——与 `.mcsup-trash/` 同一条理由：判定是工具
的推断，可能是错的。非交互入口只展示 `blocked`，不替用户做决定。

```
server create --variant modrinth-modpack
   └─→ ModpackService::prepare ──→ port::ModpackSource / ModpackExtractor
         ├─→ 注册表写入 MC 版本与加载器变体
         ├─→ ServerJarSource（Fabric / Forge / NeoForge / Quilt…）
         └─→ ModpackService::apply ──→ port::ArtifactDownloader
```

Forge 的下载物是**安装器**而非服务端：Provider 以 `port::InstallKind::Installer`
声明这一点，并给出安装器产出的参数文件启动规格（`domain::LaunchSpec::Args`）。
真正运行 `--installServer` 由 `port::InstallerRunner` 承担，本机实现是
`runtime::LocalInstallerRunner`，因此 `service` 不依赖 `runtime`。
Forge 的 Maven 元数据不声明 Java 要求，`java_major` 由
`domain::server::version::required_java_major` 按 MC 版本推出。

第三方接入同一 Provider 时走 builder 注入 `maven_base` / `files_base` / `picks`，
与 Fabric 的做法一致，不需要改 service。

## 下载源（镜像）

跨境链路上单主机吞吐与可达性都不可靠：同一个文件换一个入口常常立刻可用，
镜像与官方分属不同主机时还能叠加带宽。下载源子系统把「从哪个入口下」从各条
业务流程里抽出来，做成一张表加一次选择。

三层职责，边界不重叠：

| 层 | 位置 | 职责 |
| --- | --- | --- |
| 领域规则 | `domain::mirror` | 上游面、源、目录表、候选改写、最快选择、并列次序 |
| IO 端口 | `port::mirror` | `SourceProbe`（探哪个能用）、`SourceRouteStore`（记在哪里） |
| 装配与实现 | `provider::mirror` / `service::download_source` / `storage` | 内置目录表、HTTP 探测、路由下载器、JSON 记录 |

术语三个词各指一件事，不要混用：

- **上游面**（`Upstream`）：一份官方内容的主机前缀，如
  `https://maven.minecraftforge.net`。它是选择与持久化的**单位**。
- **源**（`Source`）：一个镜像站，对上可以覆盖多个上游面，每个面给一个
  前缀（`Binding`）。官方本身也是一个源，所以「失败退回官方」没有特殊分支。
- **目录表**（`Catalog`）：全部上游面 + 全部源。`resolve(地址)` 给出该地址的
  全部候选，**官方恒为候选之一**。

为什么以「上游面」而不是「全局一个源」为粒度：没有任何镜像覆盖全部面——
BMCLAPI 没有 Modrinth 的 CDN，MCIM 没有 Mojang 的 maven。全局单值必然把某个
面固定到一个不覆盖它的源上，那比不选还慢。

选择时机与持久化：

```
第一次下载某个面 ──→ 并发探测该面全部候选 ──→ 取延迟最低
                          │                        │
                          │                        └─→ 落盘 `<data>/db/download/state.json`
                          └─ 全部失败 → 本次用官方，**不落盘**
之后同面的下载 ──→ 直接复用记录，不再探测
下载失败（可重试）──→ 作废该面记录 ──→ 重新探测并落盘新结果 ──→ 换源重下一次
```

「探测全失败不落盘」是刻意的：落盘等于把一次网络抖动记成「官方最好」，此后
再也不探镜像；不落盘则下次重探，代价只是一次注定失败的探测。

每次下载按 `info` 记一行「使用下载源」，字段含上游面与源名；批量下载逐条降到
`debug`，另给一行按源聚合的汇总——几百个文件时不刷屏，但用户仍看得出用了哪个源。

回退分两层，两层都要有：

- **同一次请求内换候选**：`file::FileDownloader` 按 `DownloadRequest::candidates()`
  依次尝试，换源时丢弃残片与段记录（上一个主机的字节不能与这一轮拼接），
  全部失败时以主地址的错误为准，但校验不符优先上报——上层凭它判断清单哈希过期
- **跨请求换源**：路由下载器在可重试失败上作废记录并重探一次，只一次；重探仍是
  同一源则不重试。404 一类「资源本就不存在」不作废记录，换源不会让它存在

日志位置约定：选源决策与换源由 `service::download_source` 记，因为它知道
「谁是选中的源」；字节层的重试（同源重连、续传）由 `file` 处理，不产生选源日志。

扩展一条新源：往 `provider::mirror::mirrors()` 里加一个 `Source`（若干
`Binding`），不需要改任何业务代码。若新源的主机不在 `upstreams()` 里，补一条
`Upstream` 即可——目录表是纯数据，加表不加密。用户自定义镜像走配置项
`download-sources`：每条前缀成为绑定到内容 CDN 的独立源，非法条目被忽略并告警，
不影响平台启动。

不是前缀替换能表达的镜像不进表：TUNA 的 Adoptium 把路径从
`.../v3/binary/latest/<ver>/ga/<os>/<arch>/jdk/hotspot/normal/...` 重排成
`Adoptium/<major>/<jdk|jre>/<arch>/<os>/<filename>`，属于路径改写而非前缀替换，
需要单独的上游面类型；BMCLAPI 的 `/v1/objects` 会 302 回官方且并发一高就 429，
收益不足，也不进表。

# Storage

Storage 持久化元数据：Workspace、Server、配置、Backup / Snapshot 元数据、
User / Role / Permission、Agent、Task、Audit 等，只存 metadata / index /
relationship / configuration / state。大文件不进数据库。

四个集合各有目录与 `state.json`：`workspaces` / `servers` / `java` / `download`。
`download` 只有一个状态文件、没有记录文件——它是一张「上游面 → 选中源」的表，
条目少且总是整体读写；拆成一条记录一个文件反而要为 `forge-maven` 这类含连字符
的键绕开文件名规则，收益为零。表里读不懂的源标识按「没有记录」处理而非判损坏：
这份文件唯一的作用是省掉一次探测，重探即可恢复。

# File

`file` 提供 list / read / write / copy / move / delete / mkdir / upload /
download / sync / checksum，全部 Streaming First。必须处理大文件、内存占用、
并发、断点续传、Chunk、Progress、Cancellation，禁止把整个 World 读进内存。

# Backup

Backup = File CAS + Snapshot：文件按内容哈希存入 CAS（`File → Hash → CAS
Object`），相同内容只存一次；Snapshot 记录各文件到哈希的映射。支持 Snapshot、
去重、Restore、Retention、GC、完整性校验、并发备份与失败恢复。

运行中备份的命令序列：先发 `save-off`（暂停写盘）→ `save-all flush`（落盘）→
等待片刻 → 建 Snapshot → 发 `save-on` 恢复写盘。备份不停服。`save-on` 由 RAII
守卫兜底，任何异常路径（扫描失败、CAS 写入失败、panic）都会执行，禁止让服务器
永久停在 save-off。

归属：

- 领域模型与判定（`Snapshot` / `SnapshotContent` / `BackupScope` /
  `RetentionPolicy` / `parse_age` / 恢复前置条件）在 `domain::backup`。
- 能力 trait（`ContentStore` / `SnapshotRepository`）在 `port::backup`。
- 编排（发控制台命令、增量复用提示、恢复物化、GC 引用计数、快照 id 生成）在
  `service::backup::BackupService`，依赖 `ServerRegistry` + `Arc<dyn SupervisorPort>`
  （不依赖 `ServerLifecycleService`）。
- 实现层在 `backup`：`cas`（SHA-256 两级分片对象存储）、`scan`（目录扫描与
  `(size, mtime)` 复用提示）、`store`（`snapshot.json` / `snapshot.partial` 记录）。

存储布局按工作区共享，`BackupLayout::for_server` 由 `server.dir` 与 `rel_dir`
反推工作区根：

```
{workspace}/backups/cas/ab/cd/<sha256>         # 内容对象，哈希即键
{workspace}/backups/snapshots/<id>/snapshot.json
{workspace}/backups/snapshots/<id>/snapshot.partial   # 写入中标记；只剩它即 failed
```

扫描口径：不跟随符号链接；跳过运行期产物（顶层 `run/`、`console.log`）；按
`BackupScope` 过滤，缺省为整个服务器目录。`mcsup.json` 恒在服务器目录并被备份。

增量 = 与父快照比较：复用提示必须用父快照记录时的 `(size, mtime)`
（`SnapshotContent::stamps()`）与当前磁盘比对，而不是用当前文件的 mtime 反推。
快照记录删除墓碑（`deleted`），否则恢复后已删文件会复活；原地恢复另外清除
「快照范围内、磁盘上存在但快照里没有」的文件（老快照无父快照、无墓碑）。

删除快照不即时删 CAS 对象，交由 prune / GC 回收；GC 的引用计数跨整个工作区
统计，避免删掉被其他服务器快照共享的对象。

# Protocol / Transport

Protocol 定义传输什么（`StartServerRequest` / `StartServerResponse` /
`LogEvent` / `FileChunk` / `BackupProgress` 等消息类型）；Transport 定义怎么传
（TLS 之上的长度前缀 JSON 帧；本地 supervisor 的 IPC 每请求一个连接，远端 RPC 复用长连接）。禁止把具体实现污染到 Domain。

> 早期设计写的是 gRPC / tonic / prost；实际落地的是自研帧层（`crates/transport`），
> 理由见 `docs/remote.md`：本地 supervisor 的进程 IPC 与远端 RPC 共用
> `protocol` 的帧与版本纪律，gRPC 会额外引入 tonic 的运行时与本地端口。

长任务的观察者（`TaskStage` / `TaskEvent` / `TaskObserver`）定义在 `port` 而不是
各入口：`service` 只往它上面抛事件，本地入口接到终端渲染，远端把事件装进
`Response::Task` 中间帧回传，客户端解帧后仍交给同一个观察者。事件是旁路，
不允许因为「终端写不进去 / 对端断开」让业务失败。新增一个耗时操作只需要
`service` 发事件，本地与远端自动同时具备进度回显。

`protocol::cli` 承载 CLI `--output json` 的机器契约 DTO：它是 CLI 对第三方的
稳定承诺，与 REST / WebSocket / 桌面客户端共用同一批类型，因此作为「唯一
契约来源」放在 `protocol` 是**有意**的，不属于入口逻辑泄漏。渲染（表格、
着色、进度）不在此层，仍在入口。

# Agent

Agent 是远程执行节点：`mcsup-server / CLI → RemoteBackend → RPC → Agent →
Runtime / File / Backup / Domain`。

Agent 是高权限程序，必须高性能、低资源占用、安全、可审计，支持 Streaming、
Cancellation、版本协商与 Capability。禁止默认开放公网且无认证、无 TLS；必须
实现 Authentication / Authorization / TLS / Pairing / Token / Permission /
Audit / Capability Negotiation / Protocol Version。

# REST API

`apps/server` 负责 HTTP / REST / WebSocket；`api` 负责
Request / Response / Error DTO、分页、过滤、排序、API Contract 与 OpenAPI。

HTTP Handler 只做 `Parse → Validate → Auth → Service → Map Response`，禁止
直接操作数据库或启动 Minecraft，禁止直接暴露 Domain Model 作为公共 API
Contract。

# Web（独立项目）

Web 前端不在本仓库，后续作为独立项目通过 REST API 与 WebSocket 通信，禁止
直接访问数据库、Domain、Runtime、Backup、Provider。

WebSocket 用于 Server Status / Logs、Player Events、Backup 与 File Transfer
Progress、Task Progress、Domain Events。

# Auth

`auth` 负责 Authentication / Authorization / Identity / Credential / Token /
Session / Role / Permission，权限形如 `server:read`、`backup:restore`、
`file:write`，角色可先有 Admin / Operator / Viewer。禁止一开始就构建复杂
RBAC / ABAC，先满足真实需求。

# CLI

CLI 必须非常薄：`Parse → Validate → Call Service → Display → Exit Code`，例如
`mcsup server start` 只是转调 Service，不实现 Server 启动逻辑。可以提供 JSON
与人类可读输出、Progress、交互 / 非交互模式、Shell Completion、Exit Codes。

# TUI

[目标] TUI 是 CLI 入口的一部分（与 CLI 同一个 app），基于 ratatui / crossterm，
只负责 Input / Rendering / Interaction。业务统一进入 Service，禁止复制 CLI 的
业务逻辑。当前尚未实现：`apps/cli` 只有交互选择器（`input.rs` 用 crossterm 的
raw mode 读键位），没有任何全屏 TUI 模块。

# MCP

MCP 是另一种用户入口，Tool 如 `server.start` / `backup.restore` /
`file.upload`，必须调用 Service，禁止自己实现业务逻辑。

# Domain Event

跨模块通信尽量通过 Domain Event 解耦，例如 `ServerStarted` / `ServerStopped`
/ `ServerCrashed` / `PlayerJoined` / `BackupCompleted` /
`FileTransferCompleted`。消费者包括 CLI、API、MCP、Scheduler、Monitoring、
Audit、Notification；Domain / Service 不直接依赖这些 UI。

事件类型定义在 `domain::server::event::DomainEvent`，出口是
`port::EventSink`（`publish(&DomainEvent)`）。`service` 在关键节点发布事实：
`start` 成功发 `ServerStarting`、`stop` 成功发 `ServerStopped`、`backup create`
成功发 `BackupCompleted`。默认装配 `port::NoopEventSink` 丢弃事件；宿主经
`McsupBuilder::events` 注入自己的订阅实现。事件是旁路——`publish` 不得让
发不出去影响业务。

# 状态机

生命周期状态必须用 enum 显式建模，禁止用多个 bool 拼接。例如用
`ServerStatus`，而不是 `running: bool` + `starting: bool` + `stopping: bool`
+ `error: bool`——后者会出现 `running` 与 `starting` 同时为真的非法组合。
Enum 让非法状态难以表达。

# 新代码归属

新代码属于哪个模块、禁止哪些架构捷径，见 `AGENTS.md` 的「代码归属与依赖方向」
一节；各层的职责与边界见本文上面的分层章节。

# 最终依赖模型

必须尽量保持：

入口是并列的，彼此不依赖，只向下依赖 Service：

```
   CLI(含 TUI)   API   MCP
        \         |    /
         \        |   /
          v       v  v
               Service
                 |
                 v
              Backend
             /       \
            v         v
         Local      Remote
           |           |
           |        transport
           |           |
           |        protocol
           |           |
           |         Agent
           v           v
   Runtime / File / Backup / Provider / ...
                 |
                 v
              Storage
```


