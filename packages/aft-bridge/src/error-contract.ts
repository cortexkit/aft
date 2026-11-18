/**
 * Host-neutral error adaptation for agent tool failures.
 *
 * The Rust response owns the logical code, message, and structured fields. Host
 * adapters may attach those values to their native error surface, but must not
 * rewrite the contract-owned message while doing so.
 */

import {
  isConsumerReconnectTransient,
  SocketClosedError,
  StaleRouteHandleError,
  SubcCallError,
  SubcError,
} from "@cortexkit/subc-client";

import {
  BridgeTransportUnavailableError,
  BridgeTransportUnknownOutcomeError,
  isBridgeTransportTimeout,
  type ShellTimeoutOutcome,
} from "./bridge.js";
import {
  isDispatchedRequestDeadlineError,
  isLocalRouteCloseRejection,
  isSubcClientClosedError,
  isSubcFrameReadTimeoutError,
  isSubcRouteTornDownError,
  SubcFrameReadTimeoutError,
  SubcRootGenerationExpiredError,
  SubcRootReapedError,
  SubcRouteClosedMidCallError,
  SubcTransportShuttingDownError,
} from "./subc-transport.js";

export { isSubcFrameReadTimeoutError, SubcFrameReadTimeoutError };

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
 * Agent-facing guidance for a shell call that timed out at the standalone
 * bridge when the bridge could not learn whether AFT started the command: no
 * reply came even after the grace period. It must not claim that no task
 * exists, because AFT may still be starting or running the command.
 */
export const SHELL_TIMEOUT_UNKNOWN_DISPOSITION =
  "AFT did not answer before the deadline, so whether this command started is unknown. The command may not have started; check bash_status before rerunning.";

/** Guidance for a timed-out shell call whose late reply named its task. */
export function shellTimeoutTaskDisposition(taskId: string): string {
  return `AFT started this command as task ${taskId}, but its reply arrived after the deadline. Use bash_status or bash_watch with task ID ${taskId} to follow it; do not re-run the command.`;
}

/** Guidance for a timed-out shell call whose late reply was an error without a task. */
export function shellTimeoutNotStartedDisposition(code?: string, message?: string): string {
  const answer = [code, message].filter((part) => part !== undefined && part !== "").join(": ");
  return `AFT answered after the deadline that it did not start this command${answer ? ` (${answer})` : ""}; no task ID was assigned.`;
}

function shellTimeoutDisposition(outcome: ShellTimeoutOutcome | undefined): string {
  switch (outcome?.kind) {
    case "task_started":
      return shellTimeoutTaskDisposition(outcome.taskId);
    case "not_started":
      return shellTimeoutNotStartedDisposition(outcome.code, outcome.message);
    default:
      return SHELL_TIMEOUT_UNKNOWN_DISPOSITION;
  }
}

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

const RELEASED_LOCATION_DISPOSITION =
  "AFT's plugin reloaded while this call was waiting, so the call was not sent. Nothing was started; it is safe to retry now.";

/**
 * Agent-facing guidance for a call whose route the plugin closed while the
 * request was already in flight (transport shutdown, session close, project
 * root close, or the session's route being replaced). The request had been
 * handed to the socket, so it may have run even though no reply came back.
 */
export const SUBC_ROUTE_CLOSED_MID_CALL_DISPOSITION =
  "The plugin closed this call's route while the call was in flight, so its outcome is UNKNOWN: it may or may not have executed. Verify actual state before re-running, and never blind-retry a mutation.";

/** Guidance for a dispatched tool call whose subc socket closed before its reply. */
export const SUBC_CONNECTION_CLOSED_MID_CALL_DISPOSITION =
  "The AFT module connection closed while this request was in flight, so its outcome is UNKNOWN: it may or may not have executed. Verify actual state before retrying, and never blindly retry a mutation.";

/** Bash keeps running inside AFT even after its caller loses the connection. */
export function bashConnectionClosedDisposition(taskId?: string): string {
  const taskFollowup = taskId
    ? `Use bash_status or bash_watch with task ID ${taskId} to check it.`
    : "If a task ID is available, use it with bash_status or bash_watch; otherwise wait for the completion notice.";
  return `The command may have run or may still be running. AFT keeps commands running when the connection drops; a completion notice will follow if it ran. ${taskFollowup} Do not rerun it.`;
}

/** A typed refusal for calls that never reached the AFT module. */
export class AftModuleUnavailableError extends SubcCallError {
  constructor(message: string, cause: unknown) {
    super("not_sent", message, "aft_module_unavailable", cause);
    this.name = "AftModuleUnavailableError";
  }
}

/** Build a refusal for a bash option the host fallback cannot reproduce. */
export function refuseAftModuleOptionWithoutFallback(
  error: unknown,
  option: string,
  refusalPrefix: string,
): AftModuleUnavailableError {
  return new AftModuleUnavailableError(
    `${refusalPrefix}; ${option} has no host fallback. Nothing was started, so it is safe to retry after the AFT module connection is restored.`,
    error,
  );
}

/**
 * Agent-facing guidance for a bash call whose response frame timed out
 * mid-read (the frame header arrived, but the body never followed). The
 * command was dispatched to the daemon and may have executed or started a
 * background task.
 */
export const BASH_FRAME_READ_TIMEOUT_DISPOSITION =
  "The command's state is UNKNOWN: the transport timed out while reading the response frame, so it may or may not have executed. Check bash_status if a task ID could have been assigned, and do not re-run side-effecting commands blindly.";

/**
 * Agent-facing guidance for a tool call whose response frame timed out
 * mid-read. The request was handed to the socket and dispatched to the
 * daemon, so it may have executed.
 */
export const SUBC_FRAME_READ_TIMEOUT_DISPOSITION =
  "The transport timed out while reading the response frame for this call, so its outcome is UNKNOWN: it may or may not have executed. Verify actual state before re-running, and never blind-retry a mutation.";

/**
 * A request that was in flight when the plugin closed its own route: the
 * subc pool's named error, or subc-client's bare rejection when it reaches here
 * without passing through the pool. subc-client raises that rejection only for
 * a request already pending on the route, so either shape has an unknown outcome.
 */
function isRouteClosedMidCallError(error: unknown): boolean {
  return error instanceof SubcRouteClosedMidCallError || isLocalRouteCloseRejection(error);
}

function isSubcConnectionClosedMidCallError(
  error: unknown,
): error is Error & { kind: "outcome_unknown" } {
  if (!(error instanceof Error)) return false;
  const fields = error as Error & { kind?: unknown };
  if (fields.kind !== "outcome_unknown") return false;
  if (error instanceof SocketClosedError) return true;
  if (error instanceof SubcError) return error.closeReason === "connection_lost";
  if (error instanceof SubcCallError) {
    return error.closeReason === "connection_lost" || hasConnectionLossCause(error);
  }
  return hasConnectionLossCause(error);
}

const CONNECTION_LOSS_ERROR_CODES = new Set(["ECONNABORTED", "ECONNRESET", "EPIPE"]);

function hasConnectionLossCause(error: unknown, depth = 0): boolean {
  if (error instanceof SocketClosedError) return true;
  if (error instanceof SubcError && error.closeReason === "connection_lost") return true;
  if (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    typeof error.code === "string" &&
    CONNECTION_LOSS_ERROR_CODES.has(error.code)
  ) {
    return true;
  }
  if (!(error instanceof Error) || depth >= 4 || error.cause === undefined) return false;
  return hasConnectionLossCause(error.cause, depth + 1);
}

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
  // prevent. A route_closed error the plugin caused by closing its own route
  // (closeReason "closed_by_caller") is not a daemon GOODBYE; it gets its own
  // unknown-outcome guidance (see isRouteClosedMidCallError), so the message
  // check on the coded arm keeps it out of the module-restart wording.
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

/**
 * True for the daemon's refusal of a data-plane request on an ALREADY-BOUND
 * route while the module behind it drains for a reload.
 *
 * Wire shape: an ERROR frame on the route's own channel with code
 * `module_reloading` and message `module endpoint for route channel <N> is
 * reloading`; the raw `SubcClient.request` path rejects it as a plain
 * `SubcError(message, "module_reloading")`. The daemon's router emits it when
 * the route's request-credit acquisition fails because the module endpoint is
 * draining, i.e. BEFORE the frame is spliced to the module, so the request
 * provably never reached AFT and resending it on a fresh route cannot execute
 * it twice.
 *
 * Deliberately limited to `module_reloading`: the other route.open reload codes
 * are never emitted on the data plane, and anything outcome-unknown (a GOODBYE
 * or a socket drop after the request was written) carries different codes or a
 * `SubcCallError` kind other than `not_sent`.
 */
export function isRouteRequestReloadRefusal(error: unknown): boolean {
  if (error instanceof SubcCallError) {
    return error.kind === "not_sent" && error.code === "module_reloading";
  }
  return error instanceof SubcError && error.code === "module_reloading";
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
 * A request rejected by a local `closeRoute` is intentionally absent: subc-client
 * rejects only requests already pending on the route, and it starts writing a
 * request's frame as it records it pending, so the request may have run. The
 * subc pool reports it as `SubcRouteClosedMidCallError` (outcome unknown). A
 * teardown that stopped the call before its request reached the client
 * (`isSubcRouteTornDownError`, or `StaleRouteHandleError` from the client) is
 * provably pre-dispatch.
 */
export function classifyBashHostFallbackError(error: unknown): BashHostFallbackCause | undefined {
  if (!(error instanceof Error) || hasEngineResponse(error) || isRouteGoodbyeError(error)) {
    return undefined;
  }
  if (isSubcConnectionClosedMidCallError(error)) return undefined;
  if (error instanceof BridgeTransportUnknownOutcomeError) return undefined;
  if (isRouteClosedMidCallError(error)) return undefined;
  if (isSubcFrameReadTimeoutError(error)) return undefined;
  if (error instanceof SubcTransportShuttingDownError) return "transport down";
  if (isSubcRouteTornDownError(error)) return "route closed before dispatch";

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

/**
 * Attach recovery guidance to unknown outcomes without changing the error's
 * class, code or retry behavior. A tool call that never reached the AFT module
 * gets a typed, safe-to-retry refusal, since nothing ran; other errors keep the
 * existing adaptation.
 */
export function adaptToolError(command: string, error: unknown): unknown {
  if (!(error instanceof Error)) return error;
  if (error instanceof AftModuleUnavailableError) return error;

  if (isSubcFrameReadTimeoutError(error)) {
    const typedError =
      error instanceof SubcFrameReadTimeoutError ? error : new SubcFrameReadTimeoutError(error);
    const disposition =
      command === "bash"
        ? BASH_FRAME_READ_TIMEOUT_DISPOSITION
        : SUBC_FRAME_READ_TIMEOUT_DISPOSITION;
    if (typedError.message.includes(disposition)) return typedError;
    typedError.message = typedError.message ? `${typedError.message} ${disposition}` : disposition;
    return typedError;
  }

  if (error instanceof BridgeTransportUnknownOutcomeError) {
    if (error.message.includes(BRIDGE_TRANSPORT_UNKNOWN_OUTCOME_DISPOSITION)) return error;
    error.message = error.message
      ? `${error.message} ${BRIDGE_TRANSPORT_UNKNOWN_OUTCOME_DISPOSITION}`
      : BRIDGE_TRANSPORT_UNKNOWN_OUTCOME_DISPOSITION;
    return error;
  }

  // The request boundary marks a raw SocketClosedError only after client.request
  // was called. Wrapped client errors retain that class in their cause; route-open
  // failures remain unmarked because no tool request was sent.
  if (isSubcConnectionClosedMidCallError(error)) {
    const disposition =
      command === "bash"
        ? bashConnectionClosedDisposition(taskIdFromError(error))
        : SUBC_CONNECTION_CLOSED_MID_CALL_DISPOSITION;
    if (error.message.includes(disposition)) return error;
    error.message = error.message ? `${error.message} ${disposition}` : disposition;
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

  // Also ahead of the bash branch, for the same reason: the request was in
  // flight when the plugin closed its route, so it may already have run.
  if (isRouteClosedMidCallError(error)) {
    if (error.message.includes(SUBC_ROUTE_CLOSED_MID_CALL_DISPOSITION)) return error;
    error.message = error.message
      ? `${error.message} ${SUBC_ROUTE_CLOSED_MID_CALL_DISPOSITION}`
      : SUBC_ROUTE_CLOSED_MID_CALL_DISPOSITION;
    return error;
  }

  // subc-client's own deadline on a bash request that was already sent: AFT
  // may have started the command, and AFT keeps a started command running, so
  // the caller gets the same do-not-rerun guidance as for a dropped connection
  // instead of a bare timeout that reads as a failure to retry.
  if (command === "bash" && isDispatchedRequestDeadlineError(error)) {
    const disposition = bashConnectionClosedDisposition(taskIdFromError(error));
    if (error.message.includes(disposition)) return error;
    error.message = error.message ? `${error.message} ${disposition}` : disposition;
    return error;
  }

  // A timed-out request was sent, so AFT may have started the command. The
  // re-run text below ("no task was created") would be a guess presented as a
  // fact; the bridge's late-reply outcome says what is actually known.
  if ((command === "bash" || command === "powershell") && isBridgeTransportTimeout(error)) {
    const outcome = (error as { shellOutcome?: ShellTimeoutOutcome }).shellOutcome;
    const disposition = shellTimeoutDisposition(outcome);
    if (error.message.includes(disposition)) return error;
    error.message = error.message ? `${error.message} ${disposition}` : disposition;
    return error;
  }

  if (command !== "bash" && isBashTransportDeadError(error)) {
    return new AftModuleUnavailableError(
      error instanceof BridgeTransportUnavailableError &&
        error.message === "AFT bridge Location lease has already been released"
        ? RELEASED_LOCATION_DISPOSITION
        : "The AFT module connection is down or unavailable; this AFT-backed option has no host fallback. Nothing was started, so it is safe to retry after the connection is restored.",
      error,
    );
  }

  if (command !== "bash" || !isBashTransportDeadError(error)) return error;
  if (error.message.includes(BASH_TRANSPORT_DISPOSITION)) return error;
  error.message = error.message
    ? `${error.message} ${BASH_TRANSPORT_DISPOSITION}`
    : BASH_TRANSPORT_DISPOSITION;
  return error;
}

function taskIdFromError(error: Error): string | undefined {
  for (
    let current: unknown = error, depth = 0;
    current instanceof Error && depth < 5;
    current = current.cause, depth += 1
  ) {
    const fields = current as Error & { taskId?: unknown; task_id?: unknown };
    const taskId = fields.taskId ?? fields.task_id;
    if (typeof taskId === "string" && taskId.length > 0) return taskId;
  }
  return undefined;
}
