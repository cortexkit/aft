/// <reference path="../bun-test.d.ts" />
/**
 * OpenCode 2 files AFT's shell permission refusal as a permission rejection.
 *
 * When the host refuses a shell command's permission, AFT shows the model its
 * own refusal text (which names the command parts and the host's reason). The
 * host decides how to file a failed tool call from the error the tool fails
 * with: only its own `Tool.Error` holding its own permission error becomes
 * `permission.rejected`; anything else becomes `tool.execution`. These tests
 * run the shipped bash definition through the OpenCode 2 projection with a
 * host that refuses, and check both that the failure is filed as a
 * permission rejection and that the model still sees AFT's text unchanged.
 * When the host's errors do not have the expected shape, the refusal must
 * keep today's plain error and text.
 */
import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import { BashPermissionLoopError, type BridgePool } from "@cortexkit/aft-bridge";
import { Error as ToolError } from "@opencode/plugin/promise/tool";
import { Cause, Effect, Schema } from "effect";

import { executeV2Bash } from "../tools/bash/executor.js";
import { createBashTool } from "../tools/bash.js";
import { hostPermissionRefusal, projectV2Tool } from "../tools/definitions/v2.js";
import type { PluginContext } from "../types.js";

const PROJECT_CWD = resolve(import.meta.dir, "../../../..");
const COMMAND = "touch denied";
const REFUSAL_TAIL =
  "The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.";
/** AFT's refusal text for a deny rule, as the model sees it today. */
const RULE_REFUSAL = `Permission denied for this shell command: touch denied (Permission denied: shell). ${REFUSAL_TAIL}`;
/** AFT's refusal text when the person rejects the prompt with feedback. */
const FEEDBACK_REFUSAL = `Permission denied for this shell command: touch denied (Use a narrower path). ${REFUSAL_TAIL}`;

// The host's permission errors, which the plugin API does not export. Same
// fields and message as @opencode/core 2.1.0 `src/permission.ts`
// (dist/chunks/shell-envzrdm3.js:97-110), unchanged from 2.0.29.
class CorrectedError extends Schema.TaggedError<CorrectedError>()("Permission.CorrectedError", {
  feedback: Schema.String,
}) {}

class BlockedError extends Schema.TaggedError<BlockedError>()("Permission.BlockedError", {
  rules: Schema.Array(Schema.Unknown),
  permission: Schema.String,
  resources: Schema.Array(Schema.String),
  reason: Schema.optional(Schema.String),
}) {}
Object.defineProperty(BlockedError.prototype, "message", {
  get(this: BlockedError) {
    return this.reason ?? `Permission denied: ${this.permission}`;
  },
});

/** A host whose blocked error no longer shows `reason` as its message. */
class ReasonlessBlockedError extends Schema.TaggedError<ReasonlessBlockedError>()(
  "Permission.BlockedError",
  {
    rules: Schema.Array(Schema.Unknown),
    permission: Schema.String,
    resources: Schema.Array(Schema.String),
    reason: Schema.optional(Schema.String),
  },
) {}
Object.defineProperty(ReasonlessBlockedError.prototype, "message", {
  get(this: ReasonlessBlockedError) {
    return `Permission denied: ${this.permission}`;
  },
});

/** A host whose blocked error refuses to be built with the fields AFT passes. */
class UnbuildableBlockedError extends Error {
  readonly _tag = "Permission.BlockedError";
  readonly permission = "shell";
  constructor(props?: Record<string, unknown>) {
    super("Permission denied: shell");
    if (props?.reason !== undefined) throw new Error("unexpected field: reason");
  }
}

const RULES = [{ action: "shell", resource: "touch *", effect: "deny" }];

/** What the plugin's `permission.assert` fails with on a deny rule (host.ts:432-436). */
function ruleDenial(
  blocked: Error = new BlockedError({ rules: RULES, permission: "shell", resources: [COMMAND] }),
) {
  return new ToolError({ message: blocked.message, error: blocked });
}

/** What it fails with when the person rejects the prompt with feedback. */
function feedbackRejection() {
  const corrected = new CorrectedError({ feedback: "Use a narrower path." });
  return new ToolError({ message: corrected.feedback, error: corrected });
}

/**
 * How the host files a failed plugin tool call: the tool runtime turns a
 * failure that is not a `Tool.Error` into one with only a message
 * (`src/tool/runtime.ts`, dist/chunks/shell-pyv0edze.js:27), and
 * `toSessionError` (`src/session/to-session-error.ts`,
 * dist/chunks/shell-v6abvp0h.js:72-80) classifies it. Both are the same in
 * 2.0.29 and 2.1.0; only the branches these tests reach are copied.
 */
function hostFiling(failure: unknown): { type: string; message: string } {
  const classify = (cause: unknown): { type: string; message: string } => {
    if (cause instanceof BlockedError)
      return { type: "permission.rejected", message: cause.message };
    if (cause instanceof CorrectedError) {
      return { type: "permission.rejected", message: cause.feedback };
    }
    if (cause instanceof ToolError) {
      if (cause.error === undefined) return { type: "tool.execution", message: cause.message };
      const unwrapped = classify(cause.error);
      return unwrapped.message === ""
        ? { ...unwrapped, type: "tool.execution", message: cause.message }
        : unwrapped;
    }
    return { type: "unknown", message: cause instanceof Error ? cause.message : String(cause) };
  };
  return classify(
    failure instanceof ToolError
      ? failure
      : new ToolError({ message: failure instanceof Error ? failure.message : String(failure) }),
  );
}

/** Run the shipped bash tool on OpenCode 2 against a host that refuses with `refusal`. */
async function refusedShell(refusal: unknown): Promise<unknown> {
  const bridge = {
    send: async (command: string) => {
      if (command !== "bash") return { success: true };
      return {
        success: false,
        code: "permission_required",
        asks: [{ kind: "bash", patterns: [COMMAND], always: ["touch *"] }],
      };
    },
  };
  const ctx: PluginContext = {
    pool: { getBridge: () => bridge } as unknown as BridgePool,
    client: {
      lsp: { status: async () => ({ data: [] }) },
      find: { symbols: async () => ({ data: [] }) },
    } as never,
    config: {} as PluginContext["config"],
    storageDir: "/tmp/aft-test",
  };
  const projected = projectV2Tool(
    "bash",
    createBashTool(ctx),
    { directory: PROJECT_CWD },
    {
      executeBash: executeV2Bash,
      requestPermission: async () => {
        throw refusal;
      },
    },
  );
  const exit = await Effect.runPromiseExit(
    projected.execute(
      { command: COMMAND },
      {
        sessionID: "shell-refusal",
        messageID: "message",
        id: "call",
        progress: () => Effect.void,
      },
    ),
  );
  if (exit._tag !== "Failure") throw new Error("the refused shell call succeeded");
  return Cause.squash(exit.cause);
}

describe("OpenCode 2 shell permission refusal", () => {
  test("a deny rule is filed as the host's permission rejection with AFT's text", async () => {
    const failure = await refusedShell(ruleDenial());

    expect(failure).toBeInstanceOf(ToolError);
    const blocked = (failure as ToolError).error;
    expect(blocked).toBeInstanceOf(BlockedError);
    expect(blocked).toMatchObject({ rules: RULES, permission: "shell", resources: [COMMAND] });
    expect((failure as ToolError).message).toBe(RULE_REFUSAL);
    expect(hostFiling(failure)).toEqual({ type: "permission.rejected", message: RULE_REFUSAL });
  });

  test("a rejection with feedback is filed as a permission rejection with AFT's text", async () => {
    const failure = await refusedShell(feedbackRejection());

    expect(failure).toBeInstanceOf(ToolError);
    expect((failure as ToolError).error).toBeInstanceOf(CorrectedError);
    expect(hostFiling(failure)).toEqual({
      type: "permission.rejected",
      message: FEEDBACK_REFUSAL,
    });
  });

  const fallbacks: Array<[string, () => unknown, string]> = [
    [
      "a permission error that is a plain object, not the host's class",
      () =>
        new ToolError({
          message: "Permission denied: shell",
          error: { _tag: "Permission.BlockedError", permission: "shell", resources: [COMMAND] },
        }),
      RULE_REFUSAL,
    ],
    [
      "a blocked error whose message ignores its reason",
      () =>
        ruleDenial(
          new ReasonlessBlockedError({ rules: RULES, permission: "shell", resources: [COMMAND] }),
        ),
      RULE_REFUSAL,
    ],
    [
      "a blocked error that cannot be rebuilt with AFT's fields",
      () => ruleDenial(new UnbuildableBlockedError()),
      RULE_REFUSAL,
    ],
    [
      "a Tool.Error without a permission error inside",
      () => new ToolError({ message: "Use a narrower path." }),
      FEEDBACK_REFUSAL,
    ],
    [
      "a Tool.Error holding an error that is not a permission refusal",
      () => {
        const notFound = Object.assign(new Error("Session not found: ses_x"), {
          _tag: "Session.NotFoundError",
        });
        return new ToolError({ message: "Permission denied: shell", error: notFound });
      },
      RULE_REFUSAL,
    ],
  ];
  for (const [label, refusal, text] of fallbacks) {
    test(`keeps today's error and text for ${label}`, async () => {
      const failure = await refusedShell(refusal());

      expect(failure).toBeInstanceOf(BashPermissionLoopError);
      expect(failure).not.toBeInstanceOf(ToolError);
      expect(hostFiling(failure)).toEqual({ type: "tool.execution", message: text });
    });
  }

  test("only a shell refusal that carries the host's failure is rebuilt", () => {
    const unsettled = new BashPermissionLoopError("bash permission not settled after 8 rounds");
    expect(hostPermissionRefusal(unsettled)).toBe(unsettled);

    const otherError = new Error(RULE_REFUSAL, { cause: ruleDenial() });
    expect(hostPermissionRefusal(otherError)).toBe(otherError);

    const hostError = ruleDenial();
    expect(hostPermissionRefusal(hostError)).toBe(hostError);
  });
});
