/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import {
  type RouteHandle,
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
  BASH_TRANSPORT_DISPOSITION,
  BRIDGE_TRANSPORT_UNKNOWN_OUTCOME_DISPOSITION,
  isBashTransportDeadError,
  SUBC_MODULE_RESTART_DISPOSITION,
} from "../error-contract.js";
import {
  SubcRootGenerationExpiredError,
  SubcRootReapedError,
  SubcTransportShuttingDownError,
} from "../subc-transport.js";

/** Shaped exactly as subc-client raises it: bare SubcError, no code. */
function routeGoodbyeError(): SubcError {
  return new SubcError("route closed by subc (GOODBYE)");
}

describe("isBashTransportDeadError", () => {
  const transportDead: Array<[string, Error]> = [
    ["bridge spawn failure", new BridgeTransportUnavailableError("spawn failed")],
    ["standalone bridge shutdown", new BridgeTransportUnavailableError("Bridge is shutting down")],
    ["subc transport shutdown", new SubcTransportShuttingDownError()],
    [
      "module endpoint unavailable after retries",
      new SubcCallError("not_sent", "module unavailable", "unknown_module"),
    ],
    [
      "module reloading after retries",
      new SubcCallError("not_sent", "module reloading", "module_reloading"),
    ],
    [
      "target unavailable after retries",
      new SubcCallError("not_sent", "target unavailable", "target_unavailable"),
    ],
    [
      "bind timeout after retries",
      new SubcCallError("not_sent", "bind timed out", "module_timeout"),
    ],
    ["connection dropped", new SubcCallError("outcome_unknown", "connection dropped")],
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
    ["live bridge request timeout", new BridgeTransportTimeoutError("bash", 100, "bridge busy")],
    ["ordinary tool failure", new Error("command failed")],
  ];

  for (const [name, error] of engineAlive) {
    test(`rejects engine-alive shape: ${name}`, () => {
      expect(isBashTransportDeadError(error)).toBe(false);
    });
  }
});

describe("adaptToolError", () => {
  test("adds bash transport disposition guidance while preserving the error", () => {
    const original = new BridgeTransportTimeoutError("bash", 11_000, "bash transport timed out");

    let thrown: unknown;
    try {
      throw original;
    } catch (error) {
      thrown = adaptToolError("bash", error);
    }

    expect(thrown).toBe(original);
    expect(original.message).toBe(`bash transport timed out ${BASH_TRANSPORT_DISPOSITION}`);
    expect(isBridgeTransportTimeout(original)).toBe(true);
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

  test("disposition is appended once when the error passes through twice", () => {
    const original = routeGoodbyeError();

    adaptToolError("read", original);
    adaptToolError("read", original);

    const occurrences = original.message.split(SUBC_MODULE_RESTART_DISPOSITION).length - 1;
    expect(occurrences).toBe(1);
  });

  test("a differently-coded SubcError is left alone — GOODBYE never borrows other codes", () => {
    // module_reloading is proven-not-forwarded and retryable; it must not be
    // dressed up as an unknown outcome.
    const coded = new SubcError("route closed by subc (GOODBYE)", "module_reloading");

    const adapted = adaptToolError("write", coded);

    expect(adapted).toBe(coded);
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

  test("a local closeRoute with the route_closed code is NOT a GOODBYE", () => {
    // Same code, different mechanism: closeRoute is a deliberate local close
    // with a known outcome; the unknown-outcome guidance would be wrong.
    const local = new SubcError("route closed by closeRoute", "route_closed");

    const adapted = adaptToolError("write", local);

    expect(adapted).toBe(local);
    expect(local.message).not.toContain(SUBC_MODULE_RESTART_DISPOSITION);
  });
});
