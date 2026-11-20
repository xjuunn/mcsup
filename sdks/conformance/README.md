# SDK 一致性用例

四份 SDK（Rust / TypeScript / Python / Java）生成 descriptor 后必须逐字节一致
（`plugins.md` §11.7 / §14.4），运行时行为由 `cases.json` 统一描述。

## 目录

| 路径 | 内容 |
| --- | --- |
| `schema/*.json` | schema 归一化用例（§14.5）：同一组声明 → 期望 descriptor 片段；含禁止构造与超限的失败用例 |
| `frames/*.json` | 帧级用例（§14.1）：握手、调用、错误结果、取消、未知帧 |
| `cases.json` | 行为用例：调用 / 配置变更 / 握手 / 关闭的期望结果 |
| `descriptor/chat-bridge.json` | 参考 descriptor：`chat-bridge` 用例的归一化产物（紧凑 JSON + 换行） |

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

`kind` ∈ `handshake`（可带 `reject`）/ `call` / `cancel` / `unknown-frame`
（`inject` 一帧未知 type）。字节级错误（单帧 1 MiB、深度 32）属传输层，
由各语言的传输测试覆盖，不在此列。

## cases.json 用例格式

| 键 | 说明 |
| --- | --- |
| `kind` | `query` / `action` / `stream` / `config` / `handshake` / `shutdown` / `unknown-frame` |
| `granted` | 可选；覆盖宿主实际授予的能力（默认等于插件声明），用于越界能力用例 |
| `expect` | 期望结果；错误只断言 `code` |

## 新增用例

1. 在对应目录追加一个文件，或在 `cases.json` 的 `cases` 数组追加一条，
   `id` 用 `<领域>.<行为>` 命名；
2. 用例必须能由任意语言的测试驱动复现（不依赖语言特有 API）；
3. 若改变了 `descriptor/chat-bridge.json`，同一次提交里更新它，并在提交信息
   说明归一化规则的变化。

## 覆盖范围

- schema：§14.5 表中 12 条，含 union / 递归 / 外部 `$ref` / 超限的失败路径
- 帧与行为：握手成功 / 拒绝、调用成功 / 输入非法 / 未知 op / 处理器抛错 /
  重入调用 / 越界能力（`denied`）、未知帧忽略、超时、取消、seq 连续与 done、
  配置回滚、关闭退出码

宿主侧职责（心跳丢失检测、流式 seq 缺口检测）由宿主测试套件覆盖：
插件侧的对偶不变量（连续 seq、按时回 pong）已在上述用例中断言。
