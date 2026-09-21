import { describe, expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const packageRoot = fileURLToPath(new URL("../../", import.meta.url));

/**
 * OpenCode 2 resolves a plugin given as a DIRECTORY by joining paths literally:
 * `<dir>/server`, then `<dir>/index`, and separately `<dir>/tui`
 * (`resolve()` in @opencode/plugin dist/host.js). It never consults
 * package.json exports for that shape, and it swallows every resolution error
 * — so a package missing these files is skipped in silence, with no load
 * attempt and no warning to explain it.
 *
 * V1 resolves the same directory through package.json `main`, which is why the
 * V1 load-matrix rows pass without these files and why the gap went unseen:
 * the only V2 coverage went through explicit entry files or the E2E harness's
 * generated wrapper, both of which bypass directory resolution.
 *
 * This file is the static half of the guard and runs without a build. The
 * runtime half — that the host actually reaches these entries and gets a
 * working plugin — is the load matrix's directory row against a real GA host,
 * which is the only place that can honestly prove it.
 */
describe("directory-path plugin entrypoints", () => {
  const entries = [
    {
      file: "index.js",
      target: "./dist/index.js",
      why: "V1 default, and V2's fallback after server",
    },
    {
      file: "server.js",
      target: "./dist/entry/server.js",
      why: "V2 server, resolved before index",
    },
    {
      file: "tui.js",
      target: "./src/entry/tui.mjs",
      why: "V2 TUI feature; absent it, the sidebar vanishes silently",
    },
  ];

  for (const { file, target, why } of entries) {
    test(`${file} re-exports ${target} (${why})`, async () => {
      const source = await readFile(join(packageRoot, file), "utf8");
      // A re-export, not a copy: the shim exists only to give the literal
      // path resolver something to find, and must not drift from the entry
      // the exports map serves to a named install.
      expect(source).toContain(`from "${target}"`);
      expect(source).toMatch(/export\s+(\*|\{\s*default\s*\})/);
    });
  }

  test("every entrypoint is published, because installed plugins are also loaded by directory", async () => {
    // The Docker harness points at `<node_modules>/@cortexkit/aft-opencode` as
    // a directory, and a user may do the same; omitting these from `files`
    // would reintroduce the silent skip for anyone who installs the package
    // rather than cloning it.
    const manifest = JSON.parse(await readFile(join(packageRoot, "package.json"), "utf8")) as {
      files?: string[];
      exports?: Record<string, unknown>;
    };
    for (const { file } of entries) {
      expect(manifest.files).toContain(file);
    }
    // The exports map still serves named installs; the shims are additional,
    // never a replacement.
    expect(Object.keys(manifest.exports ?? {})).toEqual(
      expect.arrayContaining([".", "./server", "./tui"]),
    );
  });
});
