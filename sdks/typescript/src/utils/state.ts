/**
 * 状态存储工具
 *
 * 包装 `ctx["storage:kv"]` 的常用模式：JSON 存取、计数器、游标、TTL 缓存。
 *
 * 按 SDK 分层约束（sdk-design.md §5），工厂函数**显式接 `KvClient`**，
 * 不做隐式注册、不持有模块级状态；未授予 `storage:kv` 时调用会在
 * 取 client 处直接 denied，与能力模型一致。
 */

import type { KvClient } from "../ctx.js";

const textDecoder = new TextDecoder();
const textEncoder = new TextEncoder();

/**
 * JSON 存取视图：KV 的值本体是 bytes，这里约定「值 = UTF-8 JSON」。
 * `get` 解析失败按 `null` 处理（视为未写入），不抛错。
 */
export interface KvJson<T> {
  get(key: string): Promise<T | null>;
  set(key: string, value: T): Promise<void>;
  delete(key: string): Promise<boolean>;
}

/** JSON 存取视图工厂。 */
export function kvJson<T>(kv: KvClient, prefix = ""): KvJson<T> {
  const full = (key: string): string => prefix + key;
  return {
    async get(key: string): Promise<T | null> {
      const raw = await kv.get(full(key));
      if (raw === null) {
        return null;
      }
      try {
        return JSON.parse(textDecoder.decode(raw)) as T;
      } catch {
        return null;
      }
    },
    async set(key: string, value: T): Promise<void> {
      const encoded = JSON.stringify(value);
      // `JSON.stringify` 对 undefined / 函数 / symbol 返回 undefined，
      // 交给 TextEncoder 会写成字符串 "undefined"，读回时解析失败被当成未写入
      if (encoded === undefined) {
        throw new TypeError(`kvJson cannot store a value that JSON cannot represent (key ${key})`);
      }
      await kv.set(full(key), textEncoder.encode(encoded));
    },
    delete(key: string): Promise<boolean> {
      return kv.delete(full(key));
    },
  };
}

/** 文本存取视图：KV 的字节按 UTF-8 编解码，不存在返回 `null` */
export interface KvText {
  get(key: string): Promise<string | null>;
  set(key: string, value: string): Promise<void>;
  delete(key: string): Promise<boolean>;
}

/** 文本存取视图工厂。 */
export function kvText(kv: KvClient, prefix = ""): KvText {
  const full = (key: string): string => prefix + key;
  return {
    async get(key: string): Promise<string | null> {
      const raw = await kv.get(full(key));
      return raw === null ? null : textDecoder.decode(raw);
    },
    async set(key: string, value: string): Promise<void> {
      await kv.set(full(key), textEncoder.encode(value));
    },
    delete(key: string): Promise<boolean> {
      return kv.delete(full(key));
    },
  };
}

/** 原子性说明：KV 无事务，「读-改-写」在单插件内串行调用时安全；并发场景用 `pLimit(1)` 收敛。 */
export interface KvCounter {
  /** 当前值（不存在为 0） */
  get(key: string): Promise<number>;
  /** 加 `delta`（可为负），返回新值 */
  add(key: string, delta?: number): Promise<number>;
  /** 加 1 并返回新值 */
  incr(key: string): Promise<number>;
  /** 重置 */
  reset(key: string): Promise<void>;
}

/** 计数器：`kvCounter(ctx["storage:kv"], "stats:")`。 */
export function kvCounter(kv: KvClient, prefix = ""): KvCounter {
  const full = (key: string): string => prefix + key;
  const decode = (raw: Uint8Array | null): number => {
    if (raw === null) {
      return 0;
    }
    // 用 `Number` 而不是 `parseInt`：`"1.5"` 被 parseInt 静默截成 1，
    // 之后每次 get/add 的结果会比写入值小，且没有报错。
    const n = Number(textDecoder.decode(raw));
    return Number.isSafeInteger(n) ? n : 0;
  };
  const add = async (key: string, delta = 1): Promise<number> => {
    if (!Number.isInteger(delta)) {
      throw new TypeError(`counter delta must be an integer, got ${delta}`);
    }
    const next = decode(await kv.get(full(key))) + delta;
    if (!Number.isSafeInteger(next)) {
      throw new RangeError(`counter ${key} is out of the safe integer range: ${next}`);
    }
    await kv.set(full(key), String(next));
    return next;
  };
  return {
    async get(key) {
      return decode(await kv.get(full(key)));
    },
    add,
    incr(key) {
      return add(key, 1);
    },
    async reset(key) {
      await kv.set(full(key), "0");
    },
  };
}

/**
 * 游标：tail / 分页拉取的「上次读到哪里」。
 *
 * 游标值为非负整数（console tail offset、平台消息 seq 等），
 * `advance` 只允许前进，防止异常回调把游标回拨造成重复消费。
 */
export interface KvCursor {
  get(): Promise<number>;
  /** 前进到 `next`；`next` 小于当前值时忽略，返回生效后的游标 */
  advance(next: number): Promise<number>;
}

/** 游标（单键版，多游标请传不同 key 或用 prefix 前缀化）。 */
export function kvCursor(kv: KvClient, key: string, initial = 0): KvCursor {
  const decode = (raw: Uint8Array | null): number => {
    if (raw === null) {
      return initial;
    }
    const n = Number.parseInt(textDecoder.decode(raw), 10);
    return Number.isSafeInteger(n) && n >= 0 ? n : initial;
  };
  return {
    async get() {
      return decode(await kv.get(key));
    },
    async advance(next) {
      const current = decode(await kv.get(key));
      if (next <= current) {
        return current;
      }
      await kv.set(key, String(next));
      return next;
    },
  };
}

/** TTL 缓存条目（落 KV 的编码形态） */
interface CacheEntry {
  readonly value: unknown;
  /** 过期时间戳；`null` 表示不过期。旧条目可能没有该字段，按不过期处理。 */
  readonly expiresAt: number | null;
}

/**
 * TTL 缓存：值 + 过期时间一起存进 KV，进程重启后缓存仍然生效。
 *
 * 过期条目在读取时惰性删除；`purge` 可主动清理一个前缀下的过期条目。
 * 注意：这是「缓存」不是锁，跨插件实例并发写同一键时后写者胜。
 */
export interface KvTtlCache {
  get<T>(key: string): Promise<T | null>;
  /** `ttlMs` 省略或为 `null` 时不过期 */
  set<T>(key: string, value: T, ttlMs?: number | null): Promise<void>;
  /** 读穿：未命中时执行 `loader` 并写入；`ttlMs` 省略或为 `null` 时不过期 */
  getOrLoad<T>(key: string, ttlMs: number | null | undefined, loader: () => Promise<T>): Promise<T>;
  /** 删除 */
  delete(key: string): Promise<boolean>;
  /** 清理前缀下已过期的条目，返回清理数量 */
  purge(prefix?: string): Promise<number>;
}

/** TTL 缓存。`cachePrefix` 下所有键被本缓存独占。 */
export function kvTtlCache(kv: KvClient, cachePrefix = "cache:"): KvTtlCache {
  const json = kvJson<CacheEntry>(kv, cachePrefix);
  const expired = (entry: CacheEntry): boolean =>
    entry.expiresAt !== null && entry.expiresAt !== undefined && entry.expiresAt <= Date.now();

  /**
   * 读条目；过期条目惰性删除后按未命中处理。
   *
   * `null` 只表示「键不存在」或「已过期」，与「缓存值本身是 null」区分开：
   * 缓存的 `value` 可以是 `null`，不能和未命中混为一谈。
   */
  const readEntry = async (key: string): Promise<CacheEntry | null> => {
    const entry = await json.get(key);
    if (entry === null) {
      return null;
    }
    if (expired(entry)) {
      await json.delete(key);
      return null;
    }
    return entry;
  };

  const get = async <T>(key: string): Promise<T | null> => {
    const entry = await readEntry(key);
    return entry === null ? null : (entry.value as T);
  };

  const set = async <T>(key: string, value: T, ttlMs?: number | null): Promise<void> => {
    let expiresAt: number | null = null;
    if (ttlMs !== undefined && ttlMs !== null) {
      if (!Number.isInteger(ttlMs) || ttlMs < 1) {
        throw new RangeError(`ttlMs must be a positive integer, got ${ttlMs}`);
      }
      expiresAt = Date.now() + ttlMs;
    }
    await json.set(key, { value, expiresAt });
  };

  return {
    get,
    set,
    async getOrLoad<T>(key: string, ttlMs: number | null | undefined, loader: () => Promise<T>): Promise<T> {
      const entry = await readEntry(key);
      if (entry !== null) {
        return entry.value as T;
      }
      const value = await loader();
      await set(key, value, ttlMs);
      return value;
    },
    delete(key) {
      return json.delete(key);
    },
    async purge(prefix = "") {
      let removed = 0;
      // `kv.list` 单次至多 1000 条：用上一页的最后一个键作为 `after` 续页，
      // 只查一次会漏掉排在后面的过期键。
      const base = cachePrefix + prefix;
      let after: string | undefined;
      for (;;) {
        const keys = await kv.list(base, after);
        if (keys.length === 0) {
          return removed;
        }
        for (const full of keys) {
          const key = full.slice(cachePrefix.length);
          const entry = await json.get(key);
          if (entry !== null && expired(entry) && (await json.delete(key))) {
            removed += 1;
          }
        }
        const last = keys[keys.length - 1];
        // 旧宿主不认识 `after`，会原样返回第一页；不前进就停下，避免死循环
        if (last === after) {
          return removed;
        }
        after = last;
      }
    },
  };
}
