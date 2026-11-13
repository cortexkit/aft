import { expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { maybeAppendGrepSearchHint } from "../bash-hints.js";
import { hostFallbackPathWithShims } from "../bash-host-fallback.js";
import { formatCallgraphSections } from "../callgraph-format.js";

test("host fallback prepends the platform gh shim", () => {
  const root = mkdtempSync(join(tmpdir(), "aft-shim-platform-"));
  try {
    const shims = join(root, "shims");
    mkdirSync(shims);
    for (const platform of ["win32", "linux"] as const) {
      const name = platform === "win32" ? "gh.cmd" : "gh";
      writeFileSync(join(shims, name), "shim");
      expect(hostFallbackPathWithShims({ AFT_STORAGE_DIR: root, PATH: "ambient" }, platform)).toBe(
        `${shims}${platform === "win32" ? ";" : ":"}ambient`,
      );
      rmSync(join(shims, name));
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("callgraph preserves a sibling of the home directory", () => {
  const file = `${homedir()}2/x.ts`;
  expect(
    formatCallgraphSections("call_tree", { name: "run", file, line: 1, children: [] }).join("\n"),
  ).toContain(file);
});

test("grep slash patterns do not count as path operands", () => {
  const root = mkdtempSync(join(tmpdir(), "aft-grep-patterns-"));
  try {
    const old = new Date(Date.now() - 61_000);
    // A directory matching the pattern would wrongly trigger the hint if the
    // pattern were collected as a path, even with missing operands suppressed.
    for (const operand of ["a/b", "src"]) {
      const dir = join(root, operand);
      mkdirSync(dir, { recursive: true });
      utimesSync(dir, old, old);
    }
    writeFileSync(join(root, "file.ts"), "a/b\n");
    for (const command of [
      "grep 'a/b' /etc/hosts",
      "grep -e 'a/b' /etc/hosts",
      "grep --regexp='a/b' /etc/hosts",
      "rg -- 'a/b' /etc/hosts",
    ]) {
      expect(maybeAppendGrepSearchHint("hits", command, true, root)).toBe("hits");
    }
    expect(maybeAppendGrepSearchHint("hits", "grep 'a/b' src/", true, root)).not.toBe("hits");
    expect(maybeAppendGrepSearchHint("hits", "grep 'a/b' file.ts", true, root)).toBe("hits");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

import { relativePathEscapesRoot, shortenHomePath } from "../path-display.js";

test("home shortening respects platform separators and case", () => {
  for (const [path, home, platform, expected] of [
    ["/", "/", "linux", "~"],
    ["/home/user", "/home/user", "linux", "~"],
    ["/home/user/x", "/home/user", "linux", "~/x"],
    ["/home/user2/x", "/home/user", "linux", "/home/user2/x"],
    ["C:\\Users\\USER\\x", "c:\\users\\user", "win32", "~\\x"],
    ["C:/Users/USER/x", "c:\\users\\user", "win32", "~/x"],
    ["C:/Users/user2/x", "C:/Users/user", "win32", "C:/Users/user2/x"],
  ] as const)
    expect(shortenHomePath(path, home, platform)).toBe(expected);
});

test("containment rejects parent segments but allows dot-dot names", () => {
  for (const platform of ["linux", "win32"] as const) {
    for (const rel of ["", "..cache/file", "..cache", "file"])
      expect(relativePathEscapesRoot(rel, platform)).toBe(false);
    for (const rel of ["..", "../file", "/absolute"])
      expect(relativePathEscapesRoot(rel, platform)).toBe(true);
  }
  for (const rel of ["..\\file", "D:\\file"])
    expect(relativePathEscapesRoot(rel, "win32")).toBe(true);
});
