/**
 * descriptor 校验规则（plugins.md §11.2 / §11.3 / §11.4 / §11.5 / §11.7 第 5 条）
 *
 * 校验分两层：
 *   - 结构层：op 字段约束、display 形状与引用、默认推断；
 *   - schema 层：子集白名单、禁止构造、限值。
 *
 * 所有错误都带字段路径（如 `ops.history.output.properties.items`），
 * 便于插件作者定位到具体的 schema 位置。
 */

import { z } from "zod";

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/** descriptor 构建失败：`path` 指向出问题的位置 */
export class DescriptorError extends Error {
  /** 字段路径，如 `ops.history.output` */
  public readonly path: string;

  public constructor(path: string, message: string) {
    super(`${path}: ${message}`);
    this.name = "DescriptorError";
    this.path = path;
  }
}

function fail(path: string, message: string): never {
  throw new DescriptorError(path, message);
}

// ---------------------------------------------------------------------------
// 限值（§11.5）
// ---------------------------------------------------------------------------

/** schema 深度上限 */
export const MAX_SCHEMA_DEPTH = 6;
/** 单对象属性数上限 */
export const MAX_OBJECT_PROPERTIES = 64;
/** `enum` 取值数上限 */
export const MAX_ENUM_VALUES = 100;
/** schema 节点总数上限 */
export const MAX_SCHEMA_NODES = 512;
/** 单字符串 `maxLength` 上限 */
export const MAX_STRING_LENGTH = 8192;
/** 单个 schema 序列化字节上限 */
export const MAX_SCHEMA_BYTES = 64 * 1024;
/** 整个 descriptor 字节上限 */
export const MAX_DESCRIPTOR_BYTES = 256 * 1024;

// ---------------------------------------------------------------------------
// 结构层：op 字段（§11.2）
// ---------------------------------------------------------------------------

/** op id：`^[a-z][a-z0-9_]{0,63}$` */
export const OP_ID_PATTERN = /^[a-z][a-z0-9_]{0,63}$/;

/** `title` 字符上限 */
export const MAX_OP_TITLE = 64;
/** `description` 字符上限 */
export const MAX_OP_DESCRIPTION = 512;
/** `timeout_ms` 下界 */
export const MIN_OP_TIMEOUT_MS = 1000;
/** `timeout_ms` 上界 */
export const MAX_OP_TIMEOUT_MS = 300_000;
/** `timeout_ms` 缺省 */
export const DEFAULT_OP_TIMEOUT_MS = 30_000;

const OP_KINDS = new Set(["query", "action", "stream"]);
const OP_RISKS = new Set(["read", "mutating", "destructive"]);

/** 校验单个 op 的字段约束；`path` 形如 `ops.<id>` */
export function validateOpFields(
  path: string,
  op: { readonly id: string; readonly kind: string; readonly title: string; readonly description?: string; readonly risk: string; readonly timeoutMs?: number },
): void {
  if (!OP_ID_PATTERN.test(op.id)) {
    fail(`${path}.id`, `${op.id} must match ${OP_ID_PATTERN.source}`);
  }
  if (!OP_KINDS.has(op.kind)) {
    fail(`${path}.kind`, `must be one of ${[...OP_KINDS].join(", ")}`);
  }
  if (op.title.trim() === "") {
    fail(`${path}.title`, "is required");
  }
  if ([...op.title].length > MAX_OP_TITLE) {
    fail(`${path}.title`, `is ${[...op.title].length} chars, limit is ${MAX_OP_TITLE}`);
  }
  if (op.description !== undefined && [...op.description].length > MAX_OP_DESCRIPTION) {
    fail(`${path}.description`, `is ${[...op.description].length} chars, limit is ${MAX_OP_DESCRIPTION}`);
  }
  if (!OP_RISKS.has(op.risk)) {
    fail(`${path}.risk`, `must be one of ${[...OP_RISKS].join(", ")}`);
  }
  if (op.timeoutMs !== undefined) {
    if (!Number.isInteger(op.timeoutMs) || op.timeoutMs < MIN_OP_TIMEOUT_MS || op.timeoutMs > MAX_OP_TIMEOUT_MS) {
      fail(
        `${path}.timeout_ms`,
        `is ${op.timeoutMs}, must be an integer in ${MIN_OP_TIMEOUT_MS}..=${MAX_OP_TIMEOUT_MS}`,
      );
    }
  }
}

// ---------------------------------------------------------------------------
// 结构层：display（§11.3）
// ---------------------------------------------------------------------------

/** 六种展示类型 */
export const DISPLAY_TYPES = ["form", "table", "detail", "chat", "log", "metrics"] as const;

/** 展示类型 */
export type DisplayType = (typeof DISPLAY_TYPES)[number];

/** 各展示类型允许的额外字段 */
const DISPLAY_FIELDS: Record<DisplayType, readonly string[]> = {
  form: [],
  table: ["columns"],
  detail: ["fields"],
  chat: ["send", "stream", "text_field", "from_field", "at_field"],
  log: ["text_field", "level_field", "at_field"],
  metrics: ["value_field", "at_field", "unit"],
};

/** 字段名缺省值（§11.3） */
export const DISPLAY_FIELD_DEFAULTS = {
  text_field: "text",
  from_field: "from",
  at_field: "at",
  level_field: "level",
  value_field: "value",
} as const;

/** 校验 display 对象本身的形状；`path` 形如 `ops.<id>.display` */
export function validateDisplayObject(path: string, display: unknown): DisplayType | undefined {
  if (display === undefined) {
    return undefined;
  }
  if (typeof display !== "object" || display === null || Array.isArray(display)) {
    fail(path, "must be an object");
  }
  const record = display as Record<string, unknown>;
  const type = record["type"];
  if (typeof type !== "string" || !(DISPLAY_TYPES as readonly string[]).includes(type)) {
    fail(`${path}.type`, `must be one of ${DISPLAY_TYPES.join(", ")}`);
  }
  const allowed = DISPLAY_FIELDS[type as DisplayType];
  for (const key of Object.keys(record)) {
    if (key !== "type" && !allowed.includes(key)) {
      fail(`${path}.${key}`, `is not allowed for display type ${type}`);
    }
  }
  for (const key of ["send", "stream", "text_field", "from_field", "at_field", "level_field", "value_field", "unit"]) {
    const value = record[key];
    if (value !== undefined && typeof value !== "string") {
      fail(`${path}.${key}`, "must be a string");
    }
  }
  return type as DisplayType;
}

// ---------------------------------------------------------------------------
// 结构层：默认推断（§11.4）
// ---------------------------------------------------------------------------

/** 推断上下文：只看 output 的顶层形状 */
export interface ShapeHint {
  /** 顶层是 object */
  readonly isObject: boolean;
  /** 顶层 object 的 `items` 是 array */
  readonly hasItemsArray: boolean;
}

/** 从 schema 形状取推断所需的两个信号 */
export function shapeHint(shape: { readonly type: string | undefined; readonly hasItemsArray: boolean }): ShapeHint {
  return { isObject: shape.type === "object", hasItemsArray: shape.hasItemsArray };
}

/**
 * 按 §11.4 推断 display。
 *
 * - `action` → `form`
 * - `query` 输出含 `items` 数组 → `table`
 * - `query` 输出是 object → `detail`
 * - `stream` → `log`
 * - 其余必须显式声明，否则失败
 */
export function inferDisplay(path: string, kind: string, hint: ShapeHint): DisplayType {
  if (kind === "action") {
    return "form";
  }
  if (kind === "query") {
    if (!hint.isObject) {
      fail(`${path}.display`, "cannot infer display for a non-object query output; declare display explicitly");
    }
    return hint.hasItemsArray ? "table" : "detail";
  }
  if (kind === "stream") {
    return "log";
  }
  return fail(`${path}.display`, "cannot infer display; declare it explicitly");
}

// ---------------------------------------------------------------------------
// 结构层：display 输出形状与引用（§11.3）
// ---------------------------------------------------------------------------

/** schema 的形状摘要：够判断 display 要求即可 */
export interface SchemaShape {
  readonly type: string | undefined;
  readonly required: readonly string[];
  readonly properties: Readonly<Record<string, SchemaShape>>;
  /** `array.items` */
  readonly items?: SchemaShape;
  readonly hasItemsArray: boolean;
}

function requireField(path: string, shape: SchemaShape, field: string): SchemaShape {
  const child = shape.properties[field];
  if (child === undefined) {
    return fail(`${path}.properties.${field}`, "is required by the display type");
  }
  if (!shape.required.includes(field)) {
    return fail(`${path}.required`, `must include ${field} for this display type`);
  }
  return child;
}

function requireObject(path: string, shape: SchemaShape): void {
  if (shape.type !== "object") {
    fail(path, "must be an object for this display type");
  }
}

function requireArrayItems(path: string, shape: SchemaShape, field: string): SchemaShape {
  const container = requireField(path, shape, field);
  if (container.type !== "array" || container.items === undefined) {
    fail(`${path}.properties.${field}`, "must be an array of object");
  }
  requireObject(`${path}.properties.${field}.items`, container.items);
  return container.items as SchemaShape;
}

function requireStringChild(path: string, parent: SchemaShape, field: string): void {
  const child = parent.properties[field];
  if (child === undefined) {
    fail(`${path}.properties.${field}`, "is required by the display type");
  }
  if (child.type !== "string") {
    fail(`${path}.properties.${field}`, "must be a string");
  }
}

/**
 * 校验 display 与 output 形状是否匹配。
 *
 * @param path `ops.<id>.output`
 * @param type display 类型
 * @param output 输出 schema 形状
 * @param stream 是否为 stream op（`log` / `metrics` 允许单条 chunk 形状）
 */
export function validateDisplayShape(path: string, type: DisplayType, output: SchemaShape, stream: boolean): void {
  switch (type) {
    case "table":
      requireArrayItems(path, output, "items");
      return;
    case "chat": {
      const item = requireArrayItems(path, output, "items");
      for (const field of ["id", "at", "from", "text"]) {
        requireStringChild(`${path}.properties.items.items`, item, field);
      }
      return;
    }
    case "log": {
      if (stream) {
        requireObject(path, output);
        requireStringChild(path, output, "at");
        requireStringChild(path, output, "text");
        return;
      }
      const line = requireArrayItems(path, output, "lines");
      requireStringChild(`${path}.properties.lines.items`, line, "at");
      requireStringChild(`${path}.properties.lines.items`, line, "text");
      return;
    }
    case "metrics": {
      if (stream) {
        requireObject(path, output);
        requireStringChild(path, output, "at");
        requireStringChild(path, output, "value");
        return;
      }
      const point = requireArrayItems(path, output, "points");
      requireStringChild(`${path}.properties.points.items`, point, "at");
      requireStringChild(`${path}.properties.points.items`, point, "value");
      return;
    }
    case "detail":
      requireObject(path, output);
      return;
    case "form":
      // 输出可为空 object
      requireObject(path, output);
      return;
  }
}

/**
 * 校验 `chat.send` / `chat.stream` 指向已声明的 op，且 kind 正确。
 *
 * @param path `ops.<id>.display`
 * @param display display 对象
 * @param kinds 已声明 op 的 id → kind
 */
export function validateDisplayRefs(path: string, display: unknown, kinds: ReadonlyMap<string, string>): void {
  if (typeof display !== "object" || display === null) {
    return;
  }
  const record = display as Record<string, unknown>;
  if (record["type"] !== "chat") {
    return;
  }
  const send = record["send"];
  const stream = record["stream"];
  if (typeof send === "string") {
    const kind = kinds.get(send);
    if (kind === undefined) {
      fail(`${path}.send`, `refers to undeclared op ${send}`);
    }
    if (kind !== "action") {
      fail(`${path}.send`, `must refer to an action op, but ${send} is ${kind}`);
    }
  }
  if (typeof stream === "string") {
    const kind = kinds.get(stream);
    if (kind === undefined) {
      fail(`${path}.stream`, `refers to undeclared op ${stream}`);
    }
    if (kind !== "stream") {
      fail(`${path}.stream`, `must refer to a stream op, but ${stream} is ${kind}`);
    }
  }
}

// ---------------------------------------------------------------------------
// schema 层：从归一化 JSON Schema 提取形状
// ---------------------------------------------------------------------------

/** 从归一化 JSON Schema 读取形状摘要（只关心顶层与一层子字段） */
export function schemaShape(schema: unknown): SchemaShape {
  if (typeof schema !== "object" || schema === null || Array.isArray(schema)) {
    return { type: undefined, required: [], properties: {}, hasItemsArray: false };
  }
  const record = schema as Record<string, unknown>;
  const type = typeof record["type"] === "string" ? record["type"] : undefined;
  const required = Array.isArray(record["required"]) ? (record["required"] as string[]) : [];
  const rawProperties = record["properties"];
  const properties: Record<string, SchemaShape> = {};
  if (typeof rawProperties === "object" && rawProperties !== null && !Array.isArray(rawProperties)) {
    for (const [key, value] of Object.entries(rawProperties as Record<string, unknown>)) {
      properties[key] = schemaShape(value);
    }
  }
  const rawItems = record["items"];
  const items = rawItems === undefined || repeatedTupleMarker(rawItems) ? undefined : schemaShape(rawItems);
  const itemsProp = properties["items"];
  const hasItemsArray = itemsProp !== undefined && itemsProp.type === "array";
  return { type, required, properties, items, hasItemsArray };
}

function repeatedTupleMarker(value: unknown): boolean {
  return value === false;
}

// ---------------------------------------------------------------------------
// schema 层：子集与限值（§11.5 / §11.7 第 5 条）
// ---------------------------------------------------------------------------

/** 禁止出现的键：组合、条件、元组、模式属性 */
const FORBIDDEN_KEYS: Readonly<Record<string, string>> = {
  oneOf: "union (oneOf) is not allowed; use a flat object with optional fields",
  anyOf: "union (anyOf) is not allowed; use a flat object with optional fields",
  allOf: "allOf is not allowed; flatten the object",
  if: "conditional schema (if/then/else) is not allowed",
  then: "conditional schema (if/then/else) is not allowed",
  else: "conditional schema (if/then/else) is not allowed",
  not: "`not` is not allowed",
  patternProperties: "patternProperties is not allowed; use a flat object",
  prefixItems: "tuples (prefixItems) are not allowed; use an array with a single item schema",
  contains: "`contains` is not allowed",
  unevaluatedProperties: "unevaluatedProperties is not allowed",
  unevaluatedItems: "unevaluatedItems is not allowed",
  dependentRequired: "dependentRequired is not allowed",
  dependentSchemas: "dependentSchemas is not allowed",
  propertyNames: "propertyNames is not allowed; use a flat object",
  $defs: "normalized schemas must not contain $defs",
  definitions: "normalized schemas must not contain definitions",
};

/** 允许出现的键（其余一律拒绝，防止语言特有的扩展悄悄流入） */
const ALLOWED_KEYS = new Set([
  "type",
  "properties",
  "required",
  "additionalProperties",
  "items",
  "enum",
  "const",
  "default",
  "minimum",
  "maximum",
  "exclusiveMinimum",
  "exclusiveMaximum",
  "minLength",
  "maxLength",
  "pattern",
  "minItems",
  "maxItems",
  "format",
  "title",
  "description",
  "examples",
  "x-mcsup-secret",
  "x-mcsup-label",
  "x-mcsup-format",
  "x-mcsup-unit",
  "x-mcsup-placeholder",
]);

/** 允许的 `type` 取值 */
const ALLOWED_TYPES = new Set(["string", "number", "integer", "boolean", "object", "array"]);

/** 允许的 `format` 取值 */
const ALLOWED_FORMATS = new Set(["date-time", "duration", "uri", "uuid"]);

/** 子 schema 的键与路径片段（决定路径如何拼接） */
function childEntries(record: Record<string, unknown>): readonly [string, unknown][] {
  const out: [string, unknown][] = [];
  const props = record["properties"];
  if (typeof props === "object" && props !== null && !Array.isArray(props)) {
    for (const [key, value] of Object.entries(props as Record<string, unknown>)) {
      out.push([`properties.${key}`, value]);
    }
  }
  if (record["items"] !== undefined) {
    out.push(["items", record["items"]]);
  }
  if (record["additionalProperties"] !== undefined && record["additionalProperties"] !== false) {
    out.push(["additionalProperties", record["additionalProperties"]]);
  }
  return out;
}

interface SchemaStats {
  nodes: number;
}

/**
 * 校验一个归一化 schema 是否落在 §11.5 子集内，并检查限值。
 *
 * @param path 字段路径，如 `ops.history.output`
 * @param schema 归一化 JSON Schema
 *
 * @throws {DescriptorError} 出界构造或超限，`path` 指到具体字段
 */
export function validateSchemaSubset(path: string, schema: unknown): void {
  const stats: SchemaStats = { nodes: 0 };
  walk(path, schema, 1, stats);
  const bytes = Buffer.byteLength(JSON.stringify(schema), "utf8");
  if (bytes > MAX_SCHEMA_BYTES) {
    fail(path, `serialized schema is ${bytes} bytes, limit is ${MAX_SCHEMA_BYTES}`);
  }
}

function walk(path: string, schema: unknown, depth: number, stats: SchemaStats): void {
  if (schema === false || schema === true) {
    // `items: false` 是元组收尾；元组本身已在 prefixItems 处拒绝过
    if (schema === true) {
      fail(path, "boolean schema `true` is not allowed; declare the shape explicitly");
    }
    return;
  }
  if (typeof schema !== "object" || schema === null || Array.isArray(schema)) {
    return fail(path, "must be a schema object");
  }
  const record = schema as Record<string, unknown>;
  stats.nodes += 1;
  if (stats.nodes > MAX_SCHEMA_NODES) {
    fail(path, `schema has more than ${MAX_SCHEMA_NODES} nodes`);
  }
  if (depth > MAX_SCHEMA_DEPTH) {
    fail(path, `schema depth exceeds ${MAX_SCHEMA_DEPTH}`);
  }
  for (const [key, reason] of Object.entries(FORBIDDEN_KEYS)) {
    if (record[key] !== undefined) {
      fail(`${path}.${key}`, reason);
    }
  }
  for (const key of Object.keys(record)) {
    if (!ALLOWED_KEYS.has(key) && !Object.prototype.hasOwnProperty.call(FORBIDDEN_KEYS, key)) {
      fail(`${path}.${key}`, "is not part of the supported schema subset");
    }
  }
  const type = record["type"];
  if (type !== undefined) {
    if (typeof type !== "string") {
      fail(`${path}.type`, "nullable fields must be optional instead of a null type");
    }
    if (type === "null") {
      fail(`${path}.type`, "`type: null` is not allowed; make the field optional");
    }
    if (!ALLOWED_TYPES.has(type)) {
      fail(`${path}.type`, `${type} is not supported`);
    }
  }
  const format = record["format"];
  if (format !== undefined && (typeof format !== "string" || !ALLOWED_FORMATS.has(format))) {
    fail(`${path}.format`, `${String(format)} is not one of ${[...ALLOWED_FORMATS].join(", ")}`);
  }
  const maxLength = record["maxLength"];
  if (maxLength !== undefined) {
    if (typeof maxLength !== "number" || !Number.isInteger(maxLength) || maxLength < 0) {
      fail(`${path}.maxLength`, "must be a non-negative integer");
    }
    if (maxLength > MAX_STRING_LENGTH) {
      fail(`${path}.maxLength`, `is ${maxLength}, limit is ${MAX_STRING_LENGTH}`);
    }
  }
  const enumValues = record["enum"];
  if (enumValues !== undefined) {
    if (!Array.isArray(enumValues)) {
      fail(`${path}.enum`, "must be an array");
    }
    if ((enumValues as unknown[]).length > MAX_ENUM_VALUES) {
      fail(`${path}.enum`, `has ${(enumValues as unknown[]).length} values, limit is ${MAX_ENUM_VALUES}`);
    }
    for (const value of enumValues as unknown[]) {
      if (typeof value !== "string" && typeof value !== "number") {
        fail(`${path}.enum`, "values must be strings or integers");
      }
    }
  }
  const props = record["properties"];
  if (props !== undefined) {
    if (typeof props !== "object" || props === null || Array.isArray(props)) {
      fail(`${path}.properties`, "must be an object");
    }
    const count = Object.keys(props as Record<string, unknown>).length;
    if (count > MAX_OBJECT_PROPERTIES) {
      fail(`${path}.properties`, `has ${count} properties, limit is ${MAX_OBJECT_PROPERTIES}`);
    }
    if (type === undefined) {
      fail(path, "an object with properties must declare type: object");
    }
  }
  if (type === "object" && record["additionalProperties"] !== false) {
    fail(`${path}.additionalProperties`, "must be false on object schemas");
  }
  if (type === "array" && record["items"] === undefined) {
    fail(`${path}.items`, "array schemas must declare a single item schema");
  }
  for (const [key, value] of childEntries(record)) {
    if (key === "items" && value === false) {
      // 元组收尾标记，由 prefixItems 检查负责报错
      continue;
    }
    walk(`${path}.${key}`, value, depth + 1, stats);
  }
}

/**
 * 校验整个 descriptor 的字节数（§11.5 第 7 条）。
 *
 * @param path 出问题时的字段路径
 * @param text 序列化后的紧凑 JSON
 */
export function validateDescriptorSize(path: string, text: string): void {
  const bytes = Buffer.byteLength(text, "utf8");
  if (bytes > MAX_DESCRIPTOR_BYTES) {
    fail(path, `descriptor is ${bytes} bytes, limit is ${MAX_DESCRIPTOR_BYTES}`);
  }
}

// ---------------------------------------------------------------------------
// zod 层：禁止 transform / refine / union / 递归
// ---------------------------------------------------------------------------

interface ZodCheck {
  readonly def?: { readonly type?: string };
  readonly _zod?: { readonly def?: { readonly check?: string } };
}

interface ZodDef {
  readonly type?: string;
  readonly checks?: readonly ZodCheck[];
  readonly shape?: Record<string, z.ZodType>;
  readonly element?: z.ZodType;
  readonly innerType?: z.ZodType;
  readonly in?: z.ZodType;
  readonly out?: z.ZodType;
  readonly getter?: () => z.ZodType;
}

const TRANSPARENT_ZOD = new Set(["optional", "default", "prefault", "nullable", "readonly", "catch", "nonoptional"]);

/**
 * 在生成 JSON Schema 之前检查 zod 定义本身。
 *
 * 这些构造无法用 JSON Schema 表达，且 zod 会静默丢弃其语义，因此必须在
 * 源头拒绝（§11.5 禁止表最后两行）。
 *
 * @param path 字段路径
 * @param schema zod schema
 */
export function validateZodSchema(path: string, schema: z.ZodType): void {
  const seen = new Set<z.ZodType>();
  visitZod(path, schema, seen);
}

function visitZod(path: string, schema: z.ZodType, seen: Set<z.ZodType>): void {
  const def = (schema as unknown as { _zod?: { def?: ZodDef } })._zod?.def;
  if (def === undefined) {
    return;
  }
  const type = def.type;
  if (type === undefined) {
    return;
  }
  switch (type) {
    case "pipe":
      return fail(path, "transform / pipe is not allowed; do the conversion in the handler");
    case "union":
    case "intersection":
    case "discriminated_union":
      return fail(path, "union is not allowed; use a flat object with optional fields");
    case "lazy":
      return fail(path, "recursive schemas are not allowed");
    case "tuple":
      return fail(path, "tuples are not allowed; use an array with a single item schema");
    case "record":
    case "map":
    case "set":
      return fail(path, "open-ended maps are not allowed; use a flat object");
    case "bigint":
    case "date":
    case "symbol":
    case "undefined":
    case "void":
    case "any":
    case "unknown":
    case "never":
    case "nan":
      return fail(path, `${type} is not part of the supported schema subset`);
    default:
      break;
  }
  if (TRANSPARENT_ZOD.has(type)) {
    if (def.innerType !== undefined) {
      visitZod(path, def.innerType, seen);
    }
    return;
  }
  if (seen.has(schema)) {
    return fail(path, "recursive schemas are not allowed");
  }
  seen.add(schema);
  for (const check of def.checks ?? []) {
    const kind = check.def?.type ?? (check as unknown as { _zod?: { def?: { check?: string } } })._zod?.def?.check;
    if (kind === "custom") {
      return fail(path, ".refine() / .superRefine() is not allowed; validate inside the handler");
    }
  }
  if (def.shape !== undefined) {
    for (const [key, child] of Object.entries(def.shape)) {
      visitZod(`${path}.${key}`, child, seen);
    }
  }
  if (def.element !== undefined) {
    visitZod(`${path}.$items`, def.element, seen);
  }
}
