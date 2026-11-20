# 插件 SDK 设计规约

本文定义插件 SDK 的**代码风格架构与设计要求**，是 `docs/plugins.md` 第 14 节的
细化：第 14 节回答「SDK 是什么、公开面多大、版本怎么绑」，本文回答「插件作者
用什么风格写、三层结构怎么划、类型安全怎么保、哪些做法禁止」。

设计或实现任何语言 SDK、评审插件作者体验、新增 hooks / builder 时查阅本文。
协议、宿主、能力目录以 `docs/plugins.md` 为准；两处不一致时以 `plugins.md`
为准并回改本文。

# 1. 定位与范围

## 1.1 SDK 服务的是插件作者

SDK 的唯一目标：让插件作者用**最少的代码、最强的类型保证**写出一个正确的
插件。衡量标准（按优先级）：

1. 最小插件行数与概念数
2. 越权、拼错、漏校验在**编译期**暴露
3. 运行期失败有统一、可恢复的错误类型
4. 新增一个功能（op / 订阅 / 能力）改动量趋近一行
5. 性能（SDK 自身的调度与序列化开销可忽略）

## 1.2 范围

本文覆盖：

- 三层架构（内核 / Builder / Hooks）与依赖方向
- 各层的边界规约与命名规约
- 类型安全的四个支点
- descriptor 生成与契约消费的落点（与 `plugins.md` §14.4 对齐）
- 明确禁止的做法及理由

本文不覆盖：帧协议、能力清单、宿主行为——见 `docs/plugins.md`。

## 1.3 与现有文档的关系

`plugins.md` 附录 D 的 TypeScript 示例（`queries` / `actions` / `streams` /
`onStart`）是本规约中**内核层**的权威形态。本文不改变它，只在其上补充
Builder 与 Hooks 两层糖。

# 2. 多语言策略

## 2.1 风格规约当前只针对 TypeScript

当前唯一明确指定代码风格的是 TypeScript SDK（`sdks/typescript`，npm 包
`mcsup-plugin`）。本文第 4–7 节的风格细节**只约束 TypeScript**。

Rust / Java / Python / Go 等语言的 SDK 在立项设计时，再按语言习惯确定各自
的风格（如 Rust 可能是宏 + trait，Python 可能是装饰器 + dataclass），**不得
提前照搬 TypeScript 的三层糖**。但下列内容对所有语言通用：

| 通用层 | 内容 | 来源 |
| --- | --- | --- |
| 内核中间表示 | 「一份插件定义」的语义模型：元数据 + 能力 + config schema + ops + 生命周期 | 本文 §3 |
| descriptor 归一化管线 | 类型 → descriptor 的生成方向与归一化要求 | `plugins.md` §14.4 |
| 契约消费管线 | protocol contracts → 各语言类型 | `plugins.md` §14.4 |
| 分层与禁止项 | 禁止业务能力进 SDK、禁止隐式注册、禁止破坏单一内核 | 本文 §5、§8 |

## 2.2 新语言 SDK 的立项动作

设计新语言 SDK 时按顺序回答：

1. 该语言的「声明式内核」长什么样？（对象字面量 / trait impl / 装饰器 / struct）
2. 该语言的 Builder 与 Hooks 糖是否值得引入？（不强求三件套齐全，只保留内核）
3. 该语言的 schema 层选型？（Rust schemars / Python pydantic / Java victools，
   见 `plugins.md` §14.4）
4. 结论与示例补进本文 §2.1 的表格，并在 `sdks/<lang>/README.md` 落地。

# 3. 核心模型：单一内核

## 3.1 一切归约到 PluginDefinition

SDK 内部只有一种插件表示（语义模型，TypeScript 里即 `definePlugin` 的参数）：

```
PluginDefinition =
    meta          插件元数据（id / version / title / …，→ manifest）
  + capabilities  能力声明（→ manifest，决定 Ctx 的形状）
  + config        zod schema（→ descriptor config 节 + 运行时校验 + TS 类型）
  + queries       查询型 op（→ descriptor ops 节）
  + actions       动作型 op（→ descriptor ops 节）
  + streams       流式 op（→ descriptor ops 节）
  + 生命周期      onStart / onStop / onConfigChange / 事件订阅
```

规则：

- `[规范]` 任何风格（内核、Builder、Hooks、预设）最终都必须归约到同一个
  `PluginDefinition`；不存在第二种插件表示。
- `[规范]` manifest 与 descriptor 由 `PluginDefinition` 在构建时生成
  （`plugins.md` §14.4 的生产方向），作者不手写 JSON，不存在两份漂移。
- `[禁止]` Builder 或 Hooks 绕过内核直接产生「可运行的插件」。

## 3.2 为什么单一内核是硬性的

- 两种写法的作者共享同一组类型推导、同一个 `Ctx`、同一份生成产物，互转是
  纯代码搬移；
- descriptor 归一化只需实现一次（`plugins.md` §14.4 要求多语言产物逐字节
  一致，前提就是中间表示唯一）；
- 未来加任何新风格（函数式管道、事件总线等），只需再写一个归约适配器，
  内核与已发布的插件不受影响。

# 4. 三层架构

```
预设工厂（可选，最外层糖，如 chatBridgePreset）
        ↓ 产出
链式 Builder（构造「值」片段）     Hooks（构造「行为」块）
        ↘                     ↙
     definePlugin（唯一内核）→ PluginDefinition → manifest / descriptor
```

| 层 | 产物 | 使用场景 |
| --- | --- | --- |
| 内核 `definePlugin` | 完整插件定义 | 插件的唯一装配点 |
| Builder | `PluginDefinition` 的**片段**（op、过滤管线、schema 组合等值对象） | 可复用、可导出、可单测的工具对象 |
| Hooks | 需要插件上下文的**行为块**（tail 状态机、重试、KV 缓存等） | 跨插件复用逻辑，一行接入 |
| 预设工厂 | 完整 `PluginDefinition` | 常见插件形态的半成品模板 |

## 4.1 内核层（声明式对象）

插件的唯一装配点。作者在 `definePlugin({...})` 里声明元数据、能力、config、
ops 与生命周期，形态与 `plugins.md` 附录 D 一致。特性：

- 类型全部推导：config 类型来自 zod，op 输入输出来自 schema，无需手写类型标注
- 装配点唯一：一个 `definePlugin` 调用 = 一个插件，没有隐式注册
- 热更新语义显式：`onConfigChange` 由作者显式处理或走默认重读

## 4.2 Builder 层（链式，造「值」）

链式 Builder 用于构造 `PluginDefinition` 的片段，片段再回填进声明式对象：

```ts
const statusOp = op("status")
  .kind("query")
  .title("运行状态")
  .run((input, ctx) => ctx.state.summary());

const filter = lineFilter()
  .stripAnsi()
  .include(/hello/i)
  .map((line) => parseChat(line.text));

export default definePlugin({
  // ...
  ops: { status: statusOp },     // 片段回填
});
```

特性与约束：

- Builder 是纯值构造器：无环境状态、无 IO、可单测、可导出为工具库
- Builder 的终点**必须**是内核可接受的片段类型，而不是插件
- 允许在 Builder 链中做输入校验与形状推导，产出物类型进内核时完成最终检查

## 4.3 Hooks 层（造「行为」）

Hook 是「需要插件上下文的可复用行为工厂」，统一签名 `useX(ctx, options)`，
返回一个能力对象：

```ts
function useConsoleTail(ctx: Ctx, opts: { since: "tail" }) {
  return {
    subscribe(cb: (line: Line, server: string) => void): void { /* 游标、轮询、重连在内部 */ },
    cursor(server: string): number { /* ... */ },
  };
}

export default definePlugin({
  // ...
  start(ctx) {
    const tail = useConsoleTail(ctx, { since: "tail" });
    tail.subscribe((line, server) => ctx.log(`[${server}] ${line.text}`));
  },
});
```

特性与约束：

- Hook 显式接收 `ctx`，`[禁止]` 依赖模块级隐式执行上下文（React 式）；
  hook 在未获得合法 `ctx` 时不得产生任何副作用
- Hook 返回的能力对象承担全部脏活（轮询、游标、重试、缓存），业务回调只留
  一个函数——这是「新功能只改极少代码」的主要杠杆
- 内置 hooks 保持小而稳；第三方用同一 `Ctx` 类型发布自己的 `useX` 包，
  类型约束（§6.1）使非官方 hook 同样编译期安全

## 4.4 预设工厂层（可选）

针对常见插件形态提供半成品模板，内部仍归约到内核：

```ts
export default chatBridgePreset({
  id: "chat-bridge",
  config: z.object({ emitChat: z.boolean().default(true) }),
  onLine(ctx, line) { /* ... */ },
});
```

- 预设必须声明它固定了哪些能力与 op，作者跳出的部分回落到内核写法
- 预设属于 SDK 的次版本可选 API（§9 版本规则），不进入最小公开面

## 4.5 工具集层（次版本可选）

通用函数集中放在 `utils`，插件不要再各自实现一份：

- 纯函数：文本 / 时间 / 切片 / 截断、重试与并发、Minecraft 颜色码与命令转义
- 需要状态的封装（KV 计数、游标、TTL 缓存）**显式接能力 client**
  （`kvCounter(ctx["storage:kv"])`），不接 `ctx`、不做隐式注册，能力未授予时
  在取 client 处直接 `denied`
- 命名不用 `useX`（Hooks 专用，§7）；工具不得包含第三方平台客户端或业务判定，
  边界同 `plugins.md` §14.2 的禁止项

# 5. 三条边界规约（硬性）

1. **Builder 只造片段，不造插件**：`.build()` 的产物必须是内核可接受的片段；
   半成品状态只允许存在于片段级别。
2. **Hooks 显式接 ctx，禁止顶层隐式注册**：hook 的注册行为只发生在
   `definePlugin` 参数与生命周期回调内部；模块 import 不得产生注册副作用。
3. **依赖方向单向**：预设 → Builder / Hooks → 内核。Hooks 内部可以用 Builder；
   Builder 不依赖 Hooks；两者都不得暴露插件级注册口。

违反任一条的实现不予合入。

# 6. 类型安全要求

四个支点，全部落在内核类型上：

## 6.1 能力驱动的 Ctx

`Ctx` 的形状由 `capabilities` 字面量数组推导：未声明 `console:read` 时
`ctx.console` 不存在（编译期报错），声明了 `storage:kv` 时 `ctx.kv` 才出现。
越权调用在编译期失败，而不是等宿主拒绝。第三方 Hook 的 `ctx` 参数同样受此
约束，非官方 hook 无法要求未声明的能力。

## 6.2 zod 是唯一 schema 层

- config 与 op 的输入输出只写一遍 zod，同时得到：编译期类型、运行时校验、
  descriptor 的 JSON schema（构建时生成，`plugins.md` §14.4）
- `[禁止]` 绕过 zod 手写类型再单独手写 schema
- `[规范]` default 写齐后字段类型非空；热更新用同一 schema 校验，编译期与
  运行时零漂移
- 密钥、展示标签等扩展语义走 zod `.meta()`（对齐附录 D 的 `secret` / `label`）

## 6.3 推导优先

op 输出类型从 `run` 的返回值与 output schema 双向校验；能用推导的不给作者
留手写标注位。SDK 公开面里不出现要求作者写 `any` / 断言的位置。

## 6.4 统一错误类型

运行期失败统一为带 `code` / `category` / `retryable` 的错误类型（对齐
`plugins.md` §6 错误码），重试判断由 SDK 提供，作者不解析字符串。

# 7. 命名与 API 规约

| 对象 | 规约 | 示例 |
| --- | --- | --- |
| 内核入口 | `definePlugin`，参数键与附录 D 一致（`config` / `queries` / `actions` / `streams` / `onStart` / `onStop`） | `definePlugin({...})` |
| Builder 工厂 | 小写动词或名词工厂函数，返回链式 builder，以 `.build()` 收尾 | `op("status")`、`lineFilter()` |
| Hook | `useX` 前缀，第一参数恒为 `ctx` | `useConsoleTail(ctx, opts)` |
| 工具 | 普通动词 / 名词函数，不用 `useX`；状态类工厂显式接能力 client | `sanitizeChat`、`kvCounter(kv)` |
| 预设 | `xPreset` 后缀 | `chatBridgePreset({...})` |
| 错误 | `PluginError` | — |
| 测试 | `testing.createMockHost()`（对齐 `plugins.md` §14.2） | — |

# 8. 明确禁止（及理由）

| 禁止项 | 理由 |
| --- | --- |
| 装饰器注册 | 实验性语法，`emitDecoratorMetadata` 在 esbuild/tsx 下不一致，类型擦除破坏类型安全 |
| 响应式 / Signal 状态模型 | 热更新语义被迫隐式化，调试栈深，非前端作者心智负担大 |
| 模块顶层隐式注册（import 副作用） | 注册时机不可见、顺序不可控，与「装配点唯一」冲突 |
| Builder 直接产出可运行插件 | 引入插件级半成品状态 |
| 函数式管道 / Mixin 作为 SDK 风格 | 报错可读性差或冲突规约成本高；如需支持，走归约适配器，不进内核 |
| SDK 内置业务能力（Telegram 客户端、AI 客户端等） | `plugins.md` §14.2 已禁止；SDK 只做协议、类型、生命周期 |
| 多于一个的插件表示 | 见 §3.2 |

# 9. 版本与演进

- 版本绑定沿用 `plugins.md` §14.3：SDK 主版本 == plugin API 主版本。
- Builder / Hooks / 预设属于次版本可选 API；内核参数键的变更视同主版本变更。
- 新增风格走「归约适配器」路径：先给出到 `PluginDefinition` 的映射，再讨论
  是否进公开面；无法归约的风格不收。
- 多语言立项按 §2.2 执行，结论回填本文。

# 10. 测试要求

- 三层各自可独立单测：内核（定义 → 产物）、Builder（片段形状）、Hook
  （行为块）
- 与宿主的交互用 `testing.createMockHost()`（内存双工，不开 socket）；
  跨语言一致性用 `sdks/conformance`（对齐 `plugins.md` §20）
- 生成管线测试：同一 `PluginDefinition` 在两次构建下 descriptor 逐字节一致
- 每个内置 hook 至少一条「无副作用前提失败即抛错」的用例

# 11. 参考实现

`/Users/Admin/Projects/chat-bridge`（仓库外）是本规约的参考实现素材：其手写
协议层（连接、握手、轮询、游标、错误节流）在 SDK 化后应完整消失于
`useConsoleTail` 内部，业务侧只保留解析与输出配置。SDK 首版验收以
「chat-bridge 迁移后行数与概念数显著下降、行为不变」为准。
