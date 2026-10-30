import type { ToolContext } from "@opencode-ai/plugin";
import { classifyPermissionError, permissionRuleDenial, runAsk } from "./permissions.js";

type Runtime = Partial<Pick<ToolContext, "ask">> & {
  effectAbort?: AbortSignal;
  /** OpenCode 1's per-call cancellation signal (`ToolContext.abort`). */
  abort?: AbortSignal;
  hostVersion?: 2;
  sessionID?: string;
  messageID?: string;
  id?: string;
  callID?: string;
};
type Continuation = { key: string; tokens: string[] };
const continuations = new WeakMap<object, Continuation>();
const hostGrants = new WeakMap<object, readonly string[]>();

export function permissionOperation(tool: string, args: Record<string, unknown>): string {
  if (["hashline_preflight", "hashline_edit", "edit", "edit_symbol"].includes(tool)) return "edit";
  if (["undo_preview", "undo"].includes(tool)) return "safety:undo";
  if (["checkpoint_paths", "restore_checkpoint"].includes(tool)) return "safety:restore";
  if (tool === "checkpoint") return "safety:checkpoint";
  if (tool === "safety") return `safety:${String(args.op ?? "")}`;
  return (
    ({ read_file: "read", write_file: "write", delete_file: "delete" } as Record<string, string>)[
      tool
    ] ?? tool
  );
}

export function inheritHostPermissionGrants(source: object, target: object): void {
  const grants = hostGrants.get(source);
  if (grants) hostGrants.set(target, grants);
}

/** Shell approvals come from the plugin's permission loop, never agent fields. */
export function withHostPermissionGrants(args: Record<string, unknown>, grants: readonly string[]) {
  const { permissions_granted: _untrusted, ...clean } = args;
  hostGrants.set(clean, [...grants]);
  return clean;
}

/** Hard ceiling on sends for one call. The engine reports one refused outside
 * path per attempt, so a batch needs one round per distinct outside path plus
 * the final run; this is far above any real batch. */
export const MAX_OUTSIDE_PERMISSION_ROUNDS = 256;

/** How many times one operation and path may be asked again because the
 * user's approval arrived after its token expired. */
export const MAX_EXPIRED_REASKS_PER_PATH = 2;

/** The engine asked again for outside access that was already approved in
 * this call, or the ceiling was reached: retrying would not make progress. */
export class OutsidePermissionStalled extends Error {
  readonly code = "outside_permission_stalled";
  constructor(message: string) {
    super(message);
    this.name = "OutsidePermissionStalled";
  }
}

/** Present AFT's outside-path requests through the host callback: OpenCode 2
 * binds it to permission.assert, while OpenCode 1 binds it to context.ask. */
export async function withOutsidePermissions<T extends Record<string, unknown>>(
  runtime: Runtime,
  args: Record<string, unknown>,
  send: (args: Record<string, unknown>) => Promise<T>,
  preview = false,
  operation = "",
): Promise<T> {
  const { permissions_granted: _untrusted, ...clean } = args;
  const inherited = hostGrants.get(args) ?? [];
  // The continuation is stored on this call's runtime object, so it already
  // belongs to one tool call. The key adds the operation family and the
  // session, message and call identifiers the host provides, so a
  // continuation never moves to a different operation. It must not include
  // the arguments: a preview and its apply send different arguments
  // (undo_preview's `file`, then safety's `op` and its own path argument), and OpenCode 1
  // gives no call ID, so an argument-based key would never match and the user
  // would be asked twice.
  const key = JSON.stringify([
    permissionOperation(operation, clean),
    runtime.sessionID ?? null,
    runtime.messageID ?? null,
    runtime.id ?? runtime.callID ?? null,
  ]);
  const prior = continuations.get(runtime);
  let granted = prior?.key === key ? [...prior.tokens] : [];
  continuations.delete(runtime);
  // OpenCode 2 cancels through the Effect signal, OpenCode 1 through the
  // call's own `abort`; either one stops an outside-access prompt.
  const signals = [runtime.effectAbort, runtime.abort].filter(
    (signal, index, all): signal is AbortSignal =>
      signal !== undefined && all.indexOf(signal) === index,
  );
  const throwIfCancelled = () => {
    for (const signal of signals) signal.throwIfAborted();
  };
  // What this call has approved so far, by operation and path. A batch keeps
  // retrying while each round approves something new; the engine asking
  // again for something already approved means retrying cannot finish.
  const approvedThisCall = new Set<string>();
  // Re-asks the engine marked `approval_expired`, per operation and path:
  // the user said yes after the token expired, so asking again is progress.
  const expiredReasks = new Map<string, number>();
  // A reconnect can replace the route while its prompt is open. Offer one
  // fresh approval, but do not loop through repeated reconnects in one call.
  const replacedRouteReasks = new Set<string>();
  for (let round = 0; ; round++) {
    // Before an approved retry: a call cancelled while it waited for an
    // answer must not run.
    if (round > 0) throwIfCancelled();
    if (round >= MAX_OUTSIDE_PERMISSION_ROUNDS)
      throw new OutsidePermissionStalled(
        `outside-access permission retry stopped after ${MAX_OUTSIDE_PERMISSION_ROUNDS} rounds`,
      );
    const approvals = [...inherited, ...granted];
    const response = await send(
      approvals.length ? { ...clean, permissions_granted: approvals } : clean,
    );
    granted = Array.isArray(response.permissions_continuation)
      ? response.permissions_continuation.filter(
          (id): id is string => typeof id === "string" && id.startsWith("outside:"),
        )
      : [];
    delete response.permissions_continuation;
    if (response.success !== false || response.code !== "permission_required") {
      if (preview && response.success !== false && granted.length)
        continuations.set(runtime, { key, tokens: granted });
      return response;
    }
    const asks = response.asks as
      | Array<{
          kind: string;
          grant_id?: string;
          patterns?: string[];
          always?: string[];
          path?: string;
          parentDir?: string;
          operation?: string;
          reason?: string;
        }>
      | undefined;
    if (
      !asks?.length ||
      !asks.every(
        (ask) => ask.kind === "external_directory" && ask.grant_id?.startsWith("outside:"),
      )
    ) {
      if (granted.length) continuations.set(runtime, { key, tokens: granted });
      return response;
    }
    if (typeof runtime.ask !== "function")
      throw new Error(
        "Permission denied: outside access requires a permission-capable OpenCode host.",
      );
    // The same operation on the same path is the same request. An ask that
    // names no path can only be told apart by its own grant ID.
    const askKey = (ask: (typeof asks)[number]) =>
      ask.path === undefined
        ? JSON.stringify(["grant", ask.grant_id])
        : JSON.stringify([ask.operation ?? "", ask.path]);
    for (const ask of asks) {
      const key = askKey(ask);
      if (!approvedThisCall.has(key)) continue;
      const where = ask.path ?? ask.parentDir ?? "an outside path";
      const replacedRoute = ask.reason === "approval_route_replaced";
      // A fresh route needs fresh approval; no token crosses the route boundary.
      // Any other refusal, or a second replacement, cannot finish this call.
      if (
        (ask.reason !== "approval_expired" && !replacedRoute) ||
        (replacedRoute && replacedRouteReasks.has(key))
      )
        throw new OutsidePermissionStalled(
          `outside access to ${where} was refused again after it was approved`,
        );
      if (replacedRoute) {
        replacedRouteReasks.add(key);
        continue;
      }
      const reasked = expiredReasks.get(key) ?? 0;
      if (reasked >= MAX_EXPIRED_REASKS_PER_PATH)
        throw new OutsidePermissionStalled(
          `outside access to ${where} kept expiring before it was approved`,
        );
      expiredReasks.set(key, reasked + 1);
    }
    for (const ask of asks) {
      throwIfCancelled();
      try {
        await runAsk(
          runtime.ask({
            permission: "external_directory",
            patterns: ask.patterns ?? [],
            always: ask.always ?? [],
            metadata: { filepath: ask.path, parentDir: ask.parentDir },
          }),
        );
      } catch (error) {
        // The OC2 adapter preserves host denial and step-abort objects.
        if (runtime.hostVersion === 2) throw error;
        const failure = classifyPermissionError(error, "Permission denied (external directory).");
        throw new Error(
          failure.kind === "rule_denied"
            ? permissionRuleDenial(
                "external_directory",
                ask.parentDir ?? ask.path ?? ask.patterns?.[0] ?? "outside path",
              )
            : failure.message,
        );
      }
      // An answer that arrives after cancellation is ignored.
      throwIfCancelled();
      approvedThisCall.add(askKey(ask));
      if (ask.grant_id && !granted.includes(ask.grant_id)) granted.push(ask.grant_id);
    }
  }
}
