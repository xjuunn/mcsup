import * as fs from "node:fs";
import * as path from "node:path";
import { describe, expect, it } from "vitest";

import { createMockHost } from "../src/testing.js";
import type { RejectCode } from "../src/protocol.js";
import { chatBridge } from "./fixtures.js";

/** docs/plugins-testing.md §5.1：cases.json 是唯一登记处 */
interface RegisteredCase {
  readonly id: string;
  readonly kind: string;
  readonly input: string;
  readonly expect: string;
}

/** behavior 输入文件：scenario 决定 runner 走哪条驱动路径 */
interface BehaviorInput {
  readonly scenario: "op" | "unknown-frame" | "stream" | "config" | "handshake" | "shutdown" | "contract";
  readonly op?: string;
  readonly op_kind?: string;
  readonly granted?: readonly string[];
  readonly inject?: Record<string, unknown>;
  readonly input?: unknown;
}

const CONFORMANCE = path.resolve("..", "conformance");
const cases = (JSON.parse(fs.readFileSync(path.join(CONFORMANCE, "cases.json"), "utf8")) as {
  cases: RegisteredCase[];
}).cases;

function loadCase(testCase: RegisteredCase): { input: BehaviorInput; expect: Record<string, unknown> } {
  if (testCase.kind !== "behavior") {
    throw new Error(`unsupported conformance kind ${testCase.kind} for ${testCase.id}`);
  }
  return {
    input: JSON.parse(fs.readFileSync(path.join(CONFORMANCE, testCase.input), "utf8")) as BehaviorInput,
    expect: JSON.parse(fs.readFileSync(path.join(CONFORMANCE, testCase.expect), "utf8")) as Record<string, unknown>,
  };
}

/** 与本文件 fixtures 对应的会话（`counter` 计数在测试间会累加，用例以相对值断言） */
async function startBridge(granted?: readonly string[]) {
  const host = createMockHost(chatBridge, {
    config: { token: "t" },
    granted,
    // 宿主会确认写入类能力；reentrant 用例会 set 一次 KV
    handle: ({ capability, method }) => {
      if (capability === "storage:kv" && method === "set") {
        return { ok: true };
      }
      return null;
    },
  });
  await host.start();
  return host;
}

describe("conformance cases", () => {
  for (const testCase of cases) {
    it(testCase.id, async () => {
      const { input, expect: expected } = loadCase(testCase);
      if (input.scenario === "op") {
        const host = await startBridge(input.granted);
        const result = await host.callOp(input.op ?? "", input.input ?? {});
        if (expected["ok"] === true) {
          expect(result.ok).toBe(true);
          if (result.ok && input.op === "counter") {
            expect(result.output).toMatchObject({ channel: "global" });
          }
        } else {
          expect(result.ok).toBe(false);
          if (!result.ok) {
            expect(result.error.code).toBe((expected["error"] as { code: string }).code);
          }
        }
        await host.shutdown();
        return;
      }
      if (input.scenario === "unknown-frame") {
        const host = await startBridge();
        host.inject(input.inject as never);
        const result = await host.callOp(input.op ?? "", input.input ?? {});
        expect(result.ok).toBe(expected["ok"]);
        await host.shutdown();
        return;
      }
      if (input.scenario === "stream") {
        const host = await startBridge();
        const result = await host.callOp(input.op ?? "", input.input ?? {});
        expect(result.ok).toBe(true);
        const streams = host.frames.filter(
          (f): f is Extract<(typeof host.frames)[number], { type: "stream" }> => f.type === "stream",
        );
        // 最后一个真实分块带 done:true；不再额外发 chunk:{} 收尾（§6.5）
        expect(streams.map((f) => f.chunk)).toEqual(expected["chunks"]);
        expect(streams.map((f) => f.seq)).toEqual(streams.map((_f, index) => index));
        expect(streams.at(-1)?.done).toBe(true);
        expect(streams.slice(0, -1).every((f) => !f.done)).toBe(true);
        expect(expected["done"]).toBe(true);
        await host.shutdown();
        return;
      }
      if (input.scenario === "config") {
        const host = await startBridge();
        const payload = input.input as { revision: number; values: unknown };
        const result = await host.changeConfig(payload.revision, payload.values);
        expect(result.ok).toBe(expected["ok"]);
        if (!result.ok && expected["error"] !== undefined) {
          expect(result.error.code).toBe((expected["error"] as { code: string }).code);
        }
        await host.shutdown();
        return;
      }
      if (input.scenario === "handshake") {
        const payload = (input.input ?? {}) as Record<string, unknown>;
        const host = createMockHost(chatBridge, {
          config: payload["config"] ?? { token: "t" },
          reject: payload["reject"] as { code: RejectCode; message: string } | undefined,
        });
        await host.start();
        expect(await host.waitExit()).toBe(expected["exit_code"]);
        return;
      }
      if (input.scenario === "shutdown") {
        const host = await startBridge();
        expect(await host.shutdown((input.input as { grace_ms: number }).grace_ms)).toBe(0);
        return;
      }
      throw new Error(`unsupported behavior scenario ${String(input.scenario)} for ${testCase.id}`);
    });
  }
});
