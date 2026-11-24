/**
 * `mcsup-plugin publish` 的端到端用例：真起一个仓库 + 一个裸远端。
 *
 * 只验证三件对作者最要紧的事：
 *   1. 产物**原样**落到发布分支（没有 `build/` 前缀、没有工程源码）；
 *   2. 作者的工作区、暂存区与当前分支完全不受影响；
 *   3. `--dry-run` 只造提交、不推送。
 *
 * 没有 git 的环境（精简容器）跳过。
 */

import { execFileSync } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

const bin = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../bin/mcsup-plugin.js",
);

/**
 * 去掉调用方遗留的 `GIT_*`：门禁由 `.githooks/pre-commit` 触发时，git 会把
 * `GIT_DIR` / `GIT_INDEX_FILE` 等导出到 hook 环境，子进程里的 `git init` 会
 * 因此不落在临时目录，而是落回宿主仓库（随后 `remote add origin` 撞上已存在
 * 的 origin）。测试必须自带隔离环境，不依赖调用者是不是 git。
 */
const cleanEnv: NodeJS.ProcessEnv = Object.fromEntries(
  Object.entries(process.env).filter(([key]) => !key.toUpperCase().startsWith("GIT_")),
);

function git(cwd: string, args: readonly string[]): string {
  return execFileSync("git", [...args], {
    cwd,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    env: cleanEnv,
  });
}

function gitOrUndefined(cwd: string, args: readonly string[]): string | undefined {
  try {
    return git(cwd, args);
  } catch {
    return undefined;
  }
}

function hasGit(): boolean {
  return gitOrUndefined(process.cwd(), ["--version"]) !== undefined;
}

interface Fixture {
  readonly root: string;
  readonly repo: string;
  readonly remote: string;
}

/** 一个已提交 README 的仓库 + 一个空的裸远端，外加一份产物目录 */
function fixture(): Fixture {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "mcsup-publish-"));
  const repo = path.join(root, "plugin");
  const remote = path.join(root, "remote.git");
  fs.mkdirSync(repo, { recursive: true });

  git(root, ["init", "--bare", "--quiet", remote]);
  git(repo, ["init", "--quiet"]);
  git(repo, ["config", "user.email", "test@example.com"]);
  git(repo, ["config", "user.name", "test"]);
  git(repo, ["remote", "add", "origin", remote]);
  fs.writeFileSync(path.join(repo, "README.md"), "source\n", "utf8");
  git(repo, ["add", "README.md"]);
  git(repo, ["commit", "--quiet", "-m", "init"]);

  const out = path.join(repo, "build");
  fs.mkdirSync(path.join(out, "dist"), { recursive: true });
  fs.writeFileSync(
    path.join(out, "manifest.json"),
    JSON.stringify({ id: "demo", version: "0.1.0" }),
    "utf8",
  );
  fs.writeFileSync(path.join(out, "descriptor.json"), JSON.stringify({ schema: 1 }), "utf8");
  fs.writeFileSync(path.join(out, "dist", "index.js"), "export default 1;\n", "utf8");
  return { root, repo, remote };
}

function publish(dir: Fixture, args: readonly string[]): string {
  const result = execFileSync(
    process.execPath,
    [bin, "publish", "--no-build", "--out", "build", ...args],
    { cwd: dir.repo, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], env: cleanEnv },
  );
  return result;
}

describe("发布到 dist 分支（§15.1 / 附录 E）", () => {
  it("产物原样落到发布分支，工作区与当前分支不受影响", () => {
    if (!hasGit()) {
      return;
    }
    const dir = fixture();
    const branchBefore = git(dir.repo, ["rev-parse", "--abbrev-ref", "HEAD"]).trim();
    const headBefore = git(dir.repo, ["rev-parse", "HEAD"]).trim();

    publish(dir, ["--branch", "dist"]);

    const files = git(dir.remote, ["ls-tree", "-r", "--name-only", "dist"])
      .split("\n")
      .filter((line) => line !== "")
      .sort();
    expect(files).toEqual(["descriptor.json", "dist/index.js", "manifest.json"]);

    // 工作区、暂存区、当前分支与 HEAD 都没动：`build/` 仍是唯一的未跟踪项
    expect(git(dir.repo, ["diff", "--cached", "--name-only"]).trim()).toBe("");
    const status = git(dir.repo, ["status", "--porcelain"]).trim();
    expect(status.split(/\r?\n/u)).toEqual(["?? build/"]);
    expect(git(dir.repo, ["rev-parse", "HEAD"]).trim()).toBe(headBefore);
    expect(git(dir.repo, ["symbolic-ref", "--short", "HEAD"]).trim()).toBe(branchBefore);
  });

  it("提交信息默认用 manifest 的 id 与版本，也可用 --message 覆盖", () => {
    if (!hasGit()) {
      return;
    }
    const dir = fixture();
    publish(dir, ["--branch", "dist"]);
    expect(git(dir.remote, ["log", "-1", "--pretty=%s", "dist"]).trim()).toBe(
      "release: demo 0.1.0",
    );

    publish(dir, ["--branch", "other", "--message", "hand written"]);
    expect(git(dir.remote, ["log", "-1", "--pretty=%s", "other"]).trim()).toBe("hand written");
  });

  it("--dry-run 只造提交，不推送", () => {
    if (!hasGit()) {
      return;
    }
    const dir = fixture();
    publish(dir, ["--branch", "dist", "--dry-run"]);
    expect(gitOrUndefined(dir.remote, ["rev-parse", "--verify", "refs/heads/dist"])).toBeUndefined();
  });
});
