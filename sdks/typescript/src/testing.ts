/**
 * 测试设施：内存双工 MockHost（sdk-design.md §10）
 *
 * 不开 socket：宿主帧直接投给运行时，插件帧被记录并可断言。
 */

import type { Capability, DefinedPlugin, PluginDefinition } from "./define.js";
import type { CtxFor, ScopeView } from "./ctx.js";
import {
  DEFAULT_EVENT_QUEUE,
  DEFAULT_INFLIGHT,
  DEFAULT_LOG_LINES_PER_SEC,
  MAX_FRAME_BYTES,
  PROTOCOL_VERSION,
  type CallFrame,
  type InboundFrame,
  type OutboundFrame,
  type RejectCode,
  type WelcomeLimits,
} from "./protocol.js";
import { type FrameChannel } from "./connection.js";
import { Session } from "./runtime.js";
import type { PluginErrorCode } from "./errors.js";
import type { SchemaLike } from "./schema.js";

/** 宿主默认限额（§5.4）：数值来自生成契约，形状对应 welcome.limits */
const DEFAULT_LIMITS: WelcomeLimits = {
  inflight: DEFAULT_INFLIGHT,
  frame_bytes: MAX_FRAME_BYTES,
  event_queue: DEFAULT_EVENT_QUEUE,
  log_lines_per_sec: DEFAULT_LOG_LINES_PER_SEC,
};

/** 测试可覆盖的 welcome.limits 字段 */
export type MockLimits = Partial<WelcomeLimits>;

export interface MockHostOptions {
  /** 本次授予的能力；默认等于插件声明 */
  readonly granted?: readonly string[];
  /** 初始配置（已校验的对象），默认 `{}` */
  readonly config?: unknown;
  readonly configRevision?: number;
  readonly sessionId?: string;
  readonly scope?: ScopeView;
  /** 握手下发的限额；未给出的字段用默认值 */
  readonly limits?: MockLimits;
  /** 握手直接拒绝 */
  readonly reject?: { readonly code: RejectCode; readonly message: string };
  /** 宿主对能力调用的处理 */
  readonly handle?: (call: { readonly capability: string; readonly method: string; readonly input: unknown }) => unknown;
}

/** 断言失败 */
export class MockAssertionError extends Error {
  public constructor(message: string) {
    super(message);
    this.name = "MockAssertionError";
  }
}

/** 内存宿主 */
export class MockHost<C extends readonly Capability[], Cfg extends SchemaLike = SchemaLike> {
  private readonly def: PluginDefinition<C, string, Cfg>;
  private readonly options: MockHostOptions;
  private readonly session: Session<C, Cfg>;

  private readonly outbound: OutboundFrame[] = [];
  private readonly listeners = new Set<(frame: OutboundFrame) => void>();
  private readonly exitWaiters = new Set<(code: number) => void>();
  private frameHandler: ((frame: InboundFrame) => void) | undefined;
  private closeHandler: ((reason: string) => void) | undefined;
  private hostCallId = 100;
  /** 插件是否已收到 welcome */
  public welcomed = false;
  /** 会话结束的退出码 */
  public exitCode: number | undefined;

  public constructor(
    plugin: DefinedPlugin<C, string, Cfg> | PluginDefinition<C, string, Cfg>,
    options: MockHostOptions = {},
  ) {
    this.def = "definition" in plugin ? plugin.definition : plugin;
    this.options = options;
    const channel = this.makeChannel();
    this.session = new Session(this.def, channel);
  }

  /** 启动会话并完成握手；退出码用 `waitExit()` 取 */
  public async start(): Promise<void> {
    const done = this.session.start("0".repeat(64));
    done.then((code) => {
      this.exitCode = code;
      const waiters = [...this.exitWaiters];
      this.exitWaiters.clear();
      for (const waiter of waiters) {
        waiter(code);
      }
    });
    // 等 hello 落地后自动完成握手
    await this.flush();
    const hello = this.lastFrame("hello");
    if (hello === undefined) {
      throw new MockAssertionError("plugin did not send hello");
    }
    if (this.options.reject !== undefined) {
      this.deliver({
        type: "reject",
        code: this.options.reject.code,
        message: this.options.reject.message,
      });
      await this.flush();
      return;
    }
    this.deliver({
      type: "welcome",
      session_id: this.options.sessionId ?? "s-test",
      host_version: "0.0.0-test",
      granted: this.options.granted ?? this.def.capabilities,
      limits: { ...DEFAULT_LIMITS, ...this.options.limits },
      config: { revision: this.options.configRevision ?? 1, values: this.options.config ?? {} },
      scope: this.options.scope ?? { kind: this.def.scope ?? "machine" },
    });
    this.welcomed = true;
    await this.flush();
  }

  /** 插件上下文（测试可直接调用能力） */
  public get ctx(): CtxFor<C, Cfg> {
    return this.session.context;
  }

  /** 插件发出的全部帧 */
  public get frames(): readonly OutboundFrame[] {
    return this.outbound;
  }

  /** 调用一个 op 并等结果 */
  public async callOp(op: string, input: unknown = {}): Promise<CallResult> {
    const id = this.nextHostId();
    this.deliver({ type: "call", id, op, input });
    return await this.awaitResult(id);
  }

  /** 发配置变更并等回执 */
  public async changeConfig(revision: number, values: unknown): Promise<CallResult> {
    const id = this.nextHostId();
    this.deliver({ type: "config", id, revision, values });
    return await this.awaitResult(id);
  }

  /** 投递事件 */
  public async emit(kind: string, data: unknown, server?: string): Promise<void> {
    this.deliver({ type: "event", seq: this.nextHostId(), kind, at: new Date().toISOString(), server, data });
    await this.flush();
  }

  /** 发送取消 */
  public cancel(id: number, reason = "test"): void {
    this.deliver({ type: "cancel", id, reason });
  }

  /** 注入一帧原始宿主帧（帧一致性用例用；绕过高层封装） */
  public inject(frame: InboundFrame): void {
    this.deliver(frame);
  }

  /** 发送 shutdown 并等进程退出码 */
  public async shutdown(graceMs = 5000): Promise<number> {
    this.deliver({ type: "shutdown", grace_ms: graceMs });
    return await this.waitExit();
  }

  /** 心跳往返 */
  public async ping(n = 1): Promise<void> {
    const before = this.outbound.length;
    this.deliver({ type: "ping", n });
    await this.flush();
    const pong = this.outbound.slice(before).find((f) => f.type === "pong");
    if (pong === undefined || pong.n !== n) {
      throw new MockAssertionError(`expected pong ${n}`);
    }
  }

  /** 等会话结束（默认最多 5 s） */
  public async waitExit(timeoutMs = 5_000): Promise<number> {
    if (this.exitCode !== undefined) {
      return this.exitCode;
    }
    return await new Promise<number>((resolve, reject) => {
      let timer: NodeJS.Timeout | undefined;
      const onExit = (code: number): void => {
        if (timer !== undefined) {
          clearTimeout(timer);
        }
        resolve(code);
      };
      timer = setTimeout(() => {
        this.exitWaiters.delete(onExit);
        reject(new MockAssertionError("session did not exit"));
      }, timeoutMs);
      this.exitWaiters.add(onExit);
    });
  }

  /** 等待并返回某 id 的结果帧（默认最多 5 s） */
  public async awaitResult(id: number, timeoutMs = 5_000): Promise<CallResult> {
    const existing = this.resultOf(id);
    if (existing !== undefined) {
      return existing;
    }
    return await new Promise<CallResult>((resolve, reject) => {
      let timer: NodeJS.Timeout | undefined;
      const listener = (frame: OutboundFrame): void => {
        if (frame.type !== "result" || frame.id !== id) {
          return;
        }
        this.listeners.delete(listener);
        if (timer !== undefined) {
          clearTimeout(timer);
        }
        resolve(toCallResult(frame));
      };
      timer = setTimeout(() => {
        this.listeners.delete(listener);
        reject(new MockAssertionError(`no result for call ${id}`));
      }, timeoutMs);
      this.listeners.add(listener);
    });
  }

  private resultOf(id: number): CallResult | undefined {
    for (const frame of this.outbound) {
      if (frame.type === "result" && frame.id === id) {
        return toCallResult(frame);
      }
    }
    return undefined;
  }

  /** 某 id 的 stream 帧 */
  public streamFrames(id: number): Array<Extract<OutboundFrame, { type: "stream" }>> {
    return this.outbound.filter((f): f is Extract<OutboundFrame, { type: "stream" }> => f.type === "stream" && f.id === id);
  }

  /** 最近一帧指定类型 */
  public lastFrame<T extends OutboundFrame["type"]>(type: T): Extract<OutboundFrame, { type: T }> | undefined {
    for (let i = this.outbound.length - 1; i >= 0; i -= 1) {
      const frame = this.outbound[i];
      if (frame?.type === type) {
        return frame as Extract<OutboundFrame, { type: T }>;
      }
    }
    return undefined;
  }

  private nextHostId(): number {
    this.hostCallId += 1;
    return this.hostCallId;
  }

  private deliver(frame: InboundFrame): void {
    if (this.frameHandler === undefined) {
      throw new MockAssertionError("session is not started");
    }
    // 内存通道直接投递类型化的帧；信封 `v` 由这里补上
    this.frameHandler({ v: PROTOCOL_VERSION, ...frame } as unknown as InboundFrame);
  }

  /** 让出若干轮事件循环，等运行时把已投递的帧处理完 */
  private async flush(): Promise<void> {
    for (let i = 0; i < 8; i += 1) {
      await tick();
    }
  }

  private makeChannel(): FrameChannel {
    const host = this;
    return {
      onFrame(handler) {
        host.frameHandler = handler;
      },
      onClose(handler) {
        host.closeHandler = handler;
      },
      send(frame) {
        host.outbound.push(frame);
        for (const listener of host.listeners) {
          listener(frame);
        }
      },
      async call(capability, method, input) {
        if (host.options.handle !== undefined) {
          return await host.options.handle({ capability, method, input });
        }
        return null;
      },
      close() {
        const handler = host.closeHandler;
        return new Promise<void>((resolve) => {
          queueMicrotask(() => {
            handler?.("closed by mock host");
            resolve();
          });
        });
      },
    };
  }
}

/** 调用结果 */
export type CallResult =
  | { readonly ok: true; readonly output: unknown }
  | { readonly ok: false; readonly error: { readonly code: PluginErrorCode; readonly message: string } };

function toCallResult(frame: Extract<OutboundFrame, { type: "result" }>): CallResult {
  if (frame.ok) {
    return { ok: true, output: frame.output };
  }
  return {
    ok: false,
    error: { code: frame.error?.code ?? "internal", message: frame.error?.message ?? "failed" },
  };
}

function tick(): Promise<void> {
  return new Promise<void>((resolve) => {
    setTimeout(resolve, 0);
  });
}

/** 创建内存宿主 */
export function createMockHost<C extends readonly Capability[], Cfg extends SchemaLike = SchemaLike>(
  plugin: DefinedPlugin<C, string, Cfg> | PluginDefinition<C, string, Cfg>,
  options: MockHostOptions = {},
): MockHost<C, Cfg> {
  return new MockHost(plugin, options);
}

/** 重新导出协议帧类型，便于断言 */
export type { CallFrame, InboundFrame, OutboundFrame };
