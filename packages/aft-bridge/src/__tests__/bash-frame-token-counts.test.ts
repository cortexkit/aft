/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { formatForegroundResult } from "../bash-format.js";
import { type BashCompletedPayload, BinaryBridge } from "../bridge.js";
import { cachedExecutable } from "./test-utils/cached-executable.js";

let workDir: string;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-bridge-bash-frame-"));
});

afterEach(() => {
  rmSync(workDir, { recursive: true, force: true });
});

function writeExecutable(_name: string, source: string): string {
  return cachedExecutable(source);
}

async function readPushedCompletion(frame: Record<string, unknown>): Promise<BashCompletedPayload> {
  const script = writeExecutable(
    "push-frame.js",
    `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  const newline = buffer.indexOf("\\n");
  if (newline === -1) return;
  const req = JSON.parse(buffer.slice(0, newline));
  process.stdout.write(${JSON.stringify(`${JSON.stringify(frame)}\n`)});
  process.stdout.write(JSON.stringify({ id: req.id, success: true, version: "0.0.0-test" }) + "\\n");
});
`,
  );

  let pushed: BashCompletedPayload | undefined;
  const bridge = new BinaryBridge(script, workDir, {
    timeoutMs: 5_000,
    maxRestarts: 0,
    onBashCompletion: (completion) => {
      pushed = completion;
    },
  });

  try {
    await bridge.send("version");
    expect(pushed).toBeDefined();
    return pushed as BashCompletedPayload;
  } finally {
    await bridge.shutdown();
  }
}

describe("bash_completed token-count push frames", () => {
  for (const outcome of ["remote_job", "remote_no_job", "local"]) {
    test(`unknown outcome parity: ${outcome}`, async () => {
      const fixture = join(
        import.meta.dir,
        "../../../../crates/aft/tests/fixtures/subc_parity/format",
        `bash_unknown_${outcome}`,
      );
      const input = JSON.parse(readFileSync(join(fixture, "input.json"), "utf8"));
      const expected = readFileSync(join(fixture, "expected.txt"), "utf8");
      expect(formatForegroundResult(input.native_response_json)).toBe(expected);
      const completion = await readPushedCompletion({
        type: "bash_completed",
        task_id: "bash-unknown",
        status: "fate_unknown",
        exit_code: null,
        command: "printf done",
        output_preview: input.native_response_json.output_preview,
        status_reason: input.native_response_json.output,
      });
      expect(completion.output_preview).toBe(expected);
      expect(completion.status_reason).toBe(expected);
    });
  }

  test("bash_completed_frame_preserves_incomplete_capture_without_changing_exit", async () => {
    const reason = "PTY output may be incomplete: output drain deadline expired before EOF";
    const completion = await readPushedCompletion({
      type: "bash_completed",
      task_id: "bash-incomplete",
      session_id: "session-1",
      status: "completed",
      exit_code: 0,
      command: "deploy",
      output_incomplete: true,
      status_reason: reason,
    });
    expect(completion.status).toBe("completed");
    expect(completion.exit_code).toBe(0);
    expect(completion.output_incomplete).toBe(true);
    expect(completion.status_reason).toBe(reason);
  });

  test("bash_completed_frame_passes_token_counts_through", async () => {
    const completion = await readPushedCompletion({
      type: "bash_completed",
      task_id: "bash-token-1",
      session_id: "session-1",
      status: "completed",
      exit_code: 0,
      command: "echo hello",
      output_preview: "hello\n",
      output_truncated: false,
      original_tokens: 2,
      compressed_tokens: 2,
      tokens_skipped: false,
    });

    expect(completion.original_tokens).toBe(2);
    expect(completion.compressed_tokens).toBe(2);
    expect(completion.tokens_skipped).toBe(false);
  });

  test("bash_completed_frame_backward_compat_without_token_fields", async () => {
    const completion = await readPushedCompletion({
      type: "bash_completed",
      task_id: "bash-token-old",
      session_id: "session-1",
      status: "completed",
      exit_code: 0,
      command: "true",
      output_preview: "",
      output_truncated: false,
    });

    expect(completion.original_tokens).toBeUndefined();
    expect(completion.compressed_tokens).toBeUndefined();
    expect(completion.tokens_skipped).toBeUndefined();
  });
});
