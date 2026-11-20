import { describe, expect, it } from "vitest";

import { MAX_JSON_DEPTH, PROTOCOL_VERSION, ProtocolError, encodeFrame, parseFrame, wireError } from "../src/protocol.js";

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

  it("超过 1 MiB 拒绝", () => {
    const huge = `{"v":1,"type":"ping","n":1,"pad":"${"x".repeat(1024 * 1024)}"}`;
    expect(() => parseFrame(huge)).toThrow(/size limit/);
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