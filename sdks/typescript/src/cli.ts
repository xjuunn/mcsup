/**
 * mcsup-plugin：插件构建与发布工具
 *
 * `mcsup-plugin build` 的执行顺序：
 *   1. tsc 把插件源码编译到 dist
 *   2. 生成 `bin/entry.mjs`：导入编译产物默认导出并调用 `run()`
 *   3. 用 node --mcsup-dump-descriptor 取归一化 descriptor
 *   4. 用 node --mcsup-dump-manifest 取 manifest（entry 由本工具写入）
 *   5. 生成平台启动器（bin/run.sh / bin/run.cmd）并写进 manifest.entry
 *
 * 产物目录布局（plugins.md §10 / §11）：
 *   <out>/manifest.json
 *   <out>/descriptor.json
 *   <out>/dist/...
 *   <out>/bin/run.sh, bin/run.cmd
 *   <out>/bin/entry.mjs（固定名：插件入口的薄包装，负责自动 run）
 *
 * 作者只需 `export default definePlugin({...})`，不必写 `run()` 样板。
 *
 * `mcsup-plugin publish` 把产物发布成用户能直接装的 GitHub 来源
 * （plugins.md §15.1 / 附录 E）：
 *   1. 跑一次 `build`（`--no-build` 跳过）；
 *   2. 用临时索引把 `<out>` 写成**只含产物**的一次提交；
 *   3. 把该提交推到 `<remote>` 的 `<branch>`（默认 `origin` / `dist`）。
 *
 * 全程用 git 底层命令（`read-tree` / `add` / `write-tree` / `commit-tree` /
 * `push`），并靠 `GIT_INDEX_FILE` + `GIT_WORK_TREE` 把索引与工作树指到产物
 * 目录：作者的工作区、暂存区与当前分支都不受影响，不必手工切 `dist` 分支。
 * `dist` 分支只放产物，源码留在默认分支，用户装
 * `https://github.com/<owner>/<repo>/tree/dist` 即可。
 */

import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { pathToFileURL } from "node:url";

import { serializeNormalized } from "./descriptor.js";

interface BuildOptions {
  readonly cwd: string;
  readonly outDir: string;
  readonly entry: string;
  readonly descriptor: string;
  readonly manifest: string;
}

interface PublishOptions extends BuildOptions {
  readonly branch: string;
  readonly remote: string;
  readonly message?: string;
  readonly skipBuild: boolean;
  readonly dryRun: boolean;
}

/** 启动器固定名：与插件 id 无关，manifest 里的路径不随 id 变化 */
const ENTRY_WRAPPER = "entry.mjs";

/**
 * 会被父进程（pre-commit 钩子、IDE 里的 git 操作）泄漏进来的变量。
 *
 * 它们会越过 `cwd` 把命令作用到别的仓库上，必须先清掉再盖上我们自己的
 * `GIT_DIR` / `GIT_INDEX_FILE` / `GIT_WORK_TREE`。
 */
const GIT_ISOLATION = [
  "GIT_DIR",
  "GIT_WORK_TREE",
  "GIT_INDEX_FILE",
  "GIT_OBJECT_DIRECTORY",
  "GIT_ALTERNATE_OBJECT_DIRECTORIES",
  "GIT_COMMON_DIR",
  "GIT_NAMESPACE",
] as const;

const HELP = `mcsup-plugin <command> [options]

Commands:
  build        编译插件并生成 manifest / descriptor
  publish      构建产物并推送到 GitHub 的 dist 分支（用户可直接安装）

Build options:
  --out <dir>        产物目录，默认 ./build
  --entry <file>     插件入口源码（相对 cwd），默认 src/index.ts
  --descriptor <f>   descriptor 输出名，默认 descriptor.json
  --manifest <f>     manifest 输出名，默认 manifest.json

Publish options（另含全部 build 选项）:
  --branch <name>    发布分支，默认 dist
  --remote <name>    git remote，默认 origin
  --message <text>   提交信息，默认 "release: <id> <version>"
  --no-build         跳过构建，只发布 <out> 里已有的产物
  --dry-run          只生成提交，不推送
`;

function fail(message: string): never {
  process.stderr.write(`mcsup-plugin: ${message}\n`);
  process.exit(1);
}

export function main(argv: readonly string[]): void {
  const command = argv[2];
  if (command === undefined || command === "--help" || command === "-h" || command === "help") {
    process.stdout.write(HELP);
    process.exit(0);
  }
  if (command === "build") {
    build(parseBuildArgs(argv.slice(3)));
    return;
  }
  if (command === "publish") {
    publish(parsePublishArgs(argv.slice(3)));
    return;
  }
  fail(`unknown command ${command}`);
}

function parseBuildArgs(args: readonly string[]): BuildOptions {
  const options = {
    cwd: process.cwd(),
    outDir: path.resolve(process.cwd(), "build"),
    entry: "src/index.ts",
    descriptor: "descriptor.json",
    manifest: "manifest.json",
  };
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    const value = (): string => {
      const next = args[i + 1];
      if (next === undefined) {
        fail(`${arg} requires a value`);
      }
      i += 1;
      return next;
    };
    switch (arg) {
      case "--out":
        options.outDir = path.resolve(options.cwd, value());
        break;
      case "--entry":
        options.entry = value();
        break;
      case "--descriptor":
        options.descriptor = value();
        break;
      case "--manifest":
        options.manifest = value();
        break;
      default:
        fail(`unknown build option ${arg}`);
    }
  }
  return options;
}

function parsePublishArgs(args: readonly string[]): PublishOptions {
  const cwd = process.cwd();
  let outDir = path.resolve(cwd, "build");
  let entry = "src/index.ts";
  let descriptor = "descriptor.json";
  let manifest = "manifest.json";
  let branch = "dist";
  let remote = "origin";
  let message: string | undefined;
  let skipBuild = false;
  let dryRun = false;
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    const value = (): string => {
      const next = args[i + 1];
      if (next === undefined) {
        fail(`${arg} requires a value`);
      }
      i += 1;
      return next;
    };
    switch (arg) {
      case "--out":
        outDir = path.resolve(cwd, value());
        break;
      case "--entry":
        entry = value();
        break;
      case "--descriptor":
        descriptor = value();
        break;
      case "--manifest":
        manifest = value();
        break;
      case "--branch":
        branch = value();
        break;
      case "--remote":
        remote = value();
        break;
      case "--message":
        message = value();
        break;
      case "--no-build":
        skipBuild = true;
        break;
      case "--dry-run":
        dryRun = true;
        break;
      default:
        fail(`unknown publish option ${arg}`);
    }
  }
  return { cwd, outDir, entry, descriptor, manifest, branch, remote, message, skipBuild, dryRun };
}

function build(options: BuildOptions): void {
  const tsconfig = findTsconfig(options.cwd);
  process.stderr.write(`[build] 编译 ${tsconfig} ...\n`);
  try {
    execFileSync(process.execPath, [requireBinPath("typescript", "bin/tsc"), "-p", tsconfig, "--outDir", path.join(options.outDir, "dist"), "--noEmit", "false"], {
      cwd: options.cwd,
      stdio: ["ignore", "inherit", "inherit"],
    });
  } catch {
    fail("typescript compile failed");
  }

  const compiledEntry = path.join(options.outDir, "dist", relJs(options.entry, options.cwd));
  if (!fs.existsSync(compiledEntry)) {
    fail(`compiled entry not found: ${compiledEntry}`);
  }
  const wrapper = writeEntryWrapper(options);

  // 必须在 dump 之前 vendor：启动器 `import "mcsup-plugin"`，装到宿主之后
  // 作者的 node_modules 不在旁边，只有产物自带的这一份能解析。
  process.stderr.write(`[build] vendor 运行时依赖 ...\n`);
  const vendored = vendorRuntimeDependencies(options.cwd, options.outDir);
  process.stderr.write(`[build] 依赖：${vendored.join(", ") || "（无）"}\n`);

  // dump 也走启动器：导入插件模块再走 run() 的 dump 分支，
  // 与真实启动同一条路径，避免两处入口行为分叉
  const manifest = dumpManifest(options, wrapper);
  manifest.entry = writeLaunchers(options);
  writeJson(path.join(options.outDir, options.manifest), manifest);

  process.stderr.write(`[build] 生成 descriptor ...\n`);
  const descriptor = dumpDescriptor(options, wrapper);
  writeJson(path.join(options.outDir, options.descriptor), descriptor);

  process.stderr.write(`[build] 完成：${options.outDir}\n`);
}

/**
 * 发布：把 `<out>` 写成一次只含产物的提交，推到 `<remote>/<branch>`。
 *
 * 关键点：不切分支、不动工作区、不动暂存区。git 的索引与工作树分别由
 * `GIT_INDEX_FILE` / `GIT_WORK_TREE` 指过去，`GIT_DIR` 显式解析成绝对路径
 * （`<out>` 可能根本不在仓库目录里）。所以作者可以随时跑，正在写代码也不怕。
 */
function publish(options: PublishOptions): void {
  if (!isValidBranchName(options.branch)) {
    fail(`invalid branch name ${options.branch}`);
  }
  if (!options.skipBuild) {
    build(options);
  }
  const manifestPath = path.join(options.outDir, options.manifest);
  if (!fs.existsSync(manifestPath)) {
    fail(`no ${options.manifest} in ${options.outDir}; run \`mcsup-plugin build\` first`);
  }
  const id = readManifestField(manifestPath, "id");
  const version = readManifestField(manifestPath, "version");
  const message = options.message ?? publishCommitMessage(id, version);

  const work = fs.mkdtempSync(path.join(os.tmpdir(), "mcsup-plugin-publish-"));
  try {
    const gitDir = runGit(cleanEnv(), options.cwd, ["rev-parse", "--absolute-git-dir"]).trim();
    const env = cleanEnv();
    env.GIT_DIR = gitDir;
    env.GIT_INDEX_FILE = path.join(work, "index");
    env.GIT_WORK_TREE = options.outDir;
    // 空索引起步：产物分支只放产物，不带工程源码，也不受当前暂存区影响
    runGit(env, options.cwd, ["read-tree", "--empty"]);
    // 工作树是产物目录，`add .` 因此写到以产物为根的路径（没有 build/ 前缀）
    runGit(env, options.outDir, ["add", "--all", "--force", "."]);
    const tree = runGit(env, options.cwd, ["write-tree"]).trim();
    const parent =
      revParse(env, options.cwd, `refs/heads/${options.branch}`) ??
      revParse(env, options.cwd, `refs/remotes/${options.remote}/${options.branch}`);
    const commitArgs = ["commit-tree", tree, "-m", message];
    if (parent !== undefined) {
      commitArgs.push("-p", parent);
    }
    const commit = runGit(env, options.cwd, commitArgs).trim();
    if (options.dryRun) {
      process.stderr.write(
        `[publish] 预演：${options.branch} 会指向 ${shortId(commit)}（未推送）\n`,
      );
      printInstallHint(env, options);
      return;
    }
    runGit(env, options.cwd, ["push", "--quiet", options.remote, `${commit}:refs/heads/${options.branch}`]);
    process.stderr.write(
      `[publish] 已推送到 ${options.remote}/${options.branch}（${shortId(commit)}）\n`,
    );
    printInstallHint(env, options);
  } finally {
    fs.rmSync(work, { recursive: true, force: true });
  }
}

/** 提交信息；manifest 缺字段时退化成只有插件名 */
export function publishCommitMessage(
  id: string | undefined,
  version: string | undefined,
): string {
  if (id === undefined) {
    return version === undefined ? "release: plugin" : `release: plugin ${version}`;
  }
  return version === undefined ? `release: ${id}` : `release: ${id} ${version}`;
}

/**
 * 分支名是否可安全交给 `git push <commit>:refs/heads/<branch>`。
 *
 * 按 `git check-ref-format` 的规则做保守检查：拒绝空格、`..`、空段、以 `.`
 * 开头的段、以 `.` 或 `.lock` 结尾的段、`@{` 与 `@` 本身，避免把引用名交给
 * git 自己报一堆难懂的错。
 */
export function isValidBranchName(branch: string): boolean {
  if (!/^[A-Za-z0-9_][A-Za-z0-9._/-]*$/u.test(branch)) {
    return false;
  }
  if (branch === "@" || branch.includes("..") || branch.includes("//") || branch.includes("@{")) {
    return false;
  }
  return branch.split("/").every((part) => {
    if (part === "" || part.startsWith(".") || part.endsWith(".")) {
      return false;
    }
    return !part.endsWith(".lock");
  });
}

/**
 * 从 remote 地址里取 GitHub 的 `owner/repo`。
 *
 * 支持 `https://github.com/o/r(.git)`、`git@github.com:o/r(.git)` 与
 * `ssh://git@github.com/o/r(.git)`。其它 host 返回 `undefined`。
 */
export function parseGithubRemote(
  url: string,
): { readonly owner: string; readonly repo: string } | undefined {
  const match = /^(?:[a-z][a-z0-9+.-]*:\/\/)?(?:[^@/\s]+@)?github\.com[/:](.+)$/iu.exec(
    url.trim(),
  );
  const rest = match?.[1];
  if (rest === undefined) {
    return undefined;
  }
  const [owner, repoWithSuffix] = rest.split("/");
  const repo = repoWithSuffix?.replace(/\.git$/iu, "");
  if (owner === undefined || owner === "" || repo === undefined || repo === "") {
    return undefined;
  }
  return { owner, repo };
}

/** 读 manifest 的字符串字段；非字符串或缺失时返回 `undefined` */
function readManifestField(file: string, field: string): string | undefined {
  let raw: string;
  try {
    raw = fs.readFileSync(file, "utf8");
  } catch (err) {
    fail(`cannot read ${file}: ${String(err)}`);
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (err) {
    fail(`invalid ${file}: ${String(err)}`);
  }
  if (typeof parsed !== "object" || parsed === null) {
    return undefined;
  }
  const value = (parsed as Record<string, unknown>)[field];
  return typeof value === "string" ? value : undefined;
}

/** 打印用户可直接复制的安装命令 */
function printInstallHint(env: NodeJS.ProcessEnv, options: PublishOptions): void {
  const url = gitTry(env, options.cwd, ["remote", "get-url", options.remote])?.trim();
  const repo = url === undefined || url === "" ? undefined : parseGithubRemote(url);
  const target =
    repo === undefined
      ? `https://github.com/<owner>/<repo>/tree/${options.branch}`
      : `https://github.com/${repo.owner}/${repo.repo}/tree/${options.branch}`;
  process.stderr.write(`[publish] 安装：mcsup plugin install ${target}\n`);
}

function shortId(commit: string): string {
  return commit.slice(0, 7);
}

/** 复制一份环境变量并清掉会指向别的仓库的 `GIT_*` */
function cleanEnv(): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env };
  for (const name of GIT_ISOLATION) {
    delete env[name];
  }
  return env;
}

/** 跑 git，失败时把 stderr 的第一行当作错误原因 */
function runGit(env: NodeJS.ProcessEnv, cwd: string, args: readonly string[]): string {
  try {
    return execFileSync("git", [...args], {
      cwd,
      env,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    });
  } catch (err) {
    fail(`git ${args.join(" ")} failed: ${gitDetail(err)}`);
  }
}

/** 跑 git，失败时返回 `undefined`（用于「可能不存在」的查询） */
function gitTry(env: NodeJS.ProcessEnv, cwd: string, args: readonly string[]): string | undefined {
  try {
    return execFileSync("git", [...args], {
      cwd,
      env,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    });
  } catch {
    return undefined;
  }
}

function revParse(env: NodeJS.ProcessEnv, cwd: string, ref: string): string | undefined {
  const value = gitTry(env, cwd, ["rev-parse", "--verify", "--quiet", ref])?.trim();
  return value === undefined || value === "" ? undefined : value;
}

function gitDetail(err: unknown): string {
  const stderr = (err as { stderr?: string | Buffer }).stderr;
  const text = typeof stderr === "string" ? stderr : stderr?.toString("utf8") ?? String(err);
  const line = text
    .split(/\r?\n/u)
    .map((entry) => entry.trim())
    .find((entry) => entry !== "");
  return line ?? String(err);
}

/**
 * 把运行时依赖整棵拷进 `<out>/node_modules`。
 *
 * 为什么必须做：`bin/entry.mjs` 与编译产物都 `import "mcsup-plugin"`（还有
 * 作者自己声明的依赖、以及 zod 这个 peer）。产物装到宿主目录后，作者的工程
 * 目录不在旁边，`node_modules` 又不在产物里，进程会以
 * `ERR_MODULE_NOT_FOUND` 直接退出，宿主只能报 `handshake_timeout`。
 *
 * 只拷生产依赖（`dependencies` + 可解析的 `peerDependencies`），不碰
 * `devDependencies`；`dereference` 把符号链接展开成实体文件——宿主的产物校验
 * 会拒绝符号链接（`docs/plugins.md` §10.3）。
 *
 * @returns 已 vendor 的包名（按发现顺序）
 */
export function vendorRuntimeDependencies(cwd: string, outDir: string): readonly string[] {
  const root = path.join(outDir, "node_modules");
  fs.rmSync(root, { recursive: true, force: true });
  const project = readPackageJson(path.join(cwd, "package.json"), "工程 package.json");
  const queue: Array<{ readonly name: string; readonly from: string; readonly required: boolean }> = [];
  for (const name of Object.keys(dependencyFields(project, "dependencies"))) {
    queue.push({ name, from: cwd, required: true });
  }
  // 启动器固定 import 本包名，作者把它写进 devDependencies 也要带上
  queue.push({ name: SDK_PACKAGE, from: cwd, required: true });
  for (const name of Object.keys(dependencyFields(project, "peerDependencies"))) {
    queue.push({ name, from: cwd, required: false });
  }

  const vendored: string[] = [];
  const seen = new Set<string>();
  for (let item = queue.shift(); item !== undefined; item = queue.shift()) {
    if (seen.has(item.name)) {
      continue;
    }
    const source = resolvePackageDir(item.from, item.name);
    if (source === undefined) {
      if (item.required) {
        fail(`dependency ${item.name} is not installed; run npm install`);
      }
      continue;
    }
    seen.add(item.name);
    const target = path.join(root, ...item.name.split("/"));
    try {
      copyTree(source, target);
    } catch (err) {
      fail(`failed to vendor ${item.name}: ${String(err)}`);
    }
    vendored.push(item.name);
    const pkg = readPackageJson(path.join(source, "package.json"), `${item.name} package.json`);
    for (const name of Object.keys(dependencyFields(pkg, "dependencies"))) {
      queue.push({ name, from: source, required: true });
    }
    for (const name of Object.keys(dependencyFields(pkg, "peerDependencies"))) {
      queue.push({ name, from: source, required: false });
    }
  }
  return vendored;
}

/** 启动器固定依赖的 SDK 包名 */
const SDK_PACKAGE = "mcsup-plugin";

/**
 * 递归拷贝目录树，跟随符号链接。
 *
 * 不用 `fs.cpSync({dereference:true})`：Node 24 上它对文件符号链接不生效
 * （实测仍拷成链接），而宿主的产物校验会拒绝符号链接（`docs/plugins.md`
 * §10.3）。去重只针对当前递归路径上的祖先，避免符号链接成环时无限递归；
 * 指向已拷过的兄弟目录的链接必须再拷一份，否则产物里缺少那个路径。
 *
 * 包内的 `node_modules` 一律跳过：那是这个包装过的依赖，由队列按
 * `package.json` 重新解析并铺到顶层——照抄会把整棵开发依赖
 * （typescript / vitest）塞进产物，`file:` 装的 SDK 尤其明显。
 */
function copyTree(source: string, target: string, ancestors: Set<string> = new Set()): void {
  const info = fs.statSync(source);
  if (!info.isDirectory()) {
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.copyFileSync(source, target);
    return;
  }
  const real = fs.realpathSync(source);
  if (ancestors.has(real)) {
    // 符号链接指回当前路径上的祖先目录：拷贝下去必然无限递归
    return;
  }
  ancestors.add(real);
  try {
    fs.mkdirSync(target, { recursive: true });
    for (const entry of fs.readdirSync(source)) {
      if (entry === "node_modules") {
        continue;
      }
      copyTree(path.join(source, entry), path.join(target, entry), ancestors);
    }
  } finally {
    ancestors.delete(real);
  }
}

/** 读取 package.json；缺失或损坏时给出可读的错误 */
function readPackageJson(file: string, label: string): Record<string, unknown> {
  let raw: string;
  try {
    raw = fs.readFileSync(file, "utf8");
  } catch {
    fail(`${label} not found: ${file}`);
  }
  try {
    return JSON.parse(raw) as Record<string, unknown>;
  } catch (err) {
    fail(`invalid ${label}: ${String(err)}`);
  }
}

/** 取 `dependencies` / `peerDependencies` 的键集合；缺失时为空 */
function dependencyFields(pkg: Record<string, unknown>, field: string): Record<string, string> {
  const value = pkg[field];
  if (typeof value !== "object" || value === null) {
    return {};
  }
  return value as Record<string, string>;
}

/** 从 `from` 起向上找 `node_modules/<name>/package.json`（兼容嵌套安装） */
function resolvePackageDir(from: string, name: string): string | undefined {
  const segments = name.split("/");
  let dir = path.resolve(from);
  for (;;) {
    const candidate = path.join(dir, "node_modules", ...segments);
    if (fs.existsSync(path.join(candidate, "package.json"))) {
      return candidate;
    }
    const parent = path.dirname(dir);
    if (parent === dir) {
      return undefined;
    }
    dir = parent;
  }
}

/**
 * 写 `bin/entry.mjs`：导入编译产物，取默认导出交给 `run()`。
 *
 * `run()` 幂等，作者的模块若自己写过 `run(plugin)` 也不会跑第二次。
 */
function writeEntryWrapper(options: BuildOptions): string {
  const binDir = path.join(options.outDir, "bin");
  fs.mkdirSync(binDir, { recursive: true });
  const wrapper = path.join(binDir, ENTRY_WRAPPER);
  const target = `../dist/${relJs(options.entry, options.cwd).split(path.sep).join("/")}`;
  fs.writeFileSync(wrapper, entryWrapperSource(target), "utf8");
  return wrapper;
}

/** 启动器源码；单独导出便于断言生成内容 */
export function entryWrapperSource(target: string): string {
  return `#!/usr/bin/env node
import { run } from "mcsup-plugin";

let mod;
try {
  mod = await import(${JSON.stringify(target)});
} catch (err) {
  console.error("[mcsup-plugin] failed to load plugin module:", err);
  process.exit(1);
}

// 默认导出优先；同时兼容具名导出 plugin
const plugin = mod.default ?? mod.plugin;
if (plugin === undefined) {
  console.error("[mcsup-plugin] plugin module has no default export; use 'export default definePlugin({...})'");
  process.exit(1);
}

process.exitCode = await run(plugin);
`;
}

function dumpDescriptor(options: BuildOptions, entry: string): Record<string, unknown> {
  const out = runNode(entry, ["--mcsup-dump-descriptor"], options.cwd);
  return JSON.parse(out) as Record<string, unknown>;
}

function dumpManifest(options: BuildOptions, entry: string): Record<string, unknown> {
  const out = runNode(entry, ["--mcsup-dump-manifest"], options.cwd);
  return JSON.parse(out) as Record<string, unknown>;
}

function runNode(entry: string, args: readonly string[], cwd: string): string {
  try {
    return execFileSync(process.execPath, [entry, ...args], { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "inherit"] });
  } catch (err) {
    fail(`failed to dump plugin metadata: ${String(err)}`);
  }
}

function requireBinPath(pkg: string, rel: string): string {
  // 从插件工程目录解析，不依赖本文件所在位置
  const requireFromProject = createRequire(path.join(process.cwd(), "package.json"));
  try {
    return requireFromProject.resolve(`${pkg}/${rel}`);
  } catch {
    fail(`${pkg} is not installed; run npm install`);
  }
}

function findTsconfig(cwd: string): string {
  for (const name of ["tsconfig.build.json", "tsconfig.json"]) {
    const candidate = path.join(cwd, name);
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }
  fail("no tsconfig.json found in " + cwd);
}

/**
 * `src/index.ts` → `index.js`（tsconfig 以 `src` 为 rootDir）。
 *
 * 入口必须落在 `<cwd>/src` 下：产物路径是相对 rootDir 的位置，用
 * 「最后一个 `/src/`」猜会在 `src/nested/src/index.ts` 上算错。
 */
function relJs(entry: string, cwd: string): string {
  const srcRoot = path.resolve(cwd, "src");
  const absolute = path.resolve(cwd, entry);
  const rel = path.relative(srcRoot, absolute);
  if (rel === "" || rel.startsWith(`..${path.sep}`) || rel === ".." || path.isAbsolute(rel)) {
    fail(`entry ${entry} must be inside ${srcRoot}`);
  }
  return rel.split(path.sep).join("/").replace(/\.tsx?$/, ".js");
}

interface ProcessEntry {
  readonly type: "process";
  readonly cmd: string;
  readonly platforms?: Record<string, string>;
}

function writeLaunchers(options: BuildOptions): ProcessEntry {
  const binDir = path.join(options.outDir, "bin");
  const unix = path.join(binDir, "run.sh");
  fs.writeFileSync(
    unix,
    `#!/bin/sh\nexec node "$(dirname "$0")/${ENTRY_WRAPPER}" "$@"\n`,
    "utf8",
  );
  fs.chmodSync(unix, 0o755);

  const win = path.join(binDir, "run.cmd");
  fs.writeFileSync(win, `@echo off\r\nnode "%~dp0${ENTRY_WRAPPER}" %*\r\n`, "utf8");

  return {
    type: "process",
    cmd: "./bin/run.sh",
    // 平台键必须与宿主 platform_key() 一致（darwin/linux/win32 + x64/arm64），
    // 否则 Windows 会回退到无法执行的 run.sh。
    platforms: platformEntries(),
  };
}

/** 产物内置两个启动器：POSIX 用 run.sh，Windows 用 run.cmd。 */
export function platformEntries(): Record<string, string> {
  return {
    "linux-x64": "./bin/run.sh",
    "linux-arm64": "./bin/run.sh",
    "darwin-x64": "./bin/run.sh",
    "darwin-arm64": "./bin/run.sh",
    "win32-x64": "./bin/run.cmd",
    "win32-arm64": "./bin/run.cmd",
  };
}

function writeJson(file: string, value: unknown): void {
  // 与 SDK 的归一化共用同一实现，避免两处排序规则漂移
  fs.writeFileSync(file, serializeNormalized(value), "utf8");
}

// 仅作为可执行入口时运行 main；被测试或 bin 启动器 import 时显式调用
const invokedDirectly =
  process.argv[1] !== undefined && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href;
if (invokedDirectly) {
  main(process.argv);
}
