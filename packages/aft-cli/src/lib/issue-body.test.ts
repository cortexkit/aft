import { describe, expect, test } from "bun:test";
import { capBodyToGithubLimit, extractRecentErrors, MAX_GITHUB_BODY_BYTES } from "./issue-body";

describe("extractRecentErrors", () => {
  test("matches the documented sessionLog / slog error shapes", () => {
    const log = [
      "2026-05-22 12:00:00 [INFO] [aft] semantic index ready in 42ms",
      "2026-05-22 12:00:01 [INFO] [aft] [ses_x] compiled 12 files; 0 failed", // telemetry, NOT an error
      "2026-05-22 12:00:02 bridge crashed: SIGKILL after timeout",
      "2026-05-22 12:00:03 [aft] configure failed: project_too_large",
      "2026-05-22 12:00:04 Error: Connection reset",
      "2026-05-22 12:00:05 TypeError: cannot read property 'foo' of undefined",
      "2026-05-22 12:00:06 EMERGENCY: aborting session ses_abc",
      "2026-05-22 12:00:07 some other info line",
      "2026-05-22 12:00:08 caught exception during cleanup",
      "2026-05-22 12:00:09 thread 'main' panicked at crates/aft/src/foo.rs:42:5:",
    ].join("\n");

    const matches = extractRecentErrors(log, 20);

    // Real errors are matched.
    expect(matches).toContain("2026-05-22 12:00:02 bridge crashed: SIGKILL after timeout");
    expect(matches).toContain("2026-05-22 12:00:03 [aft] configure failed: project_too_large");
    expect(matches).toContain("2026-05-22 12:00:04 Error: Connection reset");
    expect(matches).toContain(
      "2026-05-22 12:00:05 TypeError: cannot read property 'foo' of undefined",
    );
    expect(matches).toContain("2026-05-22 12:00:06 EMERGENCY: aborting session ses_abc");
    expect(matches).toContain("2026-05-22 12:00:08 caught exception during cleanup");
    expect(matches).toContain(
      "2026-05-22 12:00:09 thread 'main' panicked at crates/aft/src/foo.rs:42:5:",
    );

    // Past-tense "0 failed" telemetry MUST NOT be classified as an error.
    expect(matches).not.toContain(
      "2026-05-22 12:00:01 [INFO] [aft] [ses_x] compiled 12 files; 0 failed",
    );
    expect(matches).not.toContain("2026-05-22 12:00:07 some other info line");
  });

  test("matches V8 stack-trace frames", () => {
    const log = [
      "Error: thing broke",
      "    at SomeFn (file:///foo.ts:42:5)",
      "    at processTransform (file:///bar.ts:13:9)",
      "    at file:///baz.ts:7:1",
    ].join("\n");

    const matches = extractRecentErrors(log, 20);
    // All four lines qualify — the Error and three stack frames.
    expect(matches.length).toBe(4);
  });

  test("returns matches in chronological order", () => {
    const log = [
      "bridge failed: first error",
      "info noise",
      "bridge failed: second error",
      "info noise",
      "bridge failed: third error",
    ].join("\n");

    const matches = extractRecentErrors(log, 10);
    expect(matches).toEqual([
      "bridge failed: first error",
      "bridge failed: second error",
      "bridge failed: third error",
    ]);
  });

  test("caps at the requested limit (newest-first selection, oldest-first output)", () => {
    const lines: string[] = [];
    for (let i = 0; i < 50; i += 1) {
      lines.push(`bridge failed: error ${i}`);
    }
    const matches = extractRecentErrors(lines.join("\n"), 5);
    // We asked for 5; the 5 NEWEST errors should be returned, in
    // chronological (oldest-first) order: 45, 46, 47, 48, 49.
    expect(matches.length).toBe(5);
    expect(matches[0]).toBe("bridge failed: error 45");
    expect(matches[4]).toBe("bridge failed: error 49");
  });

  test("returns empty array when no errors found", () => {
    const log = ["info line 1", "info line 2", "compiled 12 files in 42ms"].join("\n");
    expect(extractRecentErrors(log, 20)).toEqual([]);
  });

  test("handles empty input gracefully", () => {
    expect(extractRecentErrors("", 20)).toEqual([]);
  });
});

describe("capBodyToGithubLimit", () => {
  /**
   * Build a synthetic issue body that mirrors the doctor flow's real output:
   * a few small sections followed by `## Logs (last 200 lines per harness)`
   * containing one or more per-harness fenced blocks.
   */
  function makeBody(opts: {
    logLineCount: number;
    lineSize?: number;
    trailerLine?: string;
  }): string {
    const lineSize = opts.lineSize ?? 80;
    const logLines: string[] = [];
    for (let i = 0; i < opts.logLineCount; i += 1) {
      const prefix = `LINE${String(i).padStart(6, "0")}: `;
      const padding = "x".repeat(Math.max(0, lineSize - prefix.length));
      logLines.push(prefix + padding);
    }

    return [
      "## Description",
      "Test description for the cap helper.",
      "",
      "## Environment",
      "- AFT CLI: v0.28.2",
      "",
      "## Diagnostics",
      "_diagnostics block_",
      "",
      "## Recent errors (last 20, sanitized)",
      "```",
      "bridge crashed: critical 1",
      "bridge crashed: critical 2",
      "```",
      "",
      "## Logs (last 200 lines per harness)",
      "#### opencode log (~/.cache/aft/log)",
      "",
      "```",
      logLines.join("\n"),
      "```",
      "",
      opts.trailerLine ?? "_Usernames and home paths have been stripped from this report._",
    ].join("\n");
  }

  test("returns body unchanged when already within budget", () => {
    const body = makeBody({ logLineCount: 20 });
    const capped = capBodyToGithubLimit(body, 100_000);
    expect(capped).toBe(body);
  });

  test("truncates the main log section when body exceeds budget", () => {
    const body = makeBody({ logLineCount: 5000, lineSize: 200 });
    const originalBytes = Buffer.byteLength(body, "utf8");

    const capped = capBodyToGithubLimit(body, 60_000);
    const cappedBytes = Buffer.byteLength(capped, "utf8");

    // The body must now fit the budget.
    expect(cappedBytes).toBeLessThanOrEqual(60_000);
    // And it must actually be smaller than the input (proving truncation
    // happened, not just trivially passed-through).
    expect(cappedBytes).toBeLessThan(originalBytes);
  });

  test("preserves the Recent errors section after truncation", () => {
    const body = makeBody({ logLineCount: 5000, lineSize: 200 });
    const capped = capBodyToGithubLimit(body, 60_000);

    // The errors section MUST survive truncation — that's the whole
    // point of separating it from the main log block.
    expect(capped).toContain("## Recent errors (last 20, sanitized)");
    expect(capped).toContain("bridge crashed: critical 1");
    expect(capped).toContain("bridge crashed: critical 2");
  });

  test("inserts the truncation marker when log lines are dropped", () => {
    const body = makeBody({ logLineCount: 5000, lineSize: 200 });
    const capped = capBodyToGithubLimit(body, 60_000);

    expect(capped).toContain("[truncated for GitHub 64KB limit");
  });

  test("drops oldest log lines first (keeps newest)", () => {
    const body = makeBody({ logLineCount: 5000, lineSize: 200 });
    const capped = capBodyToGithubLimit(body, 60_000);

    // The last log line (LINE004999) should be preserved — it's the
    // newest and the most relevant.
    expect(capped).toContain("LINE004999:");

    // The first log line (LINE000000) should be gone — it's the oldest.
    expect(capped).not.toContain("LINE000000:");
  });

  test("preserves the Description, Environment, and Diagnostics sections", () => {
    const body = makeBody({ logLineCount: 5000, lineSize: 200 });
    const capped = capBodyToGithubLimit(body, 60_000);

    expect(capped).toContain("## Description");
    expect(capped).toContain("Test description for the cap helper.");
    expect(capped).toContain("## Environment");
    expect(capped).toContain("- AFT CLI: v0.28.2");
    expect(capped).toContain("## Diagnostics");
  });

  test("logs section runs to end-of-body when no trailing top-level heading exists", () => {
    // AFT's real doctor output places the logs section last (followed only
    // by an italic footer paragraph, not a `## ` heading). Make sure the
    // cap correctly handles that shape — truncation stops at end-of-body
    // and preserves the italic footer line.
    const body = makeBody({
      logLineCount: 5000,
      lineSize: 200,
      trailerLine: "_Usernames and home paths have been stripped from this report._",
    });
    const capped = capBodyToGithubLimit(body, 60_000);

    expect(capped).toContain("_Usernames and home paths have been stripped");
    expect(Buffer.byteLength(capped, "utf8")).toBeLessThanOrEqual(60_000);
  });

  test("uses MAX_GITHUB_BODY_BYTES as the default budget", () => {
    // Building a body well past 60KB to force the default-budget path
    // to engage. 80 chars * 5000 lines ≈ 400KB just for the log block.
    const body = makeBody({ logLineCount: 5000, lineSize: 80 });
    const capped = capBodyToGithubLimit(body);
    expect(Buffer.byteLength(capped, "utf8")).toBeLessThanOrEqual(MAX_GITHUB_BODY_BYTES);
  });

  test("falls back to raw byte truncation when log heading is missing", () => {
    // Synthetic input with no `## Logs (last` heading — exercises the
    // defensive fallback path. We pad with non-ASCII to ensure UTF-8
    // boundary handling doesn't corrupt the slice.
    const body = `## Other\n${"ü".repeat(50_000)}\n## End`;
    const capped = capBodyToGithubLimit(body, 10_000);
    expect(Buffer.byteLength(capped, "utf8")).toBeLessThanOrEqual(10_000);
    expect(capped).toContain("[truncated for GitHub 64KB limit]");
  });
});
