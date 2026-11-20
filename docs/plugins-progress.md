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
| M4 | 签名 / provenance / 撤销列表、Python / Java SDK、市场审核 | 未开始 |
| M4 | 性能实测与回归基线 | 部分完成（descriptor / 帧 / 分发 / 扇出已测，见「性能」） |

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

### M4：Java 能力与 minisign 签名

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

1. 签名链路剩余部分：Sigstore（cosign）证书链与身份绑定、provenance、撤销
   列表。minisign / ed25519 与「未签名默认拒绝、`--allow-unsigned` 仅本地」
   已完成。
2. 撤销列表命中时拒绝启动，并与 `quarantine` / `block` 共用状态迁移。
3. 补齐 `BENCH-001` / `002` / `005` / `006`：握手冷 / 热启动、宿主空闲内存、
   100 个非 resident 插件的安装与列表。需要让基准能拉起 fixture 子进程
   （当前 fixture 是 `mcsup-plugin` 的测试 bin，依赖包不会为基准构建它）。
4. `sdks/` 建立后再补 Python / Java SDK 与四语言 descriptor 一致性验收。
5. REST / WS、MCP 入口在其仓库落地后补投影，判定仍复用 `domain` / `service`。

## 性能

基准落位 `apps/bench/benches/plugin.rs`，跑法：

```
cargo bench -p bench --bench plugin
```

环境：Apple M4 / macOS 26.6.2 / rustc 1.98.1，release profile，criterion
默认采样（100 样本）。数值是均值，仅用于回归对比，不与不同机器比较。

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

`BENCH-001` / `002`（握手冷 / 热启动）、`BENCH-005`（宿主空闲内存）、
`BENCH-006`（100 个非 resident 插件安装与列表）尚未测量：它们需要基准拉起
真实 fixture 子进程，而 fixture 目前只是 `mcsup-plugin` 的测试 bin，Cargo
不会为依赖包的基准构建它。未测项不写「达标」结论。
