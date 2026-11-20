/**
 * mcsup-plugin：插件构建工具
 *
 * `mcsup-plugin build` 的执行顺序：
 *   1. tsc 把插件源码编译到 dist
 *   2. 用 node --mcsup-dump-descriptor 取归一化 descriptor
 *   3. 用 node --mcsup-dump-manifest 取 manifest（entry 由本工具写入）
 *   4. 生成平台启动器（bin/run.sh / bin/run.cmd）并写进 manifest.entry
 *
 * 产物目录布局（plugins.md §10 / §11）：
 *   <out>/manifest.json
 *   <out>/descriptor.json
 *   <out>/dist/...
 *   <out>/bin/run.sh, bin/run.cmd
 *   <out>/bin/<id>.mjs（插件入口的薄包装）
 */

import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";

interface BuildOptions {
  readonly cwd: string;
  readonly outDir: string;
  readonly entry: string;
  readonly descriptor: string;
  readonly manifest: string;
}

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

function main(argv: readonly string[]): void {
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
    execFileSync(process.execPath, [requireBinPath("typescript", "bin/tsc"), "-p", tsconfig, "--outDir", path.join(options.outDir, "dist")], {
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

  const manifest = dumpManifest(options, compiledEntry);
  manifest.entry = writeLaunchers(options, manifest.id as string);
  writeJson(path.join(options.outDir, options.manifest), manifest);

  process.stderr.write(`[build] 生成 descriptor ...\n`);
  const descriptor = dumpDescriptor(options, compiledEntry);
  writeJson(path.join(options.outDir, options.descriptor), descriptor);

  process.stderr.write(`[build] 完成：${options.outDir}\n`);
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

function writeLaunchers(options: BuildOptions, id: string): ProcessEntry {
  const binDir = path.join(options.outDir, "bin");
  fs.mkdirSync(binDir, { recursive: true });
  const wrapperName = `${id}.mjs`;
  const wrapper = path.join(binDir, wrapperName);
  // 包装：把 argv 原样交给编译产物，进程退出码即插件退出码
  fs.writeFileSync(
    wrapper,
    `#!/usr/bin/env node\nimport(${JSON.stringify("../dist/" + relJs(options.entry).split(path.sep).join("/"))}).catch((err) => {\n  console.error(err);\n  process.exit(1);\n});\n`,
    "utf8",
  );

  const unix = path.join(binDir, "run.sh");
  fs.writeFileSync(
    unix,
    `#!/bin/sh\nexec node "$(dirname "$0")/${wrapperName}" "$@"\n`,
    "utf8",
  );
  fs.chmodSync(unix, 0o755);

  const win = path.join(binDir, "run.cmd");
  fs.writeFileSync(win, `@echo off\r\nnode "%~dp0${wrapperName}" %*\r\n`, "utf8");

  return {
    type: "process",
    cmd: "./bin/run.sh",
    platforms: { "windows-x64": "./bin/run.cmd", "windows-arm64": "./bin/run.cmd" },
  };
}

function writeJson(file: string, value: unknown): void {
  // 与 SDK 的归一化一致：键字典序 + 紧凑 JSON + 末尾换行
  const normalized = (input: unknown): string => {
    const sort = (v: unknown): unknown => {
      if (Array.isArray(v)) {
        return v.map(sort);
      }
      if (typeof v === "object" && v !== null) {
        const out: Record<string, unknown> = {};
        for (const key of Object.keys(v as Record<string, unknown>).sort()) {
          out[key] = sort((v as Record<string, unknown>)[key]);
        }
        return out;
      }
      return v;
    };
    return `${JSON.stringify(sort(input))}\n`;
  };
  fs.writeFileSync(file, normalized(value), "utf8");
}

main(process.argv);