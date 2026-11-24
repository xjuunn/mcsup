import { execFileSync } from "node:child_process";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { entryWrapperSource, isValidBranchName, parseGithubRemote, platformEntries, publishCommitMessage } from "../src/cli.js";

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

describe("发布到 GitHub（§15.1 / 附录 E）", () => {
  it("提交信息带上插件 id 与版本", () => {
    expect(publishCommitMessage("chat-bridge", "0.3.1")).toBe("release: chat-bridge 0.3.1");
    expect(publishCommitMessage("chat-bridge", undefined)).toBe("release: chat-bridge");
    expect(publishCommitMessage(undefined, "0.3.1")).toBe("release: plugin 0.3.1");
    expect(publishCommitMessage(undefined, undefined)).toBe("release: plugin");
  });

  it("分支名拒绝注入与非法引用形态", () => {
    for (const ok of ["dist", "release/0.3.1", "v0.3.1", "a_b-c.d"]) {
      expect(isValidBranchName(ok), ok).toBe(true);
    }
    for (const bad of [
      "",
      "-x",
      "a..b",
      "a//b",
      "dist/",
      "dist.lock",
      "a b",
      "refs/heads/x;rm",
      "a/.b",
      "a/b.",
      "a./b",
      "a.lock/b",
      "@",
    ]) {
      expect(isValidBranchName(bad), bad).toBe(false);
    }
  });

  it("从 remote 地址解析出 owner/repo", () => {
    expect(parseGithubRemote("https://github.com/example/chat-bridge.git")).toEqual({
      owner: "example",
      repo: "chat-bridge",
    });
    expect(parseGithubRemote("https://github.com/example/chat-bridge")).toEqual({
      owner: "example",
      repo: "chat-bridge",
    });
    expect(parseGithubRemote("git@github.com:example/chat-bridge.git")).toEqual({
      owner: "example",
      repo: "chat-bridge",
    });
    expect(parseGithubRemote("ssh://git@github.com/example/chat-bridge.git")).toEqual({
      owner: "example",
      repo: "chat-bridge",
    });
    expect(parseGithubRemote("https://gitlab.com/example/chat-bridge")).toBeUndefined();
    expect(parseGithubRemote("https://github.com/example")).toBeUndefined();
  });
});
