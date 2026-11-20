/**
 * 运行时：连接、握手、分派、退出（plugins.md §5 / §6 / §7）
 *
 * 插件作者只需要 `run(definePlugin({...}))`。SDK 负责：
 *   - argv / 环境变量解析，`--mcsup-dump-descriptor` 与
 *     `--mcsup-dump-manifest` 供构建期使用
 *   - hello → welcome 握手；reject 退出码 4
 *   - 宿主 call 分派到 op（输入输出按 schema 校验）
 *   - 流式 op 的 stream 帧、seq、取消
 *   - config 帧校验与回 result、ping/pong、shutdown 优雅退出
 */

import { arch, platform } from "node:os";
import { z } from "zod";

import { Connection } from "./connection.js";
import {
  buildDescriptor,
  buildManifest,
  collectOps,
  serializeNormalized,
  type ResolvedOp,
} from "./descriptor.js";
import {
  type Capability,
  type DefinedPlugin,
  type PluginDefinition,
  subscriptionHandlers,
  subscriptionKinds,
} from "./define.js";
import { EventBus, createCtx, type CtxEnvironment, type CtxFor } from "./ctx.js";
import { PluginError, toPluginError } from "./errors.js";
import { DEFAULT_OP_TIMEOUT_MS } from "./validate.js";
import {
  MAX_FRAME_BYTES,
  PLUGIN_API_VERSION,
  type CallFrame,
  type InboundFrame,
  type OutboundFrame,
} from "./protocol.js";

/** 流式总字节上限（plugins.md §5.4） */
export const MAX_STREAM_BYTES = 16 * 1024 * 1024;

function frameBytes(chunk: unknown): number {
  return Buffer.byteLength(JSON.stringify(chunk) ?? "", "utf8");
}

/** 退出码（§6.7） */
export const ExitCode = {
  Ok: 0,
  Failure: 1,
  Protocol: 2,
  Config: 3,
  ApiMismatch: 4,
} as const;

export type ExitCode = (typeof ExitCode)[keyof typeof ExitCode];

/** 运行参数 */
export interface RunOptions {
  /** 覆盖 argv（测试用） */
  readonly argv?: readonly string[];
  /** 覆盖环境变量（测试用） */
  readonly env?: Readonly<Record<string, string | undefined>>;
}

interface ParsedArgs {
  readonly endpoint?: string;
  readonly dumpDescriptor: boolean;
  readonly dumpManifest: boolean;
}

function parseArgs(argv: readonly string[]): ParsedArgs {
  const out: { endpoint?: string; dumpDescriptor: boolean; dumpManifest: boolean } = {
    dumpDescriptor: false,
    dumpManifest: false,
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    switch (arg) {
      case "--endpoint": {
        const next = argv[i + 1];
        if (next === undefined) {
          throw new Error("--endpoint requires a value");
        }
        out.endpoint = next;
        i += 1;
        break;
      }
      case "--mcsup-dump-descriptor":
        out.dumpDescriptor = true;
        break;
      case "--mcsup-dump-manifest":
        out.dumpManifest = true;
        break;
      case "--server":
      case "--log-level": {
        // 宿主传入但不改变 SDK 行为（scope 已写进 manifest）
        i += 1;
        break;
      }
      default:
        // 未知参数忽略，避免宿主新增参数导致插件启动失败
        break;
    }
  }
  return out;
}

function platformTag(): string {
  return `${platform()}-${arch()}`;
}

/**
 * 启动插件进程。
 *
 * @returns 进程退出码；入口执行 `process.exit(await run(plugin))`
 */
export async function run<C extends readonly Capability[], Id extends string>(
  plugin: DefinedPlugin<C, Id> | PluginDefinition<C, Id>,
  options: RunOptions = {},
): Promise<number> {
  const def = ("definition" in plugin ? plugin.definition : plugin) as PluginDefinition<C, Id>;
  const argv = options.argv ?? process.argv;
  const env = options.env ?? process.env;

  let args: ParsedArgs;
  try {
    args = parseArgs(argv);
  } catch (err) {
    process.stderr.write(`mcsup-plugin: ${String(err)}\n`);
    return ExitCode.Failure;
  }

  if (args.dumpDescriptor) {
    try {
      process.stdout.write(serializeNormalized(buildDescriptor(def)));
      return ExitCode.Ok;
    } catch (err) {
      process.stderr.write(`mcsup-plugin: descriptor failed: ${String(err)}\n`);
      return ExitCode.Failure;
    }
  }
  if (args.dumpManifest) {
    // 构建期用占位 entry；打包工具写入实际启动器
    const entry = env["MCSUP_ENTRY_CMD"] ?? "./bin/run.sh";
    try {
      process.stdout.write(serializeNormalized(buildManifest(def, entry)));
      return ExitCode.Ok;
    } catch (err) {
      process.stderr.write(`mcsup-plugin: manifest failed: ${String(err)}\n`);
      return ExitCode.Failure;
    }
  }

  const endpoint = args.endpoint ?? env["MCSUP_PLUGIN_ENDPOINT"];
  const token = env["MCSUP_PLUGIN_TOKEN"];
  if (endpoint === undefined || endpoint === "") {
    process.stderr.write("mcsup-plugin: missing --endpoint\n");
    return ExitCode.Protocol;
  }
  if (token === undefined || token === "") {
    process.stderr.write("mcsup-plugin: missing MCSUP_PLUGIN_TOKEN\n");
    return ExitCode.Protocol;
  }

  let channel: FrameChannel;
  try {
    channel = await SocketChannel.open(endpoint);
  } catch (err) {
    process.stderr.write(`mcsup-plugin: cannot reach host endpoint: ${String(err)}\n`);
    return ExitCode.Failure;
  }
  return await new Session<C>(def, channel).start(token);
}

// ---------------------------------------------------------------------------
// 通道抽象
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
  send(frame: OutboundFrame): void;
  call(capability: string, method: string, input: unknown, timeoutMs?: number): Promise<unknown>;
  close(): void;
}

class SocketChannel implements FrameChannel {
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

  public send(frame: OutboundFrame): void {
    this.connection.send(frame);
  }

  public async call(capability: string, method: string, input: unknown, timeoutMs?: number): Promise<unknown> {
    return await this.connection.call(capability, method, input, timeoutMs);
  }

  public close(): void {
    this.connection.close();
  }
}

// ---------------------------------------------------------------------------
// 会话
// ---------------------------------------------------------------------------

/** 一次插件会话（握手完成后的运行期） */
export class Session<C extends readonly Capability[]> {
  private readonly def: PluginDefinition<C>;
  private readonly channel: FrameChannel;
  private readonly ops: ReadonlyMap<string, ResolvedOp>;
  private readonly ctxEnv: CtxEnvironment;
  private readonly events = new EventBus();
  private readonly inflight = new Map<number, AbortController>();

  private ctx: CtxFor<C>;
  private readonly exitPromise: Promise<number>;
  private resolveExit: ((code: number) => void) | undefined;

  private exitCode: number = ExitCode.Ok;
  private welcomed = false;
  private closed = false;
  private configValues: unknown = undefined;
  private configRevision = 0;

  public constructor(def: PluginDefinition<C>, channel: FrameChannel) {
    this.def = def;
    this.channel = channel;
    this.ops = new Map(collectOps(def).map((op) => [op.id, op]));

    for (const kind of subscriptionKinds(def.subscriptions)) {
      this.events.subscribe([kind]);
    }
    for (const { kind, handler } of subscriptionHandlers(def.subscriptions)) {
      this.events.on(kind, handler as never);
    }

    // 先建 promise 再挂 ctx，避免闭包提前读取未初始化的字段
    this.exitPromise = new Promise<number>((resolve) => {
      this.resolveExit = resolve;
    });

    this.ctxEnv = {
      caller: {
        call: async (capability: string, method: string, input: unknown, timeoutMs?: number) =>
          await channel.call(capability, method, input, timeoutMs),
      },
      config: () => this.configValues,
      sessionId: "",
      limits: { inflight: 8, frameBytes: 1024 * 1024, eventQueue: 1024, logLinesPerSec: 200 },
      scope: { kind: def.scope ?? "machine" },
      events: this.events,
      logLine: (level: string, message: string) => {
        this.log(level, message);
      },
    };
    // 握手前先按声明构造；welcome 后按实际授予重建
    this.ctx = createCtx<C>(this.ctxEnv, def.capabilities);

    channel.onFrame((frame) => {
      this.handleFrame(frame);
    });
    channel.onClose((reason) => {
      if (this.closed) {
        return;
      }
      this.closed = true;
      this.log("warn", `connection closed: ${reason}`);
      // 协议错误必须按 §6.7 以 2 退出，不能当成正常关闭
      if (reason.startsWith("protocol error")) {
        this.exitCode = ExitCode.Protocol;
      }
      this.resolveExitOnce(this.exitCode);
    });
  }

  /** 发 hello 并等待会话结束 */
  public async start(token: string): Promise<number> {
    this.channel.send({
      type: "hello",
      token,
      api: PLUGIN_API_VERSION,
      plugin_id: this.def.id,
      plugin_version: this.def.version,
      platform: platformTag(),
      pid: process.pid,
      requested: this.def.capabilities,
      subscriptions: this.events.subscribedKinds(),
    });
    return await this.exitPromise;
  }

  /** 供测试观察的上下文 */
  public get context(): CtxFor<C> {
    return this.ctx;
  }

  private log(level: string, message: string): void {
    // 日志走 stderr，不进入帧（§5.3）
    process.stderr.write(`[${this.def.id}] ${level}: ${message}\n`);
  }

  private resolveExitOnce(code: number): void {
    const resolve = this.resolveExit;
    this.resolveExit = undefined;
    resolve?.(code);
  }

  private handleFrame(frame: InboundFrame): void {
    switch (frame.type) {
      case "welcome": {
        this.onWelcome(frame);
        return;
      }
      case "reject": {
        this.log("error", `handshake rejected (${frame.code}): ${frame.message}`);
        this.exitCode = frame.code === "api_mismatch" ? ExitCode.ApiMismatch : ExitCode.Protocol;
        this.finish();
        return;
      }
      case "call": {
        void this.dispatchCall(frame);
        return;
      }
      case "event": {
        void this.dispatchEvent(frame);
        return;
      }
      case "config": {
        void this.applyConfig(frame);
        return;
      }
      case "cancel": {
        this.inflight.get(frame.id)?.abort(new PluginError("cancelled", frame.reason ?? "cancelled by host"));
        return;
      }
      case "ping": {
        this.channel.send({ type: "pong", n: frame.n });
        return;
      }
      case "shutdown": {
        void this.shutdown(frame.grace_ms);
        return;
      }
      case "result": {
        // 结果配对由通道完成，不会到达这里
        return;
      }
      case "unknown": {
        this.log("debug", `ignored unknown frame type ${frame.kind}`);
        return;
      }
    }
  }

  private onWelcome(frame: Extract<InboundFrame, { type: "welcome" }>): void {
    this.welcomed = true;
    this.ctxEnv.sessionId = frame.session_id;
    this.ctxEnv.limits = {
      inflight: frame.limits.inflight,
      frameBytes: frame.limits.frame_bytes,
      eventQueue: frame.limits.event_queue,
      logLinesPerSec: frame.limits.log_lines_per_sec,
    };
    this.ctxEnv.scope = frame.scope;
    const granted = this.def.capabilities.filter((cap) => frame.granted.includes(cap));
    const ungranted = this.def.capabilities.filter((cap) => !frame.granted.includes(cap));
    this.ctx = createCtx<C>(this.ctxEnv, granted as unknown as C, ungranted);
    const parsed = this.def.config.safeParse(frame.config.values);
    if (!parsed.success) {
      this.log("error", `initial config is invalid: ${z.prettifyError(parsed.error)}`);
      this.exitCode = ExitCode.Config;
      this.finish();
      return;
    }
    this.configRevision = frame.config.revision;
    this.configValues = parsed.data;
    void this.runStart();
  }

  private async runStart(): Promise<void> {
    try {
      await this.def.onStart?.(this.ctx);
    } catch (err) {
      const error = toPluginError(err);
      this.log("error", `onStart failed: ${error.message}`);
      this.exitCode = error.code === "invalid_input" ? ExitCode.Config : ExitCode.Failure;
      this.finish();
    }
  }

  private async dispatchEvent(frame: Extract<InboundFrame, { type: "event" }>): Promise<void> {
    const meta = { kind: frame.kind, server: frame.server, at: frame.at };
    try {
      const handled = await this.events.dispatch(frame.kind, frame.data, meta);
      if (!handled && this.def.onEvent !== undefined) {
        await this.def.onEvent(frame.data, this.ctx, meta);
      }
    } catch (err) {
      this.log("error", `event ${frame.kind} handler failed: ${toPluginError(err).message}`);
    }
  }

  private async applyConfig(frame: Extract<InboundFrame, { type: "config" }>): Promise<void> {
    if (frame.revision <= this.configRevision) {
      this.channel.send({ type: "result", id: frame.id, ok: true, output: { stale: true } });
      return;
    }
    const parsed = this.def.config.safeParse(frame.values);
    if (!parsed.success) {
      this.channel.send({
        type: "result",
        id: frame.id,
        ok: false,
        error: {
          code: "invalid_input",
          category: "usage",
          retryable: false,
          message: z.prettifyError(parsed.error),
        },
      });
      return;
    }
    const previousValues = this.configValues;
    const previousRevision = this.configRevision;
    this.configValues = parsed.data;
    this.configRevision = frame.revision;
    try {
      await this.def.onConfigChange?.(parsed.data, this.ctx);
      this.channel.send({ type: "result", id: frame.id, ok: true, output: { revision: frame.revision } });
    } catch (err) {
      // 应用失败：回滚并回 ok:false（§6.6 宿主侧转 degraded）
      const error = toPluginError(err);
      this.configValues = previousValues;
      this.configRevision = previousRevision;
      this.channel.send({
        type: "result",
        id: frame.id,
        ok: false,
        error: { code: error.code, category: error.category, retryable: error.retryable, message: error.message },
      });
    }
  }

  private async shutdown(graceMs: number): Promise<void> {
    if (this.closed) {
      return;
    }
    this.closed = true;
    for (const [, controller] of this.inflight) {
      controller.abort(new PluginError("cancelled", "plugin shutting down"));
    }
    this.inflight.clear();
    try {
      await this.def.onStop?.(this.ctx);
    } catch (err) {
      this.log("error", `onStop failed: ${toPluginError(err).message}`);
      this.exitCode = ExitCode.Failure;
    }
    // grace_ms 只是宿主给的上限，插件做完即退；这里不等满
    void graceMs;
    this.channel.close();
    this.resolveExitOnce(this.exitCode);
  }

  private finish(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.inflight.clear();
    this.channel.close();
    this.resolveExitOnce(this.exitCode);
  }

  private replyError(id: number, error: PluginError): void {
    this.channel.send({
      type: "result",
      id,
      ok: false,
      error: { code: error.code, category: error.category, retryable: error.retryable, message: error.message },
    });
  }

  private async dispatchCall(frame: CallFrame): Promise<void> {
    if (!this.welcomed) {
      this.replyError(frame.id, new PluginError("conflict", "call before welcome"));
      return;
    }
    if (frame.op === undefined) {
      this.replyError(frame.id, PluginError.invalidInput("call has no op"));
      return;
    }
    if (frame.op === "contract.call") {
      // v1 简单处理：未声明契约时明确拒绝，不假装成功
      this.replyError(frame.id, new PluginError("unsupported", "contract.call is not supported by this plugin"));
      return;
    }
    const op = this.ops.get(frame.op);
    if (op === undefined) {
      this.replyError(frame.id, PluginError.notFound(`unknown op ${frame.op}`));
      return;
    }
    if (op.run === undefined) {
      this.replyError(frame.id, PluginError.internal(`op ${op.id} has no run handler`));
      return;
    }
    const parsed = op.input.safeParse(frame.input);
    if (!parsed.success) {
      this.replyError(frame.id, PluginError.invalidInput(z.prettifyError(parsed.error)));
      return;
    }

    const controller = new AbortController();
    this.inflight.set(frame.id, controller);
    try {
      if (op.kind === "stream") {
        await this.runStream(op, parsed.data, frame.id, controller.signal);
        return;
      }
      const output = await this.runOnce(op, parsed.data, controller.signal);
      const checked = op.output.safeParse(output);
      if (!checked.success) {
        this.replyError(
          frame.id,
          PluginError.internal(`op ${op.id} returned an invalid output: ${z.prettifyError(checked.error)}`),
        );
        return;
      }
      if (frameBytes(checked.data) > MAX_FRAME_BYTES) {
        this.replyError(frame.id, new PluginError("limit_exceeded", `op ${op.id} output exceeds ${MAX_FRAME_BYTES} bytes`));
        return;
      }
      this.channel.send({ type: "result", id: frame.id, ok: true, output: checked.data });
    } catch (err) {
      this.replyError(frame.id, toPluginError(err));
    } finally {
      this.inflight.delete(frame.id);
    }
  }

  private async runOnce(op: ResolvedOp, input: unknown, signal: AbortSignal): Promise<unknown> {
    const run = op.run as (input: unknown, ctx: unknown) => unknown;
    const result = Promise.resolve(run(input, this.ctx));
    // descriptor 生成时已写入缺省 30 s（§11.2）；op 缺失 timeout_ms 时兜底
    return await raceAbort(withTimeout(result, op.timeoutMs ?? DEFAULT_OP_TIMEOUT_MS), signal);
  }

  private async runStream(op: ResolvedOp, input: unknown, callId: number, signal: AbortSignal): Promise<void> {
    const run = op.run as (input: unknown, ctx: unknown) => unknown;
    let chunks: unknown;
    try {
      chunks = await raceAbort(Promise.resolve(run(input, this.ctx)), signal);
    } catch (err) {
      this.replyError(callId, toPluginError(err));
      return;
    }
    await pumpStream(chunks, op, this.channel, callId, signal);
  }
}

// ---------------------------------------------------------------------------
// 流式泵
// ---------------------------------------------------------------------------

interface StreamOpShape {
  readonly id: string;
  readonly output: z.ZodType;
}

/**
 * 把迭代器泵成 stream 帧（§6.5）。
 *
 * - `seq` 从 0 连续递增，末尾发 `done: true` 并回 result
 * - 取消后停止读取，回 `cancelled` 结果，不再写流
 * - 每块按 output fragment schema 校验
 */
export async function pumpStream(
  chunks: unknown,
  op: StreamOpShape,
  channel: FrameChannel,
  callId: number,
  signal: AbortSignal,
): Promise<void> {
  const iterator = toAsyncIterator(chunks);
  if (iterator === undefined) {
    channel.send({
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
  try {
    for (;;) {
      let next: IteratorResult<unknown>;
      try {
        next = await raceAbort(iterator.next(), signal);
      } catch (err) {
        if (signal.aborted) {
          throw new PluginError("cancelled", abortReason(signal));
        }
        throw err;
      }
      if (next.done === true) {
        break;
      }
      const checked = op.output.safeParse(next.value);
      if (!checked.success) {
        channel.send({
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
      const bytes = frameBytes(checked.data);
      total += bytes;
      if (bytes > MAX_FRAME_BYTES || total > MAX_STREAM_BYTES) {
        channel.send({
          type: "result",
          id: callId,
          ok: false,
          error: {
            code: "limit_exceeded",
            category: "usage",
            retryable: false,
            message: `stream op ${op.id} exceeds the stream limit (${total} bytes)`,
          },
        });
        return;
      }
      channel.send({ type: "stream", id: callId, seq, chunk: checked.data, done: false });
      seq += 1;
    }
    channel.send({ type: "stream", id: callId, seq, chunk: {}, done: true });
    channel.send({ type: "result", id: callId, ok: true, output: { completed: true, chunks: seq } });
  } catch (err) {
    const error = toPluginError(err);
    channel.send({
      type: "result",
      id: callId,
      ok: false,
      error: { code: error.code, category: error.category, retryable: error.retryable, message: error.message },
    });
  } finally {
    await iterator.return?.();
  }
}

function abortReason(signal: AbortSignal): string {
  const reason = signal.reason as unknown;
  return reason instanceof Error ? reason.message : "cancelled by host";
}

function raceAbort<T>(promise: Promise<T>, signal: AbortSignal): Promise<T> {
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

async function withTimeout<T>(promise: Promise<T>, timeoutMs: number): Promise<T> {
  return await new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new PluginError("timeout", `op timed out after ${timeoutMs} ms`));
    }, timeoutMs);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (err: unknown) => {
        clearTimeout(timer);
        reject(err instanceof Error ? err : new Error(String(err)));
      },
    );
  });
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