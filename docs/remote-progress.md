# 远程管理实施记录

本文件记录 `docs/remote.md` 的实施进度、已完成内容与后续待办。设计目标与
边界仍以 `docs/remote.md` 为准，此处只记「做到哪、下一步做什么」。

## 当前状态

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| 1 | `port` 身份键改造（`ServerRef`、`status_many`） | 完成 |
| 2 | `service` 来源维度、`[remotes.*]` 配置 | 完成 |
| 3 | `auth`：配对码 / token / 权限 / 撤销 | 完成 |
| 4 | `protocol`：远端帧与版本协商 | 完成 |
| 5 | `transport`：TLS 帧传输与连接池 | 完成 |
| 6 | `apps/agent`：`mcsup remote serve` | 完成 |
| 7 | 远端仓库（workspace / server / java） | 完成 |
| 8 | `RemoteBackend`：远端 supervisor（`RemoteSupervisor` + `RoutingSupervisor`） | 完成 |
| 9 | 远端备份（元数据帧路由）+ 远端日志尾部 | 完成（流式进度未做） |
| 10 | CLI `remote` 命令与 `--origin`（含 SSH 接入） | 完成（局域网发现待实现） |
| 11 | 远端能力对等：props / set / console / version / install / modpack / java install·uninstall | 完成 |
| 12 | `server link` 远端化、`server create --origin` 交互向导、系统钥匙串 | 完成（跨机器真机仍待手工） |

## 已完成

### 阶段 11：远端能力对等

此前远端只覆盖「协议已定义的帧」，`server props` / `set` / `console` /
`version` / `install`、`server create --modpack` 与 `java install/uninstall`
只能在本机使用。本阶段把这些命令补齐到与本地一致的语义。

#### 协议与线格式

- 请求：`ServerCreateModpack` / `PropsGet` / `PropsSet` / `SettingsSet` /
  `JavaInstall` / `JavaUninstall`；响应：`ModpackApplied` / `Props` /
  `SettingsChanged` / `JavaUninstalled`。
- `Props` 传**原文**而不是键值表：注释、顺序与空行属于用户文件内容，键值表
  无法无损表达；两端复用同一份 `Properties::parse`，`render()` 规范化行尾。
- `WirePropUpdate { key, value }` 用结构体而非二元组：帧是 JSON，字段名让
  审计日志与抓包都能直接读懂。
- 权限：`PropsGet` / `Version` 归读；`PropsSet` / `SettingsSet` /
  `ServerCreateModpack` / `JavaInstall` / `JavaUninstall` 归写。
- `ServerCreateModpack` 一帧完成本地「选版本 → 登记 → 装加载器 → 铺
  overrides」，因为清单要在对端读，客户端无法预先知道 MC 版本与加载器变体。

#### agent 分发（`crates/mcsup/src/remote/ops.rs`）

所有新帧转发给与本地 CLI **同一份** `service`：props 用 `ServerPropsService`、
设置用 `ServerSettingsService`、整合包用 `ModpackService::prepare + apply`、
Java 用 `JavaService::install/uninstall`。失败按领域错误分类映射
`ErrorKind`（not_found / invalid / io / busy），不按文案判级。

#### 客户端（`crates/backend/src/remote/mirror.rs`）

镜像仓库新增与帧一一对应的显式方法：`create_modpack` / `props` /
`set_props` / `set_setting`、`RemoteJavaRepository::install/uninstall`。
`RemoteModpackResult` 与 `ModpackApplied` 一一对应，供入口渲染同一份创建行。

#### CLI（Thin Entry Point）

- `commands/origin.rs` 新增 `RemoteTarget` 与 `remote_target()`：一次解析出
  成员、对端 id 与对端注册记录，props / settings / console / logs / install /
  java 统一复用。判定「本地优先 / 唯一远端 / 多远端重名报歧义」是纯函数
  `pick_hit`，可单测。
- `server props`：`Backend::{Local, Remote}`，远端原文进出。
- `server set`：`Target::{Local, Remote}`，交互默认值取对端记录，
  `java-version` 候选取对端已注册运行时。
- `server console`：日志走 `LogsFollow` 长轮询，注入与探活按**对端 id**
  走 `SendLine` / `StatusMany`——同名本地服务器不会被误注入。
- `server version`：改用 `open_registry_with_remotes()` 的 `get`，不新增帧。
- `server install`：裸名指向唯一远端；缺 `--version` 沿用在册版本（远端没有
  「列版本」帧）。
- `server create --modpack --origin`：对端执行 `prepare + 登记 + 核心安装 +
  apply`，返回布置统计；`--modpack` 与只选加载器的 `--variant` 互斥。
- `java install/uninstall --origin`：目标解析用对端运行时列表；`server start`
  缺 Java 且目标在远端时，装到对端。

测试：协议新帧往返与权限；backend 真 TLS 垫片覆盖 `create_modpack` /
`install` / props 往返 / `set_setting` / java install+uninstall；CLI 侧
`pick_hit` 4 用例（本地优先、唯一远端、跨远端歧义、限定不回落）。

### 阶段 12：本地命令远端化补全与系统钥匙串

#### `server link --origin`

- 协议：`Request::ServerLink` / `Response::Linked`（`server` / `target` /
  `linked`）。`target` 是**对端机器**上的路径，客户端拼不出也不该拼。
- agent：转发给与本地 CLI 同一份 `ServerRegistry::link`，符号链接 / marker /
  入库全在对端；`registry_failure` 负责 `not_found` / `already_exists` /
  `invalid` 分类。
- 客户端：`RemoteServerRepository::link_directory`（不与既有的
  `link() -> RemoteLink` 重名）。
- CLI：`server link <path> --origin <name>`，`<path>` 是对端路径；本地路径
  行为不变。

#### `server create --origin` 交互向导

- 协议新增两个只读帧：`ServerVariants`（对端可用变体）与
  `ServerVersions { variant }`（对端某变体的版本列表，含 `WireVersion`）。
  权限归 `Read`：变体 / 版本是元数据，viewer 可看不可写。
- agent：变体来自 `core.jar_sources`（去重保序）；版本走同一份
  `ServerJarSource::versions`，排序复用领域层
  `ServerVersion::sort_display_order`，与本地向导顺序一致。
- CLI：`create_remote` 在 TTY 下缺 `--variant` / `--version` 时从对端列表
  交互补齐（名称缺失也可交互输入）；非交互仍要求显式参数。选择逻辑抽取为
  `install::select_version`，本地与远端共用。
- 整合包变体仍要求显式 `--modpack`：清单要在对端读，远端侧不提供交互式
  整合包搜索。

#### 系统钥匙串

- `auth::credential` 新增 `KeyringCredentialStore<B: KeyringBackend>`：每个
  远端名一条 entry（secret 是 `StoredCredential` 的 JSON）；钥匙串不能枚举，
  `names()` 由独立索引 entry（`{service}:index`）维护。
- `KeyringBackend` 抽象出读写删，`SystemKeyring` 用 `keyring` crate（v1 兼容
  层）覆盖 macOS / Windows / Linux；`MemoryKeyring`（测试）驱动核心逻辑。
- `CredentialVault` 选择后端：`MCSUP_CREDENTIAL_STORE=auto|keyring|file`，
  `auto` 在 macOS / Windows 用钥匙串，其余 Unix 仅在存在会话总线 / 运行时
  目录时使用，否则落到 `0600` 文件。`describe()` 写进配对记录的
  `credential` 字段（`keyring` 或 `file:<路径>`）。
- CLI 的 `credential_store()` 一律返回 `CredentialVault`；集成测试用
  `MCSUP_CREDENTIAL_STORE=file` 保持无桌面依赖。

#### 测试

- 协议：3 个新请求 / 3 个新响应的往返、权限映射、`WireVersion` 领域往返。
- backend 真 TLS 垫片：`link_directory`、`variants`、`versions`。
- agent 集成：真实 agent 上跑 link / variants / versions（`agent_roundtrip`）。
- CLI 集成：`connect → workspace init --origin → server link --origin →
  server ls --origin`（真 agent + 真 CLI）。
- auth：钥匙串存储的往返 / 索引 / 幂等删除，后端选择解析与 vault 委派。

### 阶段 9：远端备份与远端日志尾部

#### 备份路由收在 `BackupService`

- `port::RemoteBackupPort`（`port/backup.rs`）：按服务器稳定身份寻址、
  域类型出入参的六个方法（create / list / restore / delete / verify /
  prune）。与 `BackupStore` 的分工：`BackupStore` 是**数据能力**三件套
  （CAS / 快照索引 / 扫描器，编排仍在 service），`RemoteBackupPort` 是
  **整段执行**——远端的 `BackupLayout` 依赖对端目录事实（workspace 推导、
  CAS 落点），客户端拼不出来也不该拼。
- `BackupService` 新增 `remote: Option<Arc<dyn RemoteBackupPort>>` +
  `with_remote_backups()`；`route_remote()` 按
  `ServerRegistry::get_sourced()`（新方法）查**真实来源**分叉，远端条目
  整段转发、发布 `BackupCompleted`（含 `added_bytes`）；远端未注入执行器时
  按 Unreachable 拒绝——有远端条目却没装配连接是装配错误，静默落到本地
  会在错误的磁盘上写快照。
- 跨线只走**元数据帧**：扫描、CAS、恢复写盘全部发生在对端，帧载荷是
  `WireSnapshot`（快照三份清单：entries / deleted / stamps），CAS 对象
  留在对端。这也意味着远端 `backup create` 不会把世界文件字节拉回客户端。
- `RemoteBackupError` 变体与协议 `ErrorKind` 对齐：NotFound / Invalid /
  Unreachable / Protocol / Io；`service` 包进 `BackupError::Supervisor`，
  入口按文案提示不再二次判级。

#### 协议帧（`protocol::remote`）

- 请求：`BackupCreate{label, scope, no_reuse, flush_wait_ms}` /
  `BackupList{page, page_size}` / `BackupRestore{snapshot, to_new}` /
  `BackupDelete` / `BackupVerify` / `BackupPrune{keep, older_than_secs,
  dry_run}`，全部要求 `Permission::Backup`。
- 响应：`SnapshotCreated{snapshot, skipped}` / `Snapshots{items, total,
  page, page_size}` / `Restored{snapshot, destination, files, bytes,
  safety_snapshot, deleted}` / `SnapshotDeleted{snapshot, orphaned}` /
  `SnapshotVerified{snapshot, objects, bytes, broken}` / `Pruned{removed,
  objects_reclaimed, bytes_reclaimed, dry_run}`。
- `wire::WireSnapshot`：`SnapshotId` / `RelPath` / `ContentHash` /
  `BackupScope` / `SnapshotStatus` 文本互转，`SnapshotContent::new +
  with_stamps` 重建，往返一致性由测试锁定。
- 备份恢复写路径的领域判定（可恢复性、运行中禁恢复、目标冲突、范围空）
  仍收敛在 `service` / `domain`，agent 分发与客户端都只搬参数。

#### backend 客户端侧（`backend/remote/backup.rs`）

- `RemoteBackups`：单远端 RPC 客户端，`ErrorKind → RemoteBackupError`
  映射出口唯一；对端返回非预期帧按 `Protocol` 报错。
- `RoutingBackups`：多台远端按 id 查组合仓库来源分派（与
  `RoutingSupervisor` 同构）；本地条目报 Unreachable（service 先分叉，
  走到这里即装配错误）。
- CLI：`open_mcsup_with_remotes` 为每台远端构造 `RemoteBackups` 装入
  `RoutingBackups`，经 `McsupBuilder::remote_backups()`（新注入口）注入；
  `backup` 命令切换到带远端装配。

#### agent 侧（`crates/mcsup/src/remote/ops.rs`）

六个 backup 方法转发给与本地 CLI **同一份** `BackupService`（不复制业务
逻辑）；`backup_failure()` 按领域错误分类：
SnapshotNotFound→not_found；NotRestorable / RestoreWhileRunning /
TargetExists / EmptyScope / InvalidLabel / ConflictingRetention /
VerifyFailed→invalid；其余→io。`InvalidScopeEntry` 在入帧解析期报 invalid。

#### 远端日志尾部（并回本阶段）

- `SupervisorPort::logs_tail`（新 trait 方法，默认拒绝——本地不经手日志）；
  `RemoteSupervisor` 覆写为一次 `LogsTail` RPC。
- `RegisteredServer::is_remote()` / `remote_ref()`（镜像条目 `dir` 为空即
  远端）；`BackupService::log_tail` 按条目来源分叉：远端走 RPC，本地仍
  直读 `console.log`。`server logs` 的 tail 至此对远端可用（follow 仍仅
  本地，属阶段 9 剩余的流式部分）。

测试：协议 12 帧往返；backend 真 TLS 垫片 5 用例（create/list、restore
安全快照、verify/delete/prune、NotFound 映射 ×2）；service `backup_routing`
4 用例（远端转发、未注入 Unreachable、本地不触执行器、未知服务器）；agent
roundtrip 全量权限覆盖补 `BackupList` 样例（`Permission::Backup` 豁免删除）。

### 阶段 8：远端 supervisor 与路由（RemoteBackend）

#### `RemoteSupervisor`（`backend/remote/supervisor.rs`）

`port::SupervisorPort` 的 RPC 实现，与 `runtime::LocalSupervisor` 对偶：
本地走 IPC 守护进程，远端走 transport 帧。判定全部在对端 `service`（agent
的 ops 分发），客户端只做参数搬运、解帧与错误映射。`status_many` 覆写成
一次 `StatusMany` RPC（trait 默认实现逐台 status，远端会 N+1）。

#### `RoutingSupervisor`（`backend/remote/router.rs`）

持本地 supervisor + 每台远端 `RemoteSupervisor`，按 id 查组合仓库
`list_sourced()` 分派；`status_many` 按目标分桶（桶键 `Origin`）、各桶
一次批量调用、输出顺序与入参一致；未知 id → `SupervisorError::Unreachable`。

#### CLI 装配（`apps/cli/src/bootstrap.rs`）

`open_mcsup_with_remotes` 为每台远端构造 `RemoteSupervisor`（连接来自
成员表），与 `runtime::LocalSupervisor` 一起装入 `RoutingSupervisor`，
经 `McsupBuilder::supervisor()` 注入。无已配对远端时与原装配完全等价。

测试：`RoutingSupervisor` 5 用例（本地分派、远端分派、未知 id、批量分桶
保序、远端不可达）；`RemoteSupervisor` 走真 TLS 垫片的往返用例。

### 阶段 7：远端仓库与 CLI 装配接入

代码在 `crates/backend/src/remote/`（客户端侧）与 `apps/cli/src/bootstrap.rs`
（装配）：

- `RemoteLink`：一条远端连接的网关，`call` / `expect`（要求特定载荷）；
  `ErrorKind → RepositoryError` 映射的唯一出口。
- 三个只读镜像仓库 `RemoteWorkspaceRepository` / `RemoteServerRepository` /
  `RemoteJavaRepository`：读走 list 帧；写暴露与帧一一对应的显式方法
  （`init` / `set_current` / `remove` / `create` / `scan`），trait 写方法
  显式报错——客户端不伪装本地仓库，目录与 marker 动作必须发生在对端。
  `list_sourced()` 覆写为远端来源标注。
- 组合仓库 `Combined*Repository` + `RemoteMembers`：读路径本地 + 逐台远端
  串行合并，远端不可达降级为本地条目并记 warn（离线不阻塞）；写路径**委托
  本地仓库**——能到达组合仓库的写只有 service 的本地操作（`init` /
  `set_current` / `create` / `upsert` / `update_core` …），远端写由路由层
  分派给镜像显式方法，不经组合仓库。
- CLI 装配：`remote_members` 读 `config.toml [remotes.*]` +
  `FileCredentialStore` 构造成员表，过滤非法名 / 非 `direct` / 缺指纹 /
  缺凭证（逐台跳过不阻塞）；`open_mcsup_with_remotes` 经 `McsupBuilder`
  注入三个组合仓库，`open_registry_with_remotes` / `open_java_service` /
  `workspace` 命令同口径。无已配对远端时与原装配完全等价。
- 测试：backend 侧 9 个（真 TLS 垫片 agent + 组合层行为），CLI 侧 6 个
  （成员装配过滤规则）。

### 阶段 1：`port` 身份键改造

远端只认稳定身份，不认客户端路径。`SupervisorPort` 原先用本地 `Path` 当键，
远端无意义，改为身份键 + 可选本地目录：

- `ServerRef<'a> { id: &'a ServerId, dir: Option<&'a Path> }`，
  `local()` / `remote()` 两个构造器。远端实现只读 `id`，本地实现用 `dir`
  省一次注册表查询。
- `stop` / `status` / `send_line` / `shutdown` 全部改收 `&ServerRef<'_>`。
- `SupervisorStartRequest` 增加 `server_id`：远端拉起的 supervisor 需要靠
  身份自报家门，不能依赖客户端进程的 `current_exe()`。
- 新增 `status_many`，默认实现逐台调用 `status`。远端实现覆写成单次批量
  RPC，避免 `statuses()` 变成 N+1 串行请求。

同步改动：`runtime::LocalSupervisor`、`testkit` 的 `FakeSupervisor`（记录
`ServerCall { id, dir }`，原 `dirs()` 改 `calls()`）、`service` 内两处测试替身、
`ServerRegistry::server_ref()`、`lifecycle::statuses()` 改走 `status_many`。

### 阶段 2：来源维度与远端配置

- `port::origin`：`Origin` / `RemoteName` / `OriginError` / `Sourced<T>`。
  放 `port` 而不是 `service`，因为**组合仓库**要给合并列表里的每条标注来源，
  而实现层不能依赖 `service`。
- `service::origin`：只留入口语义——`OriginFilter`（`all` / `local` /
  `remote` / `<name>`）、`Selector`（`origin/name` 限定名）、`resolve_index`
  （跨来源唯一性解析）。类型本体从 `port` 复用，不再重复定义。
- `port` 的三个仓库 trait 加默认方法 `list_sourced()`，默认全部标
  `Origin::Local`；组合仓库覆写。`ServerRegistry::list_sourced()` 透传，
  远端条目的 `dir` 为空、`linked` 恒为 `false`（符号链接是客户端侧细节）。
- `service::Config` 增加 `[remotes.*]`：`RemoteRecord`（原始 target 字符串 +
  `via` 字段）、`RemoteVia`、`parse_document` 校验、
  `remote_names` / `remote` / `put_remote` / `remove_remote`。

### 阶段 3：`auth`

新 crate `crates/auth`：

- `code.rs`：配对码状态机（短时有效、单次使用、限速尝试）。
- `token.rs`：`ClientId`、`Permission`、`Role`（Admin / Operator / Viewer）、
  `RevocationList`、带 TTL 的 token。
- `credential.rs`：`CredentialStore` trait + `FileCredentialStore`（权限
  `0600`）。钥匙串已在阶段 12 以同一 trait 的另一实现（`KeyringCredentialStore`
  / `CredentialVault`）补上。

### 阶段 4：`protocol` 远端帧

新模块 `protocol::remote`（与本地 IPC 的 `protocol` 平级，互不共用版本号）：

- 独立 `REMOTE_PROTOCOL_VERSION = 1`。不复用 `PROTOCOL_VERSION`：远端两端
  可以分别升级，把版本绑在一起会让「本地 CLI 与 supervisor 的 IPC 协议」
  和「跨机器协议」互相拖住。
- `wire.rs` 定义线格式与双向转换。**线格式单独一层**而不是给 `domain` /
  `storage` 加 `serde`：`domain` 不依赖 serde 是既有约束，`storage` 的记录是
  本机口径（带路径、带本地状态），直接序列化会把本机假设发到对端。
  `TryFrom<&T>` / `TryFrom<Wire>` 成对出现，用测试锁往返一致。
- `Capabilities` 是位掩码 + `Capability` 枚举（`bit()` / `ALL` / `has()`），
  不是若干个 `bool` 字段：新增能力不破坏旧客户端，也不触发
  `struct_excessive_bools`。
- 帧集按**操作**而不是仓库 CRUD：写操作整条在 agent 侧跑同一套 `service`，
  客户端只搬参数。`Request::Start` 不带 `LaunchSpec`——启动方式是对端的事实，
  客户端指定「用哪个文件启动」等于让它选远端要执行的文件。
- `WireStatusEntry` 用两个可空字段（`status` / `error`）而不是 `Result`：
  `serde` 的外部标签会把线格式变成 `{"Ok":…}`，与「本来就是 `null`」难区分。
  不变量（恰好一个为 `Some`）由 `into_result()` 断言。
- 权限映射在 `Request::required_permission()` 一处：`LogsTail` 要 `Logs`
  而不是 `Read`（控制台可能含玩家 IP 与管理员操作），`ClientList` /
  `ClientRevoke` / `Shutdown` 要 `Manage`。

### 阶段 5：`transport`

新 crate `crates/transport`：只搬字节，不含业务判定。依赖收敛在契约 crate
（`domain` / `port` / `protocol` / `auth`）。

- 自签身份 `ServerIdentity::load_or_create(dir)`（`rcgen` + `pem`），
  指纹是证书 DER 的 SHA-256，文本 `sha256:<64 hex>`。
- 客户端信任模型是**固定指纹**（TOFU）而不是 CA：每台被连接端各自自签，
  没有公共 CA 可依赖；自定义 `ServerCertVerifier` 只比对指纹，不查有效期 /
  名称 / 链。`Hello` 里 agent 自报的指纹必须与 TLS 层固定的一致，否则按
  中间人拒绝。
- `Session` 拆成 `send_request` / `recv_response` / `send_response` /
  `recv_request`，不用泛型 `send`：两端共用同一个类型，若允许「客户端发
  响应」，状态机就只能靠运行时断言兜住。
- 服务端严格三步：`Hello` →（`Enroll` | `Authenticate`）→ 业务帧。
  `serve_connection` 拆成 `say_hello` / `authenticate` / `dispatch`；
  `authenticate` 返回 `Option<SessionInfo>` 表示连接是否继续——配对成功后
  token 已交付，不在同一条连接上继续，让「配对」与「日常」走完全相同的认证
  路径。
- `Client` 按需建连、复用连接，断线重试一次（远端重启是常态，把重启表现成
  失败会让每个脚本都要自己重试）；**建连本身就失败时不重试**——没有可重建的
  断连，重试只会把不可达远端的等待翻倍。
- `Server` 一连接一线程，上限 `MAX_CONNECTIONS = 32`；单条连接的失败只影响
  它自己。
- `StopHandle`（`request_stop()` / `is_stopping()`）：置停止标志后自连一次
  踢醒阻塞的 `accept`。不引入额外线程、不轮询。
- 刻意保持**同步**：`service` 的编排接口是同步的，CLI 是单命令进程，异步只
  会在中间多出一层 `block_on`。

### 阶段 6：agent

- `crates/mcsup/src/remote/`：`transport::Handler` 的完整实现。放门面而不是
  `apps/agent`，因为 `mcsup remote serve` 与 `mcsup-agent` 必须跑**同一份**
  实现，而 `apps` 之间禁止 Cargo 依赖。
  - `mod.rs`：`Agent`（装配、身份、名册、配对码、绑定、服务）、`PairingWatch`、
    `AgentServer`、`AgentError`、`impl Handler for Agent`。
  - `ops.rs`：一帧 `Request` → 一次 `service` 调用 → 一帧 `Response`，以及
    `ErrorKind` 映射（按各错误的 `kind()` 而非文案）。
  - `supervise.rs`：`__supervise` 参数解析与入口，与 `mcsup __supervise`
    解析同一份参数形状。
- `apps/agent`（package `agent`，bin `mcsup-agent`）：薄入口，只做参数解析与
  打印。缺省监听 `0.0.0.0:7681`。
- 配对码在常驻进程里**过期自动轮换**：`PairingCodeStore::expired()` /
  `rotate_if_expired()`，只有自然过期才换新。用掉与锁死都保持关闭，否则限速
  形同虚设。
- agent 侧 `start` 从注册表读 `meta.launch`，`supervisor_exe` 默认是
  `current_exe()`（`mcsup-agent` 自己也实现了 `__supervise`，省掉第二个 bin）。
- 认证与业务都**不缓存**判定结果：名册每次读盘、配对码每次读内存状态。缓存
  会让「用户刚撤销了客户端」与「agent 还认这个 token」之间出现窗口。
- 审计：每帧记调用方、角色、操作、结果；参数不进日志。

### 附带修复：Java 安装下错架构

`mcsup java install` 在 aarch64 机器上装下了 x86-64 的包，`java` 直接
`cannot execute`。根因是 foojay 的架构参数名是 `architecture`，写成 `arch`
会被上游**静默忽略**，返回全架构列表，service 取第一个可解压包就中了 x64。

修复：

- `foojay`：参数名改正，`limit` 提到 200。
- 三家源（foojay / adoptium / azul）统一加**本地复核**：声明架构 + 归档文件名
  双重复核，OS 同名校验。上游过滤不可全信，azul 的 `arch` 参数同样会被静默
  忽略，且响应里根本没有架构字段，只能从文件名推断。
- `port::current_arch()` 补齐 x86 / arm / riscv64 / ppc64le / s390x，不再把
  一切非 aarch64 当成 x64。
- 回归测试锁住归一规则与「aarch64 请求不得接受 x64 归档」。

### 阶段 10：CLI（已完成的部分）

- `remote serve`：前台运行 `mcsup::remote::Agent`，打印名称 / 地址 / 指纹 /
  配对码；码过期自动轮换。`--status` 离线读身份、名册与 pid；`--stop` 按
  `{data_dir}/remote/agent.pid` 终止前台进程。**系统服务注册（systemd /
  launchd）尚未实现**，`serve` 目前是前台进程。
- `remote connect <host[:port]>`：`Client::probe` 捕获 TLS 指纹 → 展示并确认
  （非交互需 `-y`）→ `Enroll` → 写 `credentials.toml`（0600）与
  `config.toml [remotes.*]` → 打印同步摘要。交互确认指纹后提示命名（默认
  最小空闲序号 `r1` / `r2` …；同名指向别的 target 时警示重问），`--name`
  跳过提示；
  `--code` 给配对码，`--read-only` 以 viewer 配对。
- `remote reconnect <name>`：复用记录，先 probe 校验指纹未变，再用已存 token
  认证；指纹变化要求重新 `connect`。
- `remote list` / `status` / `ping` / `disconnect` / `revoke`：`list` 每台做一次
  `Ping` 探测（`ok` / `unreachable` / `unauthorized`）；`revoke` 在 serve 端
  离线删除名册记录。
- `--origin`：`workspace ls` / `server ls` / `java ls` 支持
  `all|local|remote|<name>`，`MCSUP_ORIGIN` 作为缺省；人类输出加 `ORIGIN`
  列，JSON 每项加 `origin` 字段。`workspace init/use/remove`、
  `server create/remove`、`java scan` 支持 `--origin <远端>` 直接在对端执行。
- 限定名 `origin/name` 接到服务器选择器；同名跨来源时明确拒绝远端路由，
  避免落到本地同名服务器。
- 退出码 6（未授权：token 失效 / 权限不足 / 指纹变化）。
- 修复：`commands::run` 重建 `Printer` 时丢失全局 `-y`，导致 `-y` 对确认
  式提问完全失效。

### 阶段 10：SSH 接入

`mcsup remote connect user@host` 与直连共用同一条配对 / 落盘路径，差别只在
「地址从哪来」：

- `apps/cli/src/ssh.rs`：`user@host` 解析、SSH 引导握手读取、`ssh -L` 隧道、
  进程内**会话**登记（引导进程 + 隧道进程成对持有，Drop 时一起收）。
- 远端入口 `mcsup::remote::serve_ssh_main`（隐藏子命令 `remote __serve-ssh`）：
  绑定 `127.0.0.1:0`，stdout 首行握手 JSON（`SshHandshake`：地址 / 指纹 /
  配对码 / 主机名），之后每行是配对码轮换。stdout 是唯一的机器通道，
  stderr 留给日志。
- 会话寿命由两条线索决定，任一断掉即收摊，不留孤儿 agent：**stdin EOF**
  （本地进程一退出，它持有的管道关闭）与 **stdout 写失败**（`EPIPE`）。
  两条都只 `request_stop`，让监听线程正常收尾。
- 每条需要远端的命令都重新引导一次：远端 agent 监听随机端口且随上一条 SSH
  会话结束，记录里存上次的端口没有意义。稳定的是 `user@host`。
- 装配层不建隧道：`build_member` 对 `via = ssh` 构造**延迟解析**的
  `RemoteLink`（`RemoteLink::lazy`），第一次真正发帧时才调 `ssh::endpoint_for`。
  纯本地命令因此不为远端付连接代价，离线远端也不会把本地命令卡在 SSH 超时上。
- `remote reconnect` / `list` / `status` / `ping` 全部经 `ssh::endpoint()`
  取实际可连地址；`list` 里隧道建不起来按 `unreachable` 降级，与直连远端
  不可达同一语义。
- 入口在 `main` 持 `ssh::SessionGuard`：静态登记表不会在进程退出时 drop，
  守卫负责把 `ssh` 子进程收掉（否则 `ssh -N -L` 会活到系统重启）。
- 修复 WIP 里的地址错误：SSH 配对成功后的同步摘要原先拿 `user@host` 当 TCP
  地址去连，现在走本次会话的隧道地址。
- 记录里不再有 `remote_addr`：它只对「同一条命令内的那条 agent」有意义，
  对下一条命令是过期信息，留着会诱导按陈旧端口建隧道。

测试：`apps/cli/tests/cli_remote_ssh.rs` 用**假 ssh**（`examples/fake_ssh.rs`，
实现 CLI 用到的两条命令契约：引导读握手、`-L` 真 TCP 转发）+ 真 agent + 真
CLI 二进制，覆盖 `connect user@host → list → ping → reconnect →
workspace init/ls --origin → disconnect`，以及 SSH 失败时的握手错误与 `list`
降级。示例只在测试时按需编译，不进 `cargo install` 的产物。

测试：`apps/cli/tests/cli_remote.rs` 用真 agent + 真 CLI 覆盖
`connect → list → workspace init --origin → workspace ls --origin → ping →
disconnect`、`server create/list/status/remove --origin`、`java scan --origin`，
以及无 `-y` 拒绝信任未知指纹、死端点失败路径。

## 后续待办

### 局域网发现与系统服务注册（未做）

- `connect` 无参数目前报用法错误；局域网发现需要 mDNS/SSDP 或端口扫描，
  并处理多网卡、防火墙与同名冲突。
- 系统服务注册（systemd unit / launchd plist）未做，`serve` 仍是前台进程。
  SSH 接入不需要它（每条命令临时起 agent），直连接入才需要常驻。
- SSH 接入已落地，见「阶段 10：SSH 接入」；跨机器真机验证（两台物理机 +
  局域网地址）仍待手工执行。

### 阶段 9 剩余：流式（未做）

现状：`log_tail` 已走单次 `LogsTail` RPC（阶段 9 并回本阶段完成），
`backup create` 的实时进度在远端路径上被丢弃（客户端只拿到最终结果帧）。
尚未做的：

- 日志 follow：`server console` attach 需要服务端流式推帧（当前 `LogLines`
  是一次拉取，follow 仍仅本地）。设计方向：在 `protocol::remote` 增加
  服务端主动推送的流式帧（如 `LogChunk`），连接上以「请求订阅 → 持续推送」
  模型承载，与现有请求-响应帧共存；断线即取消订阅，不引入会话状态。
- 备份进度：**维持远端路径不回传进度的现状**。远端 `backup create` 期间
  客户端 spinner 持续运转（RPC 未返回即未结束），并非无回显；而事件日志 +
  `BackupProgress` 轮询帧需要 agent 侧引入跨请求状态（进度快照、过期清理、
  快照 ID 关联），与「无会话状态、断线即取消」的协议纪律相悖。重新评估的
  触发条件：备份时长增长到分钟级且用户反馈需要取消能力（取消需要同一套
  事件通道，届时一并设计）。
- 日志 follow 已由 `LogsFollow` 长轮询落地（见阶段 10 记录）；备份进度的
  取舍见上。`docs/remote.md` 的相应措辞已同步收敛。

### 阶段 10 收尾

- 阶段 10 已完成；剩余项见「局域网发现与系统服务注册」与「阶段 9 剩余：流式」。
- `docs/remote.md` 的用户接入一节已按实际能力收敛（SSH 已接入；局域网发现 /
  系统服务注册明确标为未实现）。

## 已确认的设计取舍

- 备份路由收在 `BackupService`（按组合仓库真实来源分叉），而不是给
  `BackupStore` 的 Scanner / CAS trait 硬套远端语义——那些能力的实现依赖
  本地目录事实，硬套会把「数据存在哪里」与「在哪台机器执行」两个轴搅在
  一起。远端走 `RemoteBackupPort` 整段执行 + 元数据帧，CAS 对象不跨线。
- `connect` 保留首次配对，新增 `reconnect <name>`；记录存原始 target 字符串
  + `via` 字段。
- token 绑定客户端密钥对（mTLS 降级为 enrollment 凭据），带 TTL。
- 「无常驻会话」改为「无常驻客户端进程」。
- `McsupSet` 路由替代 `McsupBuilder` 注入替换，解决「唯一装配分叉」与
  `--origin all` 合并展示的矛盾。
- 判定逻辑收敛：`Origin` / `RemoteName` / `Sourced` 在 `port`（实现层要用），
  `OriginFilter` / `Selector` / `resolve_index` 在 `service`（入口语义）。
- SSH 接入**每条命令重新引导**，记录里不存远端端口：远端 agent 监听随机端口
  且随 SSH 会话结束，存下来的端口对下一条命令必然是过期信息。稳定的是
  `user@host`，重放它就够了。
- 远端 agent 的寿命绑在 SSH 会话上（stdin EOF + stdout 写失败），而不是靠
  常驻守护或 pid 文件：SSH 没有「会话结束」通知，只有流关闭，这是唯一可靠的
  信号，也让「无会话状态」的协议纪律保持不变。
- 装配层用**延迟解析地址**（`RemoteLink::lazy`）而不是装配期建隧道：装配发生
  在每个命令的开头，本地命令不该为远端付连接代价，离线远端也不该把本地命令
  卡在 SSH 超时上。解析失败不缓存，远端恢复后下一次命令即可重试。
- 假 ssh 由测试用 `rustc` 现场编译（`apps/cli/tests/support/fake_ssh.rs`），
  不做成 `examples/`：`cargo test` 不构建 examples，而测试里再跑 `cargo build`
  会按另一套 feature 组合重建整棵依赖树。假 ssh 只用标准库，与 cargo 决议无关。

## 测试环境

- 机器是 aarch64，无 `sudo`、无系统 java。
- 测试用 Java 装在 `~/Downloads/mcsup-test/home/.local/share/mcsup/java/temurin-21`
  （由 `mcsup java install 21 --vendor temurin -y` 安装，已验证可执行）。
- `apps/cli/tests/cli_e2e.rs` 隔离 `HOME`，靠宿主 `PATH` 找 java，跑测试前需把
  该 `bin` 目录加进 `PATH`。
- 远端链路的自动化验证：`apps/cli/tests/cli_remote.rs` 在本机起真 agent +
  真 CLI，覆盖配对、来源过滤、远端建站 / 删站、`server link --origin`、远端
  Java 扫描与断开；`agent_roundtrip` 覆盖 link / variants / versions 三个新帧。
  跨机器真机验证（两台物理机 + 局域网地址 + 远端备份）仍待手工执行——本机
  自动化只能验证协议与业务链路，不能替代真实的网络 / 防火墙 / 多网卡验证。
