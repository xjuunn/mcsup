import { describe, expect, it } from "vitest";

import { action, definePlugin, z } from "../src/index.js";
import { createMockHost } from "../src/testing.js";

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

const expectedRuntime = {
  id: "jdk-21",
  path: "/opt/java/21",
  major: 21,
  version: "21.0.2",
  vendor: "Temurin",
  managed: true,
  createdAt: 1_700_000_000,
  updatedAt: 1_700_000_100,
};

const text = (bytes: Uint8Array): string => new TextDecoder().decode(bytes);

describe("能力客户端按宿主线上形状解码", () => {
  it("全部能力组走一遍", async () => {
    const calls: Array<{ capability: string; method: string; input: unknown }> = [];
    const handle = ({
      capability,
      method,
      input,
    }: {
      capability: string;
      method: string;
      input: unknown;
    }): unknown => {
      calls.push({ capability, method, input });
      const key = `${capability}.${method}`;
      switch (key) {
        case "server:read.list":
          return { servers: [{ name: "survival", status: "running", version: "1.21.4" }] };
        case "server:read.status":
          return { status: "running", version: "1.21.4", players: 2, uptime_secs: 90 };
        case "server:read.version":
          return { version: "1.21.4" };
        case "server:read.properties":
          return { properties: { motd: "hi" } };
        case "console:read.tail":
          return { lines: [{ cursor: 4, text: "[INFO] ready" }], next: 4 };
        case "console:write.send_line":
          return { ok: true };
        case "storage:kv.get":
          return { value: Buffer.from("kv").toString("base64") };
        case "storage:kv.set":
          return { ok: true };
        case "storage:kv.delete":
          return { deleted: true };
        case "storage:kv.list":
          return { keys: ["a", "b"] };
        case "secret:rw.get":
          return { value: Buffer.from("pw").toString("base64") };
        case "secret:rw.set":
          return { ok: true };
        case "secret:rw.delete":
          return { deleted: false };
        case "net:outbound.request":
          return {
            status: 200,
            headers: { "content-type": "text/plain" },
            body: Buffer.from("pong").toString("base64"),
            body_len: 4,
          };
        case "files:read.read":
          return { path: "a.txt", size: 2, data: Buffer.from("hi").toString("base64") };
        case "files:read.list":
          return { entries: [{ path: "mods/a.jar", is_dir: false, size: 3 }] };
        case "files:write.write":
          return { ok: true, size: 2 };
        case "files:write.remove":
          return { removed: true };
        case "server:lifecycle.start":
        case "server:lifecycle.restart":
        case "backup.restore":
          return { ok: true };
        case "server:lifecycle.stop":
          return { outcome: "stopped", elapsed_ms: 15 };
        case "backup.create":
          return { snapshot: { id: "s1", created_at: 10, added_bytes: 20 } };
        case "backup.list":
          return { snapshots: [{ id: "s2", created_at: 11, added_bytes: 0 }] };
        case "backup.verify":
          return { healthy: true };
        case "backup.prune":
          return { removed: 2 };
        case "backup.delete":
          return { deleted: true };
        case "content.list":
          return { items: [{ file: "a.jar", size: 1 }] };
        case "content.install":
          return { file: "a.jar", size: 1, project: "p", version: "1.2" };
        case "content.remove":
          return { removed: 1 };
        case "java.list":
          return { runtimes: [runtime] };
        case "java.install":
          return { runtime };
        case "java.remove":
          return { removed: true };
        default:
          throw new Error(`unexpected ${key}`);
      }
    };

    const plugin = definePlugin({
      id: "probe",
      title: "能力探针",
      version: "1.0.0",
      license: "MIT",
      capabilities: [
        "server:read",
        "console:read",
        "console:write",
        "storage:kv",
        "secret:rw",
        "net:outbound",
        "files:read",
        "files:write",
        "server:lifecycle",
        "backup",
        "content",
        "java",
      ],
      config: z.object({}),
      actions: {
        probe: action("probe")
          .output(z.unknown())
          .run(async (_input, ctx) => {
            const servers = await ctx["server:read"].servers.list();
            const status = await ctx["server:read"].servers.status("survival");
            const version = await ctx["server:read"].servers.version("survival");
            const properties = await ctx["server:read"].servers.properties("survival");

            const page = await ctx["console:read"].tail("survival", 3, 10);
            await ctx["console:write"].sendLine("survival", "say hi");

            const kvValue = await ctx["storage:kv"].get("k");
            await ctx["storage:kv"].set("k", "v");
            const kvDeleted = await ctx["storage:kv"].delete("k");
            const keys = await ctx["storage:kv"].list("p");

            const secret = await ctx["secret:rw"].get("token");
            await ctx["secret:rw"].set("token", "next");
            const secretDeleted = await ctx["secret:rw"].delete("token");

            const response = await ctx["net:outbound"].request({
              method: "POST",
              url: "https://example.test/ping",
              headers: { "x-a": "b" },
              body: "payload",
              timeoutMs: 1_500,
            });

            const file = await ctx["files:read"].read("survival", "a.txt");
            const entries = await ctx["files:read"].list("survival", "mods");
            const wrote = await ctx["files:write"].write("survival", "a.txt", "hi", true);
            const removedFile = await ctx["files:write"].remove("survival", "a.txt");

            await ctx["server:lifecycle"].start("survival");
            const stopped = await ctx["server:lifecycle"].stop("survival", 30);
            await ctx["server:lifecycle"].restart("survival", 30);

            const snapshot = await ctx["backup"].create("survival", "before");
            const snapshots = await ctx["backup"].list("survival");
            await ctx["backup"].restore("survival", "s1");
            const healthy = await ctx["backup"].verify("survival", "s1");
            const pruned = await ctx["backup"].prune("survival", 3);
            const deletedSnapshot = await ctx["backup"].delete("survival", "s1");

            const items = await ctx["content"].list("survival", { kind: "mod", identify: true });
            const installed = await ctx["content"].install("survival", "p", {
              version: "1.2",
              kind: "mod",
            });
            const removedContent = await ctx["content"].remove("survival", "a.jar", "mod");

            const runtimes = await ctx["java"].list();
            const installedJava = await ctx["java"].install(21);
            const removedJava = await ctx["java"].remove("/opt/java/21");

            return {
              servers,
              status,
              version,
              properties,
              page,
              kvValue: kvValue === null ? null : text(kvValue),
              kvDeleted,
              keys,
              secret,
              secretDeleted,
              response: {
                status: response.status,
                headers: response.headers,
                body: text(response.body),
                bodyLen: response.bodyLen,
              },
              file: { path: file.path, size: file.size, data: text(file.data) },
              entries,
              wrote,
              removedFile,
              stopped,
              snapshot,
              snapshots,
              healthy,
              pruned,
              deletedSnapshot,
              items,
              installed,
              removedContent,
              runtimes,
              installedJava,
              removedJava,
            };
          }),
      },
    });

    const host = createMockHost(plugin, { handle });
    await host.start();
    const result = await host.callOp("probe", {});
    expect(result.ok).toBe(true);
    if (!result.ok) {
      return;
    }
    expect(result.output).toEqual({
      servers: [{ name: "survival", status: "running", version: "1.21.4" }],
      status: { status: "running", version: "1.21.4", players: 2, uptimeSecs: 90 },
      version: "1.21.4",
      properties: { motd: "hi" },
      page: { lines: [{ cursor: 4, text: "[INFO] ready" }], next: 4 },
      kvValue: "kv",
      kvDeleted: true,
      keys: ["a", "b"],
      secret: "pw",
      secretDeleted: false,
      response: {
        status: 200,
        headers: { "content-type": "text/plain" },
        body: "pong",
        bodyLen: 4,
      },
      file: { path: "a.txt", size: 2, data: "hi" },
      entries: [{ path: "mods/a.jar", isDir: false, size: 3 }],
      wrote: { ok: true, size: 2 },
      removedFile: true,
      stopped: { outcome: "stopped", elapsedMs: 15 },
      snapshot: { id: "s1", createdAt: 10, addedBytes: 20 },
      snapshots: [{ id: "s2", createdAt: 11, addedBytes: 0 }],
      healthy: true,
      pruned: 2,
      deletedSnapshot: true,
      items: [{ file: "a.jar", size: 1 }],
      installed: { file: "a.jar", size: 1, project: "p", version: "1.2" },
      removedContent: 1,
      runtimes: [expectedRuntime],
      installedJava: expectedRuntime,
      removedJava: true,
    });

    // 写入方向必须按线上约定编码：字符串 UTF-8 → base64
    expect(calls).toContainEqual({
      capability: "console:read",
      method: "tail",
      input: { server: "survival", cursor: 3, max_lines: 10 },
    });
    expect(calls).toContainEqual({
      capability: "storage:kv",
      method: "set",
      input: { key: "k", value: Buffer.from("v").toString("base64") },
    });
    expect(calls).toContainEqual({
      capability: "secret:rw",
      method: "set",
      input: { key: "token", value: Buffer.from("next").toString("base64") },
    });
    expect(calls).toContainEqual({
      capability: "net:outbound",
      method: "request",
      input: {
        method: "POST",
        url: "https://example.test/ping",
        headers: { "x-a": "b" },
        body: Buffer.from("payload").toString("base64"),
        timeout_ms: 1_500,
      },
    });
    expect(calls).toContainEqual({
      capability: "files:write",
      method: "write",
      input: {
        server: "survival",
        path: "a.txt",
        data: Buffer.from("hi").toString("base64"),
        overwrite: true,
      },
    });
    expect(calls).toContainEqual({
      capability: "content",
      method: "install",
      input: { server: "survival", project: "p", version: "1.2", kind: "mod" },
    });
    await host.shutdown();
  });

  it("宿主返回旧裸值（如 list 直接回数组）视为违约", async () => {
    const plugin = definePlugin({
      id: "legacy-shape",
      title: "旧形状",
      version: "1.0.0",
      license: "MIT",
      capabilities: ["server:read"],
      config: z.object({}),
      actions: {
        list: action("list")
          .output(z.unknown())
          .run(async (_input, ctx) => await ctx["server:read"].servers.list()),
      },
    });
    const host = createMockHost(plugin, {
      handle: () => [{ name: "survival", status: "running" }],
    });
    await host.start();
    const result = await host.callOp("list", {});
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.code).toBe("internal");
      expect(result.error.message).toMatch(/server:read\.list/);
    }
    await host.shutdown();
  });
});
