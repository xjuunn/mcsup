/**
 * 测试设施：内存双工 MockHost（sdk-design.md §10）
 *
 * 不开 socket：宿主帧直接投给运行时，插件帧被记录并可断言。
 */

import type { Capability, DefinedPlugin, PluginDefinition } from "./define.js";
import { PROTOCOL_VERSION, type CallFrame, type InboundFrame, type OutboundFrame } from "./protocol.js";
import { type FrameChannel, Session } from "./runtime.js";

/** 宿主默认限额（§5.4） */
const DEFAULT_LIMITS = { inflight: 8, frame_bytes: 1024 * 1024, event_queue: 1024, log_lines_per_sec: 200 };

export interface MockHostOptions {
  /** 本次授予的能力；默认等于插件声明 */
  readonly granted?: readonly string[];
  /** 初始配置（已校验的对象），默认 `{}` */
  readonly config?: unknown;
  readonly configRevision?: number;
  readonly sessionId?: string;
  readonly scope?: { readonly kind: string; readonly server?: string };
  /** 握手直接拒绝 */
  readonly reject?: { readonly code: string; readonly message: string };
  /** 宿主对能力调用的处理 */
  readonly handle?: (call: { readonly capability: string; readonly method: string; readonly input: unknown }) => unknown;
}

interface PendingResult {
  readonly resolve: (frame: OutboundFrame) => void;
}

/** 断言失败 */
export class MockAssertionError extends Error {
  public constructor(message: string) {
    super(message);
    this.name = "MockAssertionError";
  }
}

/** 内存宿主 */
export class MockHost<C extends readonly Capability[]> {
  private readonly def: PluginDefinition<C>;
  private readonly options: MockHostOptions;
  private readonly session: Session<C>;

  private readonly outbound: OutboundFrame[] = [];
  private readonly listeners = new Set<(frame: OutboundFrame) => void>();
  private readonly waiting: PendingResult[] = [];
  private frameHandler: ((frame: InboundFrame) => void) | undefined;
  private closeHandler: ((reason: string) => void) | undefined;
  private hostCallId = 100;
  /** 插件是否已收到 welcome */
  public welcomed = false;
  /** 会话结束的退出码 */
  public exitCode: number | undefined;

  public constructor(plugin: DefinedPlugin<C> | PluginDefinition<C>, options: MockHostOptions = {}) {
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
      limits: DEFAULT_LIMITS,
      config: { revision: this.options.configRevision ?? 1, values: this.options.config ?? {} },
      scope: this.options.scope ?? { kind: this.def.scope ?? "machine" },
    });
    this.welcomed = true;
    await this.flush();
  }

  /** 插件上下文（测试可直接调用能力） */
  public get ctx() {
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

  /** 等会话结束 */
  public async waitExit(): Promise<number> {
    if (this.exitCode !== undefined) {
      return this.exitCode;
    }
    for (let i = 0; i < 200; i += 1) {
      await tick();
      if (this.exitCode !== undefined) {
        return this.exitCode;
      }
    }
    throw new MockAssertionError("session did not exit");
  }

  /** 等待并返回某 id 的结果帧 */
  public async awaitResult(id: number): Promise<CallResult> {
    for (let i = 0; i < 400; i += 1) {
      const existing = this.outbound.find((f) => f.type === "result" && f.id === id);
      if (existing !== undefined) {
        const result = existing as Extract<OutboundFrame, { type: "result" }>;
        if (result.ok) {
          return { ok: true, output: result.output };
        }
        return {
          ok: false,
          error: { code: result.error?.code ?? "internal", message: result.error?.message ?? "failed" },
        };
      }
      await tick();
    }
    throw new MockAssertionError(`no result for call ${id}`);
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

  private deliver(frame: Omit<InboundFrame, "v"> | InboundFrame): void {
    if (this.frameHandler === undefined) {
      throw new MockAssertionError("session is not started");
    }
    const payload = { v: PROTOCOL_VERSION, ...(frame as object) };
    this.frameHandler(payload as unknown as InboundFrame);
  }

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
        // live/stream 帧不会改变会话状态，立即回放给运行时也不会重入
        // （运行时只在事件循环里处理帧），这里只把 result 之外的控制帧回放给它
      },
      async call(capability, method, input) {
        if (host.options.handle !== undefined) {
          return await host.options.handle({ capability, method, input });
        }
        return null;
      },
      close() {
        const handler = host.closeHandler;
        queueMicrotask(() => {
          handler?.("closed by mock host");
        });
      },
    };
  }
}

/** 调用结果 */
export type CallResult =
  | { readonly ok: true; readonly output: unknown }
  | { readonly ok: false; readonly error: { readonly code: string; readonly message: string } };

function tick(): Promise<void> {
  return new Promise<void>((resolve) => {
    setTimeout(resolve, 0);
  });
}

/** 创建内存宿主 */
export function createMockHost<C extends readonly Capability[]>(
  plugin: DefinedPlugin<C> | PluginDefinition<C>,
  options: MockHostOptions = {},
): MockHost<C> {
  return new MockHost(plugin, options);
}

/** 重新导出协议帧类型，便于断言 */
export type { CallFrame, InboundFrame, OutboundFrame };