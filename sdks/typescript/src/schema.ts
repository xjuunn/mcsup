/**
 * 字段 DSL：覆盖 `plugins.md` §11.5 允许的 schema 子集。
 *
 * 插件作者只写 `s.*`，SDK 在内部归约为 zod，仍然同时得到：编译期类型、
 * 运行期校验、descriptor 的 JSON Schema（`plugins.md` §14.4）。zod 保留为
 * 逃生口：`SchemaLike` 同时接受字段与 zod schema（sdk-design.md §6.2）。
 *
 * 只提供子集内的构造，不追求完整 zod 表达力；超出子集的写法应直接使用 zod。
 */

import { z } from "./zod.js";

import type { McsupFormat } from "./validate.js";

/**
 * 一个字段。
 *
 * `schema` 是归约后的 zod schema；`__type` 只用于类型推导，运行期不存在，
 * 也永远不会被读取。
 */
export interface Field<T> {
  readonly kind: "field";
  readonly schema: z.ZodType;
  /** 仅用于类型推导 */
  readonly __type?: T;
}

/** SDK 接受的 schema：字段 DSL，或直接写 zod */
export type SchemaLike<T = unknown> = Field<T> | z.ZodType;

/** 从字段或 zod schema 推导输出类型 */
export type InferSchema<S> = S extends Field<infer T> ? T : S extends z.ZodType ? z.output<S> : unknown;

/**
 * 对象形状推导：值可能为 `undefined` 的键变成可选键。
 *
 * `s.optional(...)` 的语义是「键可以缺席」，与 zod `.optional()` 一致；
 * 若只推导值类型，`run` 的返回值会被要求写出 `next: undefined`。
 */
export type ObjectOf<S extends Record<string, SchemaLike>> = {
  [K in keyof S as undefined extends InferSchema<S[K]> ? never : K]: InferSchema<S[K]>;
} & {
  [K in keyof S as undefined extends InferSchema<S[K]> ? K : never]?: Exclude<InferSchema<S[K]>, undefined>;
};

/** 是否为字段 DSL 产物 */
export function isField(value: unknown): value is Field<unknown> {
  return typeof value === "object" && value !== null && (value as { kind?: unknown }).kind === "field";
}

/** 归约为 zod schema；descriptor 生成、运行期校验与配置热更新共用 */
export function toZod(schema: SchemaLike): z.ZodType {
  return isField(schema) ? schema.schema : schema;
}

// ---------------------------------------------------------------------------
// 选项
// ---------------------------------------------------------------------------

/** 所有字段共用的展示语义（`plugins.md` §11.5 的 `x-mcsup-*` 扩展） */
export interface FieldOptions {
  /** 界面标签 */
  readonly label?: string;
  /** 密钥字段：界面遮蔽，日志打码 */
  readonly secret?: boolean;
  /** schema 描述，写入 JSON Schema 的 `description` */
  readonly describe?: string;
  /** 展示格式；标准 JSON Schema 格式请用 `s.datetime` / `s.uuid` / `s.url` */
  readonly format?: McsupFormat;
}

export interface StringOptions extends FieldOptions {
  readonly min?: number;
  readonly max?: number;
  /** 正则约束（字符串形式，便于 JSON 化） */
  readonly pattern?: string;
  readonly default?: string;
}

export interface NumberOptions extends FieldOptions {
  readonly min?: number;
  readonly max?: number;
  readonly default?: number;
}

export interface BooleanOptions extends FieldOptions {
  readonly default?: boolean;
}

export interface ArrayOptions extends FieldOptions {
  readonly minItems?: number;
  readonly maxItems?: number;
}

// ---------------------------------------------------------------------------
// 构造
// ---------------------------------------------------------------------------

function fieldOf<T>(schema: z.ZodType): Field<T> {
  return { kind: "field", schema };
}

function withMeta<S extends z.ZodType>(schema: S, options: FieldOptions | undefined): S {
  if (options === undefined) {
    return schema;
  }
  const meta: Record<string, unknown> = {};
  if (options.label !== undefined) {
    meta["label"] = options.label;
  }
  if (options.secret !== undefined) {
    meta["secret"] = options.secret;
  }
  if (options.format !== undefined) {
    meta["format"] = options.format;
  }
  let out: z.ZodType = Object.keys(meta).length > 0 ? schema.meta(meta) : schema;
  if (options.describe !== undefined) {
    out = out.describe(options.describe);
  }
  return out as S;
}

function withDefault<T>(schema: z.ZodType, value: T | undefined): z.ZodType {
  return value === undefined ? schema : schema.default(value);
}

/** 字符串 */
export function string(options: StringOptions = {}): Field<string> {
  let schema = z.string();
  if (options.min !== undefined) {
    schema = schema.min(options.min);
  }
  if (options.max !== undefined) {
    schema = schema.max(options.max);
  }
  if (options.pattern !== undefined) {
    schema = schema.regex(new RegExp(options.pattern));
  }
  return fieldOf<string>(withDefault(withMeta(schema, options), options.default));
}

/** ISO 8601 时间（JSON Schema `format: date-time`） */
export function datetime(options: FieldOptions & { readonly offset?: boolean } = {}): Field<string> {
  const schema = options.offset === true ? z.string().datetime({ offset: true }) : z.string().datetime();
  return fieldOf<string>(withMeta(schema, options));
}

/** UUID（JSON Schema `format: uuid`） */
export function uuid(options: FieldOptions = {}): Field<string> {
  return fieldOf<string>(withMeta(z.string().uuid(), options));
}

/** URI（JSON Schema `format: uri`） */
export function url(options: FieldOptions = {}): Field<string> {
  return fieldOf<string>(withMeta(z.string().url(), options));
}

/** 数字（浮点） */
export function number(options: NumberOptions = {}): Field<number> {
  let schema = z.number();
  if (options.min !== undefined) {
    schema = schema.min(options.min);
  }
  if (options.max !== undefined) {
    schema = schema.max(options.max);
  }
  return fieldOf<number>(withDefault(withMeta(schema, options), options.default));
}

/** 整数 */
export function integer(options: NumberOptions = {}): Field<number> {
  let schema = z.int();
  if (options.min !== undefined) {
    schema = schema.min(options.min);
  }
  if (options.max !== undefined) {
    schema = schema.max(options.max);
  }
  return fieldOf<number>(withDefault(withMeta(schema, options), options.default));
}

/** 布尔 */
export function boolean(options: BooleanOptions = {}): Field<boolean> {
  return fieldOf<boolean>(withDefault(withMeta(z.boolean(), options), options.default));
}

/** 字符串枚举（`plugins.md` §11.5 只允许字符串或整数枚举） */
export function enumOf<const T extends readonly [string, ...string[]]>(
  values: T,
  options: FieldOptions & { readonly default?: T[number] } = {},
): Field<T[number]> {
  return fieldOf<T[number]>(withDefault(withMeta(z.enum(values), options), options.default));
}

/** 数组；`items` 可以是字段或 zod schema */
export function array<S extends SchemaLike>(items: S, options: ArrayOptions = {}): Field<InferSchema<S>[]> {
  let schema = z.array(toZod(items));
  if (options.minItems !== undefined) {
    schema = schema.min(options.minItems);
  }
  if (options.maxItems !== undefined) {
    schema = schema.max(options.maxItems);
  }
  return fieldOf<InferSchema<S>[]>(withMeta(schema, options));
}

/** 对象；键顺序即界面顺序 */
export function object<S extends Record<string, SchemaLike>>(
  shape: S,
  options: FieldOptions = {},
): Field<ObjectOf<S>> {
  const zodShape: Record<string, z.ZodType> = {};
  for (const [key, value] of Object.entries(shape)) {
    zodShape[key] = toZod(value);
  }
  return fieldOf<ObjectOf<S>>(withMeta(z.object(zodShape), options));
}

/** 可选字段；`undefined` 不进 JSON，descriptor 里表现为非 required */
export function optional<S extends SchemaLike>(items: S): Field<InferSchema<S> | undefined> {
  return fieldOf<InferSchema<S> | undefined>(toZod(items).optional());
}

/**
 * 字段 DSL 入口。
 *
 * `enum` 是保留字，只能作为属性名出现，因此这里用对象而不是具名导出。
 */
export const s = {
  string,
  datetime,
  uuid,
  url,
  number,
  integer,
  boolean,
  enum: enumOf,
  array,
  object,
  optional,
} as const;

/**
 * 给任意 schema 追加 `.meta()` 展示键，保持入参形状（字段仍是字段）。
 *
 * Builder 的 `labeled` / `secretInput` 用它，避免作者为了加标签而改用 zod。
 */
export function annotate<S extends SchemaLike>(schema: S, meta: Record<string, unknown>): S {
  const zod = toZod(schema).meta(meta);
  return (isField(schema) ? { kind: "field", schema: zod } : zod) as S;
}
