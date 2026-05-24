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
  FIXTURES,
  type FormatPreset,
  genericErrorFormatterShim,
  hangingFormatterShim,
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
      content: '[package]\nname = "mixed_format_test"\nversion = "0.0.0"\nedition = "2021"\n',
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
    sessionID: `format-on-edit-apply-patch-${Math.random()}`,
    messageID: "format-on-edit-apply-patch-message",
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
  configOverrides: Record<string, unknown> = { format_on_edit: true, validate_on_edit: "syntax" },
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
    { storage_dir: join(h.tempDir, ".storage"), harness: "opencode", ...configOverrides },
  );
  return {
    h,
    tools: hoistedTools(createPluginContext(pool, join(h.tempDir, ".storage"))),
    sdkCtx: createSdkContext(h.tempDir),
    pool,
  };
}

maybeDescribe("e2e format_on_edit apply_patch", () => {
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

  async function harness(
    preset: FormatPreset,
    shims?: FakeFormatterShim[],
    configOverrides?: Record<string, unknown>,
  ) {
    const created = await createToolHarness(preparedBinary, preset, shims, configOverrides);
    harnesses.push(created.h);
    pools.push(created.pool);
    return created;
  }

  test("add hunk triggers formatter", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_PRESET);

    const output = await tools.apply_patch.execute(
      {
        patchText: `*** Begin Patch
*** Add File: new.ts
+${FIXTURES.ts_deformatted.split("\n").join("\n+").trimEnd()}
*** End Patch`,
      },
      sdkCtx,
    );

    expect(output).toContain("Created new.ts");
    expect(await readTextFile(h.path("new.ts"))).toBe(
      FIXTURES.ts_deformatted.replace(/ {2,}/g, " "),
    );
  });

  test("update hunk triggers formatter", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_PRESET);
    await writeFile(h.path("existing.ts"), "export const value = 1;\n", "utf8");

    const output = await tools.apply_patch.execute(
      {
        patchText: `*** Begin Patch
*** Update File: existing.ts
@@
-export const value = 1;
+export    const   value   = 2;
*** End Patch`,
      },
      sdkCtx,
    );

    expect(output).toContain("Updated existing.ts");
    expect(await readTextFile(h.path("existing.ts"))).toBe("export const value = 2;\n");
  });

  test("multi-file Add+Update both format", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_PRESET);
    await writeFile(h.path("old.ts"), "export const oldValue = 1;\n", "utf8");

    const output = await tools.apply_patch.execute(
      {
        patchText: `*** Begin Patch
*** Add File: added.ts
+export    const   added   = 1;
*** Update File: old.ts
@@
-export const oldValue = 1;
+export    const   oldValue   = 2;
*** End Patch`,
      },
      sdkCtx,
    );

    expect(output).toContain("Created added.ts");
    expect(output).toContain("Updated old.ts");
    expect(await readTextFile(h.path("added.ts"))).toBe("export const added = 1;\n");
    expect(await readTextFile(h.path("old.ts"))).toBe("export const oldValue = 2;\n");
  });

  test("move hunk formats destination", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_PRESET);
    await writeFile(h.path("from.ts"), "export const value = 1;\n", "utf8");

    const output = await tools.apply_patch.execute(
      {
        patchText: `*** Begin Patch
*** Update File: from.ts
*** Move to: nested/to.ts
@@
-export const value = 1;
+export    const   value   = 3;
*** End Patch`,
      },
      sdkCtx,
    );

    expect(output).toContain("Updated and moved from.ts → nested/to.ts");
    await expect(readTextFile(h.path("from.ts"))).rejects.toThrow();
    expect(await readTextFile(h.path("nested", "to.ts"))).toBe("export const value = 3;\n");
  });

  test("delete hunk does NOT trigger formatter", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_PRESET);
    await writeFile(h.path("delete-me.ts"), "export    const   gone   = 1;\n", "utf8");
    await writeFile(h.path("kept.ts"), "export    const   kept   = 1;\n", "utf8");

    const output = await tools.apply_patch.execute(
      {
        patchText: `*** Begin Patch
*** Delete File: delete-me.ts
*** End Patch`,
      },
      sdkCtx,
    );

    expect(output).toContain("Deleted delete-me.ts");
    await expect(readTextFile(h.path("delete-me.ts"))).rejects.toThrow();
    expect(await readTextFile(h.path("kept.ts"))).toBe("export    const   kept   = 1;\n");
  });

  test("mixed-language patch", async () => {
    const rustShim: FakeFormatterShim = {
      name: "rustfmt",
      script: `#!/bin/sh
file="$1"
cat > "$file" <<'EOF'
fn main() {
    let x = 42;
}
EOF
`,
    };
    const { h, tools, sdkCtx } = await harness(BIOME_TS_AND_RUSTFMT_PRESET, [
      tsCollapseSpacesShim("biome"),
      rustShim,
    ]);

    const output = await tools.apply_patch.execute(
      {
        patchText: `*** Begin Patch
*** Add File: src.ts
+export    const   tsValue   = 1;
*** Add File: main.rs
+fn   main(){let    x=42;}
*** End Patch`,
      },
      sdkCtx,
    );

    expect(output).toContain("Created src.ts");
    expect(output).toContain("Created main.rs");
    expect(await readTextFile(h.path("src.ts"))).toBe("export const tsValue = 1;\n");
    expect(await readTextFile(h.path("main.rs"))).toBe("fn main() {\n    let x = 42;\n}\n");
  });

  test("patch with formatter excluded path", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_EXCLUDED_PRESET, [biomeExcludedPathShim()]);
    await mkdir(h.path("scratch"), { recursive: true });

    const output = await tools.apply_patch.execute(
      {
        patchText: `*** Begin Patch
*** Add File: scratch/foo.ts
+export    const   foo   = 1;
*** End Patch`,
      },
      sdkCtx,
    );

    expect(output).toContain("Created scratch/foo.ts");
    expect(await readTextFile(h.path("scratch", "foo.ts"))).toBe("export    const   foo   = 1;\n");
    const response = await h.bridge.send("write", {
      file: h.path("scratch", "probe.ts"),
      content: "export    const   probe   = 1;\n",
    });
    expect(response.formatted).toBe(false);
    expect(response.format_skipped_reason).toBe("formatter_excluded_path");
  });

  test("patch with formatter timeout", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_PRESET, [hangingFormatterShim()], {
      format_on_edit: true,
      validate_on_edit: "syntax",
    });

    await expect(
      tools.apply_patch.execute(
        {
          patchText: `*** Begin Patch
*** Add File: timeout.ts
+export    const   slow   = 1;
*** End Patch`,
        },
        sdkCtx,
      ),
    ).rejects.toThrow("timed out");

    await expect(readTextFile(h.path("timeout.ts"))).rejects.toThrow();
  }, 25_000);

  test("patch with formatter generic error", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_PRESET, [genericErrorFormatterShim()]);

    const output = await tools.apply_patch.execute(
      {
        patchText: `*** Begin Patch
*** Add File: error.ts
+export    const   badFormat   = 1;
*** End Patch`,
      },
      sdkCtx,
    );

    expect(output).toContain("Created error.ts");
    expect(await readTextFile(h.path("error.ts"))).toBe("export    const   badFormat   = 1;\n");
    const response = await h.bridge.send("write", {
      file: h.path("error-probe.ts"),
      content: "export    const   badFormat   = 2;\n",
    });
    expect(response.formatted).toBe(false);
    expect(response.format_skipped_reason).toBe("error");
  });

  test("partial patch failure: successful Add gets formatted, failed Update does not", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_PRESET);
    await writeFile(h.path("target.ts"), "export const target = 1;\n", "utf8");

    const output = await tools.apply_patch.execute(
      {
        patchText: `*** Begin Patch
*** Add File: ok.ts
+export    const   ok   = 1;
*** Update File: target.ts
@@
-export const missing = 1;
+export    const   target   = 2;
*** End Patch`,
      },
      sdkCtx,
    );

    expect(output).toContain("Created ok.ts");
    expect(output).toContain("Failed to update target.ts");
    expect(output).toContain("Patch partially applied");
    expect(await readTextFile(h.path("ok.ts"))).toBe("export const ok = 1;\n");
    expect(await readTextFile(h.path("target.ts"))).toBe("export const target = 1;\n");
  });

  test("patch where ALL hunks fail does NOT format anything", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_PRESET);
    await writeFile(h.path("unchanged.ts"), "export    const   unchanged   = 1;\n", "utf8");

    await expect(
      tools.apply_patch.execute(
        {
          patchText: `*** Begin Patch
*** Update File: unchanged.ts
@@
-export const missing = 1;
+export    const   changed   = 2;
*** End Patch`,
        },
        sdkCtx,
      ),
    ).rejects.toThrow("Patch failed");
    expect(await readTextFile(h.path("unchanged.ts"))).toBe("export    const   unchanged   = 1;\n");
  });

  test("format_on_edit=false config", async () => {
    const { h, tools, sdkCtx } = await harness(BIOME_TS_PRESET, [tsCollapseSpacesShim("biome")], {
      format_on_edit: false,
      validate_on_edit: "syntax",
    });

    const output = await tools.apply_patch.execute(
      {
        patchText: `*** Begin Patch
*** Add File: disabled.ts
+export    const   disabled   = 1;
*** End Patch`,
      },
      sdkCtx,
    );

    expect(output).toContain("Created disabled.ts");
    expect(await readTextFile(h.path("disabled.ts"))).toBe("export    const   disabled   = 1;\n");
  });
});
