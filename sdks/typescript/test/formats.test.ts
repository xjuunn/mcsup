import { describe, expect, it } from "vitest";

import { jsonSchemaOf } from "../src/descriptor.js";
import { z } from "../src/index.js";
import { isJsonSchemaFormat, isMcsupFormat, MCSup_FORMATS } from "../src/validate.js";

const property = (schema: z.ZodType, name = "value"): Record<string, unknown> => {
  const json = jsonSchemaOf(z.object({ [name]: schema }), false, "test") as {
    properties: Record<string, Record<string, unknown>>;
  };
  return json.properties[name]!;
};

describe("自定义展示格式（§11.6）", () => {
  it("六个 x-mcsup-format 全部接受并保留在扩展键", () => {
    for (const format of MCSup_FORMATS) {
      const field = property(z.string().meta({ format }));
      expect(field["x-mcsup-format"]).toBe(format);
      expect(field["format"]).toBeUndefined();
    }
  });

  it("未知展示格式拒绝", () => {
    expect(() => jsonSchemaOf(z.object({ v: z.string().meta({ format: "html" }) }), false)).toThrow(
      /x-mcsup-format/,
    );
    expect(() =>
      jsonSchemaOf(z.object({ v: z.string().meta({ format: "uuidv4" }) }), false),
    ).toThrow(/standard JSON Schema format/);
  });

  it("x-mcsup-format 只能用于字符串字段", () => {
    expect(() =>
      jsonSchemaOf(z.object({ n: z.number().meta({ format: "plain" }) }), false),
    ).toThrow(/requires a string schema/);
  });
});

describe("标准 JSON Schema format", () => {
  it("email / date / time / hostname / ipv4 等写入 format", () => {
    const formats = ["email", "date", "time", "hostname", "ipv4", "ipv6", "uri", "uuid"] as const;
    for (const format of formats) {
      const field = property(z.string().meta({ format }));
      expect(field["format"]).toBe(format);
      expect(field["x-mcsup-format"]).toBeUndefined();
    }
  });

  it("zod 的 .email() 保持标准 format", () => {
    const field = property(z.email());
    expect(field["format"]).toBe("email");
    expect(field["x-mcsup-format"]).toBeUndefined();
  });

  it("未知标准格式拒绝", () => {
    expect(() =>
      jsonSchemaOf(z.object({ v: z.string().meta({ format: "not-a-format" }) }), false),
    ).toThrow(/must be one of|standard JSON Schema format/);
  });

  it("判定函数与格式单表一致", () => {
    expect(isMcsupFormat("minecraft-text")).toBe(true);
    expect(isMcsupFormat("email")).toBe(false);
    expect(isJsonSchemaFormat("email")).toBe(true);
    expect(isJsonSchemaFormat("plain")).toBe(false);
  });
});
