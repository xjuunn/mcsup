/**
 * 交互式 setup 字段（descriptor schema 5）
 *
 * 宿主在安装向导里遇到 `interactive: true` 的字段时不直接提问，而是以子
 * 进程重新拉起插件入口，并设置两个环境变量：
 *
 *   MCSUP_SETUP_FIELD        配置路径（descriptor 里声明的 `path`）
 *   MCSUP_SETUP_SECRET_FILE  宿主准备的临时文件（0600）
 *
 * 插件此时不连接宿主、不建会话，只执行 `setup.interactive[path]`，把返回的
 * 密钥写入该文件后退出。子进程继承终端，所以二维码、设备授权这类平台特定
 * 流程可以照常显示。密钥只经文件传递：不进 argv，也不回显到 stdout。
 */

import { writeFileSync } from "node:fs";

import type {
  Capability,
  DefinedPlugin,
  PluginDefinition,
} from "./define.js";
import { ExitCode } from "./protocol.js";
import type { SchemaLike } from "./schema.js";

/**
 * 执行一个交互式 setup 字段
 *
 * @returns 进程退出码：0 表示密钥已写入，非 0 表示插件侧失败
 */
export async function runSetupField<
  C extends readonly Capability[],
  Id extends string,
  Cfg extends SchemaLike,
>(
  plugin: DefinedPlugin<C, Id, Cfg> | PluginDefinition<C, Id, Cfg>,
  env: Readonly<Record<string, string | undefined>>,
): Promise<number> {
  const def = ("definition" in plugin ? plugin.definition : plugin) as PluginDefinition<C, Id, Cfg>;
  const field = env["MCSUP_SETUP_FIELD"]?.trim();
  const secretFile = env["MCSUP_SETUP_SECRET_FILE"];
  if (field === undefined || field === "") {
    process.stderr.write("mcsup-plugin setup: missing MCSUP_SETUP_FIELD\n");
    return ExitCode.Protocol;
  }
  if (secretFile === undefined || secretFile === "") {
    process.stderr.write("mcsup-plugin setup: missing MCSUP_SETUP_SECRET_FILE\n");
    return ExitCode.Protocol;
  }
  const handler = def.setup?.interactive?.[field];
  if (handler === undefined) {
    process.stderr.write(`mcsup-plugin setup: field ${field} has no interactive handler\n`);
    return ExitCode.Failure;
  }
  let secret: unknown;
  try {
    secret = await handler({ field });
  } catch (err) {
    process.stderr.write(`mcsup-plugin setup: field ${field} failed: ${String(err)}\n`);
    return ExitCode.Failure;
  }
  if (typeof secret !== "string" || secret.trim() === "") {
    process.stderr.write(`mcsup-plugin setup: field ${field} returned an empty value\n`);
    return ExitCode.Failure;
  }
  try {
    // 0600：临时密钥只允许当前用户读取；宿主读完后立即删除
    writeFileSync(secretFile, secret, { encoding: "utf8", mode: 0o600, flag: "w" });
  } catch (err) {
    process.stderr.write(`mcsup-plugin setup: cannot write secret file: ${String(err)}\n`);
    return ExitCode.Failure;
  }
  return ExitCode.Ok;
}