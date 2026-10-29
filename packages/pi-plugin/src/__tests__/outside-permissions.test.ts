import { describe, expect, mock, test } from "bun:test";
import type { AftProjectTransport } from "@cortexkit/aft-bridge";
import type {
  ExtensionAPI,
  ExtensionContext,
  ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { bindToolRegistrationFunnel } from "../tool-registration.js";
import {
  callBridge,
  callToolCallForSession,
  withOutsidePermissionOperation,
  withOutsidePermissions,
} from "../tools/_shared.js";
import type { PluginContext } from "../types.js";

const ask = (id: string) => ({
  success: false,
  code: "permission_required",
  text: "outside access needed",
  asks: [
    {
      kind: "external_directory",
      grant_id: `outside:${id}`,
      path: `/outside/${id}`,
      operation: "read",
    },
  ],
});
const context = (confirm: (...args: unknown[]) => Promise<boolean>, signal?: AbortSignal) =>
  ({ hasUI: true, ui: { confirm }, signal }) as unknown as ExtensionContext;

describe("engine outside-access asks", () => {
  test("registered Pi operation carries undo preview approval through apply", async () => {
    const calls: Array<{ name: string; args: Record<string, unknown> }> = [];
    const answer = (name: string, args: Record<string, unknown>) => {
      calls.push({ name, args: structuredClone(args) });
      const grants = args.permissions_granted as string[] | undefined;
      if (name === "undo_preview")
        return grants?.includes("outside:preview")
          ? { success: true, text: "preview", permissions_continuation: ["outside:apply"] }
          : ask("preview");
      return grants?.includes("outside:apply") || grants?.includes("outside:apply-new")
        ? { success: true, text: "applied" }
        : ask("apply-new");
    };
    const bridge = {
      send: async (name: string, args: Record<string, unknown>) => answer(name, args),
      toolCall: async (_session: unknown, name: string, args: Record<string, unknown>) =>
        answer(name, args),
    } as unknown as AftProjectTransport;
    const prompt = mock(async () => true);
    const extCtx = {
      ...context(prompt),
      sessionManager: { getSessionId: () => "captured" },
    } as unknown as ExtensionContext;
    let registered: ToolDefinition | undefined;
    const pi = bindToolRegistrationFunnel(
      {
        registerTool: (tool: ToolDefinition) => {
          registered = tool;
        },
      } as unknown as ExtensionAPI,
      { config: {} } as unknown as PluginContext,
      "pi",
    );
    pi.registerTool({
      name: "test_undo",
      label: "undo",
      description: "fixture",
      parameters: Type.Object({}),
      execute: async (_id, _params, _signal, _update, ctx) => {
        await callBridge(bridge, "undo_preview", { file: "/outside/a" }, ctx);
        const result = await callToolCallForSession(
          bridge,
          "safety",
          { op: "undo", path: "/outside/a" },
          "captured",
          ctx,
        );
        return { content: [{ type: "text" as const, text: result.text }], details: {} };
      },
    });
    if (!registered) throw new Error("tool not registered");
    await registered.execute("logical-call", {}, new AbortController().signal, undefined, extCtx);
    expect(prompt).toHaveBeenCalledTimes(1);
    expect(calls.find((call) => call.name === "safety")?.args.permissions_granted).toEqual([
      "outside:apply",
    ]);
  });
  test("path tool dispatch prompts and retries in the captured session", async () => {
    const calls: unknown[][] = [];
    const prompt = mock(async () => true);
    const bridge = {
      toolCall: async (...args: unknown[]) => {
        calls.push([args[0], args[1], structuredClone(args[2])]);
        return calls.length === 1
          ? ask("1")
          : calls.length === 2
            ? { ...ask("2"), permissions_continuation: ["outside:renewed-1"] }
            : { success: true, text: "allowed" };
      },
    } as unknown as AftProjectTransport;
    const result = await callToolCallForSession(
      bridge,
      "read",
      { path: "/outside/file" },
      "captured",
      context(prompt),
    );
    expect(result.text).toBe("allowed");
    expect(prompt).toHaveBeenCalledTimes(2);
    expect(calls.map((call) => call[0])).toEqual(["captured", "captured", "captured"]);
    expect(calls[2]?.[2]).toMatchObject({
      permissions_granted: ["outside:renewed-1", "outside:2"],
    });
  });

  test("rawArgs cannot seed forged or replayed permission tokens", async () => {
    const calls: Record<string, unknown>[] = [];
    const prompt = mock(async () => true);
    const bridge = {
      toolCall: async (_session: unknown, _name: string, args: Record<string, unknown>) => {
        calls.push(structuredClone(args));
        return calls.length === 1 ? ask("issued") : { success: true, text: "allowed" };
      },
    } as unknown as AftProjectTransport;
    await callToolCallForSession(
      bridge,
      "read",
      {
        path: "/outside/a",
        permissions_granted: [
          'outside:{"path":"/outside/a","kind":"file","mode":"read"}',
          "outside:old-issued-token",
        ],
      },
      "captured",
      context(prompt),
    );
    expect(calls[0]).not.toHaveProperty("permissions_granted");
    expect(calls[1]?.permissions_granted).toEqual(["outside:issued"]);
    expect(prompt).toHaveBeenCalledTimes(1);
  });

  test("a refusal never sends approved IDs", async () => {
    const send = mock(async () => ask("no"));
    await expect(
      withOutsidePermissions(
        {},
        context(async () => false),
        send,
      ),
    ).rejects.toMatchObject({ code: "permission_denied" });
    expect(send).toHaveBeenCalledTimes(1);
  });

  test("a session without Pi's own screen proceeds without a prompt", async () => {
    // Print, JSON and RPC sessions, and a call with no extension context,
    // get no AFT prompt (only Pi's own terminal UI is asked): outside access
    // is allowed as it was before AFT asked, and nothing waits on an RPC
    // client's answer.
    const confirm = mock(async () => true);
    const contexts: Array<[string, ExtensionContext | undefined]> = [
      ...["rpc", "json", "print"].map((mode): [string, ExtensionContext] => [
        mode,
        { ...context(confirm), mode } as unknown as ExtensionContext,
      ]),
      ["no ui", { hasUI: false, ui: { confirm } } as unknown as ExtensionContext],
      ["no context", undefined],
    ];
    for (const [label, extCtx] of contexts) {
      const sends: Array<Record<string, unknown>> = [];
      const send = async (args: Record<string, unknown>) => {
        sends.push(structuredClone(args));
        return sends.length === 1 ? ask(label) : { success: true, text: "outside file body" };
      };
      const result = await withOutsidePermissionOperation(
        () => withOutsidePermissions({}, extCtx, send),
        undefined,
        { harness: "pi", askOnPi: true },
      );
      expect(result.text).toBe("outside file body");
      expect(sends[1]?.permissions_granted).toEqual([`outside:${label}`]);
    }
    expect(confirm).not.toHaveBeenCalled();
  });

  test("Pi's terminal UI still prompts for outside access", async () => {
    let sends = 0;
    const send = async () => (++sends === 1 ? ask("tui") : { success: true, text: "allowed" });
    const confirm = mock(async () => true);
    const extCtx = { ...context(confirm), mode: "tui" } as unknown as ExtensionContext;
    const result = await withOutsidePermissions({}, extCtx, send);
    expect(result.success).toBe(true);
    expect(confirm).toHaveBeenCalledTimes(1);
  });

  test("cancellation during confirmation never sends the approved retry", async () => {
    const abort = new AbortController();
    const send = mock(async () => ask("cancelled"));
    await expect(
      withOutsidePermissions(
        {},
        context(async () => {
          abort.abort();
          return true;
        }, abort.signal),
        send,
      ),
    ).rejects.toThrow();
    expect(send).toHaveBeenCalledTimes(1);
  });

  test("cancelling the tool call while a confirmation is pending stops the approved write", async () => {
    // The extension context has no signal of its own: only the signal Pi
    // passes to this call's execute can cancel it.
    const sends: Array<Record<string, unknown>> = [];
    const bridge = {
      toolCall: async (_session: unknown, _name: string, args: Record<string, unknown>) => {
        sends.push(structuredClone(args));
        return sends.length === 1 ? ask("write") : { success: true, text: "written" };
      },
    } as unknown as AftProjectTransport;
    const call = new AbortController();
    const extCtx = context(async () => {
      call.abort();
      return true;
    });
    let registered: ToolDefinition | undefined;
    const pi = bindToolRegistrationFunnel(
      {
        registerTool: (tool: ToolDefinition) => {
          registered = tool;
        },
      } as unknown as ExtensionAPI,
      { config: {} } as unknown as PluginContext,
      "pi",
    );
    pi.registerTool({
      name: "test_write",
      label: "write",
      description: "fixture",
      parameters: Type.Object({}),
      execute: async (_id, _params, _signal, _update, ctx) => {
        const result = await callToolCallForSession(
          bridge,
          "write",
          { path: "/outside/file", content: "late" },
          "captured",
          ctx,
        );
        return { content: [{ type: "text" as const, text: result.text }], details: {} };
      },
    });
    if (!registered) throw new Error("tool not registered");
    const outcome = registered.execute("call", {}, call.signal, undefined, extCtx);
    await expect(outcome).rejects.toThrow(/abort/i);
    expect(sends).toHaveLength(1);
    expect(sends[0]).not.toHaveProperty("permissions_granted");
  });
});

/**
 * A scripted engine for a batch touching `count` distinct outside files: like
 * the real engine it reports only the first refused file per attempt, and
 * renews the approvals it was sent.
 */
function batchEngine(count: number, refuseAgain = false) {
  const sends: Array<Record<string, unknown>> = [];
  const send = async (args: Record<string, unknown>) => {
    sends.push(structuredClone(args));
    const granted = (args.permissions_granted as string[] | undefined) ?? [];
    for (let file = 0; file < count; file++) {
      const id = `outside:file-${file}`;
      if (refuseAgain || !granted.includes(id))
        return {
          success: false,
          code: "permission_required",
          permissions_continuation: granted,
          asks: [
            {
              kind: "external_directory",
              grant_id: id,
              path: `/outside/file-${file}`,
              operation: "write",
            },
          ],
        } as Record<string, unknown>;
    }
    return { success: true, text: `wrote ${count} files` } as Record<string, unknown>;
  };
  return { send, sends };
}

describe("outside-access batches", () => {
  test("a 32-file outside batch with every ask approved completes", async () => {
    const engine = batchEngine(32);
    const confirm = mock(async () => true);
    const result = await withOutsidePermissions({}, context(confirm), engine.send);
    expect(result.success).toBe(true);
    expect(confirm).toHaveBeenCalledTimes(32);
    expect(engine.sends).toHaveLength(33);
  });

  test("a batch stops at the first denied path", async () => {
    const engine = batchEngine(32);
    let prompts = 0;
    await expect(
      withOutsidePermissions(
        {},
        context(async () => ++prompts !== 6),
        engine.send,
      ),
    ).rejects.toMatchObject({ code: "permission_denied" });
    expect(prompts).toBe(6);
    expect(engine.sends).toHaveLength(6);
  });

  test("an approved path refused again stops with a typed error instead of asking forever", async () => {
    const engine = batchEngine(1, true);
    const confirm = mock(async () => true);
    await expect(withOutsidePermissions({}, context(confirm), engine.send)).rejects.toMatchObject({
      code: "outside_permission_stalled",
    });
    expect(confirm).toHaveBeenCalledTimes(1);
    expect(engine.sends).toHaveLength(2);
  });
});

/**
 * A scripted engine that invalidates the first approvals for one outside path.
 * Each invalidated retry gets the supplied reason (expiry by default).
 */
function invalidatingEngine(invalidations: number, reason: string | null = "approval_expired") {
  const sends: Array<Record<string, unknown>> = [];
  let issued = 0;
  const send = async (args: Record<string, unknown>) => {
    sends.push(structuredClone(args));
    const granted = (args.permissions_granted as string[] | undefined) ?? [];
    const latest = `outside:late-${issued}`;
    if (granted.includes(latest) && issued >= invalidations)
      return { success: true, text: "written" } as Record<string, unknown>;
    const presented = granted.includes(latest);
    if (presented) issued++;
    return {
      success: false,
      code: "permission_required",
      asks: [
        {
          kind: "external_directory",
          grant_id: `outside:late-${issued}`,
          path: "/outside/late",
          operation: "write",
          ...(presented && reason ? { reason } : {}),
        },
      ],
    } as Record<string, unknown>;
  };
  return { send, sends };
}

describe("outside-access approvals invalidated before retry", () => {
  test("an approval that arrives after its token expired is asked once more", async () => {
    const engine = invalidatingEngine(1);
    const confirm = mock(async () => true);
    const result = await withOutsidePermissions({}, context(confirm), engine.send);
    expect(result.success).toBe(true);
    expect(confirm).toHaveBeenCalledTimes(2);
    expect(engine.sends).toHaveLength(3);
  });

  test("an approval refused again without expiry still stalls", async () => {
    const engine = invalidatingEngine(1, null);
    const confirm = mock(async () => true);
    await expect(withOutsidePermissions({}, context(confirm), engine.send)).rejects.toMatchObject({
      code: "outside_permission_stalled",
    });
    expect(confirm).toHaveBeenCalledTimes(1);
  });

  test("route replacement asks for a fresh approval", async () => {
    const engine = invalidatingEngine(1, "approval_route_replaced");
    const confirm = mock(async () => true);
    const result = await withOutsidePermissions({}, context(confirm), engine.send);
    expect(result.success).toBe(true);
    expect(confirm).toHaveBeenCalledTimes(2);
    expect(engine.sends).toHaveLength(3);
    expect(engine.sends[2]?.permissions_granted).toEqual(["outside:late-1"]);
  });

  test("denying the replacement route's prompt never sends its token", async () => {
    const engine = invalidatingEngine(1, "approval_route_replaced");
    let decisions = 0;
    const confirm = mock(async () => ++decisions === 1);
    await expect(withOutsidePermissions({}, context(confirm), engine.send)).rejects.toMatchObject({
      code: "permission_denied",
    });
    expect(confirm).toHaveBeenCalledTimes(2);
    expect(engine.sends).toHaveLength(2);
  });

  test("repeated route replacement stops after one re-ask", async () => {
    const engine = invalidatingEngine(10, "approval_route_replaced");
    const confirm = mock(async () => true);
    await expect(withOutsidePermissions({}, context(confirm), engine.send)).rejects.toMatchObject({
      code: "outside_permission_stalled",
    });
    expect(confirm).toHaveBeenCalledTimes(2);
    expect(engine.sends).toHaveLength(3);
  });

  test("a path that keeps expiring stops at the re-ask limit", async () => {
    const engine = invalidatingEngine(10);
    const confirm = mock(async () => true);
    await expect(withOutsidePermissions({}, context(confirm), engine.send)).rejects.toMatchObject({
      code: "outside_permission_stalled",
    });
    expect(confirm).toHaveBeenCalledTimes(3);
    expect(engine.sends).toHaveLength(4);
  });
});

/**
 * Register one tool through the real registration funnel with `config`, and
 * run it once in `extCtx` against an engine that asks for one outside path
 * and then answers with the file's body.
 */
async function runRegistered(
  config: Record<string, unknown>,
  extCtx: ExtensionContext,
  harness: "pi" | "omp" = "pi",
  engineAnswer: Record<string, unknown> = { success: true, text: "outside file body" },
) {
  const sends: Array<Record<string, unknown>> = [];
  const bridge = {
    toolCall: async (_session: unknown, _name: string, args: Record<string, unknown>) => {
      sends.push(structuredClone(args));
      return sends.length === 1 && engineAnswer.success !== false ? ask("knob") : engineAnswer;
    },
  } as unknown as AftProjectTransport;
  let registered: ToolDefinition | undefined;
  const pi = bindToolRegistrationFunnel(
    {
      registerTool: (tool: ToolDefinition) => {
        registered = tool;
      },
    } as unknown as ExtensionAPI,
    { config } as unknown as PluginContext,
    harness,
  );
  pi.registerTool({
    name: "test_read",
    label: "read",
    description: "fixture",
    parameters: Type.Object({}),
    execute: async (_id, _params, _signal, _update, ctx) => {
      const result = await callToolCallForSession(
        bridge,
        "read",
        { path: "/outside/knob" },
        "captured",
        ctx,
      );
      if (result.success === false)
        throw Object.assign(new Error(String(result.message)), { code: result.code });
      return { content: [{ type: "text" as const, text: result.text }], details: {} };
    },
  });
  if (!registered) throw new Error("tool not registered");
  const outcome = await registered.execute(
    "call",
    {},
    new AbortController().signal,
    undefined,
    extCtx,
  );
  return { text: (outcome.content[0] as { text: string }).text, sends };
}

const tui = (confirm: (...args: unknown[]) => Promise<boolean>) =>
  ({ ...context(confirm), mode: "tui" }) as unknown as ExtensionContext;

describe("pi.external_access_ask", () => {
  test("Pi's terminal UI asks when the setting is on, and by default", async () => {
    for (const config of [{ pi: { external_access_ask: true } }, {}]) {
      const confirm = mock(async () => true);
      const { text, sends } = await runRegistered(config, tui(confirm));
      expect(text).toBe("outside file body");
      expect(confirm).toHaveBeenCalledTimes(1);
      expect(sends[1]?.permissions_granted).toEqual(["outside:knob"]);
    }
  });

  test("with the setting off Pi's terminal UI does not ask and the file is read", async () => {
    const confirm = mock(async () => false);
    const { text, sends } = await runRegistered(
      { pi: { external_access_ask: false } },
      tui(confirm),
    );
    expect(text).toBe("outside file body");
    expect(confirm).not.toHaveBeenCalled();
    expect(sends[1]?.permissions_granted).toEqual(["outside:knob"]);
  });

  test("a print or RPC session gets the file without asking, setting on or off", async () => {
    for (const external_access_ask of [true, false]) {
      for (const mode of ["print", "rpc"]) {
        const confirm = mock(async () => false);
        const extCtx = { ...context(confirm), mode } as unknown as ExtensionContext;
        const { text } = await runRegistered({ pi: { external_access_ask } }, extCtx);
        expect(text).toBe("outside file body");
        expect(confirm).not.toHaveBeenCalled();
      }
    }
  });

  test("an engine refusal (floor or restrict_to_project_root) is never retried or asked about", async () => {
    // Floors and a configured restriction are engine denials, not asks: no
    // mode turns them into an approval.
    const refusal = { success: false, code: "path_outside_root", message: "refused" };
    const modes: ExtensionContext[] = [
      tui(mock(async () => true)),
      { ...context(mock(async () => true)), mode: "print" } as unknown as ExtensionContext,
      { ...context(mock(async () => true)), mode: "rpc" } as unknown as ExtensionContext,
    ];
    for (const harness of ["pi", "omp"] as const) {
      for (const external_access_ask of [true, false]) {
        for (const extCtx of modes) {
          const error = await runRegistered(
            { pi: { external_access_ask } },
            extCtx,
            harness,
            refusal,
          ).catch((e: unknown) => e);
          expect(error).toMatchObject({ code: "path_outside_root" });
          expect((extCtx.ui.confirm as ReturnType<typeof mock>).mock.calls).toHaveLength(0);
        }
      }
    }
  });

  test("on OMP AFT never asks: OMP's approval mode decides through the tools' tiers", async () => {
    for (const external_access_ask of [true, false]) {
      const confirm = mock(async () => false);
      const { text } = await runRegistered({ pi: { external_access_ask } }, tui(confirm), "omp");
      expect(text).toBe("outside file body");
      expect(confirm).not.toHaveBeenCalled();
    }
  });
});

describe("asks that are not outside-path asks", () => {
  const outsideAsk = {
    kind: "external_directory",
    grant_id: "outside:a",
    path: "/outside/a",
    operation: "read",
  };
  const otherAsks: Array<[string, Array<Record<string, unknown>>]> = [
    ["bash rule", [{ kind: "bash", grant_id: "bash:rm-rf", patterns: ["rm -rf *"] }]],
    ["escalation", [{ kind: "escalation", grant_id: "escalation:host" }]],
    ["unknown kind", [{ kind: "network", grant_id: "outside:looks-alike" }]],
    [
      "external_directory without an outside: grant",
      [{ kind: "external_directory", grant_id: "bash:dir", path: "/outside/a" }],
    ],
    ["outside ask beside a bash rule", [outsideAsk, { kind: "bash", grant_id: "bash:rm" }]],
  ];
  const policies = [
    { harness: "pi", askOnPi: false },
    { harness: "pi", askOnPi: true },
    { harness: "omp", askOnPi: true },
  ] as const;

  test("are never approved without a prompt, in any mode or setting", async () => {
    const confirm = mock(async () => true);
    const contexts: Array<ExtensionContext | undefined> = [
      { ...context(confirm), mode: "tui" } as unknown as ExtensionContext,
      { ...context(confirm), mode: "print" } as unknown as ExtensionContext,
      { ...context(confirm), mode: "rpc" } as unknown as ExtensionContext,
      undefined,
    ];
    for (const [label, asks] of otherAsks) {
      for (const policy of policies) {
        for (const extCtx of contexts) {
          const sends: Array<Record<string, unknown>> = [];
          const send = async (args: Record<string, unknown>) => {
            sends.push(structuredClone(args));
            return { success: false, code: "permission_required", asks } as Record<string, unknown>;
          };
          const response = await withOutsidePermissionOperation(
            () => withOutsidePermissions({}, extCtx, send),
            undefined,
            policy,
          );
          // Handed back untouched after one send: nothing was approved.
          expect(response.code, label).toBe("permission_required");
          expect(sends, label).toHaveLength(1);
          expect(sends[0], label).not.toHaveProperty("permissions_granted");
        }
      }
    }
    expect(confirm).not.toHaveBeenCalled();
  });
});
