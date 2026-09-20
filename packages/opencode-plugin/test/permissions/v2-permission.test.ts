import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import { Effect } from "effect";
import {
  decidePermission,
  PermissionDeniedError,
  PermissionPromptUnavailableError,
  PermissionRejectedError,
  PermissionRulesUnavailableError,
  requestPermission,
  type V2PermissionClient,
  type V2PermissionCreateInput,
  type V2PermissionEffect,
  type V2PermissionHostContext,
  type V2PermissionReply,
  type V2PermissionRule,
} from "../../src/permissions/v2.js";
import {
  createV2PromptChannel,
  type V2PromptChannel,
  type V2PromptChannelResult,
} from "../../src/permissions/v2-service.js";
import { projectV2Tool, type V2ProviderTool } from "../../src/tools/definitions/v2.js";
import { hoistedV2ToolConsumers } from "../../src/tools/hoisted/v2.js";
import { hoistedTools } from "../../src/tools/hoisted.js";
import type { PluginContext } from "../../src/types.js";

const PROJECT_ROOT = resolve(import.meta.dir, "../../../..");
const LOCATION = {
  directory: PROJECT_ROOT,
  project: { directory: PROJECT_ROOT, canonical: PROJECT_ROOT },
};
const EXECUTION_CONTEXT = {
  sessionID: "session-v2",
  messageID: "message-v2",
  id: "call-v2",
  agent: "agent-v2",
  progress: () => Effect.succeed(undefined),
};
const REQUEST = {
  permission: "edit",
  patterns: ["file.ts"],
  always: ["file.ts"],
  metadata: { filepath: "/work/project/file.ts", diff: "@@ -1 +1 @@" },
};

/**
 * OpenCode's own starting ruleset, copied from `Agent.Info.default` in
 * `@opencode/schema`. Keeping the real thing here is the point of the suite:
 * an unmodified GA install must let ordinary tool calls through.
 */
const OPENCODE_DEFAULT_RULES: readonly V2PermissionRule[] = [
  { action: "*", resource: "*", effect: "allow" },
  { action: "external_directory", resource: "*", effect: "ask" },
  { action: "read", resource: "*.env", effect: "ask" },
  { action: "read", resource: "*.env.*", effect: "ask" },
  { action: "read", resource: "*.env.example", effect: "allow" },
];

type HostCalls = {
  readonly agents: string[];
  readonly sessions: string[];
};

function permissionHost(
  agentRules: readonly V2PermissionRule[] | undefined,
  options: {
    sessionRules?: readonly V2PermissionRule[];
    sessionAgent?: string;
    agentFailure?: string;
    sessionFailure?: string;
  } = {},
): { host: V2PermissionHostContext; calls: HostCalls } {
  const calls: HostCalls = { agents: [], sessions: [] };
  const host: V2PermissionHostContext = {
    agent: {
      get: ({ agentID }) =>
        Effect.suspend(() => {
          calls.agents.push(agentID);
          if (options.agentFailure) return Effect.fail(new Error(options.agentFailure));
          return Effect.succeed({
            data: agentRules === undefined ? {} : { permissions: agentRules },
          });
        }),
    },
    session: {
      get: ({ sessionID }) =>
        Effect.suspend(() => {
          calls.sessions.push(sessionID);
          if (options.sessionFailure) return Effect.fail(new Error(options.sessionFailure));
          return Effect.succeed({
            ...(options.sessionAgent === undefined ? {} : { agent: options.sessionAgent }),
            ...(options.sessionRules === undefined ? {} : { permissions: options.sessionRules }),
          });
        }),
    },
  };
  return { host, calls };
}

/**
 * A pushable stand-in for the service's event stream.
 *
 * The prompt code holds this open across `permission.create`, so the test has
 * to be able to deliver a reply after the request exists, and to observe that
 * the iterator was closed again afterwards.
 */
function eventFeed() {
  const queued: unknown[] = [];
  let pending: ((result: IteratorResult<unknown>) => void) | undefined;
  let closed = false;

  return {
    push(event: unknown): void {
      if (pending) {
        const deliver = pending;
        pending = undefined;
        deliver({ value: event, done: false });
        return;
      }
      queued.push(event);
    },
    get closed(): boolean {
      return closed;
    },
    stream: {
      [Symbol.asyncIterator]: () => ({
        next(): Promise<IteratorResult<unknown>> {
          if (queued.length > 0) {
            return Promise.resolve({ value: queued.shift(), done: false });
          }
          return new Promise<IteratorResult<unknown>>((deliver) => {
            pending = deliver;
          });
        },
        return(): Promise<IteratorResult<unknown>> {
          closed = true;
          if (pending) {
            const deliver = pending;
            pending = undefined;
            deliver({ value: undefined, done: true });
          }
          return Promise.resolve({ value: undefined, done: true });
        },
      }),
    } as AsyncIterable<unknown>,
  };
}

function repliedEvent(sessionID: string, requestID: string, reply: V2PermissionReply) {
  return {
    id: "evt-1",
    created: 1,
    type: "permission.replied",
    location: { directory: PROJECT_ROOT },
    data: { sessionID, requestID, reply },
  };
}

type PromptService = {
  readonly channel: V2PromptChannel;
  readonly creates: V2PermissionCreateInput[];
  readonly order: string[];
  readonly feed: ReturnType<typeof eventFeed>;
  discoveries(): number;
};

function promptService(effects: readonly V2PermissionEffect[] = ["ask"]): PromptService {
  const creates: V2PermissionCreateInput[] = [];
  const order: string[] = [];
  const feed = eventFeed();
  let discoveries = 0;
  const client: V2PermissionClient = {
    permission: {
      create: async (input) => {
        order.push("create");
        creates.push(input);
        return { id: `per-${creates.length}`, effect: effects[creates.length - 1] ?? "ask" };
      },
    },
    event: {
      subscribe: async () => {
        order.push("subscribe");
        return { stream: feed.stream };
      },
    },
  };
  const channel = createV2PromptChannel(async () => {
    discoveries += 1;
    return { client };
  });
  return { channel, creates, order, feed, discoveries: () => discoveries };
}

function unreachableService(
  observed = "no healthy, compatible local OpenCode service is registered",
) {
  let discoveries = 0;
  const channel = createV2PromptChannel(async (): Promise<V2PromptChannelResult> => {
    discoveries += 1;
    return { unavailable: observed };
  });
  return { channel, discoveries: () => discoveries };
}

function pluginContext(
  bridgeResponse: (name: string, preview: boolean) => Record<string, unknown>,
): PluginContext {
  const bridge = {
    toolCall: async (
      _sessionID: string | undefined,
      name: string,
      _args: Record<string, unknown>,
      options?: { preview?: boolean },
    ) => bridgeResponse(name, options?.preview === true),
  };
  return {
    pool: { getBridge: () => bridge } as unknown as BridgePool,
    client: {},
    config: { tool_surface: "all", hoist_builtin_tools: true },
    hashlineEffective: false,
    storageDir: "/isolated/storage",
  } as PluginContext;
}

function projectedFilesystemTools(
  host: V2PermissionHostContext,
  bridgeResponse: (name: string, preview: boolean) => Record<string, unknown>,
) {
  const definitions = hoistedTools(pluginContext(bridgeResponse));
  const consumers = hoistedV2ToolConsumers(host, unreachableService().channel);
  const project = (name: keyof typeof definitions) =>
    projectV2Tool(name, definitions[name], LOCATION, consumers);
  return {
    read: project("read"),
    write: project("write"),
    edit: project("edit"),
    aft_delete: project("aft_delete"),
    aft_move: project("aft_move"),
  };
}

async function execute(tool: V2ProviderTool, input: Record<string, unknown>) {
  return await Effect.runPromise(tool.execute(input, EXECUTION_CONTEXT));
}

describe("OpenCode V2 permission evaluation", () => {
  test("allows a request the host's configured rules already allow", async () => {
    const { host, calls } = permissionHost(OPENCODE_DEFAULT_RULES);
    const service = promptService();

    await requestPermission(host, REQUEST, EXECUTION_CONTEXT, service.channel);

    expect(calls.agents).toEqual(["agent-v2"]);
    expect(calls.sessions).toEqual(["session-v2"]);
    // An allowed rule is the host's own answer, so it must cost no round trip.
    expect(service.discoveries()).toBe(0);
    expect(service.creates).toEqual([]);
  });

  test("reads inside the project root need no permission under the default ruleset", async () => {
    const { host } = permissionHost(OPENCODE_DEFAULT_RULES);
    const service = promptService();

    await requestPermission(
      host,
      { permission: "read", patterns: ["src/index.ts"], always: ["*"], metadata: {} },
      EXECUTION_CONTEXT,
      service.channel,
    );

    expect(service.creates).toEqual([]);
  });

  test("keeps the host's own ask rules, including the dotenv carve-outs", async () => {
    const { host } = permissionHost(OPENCODE_DEFAULT_RULES);
    const service = promptService(["allow", "allow"]);
    const read = (resource: string) =>
      requestPermission(
        host,
        { permission: "read", patterns: [resource], always: ["*"], metadata: {} },
        EXECUTION_CONTEXT,
        service.channel,
      );

    await read(".env");
    await read(".env.local");
    await read(".env.example");

    expect(service.creates.map((create) => create.resources)).toEqual([[".env"], [".env.local"]]);
  });

  test("denies when a rule denies and quotes the deciding rule", async () => {
    const { host } = permissionHost([
      { action: "*", resource: "*", effect: "allow" },
      { action: "edit", resource: "file.ts", effect: "deny" },
    ]);
    const service = promptService();

    const denial = await requestPermission(host, REQUEST, EXECUTION_CONTEXT, service.channel).catch(
      (error: unknown) => error as Error,
    );

    expect(denial).toBeInstanceOf(PermissionDeniedError);
    expect(denial.message).toContain('"edit" on "file.ts" is "deny"');
    expect(service.creates).toEqual([]);
  });

  test("lets a session rule override the agent rule that precedes it", async () => {
    const { host } = permissionHost([{ action: "*", resource: "*", effect: "allow" }], {
      sessionRules: [{ action: "edit", resource: "*", effect: "deny" }],
    });

    await expect(
      requestPermission(host, REQUEST, EXECUTION_CONTEXT, promptService().channel),
    ).rejects.toBeInstanceOf(PermissionDeniedError);
  });

  test("falls back to the session's agent when the call names none", async () => {
    const { host, calls } = permissionHost(OPENCODE_DEFAULT_RULES, {
      sessionAgent: "agent-from-session",
    });

    await requestPermission(
      host,
      REQUEST,
      { ...EXECUTION_CONTEXT, agent: undefined },
      promptService().channel,
    );

    expect(calls.agents).toEqual(["agent-from-session"]);
  });

  test("refuses with the real reason when the rules cannot be read", async () => {
    const unreadableAgent = permissionHost(OPENCODE_DEFAULT_RULES, {
      agentFailure: "agent lookup exploded",
    });
    const agentFailure = await requestPermission(
      unreadableAgent.host,
      REQUEST,
      EXECUTION_CONTEXT,
      promptService().channel,
    ).catch((error: unknown) => error as Error);
    expect(agentFailure).toBeInstanceOf(PermissionRulesUnavailableError);
    expect(agentFailure.message).toContain("could not read this session's OpenCode permission");
    expect(agentFailure.message).toContain("agent lookup exploded");

    const namelessAgent = permissionHost(OPENCODE_DEFAULT_RULES);
    await expect(
      requestPermission(
        namelessAgent.host,
        REQUEST,
        { ...EXECUTION_CONTEXT, agent: undefined },
        promptService().channel,
      ),
    ).rejects.toBeInstanceOf(PermissionRulesUnavailableError);

    const rulelessAgent = permissionHost(undefined);
    await expect(
      requestPermission(rulelessAgent.host, REQUEST, EXECUTION_CONTEXT, promptService().channel),
    ).rejects.toBeInstanceOf(PermissionRulesUnavailableError);
  });

  test("matches actions and resources the way the host's wildcard matcher does", () => {
    const rules: readonly V2PermissionRule[] = [
      { action: "*", resource: "*", effect: "allow" },
      { action: "bash", resource: "git push *", effect: "ask" },
    ];
    const ask = (permission: string, resource: string) =>
      decidePermission({ permission, patterns: [resource], always: [], metadata: {} }, rules)
        .effect;

    expect(ask("bash", "git push")).toBe("ask");
    expect(ask("bash", "git push --force origin main")).toBe("ask");
    expect(ask("bash", "git status")).toBe("allow");
  });

  test("denies the whole request when any one resource is denied", () => {
    const decision = decidePermission(
      { permission: "edit", patterns: ["a.ts", "b.ts"], always: [], metadata: {} },
      [
        { action: "*", resource: "*", effect: "allow" },
        { action: "edit", resource: "b.ts", effect: "deny" },
      ],
    );

    expect(decision).toMatchObject({ effect: "deny", resource: "b.ts" });
  });
});

describe("OpenCode V2 permission prompts", () => {
  const askEverything: readonly V2PermissionRule[] = [
    { action: "*", resource: "*", effect: "ask" },
  ];

  test("raises the request on the service and completes when the user allows it once", async () => {
    const { host } = permissionHost(askEverything);
    const service = promptService(["ask"]);

    const pending = requestPermission(host, REQUEST, EXECUTION_CONTEXT, service.channel);
    // The reply can only be sent once the request exists, which is also what
    // makes this a test of the wait rather than of the create response.
    await Promise.resolve();
    await Promise.resolve();
    service.feed.push(repliedEvent("session-v2", "per-1", "once"));

    await pending;

    expect(service.creates).toEqual([
      {
        sessionID: "session-v2",
        action: "edit",
        resources: ["file.ts"],
        save: ["file.ts"],
        metadata: { filepath: "/work/project/file.ts", diff: "@@ -1 +1 @@" },
        source: { type: "tool", messageID: "message-v2", id: "call-v2" },
      },
    ]);
    expect(service.feed.closed).toBe(true);
  });

  test("subscribes to the event stream before creating the request", async () => {
    const { host } = permissionHost(askEverything);
    const service = promptService(["allow"]);

    await requestPermission(host, REQUEST, EXECUTION_CONTEXT, service.channel);

    // A headless client can answer faster than a listener attached afterwards
    // would start, so the order here is the whole point.
    expect(service.order).toEqual(["subscribe", "create"]);
  });

  test("an edit request carries its diff so the host renders its own patch view", async () => {
    const { host } = permissionHost(askEverything);
    const service = promptService(["allow"]);

    await requestPermission(host, REQUEST, EXECUTION_CONTEXT, service.channel);

    expect(service.creates[0]?.metadata).toMatchObject({ diff: "@@ -1 +1 @@" });
  });

  test("an immediate allow settles without waiting for a reply", async () => {
    const { host } = permissionHost(askEverything);
    const service = promptService(["allow"]);

    await requestPermission(host, REQUEST, EXECUTION_CONTEXT, service.channel);

    expect(service.feed.closed).toBe(true);
  });

  test("an immediate deny from the service fails the call", async () => {
    const { host } = permissionHost(askEverything);
    const service = promptService(["deny"]);

    await expect(
      requestPermission(host, REQUEST, EXECUTION_CONTEXT, service.channel),
    ).rejects.toBeInstanceOf(PermissionDeniedError);
    expect(service.feed.closed).toBe(true);
  });

  test("a rejected prompt fails as a user rejection, not a rule denial", async () => {
    const { host } = permissionHost(askEverything);
    const service = promptService(["ask"]);

    const pending = requestPermission(host, REQUEST, EXECUTION_CONTEXT, service.channel);
    await Promise.resolve();
    await Promise.resolve();
    service.feed.push(repliedEvent("session-v2", "per-1", "reject"));

    await expect(pending).rejects.toBeInstanceOf(PermissionRejectedError);
  });

  test("ignores replies that belong to another request or another session", async () => {
    const { host } = permissionHost(askEverything);
    const service = promptService(["ask"]);

    const pending = requestPermission(host, REQUEST, EXECUTION_CONTEXT, service.channel);
    await Promise.resolve();
    await Promise.resolve();
    service.feed.push(repliedEvent("session-v2", "per-other", "reject"));
    service.feed.push(repliedEvent("other-session", "per-1", "reject"));
    service.feed.push({ type: "session.idle", data: {} });
    service.feed.push(repliedEvent("session-v2", "per-1", "always"));

    await pending;
  });

  test("discovers the service once and reuses it for later prompts", async () => {
    const { host } = permissionHost(askEverything);
    const service = promptService(["allow", "allow"]);

    await requestPermission(host, REQUEST, EXECUTION_CONTEXT, service.channel);
    await requestPermission(host, REQUEST, EXECUTION_CONTEXT, service.channel);

    expect(service.creates).toHaveLength(2);
    expect(service.discoveries()).toBe(1);
  });

  test("refuses with what discovery observed when no service can be reached", async () => {
    const { host } = permissionHost(askEverything);
    const unreachable = unreachableService("no healthy, compatible local OpenCode service");

    const refusal = await requestPermission(
      host,
      REQUEST,
      EXECUTION_CONTEXT,
      unreachable.channel,
    ).catch((error: unknown) => error as Error);

    expect(refusal).toBeInstanceOf(PermissionPromptUnavailableError);
    expect(refusal.message).toContain("could not reach the OpenCode service to raise a prompt");
    expect(refusal.message).toContain("no healthy, compatible local OpenCode service");
    expect(refusal.message).toContain('"effect": "allow"');
    // The refusal must report what was observed. These two wordings each
    // claimed something about the host that was not true: the first blamed it
    // for an endpoint AFT never called, the second denied that a plugin can
    // open a prompt at all.
    expect(refusal.message).not.toContain("did not provide a permission request endpoint");
    expect(refusal.message).not.toContain("no way to open a permission prompt");
  });

  test("retries discovery after a failure so a service started later is found", async () => {
    const { host } = permissionHost(askEverything);
    let attempts = 0;
    const client: V2PermissionClient = {
      permission: {
        create: async () => ({ id: "per-1", effect: "allow" as const }),
      },
      event: { subscribe: async () => ({ stream: eventFeed().stream }) },
    };
    const channel = createV2PromptChannel(async (): Promise<V2PromptChannelResult> => {
      attempts += 1;
      return attempts === 1 ? { unavailable: "not up yet" } : { client };
    });

    await expect(
      requestPermission(host, REQUEST, EXECUTION_CONTEXT, channel),
    ).rejects.toBeInstanceOf(PermissionPromptUnavailableError);
    await requestPermission(host, REQUEST, EXECUTION_CONTEXT, channel);

    expect(attempts).toBe(2);
  });
});

describe("OpenCode V2 permission consumer binding", () => {
  test("binds to the GA domains that carry the host's rules", () => {
    const { host } = permissionHost(OPENCODE_DEFAULT_RULES);

    expect(typeof hoistedV2ToolConsumers(host).requestPermission).toBe("function");
  });

  test("does not depend on a `client` member no V2 host defines", () => {
    expect(hoistedV2ToolConsumers({ client: { permission: {}, event: {} } })).toEqual({});
  });

  test("stays fail-closed when either rules domain is absent", () => {
    const { host } = permissionHost(OPENCODE_DEFAULT_RULES);

    expect(hoistedV2ToolConsumers({})).toEqual({});
    expect(hoistedV2ToolConsumers({ agent: host.agent })).toEqual({});
    expect(hoistedV2ToolConsumers({ session: host.session })).toEqual({});
  });

  test("routes an ask through the Location's own prompt channel", async () => {
    const { host } = permissionHost([{ action: "*", resource: "*", effect: "ask" }]);
    const service = promptService(["allow"]);
    const consumers = hoistedV2ToolConsumers(host, service.channel);

    await consumers.requestPermission?.(REQUEST, EXECUTION_CONTEXT);

    expect(service.creates).toHaveLength(1);
  });

  test("bash surfaces a host-visible Error when no evaluator is bound", async () => {
    const definition = {
      description: "permission refusal probe",
      args: {},
      execute: async (_input: unknown, runtime: { ask(request: unknown): Promise<void> }) => {
        await runtime.ask({ permission: "bash", patterns: ["printf fixture"], always: [] });
        return "unreachable";
      },
    };
    const bash = projectV2Tool("bash", definition as never, LOCATION);

    await expect(execute(bash, {})).rejects.toThrow(
      'The "bash" operation was refused because this AFT runtime has no permission evaluator bound',
    );
  });
});

describe("OpenCode V2 projected filesystem tools", () => {
  test("hoisted edit sends PatchDiff metadata through the registration consumer seam", async () => {
    const seen: unknown[] = [];
    const { host } = permissionHost(OPENCODE_DEFAULT_RULES);
    const recording: V2PermissionHostContext = {
      agent: host.agent,
      session: host.session,
    };
    const consumers = hoistedV2ToolConsumers(recording, unreachableService().channel);
    const definitions = hoistedTools(
      pluginContext((_name, preview) =>
        preview
          ? { success: true, preview_diff: "@@ -1 +1 @@\n-old\n+new" }
          : { success: true, text: "edited" },
      ),
    );
    const edit = projectV2Tool("edit", definitions.edit, LOCATION, {
      requestPermission: (request, context) => {
        seen.push(request);
        return consumers.requestPermission?.(request, context) ?? Promise.resolve();
      },
    });

    await execute(edit, { path: "file.ts", edits: [{ oldString: "old", newString: "new" }] });

    expect(seen[0]).toMatchObject({
      permission: "edit",
      patterns: ["file.ts"],
      metadata: {
        filepath: resolve(PROJECT_ROOT, "file.ts"),
        diff: "@@ -1 +1 @@\n-old\n+new",
      },
    });
  });

  test("read, write, and edit fail with the rendered denial instead of a JSON envelope", async () => {
    for (const name of ["read", "write", "edit"] as const) {
      const { host } = permissionHost([{ action: "*", resource: "*", effect: "deny" }]);
      const tools = projectedFilesystemTools(host, (_tool, preview) =>
        preview ? { success: true, preview_diff: "diff" } : { success: true, text: "ok" },
      );
      const input =
        name === "read"
          ? { path: "file.ts" }
          : name === "write"
            ? { path: "file.ts", content: "new" }
            : { path: "file.ts", edits: [{ oldString: "old", newString: "new" }] };

      const failure = await execute(tools[name], input).then(
        (result) => result,
        (error: unknown) => error as Error,
      );

      expect(failure, `${name} must fail the call`).toBeInstanceOf(Error);
      expect((failure as Error).message).toContain("permission_denied");
      expect((failure as Error).message).not.toContain('"success"');
    }
  });

  test("aft_delete and aft_move reject with the deciding rule", async () => {
    for (const name of ["aft_delete", "aft_move"] as const) {
      const { host } = permissionHost([{ action: "edit", resource: "*", effect: "deny" }]);
      const tools = projectedFilesystemTools(host, () => ({ success: true, text: "ok" }));
      const input =
        name === "aft_delete"
          ? { files: ["file.ts"] }
          : { path: "file.ts", destination: "moved.ts" };

      const denied = execute(tools[name], input);
      await expect(denied).rejects.toThrow("Permission denied");
      await expect(denied).rejects.toThrow('"edit" on "*" is "deny"');
    }
  });
});
