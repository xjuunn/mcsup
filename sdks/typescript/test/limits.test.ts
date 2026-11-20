import { describe, expect, it } from "vitest";

import { action, definePlugin, query, stream, z } from "../src/index.js";
import { createMockHost } from "../src/testing.js";

function pluginWith(spec: Parameters<typeof definePlugin>[0]) {
  return definePlugin(spec);
}

describe("限额（§9.2 / §5.4）", () => {
  it("非流式输出超过 1 MiB → limit_exceeded", async () => {
    const plugin = pluginWith({
      id: "big",
      title: "big",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["server:read"],
      config: z.object({}),
      queries: {
        huge: query("huge")
          .output(z.object({ text: z.string() }))
          .run(() => ({ text: "x".repeat(2 * 1024 * 1024) })),
      },
    });
    const host = createMockHost(plugin);
    await host.start();
    const result = await host.callOp("huge", {});
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.code).toBe("limit_exceeded");
    }
    await host.shutdown();
  });

  it("流式累计超过 16 MiB → 终止并回 limit_exceeded", async () => {
    const plugin = pluginWith({
      id: "flood",
      title: "flood",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["server:read"],
      config: z.object({}),
      streams: {
        flood: stream("flood")
          .output(z.object({ blob: z.string() }))
          .run(function* () {
            for (let i = 0; i < 40; i += 1) {
              yield { blob: "x".repeat(512 * 1024) };
            }
          }),
      },
    });
    const host = createMockHost(plugin);
    await host.start();
    const result = await host.callOp("flood", {});
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.code).toBe("limit_exceeded");
    }
    await host.shutdown();
  });

  it("op 超时 → timeout 且标记可重试", async () => {
    const plugin = pluginWith({
      id: "slowish",
      title: "slowish",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["server:read"],
      config: z.object({}),
      actions: {
        hang: action("hang")
          .timeout(20)
          .output(z.object({ ok: z.literal(true) }))
          .run(async () => {
            await new Promise((resolve) => setTimeout(resolve, 500));
            return { ok: true as const };
          }),
      },
    });
    const host = createMockHost(plugin);
    await host.start();
    const result = await host.callOp("hang", {});
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.code).toBe("timeout");
    }
    await host.shutdown();
  });
});