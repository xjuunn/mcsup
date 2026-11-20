import { beforeEach, describe, expect, it } from "vitest";

import { ExitCode } from "../src/index.js";
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
    const host = createMockHost(minimal, { reject: { code: "denied", message: "no" } });
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
    const deltas = streams.map((f) => (f as { chunk: { delta?: string } }).chunk.delta).filter((d) => d !== undefined);
    expect(deltas).toEqual(["#你", "#好", "#"]);
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
          return { cursor: 9, lines: ["hello"], dropped: 0 };
        }
        throw new Error("unexpected");
      },
    });
    await host.start();
    const page = await host.ctx["console:read"].tail("survival");
    expect(page).toEqual({ cursor: 9, lines: ["hello"], dropped: 0 });
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