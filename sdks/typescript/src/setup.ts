/**
 * 交互式 setup 字段（descriptor schema 5）
 *
 * 宿主在安装向导里遇到 `interactive: true` 的字段时不直接提问，而是以子
 * 进程重新拉起插件入口，并设置以下环境变量：
 *
 *   MCSUP_SETUP_FIELD        配置路径（descriptor 里声明的 `path`）
 *   MCSUP_SETUP_SECRET_FILE  宿主准备的临时文件（0600）
 *   MCSUP_SETUP_FIELDS       本次允许回填的全部交互式路径（JSON 数组，可省）
 *   MCSUP_SETUP_UI_FILE      界面事件文件（可省）：按行追加 JSON，宿主实时渲染
 *
 * 插件此时不连接宿主、不建会话，只执行 `setup.interactive[path]`，把返回的
 * 密钥写入该文件后退出。子进程继承终端，所以二维码、设备授权这类平台特定
 * 流程可以照常显示。密钥只经文件传递：不进 argv，也不回显到 stdout。
 *
 * UI 事件文件供插件异步描述界面，宿主负责排版：
 *
 *   {"type":"qr","content":"https://…","label":"用手机微信扫码"}
 *   {"type":"status","text":"已扫码，等待手机确认"}
 *
 * 宿主没有设置 `MCSUP_SETUP_UI_FILE` 时，插件应退回自己绘制二维码，保持与
 * 旧宿主可用。
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
  const parsed = parseAllowedFields(env["MCSUP_SETUP_FIELDS"], field);
  if (parsed instanceof Error) {
    process.stderr.write(`mcsup-plugin setup: ${parsed.message}\n`);
    return ExitCode.Protocol;
  }
  const allowed = parsed.paths;
  const handler = def.setup?.interactive?.[field];
  if (handler === undefined) {
    process.stderr.write(`mcsup-plugin setup: field ${field} has no interactive handler\n`);
    return ExitCode.Failure;
  }
  let result: unknown;
  try {
    result = await handler({ field, fields: allowed });
  } catch (err) {
    process.stderr.write(`mcsup-plugin setup: field ${field} failed: ${String(err)}\n`);
    return ExitCode.Failure;
  }
  const payload = toPayload(result, field, allowed, parsed.legacy);
  if (payload instanceof Error) {
    process.stderr.write(`mcsup-plugin setup: ${payload.message}\n`);
    return ExitCode.Failure;
  }
  try {
    // 0600：临时密钥只允许当前用户读取；宿主读完后立即删除
    writeFileSync(secretFile, payload, { encoding: "utf8", mode: 0o600, flag: "w" });
  } catch (err) {
    process.stderr.write(`mcsup-plugin setup: cannot write secret file: ${String(err)}\n`);
    return ExitCode.Failure;
  }
  return ExitCode.Ok;
}

/**
 * 解析宿主允许回填的路径列表
 *
 * 旧宿主不设置该变量：只允许当前字段，并保持单字符串写盘格式。
 */
function parseAllowedFields(
  raw: string | undefined,
  field: string,
): { readonly paths: readonly string[]; readonly legacy: boolean } | Error {
  const text = raw?.trim();
  if (text === undefined || text === "") {
    return { paths: [field], legacy: true };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch (err) {
    return new Error(`MCSUP_SETUP_FIELDS is not valid JSON: ${String(err)}`);
  }
  if (!Array.isArray(parsed) || parsed.length === 0) {
    return new Error("MCSUP_SETUP_FIELDS must be a non-empty JSON array");
  }
  const paths = parsed.filter((item): item is string => typeof item === "string");
  if (paths.length !== parsed.length) {
    return new Error("MCSUP_SETUP_FIELDS must contain only strings");
  }
  if (!paths.includes(field)) {
    return new Error(`MCSUP_SETUP_FIELDS does not include the requested field ${field}`);
  }
  return { paths, legacy: false };
}

/**
 * 校验处理函数的返回值并序列化成密钥文件内容
 *
 * 字符串保持原样（旧宿主只认这一种）；对象写 JSON，供新宿主一次回填多个
 * 配置项。旧宿主（没有 `MCSUP_SETUP_FIELDS`）只取当前字段的值。
 */
function toPayload(
  result: unknown,
  field: string,
  allowed: readonly string[],
  legacy: boolean,
): string | Error {
  if (typeof result === "string") {
    const text = result.trim();
    if (text === "") {
      return new Error(`field ${field} returned an empty value`);
    }
    return text;
  }
  if (result === null || typeof result !== "object" || Array.isArray(result)) {
    return new Error(`field ${field} must return a string or an object of strings`);
  }
  const entries = Object.entries(result as Record<string, unknown>);
  if (entries.length === 0) {
    return new Error(`field ${field} returned an empty object`);
  }
  // 旧宿主不理解 JSON 对象：只交出当前字段，其余键忽略
  if (legacy) {
    const own = (result as Record<string, unknown>)[field];
    if (typeof own !== "string" || own.trim() === "") {
      return new Error(`field ${field} did not return its own value`);
    }
    return own.trim();
  }
  const values: Record<string, string> = {};
  for (const [path, value] of entries) {
    if (!allowed.includes(path)) {
      return new Error(`field ${field} returned undeclared path ${path}`);
    }
    if (typeof value !== "string" || value.trim() === "") {
      return new Error(`field ${field} returned an empty or non-string value for ${path}`);
    }
    values[path] = value.trim();
  }
  if (!(field in values)) {
    return new Error(`field ${field} did not return its own value`);
  }
  return JSON.stringify(values);
}
