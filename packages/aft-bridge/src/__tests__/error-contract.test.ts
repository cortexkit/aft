/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import {
  type RouteHandle,
  SocketClosedError,
  SocketTimeoutError,
  StaleRouteHandleError,
  SubcCallError,
  SubcError,
} from "@cortexkit/subc-client";
import {
  BridgeTransportTimeoutError,
  BridgeTransportUnavailableError,
  BridgeTransportUnknownOutcomeError,
  isBridgeTransportTimeout,
} from "../bridge.js";
import {
  AftToolError,
  adaptToolError,
  BASH_FRAME_READ_TIMEOUT_DISPOSITION,
  BASH_TRANSPORT_DISPOSITION,
  BRIDGE_TRANSPORT_UNKNOWN_OUTCOME_DISPOSITION,
  classifyBashHostFallbackError,
  isBashTransportDeadError,
  isRouteOpenReloadWindowError,
  isSubcFrameReadTimeoutError,
  SHELL_TIMEOUT_UNKNOWN_DISPOSITION,
  SUBC_CONNECTION_CLOSED_MID_CALL_DISPOSITION,
  SUBC_FRAME_READ_TIMEOUT_DISPOSITION,
  SUBC_MODULE_RESTART_DISPOSITION,
  SUBC_ROUTE_CLOSED_MID_CALL_DISPOSITION,
  shellTimeoutNotStartedDisposition,
  shellTimeoutTaskDisposition,
  toolErrorFromResponse,
} from "../error-contract.js";
import {
  isLocalRouteCloseRejection,
  SubcFrameReadTimeoutError,
  SubcRootGenerationExpiredError,
  SubcRootReapedError,
  SubcRouteClosedMidCallError,
  SubcTransportShuttingDownError,
} from "../subc-transport.js";

test("toolErrorFromResponse preserves unsafe_temp_path and its exact refusal message", () => {
  const response = {
    success: false,
    code: "unsafe_temp_path",
    message:
      "temporary path '/tmp/foreign' is another session's scratch folder; AFT changes only this session's scratch folders",
    text: "rendered refusal",
  };
  const error = toolErrorFromResponse("delete", response);
  expect(error).toBeInstanceOf(AftToolError);
  expect(error.code).toBe("unsafe_temp_path");
  expect(error.message).toBe(response.message);
  expect(error.response).toBe(response);
  expect(error.cause).toEqual({ code: "unsafe_temp_path", message: response.message, response });
});

/** Shaped exactly as subc-client raises it: bare SubcError, no code. */
function routeGoodbyeError(): SubcError {
  return new SubcError("route closed by subc (GOODBYE)");
}

/** Mirror subc-client's daemon Error-frame decoding into its public typed error. */
function daemonErrorBody(code: string, message: string): SubcError {
  return new SubcError(message, code);
}

describe("isBashTransportDeadError", () => {
  const transportDead: Array<[string, Error]> = [
    ["bridge spawn failure", new BridgeTransportUnavailableError("spawn failed")],
    ["standalone bridge shutdown", new BridgeTransportUnavailableError("Bridge is shutting down")],
    [
      "released Location lease before dispatch",
      new BridgeTransportUnavailableError("lease released"),
    ],
    ["subc transport shutdown", new SubcTransportShuttingDownError()],
    ["managed request rejected before send", new SubcCallError("not_sent", "client down")],
    ["subc client closed before send", new SubcError("client closed")],
    [
      "stale route after retries",
      new StaleRouteHandleError({ channel: 1, epoch: 1 } as RouteHandle),
    ],
    [
      "root generation expired",
      new SubcRootGenerationExpiredError({
        canonicalRoot: "/project",
        expectedGeneration: 1,
      } as never),
    ],
    [
      "root reaped",
      new SubcRootReapedError({ canonicalRoot: "/project", expectedGeneration: 1 } as never),
    ],
    ...(["ECONNREFUSED", "ECONNRESET", "EPIPE", "ETIMEDOUT", "ENOENT"] as const).map(
      (code): [string, Error] => [
        `node transport ${code}`,
        Object.assign(new Error(`transport failed: ${code}`), { code }),
      ],
    ),
  ];

  for (const [name, error] of transportDead) {
    test(`accepts transport-dead shape: ${name}`, () => {
      expect(isBashTransportDeadError(error)).toBe(true);
    });
  }

  test("accepts the module-unavailable daemon error body as module down", () => {
    const error = daemonErrorBody(
      "module_warming",
      "module_id 'aft' is supervised but not available (state=stopped, enabled=true, live=false) The AFT daemon module did not return within the 15s reload window.",
    );

    expect(classifyBashHostFallbackError(error)).toBe("module down");
    expect(isBashTransportDeadError(error)).toBe(true);
  });

  test("accepts the route.bind timeout daemon error body as bind timed out", () => {
    const error = daemonErrorBody(
      "module_timeout",
      "module_id 'aft' did not answer route.bind within 12s",
    );

    expect(classifyBashHostFallbackError(error)).toBe("bind timed out");
    expect(isBashTransportDeadError(error)).toBe(true);
  });

  test("route-closed fallback requires proof that dispatch never started", () => {
    const beforeDispatch = new StaleRouteHandleError({
      channel: 1,
      epoch: 1,
    } as RouteHandle);
    const whilePending = new SubcError(
      "route closed by closeRoute",
      "route_closed",
      undefined,
      "closed_by_caller",
    );

    expect(classifyBashHostFallbackError(beforeDispatch)).toBe("route closed before dispatch");
    expect(isBashTransportDeadError(beforeDispatch)).toBe(true);
    expect(classifyBashHostFallbackError(whilePending)).toBeUndefined();
    expect(isBashTransportDeadError(whilePending)).toBe(false);
  });

  test("rejects a request in flight when the plugin closed its route", () => {
    const error = new SubcRouteClosedMidCallError(
      "session closed",
      new SubcError("route closed by closeRoute", "route_closed", undefined, "closed_by_caller"),
    );

    expect(classifyBashHostFallbackError(error)).toBeUndefined();
    expect(isBashTransportDeadError(error)).toBe(false);
  });

  test("rejects a managed outcome-unknown call because dispatch may have executed", () => {
    const error = new SubcCallError(
      "outcome_unknown",
      "connection dropped after request bytes were queued",
    );

    expect(classifyBashHostFallbackError(error)).toBeUndefined();
    expect(isBashTransportDeadError(error)).toBe(false);
  });

  test("lifecycle outcome-unknown rejections never enable bash fallback", () => {
    for (const error of [
      new BridgeTransportUnknownOutcomeError("Bridge shutting down"),
      new BridgeTransportUnknownOutcomeError("Bridge restarting with updated binary: /new/aft"),
    ]) {
      expect(classifyBashHostFallbackError(error)).toBeUndefined();
      expect(isBashTransportDeadError(error)).toBe(false);
    }
  });

  test("rejects a post-execution AFT error response", () => {
    const error = new AftToolError("permission required", "permission_required", {
      success: false,
      code: "permission_required",
    });

    expect(classifyBashHostFallbackError(error)).toBeUndefined();
    expect(isBashTransportDeadError(error)).toBe(false);
  });

  const engineAlive: Array<[string, Error]> = [
    [
      "permission response",
      new AftToolError("permission required", "permission_required", {
        success: false,
        code: "permission_required",
      }),
    ],
    [
      "outside-root response",
      new AftToolError("outside root", "path_outside_root", {
        success: false,
        code: "path_outside_root",
      }),
    ],
    [
      "invalid request response",
      new AftToolError("invalid", "invalid_request", {
        success: false,
        code: "invalid_request",
      }),
    ],
    [
      "host adapter response wrapper",
      Object.assign(new Error("module_reloading"), {
        code: "module_reloading",
        response: { success: false, code: "module_reloading" },
      }),
    ],
    ["route GOODBYE with unknown outcome", routeGoodbyeError()],
    [
      "standalone write with unknown outcome",
      new BridgeTransportUnknownOutcomeError("write failed"),
    ],
    [
      "lifecycle shutdown after dispatch",
      new BridgeTransportUnknownOutcomeError("Bridge shutting down"),
    ],
    [
      "binary replacement after dispatch",
      new BridgeTransportUnknownOutcomeError("Bridge restarting with updated binary: /new/aft"),
    ],
    ["live bridge request timeout", new BridgeTransportTimeoutError("bash", 100, "bridge busy")],
    [
      "client-closed text with an engine response",
      Object.assign(new SubcError("client closed"), {
        response: { success: false, code: "client_closed_by_engine" },
      }),
    ],
    ["ordinary tool failure", new Error("command failed")],
  ];

  for (const [name, error] of engineAlive) {
    test(`rejects engine-alive shape: ${name}`, () => {
      expect(isBashTransportDeadError(error)).toBe(false);
    });
  }
});

describe("isRouteOpenReloadWindowError", () => {
  test.each([
    [
      "fast module reload refusal",
      new SubcError("module_id 'aft' is reloading", "module_reloading"),
    ],
    [
      "warming supervisor refusal",
      Object.assign(new SubcError("module supervised but not available", "module_warming"), {
        state: "running",
        enabled: true,
        live: false,
      }),
    ],
    [
      "legacy not-live supervisor refusal",
      Object.assign(new SubcError("module supervised but not available", "target_unavailable"), {
        state: "running",
        enabled: true,
        live: false,
      }),
    ],
    [
      "bind relay closed before acknowledgement",
      new SubcError("connection closed during route.bind relay", "target_unavailable"),
    ],
  ])("accepts pre-send route.open reload shape: %s", (_name, error) => {
    expect(isRouteOpenReloadWindowError(error)).toBe(true);
  });

  test("rejects a route GOODBYE because its request outcome is unknown", () => {
    expect(isRouteOpenReloadWindowError(routeGoodbyeError())).toBe(false);
  });
});

describe("adaptToolError", () => {
  // A bridge timeout means the request was sent, so AFT may have started the
  // command. The guidance therefore comes from what the bridge learned after
  // the deadline, never from the "no task was created, re-run" text.
  test("a bash timeout with no late reply admits the outcome is unknown, preserving the error", () => {
    const original = new BridgeTransportTimeoutError("bash", 11_000, "bash transport timed out", {
      kind: "unknown",
    });

    let thrown: unknown;
    try {
      throw original;
    } catch (error) {
      thrown = adaptToolError("bash", error);
    }

    expect(thrown).toBe(original);
    expect(original.message).toBe(`bash transport timed out ${SHELL_TIMEOUT_UNKNOWN_DISPOSITION}`);
    expect(original.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
    expect(isBridgeTransportTimeout(original)).toBe(true);
  });

  test("a bash timeout without any outcome also gets the unknown text", () => {
    const original = new BridgeTransportTimeoutError("bash", 11_000, "bash transport timed out");

    adaptToolError("bash", original);

    expect(original.message).toContain(SHELL_TIMEOUT_UNKNOWN_DISPOSITION);
    expect(original.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
  });

  test("a bash timeout whose late reply named a task names that task", () => {
    for (const command of ["bash", "powershell"]) {
      const original = new BridgeTransportTimeoutError(command, 11_000, "timed out", {
        kind: "task_started",
        taskId: "bash-123",
      });

      adaptToolError(command, original);
      adaptToolError(command, original);

      expect(original.message).toBe(`timed out ${shellTimeoutTaskDisposition("bash-123")}`);
      expect(original.message).toContain("Use bash_status or bash_watch with task ID bash-123");
    }
  });

  test("a bash timeout whose late reply was an error reports that answer", () => {
    const original = new BridgeTransportTimeoutError("bash", 11_000, "timed out", {
      kind: "not_started",
      code: "permission_required",
      message: "bash command requires permission",
    });

    adaptToolError("bash", original);

    expect(original.message).toBe(
      `timed out ${shellTimeoutNotStartedDisposition("permission_required", "bash command requires permission")}`,
    );
    expect(original.message).toContain("no task ID was assigned");
  });

  test("does not add bash transport guidance to non-bash commands", () => {
    const original = new BridgeTransportTimeoutError("read", 11_000, "read transport timed out");

    const adapted = adaptToolError("read", original);

    expect(adapted).toBe(original);
    expect(original.message).toBe("read transport timed out");
    expect(original.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
  });

  test("a route GOODBYE reports an UNKNOWN outcome, never a failure", () => {
    const original = routeGoodbyeError();

    const adapted = adaptToolError("write", original);

    expect(adapted).toBe(original);
    expect(original.message).toContain(SUBC_MODULE_RESTART_DISPOSITION);
    // The wording is the safety property: an operator or agent that reads
    // "failed" re-runs the call, which double-applies a mutation that may
    // already have landed before the daemon dropped the reply.
    expect(original.message).toContain("UNKNOWN");
    expect(original.message).toContain("never blind-retry a mutation");
    expect(original.message).not.toContain("Re-run the command.");
  });

  test("a GOODBYE'd bash call gets the unknown-outcome text, not the re-run text", () => {
    // BASH_TRANSPORT_DISPOSITION asserts no task was created and says to re-run.
    // That is true for a not-sent transport failure and FALSE for a GOODBYE,
    // where the command may already have executed.
    const original = routeGoodbyeError();

    adaptToolError("bash", original);

    expect(original.message).toContain(SUBC_MODULE_RESTART_DISPOSITION);
    expect(original.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
  });

  test("an outcome-unknown standalone write never gets bash re-run guidance", () => {
    const original = new BridgeTransportUnknownOutcomeError("stdin write failed");

    adaptToolError("bash", original);

    expect(original.message).toContain(BRIDGE_TRANSPORT_UNKNOWN_OUTCOME_DISPOSITION);
    expect(original.message).toContain("UNKNOWN");
    expect(original.message).toContain("never blind-retry a mutation");
    expect(original.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
  });

  test("a lifecycle shutdown mid-call gets the unknown-outcome disposition", () => {
    const original = new BridgeTransportUnknownOutcomeError("[aft-plugin] Bridge shutting down");

    adaptToolError("bash", original);

    expect(original.message).toContain(BRIDGE_TRANSPORT_UNKNOWN_OUTCOME_DISPOSITION);
    expect(original.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
  });

  test("disposition is appended once when the error passes through twice", () => {
    const original = routeGoodbyeError();

    adaptToolError("read", original);
    adaptToolError("read", original);

    const occurrences = original.message.split(SUBC_MODULE_RESTART_DISPOSITION).length - 1;
    expect(occurrences).toBe(1);
  });

  test("a dispatched bash connection close names an available task and forbids reruns", () => {
    const original = Object.assign(new SocketClosedError("subc closed the connection"), {
      kind: "outcome_unknown" as const,
      taskId: "bash-known-task",
    });

    const adapted = adaptToolError("bash", original) as Error;

    expect(adapted).toBe(original);
    expect(adapted.message).toContain("The command may have run or may still be running");
    expect(adapted.message).toContain("AFT keeps commands running when the connection drops");
    expect(adapted.message).toContain("a completion notice will follow if it ran");
    expect(adapted.message).toContain("task ID bash-known-task");
    expect(adapted.message).toContain("Do not rerun it");
    expect(adapted.message).not.toContain("Re-run the command.");
  });

  test("a dispatched non-bash connection close gets the generic unknown-outcome guidance", () => {
    const original = Object.assign(new SocketClosedError("subc closed the connection"), {
      kind: "outcome_unknown" as const,
    });

    const adapted = adaptToolError("read", original) as Error;

    expect(adapted).toBe(original);
    expect(adapted.message).toContain(SUBC_CONNECTION_CLOSED_MID_CALL_DISPOSITION);
  });

  test("a subc-wrapped socket close is classified by SocketClosedError, not prose", () => {
    const original = new SubcCallError(
      "outcome_unknown",
      "unrelated wording",
      undefined,
      new SocketClosedError("different socket wording"),
    );

    const adapted = adaptToolError("read", original) as Error;

    expect(adapted).toBe(original);
    expect(adapted.message).toContain(SUBC_CONNECTION_CLOSED_MID_CALL_DISPOSITION);
  });

  test("a connection_lost SubcError gets guidance from its typed close reason", () => {
    const original = Object.assign(
      new SubcError("unrelated wording", "ECONNRESET", undefined, "connection_lost"),
      { kind: "outcome_unknown" as const },
    );

    const adapted = adaptToolError("read", original) as Error;

    expect(adapted).toBe(original);
    expect(adapted.message).toContain(SUBC_CONNECTION_CLOSED_MID_CALL_DISPOSITION);
  });

  test("a dispatched reset code gets guidance without matching error prose", () => {
    const original = Object.assign(new Error("unrelated wording"), {
      code: "ECONNRESET",
      kind: "outcome_unknown" as const,
    });

    const adapted = adaptToolError("read", original) as Error;

    expect(adapted).toBe(original);
    expect(adapted.message).toContain(SUBC_CONNECTION_CLOSED_MID_CALL_DISPOSITION);
  });

  test("an explicit not_sent call stays a safe-to-retry refusal", () => {
    const original = new SubcCallError(
      "not_sent",
      "request was refused before writing",
      "ECONNRESET",
      new SocketClosedError("subc connection closed"),
    );

    const adapted = adaptToolError("read", original) as Error & { kind?: unknown };

    expect(adapted).toMatchObject({ kind: "not_sent" });
    expect(adapted.message).toContain("safe to retry");
    expect(adapted.message).not.toContain("outcome is UNKNOWN");
  });

  test("a module-reloading refusal stays typed as pre-dispatch and safe to retry", () => {
    // When the module is reloading, the daemon rejects the bind before sending
    // the request to the AFT module. Keep this as a typed safe-to-retry refusal,
    // not guidance for a reply lost after dispatch.
    const coded = new SubcError("route closed by subc (GOODBYE)", "module_reloading");

    const adapted = adaptToolError("write", coded);

    expect(adapted).toMatchObject({ code: "aft_module_unavailable", kind: "not_sent" });
    expect((adapted as Error).message).toContain("safe to retry");
    expect((adapted as Error).message).not.toContain("outcome is UNKNOWN");
    expect(coded.message).not.toContain(SUBC_MODULE_RESTART_DISPOSITION);
  });

  test("a route_closed-coded GOODBYE gets the disposition (newer client generations)", () => {
    // The subc-client source now stamps code "route_closed" on the GOODBYE
    // failure; the shipped 0.5.0 line throws it bare. Both shapes must match
    // or a client upgrade silently drops the unknown-outcome guidance.
    const coded = new SubcError("route closed by subc (GOODBYE)", "route_closed");

    const adapted = adaptToolError("write", coded) as Error;

    expect(adapted.message).toContain(SUBC_MODULE_RESTART_DISPOSITION);
  });

  test("a local route close is recognised by the client's typed close reason, not its message", () => {
    // subc-client types its own close of a route or connection as
    // closed_by_caller; the message text is not part of that contract.
    expect(
      isLocalRouteCloseRejection(
        new SubcError("any wording", "route_closed", undefined, "closed_by_caller"),
      ),
    ).toBe(true);
    for (const error of [
      new SubcError("route closed by closeRoute", "route_closed"),
      new SubcError("route closed by subc (GOODBYE)", "route_closed", undefined, "reload"),
      new SubcError("connection closed", "connection_closed", undefined, "connection_lost"),
      new SubcCallError("not_sent", "route was closed before route.open completed", "route_closed"),
    ]) {
      expect(isLocalRouteCloseRejection(error)).toBe(false);
    }
  });

  test("a local closeRoute with the route_closed code is NOT a GOODBYE, but its outcome is unknown", () => {
    // The route_closed code has two sources: a daemon GOODBYE, and closeRoute,
    // which is the plugin closing its own route, so the module-restart wording
    // would be wrong here. subc-client raises the closeRoute rejection only for
    // a request already pending (and written) on the route, so it carries its
    // own unknown-outcome guidance instead.
    const local = new SubcError(
      "route closed by closeRoute",
      "route_closed",
      undefined,
      "closed_by_caller",
    );

    const adapted = adaptToolError("write", local);

    expect(adapted).toBe(local);
    expect(local.message).not.toContain(SUBC_MODULE_RESTART_DISPOSITION);
    expect(local.message).toContain(SUBC_ROUTE_CLOSED_MID_CALL_DISPOSITION);
  });

  test("a bash call whose route the plugin closed mid-call never gets re-run guidance", () => {
    const midCall = new SubcRouteClosedMidCallError(
      "transport shutting down",
      new SubcError("route closed by closeRoute", "route_closed", undefined, "closed_by_caller"),
    );

    adaptToolError("bash", midCall);
    adaptToolError("bash", midCall);

    expect(midCall.message).toStartWith(
      "the command may have run; the route closed mid-call: transport shutting down",
    );
    expect(midCall.message.split(SUBC_ROUTE_CLOSED_MID_CALL_DISPOSITION)).toHaveLength(2);
    expect(midCall.message).toContain("UNKNOWN");
    expect(midCall.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
    expect(midCall.message).not.toContain(SUBC_MODULE_RESTART_DISPOSITION);
  });

  test("a frame-read timeout never enables bash host fallback", () => {
    const rawSocketTimeout = new SocketTimeoutError("timed out waiting for 359 bytes");
    const subcLost = new SubcError(
      "timed out waiting for 359 bytes",
      undefined,
      undefined,
      "connection_lost",
    );
    const typed = new SubcFrameReadTimeoutError(subcLost);

    for (const error of [rawSocketTimeout, subcLost, typed]) {
      expect(isSubcFrameReadTimeoutError(error)).toBe(true);
      expect(classifyBashHostFallbackError(error)).toBeUndefined();
      expect(isBashTransportDeadError(error)).toBe(false);
    }
  });

  test("connect and write SocketTimeoutErrors keep previous transport-dead classification and are NOT mapped to outcome_unknown", () => {
    const connectTimeout = new SocketTimeoutError("timed out connecting to 127.0.0.1:12345");
    const writeTimeout = new SocketTimeoutError("timed out writing to subc");

    for (const error of [connectTimeout, writeTimeout]) {
      expect(isSubcFrameReadTimeoutError(error)).toBe(false);
      expect(classifyBashHostFallbackError(error)).toBe("transport down");
      expect(isBashTransportDeadError(error)).toBe(true);

      const adapted = adaptToolError("bash", error) as Error;
      expect(adapted).not.toBeInstanceOf(SubcFrameReadTimeoutError);
      expect(adapted.message).toContain(BASH_TRANSPORT_DISPOSITION);
      expect(adapted.message).not.toContain(BASH_FRAME_READ_TIMEOUT_DISPOSITION);
      expect(adapted.message).not.toContain(SUBC_FRAME_READ_TIMEOUT_DISPOSITION);
    }
  });

  test("a frame-read timeout on bash gets the unknown-outcome bash disposition", () => {
    const original = new SubcFrameReadTimeoutError(
      new SubcError("timed out waiting for 359 bytes", undefined, undefined, "connection_lost"),
    );

    const adapted = adaptToolError("bash", original) as SubcFrameReadTimeoutError;

    expect(adapted).toBe(original);
    expect(adapted.kind).toBe("outcome_unknown");
    expect(adapted.code).toBe("frame_read_timeout");
    expect(adapted.message).toStartWith(
      "the command may have run; timed out waiting for frame body bytes from the AFT daemon",
    );
    expect(adapted.message).toContain(BASH_FRAME_READ_TIMEOUT_DISPOSITION);
    expect(adapted.message).toContain("UNKNOWN");
    expect(adapted.message).toContain("bash_status");
    expect(adapted.message).toContain("do not re-run side-effecting commands blindly");
    expect(adapted.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
    expect(adapted.message).not.toContain(SUBC_MODULE_RESTART_DISPOSITION);
  });

  test("a frame-read timeout on a read tool gets the unknown-outcome tool disposition", () => {
    const original = new SubcFrameReadTimeoutError(
      new SocketTimeoutError("timed out waiting for 359 bytes"),
    );

    const adapted = adaptToolError("read", original) as SubcFrameReadTimeoutError;

    expect(adapted).toBe(original);
    expect(adapted.kind).toBe("outcome_unknown");
    expect(adapted.code).toBe("frame_read_timeout");
    expect(adapted.message).toStartWith(
      "the command may have run; timed out waiting for frame body bytes from the AFT daemon",
    );
    expect(adapted.message).toContain(SUBC_FRAME_READ_TIMEOUT_DISPOSITION);
    expect(adapted.message).toContain("UNKNOWN");
    expect(adapted.message).toContain("never blind-retry a mutation");
    expect(adapted.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
    expect(adapted.message).not.toContain(BASH_FRAME_READ_TIMEOUT_DISPOSITION);
  });

  test("raw SocketTimeoutError and SubcError are mapped to SubcFrameReadTimeoutError", () => {
    const rawSocket = new SocketTimeoutError("timed out waiting for 359 bytes");
    const adaptedBash = adaptToolError("bash", rawSocket);
    expect(adaptedBash).toBeInstanceOf(SubcFrameReadTimeoutError);
    expect((adaptedBash as SubcFrameReadTimeoutError).cause).toBe(rawSocket);
    expect((adaptedBash as SubcFrameReadTimeoutError).message).toContain(
      BASH_FRAME_READ_TIMEOUT_DISPOSITION,
    );

    const rawSubc = new SubcError(
      "timed out waiting for 100 bytes",
      undefined,
      undefined,
      "connection_lost",
    );
    const adaptedRead = adaptToolError("read", rawSubc);
    expect(adaptedRead).toBeInstanceOf(SubcFrameReadTimeoutError);
    expect((adaptedRead as SubcFrameReadTimeoutError).cause).toBe(rawSubc);
    expect((adaptedRead as SubcFrameReadTimeoutError).message).toContain(
      SUBC_FRAME_READ_TIMEOUT_DISPOSITION,
    );
  });

  test("frame-read timeout disposition is appended once when passed through twice", () => {
    const error = new SubcFrameReadTimeoutError(
      new SocketTimeoutError("timed out waiting for 359 bytes"),
    );

    adaptToolError("bash", error);
    adaptToolError("bash", error);

    const occurrences = error.message.split(BASH_FRAME_READ_TIMEOUT_DISPOSITION).length - 1;
    expect(occurrences).toBe(1);
  });
});
