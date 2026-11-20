/**
 * MCSUP 插件 SDK
 *
 * 公开面（plugins.md §14.2）刻意保持小：
 * `definePlugin` / `z` / `PluginContext` / `PluginError`；
 * Builder 与 Hooks 是次版本可选 API（sdk-design.md §9）。
 */

export { definePlugin, subscriptionKinds, subscriptionHandlers } from "./define.js";
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

export { run, ExitCode, Session } from "./runtime.js";
export type { ExitCode as ExitCodeValue, FrameChannel, RunOptions } from "./runtime.js";

// descriptor 校验（plugins.md §11.2–§11.5）：作者构建期遇到错误时定位用
export { DescriptorError } from "./validate.js";
export {
  DEFAULT_OP_TIMEOUT_MS,
  MAX_DESCRIPTOR_BYTES,
  MAX_ENUM_VALUES,
  MAX_OBJECT_PROPERTIES,
  MAX_SCHEMA_BYTES,
  MAX_SCHEMA_DEPTH,
  MAX_SCHEMA_NODES,
  MAX_STRING_LENGTH,
  OP_ID_PATTERN,
} from "./validate.js";

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
  LogClient,
  NetClient,
  NetRequest,
  NetResponse,
  SecretClient,
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
export type { KvCounter, KvCursor, KvTtlCache, WithRetryOptions } from "./utils/index.js";

// 协议常量（作者偶尔需要，如手写集成测试）
export { MAX_FRAME_BYTES, MAX_JSON_DEPTH, PLUGIN_API_VERSION, PROTOCOL_VERSION } from "./protocol.js";