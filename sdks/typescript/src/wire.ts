/**
 * 宿主能力调用的线上形状（plugins.md §9.1）
 *
 * 宿主返回的字段一律 snake_case，且很多方法把结果包在具名字段里
 * （`{"servers": [...]}`、`{"deleted": true}`）。SDK 在这一层做严格解码：
 * 字段缺失或类型不符视为宿主违约，抛 `internal`，绝不把 `{}` / `true`
 * 当成有效结果继续往下传（§8.4 不降级）。
 */

import { PluginError } from "./errors.js";
import type {
  ConsoleLineEntry,
  ConsolePage,
  ContentIdentity,
  ContentItem,
  ContentKind,
  FileEntry,
  JavaRuntime,
  NetResponse,
  ServerStatusView,
  ServerSummary,
  SnapshotView,
  StopOutcome,
} from "./ctx.js";

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

/** 字符串或字节按 UTF-8 编码后 base64（线上约定） */
export function encodeBase64(value: string | Uint8Array): string {
  const bytes = typeof value === "string" ? textEncoder.encode(value) : value;
  return Buffer.from(bytes).toString("base64");
}

/** base64 → 字节；用于文件 / 网络响应体 */
export function decodeBase64(value: string): Uint8Array {
  return new Uint8Array(Buffer.from(value, "base64"));
}

/** base64 → UTF-8 文本；用于 `secret:rw` 的取值 */
export function decodeSecretText(value: string): string {
  return textDecoder.decode(decodeBase64(value));
}

function invalid(what: string, detail: string): never {
  throw PluginError.internal(`host returned an invalid ${what}: ${detail}`);
}

function asObject(value: unknown, what: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    invalid(what, "expected an object");
  }
  return value as Record<string, unknown>;
}

function asArray(value: unknown, what: string): readonly unknown[] {
  if (!Array.isArray(value)) {
    invalid(what, "expected an array");
  }
  return value;
}

function asString(value: unknown, what: string): string {
  if (typeof value !== "string") {
    invalid(what, "expected a string");
  }
  return value;
}

function asNumber(value: unknown, what: string): number {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
    invalid(what, "expected a non-negative number");
  }
  return value;
}

function asBoolean(value: unknown, what: string): boolean {
  if (typeof value !== "boolean") {
    invalid(what, "expected a boolean");
  }
  return value;
}

/** 可空字符串：`null` / 缺省都归一为 `null` */
function asNullableString(value: unknown, what: string): string | null {
  return value === null || value === undefined ? null : asString(value, what);
}

/** 可空数字：`null` / 缺省都归一为 `null` */
function asNullableNumber(value: unknown, what: string): number | null {
  return value === null || value === undefined ? null : asNumber(value, what);
}

/** 具名结果对象里的一个字段 */
function field(record: Record<string, unknown>, key: string, what: string): unknown {
  return record[key];
}

function stringRecord(value: unknown, what: string): Record<string, string> {
  const record = asObject(value, what);
  const out: Record<string, string> = {};
  for (const [key, item] of Object.entries(record)) {
    out[key] = asString(item, `${what}.${key}`);
  }
  return out;
}

// ---------------------------------------------------------------------------
// 确认类结果
// ---------------------------------------------------------------------------

/** 宿主确认帧（`{ok:true}`）；`ok` 不是 `true` 即视为违约 */
export function expectAck(value: unknown, what: string): void {
  const record = asObject(value, what);
  if (field(record, "ok", what) !== true) {
    invalid(what, "expected ok:true");
  }
}

// ---------------------------------------------------------------------------
// server:read
// ---------------------------------------------------------------------------

/** `server:read.list` → `{servers:[{name,status,version}]}` */
export function decodeServerList(value: unknown): readonly ServerSummary[] {
  const what = "server:read.list";
  const record = asObject(value, what);
  return asArray(field(record, "servers", what), `${what}.servers`).map((item, index) => {
    const entry = asObject(item, `${what}.servers[${index}]`);
    const where = `${what}.servers[${index}]`;
    return {
      name: asString(field(entry, "name", where), `${where}.name`),
      status: asString(field(entry, "status", where), `${where}.status`),
      version: asNullableString(field(entry, "version", where), `${where}.version`),
    };
  });
}

/** `server:read.status` → `{status,version,players,uptime_secs}` */
export function decodeServerStatus(value: unknown): ServerStatusView {
  const what = "server:read.status";
  const record = asObject(value, what);
  return {
    status: asString(field(record, "status", what), `${what}.status`),
    version: asNullableString(field(record, "version", what), `${what}.version`),
    players: asNullableNumber(field(record, "players", what), `${what}.players`),
    uptimeSecs: asNullableNumber(field(record, "uptime_secs", what), `${what}.uptime_secs`),
  };
}

/** `server:read.version` → `{version}` */
export function decodeServerVersion(value: unknown): string | null {
  const what = "server:read.version";
  const record = asObject(value, what);
  return asNullableString(field(record, "version", what), `${what}.version`);
}

/** `server:read.properties` → `{properties}` */
export function decodeServerProperties(value: unknown): Readonly<Record<string, string>> {
  const what = "server:read.properties";
  const record = asObject(value, what);
  return stringRecord(field(record, "properties", what), `${what}.properties`);
}

// ---------------------------------------------------------------------------
// console:read
// ---------------------------------------------------------------------------

/** `console:read.tail` → `{lines:[{cursor,text}],next}` */
export function decodeConsolePage(value: unknown): ConsolePage {
  const what = "console:read.tail";
  const record = asObject(value, what);
  const lines: ConsoleLineEntry[] = asArray(field(record, "lines", what), `${what}.lines`).map(
    (item, index) => {
      const where = `${what}.lines[${index}]`;
      const entry = asObject(item, where);
      return {
        cursor: asNumber(field(entry, "cursor", where), `${where}.cursor`),
        text: asString(field(entry, "text", where), `${where}.text`),
      };
    },
  );
  return { lines, next: asNumber(field(record, "next", what), `${what}.next`) };
}

// ---------------------------------------------------------------------------
// storage:kv / secret:rw
// ---------------------------------------------------------------------------

/** `storage:kv.get` → `{value: base64|null}` */
export function decodeKvGet(value: unknown): Uint8Array | null {
  const what = "storage:kv.get";
  const record = asObject(value, what);
  const raw = asNullableString(field(record, "value", what), `${what}.value`);
  return raw === null ? null : decodeBase64(raw);
}

/** `storage:kv.list` → `{keys}` */
export function decodeKvList(value: unknown): readonly string[] {
  const what = "storage:kv.list";
  const record = asObject(value, what);
  return asArray(field(record, "keys", what), `${what}.keys`).map((item, index) =>
    asString(item, `${what}.keys[${index}]`),
  );
}

/** `storage:kv.delete` / `secret:rw.delete` → `{deleted}` */
export function decodeDeleted(value: unknown, what: string): boolean {
  const record = asObject(value, what);
  return asBoolean(field(record, "deleted", what), `${what}.deleted`);
}

/** `secret:rw.get` → `{value: base64|null}`，解码为 UTF-8 文本 */
export function decodeSecretGet(value: unknown): string | null {
  const what = "secret:rw.get";
  const record = asObject(value, what);
  const raw = asNullableString(field(record, "value", what), `${what}.value`);
  return raw === null ? null : decodeSecretText(raw);
}

// ---------------------------------------------------------------------------
// net:outbound
// ---------------------------------------------------------------------------

/** `net:outbound.request` → `{status,headers,body,body_len}` */
export function decodeNetResponse(value: unknown): NetResponse {
  const what = "net:outbound.request";
  const record = asObject(value, what);
  const body = decodeBase64(asString(field(record, "body", what), `${what}.body`));
  return {
    status: asNumber(field(record, "status", what), `${what}.status`),
    headers: stringRecord(field(record, "headers", what), `${what}.headers`),
    body,
    bodyLen: asNumber(field(record, "body_len", what), `${what}.body_len`),
  };
}

// ---------------------------------------------------------------------------
// files:read / files:write
// ---------------------------------------------------------------------------

/** `files:read.read` → `{path,size,data}` */
export function decodeFileRead(value: unknown): { readonly path: string; readonly size: number; readonly data: Uint8Array } {
  const what = "files:read.read";
  const record = asObject(value, what);
  return {
    path: asString(field(record, "path", what), `${what}.path`),
    size: asNumber(field(record, "size", what), `${what}.size`),
    data: decodeBase64(asString(field(record, "data", what), `${what}.data`)),
  };
}

/** `files:read.list` → `{entries:[{path,is_dir,size}]}` */
export function decodeFileList(value: unknown): readonly FileEntry[] {
  const what = "files:read.list";
  const record = asObject(value, what);
  return asArray(field(record, "entries", what), `${what}.entries`).map((item, index) => {
    const where = `${what}.entries[${index}]`;
    const entry = asObject(item, where);
    return {
      path: asString(field(entry, "path", where), `${where}.path`),
      isDir: asBoolean(field(entry, "is_dir", where), `${where}.is_dir`),
      size: asNumber(field(entry, "size", where), `${where}.size`),
    };
  });
}

/** `files:write.write` → `{ok:true,size}` */
export function decodeFileWrite(value: unknown): { readonly ok: true; readonly size: number } {
  const what = "files:write.write";
  const record = asObject(value, what);
  if (field(record, "ok", what) !== true) {
    invalid(what, "expected ok:true");
  }
  return { ok: true, size: asNumber(field(record, "size", what), `${what}.size`) };
}

/** `files:write.remove` → `{removed}` */
export function decodeRemoved(value: unknown, what: string): boolean {
  const record = asObject(value, what);
  return asBoolean(field(record, "removed", what), `${what}.removed`);
}

/** 批量移除结果 → `{removed: 数量}`（backup.prune / content.remove） */
export function decodeRemovedCount(value: unknown, what: string): number {
  const record = asObject(value, what);
  return asNumber(field(record, "removed", what), `${what}.removed`);
}

// ---------------------------------------------------------------------------
// server:lifecycle
// ---------------------------------------------------------------------------

/** `server:lifecycle.stop` → `{outcome,elapsed_ms|waited_ms}` */
export function decodeStopOutcome(value: unknown): StopOutcome {
  const what = "server:lifecycle.stop";
  const record = asObject(value, what);
  const outcome = asString(field(record, "outcome", what), `${what}.outcome`);
  switch (outcome) {
    case "stopped":
      return {
        outcome: "stopped",
        elapsedMs: asNumber(field(record, "elapsed_ms", what), `${what}.elapsed_ms`),
      };
    case "already_stopped":
      return { outcome: "already_stopped" };
    case "timed_out":
      return {
        outcome: "timed_out",
        waitedMs: asNumber(field(record, "waited_ms", what), `${what}.waited_ms`),
      };
    default:
      return invalid(what, `unknown outcome ${outcome}`);
  }
}

// ---------------------------------------------------------------------------
// backup
// ---------------------------------------------------------------------------

function decodeSnapshot(value: unknown, what: string): SnapshotView {
  const record = asObject(value, what);
  return {
    id: asString(field(record, "id", what), `${what}.id`),
    createdAt: asNumber(field(record, "created_at", what), `${what}.created_at`),
    addedBytes: asNumber(field(record, "added_bytes", what), `${what}.added_bytes`),
  };
}

/** `backup.create` → `{snapshot}` */
export function decodeBackupCreate(value: unknown): SnapshotView {
  const what = "backup.create";
  const record = asObject(value, what);
  return decodeSnapshot(field(record, "snapshot", what), `${what}.snapshot`);
}

/** `backup.list` → `{snapshots}` */
export function decodeBackupList(value: unknown): readonly SnapshotView[] {
  const what = "backup.list";
  const record = asObject(value, what);
  return asArray(field(record, "snapshots", what), `${what}.snapshots`).map((item, index) =>
    decodeSnapshot(item, `${what}.snapshots[${index}]`),
  );
}

/** `backup.verify` → `{healthy}` */
export function decodeBackupVerify(value: unknown): boolean {
  const what = "backup.verify";
  const record = asObject(value, what);
  return asBoolean(field(record, "healthy", what), `${what}.healthy`);
}

// ---------------------------------------------------------------------------
// content
// ---------------------------------------------------------------------------

/** `content.list` → `{items:[{file,size,identity?}]}` */
export function decodeContentList(value: unknown): readonly ContentItem[] {
  const what = "content.list";
  const record = asObject(value, what);
  return asArray(field(record, "items", what), `${what}.items`).map((item, index) => {
    const where = `${what}.items[${index}]`;
    const entry = asObject(item, where);
    const decoded: ContentItem = {
      file: asString(field(entry, "file", where), `${where}.file`),
      size: asNumber(field(entry, "size", where), `${where}.size`),
    };
    const identity = field(entry, "identity", where);
    return identity === undefined ? decoded : { ...decoded, identity: decodeContentIdentity(identity, `${where}.identity`) };
  });
}

const CONTENT_KINDS: readonly ContentKind[] = ["mod", "plugin", "resourcepack", "datapack", "shader", "modpack"];

/** `content.list[].identity`：宿主按哈希反查的结果，字段缺失以 null 表示 */
function decodeContentIdentity(value: unknown, what: string): ContentIdentity {
  const record = asObject(value, what);
  const kind = asNullableString(field(record, "kind", what), `${what}.kind`);
  if (kind !== null && !(CONTENT_KINDS as readonly string[]).includes(kind)) {
    invalid(what, `.kind is not a known content kind: ${kind}`);
  }
  return {
    hash: asString(field(record, "hash", what), `${what}.hash`),
    project_id: asString(field(record, "project_id", what), `${what}.project_id`),
    slug: asNullableString(field(record, "slug", what), `${what}.slug`),
    title: asNullableString(field(record, "title", what), `${what}.title`),
    version_id: asString(field(record, "version_id", what), `${what}.version_id`),
    version_number: asString(field(record, "version_number", what), `${what}.version_number`),
    channel: asString(field(record, "channel", what), `${what}.channel`),
    date_published: asString(field(record, "date_published", what), `${what}.date_published`),
    kind: kind as ContentKind | null,
  };
}

/** `content.install` → `{file,size,project,version}` */
export function decodeContentInstall(value: unknown): {
  readonly file: string;
  readonly size: number;
  readonly project: string;
  readonly version: string | null;
} {
  const what = "content.install";
  const record = asObject(value, what);
  return {
    file: asString(field(record, "file", what), `${what}.file`),
    size: asNumber(field(record, "size", what), `${what}.size`),
    project: asString(field(record, "project", what), `${what}.project`),
    version: asNullableString(field(record, "version", what), `${what}.version`),
  };
}

// ---------------------------------------------------------------------------
// java
// ---------------------------------------------------------------------------

function decodeJavaRuntime(value: unknown, what: string): JavaRuntime {
  const record = asObject(value, what);
  return {
    id: asString(field(record, "id", what), `${what}.id`),
    path: asString(field(record, "path", what), `${what}.path`),
    major: asNumber(field(record, "major", what), `${what}.major`),
    version: asString(field(record, "version", what), `${what}.version`),
    vendor: asString(field(record, "vendor", what), `${what}.vendor`),
    managed: asBoolean(field(record, "managed", what), `${what}.managed`),
    createdAt: asNumber(field(record, "created_at", what), `${what}.created_at`),
    updatedAt: asNumber(field(record, "updated_at", what), `${what}.updated_at`),
  };
}

/** `java.list` → `{runtimes}` */
export function decodeJavaList(value: unknown): readonly JavaRuntime[] {
  const what = "java.list";
  const record = asObject(value, what);
  return asArray(field(record, "runtimes", what), `${what}.runtimes`).map((item, index) =>
    decodeJavaRuntime(item, `${what}.runtimes[${index}]`),
  );
}

/** `java.install` → `{runtime}` */
export function decodeJavaInstall(value: unknown): JavaRuntime {
  const what = "java.install";
  const record = asObject(value, what);
  return decodeJavaRuntime(field(record, "runtime", what), `${what}.runtime`);
}
