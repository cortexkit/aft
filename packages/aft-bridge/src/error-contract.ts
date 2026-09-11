/**
 * Host-neutral error adaptation for agent tool failures.
 *
 * The Rust response owns the logical code, message, and structured fields. Host
 * adapters may attach those values to their native error surface, but must not
 * rewrite the contract-owned message while doing so.
 */

import {
  isConsumerReconnectTransient,
  StaleRouteHandleError,
  SubcCallError,
  SubcError,
} from "@cortexkit/subc-client";

import {
  BridgeTransportUnavailableError,
  BridgeTransportUnknownOutcomeError,
  isBridgeTransportTimeout,
} from "./bridge.js";
import {
  isSubcClientClosedError,
  SubcRootGenerationExpiredError,
  SubcRootReapedError,
  SubcTransportShuttingDownError,
} from "./subc-transport.js";

export interface AftToolErrorCause {
  code: string;
  message: string;
  response: Record<string, unknown>;
}

export class AftToolError extends Error {
  readonly code: string;
  readonly response: Record<string, unknown>;
  declare readonly cause: AftToolErrorCause;

  constructor(message: string, code: string, response: Record<string, unknown>) {
    const cause: AftToolErrorCause = { code, message, response };
    super(message, { cause });
    this.name = "AftToolError";
    this.code = code;
    this.response = response;
  }
}

/**
 * Lift a failed bridge response into a host error without losing its logical
 * code or structured response fields.
 */
export function toolErrorFromResponse(
  command: string,
  response: Record<string, unknown>,
): AftToolError {
  const code =
    typeof response.code === "string" && response.code.length > 0 ? response.code : "unknown_error";
  const message =
    typeof response.message === "string" && response.message.length > 0
      ? response.message
      : `${command} failed`;
  return new AftToolError(message, code, response);
}

/** Agent-facing guidance for a bash request whose transport outcome is unknown. */
export const BASH_TRANSPORT_DISPOSITION =
  "The transport to the AFT daemon was interrupted; no background task was created for this command and no task ID exists. Re-run the command. Do not poll bash_status for it.";

/**
 * Agent-facing guidance for a call the daemon GOODBYE'd mid-flight.
 *
 * Deliberately does NOT say the call failed. The daemon emits route GOODBYEs
 * after its drain wait regardless of whether that drain completed, so a call
 * in flight at GOODBYE was admitted BEFORE the gate closed and may already
 * have run to completion with only its reply lost. "Failed" reads as an
 * invitation to re-run, which double-applies a mutation that already landed.
 */
export const SUBC_MODULE_RESTART_DISPOSITION =
  "The AFT daemon module restarted while this call was in flight, so its outcome is UNKNOWN: it may or may not have executed. Verify actual state before re-running, and never blind-retry a mutation.";

/** Agent-facing guidance for a standalone request whose write outcome is unknown. */
export const BRIDGE_TRANSPORT_UNKNOWN_OUTCOME_DISPOSITION =
  "The standalone AFT transport failed after this call may have been sent, so its outcome is UNKNOWN: it may or may not have executed. Verify actual state before re-running, and never blind-retry a mutation.";

/**
 * A route GOODBYE delivered against an in-flight request.
 *
 * COUPLING: subc-client raises this as a bare `SubcError` carrying no code
 * (client.ts, the `FrameType.Goodbye` branch), so the message literal is the
 * only discriminator available. Asked upstream for a stable `code` on that
 * error; until it exists this match is the seam and will fail open (no
 * disposition appended) rather than misclassify.
 */
function isRouteGoodbyeError(error: unknown): boolean {
  if (!(error instanceof SubcError)) return false;
  // Two client generations mint the GOODBYE failure differently: the shipped
  // 0.5.0 line throws it BARE (no code), while the current subc-client source
  // stamps code "route_closed". Match both so a client upgrade cannot
  // silently stop the unknown-outcome disposition from being appended — a
  // false negative here recreates the blind re-run this contract exists to
  // prevent. "route closed by closeRoute" (same code in newer clients) is a
  // deliberate local close, not a daemon GOODBYE: outcome-known, excluded by
  // the message check on the coded arm.
  if (error.code === undefined) {
    return error.message.includes("route closed by subc");
  }
  return error.code === "route_closed" && error.message.includes("route closed by subc");
}

/**
 * True for the daemon's temporary route.open refusals while an AFT module is
 * restarting. `module_reloading` is the fast refusal; `module_warming` is the
 * newer supervision-state response; `target_unavailable` covers the legacy
 * `live=false` response and a bind relay that closes before its acknowledgement.
 *
 * This classifier is only used for route.open rejections: route.open rejections
 * are provably pre-send AS A CLASS (the daemon refuses-or-assigns atomically;
 * forwarding requires the channel the rejection withholds). The stable code is
 * therefore sufficient here and no message prose participates in the match.
 */
export function isRouteOpenReloadWindowError(error: unknown): boolean {
  if (error === null || typeof error !== "object") return false;
  const code = (error as { code?: unknown }).code;
  return code === "module_reloading" || code === "module_warming" || code === "target_unavailable";
}

function hasEngineResponse(error: Error): boolean {
  const response = (error as Error & { response?: unknown }).response;
  if (response !== null && typeof response === "object") return true;

  const cause = error.cause;
  if (cause === null || typeof cause !== "object") return false;
  const causeResponse = (cause as { response?: unknown }).response;
  return causeResponse !== null && typeof causeResponse === "object";
}

const SUBC_MODULE_DOWN_CODES = new Set([
  "unknown_module",
  "module_reloading",
  "module_warming",
  "target_unavailable",
]);

export type BashHostFallbackCause =
  | "module down"
  | "bind timed out"
  | "route closed before dispatch"
  | "transport down";

function classifySubcPreDispatchError(error: Error): BashHostFallbackCause | undefined {
  if (!(error instanceof SubcError) && !(error instanceof SubcCallError)) return undefined;
  if (error instanceof SubcCallError && error.kind !== "not_sent") return undefined;

  if (error.code === "module_timeout") return "bind timed out";
  if (SUBC_MODULE_DOWN_CODES.has(error.code ?? "")) return "module down";
  if (error instanceof SubcCallError && error.code === "route_closed") {
    return "route closed before dispatch";
  }
  return undefined;
}

/**
 * Classify failures that prove bash never reached AFT, including daemon replies
 * that reject route binding before a module channel exists. A structured engine
 * response always wins because it proves AFT executed enough of the request to
 * return a logical result.
 *
 * A raw `SubcError("route closed by closeRoute", "route_closed")` is intentionally
 * absent. The raw request path creates it after a request is pending but exposes
 * no queued/write marker, so its outcome may be unknown. A future transport can
 * safely admit that case by using subc-client's managed request path and passing
 * through `SubcCallError.kind === "not_sent"`.
 */
export function classifyBashHostFallbackError(error: unknown): BashHostFallbackCause | undefined {
  if (!(error instanceof Error) || hasEngineResponse(error) || isRouteGoodbyeError(error)) {
    return undefined;
  }
  if (error instanceof BridgeTransportUnknownOutcomeError) return undefined;
  if (error instanceof SubcTransportShuttingDownError) return "transport down";

  const subcPreDispatch = classifySubcPreDispatchError(error);
  if (subcPreDispatch !== undefined) return subcPreDispatch;
  if (error instanceof SubcCallError) {
    return error.kind === "not_sent" ? "transport down" : undefined;
  }
  if (error instanceof StaleRouteHandleError) return "route closed before dispatch";

  return error instanceof BridgeTransportUnavailableError ||
    isConsumerReconnectTransient(error) ||
    isSubcClientClosedError(error) ||
    error instanceof SubcRootGenerationExpiredError ||
    error instanceof SubcRootReapedError
    ? "transport down"
    : undefined;
}

/** True only when bash could not reach a live AFT engine. */
export function isBashTransportDeadError(error: unknown): error is Error {
  return classifyBashHostFallbackError(error) !== undefined;
}

function isTransportClassError(error: unknown): boolean {
  return isBridgeTransportTimeout(error) || isBashTransportDeadError(error);
}

/**
 * Append agent-facing recovery guidance without changing the original error
 * object, class, code, or retry behavior. Unknown-outcome errors append safety
 * guidance on every command; proven bash transport failures append re-run
 * guidance. Commands matching neither retain their errors untouched.
 */
export function adaptToolError(command: string, error: unknown): unknown {
  if (!(error instanceof Error)) return error;

  if (error instanceof BridgeTransportUnknownOutcomeError) {
    if (error.message.includes(BRIDGE_TRANSPORT_UNKNOWN_OUTCOME_DISPOSITION)) return error;
    error.message = error.message
      ? `${error.message} ${BRIDGE_TRANSPORT_UNKNOWN_OUTCOME_DISPOSITION}`
      : BRIDGE_TRANSPORT_UNKNOWN_OUTCOME_DISPOSITION;
    return error;
  }

  // Checked before the bash branch, and applied to every command: a GOODBYE'd
  // call has an unknown outcome, so BASH_TRANSPORT_DISPOSITION's "no task was
  // created, re-run the command" would be actively wrong here.
  if (isRouteGoodbyeError(error)) {
    if (error.message.includes(SUBC_MODULE_RESTART_DISPOSITION)) return error;
    error.message = error.message
      ? `${error.message} ${SUBC_MODULE_RESTART_DISPOSITION}`
      : SUBC_MODULE_RESTART_DISPOSITION;
    return error;
  }

  if (command !== "bash" || !isTransportClassError(error)) return error;
  if (error.message.includes(BASH_TRANSPORT_DISPOSITION)) return error;
  error.message = error.message
    ? `${error.message} ${BASH_TRANSPORT_DISPOSITION}`
    : BASH_TRANSPORT_DISPOSITION;
  return error;
}
