/**
 * 能力客户端（plugins.md §9.1）
 *
 * `Ctx` 的形状由 `capabilities` 字面量数组决定：只出现已声明能力的方法组。
 * 每次调用都是一次 `call` 帧，错误已归一为 `PluginError`。
 */

import type { OpenCapabilityName } from "./contracts/v1.js";
import { PluginError } from "./errors.js";
import type { InferSchema, SchemaLike } from "./schema.js";
import { kvCounter, kvCursor, kvJson, kvText, kvTtlCache } from "./utils/state.js";
import type { KvCounter, KvCursor, KvJson, KvText, KvTtlCache } from "./utils/state.js";
import {
  decodeBackupCreate,
  decodeBackupList,
  decodeBackupVerify,
  decodeConsolePage,
  decodeContentInstall,
  decodeContentList,
  decodeDeleted,
  decodeFileList,
  decodeFileRead,
  decodeFileWrite,
  decodeJavaInstall,
  decodeJavaList,
  decodeKvGet,
  decodeKvList,
  decodeNetResponse,
  decodeRemoved,
  decodeRemovedCount,
  decodeSecretGet,
  decodeServerList,
  decodeServerProperties,
  decodeServerStatus,
  decodeServerVersion,
  decodeStopOutcome,
  encodeBase64,
  expectAck,
} from "./wire.js";

/** 能力名，与内核的 `Capability` 同义；直接指向生成契约以保持依赖单向 */
type Capability = OpenCapabilityName;

/** 服务器名 */
export type ServerName = string;

/** 通用能力调用器（Connection.call 的类型投影） */
export interface CapabilityCaller {
  readonly call: (capability: string, method: string, input: unknown, timeoutMs?: number) => Promise<unknown>;
}

/** 出站契约调用选项（plugins.md §6.4 / §12.4） */
export interface ContractCallOptions {
  /** 单播目标插件 id；广播时省略 */
  readonly target?: string;
  /** 嵌套调用必须原样带回宿主分配的 trace */
  readonly trace?: string;
  readonly timeoutMs?: number;
}

/** 插件间契约客户端 */
export interface ContractClient {
  call(contract: string, input: unknown, options?: ContractCallOptions): Promise<unknown>;
}

// ---------------------------------------------------------------------------
// 服务器只读
// ---------------------------------------------------------------------------

export interface ServerSummary {
  readonly name: string;
  readonly status: string;
  /** 已安装的版本；未安装为 null */
  readonly version: string | null;
}

export interface ServerStatusView {
  readonly status: string;
  readonly version: string | null;
  readonly players: number | null;
  readonly uptimeSecs: number | null;
}

/** 控制台一行的游标与文本 */
export interface ConsoleLineEntry {
  readonly cursor: number;
  readonly text: string;
}

export interface ConsolePage {
  /** 本页行；每行带自己的游标 */
  readonly lines: readonly ConsoleLineEntry[];
  /** 下一页应传入的游标 */
  readonly next: number;
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
  list(prefix?: string, after?: string): Promise<readonly string[]>;
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

export type StopOutcome =
  | { readonly outcome: "stopped"; readonly elapsedMs: number }
  | { readonly outcome: "already_stopped" }
  | { readonly outcome: "timed_out"; readonly waitedMs: number };

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
  /** 创建时间（Unix 秒） */
  readonly createdAt: number;
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

export interface ContentInstallResult {
  readonly file: string;
  readonly size: number;
  readonly project: string;
  /** 宿主选定的版本号；无法确定时为 null */
  readonly version: string | null;
}

export interface ContentClient {
  list(server: ServerName, opts?: { readonly kind?: ContentKind; readonly identify?: boolean }): Promise<readonly ContentItem[]>;
  install(server: ServerName, project: string, opts?: { readonly version?: string; readonly kind?: ContentKind }): Promise<ContentInstallResult>;
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
  /** 创建时间（Unix 秒） */
  readonly createdAt: number;
  /** 更新时间（Unix 秒） */
  readonly updatedAt: number;
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

/** 限额视图（`welcome.limits` 的内部形态） */
export interface LimitsView {
  readonly inflight: number;
  readonly frameBytes: number;
  readonly eventQueue: number;
  readonly logLinesPerSec: number;
}

/** 作用域视图（`welcome.scope`） */
export interface ScopeView {
  readonly kind: string;
  readonly server?: string;
}

/**
 * 宿主界面语言标签（BCP 47 主标签，当前为 `zh` / `en`）。
 *
 * 用字符串而不是联合：宿主新增语言时旧 SDK 不应编译失败。判断请用
 * `pickLocale` / `isZhLocale`，不要直接比较。
 */
export type Locale = string;

/** 是否中文界面；空值与无法识别的标签按非中文处理。 */
export function isZhLocale(locale: string): boolean {
  return locale.trim().toLowerCase().startsWith("zh");
}

/** 按宿主语言二选一。 */
export function pickLocale(locale: string, zh: string, en: string): string {
  return isZhLocale(locale) ? zh : en;
}

/** `ctx.storage` 门面；仅在声明 `storage:kv` 时存在 */
export interface StorageFacade {
  /** 原始 client，需要未封装的读写时用 */
  readonly kv: KvClient;
  /** JSON 值（UTF-8 编码） */
  json<T>(prefix?: string): KvJson<T>;
  /** 纯文本值 */
  text(prefix?: string): KvText;
  /** 计数（非原子：单插件内串行调用时安全，并发请用 `pLimit(1)` 收敛） */
  counter(prefix?: string): KvCounter;
  /** 单调前进的游标 */
  cursor(key: string, initial?: number): KvCursor;
  /** 带 TTL 的持久缓存 */
  cache(prefix?: string): KvTtlCache;
}

/** 声明 `storage:kv` 时才出现的 `storage` 成员 */
type StorageMember<C extends readonly Capability[]> = "storage:kv" extends C[number]
  ? { readonly storage: StorageFacade }
  : unknown;

/**
 * 能力驱动的上下文（`CtxFor<capabilities, config>`）。
 *
 * 未声明的能力对应字段在类型上不存在；`log` 恒可用。`config` 的类型由
 * `config` 字段的 schema 推导。
 */
export type CtxFor<C extends readonly Capability[], Cfg extends SchemaLike = SchemaLike> = {
  [K in keyof CtxMembers as K extends C[number] ? K : never]: CtxMembers[K];
} & StorageMember<C> & {
  readonly log: LogClient;
  /** 插件间契约调用；声明 `consumes` 后由宿主路由 */
  readonly contract: ContractClient;
  readonly config: InferSchema<Cfg>;
  readonly sessionId: string;
  readonly limits: LimitsView;
  readonly scope: ScopeView;
  /** 宿主界面语言（`zh` / `en`），供插件做多语言文案 */
  readonly locale: Locale;
  /** 宿主版本 */
  readonly hostVersion: string;
  /** 已授予的能力（`welcome.granted` 与声明的交集） */
  readonly granted: readonly Capability[];
  /**
   * 该能力当前是否已授予。
   *
   * 未授予的成员访问会抛 `denied`，需要「缺能力就降级」的分支必须先问它，
   * 不能靠 `ctx["x"] === undefined` 判断。
   */
  has(capability: Capability): boolean;
};

/** 运行期构造上下文所需的环境（welcome 后会被改写） */
export interface CtxEnvironment {
  readonly caller: CapabilityCaller;
  /** 出站契约调用；与能力调用共用 inflight 限额 */
  readonly contractCaller: (contract: string, input: unknown, options?: ContractCallOptions) => Promise<unknown>;
  readonly config: () => unknown;
  sessionId: string;
  limits: LimitsView;
  scope: ScopeView;
  locale: Locale;
  /** 宿主版本；welcome 之前为空 */
  hostVersion: string;
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

  /**
   * 投递一个事件；返回是否有人处理。
   *
   * 单个 handler 抛错不影响其余 handler 与 `*` 兜底：错误收集后由调用方记录。
   * 串行扇出的一个副作用是首个失败会吞掉后续订阅者，这里必须隔离。
   */
  public async dispatch(kind: string, data: unknown, meta: EventMeta): Promise<boolean> {
    let handled = false;
    const failures: unknown[] = [];
    for (const target of [kind, "*"]) {
      const set = this.handlers.get(target);
      if (set === undefined) {
        continue;
      }
      for (const handler of [...set]) {
        handled = true;
        try {
          await handler(data, meta);
        } catch (err) {
          failures.push(err);
        }
      }
    }
    if (failures.length === 1) {
      throw failures[0];
    }
    if (failures.length > 1) {
      throw new AggregateError(failures, `${failures.length} event handlers failed for ${kind}`);
    }
    return handled;
  }
}

// ---------------------------------------------------------------------------
// 构造
// ---------------------------------------------------------------------------

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
      list: async () => decodeServerList(await call("server:read", "list", {})),
      status: async (server) => decodeServerStatus(await call("server:read", "status", { server })),
      version: async (server) => decodeServerVersion(await call("server:read", "version", { server })),
      properties: async (server) =>
        decodeServerProperties(await call("server:read", "properties", { server })),
    };
    members["server:read"] = { servers };
  }
  if (has("console:read")) {
    const consoleRead: ConsoleReadClient = {
      tail: async (server, cursor = null, maxLines = 200) =>
        decodeConsolePage(
          await call("console:read", "tail", { server, cursor, max_lines: maxLines }),
        ),
    };
    members["console:read"] = consoleRead;
  }
  if (has("console:write")) {
    const consoleWrite: ConsoleWriteClient = {
      sendLine: async (server, line) => {
        expectAck(await call("console:write", "send_line", { server, line }), "console:write.send_line");
      },
    };
    members["console:write"] = consoleWrite;
  }
  if (has("storage:kv")) {
    const kv: KvClient = {
      get: async (key) => decodeKvGet(await call("storage:kv", "get", { key })),
      set: async (key, value) => {
        expectAck(
          await call("storage:kv", "set", { key, value: encodeBase64(value) }),
          "storage:kv.set",
        );
      },
      delete: async (key) => decodeDeleted(await call("storage:kv", "delete", { key }), "storage:kv.delete"),
      list: async (prefix = "", after) =>
        decodeKvList(await call("storage:kv", "list", { prefix, after })),
    };
    members["storage:kv"] = kv;
    members["storage"] = {
      kv,
      json: <T,>(prefix = "") => kvJson<T>(kv, prefix),
      text: (prefix = "") => kvText(kv, prefix),
      counter: (prefix = "") => kvCounter(kv, prefix),
      cursor: (key, initial = 0) => kvCursor(kv, key, initial),
      cache: (prefix = "cache:") => kvTtlCache(kv, prefix),
    } satisfies StorageFacade;
  }
  if (has("secret:rw")) {
    const secrets: SecretClient = {
      get: async (key) => decodeSecretGet(await call("secret:rw", "get", { key })),
      set: async (key, value) => {
        // 线上是 base64，和 KV 一致；UTF-8 文本先编码
        expectAck(
          await call("secret:rw", "set", { key, value: encodeBase64(value) }),
          "secret:rw.set",
        );
      },
      delete: async (key) => decodeDeleted(await call("secret:rw", "delete", { key }), "secret:rw.delete"),
    };
    members["secret:rw"] = secrets;
  }
  if (has("net:outbound")) {
    const net: NetClient = {
      request: async (req) =>
        decodeNetResponse(
          await call("net:outbound", "request", {
            method: req.method ?? "GET",
            url: req.url,
            headers: req.headers ?? {},
            body: req.body === undefined || req.body === null ? null : encodeBase64(req.body),
            timeout_ms: req.timeoutMs,
          }),
        ),
    };
    members["net:outbound"] = net;
  }
  if (has("files:read")) {
    const filesRead: FilesReadClient = {
      read: async (server, path) => decodeFileRead(await call("files:read", "read", { server, path })),
      list: async (server, path) => decodeFileList(await call("files:read", "list", { server, path })),
    };
    members["files:read"] = filesRead;
  }
  if (has("files:write")) {
    const filesWrite: FilesWriteClient = {
      write: async (server, path, data, overwrite = false) =>
        decodeFileWrite(
          await call("files:write", "write", {
            server,
            path,
            data: encodeBase64(data),
            overwrite,
          }),
        ),
      remove: async (server, path) =>
        decodeRemoved(await call("files:write", "remove", { server, path }), "files:write.remove"),
    };
    members["files:write"] = filesWrite;
  }
  if (has("server:lifecycle")) {
    const lifecycle: LifecycleClient = {
      start: async (server) => {
        expectAck(await call("server:lifecycle", "start", { server }), "server:lifecycle.start");
      },
      stop: async (server, graceSecs) =>
        decodeStopOutcome(
          await call("server:lifecycle", "stop", { server, grace_secs: graceSecs }),
        ),
      restart: async (server, graceSecs) => {
        expectAck(
          await call("server:lifecycle", "restart", { server, grace_secs: graceSecs }),
          "server:lifecycle.restart",
        );
      },
    };
    members["server:lifecycle"] = lifecycle;
  }
  if (has("backup")) {
    const backup: BackupClient = {
      create: async (server, label) => decodeBackupCreate(await call("backup", "create", { server, label })),
      list: async (server) => decodeBackupList(await call("backup", "list", { server })),
      restore: async (server, snapshot) => {
        expectAck(await call("backup", "restore", { server, snapshot }), "backup.restore");
      },
      verify: async (server, snapshot) =>
        decodeBackupVerify(await call("backup", "verify", { server, snapshot })),
      prune: async (server, keep) =>
        decodeRemovedCount(await call("backup", "prune", { server, keep }), "backup.prune"),
      delete: async (server, snapshot) =>
        decodeDeleted(await call("backup", "delete", { server, snapshot }), "backup.delete"),
    };
    members["backup"] = backup;
  }
  if (has("content")) {
    const content: ContentClient = {
      list: async (server, opts) =>
        decodeContentList(
          await call("content", "list", { server, kind: opts?.kind, identify: opts?.identify }),
        ),
      install: async (server, project, opts) =>
        decodeContentInstall(
          await call("content", "install", {
            server,
            project,
            version: opts?.version,
            kind: opts?.kind,
          }),
        ),
      remove: async (server, selector, kind) =>
        decodeRemovedCount(
          await call("content", "remove", { server, selector, kind }),
          "content.remove",
        ),
    };
    members["content"] = content;
  }
  if (has("java")) {
    const java: JavaClient = {
      list: async () => decodeJavaList(await call("java", "list", {})),
      install: async (major) => decodeJavaInstall(await call("java", "install", { major })),
      remove: async (path) =>
        decodeRemoved(await call("java", "remove", { path }), "java.remove"),
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

  const contract: ContractClient = {
    call: async (name, input, options) => await env.contractCaller(name, input, options),
  };

  const ctx = {
    ...members,
    contract,
    log,
    get config(): unknown {
      return env.config();
    },
    sessionId: env.sessionId,
    limits: env.limits,
    scope: env.scope,
    locale: env.locale,
    hostVersion: env.hostVersion,
    granted: Object.freeze([...(capabilities as readonly Capability[])]),
    has,
  };
  // 声明过但未授予的能力：访问即 denied，而不是静默 undefined（§8.4 不降级）
  for (const capability of ungranted) {
    Object.defineProperty(ctx, capability, {
      enumerable: false,
      configurable: true,
      get() {
        throw new PluginError("denied", `${capability} capability was not granted`);
      },
    });
    // `storage` 是 `storage:kv` 的便利门面，未授予时同样 denied
    if (capability === "storage:kv") {
      Object.defineProperty(ctx, "storage", {
        enumerable: false,
        configurable: true,
        get() {
          throw new PluginError("denied", "storage:kv capability was not granted");
        },
      });
    }
  }
  return ctx as CtxFor<C, Cfg>;
}
