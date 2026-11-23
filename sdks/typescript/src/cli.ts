/**
 * mcsup-plugin：插件构建工具
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
 */

import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import * as fs from "node:fs";
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

/** 启动器固定名：与插件 id 无关，manifest 里的路径不随 id 变化 */
const ENTRY_WRAPPER = "entry.mjs";

const HELP = `mcsup-plugin <command> [options]

Commands:
  build        编译插件并生成 manifest / descriptor

Build options:
  --out <dir>        产物目录，默认 ./build
  --entry <file>     插件入口源码（相对 cwd），默认 src/index.ts
  --descriptor <f>   descriptor 输出名，默认 descriptor.json
  --manifest <f>     manifest 输出名，默认 manifest.json
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
  if (command !== "build") {
    fail(`unknown command ${command}`);
  }
  const options = parseBuildArgs(argv.slice(3));
  build(options);
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

  const compiledEntry = path.join(options.outDir, "dist", relJs(options.entry));
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
 * §10.3）。目录按真实路径去重，避免符号链接成环时无限递归。
 *
 * 包内的 `node_modules` 一律跳过：那是这个包装过的依赖，由队列按
 * `package.json` 重新解析并铺到顶层——照抄会把整棵开发依赖
 * （typescript / vitest）塞进产物，`file:` 装的 SDK 尤其明显。
 */
function copyTree(source: string, target: string, seen: Set<string> = new Set()): void {
  const info = fs.statSync(source);
  if (!info.isDirectory()) {
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.copyFileSync(source, target);
    return;
  }
  const real = fs.realpathSync(source);
  if (seen.has(real)) {
    return;
  }
  seen.add(real);
  fs.mkdirSync(target, { recursive: true });
  for (const entry of fs.readdirSync(source)) {
    if (entry === "node_modules") {
      continue;
    }
    copyTree(path.join(source, entry), path.join(target, entry), seen);
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
  const target = `../dist/${relJs(options.entry).split(path.sep).join("/")}`;
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

/** `src/index.ts` → `index.js`（tsconfig 以 src 为 rootDir） */
function relJs(entry: string): string {
  const normalized = entry.split(path.sep).join("/");
  const marker = normalized.lastIndexOf("/src/");
  const rel = marker >= 0 ? normalized.slice(marker + 5) : path.basename(normalized);
  return rel.replace(/\.tsx?$/, ".js");
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
