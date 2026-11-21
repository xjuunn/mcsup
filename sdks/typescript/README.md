# mcsup-plugin（TypeScript 插件 SDK）

MCSUP 平台插件的 TypeScript SDK。插件作者只需 `export default definePlugin({...})`，
SDK 负责协议帧、握手、分派、流、配置热更新、心跳与退出。

- 规范：`docs/plugins.md`（协议、能力、manifest、descriptor）
- 风格：`docs/sdk-design.md`（单一内核、三层架构、类型安全支点）

本包是**插件 SDK**，不是宿主。宿主在 `crates/plugin`，本包不依赖任何 Rust crate。

## 最小插件

```ts
import { definePlugin, query, s } from "mcsup-plugin";

export default definePlugin({
  id: "hello-world",
  title: "Hello World",
  version: "0.1.0",
  license: "MIT",
  capabilities: ["server:read"],
  config: s.object({ greeting: s.string({ default: "hi" }) }),
  queries: {
    servers: query("servers")
      .output(s.object({ items: s.array(s.string()), next: s.optional(s.string()) }))
      .pagination({ cursorField: "next", limitField: "limit" })
      .run(async (_input, ctx) => ({
        items: (await ctx["server:read"].servers.list()).map((server) => server.name),
      })),
  },
});
```

入口文件只需导出定义，**不用**写 `run()` 样板：`mcsup-plugin build` 会生成
`bin/entry.mjs`，由它导入默认导出并交给 `run()`。若作者自己写了
`process.exit(await run(plugin))`，`run()` 幂等，不会连接两次。

## 字段 DSL（不必直接写 zod）

`s.*` 覆盖 `docs/plugins.md` §11.5 允许的 schema 子集，SDK 内部归约为 zod，
因此仍然同时得到：编译期类型、运行期校验、descriptor 的 JSON Schema。

| 构造 | 产出 |
| --- | --- |
| `s.string({ min, max, pattern, default, label, secret, describe })` | 字符串 |
| `s.integer({ min, max, default })` / `s.number(...)` | 整数 / 浮点 |
| `s.boolean({ default })` | 布尔 |
| `s.enum(["a", "b"], { default })` | 字符串枚举 |
| `s.datetime({ offset })` / `s.uuid()` / `s.url()` | 标准 JSON Schema `format` |
| `s.array(items, { minItems, maxItems })` | 数组 |
| `s.object(shape)` | 对象；值可为 `undefined` 的键成为非必填 |
| `s.optional(schema)` | 可选字段 |

```ts
config: s.object({
  token: s.string({ min: 1, secret: true, label: "Bot Token" }),
  channels: s.array(s.string(), { label: "频道" }),
}),
```

超出子集时直接用 zod（逃生口）：`input` / `output` / `config` 接受
`Field<T> | z.ZodType`，两者可以混用，例如 `s.object({ raw: z.string().email() })`。
`z` 仍是 peer dependency。

## 公开面

| 导出 | 作用 |
| --- | --- |
| `definePlugin({...})` | 唯一的插件装配点 |
| `s` | 字段 DSL（上方） |
| `z` | 重新导出的 zod（peer dependency，逃生口） |
| `run(plugin)` | 连接宿主、握手、分派、退出（构建产物已自动调用） |
| `PluginContext` | 能力客户端，形状由 `capabilities` 推导 |
| `PluginError` | 带 `code` / `category` / `retryable` 的错误 |
| `testing.createMockHost()` | 内存双工宿主，不开 socket |

Builder（`query` / `action` / `stream` / `op` / `lineFilter`）、Hooks
（`useConsoleTail` / `useRetry` / `useKvCache` / `useEvents`）与预设属次版本
可选 API，用法见 `docs/sdk-design.md` §4。

内部实现（`Session`、`ExitCode`、descriptor 校验常量、订阅归约函数）不从包入口
导出；测试需要时从具体模块导入。

## 工具集

常用函数已由 SDK 提供，插件不要再各自实现一份。全部是纯函数或显式接能力
client 的工厂，不依赖模块级状态：

```ts
import {
  sanitizeChat, stripColorCodes, buildCommand, isPlayerName,  // Minecraft
  withRetry, pLimit, mapLimit, buildUrl,                     // 网络与重试
  chunk, clamp, truncate, unique, sleep, randomId, nowIso,    // 通用
  kvJson, kvCounter, kvCursor, kvTtlCache,                    // 状态存储
} from "mcsup-plugin";
```

| 分组 | 提供 |
| --- | --- |
| 通用 | `sleep` `clamp` `chunk` `unique` `truncate`（按码点）`randomId` `nowIso` |
| 网络与重试 | `withRetry`（指数退避 + 抖动，只重试 retryable 错误）`pLimit` `mapLimit` `buildUrl` |
| Minecraft | `stripColorCodes` `legacyColorCodes` `sanitizeChat`（去控制字符、折叠换行、限长）`isPlayerName` `buildCommand`（参数转义） |
| 状态存储 | `kvJson` `kvCounter` `kvCursor`（只前进）`kvTtlCache`（TTL + 读穿） |

示例：

```ts
const counter = kvCounter(ctx["storage:kv"], "stats:");
await counter.incr("chat:forwarded");

const text = sanitizeChat(event.message);
await ctx["console:write"].sendLine("survival", buildCommand("say", [text]));

await withRetry(() => ctx["net:outbound"].request({ url }), { attempts: 3 });
```

`utils` 属于次版本可选 API；纯函数不接 `ctx`，状态工具显式传入
`ctx["storage:kv"]`，能力未授予时会在取 client 处直接 `denied`。

## 能力驱动的 Ctx

`ctx` 只包含已声明能力对应的字段（原生对象写法与 Builder 写法一致）：

```ts
const plugin = definePlugin({
  // ...
  capabilities: ["console:read"],
  onStart(ctx) {
    ctx["console:read"].tail("survival"); // OK
    // ctx["console:write"]                // 编译期报错：未声明
  },
});
```

运行期只挂载宿主实际授予的字段；声明了但未被授予时，访问会抛出
`PluginError("denied")`，而不是静默返回 `undefined`。

`config` 的类型同样来自 schema，`onStart` / `onConfigChange` / `run` 里都能直接用：

```ts
config: s.object({ greeting: s.string({ default: "hi" }) }),
onStart(ctx) {
  ctx.log.info(ctx.config.greeting); // string
},
```

Builder 写法用 `.capabilities([...])` 收紧处理函数里的 `ctx`：

```ts
const status = query("status")
  .capabilities(["server:read"])
  .output(s.object({ state: s.string() }))
  .run(async (_input, ctx) => {
    const servers = await ctx["server:read"].servers.list();
    return { state: servers[0]?.status ?? "unknown" };
  });
```

## 错误

所有失败都是 `PluginError`，`code` 与 `docs/plugins.md` §6.4 一一对应；
`timeout` / `unavailable` / `crashed` 标记 `retryable`，可直接喂给 `useRetry`。

未授予的能力调用返回 `denied`，SDK 不降级、不假装成功。

## 构建

```sh
npx mcsup-plugin build --out build
```

产物：

```
build/
├── manifest.json      # plugins.md §10
├── descriptor.json    # plugins.md §11（归一化后，紧凑 JSON + 换行）
├── bin/entry.mjs      # 固定名入口包装：导入默认导出并调用 run()
├── bin/run.sh         # 平台启动器（Windows 为 bin/run.cmd）
└── dist/...           # tsc 编译产物
```

`manifest.entry.cmd` 指向 `bin/run.sh`，`platforms` 给出各平台启动器；
Windows 上宿主经 `cmd.exe /C` 执行 `bin/run.cmd`。宿主拉起时追加
`--endpoint ...` 并注入 `MCSUP_PLUGIN_TOKEN`。

## 测试

```sh
npm run build   # 构建 dist（process 用例依赖它）
npm test        # vitest：协议 / descriptor / builders / hooks / runtime / conformance / 真实进程
```

`test/conformance.test.ts` 读取仓库内 `sdks/conformance/cases.json`，与其它语言
SDK 共用同一组行为用例。

## 版本

SDK 主版本 == plugin API 主版本（`docs/plugins.md` §14.3）。插件写不写 `api`
都可以：省略时握手使用当前 SDK 版本，写了不一致的值会在 `definePlugin` 时报错。