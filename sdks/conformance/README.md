# SDK 一致性用例

四份 SDK（Rust / TypeScript / Python / Java）生成 descriptor 后必须逐字节一致
（`plugins.md` §11.7 / §14.4），运行时行为由 `cases.json` 统一描述。

## 目录

| 路径 | 内容 |
| --- | --- |
| `schema/*.json` | schema 归一化用例（§14.5）：同一组声明 → 期望 descriptor 片段；含禁止构造与超限的失败用例 |
| `frames/*.json` | 帧级用例（§14.1）：握手、调用、错误结果、取消、未知帧 |
| `behavior/*.json` | 行为用例输入 `<name>.json` 与期望 `<name>.expected.json`：调用 / 配置变更 / 握手 / 关闭 / 契约 |
| `render/*.json` | descriptor → 渲染计划：输入 `<name>.json` 与期望计划 `<name>.plan.json` |
| `fixtures/*.json` | 合法 / 非法 manifest、descriptor 样例（安装与审核用例共用） |
| `descriptor/chat-bridge.json` | 参考 descriptor：`chat-bridge` 用例的归一化产物（紧凑 JSON + 换行） |
| `cases.json` | 全部跨语言用例的唯一登记处（§5.1）；`report/` 是 CI 产物，不提交 |

## cases.json 格式

```json
{
  "version": 1,
  "cases": [
    {
      "id": "SDK-BHV-001",
      "kind": "behavior",
      "input": "behavior/query_returns_output.json",
      "expect": "behavior/query_returns_output.expected.json"
    }
  ]
}
```

- `kind` ∈ `frames` / `schema` / `behavior` / `render` / `security`。
- `input` / `expect` 相对 `conformance/`。
- 新增用例先登记，再实现；报告 ID 集合必须等于 `cases.json`（§3.2 / `PLAT-011`）。

## behavior 输入格式

```json
{ "scenario": "op", "op": "counter", "op_kind": "query", "input": { "channel": "global" } }
```

- `scenario` ∈ `op` / `unknown-frame` / `stream` / `config` / `handshake` /
  `shutdown` / `contract`。
- 期望文件与 `plugins.md` 的结果帧同形：`{ "ok": ..., "output": ... }` 或
  `{ "ok": false, "error": { "code": ... } }`；握手 / 关闭类用
  `{ "exit_code": ... }`。

## schema 用例格式

```json
{
  "id": "required-string-maxlength",
  "field": { "name": "token", "type": "string", "required": true, "maxLength": 64 },
  "expect": { "type": "string", "maxLength": 64 }
}
```

- `field` 是语言无关的字段声明：各语言把它映射成自己的 schema 库，再比对
  `expect` 片段（键字典序排序后逐字节相等）。
- `required: false` 断言该字段不出现在 `required`，且产物不含 `null` 类型。
- `absent` 列出片段中不得出现的 JSON 关键字。
- 失败用例用 `failure`：`reason` ∈ `union` / `recursion` / `external-ref` /
  `limit`，`message-contains` 是错误消息必须包含的片段。

## frames 用例格式

```json
{
  "id": "call.result",
  "kind": "call",
  "op": "counter",
  "input": { "channel": "global" },
  "expect": { "ok": true, "output": { "count": 0, "channel": "global" } }
}
```

## Runner 契约

各语言提供同名入口（§5.2）：

```
cargo run -p mcsup-plugin-sdk-conformance -- --cases ../conformance/cases.json
npm run conformance -- --cases ../conformance/cases.json
python -m mcsup_plugin.conformance --cases ../conformance/cases.json
java -jar conformance.jar --cases ../conformance/cases.json
```

- 按 `cases.json` 顺序执行，退出码：全过 0，任一失败 1，清单非法 2。
- 报告行：`{"id":"SDK-BHV-001","status":"pass","duration_ms":1}`。
- 比较前先经 `conformance::canonicalize` 规范化易变字段（§5.3）。

## 当前状态

- `behavior`：13 个 SDK 用例（`SDK-BHV-001..013`）已登记，TypeScript runner
  已驱动；Rust / Python / Java runner 待补。
- `render` / `contract` 样例文件已就位（`render/chat_descriptor.*`、
  `behavior/contract_cycle.*`），待 runner 支持后登记对应
  `WEB-RENDER-*` / `CON-RTE-*` ID。
- Rust 宿主侧用例在 `crates/plugin/tests/host_lifecycle.rs`，矩阵 ID 写进
  测试函数名（如 `con_rte_005_contract_cycle_is_rejected_as_conflict`）。
