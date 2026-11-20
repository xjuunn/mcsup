# mcsup-plugin（TypeScript 插件 SDK）

MCSUP 平台插件的 TypeScript SDK。插件作者只需声明一个 `PluginDefinition`，
SDK 负责协议帧、握手、分派、流、配置热更新、心跳与退出。

- 规范：`docs/plugins.md`（协议、能力、manifest、descriptor）
- 风格：`docs/sdk-design.md`（单一内核、三层架构、类型安全支点）

本包是**插件 SDK**，不是宿主。宿主在 `crates/plugin`，本包不依赖任何 Rust crate。

## 最小插件

```ts
import { definePlugin, run, z } from "mcsup-plugin";

export default definePlugin({
  id: "hello-world",
  title: "Hello World",
  version: "0.1.0",
  license: "MIT",
  capabilities: ["server:read"],
  config: z.object({ greeting: z.string().default("hi") }),
  queries: {
    servers: {
      kind: "query",
      input: z.object({}),
      output: z.array(z.string()),
      run: async (_input, ctx) => (await ctx["server:read"].servers.list()).map((s) => s.name),
    },
  },
});

process.exit(await run((await import("./plugin.js")).default));
```

入口文件必须调用 `run(...)`；构建产物由 `mcsup-plugin build` 打包。

## 公开面

| 导出 | 作用 |
| --- | --- |
| `definePlugin({...})` | 唯一的插件装配点 |
| `run(plugin)` | 连接宿主、握手、分派、退出 |
| `z` | 重新导出的 zod（peer dependency） |
| `PluginContext` | 能力客户端，形状由 `capabilities` 推导 |
| `PluginError` | 带 `code` / `category` / `retryable` 的错误 |
| `testing.createMockHost()` | 内存双工宿主，不开 socket |

Builder（`query` / `action` / `stream` / `op` / `lineFilter`）、Hooks
（`useConsoleTail` / `useRetry` / `useKvCache` / `useEvents`）与预设属次版本
可选 API，用法见 `docs/sdk-design.md` §4。

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

Builder 写法用 `.capabilities([...])` 收紧处理函数里的 `ctx`：

```ts
const status = query("status")
  .capabilities(["server:read"])
  .output(z.object({ state: z.string() }))
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
├── bin/run.sh         # 平台启动器（Windows 为 bin/run.cmd）
└── dist/...           # tsc 编译产物
```

`manifest.entry.cmd` 指向 `bin/run.sh`；宿主拉起时追加
`--endpoint ...` 并注入 `MCSUP_PLUGIN_TOKEN`。

## 测试

```sh
npm run build   # 构建 dist（process 用例依赖它）
npm test        # vitest：协议 / descriptor / builders / hooks / runtime / conformance / 真实进程
```

`test/conformance.test.ts` 读取仓库内 `sdks/conformance/cases.json`，与其它语言
SDK 共用同一组行为用例。

## 版本

SDK 主版本 == plugin API 主版本（`docs/plugins.md` §14.3）。`PLUGIN_API_VERSION`
导出于本包，握手时由 SDK 自动填写。
