import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

import { resolvePluginVersion } from "../plugin-version.js";

const packageRoot = resolve(import.meta.dir, "../..");
const expectedVersion = (
  JSON.parse(readFileSync(join(packageRoot, "package.json"), "utf8")) as {
    version: string;
  }
).version;

describe("resolvePluginVersion", () => {
  // Every entry the package publishes bundles index.ts at its own depth. A
  // depth that cannot see the manifest turns into a request for binary
  // v0.0.0 and a refused plugin load, so the check is per published entry,
  // not per source file.
  for (const entry of ["dist/index.js", "dist/entry/server.js"]) {
    test(`${entry} resolves the package version`, () => {
      const url = pathToFileURL(join(packageRoot, entry)).href;
      expect(resolvePluginVersion(url)).toBe(expectedVersion);
    });
  }

  test("a location with no matching manifest above it reports 0.0.0, never a foreign version", () => {
    const root = mkdtempSync(join(tmpdir(), "aft-plugin-version-"));
    // A foreign manifest one level up must not be trusted as ours.
    writeFileSync(
      join(root, "package.json"),
      JSON.stringify({ name: "someone-else", version: "9.9.9" }),
    );
    mkdirSync(join(root, "dist", "entry"), { recursive: true });
    const url = pathToFileURL(join(root, "dist", "entry", "server.js")).href;
    expect(resolvePluginVersion(url)).toBe("0.0.0");
  });
});
