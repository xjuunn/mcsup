# 远程管理实施记录

本文件记录 `docs/remote.md` 的实施进度、已完成内容与后续待办。设计目标与
边界仍以 `docs/remote.md` 为准，此处只记「做到哪、下一步做什么」。

## 当前状态

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| 1 | `port` 身份键改造（`ServerRef`、`status_many`） | 完成 |
| 2 | `service` 来源维度、`[remotes.*]` 配置 | 完成 |
| 3 | `auth`：配对码 / token / 权限 / 撤销 | 完成 |
| 4 | `protocol`：远端帧与版本协商 | 待开始 |
| 5 | `transport`：TLS 帧传输与连接池 | 待开始 |
| 6 | `apps/agent`：`mcsup remote serve` | 待开始 |
| 7 | 远端仓库（workspace / server / java） | 待开始 |
| 8 | `RemoteBackend`：远端 supervisor | 待开始 |
| 9 | 流式：日志 / 进度 / 备份 | 待开始 |
| 10 | CLI `remote` 命令与 `--origin` | 待开始 |

## 已完成

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
  `0600`）。钥匙串后续以同一 trait 的另一实现补上，本次不引入 `keyring`。

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

## 后续待办

### 阶段 4：`protocol` 远端帧

- 独立 `REMOTE_PROTOCOL_VERSION`，复用 `protocol::Frame` 编解码。
- 帧集：`Hello`（协议版本 / agent 版本 / 能力）、`Enroll`（配对码换 token）、
  `Ping`、`StatusMany`、`Start`、`Stop`、`SendLine`、`Shutdown`。
- 版本不匹配直接拒绝，不做兼容猜测；破坏性变更走新增字段 + 版本递增。

### 阶段 5：`transport`

- TLS（`tokio-rustls` + `ring`，均已在 `Cargo.lock`），不提供明文模式。
- 自签证书用 `rcgen` 生成（已获批）。
- 请求-响应、超时、重连、连接池。
- 幂等键是 transport 层关注点，不进 `port`：`RemoteSupervisor` 每次调用生成
  UUID，重试复用同一个 UUID，agent 侧按键去重。

### 阶段 6：`apps/agent`

- `mcsup-agent`（bin 名带前缀，package 名 `agent`）。
- 首次启动生成密钥对与自签证书，打印指纹（TOFU）。
- 最简支持 `Hello` / `Ping`，再逐步开放能力。
- 审计：记录调用方、时间、操作、结果。

### 阶段 7：远端仓库

- `RemoteWorkspaceRepository` / `RemoteServerRepository` /
  `RemoteJavaRepository`，实现 `port` 的对应 trait 并覆写
  `list_sourced()`。
- 打通 `workspace ls` / `init` / `use`、`server create`、`java ls` / `scan` /
  `install`。
- `ServerRegistry::list()` 里的 `symlink_metadata()` 是本地 syscall，远端会
  静默错——远端条目走 `list_sourced()` 分支跳过。

### 阶段 8：`RemoteBackend`

- 实现 `SupervisorPort`，远端由 agent 拉起 `mcsup __supervise` 子进程。
- 用 `status_many` 批量接口，避免 N+1。

### 阶段 9：流式

- 日志、进度、备份进度以流式帧承载，接口沿用 `ProgressSink` / `EventSink`。

### 阶段 10：CLI

- `remote` 子命令：`serve` / `connect` / `reconnect` / `list` / `ping` /
  `disconnect` / `revoke`。
- `connect <target>` 首次配对，`reconnect <name>` 复用已配对记录。
- 全局 `--origin` 过滤。
- 退出码新增 6 = 未授权。

## 已确认的设计取舍

- `connect` 保留首次配对，新增 `reconnect <name>`；记录存原始 target 字符串
  + `via` 字段。
- token 绑定客户端密钥对（mTLS 降级为 enrollment 凭据），带 TTL。
- 「无常驻会话」改为「无常驻客户端进程」。
- `McsupSet` 路由替代 `McsupBuilder` 注入替换，解决「唯一装配分叉」与
  `--origin all` 合并展示的矛盾。
- 判定逻辑收敛：`Origin` / `RemoteName` / `Sourced` 在 `port`（实现层要用），
  `OriginFilter` / `Selector` / `resolve_index` 在 `service`（入口语义）。

## 测试环境

- 机器是 aarch64，无 `sudo`、无系统 java。
- 测试用 Java 装在 `~/Downloads/mcsup-test/home/.local/share/mcsup/java/temurin-21`
  （由 `mcsup java install 21 --vendor temurin -y` 安装，已验证可执行）。
- `apps/cli/tests/cli_e2e.rs` 隔离 `HOME`，靠宿主 `PATH` 找 java，跑测试前需把
  该 `bin` 目录加进 `PATH`。
