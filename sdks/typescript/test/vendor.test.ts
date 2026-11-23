import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import { afterEach, describe, expect, it } from "vitest";

import { vendorRuntimeDependencies } from "../src/cli.js";

/** 建一个假的插件工程：node_modules 里放需要的包 */
function fakeProject(): { readonly cwd: string; readonly out: string } {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "mcsup-vendor-"));
  const cwd = path.join(root, "plugin");
  const out = path.join(cwd, "build");
  const pkg = (dir: string, json: Record<string, unknown>): void => {
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify(json), "utf8");
  };
  pkg(cwd, {
    name: "demo-plugin",
    version: "0.1.0",
    type: "module",
    dependencies: { "mcsup-plugin": "file:../../../sdks/typescript" },
    devDependencies: { typescript: "^5.0.0" },
  });
  fs.writeFileSync(path.join(cwd, "package.json"), JSON.stringify({
    name: "demo-plugin",
    version: "0.1.0",
    type: "module",
    dependencies: { "mcsup-plugin": "file:.." },
    devDependencies: { typescript: "^5.0.0" },
  }), "utf8");

  // 运行时依赖：SDK（带一个嵌套依赖与一个不可解析的 peer）、zod（peer）
  const sdk = path.join(cwd, "node_modules", "mcsup-plugin");
  pkg(sdk, {
    name: "mcsup-plugin",
    version: "0.1.0",
    dependencies: { "nested-dep": "^1.0.0" },
    peerDependencies: { zod: "^4.0.0", "optional-missing": "^1.0.0" },
  });
  fs.mkdirSync(path.join(sdk, "dist"), { recursive: true });
  fs.writeFileSync(path.join(sdk, "dist", "index.js"), "export {};\n", "utf8");
  pkg(path.join(sdk, "node_modules", "nested-dep"), { name: "nested-dep", version: "1.0.0" });
  pkg(path.join(cwd, "node_modules", "zod"), { name: "zod", version: "4.0.0" });
  // 开发依赖：不得进产物
  pkg(path.join(cwd, "node_modules", "typescript"), { name: "typescript", version: "5.0.0" });

  return { cwd, out };
}

const roots: string[] = [];

afterEach(() => {
  for (const root of roots.splice(0)) {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

describe("mcsup-plugin build 的依赖 vendor", () => {
  it("带上生产依赖与 peer，不拷 devDependencies", () => {
    const { cwd, out } = fakeProject();
    roots.push(path.resolve(cwd, ".."));

    const vendored = vendorRuntimeDependencies(cwd, out);

    expect([...vendored].sort()).toEqual(["mcsup-plugin", "nested-dep", "zod"]);
    const modules = path.join(out, "node_modules");
    expect(fs.existsSync(path.join(modules, "mcsup-plugin", "dist", "index.js"))).toBe(true);
    expect(fs.existsSync(path.join(modules, "mcsup-plugin", "package.json"))).toBe(true);
    expect(fs.existsSync(path.join(modules, "zod", "package.json"))).toBe(true);
    // 嵌套依赖按真实位置解析后落到顶层，产物自带完整依赖树
    expect(fs.existsSync(path.join(modules, "nested-dep", "package.json"))).toBe(true);
    expect(fs.existsSync(path.join(modules, "typescript"))).toBe(false);
    // 包内 node_modules 不照抄：否则 file: 装的依赖会把整棵开发依赖带进产物
    expect(fs.existsSync(path.join(modules, "mcsup-plugin", "node_modules"))).toBe(false);
  });

  it("符号链接展开成实体文件（宿主拒绝符号链接产物）", () => {
    const { cwd, out } = fakeProject();
    roots.push(path.resolve(cwd, ".."));
    const target = path.join(cwd, "node_modules", "zod", "linked.js");
    fs.writeFileSync(target, "export const linked = 1;\n", "utf8");
    const link = path.join(cwd, "node_modules", "mcsup-plugin", "aliased.js");
    fs.symlinkSync(target, link);

    vendorRuntimeDependencies(cwd, out);

    const copied = path.join(out, "node_modules", "mcsup-plugin", "aliased.js");
    expect(fs.lstatSync(copied).isSymbolicLink()).toBe(false);
    expect(fs.readFileSync(copied, "utf8")).toContain("linked");
  });

  it("重复构建时不残留上一次的依赖", () => {
    const { cwd, out } = fakeProject();
    roots.push(path.resolve(cwd, ".."));
    const stale = path.join(out, "node_modules", "gone");
    fs.mkdirSync(stale, { recursive: true });

    vendorRuntimeDependencies(cwd, out);

    expect(fs.existsSync(stale)).toBe(false);
  });
});
