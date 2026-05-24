/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type BashCompletedPayload, BinaryBridge } from "../bridge.js";

let workDir: string;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-bridge-bash-frame-"));
});

afterEach(() => {
  rmSync(workDir, { recursive: true, force: true });
});

function writeExecutable(name: string, source: string): string {
  const path = join(workDir, name);
  writeFileSync(path, source);
  chmodSync(path, 0o755);
  return path;
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
