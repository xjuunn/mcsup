/**
 * 统一错误类型
 *
 * 插件侧所有失败（宿主能力调用、op 处理、配置校验）都表达为
 * {@link PluginError}；`code` / `category` / `retryable` 与宿主错误码表
 * 一一对应（plugins.md §6.4），作者据此决定是否重试，不需要解析文本。
 */

/** 宿主错误码，与 `domain::plugin::error::PluginErrorCode` 一致 */
export type PluginErrorCode =
  | "invalid_input"
  | "not_found"
  | "denied"
  | "conflict"
  | "timeout"
  | "cancelled"
  | "unavailable"
  | "crashed"
  | "limit_exceeded"
  | "unsupported"
  | "internal";

/** 线上分类文本，与 `category_of` 一致 */
export type PluginErrorCategory =
  | "usage"
  | "not_found"
  | "unauthorized"
  | "conflict"
  | "cancelled"
  | "backend"
  | "failure";

/** 错误码 → 分类；与宿主 `category_of` 保持一致 */
export function categoryOf(code: PluginErrorCode): PluginErrorCategory {
  switch (code) {
    case "invalid_input":
    case "limit_exceeded":
    case "unsupported":
      return "usage";
    case "not_found":
      return "not_found";
    case "denied":
      return "unauthorized";
    case "conflict":
      return "conflict";
    case "cancelled":
      return "cancelled";
    case "timeout":
    case "unavailable":
    case "crashed":
      return "backend";
    default:
      return "failure";
  }
}

/** 错误码是否可重试；与宿主 `retryable()` 保持一致 */
export function isRetryable(code: PluginErrorCode): boolean {
  return code === "timeout" || code === "unavailable" || code === "crashed";
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
