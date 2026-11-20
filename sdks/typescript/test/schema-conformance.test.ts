/**
 * §14.5 schema 一致性用例驱动：`sdks/conformance/schema/*.json`
 *
 * 每份用例用语言无关的字段声明描述一个 schema，驱动构造 zod schema 并比对
 * 归一化产物；`failure` 用例断言构建失败且错误消息包含指定片段。
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { describe, expect, it } from "vitest";
import { z } from "../src/index.js";
import { jsonSchemaOf } from "../src/descriptor.js";
import { validateSchemaSubset } from "../src/validate.js";

interface FieldDecl {
  readonly name: string;
  readonly type: "string" | "integer" | "boolean";
  readonly required?: boolean;
  readonly maxLength?: number;
  readonly enum?: readonly string[];
  readonly items?: { readonly type: "object"; readonly required?: readonly string[]; readonly fields: readonly FieldDecl[] };
  readonly fields?: readonly FieldDecl[];
  readonly default?: unknown;
  readonly meta?: { readonly secret?: boolean; readonly format?: string };
}

interface SchemaCase {
  readonly id: string;
  readonly description?: string;
  readonly field?: FieldDecl;
  readonly expect?: Record<string, unknown>;
  readonly absent?: readonly string[];
  readonly failure?: { readonly reason: string; readonly "message-contains": string; readonly field?: FieldDecl };
}

const DIR = path.resolve("..", "conformance", "schema");
const files = fs.readdirSync(DIR).filter((f) => f.endsWith(".json")).sort();

/** 键字典序排序（归一化第 4 条），让期望片段可以按声明顺序书写 */
function sorted(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map(sorted);
  }
  if (typeof value === "object" && value !== null) {
    const out: Record<string, unknown> = {};
    for (const key of Object.keys(value as Record<string, unknown>).sort()) {
      out[key] = sorted((value as Record<string, unknown>)[key]);
    }
    return out;
  }
  return value;
}

function buildField(field: FieldDecl): z.ZodType {
  let schema: z.ZodType;
  switch (field.type) {
    case "string":
      schema = field.maxLength !== undefined ? z.string().max(field.maxLength) : z.string();
      break;
    case "integer":
      schema = z.number().int();
      break;
    case "boolean":
      schema = z.boolean();
      break;
  }
  if (field.enum !== undefined) {
    schema = z.enum(field.enum as [string, ...string[]]);
  }
  if (field.items !== undefined) {
    schema = z.array(buildObject(field.items));
  }
  if (field.fields !== undefined) {
    schema = buildObject({ required: field.required === true ? [field.name] : [], fields: field.fields });
  }
  if (field.meta?.secret === true) {
    schema = schema.meta({ secret: true });
  }
  if (field.meta?.format !== undefined) {
    schema = schema.meta({ format: field.meta.format });
  }
  if (field.default !== undefined) {
    schema = schema.default(field.default as never);
  } else if (field.required !== true) {
    schema = schema.optional();
  }
  return schema;
}

function buildObject(spec: { readonly required?: readonly string[]; readonly fields: readonly FieldDecl[] }): z.ZodType {
  const shape: Record<string, z.ZodType> = {};
  for (const field of spec.fields) {
    shape[field.name] = buildField(field);
  }
  return z.object(shape);
}

describe("schema conformance (§14.5)", () => {
  for (const file of files) {
    const testCase = JSON.parse(fs.readFileSync(path.join(DIR, file), "utf8")) as SchemaCase;
    it(testCase.id, () => {
      if (testCase.failure !== undefined) {
        const failure = testCase.failure;
        expect(() => {
          if (failure.reason === "union") {
            jsonSchemaOf(z.object({ u: z.union([z.string(), z.number()]) }), false, "config");
          } else if (failure.reason === "recursion") {
            const node: z.ZodType = z.lazy(() => node);
            jsonSchemaOf(node, false, "config");
          } else if (failure.reason === "external-ref") {
            validateSchemaSubset("config", { $ref: "https://example.com/schema.json", type: "string" });
          } else if (failure.reason === "limit") {
            if (failure.field === undefined) {
              throw new Error(`case ${testCase.id} is missing failure.field`);
            }
            jsonSchemaOf(z.object({ [failure.field.name]: buildField(failure.field) }), false, "config");
          } else {
            throw new Error(`unknown failure reason ${failure.reason}`);
          }
        }).toThrow(new RegExp(failure["message-contains"].replace(/[$()*+.?[\\\]^{|}]/g, "\\$&")));
        return;
      }
      if (testCase.field === undefined || testCase.expect === undefined) {
        throw new Error(`case ${testCase.id} needs either field+expect or failure`);
      }
      const json = jsonSchemaOf(z.object({ [testCase.field.name]: buildField(testCase.field) }), false, "config") as {
        required?: string[];
        properties: Record<string, unknown>;
      };
      const actual = sorted(json.properties[testCase.field.name]);
      expect(actual).toEqual(sorted(testCase.expect));
      if (testCase.field.required === true) {
        expect(json.required).toContain(testCase.field.name);
      } else {
        expect(json.required ?? []).not.toContain(testCase.field.name);
      }
      const serialized = JSON.stringify(actual);
      for (const forbidden of testCase.absent ?? []) {
        expect(serialized).not.toContain(`"${forbidden}"`);
      }
    });
  }
});
