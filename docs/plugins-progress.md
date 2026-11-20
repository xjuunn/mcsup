# 插件系统实施记录

本文件记录 `docs/plugins.md` 的实施进度、已完成内容与后续待办。设计目标、
边界与条款仍以 `docs/plugins.md` 为准，测试口径以 `docs/plugins-testing.md`
为准，此处只记「做到哪、下一步做什么」。

## 当前状态

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| M0 | 契约冻结：manifest / descriptor / 能力 / 状态 / 错误码 / 帧 | 完成（conformance 清单目录未建，见「偏差」） |
| M1 | 宿主进程、握手、帧循环、退避、空闲回收；最小能力与 CLI | 完成（Rust / TS SDK 未在本仓库提供） |
| M2 | 写能力、出站代发、契约路由、配置回滚与隔离 | 完成 |
| M3 | 生命周期、备份、文件、内容能力；六类展示投影 | 完成（REST / WS、MCP 入口不在本仓库） |
| M4 | `java` 能力 | 完成 |
| M4 | 签名 / provenance / 撤销列表、Python / Java SDK、市场审核 | 部分完成（minisign 与撤销列表已完成；provenance / Sigstore / SDK / 市场审核未开始） |
| M4 | 性能实测与回归基线 | 完成（`BENCH-001`~`006` 全部落位，见「性能」） |

## 已完成

### M0：契约冻结

- `domain::plugin`：`manifest`（能力、订阅、范围、来源、版本）、`capability`
  （15 项闭集）、`descriptor`（op 表、schema 内联、`display` 六型校验与
  引用校验）、`state`（9 状态迁移）、`error`（11 个稳定错误码）、`config`。
- `protocol::plugin`：帧编解码、`call` / `result` / `event` / `contract`、
  根调用 `trace` 分配、路由（广播 / 单播 / `target`）。
- 数值上限集中在 `domain` / `protocol` 常量，文档与常量同源。

### M1：宿主与最小能力

- `crates/plugin`：进程拉起、端点、握手（失败诊断含退出方式与日志尾部）、
  心跳、帧循环、退避、空闲回收、隔离 / 熔断。
- `service::plugin::HostApiImpl`：`ServerRead` / `ConsoleIo`。
- `storage`：`PluginStore` / `KvStore` / `SecretStore` / `AuditSink`。
- `provider`：`ArtifactSource`（`from` / `url` / GitHub `git`）、
  `ArtifactVerifier`（摘要校验；签名未接入）。
- CLI：`list` / `install` / `enable` / `disable` / `status` / `call` /
  `ops` / `logs` / `config` / `grant` / `revoke` / `restart` / `rollback` /
  `block` / `unblock` / `quarantine` / `release` / `doctor` 等。

### M2：写能力、出站、契约与配置

- 能力：`console:write` / `net:outbound` / `secret:rw`。
- `net:outbound`：域名白名单 + SSRF 防护（私网 / 回环 / 链路本地 / CGNAT /
  元数据地址拒绝，手动重定向 ≤ 3 跳且逐跳重校验，响应 ≤ 8 MiB）。
- 契约路由：广播不允许 `target`；单播多提供方必须指定；调用链按
  `(plugin, contract)` 检循环、深度 ≤ 4、扇出 ≤ 32；每次投递写审计。
- 配置帧、回滚、`degraded` 状态；能力拉黑（`block`）与隔离
  （`quarantine`）即时停止且不自动恢复。

### M3：平台写操作与六类展示

- `server:lifecycle`：`start` / `stop` / `restart`，服务器状态判定收敛在
  `domain::server::status`，插件层只映射错误。
- `backup`：`create` / `list` / `restore` / `verify` / `prune` / `delete`，
  错误按 `BackupError::category()` 映射。
- `files:read` / `files:write`：相对路径经 `domain::path::RelPath` 校验，
  canonicalize 前缀比较；`mcsup.json` 写删一律 `denied`，运行中启动文件
  写删 `conflict`；读写限额 16 MiB。
- `content`：`list` / `install` / `remove`，走 `ModService` 的
  `plan_install` + `install`，运行态判定与 CLI 共用一套。
- 六类展示：`form` / `table` / `detail` / `chat` / `log` / `metrics` 的形状与
  引用校验在 `domain::plugin::descriptor`，CLI `plugin ops` 只做投影。

### M4：Java 能力、minisign 签名与撤销列表

- `java`：`list` / `install` / `remove` 三个帧，实现落
  `service::plugin::HostApiImpl`，安装固定 `jdk` 包类型、厂商交给来源排序，
  与 CLI `java install` 默认路径一致。
- 错误映射：未登记 → `not_found`，扫描 / 上游 / 下载 → `unavailable`，
  大版本越界 → `invalid_input`。
- `provider::plugin_sig`：minisign 公钥 / 签名解析、ed25519 验签（含全局
  签名与 trusted comment）、信任库加载与 Unix 权限检查（目录 `0700`、
  公钥 `0600`）。签名对象是压缩包本体：本地读 `<file>.minisig`，URL 取
  `<url>.minisig`（404 视为未签名）。
- 未签名策略收敛在 `service`：本地来源可 `--allow-unsigned`（记录
  `signed = false` / `untrusted`），远端来源无签名一律拒绝；`plugin update`
  沿用原产物的信任姿态。`plugin verify` 对未安装的压缩包重跑来源校验。
- 撤销列表：判定（id 精确匹配、空版本 = 全部、非法条目整体拒绝）在
  `domain::plugin::revocation`，缓存读写与权限（`0600`、原子替换、拒绝符号
  链接）在 `provider::plugin_revoke`，经 `port::plugin::PluginRevocations` 注入
  `service`。`enable` / `start` 在状态变更前拒绝命中版本；列表不可用时只记
  `warning`，`plugin doctor` 报出。CLI 为 `plugin revocations [import|fetch]`。

## 与设计文档的偏差

以下偏差来自本仓库当前形态，不是实现缺失：

| 设计文档要求 | 仓库现状 |
| --- | --- |
| 入口含 `apps/server`、`apps/mcp` | 仓库只有 `apps/agent` / `bench` / `cli` / `dev`；REST / WS、MCP 投影无法在本仓库实现 |
| `sdks/` 四语言 SDK 与 `sdks/conformance` 用例登记 | 无 `sdks/` 目录；测试以 Rust 单测 / 集成测试承载，用例 ID 写在测试函数名或注释里 |
| `scripts/check-plugin-tests.sh` 汇总用例 ID | 脚本不存在；用例由 `scripts/check.sh` 的常规 `test` 步骤覆盖 |
| M1 交付 Rust / TS SDK | 未提供 SDK 包，只有测试 fixture 插件 |

## 待办

按依赖顺序：

1. 签名链路剩余部分：Sigstore（cosign）证书链与身份绑定、provenance。
   minisign / ed25519、撤销列表与「未签名默认拒绝、`--allow-unsigned` 仅本地」
   已完成。
2. 撤销列表的自动拉取：当前只能 `plugin revocations fetch` 手工更新，尚无内置
   定时任务与上游地址配置（需市场侧先确定地址与格式）。
3. `sdks/` 建立后再补 Python / Java SDK 与四语言 descriptor 一致性验收。
4. REST / WS、MCP 入口在其仓库落地后补投影，判定仍复用 `domain` / `service`。

## 性能

基准分三个目标，互不依赖：

| 目标 | 覆盖 | 判定方式 |
| --- | --- | --- |
| `benches/plugin.rs` | descriptor、帧、分发、扇出、列表 | criterion 均值，回归对比用 |
| `benches/plugin_handshake.rs` | 冷 / 热启动握手 | 自采样 30 次算 p95，超预算退出码非 0 |
| `benches/plugin_memory.rs` | 宿主空闲内存 | 独立宿主进程 RSS |

```
cargo bench -p bench --bench plugin
cargo bench -p bench --bench plugin_handshake
cargo bench -p bench --bench plugin_memory
```

环境：Apple M4 / macOS 26.6.2 / rustc 1.98.1，release profile。criterion 用
默认采样（100 样本）报均值；握手按阈值定义取 p95（预热 5 次、采样 30 次）。
数值仅用于回归对比，不与不同机器比较。

单进程内的热路径：

| 用例 | 对应目标 | 实测 | 结论 |
| --- | --- | --- | --- |
| descriptor 校验 1 op | 安装路径 | 3.02 µs | — |
| descriptor 校验 20 op | 安装路径 | 54.4 µs | — |
| descriptor 校验 100 op | 安装路径 | 273.9 µs | — |
| call 帧写入 + 读回 | 每帧编解码 | 16.07 µs | — |
| result 帧写入 + 读回 | 每帧编解码 | 15.87 µs | — |
| `server:read.list` 分发 | `BENCH-003`（< 5 ms） | 1.73 µs | 达标 |
| 事件扇出 1 订阅者 | `BENCH-004`（≥ 5000/s） | 542.7 ns ≈ 1.84M/s | 达标 |
| 事件扇出 10 订阅者 | `BENCH-004` | 544.5 ns ≈ 1.84M/s | 达标（订阅者为空实现，只测分发开销） |
| 100 个非 resident 插件列表 | `BENCH-006`（< 1 s） | 1.82 ms | 达标（真实文件后端） |

带真实子进程的项：

| 用例 | 对应目标 | 实测 | 结论 |
| --- | --- | --- | --- |
| 冷启动握手 | `BENCH-001`（p95 < 500 ms） | p50 287.0 ms / p95 319.1 ms | 达标（每次新拷贝 fixture，含首次执行校验） |
| 热启动握手 | `BENCH-002`（p95 < 150 ms） | p50 26.6 ms / p95 28.4 ms | 达标 |
| 宿主空闲内存（空） | `BENCH-005`（< 40 MiB） | 5.8 MiB | 达标 |
| 宿主空闲内存（100 条记录） | `BENCH-005` | 6.2 MiB | 达标，且无会话进程常驻 |

口径与已知偏差：

- 冷启动样本的 max 到过 604.9 ms（p95 仍达标）：首次执行的可执行校验在本机
  有长尾，属操作系统成本，不是握手逻辑。
- `BENCH-006` 只量「列表」，安装写盘在同一条 `put` 路径上单独耗时可忽略，
  未单列。
- `BENCH-005` 量的是宿主库进程（`mcsup` + `plugin` + `storage`），不含 CLI /
  TUI 自身的常驻部分：那部分与插件无关。
