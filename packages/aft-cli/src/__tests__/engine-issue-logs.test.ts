import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { collectIssueEngineLog } from "../commands/doctor.js";
import { capBodyToGithubLimit } from "../lib/issue-body.js";
import { sanitizeContent } from "../lib/sanitize.js";

function withLogs(run: (directory: string) => void) {
  const directory = mkdtempSync(join(tmpdir(), "aft-engine-issue-"));
  try {
    run(directory);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

const stamp = "2026-01-01T10:01:00.123Z [aft]";

describe("doctor --issue engine logs", () => {
  test("collects exact engine filenames and the single rotation, not unrelated files", () => {
    withLogs((directory) => {
      writeFileSync(join(directory, "aft-123.log"), `${stamp} WARN active-engine\n`);
      writeFileSync(join(directory, "aft-123.log.1"), `${stamp} ERROR failed: rotated-engine\n`);
      for (const name of ["aft-plugin.log", "aft-subc.log", "aft-123.log.2", "aft-abc.log"]) {
        writeFileSync(join(directory, name), "must-not-collect\n");
      }
      const result = collectIssueEngineLog(join(directory, "aft-plugin.log"), null);
      expect(result.logText).toContain("active-engine");
      expect(result.logText).toContain("rotated-engine");
      expect(result.errors).toContain("rotated-engine");
      expect(result.logText).not.toContain("must-not-collect");
      expect(result.notice).toBe("");
    });
  });

  test("filters engine records by selected session, time window and project", () => {
    withLogs((directory) => {
      writeFileSync(
        join(directory, "aft-123.log"),
        [
          `${stamp} INFO [ses_target] selected-tag`,
          `${stamp} ERROR [ses_foreign] foreign-tag`,
          `${stamp} WARN root=/projects/target selected-project`,
          `${stamp} WARN root=/projects/foreign foreign-project`,
          "2026-01-01T12:00:00.123Z [aft] WARN root=/projects/target outside-window",
        ].join("\n"),
      );
      const result = collectIssueEngineLog(join(directory, "aft-plugin.log"), {
        id: "ses_target",
        title: "target",
        projectRoot: "/projects/target",
        startedAt: Date.parse("2026-01-01T10:00:00Z"),
        lastActivity: Date.parse("2026-01-01T10:05:00Z"),
      });
      expect(result.logText).toContain("selected-tag");
      expect(result.logText).toContain("selected-project");
      expect(result.logText).not.toContain("foreign-");
      expect(result.logText).not.toContain("outside-window");
    });
  });

  test("shares the output budget and uses the existing report redaction and body cap", () => {
    withLogs((directory) => {
      const token = `ghp_${"A".repeat(36)}`;
      for (const pid of [123, 456]) {
        writeFileSync(
          join(directory, `aft-${pid}.log`),
          Array.from(
            { length: 300 },
            (_, i) =>
              `${stamp} INFO row=${i} token=${token} path=${homedir()}/private-project ${"x".repeat(400)}`,
          ).join("\n"),
        );
      }
      const result = collectIssueEngineLog(join(directory, "aft-plugin.log"), null);
      expect(result.logText.split("\n")).toHaveLength(200);
      const body = capBodyToGithubLimit(
        sanitizeContent(`## Logs (last 200 lines)\n${result.logText}`),
      );
      expect(body).not.toContain(token);
      expect(body).not.toContain(homedir());
      expect(Buffer.byteLength(body)).toBeLessThanOrEqual(60_000);
    });
  });

  test("absent log directories return no history", () => {
    withLogs((directory) => {
      expect(collectIssueEngineLog(join(directory, "absent", "aft-plugin.log"), null).logText).toBe(
        "",
      );
    });
  });
});
