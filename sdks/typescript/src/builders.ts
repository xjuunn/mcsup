/**
 * Builder 层：造「值」片段（sdk-design.md §4.2）
 *
 * Builder 是纯值构造器：无环境状态、无 IO，产物是内核可接受的 op 片段。
 * 终点不是插件，`.build()` 回来的片段要回填进 `definePlugin({...})`。
 */

import { z } from "zod";

import type { OpSpec, PluginContext, Capability, StreamChunks } from "./define.js";

/** 风险等级（plugins.md §8.3） */
export type OpRisk = "read" | "mutating" | "destructive";

/** op 种类 */
export type OpKind = "query" | "action" | "stream";

const emptyObject = (): z.ZodType => z.object({}).strict();

interface Draft {
  id: string;
  kind?: OpKind;
  title?: string;
  description?: string;
  risk?: OpRisk;
  display?: unknown;
  input: z.ZodType;
  output: z.ZodType;
  timeoutMs?: number;
  pagination?: unknown;
  run?: unknown;
}

/** builder 上暂存 schema / 能力声明的键（供 run 推导参数类型） */
const InputKey: unique symbol = Symbol("input");
const OutputKey: unique symbol = Symbol("output");
const CapKey: unique symbol = Symbol("capabilities");

type WithInput<B, S extends z.ZodType> = B & { readonly [InputKey]: S };
type WithOutput<B, S extends z.ZodType> = B & { readonly [OutputKey]: S };
type InputOf<B> = B extends { readonly [InputKey]: infer S extends z.ZodType } ? S : z.ZodType;
type OutputOf<B> = B extends { readonly [OutputKey]: infer S extends z.ZodType } ? S : z.ZodType;
type CapOf<B> = B extends { readonly [CapKey]: infer C extends readonly Capability[] } ? C : readonly Capability[];

abstract class BaseOpBuilder<K extends OpKind, I extends z.ZodType, O extends z.ZodType, R> {
  protected readonly draft: Draft;

  protected constructor(id: string, kind: K) {
    this.draft = { id, kind, input: emptyObject(), output: emptyObject() };
  }

  /** 展示名；省略时用 id */
  public title(title: string): this {
    this.draft.title = title;
    return this;
  }

  public describe(description: string): this {
    this.draft.description = description;
    return this;
  }

  /** 风险等级；默认 `read`。query 必须是 read，action/stream 可用更高等级 */
  public risk(risk: OpRisk): this {
    this.draft.risk = risk;
    return this;
  }

  /** 界面提示（plugins.md §11.3）；缺省 `detail`（stream 为 `log`） */
  public display(display: unknown): this {
    this.draft.display = display;
    return this;
  }

  /** 输入 schema */
  public input<S extends z.ZodType>(schema: S): this & { readonly [InputKey]: S } {
    this.draft.input = schema;
    return this as unknown as this & { readonly [InputKey]: S };
  }

  /** 输出 schema */
  public output<S extends z.ZodType>(schema: S): this & { readonly [OutputKey]: S } {
    this.draft.output = schema;
    return this as unknown as this & { readonly [OutputKey]: S };
  }

  /**
   * 声明本 op 需要的能力，`run` 里的 `ctx` 只暴露这些成员。
   *
   * 省略时 `ctx` 是完整能力集合（运行期仍只挂载实际授予的部分）。
   */
  public capabilities<const C extends readonly Capability[]>(caps: C): this & { readonly [CapKey]: C } {
    return this as unknown as this & { readonly [CapKey]: C };
  }

  /** 超时毫秒 */
  public timeout(ms: number): this {
    if (!Number.isInteger(ms) || ms <= 0) {
      throw new Error("op timeout must be a positive integer");
    }
    this.draft.timeoutMs = ms;
    return this;
  }

  /** 游标分页描述（plugins.md §11.4） */
  public pagination(p: { readonly cursorField: string; readonly limitField: string; readonly defaultLimit?: number; readonly maxLimit?: number }): this {
    this.draft.pagination = {
      cursor_field: p.cursorField,
      limit_field: p.limitField,
      default_limit: p.defaultLimit,
      max_limit: p.maxLimit,
    };
    return this;
  }
}

/** 查询型 op builder */
export class QueryBuilder<I extends z.ZodType = z.ZodType, O extends z.ZodType = z.ZodType> extends BaseOpBuilder<"query", I, O, { query: true }> {
  private constructor(id: string) {
    super(id, "query");
    this.draft.risk = "read";
  }

  public static of(id: string): QueryBuilder {
    return new QueryBuilder(id);
  }

  /** 处理函数：输入已校验，返回值需与 output schema 一致 */
  public run<B extends this>(
    this: B,
    fn: (input: z.output<InputOf<B>>, ctx: PluginContext<CapOf<B>>) => Promise<z.output<OutputOf<B>>> | z.output<OutputOf<B>>,
  ): OpSpec<InputOf<B>, OutputOf<B>> {
    this.draft.run = fn;
    return this.build();
  }

  public build<B extends this>(this: B): OpSpec<InputOf<B>, OutputOf<B>> {
    return finalize(this.draft) as unknown as OpSpec<InputOf<B>, OutputOf<B>>;
  }
}

/** 动作型 op builder */
export class ActionBuilder<I extends z.ZodType = z.ZodType, O extends z.ZodType = z.ZodType> extends BaseOpBuilder<"action", I, O, { action: true }> {
  private constructor(id: string) {
    super(id, "action");
    this.draft.risk = "mutating";
  }

  public static of(id: string): ActionBuilder {
    return new ActionBuilder(id);
  }

  public run<B extends this>(
    this: B,
    fn: (input: z.output<InputOf<B>>, ctx: PluginContext<CapOf<B>>) => Promise<z.output<OutputOf<B>>> | z.output<OutputOf<B>>,
  ): OpSpec<InputOf<B>, OutputOf<B>> {
    this.draft.run = fn;
    return this.build();
  }

  public build<B extends this>(this: B): OpSpec<InputOf<B>, OutputOf<B>> {
    return finalize(this.draft) as unknown as OpSpec<InputOf<B>, OutputOf<B>>;
  }
}

/** 流式 op builder：run 返回分块迭代器 */
export class StreamBuilder<I extends z.ZodType = z.ZodType, O extends z.ZodType = z.ZodType> extends BaseOpBuilder<"stream", I, O, { stream: true }> {
  private constructor(id: string) {
    super(id, "stream");
    this.draft.risk = "read";
    this.draft.output = z.object({}).passthrough();
  }

  public static of(id: string): StreamBuilder {
    return new StreamBuilder(id);
  }

  /** 处理函数返回同步或异步分块迭代器；SDK 负责 stream 帧与 seq */
  public run<B extends this>(
    this: B,
    fn: (input: z.output<InputOf<B>>, ctx: PluginContext<CapOf<B>>) => StreamChunks<z.output<OutputOf<B>>>,
  ): OpSpec<InputOf<B>, OutputOf<B>> {
    this.draft.run = fn;
    return this.build();
  }

  public build<B extends this>(this: B): OpSpec<InputOf<B>, OutputOf<B>> {
    return finalize(this.draft) as unknown as OpSpec<InputOf<B>, OutputOf<B>>;
  }
}

/** 工厂：查询 */
export function query<I extends z.ZodType, O extends z.ZodType = z.ZodType>(id: string): QueryBuilder<I, O> {
  return QueryBuilder.of(id) as QueryBuilder<I, O>;
}

/** 工厂：动作 */
export function action<I extends z.ZodType, O extends z.ZodType = z.ZodType>(id: string): ActionBuilder<I, O> {
  return ActionBuilder.of(id) as ActionBuilder<I, O>;
}

/** 工厂：流 */
export function stream<I extends z.ZodType, O extends z.ZodType = z.ZodType>(id: string): StreamBuilder<I, O> {
  return StreamBuilder.of(id) as StreamBuilder<I, O>;
}

/** 通用 op 工厂：先声明 id，再 `.kind(...)` */
export function op(id: string): OpBuilderEntry {
  return {
    kind: ((kind: OpKind) => {
      switch (kind) {
        case "query":
          return query(id);
        case "action":
          return action(id);
        case "stream":
          return stream(id);
      }
    }) as OpBuilderEntry["kind"],
  };
}

/** 通用 op 入口：只有 `.kind()` */
export interface OpBuilderEntry {
  kind(kind: "query"): QueryBuilder;
  kind(kind: "action"): ActionBuilder;
  kind(kind: "stream"): StreamBuilder;
}

function finalize(draft: Draft): OpSpec {
  if (draft.kind === undefined) {
    throw new Error(`op ${draft.id} has no kind`);
  }
  if (draft.kind === "query" && draft.risk !== undefined && draft.risk !== "read") {
    throw new Error(`op ${draft.id}: query must use risk read`);
  }
  return {
    kind: draft.kind,
    title: draft.title,
    description: draft.description,
    risk: draft.risk,
    display: draft.display,
    input: draft.input,
    output: draft.output,
    timeoutMs: draft.timeoutMs,
    pagination: draft.pagination,
    run: draft.run,
  } as OpSpec;
}

// ---------------------------------------------------------------------------
// 值片段：行过滤管线
// ---------------------------------------------------------------------------

/** 一行控制台文本 */
export interface ConsoleLine {
  readonly text: string;
  readonly server: string;
  readonly at: number;
}

/** 行过滤管线：纯值构造，无 IO */
export class LineFilter {
  private readonly steps: (() => (line: ConsoleLine) => ConsoleLine | ConsoleLine[] | null)[] = [];

  /** 去掉 ANSI 颜色 / 控制序列 */
  public stripAnsi(): this {
    this.steps.push(() => (line) => ({ ...line, text: stripAnsi(line.text) }));
    return this;
  }

  /** 只保留匹配的行 */
  public include(pattern: RegExp): this {
    this.steps.push(() => (line) => (pattern.test(line.text) ? line : null));
    return this;
  }

  /** 丢弃匹配的行 */
  public exclude(pattern: RegExp): this {
    this.steps.push(() => (line) => (pattern.test(line.text) ? null : line));
    return this;
  }

  /** 一行变多行（如把 log 展开为多个聊天段） */
  public flatMap(fn: (line: ConsoleLine) => readonly ConsoleLine[]): this {
    this.steps.push(() => (line) => {
      const out = fn(line);
      return out.length === 0 ? null : [...out];
    });
    return this;
  }

  /** 改写一行 */
  public map(fn: (line: ConsoleLine) => ConsoleLine): this {
    this.steps.push(() => (line) => fn(line));
    return this;
  }

  /** 收尾：得到一个纯函数（`null` 表示丢弃） */
  public build(): (line: ConsoleLine) => readonly ConsoleLine[] {
    const compiled = this.steps.map((make) => make());
    return (line) => {
      let current: readonly ConsoleLine[] = [line];
      for (const step of compiled) {
        const next: ConsoleLine[] = [];
        for (const item of current) {
          const result = step(item);
          if (result === null) {
            continue;
          }
          if (Array.isArray(result)) {
            next.push(...result);
          } else {
            next.push(result);
          }
        }
        current = next;
      }
      return current;
    };
  }
}

/** 工厂：行过滤管线 */
export function lineFilter(): LineFilter {
  return new LineFilter();
}

const ANSI = /\u001b\[[0-9;]*[A-Za-z]|\u001b\][^\u0007]*\u0007/g;

function stripAnsi(text: string): string {
  return text.replace(ANSI, "");
}

// ---------------------------------------------------------------------------
// 值片段：schema 组合
// ---------------------------------------------------------------------------

/** 可选的 title 包装：给 schema 加展示名（→ x-mcsup-label） */
export function labeled<T extends z.ZodType>(schema: T, label: string): T {
  return schema.meta({ label }) as T;
}

/** 标记为密钥输入（→ x-mcsup-secret） */
export function secretInput<T extends z.ZodType>(schema: T): T {
  return schema.meta({ secret: true }) as T;
}

/** 统一分页输入片段：在已有对象 schema 上追加 `cursor` / `limit` */
export function pagedInput(base: z.ZodObject<z.ZodRawShape>): z.ZodType {
  return base.extend({
    cursor: z.number().int().optional(),
    limit: z.number().int().min(1).max(500).optional(),
  });
}