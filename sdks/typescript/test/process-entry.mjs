// 真实进程入口：使用编译后的 dist，验证 socket / 帧 / 退出码全链路
import { action, definePlugin, query, run, z } from "../dist/index.js";

const plugin = definePlugin({
  id: "fixture",
  title: "Fixture",
  version: "0.0.1",
  license: "MIT",
  capabilities: ["server:read"],
  config: z.object({}),
  queries: {
    status: query("status").output(z.object({ ok: z.literal(true) })).run(() => ({ ok: true })),
  },
  actions: {
    noop: action("noop").run(() => ({ ok: true })),
  },
});

process.exit(await run(plugin));
