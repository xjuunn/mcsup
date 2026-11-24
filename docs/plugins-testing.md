# 平台插件测试规范

来自 `AGENTS.md`，配套 `docs/plugins.md`。

本文把 `docs/plugins.md` 的每一条规范映射到可执行用例，定义 conformance 文件
格式、fixture 行为、覆盖率门禁与里程碑验收。实现插件相关代码时，先读设计
文档，再按本文写测试；没有测试 ID 的规范条款视为未实现。

---

# 阅读约定

| 标记 | 含义 |
| --- | --- |
| `[必须]` | 合并前必须存在并通过 |
| `[禁止]` | 不得以「时间不够」为由跳过 |
| `[目标]` | 依赖尚未落地的入口（如 `apps/server`），先冻结契约与 fixture |

冲突处理：设计语义以 `docs/plugins.md` 为准；测试如何断言以本文为准。两者
冲突时先改文档，再改测试，不允许只在测试里形成新语义。

---

# 1. 目标与非目标

## 1.1 目标

| 目标 | 可验证的结果 |
| --- | --- |
| 条款全覆盖 | 每条硬规则与关键小节至少一个测试 ID，追踪矩阵无空项 |
| 行为可复现 | 失败能用一条命令复现，不依赖人工点击或真实外网 |
| 协议字节稳定 | 同一 conformance 输入在四种 SDK 与宿主上产出逐字节一致的结果 |
| 失败路径覆盖 | 每个成功用例至少有一个对应的拒绝 / 超时 / 崩溃用例 |
| 跨平台一致 | macOS / Linux / Windows 跑同一套用例，差异只在端点实现 |
| 性能可回归 | 关键指标有基线，超出阈值即失败 |

## 1.2 非目标

- 不追求 100% 行覆盖率。追求「规范条款 100% 有对应用例」。
- 不在单元测试里访问真实网络、真实用户目录、真实 Minecraft 进程。
- 不用 sleep 等待异步结果；用暂停时钟或显式同步点。
- 不测试第三方库内部行为；只测试本仓库的契约与适配。

---

# 2. 测试分层与所有权

| 层 | 运行位置 | 负责 crate | 典型耗时 | 是否进 `check.sh` |
| --- | --- | --- | --- | --- |
| 单元 | 同进程 | `domain` / `protocol` / `port` | 毫秒 | 是 |
| 契约 | 同进程 + golden 文件 | `protocol` / `sdks/*` | 毫秒 | 是（Rust）/ `check-sdk.sh` |
| 集成 | 真实子进程 + 真实端点 | `plugin` | 秒 | 是 |
| 端到端 | CLI / API / MCP | `apps/*` | 秒到分钟 | 是（CLI）；入口落地后补 |
| 安全 | 恶意产物 / 恶意帧 | `plugin` / `service` / `provider` | 秒 | 是 |
| 性能 | 基准 | `apps/bench` | 分钟 | 否，里程碑手动跑 |

所有权规则：

- `domain` 的规则测试不得依赖 `plugin`：纯类型、纯函数、无 IO。
- `plugin` 的集成测试不得依赖 `service`：用 `MockHostApi` 注入。
- `service::plugin` 的测试不得拉起真实插件：用 `MockSupervisor` 注入。
- CLI 测试不得依赖真实数据目录：用临时目录 + `--data-dir`。
- SDK 测试不得依赖宿主：用 `MockHost` / 内存双工。

`[禁止]` 在 `apps/cli` 的测试里复制业务判定；判定必须在 `domain` / `service`
有对应单测。

---

# 3. 测试 ID 与报告

## 3.1 命名

格式：`<组>-<域>-<三位序号>`，例如 `HOST-EP-003`、`SEC-ZIP-007`。

| 前缀 | 范围 |
| --- | --- |
| `DOM-MAN` | manifest 校验 |
| `DOM-DES` | descriptor 校验 |
| `DOM-CAP` | 能力集合与交集 |
| `DOM-STA` | 状态机 |
| `DOM-CON` | 契约标识解析 |
| `DOM-EVT` | 事件目录与映射 |
| `PROTO-FRM` | 帧编解码 |
| `PROTO-HSK` | 握手 |
| `PROTO-CALL` | 调用 / 结果 |
| `PROTO-STR` | 流 |
| `PROTO-CFG` | 配置 |
| `PROTO-CTL` | 控制通道 |
| `PROTO-VER` | 版本协商 |
| `HOST-PRC` | 进程生命周期 |
| `HOST-EP` | 端点 |
| `HOST-HB` | 心跳 |
| `HOST-RST` | 崩溃与退避 |
| `HOST-IDLE` | 空闲回收 |
| `HOST-LIM` | 限额 |
| `HOST-EVT` | 事件扇出 |
| `HOST-LOG` | 日志采集 |
| `CAP-GATE` | 能力门 |
| `API-*` | HostApi 各能力 |
| `CON-*` | 契约路由与载荷 |
| `INST-*` | 安装、校验、升级 |
| `CLI-*` | 命令行契约 |
| `WEB-*` | descriptor 投影与 REST / WS |
| `SDK-*` | 四种 SDK |
| `SEC-*` | 安全与恶意输入 |
| `PLAT-*` | 平台差异 |
| `E2E-*` | 跨组件流程 |

## 3.2 报告

每个测试必须能被报告为：

```json
{"id":"HOST-RST-003","status":"pass","duration_ms":12}
```

CI 汇总后与 `sdks/conformance/cases.json` 的 ID 集合比对：

- 缺少 ID → 失败（规范条款没人测）。
- 多出 ID → 失败（用例未登记，无法追踪）。
- `status != pass` → 失败。
- `ignored` / `skipped` → 失败，除非在 `cases.json` 标记 `platform` 且当前平台
  不在矩阵内。

`[禁止]` 用 `#[ignore]` 或 `test.skip` 长期隐藏失败用例。临时跳过必须带
`reason` 与到期日期，并在里程碑验收前清零。

## 3.3 目录

```
sdks/conformance/
├── cases.json                  # 用例登记（唯一清单）
├── schema/                     # 类型 → descriptor 片段
├── frames/                     # 帧输入与期望输出
├── behavior/                   # 行为用例（JSON）
├── render/                     # descriptor → 渲染计划
├── fixtures/                   # 合法 / 非法 manifest、descriptor、产物
└── report/                     # CI 产物，不提交
```

Rust 侧用例放在 `crates/plugin/tests/`，ID 写进测试函数名或属性：

```rust
#[test]
fn host_ep_003_socket_permissions_are_0600() { ... }
```

TypeScript 侧用 `test("PROTO-FRM-004 ...")`，由报告器解析前缀。

---

# 4. 需求追踪矩阵

每一行是 `docs/plugins.md` 的一条规范。`[必须]` 表示合并前必须有对应用例。

## 4.1 硬规则

| 规则 | 测试 ID | 断言 |
| --- | --- | --- |
| H1 外部进程 | `PLAT-001` `HOST-PRC-001` | 只有进程形态；动态库入口被 manifest 校验拒绝 |
| H2 单一端点 | `HOST-EP-002` `SEC-PATH-004` | 插件只能经端点；直接读 `db/` 的 fixture 无法获得数据 |
| H3 安装不执行代码 | `INST-PKG-010` `SEC-ZIP-009` | 含 `preinstall` / `build.rs` / 可执行脚本的产物不产生任何子进程 |
| H4 三方交集 | `DOM-CAP-003` `CAP-GATE-004` | 缺任一集合即不授予 |
| H5 不是沙箱 | `DOM-MAN-014` `CLI-OUT-009` | 文档 / CLI / 市场文案不含「sandbox / 沙箱」承诺 |
| H6 判定在 domain | `DOM-STA-001` `DOM-CAP-001` | 入口层无状态迁移与能力判定 |
| H7 禁止直连 | `CON-RTE-001` `SEC-PATH-005` | 插件间 socket / 文件共享被审计标记；契约只能经宿主 |
| H8 契约闭集 | `DOM-CON-004` `INST-PKG-006` | 未知契约安装失败 |
| H9 descriptor 构建时生成 | `SDK-GEN-002` `SDK-GEN-003` | 运行时无生成路径；`--dump` 不连接宿主 |
| H10 宿主校验输入 | `API-SRV-002` `PROTO-CALL-002` | 非法输入在宿主侧被拒，插件 handler 未被调用 |
| H11 单帧 24 MiB | `HOST-LIM-001` `PROTO-FRM-006` | 24 MiB 通过，24 MiB + 1 字节拒绝 |
| H12 禁止热替换 | `HOST-PRC-012` `INST-UPG-004` | 升级先停进程，不原地替换运行中的文件 |
| H13 升级重置授权 | `INST-UPG-003` | 新增能力 → `pending_approval` |
| H14 单实例 | `HOST-PRC-004` `PLAT-004` | 同 `(id, scope, server)` 第二个实例被拒 |
| H15 统一 op 表 | `CLI-CMD-010` `WEB-REST-006` | CLI / REST / MCP 对同一 op 的输入输出一致 |
| H16 日志不走协议 | `HOST-LOG-001` `HOST-LOG-005` | stdout 混入 JSON 不会产生帧 |
| H17 密钥只写不读 | `API-SEC-002` `CLI-OUT-006` | 读取返回 `***`；审计无明文 |
| H18 卸载清数据 | `INST-UPG-007` `INST-UPG-008` `CLI-CMD-016` `CLI-CMD-027` | 默认连数据与日志一起删；`--keep-data` 保留目录但删密钥；`--keep-secrets` 连密钥一起留；无法确认时缺 `--yes` 拒绝 |
| H19 版本独立 | `PROTO-VER-001` `SDK-GEN-008` | 四个版本常量可独立递增，互不牵连 |
| H20 进度回显 | `CLI-UI-002` `CLI-UI-004` | 长步骤有 spinner；JSON 模式无进度 |
| H21 插件不写前端 | `DOM-DES-018` `WEB-RENDER-001` | 产物含 `.html/.css/.js` 被拒 |
| H22 离线校验 | `INST-VER-002` | 摘要 / 平台不匹配时拒绝；不校验签名 |
| H23 条款有测试 | `PLAT-011` | 报告 ID 集合等于 `cases.json`；矩阵无空项 |

## 4.2 关键小节

| 小节 | 测试 ID | 断言 |
| --- | --- | --- |
| 4.4 模块落位 | `PLAT-002` | crate 依赖方向符合架构脚本 |
| 4.5 HostApi 反转 | `PLAT-003` | `plugin` 不依赖 `service`，能力经 trait 注入 |
| 4.6 事件注入 | `HOST-EVT-001` | `McsupBuilder::events` 注入的 sink 收到领域事件 |
| 4.7 宿主单实例 | `HOST-PRC-004` | 锁文件由内核持有，崩溃后自动释放 |
| 5.1 令牌不进 argv | `HOST-PRC-006` | 进程命令行不含令牌 |
| 5.2 端点回退 | `HOST-EP-005` `PLAT-005` | 长路径 / Windows 回退 TCP，且只绑回环 |
| 5.3 日志限速 | `HOST-LOG-004` | 超出 200 行/秒被丢弃并汇总 |
| 5.4 限额 | `HOST-LIM-001..010` | 帧、队列、流、超时逐项 |
| 5.5 空闲回收 | `HOST-IDLE-001..006` | 非 resident 停止；resident 保留 |
| 5.6 崩溃退避 | `HOST-RST-001..008` | 1/2/4/8/16 s；第 5 次 `failed` |
| 6.3 握手顺序 | `PROTO-HSK-001..012` | 先 `hello` 后 `welcome`；越界能力不授予 |
| 6.4 错误码 | `PROTO-CALL-005` | 11 个错误码的 `code/category/retryable` 映射 |
| 6.1 解码上限 | `PROTO-FRM-006` `PROTO-FRM-012` | 24 MiB 与深度 32 在反序列化前被拒 |
| 6.5 流缺口 | `PROTO-STR-004` | `seq` 跳号即终止 |
| 6.6 配置回滚 | `PROTO-CFG-003` | 插件回错后恢复旧 revision |
| 6.9 控制通道 | `PROTO-CTL-001..010` | CLI / server / agent 共用同一帧集 |
| 7.3 状态迁移 | `DOM-STA-002` | 全状态 × 事件矩阵，允许 / 禁止逐项 |
| 8.2 能力目录 | `DOM-CAP-005` `CAP-GATE-001` | 15 项能力（含不开放项）逐一校验 |
| 8.3 风险映射 | `DOM-CAP-010` `WEB-AUTH-003` | read/mutating/destructive → 角色 |
| 8.5 授权生命周期 | `INST-UPG-002` `CAP-GATE-008` | 升级 / 撤销 / 禁用 |
| 9.2 HostApi 限额 | `API-KV-003` `API-NET-005` | 超限返回 `limit_exceeded`，不截断成功 |
| 9.3 SSRF | `SEC-SSRF-001..010` | 私网、元数据、重定向、DNS 重绑 |
| 10.3 manifest 校验 | `DOM-MAN-001..020` | 逐字段合法 / 非法 |
| 11.3 展示类型 | `DOM-DES-010..017` | 六种类型形状与引用一致性 |
| 11.5 schema 子集 | `DOM-DES-020..030` | 允许 / 禁止 / 限值 |
| 11.6 扩展键 | `DOM-DES-031..034` | secret 仅输入；format 仅字符串 |
| 11.7 归一化 | `SDK-GEN-004..008` | 四语言逐字节一致 |
| 12.4 契约规则 | `CON-RTE-001..012` | 广播 / 单播 / 循环 / 深度 / 超时 |
| 12.5 契约版本 | `CON-RTE-010` | `@1` 与 `@2` 并存路由 |
| 13.3 渲染规则 | `WEB-RENDER-001..012` | 转义、密钥、分页、降级 |
| 13.4 REST 契约 | `WEB-REST-001..012` | 路径、状态码、DTO |
| 14.3 版本规则 | `SDK-GEN-009` | SDK 主版本 == plugin API 主版本 |
| 15.4 安装流水线 | `INST-PKG-001..015` | 解析到登记的全链路 |
| 15.6 升级回滚 | `INST-UPG-001..010` | 失败自动回滚 |
| 16.2 审核清单 | `SEC-SIG-006` | 自动审核项逐条（不含签名） |
| 18.3 退出码 | `CLI-EXIT-001..010` | 七类错误 + 插件特有场景 |
| 19.1 审计 | `HOST-EVT-007` `CON-RTE-011` | 字段完整、无明文 |
| 20.5 性能 | `BENCH-001..006` | 握手、调用、扇出、内存、列表 |
| 20.0 条款追踪 | `PLAT-011` | 报告 ID 集合等于 `cases.json`，缺项多项均失败 |

# 5. Conformance 规范

## 5.1 唯一清单

`sdks/conformance/cases.json` 是全部跨语言用例的唯一登记处。任何 SDK 或宿主
新增用例，先登记，再实现。

```json
{
  "version": 1,
  "cases": [
    {
      "id": "PROTO-FRM-001",
      "kind": "frames",
      "input": "frames/hello_minimal.jsonl",
      "expect": "frames/hello_minimal.expected.jsonl",
      "platforms": ["all"]
    },
    {
      "id": "DOM-DES-021",
      "kind": "schema",
      "input": "schema/union_rejected.json",
      "expect": "schema/union_rejected.error.json",
      "languages": ["rust", "typescript", "python", "java"]
    },
    {
      "id": "WEB-RENDER-003",
      "kind": "render",
      "input": "render/chat_descriptor.json",
      "expect": "render/chat_descriptor.plan.json"
    },
    {
      "id": "CON-RTE-005",
      "kind": "behavior",
      "input": "behavior/contract_cycle.json",
      "expect": "behavior/contract_cycle.expected.json"
    }
  ]
}
```

字段：

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `id` | 是 | 测试 ID，全局唯一 |
| `kind` | 是 | `frames` / `schema` / `behavior` / `render` / `security` |
| `input` | 是 | 相对 `conformance/` 的输入文件 |
| `expect` | 是 | 期望输出或错误 |
| `platforms` | 否 | 缺省 `["all"]`；否则列 `linux` / `macos` / `windows` |
| `languages` | 否 | `schema` 用例适用语言；缺省全部 |
| `skip_reason` | 否 | 仅允许带到期日期的临时跳过 |

## 5.2 Runner 契约

每个 SDK 提供同名入口：

```
cargo run -p mcsup-plugin-sdk-conformance -- --cases ../conformance/cases.json
npm run conformance -- --cases ../conformance/cases.json
python -m mcsup_plugin.conformance --cases ../conformance/cases.json
java -jar conformance.jar --cases ../conformance/cases.json
```

输出：

```json
{"version":1,"results":[{"id":"PROTO-FRM-001","status":"pass","duration_ms":1}]}
```

规则：

- 退出码：全部通过 0；任一失败 1；清单本身非法 2。
- 必须按 `cases.json` 顺序执行；不得静默跳过 `platforms` 不匹配以外的用例。
- 失败时附带实际输出与期望输出的 unified diff，便于定位。
- 运行器自身不得访问网络、不得读用户目录。

## 5.3 规范化

帧 golden 含易变字段，比较前必须经过唯一实现 `conformance::canonicalize`：

| 路径 | 替换 |
| --- | --- |
| `session_id` | `<session>` |
| `pid` | `<pid>` |
| `at`（任意层级） | `<timestamp>` |
| `uptime_secs` | `<uptime>` |
| `host_version` | `<host_version>` |
| `config.revision` | `<revision>` |
| `error.message` | 保留；若含 OS 文本，用例改用 `error.code` 断言 |
| `source.artifact_sha256` | `<sha256>` |

规范化同时做：

1. 递归按字典序排序对象键；
2. 去除行尾空白；
3. 统一 LF；
4. 数字用最短往返表示，禁止 `1.0` 与 `1` 混用；
5. 时间统一 RFC3339 UTC，精确到秒（需要毫秒的字段单独声明）。

`[禁止]` 在用例里写真实 PID、真实时间戳、真实 session id。含这些值的 golden
视为不可复现。

## 5.4 帧 transcript

`frames/*.jsonl` 用方向前缀描述一次会话：

```
# 握手
< {"v":1,"type":"hello","token":"<token>","api":1,"plugin_id":"fixture",...}
> {"v":1,"type":"welcome","session_id":"<session>","granted":["events"],...}
# 调用
> {"v":1,"type":"call","id":7,"op":"echo","input":{"text":"hi"}}
< {"v":1,"type":"result","id":7,"ok":true,"output":{"text":"hi"}}
```

规则：

- `<` 是插件 → 宿主，`>` 是宿主 → 插件。
- `#` 开头为注释；空行忽略。
- 一行一帧；不得跨行。
- 未写的方向表示「不发送」；测试断言不得凭空多出帧。
- 结束用 `# EOF` 标记，表示主动关闭端点。

## 5.5 Fixture 插件

`wire-fixture` 是手写实现，不依赖 SDK，放在 `crates/plugin/tests/fixtures/`。
它只实现帧循环，用于验证宿主不依赖 SDK 的行为。

| 模式 | 行为 | 覆盖用例 |
| --- | --- | --- |
| `ok` | 正常握手，回显 `echo` | 基线 |
| `sleep-handshake` | 连接后 30 s 才发 `hello` | `HOST-PRC-007` |
| `reject-api` | `hello.api=2` | `PROTO-HSK-006` |
| `deny-token` | 发送错误令牌 | `PROTO-HSK-005` |
| `unknown-frame` | 握手后发未知 `type`，继续工作 | `PROTO-FRM-010` |
| `oversize` | 发 24 MiB + 1 字节帧 | `HOST-LIM-001` |
| `crash-start` | 握手前退出码 1 | `HOST-PRC-008` |
| `crash-after` | 握手成功后立即崩溃 | `HOST-RST-001` |
| `ignore-ping` | 不回复 `ping` | `HOST-HB-002` |
| `ignore-cancel` | 收到 `cancel` 后继续发流 | `PROTO-STR-006` |
| `stream-gap` | `seq` 从 0 跳到 2 | `PROTO-STR-004` |
| `slow-op` | `echo` 延迟 5 s | `HOST-LIM-006` |
| `bad-config` | 对 `config` 回 `ok:false` | `PROTO-CFG-003` |
| `stdout-json` | 向 stdout 打印完整 JSON 帧 | `HOST-LOG-005` |
| `stdout-flood` | 1 s 打 1000 行 | `HOST-LOG-004` |
| `exit-2` | 收到 `shutdown` 后退出码 2 | `HOST-PRC-011` |
| `reentry` | handler 内调用宿主 `server.list` | `HOST-PRC-013` |
| `secret-leak` | 把配置里的密钥打到 stdout | `HOST-LOG-006` |
| `event-drop` | 不读事件直到队列溢出 | `HOST-EVT-005` |
| `multi-instance` | 启动后尝试再连同一端点 | `HOST-PRC-004` |

`sdk-fixture` 用 Rust SDK 编写，验证 SDK 自动处理握手、心跳、取消、退出码；
它与 `wire-fixture` 跑同一组行为用例，结果必须一致。

---

# 6. 单元测试矩阵

## 6.1 manifest（`DOM-MAN`）

| ID | 输入 | 期望 |
| --- | --- | --- |
| `DOM-MAN-001` | 最小合法 manifest | `Ok`，字段与默认值 |
| `DOM-MAN-002` | `id` 含大写 / 下划线 / 空格 | `invalid_id` |
| `DOM-MAN-003` | `id` 为保留名（`mcsup` / `core` / `mod`） | `reserved_id` |
| `DOM-MAN-004` | `version` 非 SemVer | `invalid_version` |
| `DOM-MAN-005` | `api` 高于宿主 | `unsupported_api` |
| `DOM-MAN-006` | `schema` 不是 1 | `unsupported_schema` |
| `DOM-MAN-007` | `entry.cmd` 为绝对路径 | `invalid_entry` |
| `DOM-MAN-008` | `entry.cmd` 含 `..` | `invalid_entry` |
| `DOM-MAN-009` | `entry.type` 不是 `process` | `unsupported_entry` |
| `DOM-MAN-010` | 未知能力 | `unknown_capability` |
| `DOM-MAN-011` | 未知订阅 | `unknown_subscription` |
| `DOM-MAN-012` | 订阅非空但无 `events` | `missing_capability` |
| `DOM-MAN-013` | `console:line` 但无 `console:read` | `missing_capability` |
| `DOM-MAN-014` | 能力列表含 `sandbox` 之类伪能力 | `unknown_capability` |
| `DOM-MAN-015` | `net.allow` 非空但无 `net:outbound` | `missing_capability` |
| `DOM-MAN-016` | `scope=server` 提供 `metrics.sample@1` | `scope_conflict` |
| `DOM-MAN-017` | `provides` 未知契约 | `unknown_contract` |
| `DOM-MAN-018` | `provides` 所需能力缺失 | `missing_capability` |
| `DOM-MAN-019` | 未知顶层字段 | `unknown_field` |
| `DOM-MAN-020` | `resources` 越界 | `invalid_resource` |

属性测试：随机生成合法 manifest，序列化 → 反序列化 → 再校验，必须保持等价；
随机删除任一必填字段，必须失败且错误字段名正确。

## 6.2 descriptor（`DOM-DES`）

| ID | 输入 | 期望 |
| --- | --- | --- |
| `DOM-DES-001` | 最小合法 descriptor（一个 query） | `Ok` |
| `DOM-DES-002` | 重复 op id | `duplicate_op` |
| `DOM-DES-003` | op id 含大写 / 连字符 | `invalid_op_id` |
| `DOM-DES-004` | `kind` 非法 | `invalid_kind` |
| `DOM-DES-005` | `risk` 非法 | `invalid_risk` |
| `DOM-DES-006` | `timeout_ms` 超出 1000..300000 | `invalid_timeout` |
| `DOM-DES-007` | `input` 不是对象 schema | `invalid_schema` |
| `DOM-DES-008` | `output` 引用不存在 | `dangling_ref` |
| `DOM-DES-009` | `config` 引用不存在 | `dangling_ref` |
| `DOM-DES-010` | `display.type` 未知 | `unknown_display` |
| `DOM-DES-011` | `chat.send` 指向 query | `invalid_display_ref` |
| `DOM-DES-012` | `chat.stream` 指向 action | `invalid_display_ref` |
| `DOM-DES-013` | `chat` 输出缺 `items` | `display_shape_mismatch` |
| `DOM-DES-014` | `table` 输出 `items` 不是数组 | `display_shape_mismatch` |
| `DOM-DES-015` | `metrics` 输出缺 `points` | `display_shape_mismatch` |
| `DOM-DES-016` | `log` 输出缺 `lines` | `display_shape_mismatch` |
| `DOM-DES-017` | `detail` 输出是数组 | `display_shape_mismatch` |
| `DOM-DES-018` | 产物含 `index.html` | `forbidden_asset` |
| `DOM-DES-019` | 同一 op 同时声明 `stream` 与 `pagination` | `invalid_op` |
| `DOM-DES-020` | schema 深度 6 | `Ok` |
| `DOM-DES-021` | schema 深度 7 | `schema_too_deep` |
| `DOM-DES-022` | 对象 64 个属性 | `Ok` |
| `DOM-DES-023` | 对象 65 个属性 | `schema_too_wide` |
| `DOM-DES-024` | enum 100 项 | `Ok` |
| `DOM-DES-025` | enum 101 项 | `enum_too_large` |
| `DOM-DES-026` | `oneOf` / `anyOf` / `allOf` | `unsupported_schema` |
| `DOM-DES-027` | 递归 `$ref` | `recursive_ref` |
| `DOM-DES-028` | 外部 `$ref` | `external_ref` |
| `DOM-DES-029` | `type: "null"` | `unsupported_schema` |
| `DOM-DES-030` | schema 节点 513 | `schema_too_large` |
| `DOM-DES-031` | `x-mcsup-secret` 在输出 | `secret_on_output` |
| `DOM-DES-032` | `x-mcsup-format` 在非字符串 | `invalid_format_target` |
| `DOM-DES-033` | 未知 `x-mcsup-*` 键 | `Ok`（忽略但记录） |
| `DOM-DES-034` | `x-mcsup-format: "html"` | `unsupported_format` |

## 6.3 能力（`DOM-CAP`）

| ID | 断言 |
| --- | --- |
| `DOM-CAP-001` | 能力名解析唯一，未知即错 |
| `DOM-CAP-002` | 去向交集为空集时 `granted` 为空 |
| `DOM-CAP-003` | 三方交集：manifest ∩ requested ∩ approved |
| `DOM-CAP-004` | `requested` 超出 manifest 的部分被剔除 |
| `DOM-CAP-005` | 15 项能力（含 `process:spawn` / `mc:client`）全部可解析 |
| `DOM-CAP-006` | 不开放能力不可授予 |
| `DOM-CAP-007` | 订阅所需能力检查（`console:line` → `console:read`） |
| `DOM-CAP-008` | 契约提供方能力检查 |
| `DOM-CAP-009` | 风险排序 `read < mutating < destructive` |
| `DOM-CAP-010` | `read` → Viewer / `mutating` → Operator / `destructive` → Admin |
| `DOM-CAP-011` | 撤销后交集立即缩小 |
| `DOM-CAP-012` | 空集合序列化稳定 |
| `DOM-CAP-013` | 能力集合去重且排序稳定 |
| `DOM-CAP-014` | 升级新增能力 → `pending_approval` |
| `DOM-CAP-015` | 升级减少能力 → 直接生效 |

属性测试：任意三个集合的交集满足交换律、幂等、与空集为空；生成的 `granted`
永远是三者子集。

## 6.4 状态机（`DOM-STA`）

| ID | 断言 |
| --- | --- |
| `DOM-STA-001` | 状态枚举与迁移表一一对应，无遗漏 |
| `DOM-STA-002` | 全状态 × 全事件矩阵：允许的迁移成功，其余返回 `invalid_transition` |
| `DOM-STA-003` | `enabled=false` 时 `start` 返回 `conflict` |
| `DOM-STA-004` | `starting` 只能到 `running` / `failed` |
| `DOM-STA-005` | `stopping` 超时到 `failed` |
| `DOM-STA-006` | 崩溃计数到 5 → `failed` |
| `DOM-STA-007` | 手动 `start` 清零计数 |
| `DOM-STA-008` | 升级新增能力 → `pending_approval` |
| `DOM-STA-009` | 卸载前必须 `disabled` 且进程退出 |
| `DOM-STA-010` | 迁移幂等：重复 `stop` 不产生非法状态 |
| `DOM-STA-011` | 迁移错误带 `from` / `to` / `event` |
| `DOM-STA-012` | 状态序列化稳定（小写下划线） |

矩阵实现建议：用表驱动，`(from, event) -> Result<to>`，测试遍历所有组合，禁止
手写 90 个 `if`。

## 6.5 契约标识与事件（`DOM-CON` / `DOM-EVT`）

| ID | 断言 |
| --- | --- |
| `DOM-CON-001` | `chat.publish@1` 解析为 `(name, version)` |
| `DOM-CON-002` | 缺少 `@` 或版本非数字 → 错 |
| `DOM-CON-003` | 版本 `@0` / `@999` → 未知契约 |
| `DOM-CON-004` | 闭集之外的名字 → 未知契约 |
| `DOM-CON-005` | `@1` 与 `@2` 可区分 |
| `DOM-CON-006` | 单播契约的 `target` 只在单播合法 |
| `DOM-CON-007` | 契约所需能力映射表完整 |
| `DOM-CON-008` | 同一插件可提供多个版本 |
| `DOM-CON-009` | 契约名大小写敏感 |
| `DOM-CON-010` | 空字符串 / 超长名拒绝 |
| `DOM-EVT-001` | 10 个订阅名全部可解析 |
| `DOM-EVT-002` | `DomainEvent` → 线格式映射完整 |
| `DOM-EVT-003` | 未知事件不投递 |
| `DOM-EVT-004` | `console:line` 需要 `console:read` |
| `DOM-EVT-005` | `ChatMessage` 解析：vanilla `<name> text` |
| `DOM-EVT-006` | 解析器对畸形行返回 `None`，不 panic |
| `DOM-EVT-007` | 颜色码 `§` 保留在原始文本，渲染层处理 |
| `DOM-EVT-008` | 玩家进出行解析 |

## 6.6 配置实例（`DOM-CFG`）

| ID | 输入 | 期望 |
| --- | --- | --- |
| `DOM-CFG-001` | 合法对象 | 通过 |
| `DOM-CFG-002` | 缺必填 | 错误路径指向字段 |
| `DOM-CFG-003` | `additionalProperties:false` 下多字段 | 拒绝 |
| `DOM-CFG-004` | 整数 / 数值 / 布尔类型不符 | 拒绝 |
| `DOM-CFG-005` | `minimum` / `maximum` / exclusive 边界 | 边界值按声明接受或拒绝 |
| `DOM-CFG-006` | `minLength` / `maxLength` / `pattern` | 越界或模式不符拒绝 |
| `DOM-CFG-007` | `minItems` / `maxItems` / `items` | 逐项校验 |
| `DOM-CFG-008` | 默认值 | 递归收集，不臆造类型默认 |
| `DOM-CFG-009` | `x-mcsup-secret` | 读取打码为 `***` |
| `DOM-CFG-010` | 未知关键字 | 按注解忽略，不拒绝旧插件 |

# 7. 协议测试矩阵

## 7.1 帧编解码（`PROTO-FRM`）

| ID | 输入 | 期望 |
| --- | --- | --- |
| `PROTO-FRM-001` | 合法帧 | 编解码往返等价 |
| `PROTO-FRM-002` | 含未知字段 | 忽略，解析成功 |
| `PROTO-FRM-003` | 未知 `type` | 忽略，连接保持 |
| `PROTO-FRM-004` | `v=2` | `unsupported_version`，关闭 |
| `PROTO-FRM-005` | 非法 JSON | `invalid_frame` |
| `PROTO-FRM-006` | 24 MiB + 1 字节 | `frame_too_large` |
| `PROTO-FRM-007` | CRLF 行尾 | 解析成功 |
| `PROTO-FRM-008` | 空行 | `invalid_frame` |
| `PROTO-FRM-009` | 重复键 | `invalid_frame` |
| `PROTO-FRM-010` | 帧后带垃圾字符 | `invalid_frame` |
| `PROTO-FRM-011` | 非法 UTF-8 | `invalid_frame` |
| `PROTO-FRM-012` | JSON 深度 33 | `invalid_frame` |
| `PROTO-FRM-013` | `id` 超出 u64 | `invalid_frame` |
| `PROTO-FRM-014` | 顶层不是对象 | `invalid_frame` |
| `PROTO-FRM-015` | 缺 `type` | `invalid_frame` |
| `PROTO-FRM-016` | 24 MiB - 1 字节 | 解析成功 |
| `PROTO-FRM-017` | `NaN` / `Infinity` | `invalid_frame` |
| `PROTO-FRM-018` | 未知嵌套字段 | 忽略 |
| `PROTO-FRM-019` | 读到 EOF | `unexpected_eof` |
| `PROTO-FRM-020` | 写出 | 恰好一个 `\n` 且立即 flush |

属性测试：任意合法帧序列化后必须单行、无 BOM、无多余空白；随机字节流不得
panic，只能返回错误。

## 7.2 握手（`PROTO-HSK`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `PROTO-HSK-001` | 正常握手 | `welcome`，`granted` 正确 |
| `PROTO-HSK-002` | 插件先发 `call` | `protocol_error`，关闭 |
| `PROTO-HSK-003` | 缺 `token` | `reject{code:"unauthorized"}` |
| `PROTO-HSK-004` | 错误 `token` | 同上，不泄露有效信息 |
| `PROTO-HSK-005` | `plugin_id` 与记录不符 | `reject{code:"identity_mismatch"}` |
| `PROTO-HSK-006` | `api=2` | `reject{code:"api_mismatch"}` |
| `PROTO-HSK-007` | `plugin_version` 与记录不符 | `reject{code:"identity_mismatch"}` |
| `PROTO-HSK-008` | `requested` 超出 manifest | 越界项不授予，写审计 |
| `PROTO-HSK-009` | 订阅所需能力未授予 | 该订阅不生效，写审计 |
| `PROTO-HSK-010` | `welcome` 前发 `event` | `protocol_error` |
| `PROTO-HSK-011` | `reject` 后插件不退 | 5 s 后强杀 |
| `PROTO-HSK-012` | 重复 `hello` | `protocol_error` |

## 7.3 调用（`PROTO-CALL`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `PROTO-CALL-001` | 宿主调用 op | `result.ok=true` |
| `PROTO-CALL-002` | 输入不合 descriptor | `invalid_input`，handler 未执行 |
| `PROTO-CALL-003` | 插件调用宿主能力 | 成功 |
| `PROTO-CALL-004` | 未知 `method` | `not_found` |
| `PROTO-CALL-005` | 11 个错误码 | `code/category/retryable` 映射逐项 |
| `PROTO-CALL-006` | 未知 `id` 的 `result` | 忽略并记一行 |
| `PROTO-CALL-007` | 同方向重复 `id` | `protocol_error` |
| `PROTO-CALL-008` | 双向同 `id` | 互不冲突 |
| `PROTO-CALL-009` | 重入调用 | 成功，不死锁 |
| `PROTO-CALL-010` | 超时 | 发 `cancel`，返回 `timeout` |
| `PROTO-CALL-011` | 输出超过 1 MiB | `limit_exceeded` |
| `PROTO-CALL-012` | 取消后迟到的 `result` | 忽略 |

## 7.4 流（`PROTO-STR`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `PROTO-STR-001` | 连续分块 | `seq` 0..n，`done` 正确 |
| `PROTO-STR-002` | `done` 后继续发 | `protocol_error` |
| `PROTO-STR-003` | chunk 不符合 fragment schema | `invalid_input` |
| `PROTO-STR-004` | `seq` 跳号 | 终止流 |
| `PROTO-STR-005` | 累计 16 MiB | 终止，`limit_exceeded` |
| `PROTO-STR-006` | 忽略 `cancel` | 1 s 后强制终止 |
| `PROTO-STR-007` | 空闲 60 s | 终止 |
| `PROTO-STR-008` | 同 id 并发两条流 | 拒绝 |
| `PROTO-STR-009` | 多个流并行 | 各自 `seq` 独立 |
| `PROTO-STR-010` | 宿主侧背压 | 暂停读取，不 OOM |

## 7.5 配置（`PROTO-CFG`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `PROTO-CFG-001` | 合法变更 | `result.ok`，revision +1 |
| `PROTO-CFG-002` | 不符 schema | 不发帧，返回用户错误 |
| `PROTO-CFG-003` | 插件回 `ok:false` | 恢复旧值，状态 `degraded` |
| `PROTO-CFG-004` | 乱序 revision | 丢弃过期帧 |
| `PROTO-CFG-005` | 含密钥 | `config get` 返回 `***` |
| `PROTO-CFG-006` | `starting` 期间变更 | 排队到 `welcome` 后 |
| `PROTO-CFG-007` | `config.json` 落盘 | `0600`，卸载（默认）时随目录删除 |

## 7.6 控制通道（`PROTO-CTL`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `PROTO-CTL-001` | `list` | 状态与版本 |
| `PROTO-CTL-002` | `status` | 能力、授权、最近错误 |
| `PROTO-CTL-003` | `start/stop/restart` | 状态迁移正确 |
| `PROTO-CTL-004` | `call` | 与插件 op 一致 |
| `PROTO-CTL-005` | `stream` | `stream` 帧持续到 `done` |
| `PROTO-CTL-006` | `logs` | 游标分页 |
| `PROTO-CTL-007` | `config_get/set` | 密钥打码 |
| `PROTO-CTL-008` | `grant/revoke` | 授权即时生效 |
| `PROTO-CTL-009` | `reload` | 安装后宿主读到新记录 |
| `PROTO-CTL-010` | TCP 回退认证 | 缺令牌拒绝，错误的拒绝 |
| `PROTO-CTL-011` | `config_apply` | 只下发已落盘配置，revision 不双增 |

## 7.7 版本（`PROTO-VER`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `PROTO-VER-001` | 四个版本常量 | 各自独立，互不派生 |
| `PROTO-VER-002` | 插件 API 高于宿主 | 拒绝，提示升级宿主 |
| `PROTO-VER-003` | 插件 API 低于宿主 | 按相等规则拒绝，不做兼容 |
| `PROTO-VER-004` | manifest schema 不匹配 | 安装拒绝 |
| `PROTO-VER-005` | descriptor schema 不匹配 | 安装拒绝 |
| `PROTO-VER-006` | 控制通道版本不匹配 | 明确错误，不猜测 |

---

# 8. 宿主集成矩阵

## 8.1 进程（`HOST-PRC`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `HOST-PRC-001` | 只支持进程入口 | 其它 `entry.type` 安装失败 |
| `HOST-PRC-002` | 入口无执行权限 | 启动失败，错误可操作 |
| `HOST-PRC-003` | 工作目录 | 插件 cwd 是其数据目录，不是用户 cwd |
| `HOST-PRC-004` | 同实例二次启动 | 第二个进程不拉起，返回已有会话 |
| `HOST-PRC-005` | 优雅停止 | `shutdown` 后退出码 0 |
| `HOST-PRC-006` | 命令行 | 不含令牌；令牌只在环境变量 |
| `HOST-PRC-007` | 握手超时 | 10 s 后杀掉，状态 `failed` |
| `HOST-PRC-008` | 握手前崩溃 | 记录退出码与最后 20 行 |
| `HOST-PRC-009` | 握手后崩溃 | 触发退避 |
| `HOST-PRC-010` | 退出码记录 | 0/1/2/3/4 原样记录 |
| `HOST-PRC-011` | `shutdown` 超时 | 5 s 后强杀，记审计 |
| `HOST-PRC-012` | 运行中升级 | 先停进程再切换，不原地覆盖 |
| `HOST-PRC-013` | 重入 | 嵌套调用成功 |
| `HOST-PRC-014` | 进程组终止 | 子进程一并清理 |
| `HOST-PRC-015` | stdout / stderr | 分别采集，不混入协议 |
| `HOST-PRC-016` | 同一宿主内重复拉起 | 连续多次拉起 `.cmd` 入口都成功；控制台不靠继承（宿主 detached、插件进程自带无窗口控制台） |

## 8.2 端点（`HOST-EP`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `HOST-EP-001` | Unix socket 命名 | 含 FNV 哈希，同实例稳定 |
| `HOST-EP-002` | 权限 | `0600` |
| `HOST-EP-003` | Windows 管道 | 当前用户可连，其他用户不可 |
| `HOST-EP-004` | 路径过长 | 回退 `$TMPDIR` |
| `HOST-EP-005` | 仍过长 | 回退回环 TCP |
| `HOST-EP-006` | TCP 绑定 | 只绑 `127.0.0.1` |
| `HOST-EP-007` | 退出清理 | socket / 管道被删除 |
| `HOST-EP-008` | 错误令牌 | 握手拒绝 |
| `HOST-EP-009` | 连接超时 | 10 s 内未连上即失败 |
| `HOST-EP-010` | 残留 socket | 启动时安全清理，不删除他人 socket |
| `HOST-EP-011` | 两插件 | 端点互不冲突 |
| `HOST-EP-012` | server 作用域 | 端点哈希含 server |

## 8.3 心跳（`HOST-HB`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `HOST-HB-001` | 15 s 间隔 | 按时发送 |
| `HOST-HB-002` | 3 次未回 | 45 s 后终止 |
| `HOST-HB-003` | `pong` 回显 `n` | 匹配计数 |
| `HOST-HB-004` | 错误 `n` | 忽略，不重置超时 |
| `HOST-HB-005` | 长 op 期间 | 心跳不中断调用 |
| `HOST-HB-006` | `shutdown` 后 | 不再发心跳 |

## 8.4 崩溃与退避（`HOST-RST`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `HOST-RST-001` | 握手后崩溃 | 1 s 后首次重启 |
| `HOST-RST-002` | 连续崩溃 | 延迟 1/2/4/8/16 s |
| `HOST-RST-003` | 10 分钟内第 5 次 | 转 `failed`，停止重启 |
| `HOST-RST-004` | 手动 `start` | 清零计数并成功 |
| `HOST-RST-005` | `starting` 崩溃 | 计一次，状态 `failed` |
| `HOST-RST-006` | `stopping` 崩溃 | 记 `stopped`，不重启 |
| `HOST-RST-007` | `disabled` 状态 | 不自动重启 |
| `HOST-RST-008` | 时钟推进 | 用暂停时钟断言，不 sleep |

## 8.5 空闲（`HOST-IDLE`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `HOST-IDLE-001` | 非 resident 120 s | 进程停止，记录保留 |
| `HOST-IDLE-002` | 活动重置计时 | 不停止 |
| `HOST-IDLE-003` | resident | 永不因空闲停止 |
| `HOST-IDLE-004` | 宿主 300 s 空闲 | 宿主退出 |
| `HOST-IDLE-005` | 有 resident | 宿主不退出 |
| `HOST-IDLE-006` | 控制连接 | 连接存在时不退出 |

## 8.6 限额（`HOST-LIM`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `HOST-LIM-001` | 24 MiB + 1 | 终止会话 |
| `HOST-LIM-002` | 出站 inflight 8 | 第 9 个排队 |
| `HOST-LIM-003` | 队列 32 | 第 33 个 `limit_exceeded` |
| `HOST-LIM-004` | 事件队列 1024 | 丢最旧 |
| `HOST-LIM-005` | 事件溢出 | `seq` 缺口 + 指标 |
| `HOST-LIM-006` | op 30 s | `timeout` + `cancel` |
| `HOST-LIM-007` | 声明 300 s | 按声明，超过 300 拒绝安装 |
| `HOST-LIM-008` | 流 16 MiB | 终止流 |
| `HOST-LIM-009` | 流空闲 60 s | 终止流 |
| `HOST-LIM-010` | 日志 200 行/秒 | 丢弃并汇总 |

## 8.7 事件（`HOST-EVT`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `HOST-EVT-001` | `McsupBuilder::events` 注入 | sink 收到领域事件 |
| `HOST-EVT-002` | 10 个 kind 映射 | 线格式与附录 A 一致 |
| `HOST-EVT-003` | server 作用域 | 只收到本服事件 |
| `HOST-EVT-004` | machine 作用域 | 收到所有服事件 |
| `HOST-EVT-005` | 队列满 | 丢最旧，`seq` 连续缺口 |
| `HOST-EVT-006` | 宿主重启 | 不补发历史事件 |
| `HOST-EVT-007` | 审计 | 字段完整，无明文 |
| `HOST-EVT-008` | 同会话顺序 | 事件按产生顺序 |
| `HOST-EVT-009` | `welcome` 前 | 不投递 |
| `HOST-EVT-010` | 控制台追随 | 500 ms 轮询去重 |

## 8.8 日志（`HOST-LOG`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `HOST-LOG-001` | stdout / stderr | 分别采集，加前缀 |
| `HOST-LOG-002` | 前缀 | `[<plugin_id>]` |
| `HOST-LOG-003` | 单行 8 KiB | 截断并标注 |
| `HOST-LOG-004` | 1000 行/秒 | 限速并汇总 |
| `HOST-LOG-005` | stdout 打印 JSON 帧 | 不产生协议帧 |
| `HOST-LOG-006` | 打印密钥 | 替换为 `***` |
| `HOST-LOG-007` | ANSI / 控制字符 | 剥离后再落盘 |
| `HOST-LOG-008` | 轮转 | 按大小与时间 |

# 9. 能力与 HostApi 测试矩阵

## 9.1 能力门（`CAP-GATE`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `CAP-GATE-001` | 已授予能力 | 调用成功 |
| `CAP-GATE-002` | 未授予能力 | `denied`，`retryable=false` |
| `CAP-GATE-003` | 不开放能力 | 安装即拒绝 |
| `CAP-GATE-004` | 三方交集为空 | `denied`，审计记录三方差异 |
| `CAP-GATE-005` | 撤销后调用 | 立即 `denied` |
| `CAP-GATE-006` | 越界 `requested` | 忽略并审计 |
| `CAP-GATE-007` | 插件伪造能力名 | `invalid_input` |
| `CAP-GATE-008` | 撤销后重启 | `granted` 不含该项 |
| `CAP-GATE-009` | 多能力调用 | 只检查被调用的那一项 |
| `CAP-GATE-010` | `console:read` 不能写 | 写调用 `denied` |
| `CAP-GATE-011` | `server:read` 不能启停 | 启停 `denied` |
| `CAP-GATE-012` | 审计字段 | plugin / capability / method / result 完整 |

## 9.2 服务器只读（`API-SRV`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `API-SRV-001` | `list` | 只含当前作用域可见服务器 |
| `API-SRV-002` | 非法服务器名 | `invalid_input`，不触达 service |
| `API-SRV-003` | `status` | 状态与 CLI 一致 |
| `API-SRV-004` | 不存在的服务器 | `not_found` |
| `API-SRV-005` | `properties` | 键值稳定，不含密钥 |
| `API-SRV-006` | 远端服务器 | 经 backend 路由，不绕过 |
| `API-SRV-007` | 大量服务器 | 分页 / 上限生效 |
| `API-SRV-008` | 服务层错误 | 映射为 `HostError`，不泄露内部类型 |

## 9.3 控制台（`API-CON`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `API-CON-001` | `tail` 游标 | 不重复、不丢行 |
| `API-CON-002` | `tail` 上限 500 | 超限 `limit_exceeded` |
| `API-CON-003` | `send_line` | 注入成功 |
| `API-CON-004` | 换行注入 | 拒绝 |
| `API-CON-005` | 4096 字节 | 通过；4097 拒绝 |
| `API-CON-006` | 未运行服务器 | `conflict` |
| `API-CON-007` | 未授予 `console:write` | `denied` |
| `API-CON-008` | 控制台行解析 | 聊天 / 玩家事件只产生一次 |

## 9.4 键值（`API-KV`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `API-KV-001` | set / get / delete | 往返一致 |
| `API-KV-002` | 命名空间隔离 | 不同插件互不可见 |
| `API-KV-003` | 值 256 KiB | 通过；+1 拒绝 |
| `API-KV-004` | 总量 64 MiB | 超限 `limit_exceeded` |
| `API-KV-005` | 键非法字符 | `invalid_input` |
| `API-KV-006` | 前缀 list | 分页 ≤1000 |
| `API-KV-007` | 并发写 | 串行化，不丢更新 |
| `API-KV-008` | 损坏文件 | 只影响该键，读取其余正常 |

## 9.5 密钥（`API-SEC`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `API-SEC-001` | set / get | 插件侧得到明文 |
| `API-SEC-002` | 入口读取 | 返回 `***` |
| `API-SEC-003` | 审计 | 无明文 |
| `API-SEC-004` | 日志 | 精确值被替换 |
| `API-SEC-005` | 文件权限 | `0600` |
| `API-SEC-006` | 删除 | 立即不可读 |

## 9.6 出站（`API-NET`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `API-NET-001` | 白名单域名 | 成功 |
| `API-NET-002` | 非白名单 | `denied` |
| `API-NET-003` | 重定向到私网 | 拒绝 |
| `API-NET-004` | 响应 8 MiB | 通过；+1 拒绝 |
| `API-NET-005` | 超时 | `timeout` |
| `API-NET-006` | 非 http/https | `invalid_input` |
| `API-NET-007` | DNS 重绑 | 每次解析都校验 |
| `API-NET-008` | 代理配置 | 遵循平台配置，不绕过 |

## 9.7 文件（`API-FS`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `API-FS-001` | 相对路径读写 | 成功 |
| `API-FS-002` | `..` | `denied` |
| `API-FS-003` | 绝对路径 | `denied` |
| `API-FS-004` | 符号链接逃逸 | `denied` |
| `API-FS-005` | 16 MiB | 通过；+1 拒绝 |
| `API-FS-006` | 覆盖 `mcsup.json` | `denied` |
| `API-FS-007` | 运行中 jar | `conflict` |
| `API-FS-008` | Windows 大小写 / UNC | 归一化后校验 |

落位：`service::plugin` 的 `ServerFiles` 负责路径、限额与保留名判定，
`crates/plugin/src/api.rs` 只做帧分发。单测名以 `api_fs_` 开头，可用
`cargo test -p mcsup-service --lib api_fs` 过滤；符号链接用例在 unix 上执行。

## 9.8 生命周期 / 备份 / 内容 / Java（`API-LIFE` / `API-BAK` / `API-CNT` / `API-JAVA`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `API-LIFE-001` | start / stop / restart | 与 CLI 一致 |
| `API-LIFE-002` | 已在运行 | `conflict` |
| `API-LIFE-003` | 未授予能力 | `denied` |
| `API-LIFE-004` | 停止超时 | `timeout`，服务器仍在运行 |
| `API-LIFE-005` | 远端服务器 | 经 backend，不本地执行 |
| `API-LIFE-006` | 审计 | 记录动作与结果 |
| `API-BAK-001` | create / list / restore | 成功 |
| `API-BAK-002` | 不存在快照 | `not_found` |
| `API-BAK-003` | 恢复运行中服务器 | `conflict` |
| `API-BAK-004` | 未授予 | `denied` |
| `API-BAK-005` | 配额 | `limit_exceeded` |
| `API-BAK-006` | 审计 | 快照 id 记录 |
| `API-CNT-001` | 列出已装内容 | 与 mod 列表一致 |
| `API-CNT-002` | 安装 / 移除 | 成功 |
| `API-CNT-003` | 不兼容版本 | `conflict` |
| `API-CNT-004` | 未授予 | `denied` |
| `API-CNT-005` | 网络失败 | `unavailable`，可重试 |
| `API-CNT-006` | 审计 | 文件与来源记录 |
| `API-JAVA-001` | 列出托管 JDK | 成功 |
| `API-JAVA-002` | 安装 / 卸载 | 成功 |
| `API-JAVA-003` | 未授予 | `denied` |
| `API-JAVA-004` | 平台不支持 | `unsupported` |

落位：`server:lifecycle` / `backup` / `content` / `java` 由
`service::plugin::HostApiImpl` 实现，`crates/plugin/src/api.rs` 只做帧分发。生命周期错误按「服务器状态」收敛：
已在运行 / 未在运行 / EULA 未确认 / 缺核心均为 `conflict`，停机超时为
`timeout`。备份错误按 `BackupError::category()` 映射（不存在 → `not_found`，
运行中恢复 → `conflict`，磁盘 / 远端不可用 → `unavailable`）。内容错误按
`ModError::category()` 映射（运行中 → `conflict`，选择器未命中 → `not_found`，
纯客户端内容 → `invalid_input`，上游 / 磁盘失败 → `unavailable`），远端服务器
返回 `unsupported`。Java 错误按 `JavaError::category()` 映射（未登记 →
`not_found`，扫描 / 上游 / 下载 → `unavailable`）。

审计在会话层统一记录：`action` 为 `capability:<能力>:<方法>`，成功结果只摘
`snapshot` / `file` 字段（备份与内容审计的检索键），失败记录错误码与消息。
对应单测可用 `cargo test -p mcsup-service --lib api_life` / `api_bak` / `api_cnt` /
`api_java` 与 `cargo test -p mcsup-plugin --lib api::tests` 过滤。

## 9.9 展示类型投影（`DESC-DISPLAY`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `DESC-DISPLAY-001` | 六种 display 的合法形状 | descriptor 校验通过，类型不被改写 |
| `DESC-DISPLAY-002` | 输出形状不匹配 | `DisplayShapeMismatch`，安装失败 |
| `DESC-DISPLAY-003` | `chat` 引用未声明的 `send` / `stream` | `InvalidDisplayRef` |
| `DESC-DISPLAY-004` | CLI `plugin ops` | 人类表格含「展示」列，JSON 含 `display` |

落位：判定在 `domain::plugin::descriptor`（`validate_display_shape` /
`validate_display_refs`），CLI 只投影 `ResolvedOp::display`，不重复判断。
六种类型的正例见单测 `all_six_display_types_resolve`，可用
`cargo test -p mcsup-domain --lib descriptor` 过滤。

---

# 10. 契约测试矩阵

## 10.1 路由（`CON-RTE`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `CON-RTE-001` | 插件直连 | 无法建立；尝试被审计 |
| `CON-RTE-002` | 广播 | 所有提供方收到 |
| `CON-RTE-003` | 单播一个提供方 | 成功 |
| `CON-RTE-004` | 单播零提供方 | `not_found` |
| `CON-RTE-005` | 单播多个提供方无 `target` | `conflict`，列出候选 |
| `CON-RTE-006` | `target` 指定 | 只调用目标 |
| `CON-RTE-007` | 循环 A→B→A | 第二次拒绝 |
| `CON-RTE-008` | 深度 4 / 5 | 4 通过，5 拒绝 |
| `CON-RTE-009` | 父调用超时 | 子调用被取消 |
| `CON-RTE-010` | `@1` / `@2` 并存 | 按请求版本路由 |
| `CON-RTE-011` | 审计 | caller / provider / contract / result |
| `CON-RTE-012` | 单请求 32 次 | 第 33 次 `limit_exceeded` |
| `CON-RTE-013` | 调用方未声明 `consumes` | `denied` |
| `CON-RTE-014` | 未知或过期 `trace` | `conflict` |

## 10.2 各契约（`CON-CHAT` / `CON-AGENT` / `CON-MET` / `CON-INC` / `CON-LIFE`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `CON-CHAT-001` | publish 全局 | 提供方收到 |
| `CON-CHAT-002` | publish 指定玩家 | 只发给目标 |
| `CON-CHAT-003` | ask 收到回复 | 返回 answers |
| `CON-CHAT-004` | ask 超时 | 空数组，不报错 |
| `CON-CHAT-005` | ask 正则非法 | `invalid_input` |
| `CON-CHAT-006` | 消息超长 | `invalid_input` |
| `CON-CHAT-007` | 提供方未授予写 | 调用失败，不静默 |
| `CON-CHAT-008` | 广播部分失败 | 整体成功，`failures` 完整 |
| `CON-AGENT-001` | 基本对话 | 返回文本 |
| `CON-AGENT-002` | 会话 id 复用 | 上下文连续 |
| `CON-AGENT-003` | 超长消息 | `invalid_input` |
| `CON-AGENT-004` | 超时 | `timeout`，可重试 |
| `CON-AGENT-005` | 无提供方 | `not_found` |
| `CON-AGENT-006` | 流式 | 不走契约，走 op stream |
| `CON-MET-001` | 指定指标 | 返回样本 |
| `CON-MET-002` | 空 names | 返回默认集合 |
| `CON-MET-003` | 未知指标 | 忽略该名 |
| `CON-MET-004` | 部分提供方失败 | `failures` 完整 |
| `CON-MET-005` | 单位 | 原样返回 |
| `CON-MET-006` | server 作用域 | 只采样本服 |
| `CON-INC-001` | 合法上报 | `accepted=true` |
| `CON-INC-002` | 严重级别 | 枚举校验 |
| `CON-INC-003` | evidence 过大 | `invalid_input` |
| `CON-INC-004` | 重复上报 | 允许，id 不同 |
| `CON-INC-005` | 提供方失败 | `failures` 完整 |
| `CON-INC-006` | 审计 | 不含聊天正文 |
| `CON-LIFE-001` | start / stop / restart | 成功 |
| `CON-LIFE-002` | grace 传递 | 生效 |
| `CON-LIFE-003` | 提供方无能力 | `denied` |
| `CON-LIFE-004` | 多提供方无 target | `conflict` |
| `CON-LIFE-005` | 结果 | `ok` / `outcome` |
| `CON-LIFE-006` | 审计 | 动作与服务器记录 |

---

# 11. 安装与安全测试矩阵

## 11.1 来源（`INST-SRC`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `INST-SRC-001` | 本地 zip | 成功 |
| `INST-SRC-002` | URL + sha256 | 成功 |
| `INST-SRC-003` | URL 缺 sha256 | 拒绝（除非 `--allow-unpinned`） |
| `INST-SRC-004` | sha256 不匹配 | 拒绝 |
| `INST-SRC-005` | git tag | 解析到 commit |
| `INST-SRC-006` | git 完整 SHA | 成功 |
| `INST-SRC-007` | git 分支名 | 拒绝，要求 tag / SHA |
| `INST-SRC-008` | 索引与 manifest 不一致 | 拒绝 |
| `INST-SRC-009` | 当前平台无产物 | `unsupported` |
| `INST-SRC-010` | 离线缓存 | 复用已下载产物 |

## 11.2 校验（`INST-VER`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `INST-VER-001` | 合法产物 | 全部校验通过 |
| `INST-VER-002` | 摘要不匹配 | 拒绝 |
| `INST-VER-003` | manifest 非法 | 拒绝 |
| `INST-VER-004` | descriptor 非法 | 拒绝 |
| `INST-VER-005` | 入口不可执行 | 拒绝 |
| `INST-VER-006` | 平台不匹配 | `unsupported` |
| `INST-VER-007` | 路径逃逸 | 拒绝 |
| `INST-VER-008` | 解包超限 | 拒绝 |
| `INST-VER-009` | 文件数超限 | 拒绝 |
| `INST-VER-010` | 压缩比异常 | 拒绝 |

| `INST-VER-012` | 撤销列表命中 | 拒绝启动 |

撤销相关的落位（`provider::plugin_revoke` 的缓存用例 + `crates/service/tests/plugin_revocation.rs`）：

- `INST-VER-012`：命中版本 `enable` 被拒（`Unauthorized` / 退出码 6）；
  `versions: []` 视为全部版本；其他版本不受影响。
- 列表读不懂时不阻断已安装插件，`plugin doctor` 报 `warning`；
  未装配撤销检查时不读文件也不阻断。
- CLI 契约：`plugin revocations import` / 查看 / 命中后 `enable` 退出码 6、
  非法列表整体拒绝（退出码 2）、`fetch` 只接受 https。

## 11.3 打包（`INST-PKG`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `INST-PKG-001` | 临时目录 → 原子移动 | 无半装状态 |
| `INST-PKG-002` | 失败清理 | 临时目录删除 |
| `INST-PKG-003` | 同 id 并发 | 串行化 |
| `INST-PKG-004` | 安装期间不启动 | 无子进程 |
| `INST-PKG-005` | 已存在版本 | 拒绝（除非 `--force`） |
| `INST-PKG-006` | 未知契约 | 拒绝 |
| `INST-PKG-007` | 重复文件 | 拒绝 |
| `INST-PKG-008` | 大小写冲突 | 拒绝 |
| `INST-PKG-009` | 已安装旧版本 | 并存，不覆盖 |
| `INST-PKG-010` | 含 `preinstall` | 不执行，仅记录 |
| `INST-PKG-011` | 保留 2 个版本 | 第 3 个按策略清理 |
| `INST-PKG-012` | 记录写入失败 | 回滚产物 |
| `INST-PKG-013` | 磁盘满 | 明确错误，无半装 |
| `INST-PKG-014` | 只读目录 | 明确错误 |
| `INST-PKG-015` | 权限 | 产物不可被其他用户改 |

## 11.4 升级（`INST-UPG`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `INST-UPG-001` | 升级成功 | `active_version` 切换 |
| `INST-UPG-002` | 回滚 | 切回上一版本 |
| `INST-UPG-003` | 新增能力 | `pending_approval` |
| `INST-UPG-004` | 运行中升级 | 先停进程 |
| `INST-UPG-005` | 启动失败 | 自动回滚 |
| `INST-UPG-006` | 降级且能力减少 | 直接生效 |
| `INST-UPG-007` | 卸载 | `--keep-data` 保留目录但删密钥；`--keep-secrets` 保留密钥 |
| `INST-UPG-008` | 默认卸载 | 数据目录、日志与记录一起删除 |
| `INST-UPG-009` | `--prune` | 更旧版本清理；当前与回滚目标保留 |
| `INST-UPG-010` | 并发升级 | 第二个拒绝 |
| `INST-UPG-011` | `update --to <当前版本>` | 拒绝，不改 `previous_version` |
| `INST-UPG-012` | 崩溃残留临时目录 | 超过一小时后下次安装清理 |

## 11.6 安全（`SEC-*`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `SEC-ZIP-001` | `../` 路径 | 拒绝 |
| `SEC-ZIP-002` | 绝对路径 | 拒绝 |
| `SEC-ZIP-003` | 符号链接 | 拒绝 |
| `SEC-ZIP-004` | 硬链接 | 拒绝 |
| `SEC-ZIP-005` | 设备文件 | 拒绝 |
| `SEC-ZIP-006` | 压缩炸弹 | 拒绝 |
| `SEC-ZIP-007` | 文件数 5001 | 拒绝 |
| `SEC-ZIP-008` | 路径深度 9 | 拒绝 |
| `SEC-ZIP-009` | 可执行脚本 | 不执行，记录 |
| `SEC-ZIP-010` | 保留名 / 尾随点 | 拒绝 |
| `SEC-SIG-004` | 撤销 | 拒绝 |
| `SEC-SIG-006` | 自动审核项 | 逐条通过 / 拒绝 |
| `SEC-SIG-007` | 摘要不匹配 | 拒绝 |
| `SEC-SIG-008` | 平台产物错配 | 拒绝 |
| `SEC-SIG-009` | 旧版本重放 | 拒绝或提示 |
| `SEC-SSRF-001` | `localhost` | 拒绝 |
| `SEC-SSRF-002` | `127.0.0.1` | 拒绝 |
| `SEC-SSRF-003` | `::1` | 拒绝 |
| `SEC-SSRF-004` | 10/8 | 拒绝 |
| `SEC-SSRF-005` | 172.16/12 | 拒绝 |
| `SEC-SSRF-006` | 192.168/16 | 拒绝 |
| `SEC-SSRF-007` | 169.254.169.254 | 拒绝 |
| `SEC-SSRF-008` | 重定向到私网 | 拒绝 |
| `SEC-SSRF-009` | DNS 重绑 | 二次解析拒绝 |
| `SEC-SSRF-010` | `file://` / `gopher://` | 拒绝 |
| `SEC-PATH-001` | `..` | 拒绝 |
| `SEC-PATH-002` | 绝对路径 | 拒绝 |
| `SEC-PATH-003` | 符号链接 | 拒绝 |
| `SEC-PATH-004` | 直接读数据库 | 无权限 / 不可达 |
| `SEC-PATH-005` | 插件间文件共享 | 审计标记 |
| `SEC-PATH-006` | Windows 盘符 | 拒绝 |
| `SEC-PATH-007` | UNC 路径 | 拒绝 |
| `SEC-PATH-008` | 大小写变体 | 归一化后拒绝 |
| `SEC-LOG-001` | ANSI 转义 | 剥离 |
| `SEC-LOG-002` | 换行注入 | 转义 |
| `SEC-LOG-003` | JSON 注入 | 按文本 |
| `SEC-LOG-004` | 格式串 | 不解释 |
| `SEC-LOG-005` | 超大行 | 截断 |
| `SEC-LOG-006` | 密钥 | 替换 |
| `SEC-LOG-007` | 已知密钥值 | 精确替换为 `***`；≥ 8 字节、最多 64 条 |
| `SEC-LOG-008` | 过短 / 非 UTF-8 密钥值 | 不进打码表，避免整行被替换 |
| `SEC-DOS-001` | JSON 深度 33 | 拒绝 |
| `SEC-DOS-002` | 帧洪泛 | 限速 / 断开 |
| `SEC-DOS-003` | 事件洪泛 | 丢弃 + 指标 |
| `SEC-DOS-004` | 流洪泛 | 终止 |
| `SEC-DOS-005` | op 洪泛 | 队列上限 |
| `SEC-DOS-006` | 慢握手 | 10 s 超时 |
| `SEC-DOS-007` | 多插件同时启动 | 有上限，不拖垮宿主 |
| `SEC-DOS-008` | 超大 schema | 安装拒绝 |
| `SEC-KEY-001` | 有机器密钥 | `secrets.json` 中不出现明文，格式为 `v/nonce/data` 信封 |
| `SEC-KEY-002` | 读取旧明文文件 | 可读，下一次写入即迁移为密文 |
| `SEC-KEY-003` | 机器密钥不可用 | 读到密文时报 `denied`，不静默降级 |
| `SEC-KEY-004` | 密文被改动 | 认证失败，报损坏 |
| `SEC-KEY-005` | 密文跨插件复制 | AAD 绑定插件 id，解密失败 |
| `SEC-KEY-006` | 卸载 | 默认删除 `secrets.json`，其余数据保留 |
| `SEC-KEY-007` | 卸载 `--keep-secrets` | 密钥保留，供重装使用 |

落位：

- `SEC-LOG-006` / `007` / `008`：`crates/plugin/src/process.rs` 单测
  （`known_secret_values_are_redacted`、`short_and_duplicate_secrets_are_skipped`、
  `non_utf8_secret_is_skipped`、`redact_table_is_bounded`）；宿主在
  `PluginHost::start_session` 把 `SecretStore::values` 并进 `SpawnPlan.redact`。
- `SEC-KEY-001..005`：`crates/storage/src/plugin.rs` 单测
  （`encrypted_secrets_do_not_contain_plaintext`、`..._migrates`、
  `encrypted_secrets_need_the_machine_key`、`tampered_secrets_are_rejected`、
  `encrypted_secrets_are_bound_to_the_plugin`）；密钥本身的密封 / 打开在
  `crates/auth/src/machine_key.rs` 单测。
- `SEC-KEY-006` / `007`：`crates/service/src/plugin.rs` 单测
  `remove_handles_data_by_mode`（`Delete` / `Keep` / `Keep { secrets: true }`
  三种落点）。
- 机器密钥只存系统钥匙串；不可用时退化为 `0600` 明文，`plugin doctor`
  报告降级。退化路径没有「加密生效」的假象，因此不按 `SEC-KEY-001` 判通过。

## 11.7 平台（`PLAT`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `PLAT-001` | 入口形态 | 只接受进程 |
| `PLAT-002` | 依赖方向 | 架构脚本通过 |
| `PLAT-003` | HostApi 注入 | `plugin` 不依赖 `service` |
| `PLAT-004` | 单实例锁 | 第二实例被拒 |
| `PLAT-005` | 端点回退 | 三平台一致语义 |
| `PLAT-006` | 进程组 | 子进程清理 |
| `PLAT-007` | 路径大小写 | Windows 不敏感 |
| `PLAT-008` | 信号 | SIGTERM / Ctrl-C 走优雅退出 |
| `PLAT-009` | 长路径 | macOS socket 回退 |
| `PLAT-010` | 非 UTF-8 文件名 | 拒绝或跳过，不 panic |
| `PLAT-011` | 条款追踪门禁 | 报告 ID 集合与 `cases.json` 完全相等，缺项与多项都失败 |

# 12. CLI / Web / SDK 测试矩阵

## 12.1 CLI 命令（`CLI-CMD`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `CLI-CMD-001` | `list` 空 | 表头 + 「无插件」 |
| `CLI-CMD-002` | `install --from` | 安装并展示能力 |
| `CLI-CMD-003` | `install --url` | 下载、校验、安装 |
| `CLI-CMD-004` | `install https://github.com/<o>/<r>` | 解析引用到 commit，下载仓库归档并安装 |
| `CLI-CMD-005` | 非法来源组合 | 退出 2，说明互斥 |
| `CLI-CMD-006` | `enable` | 状态 `stopped`，`enabled=true` |
| `CLI-CMD-007` | `disable` | 状态 `disabled` |
| `CLI-CMD-008` | `start` | 握手后 `running` |
| `CLI-CMD-009` | `stop --grace` | 优雅停止 |
| `CLI-CMD-010` | `call` query | 输出与 descriptor 一致 |
| `CLI-CMD-011` | `call` action | 风险确认 |
| `CLI-CMD-012` | `call --stream` | 逐块输出 |
| `CLI-CMD-013` | `config get/set` | 密钥打码 |
| `CLI-CMD-014` | `grant/revoke` | 即时生效 |
| `CLI-CMD-015` | `logs --follow` | 游标续读 |
| `CLI-CMD-016` | `remove` | 默认删配置与数据；`--keep-data` 保留；`--keep-secrets` 连密钥保留 |
| `CLI-CMD-017` | `update --to` | 切换版本 |
| `CLI-CMD-018` | `rollback` | 切回上一版本 |
| `CLI-CMD-019` | `verify` | 摘要与元数据复查 |
| `CLI-CMD-020` | `doctor` | 汇总检查项 |
| `CLI-CMD-021` | `block` / `unblock` | 立即停止并禁用；解除后不自动启用 |
| `CLI-CMD-022` | `quarantine` / `release` | 产物保留，启动被拒绝 |
| `CLI-CMD-023` | `config set --secret` | 值从 stdin 读取，回显与输出均不含明文 |
| `CLI-CMD-027` | `remove` 确认 | 启用中问「禁用并移除」、已停用问「移除」，默认答「否」；无 TTY 又无 `--yes` 退出 2 且插件不动 |

## 12.2 输出（`CLI-OUT`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `CLI-OUT-001` | 表格 | 列宽按显示宽度，不按字节 |
| `CLI-OUT-002` | `--output json` | stdout 恰好一行 JSON |
| `CLI-OUT-003` | JSON 键序 | 稳定，按约定顺序 |
| `CLI-OUT-004` | 结构化错误 | `code` / `category` / `message` |
| `CLI-OUT-005` | 能力列表 | 排序稳定 |
| `CLI-OUT-006` | 密钥 | 永不回显 |
| `CLI-OUT-007` | 中文 / emoji | 宽度正确，不截断 |
| `CLI-OUT-008` | 空列表 | 不输出 `null` |
| `CLI-OUT-009` | 文案 | 不含 sandbox / 沙箱 |
| `CLI-OUT-010` | 日志行 | 前缀与时间稳定 |
| `CLI-OUT-011` | 分页 | `next` 原样 |
| `CLI-OUT-012` | `ops` | 按 id 排序 |

## 12.3 退出码（`CLI-EXIT`）

| ID | 场景 | 退出码 |
| --- | --- | --- |
| `CLI-EXIT-001` | `Failure` | 1 |
| `CLI-EXIT-002` | `Usage` | 2 |
| `CLI-EXIT-003` | `NotFound` | 3 |
| `CLI-EXIT-004` | `Conflict` | 4 |
| `CLI-EXIT-005` | `Backend` | 5 |
| `CLI-EXIT-006` | `Unauthorized` | 6 |
| `CLI-EXIT-007` | `Cancelled` | 130 |
| `CLI-EXIT-008` | 插件不存在 | 3 |
| `CLI-EXIT-009` | 能力未授予 | 6 |
| `CLI-EXIT-010` | 插件崩溃 | 5 |

## 12.4 交互（`CLI-UI`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `CLI-UI-001` | 长步骤 | spinner 单行刷新 |
| `CLI-UI-002` | JSON 模式 | 无进度输出 |
| `CLI-UI-003` | 进度去向 | stderr，不污染 stdout |
| `CLI-UI-004` | 无静默等待 | 每阶段有文案 |
| `CLI-UI-005` | 下载 | 字节进度条 |
| `CLI-UI-006` | 非 TTY | 不输出动画，保留阶段行 |
| `CLI-UI-007` | Ctrl-C | 取消并退出 130 |
| `CLI-UI-008` | 错误 | 带可操作提示 |
| `CLI-UI-009` | 安装向导取消 | 回退记录 / 产物，静默退出 130 |

## 12.5 Web 投影（`WEB-*`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `WEB-DESC-001` | 合法 descriptor | 解析为 op 表 |
| `WEB-DESC-002` | 未知 display | 安装即拒绝，Web 不会收到 |
| `WEB-DESC-003` | ETag | 内容不变则 304 |
| `WEB-DESC-004` | 配置 schema | 生成表单字段 |
| `WEB-DESC-005` | 密钥字段 | 输入打码，读取 `***` |
| `WEB-DESC-006` | 分页 | 使用 descriptor 的上限 |
| `WEB-DESC-007` | 版本 | `api` 不匹配拒绝渲染 |
| `WEB-DESC-008` | 空 ops | 只显示通用区域 |
| `WEB-DESC-009` | 超长标题 | 截断，不破坏布局 |
| `WEB-DESC-010` | 未知扩展键 | 忽略 |
| `WEB-RENDER-001` | form | 输入 → 提交 |
| `WEB-RENDER-002` | table | 列、分页、空态 |
| `WEB-RENDER-003` | detail | 键值顺序稳定 |
| `WEB-RENDER-004` | chat | 消息、发送、实时追加 |
| `WEB-RENDER-005` | log | 追加、自动滚动、级别 |
| `WEB-RENDER-006` | metrics | 数值、单位、趋势 |
| `WEB-RENDER-007` | 形状不匹配 | 降级 table / detail |
| `WEB-RENDER-008` | 转义 | `<img onerror>` 按文本 |
| `WEB-RENDER-009` | minecraft-text | `§` 颜色正确，不产生 HTML |
| `WEB-RENDER-010` | markdown-sanitized | 危险标签 / 属性被剥离 |
| `WEB-RENDER-011` | 空结果 | 空态，不报错 |
| `WEB-RENDER-012` | 插件崩溃 | `Unavailable` 卡片 + 重试 |
| `WEB-REST-001` | 列表 | 分页与过滤 |
| `WEB-REST-002` | 详情 | manifest + 授权 |
| `WEB-REST-003` | descriptor | ETag / 缓存 |
| `WEB-REST-004` | ops | 与 CLI 同源 |
| `WEB-REST-005` | call query | 200 |
| `WEB-REST-006` | call action | 与 CLI 输入输出一致 |
| `WEB-REST-007` | 流式 | 101 / WS |
| `WEB-REST-008` | 配置读写 | 密钥打码 |
| `WEB-REST-009` | 启停 | 状态码映射 |
| `WEB-REST-010` | 授权 | grant / revoke |
| `WEB-REST-011` | 错误 | 400/403/404/409/502/503/504 |
| `WEB-REST-012` | 未授权 | 401 / 403 区分 |
| `WEB-WS-001` | 订阅 | 持续收到帧 |
| `WEB-WS-002` | 断线重连 | cursor 续读 |
| `WEB-WS-003` | 背压 | 丢弃并提示刷新 |
| `WEB-WS-004` | 关闭 | 不悬挂 |
| `WEB-WS-005` | 多标签页 | 各自独立会话 |
| `WEB-WS-006` | 插件崩溃 | 关闭并给出原因 |
| `WEB-AUTH-001` | Viewer + read | 允许 |
| `WEB-AUTH-002` | Viewer + mutating | 403 |
| `WEB-AUTH-003` | Operator + mutating | 允许 |
| `WEB-AUTH-004` | Operator + destructive | 403 |
| `WEB-AUTH-005` | Admin + destructive | 允许 + 二次确认 |
| `WEB-AUTH-006` | 能力未授予 | 403 + 申请入口 |
| `WEB-AUTH-007` | 撤销后刷新 | 立即失效 |
| `WEB-AUTH-008` | 过期令牌 | 401 |

## 12.6 SDK（`SDK-*`）

| ID | 场景 | 期望 |
| --- | --- | --- |
| `SDK-RS-001` | `run` 握手 | 自动读端点 / 令牌 |
| `SDK-RS-002` | `--dump-descriptor` | 输出 JSON，不连接宿主 |
| `SDK-RS-003` | 配置反序列化 | 类型与默认值 |
| `SDK-RS-004` | op 路由 | 按 id 分发 |
| `SDK-RS-005` | 事件反序列化 | 强类型 |
| `SDK-RS-006` | 宿主调用 | `ctx.*` 映射到帧 |
| `SDK-RS-007` | 取消 | 传播到 handler |
| `SDK-RS-008` | 错误映射 | `PluginError` → 帧 |
| `SDK-RS-009` | 退出码 | 0/2/3/4 |
| `SDK-RS-010` | `MockHost` | 无 socket 测试 |
| `SDK-RS-011` | 无运行时依赖 | 不引入 schema 校验库路径 |
| `SDK-RS-012` | 重入 | 成功 |
| `SDK-RS-013` | 流 | 分块与 `done` |
| `SDK-RS-014` | 密钥配置 | 不写日志 |
| `SDK-RS-015` | MSRV | 声明版本可编译 |
| `SDK-TS-001` | `definePlugin` | 生成 manifest 片段 |
| `SDK-TS-002` | `build` | descriptor + zip |
| `SDK-TS-003` | `--dump-descriptor` | 不连接宿主 |
| `SDK-TS-004` | zod 4 | 仅支持 4.x |
| `SDK-TS-005` | zod peer | 不打包第二份 |
| `SDK-TS-006` | ESM / CJS | 两种都能加载 |
| `SDK-TS-007` | Node 20 / 22 | 测试通过 |
| `SDK-TS-008` | 事件 | 类型安全 |
| `SDK-TS-009` | 宿主调用 | Promise / 超时 |
| `SDK-TS-010` | 取消 | AbortSignal |
| `SDK-TS-011` | 错误 | `PluginError` |
| `SDK-TS-012` | 退出 | 0/2/3/4 |
| `SDK-TS-013` | `createMockHost` | 无 socket |
| `SDK-TS-014` | 流 | async iterator |
| `SDK-TS-015` | 打包 | 无 devDependency 进入产物 |
| `SDK-TS-016` | `ctx.storage` | 仅 `storage:kv` 时存在，读写走同一 KV |
| `SDK-TS-017` | `ctx.locale` | welcome 下发；缺失回退 `en` |
| `SDK-PY-001` | `--dump-descriptor` | 与 Rust 逐字节一致 |
| `SDK-PY-002` | pydantic 配置 | 类型、默认值、`$defs` 内联 |
| `SDK-PY-003` | op 路由 | 按 id 分发 |
| `SDK-PY-004` | 事件 | 反序列化为强类型 |
| `SDK-PY-005` | 宿主调用 | 异步且可取消 |
| `SDK-PY-006` | 取消 | 传播到 handler |
| `SDK-PY-007` | 错误映射 | `PluginError` → 帧 |
| `SDK-PY-008` | Optional 折叠 | 不出现 `null` 字段 |
| `SDK-JAVA-001` | `--dump-descriptor` | 与 Rust 逐字节一致 |
| `SDK-JAVA-002` | record 配置 | 类型、默认值、`$defs` 内联 |
| `SDK-JAVA-003` | op 路由 | 按 id 分发 |
| `SDK-JAVA-004` | 事件 | 反序列化为强类型 |
| `SDK-JAVA-005` | 宿主调用 | 可取消 |
| `SDK-JAVA-006` | 取消 | 传播到 handler |
| `SDK-JAVA-007` | 错误映射 | `PluginError` → 帧 |
| `SDK-JAVA-008` | TCP 回退 | 端点解析与重连 |
| `SDK-GEN-001` | schema 子集 | 只生成允许构造 |
| `SDK-GEN-002` | 构建时生成 | 运行时无生成入口 |
| `SDK-GEN-003` | dump 不连接 | 无端点也成功 |
| `SDK-GEN-004` | 归一化 | 键排序、去噪 |
| `SDK-GEN-005` | `$defs` 内联 | 无残留 |
| `SDK-GEN-006` | Optional 折叠 | 不出现 `null` |
| `SDK-GEN-007` | 四语言一致 | 逐字节相等 |
| `SDK-GEN-008` | 版本常量 | 与宿主一致 |
| `SDK-GEN-009` | 主版本规则 | SDK major == API major |
| `SDK-GEN-010` | 契约生成 | `git diff` 无漂移 |

`SDK-PY-*` / `SDK-JAVA-*` 的 8 项固定为：dump、配置、op 路由、事件、宿主调用、
取消、错误映射、归一化。测试名必须带完整 ID，便于报告器统计。

---

# 13. 端到端与性能

## 13.1 端到端（`E2E`）

| ID | 流程 | 期望 |
| --- | --- | --- |
| `E2E-001` | 安装 → 启用 → 调用 → 日志 → 停用 | 全链路成功 |
| `E2E-002` | 聊天桥接收游戏聊天 | 控制台与 Web 数据一致 |
| `E2E-003` | 聊天桥发送消息 | 只发一次，不重复 |
| `E2E-004` | AI agent 对话 | 流式返回 |
| `E2E-005` | 监控 → 故障 → 重启 → 通告 | 契约链路完整 |
| `E2E-006` | 升级失败 | 自动回滚，服务不中断 |
| `E2E-007` | 崩溃循环 | `failed` 后不再重启 |
| `E2E-008` | 配置错误 | 回滚并提示 |
| `E2E-009` | 未授权操作 | 拒绝且有审计 |
| `E2E-010` | 卸载 | 数据保留、密钥删除，进程退出 |
| `E2E-011` | 多服务器 | 事件不串服 |
| `E2E-012` | 宿主重启 | 插件按策略恢复 |

端到端用真实 CLI + 真实 fixture 插件 + 临时数据目录，不 mock 协议。

## 13.2 性能（`BENCH`）

| ID | 指标 | 阈值 |
| --- | --- | --- |
| `BENCH-001` | 冷启动握手 p95 | < 500 ms |
| `BENCH-002` | 热启动握手 p95 | < 150 ms |
| `BENCH-003` | op 额外开销 p95 | < 5 ms |
| `BENCH-004` | 事件扇出 | 5000 事件/秒不丢 |
| `BENCH-005` | 宿主空闲内存 | < 40 MiB |
| `BENCH-006` | 100 个非 resident 插件列表 | < 1 s |

基准不进入 `check.sh`；M1 起每个里程碑跑一次，结果写入
`docs/plugins-progress.md`。回归超过 20% 必须解释或修复。

落位（三个独立目标，均不进入 `check.sh`）：

| 目标 | 覆盖 | 跑法 |
| --- | --- | --- |
| `apps/bench/benches/plugin.rs` | `BENCH-003`、`BENCH-004`、`BENCH-006` | `cargo bench -p bench --bench plugin` |
| `apps/bench/benches/plugin_handshake.rs` | `BENCH-001`、`BENCH-002` | `cargo bench -p bench --bench plugin_handshake` |
| `apps/bench/benches/plugin_memory.rs` | `BENCH-005` | `cargo bench -p bench --bench plugin_memory` |

- `BENCH-001` / `002`：真实 `PluginHost` 拉起真实子进程。冷启动每次使用新拷贝
  的 fixture（新 inode 的首次执行，含可执行校验），热启动复用同一产物目录。
  自采样 30 次取 p95，超预算退出码非 0。fixture 是 `apps/bench` 自带的
  `mcsup-plugin-bench-fixture`，只依赖门面，靠 `CARGO_BIN_EXE_*` 定位。
- `BENCH-005`：基准把自身当宿主进程再拉一次，构造 `PluginHost` 后空闲，
  父进程用 `ps` 采样 RSS（空宿主与 100 条记录各一次）。
- `BENCH-006`：真实文件后端下 `PluginService::list` 100 条非 resident 记录。

---

# 14. 覆盖率与门禁

## 14.1 覆盖率阈值

覆盖率是下限，不是目标。先保证条款覆盖，再谈行覆盖。

| 范围 | 行覆盖 | 分支覆盖 |
| --- | --- | --- |
| `domain::plugin` | ≥ 95% | ≥ 90% |
| `protocol::plugin` | ≥ 95% | ≥ 90% |
| `plugin` | ≥ 85% | ≥ 80% |
| `service::plugin` | ≥ 85% | ≥ 80% |
| `apps/cli` 的 `plugin` 命令 | ≥ 80% | ≥ 70% |
| Rust SDK | ≥ 85% | ≥ 75% |
| TS SDK | ≥ 85% | ≥ 75% |
| Python / Java SDK | ≥ 80% | ≥ 70% |

不计入覆盖率的代码必须在源码注释里写明理由（如平台 `cfg` 分支、不可达的
防御分支），并在 `scripts/check-plugin-tests.sh` 的排除表中登记。

`[禁止]` 为了让覆盖率达标而写无断言的「调用一下」测试。

## 14.2 用例覆盖

- `cases.json` 中每个 ID 至少被执行一次。
- 每个 `[必须]` 的规范条款至少有一个正向 + 一个失败路径用例。
- 错误码 11 项、能力 15 项、展示 6 种、契约 6 个、状态 9 个必须逐项覆盖。
- 新增错误码 / 能力 / 契约 / 展示类型时，同一 PR 内补用例，否则门禁失败。

## 14.3 门禁脚本

| 脚本 | 内容 |
| --- | --- |
| `scripts/check.sh` | 既有 Rust 门禁 + `plugin` 单测与集成测试 |
| `scripts/check-sdk.sh` | 四语言 SDK + conformance + 生成一致 |
| `scripts/check-plugin-tests.sh`（`[待实现]`） | 运行用例、汇总报告、比对 ID 集合、检查覆盖率 |

`check-plugin-tests.sh` 的退出条件：

1. 报告中的 ID 集合等于 `cases.json`；
2. 无 `fail` / `ignored` / `skipped`（带有效期的除外）；
3. 覆盖率不低于阈值；
4. 没有新增的 `TODO: test` 标记。

## 14.4 CI 矩阵

| Job | 平台 | 内容 |
| --- | --- | --- |
| `plugin-linux` | ubuntu-latest | 全量 + 安全用例 |
| `plugin-macos` | macos-14 | 全量 + Unix socket |
| `plugin-windows` | windows-latest | 全量 + 命名管道 + TCP 回退 |
| `plugin-msrv` | ubuntu-latest | 最低 Rust 版本 |
| `sdk-rust` | 三平台 | Rust SDK + conformance |
| `sdk-ts` | ubuntu + windows | Node 20 / 22 |
| `sdk-python` | ubuntu | 3.11 / 3.12 |
| `sdk-java` | ubuntu | JDK 21 |
| `plugin-nightly` | 三平台 | 长稳、并发、随机化 |

PR 必须三平台绿；nightly 失败不阻塞合并，但必须在 24 小时内处理或登记。

---

# 15. 里程碑验收

每个里程碑的验收命令必须可复制执行。未列出的用例可以后补，列出的必须全绿。

## M0

```
cargo test -p mcsup-domain plugin
cargo test -p mcsup-protocol plugin
./scripts/check-plugin-tests.sh --only DOM-,PROTO-
./scripts/check-architecture.sh
```

验收：

- DOM-MAN / DES / CAP / STA / CON / EVT 全绿；
- PROTO-FRM / HSK / VER 全绿；
- 四种语言对 `schema/*` 的产物逐字节一致（至少 Rust + TS）；
- 没有插件进程被拉起（用系统进程列表断言）。

## M1

```
cargo test -p mcsup-plugin
cargo test -p cli plugin
./scripts/check-plugin-tests.sh --only HOST-,CAP-,API-,INST-VER,CLI-
```

验收：

- HOST-PRC / EP / HB / RST / IDLE / LIM / EVT / LOG 全绿；
- CAP-GATE、API-SRV / CON 全绿；
- E2E-001 / 002 / 007 / 010 / 012 全绿；
- 三平台端点用例通过；
- 覆盖率达标。

## M2

```
./scripts/check-plugin-tests.sh --only API-SEC,API-NET,CON-,INST-UPG,CLI-CMD,CLI-EXIT
```

验收：

- 六个契约全绿；
- 配置回滚、密钥打码、SSRF 全绿；
- E2E-003 / 004 / 005 / 008 / 009 全绿；
- 契约循环与深度用例通过。

## M3

```
./scripts/check-plugin-tests.sh --only WEB-,API-FS,API-LIFE,API-BAK,API-CNT,E2E-
```

验收：

- 六种 display 的 render plan 全绿；
- REST / WS 契约冻结并有快照；
- CLI / MCP 同源用例通过；
- 破坏性操作权限用例通过。

## M4

```
./scripts/check-sdk.sh
./scripts/check-plugin-tests.sh --only SDK-,SEC-SIG
```

验收：

- 四语言 descriptor 一致；
- 摘要、撤销全绿；签名与 provenance 暂不实现（见 `docs/plugins-progress.md`）；
- Python / Java SDK 过 conformance；
- 升级失败自动回滚端到端通过。

---

# 16. 完成定义与回归

## 16.1 单个 PR 的完成定义

1. 变更涉及的规范条款在追踪矩阵中有测试 ID。
2. 正向与失败路径都有用例。
3. 审计、日志、指标至少覆盖其一（涉及能力 / 契约 / 安装时必须覆盖审计）。
4. 数值变更同步了设计文档、常量与 conformance。
5. 三平台 CI 绿；无法覆盖的平台有明确说明与替代用例。
6. 没有新增 `#[ignore]` / `test.skip` / `TODO: test`。
7. 覆盖率不低于阈值，且不低于变更前。
8. 文档中的示例与实现一致（示例代码可编译或明确标注为形态示例）。

## 16.2 缺陷回归

- 每个 bug 修复必须新增一个测试 ID，命名以 `REG-` 开头并在 `cases.json` 登记。
- `[禁止]` 删除失败用例来让门禁变绿；只能修复实现或按流程调整规范。
- 同一模块连续两次回归失败，必须在 `docs/plugins-progress.md` 记录原因与改进。

## 16.3 Flaky 策略

| 情况 | 处理 |
| --- | --- |
| 首次 flaky | 记录 ID 与日志，重跑一次 |
| 同一 ID 一周内两次 | 视为缺陷，必须修复，不得重试掩盖 |
| 时间 / 端口相关 | 改用暂停时钟与内核分配端口 |
| 网络相关 | 改本地 fixture，禁止真实外网 |
| 平台相关 | 标记 `platforms`，并在其它平台保留等价用例 |

## 16.4 测试数据

- 一律使用系统临时目录下的唯一目录，测试结束删除。
- 不写用户真实 `{data_dir}`；通过显式参数注入。
- 固定端口只在必要时使用，优先内核分配。
- golden 文件不包含机器名、用户名、绝对路径。
- 大文件用生成器，不提交二进制；需要固定字节的用小型夹具。

---

# 17. 附：用例登记示例

```json
{
  "version": 1,
  "cases": [
    {"id": "HOST-EP-002", "kind": "behavior", "input": "behavior/socket_perms.json",
     "expect": "behavior/socket_perms.expected.json"},
    {"id": "PROTO-FRM-006", "kind": "frames", "input": "frames/oversize.jsonl",
     "expect": "frames/oversize.expected.jsonl"},
    {"id": "DOM-DES-021", "kind": "schema", "input": "schema/depth7.json",
     "expect": "schema/depth7.error.json",
     "languages": ["rust", "typescript", "python", "java"]},
    {"id": "SEC-SSRF-007", "kind": "security", "input": "behavior/metadata_ip.json",
     "expect": "behavior/metadata_ip.expected.json"},
    {"id": "WEB-RENDER-004", "kind": "render", "input": "render/chat.json",
     "expect": "render/chat.plan.json"}
  ]
}
```

登记后，任何运行器缺少该 ID 的结果都算失败。新增规范条款时，先在这里加用例，
再写实现；顺序不能反。




