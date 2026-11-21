import { execFileSync } from "node:child_process";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { entryWrapperSource, platformEntries } from "../src/cli.js";

describe("平台启动器（§10.1）", () => {
  it("平台键与宿主 platform_key() 一致", () => {
    const entries = platformEntries();
    expect(Object.keys(entries).sort()).toEqual([
      "darwin-arm64",
      "darwin-x64",
      "linux-arm64",
      "linux-x64",
      "win32-arm64",
      "win32-x64",
    ]);
    expect(entries["win32-x64"]).toBe("./bin/run.cmd");
    expect(entries["win32-arm64"]).toBe("./bin/run.cmd");
    for (const key of ["linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64"]) {
      expect(entries[key]).toBe("./bin/run.sh");
    }
  });

  it("bin 启动器显式调用 main", () => {
    const bin = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../bin/mcsup-plugin.js");
    // 未知命令会走 main 的失败分支；若启动器没有转发，进程会静默退出 0
    const run = (): void => {
      execFileSync(process.execPath, [bin, "unknown-command"], { stdio: "ignore" });
    };
    expect(run).toThrow();
    try {
      run();
    } catch (err) {
      expect((err as { status?: number }).status).toBe(1);
    }
  });

  it("entry 包装器导入默认导出并交给 run，作者不必写启动样板", () => {
    const source = entryWrapperSource("../dist/index.js");
    expect(source).toContain('import { run } from "mcsup-plugin"');
    expect(source).toContain('await import("../dist/index.js")');
    expect(source).toContain("mod.default ?? mod.plugin");
    expect(source).toContain("process.exitCode = await run(plugin)");
  });
});
