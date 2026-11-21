/**
 * Minecraft 专用工具（纯函数）
 *
 * 聊天桥、日志机器人等插件几乎都要处理同一批问题：颜色码、
 * 聊天 sanitize、玩家名与命令注入。这里集中实现，避免各插件各写一套。
 */

import { truncate } from "./general.js";

/** SECTION 符号（Minecraft 颜色码前缀） */
const SECTION = "\u00a7";
/** 颜色 / 格式码字符集：0-9 颜色，a-f 颜色，k-o 格式，r 重置 */
const CODE_CHARS = "0123456789abcdefklmnorABCDEFKLMNOR";

/** 去掉所有 `§x` 颜色 / 格式码，返回纯文本（存库、比对、发平台前用）。 */
export function stripColorCodes(text: string): string {
  let out = "";
  for (let i = 0; i < text.length; i += 1) {
    const next = text.charAt(i + 1);
    if (text[i] === SECTION && next !== "" && CODE_CHARS.includes(next)) {
      i += 1;
      continue;
    }
    out += text[i];
  }
  return out;
}

/** 把常见的 `&x` 写法转换成真正的 `§x`（配置里写 `&a` 比写 `\u00a7a` 方便）。 */
export function legacyColorCodes(text: string): string {
  return text.replace(/&([0-9a-fk-orA-FK-OR])/g, (_m, c: string) => SECTION + c.toLowerCase());
}

/** 控制 / 换行字符（聊天注入的主要载体） */
const CONTROL = /[\u0000-\u001f\u007f]/g;

/**
 * 聊天文本 sanitize：
 * 1. 去掉颜色码
 * 2. 去掉控制字符，换行折叠为空格（防止伪造多行聊天 / 控制台注入）
 * 3. 按码点截断到 `max`（Minecraft 聊天上限 256）
 */
export function sanitizeChat(text: string, max = 256): string {
  const stripped = stripColorCodes(text).replace(CONTROL, " ");
  const collapsed = stripped.replace(/ {2,}/g, " ").trim();
  // 与 truncate 共用码点截断；max <= 0 表示硬截断为空
  if (max <= 0) {
    return "";
  }
  return truncate(collapsed, max, "");
}

/** 玩家名：`[A-Za-z0-9_]`，1..16 字符（Java 版规则） */
const PLAYER_NAME = /^[A-Za-z0-9_]{1,16}$/;

/** 校验玩家名；不合法的输入（含 `§`、空格、注入片段）直接拒绝。 */
export function isPlayerName(name: string): boolean {
  return PLAYER_NAME.test(name);
}

/**
 * 构造一条发往控制台的命令行（`console:write.sendLine` 用）。
 *
 * - `command` 必须以 `/` 开头（不带也自动补）；只允许命令自身的合法字符
 * - `args` 逐个拼接：带空格的参数自动加双引号并转义内部 `"` 与 `\`
 * - 参数中的换行 / 控制字符直接抛错，不做静默折叠
 *
 * 这不是沙箱边界（宿主按能力裁权），只是防止拼参数时的常见错误。
 */
export function buildCommand(command: string, args: readonly string[] = []): string {
  const trimmed = command.trim();
  // 换行 / 控制字符可以在命令本体里伪造第二行，必须在形状校验之前拒绝
  if (/[\r\n\u0000-\u001f\u007f]/.test(trimmed)) {
    throw new TypeError("command must not contain control characters or newlines");
  }
  if (!/^[a-zA-Z][a-zA-Z0-9_.:-]*( .*)?$/.test(trimmed.replace(/^\//, ""))) {
    throw new TypeError(`invalid command: ${JSON.stringify(command)}`);
  }
  const parts = [trimmed.startsWith("/") ? trimmed : `/${trimmed}`];
  for (const arg of args) {
    if (/[\r\n\u0000-\u001f]/.test(arg)) {
      throw new TypeError("command args must not contain control characters or newlines");
    }
    if (arg === "") {
      parts.push('""');
    } else if (/[\s"]/.test(arg)) {
      parts.push(`"${arg.replace(/(["\\])/g, "\\$1")}"`);
    } else {
      parts.push(arg);
    }
  }
  return parts.join(" ");
}
