/**
 * 内核层：PluginDefinition（plugins.md 附录 D，sdk-design.md §3 / §4.1）
 *
 * 一个 `definePlugin({...})` 调用就是一个插件。SDK 内部只有这一种插件表示；
 * Builder 与 Hooks 的产物最终都回填进这里，manifest 与 descriptor 由它生成。
 */

import { z } from "zod";

import type { OpenCapabilityName } from "./contracts/v1.js";
import type { OpSpecLike, ResolvedOp } from "./descriptor.js";
import type { CtxFor } from "./ctx.js";

/** 能力名（plugins.md §8.2）：来自生成契约，未开放项不在列表内 */
export type Capability = OpenCapabilityName;

/**
 * 能力驱动的插件上下文：`ctx` 上只出现已声明能力对应的方法组。
 *
 * 未声明 `console:read` 时 `ctx.console` 在类型上不存在，越权调用在编译期
 * 报错（sdk-design.md §6.1）。
 */
export type PluginContext<C extends readonly Capability[] = readonly Capability[]> = CtxFor<C>;

/** 流式 op 的分块产出：同步或异步迭代器均可 */
export type StreamChunks<T> = Iterable<T> | AsyncIterable<T>;

/** op 片段：原生对象或 Builder 产物（descriptor.ts `OpSpecLike`） */
export type OpSpec<I extends z.ZodType = z.ZodType, O extends z.ZodType = z.ZodType> = OpSpecLike & {
  readonly input: I;
  readonly output: O;
  readonly kind: "query" | "action" | "stream";
};

/** 事件处理函数 */
export type EventHandler<C extends readonly Capability[]> = (
  data: unknown,
  ctx: PluginContext<C>,
  meta: { readonly kind: string; readonly server?: string; readonly at: string },
) => void | Promise<void>;

/** 事件订阅声明：`kind` 或 `{kind, handler}` */
export type Subscription = string | readonly string[] | { readonly kind: string; readonly handler: EventHandler<never> };

/** 插件定义（`definePlugin` 的参数） */
export interface PluginDefinition<
  C extends readonly Capability[] = readonly Capability[],
  Id extends string = string,
> {
  /** 全局唯一 id，`^[a-z][a-z0-9-]{1,63}$` */
  readonly id: Id;
  /** 展示名，≤ 64 字符 */
  readonly title: string;
  /** SemVer，与产物目录一致 */
  readonly version: string;
  /** SPDX 标识 */
  readonly license: string;
  readonly description?: string;
  /** `machine`（默认）/ `server` */
  readonly scope?: "machine" | "server";
  /** 常驻进程；默认惰性启动 */
  readonly resident?: boolean;
  /** 申请的能力；同时决定 `ctx` 的形状 */
  readonly capabilities: C;
  /** 订阅的领域事件；要求声明 `events` 能力 */
  readonly subscriptions?: readonly Subscription[];
  /** `net:outbound` 的域名白名单 */
  readonly net?: { readonly allow: readonly string[] };
  readonly provides?: readonly string[];
  readonly consumes?: readonly string[];
  readonly publisher?: { readonly name: string; readonly url?: string };
  /** 配置 schema（zod），热更新用同一 schema 校验 */
  readonly config: z.ZodType;
  readonly queries?: Readonly<Record<string, OpSpec>>;
  readonly actions?: Readonly<Record<string, OpSpec>>;
  readonly streams?: Readonly<Record<string, OpSpec>>;
  /** 握手完成、配置就绪后调用 */
  readonly onStart?: (ctx: PluginContext<C>) => void | Promise<void>;
  /** 收到 shutdown 时调用；超时由宿主裁定 */
  readonly onStop?: (ctx: PluginContext<C>) => void | Promise<void>;
  /** 配置变更；返回后 SDK 回 result */
  readonly onConfigChange?: (config: unknown, ctx: PluginContext<C>) => void | Promise<void>;
  /** 事件投递；未在 `subscriptions` 里写 handler 时回落于此 */
  readonly onEvent?: EventHandler<C>;
}

/** `definePlugin` 的返回值：原定义 + 归约出的 op 列表 */
export interface DefinedPlugin<
  C extends readonly Capability[] = readonly Capability[],
  Id extends string = string,
> {
  readonly definition: PluginDefinition<C, Id>;
}

/**
 * 定义一个插件。
 *
 * 只做静态归约：记录定义、归一化 op 列表，不产生注册副作用、不触达 IO。
 *
 * @throws {Error} 能力未声明却使用了对应字段（如订阅事件但无 `events`）
 */
export function definePlugin<const C extends readonly Capability[], Id extends string>(
  def: PluginDefinition<C, Id>,
): DefinedPlugin<C, Id> {
  if (def.subscriptions !== undefined && !(def.capabilities as readonly string[]).includes("events")) {
    throw new Error("subscriptions require the events capability");
  }
  if (def.net?.allow !== undefined && !(def.capabilities as readonly string[]).includes("net:outbound")) {
    throw new Error("net.allow requires the net:outbound capability");
  }
  for (const group of [def.queries, def.actions, def.streams] as const) {
    if (group === undefined) {
      continue;
    }
    for (const [id, spec] of Object.entries(group)) {
      if (spec === undefined) {
        throw new Error(`op ${id} is undefined`);
      }
    }
  }
  return { definition: def };
}

/** 从订阅声明里取事件名（归约用） */
export function subscriptionKinds(subscriptions: readonly Subscription[] | undefined): readonly string[] {
  if (subscriptions === undefined) {
    return [];
  }
  const kinds: string[] = [];
  for (const item of subscriptions) {
    if (typeof item === "string") {
      kinds.push(item);
    } else if (Array.isArray(item)) {
      kinds.push(...(item as readonly string[]));
    } else {
      kinds.push((item as { kind: string }).kind);
    }
  }
  return kinds;
}

/** 从订阅声明里取 handler（`onEvent` 兜底） */
export function subscriptionHandlers(
  subscriptions: readonly Subscription[] | undefined,
): readonly { readonly kind: string; readonly handler: EventHandler<never> }[] {
  if (subscriptions === undefined) {
    return [];
  }
  return subscriptions.flatMap((item) =>
    typeof item === "string" || Array.isArray(item) ? [] : [item as { kind: string; handler: EventHandler<never> }],
  );
}

/** op 运行期失败统一映射：op 抛出的任意值由运行时归一为 PluginError */
export type OpFailure = unknown;

/** 归约后的 op（对外可见的只读视图） */
export type PluginOp = ResolvedOp;