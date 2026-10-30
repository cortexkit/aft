import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { lspNpmInstallInvocation, lspPublicationCutoff } from "../lsp-npm-policy.js";
import { npmInvocation, type ResolvedNpm } from "../npm-resolver.js";

async function withNpmVersion(
  version: string,
  status: number,
  fn: (npm: ResolvedNpm) => void | Promise<void>,
): Promise<void> {
  const root = mkdtempSync(join(tmpdir(), "aft-npm-policy-"));
  const windows = process.platform === "win32";
  const command = join(root, windows ? "npm.cmd" : "npm");
  try {
    writeFileSync(
      command,
      windows
        ? `@echo off\r\necho ${version}\r\nexit /b ${status}\r\n`
        : `#!/bin/sh\nprintf '%s\\n' '${version}'\nexit ${status}\n`,
      { mode: 0o755 },
    );
    await fn({ command, binDir: root });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

describe("shared LSP npm policy", () => {
  test("unpinned installs apply the exact publication grace window", () => {
    expect(lspPublicationCutoff(3, false, Date.parse("2026-10-10T12:00:00Z"))).toBe(
      "2026-10-07T12:00:00.000Z",
    );
  });

  test("explicit pins bypass publication filtering without probing npm", async () => {
    const before = lspPublicationCutoff(3, true, Date.parse("2026-10-10T12:00:00Z"));
    expect(before).toBeUndefined();
    const npm = { command: "/nonexistent/npm", binDir: null };
    expect(await lspNpmInstallInvocation(npm, "server@1.2.3", before)).toEqual(
      npmInvocation(npm, ["install", "--no-save", "--ignore-scripts", "--silent", "server@1.2.3"]),
    );
  });

  test("supported npm carries the cutoff and suppresses lifecycle scripts", async () => {
    for (const version of ["7.0.0", "11.6.0"]) {
      await withNpmVersion(version, 0, async (npm) => {
        expect(await lspNpmInstallInvocation(npm, "server", "2026-10-07T12:00:00.000Z")).toEqual(
          npmInvocation(npm, [
            "install",
            "--no-save",
            "--ignore-scripts",
            "--silent",
            "--before=2026-10-07T12:00:00.000Z",
            "server",
          ]),
        );
      });
    }
  });

  test("old or unverifiable npm cannot perform publication-filtered installs", async () => {
    for (const [version, status] of [
      ["6.14.0", 0],
      ["unknown", 0],
      ["11.6.0", 1],
    ] as const) {
      await withNpmVersion(version, status, async (npm) => {
        expect(await lspNpmInstallInvocation(npm, "server", "2026-10-07T12:00:00.000Z")).toBeNull();
      });
    }
    expect(
      await lspNpmInstallInvocation(
        { command: "/nonexistent/npm", binDir: null },
        "server",
        "cutoff",
      ),
    ).toBeNull();
  });

  test("a cached capability refusal does not block explicit pins for the same npm path", async () => {
    await withNpmVersion("6.14.0", 0, async (npm) => {
      expect(await lspNpmInstallInvocation(npm, "server", "2026-10-07T12:00:00.000Z")).toBeNull();
      expect(await lspNpmInstallInvocation(npm, "server@1.2.3")).toEqual(
        npmInvocation(npm, [
          "install",
          "--no-save",
          "--ignore-scripts",
          "--silent",
          "server@1.2.3",
        ]),
      );
    });
  });
});
