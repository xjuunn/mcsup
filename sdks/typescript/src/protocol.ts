/**
 * 帧协议（plugins.md §6）
 *
 * 一行一帧 JSON + `\n`，字段 snake_case。已知类型缺关键字段视为协议错误
 * （终止会话）；未知 `type` 忽略并保持连接。单行 ≤ 1 MiB、嵌套深度 ≤ 32
 * 在读取层检查。
 */

/** 插件协议版本；与宿主 `PLUGIN_PROTOCOL_VERSION` 一致 */
export const PROTOCOL_VERSION = 1;

/** 插件 API 主版本；与宿主 `PLUGIN_API_VERSION` 一致 */
export const PLUGIN_API_VERSION = 1;

/** 单帧字节上限 */
export const MAX_FRAME_BYTES = 1024 * 1024;

/** JSON 嵌套深度上限 */
export const MAX_JSON_DEPTH = 32;

/** 协议错误：必须终止会话 */
export class ProtocolError extends Error {
  public constructor(message: string) {
    super(message);
    this.name = "ProtocolError";
  }
}

// ---------------------------------------------------------------------------
// 帧类型
// ---------------------------------------------------------------------------

/** 插件 → 宿主：握手 */
export interface HelloFrame {
  readonly type: "hello";
  readonly token: string;
  readonly api: number;
  readonly plugin_id: string;
  readonly plugin_version: string;
  readonly platform: string;
  readonly pid: number;
  readonly requested: readonly string[];
  readonly subscriptions: readonly string[];
}

export interface WelcomeLimits {
  readonly inflight: number;
  readonly frame_bytes: number;
  readonly event_queue: number;
  readonly log_lines_per_sec: number;
}

/** 宿主 → 插件：授予 */
export interface WelcomeFrame {
  readonly type: "welcome";
  readonly session_id: string;
  readonly host_version: string;
  readonly granted: readonly string[];
  readonly limits: WelcomeLimits;
  readonly config: { readonly revision: number; readonly values: unknown };
  readonly scope: { readonly kind: string; readonly server?: string };
}

/** 宿主 → 插件：拒绝握手 */
export interface RejectFrame {
  readonly type: "reject";
  readonly code: string;
  readonly message: string;
  readonly details?: unknown;
}

/** 宿主 → 插件：事件投递 */
export interface EventFrame {
  readonly type: "event";
  readonly seq: number;
  readonly kind: string;
  readonly at: string;
  readonly server?: string;
  readonly data: unknown;
}

/** 双向：调用 */
export interface CallFrame {
  readonly type: "call";
  readonly id: number;
  readonly op?: string;
  readonly capability?: string;
  readonly method?: string;
  readonly contract?: string;
  readonly input: unknown;
}

export interface WireError {
  readonly code: string;
  readonly category: string;
  readonly retryable: boolean;
  readonly message: string;
}

/** 双向：调用结果 */
export interface ResultFrame {
  readonly type: "result";
  readonly id: number;
  readonly ok: boolean;
  readonly output?: unknown;
  readonly error?: WireError;
}

/** 插件 → 宿主：流式分块 */
export interface StreamFrame {
  readonly type: "stream";
  readonly id: number;
  readonly seq: number;
  readonly chunk: unknown;
  readonly done: boolean;
}

/** 双向：取消 */
export interface CancelFrame {
  readonly type: "cancel";
  readonly id: number;
  readonly reason?: string;
}

/** 宿主 → 插件：配置变更（需回 result） */
export interface ConfigFrame {
  readonly type: "config";
  readonly id: number;
  readonly revision: number;
  readonly values: unknown;
}

/** 宿主 → 插件：心跳 */
export interface PingFrame {
  readonly type: "ping";
  readonly n: number;
}

/** 插件 → 宿主：心跳回应 */
export interface PongFrame {
  readonly type: "pong";
  readonly n: number;
}

/** 宿主 → 插件：优雅退出 */
export interface ShutdownFrame {
  readonly type: "shutdown";
  readonly grace_ms: number;
}

/** 未知 `type`：忽略并记日志 */
export interface UnknownFrame {
  readonly type: "unknown";
  readonly kind: string;
}

/** 宿主 → 插件方向的帧 */
export type InboundFrame =
  | WelcomeFrame
  | RejectFrame
  | EventFrame
  | CallFrame
  | ResultFrame
  | CancelFrame
  | ConfigFrame
  | PingFrame
  | ShutdownFrame
  | UnknownFrame;

/** 插件 → 宿主方向的帧 */
export type OutboundFrame =
  | HelloFrame
  | ResultFrame
  | StreamFrame
  | CallFrame
  | PongFrame;

// ---------------------------------------------------------------------------
// 编解码
// ---------------------------------------------------------------------------

interface LooseFrame {
  readonly v?: unknown;
  readonly type?: unknown;
  readonly [key: string]: unknown;
}

function frameDepth(value: unknown, depth: number): number {
  if (depth > MAX_JSON_DEPTH) {
    return depth;
  }
  if (Array.isArray(value)) {
    let max = depth;
    for (const item of value) {
      max = Math.max(max, frameDepth(item, depth + 1));
    }
    return max;
  }
  if (typeof value === "object" && value !== null) {
    let max = depth;
    for (const item of Object.values(value)) {
      max = Math.max(max, frameDepth(item, depth + 1));
    }
    return max;
  }
  return depth;
}

function requireInt(frame: LooseFrame, key: string): number {
  const value = frame[key];
  if (typeof value !== "number" || !Number.isInteger(value)) {
    throw new ProtocolError(`frame field ${key} must be an integer`);
  }
  return value;
}

function requireString(frame: LooseFrame, key: string): string {
  const value = frame[key];
  if (typeof value !== "string") {
    throw new ProtocolError(`frame field ${key} must be a string`);
  }
  return value;
}

function optionalString(frame: LooseFrame, key: string): string | undefined {
  const value = frame[key];
  return typeof value === "string" ? value : undefined;
}

function stringArray(frame: LooseFrame, key: string): string[] {
  const value = frame[key];
  if (!Array.isArray(value) || value.some((item) => typeof item !== "string")) {
    throw new ProtocolError(`frame field ${key} must be a string array`);
  }
  return value as string[];
}

/**
 * 解析一行（不含换行）。
 *
 * @throws {ProtocolError} 非法 JSON、版本不匹配或已知类型缺字段
 */
export function parseFrame(text: string): InboundFrame {
  if (text.length > MAX_FRAME_BYTES) {
    throw new ProtocolError("frame exceeds the size limit");
  }
  let value: unknown;
  try {
    value = JSON.parse(text) as unknown;
  } catch (err) {
    throw new ProtocolError(`invalid JSON: ${String(err)}`);
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new ProtocolError("frame must be a JSON object");
  }
  const frame = value as LooseFrame;
  if (frameDepth(value, 0) > MAX_JSON_DEPTH) {
    throw new ProtocolError("frame nesting exceeds the depth limit");
  }
  const version = frame["v"];
  if (version !== PROTOCOL_VERSION) {
    throw new ProtocolError(`unsupported protocol version ${String(version)}`);
  }
  const kind = frame["type"];
  if (typeof kind !== "string") {
    throw new ProtocolError("frame is missing type");
  }
  switch (kind) {
    case "welcome": {
      const limits = frame["limits"];
      if (typeof limits !== "object" || limits === null) {
        throw new ProtocolError("welcome.limits must be an object");
      }
      const config = frame["config"];
      if (typeof config !== "object" || config === null) {
        throw new ProtocolError("welcome.config must be an object");
      }
      const scope = frame["scope"];
      if (typeof scope !== "object" || scope === null) {
        throw new ProtocolError("welcome.scope must be an object");
      }
      return {
        type: "welcome",
        session_id: requireString(frame, "session_id"),
        host_version: requireString(frame, "host_version"),
        granted: stringArray(frame, "granted"),
        limits: limits as WelcomeLimits,
        config: config as { revision: number; values: unknown },
        scope: scope as { kind: string; server?: string },
      };
    }
    case "reject":
      return {
        type: "reject",
        code: requireString(frame, "code"),
        message: requireString(frame, "message"),
        details: frame["details"],
      };
    case "event":
      return {
        type: "event",
        seq: requireInt(frame, "seq"),
        kind: requireString(frame, "kind"),
        at: requireString(frame, "at"),
        server: optionalString(frame, "server"),
        data: frame["data"],
      };
    case "call":
      return {
        type: "call",
        id: requireInt(frame, "id"),
        op: optionalString(frame, "op"),
        capability: optionalString(frame, "capability"),
        method: optionalString(frame, "method"),
        contract: optionalString(frame, "contract"),
        input: frame["input"],
      };
    case "result": {
      const ok = frame["ok"];
      if (typeof ok !== "boolean") {
        throw new ProtocolError("result.ok must be a boolean");
      }
      const result: ResultFrame = { type: "result", id: requireInt(frame, "id"), ok };
      if (frame["output"] !== undefined) {
        (result as { output?: unknown }).output = frame["output"];
      }
      if (frame["error"] !== undefined) {
        const err = frame["error"];
        if (typeof err !== "object" || err === null) {
          throw new ProtocolError("result.error must be an object");
        }
        (result as { error?: unknown }).error = err;
      }
      return result;
    }
    case "cancel": {
      const cancel: CancelFrame = { type: "cancel", id: requireInt(frame, "id") };
      const reason = optionalString(frame, "reason");
      if (reason !== undefined) {
        (cancel as { reason?: string }).reason = reason;
      }
      return cancel;
    }
    case "config":
      return {
        type: "config",
        id: requireInt(frame, "id"),
        revision: requireInt(frame, "revision"),
        values: frame["values"],
      };
    case "ping":
      return { type: "ping", n: requireInt(frame, "n") };
    case "shutdown":
      return { type: "shutdown", grace_ms: requireInt(frame, "grace_ms") };
    default:
      // 未知类型：忽略并保持连接（§6.1）
      return { type: "unknown", kind };
  }
}

/** 编码一帧（含末尾换行） */
export function encodeFrame(frame: OutboundFrame): string {
  return `${JSON.stringify({ v: PROTOCOL_VERSION, ...frame })}\n`;
}

/** 构造线上错误对象 */
export function wireError(code: string, message: string): WireError {
  return {
    code,
    category: categoryOfText(code),
    retryable: code === "timeout" || code === "unavailable" || code === "crashed",
    message,
  };
}

function categoryOfText(code: string): string {
  switch (code) {
    case "invalid_input":
    case "limit_exceeded":
    case "unsupported":
      return "usage";
    case "not_found":
      return "not_found";
    case "denied":
      return "unauthorized";
    case "conflict":
      return "conflict";
    case "cancelled":
      return "cancelled";
    case "timeout":
    case "unavailable":
    case "crashed":
      return "backend";
    default:
      return "failure";
  }
}
