import { describe, expect, it } from "vitest";

import { MAX_FRAME_BYTES, MAX_JSON_DEPTH, PROTOCOL_VERSION, ProtocolError, encodeFrame, parseFrame, wireError, wireFrameBytes } from "../src/protocol.js";

describe("协议信封", () => {
  it("编码带版本与换行", () => {
    const line = encodeFrame({ type: "pong", n: 3 });
    expect(line.endsWith("\n")).toBe(true);
    expect(JSON.parse(line) as unknown).toEqual({ v: PROTOCOL_VERSION, type: "pong", n: 3 });
  });

  it("版本不匹配是协议错误", () => {
    expect(() => parseFrame('{"v":2,"type":"ping","n":1}')).toThrow(/protocol version/i);
  });

  it("非法 JSON / 非对象 / 缺 type 都报错", () => {
    expect(() => parseFrame("{")).toThrow(ProtocolError);
    expect(() => parseFrame("[]")).toThrow(/JSON object/);
    expect(() => parseFrame('{"v":1}')).toThrow(/missing type/);
  });

  it("超过 MAX_FRAME_BYTES 拒绝", () => {
    const huge = `{"v":1,"type":"ping","n":1,"pad":"${"x".repeat(MAX_FRAME_BYTES)}"}`;
    expect(() => parseFrame(huge)).toThrow(/size limit/);
  });

  it("上限按字节而不是 UTF-16 码元计算", () => {
    // 每个汉字 3 字节：码元数不到上限，字节数已超过
    const filler = "中".repeat(Math.ceil(MAX_FRAME_BYTES / 3) + 1);
    const line = `{"v":1,"type":"ping","n":1,"pad":"${filler}"}`;
    expect(line.length).toBeLessThan(MAX_FRAME_BYTES);
    expect(() => parseFrame(line)).toThrow(/size limit/);
  });

  it("wireFrameBytes 与真实编码字节数一致", () => {
    const frame = { type: "pong" as const, n: 1 };
    expect(wireFrameBytes(frame)).toBe(Buffer.byteLength(encodeFrame(frame), "utf8"));
  });

  it("嵌套超过 32 层拒绝", () => {
    let nested = "1";
    for (let i = 0; i < MAX_JSON_DEPTH + 2; i += 1) {
      nested = `{"a":${nested}}`;
    }
    expect(() => parseFrame(`{"v":1,"type":"event","seq":1,"kind":"k","at":"t","data":${nested}}`)).toThrow(/depth/);
  });

  it("未知 type 忽略但保持连接", () => {
    expect(parseFrame('{"v":1,"type":"future-frame","x":1}')).toEqual({ type: "unknown", kind: "future-frame" });
  });

  it("已知类型缺字段报错", () => {
    expect(() => parseFrame('{"v":1,"type":"call","input":{}}')).toThrow(/id/);
    expect(() => parseFrame('{"v":1,"type":"welcome","session_id":"s"}')).toThrow(/limits/);
  });

  it("welcome 的 limits / config / scope 逐字段校验", () => {
    const welcome = (over: Record<string, unknown>): string =>
      JSON.stringify({
        v: 1,
        type: "welcome",
        session_id: "s",
        host_version: "0",
        granted: [],
        limits: { inflight: 8, frame_bytes: 25165824, event_queue: 1024, log_lines_per_sec: 200 },
        config: { revision: 1, values: {} },
        scope: { kind: "machine" },
        ...over,
      });
    expect(() => parseFrame(welcome({ limits: { inflight: 8 } }))).toThrow(/frame_bytes/);
    expect(() =>
      parseFrame(welcome({ limits: { inflight: 0, frame_bytes: 1, event_queue: 1, log_lines_per_sec: 1 } })),
    ).toThrow(/inflight/);
    expect(() => parseFrame(welcome({ config: { values: {} } }))).toThrow(/revision/);
    expect(() => parseFrame(welcome({ config: { revision: 1 } }))).toThrow(/values/);
    expect(() => parseFrame(welcome({ scope: {} }))).toThrow(/scope.kind/);
    expect(() => parseFrame(welcome({ scope: { kind: "server", server: 1 } }))).toThrow(/scope.server/);
    // 合法帧原样通过
    expect(parseFrame(welcome({})).type).toBe("welcome");
  });

  it("config 帧必须有 values", () => {
    expect(() => parseFrame('{"v":1,"type":"config","id":1,"revision":2}')).toThrow(/values/);
  });

  it("call 区分 op / capability / contract", () => {
    const opCall = parseFrame('{"v":1,"type":"call","id":7,"op":"send","input":{}}');
    expect(opCall).toMatchObject({ type: "call", id: 7, op: "send" });
    const capCall = parseFrame('{"v":1,"type":"call","id":7,"capability":"console:write","method":"send_line","input":{}}');
    expect(capCall).toMatchObject({ capability: "console:write", method: "send_line" });
    const contractCall = parseFrame('{"v":1,"type":"call","id":8,"contract":"chat.publish@1","input":{}}');
    expect(contractCall).toMatchObject({ contract: "chat.publish@1" });
  });

  it("result 保留 ok / output / error", () => {
    const ok = parseFrame('{"v":1,"type":"result","id":1,"ok":true,"output":{"a":1}}');
    expect(ok).toMatchObject({ ok: true, output: { a: 1 } });
    const bad = parseFrame('{"v":1,"type":"result","id":1,"ok":false,"error":{"code":"denied"}}');
    expect(bad).toMatchObject({ ok: false, error: { code: "denied" } });
  });

  it("wireError 的分类与可重试与 §6.4 一致", () => {
    expect(wireError("denied", "x")).toEqual({ code: "denied", category: "unauthorized", retryable: false, message: "x" });
    expect(wireError("timeout", "x")).toEqual({ code: "timeout", category: "backend", retryable: true, message: "x" });
    expect(wireError("invalid_input", "x").category).toBe("usage");
    expect(wireError("limit_exceeded", "x").category).toBe("usage");
    expect(wireError("not_found", "x").category).toBe("not_found");
    expect(wireError("conflict", "x").category).toBe("conflict");
    expect(wireError("cancelled", "x").category).toBe("cancelled");
    expect(wireError("internal", "x").category).toBe("failure");
  });
});