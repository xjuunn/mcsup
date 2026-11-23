// GENERATED FROM protocol::plugin. DO NOT EDIT.
// 由 `cargo run -q -p mcsup-protocol --example gen-contracts` 生成。
// 信封字段：每帧都带 `v`（协议版本）与 `type`（帧类型）。

// ---------------------------------------------------------------------------
// 版本与限额
// ---------------------------------------------------------------------------

/** 插件协议版本（帧信封 `v`） */
export const PROTOCOL_VERSION = 1;

/** 插件 API 主版本；SDK 主版本与它相等 */
export const PLUGIN_API_VERSION = 1;

/** 单帧字节上限 */
export const MAX_FRAME_BYTES = 25165824;

/** JSON 嵌套深度上限 */
export const MAX_JSON_DEPTH = 32;

/** inflight 缺省上限 */
export const DEFAULT_INFLIGHT = 8;

/** 事件队列缺省长度 */
export const DEFAULT_EVENT_QUEUE = 1024;

/** 日志限速缺省值（行/秒） */
export const DEFAULT_LOG_LINES_PER_SEC = 200;

/** schema 嵌套深度上限 */
export const MAX_SCHEMA_DEPTH = 6;

/** 单对象属性数上限 */
export const MAX_SCHEMA_PROPERTIES = 64;

/** enum 取值数上限 */
export const MAX_ENUM_VALUES = 100;

/** 单个 schema 节点数上限 */
export const MAX_SCHEMA_NODES = 512;

/** 单个 schema 序列化字节上限 */
export const MAX_SCHEMA_BYTES = 65536;

/** 整个 descriptor 字节上限 */
export const MAX_DESCRIPTOR_BYTES = 262144;

/** 单字符串 `maxLength` 上限 */
export const MAX_STRING_LENGTH = 8192;

/** op 超时下限（毫秒） */
export const MIN_OP_TIMEOUT_MS = 1000;

/** op 超时上限（毫秒） */
export const MAX_OP_TIMEOUT_MS = 300000;

/** op 超时缺省值（毫秒） */
export const DEFAULT_OP_TIMEOUT_MS = 30000;

/** op 标题字符上限 */
export const MAX_OP_TITLE_CHARS = 64;

/** op 描述字符上限 */
export const MAX_OP_DESCRIPTION_CHARS = 512;

// ---------------------------------------------------------------------------
// 退出码（plugins.md §6.7）
// ---------------------------------------------------------------------------

export const ExitCode = {
  Ok: 0,
  Failure: 1,
  Protocol: 2,
  Config: 3,
  ApiMismatch: 4,
} as const;

export type ExitCodeValue = (typeof ExitCode)[keyof typeof ExitCode];

// ---------------------------------------------------------------------------
// 错误码与分类
// ---------------------------------------------------------------------------

export type ErrorCode =
  | "invalid_input"
  | "not_found"
  | "denied"
  | "conflict"
  | "timeout"
  | "cancelled"
  | "unavailable"
  | "crashed"
  | "limit_exceeded"
  | "unsupported"
  | "internal"
  ;

export type ErrorCategory =
  | "usage"
  | "not_found"
  | "unauthorized"
  | "conflict"
  | "backend"
  | "cancelled"
  | "failure"
  ;

/** 错误码 → 分类（宿主与 SDK 共用一张表） */
export const ERROR_CATEGORY: Readonly<Record<ErrorCode, ErrorCategory>> = {
  invalid_input: "usage",
  not_found: "not_found",
  denied: "unauthorized",
  conflict: "conflict",
  timeout: "backend",
  cancelled: "cancelled",
  unavailable: "backend",
  crashed: "backend",
  limit_exceeded: "usage",
  unsupported: "usage",
  internal: "failure",
};

/** 可重试的错误码 */
export const RETRYABLE_ERRORS: readonly ErrorCode[] = [
  "timeout",
  "unavailable",
  "crashed",
];

// ---------------------------------------------------------------------------
// 能力与订阅名
// ---------------------------------------------------------------------------

export type CapabilityName =
  | "events"
  | "storage:kv"
  | "server:read"
  | "console:read"
  | "console:write"
  | "net:outbound"
  | "secret:rw"
  | "backup"
  | "files:read"
  | "files:write"
  | "server:lifecycle"
  | "content"
  | "java"
  | "process:spawn"
  | "mc:client"
  ;

/** 当前版本可授予的能力（未开放项不在内） */
export type OpenCapabilityName =
  | "events"
  | "storage:kv"
  | "server:read"
  | "console:read"
  | "console:write"
  | "net:outbound"
  | "secret:rw"
  | "backup"
  | "files:read"
  | "files:write"
  | "server:lifecycle"
  | "content"
  | "java"
  ;

export type SubscriptionName =
  | "chat:message"
  | "player:joined"
  | "player:left"
  | "console:line"
  | "server:starting"
  | "server:started"
  | "server:stopped"
  | "server:crashed"
  | "backup:completed"
  | "backup:failed"
  ;

/** 握手拒绝原因（plugins.md §6.2） */
export type RejectCode =
  | "unauthorized"
  | "identity_mismatch"
  | "api_mismatch"
  | "schema_mismatch"
  | "scope_mismatch"
  | "not_approved"
  | "already_running"
  | "rejected"
  ;

// ---------------------------------------------------------------------------
// 具名接口
// ---------------------------------------------------------------------------

export interface WelcomeLimits {
  readonly event_queue: number;
  readonly frame_bytes: number;
  readonly inflight: number;
  readonly log_lines_per_sec: number;
}

export interface ConfigValues {
  readonly revision: number;
  readonly values: unknown;
}

export interface WelcomeScope {
  readonly kind: string;
  readonly server?: string;
}

export interface WireError {
  readonly category: ErrorCategory;
  readonly code: ErrorCode;
  readonly message: string;
  readonly retryable: boolean;
}

// ---------------------------------------------------------------------------
// 帧
// ---------------------------------------------------------------------------

export interface HelloFrame {
  readonly type: "hello";
  readonly api: number;
  readonly pid: number;
  readonly platform: string;
  readonly plugin_id: string;
  readonly plugin_version: string;
  readonly requested: readonly string[];
  readonly subscriptions: readonly string[];
  readonly token: string;
}

export interface WelcomeFrame {
  readonly type: "welcome";
  readonly config: ConfigValues;
  readonly granted: readonly string[];
  readonly host_version: string;
  readonly limits: WelcomeLimits;
  readonly scope: WelcomeScope;
  readonly session_id: string;
}

export interface RejectFrame {
  readonly type: "reject";
  readonly code: RejectCode;
  readonly details?: unknown;
  readonly message: string;
}

export interface EventFrame {
  readonly type: "event";
  readonly at: string;
  readonly data: unknown;
  readonly kind: string;
  readonly seq: number;
  readonly server?: string;
}

export interface CallFrame {
  readonly type: "call";
  readonly capability?: string;
  readonly contract?: string;
  readonly id: number;
  readonly input: unknown;
  readonly method?: string;
  readonly op?: string;
  readonly target?: string;
  readonly trace?: string;
}

export interface ResultFrame {
  readonly type: "result";
  readonly error?: WireError;
  readonly id: number;
  readonly ok: boolean;
  readonly output?: unknown;
}

export interface StreamFrame {
  readonly type: "stream";
  readonly chunk: unknown;
  readonly done: boolean;
  readonly id: number;
  readonly seq: number;
}

export interface CancelFrame {
  readonly type: "cancel";
  readonly id: number;
  readonly reason?: string;
}

export interface ConfigFrame {
  readonly type: "config";
  readonly id: number;
  readonly revision: number;
  readonly values: unknown;
}

export interface PingFrame {
  readonly type: "ping";
  readonly n: number;
}

export interface PongFrame {
  readonly type: "pong";
  readonly n: number;
}

export interface ShutdownFrame {
  readonly type: "shutdown";
  readonly grace_ms: number;
}

/// 本地合成：未知 `type` 被忽略后由解析器产出，不对应线上帧
export interface UnknownFrame {
  readonly type: "unknown";
  readonly kind: string;
}

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

export type OutboundFrame =
  | HelloFrame
  | CallFrame
  | ResultFrame
  | StreamFrame
  | CancelFrame
  | PongFrame
  ;
