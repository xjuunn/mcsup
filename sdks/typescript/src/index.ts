/**
 * MCSUP 插件 SDK
 *
 * 公开面（plugins.md §14.2）刻意保持小：
 * 内核 `definePlugin` / `run` + 字段 DSL `s` / `z` / `PluginContext` /
 * `PluginError`；Builder 与 Hooks 是次版本可选 API（sdk-design.md §9）。
 *
 * 内部实现（会话、descriptor 校验常量、订阅归约）不从这里导出：作者写插件
 * 不需要它们，测试从具体模块导入即可。
 */

export { definePlugin } from "./define.js";
export type {
  Capability,
  DefinedPlugin,
  EventHandler,
  OpSpec,
  PluginContext,
  PluginDefinition,
  StreamChunks,
  Subscription,
} from "./define.js";

export {
  PluginError,
  categoryOf,
  isRetryable,
  toPluginError,
} from "./errors.js";
export type { PluginErrorCategory, PluginErrorCode } from "./errors.js";

export { run } from "./runtime.js";
export type { RunOptions } from "./runtime.js";

// 字段 DSL（plugins.md §11.5 子集）：归约为 zod，作者不必直接写 zod
export { s, isField, toZod } from "./schema.js";
export type { Field, InferSchema, SchemaLike } from "./schema.js";

export { z } from "./zod.js";

export type {
  BackupClient,
  ConsoleClient,
  ConsolePage,
  ConsoleReadClient,
  ConsoleWriteClient,
  ContentClient,
  ContentItem,
  ContentKind,
  EventMeta,
  EventsClient,
  FileEntry,
  FilesReadClient,
  FilesWriteClient,
  JavaClient,
  JavaRuntime,
  KvClient,
  LifecycleClient,
  LimitsView,
  LogClient,
  NetClient,
  NetRequest,
  NetResponse,
  SecretClient,
  ScopeView,
  ServerName,
  ServerStatusView,
  ServerSummary,
  SnapshotView,
  StopOutcome,
} from "./ctx.js";

// Builder（次版本可选 API）
export {
  action,
  lineFilter,
  labeled,
  op,
  pagedInput,
  query,
  secretInput,
  stream,
} from "./builders.js";
export type { ActionBuilder, ConsoleLine, LineFilter, OpKind, OpRisk, QueryBuilder, StreamBuilder } from "./builders.js";

// Hooks（次版本可选 API）
export { useConsoleTail, useEvents, useKvCache, useRetry } from "./hooks.js";
export type {
  HookContext,
  ConsoleTail,
  ConsoleTailHandler,
  ConsoleTailOptions,
  KvCache,
  KvCacheOptions,
  Retry,
  RetryOptions,
} from "./hooks.js";

// 工具集（次版本可选 API）：纯函数 + 显式接能力 client 的状态工具
export {
  buildCommand,
  buildUrl,
  chunk,
  clamp,
  isPlayerName,
  kvCounter,
  kvCursor,
  kvJson,
  kvTtlCache,
  legacyColorCodes,
  mapLimit,
  nowIso,
  pLimit,
  randomId,
  sanitizeChat,
  sleep,
  stripColorCodes,
  truncate,
  unique,
  withRetry,
} from "./utils/index.js";
export type { KvCounter, KvCursor, KvJson, KvTtlCache, WithRetryOptions } from "./utils/index.js";
