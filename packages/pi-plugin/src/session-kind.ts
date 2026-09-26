import type { ExtensionContext } from "@earendil-works/pi-coding-agent";

/**
 * Set to "1" by the pi-magic-context extension in the child `pi --print`
 * processes it launches (historian, dreamer, delegated agents). It is the
 * only signal visible while the extension loads, before Pi hands over any
 * session context.
 */
export const MAGIC_CONTEXT_SUBAGENT_ENV = "MAGIC_CONTEXT_PI_SUBAGENT";

/**
 * Why a Pi session counts as a delegated or headless worker, or `undefined`
 * for a primary session.
 *
 * Pi has no parent-session link comparable to OpenCode's `parentID`, so there
 * are two signals:
 * - `"delegated"`: pi-magic-context marked this process as one of its
 *   children through {@link MAGIC_CONTEXT_SUBAGENT_ENV}.
 * - `"headless"`: the session context reports no UI (`pi -p` or
 *   `--mode json`). Ending the turn ends the process there, so a completion
 *   reminder can never wake the session, which is the situation every worker
 *   behaviour exists for.
 *
 * Interactive and RPC sessions have a UI and are primary. A context without a
 * `hasUI` field (older hosts, tests) is primary unless the environment marks
 * the process, and no context at all (extension load) can only see the
 * environment.
 */
export type PiWorkerKind = "delegated" | "headless";

export function piWorkerKind(
  extCtx: Pick<ExtensionContext, "hasUI"> | undefined,
  env: NodeJS.ProcessEnv = process.env,
): PiWorkerKind | undefined {
  if (env[MAGIC_CONTEXT_SUBAGENT_ENV] === "1") return "delegated";
  if (extCtx?.hasUI === false) return "headless";
  return undefined;
}

/** True for every session {@link piWorkerKind} classifies as a worker. */
export function isPiWorkerSession(
  extCtx: Pick<ExtensionContext, "hasUI"> | undefined,
  env: NodeJS.ProcessEnv = process.env,
): boolean {
  return piWorkerKind(extCtx, env) !== undefined;
}

/**
 * Whether extension load should skip its eager work (warmup bridge, ONNX
 * Runtime preparation, LSP auto-install) and start the bridge on the first
 * AFT tool call instead.
 *
 * Only a delegated child skips. Those children are short-lived and mostly
 * never call an AFT tool, so the eager work is pure cost there. A plain
 * headless `pi -p` run keeps it, for three reasons: nothing tells extension
 * load that the run is headless (`hasUI` only arrives with a session context,
 * and `process.argv` is unreliable under wrappers and embeds); skipping also
 * skips resolving ONNX Runtime, which would leave a scripted run that calls
 * `search` without semantic search; and the exit hang a headless run used to
 * suffer while an install was still running is fixed in the shutdown path
 * itself (`headless-exit.test.ts`), not by avoiding the install.
 */
export function skipsEagerStartup(env: NodeJS.ProcessEnv = process.env): boolean {
  return piWorkerKind(undefined, env) === "delegated";
}
