/**
 * 内核层：PluginDefinition（plugins.md 附录 D，sdk-design.md §3 / §4.1）
 *
 * 一个 `definePlugin({...})` 调用就是一个插件。SDK 内部只有这一种插件表示；
 * Builder 与 Hooks 的产物最终都回填进这里，manifest 与 descriptor 由它生成。
 */

import { z } from "./zod.js";

import type { OpenCapabilityName } from "./contracts/v1.js";
import type { CtxFor, EventMeta } from "./ctx.js";
import type { InferSchema, SchemaLike } from "./schema.js";
import { PLUGIN_API_VERSION } from "./protocol.js";

/** 能力名（plugins.md §8.2）：来自生成契约，未开放项不在列表内 */
export type Capability = OpenCapabilityName;

/**
 * 能力驱动的插件上下文：`ctx` 上只出现已声明能力对应的方法组。
 *
 * 未声明 `console:read` 时 `ctx.console` 在类型上不存在，越权调用在编译期
 * 报错（sdk-design.md §6.1）。`config` 的类型由 `config` 字段的 schema 推导。
 */
export type PluginContext<
  C extends readonly Capability[] = readonly Capability[],
  Cfg extends SchemaLike = z.ZodType,
> = CtxFor<C, Cfg>;

/** 流式 op 的分块产出：同步或异步迭代器均可 */
export type StreamChunks<T> = Iterable<T> | AsyncIterable<T>;

/** op 种类（plugins.md §11.2） */
export type OpKind = "query" | "action" | "stream";

/** 风险等级（plugins.md §8.3） */
export type OpRisk = "read" | "mutating" | "destructive";

/** 允许 Builder / 原生对象两种写法的结构子集 */
export type OpSpecLike = Partial<ResolvedOp>;

/** op 处理函数签名 */
export type OpRun = (input: unknown, ctx: PluginContext) => unknown;

/** 归约后的 op 描述（内核内部表示） */
export interface ResolvedOp {
  readonly id: string;
  readonly kind: OpKind;
  readonly title: string;
  readonly description?: string;
  readonly risk: OpRisk;
  readonly display?: unknown;
  readonly input: z.ZodType;
  readonly output: z.ZodType;
  readonly timeoutMs?: number;
  readonly pagination?: unknown;
  readonly run?: OpRun;
}

/** op 片段：原生对象或 Builder 产物（`OpSpecLike` 加必填 schema） */
export type OpSpec<I extends SchemaLike = SchemaLike, O extends SchemaLike = SchemaLike> = OpSpecLike & {
  readonly input: I;
  readonly output: O;
  readonly kind: OpKind;
};

/** 事件处理函数 */
export type EventHandler<C extends readonly Capability[], Cfg extends SchemaLike = z.ZodType> = (
  data: unknown,
  ctx: PluginContext<C, Cfg>,
  meta: EventMeta,
) => void | Promise<void>;

/** 事件订阅声明：`kind` 或 `{kind, handler}` */
export type Subscription<
  C extends readonly Capability[] = readonly Capability[],
  Cfg extends SchemaLike = z.ZodType,
> = string | readonly string[] | { readonly kind: string; readonly handler: EventHandler<C, Cfg> };

/** 安装向导字段：宿主在授权后逐项询问，写入配置 */
export interface SetupFieldSpec {
  /** 配置路径，点号分隔（如 `qq.appId`） */
  readonly path: string;
  /** 是否必填；必填项不允许留空 */
  readonly required?: boolean;
  /** 输入时遮蔽；只能用于字符串字段 */
  readonly secret?: boolean;
  /** 覆盖 schema 的 `x-mcsup-label` */
  readonly label?: string;
  /** 覆盖 schema 的 description */
  readonly description?: string;
  /** 提示默认值；省略时取 schema 的 default */
  readonly default?: unknown;
  /** 列表字段的可选值；声明后向导渲染成多选框（仅数组字符串字段） */
  readonly choices?: readonly string[];
}

/** 安装向导：声明一次「装完即用」需要问哪些值 */
export interface SetupSpec {
  /** 配置写完是否自动启用并启动；默认 true */
  readonly start?: boolean;
  /** 按顺序提示的字段 */
  readonly fields: readonly SetupFieldSpec[];
}

/** 插件定义（`definePlugin` 的参数） */
export interface PluginDefinition<
  C extends readonly Capability[] = readonly Capability[],
  Id extends string = string,
  Cfg extends SchemaLike = z.ZodType,
> {
  /** 全局唯一 id，`^[a-z][a-z0-9-]{1,63}$` */
  readonly id: Id;
  /** 插件 API 主版本；省略时取当前 SDK 版本，写错值在装配时报错 */
  readonly api?: number;
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
  readonly subscriptions?: readonly Subscription<C, Cfg>[];
  /** `net:outbound` 的域名白名单 */
  readonly net?: { readonly allow: readonly string[] };
  readonly provides?: readonly string[];
  readonly consumes?: readonly string[];
  readonly publisher?: { readonly name: string; readonly url?: string };
  /** 配置 schema；热更新用同一 schema 校验 */
  readonly config: Cfg;
  /** 安装向导；声明后宿主在安装时补齐配置并自动启动 */
  readonly setup?: SetupSpec;
  readonly queries?: Readonly<Record<string, OpSpec>>;
  readonly actions?: Readonly<Record<string, OpSpec>>;
  readonly streams?: Readonly<Record<string, OpSpec>>;
  /** 握手完成、配置就绪后调用 */
  readonly onStart?: (ctx: PluginContext<C, Cfg>) => void | Promise<void>;
  /** 收到 shutdown 时调用；超时由宿主裁定 */
  readonly onStop?: (ctx: PluginContext<C, Cfg>) => void | Promise<void>;
  /** 配置变更；`config` 已按同一 schema 校验，返回后 SDK 回 result */
  readonly onConfigChange?: (config: InferSchema<Cfg>, ctx: PluginContext<C, Cfg>) => void | Promise<void>;
  /** 事件投递；未在 `subscriptions` 里写 handler 时回落于此 */
  readonly onEvent?: EventHandler<C, Cfg>;
}

/** `definePlugin` 的返回值：原定义 + 归约出的 op 列表 */
export interface DefinedPlugin<
  C extends readonly Capability[] = readonly Capability[],
  Id extends string = string,
  Cfg extends SchemaLike = z.ZodType,
> {
  readonly definition: PluginDefinition<C, Id, Cfg>;
}

/**
 * 定义一个插件。
 *
 * 只做静态归约：记录定义、归一化 op 列表，不产生注册副作用、不触达 IO。
 *
 * @throws {Error} 能力未声明却使用了对应字段（如订阅事件但无 `events`）、
 *   `api` 与当前插件 API 主版本不一致
 */
export function definePlugin<const C extends readonly Capability[], Id extends string, Cfg extends SchemaLike>(
  def: PluginDefinition<C, Id, Cfg>,
): DefinedPlugin<C, Id, Cfg> {
  if (def.api !== undefined && def.api !== PLUGIN_API_VERSION) {
    throw new Error(`plugin api ${def.api} does not match SDK api ${PLUGIN_API_VERSION}`);
  }
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

/** 订阅声明的结构子集：只读 `kind`，不关心 handler 的能力形状 */
export type SubscriptionShape = string | readonly string[] | { readonly kind: string };

/** 从订阅声明里取事件名（归约用） */
export function subscriptionKinds(subscriptions: readonly SubscriptionShape[] | undefined): readonly string[] {
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
export function subscriptionHandlers<C extends readonly Capability[], Cfg extends SchemaLike>(
  subscriptions: readonly Subscription<C, Cfg>[] | undefined,
): readonly { readonly kind: string; readonly handler: EventHandler<C, Cfg> }[] {
  if (subscriptions === undefined) {
    return [];
  }
  return subscriptions.flatMap((item) =>
    typeof item === "string" || Array.isArray(item)
      ? []
      : [(item as { kind: string; handler: EventHandler<C, Cfg> })],
  );
}
