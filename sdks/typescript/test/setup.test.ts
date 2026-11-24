import { mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";

import { afterEach, describe, expect, it } from "vitest";

import { definePlugin, z } from "../src/index.js";
import { ExitCode } from "../src/protocol.js";
import { run } from "../src/runtime.js";
import { runSetupField } from "../src/setup.js";

const dirs: string[] = [];

/** 新建一个临时目录，测试结束统一清理 */
function tempDir(): string {
  const dir = mkdtempSync(path.join(tmpdir(), "mcsup-setup-"));
  dirs.push(dir);
  return dir;
}

afterEach(() => {
  // 只删测试自己建的目录；失败时残留也不影响下次运行
  for (const dir of dirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

function plugin(handler: () => string | Promise<string>) {
  return definePlugin({
    id: "setup-demo",
    title: "安装向导",
    version: "1.0.0",
    license: "MIT",
    capabilities: [],
    config: z.object({ token: z.string() }),
    setup: {
      fields: [{ path: "token", required: true, secret: true, interactive: true }],
      interactive: { token: handler },
    },
  });
}

describe("交互式 setup 字段", () => {
  it("运行 hook 并把密钥写入 0600 文件", async () => {
    const dir = tempDir();
    const file = path.join(dir, "secret");
    const code = await runSetupField(plugin(() => "abc"), {
      MCSUP_SETUP_FIELD: "token",
      MCSUP_SETUP_SECRET_FILE: file,
    });
    expect(code).toBe(ExitCode.Ok);
    expect(readFileSync(file, "utf8")).toBe("abc");
    if (process.platform !== "win32") {
      expect(statSync(file).mode & 0o777).toBe(0o600);
    }
  });

  it("runtime 在缺少端点与令牌时也能进入 setup 模式", async () => {
    const dir = tempDir();
    const file = path.join(dir, "secret");
    const code = await run(plugin(() => "xyz"), {
      argv: ["node", "entry.mjs"],
      env: { MCSUP_SETUP_FIELD: "token", MCSUP_SETUP_SECRET_FILE: file },
    });
    expect(code).toBe(ExitCode.Ok);
    expect(readFileSync(file, "utf8")).toBe("xyz");
  });

  it("handler 抛错时返回失败码且不落盘", async () => {
    const dir = tempDir();
    const file = path.join(dir, "secret");
    const code = await runSetupField(
      plugin(() => {
        throw new Error("扫码超时");
      }),
      { MCSUP_SETUP_FIELD: "token", MCSUP_SETUP_SECRET_FILE: file },
    );
    expect(code).toBe(ExitCode.Failure);
    expect(() => statSync(file)).toThrow();
  });

  it("空值与未知字段被拒绝", async () => {
    const dir = tempDir();
    const file = path.join(dir, "secret");
    expect(
      await runSetupField(plugin(() => "   "), {
        MCSUP_SETUP_FIELD: "token",
        MCSUP_SETUP_SECRET_FILE: file,
      }),
    ).toBe(ExitCode.Failure);
    expect(
      await runSetupField(plugin(() => "abc"), {
        MCSUP_SETUP_FIELD: "missing",
        MCSUP_SETUP_SECRET_FILE: file,
      }),
    ).toBe(ExitCode.Failure);
  });

  it("缺少环境变量时返回协议错误", async () => {
    expect(await runSetupField(plugin(() => "abc"), {})).toBe(ExitCode.Protocol);
  });
});