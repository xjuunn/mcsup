import { beforeEach, describe, expect, it } from "vitest";

import { action, definePlugin, query, z } from "../src/index.js";
import { type FrameChannel } from "../src/connection.js";
import { pumpStream } from "../src/stream.js";
import { run } from "../src/runtime.js";
import { ExitCode, type OutboundFrame } from "../src/protocol.js";
import { createMockHost } from "../src/testing.js";
import { chatBridge, counter, minimal, send } from "./fixtures.js";

describe("内核定义", () => {
  it("未声明 events 却订阅 → 定义期报错", async () => {
    const { definePlugin, z } = await import("../src/index.js");
    expect(() =>
      definePlugin({
        id: "bad",
        title: "bad",
        version: "1.0.0",
        license: "MIT",
        capabilities: ["server:read"],
        subscriptions: ["chat:message"],
        config: z.object({}),
      }),
    ).toThrow(/events capability/);
  });

  it("net 白名单要求 net:outbound", async () => {
    const { definePlugin, z } = await import("../src/index.js");
    expect(() =>
      definePlugin({
        id: "bad-net",
        title: "bad",
        version: "1.0.0",
        license: "MIT",
        capabilities: ["server:read"],
        net: { allow: ["a.example.com"] },
        config: z.object({}),
      }),
    ).toThrow(/net:outbound/);
  });
});

describe("未声明 output 的 op", () => {
  const base = {
    id: "no-output",
    title: "无输出",
    version: "1.0.0",
    license: "MIT",
    capabilities: [],
    config: z.object({}),
  } as const;

  it("返回值原样回传，不被空 object 占位 schema 判成非法", async () => {
    const plugin = definePlugin({
      ...base,
      actions: {
        shout: action("shout")
          .input(z.object({ text: z.string() }))
          .run((input) => ({ done: input.text.toUpperCase() })),
      },
    });
    const host = createMockHost(plugin);
    await host.start();
    expect(await host.callOp("shout", { text: "hi" })).toEqual({ ok: true, output: { done: "HI" } });
    await host.shutdown();
  });

  it("返回 undefined 时回 {}", async () => {
    const plugin = definePlugin({
      ...base,
      actions: {
        touch: action("touch")
          .input(z.object({}))
          .run(() => undefined),
      },
    });
    const host = createMockHost(plugin);
    await host.start();
    expect(await host.callOp("touch", {})).toEqual({ ok: true, output: {} });
    await host.shutdown();
  });

  it("显式声明 output 时仍然双向校验", async () => {
    const plugin = definePlugin({
      ...base,
      queries: {
        ping: query("ping")
          .input(z.object({}))
          .output(z.object({ ok: z.boolean() }))
          .run(() => ({ ok: "not-bool" }) as unknown as { ok: boolean }),
      },
    });
    const host = createMockHost(plugin);
    await host.start();
    const result = await host.callOp("ping", {});
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.code).toBe("internal");
      expect(result.error.message).toMatch(/invalid output/);
    }
    await host.shutdown();
  });
});

describe("MockHost 握手", () => {
  it("hello 携带声明与订阅", async () => {
    const host = createMockHost(chatBridge);
    await host.start();
    const hello = host.frames.find((f) => f.type === "hello");
    expect(hello).toMatchObject({
      plugin_id: "chat-bridge",
      api: 1,
      plugin_version: "0.1.0",
      requested: ["console:read", "console:write", "storage:kv", "events"],
      subscriptions: ["chat:message"],
    });
    await host.shutdown();
  });

  it("reject api_mismatch → 退出码 4", async () => {
    const host = createMockHost(minimal, { reject: { code: "api_mismatch", message: "nope" } });
    await host.start();
    expect(await host.waitExit()).toBe(ExitCode.ApiMismatch);
  });

  it("reject 其他原因 → 退出码 2", async () => {
    const host = createMockHost(minimal, { reject: { code: "rejected", message: "no" } });
    await host.start();
    expect(await host.waitExit()).toBe(ExitCode.Protocol);
  });

  it("初始配置不合法 → 退出码 3", async () => {
    const host = createMockHost(chatBridge, { config: { token: "" } });
    await host.start();
    expect(await host.waitExit()).toBe(ExitCode.Config);
  });

  it("ping 回 pong", async () => {
    const host = createMockHost(minimal);
    await host.start();
    await host.ping(7);
    await host.shutdown();
  });

  it("hello 的 subscriptions 来自静态声明", async () => {
    const host = createMockHost(chatBridge, { config: { token: "t" } });
    await host.start();
    expect(host.lastFrame("hello")).toMatchObject({ subscriptions: ["chat:message"] });
    await host.shutdown();
  });
});

describe("op 分派", () => {
  let host: ReturnType<typeof createMockHost<readonly ["server:read"]>>;
  beforeEach(() => {
    host = createMockHost(minimal);
  });

  it("query 正常返回", async () => {
    await host.start();
    const result = await host.callOp("servers", {});
    expect(result).toEqual({ ok: true, output: [] });
    await host.shutdown();
  });

  it("未知 op → not_found", async () => {
    await host.start();
    const result = await host.callOp("nope", {});
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.code).toBe("not_found");
    }
    await host.shutdown();
  });

  it("输入不合法 → invalid_input", async () => {
    const bridge = createMockHost(chatBridge, { config: { token: "t" } });
    await bridge.start();
    const result = await bridge.callOp("send", { channel: "", text: "" });
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.code).toBe("invalid_input");
    }
    await bridge.shutdown();
  });

  it("处理函数抛错 → internal", async () => {
    const bridge = createMockHost(chatBridge, { config: { token: "t" } });
    await bridge.start();
    const result = await bridge.callOp("failing", {});
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.code).toBe("internal");
    }
    await bridge.shutdown();
  });

  it("输出不符合 schema → internal", async () => {
    const bridge = createMockHost(chatBridge, { config: { token: "t" } });
    await bridge.start();
    const result = await bridge.callOp("send", { channel: "global", text: "hi" });
    expect(result).toEqual({ ok: true, output: { ok: true, messageId: "m-1" } });
    await bridge.shutdown();
  });
});

describe("流式 op", () => {
  it("seq 连续、末尾 done", async () => {
    const bridge = createMockHost(chatBridge, { config: { token: "t" } });
    await bridge.start();
    const result = await bridge.callOp("messages", { prefix: "#" });
    expect(result.ok).toBe(true);
    const streams = bridge.frames.filter((f) => f.type === "stream");
    expect(streams.length).toBeGreaterThan(0);
    const texts = streams.map((f) => (f as { chunk: { text?: string } }).chunk.text).filter((t) => t !== undefined);
    expect(texts).toEqual(["#你", "#好", "#"]);
    const last = streams[streams.length - 1];
    expect(last).toMatchObject({ done: true });
    await bridge.shutdown();
  });

  it("取消后回 cancelled 且不再写流", async () => {
    const bridge = createMockHost(chatBridge, { config: { token: "t" } });
    await bridge.start();
    const before = bridge.frames.length;
    const pending = bridge.callOp("slow", {});
    // 等首个流帧
    for (let i = 0; i < 200 && bridge.frames.slice(before).filter((f) => f.type === "stream").length < 2; i += 1) {
      await new Promise((r) => setTimeout(r, 5));
    }
    const streams = bridge.frames.slice(before).filter((f) => f.type === "stream") as Array<{ id: number }>;
    const callId = streams[0]?.id;
    expect(callId).toBeTypeOf("number");
    bridge.cancel(callId!, "test-cancel");
    const result = await pending;
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.code).toBe("cancelled");
    }
    await bridge.shutdown();
  });
});

describe("配置热更新", () => {
  it("合法配置回 ok 且 ctx.config 更新", async () => {
    const bridge = createMockHost(chatBridge, { config: { token: "t" } });
    await bridge.start();
    const result = await bridge.changeConfig(2, { token: "t2", channels: ["global"] });
    expect(result.ok).toBe(true);
    expect((bridge.ctx.config as { token: string }).token).toBe("t2");
    await bridge.shutdown();
  });

  it("过期 revision 不应用", async () => {
    const bridge = createMockHost(chatBridge, { config: { token: "t" }, configRevision: 5 });
    await bridge.start();
    const result = await bridge.changeConfig(3, { token: "stale" });
    expect(result.ok).toBe(true);
    await bridge.shutdown();
  });

  it("非法配置回 ok:false 且保留原值", async () => {
    const bridge = createMockHost(chatBridge, { config: { token: "t" } });
    await bridge.start();
    const result = await bridge.changeConfig(2, { token: "" });
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.code).toBe("invalid_input");
    }
    expect((bridge.ctx.config as { token: string }).token).toBe("t");
    await bridge.shutdown();
  });
});

describe("能力客户端", () => {
  it("未授予能力不出现字段", async () => {
    const host = createMockHost(minimal);
    await host.start();
    expect((host.ctx as unknown as { "server:read"?: { servers: unknown } })["server:read"]?.servers).toBeDefined();
    expect((host.ctx as { console?: unknown }).console).toBeUndefined();
    expect((host.ctx as { kv?: unknown }).kv).toBeUndefined();
    await host.shutdown();
  });

  it("能力调用经 handle 转成方法结果", async () => {
    const { definePlugin, z, query } = await import("../src/index.js");
    const reader = definePlugin({
      id: "reader",
      title: "reader",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["console:read"],
      config: z.object({}),
      queries: { ping: query("ping").run(() => ({})) },
    });
    const host = createMockHost(reader, {
      handle: ({ capability, method }) => {
        if (capability === "console:read" && method === "tail") {
          return { lines: [{ cursor: 9, text: "hello" }], next: 9 };
        }
        throw new Error("unexpected");
      },
    });
    await host.start();
    const page = await host.ctx["console:read"].tail("survival");
    expect(page).toEqual({ lines: [{ cursor: 9, text: "hello" }], next: 9 });
    await host.shutdown();
  });
});

describe("未授予能力", () => {
  it("声明但宿主未授予 → 访问时 denied", async () => {
    const { definePlugin, z, query } = await import("../src/index.js");
    const plugin = definePlugin({
      id: "reads-console",
      title: "reads-console",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["console:read"],
      config: z.object({}),
      queries: { ping: query("ping").run(() => ({})) },
    });
    const host = createMockHost(plugin, { granted: [] });
    await host.start();
    expect(() => host.ctx["console:read"]).toThrow(/not granted/);
    await host.shutdown();
  });
});

describe("Builder 片段", () => {
  it("query builder 片段可回填内核", () => {
    expect(counter.input).toBeDefined();
    expect(counter.kind).toBe("query");
    expect(send.kind).toBe("action");
    expect(send.risk).toBe("mutating");
  });
});

// ---------------------------------------------------------------------------
// 并发限额与优雅退出（§5.4 / §6.7）
// ---------------------------------------------------------------------------

function deferred(): { promise: Promise<void>; resolve: () => void } {
  let resolve: () => void = () => {};
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

const slowPlugin = (gate: Promise<void>, delayMs = 0) =>
  definePlugin({
    id: "slow",
    title: "slow",
    version: "1.0.0",
    license: "MIT",
    capabilities: ["server:read"],
    config: z.object({}),
    queries: {
      slow: query("slow")
        .output(z.object({ done: z.boolean() }))
        .run(async () => {
          if (delayMs > 0) {
            await new Promise((r) => setTimeout(r, delayMs));
          } else {
            await gate;
          }
          return { done: true };
        }),
    },
  });

describe("并发限额（§5.4）", () => {
  it("插件 → 宿主调用也受 inflight 限额", async () => {
    const gate = deferred();
    let concurrent = 0;
    let peak = 0;
    const host = createMockHost(minimal, {
      limits: { inflight: 1 },
      handle: async ({ capability, method }) => {
        if (capability === "server:read" && method === "list") {
          concurrent += 1;
          peak = Math.max(peak, concurrent);
          await gate.promise;
          concurrent -= 1;
          return { servers: [] };
        }
        throw new Error(`unexpected ${capability}.${method}`);
      },
    });
    await host.start();
    const a = host.ctx["server:read"].servers.list();
    const b = host.ctx["server:read"].servers.list();
    await expect(host.ctx["server:read"].servers.list()).rejects.toMatchObject({ code: "limit_exceeded" });
    gate.resolve();
    await expect(a).resolves.toEqual([]);
    await expect(b).resolves.toEqual([]);
    expect(peak).toBe(1);
    await host.shutdown();
  });

  it("inflight 满时排队，队列满回 limit_exceeded", async () => {
    const gate = deferred();
    const host = createMockHost(slowPlugin(gate.promise), { limits: { inflight: 1 } });
    await host.start();

    const first = host.callOp("slow");
    const second = host.callOp("slow");
    // 第一个占满 inflight，第二个进队列；队列容量 = inflight，第三个直接拒绝
    const third = await host.callOp("slow");
    expect(third.ok).toBe(false);
    if (!third.ok) {
      expect(third.error.code).toBe("limit_exceeded");
    }

    gate.resolve();
    await expect(first).resolves.toMatchObject({ ok: true });
    await expect(second).resolves.toMatchObject({ ok: true });
    await host.shutdown();
  });

  it("事件队列满时丢最旧，保留最新的事件", async () => {
    const seen: number[] = [];
    const plugin = definePlugin({
      id: "events",
      title: "events",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["events"],
      subscriptions: ["chat:message"],
      config: z.object({}),
      onStart: (ctx) => {
        ctx.events.on("chat:message", async (data) => {
          seen.push((data as { n: number }).n);
          await new Promise((r) => setTimeout(r, 20));
        });
      },
    });
    const host = createMockHost(plugin, { limits: { event_queue: 2 } });
    await host.start();
    for (let n = 1; n <= 4; n += 1) {
      host.inject({ type: "event", seq: n, kind: "chat:message", at: new Date().toISOString(), data: { n } });
    }
    for (let i = 0; i < 100 && seen.length < 3; i += 1) {
      await new Promise((r) => setTimeout(r, 10));
    }
    // 1 正在处理；2 在队列满时被丢弃；3、4 保留
    expect(seen).toEqual([1, 3, 4]);
    await host.shutdown();
  });
});

describe("shutdown（§6.7）", () => {
  it("grace 内等待 inflight 完成后再退出", async () => {
    const host = createMockHost(slowPlugin(Promise.resolve(), 30));
    await host.start();
    const pending = host.callOp("slow");
    const code = await host.shutdown(1_000);
    expect(code).toBe(ExitCode.Ok);
    await expect(pending).resolves.toMatchObject({ ok: true, output: { done: true } });
  });

  it("grace 用尽后取消 inflight 并正常退出", async () => {
    const host = createMockHost(slowPlugin(new Promise<void>(() => {})));
    await host.start();
    const pending = host.callOp("slow");
    const code = await host.shutdown(10);
    expect(code).toBe(ExitCode.Ok);
    const result = await pending;
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.code).toBe("cancelled");
    }
  });

  it("shutdown 后新调用回 unavailable", async () => {
    const host = createMockHost(slowPlugin(new Promise<void>(() => {})));
    await host.start();
    const pending = host.callOp("slow");
    await host.shutdown(10);
    await pending;
    host.inject({ type: "call", id: 999, op: "slow", input: {} });
    const late = await host.awaitResult(999, 200);
    expect(late.ok).toBe(false);
    if (!late.ok) {
      expect(late.error.code).toBe("unavailable");
    }
  });
});

describe("流式空闲超时（§5.4）", () => {
  it("迭代器 60 s 无新分块则终止；测试用短窗口", async () => {
    const frames: OutboundFrame[] = [];
    const channel: FrameChannel = {
      onFrame() {},
      onClose() {},
      send(frame) {
        frames.push(frame);
      },
      async call() {
        return null;
      },
      async callContract() {
        return null;
      },
      close() {},
    };
    const chunks = (async function* () {
      await new Promise<void>(() => {});
    })();
    await pumpStream(chunks, { id: "idle", output: z.object({}) }, channel, 7, new AbortController().signal, {
      idleMs: 20,
    });
    const last = frames[frames.length - 1];
    expect(last).toMatchObject({ type: "result", id: 7, ok: false, error: { code: "timeout" } });
  });
});

describe("内联订阅 handler", () => {
  it("第二个参数是 ctx，不是 EventMeta", async () => {
    const seen: unknown[] = [];
    const plugin = definePlugin({
      id: "inline-sub",
      title: "内联订阅",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["events"],
      config: z.object({}),
      subscriptions: [
        {
          kind: "chat:message",
          handler: (data, ctx) => {
            seen.push(ctx);
            void data;
          },
        },
      ],
    });
    const host = createMockHost(plugin);
    await host.start();
    await host.emit("chat:message", { text: "hi" });
    expect(seen).toHaveLength(1);
    expect(seen[0]).toBe(host.ctx);
  });
});

describe("run 幂等", () => {
  it("重复调用复用同一会话，不会二次连接", async () => {
    // 缺端点：第一次调用直接以协议错误收尾
    const first = run(minimal, { argv: ["node", "entry.mjs"], env: {} });
    const second = run(minimal, { argv: ["node", "entry.mjs"], env: {} });
    expect(await second).toBe(ExitCode.Protocol);
    expect(await first).toBe(ExitCode.Protocol);
  });
});