import { spawn } from "node:child_process";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { describe, expect, it } from "vitest";

const ENTRY = path.resolve("test/process-entry.mjs");

describe("真实进程", () => {
  it("dump descriptor 不需要端点", async () => {
    const out = await new Promise<string>((resolve, reject) => {
      const child = spawn(process.execPath, [ENTRY, "--mcsup-dump-descriptor"], { stdio: ["ignore", "pipe", "pipe"] });
      let text = "";
      child.stdout.on("data", (chunk: Buffer) => {
        text += chunk.toString("utf8");
      });
      child.on("exit", () => {
        resolve(text);
      });
      child.on("error", reject);
    });
    const descriptor = JSON.parse(out) as { plugin_id: string; schema: number };
    expect(descriptor.plugin_id).toBe("fixture");
    expect(descriptor.schema).toBe(1);
    expect(out.endsWith("\n")).toBe(true);
  });

  it("握手 → 调用 → 关闭，退出码 0", async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "mcsup-plugin-test-"));
    // Windows 上 Unix socket 路径不可监听（EACCES）；node 的 net 模块原生支持
    // 命名管道（`\\.\pipe\name`），SDK 的 `connect` 走同一条 `net.createConnection`。
    const socketPath =
      process.platform === "win32"
        ? `\\\\.\\pipe\\mcsup-plugin-test-${process.pid}-${Math.random().toString(36).slice(2)}`
        : path.join(dir, "host.sock");
    const frames: Record<string, unknown>[] = [];
    const server = net.createServer();
    await new Promise<void>((resolve) => {
      server.listen(socketPath, resolve);
    });
    const accepted = new Promise<net.Socket>((resolve) => {
      server.once("connection", resolve);
    });
    const child = spawn(process.execPath, [ENTRY, "--endpoint", socketPath], {
      env: { ...process.env, MCSUP_PLUGIN_TOKEN: "b".repeat(64) },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const socket = await accepted;
    let buffer = "";
    socket.on("data", (chunk: Buffer) => {
      buffer += chunk.toString("utf8");
      for (;;) {
        const index = buffer.indexOf("\n");
        if (index < 0) {
          return;
        }
        const line = buffer.slice(0, index);
        buffer = buffer.slice(index + 1);
        if (line.length > 0) {
          frames.push(JSON.parse(line) as Record<string, unknown>);
        }
      }
    });
    const waitFrame = async (predicate: (f: Record<string, unknown>) => boolean): Promise<Record<string, unknown>> => {
      for (let i = 0; i < 400; i += 1) {
        const found = frames.find(predicate);
        if (found !== undefined) {
          return found;
        }
        await new Promise((r) => setTimeout(r, 10));
      }
      throw new Error("frame not observed");
    };

    const hello = await waitFrame((f) => f["type"] === "hello");
    expect(hello["plugin_id"]).toBe("fixture");

    socket.write(
      `${JSON.stringify({
        v: 1,
        type: "welcome",
        session_id: "s-1",
        host_version: "0.0.0",
        granted: ["server:read"],
        limits: { inflight: 8, frame_bytes: 25165824, event_queue: 1024, log_lines_per_sec: 200 },
        config: { revision: 1, values: {} },
        scope: { kind: "machine" },
      })}\n`,
    );
    await new Promise((r) => setTimeout(r, 50));

    socket.write(`${JSON.stringify({ v: 1, type: "call", id: 42, op: "status", input: {} })}\n`);
    const result = await waitFrame((f) => f["type"] === "result" && f["id"] === 42);
    expect(result["ok"]).toBe(true);

    const exitPromise = new Promise<number>((resolve) => {
      child.on("exit", (code) => {
        resolve(code ?? -1);
      });
    });
    socket.write(`${JSON.stringify({ v: 1, type: "shutdown", grace_ms: 1000 })}\n`);
    expect(await Promise.race([exitPromise, new Promise<number>((resolve) => setTimeout(() => resolve(-9), 3000))])).toBe(0);
    socket.destroy();
    server.close();
    fs.rmSync(dir, { recursive: true, force: true });
  });

  it("协议错误退出码 2", async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "mcsup-plugin-test-"));
    // Windows 上 Unix socket 路径不可监听（EACCES）；node 的 net 模块原生支持
    // 命名管道（`\\.\pipe\name`），SDK 的 `connect` 走同一条 `net.createConnection`。
    const socketPath =
      process.platform === "win32"
        ? `\\\\.\\pipe\\mcsup-plugin-test-${process.pid}-${Math.random().toString(36).slice(2)}`
        : path.join(dir, "host.sock");
    const server = net.createServer();
    await new Promise<void>((resolve) => {
      server.listen(socketPath, resolve);
    });
    const accepted = new Promise<net.Socket>((resolve) => {
      server.once("connection", resolve);
    });
    const child = spawn(process.execPath, [ENTRY, "--endpoint", socketPath], {
      env: { ...process.env, MCSUP_PLUGIN_TOKEN: "c".repeat(64) },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const socket = await accepted;
    await new Promise((r) => setTimeout(r, 100));
    const exitPromise = new Promise<number>((resolve) => {
      child.on("exit", (code) => {
        resolve(code ?? -1);
      });
    });
    socket.write('{"v":99,"type":"ping","n":1}\n');
    expect(await Promise.race([exitPromise, new Promise<number>((resolve) => setTimeout(() => resolve(-9), 3000))])).toBe(2);
    socket.destroy();
    server.close();
    fs.rmSync(dir, { recursive: true, force: true });
  });
});

