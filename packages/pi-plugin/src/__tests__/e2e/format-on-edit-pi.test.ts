/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { chmod, mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

import { createHarness, type Harness, type PreparedBinary, prepareBinary } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

// Duplicated from opencode-plugin/format-helpers.ts (trimmed to Pi needs).
interface FormatPreset {
  configFiles: Array<{ path: string; content: string }>;
  explicitFormatter?: Record<string, string>;
}

interface FakeFormatterShim {
  name: string;
  script: string;
}

const TS_INPUT = `export    function   foo( a:number,b :number ){return a+b;}
const   x={a:1,b   :2,c:3}
console.log(foo(1,2),x)
`;

const BIOME_TS_PRESET: FormatPreset = {
  configFiles: [
    {
      path: "biome.json",
      content: JSON.stringify(
        {
          formatter: { enabled: true, indentStyle: "space", indentWidth: 2 },
          files: { includes: ["**/*.ts"] },
        },
        null,
        2,
      ),
    },
    {
      path: "package.json",
      content: JSON.stringify({ name: "pi-format-test", private: true }),
    },
  ],
};

const BIOME_TS_EXCLUDED_PRESET: FormatPreset = {
  configFiles: [
    {
      path: "biome.json",
      content: JSON.stringify(
        {
          formatter: { enabled: true, indentStyle: "space", indentWidth: 2 },
          files: { includes: ["src/**/*.ts"] },
        },
        null,
        2,
      ),
    },
    {
      path: "package.json",
      content: JSON.stringify({ name: "pi-format-excluded-test" }),
    },
  ],
};

const NO_FORMATTER_PRESET: FormatPreset = { configFiles: [] };

function formatterPreset(tool: string): FormatPreset {
  return { configFiles: [], explicitFormatter: { typescript: tool } };
}

function formattingBiomeShim(name = "biome"): FakeFormatterShim {
  return {
    name,
    script: `#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "biome 2.0.0"
  exit 0
fi
file=""
for arg in "$@"; do file="$arg"; done
cat > "$file" <<'EOF'
export function foo(a: number, b: number) {
  return a + b;
}
const x = { a: 1, b: 2, c: 3 };
console.log(foo(1, 2), x);
EOF
exit 0
`,
  };
}

function excludedPathShim(name = "biome"): FakeFormatterShim {
  return {
    name,
    script: `#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "biome 2.0.0"
  exit 0
fi
echo "No files were processed in the specified paths." >&2
exit 1
`,
  };
}

async function installPreset(h: Harness, preset: FormatPreset, shims: FakeFormatterShim[] = []) {
  for (const file of preset.configFiles) {
    await mkdir(join(h.tempDir, file.path, ".."), { recursive: true });
    await writeFile(h.path(file.path), file.content, "utf8");
  }
  if (shims.length > 0) {
    const binDir = h.path("node_modules", ".bin");
    await mkdir(binDir, { recursive: true });
    for (const shim of shims) {
      const shimPath = join(binDir, shim.name);
      await writeFile(
        shimPath,
        shim.script.startsWith("#!") ? shim.script : `#!/bin/sh\n${shim.script}`,
      );
      await chmod(shimPath, 0o755);
    }
  }
  const configureParams: Record<string, unknown> = {
    project_root: h.tempDir,
    harness: "pi",
    format_on_edit: true,
    validate_on_edit: "syntax",
  };
  if (preset.explicitFormatter) configureParams.formatter = preset.explicitFormatter;
  const configured = await h.bridge.send("configure", configureParams);
  expect(configured.success).toBe(true);
}

function detailsOf(result: Awaited<ReturnType<Harness["callTool"]>>): Record<string, unknown> {
  return (result.details ?? {}) as Record<string, unknown>;
}

maybeDescribe("e2e Pi format_on_edit parity", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: Harness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await Promise.allSettled(
      harnesses.splice(0, harnesses.length).map((harness) => harness.cleanup()),
    );
  });

  async function formatHarness(
    preset: FormatPreset,
    shims: FakeFormatterShim[] = [],
    config: Record<string, unknown> = {
      format_on_edit: true,
      validate_on_edit: "syntax",
    },
  ): Promise<Harness> {
    const h = await createHarness(preparedBinary, {
      fixtureNames: [],
      timeoutMs: 30_000,
      config,
    });
    harnesses.push(h);
    await installPreset(h, preset, shims);
    return h;
  }

  test("Pi hoisted write triggers formatter", async () => {
    const h = await formatHarness(BIOME_TS_PRESET, [formattingBiomeShim()]);
    const result = await h.callTool("write", {
      filePath: "src/write.ts",
      content: TS_INPUT,
    });

    expect(await readFile(h.path("src/write.ts"), "utf8")).toContain("export function foo");
    // Pi wrapper now surfaces the formatter outcome through details.formatted
    // (fixed in v0.18.3). Successful formatting reports formatted=true with no
    // skip reason. Worker's original assertion expected `undefined` because
    // the wrapper used to drop these fields entirely.
    expect(detailsOf(result).formatted).toBe(true);
    expect(detailsOf(result).formatSkippedReason).toBeUndefined();
    expect(h.text(result)).not.toContain("Auto-formatted.");
  });

  test("Pi hoisted edit triggers formatter", async () => {
    const h = await formatHarness(BIOME_TS_PRESET, [formattingBiomeShim()]);
    await mkdir(h.path("src"), { recursive: true });
    await writeFile(h.path("src/edit.ts"), "export const value = 1;\n", "utf8");
    const result = await h.callTool("edit", {
      filePath: "src/edit.ts",
      oldString: "1",
      newString: "2",
    });

    expect(await readFile(h.path("src/edit.ts"), "utf8")).toContain("export function foo");
    expect(detailsOf(result).formatted).toBe(true);
    expect(detailsOf(result).formatSkippedReason).toBeUndefined();
  });

  test("Pi formatter_excluded_path propagates through raw Rust response", async () => {
    const h = await formatHarness(BIOME_TS_EXCLUDED_PRESET, [excludedPathShim()]);
    const file = h.path("scratch/excluded.ts");
    const response = await h.bridge.send("write", {
      file,
      content: TS_INPUT,
      create_dirs: true,
    });

    expect(response.success).toBe(true);
    expect(response.formatted).toBe(false);
    expect(response.format_skipped_reason).toBe("formatter_excluded_path");
    expect(await readFile(file, "utf8")).toBe(TS_INPUT);
  });

  test("Pi format_on_edit=false config plumbing honors the flag", async () => {
    const h = await formatHarness(BIOME_TS_PRESET, [formattingBiomeShim()], {
      format_on_edit: false,
      validate_on_edit: "syntax",
    });
    const response = await h.bridge.send("write", {
      file: h.path("src/disabled.ts"),
      content: TS_INPUT,
      create_dirs: true,
    });

    expect(response.formatted).toBe(false);
    expect(response.format_skipped_reason).toBe("no_formatter_configured");
    expect(await readFile(h.path("src/disabled.ts"), "utf8")).toBe(TS_INPUT);
  });

  test("Pi formatter_not_installed skip reason surfaces", async () => {
    const h = await formatHarness(formatterPreset("prettier"));
    const response = await h.bridge.send("write", {
      file: h.path("src/missing.ts"),
      content: TS_INPUT,
      create_dirs: true,
    });

    expect(response.formatted).toBe(false);
    expect(response.format_skipped_reason).toBe("formatter_not_installed");
  });

  test("Pi multi-file edit transaction formats both files", async () => {
    const h = await formatHarness(BIOME_TS_PRESET, [formattingBiomeShim()]);
    await mkdir(h.path("src"), { recursive: true });
    const response = await h.bridge.send("transaction", {
      operations: [
        { command: "write", file: h.path("src/a.ts"), content: TS_INPUT },
        { command: "write", file: h.path("src/b.ts"), content: TS_INPUT },
      ],
    });

    expect(response.success).toBe(true);
    const files = response.results as Array<Record<string, unknown>>;
    expect(files).toHaveLength(2);
    expect(files.every((file) => file.formatted === true)).toBe(true);
    expect(await readFile(h.path("src/a.ts"), "utf8")).toContain("export function foo");
    expect(await readFile(h.path("src/b.ts"), "utf8")).toContain("export function foo");
  });

  test("Pi glob edit_match formats every matched file", async () => {
    const h = await formatHarness(BIOME_TS_PRESET, [formattingBiomeShim()]);
    await mkdir(h.path("src"), { recursive: true });
    await writeFile(h.path("src/a.ts"), "export const a = OLD;\n", "utf8");
    await writeFile(h.path("src/b.ts"), "export const b = OLD;\n", "utf8");

    const response = await h.bridge.send("edit_match", {
      file: h.path("src/*.ts"),
      match: "OLD",
      replacement: "NEW",
    });

    expect(response.success).toBe(true);
    expect(response.total_files).toBe(2);
    expect(response.format_skipped_count).toBe(0);
    expect(response.format_skip_reasons).toEqual([]);
    const files = response.files as Array<Record<string, unknown>>;
    expect(files.every((file) => file.formatted === true)).toBe(true);
    expect(files.every((file) => file.format_skipped_reason === undefined)).toBe(true);
    expect(await readFile(h.path("src/a.ts"), "utf8")).toContain("export function foo");
    expect(await readFile(h.path("src/b.ts"), "utf8")).toContain("export function foo");
  });

  test("Pi glob edit_match surfaces actionable skip reasons in agent text", async () => {
    const h = await formatHarness(formatterPreset("biome"), [excludedPathShim()]);
    await mkdir(h.path("src"), { recursive: true });
    await writeFile(h.path("src/a.ts"), 'export const a = "OLD";\n', "utf8");
    await writeFile(h.path("src/b.ts"), 'export const b = "OLD";\n', "utf8");

    const wrapped = await h.callTool("edit", {
      filePath: h.path("src/*.ts"),
      oldString: "OLD",
      newString: "NEW",
    });

    expect(h.text(wrapped)).toContain("formatter skipped some glob edit result file(s)");
    expect(h.text(wrapped)).toContain("formatter_excluded_path");
  });

  test("Pi appendContent triggers formatter (bug #4 fix)", async () => {
    // Bug #4 (v0.18.3): append now runs auto_format. The fake biome shim
    // overwrites the entire file with formatted output, so post-format
    // content matches the shim's canonical block. The test verifies:
    //   - response.formatted is `true` (not undefined; pre-fix hardcoded)
    //   - on-disk content reflects the shim's formatting (not raw append)
    const h = await formatHarness(BIOME_TS_PRESET, [formattingBiomeShim()]);
    await mkdir(h.path("src"), { recursive: true });
    await writeFile(h.path("src/append.ts"), "export const before = 1;\n", "utf8");
    const response = await h.bridge.send("edit_match", {
      file: h.path("src/append.ts"),
      op: "append",
      append_content: TS_INPUT,
      include_diff: true,
    });

    expect(response.formatted).toBe(true);
    expect(response.format_skipped_reason).toBeUndefined();
    // The fake biome shim rewrites the file to its canonical formatted
    // block (see formattingBiomeShim above). After append+format, that's
    // what's on disk — not the raw append concatenation.
    expect(await readFile(h.path("src/append.ts"), "utf8")).toContain("export function foo");
  });

  test("Pi response shape parity: Rust formatted/format_skipped_reason flow through to wrapper details", async () => {
    const h = await formatHarness(NO_FORMATTER_PRESET);
    const raw = await h.bridge.send("write", {
      file: h.path("src/raw.ts"),
      content: TS_INPUT,
      create_dirs: true,
    });
    const wrapped = await h.callTool("write", {
      filePath: "src/wrapped.ts",
      content: TS_INPUT,
    });
    const wrappedDetails = detailsOf(wrapped);

    expect(raw.formatted).toBe(false);
    expect(raw.format_skipped_reason).toBe("no_formatter_configured");
    // Pi wrapper now exposes the same fields (in camelCase to match Pi
    // conventions). Fixed in v0.18.3 — see packages/pi-plugin/src/tools/
    // hoisted.ts::buildMutationResult.
    expect(wrappedDetails.formatted).toBe(false);
    expect(wrappedDetails.formatSkippedReason).toBe("no_formatter_configured");
    // Benign skip reasons stay silent in agent text — agent has no actionable
    // remediation when the language has no configured formatter.
    expect(h.text(wrapped)).not.toContain("Auto-formatted.");
    expect(h.text(wrapped)).not.toContain("Note: formatter");
  });

  test("Pi response shape parity: actionable skip reasons surface a one-line note in agent text", async () => {
    // formatter_not_installed (non-benign) verifies the note path. Configure
    // explicit formatter=prettier with NO prettier on the harness's PATH /
    // node_modules. Rust recognizes the explicit name (so it doesn't fall
    // back to no_formatter_configured) and returns formatter_not_installed
    // when spawn fails. "prettier" works for this because the test fixture
    // has no node_modules/.bin/prettier and the harness shouldn't have one
    // on PATH either; if a future test environment installs prettier, this
    // test should be updated to use a different recognized-but-uninstalled
    // formatter name.
    const h = await formatHarness(formatterPreset("prettier"));
    const wrapped = await h.callTool("write", {
      filePath: "src/with-note.ts",
      content: TS_INPUT,
    });
    const wrappedDetails = detailsOf(wrapped);
    expect(wrappedDetails.formatted).toBe(false);
    expect(wrappedDetails.formatSkippedReason).toBe("formatter_not_installed");
    expect(h.text(wrapped)).toContain("formatter binary not installed");
  });
});
