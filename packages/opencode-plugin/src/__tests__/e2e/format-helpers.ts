/**
 * Test helpers for format_on_edit e2e suites.
 *
 * Each test creates an isolated temp project with its own formatter config
 * files (biome.json / Cargo.toml / pyproject.toml / etc), writes/edits
 * files via the real BinaryBridge, and asserts on the post-format file
 * content + the response's `formatted` / `format_skipped_reason` fields.
 *
 * The shared `e2e/helpers.ts` harness gives us `tempDir + bridge`. This
 * helper layers formatter-config installation and per-language fixture
 * generation on top.
 */

import { existsSync } from "node:fs";
import { symlink, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { createHarness, type E2EHarness, type PreparedBinary } from "./helpers.js";

/**
 * Names of real formatters/checkers we try to symlink into the harness's
 * `node_modules/.bin/` when no shim has been provided. AFT's resolver looks
 * at `<project_root>/node_modules/.bin/<tool>` first, so this keeps tests
 * runnable in CI without polluting `PATH`.
 */
const REAL_TOOLS = ["biome"] as const;

/**
 * Walk up from this file looking for a `node_modules/.bin/` that contains
 * the named tool. In a Bun workspace, package-local `node_modules/.bin/`
 * only carries package-specific binaries; shared deps like Biome live in
 * the workspace-root `node_modules/.bin/`. We need to keep walking past
 * the first matched bin dir if it doesn't have the specific tool.
 */
function findToolInWorkspace(tool: string): string | null {
  let cur = dirname(new URL(import.meta.url).pathname);
  for (let i = 0; i < 10; i++) {
    const candidate = join(cur, "node_modules", ".bin", tool);
    if (existsSync(candidate)) return candidate;
    const next = resolve(cur, "..");
    if (next === cur) break;
    cur = next;
  }
  return null;
}

/**
 * Per-language formatter config installer. Each preset writes the minimum
 * config files a project needs for AFT's formatter detection logic in
 * `crates/aft/src/format.rs::formatter_candidates` to pick up the chosen
 * tool.
 *
 * Add new presets here as the test suite grows.
 */
export interface FormatPreset {
  /** Files to write into the temp project root before configure(). */
  configFiles: Array<{ path: string; content: string }>;
  /** Optional explicit `formatter` map for `.opencode/aft.jsonc`. */
  explicitFormatter?: Record<string, string>;
  /** Optional explicit `checker` map. */
  explicitChecker?: Record<string, string>;
}

/**
 * Biome-managed TypeScript/JavaScript project. Auto-detection picks biome
 * up via `biome.json`. The `files.includes` is intentionally permissive so
 * the formatter operates on every file the test creates.
 */
export const BIOME_TS_PRESET: FormatPreset = {
  configFiles: [
    {
      path: "biome.json",
      content: JSON.stringify(
        {
          $schema: "https://biomejs.dev/schemas/2.4.7/schema.json",
          formatter: {
            enabled: true,
            indentStyle: "space",
            indentWidth: 2,
            lineWidth: 100,
          },
          javascript: {
            formatter: { quoteStyle: "double", semicolons: "always" },
          },
          files: { includes: ["**/*.ts", "**/*.tsx", "**/*.js", "**/*.jsx"] },
        },
        null,
        2,
      ),
    },
    // Minimal package.json so biome resolves a project root unambiguously.
    {
      path: "package.json",
      content: JSON.stringify({ name: "format-on-edit-test", version: "0.0.0", private: true }),
    },
  ],
};

/**
 * Biome project where `files.includes` deliberately EXCLUDES the test file's
 * directory. Used to verify the `formatter_excluded_path` skip reason.
 */
export const BIOME_TS_EXCLUDED_PRESET: FormatPreset = {
  configFiles: [
    {
      path: "biome.json",
      content: JSON.stringify(
        {
          $schema: "https://biomejs.dev/schemas/2.4.7/schema.json",
          formatter: { enabled: true, indentStyle: "space", indentWidth: 2 },
          // Includes ONLY src/, so files written to scratch/ are excluded.
          files: { includes: ["src/**/*.ts"] },
        },
        null,
        2,
      ),
    },
    {
      path: "package.json",
      content: JSON.stringify({ name: "format-on-edit-excluded-test", version: "0.0.0" }),
    },
  ],
};

/**
 * Rust project with `Cargo.toml`. Auto-detection picks rustfmt; no rustfmt.toml
 * needed (rustfmt uses its built-in defaults when no config is present).
 */
export const RUSTFMT_PRESET: FormatPreset = {
  configFiles: [
    {
      path: "Cargo.toml",
      content: '[package]\nname = "format_on_edit_test"\nversion = "0.0.0"\nedition = "2021"\n',
    },
  ],
};

/**
 * Go project with `go.mod`. Auto-detection picks gofmt.
 */
export const GOFMT_PRESET: FormatPreset = {
  configFiles: [
    {
      path: "go.mod",
      content: "module format-on-edit-test\n\ngo 1.21\n",
    },
  ],
};

/**
 * Python project with `pyproject.toml` + `[tool.ruff]` so the formatter
 * resolves to ruff format.
 */
export const RUFF_PRESET: FormatPreset = {
  configFiles: [
    {
      path: "pyproject.toml",
      content:
        '[project]\nname = "format-on-edit-test"\nversion = "0.0.0"\n\n[tool.ruff]\nline-length = 88\n',
    },
  ],
};

/**
 * No formatter config — used to verify `no_formatter_configured` skip reason.
 */
export const NO_FORMATTER_PRESET: FormatPreset = {
  configFiles: [],
};

/**
 * Spec for a fake formatter shim installed via `node_modules/.bin/<name>`.
 * The shim is a shell script that exits with the given stderr/stdout/code
 * — used to deterministically simulate timeout/error/excluded-path/etc
 * formatter outcomes without depending on the real biome/rustfmt being
 * installed in CI.
 */
export interface FakeFormatterShim {
  /** Binary name as resolved through `node_modules/.bin/<name>`. */
  name: string;
  /** Shell script body. Receives the file path as `$1`. */
  script: string;
}

/**
 * Set up a temp project with the given formatter preset, install any fake
 * shims, configure the bridge, and return the harness.
 *
 * The `restrict_to_project_root` is intentionally left at the (post-v0.18.2)
 * default of `false` so tests don't get blocked on path validation.
 *
 * @param preparedBinary   from `prepareBinary()` in `helpers.ts`
 * @param preset           formatter preset (BIOME_TS_PRESET etc)
 * @param shims            optional fake formatter shims to install in
 *                         `node_modules/.bin/<name>`
 */
export async function createFormatHarness(
  preparedBinary: PreparedBinary,
  preset: FormatPreset,
  shims: FakeFormatterShim[] = [],
  /**
   * When true, skip the workspace-tool symlink step (Step 2b). Use this
   * in tests that explicitly want to verify the `formatter_not_installed`
   * path — the symlink would otherwise make the real formatter discoverable
   * and cause `formatted: true` instead.
   */
  suppressRealToolSymlinks = false,
): Promise<E2EHarness> {
  const harness = await createHarness(preparedBinary, {
    fixtureNames: [],
    timeoutMs: 30_000,
  });

  // Step 1: Install preset config files.
  for (const file of preset.configFiles) {
    await writeFile(harness.path(file.path), file.content, "utf8");
  }

  // Step 2: Install any fake formatter shims under node_modules/.bin/.
  const { mkdir, chmod } = await import("node:fs/promises");
  const binDir = harness.path("node_modules", ".bin");
  const shimmedNames = new Set(shims.map((s) => s.name));
  if (shims.length > 0) {
    await mkdir(binDir, { recursive: true });
    for (const shim of shims) {
      const shimPath = join(binDir, shim.name);
      // Add `#!/bin/sh` if missing.
      const body = shim.script.startsWith("#!") ? shim.script : `#!/bin/sh\n${shim.script}`;
      await writeFile(shimPath, body, "utf8");
      await chmod(shimPath, 0o755);
    }
  }

  // Step 2b: For real-tool tests (no shim provided), symlink the workspace's
  //          installed formatter into the harness's node_modules/.bin/ so
  //          AFT's project-local resolver finds it. This lets CI run the
  //          real-Biome path without putting node_modules/.bin on PATH.
  //          Skipped when suppressRealToolSymlinks=true (formatter_not_installed tests).
  await mkdir(binDir, { recursive: true });
  for (const tool of suppressRealToolSymlinks ? [] : REAL_TOOLS) {
    if (shimmedNames.has(tool)) continue; // shim takes precedence
    const src = findToolInWorkspace(tool);
    if (!src) continue;
    const dest = join(binDir, tool);
    if (existsSync(dest)) continue;
    try {
      await symlink(src, dest);
    } catch {
      // benign — race with another test or platform without symlinks
    }
  }

  // Step 3: Configure the bridge with format_on_edit on and explicit
  //         formatter/checker maps if the preset specifies them.
  const configureParams: Record<string, unknown> = {
    harness: "opencode",
    project_root: harness.tempDir,
    format_on_edit: true,
    validate_on_edit: "syntax",
  };
  if (preset.explicitFormatter) {
    configureParams.formatter = preset.explicitFormatter;
  }
  if (preset.explicitChecker) {
    configureParams.checker = preset.explicitChecker;
  }
  const configResp = await harness.bridge.send("configure", configureParams);
  if (configResp.success === false) {
    throw new Error(`bridge configure failed: ${(configResp as { message?: string }).message}`);
  }

  return harness;
}

/**
 * Common content fixtures used across multiple test files. Each is
 * intentionally deformatted so the post-format diff is observable.
 */
export const FIXTURES = {
  /** Deformatted TypeScript that biome will rewrite (spacing, semicolons). */
  ts_deformatted: `export    function   foo( a:number,b :number ){return a+b;}
const   x={a:1,b   :2,c:3}
console.log(foo(1,2),x)
`,
  /** Already-formatted TS — formatter should run but produce no diff. */
  ts_formatted: `export function foo(a: number, b: number) {
\treturn a + b;
}
`,
  /** Syntactically invalid TS — formatter typically refuses, validate fails. */
  ts_invalid: `export function broken( {{
`,
  /** Deformatted Rust — rustfmt will rewrite. */
  rust_deformatted: `fn   main(){let    x=42;let y  =  vec![1,2,   3];println!("{} {:?}",x,y);}
`,
  /** Deformatted Go — gofmt will rewrite. */
  go_deformatted: `package main
import "fmt"
func main(){x:=42;fmt.Println(x)}
`,
  /** Deformatted Python — ruff will rewrite. */
  py_deformatted: `def foo( a,b ):return a+b
x={"a":1,"b" :  2}
print( foo(1,2),x )
`,
} as const;

/**
 * Helper: compose a biome-style "excluded path" stderr response in a fake
 * shim so tests can deterministically trigger `formatter_excluded_path`.
 */
export function biomeExcludedPathShim(name = "biome"): FakeFormatterShim {
  return {
    name,
    script: `#!/bin/sh
echo "× No files were processed in the specified paths." >&2
echo "  i Check your biome.json or biome.jsonc to ensure the paths are not ignored by the configuration." >&2
exit 1
`,
  };
}

/**
 * Helper: shim that always succeeds without modifying the file. Used to
 * verify the formatted=true path runs even when the formatter is a no-op.
 */
export function noopFormatterShim(name = "biome"): FakeFormatterShim {
  return {
    name,
    script: `#!/bin/sh
exit 0
`,
  };
}

/**
 * Helper: shim that exits non-zero with an unrecognized error message —
 * used to verify the generic `error` skip reason still triggers when
 * stderr isn't a known exclusion fingerprint.
 */
export function genericErrorFormatterShim(name = "biome"): FakeFormatterShim {
  return {
    name,
    script: `#!/bin/sh
echo "fake formatter: something exploded" >&2
exit 2
`,
  };
}

/**
 * Helper: shim that hangs forever — used to test `formatter_timeout_secs`
 * + the `timeout` skip reason. Tests using this must set
 * `formatter_timeout_secs` to a small value via configure().
 */
export function hangingFormatterShim(name = "biome"): FakeFormatterShim {
  return {
    name,
    script: `#!/bin/sh
sleep 60
`,
  };
}

/**
 * Helper: shim that actually formats a TS file in a primitive way (collapses
 * runs of spaces). Useful for verifying that the formatter's modifications
 * land on disk and the response's `formatted: true` is truthful.
 */
export function tsCollapseSpacesShim(name = "biome"): FakeFormatterShim {
  return {
    name,
    // Args: "format" "--write" "<file>". The file path is the LAST arg.
    // Use POSIX-compatible last-arg extraction (${@: -1} is bash-only, not
    // supported by dash which is /bin/sh on Ubuntu/Debian CI runners).
    script: `#!/bin/sh
for file; do :; done
sed -E 's/  +/ /g' "$file" > "$file.tmp" && mv "$file.tmp" "$file"
exit 0
`,
  };
}
