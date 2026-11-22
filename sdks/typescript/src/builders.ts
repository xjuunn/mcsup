/**
 * Builder 层：造「值」片段（sdk-design.md §4.2）
 *
 * Builder 是纯值构造器：无环境状态、无 IO，产物是内核可接受的 op 片段。
 * 终点不是插件，`.build()` 回来的片段要回填进 `definePlugin({...})`。
 */

import { z } from "./zod.js";

import type { Capability, OpKind, OpRisk, OpSpec, PluginContext, StreamChunks } from "./define.js";
import { annotate, toZod, type Field, type InferSchema, type SchemaLike } from "./schema.js";

/** op 种类与风险等级由内核定义，Builder 只做复用 */
export type { OpKind, OpRisk };

const emptyObject = (): z.ZodType => z.object({}).strict();

interface Draft {
  id: string;
  kind: OpKind;
  title?: string;
  description?: string;
  risk?: OpRisk;
  display?: unknown;
  input: SchemaLike;
  output: SchemaLike;
  timeoutMs?: number;
  pagination?: unknown;
  run?: unknown;
}

/** builder 上暂存 schema / 能力声明的键（供 run 推导参数类型） */
const InputKey: unique symbol = Symbol("input");
const OutputKey: unique symbol = Symbol("output");
const CapKey: unique symbol = Symbol("capabilities");

type WithInput<B, S extends SchemaLike> = B & { readonly [InputKey]: S };
type WithOutput<B, S extends SchemaLike> = B & { readonly [OutputKey]: S };
type InputOf<B> = B extends { readonly [InputKey]: infer S extends SchemaLike } ? S : SchemaLike;
type OutputOf<B> = B extends { readonly [OutputKey]: infer S extends SchemaLike } ? S : SchemaLike;
type CapOf<B> = B extends { readonly [CapKey]: infer C extends readonly Capability[] } ? C : readonly Capability[];

abstract class BaseOpBuilder<K extends OpKind> {
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

  /** 风险等级；缺省由 descriptor 按 kind 决定（query/stream → read，action → mutating） */
  public risk(risk: OpRisk): this {
    this.draft.risk = risk;
    return this;
  }

  /** 界面提示（plugins.md §11.3）；缺省 `detail`（stream 为 `log`） */
  public display(display: unknown): this {
    this.draft.display = display;
    return this;
  }

  /** 输入 schema（字段 DSL 或 zod） */
  public input<S extends SchemaLike>(schema: S): this & { readonly [InputKey]: S } {
    this.draft.input = schema;
    return this as unknown as this & { readonly [InputKey]: S };
  }

  /** 输出 schema（字段 DSL 或 zod） */
  public output<S extends SchemaLike>(schema: S): this & { readonly [OutputKey]: S } {
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

  /** 游标分页描述（plugins.md §11.4）；宿主要求四个字段齐全 */
  public pagination(p: {
    readonly cursorField: string;
    readonly limitField: string;
    readonly defaultLimit?: number;
    readonly maxLimit?: number;
  }): this {
    const defaultLimit = p.defaultLimit ?? 50;
    const maxLimit = p.maxLimit ?? 200;
    if (!Number.isInteger(defaultLimit) || defaultLimit <= 0) {
      throw new Error("op pagination defaultLimit must be a positive integer");
    }
    if (!Number.isInteger(maxLimit) || maxLimit <= 0 || maxLimit > 1_000) {
      throw new Error("op pagination maxLimit must be an integer in 1..=1000");
    }
    if (defaultLimit > maxLimit) {
      throw new Error("op pagination defaultLimit must not exceed maxLimit");
    }
    this.draft.pagination = {
      cursor_field: p.cursorField,
      limit_field: p.limitField,
      default_limit: defaultLimit,
      max_limit: maxLimit,
    };
    return this;
  }
}

/** 查询型 op builder */
export class QueryBuilder extends BaseOpBuilder<"query"> {
  private constructor(id: string) {
    super(id, "query");
  }

  public static of(id: string): QueryBuilder {
    return new QueryBuilder(id);
  }

  /** 处理函数：输入已校验，返回值需与 output schema 一致 */
  public run<B extends this>(
    this: B,
    fn: (input: InferSchema<InputOf<B>>, ctx: PluginContext<CapOf<B>>) => Promise<InferSchema<OutputOf<B>>> | InferSchema<OutputOf<B>>,
  ): OpSpec<InputOf<B>, OutputOf<B>> {
    this.draft.run = fn;
    return this.build();
  }

  public build<B extends this>(this: B): OpSpec<InputOf<B>, OutputOf<B>> {
    return finalize(this.draft) as unknown as OpSpec<InputOf<B>, OutputOf<B>>;
  }
}

/** 动作型 op builder */
export class ActionBuilder extends BaseOpBuilder<"action"> {
  private constructor(id: string) {
    super(id, "action");
  }

  public static of(id: string): ActionBuilder {
    return new ActionBuilder(id);
  }

  public run<B extends this>(
    this: B,
    fn: (input: InferSchema<InputOf<B>>, ctx: PluginContext<CapOf<B>>) => Promise<InferSchema<OutputOf<B>>> | InferSchema<OutputOf<B>>,
  ): OpSpec<InputOf<B>, OutputOf<B>> {
    this.draft.run = fn;
    return this.build();
  }

  public build<B extends this>(this: B): OpSpec<InputOf<B>, OutputOf<B>> {
    return finalize(this.draft) as unknown as OpSpec<InputOf<B>, OutputOf<B>>;
  }
}

/** 流式 op builder：run 返回分块迭代器 */
export class StreamBuilder extends BaseOpBuilder<"stream"> {
  private constructor(id: string) {
    super(id, "stream");
  }

  public static of(id: string): StreamBuilder {
    return new StreamBuilder(id);
  }

  /** 处理函数返回同步或异步分块迭代器；SDK 负责 stream 帧与 seq */
  public run<B extends this>(
    this: B,
    fn: (input: InferSchema<InputOf<B>>, ctx: PluginContext<CapOf<B>>) => StreamChunks<InferSchema<OutputOf<B>>>,
  ): OpSpec<InputOf<B>, OutputOf<B>> {
    this.draft.run = fn;
    return this.build();
  }

  public build<B extends this>(this: B): OpSpec<InputOf<B>, OutputOf<B>> {
    return finalize(this.draft) as unknown as OpSpec<InputOf<B>, OutputOf<B>>;
  }
}

/** 工厂：查询 */
export function query(id: string): QueryBuilder {
  return QueryBuilder.of(id);
}

/** 工厂：动作 */
export function action(id: string): ActionBuilder {
  return ActionBuilder.of(id);
}

/** 工厂：流 */
export function stream(id: string): StreamBuilder {
  return StreamBuilder.of(id);
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
  private readonly steps: Array<(line: ConsoleLine) => ConsoleLine | ConsoleLine[] | null> = [];

  /** 去掉 ANSI 颜色 / 控制序列 */
  public stripAnsi(): this {
    this.steps.push((line) => ({ ...line, text: stripAnsi(line.text) }));
    return this;
  }

  /** 只保留匹配的行 */
  public include(pattern: RegExp): this {
    this.steps.push((line) => (pattern.test(line.text) ? line : null));
    return this;
  }

  /** 丢弃匹配的行 */
  public exclude(pattern: RegExp): this {
    this.steps.push((line) => (pattern.test(line.text) ? null : line));
    return this;
  }

  /** 一行变多行（如把 log 展开为多个聊天段） */
  public flatMap(fn: (line: ConsoleLine) => readonly ConsoleLine[]): this {
    this.steps.push((line) => {
      const out = fn(line);
      return out.length === 0 ? null : [...out];
    });
    return this;
  }

  /** 改写一行 */
  public map(fn: (line: ConsoleLine) => ConsoleLine): this {
    this.steps.push(fn);
    return this;
  }

  /** 收尾：得到一个纯函数（`null` 表示丢弃） */
  public build(): (line: ConsoleLine) => readonly ConsoleLine[] {
    const compiled = [...this.steps];
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
export function labeled<S extends SchemaLike>(schema: S, label: string): S {
  return annotate(schema, { label });
}

/** 标记为密钥输入（→ x-mcsup-secret） */
export function secretInput<S extends SchemaLike>(schema: S): S {
  return annotate(schema, { secret: true });
}

/** 统一分页输入片段：在已有对象 schema 上追加 `cursor` / `limit` */
export function pagedInput<S extends SchemaLike>(
  base: S,
): Field<InferSchema<S> & { readonly cursor?: number; readonly limit?: number }> {
  const zodBase = toZod(base);
  if (!(zodBase instanceof z.ZodObject)) {
    throw new Error("pagedInput requires an object schema");
  }
  return {
    kind: "field",
    schema: zodBase.extend({
      cursor: z.number().int().optional(),
      limit: z.number().int().min(1).max(500).optional(),
    }),
  };
}
