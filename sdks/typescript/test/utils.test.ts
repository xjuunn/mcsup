/** utils 单元测试：纯函数 + 显式接能力 client 的状态工具 */
import { describe, expect, it, vi } from "vitest";

import {
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
  pLimit,
  randomId,
  sanitizeChat,
  sleep,
  stripColorCodes,
  truncate,
  unique,
  withRetry,
} from "../src/index.js";
import { PluginError } from "../src/errors.js";
import type { KvClient } from "../src/ctx.js";

// ---------------------------------------------------------------------------
// general
// ---------------------------------------------------------------------------

describe("general", () => {
  it("clamp 限制范围并处理 NaN", () => {
    expect(clamp(5, 1, 3)).toBe(3);
    expect(clamp(-1, 1, 3)).toBe(1);
    expect(clamp(2, 1, 3)).toBe(2);
    expect(clamp(Number.NaN, 1, 3)).toBe(1);
  });

  it("chunk 切片并拒绝非法 size", () => {
    expect(chunk([1, 2, 3, 4, 5], 2)).toEqual([[1, 2], [3, 4], [5]]);
    expect(chunk([], 3)).toEqual([]);
    expect(() => chunk([1], 0)).toThrow(RangeError);
  });

  it("unique 去重且保持顺序", () => {
    expect(unique([3, 1, 3, 2, 1])).toEqual([3, 1, 2]);
  });

  it("truncate 按码点截断并加后缀", () => {
    // 结果总长（含后缀）不超过 max
    expect(truncate("abcdef", 3)).toBe("ab…");
    expect(truncate("abc", 5)).toBe("abc");
    // 代理对不被截半：emoji 算 1 个码点
    const s = "😀".repeat(5);
    expect(truncate(s, 3)).toBe("😀😀…");
    expect(() => truncate("x", 0)).toThrow(RangeError);
    // 后缀本身达到或超过上限时不能把结果撑破
    expect(truncate("abcdef", 2, "……")).toBe("……");
    expect(truncate("abcdef", 1, "……")).toBe("…");
    expect(truncate("abcdef", 4, "……")).toBe("ab……");
  });

  it("randomId 返回 16 位 hex 且不重复", () => {
    const a = randomId();
    const b = randomId();
    expect(a).toMatch(/^[0-9a-f]{16}$/);
    expect(a).not.toBe(b);
  });

  it("sleep 等待后 resolve", async () => {
    const start = Date.now();
    await sleep(20);
    expect(Date.now() - start).toBeGreaterThanOrEqual(15);
  });
});

// ---------------------------------------------------------------------------
// retry / 并发
// ---------------------------------------------------------------------------

describe("withRetry", () => {
  it("retryable 错误按次数重试后成功", async () => {
    let calls = 0;
    const result = await withRetry(
      () => {
        calls += 1;
        if (calls < 3) throw new PluginError("unavailable", "boom");
        return "ok";
      },
      { baseMs: 1, maxMs: 5, jitter: 0 },
    );
    expect(result).toBe("ok");
    expect(calls).toBe(3);
  });

  it("不可重试错误立即抛出", async () => {
    let calls = 0;
    await expect(
      withRetry(
        () => {
          calls += 1;
          throw new PluginError("denied", "no");
        },
        { attempts: 3, baseMs: 1 },
      ),
    ).rejects.toThrow("no");
    expect(calls).toBe(1);
  });

  it("attempts 用尽后抛最后一个错误", async () => {
    let calls = 0;
    await expect(
      withRetry(
        () => {
          calls += 1;
          throw new PluginError("timeout", `t${calls}`);
        },
        { attempts: 2, baseMs: 1, jitter: 0 },
      ),
    ).rejects.toThrow("t2");
    expect(calls).toBe(2);
  });

  it("retryOn 返回 false 时放弃重试", async () => {
    let calls = 0;
    await expect(
      withRetry(
        () => {
          calls += 1;
          throw new PluginError("unavailable", "no more");
        },
        { attempts: 3, retryOn: () => false, baseMs: 1 },
      ),
    ).rejects.toThrow("no more");
    expect(calls).toBe(1);
  });

  it("非 PluginError 的异常包装为 internal（不可重试）", async () => {
    let calls = 0;
    await expect(
      withRetry(() => {
        calls += 1;
        throw new Error("plain");
      }),
    ).rejects.toThrow("plain");
    expect(calls).toBe(1);
  });
});

describe("pLimit / mapLimit", () => {
  it("pLimit 限制并发峰值", async () => {
    const limit = pLimit(2);
    let active = 0;
    let peak = 0;
    const task = async (): Promise<void> => {
      active += 1;
      peak = Math.max(peak, active);
      await sleep(10);
      active -= 1;
    };
    await Promise.all(Array.from({ length: 6 }, () => limit(task)));
    expect(peak).toBe(2);
  });

  it("mapLimit 保持结果顺序", async () => {
    const out = await mapLimit([1, 2, 3, 4], 2, async (n) => n * 10);
    expect(out).toEqual([10, 20, 30, 40]);
  });

  it("拒绝非法 limit", () => {
    expect(() => pLimit(0)).toThrow(RangeError);
  });
});

describe("buildUrl", () => {
  it("拼接 query 并跳过 null", () => {
    expect(buildUrl("https://x/api", { a: 1, b: "y", c: null })).toBe("https://x/api?a=1&b=y");
  });
  it("已有 query 用 & 续接", () => {
    expect(buildUrl("https://x/api?z=9", { a: 1 })).toBe("https://x/api?z=9&a=1");
  });
  it("无 query 原样返回", () => {
    expect(buildUrl("https://x/api")).toBe("https://x/api");
  });
});

// ---------------------------------------------------------------------------
// minecraft
// ---------------------------------------------------------------------------

describe("minecraft", () => {
  it("stripColorCodes 去掉颜色与格式码", () => {
    expect(stripColorCodes("§aGreen§r §lBold")).toBe("Green Bold");
    expect(stripColorCodes("plain")).toBe("plain");
    // 孤立的 § 不误伤后面的普通字符
    expect(stripColorCodes("a§zb")).toBe("a§zb");
  });

  it("legacyColorCodes 把 &x 转成 §x", () => {
    expect(legacyColorCodes("&aGreen&rx")).toBe("§aGreen§rx");
    expect(legacyColorCodes("mark & not code")).toBe("mark & not code");
  });

  it("sanitizeChat 折叠换行、去控制字符并截断", () => {
    expect(sanitizeChat("§ahello\r\nworld\u0000!")).toBe("hello world !");
    expect(sanitizeChat("x".repeat(300)).length).toBe(256);
    expect(sanitizeChat("  a   b  ")).toBe("a b");
  });

  it("isPlayerName 按 Java 版规则校验", () => {
    expect(isPlayerName("Steve_123")).toBe(true);
    expect(isPlayerName("")).toBe(false);
    expect(isPlayerName("a".repeat(17))).toBe(false);
    expect(isPlayerName("bad name")).toBe(false);
    expect(isPlayerName("§aFake")).toBe(false);
  });

  it("buildCommand 拼接并转义参数", () => {
    expect(buildCommand("say", ["hello", "two words"])).toBe('/say hello "two words"');
    expect(buildCommand("/kick", ['bad"name'])).toBe('/kick "bad\\"name"');
    expect(buildCommand("tell", [""])).toBe('/tell ""');
    expect(buildCommand("help")).toBe("/help");
  });

  it("buildCommand 拒绝换行与控制字符", () => {
    expect(() => buildCommand("say", ["a\nb"])).toThrow(TypeError);
    expect(() => buildCommand("sa\ny")).toThrow(TypeError);
    // 命令本体中间的控制字符同样会伪造第二行
    expect(() => buildCommand("say\u0000x")).toThrow(TypeError);
    expect(() => buildCommand("say\x7fx")).toThrow(TypeError);
    expect(() => buildCommand("say\rz")).toThrow(TypeError);
  });
});

// ---------------------------------------------------------------------------
// state（内存 KvClient 假件）
// ---------------------------------------------------------------------------

function memKv(): KvClient & { dump(): Map<string, string> } {
  const store = new Map<string, Uint8Array>();
  const dec = (raw: Uint8Array | string): string => (typeof raw === "string" ? raw : new TextDecoder().decode(raw));
  return {
    async get(key) {
      return store.get(key) ?? null;
    },
    async set(key, value) {
      store.set(key, typeof value === "string" ? new TextEncoder().encode(value) : value);
    },
    async delete(key) {
      return store.delete(key);
    },
    async list(prefix = "") {
      return [...store.keys()].filter((k) => k.startsWith(prefix)).sort();
    },
    dump() {
      return new Map([...store.entries()].map(([k, v]) => [k, dec(v)]));
    },
  };
}

describe("state", () => {
  it("kvJson 读写 JSON 且坏数据按 null 处理", async () => {
    const kv = memKv();
    const json = kvJson<{ a: number }>(kv, "j:");
    expect(await json.get("x")).toBeNull();
    await json.set("x", { a: 1 });
    expect(await json.get("x")).toEqual({ a: 1 });
    expect(kv.dump().get("j:x")).toBe(JSON.stringify({ a: 1 }));
    await kv.set("j:bad", "not json");
    expect(await json.get("bad")).toBeNull();
    expect(await json.delete("x")).toBe(true);
  });

  it("kvCounter 累加与重置", async () => {
    const counter = kvCounter(memKv(), "c:");
    expect(await counter.get("hits")).toBe(0);
    expect(await counter.incr("hits")).toBe(1);
    expect(await counter.add("hits", 4)).toBe(5);
    expect(await counter.add("hits", -2)).toBe(3);
    await counter.reset("hits");
    expect(await counter.get("hits")).toBe(0);
  });

  it("kvCursor 只前进不回拨", async () => {
    const cursor = kvCursor(memKv(), "cur", 100);
    expect(await cursor.get()).toBe(100);
    expect(await cursor.advance(50)).toBe(100);
    expect(await cursor.advance(120)).toBe(120);
    expect(await cursor.get()).toBe(120);
    // 坏数据回落到 initial
    const kv = memKv();
    await kv.set("cur", "-5");
    expect(await kvCursor(kv, "cur", 7).get()).toBe(7);
  });

  it("kvTtlCache 过期、读穿与惰性删除", async () => {
    vi.useFakeTimers();
    try {
      const kv = memKv();
      const cache = kvTtlCache(kv);
      await cache.set("k", "v1", 1000);
      expect(await cache.get("k")).toBe("v1");
      vi.advanceTimersByTime(1500);
      expect(await cache.get("k")).toBeNull();
      expect(kv.dump().has("cache:k")).toBe(false);

      let loads = 0;
      const loader = async (): Promise<string> => {
        loads += 1;
        return "loaded";
      };
      expect(await cache.getOrLoad("k2", 1000, loader)).toBe("loaded");
      expect(await cache.getOrLoad("k2", 1000, loader)).toBe("loaded");
      expect(loads).toBe(1);
      vi.advanceTimersByTime(1500);
      expect(await cache.getOrLoad("k2", 1000, loader)).toBe("loaded");
      expect(loads).toBe(2);
      await expect(cache.set("k", "v", 0)).rejects.toThrow(RangeError);
    } finally {
      vi.useRealTimers();
    }
  });

  it("kvTtlCache.purge 清理过期条目", async () => {
    vi.useFakeTimers();
    try {
      const kv = memKv();
      const cache = kvTtlCache(kv);
      await cache.set("a", 1, 500);
      await cache.set("b", 2, 5000);
      vi.advanceTimersByTime(1000);
      expect(await cache.purge()).toBe(1);
      expect(await cache.get("b")).toBe(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it("kvTtlCache 支持不过期条目", async () => {
    vi.useFakeTimers();
    try {
      const kv = memKv();
      const cache = kvTtlCache(kv);
      await cache.set("forever", "v");
      await cache.set("explicit", "v", null);
      vi.advanceTimersByTime(365 * 24 * 60 * 60 * 1000);
      expect(await cache.get("forever")).toBe("v");
      expect(await cache.get("explicit")).toBe("v");
      expect(await cache.getOrLoad("lazy", undefined, async () => "loaded")).toBe("loaded");
      expect(await cache.get("lazy")).toBe("loaded");
      // 不过期条目不该被 purge 清掉
      expect(await cache.purge()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("kvTtlCache 坏数据按未命中处理", async () => {
    const kv = memKv();
    await kv.set("cache:bad", "not json");
    expect(await kvTtlCache(kv).get("bad")).toBeNull();
  });

  it("state 工厂不依赖 this，可安全解构", async () => {
    const counter = kvCounter(memKv(), "d:");
    const { add, incr } = counter;
    expect(await add("k", 2)).toBe(2);
    expect(await incr("k")).toBe(3);

    const cache = kvTtlCache(memKv(), "d:");
    const { getOrLoad } = cache;
    let loads = 0;
    const loader = async (): Promise<number> => {
      loads += 1;
      return 7;
    };
    expect(await getOrLoad("k", 1000, loader)).toBe(7);
    expect(await getOrLoad("k", 1000, loader)).toBe(7);
    expect(loads).toBe(1);
  });
});
