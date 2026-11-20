/**
 * §14.1/§14.5 帧一致性用例驱动：`sdks/conformance/frames/*.json`
 *
 * 帧级用例描述握手、调用、流取消与未知帧的宿主行为；由各语言的 MockHost
 * 驱动复现。字节级错误（超长帧 / 深度超限）由传输层测试覆盖，不在此列。
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { describe, expect, it } from "vitest";
import { createMockHost, type InboundFrame } from "../src/testing.js";
import { chatBridge } from "./fixtures.js";

interface FrameCase {
  readonly id: string;
  readonly kind: "handshake" | "call" | "cancel" | "unknown-frame";
  readonly description?: string;
  readonly reject?: { readonly code: string; readonly message: string };
  readonly inject?: Record<string, unknown>;
  readonly op?: string;
  readonly input?: unknown;
  readonly expect: {
    readonly exit_code?: number;
    readonly hello?: Record<string, unknown>;
    readonly ok?: boolean;
    readonly output?: Record<string, unknown>;
    readonly error?: { readonly code: string };
  };
}

const DIR = path.resolve("..", "conformance", "frames");
const files = fs.readdirSync(DIR).filter((f) => f.endsWith(".json")).sort();

describe("frames conformance (§14.1)", () => {
  for (const file of files) {
    const testCase = JSON.parse(fs.readFileSync(path.join(DIR, file), "utf8")) as FrameCase;
    it(testCase.id, async () => {
      const host = createMockHost(chatBridge, {
        config: { token: "t" },
        reject: testCase.reject,
      });
      await host.start();
      if (testCase.inject !== undefined) {
        host.inject(testCase.inject as unknown as InboundFrame);
      }
      if (testCase.kind === "handshake") {
        if (testCase.expect.hello !== undefined) {
          const hello = host.lastFrame("hello");
          expect(hello).toMatchObject(testCase.expect.hello);
        }
        expect(await host.shutdown()).toBe(testCase.expect.exit_code);
        return;
      }
      if (testCase.kind === "cancel") {
        const pending = host.callOp(testCase.op ?? "", testCase.input ?? {});
        const streamId = host.streamFrames(-1)[0]?.id;
        for (let i = 0; i < 400; i += 1) {
          const frames = host.frames.filter((f) => f.type === "stream");
          if (frames.length > 0) {
            break;
          }
          await new Promise((r) => setTimeout(r, 5));
        }
        const first = host.frames.find((f) => f.type === "stream");
        expect(first).toBeDefined();
        host.cancel((first as { id: number }).id, "conformance");
        const result = await pending;
        expect(result.ok).toBe(false);
        if (!result.ok) {
          expect(result.error.code).toBe(testCase.expect.error?.code);
        }
        await host.shutdown();
        return;
      }
      const result = await host.callOp(testCase.op ?? "", testCase.input ?? {});
      expect(result.ok).toBe(testCase.expect.ok);
      if (result.ok && testCase.expect.output !== undefined) {
        expect(result.output).toEqual(testCase.expect.output);
      }
      if (!result.ok && testCase.expect.error !== undefined) {
        expect(result.error.code).toBe(testCase.expect.error.code);
      }
      await host.shutdown();
    });
  }
});
