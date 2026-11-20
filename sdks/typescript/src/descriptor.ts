/**
 * descriptor 与 manifest 生成（plugins.md §10 / §11 / §11.7）
 *
 * 一份 `PluginDefinition` 同时产出 manifest.json 与 descriptor.json：
 * 类型（zod）→ JSON Schema → 归一化 → 产物。多语言 SDK 共用同一套归一化，
 * 保证不同语言的产物可逐字节比对。
 */

import { z } from "zod";

import { PLUGIN_API_VERSION, PROTOCOL_VERSION } from "./protocol.js";
import { subscriptionKinds, type Capability, type PluginDefinition } from "./define.js";

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
export function buildManifest<C extends readonly Capability[]>(def: PluginDefinition<C>, entryCmd: string): Manifest {
  if (!ID_PATTERN.test(def.id)) {
    throw new Error(`manifest.id ${def.id} must match ${ID_PATTERN.source}`);
  }
  if (!/^\d+\.\d+\.\d+/.test(def.version)) {
    throw new Error(`manifest.version ${def.version} must be semver`);
  }
  const capabilities = def.capabilities as readonly string[];
  if (capabilities.length === 0 || capabilities.length > 16) {
    throw new Error("manifest.capabilities must contain 1..16 items");
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
  format: "x-mcsup-format",
  unit: "x-mcsup-unit",
  placeholder: "x-mcsup-placeholder",
};

const FORMATS = new Set(["plain", "minecraft-text", "markdown-sanitized", "datetime", "duration", "bytes"]);

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
function foldNullable(value: JsonValue): JsonValue {
  if (Array.isArray(value)) {
    return value.map(foldNullable);
  }
  if (!isJsonObject(value)) {
    return value;
  }
  const out: { [key: string]: JsonValue } = {};
  for (const [key, item] of Object.entries(value)) {
    out[key] = foldNullable(item);
  }
  const anyOf = out["anyOf"];
  const typeList = out["type"];
  // zod v4 输出 `type: ["number", "null"]`，同样按可空折叠（§11.7 第 2 条）
  if (Array.isArray(typeList) && typeList.length === 2 && typeList.includes("null")) {
    const { type: _drop, ...rest } = out;
    return { ...rest, type: typeList.find((t) => t !== "null") as JsonValue };
  }
  if (Array.isArray(anyOf) && anyOf.length === 2 && anyOf.some((x) => x === null || x === ("null" as JsonValue) || (isJsonObject(x) && x["type"] === "null"))) {
    const inner = anyOf.find((x) => !(x === null || (isJsonObject(x) && x["type"] === "null")));
    if (inner !== undefined && isJsonObject(inner)) {
      return { ...inner, ...stripNullOnly(out) };
    }
  }
  return out;
}

function stripNullOnly(value: { [key: string]: JsonValue }): { [key: string]: JsonValue } {
  const { anyOf: _anyOf, ...rest } = value;
  return rest;
}

/** `.meta()` 便捷键 → `x-mcsup-*`；校验 format 枚举与输出方向禁止 secret */
function applyMetaKeys(value: JsonValue, onOutput: boolean): JsonValue {
  if (Array.isArray(value)) {
    return value.map((item) => applyMetaKeys(item, onOutput));
  }
  if (!isJsonObject(value)) {
    return value;
  }
  const out: { [key: string]: JsonValue } = {};
  for (const [key, item] of Object.entries(value)) {
    if (key === "properties" || key === "items" || key.match(/^(prefix|suffix|contains)/) !== null) {
      out[key] = applyMetaKeys(item, onOutput);
      continue;
    }
    const mapped = META_KEY_MAP[key];
    if (mapped === undefined) {
      out[key] = applyMetaKeys(item, onOutput);
      continue;
    }
    if (mapped === "x-mcsup-secret") {
      // 输出方向出现 secret 属于安装期拒绝项（§11.6）；生成阶段直接报错
      if (onOutput) {
        throw new Error("x-mcsup-secret is not allowed on output schemas");
      }
      out[mapped] = item;
      continue;
    }
    if (mapped === "x-mcsup-format") {
      if (typeof item !== "string" || !FORMATS.has(item)) {
        throw new Error(`x-mcsup-format must be one of ${[...FORMATS].join(", ")}`);
      }
    }
    out[mapped] = item;
  }
  return out;
}

/** 递归内联 `$defs` / `$ref`（zod v4 顶层 `.meta({id})` 会产生 `$ref`） */
function inlineRefs(value: JsonValue, defs: Record<string, JsonValue> | undefined): JsonValue {
  if (Array.isArray(value)) {
    return value.map((item) => inlineRefs(item, defs));
  }
  if (!isJsonObject(value)) {
    return value;
  }
  const ref = value["$ref"];
  if (typeof ref === "string" && ref.startsWith("#/$defs/") && defs !== undefined) {
    const target = defs[ref.slice("#/$defs/".length)];
    if (target === undefined) {
      throw new Error(`unresolvable $ref: ${ref}`);
    }
    const { $ref: _drop, ...rest } = value;
    const inlined = inlineRefs(target, defs);
    if (Object.keys(rest).length === 0) {
      return inlined;
    }
    const extra = inlineRefs(rest, defs);
    return { ...(isJsonObject(inlined) ? inlined : {}), ...(isJsonObject(extra) ? extra : {}) };
  }
  const out: { [key: string]: JsonValue } = {};
  for (const [key, item] of Object.entries(value)) {
    if (key === "$defs" || key === "definitions" || key === "$schema" || key === "$id") {
      continue;
    }
    out[key] = inlineRefs(item, defs);
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
 * @param schema zod schema
 * @param onOutput 输出方向（禁止 secret）
 */
export function jsonSchemaOf(schema: z.ZodType, onOutput: boolean): JsonValue {
  const raw = z.toJSONSchema(schema, { io: "input" }) as JsonValue;
  const defs = isJsonObject(raw) ? (raw["$defs"] as Record<string, JsonValue> | undefined) : undefined;
  return sortKeys(dropEmptyMetadata(applyMetaKeys(foldNullable(inlineRefs(raw, defs)), onOutput)));
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
  readonly timeout_ms?: number;
  readonly pagination?: JsonValue;
}

const OP_ID_PATTERN = /^[a-z][a-z0-9_-]{0,63}$/;

function schemaName(opId: string, direction: "in" | "out"): string {
  return `${opId}_${direction}`;
}

/**
 * 从定义生成 descriptor。
 *
 * @throws {Error} op id / risk 不合法，或输出方向出现 secret
 */
export function buildDescriptor<C extends readonly Capability[]>(def: PluginDefinition<C>): Descriptor {
  const ops: DescriptorOp[] = [];
  const schemas: Record<string, JsonValue> = {
    config: jsonSchemaOf(def.config, false),
  };
  for (const op of collectOps(def)) {
    if (!OP_ID_PATTERN.test(op.id)) {
      throw new Error(`op id ${op.id} must match ${OP_ID_PATTERN.source}`);
    }
    const inputName = schemaName(op.id, "in");
    const outputName = schemaName(op.id, "out");
    schemas[inputName] = jsonSchemaOf(op.input, false);
    schemas[outputName] = jsonSchemaOf(op.output, true);
    const entry: DescriptorOp = {
      id: op.id,
      kind: op.kind,
      title: op.title,
      risk: op.risk,
      display: sortKeys((op.display ?? { type: op.kind === "stream" ? "log" : "detail" }) as JsonValue),
      input: { $ref: `#/schemas/${inputName}` },
      output: { $ref: `#/schemas/${outputName}` },
    };
    if (op.description !== undefined) {
      (entry as { description?: string }).description = op.description;
    }
    if (op.timeoutMs !== undefined) {
      (entry as { timeout_ms?: number }).timeout_ms = op.timeoutMs;
    }
    if (op.pagination !== undefined) {
      (entry as { pagination?: JsonValue }).pagination = sortKeys(op.pagination as JsonValue);
    }
    ops.push(entry);
  }
  if (ops.length > 64) {
    throw new Error("descriptor.ops must contain at most 64 ops");
  }
  return {
    schema: 1,
    plugin_id: def.id,
    api: PLUGIN_API_VERSION,
    config: { $ref: "#/schemas/config" },
    ops,
    schemas,
  };
}

/** 归一化产物：紧凑 JSON + 末尾换行（§11.7 第 6 条） */
export function serializeNormalized(value: unknown): string {
  return `${JSON.stringify(sortKeys(value as JsonValue))}\n`;
}

/** 收集定义中的全部 op（queries / actions / streams / 预构造片段） */
export function collectOps<C extends readonly Capability[]>(def: PluginDefinition<C>): readonly ResolvedOp[] {
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
      risk: spec.risk ?? "read",
      display: spec.display,
      input: spec.input,
      output: spec.output,
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

/** 归约后的 op 描述（内部表示） */
export interface ResolvedOp {
  readonly id: string;
  readonly kind: "query" | "action" | "stream";
  readonly title: string;
  readonly description?: string;
  readonly risk: "read" | "mutating" | "destructive";
  readonly display?: unknown;
  readonly input: z.ZodType;
  readonly output: z.ZodType;
  readonly timeoutMs?: number;
  readonly pagination?: unknown;
  readonly run?: OpRun;
}

/** op 处理函数签名 */
export type OpRun = (input: unknown, ctx: unknown) => unknown;

/** 允许 Builder / 原生对象两种写法的结构子集 */
export type OpSpecLike = Partial<ResolvedOp> & {
  readonly kind?: "query" | "action" | "stream";
};

/** 协议版本常量（供生成器写入产物校验） */
export const SDK_PROTOCOL_VERSION = PROTOCOL_VERSION;
