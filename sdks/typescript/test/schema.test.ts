import { describe, expect, it } from "vitest";

import { action, definePlugin, query, s, z } from "../src/index.js";
import { buildDescriptor, buildManifest, jsonSchemaOf } from "../src/descriptor.js";
import { isField, toZod } from "../src/schema.js";

describe("字段 DSL（plugins.md §11.5 子集）", () => {
  it("s.* 产出字段，并能归约为 zod", () => {
    const field = s.string();
    expect(isField(field)).toBe(true);
    expect(isField(z.string())).toBe(false);
    expect(toZod(field)).toBe(field.schema);
    const direct = z.string();
    expect(toZod(direct)).toBe(direct);
  });

  it("object / array / optional 组合后 descriptor 结构正确", () => {
    const schema = s.object({
      items: s.array(s.object({ name: s.string({ max: 5 }) })),
      next: s.optional(s.string()),
    });
    const json = jsonSchemaOf(toZod(schema), false) as unknown as {
      readonly required: readonly string[];
      readonly properties: Record<string, Record<string, unknown>>;
    };
    expect(json.required).toEqual(["items"]);
    expect(json.properties["items"]).toMatchObject({ type: "array" });
    expect(json.properties["next"]).toMatchObject({ type: "string" });
  });

  it("默认值、标签、描述映射进 JSON Schema", () => {
    const schema = s.object({
      greeting: s.string({ default: "hi", label: "问候", describe: "开场白" }),
    });
    const json = jsonSchemaOf(toZod(schema), false) as {
      readonly properties: Record<string, Record<string, unknown>>;
    };
    expect(json.properties["greeting"]).toMatchObject({
      type: "string",
      default: "hi",
      description: "开场白",
      "x-mcsup-label": "问候",
    });
  });

  it("secret 只在输入方向允许", () => {
    const schema = s.object({ token: s.string({ secret: true }) });
    expect(() => jsonSchemaOf(toZod(schema), true)).toThrow(/secret/);
  });

  it("enum / datetime / integer 生成对应 JSON Schema", () => {
    const json = jsonSchemaOf(
      toZod(s.object({ kind: s.enum(["a", "b"]), at: s.datetime(), n: s.integer({ min: 1 }) })),
      false,
    ) as { readonly properties: Record<string, Record<string, unknown>> };
    expect(json.properties["kind"]).toMatchObject({ type: "string", enum: ["a", "b"] });
    expect(json.properties["at"]).toMatchObject({ type: "string", format: "date-time" });
    expect(json.properties["n"]).toMatchObject({ type: "integer", minimum: 1 });
  });
});

describe("DSL 与插件定义的接线", () => {
  it("DSL 写完整插件可生成 descriptor，risk 缺省按 kind 决定", () => {
    const plugin = definePlugin({
      id: "dsl-plugin",
      title: "DSL 插件",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["server:read"],
      config: s.object({ greeting: s.string({ default: "hi" }) }),
      queries: {
        servers: query("servers")
          .input(s.object({}))
          .output(s.object({ items: s.array(s.object({ name: s.string() })), next: s.optional(s.string()) }))
          .pagination({ cursorField: "next", limitField: "limit" })
          .run(() => ({ items: [] })),
      },
      actions: {
        ping: action("ping").run(() => ({ ok: true })),
      },
    });
    const d = buildDescriptor(plugin.definition);
    expect(d.schemas.config).toMatchObject({ type: "object", additionalProperties: false });
    expect(d.ops.find((op) => op.id === "servers")).toMatchObject({ kind: "query", risk: "read" });
    // action 缺省不能落成 read：有副作用的 op 默认 mutating
    expect(d.ops.find((op) => op.id === "ping")).toMatchObject({ kind: "action", risk: "mutating" });
  });

  it("原生对象写法同样按 kind 补默认 risk", () => {
    const plugin = definePlugin({
      id: "raw-ops",
      title: "原生 op",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["server:read"],
      config: s.object({}),
      actions: {
        mutate: { kind: "action", input: z.object({}), output: z.object({ ok: z.boolean() }), run: () => ({ ok: true }) },
      },
      queries: {
        read: { kind: "query", input: z.object({}), output: z.object({ ok: z.boolean() }), run: () => ({ ok: true }) },
      },
    });
    const d = buildDescriptor(plugin.definition);
    expect(d.ops.find((op) => op.id === "mutate")?.risk).toBe("mutating");
    expect(d.ops.find((op) => op.id === "read")?.risk).toBe("read");
  });

  it("空 capabilities 允许（纯计算插件）", () => {
    const plugin = definePlugin({
      id: "pure",
      title: "纯计算",
      version: "1.0.0",
      license: "MIT",
      capabilities: [],
      config: s.object({}),
    });
    const manifest = buildManifest(plugin.definition, "./bin/run.sh");
    expect(manifest.capabilities).toEqual([]);
  });

  it("api 与 SDK 主版本不一致时装配期报错", () => {
    expect(() =>
      definePlugin({
        id: "wrong-api",
        title: "错误 api",
        version: "1.0.0",
        license: "MIT",
        api: 99,
        capabilities: [],
        config: s.object({}),
      }),
    ).toThrow(/api 99/);
  });

  it("附录 D 形态（secret config + datetime + minecraft-text + 分页）可生成 descriptor", () => {
    const plugin = definePlugin({
      id: "chat-bridge-dsl",
      title: "聊天桥",
      version: "0.1.0",
      license: "MIT",
      capabilities: ["console:read", "events"],
      subscriptions: ["chat:message"],
      config: s.object({ token: s.string({ min: 1, secret: true, label: "Bot Token" }) }),
      queries: {
        history: query("history")
          .input(
            s.object({
              channel: s.string(),
              cursor: s.optional(s.integer()),
              limit: s.integer({ max: 100, default: 50 }),
            }),
          )
          .output(
            s.object({
              items: s.array(
                s.object({
                  id: s.string(),
                  at: s.datetime(),
                  from: s.string(),
                  text: s.string({ format: "minecraft-text" }),
                  direction: s.enum(["in", "out"]),
                }),
              ),
              next: s.optional(s.integer()),
            }),
          )
          .pagination({ cursorField: "next", limitField: "limit" })
          .run(() => ({ items: [] })),
      },
    });
    const d = buildDescriptor(plugin.definition);
    expect(d.ops[0]).toMatchObject({ id: "history", kind: "query", risk: "read", display: { type: "table" } });
    expect(JSON.stringify(d.schemas.config)).toContain("x-mcsup-secret");
    expect(JSON.stringify(d.schemas.history_out)).toContain('"format":"date-time"');
    expect(JSON.stringify(d.schemas.history_out)).toContain('"x-mcsup-format":"minecraft-text"');
  });
});