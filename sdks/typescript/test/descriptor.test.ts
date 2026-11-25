import { describe, expect, it } from "vitest";
import { definePlugin, action, query, stream, z } from "../src/index.js";
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
      "reentrant_in",
      "reentrant_out",
      "send_in",
      "send_out",
      "slow_in",
      "slow_out",
    ]);
    expect(d.config).toEqual({ $ref: "#/schemas/config" });
    expect(d.ops.map((op) => op.id)).toEqual(["counter", "send", "failing", "reentrant", "messages", "slow"]);
    // 未声明 setup 时保持旧版本，兼容只认识 schema 1 的宿主
    expect(d.schema).toBe(1);
    expect(d.setup).toBeUndefined();
  });

  it("setup 生成 schema 2 与逐项提示块", () => {
    const def = {
      ...chatBridge.definition,
      setup: {
        start: false,
        fields: [
          { path: "token", required: true, secret: true, label: "Token", description: "访问令牌" },
          { path: "nested", default: "x" },
        ],
      },
    } as typeof chatBridge.definition;
    const d = buildDescriptor(def);
    expect(d.schema).toBe(2);
    expect(d.setup).toEqual({
      start: false,
      fields: [
        { path: "token", required: true, secret: true, label: "Token", description: "访问令牌" },
        { path: "nested", default: "x" },
      ],
    });
  });

  it("setup 透传 choicesFrom 动态选项来源", () => {
    const def = {
      ...chatBridge.definition,
      setup: {
        fields: [{ path: "qq.server", required: true, choicesFrom: "servers" }],
      },
    } as typeof chatBridge.definition;
    expect(buildDescriptor(def).setup).toEqual({
      fields: [{ path: "qq.server", required: true, choicesFrom: "servers" }],
    });
  });

  it("setup 字段条件生成 schema 4 与 when 块", () => {
    const def = {
      ...chatBridge.definition,
      setup: {
        fields: [
          { path: "platforms", required: true, choices: ["qq", "telegram"] },
          { path: "qq.appId", required: true, when: { path: "platforms", includes: "qq" } },
        ],
      },
    } as typeof chatBridge.definition;
    const d = buildDescriptor(def);
    expect(d.schema).toBe(4);
    expect(d.setup).toEqual({
      fields: [
        { path: "platforms", required: true, choices: ["qq", "telegram"] },
        { path: "qq.appId", required: true, when: { path: "platforms", includes: "qq" } },
      ],
    });
  });

  it("交互式字段生成 schema 5 并保留 interactive 标记", () => {
    const def = {
      ...chatBridge.definition,
      setup: {
        fields: [{ path: "token", required: true, secret: true, interactive: true }],
        interactive: { token: () => "from-hook" },
      },
    } as typeof chatBridge.definition;
    const d = buildDescriptor(def);
    expect(d.schema).toBe(5);
    expect(d.setup).toEqual({
      fields: [{ path: "token", required: true, secret: true, interactive: true }],
    });
  });

  it("对象描述生成 schema 6 并按 style 归一化", () => {
    const def = {
      ...chatBridge.definition,
      setup: {
        fields: [
          {
            path: "qq.appId",
            description: {
              style: "steps",
              title: " 获取 AppID ",
              steps: [" 打开 https://q.qq.com/ ", "复制 AppID"],
              links: [{ label: "文档", url: " https://bot.q.qq.com/wiki/ " }],
            },
          },
        ],
      },
    } as typeof chatBridge.definition;
    const d = buildDescriptor(def);
    expect(d.schema).toBe(6);
    expect(d.setup).toEqual({
      fields: [
        {
          path: "qq.appId",
          description: {
            style: "steps",
            title: "获取 AppID",
            steps: ["打开 https://q.qq.com/", "复制 AppID"],
            links: [{ label: "文档", url: "https://bot.q.qq.com/wiki/" }],
          },
        },
      ],
    });
  });

  it("字符串描述按原样输出，不抬高 schema 版本", () => {
    const def = {
      ...chatBridge.definition,
      setup: {
        fields: [{ path: "qq.appId", description: "第一行\n第二行" }],
      },
    } as typeof chatBridge.definition;
    const d = buildDescriptor(def);
    expect(d.schema).toBe(2);
    expect(d.setup).toEqual({
      fields: [{ path: "qq.appId", description: "第一行\n第二行" }],
    });
  });

  it("对象描述拒绝错的形状与非 http 链接", () => {
    const withDescription = (description: unknown) =>
      ({
        ...chatBridge.definition,
        setup: { fields: [{ path: "token", description }] },
      }) as typeof chatBridge.definition;
    expect(() =>
      buildDescriptor(withDescription({ style: "steps", text: "x" })),
    ).toThrow(/declare steps/);
    expect(() => buildDescriptor(withDescription({ steps: ["x"] }))).toThrow(
      /is for style steps/,
    );
    expect(() =>
      buildDescriptor(withDescription({ text: "x", links: [{ label: "a", url: "ftp://x" }] })),
    ).toThrow(/http/);
    expect(() => buildDescriptor(withDescription({ title: " ", text: "x" }))).toThrow(/1\.\.=64/);
  });

  it("交互式字段必须与 handler 一一对应", () => {
    const missing = {
      ...chatBridge.definition,
      setup: {
        fields: [{ path: "token", secret: true, interactive: true }],
      },
    } as typeof chatBridge.definition;
    expect(() => buildDescriptor(missing)).toThrow(/no handler in setup\.interactive/);

    const orphan = {
      ...chatBridge.definition,
      setup: {
        fields: [{ path: "token", secret: true }],
        interactive: { token: () => "x" },
      },
    } as typeof chatBridge.definition;
    expect(() => buildDescriptor(orphan)).toThrow(/does not declare interactive/);

    const unknown = {
      ...chatBridge.definition,
      setup: {
        fields: [{ path: "token", secret: true, interactive: true }],
        interactive: { token: () => "x", other: () => "y" },
      },
    } as typeof chatBridge.definition;
    expect(() => buildDescriptor(unknown)).toThrow(/does not match a declared setup field/);
  });

  it("when 只接受 includes / equals 之一", () => {
    const withWhen = (when: unknown) =>
      ({
        ...chatBridge.definition,
        setup: { fields: [{ path: "platforms" }, { path: "token", when }] },
      }) as typeof chatBridge.definition;
    expect(() => buildDescriptor(withWhen({ path: "platforms" }))).toThrow(/includes or equals/);
    expect(() =>
      buildDescriptor(withWhen({ path: "platforms", includes: "qq", equals: true })),
    ).toThrow(/both/);
    expect(() => buildDescriptor(withWhen({ path: " ", includes: "qq" }))).toThrow(
      /non-empty string/,
    );
    expect(buildDescriptor(withWhen({ path: "platforms", equals: true })).setup).toEqual({
      fields: [
        { path: "platforms" },
        { path: "token", when: { path: "platforms", equals: true } },
      ],
    });
  });

  it("bind 生成 schema 3 与服务器路径", () => {
    const def = {
      ...chatBridge.definition,
      bind: { servers: ["qq.server", "extra"] },
    } as typeof chatBridge.definition;
    const d = buildDescriptor(def);
    expect(d.schema).toBe(3);
    expect(d.bind).toEqual({ servers: ["qq.server", "extra"] });
  });

  it("bind 拒绝空路径、重复与超限", () => {
    const withBind = (servers: string[]) =>
      ({ ...chatBridge.definition, bind: { servers } }) as typeof chatBridge.definition;
    expect(() => buildDescriptor(withBind([" "]))).toThrow(/must not be empty/);
    expect(() => buildDescriptor(withBind(["a", " a "]))).toThrow(/duplicate/);
    expect(() => buildDescriptor(withBind(Array.from({ length: 9 }, (_, i) => `s${i}`)))).toThrow(
      /more than 8/,
    );
  });

  it("op 元数据与 display 缺省", () => {
    const d = buildDescriptor(chatBridge.definition);
    const send = d.ops.find((op) => op.id === "send");
    // action 缺省推断为 form（§11.4）
    expect(send).toMatchObject({ kind: "action", title: "发送消息", risk: "mutating", display: { type: "form" } });
    const messages = d.ops.find((op) => op.id === "messages");
    expect(messages).toMatchObject({ kind: "stream", display: { type: "log" } });
    const counter = d.ops.find((op) => op.id === "counter");
    expect(counter).toMatchObject({ kind: "query", display: { type: "detail" }, risk: "read", timeout_ms: 30000 });
  });

  it("查询输出含 items 数组时推断为 table 并要求分页", () => {
    const paged = definePlugin({
      id: "paged",
      title: "分页查询",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["server:read"],
      config: z.object({}),
      queries: {
        rows: query("rows")
          .input(z.object({}))
          .output(z.object({ items: z.array(z.object({ name: z.string() })), next: z.string().optional() }))
          .pagination({ cursorField: "next", limitField: "limit" })
          .run(() => ({ items: [] })),
      },
    });
    const d = buildDescriptor(paged.definition);
    expect(d.ops[0]).toMatchObject({ display: { type: "table" }, pagination: { cursor_field: "next", limit_field: "limit" } });
  });

  it("列表类 query 缺 pagination 时失败并指名字段", () => {
    const bad = definePlugin({
      id: "paged",
      title: "分页查询",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["server:read"],
      config: z.object({}),
      queries: {
        rows: query("rows")
          .input(z.object({}))
          .output(z.object({ items: z.array(z.object({ name: z.string() })) }))
          .run(() => ({ items: [] })),
      },
    });
    expect(() => buildDescriptor(bad.definition)).toThrow(/ops\.rows\.pagination/);
  });

  it("非 object 的 query 输出必须显式声明 display", () => {
    const bad = definePlugin({
      id: "bare",
      title: "裸数组",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["server:read"],
      config: z.object({}),
      queries: {
        names: query("names").input(z.object({})).output(z.array(z.string())).run(() => []),
      },
    });
    expect(() => buildDescriptor(bad.definition)).toThrow(/ops\.names\.display/);
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
    expect(nestedKeys).toEqual(["additionalProperties", "properties", "required", "type"]);
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

  it("zod 标准 format 保留为 format，不当作展示格式", () => {
    const json = jsonSchemaOf(
      z.object({ at: z.iso.datetime(), id: z.uuid(), link: z.url() }),
      false,
    ) as { properties: Record<string, Record<string, unknown>> };
    expect(json.properties["at"]?.["format"]).toBe("date-time");
    expect(json.properties["id"]?.["format"]).toBe("uuid");
    expect(json.properties["link"]?.["format"]).toBe("uri");
    for (const key of ["at", "id", "link"]) {
      expect(json.properties[key]?.["x-mcsup-format"]).toBeUndefined();
    }
  });

  it("标准 JSON Schema format 不被误判为自定义展示格式", () => {
    const descriptor = buildDescriptor(
      definePlugin({
        id: "mail",
        title: "mail",
        version: "1.0.0",
        license: "MIT",
        capabilities: ["server:read"],
        config: z.object({ mail: z.email() }),
      }).definition,
    );
    expect(JSON.stringify(descriptor.schemas["config"])).toContain('"format":"email"');
    expect(JSON.stringify(descriptor.schemas["config"])).not.toContain("x-mcsup-format");
  });

  it("两次构建逐字节一致", () => {
    const a = serializeNormalized(buildDescriptor(chatBridge.definition));
    const b = serializeNormalized(buildDescriptor(chatBridge.definition));
    expect(a).toBe(b);
  });

  it("拒绝 union / transform / refine / 递归 / 元组 / 外部 $ref（§11.5）", () => {
    expect(() => jsonSchemaOf(z.union([z.string(), z.number()]), false, "config")).toThrow(/union/);
    expect(() => jsonSchemaOf(z.string().transform((v) => v.length), false, "config")).toThrow(/transform/);
    expect(() => jsonSchemaOf(z.string().refine((v) => v.length > 0), false, "config")).toThrow(/refine/);
    expect(() => jsonSchemaOf(z.string().superRefine(() => undefined), false, "config")).toThrow(/refine/);
    expect(() => jsonSchemaOf(z.tuple([z.string(), z.number()]), false, "config")).toThrow(/tuple/);
    expect(() => jsonSchemaOf(z.record(z.string(), z.string()), false, "config")).toThrow(/map/);
    const cyclic: Record<string, z.ZodType> = {};
    const node: z.ZodType = z.lazy(() => node);
    cyclic["node"] = node;
    expect(() => jsonSchemaOf(node, false, "config")).toThrow(/recursive/);
  });

  it("超限报实际值与上限并指名字段（§11.5）", () => {
    expect(() => jsonSchemaOf(z.object({ s: z.string().max(8193) }), false, "config")).toThrow(
      /config\.properties\.s\.maxLength: is 8193, limit is 8192/,
    );
    const many = Object.fromEntries(Array.from({ length: 65 }, (_, i) => [`f${i}`, z.string()]));
    expect(() => jsonSchemaOf(z.object(many), false, "config")).toThrow(/config\.properties: has 65 properties, limit is 64/);
    expect(() => jsonSchemaOf(z.enum(Array.from({ length: 101 }, (_, i) => `v${i}`)), false, "config")).toThrow(
      /config\.enum: has 101 values, limit is 100/,
    );
  });

  it("深度超过 6 层失败（§11.5）", () => {
    let deep: z.ZodType = z.string();
    for (let i = 0; i < 8; i += 1) {
      deep = z.object({ n: deep });
    }
    expect(() => jsonSchemaOf(deep, false, "config")).toThrow(/depth exceeds 6/);
  });

  it("display 形状缺失失败并指名字段（§11.3）", () => {
    const bad = definePlugin({
      id: "shape",
      title: "形状",
      version: "1.0.0",
      license: "MIT",
      capabilities: [],
      config: z.object({}),
      streams: {
        logs: stream("logs")
          .output(z.object({ text: z.string() }))
          .run(function* () {
            yield { text: "x" };
          }),
      },
    });
    expect(() => buildDescriptor(bad.definition)).toThrow(/ops\.logs\.output\.properties\.at/);
  });

  it("chat 的 send / stream 必须指向对应 kind 的已声明 op（§11.3）", () => {
    const base = {
      id: "chat",
      title: "聊天",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["console:read", "console:write"],
      config: z.object({}),
    } as const;
    const output = z.object({
      items: z.array(z.object({ id: z.string(), at: z.string(), from: z.string(), text: z.string() })),
      next: z.string().optional(),
    });
    const good = definePlugin({
      ...base,
      queries: {
        history: query("history")
          .display({ type: "chat", send: "send", stream: "messages" })
          .input(z.object({}))
          .output(output)
          .pagination({ cursorField: "next", limitField: "limit" })
          .run(() => ({ items: [] })),
      },
      actions: { send: action("send").input(z.object({})).output(z.object({})).run(() => ({})) },
      streams: {
        messages: stream("messages")
          .output(z.object({ at: z.string(), text: z.string() }))
          .run(function* () {
            yield { at: "2026-11-18T09:31:00Z", text: "" };
          }),
      },
    });
    expect(buildDescriptor(good.definition).ops[0]).toMatchObject({
      display: { type: "chat", send: "send", stream: "messages" },
    });

    const dangling = definePlugin({
      ...base,
      queries: {
        history: query("history")
          .display({ type: "chat", send: "missing", stream: "messages" })
          .input(z.object({}))
          .output(output)
          .pagination({ cursorField: "next", limitField: "limit" })
          .run(() => ({ items: [] })),
      },
      streams: {
        messages: stream("messages")
          .output(z.object({ at: z.string(), text: z.string() }))
          .run(function* () {
            yield { at: "2026-11-18T09:31:00Z", text: "" };
          }),
      },
    });
    expect(() => buildDescriptor(dangling.definition)).toThrow(/ops\.history\.display\.send: refers to undeclared op missing/);
  });

  it("op 字段约束失败并指名字段（§11.2）", () => {
    expect(() => buildDescriptor({ ...minimal.definition, queries: {} })).not.toThrow();
    const longTitle = definePlugin({
      id: "long",
      title: "长标题",
      version: "1.0.0",
      license: "MIT",
      capabilities: [],
      config: z.object({}),
      queries: {
        q: query("q").title("x".repeat(65)).input(z.object({})).output(z.object({})).run(() => ({})),
      },
    });
    expect(() => buildDescriptor(longTitle.definition)).toThrow(/ops\.q\.title: is 65 chars, limit is 64/);

    const badTimeout = definePlugin({
      id: "timeout",
      title: "超时",
      version: "1.0.0",
      license: "MIT",
      capabilities: [],
      config: z.object({}),
      queries: {
        q: query("q").timeout(999).input(z.object({})).output(z.object({})).run(() => ({})),
      },
    });
    expect(() => buildDescriptor(badTimeout.definition)).toThrow(/ops\.q\.timeout_ms/);
  });

  it("op id 不接受连字符（§11.2）", () => {
    const bad = definePlugin({
      id: "ids",
      title: "标识",
      version: "1.0.0",
      license: "MIT",
      capabilities: [],
      config: z.object({}),
      queries: {
        "bad-id": query("bad-id").input(z.object({})).output(z.object({})).run(() => ({})),
      },
    });
    expect(() => buildDescriptor(bad.definition)).toThrow(/ops\.bad-id\.id/);
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