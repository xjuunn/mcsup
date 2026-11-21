/**
 * descriptor 与 manifest 生成（plugins.md §10 / §11 / §11.7）
 *
 * 一份 `PluginDefinition` 同时产出 manifest.json 与 descriptor.json：
 * 类型（zod）→ JSON Schema → 归一化 → 产物。多语言 SDK 共用同一套归一化，
 * 保证不同语言的产物可逐字节比对。
 */

import { z } from "./zod.js";

import { PLUGIN_API_VERSION } from "./protocol.js";
import {
  subscriptionKinds,
  type Capability,
  type OpKind,
  type OpRisk,
  type OpSpecLike,
  type PluginDefinition,
  type ResolvedOp,
} from "./define.js";
import { toZod, type SchemaLike } from "./schema.js";
import {
  DEFAULT_OP_TIMEOUT_MS,
  DescriptorError,
  MAX_DESCRIPTOR_BYTES,
  OP_ID_PATTERN,
  inferDisplay,
  schemaShape,
  shapeHint,
  validateDescriptorSize,
  validateDisplayObject,
  validateDisplayRefs,
  validateDisplayShape,
  validateOpFields,
  validateSchemaSubset,
  validateZodSchema,
} from "./validate.js";

// ---------------------------------------------------------------------------
// manifest
// ---------------------------------------------------------------------------

/** manifest 结构（plugins.md §10.1） */
export interface Manifest {
  readonly schema: 1;
  readonly id: string;
  readonly name: string;
  readonly description?: string;
  readonly version: string;
  readonly api: number;
  readonly entry: { readonly type: "process"; readonly cmd: string; readonly platforms?: Record<string, string> };
  readonly scope?: "machine" | "server";
  readonly resident?: boolean;
  readonly capabilities: readonly string[];
  readonly net?: { readonly allow: readonly string[] };
  readonly subscriptions?: readonly string[];
  readonly provides?: readonly string[];
  readonly consumes?: readonly string[];
  readonly publisher?: { readonly name: string; readonly url?: string };
  readonly license: string;
}

/** manifest schema 版本（plugins.md §10.1） */
export const MANIFEST_SCHEMA_VERSION = 1 as const;

const ID_PATTERN = /^[a-z][a-z0-9-]{1,63}$/;

/**
 * 从定义生成 manifest。
 *
 * `entry.cmd` 由打包工具（`mcsup-plugin build`）传入：打包时生成平台启动器并
 * 把实际路径写进 manifest；生成 descriptor 的纯校验阶段传占位值。
 *
 * @throws {Error} id / version / capabilities 不合法
 */
export function buildManifest<
  C extends readonly Capability[],
  Id extends string,
  Cfg extends SchemaLike,
>(def: PluginDefinition<C, Id, Cfg>, entryCmd: string): Manifest {
  if (!ID_PATTERN.test(def.id)) {
    throw new Error(`manifest.id ${def.id} must match ${ID_PATTERN.source}`);
  }
  if (!/^\d+\.\d+\.\d+/.test(def.version)) {
    throw new Error(`manifest.version ${def.version} must be semver`);
  }
  const capabilities = def.capabilities as readonly string[];
  if (capabilities.length > 16) {
    throw new Error("manifest.capabilities must contain at most 16 items");
  }
  if (new Set(capabilities).size !== capabilities.length) {
    throw new Error("manifest.capabilities must be deduplicated");
  }
  const subscriptions = subscriptionKinds(def.subscriptions ?? []);
  if (subscriptions.length > 0 && !capabilities.includes("events")) {
    throw new Error("manifest.subscriptions requires the events capability");
  }
  if (def.license.trim() === "") {
    throw new Error("manifest.license is required (SPDX identifier)");
  }
  const manifest: Manifest = {
    schema: MANIFEST_SCHEMA_VERSION,
    id: def.id,
    name: def.title,
    version: def.version,
    api: PLUGIN_API_VERSION,
    entry: { type: "process", cmd: entryCmd },
    capabilities,
    license: def.license,
  };
  if (def.description !== undefined) {
    (manifest as { description?: string }).description = def.description;
  }
  if (def.scope !== undefined) {
    (manifest as { scope?: string }).scope = def.scope;
  }
  if (def.resident !== undefined) {
    (manifest as { resident?: boolean }).resident = def.resident;
  }
  if (def.net?.allow !== undefined && def.net.allow.length > 0) {
    if (!capabilities.includes("net:outbound")) {
      throw new Error("manifest.net.allow requires the net:outbound capability");
    }
    (manifest as { net?: { allow: readonly string[] } }).net = { allow: def.net.allow };
  }
  if (subscriptions.length > 0) {
    (manifest as { subscriptions?: readonly string[] }).subscriptions = subscriptions;
  }
  if (def.provides !== undefined && def.provides.length > 0) {
    (manifest as { provides?: readonly string[] }).provides = def.provides;
  }
  if (def.consumes !== undefined && def.consumes.length > 0) {
    (manifest as { consumes?: readonly string[] }).consumes = def.consumes;
  }
  if (def.publisher !== undefined) {
    (manifest as { publisher?: Manifest["publisher"] }).publisher = def.publisher;
  }
  return manifest;
}

// ---------------------------------------------------------------------------
// JSON Schema 归一化（§11.7）
// ---------------------------------------------------------------------------

/** zod `.meta()` 的便捷键 → descriptor 扩展键 */
const META_KEY_MAP: Record<string, string> = {
  secret: "x-mcsup-secret",
  label: "x-mcsup-label",
  unit: "x-mcsup-unit",
  placeholder: "x-mcsup-placeholder",
};

const FORMATS = new Set(["plain", "minecraft-text", "markdown-sanitized", "datetime", "duration", "bytes"]);

/**
 * 标准 JSON Schema `format` 取值。
 *
 * zod 的 `.datetime()` / `.uuid()` / `.url()` 会生成标准 `format`，不能把它
 * 当成 `.meta({ format })` 的自定义展示格式；否则合法 schema 会在构建期被
 * `FORMATS` 拒绝。`duration` 同时出现在两边，按自定义展示格式处理，保持
 * `.meta({ format: "duration" })` 的既有语义。
 */
const JSON_SCHEMA_FORMATS = new Set([
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
]);

type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

function isJsonObject(value: JsonValue): value is { [key: string]: JsonValue } {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** 键按字典序排序；数组保持声明顺序 */
function sortKeys(value: JsonValue): JsonValue {
  if (Array.isArray(value)) {
    return value.map(sortKeys);
  }
  if (isJsonObject(value)) {
    const out: { [key: string]: JsonValue } = {};
    for (const key of Object.keys(value).sort()) {
      out[key] = sortKeys(value[key] as JsonValue);
    }
    return out;
  }
  return value;
}

/** 折叠 `anyOf: [X, null]` 为单个 schema（非必填语义由 required 表达） */
function foldNullable(value: JsonValue, path: string): JsonValue {
  if (Array.isArray(value)) {
    return value.map((item) => foldNullable(item, path));
  }
  if (!isJsonObject(value)) {
    return value;
  }
  const out: { [key: string]: JsonValue } = {};
  for (const [key, item] of Object.entries(value)) {
    out[key] = foldNullable(item, key === "properties" ? path : `${path}.${key}`);
  }
  const anyOf = out["anyOf"];
  const typeList = out["type"];
  // zod v4 输出 `type: ["number", "null"]`，同样按可空折叠（§11.7 第 2 条）
  if (Array.isArray(typeList) && typeList.length === 2 && typeList.includes("null")) {
    const { type: _drop, ...rest } = out;
    return { ...rest, type: typeList.find((t) => t !== "null") as JsonValue };
  }
  if (Array.isArray(typeList) && typeList.length > 1) {
    throw new DescriptorError(`${path}.type`, `union of ${typeList.map(String).join(" | ")} is not allowed; use a flat object with optional fields`);
  }
  if (Array.isArray(anyOf) && anyOf.length === 2 && anyOf.some((x) => x === null || x === ("null" as JsonValue) || (isJsonObject(x) && x["type"] === "null"))) {
    const inner = anyOf.find((x) => !(x === null || (isJsonObject(x) && x["type"] === "null")));
    if (inner !== undefined && isJsonObject(inner)) {
      return { ...inner, ...stripNullOnly(out) };
    }
  }
  if (anyOf !== undefined) {
    throw new DescriptorError(`${path}.anyOf`, "union is not allowed; use a flat object with optional fields");
  }
  return out;
}

function stripNullOnly(value: { [key: string]: JsonValue }): { [key: string]: JsonValue } {
  const { anyOf: _anyOf, ...rest } = value;
  return rest;
}

/** 对象 schema 一律闭合（§11.5 要求 `additionalProperties:false`） */
function closeObjects(value: JsonValue): JsonValue {
  if (Array.isArray(value)) {
    return value.map(closeObjects);
  }
  if (!isJsonObject(value)) {
    return value;
  }
  const out: { [key: string]: JsonValue } = {};
  for (const [key, item] of Object.entries(value)) {
    out[key] = closeObjects(item);
  }
  // zod v4 对整数给出 ±MAX_SAFE_INTEGER 的实现边界；这不是作者的数值约束，
  // 各语言统一剥掉，避免产物漂移
  if (out["minimum"] === -Number.MAX_SAFE_INTEGER) {
    delete out["minimum"];
  }
  if (out["maximum"] === Number.MAX_SAFE_INTEGER) {
    delete out["maximum"];
  }
  if (out["type"] === "object" && !Array.isArray(out["properties"]) && out["additionalProperties"] === undefined) {
    out["additionalProperties"] = false;
  }
  return out;
}

/** `.meta()` 便捷键 → `x-mcsup-*`；校验 format 枚举与输出方向禁止 secret */
function applyMetaKeys(value: JsonValue, onOutput: boolean, path: string): JsonValue {
  if (Array.isArray(value)) {
    return value.map((item) => applyMetaKeys(item, onOutput, path));
  }
  if (!isJsonObject(value)) {
    return value;
  }
  const out: { [key: string]: JsonValue } = {};
  for (const [key, item] of Object.entries(value)) {
    if (key === "properties" && isJsonObject(item)) {
      const properties: { [key: string]: JsonValue } = {};
      for (const [name, child] of Object.entries(item)) {
        properties[name] = applyMetaKeys(child, onOutput, `${path}.properties.${name}`);
      }
      out[key] = properties;
      continue;
    }
    if (key === "items" || key === "additionalProperties") {
      out[key] = applyMetaKeys(item, onOutput, `${path}.${key}`);
      continue;
    }
    if (key === "format") {
      if (typeof item !== "string") {
        throw new DescriptorError(`${path}.x-mcsup-format`, "must be a string");
      }
      // 自定义展示格式优先；其余标准 JSON Schema 格式原样保留
      if (FORMATS.has(item)) {
        out["x-mcsup-format"] = item;
        continue;
      }
      if (JSON_SCHEMA_FORMATS.has(item)) {
        out["format"] = item;
        continue;
      }
      throw new DescriptorError(
        `${path}.x-mcsup-format`,
        `must be one of ${[...FORMATS].join(", ")} or a standard JSON Schema format`,
      );
    }
    const mapped = META_KEY_MAP[key];
    if (mapped === undefined) {
      out[key] = applyMetaKeys(item, onOutput, `${path}.${key}`);
      continue;
    }
    if (mapped === "x-mcsup-secret") {
      // 输出方向出现 secret 属于安装期拒绝项（§11.6）；生成阶段直接报错
      if (onOutput) {
        throw new DescriptorError(path, "x-mcsup-secret is not allowed on output schemas");
      }
      out[mapped] = item;
      continue;
    }
    out[mapped] = item;
  }
  return out;
}

/** 递归内联 `$defs` / `$ref`（zod v4 顶层 `.meta({id})` 会产生 `$ref`） */
function inlineRefs(value: JsonValue, defs: Record<string, JsonValue> | undefined, path: string): JsonValue {
  if (Array.isArray(value)) {
    return value.map((item) => inlineRefs(item, defs, path));
  }
  if (!isJsonObject(value)) {
    return value;
  }
  const ref = value["$ref"];
  if (typeof ref === "string") {
    if (!ref.startsWith("#/$defs/") || defs === undefined) {
      // 外部 $ref 无法解析，§11.5 明确禁止
      throw new DescriptorError(`${path}.$ref`, `${ref} is not allowed; only inline $defs references can be resolved`);
    }
    const target = defs[ref.slice("#/$defs/".length)];
    if (target === undefined) {
      throw new DescriptorError(`${path}.$ref`, `unresolvable reference ${ref}`);
    }
    const { $ref: _drop, ...rest } = value;
    const inlined = inlineRefs(target, defs, path);
    if (Object.keys(rest).length === 0) {
      return inlined;
    }
    const extra = inlineRefs(rest, defs, path);
    return { ...(isJsonObject(inlined) ? inlined : {}), ...(isJsonObject(extra) ? extra : {}) };
  }
  const out: { [key: string]: JsonValue } = {};
  for (const [key, item] of Object.entries(value)) {
    if (key === "$defs" || key === "definitions" || key === "$schema" || key === "$id") {
      continue;
    }
    out[key] = inlineRefs(item, defs, `${path}.${key}`);
  }
  return out;
}

function dropEmptyMetadata(value: JsonValue): JsonValue {
  if (Array.isArray(value)) {
    return value.map(dropEmptyMetadata);
  }
  if (!isJsonObject(value)) {
    return value;
  }
  const out: { [key: string]: JsonValue } = {};
  for (const [key, item] of Object.entries(value)) {
    if ((key === "title" || key === "description") && typeof item === "string" && item.trim() === "") {
      continue;
    }
    out[key] = dropEmptyMetadata(item);
  }
  return out;
}

/**
 * zod schema → 归一化 JSON Schema
 *
 * 生成前先在 zod 层拒绝 transform / refine / union / 递归（§11.5 禁止表），
 * 生成后再校验子集与限值。
 *
 * @param schema zod schema
 * @param onOutput 输出方向（禁止 secret）
 * @param path 字段路径，用于报错定位
 */
export function jsonSchemaOf(schema: z.ZodType, onOutput: boolean, path = "schema"): JsonValue {
  validateZodSchema(path, schema);
  const raw = z.toJSONSchema(schema, { io: "input" }) as JsonValue;
  const defs = isJsonObject(raw) ? (raw["$defs"] as Record<string, JsonValue> | undefined) : undefined;
  const normalized = sortKeys(
    dropEmptyMetadata(
      applyMetaKeys(closeObjects(foldNullable(inlineRefs(raw, defs, path), path)), onOutput, path),
    ),
  );
  validateSchemaSubset(path, normalized);
  return normalized;
}

// ---------------------------------------------------------------------------
// descriptor
// ---------------------------------------------------------------------------

/** descriptor 结构（plugins.md §11.1） */
export interface Descriptor {
  readonly schema: 1;
  readonly plugin_id: string;
  readonly api: number;
  readonly config: JsonValue;
  readonly ops: readonly DescriptorOp[];
  readonly schemas: Record<string, JsonValue>;
}

export interface DescriptorOp {
  readonly id: string;
  readonly kind: "query" | "action" | "stream";
  readonly title: string;
  readonly description?: string;
  readonly risk: "read" | "mutating" | "destructive";
  readonly display: JsonValue;
  readonly input: JsonValue;
  readonly output: JsonValue;
  readonly timeout_ms: number;
  readonly pagination?: JsonValue;
}

function schemaName(opId: string, direction: "in" | "out"): string {
  return `${opId}_${direction}`;
}

/** 已声明 op 的 id → kind（display 引用校验用） */
function opKinds(resolved: readonly ResolvedOp[]): Map<string, OpKind> {
  const kinds = new Map<string, OpKind>();
  for (const op of resolved) {
    kinds.set(op.id, op.kind);
  }
  return kinds;
}

/**
 * 从定义生成 descriptor。
 *
 * 逐条执行 §11.2 字段约束、§11.4 默认推断、§11.3 形状与引用校验、
 * §11.5 子集与限值、§11.7 归一化，任一失败抛 {@link DescriptorError}。
 *
 * @throws {DescriptorError} 超出自集 / 超限 / 引用不存在 / 输出方向出现 secret
 */
export function buildDescriptor<
  C extends readonly Capability[],
  Id extends string,
  Cfg extends SchemaLike,
>(def: PluginDefinition<C, Id, Cfg>): Descriptor {
  const resolved = collectOps(def);
  const kinds = opKinds(resolved);
  const ops: DescriptorOp[] = [];
  const schemas: Record<string, JsonValue> = {
    config: jsonSchemaOf(toZod(def.config), false, "config"),
  };
  const seen = new Set<string>();
  for (const op of resolved) {
    const path = `ops.${op.id}`;
    validateOpFields(path, op);
    if (seen.has(op.id)) {
      throw new DescriptorError(`${path}.id`, `${op.id} is declared more than once`);
    }
    seen.add(op.id);
    const inputName = schemaName(op.id, "in");
    const outputName = schemaName(op.id, "out");
    schemas[inputName] = jsonSchemaOf(toZod(op.input), false, `${path}.input`);
    const output = jsonSchemaOf(toZod(op.output), true, `${path}.output`);
    schemas[outputName] = output;
    const shape = schemaShape(output);
    const declaredType = validateDisplayObject(`${path}.display`, op.display);
    const displayType = declaredType ?? inferDisplay(path, op.kind, shapeHint(shape));
    validateDisplayShape(`${path}.output`, displayType, shape, op.kind === "stream");
    if (op.kind === "query" && shape.hasItemsArray && op.pagination === undefined) {
      throw new DescriptorError(`${path}.pagination`, "is required for list queries");
    }
    const extra = isJsonObject((op.display ?? null) as JsonValue) ? ((op.display as JsonValue) as { [key: string]: JsonValue }) : {};
    const display = sortKeys({ ...extra, type: displayType });
    validateDisplayRefs(`${path}.display`, display, kinds);
    const entry: DescriptorOp = {
      id: op.id,
      kind: op.kind,
      title: op.title,
      risk: op.risk,
      display,
      input: { $ref: `#/schemas/${inputName}` },
      output: { $ref: `#/schemas/${outputName}` },
      timeout_ms: op.timeoutMs ?? DEFAULT_OP_TIMEOUT_MS,
    };
    if (op.description !== undefined) {
      (entry as { description?: string }).description = op.description;
    }
    if (op.pagination !== undefined) {
      (entry as { pagination?: JsonValue }).pagination = sortKeys(op.pagination as JsonValue);
    }
    ops.push(entry);
  }
  if (ops.length > 64) {
    throw new DescriptorError("ops", `${ops.length} ops declared, limit is 64`);
  }
  const descriptor: Descriptor = {
    schema: 1,
    plugin_id: def.id,
    api: PLUGIN_API_VERSION,
    config: { $ref: "#/schemas/config" },
    ops,
    schemas,
  };
  validateDescriptorSize("descriptor", serializeNormalized(descriptor));
  return descriptor;
}

/** 归一化产物：紧凑 JSON + 末尾换行（§11.7 第 6 条） */
export function serializeNormalized(value: unknown): string {
  return `${JSON.stringify(sortKeys(value as JsonValue))}\n`;
}

/** 收集定义中的全部 op（queries / actions / streams / 预构造片段） */
export function collectOps<
  C extends readonly Capability[],
  Id extends string,
  Cfg extends SchemaLike,
>(def: PluginDefinition<C, Id, Cfg>): readonly ResolvedOp[] {
  const resolved: ResolvedOp[] = [];
  const push = (id: string, spec: OpSpecLike): void => {
    if (spec.kind === undefined) {
      throw new Error(`op ${id} is missing kind; use query()/action()/stream() builders or declare kind`);
    }
    if (spec.input === undefined || spec.output === undefined) {
      throw new Error(`op ${id} must declare both input and output schemas`);
    }
    resolved.push({
      id,
      kind: spec.kind,
      title: spec.title ?? id,
      description: spec.description,
      // 默认 risk 按 kind 决定：query / stream 只读，action 有副作用
      risk: spec.risk ?? defaultRisk(spec.kind),
      display: spec.display,
      input: toZod(spec.input),
      output: toZod(spec.output),
      timeoutMs: spec.timeoutMs,
      pagination: spec.pagination,
      run: spec.run,
    });
  };
  for (const [id, spec] of Object.entries(def.queries ?? {})) {
    push(id, spec);
  }
  for (const [id, spec] of Object.entries(def.actions ?? {})) {
    push(id, spec);
  }
  for (const [id, spec] of Object.entries(def.streams ?? {})) {
    push(id, spec);
  }
  return resolved;
}

/** 未声明 risk 时的默认值：有副作用的 action 不能默认成 `read`（§11.2） */
function defaultRisk(kind: OpKind): OpRisk {
  return kind === "action" ? "mutating" : "read";
}
