import { describe, expect, it, vi } from "vitest";

import { definePlugin, lineFilter, z } from "../src/index.js";
import { useConsoleTail, useEvents, useKvCache, useRetry } from "../src/hooks.js";
import { PluginError } from "../src/errors.js";
import { createMockHost } from "../src/testing.js";

const reader = definePlugin({
  id: "reader",
  title: "reader",
  version: "1.0.0",
  license: "MIT",
  capabilities: ["console:read", "storage:kv", "events"],
  config: z.object({}),
});

async function makeCtx() {
  const store = new Map<string, Uint8Array>();
  interface Page {
    readonly next: number;
    readonly lines: ReadonlyArray<{ readonly cursor: number; readonly text: string }>;
  }
  const consolePages: Page[] = [
    {
      next: 2,
      lines: [
        { cursor: 1, text: "a" },
        { cursor: 2, text: "b" },
      ],
    },
  ];
  const host = createMockHost(reader, {
    handle: ({ capability, method, input }) => {
      if (capability === "console:read") {
        const page = consolePages.shift() ?? { next: 2, lines: [] };
        return { lines: page.lines, next: page.next };
      }
      if (capability === "storage:kv") {
        const key = (input as { key: string }).key;
        if (method === "get") {
          const value = store.get(key);
          return { value: value === undefined ? null : Buffer.from(value).toString("base64") };
        }
        if (method === "set") {
          store.set(key, Buffer.from((input as { value: string }).value, "base64"));
          return { ok: true };
        }
        if (method === "delete") {
          return { deleted: store.delete(key) };
        }
      }
      throw new PluginError("denied", `${capability}.${method}`);
    },
  });
  await host.start();
  return { host, ctx: host.ctx, store };
}

describe("useConsoleTail", () => {
  it("轮询游标并投递行", async () => {
    const { host, ctx } = await makeCtx();
    const seen: string[] = [];
    const tail = useConsoleTail(ctx, { intervalMs: 5 });
    tail.subscribe("survival", (line) => {
      seen.push(`${line.server}:${line.text}`);
    });
    for (let i = 0; i < 100 && seen.length < 2; i += 1) {
      await new Promise((r) => setTimeout(r, 5));
    }
    expect(seen).toEqual(["survival:a", "survival:b"]);
    expect(tail.cursor("survival")).toBe(2);
    tail.stop();
    await host.shutdown();
  });

  it("未声明 console:read 立即抛错", async () => {
    const { host, ctx } = await makeCtx();
    const stripped = { ...ctx } as typeof ctx;
    delete (stripped as Record<string, unknown>)["console:read"];
    expect(() => useConsoleTail(stripped, {})).toThrow(/console:read/);
    await host.shutdown();
  });

  it("handler 抛错时不推进游标，重试重放整页而不是跳过剩余行", async () => {
    interface Page {
      readonly next: number;
      readonly lines: ReadonlyArray<{ readonly cursor: number; readonly text: string }>;
    }
    const pages = new Map<number | null, Page>([
      [
        null,
        {
          next: 2,
          lines: [
            { cursor: 1, text: "a" },
            { cursor: 2, text: "b" },
          ],
        },
      ],
      [
        0,
        {
          next: 2,
          lines: [
            { cursor: 1, text: "a" },
            { cursor: 2, text: "b" },
          ],
        },
      ],
      [
        2,
        {
          next: 4,
          lines: [
            { cursor: 3, text: "c" },
            { cursor: 4, text: "d" },
          ],
        },
      ],
    ]);
    const host = createMockHost(reader, {
      handle: ({ capability, input }) => {
        if (capability !== "console:read") {
          throw new PluginError("denied", capability);
        }
        const cursor = (input as { cursor: number | null }).cursor;
        const page = pages.get(cursor) ?? { next: 4, lines: [] };
        return { lines: page.lines, next: page.next };
      },
    });
    await host.start();
    let failures = 1;
    const seen: string[] = [];
    const tail = useConsoleTail(host.ctx, { intervalMs: 5 });
    tail.subscribe("survival", (line) => {
      if (line.text === "b" && failures > 0) {
        failures -= 1;
        throw new Error("boom");
      }
      seen.push(line.text);
    });
    for (let i = 0; i < 200 && !seen.includes("d"); i += 1) {
      await new Promise((r) => setTimeout(r, 5));
    }
    tail.stop();
    // b 失败后游标未推进，第二页重放时 a/b 重新投递（至少一次），c/d 不丢
    expect(seen).toEqual(["a", "a", "b", "c", "d"]);
    expect(tail.cursor("survival")).toBe(null);
    await host.shutdown();
  });
});

describe("useRetry", () => {
  it("只重试可重试错误", async () => {
    const { host, ctx } = await makeCtx();
    const retry = useRetry(ctx, { attempts: 3, baseMs: 1 });
    const fn = vi.fn().mockRejectedValueOnce(new PluginError("unavailable", "x")).mockResolvedValue("ok");
    await expect(retry(fn)).resolves.toBe("ok");
    expect(fn).toHaveBeenCalledTimes(2);

    const fatal = vi.fn().mockRejectedValue(new PluginError("denied", "no"));
    await expect(retry(fatal)).rejects.toMatchObject({ code: "denied" });
    expect(fatal).toHaveBeenCalledTimes(1);
    await host.shutdown();
  });

  it("用完尝试次数后抛出最后的错误", async () => {
    const { host, ctx } = await makeCtx();
    const retry = useRetry(ctx, { attempts: 2, baseMs: 1 });
    const fn = vi.fn().mockRejectedValue(new PluginError("timeout", "t"));
    await expect(retry(fn)).rejects.toMatchObject({ code: "timeout" });
    expect(fn).toHaveBeenCalledTimes(2);
    await host.shutdown();
  });
});

describe("useKvCache", () => {
  it("remember 命中缓存后不再调用 loader", async () => {
    const { host, ctx } = await makeCtx();
    const cache = useKvCache(ctx, { prefix: "t:" });
    const loader = vi.fn().mockResolvedValue({ v: 1 });
    await expect(cache.remember("k", loader)).resolves.toEqual({ v: 1 });
    await expect(cache.remember("k", loader)).resolves.toEqual({ v: 1 });
    expect(loader).toHaveBeenCalledTimes(1);
    await cache.delete("k");
    await expect(cache.get("k")).resolves.toBeNull();
    await host.shutdown();
  });

  it("过期条目按未命中处理，坏 JSON 不抛错", async () => {
    const { host, ctx, store } = await makeCtx();
    const cache = useKvCache(ctx, { ttlMs: 10 });
    await cache.set("k", { v: 1 });
    await expect(cache.get("k")).resolves.toEqual({ v: 1 });
    await new Promise((r) => setTimeout(r, 30));
    await expect(cache.get("k")).resolves.toBeNull();

    store.set("cache:bad", new TextEncoder().encode("not json"));
    await expect(cache.get("bad")).resolves.toBeNull();
    await host.shutdown();
  });
});

describe("useEvents", () => {
  it("订阅后由宿主事件触发", async () => {
    const { host, ctx } = await makeCtx();
    const seen: unknown[] = [];
    useEvents(ctx, "chat:message", (data) => {
      seen.push(data);
    });
    await host.emit("chat:message", { text: "hi" }, "survival");
    expect(seen).toEqual([{ text: "hi" }]);
    await host.shutdown();
  });

  it("未声明 events 立即抛错", async () => {
    const { host, ctx } = await makeCtx();
    const stripped = { ...ctx } as Record<string, unknown>;
    delete stripped["events"];
    expect(() => useEvents(stripped as typeof ctx, "x", () => {})).toThrow(/events/);
    await host.shutdown();
  });
});

describe("lineFilter", () => {
  it("stripAnsi / include / exclude / flatMap", () => {
    const filter = lineFilter()
      .stripAnsi()
      .exclude(/DEBUG/)
      .include(/hello/)
      .flatMap((line) => [{ ...line, text: line.text.toUpperCase() }]);
    const out = filter.build();
    expect(out({ text: "\u001b[31mhello\u001b[0m", server: "s", at: 0 }).map((l) => l.text)).toEqual(["HELLO"]);
    expect(out({ text: "DEBUG hello", server: "s", at: 0 })).toEqual([]);
    expect(out({ text: "nope", server: "s", at: 0 })).toEqual([]);
  });
});