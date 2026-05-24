/// <reference path="../bun-test.d.ts" />

/**
 * Tests for `sendAftRequests` — the NDJSON transport the CLI uses to
 * talk to the `aft` binary in one-shot mode (e.g. `aft doctor lsp`).
 *
 * Issue #29 had the user hitting `Unexpected identifier "AFT"` from
 * `JSON.parse(line)` because their resolved binary printed a non-JSON
 * banner to stdout before responding. The transport then crashed Bun
 * with a raw `SyntaxError` stack trace and no actionable hint.
 *
 * The new contract: non-JSON stdout lines are tolerated and remembered
 * for diagnostics. We only fail when the binary exits without producing
 * the expected number of valid responses, and then the error names the
 * binary, the noise we observed, and stderr.
 *
 * We exercise this via fake `aft` binaries written as bash scripts so
 * the test reflects the real spawn/stdio path instead of mocking it.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { sendAftRequest, sendAftRequests } from "../lib/aft-bridge.js";

let workDir: string;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-bridge-test-"));
});

afterEach(() => {
  rmSync(workDir, { recursive: true, force: true });
});

function makeFakeBinary(name: string, body: string): string {
  const path = join(workDir, name);
  writeFileSync(path, `#!/bin/bash\n${body}\n`);
  chmodSync(path, 0o755);
  return path;
}

describe("sendAftRequests — happy path", () => {
  test("collects responses by request count", async () => {
    const bin = makeFakeBinary(
      "happy",
      `while IFS= read -r line; do
        id=$(echo "$line" | sed -n 's/.*"id":"\\([^"]*\\)".*/\\1/p')
        echo "{\\"id\\":\\"$id\\",\\"success\\":true}"
      done`,
    );

    const responses = await sendAftRequests(bin, [
      { id: "a", command: "configure" },
      { id: "b", command: "version" },
    ]);

    expect(responses).toHaveLength(2);
    expect(responses[0].id).toBe("a");
    expect(responses[1].id).toBe("b");
    expect(responses[0].success).toBe(true);
  });

  test("sendAftRequest returns the single response", async () => {
    const bin = makeFakeBinary(
      "single",
      `read line
      echo '{"id":"x","success":true,"version":"0.19.4"}'`,
    );

    const response = await sendAftRequest(bin, { id: "x", command: "version" });
    expect(response.success).toBe(true);
    expect(response.version).toBe("0.19.4");
  });
});

describe("sendAftRequests — issue #29 regression: tolerate stdout noise", () => {
  /**
   * The exact failure reported in #29: the binary prints "AFT ..." text
   * before the JSON response. v0.19.3 crashed with `SyntaxError:
   * Unexpected identifier "AFT"`. v0.19.4+ must surface the JSON
   * response and ignore the banner.
   */
  test("non-JSON banner before JSON response is silently tolerated", async () => {
    const bin = makeFakeBinary(
      "banner-then-json",
      `echo "AFT background-bash-ready (legacy mode)"
      while IFS= read -r line; do
        id=$(echo "$line" | sed -n 's/.*"id":"\\([^"]*\\)".*/\\1/p')
        echo "{\\"id\\":\\"$id\\",\\"success\\":true}"
      done`,
    );

    const responses = await sendAftRequests(bin, [{ id: "a", command: "configure" }]);

    expect(responses).toHaveLength(1);
    expect(responses[0].success).toBe(true);
  });

  test("multiple non-JSON lines interleaved with JSON are tolerated", async () => {
    const bin = makeFakeBinary(
      "interleaved",
      `echo "Banner line 1"
      echo '{"id":"a","success":true}'
      echo "Banner line 2"
      read line
      read line
      echo '{"id":"b","success":true}'
      echo "Trailing log"`,
    );

    const responses = await sendAftRequests(bin, [
      { id: "a", command: "configure" },
      { id: "b", command: "version" },
    ]);

    expect(responses).toHaveLength(2);
  });

  test("malformed JSON (looks like JSON but isn't) is treated as noise", async () => {
    // The line starts with `{` so the fast-path tries to parse, fails,
    // and falls into the noise bucket.
    const bin = makeFakeBinary(
      "malformed",
      `echo '{not really json}'
      read line
      echo '{"id":"a","success":true}'`,
    );

    const responses = await sendAftRequests(bin, [{ id: "a", command: "configure" }]);

    expect(responses).toHaveLength(1);
    expect(responses[0].success).toBe(true);
  });
});

describe("sendAftRequests — actionable errors when the binary fails", () => {
  test("error message includes the binary path", async () => {
    const bin = makeFakeBinary(
      "exit-without-response",
      `echo "Some banner"
      exit 0`,
    );

    let caught: Error | null = null;
    try {
      await sendAftRequests(bin, [{ id: "a", command: "configure" }]);
    } catch (err) {
      caught = err as Error;
    }

    expect(caught).not.toBe(null);
    expect(caught?.message).toContain(bin);
    expect(caught?.message).toContain("aft exited before responding");
  });

  test("error includes stdout noise so users can see what the binary actually printed", async () => {
    // Simulates the user-reported case: wrong binary on PATH, prints
    // its own banner instead of speaking AFT NDJSON. With the old
    // code, this surfaced as `SyntaxError: Unexpected identifier "AFT"`
    // — completely unactionable. The new error must show the actual
    // output so the user can identify the wrong binary.
    const bin = makeFakeBinary(
      "wrong-tool",
      `echo "AFT (Awesome Filesystem Tool) v3.2"
      echo "Usage: aft <command>"
      exit 1`,
    );

    let caught: Error | null = null;
    try {
      await sendAftRequests(bin, [{ id: "a", command: "configure" }]);
    } catch (err) {
      caught = err as Error;
    }

    expect(caught?.message).toContain("AFT (Awesome Filesystem Tool) v3.2");
    expect(caught?.message).toContain("Usage: aft <command>");
    expect(caught?.message).toContain("non-JSON line");
    // The hint must point users at `doctor` for full diagnostics.
    expect(caught?.message).toContain("@cortexkit/aft doctor");
  });

  test("error includes stderr so panic output is visible", async () => {
    const bin = makeFakeBinary(
      "panicky",
      `echo "thread 'main' panicked at lib.rs:42:13: something exploded" >&2
      exit 101`,
    );

    let caught: Error | null = null;
    try {
      await sendAftRequests(bin, [{ id: "a", command: "configure" }]);
    } catch (err) {
      caught = err as Error;
    }

    expect(caught?.message).toContain("stderr:");
    expect(caught?.message).toContain("something exploded");
  });

  test("partial responses count is reported when the binary exits early", async () => {
    const bin = makeFakeBinary(
      "partial",
      `read line
      echo '{"id":"a","success":true}'
      exit 0`,
    );

    let caught: Error | null = null;
    try {
      await sendAftRequests(bin, [
        { id: "a", command: "configure" },
        { id: "b", command: "lsp_inspect" },
      ]);
    } catch (err) {
      caught = err as Error;
    }

    expect(caught?.message).toContain("Got 1 valid response(s) before exit");
  });

  test("skips push frames mixed with responses (issue #34 regression)", async () => {
    // Issue #34: `aft doctor lsp` printed `lsp_inspect failed` even though
    // the inspection succeeded. Root cause was the bridge emitting a
    // configure_warnings push frame before the lsp_inspect response, and
    // the response collector counted it as a response by length, dropping
    // the real inspect response on the floor. Push frames have no `id` so
    // they must be skipped, not counted.
    const bin = makeFakeBinary(
      "issue-34-push-frame",
      `read line1
      read line2
      # Configure response
      echo '{"id":"doctor-lsp-configure","success":true}'
      # Push frame between responses — exactly what bit issue #34.
      echo '{"type":"configure_warnings","project_root":"/x","warnings":[]}'
      # The real inspect response we want
      echo '{"id":"doctor-lsp-inspect","success":true,"file":"/x/y.py","diagnostics_count":2}'`,
    );

    const responses = await sendAftRequests(bin, [
      { id: "doctor-lsp-configure", command: "configure" },
      { id: "doctor-lsp-inspect", command: "lsp_inspect", file: "/x/y.py" },
    ]);

    expect(responses).toHaveLength(2);
    expect(responses[0].id).toBe("doctor-lsp-configure");
    expect(responses[1].id).toBe("doctor-lsp-inspect");
    expect(responses[1].success).toBe(true);
    expect(responses[1].diagnostics_count).toBe(2);
  });

  test("does NOT crash with raw SyntaxError stack trace (issue #29 acceptance test)", async () => {
    // The failure mode: error message must not be a bare JS SyntaxError
    // stack. It must be a normal Error with a message that names the
    // binary and shows the noise.
    const bin = makeFakeBinary(
      "issue-29-shape",
      `echo "AFT something something"
      exit 0`,
    );

    let caught: Error | null = null;
    try {
      await sendAftRequests(bin, [{ id: "a", command: "configure" }]);
    } catch (err) {
      caught = err as Error;
    }

    expect(caught).not.toBe(null);
    expect(caught?.name).toBe("Error"); // not "SyntaxError"
    expect(caught?.message).not.toMatch(/Unexpected identifier/);
    expect(caught?.message).toContain("aft exited before responding");
  });

  test("very noisy output is truncated in the error message", async () => {
    // A 100-line banner shouldn't dump all 100 lines into the error.
    let body = "";
    for (let i = 0; i < 100; i += 1) {
      body += `echo "Noise line ${i}"\n`;
    }
    body += "exit 0\n";
    const bin = makeFakeBinary("noisy", body);

    let caught: Error | null = null;
    try {
      await sendAftRequests(bin, [{ id: "a", command: "configure" }]);
    } catch (err) {
      caught = err as Error;
    }

    expect(caught?.message).toContain("100 non-JSON line(s)");
    expect(caught?.message).toContain("more line(s) omitted");
    // The first 5 lines should be present; the 50th should not.
    expect(caught?.message).toContain("Noise line 0");
    expect(caught?.message).not.toContain("Noise line 50");
  });
});

describe("sendAftRequests — process error handling", () => {
  test("rejects with the spawn error when binary doesn't exist", async () => {
    let caught: Error | null = null;
    try {
      await sendAftRequests(join(workDir, "does-not-exist"), [{ id: "a", command: "configure" }]);
    } catch (err) {
      caught = err as Error;
    }

    // Could be ENOENT from spawn, or our buildBridgeError if exit fires
    // first; either way it should be an Error with a useful message.
    expect(caught).not.toBe(null);
    expect(caught?.message.length).toBeGreaterThan(0);
  });
});
