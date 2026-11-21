/**
 * Hooks 层：造「行为」块（sdk-design.md §4.3）
 *
 * 统一签名 `useX(ctx, options)`，返回承担脏活的能力对象（轮询、游标、重试、
 * 缓存）。显式接收 `ctx`，不在模块级注册任何东西；缺少所需能力时立即抛错。
 */

import type { ConsoleLine } from "./builders.js";
import type { ConsoleReadClient, EventsClient, KvClient, LogClient } from "./ctx.js";
import { PluginError } from "./errors.js";
import { kvTtlCache } from "./utils/state.js";
import { withRetry } from "./utils/retry.js";

/**
 * Hook 只依赖上下文的公共部分：日志、配置、限额与作用域。
 * 能力成员按需取，未授予即报 denied。`Cfg` 与 `ctx.config` 的推导类型一致，
 * 自定义 hook 可写 `HookContext<typeof ctx.config>` 保留配置类型。
 */
export interface HookContext<Cfg = unknown> {
  readonly log: LogClient;
  readonly config: Cfg;
  readonly sessionId: string;
  readonly limits: {
    readonly inflight: number;
    readonly frameBytes: number;
    readonly eventQueue: number;
    readonly logLinesPerSec: number;
  };
  readonly scope: { readonly kind: string; readonly server?: string };
}

/** 从上下文里取能力成员；未声明该能力时为 undefined */
function member<T>(ctx: object, key: string): T | undefined {
  return (ctx as Record<string, T | undefined>)[key];
}

function requireObject(ctx: unknown, hook: string): object {
  if (typeof ctx !== "object" || ctx === null) {
    throw new PluginError("internal", `${hook} requires a plugin context`);
  }
  return ctx;
}

function requireMember<T>(ctx: object, key: string, hook: string): T {
  const value = member<T>(ctx, key);
  if (value === undefined) {
    // 未授予能力不降级（plugins.md §8.4）
    throw new PluginError("denied", `${hook} requires the ${key} capability`);
  }
  return value;
}

function logOf(ctx: object): LogClient {
  const log = member<LogClient>(ctx, "log");
  if (log === undefined) {
    throw new PluginError("internal", "plugin context has no log client");
  }
  return log;
}

// ---------------------------------------------------------------------------
// useConsoleTail：轮询游标 + 退避重连
// ---------------------------------------------------------------------------

export interface ConsoleTailOptions {
  /** 从当前尾部开始；默认 true */
  readonly sinceTail?: boolean;
  /** 轮询间隔（毫秒），默认 1000 */
  readonly intervalMs?: number;
  /** 每次取行数，1..500，默认 200 */
  readonly batch?: number;
  /** 初始游标；省略时由 sinceTail 决定 */
  readonly cursor?: number;
}

export type ConsoleTailHandler = (line: ConsoleLine, server: string) => void | Promise<void>;

/** 控制台尾部订阅器 */
export interface ConsoleTail {
  /** 开始轮询指定服务器；重复调用会替换现有订阅 */
  subscribe(server: string, handler: ConsoleTailHandler): void;
  /** 取消订阅，停止轮询 */
  stop(server?: string): void;
  /** 当前游标（无订阅时为 null） */
  cursor(server: string): number | null;
  /** 已丢弃的行数（宿主侧环形缓冲溢出） */
  dropped(server: string): number;
}

/**
 * 控制台尾部：内部处理游标、批大小、轮询间隔与失败退避。
 *
 * 要求 `console:read` 能力。
 */
export function useConsoleTail(
  ctx: HookContext,
  options: ConsoleTailOptions = {},
): ConsoleTail {
  const context = requireObject(ctx, "useConsoleTail");
  const consoleRead = requireMember<ConsoleReadClient>(context, "console:read", "useConsoleTail");
  const log = logOf(context);
  const interval = options.intervalMs ?? 1000;
  const batch = Math.min(Math.max(options.batch ?? 200, 1), 500);

  interface State {
    server: string;
    handler: ConsoleTailHandler;
    cursor: number | null;
    dropped: number;
    timer: NodeJS.Timeout | undefined;
    running: boolean;
    stopped: boolean;
    backoff: number;
  }
  const states = new Map<string, State>();

  const pump = async (state: State): Promise<void> => {
    if (state.stopped || state.running) {
      return;
    }
    state.running = true;
    try {
      const page = await consoleRead.tail(state.server, state.cursor, batch);
      for (const text of page.lines) {
        if (state.stopped) {
          // 游标尚未推进：下次订阅会重放整页（至少一次），而不是丢行
          return;
        }
        await state.handler({ text, server: state.server, at: Date.now() }, state.server);
      }
      // 整页处理完才提交游标；handler 抛错时保持原游标，重试不会跳过剩余行
      state.cursor = page.cursor;
      state.dropped = page.dropped;
      state.backoff = interval;
    } catch (err) {
      if (state.stopped) {
        return;
      }
      const error = err instanceof PluginError ? err : new PluginError("internal", String(err));
      if (error.code === "denied") {
        // 能力被撤销：停止而不是空转重试（§8.4 不降级）
        log.warn(`console tail stopped: ${error.message}`);
        state.stopped = true;
        return;
      }
      state.backoff = Math.min(state.backoff * 2, Math.max(interval * 30, 30_000));
    } finally {
      state.running = false;
      if (!state.stopped) {
        state.timer = setTimeout(() => {
          void pump(state);
        }, state.backoff);
        state.timer.unref();
      }
    }
  };

  const tail: ConsoleTail = {
    subscribe(server, handler) {
      tail.stop(server);
      const state: State = {
        server,
        handler,
        cursor: options.cursor ?? (options.sinceTail === false ? 0 : null),
        dropped: 0,
        timer: undefined,
        running: false,
        stopped: false,
        backoff: interval,
      };
      states.set(server, state);
      void pump(state);
    },
    stop(server) {
      const targets = server === undefined ? [...states.values()] : [states.get(server)].filter((s) => s !== undefined);
      for (const state of targets) {
        state.stopped = true;
        if (state.timer !== undefined) {
          clearTimeout(state.timer);
        }
      }
      if (server === undefined) {
        states.clear();
      } else {
        states.delete(server);
      }
    },
    cursor(server) {
      return states.get(server)?.cursor ?? null;
    },
    dropped(server) {
      return states.get(server)?.dropped ?? 0;
    },
  };
  return tail;
}

// ---------------------------------------------------------------------------
// useRetry：可重试错误自动重试
// ---------------------------------------------------------------------------

export interface RetryOptions {
  /** 最大尝试次数（含首次），默认 3 */
  readonly attempts?: number;
  /** 基准退避毫秒，默认 500 */
  readonly baseMs?: number;
  /** 最大退避毫秒，默认 30_000 */
  readonly maxMs?: number;
  /** 抖动比例（0..1），默认 0.2 */
  readonly jitter?: number;
  /** 额外判定：返回 true 才重试 */
  readonly retryOn?: (error: PluginError) => boolean;
}

/** 重试包装器 */
export interface Retry {
  <T>(fn: () => Promise<T> | T): Promise<T>;
}

/**
 * 可重试错误（timeout / unavailable / crashed）自动退避重试；
 * 其他错误立即抛出，`code` 保持不变。
 *
 * 退避与抖动实现复用 `utils/retry.ts` 的 `withRetry`，避免两套算法漂移。
 */
export function useRetry(
  ctx: HookContext,
  options: RetryOptions = {},
): Retry {
  requireObject(ctx, "useRetry");
  return async function retry<T>(fn: () => Promise<T> | T): Promise<T> {
    return await withRetry(fn, {
      attempts: options.attempts,
      baseMs: options.baseMs,
      maxMs: options.maxMs,
      jitter: options.jitter,
      retryOn: options.retryOn,
    });
  };
}

// ---------------------------------------------------------------------------
// useKvCache：JSON 值的本地缓存
// ---------------------------------------------------------------------------

export interface KvCacheOptions {
  /** 键前缀，默认 `cache:` */
  readonly prefix?: string;
  /** 缓存有效期毫秒；省略表示不过期 */
  readonly ttlMs?: number;
}

export interface KvCache {
  get<T>(key: string): Promise<T | null>;
  set(key: string, value: unknown): Promise<void>;
  delete(key: string): Promise<void>;
  /** 读缓存，未命中时调用 loader 并写回 */
  remember<T>(key: string, loader: () => Promise<T> | T): Promise<T>;
}

/** 要求 `storage:kv` 能力 */
export function useKvCache(
  ctx: HookContext,
  options: KvCacheOptions = {},
): KvCache {
  const context = requireObject(ctx, "useKvCache");
  const kv = requireMember<KvClient>(context, "storage:kv", "useKvCache");
  const cache = kvTtlCache(kv, options.prefix ?? "cache:");
  return {
    get: <T>(key: string) => cache.get<T>(key),
    set: async (key, value) => {
      await cache.set(key, value, options.ttlMs);
    },
    delete: async (key) => {
      await cache.delete(key);
    },
    remember: <T>(key: string, loader: () => Promise<T> | T) =>
      cache.getOrLoad(key, options.ttlMs, async () => await loader()),
  };
}

// ---------------------------------------------------------------------------
// useEvents：类型化事件订阅
// ---------------------------------------------------------------------------

/**
 * 事件订阅钩子：要求 `events` 能力；返回取消函数。
 */
export function useEvents<D = unknown>(
  ctx: HookContext,
  kind: string,
  handler: (data: D, meta: { readonly kind: string; readonly server?: string; readonly at: string }) => void | Promise<void>,
): () => void {
  const context = requireObject(ctx, "useEvents");
  const events = requireMember<EventsClient>(context, "events", "useEvents");
  return events.on(kind, (data, meta) => handler(data as D, meta));
}
