/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import type { ToolContext } from "@opencode-ai/plugin";
import { hoistedTools } from "../../tools/hoisted.js";
import type { PluginContext } from "../../types.js";
import { noopAsk } from "../test-helpers";
import {
  BIOME_TS_EXCLUDED_PRESET,
  BIOME_TS_PRESET,
  createFormatHarness,
  type FakeFormatterShim,
  type FormatPreset,
} from "./format-helpers.js";
import { type E2EHarness, type PreparedBinary, prepareBinary } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

const BASE_TS = `export function foo(a: number, b: number) {
  return a + b;
}

export const alpha = 1;
export const beta = 1;
`;

function createMockClient(): any {
  return {
    lsp: { status: async () => ({ data: [] }) },
    find: { symbols: async () => ({ data: [] }) },
  };
}

function createPluginContext(pool: PluginContext["pool"], storageDir: string): PluginContext {
  return { pool, client: createMockClient(), config: {} as PluginContext["config"], storageDir };
}

function createSdkContext(directory: string): ToolContext {
  return {
    sessionID: "format-on-edit-edit-e2e",
    messageID: "format-on-edit-edit-message",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  };
}

function formatterPreset(tool: string): FormatPreset {
  return { configFiles: [], explicitFormatter: { typescript: tool, rust: tool } };
}

function countingTsShim(name = "biome"): FakeFormatterShim {
  return {
    name,
    script: `#!/bin/sh
for file; do :; done
dir="$(dirname "$file")"
echo "$file" >> "$dir/formatter-count.log"
python3 - "$file" <<'PY'
import re, sys
p=sys.argv[1]
s=open(p).read()
s=re.sub(r"export\\s+const\\s+(\\w+)\\s*=\\s*([^;\\n]+);?", r"export const \\1 = \\2;", s)
s=s.replace("return a+b;", "return a + b;")
s=s.replace("function foo( a:number,b:number )", "function foo(a: number, b: number)")
open(p,"w").write(s)
PY
exit 0
`,
  };
}

function rustFormatShim(name = "rustfmt"): FakeFormatterShim {
  return {
    name,
    script: `#!/bin/sh
file="$1"
cat > "$file" <<'EOF'
fn main() {
    let x = 42;
}
EOF
exit 0
`,
  };
}

maybeDescribe("e2e format_on_edit edit tool", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await Promise.allSettled(
      harnesses.splice(0, harnesses.length).map((harness) => harness.cleanup()),
    );
  });

  async function formatHarness(preset: FormatPreset, shims: FakeFormatterShim[] = []) {
    const h = await createFormatHarness(preparedBinary, preset, shims);
    harnesses.push(h);
    return h;
  }

  async function seedFile(filePath: string, content = BASE_TS) {
    await mkdir(filePath.slice(0, filePath.lastIndexOf("/")), { recursive: true });
    await writeFile(filePath, content, "utf8");
  }

  function editTool(h: E2EHarness) {
    let data: Record<string, unknown> | undefined;
    const pool = {
      getBridge: () => ({
        send: async (command: string, params: Record<string, unknown>) => {
          const response = await h.bridge.send(command, params);
          data = response;
          return response;
        },
      }),
    } as unknown as PluginContext["pool"];
    const tools = hoistedTools(createPluginContext(pool, h.path(".storage")));
    return {
      execute: async (args: Record<string, unknown>) => {
        const output = await tools.edit.execute(args, createSdkContext(h.tempDir));
        if (!data) throw new Error("edit response was not captured");
        return { output, data };
      },
    };
  }

  function expectEditOutcome(
    output: string,
    data: Record<string, unknown>,
    formatted: boolean,
    reason?: string,
  ) {
    expect(data.formatted).toBe(formatted);
    if (reason) expect(data.format_skipped_reason).toBe(reason);
    else expect(data.format_skipped_reason).toBeUndefined();
    if (formatted) expect(output).toContain('"formatted":true');
    else expect(output).not.toContain("Auto-formatted.");
  }

  test("edit oldString/newString triggers formatter", async () => {
    const h = await formatHarness(BIOME_TS_PRESET);
    const filePath = h.path("src", "one.ts");
    await seedFile(filePath);

    const { output, data } = await editTool(h).execute({
      filePath,
      oldString: "return a + b;",
      newString: "return a+b;",
    });

    expect(await readFile(filePath, "utf8")).toContain("return a + b;");
    expectEditOutcome(output, data, true);
  });

  test("edit replaceAll triggers formatter once per file", async () => {
    const h = await formatHarness(formatterPreset("biome"), [countingTsShim()]);
    const filePath = h.path("src", "replace-all.ts");
    await seedFile(filePath);

    const { output, data } = await editTool(h).execute({
      filePath,
      oldString: "= 1;",
      newString: "=    2",
      replaceAll: true,
    });

    expect(await readFile(filePath, "utf8")).toContain("export const alpha = 2;");
    expect(
      (await readFile(h.path("src", "formatter-count.log"), "utf8")).trim().split("\n"),
    ).toHaveLength(1);
    expectEditOutcome(output, data, true);
  });

  test("edit on already-formatted file with formatted replacement", async () => {
    const h = await formatHarness(BIOME_TS_PRESET);
    const filePath = h.path("src", "formatted.ts");
    await seedFile(filePath);

    const { output, data } = await editTool(h).execute({
      filePath,
      oldString: "alpha",
      newString: "gamma",
    });

    expect(await readFile(filePath, "utf8")).toContain("export const gamma = 1;");
    expectEditOutcome(output, data, true);
  });

  test("edit appendContent triggers formatter", async () => {
    const h = await formatHarness(BIOME_TS_PRESET);
    const filePath = h.path("src", "append.ts");
    await seedFile(filePath);

    const { output, data } = await editTool(h).execute({
      filePath,
      appendContent: "export   const z=3\n",
    });

    // Bug #4 fix (v0.18.3): append now runs through auto_format like
    // write/edit do. Biome reformats `export   const z=3` to
    // `export const z = 3;` and the response carries `formatted: true`.
    // Before the fix, append hardcoded `formatted: false, format_skipped_reason: None`
    // and the messy text landed verbatim.
    const finalContent = await readFile(filePath, "utf8");
    expect(finalContent).toContain("export const z = 3;");
    expect(finalContent).not.toContain("export   const z=3");
    expect(data.formatted).toBe(true);
    expect(data.format_skipped_reason).toBeUndefined();
    // Hoisted `edit` tool returns JSON-stringified Rust response, so the
    // `formatted: true` signal is in the JSON output (the human-readable
    // "Auto-formatted." string is only used by hoisted `write`).
    expect(output).toContain('"formatted":true');
  });

  test("edit symbol replace triggers formatter", async () => {
    const h = await formatHarness(BIOME_TS_PRESET);
    const filePath = h.path("src", "symbol.ts");
    await seedFile(filePath);

    const { output, data } = await editTool(h).execute({
      filePath,
      symbol: "foo",
      content: "function foo( a:number,b:number ){return a+b;}",
    });

    expect(await readFile(filePath, "utf8")).toContain("return a + b;");
    expectEditOutcome(output, data, true);
  });

  test("edit operations[] (batch) — single file", async () => {
    const h = await formatHarness(formatterPreset("biome"), [countingTsShim()]);
    const filePath = h.path("src", "batch.ts");
    await seedFile(filePath);

    const { data } = await editTool(h).execute({
      filePath,
      edits: [
        { oldString: "alpha = 1", newString: "alpha=  2" },
        { oldString: "beta = 1", newString: "beta=  3" },
      ],
    });

    expect(await readFile(filePath, "utf8")).toContain("export const alpha = 2;");
    expect(
      (await readFile(h.path("src", "formatter-count.log"), "utf8")).trim().split("\n"),
    ).toHaveLength(1);
    expect(data.formatted).toBe(true);
  });

  test("edit operations[] — multi-file (TS + Rust)", async () => {
    const h = await formatHarness(formatterPreset("biome"), [countingTsShim(), rustFormatShim()]);
    await h.bridge.send("configure", {
      project_root: h.tempDir,
      harness: "opencode",
      format_on_edit: true,
      formatter: { typescript: "biome", rust: "rustfmt" },
    });
    const tsFile = h.path("src", "multi.ts");
    const rsFile = h.path("src", "main.rs");
    await seedFile(tsFile);
    await seedFile(rsFile, "fn main(){let x=42;}\n");

    const { output, data } = await editTool(h).execute({
      operations: [
        { file: tsFile, command: "edit_match", match: "alpha = 1", replacement: "alpha=  2" },
        { file: rsFile, command: "edit_match", match: "let x=42;", replacement: "let x=42;" },
      ],
    });

    expect(await readFile(tsFile, "utf8")).toContain("export const alpha = 2;");
    expect(await readFile(rsFile, "utf8")).toBe("fn main() {\n    let x = 42;\n}\n");
    expect(output).toContain('"files_modified":2');
    expect(data.results).toBeArray();
  });

  test("edit operations[] — one file fails", async () => {
    const h = await formatHarness(formatterPreset("biome"), [countingTsShim()]);
    const okFile = h.path("src", "ok.ts");
    const failFile = h.path("src", "fail.ts");
    await seedFile(okFile);
    await seedFile(failFile);

    await expect(
      editTool(h).execute({
        operations: [
          { file: okFile, command: "edit_match", match: "alpha = 1", replacement: "alpha=  2" },
          { file: failFile, command: "edit_match", match: "does not exist", replacement: "x" },
        ],
      }),
    ).rejects.toThrow("does not exist");

    expect(await readFile(okFile, "utf8")).toContain("export const alpha = 1;");
    expect(
      (await readFile(h.path("src", "formatter-count.log"), "utf8")).trim().split("\n"),
    ).toHaveLength(1);
  });

  test("edit on file outside formatter scope", async () => {
    const h = await formatHarness(BIOME_TS_EXCLUDED_PRESET);
    const filePath = h.path("scratch", "foo.ts");
    await seedFile(filePath);

    const { output, data } = await editTool(h).execute({
      filePath,
      oldString: "alpha",
      newString: "gamma",
    });

    expect(await readFile(filePath, "utf8")).toContain("export const gamma = 1;");
    expectEditOutcome(output, data, false, "formatter_excluded_path");
  });

  test("edit with line range", async () => {
    const h = await formatHarness(BIOME_TS_PRESET);
    const filePath = h.path("src", "line-range.ts");
    await seedFile(filePath);

    const { output, data } = await editTool(h).execute({
      filePath,
      edits: [{ startLine: 5, endLine: 5, content: "export   const alpha=  2" }],
    });

    expect(await readFile(filePath, "utf8")).toContain("export const alpha = 2;");
    expectEditOutcome(output, data, true);
  });

  test("edit with formatter_excluded_path response — agent-facing output", async () => {
    const h = await formatHarness(BIOME_TS_EXCLUDED_PRESET);
    const filePath = h.path("scratch", "agent.ts");
    await seedFile(filePath);

    const { output, data } = await editTool(h).execute({
      filePath,
      oldString: "alpha",
      newString: "delta",
    });

    expect(data.format_skipped_reason).toBe("formatter_excluded_path");
    expect(output).not.toContain("Auto-formatted.");
  });
});
