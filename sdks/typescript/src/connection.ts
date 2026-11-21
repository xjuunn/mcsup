/**
 * 与宿主的帧连接
 *
 * 读取端把字节流切成行，逐帧交给上层；写入端串行化 JSON 并 flush。
 * 插件 → 宿主的调用按 id 配对，超时或断开时统一拒绝。宿主 → 插件方向
 * 的帧交给 `onFrame`，协议错误触发 `onClose`。
 *
 * `SocketChannel` 把 `Connection` 适配成运行时使用的 `FrameChannel`；
 * 测试用内存双工实现同一接口（`testing.ts`）。
 */

import * as net from "node:net";

import {
  type InboundFrame,
  type OutboundFrame,
  MAX_FRAME_BYTES,
  ProtocolError,
  encodeFrame,
  parseFrame,
} from "./protocol.js";
import { PluginError } from "./errors.js";

const NEWLINE = 0x0a;

/** 单次能力调用的默认超时 */
export const DEFAULT_CALL_TIMEOUT_MS = 15_000;

/** 关闭时等待写缓冲 flush 与 socket 关闭的上限 */
const CLOSE_TIMEOUT_MS = 1_000;

/** 按端点连接：`tcp://host:port`、Windows 命名管道或 Unix socket 路径 */
export function connect(endpoint: string): Promise<net.Socket> {
  const socket = endpoint.startsWith("tcp://")
    ? socketForTcp(endpoint)
    : net.createConnection({ path: endpoint });
  return new Promise<net.Socket>((resolve, reject) => {
    const onError = (err: Error): void => {
      socket.destroy();
      reject(err);
    };
    socket.once("error", onError);
    socket.once("connect", () => {
      socket.removeListener("error", onError);
      socket.setNoDelay(true);
      resolve(socket);
    });
  });
}

function socketForTcp(endpoint: string): net.Socket {
  const url = new URL(endpoint);
  const port = Number.parseInt(url.port, 10);
  if (!Number.isInteger(port) || port <= 0 || port > 65_535) {
    throw new Error(`invalid tcp endpoint: ${endpoint}`);
  }
  return net.createConnection({ host: url.hostname, port });
}

export interface ConnectionHandlers {
  /** 宿主 → 插件的每一帧 */
  readonly onFrame: (frame: InboundFrame) => void;
  /** 断开（含协议错误）；reason 说明原因 */
  readonly onClose: (reason: string) => void;
}

/** 一条已建立的帧连接 */
export class Connection {
  private readonly socket: net.Socket;
  private readonly handlers: ConnectionHandlers;
  private buffer: Buffer = Buffer.alloc(0);
  private closedReason: string | undefined;
  private readonly pending = new Map<number, PendingCall>();
  private nextCallId = 1;
  /** 写链：保证帧顺序，并在 `socket.write` 返回 false 时等 drain */
  private writeChain: Promise<void> = Promise.resolve();

  private constructor(socket: net.Socket, handlers: ConnectionHandlers) {
    this.socket = socket;
    this.handlers = handlers;
    socket.on("data", (chunk: Buffer) => {
      this.consume(chunk);
    });
    socket.on("error", (err: Error) => {
      this.fail(`socket error: ${err.message}`);
    });
    socket.on("close", () => {
      this.fail(this.closedReason ?? "connection closed by host");
    });
  }

  public static async open(endpoint: string, handlers: ConnectionHandlers): Promise<Connection> {
    return new Connection(await connect(endpoint), handlers);
  }

  /**
   * 发送一帧；返回的 promise 在数据实际交给内核后完成。
   *
   * 连接已关闭时立即完成（帧丢弃）。写背压时等 `drain`，避免大帧堆积在
   * 用户态内存里。
   */
  public send(frame: OutboundFrame): Promise<void> {
    if (this.closedReason !== undefined) {
      return Promise.resolve();
    }
    const payload = encodeFrame(frame);
    const written = this.writeChain.then(async () => {
      // 已入队的帧必须写出去：close() 也会先等这条写链 flush
      await this.write(payload);
    });
    // 失败已在 write 内转成 fail；这里只保证写链不中断
    this.writeChain = written.catch(() => {});
    return written;
  }

  private async write(payload: string): Promise<void> {
    if (this.socket.destroyed) {
      return;
    }
    let flushed: boolean;
    try {
      flushed = this.socket.write(payload);
    } catch (err) {
      this.fail(`write failed: ${String(err)}`);
      return;
    }
    if (flushed) {
      return;
    }
    await new Promise<void>((resolve) => {
      const done = (): void => {
        clearTimeout(timer);
        this.socket.off("drain", done);
        this.socket.off("close", done);
        resolve();
      };
      // socket 已半关闭时可能永远不触发 drain/close，用上限兜底
      const timer = setTimeout(done, CLOSE_TIMEOUT_MS);
      this.socket.once("drain", done);
      this.socket.once("close", done);
    });
  }

  /**
   * 调用一次宿主能力
   *
   * @throws {PluginError} code=timeout / unavailable
   */
  public async call(
    capability: string,
    method: string,
    input: unknown,
    timeoutMs: number = DEFAULT_CALL_TIMEOUT_MS,
  ): Promise<unknown> {
    if (this.closedReason !== undefined) {
      throw new PluginError("unavailable", this.closedReason);
    }
    const id = this.nextCallId;
    this.nextCallId += 1;
    return await new Promise<unknown>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new PluginError("timeout", `${capability}.${method} timed out after ${timeoutMs} ms`));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      this.send({ type: "call", id, capability, method, input });
    });
  }

  /** 关闭连接；等待写缓冲 flush 与 socket 关闭，最多 1 s */
  public async close(): Promise<void> {
    this.fail("closed by plugin");
    await withDeadline(this.writeChain, CLOSE_TIMEOUT_MS);
    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        this.socket.destroy();
        resolve();
      }, CLOSE_TIMEOUT_MS);
      this.socket.once("close", () => {
        clearTimeout(timer);
        resolve();
      });
      this.socket.end();
    });
  }

  private consume(chunk: Buffer): void {
    this.buffer = this.buffer.length === 0 ? chunk : Buffer.concat([this.buffer, chunk]);
    for (;;) {
      const index = this.buffer.indexOf(NEWLINE);
      if (index < 0) {
        if (this.buffer.length > MAX_FRAME_BYTES) {
          this.fail("inbound frame exceeds the size limit");
        }
        return;
      }
      if (index > MAX_FRAME_BYTES) {
        this.fail("inbound frame exceeds the size limit");
        return;
      }
      const line = this.buffer.subarray(0, index).toString("utf8").replace(/\r$/, "");
      this.buffer = this.buffer.subarray(index + 1);
      if (line.length === 0) {
        continue;
      }
      let frame: InboundFrame;
      try {
        frame = parseFrame(line);
      } catch (err) {
        const reason = err instanceof ProtocolError ? err.message : String(err);
        this.fail(`protocol error: ${reason}`);
        return;
      }
      if (frame.type === "result") {
        this.settle(frame);
        continue;
      }
      try {
        this.handlers.onFrame(frame);
      } catch (err) {
        this.fail(`frame handler failed: ${String(err)}`);
        return;
      }
    }
  }

  private settle(frame: Extract<InboundFrame, { type: "result" }>): void {
    const pending = this.pending.get(frame.id);
    if (pending === undefined) {
      return;
    }
    this.pending.delete(frame.id);
    clearTimeout(pending.timer);
    if (frame.ok) {
      pending.resolve(frame.output ?? null);
      return;
    }
    const error = frame.error;
    pending.reject(
      new PluginError(
        (error?.code as PluginError["code"] | undefined) ?? "internal",
        error?.message ?? "call failed",
      ),
    );
  }

  private fail(reason: string): void {
    if (this.closedReason !== undefined) {
      return;
    }
    this.closedReason = reason;
    for (const [, pending] of this.pending) {
      clearTimeout(pending.timer);
      pending.reject(new PluginError("unavailable", reason));
    }
    this.pending.clear();
    this.handlers.onClose(reason);
  }
}

interface PendingCall {
  readonly resolve: (value: unknown) => void;
  readonly reject: (error: Error) => void;
  readonly timer: NodeJS.Timeout;
}

/** 等 promise 结束，最多 `ms` 毫秒；超时按完成处理（调用方只关心不再等待） */
async function withDeadline(promise: Promise<unknown>, ms: number): Promise<void> {
  await new Promise<void>((resolve) => {
    const timer = setTimeout(resolve, ms);
    promise.then(
      () => {
        clearTimeout(timer);
        resolve();
      },
      () => {
        clearTimeout(timer);
        resolve();
      },
    );
  });
}

// ---------------------------------------------------------------------------
// 帧通道
// ---------------------------------------------------------------------------

/**
 * 帧通道：真实实现走 socket，测试实现走内存双工。
 *
 * 方向与 `plugins.md` §6 一致；`result` 配对由通道自己完成，不会进入
 * `onFrame`。
 */
export interface FrameChannel {
  onFrame(handler: (frame: InboundFrame) => void): void;
  onClose(handler: (reason: string) => void): void;
  /** 发送一帧；返回 promise 表示已处理写背压 */
  send(frame: OutboundFrame): void | Promise<void>;
  call(capability: string, method: string, input: unknown, timeoutMs?: number): Promise<unknown>;
  /** 关闭通道；返回 promise 表示已 flush */
  close(): void | Promise<void>;
}

/** socket 通道：把 `Connection` 适配成 `FrameChannel` */
export class SocketChannel implements FrameChannel {
  private readonly connection: Connection;
  private frameHandler: (frame: InboundFrame) => void = () => {};
  private closeHandler: (reason: string) => void = () => {};

  private constructor(connection: Connection) {
    this.connection = connection;
  }

  public static async open(endpoint: string): Promise<SocketChannel> {
    let channel: SocketChannel | undefined;
    const connection = await Connection.open(endpoint, {
      onFrame: (frame) => {
        channel?.frameHandler(frame);
      },
      onClose: (reason) => {
        channel?.closeHandler(reason);
      },
    });
    channel = new SocketChannel(connection);
    return channel;
  }

  public onFrame(handler: (frame: InboundFrame) => void): void {
    this.frameHandler = handler;
  }

  public onClose(handler: (reason: string) => void): void {
    this.closeHandler = handler;
  }

  public send(frame: OutboundFrame): Promise<void> {
    return this.connection.send(frame);
  }

  public async call(capability: string, method: string, input: unknown, timeoutMs?: number): Promise<unknown> {
    return await this.connection.call(capability, method, input, timeoutMs);
  }

  public async close(): Promise<void> {
    await this.connection.close();
  }
}
