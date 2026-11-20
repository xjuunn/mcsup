/**
 * zod 重新导出（peer dependency）
 *
 * 插件作者只从 `mcsup-plugin` 取 `z`，保证 SDK 与插件使用同一个 zod 实例
 * （schema 才有统一行为与 descriptor 生成结果）。`package.json` 里 zod 是
 * peer，插件必须自备一份；版本不匹配时 npm 会报 peer 冲突。
 */

export { z } from "zod";