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
  ExitCode,
  MAX_FRAME_BYTES,
  PLUGIN_API_VERSION,
  type CallFrame,
  type ExitCodeValue,
  type InboundFrame,
  type OutboundFrame,
} from "./protocol.js";

/** 流式总字节上限（plugins.md §5.4） */
export const MAX_STREAM_BYTES = 16 * 1024 * 1024;

/** 流式空闲超时（plugins.md §5.4：60 s 无新分块即终止） */
export const STREAM_IDLE_TIMEOUT_MS = 60_000;

/** shutdown 取消 inflight 后等待其收尾的上限 */
const SHUTDOWN_ABORT_WAIT_MS = 1_000;

/** 默认限额（welcome 之前的兜底值，与 §5.4 一致） */
const DEFAULT_LIMITS = { inflight: 8, frameBytes: 1024 * 1024, eventQueue: 1024, logLinesPerSec: 200 };

function frameBytes(chunk: unknown): number {
  return Buffer.byteLength(JSON.stringify(chunk) ?? "", "utf8");
}

/**
 * 退出码（§6.7）：来自生成契约，禁止手写
 */
export { ExitCode };
export type ExitCode = ExitCodeValue;

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
  /** 发送一帧；返回 promise 表示已处理写背压 */
  send(frame: OutboundFrame): void | Promise<void>;
  call(capability: string, method: string, input: unknown, timeoutMs?: number): Promise<unknown>;
  /** 关闭通道；返回 promise 表示已 flush */
  close(): void | Promise<void>;
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
  /** 超出 inflight 限额后排队等待的调用 */
  private readonly callQueue: CallFrame[] = [];
  /** 有界事件队列；满时丢最旧 */
  private readonly eventQueue: Array<Extract<InboundFrame, { type: "event" }>> = [];
  /** 等 inflight 清空的 waiter */
  private readonly inflightWaiters: Array<() => void> = [];
  /** 插件 → 宿主方向的活动调用数与排队者（§5.4 同一限额） */
  private hostInflight = 0;
  private readonly hostCallWaiters: Array<{ resolve: () => void; reject: (error: PluginError) => void }> = [];

  private ctx: CtxFor<C>;
  private readonly exitPromise: Promise<number>;
  private resolveExit: ((code: number) => void) | undefined;
  private startPromise: Promise<void> | undefined;

  private exitCode: number = ExitCode.Ok;
  private welcomed = false;
  private closed = false;
  /** 已收到 shutdown：不再接受新调用 / 事件 */
  private stopping = false;
  private drainingEvents = false;
  private droppedEvents = 0;
  private logWindowStart = Date.now();
  private logWindowCount = 0;
  private suppressedLogs = 0;
  private configValues: unknown = undefined;
  private configRevision = 0;

  public constructor(def: PluginDefinition<C>, channel: FrameChannel) {
    this.def = def;
    this.channel = channel;
    this.ops = new Map(collectOps(def).map((op) => [op.id, op]));

    for (const { kind, handler } of subscriptionHandlers(def.subscriptions)) {
      this.events.on(kind, handler as never);
    }

    // 先建 promise 再挂 ctx，避免闭包提前读取未初始化的字段
    this.exitPromise = new Promise<number>((resolve) => {
      this.resolveExit = resolve;
    });

    this.ctxEnv = {
      caller: {
        call: async (capability: string, method: string, input: unknown, timeoutMs?: number) => {
          await this.acquireHostCall();
          try {
            return await channel.call(capability, method, input, timeoutMs);
          } finally {
            this.releaseHostCall();
          }
        },
      },
      config: () => this.configValues,
      sessionId: "",
      limits: { ...DEFAULT_LIMITS },
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
      this.stopping = true;
      this.log("warn", `connection closed: ${reason}`);
      // 协议错误必须按 §6.7 以 2 退出，不能当成正常关闭
      if (reason.startsWith("protocol error")) {
        this.exitCode = ExitCode.Protocol;
      }
      this.callQueue.length = 0;
      this.eventQueue.length = 0;
      this.rejectHostCallWaiters(new PluginError("unavailable", "connection closed"));
      this.abortInflight(new PluginError("unavailable", "connection closed"));
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
      // 订阅集合是静态声明；运行期不能再改（宿主按 hello 下发事件）
      subscriptions: subscriptionKinds(this.def.subscriptions),
    });
    return await this.exitPromise;
  }

  /** 供测试观察的上下文 */
  public get context(): CtxFor<C> {
    return this.ctx;
  }

  private log(level: string, message: string): void {
    // 日志限速（§5.4）：每窗口超过上限就丢行，窗口切换时汇总一次
    const now = Date.now();
    if (now - this.logWindowStart >= 1000) {
      this.logWindowStart = now;
      this.logWindowCount = 0;
      if (this.suppressedLogs > 0) {
        const suppressed = this.suppressedLogs;
        this.suppressedLogs = 0;
        // 汇总行直接写，避免被同一个限速器再次丢弃
        process.stderr.write(`[${this.def.id}] warn: suppressed ${suppressed} log line(s)\n`);
        this.logWindowCount = 1;
      }
    }
    if (this.logWindowCount >= this.ctxEnv.limits.logLinesPerSec) {
      this.suppressedLogs += 1;
      return;
    }
    this.logWindowCount += 1;
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
        this.enqueueCall(frame);
        return;
      }
      case "event": {
        this.enqueueEvent(frame);
        return;
      }
      case "config": {
        void this.applyConfig(frame);
        return;
      }
      case "cancel": {
        const reason = frame.reason ?? "cancelled by host";
        const controller = this.inflight.get(frame.id);
        if (controller !== undefined) {
          controller.abort(new PluginError("cancelled", reason));
          return;
        }
        // 还在队列里的调用直接取消，回 cancelled 而不是等它开始
        const index = this.callQueue.findIndex((queued) => queued.id === frame.id);
        if (index >= 0) {
          this.callQueue.splice(index, 1);
          this.replyError(frame.id, new PluginError("cancelled", reason));
        }
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
    this.startPromise = this.runStart();
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

  /** 入站调用入口：超限额时排队，队列也满则直接拒绝（§5.4） */
  private enqueueCall(frame: CallFrame): void {
    if (this.stopping || this.closed) {
      this.replyError(frame.id, new PluginError("unavailable", "plugin is not accepting calls"));
      return;
    }
    const limit = Math.max(this.ctxEnv.limits.inflight, 1);
    if (this.inflight.size >= limit) {
      // 队列容量与 inflight 同量：总等待上限 2 * limit
      if (this.callQueue.length >= limit) {
        this.replyError(frame.id, new PluginError("limit_exceeded", `too many in-flight calls (limit ${limit})`));
        return;
      }
      this.callQueue.push(frame);
      return;
    }
    void this.runCall(frame);
  }

  private async runCall(frame: CallFrame): Promise<void> {
    try {
      await this.dispatchCall(frame);
    } finally {
      // 无论成功、失败还是早退，都要补上排队中的调用
      this.pumpCalls();
    }
  }

  private pumpCalls(): void {
    if (this.stopping || this.closed) {
      return;
    }
    const limit = Math.max(this.ctxEnv.limits.inflight, 1);
    while (this.inflight.size < limit && this.callQueue.length > 0) {
      const next = this.callQueue.shift();
      if (next === undefined) {
        break;
      }
      void this.runCall(next);
    }
  }

  /** 事件入队；超过 event_queue 丢最旧并计数（§5.4） */
  private enqueueEvent(frame: Extract<InboundFrame, { type: "event" }>): void {
    if (this.stopping || this.closed) {
      return;
    }
    const limit = Math.max(this.ctxEnv.limits.eventQueue, 1);
    if (this.eventQueue.length >= limit) {
      this.eventQueue.shift();
      this.droppedEvents += 1;
    }
    this.eventQueue.push(frame);
    void this.drainEvents();
  }

  private async drainEvents(): Promise<void> {
    if (this.drainingEvents) {
      return;
    }
    this.drainingEvents = true;
    try {
      while (!this.stopping && !this.closed) {
        const frame = this.eventQueue.shift();
        if (frame === undefined) {
          break;
        }
        await this.dispatchEvent(frame);
      }
    } finally {
      this.drainingEvents = false;
      if (this.droppedEvents > 0) {
        const dropped = this.droppedEvents;
        this.droppedEvents = 0;
        this.log("warn", `dropped ${dropped} event(s) because the event queue is full`);
      }
    }
  }

  private waitForInflight(): Promise<void> {
    if (this.inflight.size === 0) {
      return Promise.resolve();
    }
    return new Promise<void>((resolve) => {
      this.inflightWaiters.push(resolve);
    });
  }

  private notifyInflightIfIdle(): void {
    if (this.inflight.size > 0) {
      return;
    }
    const waiters = this.inflightWaiters.splice(0);
    for (const waiter of waiters) {
      waiter();
    }
  }

  private abortInflight(error: PluginError): void {
    for (const [, controller] of this.inflight) {
      controller.abort(error);
    }
    // 不在这里 clear：调用在 dispatchCall 的 finally 里自行移除并唤醒 waiter，
    // 否则 shutdown 会在结果帧发出前就关闭通道。
  }

  private frameLimit(): number {
    return Math.min(MAX_FRAME_BYTES, Math.max(this.ctxEnv.limits.frameBytes, 1));
  }

  /** 取一个插件 → 宿主调用槽位；满额时排队，队列也满则拒绝 */
  private async acquireHostCall(): Promise<void> {
    if (this.stopping || this.closed) {
      throw new PluginError("unavailable", "plugin is shutting down");
    }
    const limit = Math.max(this.ctxEnv.limits.inflight, 1);
    if (this.hostInflight < limit) {
      this.hostInflight += 1;
      return;
    }
    if (this.hostCallWaiters.length >= limit) {
      throw new PluginError("limit_exceeded", `too many in-flight host calls (limit ${limit})`);
    }
    await new Promise<void>((resolve, reject) => {
      this.hostCallWaiters.push({ resolve, reject });
    });
  }

  /** 释放槽位；有排队者时直接把槽位转交（hostInflight 不变） */
  private releaseHostCall(): void {
    const next = this.hostCallWaiters.shift();
    if (next !== undefined) {
      next.resolve();
      return;
    }
    this.hostInflight = Math.max(this.hostInflight - 1, 0);
  }

  private rejectHostCallWaiters(error: PluginError): void {
    const waiters = this.hostCallWaiters.splice(0);
    for (const waiter of waiters) {
      waiter.reject(error);
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
    if (this.stopping || this.closed) {
      this.replyError(frame.id, new PluginError("unavailable", "plugin is shutting down"));
      return;
    }
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
    if (this.closed || this.stopping) {
      return;
    }
    // §6.7：停止接受新调用 → 完成或取消 inflight → flush → 退出码 0
    this.stopping = true;
    this.rejectHostCallWaiters(new PluginError("cancelled", "plugin shutting down"));
    while (this.callQueue.length > 0) {
      const queued = this.callQueue.shift();
      if (queued === undefined) {
        break;
      }
      this.replyError(queued.id, new PluginError("unavailable", "plugin is shutting down"));
    }
    this.eventQueue.length = 0;

    // onStart 尚未完成时也给它同样的 grace 窗口
    if (this.startPromise !== undefined) {
      await waitWithDeadline(this.startPromise, Math.max(graceMs, 0));
    }
    if (this.closed) {
      return;
    }

    if (this.inflight.size > 0) {
      await waitWithDeadline(this.waitForInflight(), Math.max(graceMs, 0));
    }
    if (this.inflight.size > 0) {
      // grace 用尽：取消剩余 inflight，并给收尾一点时间
      this.abortInflight(new PluginError("cancelled", "plugin shutting down"));
      await waitWithDeadline(this.waitForInflight(), SHUTDOWN_ABORT_WAIT_MS);
    }

    try {
      await this.def.onStop?.(this.ctx);
    } catch (err) {
      this.log("error", `onStop failed: ${toPluginError(err).message}`);
      this.exitCode = ExitCode.Failure;
    }
    this.closed = true;
    // close 会等待写缓冲 flush，避免退出前丢帧
    await this.channel.close();
    this.resolveExitOnce(this.exitCode);
  }

  private finish(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.stopping = true;
    this.rejectHostCallWaiters(new PluginError("cancelled", "plugin shutting down"));
    while (this.callQueue.length > 0) {
      const queued = this.callQueue.shift();
      if (queued === undefined) {
        break;
      }
      this.replyError(queued.id, new PluginError("unavailable", "plugin is shutting down"));
    }
    this.eventQueue.length = 0;
    this.abortInflight(new PluginError("cancelled", "plugin is shutting down"));
    void this.closeChannel();
  }

  private async closeChannel(): Promise<void> {
    const code = this.exitCode;
    await this.channel.close();
    this.resolveExitOnce(code);
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
    if (this.stopping || this.closed) {
      this.replyError(frame.id, new PluginError("unavailable", "plugin is shutting down"));
      return;
    }
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
      const result: OutboundFrame = { type: "result", id: frame.id, ok: true, output: checked.data };
      if (frameBytes(result) > this.frameLimit()) {
        this.replyError(
          frame.id,
          new PluginError("limit_exceeded", `op ${op.id} output exceeds ${this.frameLimit()} bytes`),
        );
        return;
      }
      this.channel.send(result);
    } catch (err) {
      this.replyError(frame.id, toPluginError(err));
    } finally {
      this.inflight.delete(frame.id);
      this.notifyInflightIfIdle();
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
    await pumpStream(chunks, op, this.channel, callId, signal, { frameBytes: this.frameLimit() });
  }
}

// ---------------------------------------------------------------------------
// 流式泵
// ---------------------------------------------------------------------------

interface StreamOpShape {
  readonly id: string;
  readonly output: z.ZodType;
}

/** 流式泵选项；测试可缩短空闲窗口 */
export interface PumpStreamOptions {
  /** 单帧字节上限，默认协议上限 1 MiB */
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
  let idleTimedOut = false;
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
      const checked = op.output.safeParse(next.value);
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
      const chunkBytes = frameBytes(checked.data);
      total += chunkBytes;
      const frame: OutboundFrame = { type: "stream", id: callId, seq, chunk: checked.data, done: false };
      if (frameBytes(frame) > frameLimit || total > MAX_STREAM_BYTES) {
        await channel.send({
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
      // 等写背压，避免生产者把大流堆进内存
      await channel.send(frame);
      seq += 1;
    }
    await channel.send({ type: "stream", id: callId, seq, chunk: {}, done: true });
    await channel.send({ type: "result", id: callId, ok: true, output: { completed: true, chunks: seq } });
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
      await iterator.return?.();
    }
  }
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

/** 等 promise 结束；返回 true 表示在 `ms` 内完成，false 表示超时 */
async function waitWithDeadline(promise: Promise<unknown>, ms: number): Promise<boolean> {
  if (!Number.isFinite(ms) || ms <= 0) {
    return false;
  }
  return await new Promise<boolean>((resolve) => {
    const timer = setTimeout(() => {
      resolve(false);
    }, ms);
    const done = (): void => {
      clearTimeout(timer);
      resolve(true);
    };
    promise.then(done, done);
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