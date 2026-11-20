import { describe, expect, it } from "vitest";
import { z } from "../src/index.js";
import { buildDescriptor, buildManifest, jsonSchemaOf, serializeNormalized } from "../src/descriptor.js";
import { chatBridge, minimal } from "./fixtures.js";
import * as fs from "node:fs";
import * as path from "node:path";

describe("manifest", () => {
  it("按 §10.1 生成必填字段", () => {
    const manifest = buildManifest(minimal.definition, "./bin/run.sh");
    expect(manifest).toMatchObject({
      schema: 1,
      id: "minimal",
      name: "最小插件",
      version: "1.0.0",
      api: 1,
      entry: { type: "process", cmd: "./bin/run.sh" },
      capabilities: ["server:read"],
      license: "MIT",
    });
  });

  it("订阅名从声明里归一化", () => {
    const manifest = buildManifest(chatBridge.definition, "./bin/run.sh");
    expect(manifest.subscriptions).toEqual(["chat:message"]);
    expect(manifest.description).toBe("把服务器聊天转发到外部渠道");
  });

  it("拒绝非法 id / 空 license / 重复能力", () => {
    const bad = { ...minimal.definition, id: "Bad_Id" } as unknown as typeof minimal.definition;
    expect(() => buildManifest(bad, "./run")).toThrow(/manifest.id/);

    const noLicense = { ...minimal.definition, license: " " } as typeof minimal.definition;
    expect(() => buildManifest(noLicense, "./run")).toThrow(/license/);

    const dup = { ...minimal.definition, capabilities: ["server:read", "server:read"] } as unknown as typeof minimal.definition;
    expect(() => buildManifest(dup, "./run")).toThrow(/deduplicated/);
  });

  it("net 白名单要求 net:outbound", () => {
    const bad = { ...minimal.definition, net: { allow: ["api.example.com"] } } as unknown as typeof minimal.definition;
    expect(() => buildManifest(bad, "./run")).toThrow(/net:outbound/);
  });
});

describe("descriptor", () => {
  it("schema 名固定为 config / <op>_in / <op>_out", () => {
    const d = buildDescriptor(chatBridge.definition);
    expect(Object.keys(d.schemas).sort()).toEqual([
      "config",
      "counter_in",
      "counter_out",
      "failing_in",
      "failing_out",
      "messages_in",
      "messages_out",
      "send_in",
      "send_out",
      "slow_in",
      "slow_out",
    ]);
    expect(d.config).toEqual({ $ref: "#/schemas/config" });
    expect(d.ops.map((op) => op.id)).toEqual(["counter", "send", "failing", "messages", "slow"]);
  });

  it("op 元数据与 display 缺省", () => {
    const d = buildDescriptor(chatBridge.definition);
    const send = d.ops.find((op) => op.id === "send");
    expect(send).toMatchObject({ kind: "action", title: "发送消息", risk: "mutating", display: { type: "detail" } });
    const messages = d.ops.find((op) => op.id === "messages");
    expect(messages).toMatchObject({ kind: "stream", display: { type: "log" } });
    const counter = d.ops.find((op) => op.id === "counter");
    expect(counter?.risk).toBe("read");
  });

  it("内联 $defs、折叠可空、键字典序", () => {
    const schema = z
      .object({
        nested: z.object({ b: z.string().meta({ id: "inner" }), a: z.number().optional().nullable() }).meta({ id: "outer" }),
      })
      .meta({ id: "root" });
    const json = jsonSchemaOf(schema, false) as Record<string, unknown>;
    expect(JSON.stringify(json)).not.toContain("$ref");
    expect(JSON.stringify(json)).not.toContain("$defs");
    expect(JSON.stringify(json)).not.toContain("anyOf");
    expect(JSON.stringify(json)).not.toContain('"null"');
    const keys = Object.keys(json["properties"] as Record<string, unknown>);
    expect(keys).toEqual(["nested"]);
    const nestedKeys = Object.keys((json["properties"] as Record<string, Record<string, unknown>>)["nested"]!);
    expect(nestedKeys).toEqual(["properties", "required", "type"]);
  });

  it("meta 键映射到 x-mcsup-*", () => {
    const json = jsonSchemaOf(
      z.object({ token: z.string().meta({ secret: true, label: "Token", format: "plain" }) }),
      false,
    ) as Record<string, Record<string, Record<string, unknown>>>;
    expect(json["properties"]!["token"]).toMatchObject({ "x-mcsup-secret": true, "x-mcsup-label": "Token", "x-mcsup-format": "plain" });
  });

  it("输出方向禁止 secret", () => {
    expect(() => jsonSchemaOf(z.object({ t: z.string().meta({ secret: true }) }), true)).toThrow(/secret/);
  });

  it("非法 format 拒绝", () => {
    expect(() => jsonSchemaOf(z.object({ t: z.string().meta({ format: "html" }) }), false)).toThrow(/x-mcsup-format/);
  });

  it("两次构建逐字节一致", () => {
    const a = serializeNormalized(buildDescriptor(chatBridge.definition));
    const b = serializeNormalized(buildDescriptor(chatBridge.definition));
    expect(a).toBe(b);
  });

  it("与 conformance 期望片段一致", () => {
    const file = path.resolve("..", "conformance", "descriptor", "chat-bridge.json");
    if (!fs.existsSync(file)) {
      return;
    }
    const expected = fs.readFileSync(file, "utf8");
    const actual = serializeNormalized(buildDescriptor(chatBridge.definition));
    expect(actual).toBe(expected);
  });
});