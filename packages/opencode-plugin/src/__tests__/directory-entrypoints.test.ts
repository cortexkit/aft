import { describe, expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const packageRoot = fileURLToPath(new URL("../../", import.meta.url));

/**
 * OpenCode 2 resolves a plugin given as a DIRECTORY by joining paths literally:
 * `<dir>/server`, then `<dir>/index` (`resolve()` in @opencode/plugin
 * dist/host.js). It never consults package.json exports for that shape, and it
 * swallows every resolution error — so a package missing these files is skipped
 * in silence, with no load attempt and no warning to explain it.
 *
 * V1 resolves the same directory through package.json `main`, which is why the
 * existing V1 load-matrix rows pass without these files and why the gap went
 * unseen: the only V2 coverage went through explicit entry files or the E2E
 * harness's generated wrapper, both of which bypass directory resolution.
 */
describe("directory-path plugin entrypoints", () => {
  test("the root entrypoints OpenCode 2 resolves for a directory target exist", async () => {
    for (const entry of ["index.js", "server.js", "tui.js"]) {
      const source = await readFile(join(packageRoot, entry), "utf8");
      expect(source.length).toBeGreaterThan(0);
    }
  });

  test("the V2 server entrypoint is preferred and carries the Effect plugin shape", async () => {
    // `<dir>/server` is tried before `<dir>/index`, so this file decides whether
    // a checkout loads as a V2 plugin or falls back to the V1 default export.
    const module = await import(join(packageRoot, "server.js"));
    expect(typeof module.default).toBe("object");
    expect(module.default).not.toBeNull();
    expect(Object.keys(module.default as Record<string, unknown>)).toEqual(
      expect.arrayContaining(["id", "server", "effect"]),
    );
  });

  test("the TUI entrypoint exposes the sidebar registration shape", async () => {
    // Resolved at `<dir>/tui`; absent it, the sidebar never appears and the
    // host says nothing. Imported under Bun because the chain reaches .tsx.
    const module = await import(join(packageRoot, "tui.js"));
    const entry = module.default as Record<string, unknown>;
    expect(Object.keys(entry)).toEqual(expect.arrayContaining(["id", "tui", "setup"]));
    expect(typeof entry.tui).toBe("function");
  });

  test("the root entrypoint still exposes the V1 plugin function", async () => {
    const module = await import(join(packageRoot, "index.js"));
    expect(typeof module.default).toBe("function");
  });

  test("both entrypoints are published, because installed plugins are also loaded by directory", async () => {
    // The Docker harness points at
    // `<node_modules>/@cortexkit/aft-opencode` as a directory, and a user may
    // do the same; omitting these from `files` would reintroduce the silent
    // skip for anyone who installs the package rather than cloning it.
    const manifest = JSON.parse(await readFile(join(packageRoot, "package.json"), "utf8")) as {
      files?: string[];
    };
    expect(manifest.files).toContain("index.js");
    expect(manifest.files).toContain("server.js");
    expect(manifest.files).toContain("tui.js");
  });
});
