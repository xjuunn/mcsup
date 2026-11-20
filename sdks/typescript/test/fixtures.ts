/** 测试与 conformance 共用的插件定义 */
import { action, definePlugin, lineFilter, query, stream, z } from "../src/index.js";

export const config = z.object({
  emitChat: z.boolean().default(true),
  channels: z.array(z.string()).default(["global"]),
  token: z.string().min(1).meta({ secret: true, label: "Bot Token", format: "plain" }),
});

export const counter = query("counter")
  .title("发送计数")
  .input(z.object({ channel: z.string().default("global") }))
  .output(z.object({ count: z.number().int(), channel: z.string() }))
  .run((input) => ({ count: counters.get(input.channel) ?? 0, channel: input.channel }));

const counters = new Map<string, number>();

export const send = action("send")
  .title("发送消息")
  .risk("mutating")
  .input(z.object({ channel: z.string().min(1), text: z.string().min(1).max(256) }))
  .output(z.object({ ok: z.literal(true), messageId: z.string() }))
  .run((input, ctx) => {
    counters.set(input.channel, (counters.get(input.channel) ?? 0) + 1);
    ctx.log.info(`send ${input.channel}: ${input.text.length} chars`);
    return { ok: true as const, messageId: "m-1" };
  });

export const messages = stream("messages")
  .title("实时消息")
  .input(z.object({ prefix: z.string().default("") }))
  .output(z.object({ delta: z.string() }))
  .run(function* (input) {
    for (const part of ["你", "好", ""]) {
      yield { delta: `${input.prefix}${part}` };
    }
  });

export const failing = action("failing")
  .title("总是失败")
  .run(() => {
    throw new Error("boom");
  });

export const slow = stream("slow")
  .title("慢流")
  .run(async function* () {
    for (let i = 0; i < 100; i += 1) {
      await new Promise((resolve) => setTimeout(resolve, 20));
      yield { delta: String(i) };
    }
  });

export const chatBridge = definePlugin({
  id: "chat-bridge",
  title: "聊天桥",
  version: "0.1.0",
  license: "MIT",
  description: "把服务器聊天转发到外部渠道",
  capabilities: ["console:read", "console:write", "storage:kv", "events"],
  subscriptions: ["chat:message"],
  config,
  queries: { counter },
  actions: { send, failing },
  streams: { messages, slow },
  onEvent(data, ctx) {
    void data;
    ctx.log.debug("event");
  },
});

export const minimal = definePlugin({
  id: "minimal",
  title: "最小插件",
  version: "1.0.0",
  license: "MIT",
  capabilities: ["server:read"],
  config: z.object({}),
  queries: {
    servers: query("servers").input(z.object({})).output(z.array(z.string())).run(() => []),
  },
});

export { lineFilter };
