/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { access, readFile } from "node:fs/promises";
import { join } from "node:path";
import type { ToolContext } from "@opencode-ai/plugin";
import { aftPrefixedTools, hoistedTools } from "../../tools/hoisted.js";
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
  GOFMT_PRESET,
  genericErrorFormatterShim,
  hangingFormatterShim,
  NO_FORMATTER_PRESET,
  RUFF_PRESET,
  RUSTFMT_PRESET,
  tsCollapseSpacesShim,
} from "./format-helpers.js";
import { type E2EHarness, type PreparedBinary, prepareBinary } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

const BIOME_TS_EXPECTED = `export function foo(a: number, b: number) {
  return a + b;
}
const x = { a: 1, b: 2, c: 3 };
console.log(foo(1, 2), x);
`;

const RUSTFMT_EXPECTED = `fn main() {
    let x = 42;
    let y = vec![1, 2, 3];
    println!("{} {:?}", x, y);
}
`;

const GOFMT_EXPECTED = `package main

import "fmt"

func main() { x := 42; fmt.Println(x) }
`;

const TS_FORMATTED_BY_BIOME = `export function foo(a: number, b: number) {
  return a + b;
}
`;

const RUFF_EXPECTED = `def foo(a, b):
    return a + b


x = {"a": 1, "b": 2}
print(foo(1, 2), x)
`;

function createMockClient(): any {
  return {
    lsp: { status: async () => ({ data: [] }) },
    find: { symbols: async () => ({ data: [] }) },
  };
}

function createPluginContext(pool: PluginContext["pool"], storageDir: string): PluginContext {
  return {
    pool,
    client: createMockClient(),
    config: {} as PluginContext["config"],
    storageDir,
  };
}

function createSdkContext(directory: string): ToolContext {
  return {
    sessionID: "format-on-edit-write-e2e",
    messageID: "format-on-edit-write-message",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  };
}

async function commandAvailable(command: string): Promise<boolean> {
  const paths = (process.env.PATH ?? "").split(":").filter(Boolean);
  for (const dir of paths) {
    try {
      await access(join(dir, command));
      return true;
    } catch {
      // keep searching
    }
  }
  return false;
}

async function ruffCanFormat(): Promise<boolean> {
  if (!(await commandAvailable("ruff"))) return false;
  const output = await new Promise<string>((resolve) => {
    const proc = spawn("ruff", ["--version"], { stdio: ["ignore", "pipe", "ignore"] });
    let stdout = "";
    proc.stdout.on("data", (chunk) => {
      stdout += String(chunk);
    });
    proc.on("error", () => resolve(""));
    proc.on("close", () => resolve(stdout.trim().replace(/^ruff\s+/, "")));
  });
  const [major, minor, patch] = output.split(".").map((part) => Number.parseInt(part, 10));
  if (![major, minor, patch].every(Number.isFinite)) return false;
  return major > 0 || minor > 1 || (minor === 1 && patch >= 2);
}

const gofmtAvailable = await commandAvailable("gofmt");
const ruffFormatAvailable = await ruffCanFormat();

function formatterPreset(tool: string): FormatPreset {
  return { configFiles: [], explicitFormatter: { typescript: tool } };
}

maybeDescribe("e2e format_on_edit write tools", () => {
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

  async function configure(
    h: E2EHarness,
    overrides: Record<string, unknown>,
  ): Promise<Record<string, unknown>> {
    return await h.bridge.send("configure", {
      project_root: h.tempDir,
      harness: "opencode",
      validate_on_edit: "syntax",
      ...overrides,
    });
  }

  async function executeHoistedWrite(h: E2EHarness, filePath: string, content: string) {
    let data: Record<string, unknown> | undefined;
    const pool = {
      getBridge: () => ({
        send: async (command: string, params: Record<string, unknown>) => {
          const response = await h.bridge.send(command, params);
          if (command === "write") data = response;
          return response;
        },
      }),
    } as unknown as PluginContext["pool"];
    const tools = hoistedTools(createPluginContext(pool, h.path(".storage")));
    const output = await tools.write.execute({ filePath, content }, createSdkContext(h.tempDir));
    if (!data) throw new Error("write response was not captured");
    return { output, data };
  }

  async function executePrefixedWrite(h: E2EHarness, filePath: string, content: string) {
    let data: Record<string, unknown> | undefined;
    const pool = {
      getBridge: () => ({
        send: async (command: string, params: Record<string, unknown>) => {
          const response = await h.bridge.send(command, params);
          if (command === "write") data = response;
          return response;
        },
      }),
    } as unknown as PluginContext["pool"];
    const tools = aftPrefixedTools(createPluginContext(pool, h.path(".storage")));
    const output = await tools.aft_write.execute(
      { filePath, content },
      createSdkContext(h.tempDir),
    );
    if (!data) throw new Error("write response was not captured");
    return { output, data };
  }

  function expectWriteOutcome(
    output: string,
    data: Record<string, unknown>,
    formatted: boolean,
    reason?: string,
  ) {
    expect(data.formatted).toBe(formatted);
    if (reason) expect(data.format_skipped_reason).toBe(reason);
    else expect(data.format_skipped_reason).toBeUndefined();
    if (formatted) expect(output).toContain("Auto-formatted.");
    else expect(output).not.toContain("Auto-formatted.");
  }

  test("biome formats deformatted TS", async () => {
    const h = await formatHarness(BIOME_TS_PRESET);
    const filePath = h.path("src", "deformatted.ts");

    const { output, data } = await executeHoistedWrite(h, filePath, FIXTURES.ts_deformatted);

    expect(await readFile(filePath, "utf8")).toBe(BIOME_TS_EXPECTED);
    expectWriteOutcome(output, data, true);
  });

  test("biome no-op on already-formatted TS", async () => {
    const h = await formatHarness(BIOME_TS_PRESET);
    const filePath = h.path("src", "formatted.ts");

    const { output, data } = await executePrefixedWrite(h, filePath, FIXTURES.ts_formatted);

    expect(await readFile(filePath, "utf8")).toBe(TS_FORMATTED_BY_BIOME);
    expectWriteOutcome(output, data, true);
  });

  test("biome refuses syntactically broken TS", async () => {
    const h = await formatHarness(formatterPreset("biome"), [genericErrorFormatterShim()]);
    const filePath = h.path("src", "invalid.ts");

    const { output, data } = await executeHoistedWrite(h, filePath, FIXTURES.ts_invalid);

    expect(await readFile(filePath, "utf8")).toBe(FIXTURES.ts_invalid);
    expectWriteOutcome(output, data, false, "error");
    expect(data.format_skipped_reason).not.toBe("formatter_excluded_path");
  });

  test("rustfmt formats deformatted Rust", async () => {
    const h = await formatHarness(RUSTFMT_PRESET);
    const filePath = h.path("src", "main.rs");

    const { output, data } = await executeHoistedWrite(h, filePath, FIXTURES.rust_deformatted);

    expect(await readFile(filePath, "utf8")).toBe(RUSTFMT_EXPECTED);
    expectWriteOutcome(output, data, true);
  }, 30_000);

  test.skipIf(!gofmtAvailable)(
    "gofmt formats deformatted Go (skipped: gofmt not available)",
    async () => {
      const h = await formatHarness(GOFMT_PRESET);
      const filePath = h.path("main.go");

      const { output, data } = await executeHoistedWrite(h, filePath, FIXTURES.go_deformatted);

      if (data.format_skipped_reason === "formatter_not_installed") {
        expect(await readFile(filePath, "utf8")).toBe(FIXTURES.go_deformatted);
        expectWriteOutcome(output, data, false, "formatter_not_installed");
      } else {
        expect(await readFile(filePath, "utf8")).toBe(GOFMT_EXPECTED);
        expectWriteOutcome(output, data, true);
      }
    },
  );

  test.skipIf(!ruffFormatAvailable)(
    "ruff formats deformatted Python (skipped: ruff not installed or too old for stable format)",
    async () => {
      const h = await formatHarness(RUFF_PRESET);
      const filePath = h.path("app.py");

      const { output, data } = await executeHoistedWrite(h, filePath, FIXTURES.py_deformatted);

      expect(await readFile(filePath, "utf8")).toBe(RUFF_EXPECTED);
      expectWriteOutcome(output, data, true);
    },
  );

  test("no formatter configured", async () => {
    const h = await formatHarness(NO_FORMATTER_PRESET);
    const filePath = h.path("src", "plain.ts");

    const { output, data } = await executeHoistedWrite(h, filePath, FIXTURES.ts_deformatted);

    expect(await readFile(filePath, "utf8")).toBe(FIXTURES.ts_deformatted);
    expectWriteOutcome(output, data, false, "no_formatter_configured");
  });

  test("formatter excluded path", async () => {
    const h = await formatHarness(BIOME_TS_EXCLUDED_PRESET);
    const filePath = h.path("scratch", "foo.ts");

    const { output, data } = await executeHoistedWrite(h, filePath, FIXTURES.ts_deformatted);

    expect(await readFile(filePath, "utf8")).toBe(FIXTURES.ts_deformatted);
    expectWriteOutcome(output, data, false, "formatter_excluded_path");
  });

  test("format_on_edit=false config", async () => {
    const h = await formatHarness(BIOME_TS_PRESET);
    await configure(h, { format_on_edit: false });
    const filePath = h.path("src", "disabled.ts");

    const { output, data } = await executeHoistedWrite(h, filePath, FIXTURES.ts_deformatted);

    expect(await readFile(filePath, "utf8")).toBe(FIXTURES.ts_deformatted);
    expectWriteOutcome(output, data, false, "no_formatter_configured");
  });

  test("fake formatter timeout", async () => {
    const h = await formatHarness(formatterPreset("biome"), [hangingFormatterShim()]);
    await configure(h, {
      format_on_edit: true,
      formatter_timeout_secs: 2,
      formatter: { typescript: "biome" },
    });
    const filePath = h.path("src", "timeout.ts");

    const { output, data } = await executeHoistedWrite(h, filePath, FIXTURES.ts_deformatted);

    expect(await readFile(filePath, "utf8")).toBe(FIXTURES.ts_deformatted);
    expectWriteOutcome(output, data, false, "timeout");
  }, 30_000);

  test("fake formatter not installed", async () => {
    const h = await formatHarness(formatterPreset("prettier"));
    const filePath = h.path("src", "missing.ts");

    const { output, data } = await executeHoistedWrite(h, filePath, FIXTURES.ts_deformatted);

    expect(await readFile(filePath, "utf8")).toBe(FIXTURES.ts_deformatted);
    expectWriteOutcome(output, data, false, "formatter_not_installed");
  });

  test("fake formatter generic error", async () => {
    const h = await formatHarness(formatterPreset("biome"), [genericErrorFormatterShim()]);
    await configure(h, {
      format_on_edit: true,
      formatter_timeout_secs: 5,
      formatter: { typescript: "biome" },
    });
    const filePath = h.path("src", "generic-error.ts");

    const { output, data } = await executeHoistedWrite(h, filePath, FIXTURES.ts_deformatted);

    expect(await readFile(filePath, "utf8")).toBe(FIXTURES.ts_deformatted);
    expectWriteOutcome(output, data, false, "error");
    expect(data.format_skipped_reason).not.toBe("formatter_excluded_path");
  });

  test("fake formatter excluded path", async () => {
    const h = await formatHarness(formatterPreset("biome"), [biomeExcludedPathShim()]);
    await configure(h, {
      format_on_edit: true,
      formatter_timeout_secs: 5,
      formatter: { typescript: "biome" },
    });
    const filePath = h.path("src", "shim-excluded.ts");

    const { output, data } = await executeHoistedWrite(h, filePath, FIXTURES.ts_deformatted);

    expect(await readFile(filePath, "utf8")).toBe(FIXTURES.ts_deformatted);
    expectWriteOutcome(output, data, false, "formatter_excluded_path");
  });

  test("unsupported language", async () => {
    const h = await formatHarness(BIOME_TS_PRESET);
    const filePath = h.path("notes.txt");
    const content = "alpha   beta\n";

    const { output, data } = await executeHoistedWrite(h, filePath, content);

    expect(await readFile(filePath, "utf8")).toBe(content);
    expectWriteOutcome(output, data, false, "unsupported_language");
  });

  test("multi-line content", async () => {
    const h = await formatHarness(formatterPreset("biome"), [tsCollapseSpacesShim()]);
    await configure(h, {
      format_on_edit: true,
      formatter_timeout_secs: 5,
      formatter: { typescript: "biome" },
    });
    const filePath = h.path("src", "many-lines.ts");
    const input = `${Array.from({ length: 50 }, (_, index) => `export  const   value${index}=  ${index};`).join("\n")}\n`;
    const expected = `${Array.from({ length: 50 }, (_, index) => `export const value${index}= ${index};`).join("\n")}\n`;

    const { output, data } = await executeHoistedWrite(h, filePath, input);

    expect(await readFile(filePath, "utf8")).toBe(expected);
    expectWriteOutcome(output, data, true);
  });
});
