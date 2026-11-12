/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { writeFile } from "node:fs/promises";
import { deflateSync } from "node:zlib";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolResult } from "@opencode-ai/plugin";
import { hoistedTools } from "../../tools/hoisted.js";
import type { PluginContext } from "../../types.js";
import { noopAsk } from "../test-helpers";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  lineNumberRangeText,
  lineNumberText,
  type PreparedBinary,
  prepareBinary,
  readTextFile,
  sendReadLikePlugin,
  withoutTrailingStatusBar,
} from "./helpers.js";

type ReadObjectResult = {
  output: string;
  title?: string;
  attachments?: Array<{ type: string; mime: string; url: string }>;
  metadata?: Record<string, unknown>;
};

function crc32(bytes: Buffer): number {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) {
      crc = crc & 1 ? (crc >>> 1) ^ 0xedb88320 : crc >>> 1;
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function pngChunk(type: string, data = Buffer.alloc(0)): Buffer {
  const typeBytes = Buffer.from(type, "ascii");
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length, 0);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(Buffer.concat([typeBytes, data])), 0);
  return Buffer.concat([length, typeBytes, data, crc]);
}

function onePixelPng(): Buffer {
  const signature = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(1, 0);
  ihdr.writeUInt32BE(1, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  const idat = deflateSync(Buffer.from([0, 255, 0, 0, 255]));
  return Buffer.concat([
    signature,
    pngChunk("IHDR", ihdr),
    pngChunk("IDAT", idat),
    pngChunk("IEND"),
  ]);
}

function createMockClient(): PluginContext["client"] {
  return { lsp: {}, find: {} } as PluginContext["client"];
}

function createToolContext(h: E2EHarness): ToolContext {
  return {
    messageID: "read-cutover",
    agent: "test",
    directory: h.tempDir,
    worktree: h.tempDir,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  } as ToolContext;
}

function createReadTool(h: E2EHarness) {
  const pool = { getBridge: () => h.bridge } as unknown as BridgePool;
  const ctx: PluginContext = {
    pool,
    client: createMockClient(),
    config: {} as PluginContext["config"],
    storageDir: h.path(".aft-test-storage"),
  };
  return hoistedTools(ctx).read;
}

async function executeReadTool(
  h: E2EHarness,
  args: Record<string, unknown>,
): Promise<ReadObjectResult> {
  const result = (await createReadTool(h).execute(args, createToolContext(h))) as ToolResult;
  if (typeof result === "string") {
    throw new Error(`expected object ToolResult, got string: ${result}`);
  }
  return result as ReadObjectResult;
}

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

maybeDescribe("e2e read command", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function harness(): Promise<E2EHarness> {
    const created = await createHarness(preparedBinary);
    harnesses.push(created);
    return created;
  }

  test("reads a full file with line numbers", async () => {
    const h = await harness();
    const filePath = h.path("sample.ts");

    const response = await h.bridge.send("read", { file: filePath });

    expect(response.success).toBe(true);
    expect(response.content).toBe(lineNumberText(await readTextFile(filePath)));
  });

  test("reads a line range", async () => {
    const h = await harness();
    const filePath = h.path("sample.ts");
    const source = await readTextFile(filePath);

    const response = await h.bridge.send("read", {
      file: filePath,
      start_line: 4,
      end_line: 7,
    });

    expect(response.success).toBe(true);
    expect(response.content).toBe(lineNumberRangeText(source, 4, 7));
    expect(response.start_line).toBe(4);
    expect(response.end_line).toBe(7);
  });

  test("reads with offset and limit pagination semantics", async () => {
    const h = await harness();
    const filePath = h.path("sample.ts");
    const source = await readTextFile(filePath);

    const response = await sendReadLikePlugin(h.bridge, filePath, {
      offset: 2,
      limit: 3,
    });

    expect(response.success).toBe(true);
    expect(response.content).toBe(lineNumberRangeText(source, 2, 4));
    expect(response.start_line).toBe(2);
    expect(response.end_line).toBe(4);
  });

  test("reads a directory and returns sorted entries", async () => {
    const h = await harness();

    const response = await h.bridge.send("read", { file: h.path("directory") });

    expect(response.success).toBe(true);
    expect(response.entries).toEqual(["alpha.ts", "beta.ts", "gamma.ts"]);
  });

  test("detects binary files", async () => {
    const h = await harness();

    const response = await h.bridge.send("read", { file: h.path("binary.bin") });

    expect(response.success).toBe(true);
    expect(response.binary).toBe(true);
    expect(response.message).toBe("Binary file (8 bytes), cannot display as text");
  });

  test("returns an error for a missing file", async () => {
    const h = await harness();

    const response = await h.bridge.send("read", { file: h.path("missing.ts") });

    expect(response.success).toBe(false);
    expect(response.code).toBe("not_found");
  });

  test("truncates very large reads with a hint", async () => {
    const h = await harness();
    const filePath = h.path("large.txt");
    const largeContent = Array.from(
      { length: 2000 },
      (_, index) => `line-${index}-${"x".repeat(80)}`,
    ).join("\n");
    await writeFile(filePath, `${largeContent}\n`);

    const response = await h.bridge.send("read", { file: filePath });

    expect(response.success).toBe(true);
    expect(response.truncated).toBe(true);
    expect(String(response.content)).toContain("output truncated at 50KB");
  });

  test("hoisted read tool returns server-rendered text for plain files, ranges, directories, and binaries", async () => {
    const h = await harness();

    const plainPath = h.path("sample.ts");
    const plain = await executeReadTool(h, { filePath: "sample.ts" });
    expect(withoutTrailingStatusBar(plain.output, "\n")).toBe(
      lineNumberText(await readTextFile(plainPath)),
    );
    expect(plain.title).toBe("sample.ts");
    expect(plain.metadata).toEqual({ title: "sample.ts" });

    const ranged = await executeReadTool(h, { filePath: "sample.ts", startLine: 4, endLine: 7 });
    expect(withoutTrailingStatusBar(ranged.output, "\n")).toBe(
      lineNumberRangeText(await readTextFile(plainPath), 4, 7),
    );
    expect(ranged.output).not.toContain("Use startLine/endLine");

    const directory = await executeReadTool(h, { filePath: "directory" });
    expect(withoutTrailingStatusBar(directory.output)).toBe("alpha.ts\nbeta.ts\ngamma.ts");
    expect(directory.title).toBe("directory");

    const binary = await executeReadTool(h, { filePath: "binary.bin" });
    expect(withoutTrailingStatusBar(binary.output)).toBe(
      "Binary file (8 bytes), cannot display as text",
    );
    expect(binary.title).toBe("binary.bin");
  });

  test("hoisted read tool refuses an inverted range by name instead of returning nothing", async () => {
    // Models that fill every declared property send inverted ranges like
    // this one (startLine 12, endLine 4). It must be refused by name, not
    // returned as an empty success.
    const h = await harness();
    await expect(
      executeReadTool(h, {
        filePath: "sample.ts",
        startLine: 12,
        endLine: 4,
        offset: 12,
        limit: 70,
      }),
    ).rejects.toThrow("invalid_range: startLine 12 is after endLine 4");

    // Zero placeholders next to a real offset/limit still page correctly.
    const paged = await executeReadTool(h, {
      filePath: "sample.ts",
      startLine: 0,
      endLine: 0,
      offset: 4,
      limit: 4,
    });
    expect(withoutTrailingStatusBar(paged.output, "\n")).toBe(
      lineNumberRangeText(await readTextFile(h.path("sample.ts")), 4, 7),
    );
  });

  test("hoisted read tool preserves the server footer for default truncated reads", async () => {
    const h = await harness();
    const filePath = h.path("large-hoisted.txt");
    const largeContent = Array.from(
      { length: 2000 },
      (_, index) => `line-${index}-${"x".repeat(80)}`,
    ).join("\n");
    await writeFile(filePath, `${largeContent}\n`);

    const result = await executeReadTool(h, { filePath: "large-hoisted.txt" });

    expect(result.output).toContain("output truncated at 50KB");
    expect(result.output).toContain(
      "Use startLine/endLine or offset/limit to read other sections.",
    );
  });

  test("hoisted read tool preserves image and PDF attachments while using server text", async () => {
    const h = await harness();
    const pngBytes = onePixelPng();
    const pdfBytes = Buffer.from("%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n%%EOF\n", "utf8");
    await writeFile(h.path("pixel.png"), pngBytes);
    await writeFile(h.path("doc.pdf"), pdfBytes);

    const image = await executeReadTool(h, { filePath: "pixel.png" });
    expect(withoutTrailingStatusBar(image.output)).toBe(
      `Read image attachment (image/png, 1×1, ${pngBytes.length} bytes).`,
    );
    expect(image.attachments).toEqual([
      {
        type: "file",
        mime: "image/png",
        url: `data:image/png;base64,${pngBytes.toString("base64")}`,
      },
    ]);
    expect(image.metadata?.preview).toBe(image.output);
    expect(String(image.metadata?.filepath).endsWith("/pixel.png")).toBe(true);
    expect(image.metadata?.isImage).toBe(true);
    expect(image.metadata?.isPdf).toBe(false);

    const pdf = await executeReadTool(h, { filePath: "doc.pdf" });
    expect(withoutTrailingStatusBar(pdf.output)).toBe(
      `Read PDF attachment (${pdfBytes.length} bytes).`,
    );
    expect(pdf.attachments).toEqual([
      {
        type: "file",
        mime: "application/pdf",
        url: `data:application/pdf;base64,${pdfBytes.toString("base64")}`,
      },
    ]);
    expect(pdf.metadata?.preview).toBe(pdf.output);
    expect(String(pdf.metadata?.filepath).endsWith("/doc.pdf")).toBe(true);
    expect(pdf.metadata?.isImage).toBe(false);
    expect(pdf.metadata?.isPdf).toBe(true);
  });

  test("hoisted read tool throws missing-file errors from tool_call", async () => {
    const h = await harness();

    await expect(
      createReadTool(h).execute({ filePath: "missing.ts" }, createToolContext(h)),
    ).rejects.toThrow("file not found");
  });
});
