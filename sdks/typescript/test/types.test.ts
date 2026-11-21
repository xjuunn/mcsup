import { describe, expect, it } from "vitest";

import { action, definePlugin, query, s, z } from "../src/index.js";

/**
 * 类型层断言：`@ts-expect-error` 由 `tsc -p tsconfig.test.json` 校验，
 * vitest 只确认用例被加载。若能力映射失效，tsc 会报「未使用的 @ts-expect-error」。
 */
describe("类型安全", () => {
  it("能力驱动的 Ctx 在编译期拒绝越权", () => {
    const plugin = definePlugin({
      id: "typed",
      title: "typed",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["server:read"],
      config: z.object({}),
      actions: {
        inspect: action("inspect")
          .capabilities(["server:read"])
          .input(z.object({ server: z.string() }))
          .output(z.object({ name: z.string() }))
          .run(async (input, ctx) => {
            // 已声明：可用
            const list = await ctx["server:read"].servers.list();
            void list;
            void input.server;
            // @ts-expect-error console:read 未声明
            ctx["console:read"];
            return { name: "ok" };
          }),
      },
      queries: {
        value: query("value").run(() => ({})),
      },
    });
    expect(plugin.definition.id).toBe("typed");
  });

  it("op 输入输出由 schema 推导", () => {
    const op = query("derived")
      .input(z.object({ n: z.number().int() }))
      .output(z.object({ doubled: z.number().int() }))
      .run((input) => {
        const n: number = input.n;
        return { doubled: n * 2 };
      });
    expect(op.kind).toBe("query");
  });

  it("run 返回值与 output 不符时编译期报错", () => {
    const bad = query("bad")
      .input(z.object({}))
      .output(z.object({ text: z.string() }))
      // @ts-expect-error 返回形状与 output 不一致
      .run(() => ({ text: 1 }));
    // 运行期仍会由 schema 兜底
    expect(bad.kind).toBe("query");
  });

  it("字段 DSL 推导 input / output 类型", () => {
    const op = query("dsl-typed")
      .input(s.object({ name: s.string(), count: s.integer() }))
      .output(s.object({ ok: s.boolean() }))
      .run((input) => {
        const name: string = input.name;
        const count: number = input.count;
        void name;
        void count;
        // @ts-expect-error 字段是 string
        const wrong: number = input.name;
        void wrong;
        return { ok: true };
      });
    expect(op.kind).toBe("query");
  });

  it("config 类型推导到 ctx.config，并支持默认值", () => {
    const plugin = definePlugin({
      id: "dsl-config",
      title: "DSL 配置",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["server:read"],
      config: s.object({ greeting: s.string({ default: "hi" }), limit: s.integer(), note: s.optional(s.string()) }),
      onStart(ctx) {
        const greeting: string = ctx.config.greeting;
        const limit: number = ctx.config.limit;
        const note: string | undefined = ctx.config.note;
        void greeting;
        void limit;
        void note;
        // @ts-expect-error 推导为 string
        const wrong: number = ctx.config.greeting;
        void wrong;
      },
    });
    expect(plugin.definition.id).toBe("dsl-config");
  });
});
