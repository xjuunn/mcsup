# 远程管理

mcsup 的远程能力：一台机器上的 `mcsup` 可以像管理本地资源一样管理另一台
机器上的工作区、服务器、备份与 Java 运行时。命令完全一致，差异只体现在
资源的**来源**（origin）。

## 目标与非目标

目标：

- 两个命令完成接入：被连接端 `mcsup remote serve`，连接端 `mcsup remote connect`
- 接入后所有管理命令与本地一致，不新增 `--remote`、不引入模式切换
- 远端资源出现在原有列表中，带来源标记
- 列表可按来源过滤：全部 / 仅本地 / 全部远端 / 指定远端
- 默认安全：不开放匿名访问，密钥不写进配置文件

非目标：

- 不提供「本地模式 / 远程模式」切换
- 不做双向配置同步，因此不需要冲突解决
- 不做浏览器 UI（由 `apps/server` 承担）
- 不允许远端执行任意 Shell 命令

## 设计原则

1. **身份路由，不做模式切换**：远端是资源属性，不是全局状态。
2. **只读镜像，无冲突解决**：远端元数据只供读，写操作直连远端（本地缓存
   待实现，见「数据与同步」）。
3. **按需连接，无常驻会话**：本地不跑守护进程，命令按需建连。
4. **离线不阻塞**：远端不可达时本地资源照常工作。
5. **复用既有信任**：优先复用 SSH，不新增凭证体系。
6. **判定留在 service**：远端实现只是 `port` trait 的另一种实现。

## 架构落点

| 能力 | 本地实现 | 远端实现 |
| --- | --- | --- |
| `port::Backend`（执行位置） | `LocalBackend` | `RemoteBackend` |
| `port::WorkspaceRepository` | `storage::JsonStore` | `RemoteWorkspaceRepository` |
| `port::ServerRepository` | 同上 | `RemoteServerRepository` |
| `port::JavaRuntimeRepository` | 同上 | `RemoteJavaRepository` |
| `port::BackupStore` | `LocalBackupStore` | `RemoteBackupStore` |

`service` 只依赖 `port`，因此同一份 `WorkspaceService` / `ServerRegistry` /
`JavaService` / `ServerLifecycleService` 同时服务本地与远端，不出现
`if remote { ... } else { ... }`。

唯一装配分叉在 `apps/cli/src/bootstrap.rs` 的 `open_mcsup()`：没有已配对
远端时 `Mcsup::open(data_dir)`；有远端时把远端实现注入 `McsupBuilder`。
判断只发生一次，命令层不感知来源。

## 可复用的既有设计

- `ProgressSink` / `InstallerOutputSink` / `StageSink` 是同步回调，远端实现
  只需把回调转成一帧发送。
- `CancellationToken` 基于 `Arc<AtomicBool>`，可把连接断开映射为取消。
- `EventSink` 是旁路契约（发不出去不影响业务），天然承载远端进度与状态推送。
- `protocol::Frame` + `PROTOCOL_VERSION` 已是本地 IPC 的帧与版本纪律，远程
  复用同一套，协议演进只维护一处。
- `workspace.toml` marker 记录工作区身份，远端重装或换机后目录仍可恢复。
- `resolve_selector`（精确命中优先、唯一前缀次之、冲突列候选）在 service 层
  解析，跨端语义一致。
- `McsupBuilder` 已有 `server_repository` / `workspace_repository` /
  `java_repository` / `backend` / `backup_store` 注入口，无需新装配机制。
- agent 内部就是 `Mcsup::open(remote_data_dir)`，不是第二套程序。

## 用户视角

### 被连接端（远端主机）

只需要一条命令：

```
$ mcsup remote serve
mcsup 已开始接受连接
  名称    prod-box
  地址    10.0.0.5:7681   （同一局域网可自动发现）
  配对码  4821-9375        （10 分钟内有效）
```

它当前是**前台进程**（与 `mcsup-agent` 同一份实现）；系统服务注册
（systemd / launchd）尚未实现，需要用户自行用 `nohup` / `tmux` / 服务管理器
托管。

| 命令 | 说明 |
| --- | --- |
| `mcsup remote serve [--listen <addr>] [--name <n>]` | 前台运行 agent，打印名称、地址、指纹、配对码 |
| `mcsup remote serve --status` | 离线查看本机身份指纹、pid 与已配对客户端 |
| `mcsup remote serve --stop` | 按 `{data_dir}/remote/agent.pid` 终止前台进程 |
| `mcsup remote revoke <client>` | 离线撤销某个客户端（删除名册记录，立即失效） |

### 连接端（本地主机）

一条命令，两种接入方式：

**推荐：SSH 引导（复用已有 SSH 信任）**

```
$ mcsup remote connect user@host
```

本地经 SSH 在远端执行 `mcsup remote __serve-ssh`，从 stdout 读回 agent 的
地址、证书指纹与配对码，再起一条 `ssh -N -L` 隧道把本地回环端口转到该地址。
用户不需要在远端开端口、不需要抄配对码，也不需要额外分发凭证——能登上这台
机器的 SSH 就是授权。首次连接仍需确认证书指纹（TOFU），SSH 只证明「能连上」，
不证明「agent 是同一台机器上的那个」；确认后交互终端会提示为这台远端命名，
默认取最小的空闲序号 `r1` / `r2` …。

隧道与远端 agent 都是**按命令临时起、随命令结束**：

- 每条需要远端的命令（`remote ping`、`server ls`、`backup create` …）各自
  引导一次，同一进程内的多处使用共用一条会话；
- 远端 agent 监听随机端口，随 SSH 会话结束而退出（本地进程一退出，它持有的
  管道关闭，远端读到 EOF 即收摊），不留常驻进程与孤儿端口；
- 本地 `ssh -N -L` 子进程由入口的退出守卫收掉，不会残留。

**直连 + 配对码**

```
$ mcsup remote connect host:port --code 4821-9375
```

交互终端下省略 `--code` 会提示输入；首次连接会展示证书指纹并要求确认，
随后提示为这台远端命名（默认取最小的空闲序号 `r1` / `r2` …，回车即接受），
最后才要求配对码。非交互（脚本 / 管道）下不进入命名提示，直接取该默认序号；
指纹确认仍需加 `-y`。直连接入要求远端 `remote serve` 常驻并可达
（自管端口 / 防火墙自行处理）。

**尚未实现**

- `mcsup remote connect`（无参数局域网发现）会报用法错误，提示改用
  `host[:port]` 或 `user@host`。

成功后输出同步摘要：

```
已连接 prod-box
  同步 3 个工作区、5 台服务器、4 个 Java 运行时
  指纹 sha256:…（已保存）
```

| 命令 | 说明 |
| --- | --- |
| `mcsup remote connect <host[:port]> [--code <码>] [-y]` | 配对远端（直连） |
| `mcsup remote connect --name <n>` | 指定远端名并跳过交互命名 |
| `mcsup remote connect --read-only` | 以只读权限配对（viewer） |
| `mcsup remote reconnect <name>` | 复用记录重新认证；指纹变化要求重新 connect |
| `mcsup remote list` | 已配对远端、连接状态、角色、agent 版本 |
| `mcsup remote status [<name>]` | 单个远端的详细信息（缺名等价 list） |
| `mcsup remote ping <name>` | 连通性 / 版本 / 能力，供脚本使用 |
| `mcsup remote disconnect <name>` | 删除本地配对与凭证，远端不受影响 |

`connect` 幂等：重复执行等于重新握手并刷新 token 与指纹。

### 日常：命令完全不变

```
$ mcsup workspace ls
NAME      ORIGIN     CURRENT   PATH
survival  local      yes       /home/me/mc
creative  local
web       prod-box             /srv/mc/web

$ mcsup server ls
NAME   ORIGIN    STATUS
surv   local     running
web    prod-box  stopped

$ mcsup server start web          # 自动路由到 prod-box
$ mcsup backup create web         # 同上
$ mcsup java ls                   # 本地与远端合并
```

`ORIGIN` 列就是「标记为远程」的全部形态：默认可见，不需要额外参数，也不
改变任何命令。

### 按来源过滤

`workspace ls` / `server ls` / `java ls`（以及后续的 `backup ls` 等列表
命令）统一支持 `--origin`：

| 取值 | 含义 |
| --- | --- |
| `all`（缺省） | 本地 + 全部已配对远端 |
| `local` | 仅本地 |
| `remote` | 全部远端，不含本地 |
| `<name>` | 指定远端 |

```
mcsup workspace ls --origin local
mcsup workspace ls --origin prod-box
mcsup server ls --origin remote
mcsup java ls --origin all
```

约定：

- 缺省值可经环境变量 `MCSUP_ORIGIN` 覆盖，优先级 `flag > 环境变量 > all`。
- `all` / `local` / `remote` 是保留字，不能用作远端名；远端名禁止包含 `/`。
- 过滤只影响展示与返回集合，不改变命令语义；过滤后为空是成功（退出码 0）。
- 人类可读输出增加 `ORIGIN` 列；JSON 输出每项增加 `origin` 字段，其余
  形状不变。
- 当前实现**没有本地缓存**：合并列表是实时查询，远端不可达时该来源的
  条目省略并记 warn，本地条目照常返回（离线缓存与 `(offline)` 标记待补）。
  不可达远端只等**一次**建连尝试：建连失败不重试，避免把等待翻倍。

### 限定名

资源名在本地与所有远端之间可能重名。解析顺序：

1. `origin/name` 限定名（如 `prod-box/web`、`local/web`）精确命中；
2. 不加限定且全局唯一 → 直接使用；
3. 不唯一 → 报错并列出候选。

```
$ mcsup server start web
error: “web” 存在于多个位置
  local/web
  prod-box/web
提示: 用 local/web 或 prod-box/web 指定
```

限定名已接到所有接受服务器名的命令（`start` / `stop` / `status` / `send` /
`logs` / `backup` / `install` 等）。仍按裸名交给 `service` 的命令（`start` /
`stop` / `status` / `send`）在本机与远端同名时会明确拒绝远端限定名（避免落到
本地同名服务器）；按对端 id 寻址的命令（`install` / `props` / `set` /
`console` / `logs`）不受影响，可直接用 `prod-box/web` 精确指定远端那台。

### 创建落点

创建类命令（`workspace init`、`server create`）按 `--origin` 决定落点：

- `--origin <远端名>` 直接在对端执行；缺省本地。
- `workspace init --origin prod-box --dir /srv/mc/w2` 中的 `--dir` 是**目标
  机器上的路径**，由该机器的 agent 执行同一套 `WorkspaceService::init`。
- `server create --origin prod-box` 在对端完成「登记 + 核心安装」；加
  `--modpack <项目>` 时改为对端读清单、装加载器、铺 overrides
  （`ServerCreateModpack` 一次帧完成），`--version` 是整合包版本。
- `server remove --origin prod-box` 在对端注销。
- `java scan --origin prod-box` 让对端扫描并登记本机 JVM。

### 远端能力覆盖

远端写操作的覆盖范围是「协议已定义的帧」，逐个命令看：

| 命令 | 远端执行方式 |
| --- | --- |
| `workspace init` / `use` / `remove` | `WorkspaceInit` / `WorkspaceUse` / `WorkspaceRemove`，目录动作全在对端 |
| `server create` | 普通变体：`ServerCreate` + `ServerInstall`；`--modpack`：`ServerCreateModpack` |
| `server install` | `ServerInstall`；支持裸名指向唯一远端，缺版本时沿用在册版本 |
| `server props` / `set` | `PropsGet` / `PropsSet`（原文进出）/ `SettingsSet`；判定与持久化在对端 |
| `server console` / `logs` | `LogsFollow` 长轮询 + `SendLine` / `StatusMany`（按对端 id，不按名字） |
| `server start` / `stop` / `restart` / `status` / `send` | 生命周期帧，经 `RoutingSupervisor` 按来源分派 |
| `server version` | 组合注册表 `get`（不新增帧） |
| `server backup ...` | 备份帧族（快照元数据跨线，CAS 在对端） |
| `java scan` / `install` / `uninstall` | `JavaScan` / `JavaInstall` / `JavaUninstall`，下载与登记在对端 |
| `java list` / `server list` / `workspace list` | 实时合并查询，逐台远端 `list` |

仍只有本地实现的：`server link`、`server mod`、`modpack` 的独立子命令
（尚未提供），以及 `server create --origin` 的交互向导（远端创建要求显式
`--version` 或 `--modpack`，不读本地变体 / 版本列表）。

### 离线、断开与撤销

- 远端掉线：合并列表省略该来源并记 warn（缓存落地后会改为返回缓存并标记
  `(offline)`）；写操作明确失败；本地资源完全不受影响。
- `mcsup remote disconnect <name>` 删除本地配对记录与凭证；远端服务继续运行。
- `mcsup remote revoke <client>` 在 serve 端离线删除名册记录，立即失效该
  客户端的 token。
- 未配对或 token 失效：命令报「未授权」（退出码 6），不自动降级到本地。

## 数据与同步

当前实现是**实时查询**：组合仓库每次读都逐台远端发 list 请求，没有本地
缓存（`connect` 的同步摘要只是打印当时的数据量）。这一段是已确认的设计
方向，缓存待实现：

- 目标缓存位置 `{data_dir}/remotes/<remote-id>/`，只读镜像远端的
  `workspaces` / `servers` / `java` / `backups` 元数据；远端身份用 `RemoteId`，
  不随改名变化。
- 每次读命令按 TTL 尝试刷新；失败回退缓存并标记 `(offline)`。
- 写命令直连远端，成功后回写或失效对应缓存条目。
- **不做双向同步**：因此没有版本向量、没有冲突合并、没有最后写入获胜。

## 安全

- **身份**：agent 首次 `serve` 生成密钥对与自签证书，打印指纹。
- **首次信任**：TOFU，确认后固定校验指纹；指纹变化必须显式重新配对。
- **传输**：必须 TLS，不提供明文模式。
- **凭证**：token 存操作系统钥匙串；无钥匙串时退到权限 `0600` 的文件。
- **配对码**：短时有效、单次使用、限速尝试，防止暴力猜测。
- **权限**：默认 operator，可 `--read-only`（viewer）；角色最小化为
  Admin / Operator / Viewer，先满足真实需求。
- **撤销**：revoke 立即失效 token 并断开现有连接。
- **审计**：agent 记录调用方、时间、操作、结果。
- **无 Shell**：agent 只暴露带类型的 RPC，不接受任意命令或任意路径拼接。
- **路径**：远端路径由 agent 按自身规则校验，客户端传入的路径不直接拼接
  执行。

## 协议与版本

- 复用 `protocol::Frame`，首帧为 `Hello { protocol_version, agent_version,
  capabilities }`。
- `PROTOCOL_VERSION` 不匹配直接拒绝，不猜测兼容；破坏性变更走新增字段 +
  版本递增。
- 能力协商决定可选能力（backup / file / java / streaming）是否可用。
- 日志 follow 用 `LogsFollow` 长轮询帧承载（断线即取消，无会话状态）；
  备份进度**不回传**——远端备份期间客户端 spinner 持续运转，事件通道的
  取舍见 `docs/remote-progress.md`。

## 实施阶段

1. `service::Config` 增加 `[remotes.*]` 与当前来源（纯本地，无网络）；
   `remote list` / `connect` / `disconnect` 先接本地配置。
2. `auth`：配对码、token、权限、撤销。`protocol`：`Hello` / `Capabilities`
   / 版本协商。
3. `apps/agent`：`mcsup remote serve`，最简支持 `Hello` / `Ping`。
4. `transport`：同步 TCP/TLS 帧，请求-响应、超时、重连。
5. `RemoteWorkspaceRepository` + `RemoteServerRepository`，打通
   `workspace ls` / `init` / `use` 与 `server create`。
6. `RemoteJavaRepository`，打通 `java ls` / `scan` / `install`。
7. `RemoteBackend`（`SupervisorPort` + `InstallerRunner`）：远端由 agent
   拉起 `mcsup __supervise`。
8. 远端备份：`RemoteBackupPort`（按服务器身份寻址、域类型出入参），
   `BackupService` 按条目来源路由——远端条目整段转发到对端执行（扫描、
   CAS、恢复写盘都在对端），跨线只走元数据帧（`WireSnapshot`），CAS 对象
   留在对端；agent 侧复用与本地 CLI 同一份 `BackupService`。
9. CLI：`remote serve/connect/reconnect/list/status/ping/disconnect/revoke`，
   `--origin` 过滤与 `ORIGIN` 列，创建 / 删除 / 扫描的远端路由，退出码 6。
10. 流式日志 follow 与备份进度（未做）；此时再评估 `transport` 是否切换为
    tonic。

第 1–9 步已完成；SSH 接入已落地（`remote connect user@host`，见「用户视角」）。
局域网发现、系统服务注册与流式仍未做。

实施进度、已完成内容与逐阶段待办记在 `docs/remote-progress.md`。

## 未决问题

1. `workspace init` 的远端路径语义：目标机器绝对路径（推荐，复刻本地
   逻辑）还是仅名称。
2. 远端 supervisor 形态：每服务器一个 `mcsup __supervise` 子进程（推荐，
   改动小）还是 agent 内嵌运行时。
3. `remote serve` 的缺省监听地址与防火墙引导。
4. 无系统钥匙串时的凭证落盘策略是否需要额外加密。

## 术语

| 术语 | 含义 |
| --- | --- |
| origin | 资源来源：`local` 或某个远端名 |
| 被连接端 / serve 端 | 运行 `mcsup remote serve` 的机器 |
| 连接端 / 客户端 | 运行 `mcsup remote connect` 的机器 |
| 只读镜像 | 远端仓库只供读（本地缓存待实现） |
