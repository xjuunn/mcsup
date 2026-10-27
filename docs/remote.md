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
2. **只读镜像，无冲突解决**：远端元数据拉到本地缓存供读，写操作直连远端。
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

它会把自身注册为系统服务（systemd / launchd），重启后自动拉起。之后远端
不再需要任何日常操作。

| 命令 | 说明 |
| --- | --- |
| `mcsup remote serve` | 启动并注册为系统服务，打印名称、地址、配对码 |
| `mcsup remote serve --stop` | 停止并移除系统服务 |
| `mcsup remote serve --status` | 本机服务状态、监听地址、已配对客户端 |
| `mcsup remote revoke <client>` | 撤销某个客户端的访问权限（立即失效） |

### 连接端（本地主机）

一条命令，三选一，按推荐程度排列：

**首选：复用 SSH 信任**

```
$ mcsup remote connect user@host
```

能 ssh 上去就证明已有权限。它顺手完成 agent 安装、服务拉起、密钥交换，
不需要新凭证，也不需要复制配对码。

**同局域网：自动发现**

```
$ mcsup remote connect
发现 1 台附近的 mcsup 机器：
  prod-box  10.0.0.5
正在连接… 输入 prod-box 屏幕上显示的配对码：4821-9375
已连接 prod-box
```

**跨网：直连**

```
$ mcsup remote connect host:port
```

成功后输出同步摘要：

```
已连接 prod-box
  同步 3 个工作区、5 台服务器、2 个备份、4 个 Java 运行时
  指纹 sha256:…（已保存）
```

| 命令 | 说明 |
| --- | --- |
| `mcsup remote connect [<user@host> \| <host[:port]>]` | 配对远端；无参数时局域网发现 |
| `mcsup remote connect --name <n>` | 覆盖自动识别的远端名 |
| `mcsup remote connect --read-only` | 以只读权限配对 |
| `mcsup remote` | 不带子命令：显示本机服务状态与已配对远端 |
| `mcsup remote list` | 已配对远端、连接状态、权限、agent 版本 |
| `mcsup remote status [<name>]` | 单个远端的详细信息 |
| `mcsup remote ping <name>` | 连通性 / 版本 / 能力，供脚本使用 |
| `mcsup remote disconnect <name>` | 删除本地配对与缓存，远端不受影响 |

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
- 远端不可达时保留缓存中的条目，来源标记追加 `(offline)`；JSON 用
  `available: false` 表达。

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

限定名可用于所有接受名字的命令（`start` / `stop` / `status` / `backup` /
`java` 选择等），因此选择性操作也不需要额外 flag。

### 创建落点

创建类命令（`workspace init`、`server create`）按当前工作区的来源决定落点：

- `mcsup workspace use prod-box/w1` 之后，`mcsup server create web` 建在
  `prod-box` 的 `w1` 下。
- 也可显式指定 `--origin <origin>`，优先于当前工作区来源。
- `workspace init --origin prod-box --dir /srv/mc/w2` 中的 `--dir` 是**目标
  机器上的路径**，由该机器的 agent 执行同一套 `WorkspaceService::init`。

### 离线、断开与撤销

- 远端掉线：列表仍返回缓存内容并标记 `(offline)`；写操作明确失败；本地
  资源完全不受影响。
- `mcsup remote disconnect <name>` 删除本地信任与缓存；远端服务继续运行。
- `mcsup remote revoke <client>` 在远端执行，立即失效该客户端的 token。
- 未配对或 token 失效：命令报「未授权」，不自动降级到本地。

## 数据与同步

本地缓存位于 `{data_dir}/remotes/<remote-id>/`，只读镜像远端的
`workspaces` / `servers` / `java` / `backups` 元数据。远端身份用生成的
`RemoteId`，不随改名变化。

- `connect` 做一次全量同步。
- 每次读命令按 TTL（缺省 30 秒）尝试刷新；失败回退缓存。
- 写命令直连远端，成功后回写或失效对应缓存条目。
- 本地写操作永不回写远端缓存。
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
- 进度、日志、备份进度后续以流式帧承载，接口沿用 `ProgressSink` /
  `EventSink` 语义。

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
   留在对端；agent 侧复用与本地 CLI 同一份 `BackupService`。备份进度与
   文件流式传输后续再评估；此时再评估 `transport` 是否切换为 tonic。

每一步可独立提交；第 1 步后已有可演示的连接状态，第 5 步后主要命令已
与本地一致。

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
| 只读镜像 | 本地缓存远端元数据，供读不供写 |
