/**
 * E2E coverage for aft_outline + aft_zoom.
 */

/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdir, writeFile } from "node:fs/promises";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { createHarness, type Harness, prepareBinary } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

maybeDescribe("aft_outline + aft_zoom (real bridge)", () => {
  let harness: Harness;

  beforeAll(async () => {
    harness = await createHarness(initialBinary);
  });

  afterAll(async () => {
    if (harness) await harness.cleanup();
  });

  test("outline single file — sample.ts lists functions and class", async () => {
    const result = await harness.callTool("aft_outline", { target: "sample.ts" });
    const text = harness.text(result);
    expect(text).toContain("funcA");
    expect(text).toContain("funcB");
    expect(text).toContain("SampleService");
  });

  test("outline single file keeps text details shape", async () => {
    await writeFile(harness.path("single.ts"), "export function single() { return 1; }\n", "utf8");

    const result = await harness.callTool("aft_outline", { target: "single.ts" });
    const text = harness.text(result);

    expect(result.details).toBeUndefined();
    expect(text).toContain("single.ts");
    expect(text).toContain("single");
  });

  test("outline batched files via array target", async () => {
    const result = await harness.callTool("aft_outline", {
      target: [harness.path("sample.ts"), harness.path("imports.ts")],
    });
    const text = harness.text(result);
    expect(text).toContain("sample.ts");
    expect(text).toContain("imports.ts");
  });

  test("outline array target keeps text details shape", async () => {
    await writeFile(harness.path("array-a.ts"), "export function arrayA() { return 1; }\n", "utf8");
    await writeFile(harness.path("array-b.ts"), "export function arrayB() { return 2; }\n", "utf8");

    const result = await harness.callTool("aft_outline", {
      target: [harness.path("array-a.ts"), harness.path("array-b.ts")],
    });
    const text = harness.text(result);

    expect(result.details).toBeUndefined();
    expect(text).toContain("array-a.ts");
    expect(text).toContain("array-b.ts");
    expect(text).toContain("arrayA");
    expect(text).toContain("arrayB");
  });

  test("outline directory via target", async () => {
    const result = await harness.callTool("aft_outline", { target: "." });
    const text = harness.text(result);
    expect(text).toContain("sample.ts");
    // Go file should be included
    expect(text).toContain("sample.go");
  });

  test("outline rejects empty string target", async () => {
    await expect(harness.callTool("aft_outline", { target: "" })).rejects.toThrow(/non-empty/);
  });

  test("outline auto-detects directory passed as string target", async () => {
    const result = await harness.callTool("aft_outline", { target: "directory" });
    const text = harness.text(result);
    // Directory mode returned (tree output) — real content depends on fixture
    expect(text.length).toBeGreaterThan(0);
  });

  test("outline directory target returns complete true below walk cap", async () => {
    await mkdir(harness.path("outline-small"), { recursive: true });
    await writeFile(
      harness.path("outline-small", "good.ts"),
      "export function good() { return 1; }\n",
      "utf8",
    );
    await writeFile(harness.path("outline-small", "bad.ts"), "export function bad( {\n", "utf8");

    const result = await harness.callTool("aft_outline", { target: "outline-small" });
    const response = result.details as Record<string, unknown>;

    expect(response.complete).toBe(true);
    expect(response.walk_truncated).toBe(false);
    const skipped = response.skipped_files as Array<{ file: string; reason: string }>;
    expect(skipped).toHaveLength(1);
    expect(skipped[0].file).toMatch(/outline-small[/\\]bad\.ts$/);
    expect(skipped[0].reason).toBe("parse_error");
    expect(harness.text(result)).toContain("good.ts");
    expect(harness.text(result)).toContain("good");
  });

  test("outline directory target returns complete false when Rust walk truncates", async () => {
    await mkdir(harness.path("outline-large"), { recursive: true });
    for (let index = 0; index < 205; index += 1) {
      await writeFile(
        harness.path("outline-large", `file-${String(index).padStart(3, "0")}.ts`),
        `export const value${index} = ${index};\n`,
        "utf8",
      );
    }

    const result = await harness.callTool("aft_outline", { target: "outline-large" });
    const response = result.details as Record<string, unknown>;

    expect(response.complete).toBe(false);
    expect(response.walk_truncated).toBe(true);
    expect(Array.isArray(response.skipped_files)).toBe(true);
    expect(harness.text(result)).toContain("file-000.ts");
  });

  test("zoom into single symbol returns source", async () => {
    const result = await harness.callTool("aft_zoom", {
      filePath: "sample.ts",
      symbol: "funcB",
    });
    const text = harness.text(result);
    expect(text).toContain("funcB");
    expect(text).toContain("normalize");
  });

  test("zoom multi-symbol returns array", async () => {
    const result = await harness.callTool("aft_zoom", {
      filePath: "sample.ts",
      symbols: ["funcA", "funcB"],
    });
    const text = harness.text(result);
    // Array-shaped JSON: two results
    expect(text).toContain("funcA");
    expect(text).toContain("funcB");
  });

  test("zoom with contextLines expands range", async () => {
    const result = await harness.callTool("aft_zoom", {
      filePath: "sample.ts",
      symbol: "funcA",
      contextLines: 10,
    });
    const text = harness.text(result);
    expect(text).toContain("funcA");
  });
});

/**
 * URL-mode coverage. These are critical because they exercise undici's
 * `connect.lookup` callback under the actual Node runtime — exactly the path
 * that surfaced as `ERR_INVALID_IP_ADDRESS: Invalid IP address: undefined`
 * when the dispatcher's pinned-DNS callback was called with the legacy
 * 3-arg `(err, address, family)` shape against a Node 18+ connector that
 * passes `opts.all: true` and expects `(err, [{address, family}])`.
 *
 * The mock server binds to 127.0.0.1, so we need `url_fetch_allow_private`
 * to bypass the SSRF guard.
 */
const urlMaybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

urlMaybeDescribe("aft_outline + aft_zoom — URL targets (real bridge + real fetch)", () => {
  let harness: Harness;
  let server: Server;
  let serverUrl: string;

  const markdown = [
    "# Test Document",
    "",
    "## Section A",
    "",
    "Body of section A.",
    "",
    "## Section B",
    "",
    "Body of section B.",
    "",
    "### Subsection B1",
    "",
    "More content.",
    "",
  ].join("\n");

  beforeAll(async () => {
    harness = await createHarness(initialBinary, {
      // Allow 127.0.0.1 so the SSRF guard accepts our localhost mock server.
      // Plumbed straight through `url_fetch_allow_private`.
      config: { url_fetch_allow_private: true },
    });

    server = createServer((req, res) => {
      // GitHub-like: serve markdown for the .md path, html for /html, return
      // 404 elsewhere so we also exercise the failure path. Strip query
      // strings before matching so cache-busting params still hit the doc.
      const path = (req.url ?? "").split("?")[0];
      if (path === "/doc.md" || path === "/doc") {
        res.writeHead(200, { "content-type": "text/markdown; charset=utf-8" });
        res.end(markdown);
        return;
      }
      if (path === "/doc.html") {
        res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
        res.end(`<!doctype html><html><body><h1>HTML Doc</h1><h2>Section X</h2></body></html>`);
        return;
      }
      res.writeHead(404, { "content-type": "text/plain" });
      res.end("not found");
    });

    await new Promise<void>((resolve) => {
      server.listen(0, "127.0.0.1", () => {
        const address = server.address() as AddressInfo;
        serverUrl = `http://127.0.0.1:${address.port}`;
        resolve();
      });
    });
  });

  afterAll(async () => {
    if (harness) await harness.cleanup();
    if (server) {
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
  });

  test("outline URL — markdown headings extracted via real fetch", async () => {
    const result = await harness.callTool("aft_outline", {
      target: `${serverUrl}/doc.md`,
    });
    const text = harness.text(result);

    // This is the test that locks in the Node-runtime fetch fix. Before the
    // dual-shape lookup callback was added, this call surfaced as
    // `ERR_INVALID_IP_ADDRESS: Invalid IP address: undefined` from
    // net:emitLookup under Node 18+.
    expect(text).toContain("Test Document");
    expect(text).toContain("Section A");
    expect(text).toContain("Section B");
    expect(text).toContain("Subsection B1");
  });

  test("outline URL — HTML headings extracted via real fetch", async () => {
    const result = await harness.callTool("aft_outline", {
      target: `${serverUrl}/doc.html`,
    });
    const text = harness.text(result);
    expect(text).toContain("HTML Doc");
    expect(text).toContain("Section X");
  });

  test("zoom URL — fetches and zooms into a section", async () => {
    const result = await harness.callTool("aft_zoom", {
      url: `${serverUrl}/doc.md`,
      symbol: "Section A",
    });
    const text = harness.text(result);
    expect(text).toContain("Section A");
    expect(text).toContain("Body of section A.");
  });

  test("zoom URL — uses cache for repeat fetch (no second HTTP hit)", async () => {
    let hits = 0;
    server.on("request", () => {
      hits += 1;
    });

    // Use a fresh URL the cache hasn't seen yet.
    const url = `${serverUrl}/doc?cache-test`;
    await harness.callTool("aft_outline", { target: url });
    const initialHits = hits;
    await harness.callTool("aft_outline", { target: url });
    // No additional HTTP hit on the second call.
    expect(hits).toBe(initialHits);
  });

  test("outline URL — fetch failure surfaces structured error", async () => {
    await expect(
      harness.callTool("aft_outline", { target: `${serverUrl}/missing.md` }),
    ).rejects.toThrow(/HTTP 404/);
  });

  test("zoom URL — rejects when both filePath and url provided", async () => {
    await expect(
      harness.callTool("aft_zoom", {
        filePath: "sample.ts",
        url: `${serverUrl}/doc.md`,
        symbol: "anything",
      }),
    ).rejects.toThrow(/exactly ONE of 'filePath' or 'url'/);
  });
});
