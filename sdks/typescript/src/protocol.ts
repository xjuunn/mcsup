/**
 * 帧协议（plugins.md §6）
 *
 * 契约类型（帧、错误码、能力名、限额、退出码）全部来自
 * `./contracts/v1.js`——该文件由 `protocol::plugin::contracts` 生成，
 * 禁止手写（§14.4）。本文件只保留编解码与协议错误。
 *
 * 一行一帧 JSON + `\n`，字段 snake_case。已知类型缺关键字段视为协议错误
 * （终止会话）；未知 `type` 忽略并保持连接。单行 ≤ `MAX_FRAME_BYTES`、嵌套深度 ≤ 32
 * 在读取层检查。
 */

import {
  ERROR_CATEGORY,
  MAX_FRAME_BYTES,
  MAX_JSON_DEPTH,
  PROTOCOL_VERSION,
  RETRYABLE_ERRORS,
  type CallFrame,
  type CancelFrame,
  type ErrorCode,
  type InboundFrame,
  type OutboundFrame,
  type ResultFrame,
  type WireError,
} from "./contracts/v1.js";

export * from "./contracts/v1.js";

/** 协议错误：必须终止会话 */
export class ProtocolError extends Error {
  public constructor(message: string) {
    super(message);
    this.name = "ProtocolError";
  }
}

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

function requireObjectField(frame: LooseFrame, key: string): Record<string, unknown> {
  const value = frame[key];
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new ProtocolError(`frame field ${key} must be an object`);
  }
  return value as Record<string, unknown>;
}

function requireIntIn(record: Record<string, unknown>, path: string, key: string, min: number): number {
  const value = record[key];
  if (typeof value !== "number" || !Number.isInteger(value) || value < min) {
    throw new ProtocolError(`frame field ${path}.${key} must be an integer >= ${min}`);
  }
  return value;
}

function requireOwn(record: Record<string, unknown>, path: string, key: string): unknown {
  if (!Object.prototype.hasOwnProperty.call(record, key)) {
    throw new ProtocolError(`frame field ${path}.${key} is required`);
  }
  return record[key];
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
      const limits = requireObjectField(frame, "limits");
      const config = requireObjectField(frame, "config");
      const scope = requireObjectField(frame, "scope");
      const scopeKind = scope["kind"];
      if (typeof scopeKind !== "string" || scopeKind === "") {
        throw new ProtocolError("frame field scope.kind must be a non-empty string");
      }
      const scopeServer = scope["server"];
      if (scopeServer !== undefined && typeof scopeServer !== "string") {
        throw new ProtocolError("frame field scope.server must be a string");
      }
      return {
        type: "welcome",
        session_id: requireString(frame, "session_id"),
        host_version: requireString(frame, "host_version"),
        // 旧宿主不发送 locale：按英文回退，不能因此握手失败
        locale: typeof frame["locale"] === "string" && frame["locale"] !== "" ? frame["locale"] : "en",
        granted: stringArray(frame, "granted"),
        limits: {
          inflight: requireIntIn(limits, "limits", "inflight", 1),
          frame_bytes: requireIntIn(limits, "limits", "frame_bytes", 1),
          event_queue: requireIntIn(limits, "limits", "event_queue", 1),
          log_lines_per_sec: requireIntIn(limits, "limits", "log_lines_per_sec", 1),
        },
        config: {
          revision: requireIntIn(config, "config", "revision", 0),
          values: requireOwn(config, "config", "values"),
        },
        scope: {
          kind: scopeKind,
          ...(scopeServer === undefined ? {} : { server: scopeServer }),
        },
      };
    }
    case "reject":
      return {
        type: "reject",
        code: requireString(frame, "code") as RejectCodeText,
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
        target: optionalString(frame, "target"),
        trace: optionalString(frame, "trace"),
        input: frame["input"],
      } as CallFrame;
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
        values: requireOwn(frame, "config", "values"),
      };
    case "ping":
      return { type: "ping", n: requireInt(frame, "n") };
    case "shutdown":
      return { type: "shutdown", grace_ms: requireIntIn(frame, "shutdown", "grace_ms", 0) };
    default:
      // 未知类型：忽略并保持连接（§6.1）
      return { type: "unknown", kind };
  }
}

/** 编码一帧（含末尾换行） */
export function encodeFrame(frame: OutboundFrame): string {
  return `${JSON.stringify({ v: PROTOCOL_VERSION, ...frame })}\n`;
}

/**
 * 估算一帧 JSON 的字节数。
 *
 * 用于限额检查，不含版本前缀与换行；与实际编码长度的差值固定，不影响
 * 与宿主约定的上限判定。
 */
export function frameBytes(value: unknown): number {
  return Buffer.byteLength(JSON.stringify(value) ?? "", "utf8");
}

/** 构造线上错误对象；分类与可重试性来自生成的契约表 */
export function wireError(code: string, message: string): WireError {
  const known = (ERROR_CATEGORY as Record<string, string | undefined>)[code];
  return {
    code: code as ErrorCode,
    category: (known ?? "failure") as WireError["category"],
    retryable: (RETRYABLE_ERRORS as readonly string[]).includes(code),
    message,
  };
}

/** `reject.code` 的线上文本；未知码由宿主日志记录，插件按 `rejected` 处理 */
type RejectCodeText = Extract<InboundFrame, { type: "reject" }>["code"];
