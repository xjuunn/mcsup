import { describe, expect, it } from "vitest";

import {
  action,
  definePlugin,
  z,
  type Capability,
  type ContractCallMeta,
} from "../src/index.js";
import type { SchemaLike } from "../src/schema.js";
import { createMockHost, type CallResult, type MockHost } from "../src/testing.js";

const base = {
  title: "contracts",
  version: "1.0.0",
  license: "MIT",
  config: z.object({}),
} as const;

async function callContract<C extends readonly Capability[], Cfg extends SchemaLike>(
  host: MockHost<C, Cfg>,
  id: number,
  input: unknown,
): Promise<CallResult> {
  host.inject({ type: "call", id, op: "contract.call", input });
  return await host.awaitResult(id);
}

describe("契约提供方（§6.4 / §12.4）", () => {
  it("contracts 里的处理函数必须同时出现在 provides", () => {
    expect(() =>
      definePlugin({
        ...base,
        id: "undeclared",
        capabilities: [],
        provides: [],
        contracts: { "echo@1": () => null },
      }),
    ).toThrow(/must be declared in provides/);
  });

  it("按契约名分派并透传 caller / trace", async () => {
    const seen: ContractCallMeta[] = [];
    const plugin = definePlugin({
      ...base,
      id: "provider",
      capabilities: [],
      provides: ["echo@1"],
      contracts: {
        "echo@1": (input, ctx, call) => {
          seen.push(call);
          ctx.log.debug("echo");
          return { echo: input };
        },
      },
    });
    const host = createMockHost(plugin);
    await host.start();

    const result = await callContract(host, 501, {
      contract: "echo@1",
      caller: "caller-a",
      trace: "trace-1",
      input: { text: "hi" },
    });
    expect(result).toEqual({ ok: true, output: { echo: { text: "hi" } } });
    expect(seen).toEqual([{ caller: "caller-a", trace: "trace-1" }]);
    await host.shutdown();
  });

  it("处理函数返回 undefined 时回 null", async () => {
    const plugin = definePlugin({
      ...base,
      id: "empty-provider",
      capabilities: [],
      provides: ["empty@1"],
      contracts: { "empty@1": () => undefined },
    });
    const host = createMockHost(plugin);
    await host.start();
    const result = await callContract(host, 502, {
      contract: "empty@1",
      caller: "caller-a",
      input: {},
    });
    expect(result).toEqual({ ok: true, output: null });
    await host.shutdown();
  });

  it("未提供的契约 → not_found", async () => {
    const plugin = definePlugin({
      ...base,
      id: "provider",
      capabilities: [],
      provides: ["echo@1"],
      contracts: { "echo@1": () => null },
    });
    const host = createMockHost(plugin);
    await host.start();
    const result = await callContract(host, 503, {
      contract: "missing@1",
      caller: "caller-a",
      input: {},
    });
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.code).toBe("not_found");
    }
    await host.shutdown();
  });

  it("处理函数抛错 → internal", async () => {
    const plugin = definePlugin({
      ...base,
      id: "boom-provider",
      capabilities: [],
      provides: ["boom@1"],
      contracts: {
        "boom@1": () => {
          throw new Error("kaboom");
        },
      },
    });
    const host = createMockHost(plugin);
    await host.start();
    const result = await callContract(host, 504, {
      contract: "boom@1",
      caller: "caller-a",
      input: {},
    });
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.code).toBe("internal");
      expect(result.error.message).toMatch(/kaboom/);
    }
    await host.shutdown();
  });

  it("输入形状不合法 → invalid_input", async () => {
    const plugin = definePlugin({
      ...base,
      id: "provider",
      capabilities: [],
      provides: ["echo@1"],
      contracts: { "echo@1": () => null },
    });
    const host = createMockHost(plugin);
    await host.start();

    const malformed: Array<[number, unknown]> = [
      [511, null],
      [512, { contract: "echo@1" }],
      [513, { contract: "echo@1", caller: "" }],
      [514, { contract: "echo@1", caller: "caller-a", trace: 5 }],
      [515, { contract: "", caller: "caller-a" }],
    ];
    for (const [id, input] of malformed) {
      const result = await callContract(host, id, input);
      expect(result.ok).toBe(false);
      if (!result.ok) {
        expect(result.error.code).toBe("invalid_input");
      }
    }
    await host.shutdown();
  });
});

describe("契约调用方", () => {
  it("ctx.contract.call 走 callContract 并透传 target / trace", async () => {
    const calls: Array<Record<string, unknown>> = [];
    const consumer = definePlugin({
      ...base,
      id: "consumer",
      capabilities: [],
      consumes: ["echo@1"],
      actions: {
        ask: action("ask")
          .input(z.object({}))
          .output(z.object({ got: z.unknown() }))
          .run(async (_input, ctx) => {
            const got = await ctx.contract.call(
              "echo@1",
              { text: "hi" },
              { target: "provider", trace: "trace-9" },
            );
            return { got };
          }),
      },
    });
    const host = createMockHost(consumer, {
      handleContract: (call) => {
        calls.push(call as unknown as Record<string, unknown>);
        return { echo: call.input };
      },
    });
    await host.start();
    const result = await host.callOp("ask", {});
    expect(result).toEqual({ ok: true, output: { got: { echo: { text: "hi" } } } });
    expect(calls).toEqual([
      { contract: "echo@1", input: { text: "hi" }, target: "provider", trace: "trace-9" },
    ]);
    await host.shutdown();
  });
});
