import { describe, expect, it } from "vitest";

import { PluginError } from "../src/errors.js";
import {
  decodeBackupCreate,
  decodeBackupList,
  decodeBackupVerify,
  decodeBase64,
  decodeConsolePage,
  decodeContentInstall,
  decodeContentList,
  decodeDeleted,
  decodeFileList,
  decodeFileRead,
  decodeFileWrite,
  decodeJavaInstall,
  decodeJavaList,
  decodeKvGet,
  decodeKvList,
  decodeNetResponse,
  decodeRemoved,
  decodeRemovedCount,
  decodeSecretGet,
  decodeSecretText,
  decodeServerList,
  decodeServerProperties,
  decodeServerStatus,
  decodeServerVersion,
  decodeStopOutcome,
  encodeBase64,
  expectAck,
} from "../src/wire.js";

/** 宿主违约一律是 `internal`，不是 `invalid_input`（§8.4 不降级） */
function expectInternal(fn: () => unknown, pattern?: RegExp): void {
  try {
    fn();
  } catch (error) {
    expect(error).toBeInstanceOf(PluginError);
    const pluginError = error as PluginError;
    expect(pluginError.code).toBe("internal");
    if (pattern !== undefined) {
      expect(pluginError.message).toMatch(pattern);
    }
    return;
  }
  throw new Error("expected the decoder to reject the host response");
}

const utf8 = (bytes: Uint8Array): string => new TextDecoder().decode(bytes);

describe("base64 往返", () => {
  it("字符串按 UTF-8 编码", () => {
    expect(encodeBase64("你好")).toBe(Buffer.from("你好", "utf8").toString("base64"));
    expect(decodeSecretText(encodeBase64("你好"))).toBe("你好");
  });

  it("字节原样往返", () => {
    const bytes = new Uint8Array([0, 1, 2, 250, 255]);
    expect(decodeBase64(encodeBase64(bytes))).toEqual(bytes);
  });
});

describe("确认类结果", () => {
  it("只接受 ok:true", () => {
    expect(() => expectAck({ ok: true }, "storage:kv.set")).not.toThrow();
    expectInternal(() => expectAck({ ok: false }, "storage:kv.set"), /ok:true/);
    expectInternal(() => expectAck(null, "storage:kv.set"));
  });
});

describe("server:read", () => {
  it("list 解码具名 servers 数组", () => {
    expect(
      decodeServerList({
        servers: [
          { name: "survival", status: "running", version: "1.21.4" },
          { name: "creative", status: "stopped" },
        ],
      }),
    ).toEqual([
      { name: "survival", status: "running", version: "1.21.4" },
      { name: "creative", status: "stopped", version: null },
    ]);
  });

  it("status 保留 null 语义", () => {
    expect(
      decodeServerStatus({ status: "stopped", version: null, players: null, uptime_secs: null }),
    ).toEqual({ status: "stopped", version: null, players: null, uptimeSecs: null });
    expect(
      decodeServerStatus({ status: "running", version: "1.21", players: 3, uptime_secs: 42 }),
    ).toEqual({ status: "running", version: "1.21", players: 3, uptimeSecs: 42 });
  });

  it("version 与 properties", () => {
    expect(decodeServerVersion({ version: "1.21.4" })).toBe("1.21.4");
    expect(decodeServerVersion({ version: null })).toBeNull();
    expect(decodeServerProperties({ properties: { motd: "hi", "max-players": "20" } })).toEqual({
      motd: "hi",
      "max-players": "20",
    });
  });

  it("拒绝缺少具名字段或类型错误", () => {
    expectInternal(() => decodeServerList({ servers: {} }), /servers/);
    expectInternal(() => decodeServerList({ servers: [{ name: 1 }] }), /name/);
    expectInternal(() => decodeServerStatus({ status: 1 }), /status/);
    expectInternal(() => decodeServerVersion({ version: 3 }), /version/);
    expectInternal(() => decodeServerProperties({ properties: { motd: 1 } }), /motd/);
  });
});

describe("console:read", () => {
  it("tail 解码 lines/next", () => {
    expect(
      decodeConsolePage({ lines: [{ cursor: 7, text: "[INFO] hi" }], next: 7 }),
    ).toEqual({ lines: [{ cursor: 7, text: "[INFO] hi" }], next: 7 });
  });

  it("拒绝旧的裸数组 / 缺少 next", () => {
    expectInternal(() => decodeConsolePage([{ cursor: 1, text: "x" }]));
    expectInternal(() => decodeConsolePage({ lines: [] }), /next/);
    expectInternal(() => decodeConsolePage({ lines: [{ cursor: "1", text: "x" }], next: 0 }), /cursor/);
  });
});

describe("storage:kv / secret:rw", () => {
  it("get 返回字节，null 表示不存在", () => {
    expect(utf8(decodeKvGet({ value: encodeBase64("hello") })!)).toBe("hello");
    expect(decodeKvGet({ value: null })).toBeNull();
  });

  it("list / delete", () => {
    expect(decodeKvList({ keys: ["a", "b"] })).toEqual(["a", "b"]);
    expect(decodeDeleted({ deleted: true }, "storage:kv.delete")).toBe(true);
    expect(decodeDeleted({ deleted: false }, "storage:kv.delete")).toBe(false);
  });

  it("secret 值是 base64 文本", () => {
    const encoded = encodeBase64("超级密钥");
    expect(decodeSecretGet({ value: encoded })).toBe("超级密钥");
    expect(decodeSecretGet({ value: null })).toBeNull();
  });

  it("拒绝类型错误", () => {
    expectInternal(() => decodeKvGet({ value: 5 }), /value/);
    expectInternal(() => decodeKvList({ keys: [1] }), /keys/);
    expectInternal(() => decodeDeleted({ deleted: "yes" }, "storage:kv.delete"), /deleted/);
    expectInternal(() => decodeSecretGet({ value: 5 }), /value/);
  });
});

describe("net:outbound", () => {
  it("解码 body 为字节", () => {
    const response = decodeNetResponse({
      status: 201,
      headers: { "content-type": "text/plain" },
      body: encodeBase64("created"),
      body_len: 7,
    });
    expect(response.status).toBe(201);
    expect(response.headers).toEqual({ "content-type": "text/plain" });
    expect(utf8(response.body)).toBe("created");
    expect(response.bodyLen).toBe(7);
  });

  it("拒绝缺少 body_len", () => {
    expectInternal(
      () => decodeNetResponse({ status: 200, headers: {}, body: "" }),
      /body_len/,
    );
  });
});

describe("files:read / files:write", () => {
  it("read / list", () => {
    const read = decodeFileRead({ path: "server.properties", size: 3, data: encodeBase64("abc") });
    expect(read.path).toBe("server.properties");
    expect(read.size).toBe(3);
    expect(utf8(read.data)).toBe("abc");
    expect(
      decodeFileList({ entries: [{ path: "mods", is_dir: true, size: 0 }] }),
    ).toEqual([{ path: "mods", isDir: true, size: 0 }]);
  });

  it("write / remove", () => {
    expect(decodeFileWrite({ ok: true, size: 5 })).toEqual({ ok: true, size: 5 });
    expect(decodeRemoved({ removed: true }, "files:write.remove")).toBe(true);
    expect(decodeRemovedCount({ removed: 2 }, "backup.prune")).toBe(2);
  });

  it("拒绝 ok:false 与错误类型", () => {
    expectInternal(() => decodeFileWrite({ ok: false, size: 0 }), /ok:true/);
    expectInternal(() => decodeFileRead({ path: "a", size: 1, data: 5 }), /data/);
    expectInternal(
      () => decodeFileList({ entries: [{ path: "a", is_dir: 1, size: 0 }] }),
      /is_dir/,
    );
    expectInternal(() => decodeRemoved({}, "files:write.remove"), /removed/);
    expectInternal(() => decodeRemovedCount({ removed: -1 }, "backup.prune"), /removed/);
  });
});

describe("server:lifecycle.stop", () => {
  it("三种结果分别解码", () => {
    expect(decodeStopOutcome({ outcome: "stopped", elapsed_ms: 120 })).toEqual({
      outcome: "stopped",
      elapsedMs: 120,
    });
    expect(decodeStopOutcome({ outcome: "already_stopped" })).toEqual({ outcome: "already_stopped" });
    expect(decodeStopOutcome({ outcome: "timed_out", waited_ms: 30_000 })).toEqual({
      outcome: "timed_out",
      waitedMs: 30_000,
    });
  });

  it("未知 outcome 视为宿主违约", () => {
    expectInternal(() => decodeStopOutcome({ outcome: "exploded" }), /outcome/);
    expectInternal(() => decodeStopOutcome({ outcome: "stopped" }), /elapsed_ms/);
  });
});

describe("backup", () => {
  it("create / list / verify", () => {
    expect(
      decodeBackupCreate({ snapshot: { id: "s1", created_at: 1_700_000_000, added_bytes: 10 } }),
    ).toEqual({ id: "s1", createdAt: 1_700_000_000, addedBytes: 10 });
    expect(
      decodeBackupList({
        snapshots: [
          { id: "s1", created_at: 1, added_bytes: 0 },
          { id: "s2", created_at: 2, added_bytes: 9 },
        ],
      }),
    ).toEqual([
      { id: "s1", createdAt: 1, addedBytes: 0 },
      { id: "s2", createdAt: 2, addedBytes: 9 },
    ]);
    expect(decodeBackupVerify({ healthy: true })).toBe(true);
  });

  it("拒绝缺少 snapshot / 秒级时间戳之外的形状", () => {
    expectInternal(() => decodeBackupCreate({}), /snapshot/);
    expectInternal(
      () => decodeBackupList({ snapshots: [{ id: "s", created_at: "now", added_bytes: 0 }] }),
      /created_at/,
    );
    expectInternal(() => decodeBackupVerify({ healthy: "yes" }), /healthy/);
  });
});

describe("content", () => {
  it("list 透传可选 identity", () => {
    expect(
      decodeContentList({
        items: [
          { file: "a.jar", size: 1 },
          { file: "b.jar", size: 2, identity: { project: "p" } },
        ],
      }),
    ).toEqual([
      { file: "a.jar", size: 1 },
      { file: "b.jar", size: 2, identity: { project: "p" } },
    ]);
  });

  it("install 的 version 可空", () => {
    expect(
      decodeContentInstall({ file: "a.jar", size: 1, project: "p", version: "1.0" }),
    ).toEqual({ file: "a.jar", size: 1, project: "p", version: "1.0" });
    expect(
      decodeContentInstall({ file: "a.jar", size: 1, project: "p", version: null }),
    ).toEqual({ file: "a.jar", size: 1, project: "p", version: null });
  });

  it("拒绝非数组 items 与数字 version", () => {
    expectInternal(() => decodeContentList({ items: {} }), /items/);
    expectInternal(
      () => decodeContentInstall({ file: "a", size: 1, project: "p", version: 5 }),
      /version/,
    );
  });
});

describe("java", () => {
  const runtime = {
    id: "jdk-21",
    path: "/opt/java/21",
    major: 21,
    version: "21.0.2",
    vendor: "Temurin",
    managed: true,
    created_at: 1_700_000_000,
    updated_at: 1_700_000_100,
  };

  it("list / install 解码 Unix 秒", () => {
    expect(decodeJavaList({ runtimes: [runtime] })).toEqual([
      {
        id: "jdk-21",
        path: "/opt/java/21",
        major: 21,
        version: "21.0.2",
        vendor: "Temurin",
        managed: true,
        createdAt: 1_700_000_000,
        updatedAt: 1_700_000_100,
      },
    ]);
    expect(decodeJavaInstall({ runtime })).toMatchObject({ id: "jdk-21", createdAt: 1_700_000_000 });
  });

  it("拒绝缺少 managed / 时间戳", () => {
    expectInternal(() => decodeJavaList({ runtimes: [{ ...runtime, managed: "yes" }] }), /managed/);
    expectInternal(() => decodeJavaInstall({ runtime: { ...runtime, updated_at: "now" } }), /updated_at/);
  });
});
