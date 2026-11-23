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
| `mcsup-plugin/testing` 的 `createMockHost()` | 内存双工宿主，不开 socket |

Builder（`query` / `action` / `stream` / `op` / `lineFilter`）、Hooks
（`useConsoleTail` / `useRetry` / `useKvCache` / `useEvents`）与预设属次版本
可选 API，用法见 `docs/sdk-design.md` §4。

op 的 `.output()` 可以不写：descriptor 里仍会落一个空 object 占位
（`docs/plugins.md` §11.2 要求该字段必填），但运行期不校验返回值，返回
`undefined` 按 `{}` 回传。写了 `.output()` 就按它校验返回值与流分块。

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

Builder 写法的片段看不到 `definePlugin` 的参数，要用 `.config(config)` 绑**同一个
schema 对象**，`ctx.config` 才有类型；漏绑时它是 `unknown`（取字段会报
`TS18046`），绑错对象会在 `definePlugin` 装配时抛错：

```ts
const config = s.object({ greeting: s.string({ default: "hi" }) });

export default definePlugin({
  config,
  queries: {
    ping: query("ping")
      .config(config)
      .output(s.object({ text: s.string() }))
      .run((_input, ctx) => ({ text: ctx.config.greeting })),
  },
});
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
├── dist/...           # tsc 编译产物
└── node_modules/...   # 运行时依赖，构建时 vendor 进产物
```

**产物自带依赖。** 插件装到宿主目录后，作者的工程目录不在旁边，`node_modules`
解析不到任何东西，进程会以 `ERR_MODULE_NOT_FOUND` 退出（宿主只看到握手超时）。
所以 `build` 会把这个工程的生产依赖整棵拷进 `build/node_modules`：`dependencies`
里声明的包、`mcsup-plugin` 自身、以及能解析到的 `peerDependencies`（如 `zod`），
连同它们的传递依赖；`devDependencies`（`typescript`、`vitest`）不进产物。

**产物不得有符号链接。** 安装与 `plugin verify` 遇到链接直接拒绝
（`symbolic link in artifact: …`）。npm 装出来的 `node_modules/.bin/*` 与
`file:` 依赖都是链接，vendor 时会展开成实体文件，构建后不必手工改动。

依赖包自己的 `node_modules` 不照抄（那是它装过的依赖，已由上面那条链路铺到
产物顶层）。产物仍要满足 `docs/plugins.md` §15.2 的限值——解包 ≤ 512 MiB、
≤ 5000 个文件、路径深度 ≤ 8 层——依赖树越深越容易撞上深限，撞上时安装报
`limit_exceeded: artifact path depth exceeds 8`。

`manifest.entry.cmd` 指向 `bin/run.sh`，`platforms` 给出各平台启动器；
Windows 上宿主经 `cmd.exe /C` 执行 `bin/run.cmd`。宿主拉起时追加
`--endpoint ...` 并注入 `MCSUP_PLUGIN_TOKEN`。

## 发布

```sh
npx mcsup-plugin publish            # 构建 + 推到 origin/dist
```

用户装这条命令打出的链接即可，不需要 npm / 构建步骤：

```sh
mcsup plugin install https://github.com/<owner>/<repo>/tree/dist
```

`publish` 做三件事：跑一次 `build`，用临时索引把产物写成**只含产物**的一次提交
（`read-tree --empty` → `add --all --force` → `write-tree` → `commit-tree`），
再 `push` 到 `<remote>/<branch>`。

- 不切分支、不动工作区与暂存区：索引与工作树由 `GIT_INDEX_FILE` /
  `GIT_WORK_TREE` 指到 `<out>`，`GIT_DIR` 显式解析成绝对路径。正在写代码时
  也能随时发布，`dist` 分支只放产物，源码留在默认分支。
- `--branch`（默认 `dist`）、`--remote`（默认 `origin`）、`--message`
  （默认 `release: <id> <version>`）、`--no-build`（只发已有产物）、
  `--dry-run`（只造提交、不推送）。
- 单个产物受 `docs/plugins.md` §15.2 的 128 MiB 归档上限约束：GitHub 归档就是
  仓库内容本身，产物太大时改用 `--url` / `--from` 分发。
- 仓库里不要提交符号链接：GitHub 归档会原样保留它们，安装时会被
  `symbolic link in artifact` 拒绝。`build` 已经把依赖展开成实体文件，所以把
  `build/` 直接发布出去是安全的。

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