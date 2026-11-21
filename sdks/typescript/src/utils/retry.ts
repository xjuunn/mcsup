/**
 * 网络与重试工具（纯函数，不接 ctx）
 *
 * 与 `useRetry`（hooks 层，接 `HookContext`）互补：这里是无需 ctx 的
 * 可组合版本，可直接在任意回调里使用。
 */

import { PluginError } from "../errors.js";
import { sleep } from "./general.js";

/** 重试选项 */
export interface WithRetryOptions {
  /** 最大尝试次数（含首次），默认 3 */
  readonly attempts?: number;
  /** 首次退避毫秒，默认 500；第 n 次失败后延迟 `baseMs * 2^(n-1)` */
  readonly baseMs?: number;
  /** 退避上限毫秒，默认 30_000 */
  readonly maxMs?: number;
  /** 抖动比例（0..1），实际延迟 = 计算 delay * (1 + jitter * random)，默认 0.2 */
  readonly jitter?: number;
  /** 额外的重试判定；返回 false 时即使 retryable 也放弃 */
  readonly retryOn?: (error: PluginError) => boolean;
}

/**
 * 带指数退避 + 抖动的重试。
 *
 * 只对 `PluginError.retryable === true` 的错误重试（timeout / unavailable /
 * crashed，见 errors.ts `isRetryable`）；其他错误立即抛出。
 */
export async function withRetry<T>(fn: () => Promise<T> | T, options: WithRetryOptions = {}): Promise<T> {
  const attempts = options.attempts ?? 3;
  const baseMs = options.baseMs ?? 500;
  const maxMs = options.maxMs ?? 30_000;
  const jitter = options.jitter ?? 0.2;
  if (!Number.isInteger(attempts) || attempts < 1) {
    throw new RangeError(`attempts must be a positive integer, got ${attempts}`);
  }
  for (let attempt = 1; ; attempt += 1) {
    try {
      return await fn();
    } catch (err) {
      const error = err instanceof PluginError ? err : new PluginError("internal", String(err));
      const retryable = error.retryable && (options.retryOn?.(error) ?? true);
      if (!retryable || attempt >= attempts) {
        throw error;
      }
      const delay = Math.min(baseMs * 2 ** (attempt - 1), maxMs);
      await sleep(Math.round(delay * (1 + jitter * Math.random())));
    }
  }
}

/**
 * 并发限流器：最多 `limit` 个任务同时执行。
 *
 * ```ts
 * const limit = pLimit(4);
 * await Promise.all(urls.map((u) => limit(() => net.request({ url: u }))));
 * ```
 */
export function pLimit(limit: number): <T>(fn: () => Promise<T>) => Promise<T> {
  if (!Number.isInteger(limit) || limit < 1) {
    throw new RangeError(`limit must be a positive integer, got ${limit}`);
  }
  let active = 0;
  const queue: Array<() => void> = [];

  const next = (): void => {
    active -= 1;
    const resume = queue.shift();
    if (resume !== undefined) {
      resume();
    }
  };

  return async <T>(fn: () => Promise<T>): Promise<T> => {
    if (active >= limit) {
      await new Promise<void>((resolve) => {
        queue.push(resolve);
      });
    }
    active += 1;
    try {
      return await fn();
    } finally {
      next();
    }
  };
}

/** `mapLimit`：对 `items` 逐个套 `pLimit`，保持结果顺序。 */
export async function mapLimit<T, R>(
  items: readonly T[],
  limit: number,
  fn: (item: T, index: number) => Promise<R>,
): Promise<R[]> {
  const l = pLimit(limit);
  return Promise.all(items.map((item, index) => l(() => fn(item, index))));
}

/** 把 query 对象拼到 URL 上；值为 null / undefined 的键跳过，值 encodeURIComponent。 */
export function buildUrl(base: string, query?: Readonly<Record<string, string | number | boolean | null>>): string {
  if (query === undefined) {
    return base;
  }
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) {
    if (value === null) {
      continue;
    }
    search.set(key, String(value));
  }
  const qs = search.toString();
  if (qs === "") {
    return base;
  }
  return base + (base.includes("?") ? "&" : "?") + qs;
}
