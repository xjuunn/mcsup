/**
 * 通用基础工具（纯函数）
 *
 * 无副作用、不接 `ctx`、不依赖能力 client。插件作者可直接 import，
 * 避免每个插件重复实现同样的片段。
 */

/** 等待 `ms` 毫秒。宿主 shutdown 时由 SDK 层的取消机制处理，这里只做简单等待。 */
export function sleep(ms: number): Promise<void> {
  return new Promise<void>((resolve) => {
    setTimeout(resolve, ms);
  });
}

/** 把 `n` 限制在 `[min, max]` 内。NaN 视为 `min`。 */
export function clamp(n: number, min: number, max: number): number {
  if (Number.isNaN(n)) return min;
  return Math.min(Math.max(n, min), max);
}

/** 把 `items` 切成每片至多 `size` 个的数组；`size` 必须 >= 1。 */
export function chunk<T>(items: readonly T[], size: number): T[][] {
  if (!Number.isInteger(size) || size < 1) {
    throw new RangeError(`chunk size must be a positive integer, got ${size}`);
  }
  const out: T[][] = [];
  for (let i = 0; i < items.length; i += size) {
    out.push([...items.slice(i, i + size)]);
  }
  return out;
}

/** 去重（按 SameValueZero）；保持首次出现的顺序。 */
export function unique<T>(items: readonly T[]): T[] {
  return [...new Set(items)];
}

/**
 * 截断字符串到至多 `max` 个码点，超出部分替换为 `suffix`。
 *
 * 按 Unicode 码点而非 UTF-16 码元计数，避免把代理对截成半个字符。
 */
export function truncate(text: string, max: number, suffix = "…"): string {
  if (!Number.isInteger(max) || max < 1) {
    throw new RangeError(`truncate max must be a positive integer, got ${max}`);
  }
  const cps = [...text];
  if (cps.length <= max) return text;
  const keep = Math.max(max - [...suffix].length, 0);
  return cps.slice(0, keep).join("") + suffix;
}

/**
 * 生成一个不含密级信息的随机 ID（`crypto.randomUUID` 的简写形式，8 字节 hex）。
 * 仅用于去重键 / 关联标识，禁止当 token 用（token 应走 `secret:rw`）。
 */
export function randomId(): string {
  // Web Crypto 在 Node >= 20 全局可用
  const bytes = new Uint8Array(8);
  globalThis.crypto.getRandomValues(bytes);
  return [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** ISO-8601 UTC 时间戳（毫秒精度），日志与 KV 记录的统一时间格式。 */
export function nowIso(): string {
  return new Date().toISOString();
}
