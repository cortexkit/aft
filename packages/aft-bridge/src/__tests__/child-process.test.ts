/// <reference path="../bun-test.d.ts" />

import { afterAll, describe, expect, test } from "bun:test";
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join, relative, resolve } from "node:path";
import { spawnSync, withWindowsHidden } from "../child-process.js";
import { findExecutableOnPath, findExecutablesOnPath } from "../path-lookup.js";

describe("withWindowsHidden", () => {
  test("adds options after an argv array", () => {
    expect(withWindowsHidden(["aft", ["--version"]], true)).toEqual([
      "aft",
      ["--version"],
      { windowsHide: true },
    ]);
  });

  test("merges into existing options and overrides an explicit false", () => {
    expect(
      withWindowsHidden(["aft", ["--version"], { encoding: "utf8", windowsHide: false }], true),
    ).toEqual(["aft", ["--version"], { encoding: "utf8", windowsHide: true }]);
  });

  test("treats a non-array second argument as options when argv is omitted", () => {
    expect(withWindowsHidden(["echo hi", { shell: true }], true)).toEqual([
      "echo hi",
      { shell: true, windowsHide: true },
    ]);
  });

  test("keeps options in the third slot when argv is passed as undefined", () => {
    expect(withWindowsHidden(["aft", undefined, { cwd: "/tmp" }], true)).toEqual([
      "aft",
      undefined,
      { cwd: "/tmp", windowsHide: true },
    ]);
  });

  test("inserts options before a trailing callback", () => {
    const callback = () => {};
    expect(withWindowsHidden(["aft", ["-v"], callback], true)).toEqual([
      "aft",
      ["-v"],
      { windowsHide: true },
      callback,
    ]);
  });

  test("execSync shape puts options straight after the command", () => {
    expect(withWindowsHidden(["gh --version", { stdio: "ignore" }], false)).toEqual([
      "gh --version",
      { stdio: "ignore", windowsHide: true },
    ]);
    expect(withWindowsHidden(["gh --version"], false)).toEqual([
      "gh --version",
      { windowsHide: true },
    ]);
  });

  test("the wrapped spawnSync still runs the child and returns its output", () => {
    const result = spawnSync(process.execPath, ["-e", "process.stdout.write('ok')"], {
      encoding: "utf8",
    });
    expect(result.status).toBe(0);
    expect(result.stdout).toBe("ok");
  });
});

describe("findExecutablesOnPath", () => {
  const root = mkdtempSync(join(tmpdir(), "aft-path-lookup-"));
  afterAll(() => rmSync(root, { recursive: true, force: true }));

  function dir(name: string): string {
    const path = join(root, name);
    mkdirSync(path, { recursive: true });
    return path;
  }

  // POSIX execute bits do not exist on Windows hosts.
  test.skipIf(process.platform === "win32")(
    "returns every executable hit in PATH order, skipping non-executables",
    () => {
      const first = dir("first");
      const second = dir("second");
      const noExec = dir("noexec");
      for (const d of [first, second]) {
        writeFileSync(join(d, "tool"), "");
        chmodSync(join(d, "tool"), 0o755);
      }
      writeFileSync(join(noExec, "tool"), "");
      chmodSync(join(noExec, "tool"), 0o644);
      mkdirSync(join(dir("isdir"), "tool"), { recursive: true });
      const pathValue = [noExec, "", join(root, "isdir"), first, first, second].join(delimiter);
      expect(findExecutablesOnPath("tool", { pathValue, platform: "linux" })).toEqual([
        join(first, "tool"),
        join(second, "tool"),
      ]);
      expect(findExecutableOnPath("tool", { pathValue, platform: "linux" })).toBe(
        join(first, "tool"),
      );
      expect(findExecutableOnPath("missing", { pathValue, platform: "linux" })).toBeNull();
    },
  );

  test("on Windows, finds extension variants in a semicolon-separated PATH", () => {
    const win = dir("win");
    writeFileSync(join(win, "tool.cmd"), "");
    writeFileSync(join(win, "tool.exe"), "");
    expect(findExecutablesOnPath("tool", { pathValue: `;${win}`, platform: "win32" })).toEqual([
      join(win, "tool.exe"),
      join(win, "tool.cmd"),
    ]);
  });
});

/**
 * Guard: nothing in `packages/<name>/src` may reach child processes except
 * through `aft-bridge/src/child-process.ts`, whose wrappers force
 * `windowsHide: true`. Node's own spawn functions default it to false, and on
 * Windows a console child of a host with no console (for example a background
 * service) then opens a visible console window that steals focus.
 *
 * Rather than trying to recognise every call form (`spawnSync(...)`,
 * `cp.execFileSync(...)`, destructured `require`, ...), the guard rejects every
 * way of obtaining the raw functions: any `"node:child_process"` or
 * `"child_process"` module specifier (import, dynamic import, require), and
 * Bun's own `Bun.spawn`, `Bun.spawnSync` and `Bun.$`. Tests are exempt.
 */
describe("child-process guard", () => {
  const packagesRoot = resolve(import.meta.dir, "../../..");

  /**
   * Files allowed to bypass the wrappers, each with a pattern that must still
   * match the code to prove the window is hidden some other way.
   */
  const ALLOWLIST: Record<string, RegExp> = {
    // The wrappers themselves.
    "aft-bridge/src/child-process.ts": /const HIDDEN = \{ windowsHide: true \} as const;/,
    // `--version` probe that runs inside a worker thread from inline source
    // (`eval: true`), so it cannot import the wrappers; it passes the option
    // to its spawnSync call by hand instead.
    "aft-bridge/src/version-probe.ts": /spawnSync\([^)]*\{[^}]*\n\s*windowsHide: true,\n\s*\}\);/,
  };

  const FORBIDDEN: Array<{ pattern: RegExp; what: string }> = [
    { pattern: /["'`](?:node:)?child_process["'`]/, what: "child_process module specifier" },
    { pattern: /\bBun\s*\.\s*(?:spawn|spawnSync)\b/, what: "Bun.spawn" },
    { pattern: /\bBun\s*\.\s*\$/, what: "Bun.$ shell" },
  ];

  function isTestPath(path: string): boolean {
    return /(^|\/)(__tests__|test-utils)\//.test(path) || /\.(test|spec)\.[cm]?[jt]sx?$/.test(path);
  }

  function sourceFiles(dir: string, out: string[] = []): string[] {
    for (const entry of readdirSync(dir)) {
      if (entry === "node_modules" || entry === "dist") continue;
      const path = join(dir, entry);
      if (statSync(path).isDirectory()) sourceFiles(path, out);
      else if (/\.[cm]?[jt]sx?$/.test(entry)) out.push(path);
    }
    return out;
  }

  function violations(): string[] {
    const found: string[] = [];
    for (const pkg of readdirSync(packagesRoot)) {
      const srcDir = join(packagesRoot, pkg, "src");
      if (!statSync(join(packagesRoot, pkg)).isDirectory()) continue;
      let files: string[];
      try {
        files = sourceFiles(srcDir);
      } catch {
        continue; // package without a src directory
      }
      for (const file of files) {
        const rel = relative(packagesRoot, file).split("\\").join("/");
        if (isTestPath(rel) || rel in ALLOWLIST) continue;
        const lines = readFileSync(file, "utf8").split(/\r?\n/);
        lines.forEach((line, index) => {
          for (const { pattern, what } of FORBIDDEN) {
            if (pattern.test(line)) found.push(`${rel}:${index + 1}: ${what}: ${line.trim()}`);
          }
        });
      }
    }
    return found;
  }

  test("scans the packages that ship TypeScript", () => {
    // Keeps the guard from passing vacuously if the root path is ever wrong.
    const scanned = sourceFiles(join(packagesRoot, "aft-bridge", "src"));
    expect(scanned.some((file) => file.endsWith("bridge.ts"))).toBe(true);
    for (const pkg of ["aft-cli", "opencode-plugin", "pi-plugin"]) {
      expect(sourceFiles(join(packagesRoot, pkg, "src")).length).toBeGreaterThan(0);
    }
  });

  test("no source module starts child processes outside the shared wrappers", () => {
    expect(violations()).toEqual([]);
  });

  test("allowlisted files still hide the window themselves", () => {
    for (const [rel, marker] of Object.entries(ALLOWLIST)) {
      expect({
        rel,
        hides: marker.test(readFileSync(join(packagesRoot, rel), "utf8")),
      }).toEqual({ rel, hides: true });
    }
  });
});
