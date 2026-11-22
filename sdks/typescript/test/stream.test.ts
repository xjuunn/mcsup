import { describe, expect, it } from "vitest";

import { type FrameChannel } from "../src/connection.js";
import { z } from "../src/index.js";
import { type OutboundFrame } from "../src/protocol.js";
import { pumpStream } from "../src/stream.js";

function recordingChannel(): { channel: FrameChannel; frames: OutboundFrame[] } {
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
  return { channel, frames };
}

const output = z.object({ text: z.string() });

async function pump(chunks: unknown, options: Parameters<typeof pumpStream>[5] = {}) {
  const { channel, frames } = recordingChannel();
  await pumpStream(chunks, { id: "demo", output }, channel, 11, new AbortController().signal, options);
  return frames;
}

const resultOf = (frames: readonly OutboundFrame[]) =>
  frames.filter((frame) => frame.type === "result").at(-1) as Extract<OutboundFrame, { type: "result" }>;

describe("pumpStream 收尾（§6.5）", () => {
  it("最后一个真实分块带 done:true，不再发空对象收尾", async () => {
    const frames = await pump([{ text: "a" }, { text: "b" }, { text: "c" }]);
    const streams = frames.filter(
      (frame): frame is Extract<OutboundFrame, { type: "stream" }> => frame.type === "stream",
    );
    expect(streams.map((frame) => frame.chunk)).toEqual([
      { text: "a" },
      { text: "b" },
      { text: "c" },
    ]);
    expect(streams.map((frame) => frame.seq)).toEqual([0, 1, 2]);
    expect(streams.map((frame) => frame.done)).toEqual([false, false, true]);
    expect(resultOf(frames)).toMatchObject({
      type: "result",
      id: 11,
      ok: true,
      output: { completed: true, chunks: 3 },
    });
  });

  it("同步与异步迭代器得到相同帧序列", async () => {
    async function* chunks() {
      yield { text: "x" };
      yield { text: "y" };
    }
    const frames = await pump(chunks());
    const streams = frames.filter(
      (frame): frame is Extract<OutboundFrame, { type: "stream" }> => frame.type === "stream",
    );
    expect(streams.map((frame) => frame.chunk)).toEqual([{ text: "x" }, { text: "y" }]);
    expect(streams.at(-1)?.done).toBe(true);
    expect(streams.slice(0, -1).every((frame) => !frame.done)).toBe(true);
    const result = resultOf(frames);
    expect(result.ok && result.output).toEqual({ completed: true, chunks: 2 });
  });

  it("单个分块既是数据帧也是结束帧", async () => {
    const frames = await pump([{ text: "only" }]);
    const streams = frames.filter((frame) => frame.type === "stream");
    expect(streams).toHaveLength(1);
    expect(streams[0]).toMatchObject({ seq: 0, chunk: { text: "only" }, done: true });
  });

  it("空流仍显式发一帧 done 终止符，chunks 为 0", async () => {
    const frames = await pump([]);
    const streams = frames.filter((frame) => frame.type === "stream");
    expect(streams).toHaveLength(1);
    expect(streams[0]).toMatchObject({ seq: 0, chunk: {}, done: true });
    expect(resultOf(frames)).toMatchObject({ ok: true, output: { completed: true, chunks: 0 } });
  });
});

describe("pumpStream 错误路径", () => {
  it("分块不符合 output schema → internal，且不写数据帧", async () => {
    const frames = await pump([{ text: 1 }]);
    expect(frames.filter((frame) => frame.type === "stream")).toHaveLength(0);
    expect(resultOf(frames)).toMatchObject({ ok: false, error: { code: "internal" } });
  });

  it("没有迭代器 → internal", async () => {
    const frames = await pump({ not: "an iterator" });
    expect(resultOf(frames)).toMatchObject({ ok: false, error: { code: "internal" } });
    expect(resultOf(frames).error?.message).toMatch(/did not return an iterator/);
  });

  it("帧超过单帧上限 → limit_exceeded", async () => {
    const frames = await pump([{ text: "x".repeat(200) }], { frameBytes: 32 });
    expect(frames.filter((frame) => frame.type === "stream")).toHaveLength(0);
    expect(resultOf(frames)).toMatchObject({ ok: false, error: { code: "limit_exceeded" } });
  });

  it("结束后调用迭代器的 return 做清理", async () => {
    let cleaned = false;
    function* chunks() {
      try {
        yield { text: "a" };
      } finally {
        cleaned = true;
      }
    }
    const frames = await pump(chunks());
    expect(resultOf(frames).ok).toBe(true);
    expect(cleaned).toBe(true);
  });
});
