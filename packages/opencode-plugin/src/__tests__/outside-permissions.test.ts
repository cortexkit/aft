import { describe, expect, mock, test } from "bun:test";
import { callToolCall } from "../tools/_shared.js";
import { withOutsidePermissions } from "../tools/outside-permissions.js";
import type { PluginContext } from "../types.js";

const ask = (id: string) => ({
  success: false,
  code: "permission_required",
  asks: [
    {
      kind: "external_directory",
      grant_id: `outside:${id}`,
      patterns: ["/outside/*"],
      always: ["/outside/*"],
    },
  ],
});

describe("engine outside-access asks", () => {
  test("continuation cache never forwards write approval to a different tool", async () => {
    const calls: Array<{ name: string; args: Record<string, unknown> }> = [];
    const prompt = mock(async (_ask: unknown) => {});
    const runtime = { directory: process.cwd(), ask: prompt, callID: "one-host-id" };
    const bridge = {
      toolCall: async (
        _session: unknown,
        name: string,
        args: Record<string, unknown>,
        options: { preview?: boolean },
      ) => {
        calls.push({ name, args: structuredClone(args) });
        const grants = args.permissions_granted as string[] | undefined;
        if (options?.preview)
          return grants?.includes("outside:write")
            ? {
                success: true,
                text: "preview",
                permissions_continuation: ["outside:write-renewed"],
              }
            : ask("write");
        return grants?.includes("outside:delete")
          ? { success: true, text: "deleted" }
          : ask("delete");
      },
    };
    const ctx = {
      pool: { getBridge: () => bridge },
      client: {},
      config: {},
    } as unknown as PluginContext;
    await callToolCall(
      ctx,
      runtime,
      "write",
      { path: "/outside/a", content: "x" },
      { preview: true },
    );
    await callToolCall(ctx, runtime, "delete", { files: ["/outside/a"] });
    expect(calls.find((call) => call.name === "delete")?.args).not.toHaveProperty(
      "permissions_granted",
    );
    expect(prompt).toHaveBeenCalledTimes(2);
  });
  test("path tool dispatch presents the engine ask before its approved retry", async () => {
    const calls: Record<string, unknown>[] = [];
    const prompt = mock(async (_ask: unknown) => {});
    const bridge = {
      toolCall: async (_session: unknown, _name: string, args: Record<string, unknown>) => {
        calls.push(structuredClone(args));
        return calls.length === 1 ? ask("dispatch") : { success: true, text: "allowed" };
      },
    };
    const ctx = {
      pool: { getBridge: () => bridge },
      client: {},
      config: {},
    } as unknown as PluginContext;
    const response = await callToolCall(ctx, { directory: process.cwd(), ask: prompt }, "read", {
      path: "/outside/file",
    });
    expect(response.text).toBe("allowed");
    expect(prompt).toHaveBeenCalledTimes(1);
    expect(calls[1]?.permissions_granted).toEqual(["outside:dispatch"]);
  });
  test("prompts and accumulates exact engine IDs across retries", async () => {
    const requests: Record<string, unknown>[] = [];
    const prompt = mock(async (_ask: unknown) => {});
    const result = await withOutsidePermissions(
      { ask: prompt },
      { path: "/outside/file" },
      async (args) => {
        requests.push(structuredClone(args));
        return requests.length === 1
          ? ask("1")
          : requests.length === 2
            ? { ...ask("2"), permissions_continuation: ["outside:renewed-1"] }
            : { success: true };
      },
    );
    expect(result.success).toBe(true);
    expect(prompt).toHaveBeenCalledTimes(2);
    expect(prompt.mock.calls[0]?.[0]).toMatchObject({
      permission: "external_directory",
      patterns: ["/outside/*"],
    });
    expect(requests[0]).not.toHaveProperty("permissions_granted");
    expect(requests[2]?.permissions_granted).toEqual(["outside:renewed-1", "outside:2"]);
  });

  test("rawArgs cannot seed forged or replayed permission tokens", async () => {
    const requests: Record<string, unknown>[] = [];
    const prompt = mock(async (_ask: unknown) => {});
    const bridge = {
      toolCall: async (_session: unknown, _name: string, args: Record<string, unknown>) => {
        requests.push(structuredClone(args));
        return requests.length === 1 ? ask("issued") : { success: true, text: "allowed" };
      },
    };
    const ctx = {
      pool: { getBridge: () => bridge },
      client: {},
      config: {},
    } as unknown as PluginContext;
    await callToolCall(ctx, { directory: process.cwd(), ask: prompt }, "read", {
      path: "/outside/a",
      permissions_granted: [
        'outside:{"path":"/outside/a","kind":"file","mode":"read"}',
        "outside:old-issued-token",
      ],
    });
    expect(requests[0]).not.toHaveProperty("permissions_granted");
    expect(requests[1]?.permissions_granted).toEqual(["outside:issued"]);
    expect(prompt).toHaveBeenCalledTimes(1);
  });

  test("preview approval uses only a renewed token for apply without a second prompt", async () => {
    const requests: Record<string, unknown>[] = [];
    const prompt = mock(async (_ask: unknown) => {});
    const runtime = { directory: process.cwd(), ask: prompt, callID: "one-tool-call" };
    const bridge = {
      toolCall: async (
        _session: unknown,
        _name: string,
        args: Record<string, unknown>,
        options: { preview?: boolean },
      ) => {
        requests.push(structuredClone(args));
        if (requests.length === 1) return ask("preview");
        if (options?.preview)
          return {
            success: true,
            text: "preview",
            permissions_continuation: ["outside:apply-only"],
          };
        return { success: true, text: "applied" };
      },
    };
    const ctx = {
      pool: { getBridge: () => bridge },
      client: {},
      config: {},
    } as unknown as PluginContext;
    const preview = await callToolCall(
      ctx,
      runtime,
      "write",
      { path: "/outside/a", content: "approved" },
      { preview: true },
    );
    expect(preview).not.toHaveProperty("permissions_continuation");
    await callToolCall(ctx, runtime, "write", { path: "/outside/a", content: "approved" });
    expect(requests[2]?.permissions_granted).toEqual(["outside:apply-only"]);
    expect(prompt).toHaveBeenCalledTimes(1);
  });

  test("OpenCode 1 preview and apply share one approval without a call ID", async () => {
    // OpenCode 1's tool context has a session and message but no call ID, and
    // the preview and its apply send different arguments.
    const runtime = { sessionID: "s", messageID: "m", ask: mock(async (_ask: unknown) => {}) };
    const requests: Record<string, unknown>[] = [];
    const send = async (args: Record<string, unknown>) => {
      requests.push(structuredClone(args));
      if (requests.length === 1) return ask("preview");
      if (requests.length === 2)
        return { success: true, permissions_continuation: ["outside:apply"] };
      return { success: true };
    };
    await withOutsidePermissions(runtime, { file: "/outside/a" }, send, true, "undo_preview");
    await withOutsidePermissions(
      runtime,
      { op: "undo", path: "/outside/a" },
      send,
      false,
      "safety",
    );
    expect(runtime.ask).toHaveBeenCalledTimes(1);
    expect(requests).toHaveLength(3);
    expect(requests[2]?.permissions_granted).toEqual(["outside:apply"]);
  });

  test("a rejected prompt never retries", async () => {
    const send = mock(async () => ask("denied"));
    await expect(
      withOutsidePermissions(
        {
          ask: async () => {
            throw new Error("user rejected");
          },
        },
        {},
        send,
      ),
    ).rejects.toThrow("user rejected");
    expect(send).toHaveBeenCalledTimes(1);
  });

  test("missing UI fails closed and command asks stay in the bash loop", async () => {
    await expect(withOutsidePermissions({}, {}, async () => ask("no-ui"))).rejects.toThrow(
      "permission-capable",
    );
    const commandAsk = {
      success: false,
      code: "permission_required",
      asks: [{ kind: "bash", patterns: ["echo *"] }],
    };
    expect(await withOutsidePermissions({}, {}, async () => commandAsk)).toBe(commandAsk);
  });

  test("OpenCode 1 cancellation while the prompt is pending never sends the approved write", async () => {
    // OpenCode 1 cancels a call through its own ToolContext.abort; there is
    // no Effect signal.
    const call = new AbortController();
    const send = mock(async (_args: Record<string, unknown>) => ask("write"));
    await expect(
      withOutsidePermissions(
        {
          abort: call.signal,
          ask: async () => {
            call.abort();
          },
        },
        { path: "/outside/file", content: "late" },
        send,
        false,
        "write",
      ),
    ).rejects.toThrow(/abort/i);
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0]?.[0]).not.toHaveProperty("permissions_granted");
  });

  test("cancellation during approval never sends the approved retry", async () => {
    const abort = new AbortController();
    const send = mock(async () => ask("cancelled"));
    await expect(
      withOutsidePermissions(
        {
          effectAbort: abort.signal,
          ask: async () => {
            abort.abort();
          },
        },
        {},
        send,
      ),
    ).rejects.toThrow();
    expect(send).toHaveBeenCalledTimes(1);
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
              patterns: ["/outside/*"],
              always: ["/outside/*"],
            },
          ],
        } as Record<string, unknown>;
    }
    return { success: true, text: `wrote ${count} files` } as Record<string, unknown>;
  };
  return { send, sends };
}

describe("outside-access batches", () => {
  for (const hostVersion of [1, 2] as const) {
    test(`OpenCode ${hostVersion}: a 32-file outside batch with every ask approved completes`, async () => {
      const engine = batchEngine(32);
      const prompt = mock(async (_ask: unknown) => {});
      const result = await withOutsidePermissions(
        { ask: prompt, ...(hostVersion === 2 ? { hostVersion } : {}) },
        { files: ["/outside/..."] },
        engine.send,
        false,
        "write",
      );
      expect(result.success).toBe(true);
      expect(prompt).toHaveBeenCalledTimes(32);
      expect(engine.sends).toHaveLength(33);
    });

    test(`OpenCode ${hostVersion}: a batch stops at the first denied path`, async () => {
      const engine = batchEngine(32);
      const denial = new Error("user rejected");
      let prompts = 0;
      await expect(
        withOutsidePermissions(
          {
            ask: async () => {
              prompts++;
              if (prompts === 6) throw denial;
            },
            ...(hostVersion === 2 ? { hostVersion } : {}),
          },
          { files: ["/outside/..."] },
          engine.send,
          false,
          "write",
        ),
      ).rejects.toThrow(hostVersion === 2 ? "user rejected" : /denied|rejected/i);
      expect(prompts).toBe(6);
      expect(engine.sends).toHaveLength(6);
    });
  }

  test("an approved path refused again stops with a typed error instead of asking forever", async () => {
    const engine = batchEngine(1, true);
    const prompt = mock(async (_ask: unknown) => {});
    await expect(
      withOutsidePermissions({ ask: prompt }, {}, engine.send, false, "write"),
    ).rejects.toMatchObject({ code: "outside_permission_stalled" });
    expect(prompt).toHaveBeenCalledTimes(1);
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
          patterns: ["/outside/*"],
          always: ["/outside/*"],
          ...(presented && reason ? { reason } : {}),
        },
      ],
    } as Record<string, unknown>;
  };
  return { send, sends };
}

describe("outside-access approvals invalidated before retry", () => {
  for (const hostVersion of [1, 2] as const) {
    const runtime = (prompt: (ask: unknown) => Promise<void>) => ({
      ask: prompt,
      ...(hostVersion === 2 ? { hostVersion } : {}),
    });

    test(`OpenCode ${hostVersion}: an approval that arrives after its token expired is asked once more`, async () => {
      const engine = invalidatingEngine(1);
      const prompt = mock(async (_ask: unknown) => {});
      const result = await withOutsidePermissions(runtime(prompt), {}, engine.send, false, "write");
      expect(result.success).toBe(true);
      expect(prompt).toHaveBeenCalledTimes(2);
      expect(engine.sends).toHaveLength(3);
    });

    test(`OpenCode ${hostVersion}: an approval refused again without expiry still stalls`, async () => {
      const engine = invalidatingEngine(1, null);
      const prompt = mock(async (_ask: unknown) => {});
      await expect(
        withOutsidePermissions(runtime(prompt), {}, engine.send, false, "write"),
      ).rejects.toMatchObject({ code: "outside_permission_stalled" });
      expect(prompt).toHaveBeenCalledTimes(1);
    });

    test(`OpenCode ${hostVersion}: route replacement asks for a fresh approval`, async () => {
      const engine = invalidatingEngine(1, "approval_route_replaced");
      const prompt = mock(async (_ask: unknown) => {});
      const result = await withOutsidePermissions(runtime(prompt), {}, engine.send, false, "grep");
      expect(result.success).toBe(true);
      expect(prompt).toHaveBeenCalledTimes(2);
      expect(engine.sends).toHaveLength(3);
      expect(engine.sends[2]?.permissions_granted).toEqual(["outside:late-1"]);
    });

    test(`OpenCode ${hostVersion}: repeated route replacement stops after one re-ask`, async () => {
      const engine = invalidatingEngine(10, "approval_route_replaced");
      const prompt = mock(async (_ask: unknown) => {});
      await expect(
        withOutsidePermissions(runtime(prompt), {}, engine.send, false, "grep"),
      ).rejects.toMatchObject({ code: "outside_permission_stalled" });
      expect(prompt).toHaveBeenCalledTimes(2);
      expect(engine.sends).toHaveLength(3);
    });

    test(`OpenCode ${hostVersion}: a path that keeps expiring stops at the re-ask limit`, async () => {
      const engine = invalidatingEngine(10);
      const prompt = mock(async (_ask: unknown) => {});
      await expect(
        withOutsidePermissions(runtime(prompt), {}, engine.send, false, "write"),
      ).rejects.toMatchObject({ code: "outside_permission_stalled" });
      expect(prompt).toHaveBeenCalledTimes(3);
      expect(engine.sends).toHaveLength(4);
    });
  }
});
