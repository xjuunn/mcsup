/**
 * 统一错误类型
 *
 * 插件侧所有失败（宿主能力调用、op 处理、配置校验）都表达为
 * {@link PluginError}；`code` / `category` / `retryable` 与宿主错误码表
 * 一一对应（plugins.md §6.4），作者据此决定是否重试，不需要解析文本。
 */

import {
  ERROR_CATEGORY,
  RETRYABLE_ERRORS,
  type ErrorCategory,
  type ErrorCode,
} from "./contracts/v1.js";

/** 宿主错误码，直接来自生成契约 */
export type PluginErrorCode = ErrorCode;

/** 线上分类文本，直接来自生成契约 */
export type PluginErrorCategory = ErrorCategory;

/** 错误码 → 分类；单表维护在契约生成物中 */
export function categoryOf(code: PluginErrorCode): PluginErrorCategory {
  return ERROR_CATEGORY[code];
}

/** 错误码是否可重试；单表维护在契约生成物中 */
export function isRetryable(code: PluginErrorCode): boolean {
  return RETRYABLE_ERRORS.includes(code);
}

/** 插件侧统一错误 */
export class PluginError extends Error {
  public readonly code: PluginErrorCode;
  public readonly category: PluginErrorCategory;
  public readonly retryable: boolean;

  public constructor(code: PluginErrorCode, message: string) {
    super(message);
    this.name = "PluginError";
    this.code = code;
    this.category = categoryOf(code);
    this.retryable = isRetryable(code);
  }

  /** 便捷构造：输入不合法 */
  public static invalidInput(message: string): PluginError {
    return new PluginError("invalid_input", message);
  }

  /** 便捷构造：资源不存在 */
  public static notFound(message: string): PluginError {
    return new PluginError("not_found", message);
  }

  /** 便捷构造：能力未授予或越权 */
  public static denied(message: string): PluginError {
    return new PluginError("denied", message);
  }

  /** 便捷构造：状态不允许 */
  public static conflict(message: string): PluginError {
    return new PluginError("conflict", message);
  }

  /** 便捷构造：内部错误 */
  public static internal(message: string): PluginError {
    return new PluginError("internal", message);
  }
}

/** 把任意抛出值归一成 PluginError（internal 兜底） */
export function toPluginError(err: unknown): PluginError {
  if (err instanceof PluginError) {
    return err;
  }
  const message = err instanceof Error ? err.message : String(err);
  return new PluginError("internal", message);
}
