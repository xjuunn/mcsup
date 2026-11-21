/**
 * 能力客户端（plugins.md §9.1）
 *
 * `Ctx` 的形状由 `capabilities` 字面量数组决定：只出现已声明能力的方法组。
 * 每次调用都是一次 `call` 帧，错误已归一为 `PluginError`。
 */

import type { Capability } from "./define.js";
import { PluginError } from "./errors.js";
import type { InferSchema, SchemaLike } from "./schema.js";

/** 服务器名 */
export type ServerName = string;

/** 通用能力调用器（Connection.call 的类型投影） */
export interface CapabilityCaller {
  readonly call: (capability: string, method: string, input: unknown, timeoutMs?: number) => Promise<unknown>;
}

// ---------------------------------------------------------------------------
// 服务器只读
// ---------------------------------------------------------------------------

export interface ServerSummary {
  readonly name: string;
  readonly status: string;
  readonly port?: number;
}

export interface ServerStatusView {
  readonly state: string;
  readonly pid?: number;
  readonly uptime_secs?: number;
  readonly players?: number;
}

export interface ConsolePage {
  readonly cursor: number;
  readonly lines: readonly string[];
  readonly dropped: number;
}

export interface ConsoleClient {
  /** 列服务器 */
  list(): Promise<readonly ServerSummary[]>;
  /** 状态 */
  status(server: ServerName): Promise<ServerStatusView>;
  /** 服务端版本（未知为 null） */
  version(server: ServerName): Promise<string | null>;
  /** server.properties（字符串映射） */
  properties(server: ServerName): Promise<Readonly<Record<string, string>>>;
}

// ---------------------------------------------------------------------------
// 控制台
// ---------------------------------------------------------------------------

export interface ConsoleReadClient {
  /** 游标分页；`cursor` 省略表示从当前尾部开始 */
  tail(server: ServerName, cursor?: number | null, maxLines?: number): Promise<ConsolePage>;
}

export interface ConsoleWriteClient {
  /** 注入一行命令（≤ 4096 字节，禁止换行） */
  sendLine(server: ServerName, line: string): Promise<void>;
}

// ---------------------------------------------------------------------------
// KV
// ---------------------------------------------------------------------------

export interface KvClient {
  get(key: string): Promise<Uint8Array | null>;
  set(key: string, value: Uint8Array | string): Promise<void>;
  delete(key: string): Promise<boolean>;
  /** 按前缀列键，单次 ≤ 1000，需分页 */
  list(prefix?: string): Promise<readonly string[]>;
}

// ---------------------------------------------------------------------------
// 密钥
// ---------------------------------------------------------------------------

export interface SecretClient {
  get(key: string): Promise<string | null>;
  set(key: string, value: string): Promise<void>;
  delete(key: string): Promise<boolean>;
}

// ---------------------------------------------------------------------------
// 出站网络
// ---------------------------------------------------------------------------

export interface NetRequest {
  readonly method?: string;
  readonly url: string;
  readonly headers?: Readonly<Record<string, string>>;
  /** 请求体；字符串按 UTF-8 编码，Uint8Array 原样发送 */
  readonly body?: string | Uint8Array | null;
  readonly timeoutMs?: number;
}

export interface NetResponse {
  readonly status: number;
  readonly headers: Readonly<Record<string, string>>;
  /** 响应体 */
  readonly body: Uint8Array;
  readonly bodyLen: number;
}

export interface NetClient {
  /** 经宿主代发（域名白名单 + SSRF 防护在宿主侧） */
  request(req: NetRequest): Promise<NetResponse>;
}

// ---------------------------------------------------------------------------
// 文件
// ---------------------------------------------------------------------------

export interface FileEntry {
  readonly path: string;
  readonly isDir: boolean;
  readonly size: number;
}

export interface FilesReadClient {
  read(server: ServerName, path: string): Promise<{ readonly path: string; readonly size: number; readonly data: Uint8Array }>;
  list(server: ServerName, path?: string): Promise<readonly FileEntry[]>;
}

export interface FilesWriteClient {
  /** `overwrite` 默认 false；目标存在未允许覆盖返回 conflict */
  write(server: ServerName, path: string, data: Uint8Array | string, overwrite?: boolean): Promise<{ readonly ok: true; readonly size: number }>;
  remove(server: ServerName, path: string): Promise<boolean>;
}

// ---------------------------------------------------------------------------
// 生命周期
// ---------------------------------------------------------------------------

export type StopOutcome = { readonly outcome: "stopped"; readonly elapsedMs: number } | { readonly outcome: "already_stopped" };

export interface LifecycleClient {
  start(server: ServerName): Promise<void>;
  stop(server: ServerName, graceSecs?: number): Promise<StopOutcome>;
  restart(server: ServerName, graceSecs?: number): Promise<void>;
}

// ---------------------------------------------------------------------------
// 备份
// ---------------------------------------------------------------------------

export interface SnapshotView {
  readonly id: string;
  readonly createdAt: string;
  readonly addedBytes: number;
}

export interface BackupClient {
  create(server: ServerName, label?: string): Promise<SnapshotView>;
  list(server: ServerName): Promise<readonly SnapshotView[]>;
  /** 服务器运行中返回 conflict */
  restore(server: ServerName, snapshot: string): Promise<void>;
  verify(server: ServerName, snapshot: string): Promise<boolean>;
  prune(server: ServerName, keep: number): Promise<number>;
  delete(server: ServerName, snapshot: string): Promise<boolean>;
}

// ---------------------------------------------------------------------------
// 内容
// ---------------------------------------------------------------------------

export type ContentKind = "mod" | "plugin" | "resourcepack" | "datapack" | "shader" | "modpack";

export interface ContentItem {
  readonly file: string;
  readonly size: number;
  readonly identity?: unknown;
}

export interface ContentClient {
  list(server: ServerName, opts?: { readonly kind?: ContentKind; readonly identify?: boolean }): Promise<readonly ContentItem[]>;
  install(server: ServerName, project: string, opts?: { readonly version?: string; readonly kind?: ContentKind }): Promise<{ readonly file: string; readonly size: number; readonly project: string; readonly version: string }>;
  remove(server: ServerName, selector: string, kind?: ContentKind): Promise<number>;
}

// ---------------------------------------------------------------------------
// Java
// ---------------------------------------------------------------------------

export interface JavaRuntime {
  readonly id: string;
  readonly path: string;
  readonly major: number;
  readonly version: string;
  readonly vendor: string;
  readonly managed: boolean;
  readonly createdAt: string;
  readonly updatedAt: string;
}

export interface JavaClient {
  list(): Promise<readonly JavaRuntime[]>;
  install(major: number | string): Promise<JavaRuntime>;
  remove(path: string): Promise<boolean>;
}

// ---------------------------------------------------------------------------
// 事件
// ---------------------------------------------------------------------------

export interface EventMeta {
  readonly kind: string;
  readonly server?: string;
  readonly at: string;
}

export interface EventsClient {
  /** 登记 handler；可订阅的事件名在 `definePlugin({ subscriptions })` 中静态声明 */
  on(kind: string, handler: (data: unknown, meta: EventMeta) => void | Promise<void>): () => void;
}

// ---------------------------------------------------------------------------
// 日志
// ---------------------------------------------------------------------------

/** 日志（写 stderr，不进入帧） */
export interface LogClient {
  debug(message: string): void;
  info(message: string): void;
  warn(message: string): void;
  error(message: string): void;
}

// ---------------------------------------------------------------------------
// 条件映射
// ---------------------------------------------------------------------------

/** 每个能力名 → 对应的方法组 */
export interface CtxMembers {
  readonly "events": EventsClient;
  readonly "storage:kv": KvClient;
  readonly "server:read": ServerClientGroup;
  readonly "console:read": ConsoleReadClient;
  readonly "console:write": ConsoleWriteClient;
  readonly "net:outbound": NetClient;
  readonly "secret:rw": SecretClient;
  readonly "backup": BackupClient;
  readonly "files:read": FilesReadClient;
  readonly "files:write": FilesWriteClient;
  readonly "server:lifecycle": LifecycleClient;
  readonly "content": ContentClient;
  readonly "java": JavaClient;
}

/** `server:read` 提供的能力组 */
export interface ServerClientGroup {
  readonly servers: ConsoleClient;
}

type SectionOf<C extends readonly Capability[]> = {
  [K in C[number]]: K extends keyof CtxMembers ? CtxMembers[K] : never;
}[C[number]];

/**
 * 能力驱动的上下文（`CtxFor<capabilities, config>`）。
 *
 * 未声明的能力对应字段在类型上不存在；`log` 恒可用。`config` 的类型由
 * `config` 字段的 schema 推导。
 */
export type CtxFor<C extends readonly Capability[], Cfg extends SchemaLike = SchemaLike> = {
  [K in keyof CtxMembers as K extends C[number] ? K : never]: CtxMembers[K];
} & {
  readonly log: LogClient;
  readonly config: InferSchema<Cfg>;
  readonly sessionId: string;
  readonly limits: {
    readonly inflight: number;
    readonly frameBytes: number;
    readonly eventQueue: number;
    readonly logLinesPerSec: number;
  };
  /** 当前作用域 */
  readonly scope: { readonly kind: string; readonly server?: string };
};

/** 运行期构造上下文所需的环境（welcome 后会被改写） */
export interface CtxEnvironment {
  readonly caller: CapabilityCaller;
  readonly config: () => unknown;
  sessionId: string;
  limits: CtxFor<readonly Capability[]>["limits"];
  scope: CtxFor<readonly Capability[]>["scope"];
  readonly events: EventBus;
  readonly logLine: (level: string, message: string) => void;
}

/** 事件总线：内核把宿主 event 帧投进来 */
export class EventBus {
  private readonly handlers = new Map<string, Set<(data: unknown, meta: EventMeta) => void | Promise<void>>>();

  /** 登记 handler，返回取消函数 */
  public on(kind: string, handler: (data: unknown, meta: EventMeta) => void | Promise<void>): () => void {
    let set = this.handlers.get(kind);
    if (set === undefined) {
      set = new Set();
      this.handlers.set(kind, set);
    }
    set.add(handler);
    return () => {
      set.delete(handler);
    };
  }

  /** 投递一个事件；返回是否有人处理 */
  public async dispatch(kind: string, data: unknown, meta: EventMeta): Promise<boolean> {
    let handled = false;
    for (const target of [kind, "*"]) {
      const set = this.handlers.get(target);
      if (set === undefined) {
        continue;
      }
      for (const handler of [...set]) {
        handled = true;
        await handler(data, meta);
      }
    }
    return handled;
  }
}

function asBytes(value: string | Uint8Array): Uint8Array {
  return typeof value === "string" ? new TextEncoder().encode(value) : value;
}

function b64(bytes: Uint8Array): string {
  return Buffer.from(bytes).toString("base64");
}

function unb64(text: string): Uint8Array {
  return new Uint8Array(Buffer.from(text, "base64"));
}

interface KvGetOut { readonly value: string | null }
interface KvListOut { readonly keys: readonly string[] }
interface SecretGetOut { readonly value: string | null }
interface FileReadOut { readonly path: string; readonly size: number; readonly data: string }
interface FileListOut { readonly entries: readonly { readonly path: string; readonly is_dir: boolean; readonly size: number }[] }
interface NetOut { readonly status: number; readonly headers: Readonly<Record<string, string>>; readonly body: string; readonly body_len: number }
interface ConsoleTailOut { readonly cursor: number; readonly lines: readonly string[]; readonly dropped: number }
interface BackupListOut { readonly snapshots: readonly { readonly id: string; readonly created_at: string; readonly added_bytes: number }[] }
interface BackupCreateOut { readonly snapshot: { readonly id: string; readonly created_at: string; readonly added_bytes: number } }
interface ContentListOut { readonly items: readonly { readonly file: string; readonly size: number; readonly identity?: unknown }[] }
interface JavaListOut { readonly runtimes: readonly { readonly id: string; readonly path: string; readonly major: number; readonly version: string; readonly vendor: string; readonly managed: boolean; readonly created_at: string; readonly updated_at: string }[] }
interface JavaInstallOut { readonly runtime: JavaListOut["runtimes"][number] }

/**
 * 构造上下文。
 *
 * 只挂载已声明的能力对应的字段；运行期与类型层保持一致。
 */
export function createCtx<C extends readonly Capability[], Cfg extends SchemaLike = SchemaLike>(
  env: CtxEnvironment,
  capabilities: C,
  ungranted: readonly Capability[] = [],
): CtxFor<C, Cfg> {
  const call = async (capability: string, method: string, input: unknown, timeoutMs?: number): Promise<unknown> =>
    await env.caller.call(capability, method, input, timeoutMs);

  const members: Record<string, unknown> = {};

  const has = (cap: Capability): boolean => (capabilities as readonly string[]).includes(cap);

  if (has("server:read")) {
    const servers: ConsoleClient = {
      list: async () => (await call("server:read", "list", {})) as readonly ServerSummary[],
      status: async (server) => (await call("server:read", "status", { server })) as ServerStatusView,
      version: async (server) => (await call("server:read", "version", { server })) as string | null,
      properties: async (server) => (await call("server:read", "properties", { server })) as Readonly<Record<string, string>>,
    };
    members["server:read"] = { servers };
  }
  if (has("console:read")) {
    const consoleRead: ConsoleReadClient = {
      tail: async (server, cursor = null, maxLines = 200) => {
        const out = (await call("console:read", "tail", { server, cursor, max_lines: maxLines })) as ConsoleTailOut;
        return { cursor: out.cursor, lines: out.lines, dropped: out.dropped };
      },
    };
    members["console:read"] = consoleRead;
  }
  if (has("console:write")) {
    const consoleWrite: ConsoleWriteClient = {
      sendLine: async (server, line) => {
        await call("console:write", "send_line", { server, line });
      },
    };
    members["console:write"] = consoleWrite;
  }
  if (has("storage:kv")) {
    const kv: KvClient = {
      get: async (key) => {
        const out = (await call("storage:kv", "get", { key })) as KvGetOut;
        return out.value === null ? null : unb64(out.value);
      },
      set: async (key, value) => {
        await call("storage:kv", "set", { key, value: b64(asBytes(value)) });
      },
      delete: async (key) => (await call("storage:kv", "delete", { key })) as boolean,
      list: async (prefix = "") => {
        const out = (await call("storage:kv", "list", { prefix })) as KvListOut;
        return out.keys;
      },
    };
    members["storage:kv"] = kv;
  }
  if (has("secret:rw")) {
    const secrets: SecretClient = {
      get: async (key) => ((await call("secret:rw", "get", { key })) as SecretGetOut).value,
      set: async (key, value) => {
        await call("secret:rw", "set", { key, value });
      },
      delete: async (key) => (await call("secret:rw", "delete", { key })) as boolean,
    };
    members["secret:rw"] = secrets;
  }
  if (has("net:outbound")) {
    const net: NetClient = {
      request: async (req) => {
        const out = (await call("net:outbound", "request", {
          method: req.method ?? "GET",
          url: req.url,
          headers: req.headers ?? {},
          body: req.body === undefined || req.body === null ? null : b64(asBytes(req.body)),
          timeout_ms: req.timeoutMs,
        })) as NetOut;
        return { status: out.status, headers: out.headers, body: unb64(out.body), bodyLen: out.body_len };
      },
    };
    members["net:outbound"] = net;
  }
  if (has("files:read")) {
    const filesRead: FilesReadClient = {
      read: async (server, path) => {
        const out = (await call("files:read", "read", { server, path })) as FileReadOut;
        return { path: out.path, size: out.size, data: unb64(out.data) };
      },
      list: async (server, path) => {
        const out = (await call("files:read", "list", { server, path })) as FileListOut;
        return out.entries.map((e) => ({ path: e.path, isDir: e.is_dir, size: e.size }));
      },
    };
    members["files:read"] = filesRead;
  }
  if (has("files:write")) {
    const filesWrite: FilesWriteClient = {
      write: async (server, path, data, overwrite = false) =>
        (await call("files:write", "write", { server, path, data: b64(asBytes(data)), overwrite })) as { ok: true; size: number },
      remove: async (server, path) => (await call("files:write", "remove", { server, path })) as boolean,
    };
    members["files:write"] = filesWrite;
  }
  if (has("server:lifecycle")) {
    const lifecycle: LifecycleClient = {
      start: async (server) => {
        await call("server:lifecycle", "start", { server });
      },
      stop: async (server, graceSecs) =>
        (await call("server:lifecycle", "stop", { server, grace_secs: graceSecs })) as StopOutcome,
      restart: async (server, graceSecs) => {
        await call("server:lifecycle", "restart", { server, grace_secs: graceSecs });
      },
    };
    members["server:lifecycle"] = lifecycle;
  }
  if (has("backup")) {
    const mapSnapshot = (s: { id: string; created_at: string; added_bytes: number }): SnapshotView => ({
      id: s.id,
      createdAt: s.created_at,
      addedBytes: s.added_bytes,
    });
    const backup: BackupClient = {
      create: async (server, label) => {
        const out = (await call("backup", "create", { server, label })) as BackupCreateOut;
        return mapSnapshot(out.snapshot);
      },
      list: async (server) => {
        const out = (await call("backup", "list", { server })) as BackupListOut;
        return out.snapshots.map(mapSnapshot);
      },
      restore: async (server, snapshot) => {
        await call("backup", "restore", { server, snapshot });
      },
      verify: async (server, snapshot) => ((await call("backup", "verify", { server, snapshot })) as { healthy: boolean }).healthy,
      prune: async (server, keep) => ((await call("backup", "prune", { server, keep })) as { removed: number }).removed,
      delete: async (server, snapshot) => ((await call("backup", "delete", { server, snapshot })) as { deleted: boolean }).deleted,
    };
    members["backup"] = backup;
  }
  if (has("content")) {
    const content: ContentClient = {
      list: async (server, opts) => {
        const out = (await call("content", "list", { server, kind: opts?.kind, identify: opts?.identify })) as ContentListOut;
        return out.items;
      },
      install: async (server, project, opts) =>
        (await call("content", "install", { server, project, version: opts?.version, kind: opts?.kind })) as {
          file: string;
          size: number;
          project: string;
          version: string;
        },
      remove: async (server, selector, kind) => ((await call("content", "remove", { server, selector, kind })) as { removed: number }).removed,
    };
    members["content"] = content;
  }
  if (has("java")) {
    const mapRuntime = (r: JavaListOut["runtimes"][number]): JavaRuntime => ({
      id: r.id,
      path: r.path,
      major: r.major,
      version: r.version,
      vendor: r.vendor,
      managed: r.managed,
      createdAt: r.created_at,
      updatedAt: r.updated_at,
    });
    const java: JavaClient = {
      list: async () => {
        const out = (await call("java", "list", {})) as JavaListOut;
        return out.runtimes.map(mapRuntime);
      },
      install: async (major) => mapRuntime(((await call("java", "install", { major })) as JavaInstallOut).runtime),
      remove: async (path) => ((await call("java", "remove", { path })) as { removed: boolean }).removed,
    };
    members["java"] = java;
  }
  if (has("events")) {
    const events: EventsClient = {
      on: (kind, handler) => env.events.on(kind, handler),
    };
    members["events"] = events;
  }

  const log: LogClient = {
    debug: (message) => {
      env.logLine("debug", message);
    },
    info: (message) => {
      env.logLine("info", message);
    },
    warn: (message) => {
      env.logLine("warn", message);
    },
    error: (message) => {
      env.logLine("error", message);
    },
  };

  const ctx = {
    ...members,
    log,
    config: env.config(),
    sessionId: env.sessionId,
    limits: env.limits,
    scope: env.scope,
  };
  Object.defineProperty(ctx, "config", {
    get: () => env.config(),
    enumerable: true,
  });
  // 声明过但未授予的能力：访问即 denied，而不是静默 undefined（§8.4 不降级）
  for (const capability of ungranted) {
    Object.defineProperty(ctx, capability, {
      enumerable: false,
      configurable: true,
      get() {
        throw new PluginError("denied", `${capability} capability was not granted`);
      },
    });
  }
  return ctx as CtxFor<C, Cfg>;
}
