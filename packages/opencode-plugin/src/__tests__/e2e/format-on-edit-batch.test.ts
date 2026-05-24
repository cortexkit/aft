/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { hoistedTools } from "../../tools/hoisted.js";
import type { PluginContext } from "../../types.js";
import { noopAsk } from "../test-helpers";
import {
  BIOME_TS_EXCLUDED_PRESET,
  BIOME_TS_PRESET,
  biomeExcludedPathShim,
  createFormatHarness,
  type FakeFormatterShim,
  type FormatPreset,
  tsCollapseSpacesShim,
} from "./format-helpers.js";
import {
  cleanupHarnesses,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
  readTextFile,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

// Should be promoted to format-helpers.ts if other suites need it.
const BIOME_TS_AND_RUSTFMT_PRESET: FormatPreset = {
  configFiles: [
    ...BIOME_TS_PRESET.configFiles,
    {
      path: "Cargo.toml",
      content: '[package]\nname = "batch_mixed_format_test"\nversion = "0.0.0"\nedition = "2021"\n',
    },
  ],
  explicitFormatter: { typescript: "biome", rust: "rustfmt" },
  explicitChecker: { typescript: "none", rust: "none" },
};

function createMockClient(): any {
  return {
    lsp: { status: async () => ({ data: [] }) },
    find: { symbols: async () => ({ data: [] }) },
  };
}

function createPluginContext(pool: BridgePool, storageDir: string): PluginContext {
  return { pool, client: createMockClient(), config: {} as PluginContext["config"], storageDir };
}

function createSdkContext(directory: string): ToolContext {
  return {
    sessionID: `format-on-edit-batch-${Math.random()}`,
    messageID: "format-on-edit-batch-message",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  };
}

async function createToolHarness(
  preparedBinary: PreparedBinary,
  preset: FormatPreset,
  shims: FakeFormatterShim[] = [tsCollapseSpacesShim("biome")],
): Promise<{
  h: E2EHarness;
  tools: ReturnType<typeof hoistedTools>;
  sdkCtx: ToolContext;
  pool: BridgePool;
}> {
  const h = await createFormatHarness(preparedBinary, preset, shims);
  const pool = new BridgePool(
    h.binaryPath,
    { timeoutMs: 20_000 },
    {
      storage_dir: join(h.tempDir, ".storage"),
      harness: "opencode",
      format_on_edit: true,
      validate_on_edit: "syntax",
    },
  );
  return {
    h,
    tools: hoistedTools(createPluginContext(pool, join(h.tempDir, ".storage"))),
    sdkCtx: createSdkContext(h.tempDir),
    pool,
  };
}

function parseToolJson(output: string): Record<string, unknown> {
  return JSON.parse(output.split("\n\nLSP errors detected")[0]) as Record<string, unknown>;
}

maybeDescribe("e2e format_on_edit batch operations", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];
  const pools: BridgePool[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await Promise.allSettled(pools.splice(0, pools.length).map((pool) => pool.shutdown()));
    await cleanupHarnesses(harnesses);
  });

  async function harness(preset: FormatPreset, shims?: FakeFormatterShim[]) {
    const created = await createToolHarness(preparedBinary, preset, shims);
    harnesses.push(created.h);
    pools.push(created.pool);
    return created;
  }

  test("single-file batch — formatter runs once after all edits", async () => {
    const { h, bridge } = await (async () => {
      const h = await createFormatHarness(preparedBinary, BIOME_TS_PRESET, [
        tsCollapseSpacesShim("biome"),
      ]);
      harnesses.push(h);
      return { h, bridge: h.bridge };
    })();
    const file = h.path("single.ts");
    await writeFile(
      file,
      "export const a = 1;\nexport const b = 2;\nexport const c = 3;\n",
      "utf8",
    );

    const response = await bridge.send("batch", {
      file,
      edits: [
        { match: "export const a = 1;", replacement: "export    const   a   = 10;" },
        { match: "export const b = 2;", replacement: "export    const   b   = 20;" },
        { match: "export const c = 3;", replacement: "export    const   c   = 30;" },
      ],
    });

    expect(response.success).toBe(true);
    expect(response.formatted).toBe(true);
    expect(await readTextFile(file)).toBe(
      "export const a = 10;\nexport const b = 20;\nexport const c = 30;\n",
    );
  });

  test("multi-file batch — formatter runs per file", async () => {
    const rustShim: FakeFormatterShim = {
      name: "rustfmt",
      script: `#!/bin/sh
file="$1"
sed -E 's/  +/ /g; s/{/{\n    /; s/;}/;\n}/' "$file" > "$file.tmp" && mv "$file.tmp" "$file"
`,
    };
    const { h, tools, sdkCtx } = await harness(BIOME_TS_AND_RUSTFMT_PRESET, [
      tsCollapseSpacesShim("biome"),
      rustShim,
    ]);
    await writeFile(h.path("one.ts"), "export const tsValue = 1;\n", "utf8");
    await writeFile(h.path("main.rs"), "fn main(){let x=1;}\n", "utf8");

    const response = parseToolJson(
      await tools.edit.execute(
        {
          operations: [
            {
              file: h.path("one.ts"),
              command: "edit_match",
              match: "export const tsValue = 1;",
              replacement: "export    const   tsValue   = 2;",
            },
            {
              file: h.path("main.rs"),
              command: "edit_match",
              match: "fn main(){let x=1;}",
              replacement: "fn   main(){let   x=2;}",
            },
          ],
        },
        sdkCtx,
      ),
    );

    expect(response.success).toBe(true);
    expect(response.files_modified).toBe(2);
    const results = response.results as Array<Record<string, unknown>>;
    expect(results.find((r) => String(r.file).endsWith("one.ts"))?.formatted).toBe(true);
    expect(results.find((r) => String(r.file).endsWith("main.rs"))?.formatted).toBe(false);
    expect(await readTextFile(h.path("one.ts"))).toBe("export const tsValue = 2;\n");
    expect(await readTextFile(h.path("main.rs"))).toBe("fn   main(){let   x=2;}\n");
  });

  test("multi-file batch — one file's formatter excluded", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_EXCLUDED_PRESET, [biomeExcludedPathShim()]);
    await mkdir(h.path("src"), { recursive: true });
    await mkdir(h.path("scratch"), { recursive: true });

    const response = parseToolJson(
      await tools.edit.execute(
        {
          operations: [
            {
              file: h.path("src", "in.ts"),
              command: "write",
              content: "export    const   inScope   = 1;\n",
            },
            {
              file: h.path("scratch", "out.ts"),
              command: "write",
              content: "export    const   outScope   = 1;\n",
            },
          ],
        },
        sdkCtx,
      ),
    );

    const results = response.results as Array<Record<string, unknown>>;
    expect(response.success).toBe(true);
    expect(results.find((r) => String(r.file).endsWith("src/in.ts"))?.formatted).toBe(false);
    expect(
      results.find((r) => String(r.file).endsWith("scratch/out.ts"))?.format_skipped_reason,
    ).toBe("formatter_excluded_path");
    expect(await readTextFile(h.path("src", "in.ts"))).toBe("export    const   inScope   = 1;\n");
    expect(await readTextFile(h.path("scratch", "out.ts"))).toBe(
      "export    const   outScope   = 1;\n",
    );
  });

  test("multi-file batch — one operation fails", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_PRESET);
    await writeFile(h.path("ok.ts"), "export const ok = 1;\n", "utf8");
    await writeFile(h.path("fail.ts"), "export    const   fail   = 1;\n", "utf8");

    await expect(
      tools.edit.execute(
        {
          operations: [
            {
              file: h.path("ok.ts"),
              command: "edit_match",
              match: "export const ok = 1;",
              replacement: "export    const   ok   = 2;",
            },
            {
              file: h.path("fail.ts"),
              command: "edit_match",
              match: "missing",
              replacement: "export const fail = 2;",
            },
          ],
        },
        sdkCtx,
      ),
    ).rejects.toThrow("missing");

    expect(await readTextFile(h.path("ok.ts"))).toBe("export const ok = 1;\n");
    expect(await readTextFile(h.path("fail.ts"))).toBe("export    const   fail   = 1;\n");
  });

  test("multi-file batch — write-mode operation triggers formatter", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_PRESET);

    const response = parseToolJson(
      await tools.edit.execute(
        {
          operations: [
            {
              file: h.path("written.ts"),
              command: "write",
              content: "export    const   written   = 1;\n",
            },
          ],
        },
        sdkCtx,
      ),
    );

    expect(response.success).toBe(true);
    expect((response.results as Array<Record<string, unknown>>)[0].formatted).toBe(true);
    expect(await readTextFile(h.path("written.ts"))).toBe("export const written = 1;\n");
  });

  test("glob edit across multiple files formats each matched file", async () => {
    const { h, bridge } = await (async () => {
      const h = await createFormatHarness(preparedBinary, BIOME_TS_PRESET, [
        tsCollapseSpacesShim("biome"),
      ]);
      harnesses.push(h);
      return { h, bridge: h.bridge };
    })();
    await mkdir(h.path("glob"), { recursive: true });
    await writeFile(h.path("glob", "a.ts"), "export const OLD_VALUE = 1;\n", "utf8");
    await writeFile(h.path("glob", "b.ts"), "export const OLD_VALUE = 2;\n", "utf8");

    const response = await bridge.send("edit_match", {
      file: h.path("glob", "*.ts"),
      match: "OLD_VALUE",
      replacement: "NEW_VALUE",
    });

    expect(response.success).toBe(true);
    expect(response.total_files).toBe(2);
    expect(
      (response.files as Array<Record<string, unknown>>).every((f) => f.formatted === true),
    ).toBe(true);
    expect(await readTextFile(h.path("glob", "a.ts"))).toBe("export const NEW_VALUE = 1;\n");
    expect(await readTextFile(h.path("glob", "b.ts"))).toBe("export const NEW_VALUE = 2;\n");
  });

  test("glob edit with formatter excluded for some files", async () => {
    const { h, bridge } = await (async () => {
      const h = await createFormatHarness(preparedBinary, BIOME_TS_EXCLUDED_PRESET, [
        biomeExcludedPathShim("biome"),
      ]);
      harnesses.push(h);
      return { h, bridge: h.bridge };
    })();
    await mkdir(h.path("src"), { recursive: true });
    await mkdir(h.path("scratch"), { recursive: true });
    await writeFile(h.path("src", "a.ts"), "export const OLD_VALUE = 1;\n", "utf8");
    await writeFile(h.path("scratch", "b.ts"), "export const OLD_VALUE = 2;\n", "utf8");

    const response = await bridge.send("edit_match", {
      file: h.path("**", "*.ts"),
      match: "OLD_VALUE",
      replacement: "NEW_VALUE",
    });

    expect(response.success).toBe(true);
    expect(response.total_files).toBe(2);
    const files = response.files as Array<Record<string, unknown>>;
    expect(files.find((f) => String(f.file).endsWith("src/a.ts"))?.formatted).toBe(false);
    expect(files.find((f) => String(f.file).endsWith("scratch/b.ts"))?.formatted).toBe(false);
    expect(await readTextFile(h.path("src", "a.ts"))).toBe("export const NEW_VALUE = 1;\n");
    expect(await readTextFile(h.path("scratch", "b.ts"))).toBe("export const NEW_VALUE = 2;\n");
  });

  test("batch with file deletion via empty content range", async () => {
    const { h, bridge } = await (async () => {
      const h = await createFormatHarness(preparedBinary, BIOME_TS_PRESET, [
        tsCollapseSpacesShim("biome"),
      ]);
      harnesses.push(h);
      return { h, bridge: h.bridge };
    })();
    const file = h.path("delete-lines.ts");
    await writeFile(
      file,
      "export const keep = 1;\nexport    const   remove   = 2;\nexport const alsoKeep = 3;\n",
      "utf8",
    );

    const response = await bridge.send("batch", {
      file,
      edits: [{ line_start: 2, line_end: 2, content: "" }],
    });

    expect(response.success).toBe(true);
    expect(response.formatted).toBe(true);
    expect(await readTextFile(file)).toBe("export const keep = 1;\nexport const alsoKeep = 3;\n");
  });

  test("batch operations preserve formatter on partial failure (atomic transaction)", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_PRESET);
    await writeFile(h.path("first.ts"), "export const first = 1;\n", "utf8");
    await writeFile(h.path("second.ts"), "export    const   second   = 1;\n", "utf8");

    await expect(
      tools.edit.execute(
        {
          operations: [
            {
              file: h.path("first.ts"),
              command: "edit_match",
              match: "export const first = 1;",
              replacement: "export    const   first   = 2;",
            },
            {
              file: h.path("second.ts"),
              command: "edit_match",
              match: "does-not-exist",
              replacement: "export const second = 2;",
            },
          ],
        },
        sdkCtx,
      ),
    ).rejects.toThrow("does-not-exist");

    expect(await readTextFile(h.path("first.ts"))).toBe("export const first = 1;\n");
    expect(await readTextFile(h.path("second.ts"))).toBe("export    const   second   = 1;\n");
  });
});
