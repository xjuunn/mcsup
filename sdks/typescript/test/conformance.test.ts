import * as fs from "node:fs";
import * as path from "node:path";
import { describe, expect, it } from "vitest";

import { createMockHost } from "../src/testing.js";
import type { RejectCode } from "../src/protocol.js";
import { chatBridge } from "./fixtures.js";

interface ConformanceCase {
  readonly id: string;
  readonly kind: "query" | "action" | "stream" | "config" | "handshake" | "shutdown" | "unknown-frame";
  readonly op?: string;
  readonly input?: unknown;
  readonly granted?: readonly string[];
  readonly inject?: Record<string, unknown>;
  readonly expect: Record<string, unknown>;
}

const CASES = path.resolve("..", "conformance", "cases.json");
const cases = (JSON.parse(fs.readFileSync(CASES, "utf8")) as { cases: ConformanceCase[] }).cases;

/** 与本文件 fixtures 对应的会话（`counter` 计数在测试间会累加，用例以相对值断言） */
async function startBridge(granted?: readonly string[]) {
  const host = createMockHost(chatBridge, { config: { token: "t" }, granted });
  await host.start();
  return host;
}

describe("conformance cases", () => {
  for (const testCase of cases) {
    it(testCase.id, async () => {
      if (testCase.kind === "query" || testCase.kind === "action") {
        const host = await startBridge(testCase.granted);
        const result = await host.callOp(testCase.op ?? "", testCase.input ?? {});
        if (testCase.expect["ok"] === true) {
          expect(result.ok).toBe(true);
          if (testCase.id === "query.returns-output" && result.ok) {
            expect(result.output).toMatchObject({ channel: "global" });
          }
        } else {
          expect(result.ok).toBe(false);
          if (!result.ok) {
            expect(result.error.code).toBe((testCase.expect["error"] as { code: string }).code);
          }
        }
        await host.shutdown();
        return;
      }
      if (testCase.kind === "unknown-frame") {
        const host = await startBridge();
        host.inject(testCase.inject as never);
        const result = await host.callOp(testCase.op ?? "", testCase.input ?? {});
        expect(result.ok).toBe(testCase.expect["ok"]);
        await host.shutdown();
        return;
      }
      if (testCase.kind === "stream") {
        const host = await startBridge();
        const result = await host.callOp(testCase.op ?? "", testCase.input ?? {});
        expect(result.ok).toBe(true);
        const chunks = host.frames.filter((f) => f.type === "stream").map((f) => (f as { chunk: unknown }).chunk);
        const doneAt = chunks.findIndex((c) => JSON.stringify(c) === "{}");
        expect(doneAt).toBeGreaterThan(0);
        expect(chunks.slice(0, doneAt)).toEqual(testCase.expect["chunks"]);
        expect(testCase.expect["done"]).toBe(true);
        await host.shutdown();
        return;
      }
      if (testCase.kind === "config") {
        const host = await startBridge();
        const input = testCase.input as { revision: number; values: unknown };
        const result = await host.changeConfig(input.revision, input.values);
        expect(result.ok).toBe(testCase.expect["ok"]);
        if (!result.ok && testCase.expect["error"] !== undefined) {
          expect(result.error.code).toBe((testCase.expect["error"] as { code: string }).code);
        }
        await host.shutdown();
        return;
      }
      if (testCase.kind === "handshake") {
        const input = testCase.input as Record<string, unknown>;
        const host = createMockHost(chatBridge, {
          config: input["config"] ?? { token: "t" },
          reject: input["reject"] as { code: RejectCode; message: string } | undefined,
        });
        await host.start();
        expect(await host.waitExit()).toBe(testCase.expect["exit_code"]);
        return;
      }
      if (testCase.kind === "shutdown") {
        const host = await startBridge();
        expect(await host.shutdown((testCase.input as { grace_ms: number }).grace_ms)).toBe(0);
        return;
      }
      throw new Error(`unknown conformance kind ${String(testCase.kind)}`);
    });
  }
});