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

import {
  DEFAULT_OP_TIMEOUT_MS,
  MAX_DESCRIPTOR_BYTES,
  MAX_ENUM_VALUES,
  MAX_OP_DESCRIPTION_CHARS,
  MAX_OP_TIMEOUT_MS,
  MAX_OP_TITLE_CHARS,
  MAX_SCHEMA_BYTES,
  MAX_SCHEMA_DEPTH,
  MAX_SCHEMA_NODES,
  MAX_SCHEMA_PROPERTIES,
  MAX_STRING_LENGTH,
  MIN_OP_TIMEOUT_MS,
} from "./protocol.js";
import { z } from "./zod.js";

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
// 限值（§11.2 / §11.5）
// ---------------------------------------------------------------------------
//
// 数字全部来自 `./contracts/v1.js`（由 `protocol::plugin::contracts` 生成，
// 源头是 `domain::plugin::descriptor` 的常量）。这里不再手抄第二份：宿主改
// 一个限值时 SDK 跟着变，不会出现「构建期通过、安装期被拒」的错位报错。
//
// 对外仍然从本模块导出，调用方（descriptor / runtime / 测试）不必关心它
// 实际生成在哪。

export {
  DEFAULT_OP_TIMEOUT_MS,
  MAX_DESCRIPTOR_BYTES,
  MAX_ENUM_VALUES,
  MAX_OP_DESCRIPTION_CHARS,
  MAX_OP_TIMEOUT_MS,
  MAX_OP_TITLE_CHARS,
  MAX_SCHEMA_BYTES,
  MAX_SCHEMA_DEPTH,
  MAX_SCHEMA_NODES,
  MAX_SCHEMA_PROPERTIES,
  MAX_STRING_LENGTH,
  MIN_OP_TIMEOUT_MS,
};

// ---------------------------------------------------------------------------
// 结构层：op 字段（§11.2）
// ---------------------------------------------------------------------------

/** op id：`^[a-z][a-z0-9_]{0,63}$` */
export const OP_ID_PATTERN = /^[a-z][a-z0-9_]{0,63}$/;

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
  if ([...op.title].length > MAX_OP_TITLE_CHARS) {
    fail(`${path}.title`, `is ${[...op.title].length} chars, limit is ${MAX_OP_TITLE_CHARS}`);
  }
  if (op.description !== undefined && [...op.description].length > MAX_OP_DESCRIPTION_CHARS) {
    fail(`${path}.description`, `is ${[...op.description].length} chars, limit is ${MAX_OP_DESCRIPTION_CHARS}`);
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

/**
 * 各展示类型允许的额外字段。字段缺省值（`text_field: "text"` 等）按 §11.3
 * 由宿主在 descriptor 未写时取值，SDK 不重复写进产物。
 */
const DISPLAY_FIELDS: Record<DisplayType, readonly string[]> = {
  form: [],
  table: ["columns"],
  detail: ["fields"],
  chat: ["send", "stream", "text_field", "from_field", "at_field"],
  log: ["text_field", "level_field", "at_field"],
  metrics: ["value_field", "at_field", "unit"],
};

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
export function shapeHint(shape: SchemaShape): ShapeHint {
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

/** 自定义展示格式（`x-mcsup-format`，plugins.md §11.6） */
export const MCSup_FORMATS = [
  "plain",
  "minecraft-text",
  "markdown-sanitized",
  "datetime",
  "duration",
  "bytes",
] as const;

/** 自定义展示格式 */
export type McsupFormat = (typeof MCSup_FORMATS)[number];

/**
 * 标准 JSON Schema `format` 取值（plugins.md §11.6）。
 *
 * 与 `x-mcsup-format` 分开：标准格式原样保留在 `format`，展示格式才写进
 * `x-mcsup-format`。宿主只拒绝显式禁止的自定义格式，这里按契约文档拒绝
 * 无法识别的标准值，避免产物在不同语言间漂移。
 */
export const JSON_SCHEMA_FORMATS = [
  "date-time",
  "date",
  "time",
  "duration",
  "email",
  "idn-email",
  "hostname",
  "idn-hostname",
  "ipv4",
  "ipv6",
  "uri",
  "uri-reference",
  "iri",
  "iri-reference",
  "uuid",
  "uri-template",
  "json-pointer",
  "relative-json-pointer",
  "regex",
] as const;

/** 标准 JSON Schema 格式 */
export type JsonSchemaFormat = (typeof JSON_SCHEMA_FORMATS)[number];

/** 禁止出现的键：与宿主 `validate_node` 的拒绝表保持一致 */
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
  $ref: "references must be inlined before validation",
  $defs: "normalized schemas must not contain $defs",
  definitions: "normalized schemas must not contain definitions",
  transform: "transform is not allowed; do the conversion in the handler",
  refine: "refine is not allowed; validate inside the handler",
};

/** 允许的 `type` 取值 */
const ALLOWED_TYPES = new Set(["string", "number", "integer", "boolean", "object", "array"]);

/** 允许的 `format` 取值 */
const ALLOWED_FORMATS = new Set<string>(JSON_SCHEMA_FORMATS);

/** 是否是可识别的标准 JSON Schema 格式 */
export function isJsonSchemaFormat(value: string): value is JsonSchemaFormat {
  return ALLOWED_FORMATS.has(value);
}

/** 是否是可识别的自定义展示格式 */
export function isMcsupFormat(value: string): value is McsupFormat {
  return (MCSup_FORMATS as readonly string[]).includes(value);
}

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
  const displayFormat = record["x-mcsup-format"];
  if (displayFormat !== undefined) {
    if (typeof displayFormat !== "string" || !isMcsupFormat(displayFormat)) {
      fail(
        `${path}.x-mcsup-format`,
        `${String(displayFormat)} is not one of ${MCSup_FORMATS.join(", ")}`,
      );
    }
    if (type !== "string") {
      fail(`${path}.x-mcsup-format`, "requires a string schema");
    }
  }
  const format = record["format"];
  if (format !== undefined && (typeof format !== "string" || !ALLOWED_FORMATS.has(format))) {
    fail(`${path}.format`, `${String(format)} is not one of ${JSON_SCHEMA_FORMATS.join(", ")}`);
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
    if (count > MAX_SCHEMA_PROPERTIES) {
      fail(`${path}.properties`, `has ${count} properties, limit is ${MAX_SCHEMA_PROPERTIES}`);
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
    const kind = check.def?.type ?? check._zod?.def?.check;
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
