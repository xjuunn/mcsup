import { describe, expect, it } from "vitest";

import { buildDescriptor } from "../src/descriptor.js";
import { definePlugin, query, z } from "../src/index.js";

const listOutput = z.object({ items: z.array(z.object({ name: z.string() })) });

const pagedQuery = () =>
  query("rows")
    .input(z.object({}))
    .output(listOutput)
    .pagination({ cursorField: "next", limitField: "limit" });

const pluginWithPagination = (pagination: unknown) =>
  definePlugin({
    id: "paged",
    title: "分页",
    version: "1.0.0",
    license: "MIT",
    capabilities: ["server:read"],
    config: z.object({}),
    queries: {
      rows: { ...pagedQuery().build(), pagination },
    },
  });

describe("分页 builder（§11.4）", () => {
  it("省略时写满四个字段并取默认 50 / 200", () => {
    expect(pagedQuery().build().pagination).toEqual({
      cursor_field: "next",
      limit_field: "limit",
      default_limit: 50,
      max_limit: 200,
    });
  });

  it("显式上限按声明写入", () => {
    expect(
      pagedQuery().pagination({ cursorField: "next", limitField: "limit", defaultLimit: 10, maxLimit: 50 }).build()
        .pagination,
    ).toEqual({ cursor_field: "next", limit_field: "limit", default_limit: 10, max_limit: 50 });
  });

  it("拒绝非法上限", () => {
    expect(() =>
      pagedQuery().pagination({ cursorField: "next", limitField: "limit", defaultLimit: 0 }),
    ).toThrow(/defaultLimit/);
    expect(() =>
      pagedQuery().pagination({ cursorField: "next", limitField: "limit", defaultLimit: 20, maxLimit: 10 }),
    ).toThrow(/must not exceed maxLimit/);
    expect(() =>
      pagedQuery().pagination({ cursorField: "next", limitField: "limit", maxLimit: 1_001 }),
    ).toThrow(/1\.\.=1000/);
    expect(() =>
      pagedQuery().pagination({ cursorField: "next", limitField: "limit", defaultLimit: 1.5 }),
    ).toThrow(/positive integer/);
  });
});

describe("descriptor 分页归一化（§11.4）", () => {
  it("builder 产物原样写入四个 snake_case 键", () => {
    const descriptor = buildDescriptor(pluginWithPagination(pagedQuery().build().pagination).definition);
    expect(descriptor.ops[0]?.pagination).toEqual({
      cursor_field: "next",
      limit_field: "limit",
      default_limit: 50,
      max_limit: 200,
    });
  });

  it("接受手写 camelCase 并归一化", () => {
    const descriptor = buildDescriptor(
      pluginWithPagination({
        cursorField: "next",
        limitField: "limit",
        defaultLimit: 10,
        maxLimit: 20,
      }).definition,
    );
    expect(descriptor.ops[0]?.pagination).toEqual({
      cursor_field: "next",
      limit_field: "limit",
      default_limit: 10,
      max_limit: 20,
    });
  });

  it("缺少 default_limit / max_limit 时拒绝（宿主无默认值）", () => {
    expect(() =>
      buildDescriptor(pluginWithPagination({ cursorField: "next", limitField: "limit" }).definition),
    ).toThrow(/default_limit/);
    expect(() =>
      buildDescriptor(
        pluginWithPagination({ cursorField: "next", limitField: "limit", defaultLimit: 10 }).definition,
      ),
    ).toThrow(/max_limit/);
  });

  it("拒绝越界与非整数", () => {
    const limits = (defaultLimit: number, maxLimit: number) => ({
      cursorField: "next",
      limitField: "limit",
      defaultLimit,
      maxLimit,
    });
    expect(() => buildDescriptor(pluginWithPagination(limits(20, 10)).definition)).toThrow(
      /1 <= default_limit <= max_limit <= 1000/,
    );
    expect(() => buildDescriptor(pluginWithPagination(limits(10, 1_001)).definition)).toThrow(
      /1 <= default_limit <= max_limit <= 1000/,
    );
    expect(() => buildDescriptor(pluginWithPagination(limits(1, 10.5)).definition)).toThrow(/integer/);
    expect(() => buildDescriptor(pluginWithPagination(limits(0, 10)).definition)).toThrow(
      /1 <= default_limit <= max_limit <= 1000/,
    );
  });

  it("分页字段名必须非空", () => {
    expect(() =>
      buildDescriptor(
        pluginWithPagination({ cursorField: " ", limitField: "limit", defaultLimit: 10, maxLimit: 20 })
          .definition,
      ),
    ).toThrow(/cursor_field/);
  });
});
