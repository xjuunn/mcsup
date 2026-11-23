/**
 * 流式 op 的输出泵（plugins.md §6.5 / §5.4）
 *
 * 把插件返回的同步 / 异步迭代器泵成 `stream` 帧：`seq` 连续递增、每块按
 * output schema 校验、超过限额或空闲超时终止。取消通过 `AbortSignal`
 * 传播，泵结束后不再写流。
 */

import { z } from "./zod.js";

import { type FrameChannel } from "./connection.js";
import { PluginError, toPluginError } from "./errors.js";
import { MAX_FRAME_BYTES, frameBytes, type OutboundFrame } from "./protocol.js";

/** 流式总字节上限（plugins.md §5.4） */
export const MAX_STREAM_BYTES = 16 * 1024 * 1024;

/** 流式空闲超时（plugins.md §5.4：60 s 无新分块即终止） */
export const STREAM_IDLE_TIMEOUT_MS = 60_000;

/** 泵只关心 op 的标识与分块 schema */
interface StreamOpShape {
  readonly id: string;
  readonly output: z.ZodType;
  /**
   * 作者是否显式声明过 output。
   *
   * 未声明（Builder 没调 `.output()`）时分块原样发出，不做校验——空 object
   * 占位 schema 会把每一个分块都判成非法。
   */
  readonly outputDeclared?: boolean;
}

/** 流式泵选项；测试可缩短空闲窗口 */
export interface PumpStreamOptions {
  /** 单帧字节上限，默认协议上限（`MAX_FRAME_BYTES`） */
  readonly frameBytes?: number;
  /** 空闲超时毫秒，默认 60 s */
  readonly idleMs?: number;
}

/**
 * 把迭代器泵成 stream 帧（§6.5）。
 *
 * - `seq` 从 0 连续递增，末尾发 `done: true` 并回 result
 * - 取消后停止读取，回 `cancelled` 结果，不再写流
 * - 每块按 output fragment schema 校验
 * - 空闲超过 `idleMs` 终止流（§5.4）
 */
export async function pumpStream(
  chunks: unknown,
  op: StreamOpShape,
  channel: FrameChannel,
  callId: number,
  signal: AbortSignal,
  options: PumpStreamOptions = {},
): Promise<void> {
  const frameLimit = Math.min(options.frameBytes ?? MAX_FRAME_BYTES, MAX_FRAME_BYTES);
  const idleMs = options.idleMs ?? STREAM_IDLE_TIMEOUT_MS;
  const iterator = toAsyncIterator(chunks);
  if (iterator === undefined) {
    await channel.send({
      type: "result",
      id: callId,
      ok: false,
      error: {
        code: "internal",
        category: "failure",
        retryable: false,
        message: `stream op ${op.id} did not return an iterator`,
      },
    });
    return;
  }
  let seq = 0;
  let total = 0;
  let dataChunks = 0;
  let pending: unknown | undefined;
  let idleTimedOut = false;
  /** 发送一个分块帧；帧超限时返回 false（调用方负责回错误） */
  const emit = async (chunk: unknown, done: boolean): Promise<boolean> => {
    const frame: OutboundFrame = { type: "stream", id: callId, seq, chunk, done };
    if (frameBytes(frame) > frameLimit || total > MAX_STREAM_BYTES) {
      return false;
    }
    // 等写背压，避免生产者把大流堆进内存
    await channel.send(frame);
    seq += 1;
    return true;
  };
  try {
    for (;;) {
      let next: IteratorResult<unknown>;
      try {
        next = await raceAbort(nextWithIdleTimeout(iterator, idleMs, op.id, signal), signal);
      } catch (err) {
        if (signal.aborted) {
          throw new PluginError("cancelled", abortReason(signal));
        }
        if (err instanceof PluginError && err.code === "timeout") {
          idleTimedOut = true;
        }
        throw err;
      }
      if (next.done === true) {
        break;
      }
      const checked =
        op.outputDeclared === false
          ? { success: true as const, data: next.value }
          : op.output.safeParse(next.value);
      if (!checked.success) {
        await channel.send({
          type: "result",
          id: callId,
          ok: false,
          error: {
            code: "internal",
            category: "failure",
            retryable: false,
            message: `stream op ${op.id} produced an invalid chunk: ${z.prettifyError(checked.error)}`,
          },
        });
        return;
      }
      if (pending !== undefined && !(await emit(pending, false))) {
        await sendLimitError(channel, callId, op.id, total);
        return;
      }
      pending = checked.data;
      total += frameBytes(checked.data);
      dataChunks += 1;
      if (total > MAX_STREAM_BYTES) {
        await sendLimitError(channel, callId, op.id, total);
        return;
      }
    }
    // 最后一个真实分块带 done:true；空流也必须显式收尾（§6.5）
    const last = pending === undefined ? {} : pending;
    if (!(await emit(last, true))) {
      await sendLimitError(channel, callId, op.id, total);
      return;
    }
    await channel.send({ type: "result", id: callId, ok: true, output: { completed: true, chunks: dataChunks } });
  } catch (err) {
    const error = toPluginError(err);
    await channel.send({
      type: "result",
      id: callId,
      ok: false,
      error: { code: error.code, category: error.category, retryable: error.retryable, message: error.message },
    });
  } finally {
    if (idleTimedOut) {
      // 空闲超时的迭代器可能永远不返回，不能等它的 return 阻塞退出
      void Promise.resolve(iterator.return?.()).catch(() => {});
    } else {
      // 结果帧已发出；return 抛错不应再覆盖它
      try {
        await iterator.return?.();
      } catch {
        // 忽略：清理失败不改变已确定的调用结果
      }
    }
  }
}

/** 流超限统一错误帧 */
function sendLimitError(
  channel: FrameChannel,
  callId: number,
  opId: string,
  total: number,
): Promise<void> {
  return Promise.resolve(
    channel.send({
      type: "result",
      id: callId,
      ok: false,
      error: {
        code: "limit_exceeded",
        category: "usage",
        retryable: false,
        message: `stream op ${opId} exceeds the stream limit (${total} bytes)`,
      },
    }),
  );
}

/** 把操作与取消信号竞速：取消时立即以 `cancelled` 拒绝 */
export function raceAbort<T>(promise: Promise<T>, signal: AbortSignal): Promise<T> {
  if (signal.aborted) {
    return Promise.reject(new PluginError("cancelled", abortReason(signal)));
  }
  return new Promise<T>((resolve, reject) => {
    const onAbort = (): void => {
      reject(new PluginError("cancelled", abortReason(signal)));
    };
    signal.addEventListener("abort", onAbort, { once: true });
    promise.then(
      (value) => {
        signal.removeEventListener("abort", onAbort);
        resolve(value);
      },
      (err: unknown) => {
        signal.removeEventListener("abort", onAbort);
        reject(err instanceof Error ? err : new Error(String(err)));
      },
    );
  });
}

/** 单次 `next()` 的空闲计时；超时抛 `timeout` */
function nextWithIdleTimeout(
  iterator: AsyncIterator<unknown>,
  idleMs: number,
  opId: string,
  signal: AbortSignal,
): Promise<IteratorResult<unknown>> {
  if (!Number.isFinite(idleMs) || idleMs <= 0) {
    return Promise.resolve(iterator.next());
  }
  return new Promise<IteratorResult<unknown>>((resolve, reject) => {
    const cleanup = (): void => {
      clearTimeout(timer);
      signal.removeEventListener("abort", onAbort);
    };
    const onAbort = (): void => {
      cleanup();
      reject(new PluginError("cancelled", abortReason(signal)));
    };
    const timer = setTimeout(() => {
      cleanup();
      reject(new PluginError("timeout", `stream op ${opId} was idle for ${idleMs} ms`));
    }, idleMs);
    signal.addEventListener("abort", onAbort, { once: true });
    Promise.resolve(iterator.next()).then(
      (value) => {
        cleanup();
        resolve(value);
      },
      (err: unknown) => {
        cleanup();
        reject(err instanceof Error ? err : new Error(String(err)));
      },
    );
  });
}

function abortReason(signal: AbortSignal): string {
  const reason = signal.reason as unknown;
  return reason instanceof Error ? reason.message : "cancelled by host";
}

function toAsyncIterator(chunks: unknown): AsyncIterator<unknown> | undefined {
  if (chunks === null || typeof chunks !== "object") {
    return undefined;
  }
  const asyncIter = (chunks as AsyncIterable<unknown>)[Symbol.asyncIterator];
  if (typeof asyncIter === "function") {
    return asyncIter.call(chunks) as AsyncIterator<unknown>;
  }
  const syncIter = (chunks as Iterable<unknown>)[Symbol.iterator];
  if (typeof syncIter === "function") {
    const iterator = syncIter.call(chunks) as Iterator<unknown>;
    return {
      next: async () => iterator.next(),
      return: async (value?: unknown) => iterator.return?.(value) ?? { done: true, value: undefined },
    };
  }
  return undefined;
}
