/**
 * descriptor 与 manifest 生成（plugins.md §10 / §11 / §11.7）
 *
 * 一份 `PluginDefinition` 同时产出 manifest.json 与 descriptor.json：
 * 类型（zod）→ JSON Schema → 归一化 → 产物。多语言 SDK 共用同一套归一化，
 * 保证不同语言的产物可逐字节比对。
 */

import { z } from "./zod.js";

import {
  MAX_SETUP_HELP_LINKS,
  MAX_SETUP_HELP_LINK_LABEL_CHARS,
  MAX_SETUP_HELP_LINK_URL_CHARS,
  MAX_SETUP_HELP_STEPS,
  MAX_SETUP_HELP_STEP_CHARS,
  MAX_SETUP_HELP_TEXT_CHARS,
  MAX_SETUP_HELP_TITLE_CHARS,
  PLUGIN_API_VERSION,
} from "./protocol.js";
import {
  subscriptionKinds,
  type BindSpec,
  type Capability,
  type OpKind,
  type OpRisk,
  type OpSpecLike,
  type PluginDefinition,
  type ResolvedOp,
  type SetupHelpSpec,
  type SetupSpec,
  type SetupWhenSpec,
} from "./define.js";
import { toZod, type SchemaLike } from "./schema.js";
import {
  DEFAULT_OP_TIMEOUT_MS,
  DescriptorError,
  MCSup_FORMATS,
  MAX_DESCRIPTOR_BYTES,
  OP_ID_PATTERN,
  inferDisplay,
  isJsonSchemaFormat,
  isMcsupFormat,
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
      if (isMcsupFormat(item)) {
        out["x-mcsup-format"] = item;
        continue;
      }
      if (isJsonSchemaFormat(item)) {
        out["format"] = item;
        continue;
      }
      throw new DescriptorError(
        `${path}.x-mcsup-format`,
        `must be one of ${MCSup_FORMATS.join(", ")} or a standard JSON Schema format`,
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
  readonly schema: 1 | 2 | 3 | 4 | 5 | 6;
  readonly plugin_id: string;
  readonly api: number;
  readonly config: JsonValue;
  /** schema 2 起可用：安装向导 */
  readonly setup?: JsonValue;
  /** schema 3 起可用：服务器绑定 */
  readonly bind?: JsonValue;
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

/** 分页声明归一化：四个字段都必须写进 descriptor（宿主反序列化无默认值） */
function normalizePagination(path: string, value: unknown): JsonValue {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new DescriptorError(`${path}.pagination`, "must be an object");
  }
  const record = value as Record<string, unknown>;
  const text = (snake: string, camel: string): string => {
    const raw = record[snake] ?? record[camel];
    if (typeof raw !== "string" || raw.trim() === "") {
      throw new DescriptorError(`${path}.pagination.${snake}`, "must be a non-empty string");
    }
    return raw;
  };
  const count = (snake: string, camel: string): number => {
    const raw = record[snake] ?? record[camel];
    if (typeof raw !== "number" || !Number.isInteger(raw)) {
      throw new DescriptorError(`${path}.pagination.${snake}`, "must be an integer");
    }
    return raw;
  };
  const cursorField = text("cursor_field", "cursorField");
  const limitField = text("limit_field", "limitField");
  const defaultLimit = count("default_limit", "defaultLimit");
  const maxLimit = count("max_limit", "maxLimit");
  if (defaultLimit <= 0 || maxLimit <= 0 || maxLimit > 1_000 || defaultLimit > maxLimit) {
    throw new DescriptorError(
      `${path}.pagination`,
      `limits must satisfy 1 <= default_limit <= max_limit <= 1000 (got ${defaultLimit}..${maxLimit})`,
    );
  }
  return sortKeys({ cursor_field: cursorField, limit_field: limitField, default_limit: defaultLimit, max_limit: maxLimit });
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

/** 字符数：按 Unicode 码点计，与宿主一致（`length` 按 UTF-16 单元） */
const charCount = (text: string): number => [...text].length;

/**
 * 帮助文本归一化（descriptor schema 6）
 *
 * 形状由 `style` 决定：`steps` 只接受 `steps`，`lines` / `inline` 只接受
 * `text`。两种都写会让宿主不知道该渲染哪一份，构建期直接拒绝，比运行时
 * 静默取其一好。`style` 即使省略也写进产物，宿主不必再推断默认值。
 */
function buildSetupHelp(path: string, help: SetupHelpSpec): JsonValue {
  const style = help.style ?? "lines";
  if (style !== "lines" && style !== "steps" && style !== "inline") {
    throw new DescriptorError(`${path}.style`, "must be lines / steps / inline");
  }
  const entry: Record<string, JsonValue> = { style };
  if (help.title !== undefined) {
    const title = help.title.trim();
    if (title === "" || charCount(help.title) > MAX_SETUP_HELP_TITLE_CHARS) {
      throw new DescriptorError(`${path}.title`, "must be 1..=64 characters");
    }
    entry["title"] = title;
  }
  if (style === "steps") {
    if (help.text !== undefined) {
      throw new DescriptorError(`${path}.text`, "is for style lines / inline; declare steps");
    }
    if (help.steps === undefined || help.steps.length === 0) {
      throw new DescriptorError(`${path}.steps`, "is required for style steps");
    }
    if (help.steps.length > MAX_SETUP_HELP_STEPS) {
      throw new DescriptorError(`${path}.steps`, `has more than ${MAX_SETUP_HELP_STEPS} items`);
    }
    for (const step of help.steps) {
      if (step.trim() === "" || charCount(step) > MAX_SETUP_HELP_STEP_CHARS) {
        throw new DescriptorError(`${path}.steps`, "items must be 1..=200 characters");
      }
    }
    entry["steps"] = help.steps.map((step) => step.trim());
  } else {
    if (help.steps !== undefined) {
      throw new DescriptorError(`${path}.steps`, "is for style steps; declare text");
    }
    if (help.text === undefined || help.text.trim() === "") {
      throw new DescriptorError(`${path}.text`, "is required for style lines / inline");
    }
    if (charCount(help.text) > MAX_SETUP_HELP_TEXT_CHARS) {
      throw new DescriptorError(`${path}.text`, "is longer than 512 characters");
    }
    entry["text"] = help.text;
  }
  if (help.links !== undefined) {
    if (help.links.length === 0) {
      throw new DescriptorError(`${path}.links`, "must not be empty when declared");
    }
    if (help.links.length > MAX_SETUP_HELP_LINKS) {
      throw new DescriptorError(`${path}.links`, `has more than ${MAX_SETUP_HELP_LINKS} items`);
    }
    entry["links"] = help.links.map((link) => {
      const label = link.label.trim();
      const url = link.url.trim();
      if (label === "" || charCount(label) > MAX_SETUP_HELP_LINK_LABEL_CHARS) {
        throw new DescriptorError(`${path}.links`, "label must be 1..=64 characters");
      }
      if (!url.startsWith("https://") && !url.startsWith("http://")) {
        throw new DescriptorError(`${path}.links`, "url must start with http:// or https://");
      }
      if (charCount(url) > MAX_SETUP_HELP_LINK_URL_CHARS || /\s/u.test(url)) {
        throw new DescriptorError(`${path}.links`, "url must be a single-line http(s) url");
      }
      return { label, url };
    });
  }
  return sortKeys(entry);
}

/** 安装向导归一化：只保留声明过的键，字段顺序保持声明顺序 */
function buildSetup(setup: SetupSpec): JsonValue {
  const fields = setup.fields.map((field) => {
    const entry: Record<string, JsonValue> = { path: field.path };
    if (field.required !== undefined) {
      entry["required"] = field.required;
    }
    if (field.secret !== undefined) {
      entry["secret"] = field.secret;
    }
    if (field.label !== undefined) {
      entry["label"] = field.label;
    }
    if (field.description !== undefined) {
      entry["description"] = field.description;
    }
    if (field.help !== undefined) {
      entry["help"] = buildSetupHelp(`setup.fields.${field.path}.help`, field.help);
    }
    if (field.default !== undefined) {
      entry["default"] = field.default as JsonValue;
    }
    if (field.choices !== undefined) {
      entry["choices"] = [...field.choices];
    }
    if (field.choicesFrom !== undefined) {
      entry["choicesFrom"] = field.choicesFrom;
    }
    if (field.when !== undefined) {
      entry["when"] = normalizeSetupWhen(`setup.fields.${field.path}`, field.when);
    }
    if (field.interactive !== undefined) {
      entry["interactive"] = field.interactive;
    }
    return sortKeys(entry);
  });
  checkSetupHandlers(setup);
  const result: Record<string, JsonValue> = { fields };
  if (setup.start !== undefined) {
    result["start"] = setup.start;
  }
  return sortKeys(result);
}

/**
 * 校验交互式字段与处理函数一一对应
 *
 * 声明了 `interactive` 却没有 handler，宿主执行时必然失败；handler 挂在
 * 非交互字段上则是死代码。两种都在构建期报错，不必等安装到宿主才发现。
 */
function checkSetupHandlers(setup: SetupSpec): void {
  const handlers = setup.interactive ?? {};
  for (const path of Object.keys(handlers)) {
    const field = setup.fields.find((item) => item.path === path);
    if (field === undefined) {
      throw new DescriptorError(`setup.interactive.${path}`, "does not match a declared setup field");
    }
    if (field.interactive !== true) {
      throw new DescriptorError(
        `setup.interactive.${path}`,
        "field does not declare interactive: true",
      );
    }
    if (typeof handlers[path] !== "function") {
      throw new DescriptorError(`setup.interactive.${path}`, "must be a function");
    }
  }
  for (const field of setup.fields) {
    if (field.interactive === true && handlers[field.path] === undefined) {
      throw new DescriptorError(
        `setup.fields.${field.path}`,
        "interactive field has no handler in setup.interactive",
      );
    }
  }
}

/**
 * 提问条件归一化
 *
 * 只保留声明过的键；`path` 必填、`includes` / `equals` 二选一，写错在构建期
 * 就报错，不必等安装到宿主才发现（宿主也会复校验）。
 */
function normalizeSetupWhen(path: string, when: SetupWhenSpec): JsonValue {
  if (typeof when.path !== "string" || when.path.trim() === "") {
    throw new DescriptorError(`${path}.when.path`, "must be a non-empty string");
  }
  if (when.includes === undefined && when.equals === undefined) {
    throw new DescriptorError(`${path}.when`, "must declare either includes or equals");
  }
  if (when.includes !== undefined && when.equals !== undefined) {
    throw new DescriptorError(`${path}.when`, "cannot declare both includes and equals");
  }
  const entry: Record<string, JsonValue> = { path: when.path };
  if (when.includes !== undefined) {
    if (typeof when.includes !== "string" || when.includes.trim() === "") {
      throw new DescriptorError(`${path}.when.includes`, "must be a non-empty string");
    }
    entry["includes"] = when.includes;
  }
  if (when.equals !== undefined) {
    entry["equals"] = when.equals as JsonValue;
  }
  return sortKeys(entry);
}

/** 单次绑定可声明的路径上限，与宿主 `MAX_BIND_PATHS` 一致 */
const MAX_BIND_PATHS = 8;

/**
 * 服务器绑定归一化：路径去空白后写入，顺序按声明保留。
 *
 * 空路径、重复路径与超限在构建期拒绝，错误信息指向声明处，
 * 不必等安装到宿主才发现。
 */
function buildBind(bind: BindSpec): JsonValue {
  const servers: string[] = [];
  const seen = new Set<string>();
  for (const raw of bind.servers) {
    const path = raw.trim();
    if (path === "") {
      throw new DescriptorError("bind.servers", "paths must not be empty");
    }
    if (servers.length >= MAX_BIND_PATHS) {
      throw new DescriptorError("bind.servers", `declares more than ${MAX_BIND_PATHS} paths`);
    }
    if (seen.has(path)) {
      throw new DescriptorError("bind.servers", `duplicate path ${path}`);
    }
    seen.add(path);
    servers.push(path);
  }
  return sortKeys({ servers });
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
  const setup = def.setup === undefined ? undefined : buildSetup(def.setup);
  const bind = def.bind === undefined ? undefined : buildBind(def.bind);
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
      (entry as { pagination?: JsonValue }).pagination = normalizePagination(path, op.pagination);
    }
    ops.push(entry);
  }
  if (ops.length > 64) {
    throw new DescriptorError("ops", `${ops.length} ops declared, limit is 64`);
  }
  // 版本号只升不降：帮助文本要求 6，交互式字段 5，字段条件 4，bind 3，setup 2，其余 1
  const fields = def.setup?.fields ?? [];
  const schema: 1 | 2 | 3 | 4 | 5 | 6 = fields.some((field) => field.help !== undefined)
    ? 6
    : fields.some((field) => field.interactive === true)
      ? 5
      : fields.some((field) => field.when !== undefined)
        ? 4
        : bind !== undefined
          ? 3
          : setup === undefined
            ? 1
            : 2;
  const descriptor: Descriptor = {
    schema,
    plugin_id: def.id,
    api: PLUGIN_API_VERSION,
    config: { $ref: "#/schemas/config" },
    ops,
    schemas,
  };
  if (setup !== undefined) {
    (descriptor as { setup?: JsonValue }).setup = setup;
  }
  if (bind !== undefined) {
    (descriptor as { bind?: JsonValue }).bind = bind;
  }
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
      // 原生对象写法必然显式写了 output；Builder 只有调过 `.output()` 才算声明
      outputDeclared: spec.outputDeclared !== false,
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
