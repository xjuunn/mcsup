# SDK 一致性用例

四份 SDK（Rust / TypeScript / Python / Go）生成 descriptor 后必须逐字节一致
（`plugins.md` §11.7 / §14.4），运行时行为由 `cases.json` 统一描述。

## 目录

| 路径 | 内容 |
| --- | --- |
| `cases.json` | 行为用例：一次调用 / 配置变更 / 握手 / 关闭的期望结果 |
| `descriptor/chat-bridge.json` | 参考 descriptor：`chat-bridge` 用例的归一化产物（紧凑 JSON + 换行） |

## 新增用例

1. 在 `cases.json` 的 `cases` 数组追加一条，`id` 用 `<领域>.<行为>` 命名；
2. 用例必须能由任意语言的测试驱动复现（不依赖语言特有 API）；
3. 若改变了 `descriptor/chat-bridge.json`，同一次提交里更新它，并在提交信息
   说明归一化规则的变化。

## 覆盖范围

- 握手：`reject` 退出码、初始配置非法
- 调用：正常返回、输入校验失败、未知 op、处理器抛错
- 流：seq 连续、done 标记
- 配置：应用成功、非法配置保留旧值
- 关闭：`shutdown` 后退出码 0

失败码与 `plugins.md` §6.4 的错误码表一致；分类与可重试性由 SDK 提供，
不由作者拼装。
